//! Embeddable engines and the standalone terminal runner. Terminal IO is
//! exclusively owned by `run_app`; an `AppView` can render in a host's buffer.

#[cfg(feature = "dev")]
use std::{collections::VecDeque, sync::Mutex};
use std::{
    io::{self, IsTerminal, Write},
    sync::{Arc, RwLock, mpsc},
    time::{Duration, Instant},
};

use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen, SetTitle},
};
use nsh_core::{
    MountBatch,
    platform::{Appearance, Locale, Renderer},
};
use nsh_engine::{
    App, Bundle, ColorScheme, Engine, EngineConfig, EngineControl, Environment, Idiom, Limits,
    MountSink, Os, Root,
};
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::{Clear, Paragraph, Widget, Wrap},
};

pub use crate::image::AssetBase;
use crate::{CellMetrics, Surface, SurfaceEvent, TerminalTextEngine, image::AssetReader};

static UI_SETS: RwLock<Option<Vec<nsh_core::ui::Registry>>> = RwLock::new(None);

/// Sets the UI sets served by subsequently loaded apps. The default is
/// `nsh/ui/mobile`; app-specific hosts call this before creating views.
pub fn set_ui_sets(sets: Vec<nsh_core::ui::Registry>) {
    *UI_SETS.write().unwrap_or_else(|e| e.into_inner()) = Some(sets);
}

fn ui_sets() -> Vec<nsh_core::ui::Registry> {
    UI_SETS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| vec![nsh_ui_mobile::registry()])
}

/// The app loaded by [`run_app`].
#[derive(Debug)]
pub enum AppSource {
    /// A native Rust root component.
    Rust(Root),
    /// A JavaScript bundle from trusted app resources or source code.
    Bundle(Bundle),
    /// A development server that pushes full bundles and hot updates.
    #[cfg(feature = "dev")]
    DevServer(String),
}

/// Configuration shared by an embedded [`AppView`] and [`run_app`].
#[derive(Debug, Clone)]
pub struct AppOptions {
    /// Log engine commit timings through the `log` facade.
    pub trace: bool,
    /// Location of bundled `asset:/` resources.
    pub asset_base: Option<AssetBase>,
    /// The application's reverse-domain identifier.
    pub app_id: String,
    /// The standalone terminal's title.
    pub title: String,
    /// Initial width and height in terminal columns and rows. Interactive
    /// runs use the terminal's actual size; headless scripts use this size.
    pub size: (u16, u16),
    /// Headless verification steps, for example
    /// `wait 100; key Tab; type Hello; shot screen.txt; quit`.
    pub script: Option<String>,
    /// Initial appearance, before an app selects its own.
    pub appearance: Appearance,
    /// Resource limits of a JavaScript app.
    pub limits: Limits,
    /// Virtual point dimensions of one terminal cell.
    pub metrics: CellMetrics,
    /// Native terminal components available through `nsh/native`, scoped
    /// to this view and retained across app reloads.
    pub components: crate::native::Components,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self {
            trace: false,
            asset_base: None,
            app_id: "com.longbridge.nsh.host".into(),
            title: "Native Shell".into(),
            size: (120, 40),
            script: None,
            appearance: Appearance::System,
            limits: Limits::default(),
            metrics: CellMetrics::default(),
            components: Default::default(),
        }
    }
}

enum EngineEvent {
    Batch(MountBatch),
    Error(String),
    Frame,
    Reload,
}

struct Sink(mpsc::Sender<EngineEvent>);

impl MountSink for Sink {
    fn submit(&self, batch: MountBatch) {
        let _ = self.0.send(EngineEvent::Batch(batch));
    }
    fn report_error(&self, message: &str) {
        let _ = self.0.send(EngineEvent::Error(message.into()));
    }
    fn request_frame(&self) {
        let _ = self.0.send(EngineEvent::Frame);
    }
    fn hot_update_failed(&self) {
        let _ = self.0.send(EngineEvent::Reload);
    }
}

/// One independently running NSH app, rendered into a Ratatui buffer.
///
/// The embedding host owns its terminal, chooses the area to render and
/// routes only that area's events to the view. Call [`Self::poll`] each
/// host frame and [`Self::tick`] on animation frames. Dropping the view
/// stops and joins its engine; neither construction nor drop changes the
/// terminal mode.
pub struct AppView {
    surface: Surface,
    options: AppOptions,
    environment: Environment,
    engine: Option<Engine>,
    receiver: Option<mpsc::Receiver<EngineEvent>>,
    network: Arc<dyn nsh_engine::Network>,
    assets: Arc<AssetsProvider>,
    frame_requested: bool,
    has_frame: bool,
    last_error: Option<String>,
    status: Option<String>,
    theme: Option<nsh_core::ui::Theme>,
    image_protocol: Option<crate::ImageProtocol>,
    #[cfg(feature = "dev")]
    dev: Option<DevSession>,
}

impl AppView {
    /// Creates an empty view at `options.size`, without taking terminal IO.
    pub fn new(options: AppOptions) -> Self {
        let mut surface = Surface::new(options.size.0, options.size.1, options.metrics);
        let environment = initial_environment(&options);
        surface.set_dark(environment.color_scheme == ColorScheme::Dark);
        surface.set_components(options.components.clone());
        let assets = Arc::new(AssetsProvider(RwLock::new(options.asset_base.clone())));
        Self {
            surface,
            environment,
            options,
            engine: None,
            receiver: None,
            network: crate::net::network(),
            assets,
            frame_requested: false,
            has_frame: false,
            last_error: None,
            status: None,
            theme: None,
            image_protocol: None,
            #[cfg(feature = "dev")]
            dev: None,
        }
    }

    /// Runs `app`, replacing the current engine and retained views.
    pub fn load(&mut self, app: App) -> io::Result<()> {
        self.stop_engine();
        let mut surface = Surface::new(
            self.options.size.0,
            self.options.size.1,
            self.options.metrics,
        );
        surface.inherit_image_protocol(&mut self.surface);
        self.surface = surface;
        self.surface
            .set_dark(self.environment.color_scheme == ColorScheme::Dark);
        self.surface.set_components(self.options.components.clone());
        self.surface.set_image_protocol(self.image_protocol);
        let access = match &app {
            App::Js(bundle) => bundle.policy.access(),
            App::Rust(_) => nsh_engine::Policy::default().access(),
        };
        let assets: Arc<dyn nsh_engine::Assets> = self.assets.clone();
        self.surface.set_assets(Some(assets.clone()));
        self.surface.set_access(access);
        let config = EngineConfig {
            app,
            viewport: self.surface.viewport(),
            trace: self.options.trace,
            network: Some(self.network.clone()),
            assets: Some(assets),
            environment: self.environment,
            header_heights: (
                self.options.metrics.height(),
                self.options.metrics.height() * 2.0,
            ),
            limits: self.options.limits,
            ui_sets: ui_sets(),
            runtime: None,
        };
        let (tx, rx) = mpsc::channel();
        let metrics = self.options.metrics;
        let theme = self.theme.clone();
        self.engine = Some(Engine::start_with(
            config,
            move || TerminalTextEngine::new(metrics),
            Sink(tx),
            move |control| {
                if let Some(theme) = theme {
                    control.set_theme(Some(theme));
                }
            },
        )?);
        self.receiver = Some(rx);
        self.last_error = None;
        self.has_frame = false;
        self.frame_requested = false;
        Ok(())
    }

    /// Starts a reconnecting dev-server session. Bundles and hot updates
    /// are applied by [`Self::poll`]. Ctrl+R reloads the latest full build.
    #[cfg(feature = "dev")]
    pub fn connect(&mut self, url: String) -> io::Result<()> {
        use nsh_dev::{DevClient, HostInfo, protocol::HostPlatform};
        self.stop();
        self.last_error = None;
        self.has_frame = false;
        let base = dev_asset_base(&url).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "a dev server URL must start with ws:// or wss:// and name a host",
            )
        })?;
        self.set_asset_base(Some(AssetBase::Http(base)));
        let host = HostInfo {
            device: "Terminal".into(),
            platform: HostPlatform::Tui,
            bytecode_version: nsh_engine::BYTECODE_VERSION.into(),
            ui_sets: ui_sets()
                .iter()
                .filter_map(|set| set.specifier().map(str::to_owned))
                .collect(),
        };
        let (tx, rx) = mpsc::channel();
        let client = DevClient::start(url.clone(), host, move |event| {
            let _ = tx.send(event);
        })?;
        self.dev = Some(DevSession {
            client,
            receiver: rx,
            running: None,
            latest: None,
        });
        self.status = Some(format!("Connecting to {url}"));
        Ok(())
    }

    /// Applies queued engine batches and development updates without
    /// waiting for the engine. Returns whether the display changed.
    pub fn poll(&mut self) -> io::Result<bool> {
        let mut changed = false;
        #[cfg(feature = "dev")]
        {
            let events: Vec<_> = self
                .dev
                .as_ref()
                .map(|dev| dev.receiver.try_iter().collect())
                .unwrap_or_default();
            for event in events {
                self.dev_event(event)?;
                changed = true;
            }
        }
        let events: Vec<_> = self
            .receiver
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        for event in events {
            match event {
                EngineEvent::Batch(batch) => {
                    self.surface.apply(batch);
                    self.has_frame = true;
                    changed = true;
                }
                EngineEvent::Error(message) => {
                    self.last_error = Some(message);
                    changed = true;
                }
                EngineEvent::Frame => self.frame_requested = true,
                EngineEvent::Reload => {
                    #[cfg(feature = "dev")]
                    self.reload_after_update()?;
                    // A reload replaced the receiver. Old-engine events
                    // collected above must never enter the new surface.
                    break;
                }
            }
        }
        self.dispatch_events();
        #[cfg(feature = "dev")]
        if let Some(dev) = &self.dev {
            for record in take_logs() {
                let record = record.terminal_safe();
                dev.client
                    .log(record.level.into(), &record.target, &record.message);
            }
        }
        Ok(changed)
    }

    /// Advances native animations at monotonic time `now` in seconds and
    /// delivers requested animation frames to the engine.
    pub fn tick(&mut self, now: f64) -> io::Result<bool> {
        let mut changed = self.poll()?;
        changed |= self.surface.tick(now);
        self.dispatch_events();
        if self.frame_requested {
            self.frame_requested = false;
            if let Some(control) = self.control() {
                control.frame();
            }
        }
        Ok(changed)
    }

    /// Routes an event in coordinates local to this view. Embedders
    /// subtract the rendered area's origin from mouse coordinates first.
    pub fn handle_event(&mut self, event: Event) -> bool {
        if let Event::Resize(columns, rows) = event {
            self.resize(columns, rows);
            return true;
        }
        #[cfg(feature = "dev")]
        if let Event::Key(key) = &event
            && key.kind != event::KeyEventKind::Release
            && key.code == event::KeyCode::Char('r')
            && key.modifiers.contains(event::KeyModifiers::CONTROL)
            && self.dev.is_some()
        {
            if let Err(error) = self.reload_latest() {
                self.last_error = Some(error.to_string());
            }
            return true;
        }
        let changed = self.surface.handle_event(event);
        self.dispatch_events();
        changed
    }

    /// Updates this view's viewport in terminal columns and rows.
    pub fn resize(&mut self, columns: u16, rows: u16) {
        self.options.size = (columns, rows);
        self.surface.resize(columns, rows);
        let size = self.surface.viewport();
        self.environment.window = (size.width, size.height);
        if let Some(control) = self.control() {
            control.resize(size);
            control.set_environment(self.environment);
        }
    }

    /// Paints the retained scene, then any development status or error,
    /// clipped to `area`; terminal flushing remains the host's concern.
    pub fn render(&self, area: Rect, buffer: &mut Buffer) {
        self.surface.render(area, buffer);
        if let Some(error) = &self.last_error {
            Clear.render(area, buffer);
            Paragraph::new(format!(
                "Native Shell\n\n{}\n\nCtrl+R reloads a development app. Ctrl+Q exits.",
                safe_diagnostic(error)
            ))
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: false })
            .render(area, buffer);
        } else if let Some(status) = &self.status {
            let line = Rect::new(
                area.x,
                area.y.saturating_add(area.height.saturating_sub(1)),
                area.width,
                u16::from(area.height > 0),
            );
            Paragraph::new(safe_diagnostic(status))
                .style(Style::default().fg(Color::Yellow).bg(Color::Black))
                .render(line, buffer);
        }
    }

    /// Plain text of the current scene, suitable for snapshot checks.
    pub fn snapshot(&self) -> String {
        self.surface.snapshot()
    }

    /// The retained scene, for a host's own integration.
    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    /// Mutably accesses the retained scene. [`Self::poll`] forwards its
    /// queued input events to the app.
    pub fn surface_mut(&mut self) -> &mut Surface {
        &mut self.surface
    }

    /// A cloneable handle to the current engine, or `None` before loading.
    pub fn control(&self) -> Option<EngineControl> {
        self.engine.as_ref().map(Engine::control)
    }

    /// The most recently reported app or build error, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Sets where subsequently loaded app resources resolve. Images and
    /// engine resources in a running app use the new source too.
    pub fn set_asset_base(&mut self, base: Option<AssetBase>) {
        self.options.asset_base = base;
        *self.assets.0.write().unwrap_or_else(|e| e.into_inner()) = self.options.asset_base.clone();
        self.surface.set_assets(Some(self.assets.clone()));
    }

    /// Sets the host appearance. An app's explicit appearance still wins.
    pub fn set_appearance(&mut self, appearance: Appearance) {
        self.options.appearance = appearance;
        self.environment.color_scheme = appearance.resolve(ColorScheme::Dark);
        self.surface
            .set_dark(self.environment.color_scheme == ColorScheme::Dark);
        if let Some(control) = self.control() {
            control.set_environment(self.environment);
        }
    }

    /// Applies a host theme now and to subsequently loaded apps. `None`
    /// restores the app's own theme.
    pub fn set_theme(&mut self, theme: Option<nsh_core::ui::Theme>) {
        self.theme = theme.clone();
        if let Some(control) = self.control() {
            control.set_theme(theme);
        }
    }

    /// Enables a graphics protocol already selected by the embedding host,
    /// using that terminal's physical cell dimensions. This performs no
    /// terminal query and persists across app reloads. `None` restores the
    /// universal half-block renderer. Standalone interactive apps detect
    /// the protocol before starting their terminal event reader.
    pub fn set_image_protocol(&mut self, protocol: Option<crate::ImageProtocol>) {
        self.image_protocol = protocol;
        self.surface.set_image_protocol(protocol);
    }

    /// Returns terminal commands that release this view's graphics
    /// resources. The embedding host writes them before releasing its
    /// terminal; this method performs no IO. The standalone runner owns
    /// this cleanup through its terminal guard.
    pub fn image_protocol_cleanup(&self) -> String {
        self.surface.image_protocol_cleanup()
    }

    /// Registers a terminal component factory for subsequent mounts in
    /// this view, including after an app reload.
    pub fn register_component(
        &mut self,
        name: &str,
        factory: impl Fn() -> Box<dyn crate::native::NativeComponent> + Send + Sync + 'static,
    ) {
        self.options.components.register(name, factory);
        self.surface.set_components(self.options.components.clone());
    }

    /// Stops the current app and disconnects development support.
    pub fn stop(&mut self) {
        #[cfg(feature = "dev")]
        {
            self.dev = None;
        }
        self.stop_engine();
    }

    fn stop_engine(&mut self) {
        self.engine = None;
        self.receiver = None;
        self.frame_requested = false;
    }

    fn dispatch_events(&mut self) {
        let events: Vec<_> = self.surface.drain_events().collect();
        let Some(control) = self.control() else {
            return;
        };
        for event in events {
            match event {
                SurfaceEvent::Event(id, kind, payload) => control.dispatch_event(id, kind, payload),
                SurfaceEvent::Scroll(id, x, y, count) => control.scroll(id, x, y, count),
                SurfaceEvent::States(id, states) => control.set_states(id, states),
                SurfaceEvent::AnimationsFinished(finished) => control.animations_finished(finished),
                SurfaceEvent::ComponentSize(id, size) => control.set_component_size(id, size),
            }
        }
    }

    pub(crate) fn has_frame(&self) -> bool {
        self.has_frame
    }
}

impl Drop for AppView {
    fn drop(&mut self) {
        // Stop callbacks and component tasks before the retained surface
        // releases its native components and graphical worker handles.
        self.stop();
    }
}

struct AssetsProvider(RwLock<Option<AssetBase>>);

impl nsh_engine::Assets for AssetsProvider {
    fn read(&self, uri: &str) -> Result<Vec<u8>, String> {
        let base = self.0.read().unwrap_or_else(|e| e.into_inner()).clone();
        nsh_engine::Assets::read(&AssetReader(base), uri)
    }
}

#[cfg(feature = "dev")]
struct DevSession {
    client: nsh_dev::DevClient,
    receiver: mpsc::Receiver<nsh_dev::DevEvent>,
    running: Option<(u64, u64)>,
    latest: Option<(u64, u64, Bundle)>,
}

#[cfg(feature = "dev")]
impl AppView {
    fn dev_event(&mut self, event: nsh_dev::DevEvent) -> io::Result<()> {
        use nsh_dev::{DevEvent, protocol::BundleFormat};
        match event {
            DevEvent::Connected => self.status = None,
            DevEvent::Disconnected => {
                self.status = Some("Dev server disconnected; reconnecting".into())
            }
            DevEvent::Building => self.status = Some("Building".into()),
            DevEvent::BuildFailed(message) | DevEvent::CannotRun { message, .. } => {
                self.status = None;
                self.last_error = Some(message);
            }
            DevEvent::Bundle {
                session,
                version,
                format,
                bytes,
                source_map,
                capabilities,
            } => {
                let bundle = match format {
                    // SAFETY: this developer-selected server negotiated
                    // the bytecode version before compiling its bundle.
                    BundleFormat::Bytecode => unsafe { Bundle::bytecode(bytes) },
                    BundleFormat::Source => {
                        Bundle::source(String::from_utf8(bytes).map_err(io::Error::other)?)
                    }
                }
                .with_policy(nsh_engine::Policy::development())
                .with_capabilities_json(capabilities.as_deref())
                .map_err(io::Error::other)?;
                let bundle = match source_map {
                    Some(map) => bundle.with_source_map(map),
                    None => bundle,
                };
                let Some(dev) = &mut self.dev else {
                    return Ok(());
                };
                let already_applied = dev.running == Some((session, version));
                dev.latest = Some((session, version, bundle.clone()));
                dev.running = Some((session, version));
                self.status = None;
                if !already_applied {
                    self.load(App::Js(bundle))?;
                }
            }
            DevEvent::Patch {
                session,
                version,
                base,
                code,
                source_map,
            } => {
                if self
                    .dev
                    .as_ref()
                    .is_some_and(|dev| dev.running == Some((session, base)))
                    && let Some(control) = self.control()
                {
                    control.hot_update(nsh_engine::HotUpdate {
                        name: format!("hot-{version}.js"),
                        code,
                        source_map: source_map.map(Into::into),
                    });
                    if let Some(dev) = &mut self.dev {
                        dev.running = Some((session, version));
                    }
                    self.last_error = None;
                    self.status = None;
                }
            }
        }
        Ok(())
    }

    fn reload_latest(&mut self) -> io::Result<()> {
        let latest = self.dev.as_ref().and_then(|dev| dev.latest.clone());
        if let Some((session, version, bundle)) = latest {
            if let Some(dev) = &mut self.dev {
                dev.running = Some((session, version));
            }
            self.load(App::Js(bundle))?;
        }
        Ok(())
    }

    fn reload_after_update(&mut self) -> io::Result<()> {
        let Some(dev) = &mut self.dev else {
            return Ok(());
        };
        let running = dev.running.take();
        if dev
            .latest
            .as_ref()
            .is_some_and(|(session, version, _)| Some((*session, *version)) == running)
        {
            self.reload_latest()?;
        }
        Ok(())
    }
}

fn initial_environment(options: &AppOptions) -> Environment {
    let viewport = options.metrics.viewport(options.size.0, options.size.1);
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .and_then(|value| Locale::parse(&value))
        })
        .unwrap_or_default();
    Environment {
        window: (viewport.width, viewport.height),
        renderer: Renderer::Terminal,
        color_scheme: options.appearance.resolve(ColorScheme::Dark),
        os: if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Linux
        },
        idiom: Idiom::Desktop,
        locale,
        ..Environment::default()
    }
}

fn safe_diagnostic(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .collect()
}

#[cfg(feature = "dev")]
fn dev_asset_base(url: &str) -> Option<String> {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("ws://") {
        ("http", rest)
    } else {
        ("https", url.strip_prefix("wss://")?)
    };
    let host = rest.split('/').next().filter(|host| !host.is_empty())?;
    Some(format!("{scheme}://{host}/assets/"))
}

/// Runs an app until Ctrl+Q or its quit command. Interactive mode
/// owns raw mode, the alternate screen, mouse capture and bracketed paste,
/// restoring them on every return or panic. A script runs headlessly and
/// writes only its requested snapshots; it also works with redirected IO.
/// Interactive startup queries image protocols and pixel cell dimensions
/// for at most 600 ms before reading input events. `NSH_IMAGE_PROTOCOL`
/// accepts `auto`, `halfblocks`, `kitty`, `sixel` or `iterm2`; `halfblocks`
/// skips the query. Scripts and embedded views keep the half-block default.
/// Returned errors retain their IO kind and strip terminal controls from
/// diagnostic text, including errors produced by app code during a script.
pub fn run_app(source: AppSource, options: AppOptions) -> io::Result<()> {
    run_inner(source, options)
        .map_err(|error| io::Error::new(error.kind(), safe_diagnostic(&error.to_string())))
}

fn run_inner(source: AppSource, options: AppOptions) -> io::Result<()> {
    let script = options
        .script
        .as_deref()
        .map(crate::script::Script::parse)
        .transpose()?;
    let mut view = AppView::new(options);
    let interactive = script.is_none();
    if interactive && (!io::stdin().is_terminal() || !io::stdout().is_terminal()) {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "interactive NSH apps need a terminal; --script runs headlessly",
        ));
    }
    let signals = interactive.then(ShutdownSignals::new).transpose()?;
    let mut session = if interactive {
        Some(TerminalSession::enter(&view.options.title)?)
    } else {
        None
    };
    if interactive {
        // Query synchronously before either app code or Crossterm can
        // consume input; a timeout leaves no reader behind.
        view.set_image_protocol(terminal_graphics::detect());
        if let Some(session) = &mut session {
            session.graphics_cleanup = view.image_protocol_cleanup();
        }
    }
    match source {
        AppSource::Rust(root) => view.load(App::Rust(root))?,
        AppSource::Bundle(bundle) => view.load(App::Js(bundle))?,
        #[cfg(feature = "dev")]
        AppSource::DevServer(url) => view.connect(url)?,
    }
    if let Some(script) = script {
        return crate::script::run(&mut view, script);
    }
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;
    let (columns, rows) = terminal::size()?;
    view.resize(columns, rows);
    let started = Instant::now();
    let mut dirty = true;
    while !view.surface.should_quit() && !signals.as_ref().is_some_and(ShutdownSignals::requested) {
        dirty |= view.tick(started.elapsed().as_secs_f64())?;
        if dirty {
            terminal.draw(|frame| view.render(frame.area(), frame.buffer_mut()))?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(16))? {
            dirty |= view.handle_event(event::read()?);
        }
    }
    // Local drop order restores the terminal before joining the engine:
    // an app task finishing IO must not hold raw mode open on shutdown.
    Ok(())
}

/// Capability queries own stdin only during standalone startup. The
/// upstream convenience query leaves a background reader after a timeout;
/// polling here makes the deadline and transfer to Crossterm explicit.
mod terminal_graphics {
    use std::{
        io::{self, Write},
        time::{Duration, Instant},
    };

    use ratatui_image::picker::cap_parser::{Parser, QueryStdioOptions, Response};

    use crate::{ImageProtocol, ProtocolType};

    const QUERY_TIMEOUT: Duration = Duration::from_millis(600);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Preference {
        Auto,
        Halfblocks,
        Forced(ProtocolType),
    }

    impl Preference {
        fn parse(value: Option<&str>) -> Self {
            match value.unwrap_or("auto").trim().to_ascii_lowercase().as_str() {
                "auto" => Self::Auto,
                "halfblocks" => Self::Halfblocks,
                "kitty" => Self::Forced(ProtocolType::Kitty),
                "sixel" => Self::Forced(ProtocolType::Sixel),
                "iterm2" => Self::Forced(ProtocolType::Iterm2),
                _ => {
                    log::warn!("unknown NSH_IMAGE_PROTOCOL; using auto detection");
                    Self::Auto
                }
            }
        }
    }

    pub(super) fn detect() -> Option<ImageProtocol> {
        let preference = Preference::parse(std::env::var("NSH_IMAGE_PROTOCOL").ok().as_deref());
        if preference == Preference::Halfblocks
            || (preference == Preference::Auto
                && std::env::var("TERM").is_ok_and(|term| term == "dumb"))
        {
            return None;
        }
        let term = std::env::var("TERM").ok();
        let program = std::env::var("TERM_PROGRAM").ok();
        let tmux_session = std::env::var("TMUX").ok();
        let tmux = tmux_hint(term.as_deref(), program.as_deref(), tmux_session.as_deref());
        let blacklist = preference == Preference::Auto
            && ["WEZTERM_EXECUTABLE", "KONSOLE_VERSION"]
                .into_iter()
                .any(|key| std::env::var(key).is_ok_and(|value| !value.is_empty()));
        let options = QueryStdioOptions {
            timeout: QUERY_TIMEOUT,
            blacklist_protocols: if blacklist {
                vec![ProtocolType::Kitty, ProtocolType::Sixel]
            } else {
                Vec::new()
            },
            ..QueryStdioOptions::default()
        };
        let responses = match query(&Parser::query(tmux, options)) {
            Ok(responses) => responses,
            Err(error) => {
                log::debug!("terminal graphics query failed: {error}");
                Vec::new()
            }
        };
        let iterm2 = std::env::var("TERM_PROGRAM").is_ok_and(|program| iterm2_hint(&program))
            || std::env::var("LC_TERMINAL").is_ok_and(|program| program.contains("iTerm"))
            || (tmux
                && ["ITERM_SESSION_ID", "WEZTERM_EXECUTABLE"]
                    .into_iter()
                    .any(|key| std::env::var(key).is_ok_and(|value| !value.is_empty())));
        resolve(&responses, preference, fallback_cell_size(), tmux, iterm2)
    }

    fn iterm2_hint(program: &str) -> bool {
        [
            "iTerm",
            "WezTerm",
            "mintty",
            "vscode",
            "Tabby",
            "Hyper",
            "rio",
            "Bobcat",
            "WarpTerminal",
        ]
        .into_iter()
        .any(|name| program.contains(name))
    }

    fn tmux_hint(term: Option<&str>, program: Option<&str>, session: Option<&str>) -> bool {
        term.is_some_and(|term| term.starts_with("tmux"))
            || program == Some("tmux")
            || session.is_some_and(|session| !session.is_empty())
    }

    fn resolve(
        responses: &[Response],
        preference: Preference,
        fallback: Option<(u16, u16)>,
        tmux: bool,
        iterm2: bool,
    ) -> Option<ImageProtocol> {
        let cells = responses
            .iter()
            .rev()
            .find_map(|response| match response {
                Response::CellSize(Some((width, height))) if *width > 0 && *height > 0 => {
                    Some((*width, *height))
                }
                _ => None,
            })
            .or(fallback)?;
        let protocol = match preference {
            Preference::Halfblocks => return None,
            Preference::Forced(protocol) => protocol,
            Preference::Auto if responses.contains(&Response::Kitty) => ProtocolType::Kitty,
            Preference::Auto if responses.contains(&Response::Sixel) => ProtocolType::Sixel,
            Preference::Auto if iterm2 && (!tmux || responses.contains(&Response::Status)) => {
                ProtocolType::Iterm2
            }
            Preference::Auto => return None,
        };
        ImageProtocol::new(protocol, cells.0, cells.1)
            .ok()
            .map(|protocol| protocol.with_tmux(tmux))
    }

    fn query(request: &str) -> io::Result<Vec<Response>> {
        let mut output = io::stdout().lock();
        output.write_all(request.as_bytes())?;
        output.flush()?;
        let deadline = Instant::now() + QUERY_TIMEOUT;
        let mut parser = Parser::new();
        let mut responses = Vec::new();
        // The byte budget also bounds parser memory for a terminal that
        // sends incomplete or unsolicited sequences during the probe.
        for _ in 0..16_384 {
            let Some(byte) = read_byte(deadline)? else {
                break;
            };
            let parsed = parser.push(char::from(byte));
            let done = parsed.contains(&Response::Status);
            responses.extend(parsed);
            if done {
                break;
            }
        }
        Ok(responses)
    }

    #[cfg(unix)]
    fn read_byte(deadline: Instant) -> io::Result<Option<u8>> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let mut poll = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll points to one initialized descriptor and the
            // deadline is bounded. No other terminal reader has started.
            let ready = unsafe {
                libc::poll(
                    &mut poll,
                    1,
                    remaining.as_millis().max(1).min(i32::MAX as u128) as i32,
                )
            };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 || poll.revents & libc::POLLIN == 0 {
                return Ok(None);
            }
            let mut byte = 0u8;
            // SAFETY: poll established readable stdin and byte is a valid
            // one-byte destination. The probe is the sole input reader.
            let count = unsafe { libc::read(libc::STDIN_FILENO, (&mut byte as *mut u8).cast(), 1) };
            if count == 1 {
                return Ok(Some(byte));
            }
            if count == 0 {
                return Ok(None);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    #[cfg(windows)]
    fn read_byte(deadline: Instant) -> io::Result<Option<u8>> {
        use windows_sys::Win32::System::Console::{
            GetNumberOfConsoleInputEvents, GetStdHandle, INPUT_RECORD, KEY_EVENT,
            ReadConsoleInputW, STD_INPUT_HANDLE,
        };
        // SAFETY: reads this process's existing console handle without
        // taking ownership. The interactive runner verified terminal IO.
        let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        while Instant::now() < deadline {
            let mut pending = 0;
            // SAFETY: input is the current console and pending is writable.
            if unsafe { GetNumberOfConsoleInputEvents(input, &mut pending) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if pending == 0 {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            let mut record = std::mem::MaybeUninit::<INPUT_RECORD>::uninit();
            let mut count = 0;
            // SAFETY: a queued record exists, the output covers one record,
            // and no Crossterm event reader has started to compete for it.
            if unsafe { ReadConsoleInputW(input, record.as_mut_ptr(), 1, &mut count) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if count == 0 {
                continue;
            }
            // SAFETY: ReadConsoleInputW initialized the single record above.
            let record = unsafe { record.assume_init() };
            if u32::from(record.EventType) != KEY_EVENT {
                continue;
            }
            // SAFETY: EventType selects the KeyEvent union member.
            let key = unsafe { record.Event.KeyEvent };
            if key.bKeyDown == 0 {
                continue;
            }
            // SAFETY: KEY_EVENT_RECORD's UnicodeChar member is valid for
            // ReadConsoleInputW. Capability responses contain ASCII only.
            let character = unsafe { key.uChar.UnicodeChar };
            if let Ok(byte) = u8::try_from(character) {
                return Ok(Some(byte));
            }
        }
        Ok(None)
    }

    #[cfg(not(any(unix, windows)))]
    fn read_byte(_: Instant) -> io::Result<Option<u8>> {
        Ok(None)
    }

    #[cfg(unix)]
    fn fallback_cell_size() -> Option<(u16, u16)> {
        let size = crossterm::terminal::window_size().ok()?;
        if size.columns == 0 || size.rows == 0 {
            return None;
        }
        let cell = (size.width / size.columns, size.height / size.rows);
        (cell.0 > 0 && cell.1 > 0).then_some(cell)
    }

    #[cfg(windows)]
    fn fallback_cell_size() -> Option<(u16, u16)> {
        use windows_sys::Win32::System::Console::{
            CONSOLE_FONT_INFOEX, GetCurrentConsoleFontEx, GetStdHandle, STD_OUTPUT_HANDLE,
        };
        let mut font = CONSOLE_FONT_INFOEX {
            cbSize: std::mem::size_of::<CONSOLE_FONT_INFOEX>() as u32,
            ..Default::default()
        };
        // SAFETY: retrieves the process's output handle without owning it;
        // font is initialized with the size required by the console API.
        if unsafe { GetCurrentConsoleFontEx(GetStdHandle(STD_OUTPUT_HANDLE), 0, &mut font) } == 0 {
            return None;
        }
        let cell = (
            u16::try_from(font.dwFontSize.X).ok()?,
            u16::try_from(font.dwFontSize.Y).ok()?,
        );
        (cell.0 > 0 && cell.1 > 0).then_some(cell)
    }

    #[cfg(not(any(unix, windows)))]
    fn fallback_cell_size() -> Option<(u16, u16)> {
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn responses(bytes: &str) -> Vec<Response> {
            let mut parser = Parser::new();
            bytes.chars().flat_map(|ch| parser.push(ch)).collect()
        }

        #[test]
        fn startup_probe_prefers_reported_kitty_then_sixel_and_exact_cells() {
            let replies =
                responses("\u{1b}_Gi=31;OK\u{1b}\\\u{1b}[?1;2;4c\u{1b}[6;20;10t\u{1b}[0n");
            let protocol = resolve(&replies, Preference::Auto, Some((8, 16)), false, true).unwrap();
            assert_eq!(protocol.protocol_type(), ProtocolType::Kitty);
            assert_eq!(protocol.cell_size(), (10, 20));
            let sixel = resolve(
                &responses("\u{1b}[?1;2;4c\u{1b}[6;18;9t\u{1b}[0n"),
                Preference::Auto,
                None,
                false,
                false,
            )
            .unwrap();
            assert_eq!(sixel.protocol_type(), ProtocolType::Sixel);
            assert_eq!(sixel.cell_size(), (9, 18));
        }

        #[test]
        fn protocols_require_real_cell_dimensions_and_tmux_passthrough_evidence() {
            assert!(resolve(&[Response::Kitty], Preference::Auto, None, false, false).is_none());
            assert!(resolve(&[], Preference::Auto, Some((8, 16)), false, false).is_none());
            assert!(resolve(&[], Preference::Auto, Some((8, 16)), true, true).is_none());
            assert_eq!(
                resolve(
                    &[Response::Status],
                    Preference::Auto,
                    Some((8, 16)),
                    true,
                    true
                )
                .unwrap()
                .protocol_type(),
                ProtocolType::Iterm2
            );
            assert!(
                resolve(
                    &[Response::Kitty],
                    Preference::Halfblocks,
                    Some((8, 16)),
                    false,
                    false
                )
                .is_none()
            );
            assert_eq!(
                Preference::parse(Some("halfblocks")),
                Preference::Halfblocks
            );
            assert!(tmux_hint(
                Some("screen-256color"),
                Some("iTerm.app"),
                Some("/tmp/tmux-1000/default,123,0")
            ));
            assert!(!tmux_hint(Some("screen-256color"), Some("iTerm.app"), None));
        }
    }
}

struct TerminalSession {
    graphics_cleanup: String,
}

impl TerminalSession {
    fn enter(title: &str) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        // Construct the guard first: a partial setup also needs restoring.
        let session = Self {
            graphics_cleanup: String::new(),
        };
        // An app title is data inside an OSC sequence. Strip terminal
        // controls so a package title cannot inject another sequence.
        let title: String = title.chars().filter(|ch| !ch.is_control()).collect();
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            Hide,
            SetTitle(title)
        )?;
        Ok(session)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = io::stdout().write_all(self.graphics_cleanup.as_bytes());
        let _ = execute!(
            io::stdout(),
            Show,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

struct ShutdownSignals {
    #[cfg(not(windows))]
    requested: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(unix)]
    registrations: Vec<signal_hook::SigId>,
}

impl ShutdownSignals {
    fn new() -> io::Result<Self> {
        #[cfg(not(windows))]
        let requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(unix)]
        {
            let mut registrations = Vec::new();
            for signal in [
                signal_hook::consts::SIGINT,
                signal_hook::consts::SIGTERM,
                signal_hook::consts::SIGHUP,
            ] {
                match signal_hook::flag::register(signal, requested.clone()) {
                    Ok(id) => registrations.push(id),
                    Err(error) => {
                        for id in registrations {
                            signal_hook::low_level::unregister(id);
                        }
                        return Err(error);
                    }
                }
            }
            Ok(Self {
                requested,
                registrations,
            })
        }
        #[cfg(windows)]
        {
            CONSOLE_STOP.store(false, std::sync::atomic::Ordering::Relaxed);
            // SAFETY: the handler lives for the process and only writes
            // an atomic flag. Drop unregisters this exact handler.
            if unsafe {
                windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(console_stop), 1)
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {})
        }
        #[cfg(not(any(unix, windows)))]
        Ok(Self { requested })
    }

    fn requested(&self) -> bool {
        #[cfg(windows)]
        {
            CONSOLE_STOP.load(std::sync::atomic::Ordering::Relaxed)
        }
        #[cfg(not(windows))]
        self.requested.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(windows)]
static CONSOLE_STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(windows)]
unsafe extern "system" fn console_stop(event: u32) -> windows_sys::core::BOOL {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };
    if matches!(
        event,
        CTRL_C_EVENT
            | CTRL_BREAK_EVENT
            | CTRL_CLOSE_EVENT
            | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT
    ) {
        CONSOLE_STOP.store(true, std::sync::atomic::Ordering::Relaxed);
        1
    } else {
        0
    }
}

#[cfg(windows)]
impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        // SAFETY: removes the same process-static handler registered in new.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(console_stop), 0);
        }
    }
}

#[cfg(unix)]
impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        for id in self.registrations.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

#[cfg(feature = "dev")]
struct LogRecord {
    level: log::Level,
    target: String,
    message: String,
}

#[cfg(feature = "dev")]
impl LogRecord {
    fn terminal_safe(mut self) -> Self {
        self.target = safe_diagnostic(&self.target);
        self.message = safe_diagnostic(&self.message);
        self
    }
}
#[cfg(feature = "dev")]
static LOGS: Mutex<VecDeque<LogRecord>> = Mutex::new(VecDeque::new());

/// Installs the standalone hosts' logger. Diagnostics are buffered and
/// forwarded to a connected dev server; setting `NSH_LOG` additionally
/// writes them to that file. Output strips terminal control characters
/// while retaining newlines and tabs. No log line writes over the scene.
/// Embedders keep their own logger instead.
pub fn init_logging() {
    struct Logger;
    impl log::Log for Logger {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            let message = record.args().to_string();
            if let Ok(path) = std::env::var("NSH_LOG")
                && let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
            {
                let _ = writeln!(
                    file,
                    "{} {}: {}",
                    record.level(),
                    safe_diagnostic(record.target()),
                    safe_diagnostic(&message)
                );
            }
            #[cfg(feature = "dev")]
            {
                let mut logs = LOGS.lock().unwrap_or_else(|e| e.into_inner());
                if logs.len() >= 1024 {
                    logs.pop_front();
                }
                logs.push_back(LogRecord {
                    level: record.level(),
                    target: record.target().into(),
                    message,
                });
            }
        }
        fn flush(&self) {}
    }
    static LOGGER: Logger = Logger;
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

#[cfg(feature = "dev")]
fn take_logs() -> Vec<LogRecord> {
    LOGS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect()
}

#[cfg(test)]
mod tests {
    use nsh_core::ui::prelude::*;

    use super::*;

    #[component]
    fn ScriptApp() -> impl IntoElement {
        let typed = signal(String::new());
        let count = signal(0_u32);
        view()
            .flex_col()
            .child(text("Ready"))
            .child(
                input()
                    .h(px(32.0))
                    .value(move || typed.get())
                    .on_change_text(move |value: &str| typed.set(value.to_owned())),
            )
            .child(text(move || format!("Echo:{}", typed.get())))
            .child(
                view()
                    .h(px(32.0))
                    .on_press(move |_: &()| count.update(|value| *value += 1))
                    .child(text("Increment")),
            )
            .child(text(move || format!("Count:{}", count.get())))
    }

    #[test]
    fn headless_runner_mounts_edits_activates_and_resizes_a_real_app() {
        run_app(AppSource::Rust(Root::of::<ScriptApp>()), AppOptions {
            size: (80, 24),
            script: Some("expect Ready; expect-focus; type 你好; expect Echo:你好; key Tab; key Enter; expect Count:1; resize 60 18; expect Count:1; quit".into()),
            ..Default::default()
        }).unwrap();
    }

    #[test]
    fn view_keeps_terminal_size_in_virtual_points() {
        let mut view = AppView::new(AppOptions {
            size: (80, 24),
            ..Default::default()
        });
        assert_eq!(view.surface().viewport(), nsh_core::Size::new(640.0, 384.0));
        view.resize(40, 10);
        assert_eq!(view.surface().viewport(), nsh_core::Size::new(320.0, 160.0));
        assert!(view.control().is_none());
        assert_eq!(view.environment.renderer, Renderer::Terminal);
        assert!(view.image_protocol_cleanup().is_empty());
    }

    #[test]
    fn graphics_cleanup_ids_survive_replacing_the_app() {
        let mut view = AppView::new(AppOptions::default());
        view.set_image_protocol(Some(
            crate::ImageProtocol::new(crate::ProtocolType::Kitty, 8, 16).unwrap(),
        ));
        let cleanup = view.image_protocol_cleanup();
        assert!(!cleanup.is_empty());
        for _ in 0..2 {
            view.load(App::Rust(Root::of::<ScriptApp>())).unwrap();
            assert_eq!(view.image_protocol_cleanup(), cleanup);
        }
    }

    #[test]
    fn app_errors_cannot_emit_terminal_control_sequences() {
        assert_eq!(
            safe_diagnostic("error\n\u{1b}]52;clipboard\u{7}\tline"),
            "error\n]52;clipboard\tline"
        );
        // Parsing fails before mounting the source. The public runner
        // must sanitize the error that a host prints after it returns.
        let error = run_app(
            AppSource::Rust(Root::of::<ScriptApp>()),
            AppOptions {
                script: Some("invalid\u{1b}]52\u{7}\u{9b}31m".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "invalid script step: invalid]5231m");
    }

    #[cfg(feature = "dev")]
    #[test]
    fn forwarded_logs_cannot_inject_clipboard_or_c1_terminal_commands() {
        let record = LogRecord {
            level: log::Level::Info,
            target: "js\u{1b}]52;target\u{7}\u{9b}31m".into(),
            message: "hello\n\t\u{1b}]52;c;Y2xpcGJvYXJk\u{7}\u{9b}31m\u{9d}52;c;secret\u{9c}"
                .into(),
        }
        .terminal_safe();
        assert_eq!(record.target, "js]52;target31m");
        assert_eq!(record.message, "hello\n\t]52;c;Y2xpcGJvYXJk31m52;c;secret");
        assert!(!record.target.chars().any(char::is_control));
        assert!(
            !record
                .message
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
        );
    }

    #[cfg(feature = "dev")]
    #[test]
    fn asset_urls_follow_dev_server_scheme() {
        assert_eq!(
            dev_asset_base("ws://localhost:8790/ws"),
            Some("http://localhost:8790/assets/".into())
        );
        assert_eq!(
            dev_asset_base("wss://example.org/path/ws"),
            Some("https://example.org/assets/".into())
        );
        assert_eq!(dev_asset_base("http://localhost/ws"), None);
        assert_eq!(dev_asset_base("ws:///ws"), None);
    }
}
