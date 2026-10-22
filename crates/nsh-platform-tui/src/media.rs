//! Terminal document navigation and bounded video decoding.
//!
//! Documents are read as HTML rather than executed in a browser. Video
//! bytes pass through the same capability boundary before an optional
//! FFmpeg process receives them on stdin; the decoder cannot open a URL,
//! playlist or local file. Each mounted leaf owns and cancels its work.

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    io::{Read, Write},
    process::{Child, Command as ProcessCommand, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, bounded};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nsh_core::{
    Command, EventKind, EventPayload, NativeProps, PayloadValue, ResizeMode, VideoProps, WebProps,
    WebSource, access::Access,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::image::Raster;

const MAX_DOCUMENT: usize = 8 << 20;
const MAX_TEXT: usize = 2 << 20;
const MAX_MEDIA: usize = 64 << 20;
const MAX_LINKS: usize = 512;
const MAX_HISTORY: usize = 64;
const FPS: f64 = 12.0;
const IO_TIMEOUT: Duration = Duration::from_secs(15);

type Assets = Option<Arc<dyn nsh_engine::Assets>>;
type Events = Vec<(EventKind, EventPayload)>;

/// Shared by one embedded surface, including retired loads during reload.
#[derive(Clone, Default)]
pub(crate) struct MediaRuntime {
    active: Arc<AtomicUsize>,
}

impl MediaRuntime {
    fn reserve(&self) -> Result<Permit, String> {
        self.active.try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < 8).then_some(active + 1)
        }).map(|_| Permit(self.active.clone())).map_err(|_| {
            "This terminal surface already has eight active media loads; stop a load before retrying".into()
        })
    }
}

struct Permit(Arc<AtomicUsize>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A terminal implementation of one mounted WebView or Video leaf.
pub(crate) enum MediaState {
    Web(Box<WebState>),
    Video(Box<VideoState>),
}

impl MediaState {
    pub(crate) fn new(
        props: NativeProps,
        assets: Assets,
        access: Access,
        now: f64,
        runtime: MediaRuntime,
    ) -> Self {
        match props {
            NativeProps::WebView(props) => {
                let mut state = WebState {
                    runtime,
                    ..WebState::default()
                };
                state.set(*props, assets, access);
                Self::Web(Box::new(state))
            }
            NativeProps::Video(props) => {
                let mut state = VideoState {
                    runtime,
                    ..VideoState::default()
                };
                state.set(*props, assets, access, now);
                Self::Video(Box::new(state))
            }
        }
    }

    pub(crate) fn set_props(
        &mut self,
        props: NativeProps,
        assets: Assets,
        access: Access,
        now: f64,
        runtime: MediaRuntime,
    ) {
        match (self, props) {
            (Self::Web(state), NativeProps::WebView(props)) => {
                state.runtime = runtime;
                state.set(*props, assets, access);
            }
            (Self::Video(state), NativeProps::Video(props)) => {
                state.runtime = runtime;
                state.set(*props, assets, access, now)
            }
            _ => {}
        }
    }

    pub(crate) fn set_access(&mut self, access: Access) {
        match self {
            Self::Web(state) => state.set(state.props.clone(), state.assets.clone(), access),
            Self::Video(state) => {
                if state.access == access {
                    return;
                }
                state.access = access;
                if state
                    .props
                    .source
                    .as_deref()
                    .is_some_and(|source| !state.access.allows_media(source))
                {
                    state.frame = None;
                    state.bytes = None;
                    state.fail("Video access is not granted by capabilities.network".into());
                } else if state.job.is_some() {
                    state.start(state.position, false);
                }
            }
        }
    }

    pub(crate) fn command(&mut self, command: &Command, now: f64) -> bool {
        match self {
            Self::Web(state) => state.command(command),
            Self::Video(state) => match command {
                Command::Seek(seconds) => {
                    state.seek(*seconds, now);
                    true
                }
                _ => false,
            },
        }
    }

    pub(crate) fn tick(&mut self, now: f64) -> bool {
        match self {
            Self::Web(state) => state.poll(),
            Self::Video(state) => state.tick(now),
        }
    }

    pub(crate) fn drain_events(&mut self) -> Events {
        std::mem::take(match self {
            Self::Web(state) => &mut state.events,
            Self::Video(state) => &mut state.events,
        })
    }

    pub(crate) fn raster(&self) -> Option<&Arc<Raster>> {
        match self {
            Self::Web(_) => None,
            Self::Video(state) => state.frame.as_ref(),
        }
    }

    pub(crate) fn resize_mode(&self) -> ResizeMode {
        match self {
            Self::Web(_) => ResizeMode::Contain,
            Self::Video(state) => state.props.resize_mode,
        }
    }

    /// Web rows fill the leaf. Video controls occupy its final row.
    pub(crate) fn lines(&self, columns: u16, rows: u16) -> Vec<String> {
        match self {
            Self::Web(state) => state.lines(columns, rows),
            Self::Video(state) => {
                if state.props.source.is_none() {
                    Vec::new()
                } else if let Some(error) = &state.error {
                    vec![truncate(error, usize::from(columns))]
                } else if state.props.controls {
                    let action = if state.props.paused || state.finished {
                        "Play"
                    } else {
                        "Pause"
                    };
                    let text = format!(
                        "{action}  {} / {}  {:.1}x  Space: play  ←/→: seek",
                        timestamp(state.position),
                        timestamp(state.info.duration),
                        state.props.rate,
                    );
                    vec![truncate(&text, usize::from(columns))]
                } else if state.frame.is_none() {
                    vec![truncate("Loading video…", usize::from(columns))]
                } else {
                    Vec::new()
                }
            }
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent, now: f64) -> bool {
        match self {
            Self::Web(state) => state.key(key),
            Self::Video(state) if state.props.controls => match key.code {
                KeyCode::Char(' ') if key.modifiers.is_empty() => {
                    if state.finished && state.error.is_none() && state.props.source.is_some() {
                        state.props.paused = false;
                        state.seek(0.0, now);
                        return true;
                    }
                    state.position = state.clock_position(now);
                    state.origin = state.position;
                    state.clock = now;
                    state.props.paused = !state.props.paused;
                    true
                }
                KeyCode::Left => {
                    state.seek((state.position - 5.0).max(0.0), now);
                    true
                }
                KeyCode::Right => {
                    state.seek(state.position + 5.0, now);
                    true
                }
                KeyCode::Home => {
                    state.seek(0.0, now);
                    true
                }
                _ => false,
            },
            Self::Video(_) => false,
        }
    }

    pub(crate) fn scroll(&mut self, lines: i32) -> bool {
        match self {
            Self::Web(state) if state.props.scrollable => {
                state.scroll_by(lines);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn active(&self) -> bool {
        match self {
            Self::Web(state) => state.job.is_some(),
            Self::Video(state) => {
                let playing = !state.props.paused && state.props.rate > 0.0 && !state.finished;
                playing || (state.job.is_some() && state.pending.is_none())
            }
        }
    }
}

#[derive(Clone, Default)]
struct Document {
    url: String,
    title: String,
    text: String,
    links: Vec<(String, String)>,
}

pub(crate) struct WebState {
    runtime: MediaRuntime,
    props: WebProps,
    assets: Assets,
    access: Access,
    document: Document,
    history: VecDeque<WebSource>,
    history_index: usize,
    source: WebSource,
    job: Option<LoadJob<Document>>,
    events: Events,
    scroll: usize,
    selected_link: Option<usize>,
    error: Option<String>,
    wrapped: RefCell<Option<(u16, Vec<String>)>>,
    viewport_rows: Cell<u16>,
}

impl Default for WebState {
    fn default() -> Self {
        Self {
            runtime: MediaRuntime::default(),
            props: WebProps::default(),
            assets: None,
            access: Access::all(),
            document: Document::default(),
            history: VecDeque::new(),
            history_index: 0,
            source: WebSource::None,
            job: None,
            events: Vec::new(),
            scroll: 0,
            selected_link: None,
            error: None,
            wrapped: RefCell::new(None),
            viewport_rows: Cell::new(1),
        }
    }
}

impl WebState {
    fn set(&mut self, props: WebProps, assets: Assets, access: Access) {
        let source_changed = props.source != self.props.source;
        let access_changed = access != self.access;
        let scripts_changed = props.injected_js != self.props.injected_js
            || props.injected_js_before != self.props.injected_js_before;
        self.assets = assets;
        self.access = access;
        self.props = props;
        if source_changed {
            self.navigate(self.props.source.clone(), true);
        } else if access_changed {
            self.document = Document::default();
            *self.wrapped.borrow_mut() = None;
            self.navigate(self.source.clone(), false);
        }
        if scripts_changed
            && (self.props.injected_js.is_some() || self.props.injected_js_before.is_some())
        {
            self.events.push(error(
                "Terminal WebView reads documents; page script injection requires a browser host component",
            ));
        }
    }

    fn navigate(&mut self, source: WebSource, remember: bool) {
        self.job = None;
        self.source = source.clone();
        self.error = None;
        self.scroll = 0;
        self.selected_link = None;
        if remember {
            self.history.truncate(self.history_index.saturating_add(1));
            self.history.push_back(source.clone());
            if self.history.len() > MAX_HISTORY {
                self.history.pop_front();
            }
            self.history_index = self.history.len().saturating_sub(1);
        }
        if source == WebSource::None {
            self.document = Document::default();
            *self.wrapped.borrow_mut() = None;
            return;
        }
        self.document.url = source_url(&source);
        self.events
            .push((EventKind::LoadStart, self.navigation(true)));
        self.events
            .push((EventKind::NavigationStateChange, self.navigation(true)));
        let assets = self.assets.clone();
        let access = self.access.clone();
        let user_agent = self.props.user_agent.clone();
        self.job = Some(LoadJob::spawn(
            "nsh-tui-document",
            &self.runtime,
            move |control| {
                if !access.allows_webviews() {
                    return Err("WebView access is not granted by capabilities.webview".into());
                }
                let (html, url) = match source {
                    WebSource::None => return Ok(Document::default()),
                    WebSource::Html { html, base_url } => {
                        if html.len() > MAX_DOCUMENT {
                            return Err("HTML exceeds 8 MiB".into());
                        }
                        (
                            html.to_string(),
                            base_url.map_or_else(|| "about:blank".into(), |v| v.to_string()),
                        )
                    }
                    WebSource::Uri(uri) if uri.as_ref() == "about:blank" => {
                        (String::new(), uri.to_string())
                    }
                    WebSource::Uri(uri) => {
                        let (bytes, url) = read_source(
                            &uri,
                            &assets,
                            &access,
                            true,
                            user_agent.as_deref(),
                            &control,
                        )?;
                        (String::from_utf8_lossy(&bytes).into_owned(), url)
                    }
                };
                parse_html(&html, &url)
            },
        ));
    }

    fn poll(&mut self) -> bool {
        let Some(result) = self.job.as_ref().and_then(|job| job.rx.try_recv().ok()) else {
            return false;
        };
        self.job = None;
        match result {
            Ok(document) => {
                self.document = document;
                *self.wrapped.borrow_mut() = None;
                self.events.push((EventKind::Load, self.navigation(false)));
            }
            Err(message) => {
                self.error = Some(message.clone());
                self.events.push(error(&message));
            }
        }
        self.events
            .push((EventKind::NavigationStateChange, self.navigation(false)));
        self.events
            .push((EventKind::LoadEnd, self.navigation(false)));
        true
    }

    fn navigation(&self, loading: bool) -> EventPayload {
        EventPayload::Map(vec![
            ("url", PayloadValue::Str(self.document.url.clone())),
            ("title", PayloadValue::Str(self.document.title.clone())),
            ("loading", PayloadValue::Bool(loading)),
            ("canGoBack", PayloadValue::Bool(self.history_index > 0)),
            (
                "canGoForward",
                PayloadValue::Bool(self.history_index + 1 < self.history.len()),
            ),
        ])
    }

    fn command(&mut self, command: &Command) -> bool {
        match command {
            Command::Reload => self.navigate(self.source.clone(), false),
            Command::GoBack if self.history_index > 0 => {
                self.history_index -= 1;
                self.navigate(self.history[self.history_index].clone(), false);
            }
            Command::GoForward if self.history_index + 1 < self.history.len() => {
                self.history_index += 1;
                self.navigate(self.history[self.history_index].clone(), false);
            }
            Command::GoBack | Command::GoForward => {}
            Command::StopLoading => {
                if self.job.take().is_some() {
                    self.events
                        .push((EventKind::NavigationStateChange, self.navigation(false)));
                    self.events.push((EventKind::LoadEnd, self.navigation(false)));
                }
            }
            Command::InjectJavaScript(_) | Command::PostMessage(_) => self.events.push(error(
                "Terminal WebView has no browser script context; register a host component for page scripts and messaging",
            )),
            _ => return false,
        }
        true
    }

    fn key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::ALT) {
            return match key.code {
                KeyCode::Left => self.command(&Command::GoBack),
                KeyCode::Right => self.command(&Command::GoForward),
                _ => false,
            };
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab if !self.document.links.is_empty() => {
                let backwards =
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                let count = self.document.links.len();
                if self.selected_link.is_some_and(|index| {
                    (!backwards && index + 1 == count) || (backwards && index == 0)
                }) {
                    self.selected_link = None;
                    return false;
                }
                self.selected_link = Some(match self.selected_link {
                    None if backwards => count - 1,
                    None => 0,
                    Some(index) if backwards => (index + count - 1) % count,
                    Some(index) => (index + 1) % count,
                });
                true
            }
            KeyCode::Enter => {
                if let Some((_, url)) = self
                    .selected_link
                    .and_then(|index| self.document.links.get(index))
                    .cloned()
                {
                    self.navigate(WebSource::Uri(url.into()), true);
                    true
                } else {
                    false
                }
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.command(&Command::Reload)
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End
                if self.props.scrollable =>
            {
                self.selected_link = None;
                self.scroll = self.scroll.min(self.max_scroll());
                let page = usize::from(self.viewport_rows.get().max(1));
                self.scroll = match key.code {
                    KeyCode::Up => self.scroll.saturating_sub(1),
                    KeyCode::Down => self.scroll.saturating_add(1).min(self.max_scroll()),
                    KeyCode::PageUp => self.scroll.saturating_sub(page),
                    KeyCode::PageDown => self.scroll.saturating_add(page).min(self.max_scroll()),
                    KeyCode::Home => 0,
                    _ => self.max_scroll(),
                };
                true
            }
            _ => false,
        }
    }

    fn max_scroll(&self) -> usize {
        self.wrapped.borrow().as_ref().map_or(0, |(_, lines)| {
            lines
                .len()
                .saturating_sub(usize::from(self.viewport_rows.get()))
        })
    }

    fn scroll_by(&mut self, lines: i32) {
        self.selected_link = None;
        self.scroll = self
            .scroll
            .min(self.max_scroll())
            .saturating_add_signed(lines as isize)
            .min(self.max_scroll());
    }

    fn lines(&self, columns: u16, rows: u16) -> Vec<String> {
        if columns == 0 || rows == 0 {
            return Vec::new();
        }
        self.viewport_rows.set(rows);
        if let Some(error) = &self.error {
            return wrap(error, usize::from(columns))
                .into_iter()
                .take(rows as usize)
                .collect();
        }
        if self.job.is_some() && self.document.text.is_empty() {
            return vec![truncate("Loading document…", usize::from(columns))];
        }
        let mut cache = self.wrapped.borrow_mut();
        if cache.as_ref().is_none_or(|(width, _)| *width != columns) {
            let mut text = self.document.text.clone();
            if !self.document.links.is_empty() {
                text.push_str("\n\nLinks · Tab selects · Enter opens\n");
                for (index, (label, url)) in self.document.links.iter().enumerate() {
                    text.push_str(&format!("  [{}] {label} — {url}\n", index + 1));
                }
            }
            *cache = Some((columns, wrap(&text, usize::from(columns))));
        }
        let lines = &cache.as_ref().expect("wrapped document").1;
        let mut top = self.scroll.min(lines.len().saturating_sub(rows as usize));
        let selected = self.selected_link.and_then(|index| {
            let marker = format!("  [{}]", index + 1);
            lines.iter().rposition(|line| line.starts_with(&marker))
        });
        if let Some(selected) = selected {
            if selected < top {
                top = selected;
            }
            if selected >= top + rows as usize {
                top = selected + 1 - rows as usize;
            }
        }
        lines
            .iter()
            .enumerate()
            .skip(top)
            .take(rows as usize)
            .map(|(index, line)| {
                if Some(index) == selected {
                    format!("›{}", &line[1..])
                } else {
                    line.clone()
                }
            })
            .collect()
    }
}

#[derive(Clone, Copy, Default)]
struct VideoInfo {
    width: u32,
    height: u32,
    duration: f64,
}

enum VideoMessage {
    Bytes(Arc<Vec<u8>>),
    Info(VideoInfo),
    Frame(f64, Arc<Raster>),
    Done,
    Error(String),
}

pub(crate) struct VideoState {
    runtime: MediaRuntime,
    props: VideoProps,
    assets: Assets,
    access: Access,
    bytes: Option<Arc<Vec<u8>>>,
    info: VideoInfo,
    job: Option<VideoJob>,
    pending: Option<(f64, Arc<Raster>)>,
    frame: Option<Arc<Raster>>,
    position: f64,
    clock: f64,
    origin: f64,
    last_progress: f64,
    finished: bool,
    decoded_end: bool,
    last_frame: f64,
    events: Events,
    error: Option<String>,
    waiting: bool,
}

impl Default for VideoState {
    fn default() -> Self {
        Self {
            runtime: MediaRuntime::default(),
            props: VideoProps::default(),
            assets: None,
            access: Access::all(),
            bytes: None,
            info: VideoInfo::default(),
            job: None,
            pending: None,
            frame: None,
            position: 0.0,
            clock: 0.0,
            origin: 0.0,
            last_progress: f64::NEG_INFINITY,
            finished: true,
            decoded_end: false,
            last_frame: 0.0,
            events: Vec::new(),
            error: None,
            waiting: false,
        }
    }
}

impl VideoState {
    fn set(&mut self, props: VideoProps, assets: Assets, access: Access, now: f64) {
        let source_changed = props.source != self.props.source;
        let access_changed = access != self.access;
        if !source_changed {
            self.position = self.clock_position(now);
            self.origin = self.position;
            self.clock = now;
        }
        self.assets = assets;
        self.access = access;
        self.props = props;
        if access_changed
            && self
                .props
                .source
                .as_deref()
                .is_some_and(|source| !self.access.allows_media(source))
        {
            self.frame = None;
            self.bytes = None;
            self.fail("Video access is not granted by capabilities.network".into());
            return;
        }
        self.props.rate = if self.props.rate.is_finite() {
            self.props.rate.clamp(0.0, 16.0)
        } else {
            1.0
        };
        if source_changed {
            self.job = None;
            self.bytes = None;
            self.frame = None;
            self.info = VideoInfo::default();
            self.position = 0.0;
            self.origin = 0.0;
            self.clock = now;
            self.error = None;
            self.finished = self.props.source.is_none();
            self.pending = None;
            self.decoded_end = false;
            if self.props.source.is_some() {
                self.start(0.0, true);
            }
        }
    }

    fn start(&mut self, seek: f64, loading: bool) {
        let Some(source) = self.props.source.clone() else {
            return;
        };
        if !self.access.allows_media(&source) {
            self.fail("Video access is not granted by capabilities.network".into());
            return;
        }
        self.job = None;
        self.pending = None;
        self.decoded_end = false;
        self.finished = false;
        self.waiting = true;
        if loading {
            self.events.push((EventKind::LoadStart, EventPayload::None));
        }
        self.events.push(buffer(true));
        self.job = Some(VideoJob::spawn(
            source,
            self.bytes.clone(),
            self.assets.clone(),
            self.access.clone(),
            seek,
            &self.runtime,
        ));
    }

    fn clock_position(&self, now: f64) -> f64 {
        if self.props.paused || self.finished || self.frame.is_none() || self.waiting {
            self.position
        } else {
            self.origin + (now - self.clock).max(0.0) * f64::from(self.props.rate)
        }
    }

    fn seek(&mut self, seconds: f64, now: f64) {
        if !seconds.is_finite() || self.props.source.is_none() {
            return;
        }
        let seconds = seconds.max(0.0).min(if self.info.duration > 0.0 {
            self.info.duration
        } else {
            4.0 * 60.0 * 60.0
        });
        self.position = seconds;
        self.origin = seconds;
        self.clock = now;
        // The end position still needs the final picture rather than an
        // empty decoder run. Frame timing is quantized to the output rate.
        let decode_at = if self.info.duration > 0.0 && seconds >= self.info.duration {
            (self.info.duration - 1.0 / FPS).max(0.0)
        } else {
            seconds
        };
        self.start(decode_at, false);
    }

    fn fail(&mut self, message: String) {
        self.job = None;
        self.pending = None;
        self.waiting = false;
        self.error = Some(message.clone());
        self.finished = true;
        self.events.push(error(&message));
        self.events.push(buffer(false));
    }

    fn tick(&mut self, now: f64) -> bool {
        let mut changed = false;
        let desired = self.clock_position(now);
        if self
            .pending
            .as_ref()
            .is_some_and(|(at, _)| *at <= desired || self.frame.is_none())
        {
            let (at, frame) = self.pending.take().expect("pending frame");
            if self.waiting {
                self.clock = now;
                self.origin = at;
                self.waiting = false;
                self.events.push(buffer(false));
            }
            self.frame = Some(frame);
            self.last_frame = at;
            self.position = at.max(desired);
            changed = true;
        }
        while self.pending.is_none() {
            let Some(message) = self.job.as_ref().and_then(|job| job.rx.try_recv().ok()) else {
                break;
            };
            changed = true;
            match message {
                VideoMessage::Bytes(bytes) => self.bytes = Some(bytes),
                VideoMessage::Info(info) => {
                    let first_load = self.info.width == 0;
                    self.info = info;
                    if first_load {
                        self.events.push((
                            EventKind::Load,
                            EventPayload::Map(vec![
                                ("duration", PayloadValue::Number(info.duration)),
                                ("width", PayloadValue::Number(info.width.into())),
                                ("height", PayloadValue::Number(info.height.into())),
                            ]),
                        ));
                    }
                }
                VideoMessage::Frame(at, frame) if at <= desired || self.frame.is_none() => {
                    if self.waiting {
                        self.clock = now;
                        self.origin = at;
                        self.waiting = false;
                        self.events.push(buffer(false));
                    }
                    self.frame = Some(frame);
                    self.last_frame = at;
                }
                VideoMessage::Frame(at, frame) => self.pending = Some((at, frame)),
                VideoMessage::Done => {
                    self.job = None;
                    self.decoded_end = true;
                    if self.info.duration <= 0.0 {
                        self.info.duration = self.last_frame + 1.0 / FPS;
                    }
                }
                VideoMessage::Error(message) => self.fail(message),
            }
        }
        if self.frame.is_some() && !self.props.paused && !self.finished {
            self.position = self.clock_position(now).min(if self.info.duration > 0.0 {
                self.info.duration
            } else {
                f64::MAX
            });
            changed = true;
            let ending = if self.info.duration > 0.0 {
                (self.last_frame + 1.0 / FPS).min(self.info.duration)
            } else {
                self.last_frame + 1.0 / FPS
            };
            if self.decoded_end && self.position + 1e-6 >= ending {
                self.events.push((EventKind::End, EventPayload::None));
                if self.props.repeat {
                    self.seek(0.0, now);
                } else {
                    self.finished = true;
                }
            }
            if now - self.last_progress
                >= f64::from(self.props.progress_interval_ms.max(16.0)) / 1000.0
            {
                self.last_progress = now;
                self.events.push((
                    EventKind::Progress,
                    EventPayload::Map(vec![
                        ("currentTime", PayloadValue::Number(self.position)),
                        ("duration", PayloadValue::Number(self.info.duration)),
                    ]),
                ));
            }
        }
        changed
    }
}

struct LoadJob<T> {
    rx: Receiver<Result<T, String>>,
    control: Arc<Control>,
}

impl<T: Send + 'static> LoadJob<T> {
    fn spawn(
        name: &str,
        runtime: &MediaRuntime,
        load: impl FnOnce(Arc<Control>) -> Result<T, String> + Send + 'static,
    ) -> Self {
        let (tx, rx) = bounded(1);
        let permit = match runtime.reserve() {
            Ok(permit) => permit,
            Err(message) => {
                let _ = tx.try_send(Err(message));
                return Self {
                    rx,
                    control: Arc::new(Control::default()),
                };
            }
        };
        let control = Arc::new(Control {
            _permit: Some(permit),
            ..Control::default()
        });
        let worker_control = control.clone();
        let failure_tx = tx.clone();
        let spawn = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let result = load(worker_control.clone());
                if !worker_control.cancelled() {
                    let _ = tx.send(result);
                }
            });
        if let Err(error) = spawn {
            let _ = failure_tx.try_send(Err(format!("Could not start document loader: {error}")));
        }
        Self { rx, control }
    }
}

impl<T> Drop for LoadJob<T> {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

struct VideoJob {
    rx: Receiver<VideoMessage>,
    control: Arc<Control>,
}

impl VideoJob {
    fn spawn(
        source: Arc<str>,
        bytes: Option<Arc<Vec<u8>>>,
        assets: Assets,
        access: Access,
        seek: f64,
        runtime: &MediaRuntime,
    ) -> Self {
        let (tx, rx) = bounded(3);
        let permit = match runtime.reserve() {
            Ok(permit) => permit,
            Err(message) => {
                let _ = tx.try_send(VideoMessage::Error(message));
                return Self {
                    rx,
                    control: Arc::new(Control::default()),
                };
            }
        };
        let control = Arc::new(Control {
            _permit: Some(permit),
            ..Control::default()
        });
        let worker_control = control.clone();
        let failure_tx = tx.clone();
        let spawn = std::thread::Builder::new()
            .name("nsh-tui-video".into())
            .spawn(move || {
                let result = (|| {
                    let bytes = match bytes {
                        Some(bytes) => bytes,
                        None => Arc::new(
                            read_source(&source, &assets, &access, false, None, &worker_control)?.0,
                        ),
                    };
                    let _reaper = ProcessReaper(worker_control.clone());
                    if tx.send(VideoMessage::Bytes(bytes.clone())).is_err() {
                        return Ok(());
                    }
                    decode_video(bytes, seek, &worker_control, &tx)
                })();
                if let Err(message) = result
                    && (!worker_control.cancelled()
                        || worker_control.timed_out.load(Ordering::Acquire))
                {
                    let _ = tx.send(VideoMessage::Error(message));
                }
            });
        if let Err(error) = spawn {
            let _ = failure_tx.try_send(VideoMessage::Error(format!(
                "Could not start video decoder: {error}"
            )));
        }
        Self { rx, control }
    }
}

impl Drop for VideoJob {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

#[derive(Default)]
struct ProcessState {
    child: Option<Child>,
    deadline: Option<Instant>,
}

#[derive(Default)]
struct Control {
    // A retired load keeps its permit until its final IO/watchdog thread exits.
    _permit: Option<Permit>,
    cancelled: AtomicBool,
    timed_out: AtomicBool,
    process: Mutex<ProcessState>,
    wake: Condvar,
}

struct ProcessReaper(Arc<Control>);

impl Drop for ProcessReaper {
    fn drop(&mut self) {
        let child = self.0.process.lock().expect("media process").child.take();
        self.0.wake.notify_all();
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Control {
    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn check(&self) -> Result<(), String> {
        if self.cancelled() {
            Err(if self.timed_out.load(Ordering::Acquire) {
                "Video decoder stopped responding for 15 seconds".into()
            } else {
                "Media load cancelled".into()
            })
        } else {
            Ok(())
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(child) = &mut self.process.lock().expect("media process").child {
            let _ = child.kill();
        }
        self.wake.notify_all();
    }

    fn deadline(&self, reading: bool) {
        self.process.lock().expect("media process").deadline =
            reading.then(|| Instant::now() + IO_TIMEOUT);
        self.wake.notify_all();
    }

    fn finish(&self) -> Result<(), String> {
        self.deadline(true);
        loop {
            self.check()?;
            let status = self
                .process
                .lock()
                .expect("media process")
                .child
                .as_mut()
                .ok_or("Video decoder is unavailable")?
                .try_wait()
                .map_err(|error| error.to_string())?;
            if let Some(status) = status {
                self.process.lock().expect("media process").child = None;
                self.deadline(false);
                return if status.success() {
                    Ok(())
                } else {
                    Err(
                        "Video decoding failed; use a self-contained, streamable MP4/WebM/AVI file"
                            .into(),
                    )
                };
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn read_source(
    uri: &str,
    assets: &Assets,
    access: &Access,
    web: bool,
    user_agent: Option<&str>,
    control: &Control,
) -> Result<(Vec<u8>, String), String> {
    control.check()?;
    let maximum = if web { MAX_DOCUMENT } else { MAX_MEDIA };
    let allowed = |url: &str| {
        if web {
            access.allows_webview(url)
        } else {
            access.allows_media(url)
        }
    };
    if uri.starts_with("asset:/") {
        if web && !access.allows_webviews() {
            return Err("WebView access denied".into());
        }
        let bytes = assets
            .as_ref()
            .ok_or("This app has no asset reader")?
            .read(uri)?;
        control.check()?;
        if bytes.len() > maximum {
            return Err("Media source exceeds its byte limit".into());
        }
        return Ok((bytes, uri.into()));
    }
    if !allowed(uri) {
        return Err("Media source is not granted by capabilities".into());
    }
    if !web && !uri.starts_with("http://") && !uri.starts_with("https://") {
        let bytes = crate::image::read_bytes(uri, None, access)?;
        control.check()?;
        return Ok((bytes, uri.into()));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(IO_TIMEOUT))
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .build()
        .into();
    let mut url = uri.to_owned();
    let began = Instant::now();
    for _ in 0..=10 {
        control.check()?;
        if began.elapsed() >= IO_TIMEOUT {
            return Err("Media download timed out".into());
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) || !allowed(&url) {
            return Err("Media navigation target is not granted by capabilities".into());
        }
        let mut request = agent
            .get(&url)
            .config()
            .timeout_global(Some(IO_TIMEOUT.saturating_sub(began.elapsed())))
            .build();
        if let Some(user_agent) = user_agent {
            request = request.header("User-Agent", user_agent);
        }
        let mut response = request
            .call()
            .map_err(|error| format!("Media download: {error}"))?;
        let status = response.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or("Media redirect has no Location")?;
            url = url::Url::parse(&url)
                .and_then(|base| base.join(location))
                .map_err(|error| error.to_string())?
                .into();
            continue;
        }
        if status >= 400 {
            return Err(format!("Media download returned HTTP {status}"));
        }
        if response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|size| size > maximum)
        {
            return Err("Media source exceeds its byte limit".into());
        }
        let mut bytes = Vec::new();
        let mut reader = response.body_mut().as_reader();
        let mut chunk = [0; 16 * 1024];
        loop {
            control.check()?;
            if began.elapsed() >= IO_TIMEOUT {
                return Err("Media download timed out".into());
            }
            let count = reader.read(&mut chunk).map_err(|error| error.to_string())?;
            if count == 0 {
                break;
            }
            if bytes.len().saturating_add(count) > maximum {
                return Err("Media source exceeds its byte limit".into());
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
        return Ok((bytes, url));
    }
    Err("Media download exceeded 10 redirects".into())
}

fn executable(name: &str) -> std::ffi::OsString {
    std::env::var_os(if name == "ffmpeg" {
        "NSH_FFMPEG"
    } else {
        "NSH_FFPROBE"
    })
    .unwrap_or_else(|| name.into())
}

fn start_process(
    command: &mut ProcessCommand,
    bytes: Arc<Vec<u8>>,
    control: &Arc<Control>,
) -> Result<std::process::ChildStdout, String> {
    control.check()?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!("Video needs ffmpeg and ffprobe on PATH (or NSH_FFMPEG / NSH_FFPROBE): {error}")
        })?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or("Video decoder has no input pipe")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("Video decoder has no output pipe")?;
    {
        let mut state = control.process.lock().expect("media process");
        state.child = Some(child);
        state.deadline = Some(Instant::now() + IO_TIMEOUT);
    }
    if control.cancelled() {
        control.cancel();
        return Err("Media load cancelled".into());
    }
    let writer_control = control.clone();
    std::thread::Builder::new()
        .name("nsh-video-input".into())
        .spawn(move || {
            for chunk in bytes.chunks(16 * 1024) {
                if writer_control.cancelled() || stdin.write_all(chunk).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| error.to_string())?;
    let watchdog_control = control.clone();
    std::thread::Builder::new()
        .name("nsh-video-watchdog".into())
        .spawn(move || {
            let mut state = watchdog_control.process.lock().expect("media process");
            while state.child.is_some() && !watchdog_control.cancelled() {
                if state
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    watchdog_control.timed_out.store(true, Ordering::Release);
                    watchdog_control.cancelled.store(true, Ordering::Release);
                    if let Some(child) = &mut state.child {
                        let _ = child.kill();
                    }
                    break;
                }
                state = watchdog_control
                    .wake
                    .wait_timeout(state, Duration::from_millis(100))
                    .expect("media watchdog")
                    .0;
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(stdout)
}

const INPUT_FORMATS: &str = "mov,matroska,webm,avi,ogg,mpegts,mpegvideo,flv,gif,apng";

fn decode_video(
    bytes: Arc<Vec<u8>>,
    seek: f64,
    control: &Arc<Control>,
    tx: &Sender<VideoMessage>,
) -> Result<(), String> {
    let mut probe = ProcessCommand::new(executable("ffprobe"));
    probe.args([
        "-v",
        "error",
        "-threads",
        "2",
        "-protocol_whitelist",
        "pipe",
        "-format_whitelist",
        INPUT_FORMATS,
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height,duration:format=duration",
        "-of",
        "json",
        "-i",
        "pipe:0",
    ]);
    let stdout = start_process(&mut probe, bytes.clone(), control)?;
    let mut metadata = Vec::new();
    stdout
        .take(64 * 1024 + 1)
        .read_to_end(&mut metadata)
        .map_err(|error| error.to_string())?;
    if metadata.len() > 64 * 1024 {
        control.cancel();
        return Err("Video metadata exceeds 64 KiB".into());
    }
    control.finish()?;
    let metadata: serde_json::Value =
        serde_json::from_slice(&metadata).map_err(|error| format!("Video metadata: {error}"))?;
    let stream = metadata["streams"]
        .as_array()
        .and_then(|streams| streams.first())
        .ok_or("Video contains no picture stream")?;
    let width = stream["width"].as_u64().ok_or("Video has no width")?;
    let height = stream["height"].as_u64().ok_or("Video has no height")?;
    if width == 0 || height == 0 || width > 8192 || height > 8192 || width * height > 16 << 20 {
        return Err("Video dimensions exceed 8192 pixels or 16 million pixels".into());
    }
    let duration = stream["duration"]
        .as_str()
        .or_else(|| metadata["format"]["duration"].as_str())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or(0.0);
    if duration > 4.0 * 60.0 * 60.0 {
        return Err("Video duration exceeds four hours".into());
    }
    let info = VideoInfo {
        width: width as u32,
        height: height as u32,
        duration,
    };
    if tx.send(VideoMessage::Info(info)).is_err() {
        return Ok(());
    }
    let scale = (512.0 / width as f64).min(288.0 / height as f64).min(1.0);
    let out_width = ((width as f64 * scale) as u32).max(1);
    let out_height = ((height as f64 * scale) as u32).max(1);
    let mut decoder = ProcessCommand::new(executable("ffmpeg"));
    decoder.args([
        "-v",
        "error",
        "-nostdin",
        "-threads",
        "2",
        "-filter_threads",
        "1",
        "-filter_complex_threads",
        "1",
        "-max_alloc",
        "67108864",
        "-protocol_whitelist",
        "pipe",
        "-format_whitelist",
        INPUT_FORMATS,
        "-i",
        "pipe:0",
        "-ss",
        &seek.to_string(),
        "-an",
        "-sn",
        "-dn",
        "-vf",
        &format!("scale={out_width}:{out_height},fps={FPS}"),
        "-pix_fmt",
        "rgba",
        "-threads",
        "2",
        "-f",
        "rawvideo",
        "pipe:1",
    ]);
    let mut stdout = start_process(&mut decoder, bytes, control)?;
    let frame_bytes = out_width as usize * out_height as usize * 4;
    let mut index = 0usize;
    loop {
        control.check()?;
        control.deadline(true);
        let mut rgba = vec![0; frame_bytes];
        let mut filled = 0;
        while filled < frame_bytes {
            let count = stdout
                .read(&mut rgba[filled..])
                .map_err(|error| error.to_string())?;
            if count == 0 {
                break;
            }
            filled += count;
        }
        control.deadline(false);
        if filled == 0 {
            break;
        }
        if filled != frame_bytes {
            return Err("Video decoder returned an incomplete picture".into());
        }
        if index > (4.0 * 60.0 * 60.0 * FPS) as usize {
            return Err("Video frame count exceeds four hours".into());
        }
        let frame = Arc::new(Raster {
            width: out_width,
            height: out_height,
            rgba,
        });
        if tx
            .send(VideoMessage::Frame(seek + index as f64 / FPS, frame))
            .is_err()
        {
            control.cancel();
            return Ok(());
        }
        index += 1;
    }
    control.finish()?;
    if index == 0 && seek == 0.0 {
        return Err(
            "Video contains no decodable pictures; use a streamable MP4/WebM/AVI file".into(),
        );
    }
    let _ = tx.send(VideoMessage::Done);
    Ok(())
}

fn source_url(source: &WebSource) -> String {
    match source {
        WebSource::None => String::new(),
        WebSource::Uri(uri) => uri.to_string(),
        WebSource::Html { base_url, .. } => base_url.as_deref().unwrap_or("about:blank").into(),
    }
}

fn error(message: &str) -> (EventKind, EventPayload) {
    (
        EventKind::Error,
        EventPayload::Error {
            message: message.into(),
        },
    )
}

fn buffer(buffering: bool) -> (EventKind, EventPayload) {
    (
        EventKind::Buffer,
        EventPayload::Map(vec![("isBuffering", PayloadValue::Bool(buffering))]),
    )
}

fn timestamp(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn truncate(text: &str, columns: usize) -> String {
    let mut output = String::new();
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let advance = grapheme.width();
        if width + advance > columns {
            break;
        }
        output.push_str(grapheme);
        width += advance;
    }
    output
}

fn wrap(text: &str, columns: usize) -> Vec<String> {
    if columns == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut current = String::new();
        let mut width = 0;
        for grapheme in line.graphemes(true) {
            if grapheme == "\t" {
                for _ in 0..(4 - width % 4) {
                    if width == columns {
                        lines.push(std::mem::take(&mut current));
                        width = 0;
                    }
                    current.push(' ');
                    width += 1;
                }
                continue;
            }
            let advance = grapheme.width();
            if width + advance > columns && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                width = 0;
            }
            if advance <= columns {
                current.push_str(grapheme);
                width += advance;
            }
        }
        lines.push(current);
    }
    lines
}

fn parse_html(html: &str, base: &str) -> Result<Document, String> {
    if html.len() > MAX_DOCUMENT {
        return Err("HTML exceeds 8 MiB".into());
    }
    let mut document = Document {
        url: base.into(),
        ..Document::default()
    };
    let mut cursor = 0;
    let mut hidden: Option<String> = None;
    let mut title = false;
    let mut pre = false;
    let mut link: Option<(String, usize)> = None;
    while cursor < html.len() {
        let rest = &html[cursor..];
        if rest.starts_with("<!--") {
            cursor += rest.find("-->").map_or(rest.len(), |end| end + 3);
            continue;
        }
        if rest.starts_with('<') {
            let Some(end) = tag_end(rest) else { break };
            let tag = rest[1..end].trim();
            let closing = tag.starts_with('/');
            let contents = tag.trim_start_matches('/').trim_start();
            let name_end = contents
                .find(|c: char| c.is_ascii_whitespace() || c == '/')
                .unwrap_or(contents.len());
            let name = contents[..name_end].to_ascii_lowercase();
            let attributes = &contents[name_end..];
            cursor += end + 1;
            if let Some(hiding) = &hidden {
                if closing && *hiding == name {
                    hidden = None;
                }
                continue;
            }
            match name.as_str() {
                "script" | "style" | "noscript" | "template" => {
                    if !closing {
                        hidden = Some(name);
                    }
                }
                "title" => title = !closing,
                "pre" => {
                    pre = !closing;
                    newline(&mut document.text, 2);
                }
                "br" => newline(&mut document.text, 1),
                "hr" => {
                    newline(&mut document.text, 1);
                    document.text.push_str("────────────────\n");
                }
                "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "h1"
                | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" => newline(&mut document.text, 2),
                "ul" | "ol" | "tr" | "table" => newline(&mut document.text, 1),
                "li" => {
                    newline(&mut document.text, 1);
                    if !closing {
                        document.text.push_str("• ");
                    }
                }
                "td" | "th" if closing => document.text.push_str(" | "),
                "img" if !closing => {
                    if let Some(alt) = attribute(attributes, "alt") {
                        document.text.push_str(&format!("[{alt}]"));
                    }
                }
                "a" if !closing => {
                    link = attribute(attributes, "href")
                        .and_then(|href| resolve_link(base, &href))
                        .map(|url| (url, document.text.len()));
                }
                "a" if closing => {
                    if let Some((url, start)) = link.take()
                        && document.links.len() < MAX_LINKS
                    {
                        let label = document
                            .text
                            .get(start..)
                            .unwrap_or_default()
                            .trim()
                            .to_owned();
                        let label = if label.is_empty() { url.clone() } else { label };
                        document.links.push((safe_text(&label), url));
                        document
                            .text
                            .push_str(&format!(" [{}]", document.links.len()));
                    }
                }
                _ => {}
            }
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            let text = decode_entities(&rest[..end]);
            if hidden.is_none() {
                if title {
                    document.title.push_str(&text);
                } else if pre {
                    document.text.push_str(&safe_text(&text));
                } else {
                    append_collapsed(&mut document.text, &safe_text(&text));
                }
            }
            cursor += end;
        }
        if document.text.len() > MAX_TEXT {
            return Err("Readable HTML text exceeds 2 MiB".into());
        }
    }
    document.title = safe_text(document.title.trim());
    document.text = safe_text(document.text.trim());
    Ok(document)
}

fn safe_text(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control() || matches!(*character, '\n' | '\t'))
        .collect()
}

fn newline(text: &mut String, count: usize) {
    while text.ends_with(' ') {
        text.pop();
    }
    let existing = text
        .chars()
        .rev()
        .take_while(|character| *character == '\n')
        .count();
    if !text.is_empty() {
        for _ in existing..count {
            text.push('\n');
        }
    }
}

fn append_collapsed(output: &mut String, text: &str) {
    for character in text.chars() {
        if character.is_whitespace() {
            if !output.is_empty() && !output.ends_with([' ', '\n']) {
                output.push(' ');
            }
        } else {
            output.push(character);
        }
    }
}

fn tag_end(text: &str) -> Option<usize> {
    let mut quote = None;
    for (index, character) in text.char_indices().skip(1) {
        match (quote, character) {
            (Some(expected), actual) if actual == expected => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None, '>') => return Some(index),
            _ => {}
        }
    }
    None
}

fn attribute(mut text: &str, wanted: &str) -> Option<String> {
    while !text.is_empty() {
        text = text.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '/');
        if text.is_empty() {
            break;
        }
        let end = text
            .find(|c: char| c.is_ascii_whitespace() || c == '=' || c == '/')
            .unwrap_or(text.len());
        let name = &text[..end];
        text = text[end..].trim_start();
        if !text.starts_with('=') {
            if name.is_empty() {
                text = &text[1..];
            }
            continue;
        }
        text = text[1..].trim_start();
        let value;
        if text.starts_with(['\'', '"']) {
            let quote = text.as_bytes()[0] as char;
            text = &text[1..];
            let end = text.find(quote).unwrap_or(text.len());
            value = &text[..end];
            text = text.get(end + 1..).unwrap_or_default();
        } else {
            let end = text.find(char::is_whitespace).unwrap_or(text.len());
            value = &text[..end];
            text = &text[end..];
        }
        if name.eq_ignore_ascii_case(wanted) {
            return Some(decode_entities(value));
        }
    }
    None
}

fn resolve_link(base: &str, href: &str) -> Option<String> {
    let url = url::Url::parse(base)
        .and_then(|base| base.join(href))
        .or_else(|_| url::Url::parse(href))
        .ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.into())
}

fn decode_entities(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest
            .as_bytes()
            .iter()
            .take(17)
            .position(|byte| *byte == b';')
        else {
            output.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let replacement = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            "ndash" => Some('–'),
            "mdash" => Some('—'),
            "hellip" => Some('…'),
            "copy" => Some('©'),
            "reg" => Some('®'),
            "bull" => Some('•'),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|value| u32::from_str_radix(value, 16).ok())
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|value| value.parse().ok())
                })
                .and_then(char::from_u32),
        };
        if let Some(character) = replacement {
            output.push(character);
        } else {
            output.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    fn await_web(state: &mut MediaState) {
        let began = Instant::now();
        while state.active() && began.elapsed() < Duration::from_secs(3) {
            state.tick(0.0);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!state.active(), "document did not finish");
    }

    #[test]
    fn reads_html_structure_entities_and_safe_links() {
        let document = parse_html(
            "<title>Ops &amp; fleet</title><h1>東京</h1><p>One <b>healthy</b> service.</p><script>secret()</script><style>bad</style><ul><li>Ready</li></ul><a href='../status?q=1&amp;v=2'>Status</a><a href='javascript:alert(1)'>Blocked</a><pre>a  b\nc</pre><img alt='unsafe&#27;[31m'/>",
            "https://example.com/docs/page",
        ).unwrap();
        assert_eq!(document.title, "Ops & fleet");
        assert!(document.text.contains("One healthy service."));
        assert!(document.text.contains("• Ready"));
        assert!(document.text.contains("a  b\nc"));
        assert!(!document.text.contains("secret"));
        assert!(!document.text.contains("bad"));
        assert!(!document.text.contains('\u{1b}'));
        assert_eq!(
            document.links,
            vec![("Status".into(), "https://example.com/status?q=1&v=2".into())]
        );
    }

    #[test]
    fn inline_document_emits_navigation_and_load_without_scripts() {
        let props = WebProps {
            source: WebSource::Html {
                html: "<title>Overview</title><p>Healthy 王宁</p>".into(),
                base_url: Some("https://example.com/".into()),
            },
            ..WebProps::default()
        };
        let mut state = MediaState::new(
            NativeProps::WebView(Box::new(props)),
            None,
            Access::all(),
            0.0,
            MediaRuntime::default(),
        );
        await_web(&mut state);
        let events = state.drain_events();
        assert_eq!(
            events
                .iter()
                .filter(|(kind, _)| *kind == EventKind::LoadStart)
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|(kind, _)| *kind == EventKind::Load)
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|(kind, _)| *kind == EventKind::LoadEnd)
                .count(),
            1
        );
        assert!(state.lines(40, 5).join("\n").contains("Healthy 王宁"));
        assert!(state.command(&Command::InjectJavaScript("alert(1)".into()), 0.0));
        assert!(
            state
                .drain_events()
                .iter()
                .any(|(kind, _)| *kind == EventKind::Error)
        );
    }

    #[test]
    fn denied_inline_document_never_exposes_content() {
        let access = Access {
            declared: nsh_core::access::Capabilities::from_json(&serde_json::json!({})).unwrap(),
            granted: None,
        };
        let props = WebProps {
            source: WebSource::Html {
                html: "PRIVATE".into(),
                base_url: None,
            },
            ..WebProps::default()
        };
        let mut state = MediaState::new(
            NativeProps::WebView(Box::new(props)),
            None,
            access,
            0.0,
            MediaRuntime::default(),
        );
        await_web(&mut state);
        assert!(!state.lines(40, 5).join("\n").contains("PRIVATE"));
        assert!(
            state
                .drain_events()
                .iter()
                .any(|(kind, _)| *kind == EventKind::Error)
        );
    }

    #[test]
    fn wrapping_preserves_graphemes_and_never_exceeds_columns() {
        let lines = wrap("王宁 e\u{301} 🙂 text", 4);
        assert!(lines.iter().all(|line| line.width() <= 4));
        assert_eq!(lines.concat(), "王宁 e\u{301} 🙂 text");
        assert_eq!(truncate("王宁🙂", 3), "王");
        assert_eq!(wrap("a\tb", 8), ["a   b"]);
    }

    #[test]
    fn controls_preserve_clock_on_pause_rate_and_seek() {
        let mut state = VideoState {
            frame: Some(Arc::new(Raster {
                width: 1,
                height: 1,
                rgba: vec![0; 4],
            })),
            finished: false,
            clock: 10.0,
            origin: 3.0,
            position: 3.0,
            ..VideoState::default()
        };
        assert_eq!(state.clock_position(12.0), 5.0);
        let props = VideoProps {
            paused: true,
            ..VideoProps::default()
        };
        state.set(props.clone(), None, Access::all(), 12.0);
        assert_eq!(state.clock_position(100.0), 5.0);
        state.set(
            VideoProps {
                paused: false,
                rate: 2.0,
                ..props
            },
            None,
            Access::all(),
            100.0,
        );
        assert_eq!(state.clock_position(102.0), 9.0);
        state.seek(f64::NAN, 102.0);
        assert_eq!(state.origin, 5.0);
    }

    #[test]
    fn capability_revoke_discards_queued_video_pixels_before_next_tick() {
        let frame = Arc::new(Raster {
            width: 1,
            height: 1,
            rgba: vec![255; 4],
        });
        let video = VideoState {
            props: VideoProps {
                source: Some("https://example.com/private.mp4".into()),
                ..VideoProps::default()
            },
            frame: Some(frame.clone()),
            pending: Some((10.0, frame)),
            finished: false,
            ..VideoState::default()
        };
        let mut state = MediaState::Video(Box::new(video));
        state.set_access(Access {
            declared: nsh_core::access::Capabilities::from_json(&serde_json::json!({})).unwrap(),
            granted: None,
        });
        state.tick(0.0);
        assert!(state.raster().is_none());
        assert!(
            matches!(&state, MediaState::Video(video) if video.pending.is_none() && !video.waiting)
        );
        assert!(
            state
                .drain_events()
                .iter()
                .any(|(kind, _)| *kind == EventKind::Error)
        );
    }

    #[test]
    fn retired_loader_holds_runtime_permit_until_io_really_finishes() {
        let runtime = MediaRuntime::default();
        let permits: Vec<_> = (0..7).map(|_| runtime.reserve().unwrap()).collect();
        let (began_tx, began_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        let job = LoadJob::spawn("nsh-media-limit-test", &runtime, move |_| {
            began_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        });
        began_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        drop(job);
        let refused = LoadJob::<()>::spawn("nsh-media-refused-test", &runtime, |_| {
            panic!("over-limit loader started")
        });
        assert!(
            refused
                .rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .is_err()
        );
        assert_eq!(runtime.active.load(Ordering::Acquire), 8);
        release_tx.send(()).unwrap();
        let began = Instant::now();
        while runtime.active.load(Ordering::Acquire) != 7
            && began.elapsed() < Duration::from_secs(3)
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(runtime.active.load(Ordering::Acquire), 7);
        drop(permits);
        assert_eq!(runtime.active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn web_navigation_history_uses_webview_grants_and_rechecks_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut paths = Vec::new();
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = [0; 4096];
                let count = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..count]);
                let path = request.split_whitespace().nth(1).unwrap().to_owned();
                paths.push(path.clone());
                let body = if path == "/start" {
                    "<title>Start</title><p>First page</p><a href='/next'>Next</a>"
                } else {
                    "<title>Next</title><p>Second page</p>"
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            paths
        });
        let access = Access {
            declared: nsh_core::access::Capabilities::from_json(
                &serde_json::json!({"webview":{"hosts":["127.0.0.1"]}}),
            )
            .unwrap(),
            granted: None,
        };
        assert!(!access.allows_media(&format!("http://{address}/start")));
        let props = WebProps {
            source: WebSource::Uri(format!("http://{address}/start").into()),
            ..WebProps::default()
        };
        let mut state = MediaState::new(
            NativeProps::WebView(Box::new(props)),
            None,
            access,
            0.0,
            MediaRuntime::default(),
        );
        await_web(&mut state);
        assert!(state.lines(80, 10).join("\n").contains("First page"));
        assert!(state.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 0.0));
        assert!(state.lines(80, 10).join("\n").contains("› [1]"));
        assert!(state.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 0.0));
        await_web(&mut state);
        assert!(state.lines(80, 10).join("\n").contains("Second page"));
        assert!(state.command(&Command::GoBack, 0.0));
        await_web(&mut state);
        assert!(state.lines(80, 10).join("\n").contains("First page"));
        assert_eq!(server.join().unwrap(), ["/start", "/next", "/start"]);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let count = stream.read(&mut request).unwrap();
            assert!(
                count > 0,
                "redirect request ended before its headers arrived"
            );
            write!(stream, "HTTP/1.1 302 Found\r\nLocation: http://localhost:{}/blocked\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", address.port()).unwrap();
            drop(stream);
            listener
        });
        let access = Access {
            declared: nsh_core::access::Capabilities::from_json(
                &serde_json::json!({"webview":{"hosts":["127.0.0.1"]}}),
            )
            .unwrap(),
            granted: None,
        };
        let props = WebProps {
            source: WebSource::Uri(format!("http://{address}/redirect").into()),
            ..WebProps::default()
        };
        let mut state = MediaState::new(
            NativeProps::WebView(Box::new(props)),
            None,
            access,
            0.0,
            MediaRuntime::default(),
        );
        await_web(&mut state);
        assert!(
            state
                .drain_events()
                .iter()
                .any(|(kind, _)| *kind == EventKind::Error)
        );
        let listener = server.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn document_scroll_end_then_up_moves_one_visible_row_and_tab_can_leave() {
        let props = WebProps {
            source: WebSource::Html {
                html: format!(
                    "<pre>{}</pre><a href='https://example.com/'>One</a>",
                    (0..20)
                        .map(|line| format!("row {line}\n"))
                        .collect::<String>()
                )
                .into(),
                base_url: None,
            },
            ..WebProps::default()
        };
        let mut state = MediaState::new(
            NativeProps::WebView(Box::new(props)),
            None,
            Access::all(),
            0.0,
            MediaRuntime::default(),
        );
        await_web(&mut state);
        state.lines(80, 4);
        assert!(state.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), 0.0));
        let at_end = state.lines(80, 4);
        assert!(state.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), 0.0));
        let above = state.lines(80, 4);
        assert_eq!(above[1..], at_end[..3]);
        assert!(state.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 0.0));
        assert!(!state.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 0.0));
    }

    #[cfg(unix)]
    #[test]
    fn dropping_video_process_kills_and_reaps_the_child() {
        let control = Arc::new(Control::default());
        let reaper = ProcessReaper(control.clone());
        let mut command = ProcessCommand::new("/bin/sleep");
        command.arg("30");
        let _stdout = start_process(&mut command, Arc::new(Vec::new()), &control).unwrap();
        let pid = control.process.lock().unwrap().child.as_ref().unwrap().id();
        control.cancel();
        drop(reaper);
        // SAFETY: signal zero checks whether the known child process still exists.
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
        assert!(control.process.lock().unwrap().child.is_none());
    }

    #[test]
    #[ignore = "requires ffmpeg and ffprobe executables"]
    fn ffmpeg_decodes_frames_seeks_and_finishes_playback() {
        let output = ProcessCommand::new(executable("ffmpeg"))
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x40:rate=12",
                "-t",
                "1",
                "-an",
                "-c:v",
                "mpeg4",
                "-f",
                "matroska",
                "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        struct Movie(Vec<u8>);
        impl nsh_engine::Assets for Movie {
            fn read(&self, _: &str) -> Result<Vec<u8>, String> {
                Ok(self.0.clone())
            }
        }
        let props = VideoProps {
            source: Some("asset:/sample.mkv".into()),
            controls: true,
            ..VideoProps::default()
        };
        let mut state = MediaState::new(
            NativeProps::Video(Box::new(props)),
            Some(Arc::new(Movie(output.stdout))),
            Access::all(),
            0.0,
            MediaRuntime::default(),
        );
        let began = Instant::now();
        let mut now = 0.0;
        let mut loaded = false;
        let mut ended = false;
        while began.elapsed() < Duration::from_secs(10) {
            state.tick(now);
            for (kind, payload) in state.drain_events() {
                assert_ne!(kind, EventKind::Error, "{payload:?}");
                loaded |= kind == EventKind::Load;
                ended |= kind == EventKind::End;
            }
            if state.raster().is_some() {
                now += 1.0 / FPS;
            }
            if ended {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(loaded && ended, "video did not load and finish");
        let raster = state.raster().unwrap();
        assert_eq!((raster.width, raster.height), (64, 40));
        assert!(
            raster
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[0] != pixel[1])
        );
        assert!(state.command(&Command::Seek(0.5), now));
        let began = Instant::now();
        let mut sought = false;
        while began.elapsed() < Duration::from_secs(10) {
            state.tick(now);
            if let MediaState::Video(video) = &state
                && !video.waiting
                && video.last_frame >= 0.5
            {
                sought = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            sought,
            "seek did not produce a picture at the requested time"
        );
        assert!(state.command(&Command::Seek(100.0), now));
        let began = Instant::now();
        let mut final_picture = false;
        while began.elapsed() < Duration::from_secs(10) {
            state.tick(now);
            if let MediaState::Video(video) = &state
                && !video.waiting
                && video.last_frame >= 1.0 - 1.0 / FPS
            {
                final_picture = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            final_picture,
            "seeking past the end discarded the final picture"
        );
    }
}
