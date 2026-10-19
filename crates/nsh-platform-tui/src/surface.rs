//! A retained terminal scene. One surface owns its nodes, input, animation
//! graph and graphics work; embedding several surfaces shares no app state.

use std::{collections::HashMap, sync::Arc};

use nsh_animation::{AnimationProp, AnimationValue, Graph};
use nsh_core::{
    AttributedText, Color, Command, Dialog, EventKind, EventMask, EventPayload, GestureValues,
    ImageSource, InputConfig, ModalConfig, MountBatch, MountItem, NodeId, PayloadValue,
    PointerEvents, ROOT_ID, Rect, ResizeMode, ScreenConfig, ScrollBehavior, Size, StickyCell,
    TextLayout, Transform, TransformKind, ViewKind, ViewProps, ViewStyle, access::Access,
    keymap::Keymap,
};
use ratatui::{buffer::Buffer, layout::Rect as CellRect};

use crate::{
    editor::Editor,
    image::{AnimatedRaster, Raster},
    metrics::CellMetrics,
};

/// Input and native work to forward to a surface's engine in order.
#[derive(Debug)]
pub enum SurfaceEvent {
    /// An application event on a mounted view.
    Event(NodeId, EventKind, EventPayload),
    /// A scroll offset, with the number of core adjustments applied.
    Scroll(NodeId, f32, f32, u32),
    /// Interaction state bits requested by the mounted view.
    States(NodeId, u8),
    /// Animation completions and native value reads.
    AnimationsFinished(Vec<nsh_animation::Finished>),
    /// A hosted terminal component reported its natural size in points.
    ComponentSize(NodeId, Size),
}

pub(crate) struct ScrollState {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) content: Size,
    pub(crate) horizontal: bool,
    pub(crate) enabled: bool,
    pub(crate) shows_indicator: bool,
    pub(crate) behavior: Box<ScrollBehavior>,
    pub(crate) count: u32,
    pub(crate) sticky: Vec<StickyCell>,
    pub(crate) scroll_value: Option<nsh_animation::NodeId>,
    pub(crate) mirrored: bool,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            content: Size::default(),
            horizontal: false,
            enabled: true,
            shows_indicator: true,
            behavior: Box::default(),
            count: 0,
            sticky: Vec::new(),
            scroll_value: None,
            mirrored: false,
        }
    }
}

pub(crate) struct Node {
    pub(crate) kind: ViewKind,
    pub(crate) parent: Option<NodeId>,
    pub(crate) children: Vec<NodeId>,
    pub(crate) frame: Rect,
    pub(crate) style: Box<ViewStyle>,
    pub(crate) props: Box<ViewProps>,
    pub(crate) events: EventMask,
    pub(crate) tracked: u8,
    pub(crate) text: Option<(Arc<AttributedText>, TextLayout, Rect)>,
    pub(crate) input: InputConfig,
    pub(crate) editor: Editor,
    pub(crate) input_content: Rect,
    pub(crate) scroll: ScrollState,
    pub(crate) screen: ScreenConfig,
    pub(crate) modal: ModalConfig,
    pub(crate) popped: bool,
    pub(crate) drawing: Option<Arc<nsh_core::canvas::Drawing>>,
    pub(crate) image_source: Option<ImageSource>,
    pub(crate) resize_mode: ResizeMode,
    pub(crate) image_raster: Option<Arc<Raster>>,
    pub(crate) image_animation: Option<Arc<AnimatedRaster>>,
    pub(crate) image_started: f64,
    pub(crate) canvas_raster: Option<Arc<Raster>>,
    pub(crate) bindings: Vec<(AnimationProp, nsh_animation::NodeId)>,
    pub(crate) gestures: GestureValues,
    pub(crate) canvas_args: Vec<(u16, AnimationValue)>,
    pub(crate) native: Option<Box<dyn crate::native::NativeComponent>>,
    native_size: Option<Size>,
    motion: Option<Box<crate::motion::NodeMotion>>,
    delete_pending: bool,
}

impl Node {
    fn new(kind: ViewKind) -> Self {
        Self {
            kind,
            parent: None,
            children: Vec::new(),
            frame: Rect::default(),
            style: Box::default(),
            props: Box::default(),
            events: EventMask::default(),
            tracked: 0,
            text: None,
            input: InputConfig::default(),
            editor: Editor::default(),
            input_content: Rect::default(),
            scroll: ScrollState::default(),
            screen: ScreenConfig::default(),
            modal: ModalConfig::default(),
            popped: false,
            drawing: None,
            image_source: None,
            resize_mode: ResizeMode::Cover,
            image_raster: None,
            image_animation: None,
            image_started: 0.0,
            canvas_raster: None,
            bindings: Vec::new(),
            gestures: GestureValues::default(),
            canvas_args: Vec::new(),
            native: None,
            native_size: None,
            motion: None,
            delete_pending: false,
        }
    }
}

/// A Native Shell scene that can be painted into any Ratatui rectangle.
///
/// This type does no terminal IO. Feed it mount batches and terminal events,
/// paint it into the host's buffer, then forward [`Self::drain_events`] to
/// the engine. [`crate::AppView`] combines these steps with an engine.
pub struct Surface {
    pub(crate) nodes: Vec<Option<Node>>,
    pub(crate) metrics: CellMetrics,
    pub(crate) columns: u16,
    pub(crate) rows: u16,
    pub(crate) events: Vec<SurfaceEvent>,
    pub(crate) focus: Option<NodeId>,
    pub(crate) hovered: Option<NodeId>,
    pub(crate) pressed: Option<NodeId>,
    pub(crate) keymap: Arc<Keymap>,
    pub(crate) quit: bool,
    pub(crate) dialog: Option<Dialog>,
    pub(crate) dialog_selection: usize,
    pub(crate) clipboard: String,
    pub(crate) animation: Graph,
    pub(crate) hovered_path: Vec<NodeId>,
    pub(crate) autofocus_seen: Vec<NodeId>,
    pub(crate) drag_input: Option<NodeId>,
    pub(crate) pointer: Option<crate::input::Pointer>,
    pub(crate) focus_history: Vec<NodeId>,
    pub(crate) toast: Option<(String, f64)>,
    pub(crate) dark: bool,
    pub(crate) now: f64,
    pub(crate) access: Access,
    pub(crate) components: crate::native::Components,
    graphics: crate::worker::Graphics,
    pub(crate) protocol: crate::protocol::Images,
    assets: Option<Arc<dyn nsh_engine::Assets>>,
    canvas_dirty: Vec<NodeId>,
    canvas_pending: Vec<NodeId>,
    image_cache: HashMap<String, CachedImage>,
    pending_images: HashMap<String, PendingImage>,
}

struct CachedImage {
    animation: Arc<AnimatedRaster>,
    started: f64,
    frame: usize,
}

#[derive(Default)]
struct PendingImage {
    views: Vec<NodeId>,
    requests: Vec<u32>,
}

impl Surface {
    /// An empty scene, with its dimensions in terminal columns and rows.
    pub fn new(columns: u16, rows: u16, metrics: CellMetrics) -> Self {
        let mut surface = Self {
            nodes: vec![None, Some(Node::new(ViewKind::View))],
            metrics,
            columns,
            rows,
            events: Vec::new(),
            focus: None,
            hovered: None,
            pressed: None,
            keymap: Arc::default(),
            quit: false,
            dialog: None,
            dialog_selection: 0,
            clipboard: String::new(),
            animation: Graph::default(),
            hovered_path: Vec::new(),
            autofocus_seen: Vec::new(),
            drag_input: None,
            pointer: None,
            focus_history: Vec::new(),
            toast: None,
            dark: false,
            now: 0.0,
            access: Access::all(),
            components: crate::native::Components::default(),
            graphics: crate::worker::Graphics::new(),
            protocol: crate::protocol::Images::default(),
            assets: None,
            canvas_dirty: Vec::new(),
            canvas_pending: Vec::new(),
            image_cache: HashMap::new(),
            pending_images: HashMap::new(),
        };
        surface.resize(columns, rows);
        surface
    }

    /// The virtual dimensions of one terminal cell.
    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// Enables a terminal image protocol using the host's detected pixel
    /// cell size. `None` keeps deterministic colored half-block rendering.
    /// This method performs no terminal IO or capability queries.
    pub fn set_image_protocol(&mut self, protocol: Option<crate::ImageProtocol>) {
        self.protocol.configure(protocol);
    }

    /// Escape sequences for the host to flush before unmounting this surface.
    /// They release this surface's bounded Kitty image identifiers. The
    /// embedding host owns terminal IO and decides when to write them.
    pub fn image_protocol_cleanup(&self) -> String {
        self.protocol.cleanup()
    }

    pub(crate) fn inherit_image_protocol(&mut self, previous: &mut Self) {
        self.protocol = std::mem::take(&mut previous.protocol);
        self.protocol.reset_entries();
    }

    pub(crate) fn paint_image_protocol(
        &self,
        id: NodeId,
        request: crate::protocol::Request,
        buffer: &mut Buffer,
    ) -> bool {
        self.protocol.paint(id, request, &self.graphics, buffer)
    }

    /// The layout viewport in virtual points.
    pub fn viewport(&self) -> Size {
        self.metrics.viewport(self.columns, self.rows)
    }

    /// Resizes the scene; its host also sends the new viewport to the engine.
    pub fn resize(&mut self, columns: u16, rows: u16) {
        self.columns = columns;
        self.rows = rows;
        let size = self.viewport();
        if let Some(root) = self.node_mut(ROOT_ID) {
            root.frame = Rect {
                x: 0.0,
                y: 0.0,
                width: size.width,
                height: size.height,
            };
        }
    }

    /// Supplies the same asset reader that the engine uses for this app.
    pub fn set_assets(&mut self, assets: Option<Arc<dyn nsh_engine::Assets>>) {
        self.assets = assets;
    }

    /// Sets the verified bundle's media and clipboard permissions.
    pub fn set_access(&mut self, access: Access) {
        self.access = access;
    }

    /// Chooses the fallback colors of native terminal overlays.
    pub fn set_dark(&mut self, dark: bool) {
        self.dark = dark;
    }

    /// Registers the factories this surface may instantiate through `nsh/native`.
    pub fn set_components(&mut self, components: crate::native::Components) {
        self.components = components;
    }

    /// Whether the app requested that its host close.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// The view currently receiving keyboard input.
    pub fn focused(&self) -> Option<NodeId> {
        self.focus
    }

    /// Drains application events in their original order.
    pub fn drain_events(&mut self) -> impl Iterator<Item = SurfaceEvent> {
        self.events.drain(..)
    }

    /// A plain text view of the actual rendered buffer, for scripts and tests.
    pub fn snapshot(&self) -> String {
        let area = CellRect::new(0, 0, self.columns, self.rows);
        let mut buffer = Buffer::empty(area);
        self.render_plain(area, &mut buffer);
        let mut output = String::new();
        for y in 0..self.rows {
            let mut x = 0;
            let mut line = String::new();
            while x < self.columns {
                let symbol = buffer[(x, y)].symbol();
                line.push_str(symbol);
                x += unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
            }
            output.push_str(line.trim_end());
            output.push('\n');
        }
        output
    }

    pub(crate) fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id as usize)?.as_ref()
    }
    pub(crate) fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(id as usize)?.as_mut()
    }
    pub(crate) fn emit(&mut self, id: NodeId, kind: EventKind, payload: EventPayload) {
        self.events.push(SurfaceEvent::Event(id, kind, payload));
    }

    pub(crate) fn component_context(
        &self,
        id: NodeId,
        area: CellRect,
    ) -> crate::native::ComponentContext {
        let frame = self.global_frame(id).unwrap_or_default();
        let clip = intersect(frame, self.clip_rect(id));
        let cells = |rect: Rect| {
            let left = (rect.x / self.metrics.width()).floor().max(0.0) as u16;
            let top = (rect.y / self.metrics.height()).floor().max(0.0) as u16;
            let right = ((rect.x + rect.width) / self.metrics.width())
                .ceil()
                .max(0.0) as u16;
            let bottom = ((rect.y + rect.height) / self.metrics.height())
                .ceil()
                .max(0.0) as u16;
            CellRect::new(
                area.x.saturating_add(left),
                area.y.saturating_add(top),
                right.saturating_sub(left),
                bottom.saturating_sub(top),
            )
            .intersection(area)
        };
        crate::native::ComponentContext {
            frame,
            clip,
            area: cells(frame),
            clip_area: cells(clip),
            metrics: self.metrics,
            focused: self.focus == Some(id),
        }
    }

    fn component_response(
        &mut self,
        id: NodeId,
        response: crate::native::ComponentResponse,
    ) -> bool {
        for event in response.events {
            self.emit(id, EventKind::NativeEvent, event.into());
        }
        response.handled
    }

    fn component_size(&mut self, id: NodeId) {
        if let Some(size) = self
            .node(id)
            .and_then(|node| node.native.as_ref())
            .and_then(|component| component.intrinsic_size())
            && size.width.is_finite()
            && size.height.is_finite()
            && size.width > 0.0
            && size.height > 0.0
            && self
                .node(id)
                .is_some_and(|node| node.native_size != Some(size))
        {
            if let Some(node) = self.node_mut(id) {
                node.native_size = Some(size);
            }
            self.events.push(SurfaceEvent::ComponentSize(id, size));
        }
    }

    pub(crate) fn native_event(&mut self, id: NodeId, event: &crossterm::event::Event) -> bool {
        let context = self.component_context(id, CellRect::new(0, 0, self.columns, self.rows));
        let response = self
            .node_mut(id)
            .and_then(|node| node.native.as_mut())
            .map(|component| component.handle_event(event, &context));
        let handled = response.is_some_and(|response| self.component_response(id, response));
        self.component_size(id);
        handled
    }

    fn detach(&mut self, id: NodeId) {
        if let Some(parent) = self.node(id).and_then(|n| n.parent)
            && let Some(parent) = self.node_mut(parent)
        {
            parent.children.retain(|&child| child != id);
        }
        if let Some(node) = self.node_mut(id) {
            node.parent = None;
        }
    }

    /// Applies one core commit in order, then reconciles native interaction.
    pub fn apply(&mut self, batch: MountBatch) {
        let mut finished = Vec::new();
        let mut tracked = Vec::new();
        let mut layout = None::<nsh_core::LayoutAnimation>;
        let mut scrolls = Vec::new();
        for item in batch.items {
            match item {
                MountItem::LayoutAnimation(animation) => layout = Some(animation),
                MountItem::Create { id, kind } => {
                    self.nodes.resize_with(self.nodes.len().max(id as usize + 1), || None);
                    self.nodes[id as usize] = Some(Node::new(kind));
                }
                MountItem::Insert { parent, child, before } => {
                    let shown = self.node(child).is_some_and(|n| n.kind == ViewKind::Modal && n.parent.is_none());
                    let now = self.now;
                    if let Some(node) = self.node_mut(child) {
                        let create = (node.frame == Rect::default()).then(|| layout.as_ref().and_then(|a| a.create.as_ref())).flatten();
                        if create.is_some() || node.motion.is_some() { node.motion.get_or_insert_with(Box::default).inserted(create, now); }
                        node.delete_pending = false;
                    }
                    self.detach(child);
                    if let Some(node) = self.node_mut(child) { node.parent = Some(parent); node.popped = false; }
                    if let Some(node) = self.node_mut(parent) {
                        let index = before.and_then(|id| node.children.iter().position(|&c| c == id)).unwrap_or(node.children.len());
                        node.children.insert(index, child);
                    }
                    if shown { self.emit(child, EventKind::Show, EventPayload::None); }
                }
                MountItem::Remove { id } => {
                    let modal = self.node(id).is_some_and(|n| n.kind == ViewKind::Modal && !n.popped);
                    let now = self.now;
                    let retained = self.node_mut(id).is_some_and(|node| {
                        let transition = layout.as_ref().and_then(|a| a.delete.as_ref());
                        (transition.is_some() || node.motion.is_some()) && node.motion.get_or_insert_with(Box::default).removing(transition, now)
                    });
                    if !retained { self.detach(id); }
                    if modal { self.emit(id, EventKind::Dismiss, EventPayload::None); }
                }
                MountItem::Delete { id } => {
                    let now = self.now;
                    if !self.is_dying(id)
                        && let Some(transition) = layout.as_ref().and_then(|animation| animation.delete.as_ref())
                        && let Some(node) = self.node_mut(id) {
                        node.motion.get_or_insert_with(Box::default).removing(Some(transition), now);
                    }
                    if self.is_dying(id) {
                        if let Some(node) = self.node_mut(id) { node.delete_pending = true; }
                        continue;
                    }
                    self.detach(id);
                    if let Some(node) = self.nodes.get_mut(id as usize) { *node = None; }
                    self.autofocus_seen.retain(|&seen| seen != id);
                    self.canvas_dirty.retain(|&dirty| dirty != id);
                }
                MountItem::Frame { id, frame } => {
                    let now = self.now;
                    if let Some(node) = self.node_mut(id) {
                        let transition = layout.as_ref().and_then(|a| a.update.as_ref());
                        if transition.is_some() || node.motion.is_some() {
                            let from = node.motion.as_ref().map_or(node.frame, |motion| motion.frame(node.frame, now));
                            node.motion.get_or_insert_with(Box::default).frame_changed(from, transition, now);
                        }
                        node.frame = frame;
                        if node.kind == ViewKind::Scroll { scrolls.push(id); }
                    }
                    self.dirty_canvas(id);
                }
                MountItem::ViewStyle { id, style } => {
                    let now = self.now;
                    if let Some(node) = self.node_mut(id) {
                        if style.transition.is_some() || style.animation.is_some() || node.motion.is_some() {
                            let from = node.motion.as_ref().map_or_else(|| (*node.style).clone(), |motion| motion.sample(node.frame, &node.style, now).style.into_owned());
                            node.motion.get_or_insert_with(Box::default).style_changed(&from, &style, now);
                        }
                        node.style = style;
                    }
                }
                MountItem::ViewProps { id, props } => if let Some(node) = self.node_mut(id) { node.props = props; },
                MountItem::AccessibilityLabel { id, label } => if let Some(node) = self.node_mut(id) { node.props.accessibility.label = label; },
                MountItem::TrackStates { id, states } => if let Some(node) = self.node_mut(id) { node.tracked = states; tracked.push(id); },
                MountItem::Events { id, events } => if let Some(node) = self.node_mut(id) { node.events = events; },
                MountItem::Text { id, text, layout, content } => if let Some(node) = self.node_mut(id) {
                    node.editor.label_text(&text.text);
                    node.text = Some((text, layout, content));
                },
                MountItem::Input { id, config } => if let Some(node) = self.node_mut(id) { node.input = *config; },
                MountItem::InputValue { id, text, count } => self.input_value(id, text, count),
                MountItem::InputContent { id, content } => if let Some(node) = self.node_mut(id) { node.input_content = content; },
                MountItem::ScrollConfig { id, horizontal, shows_indicator, scroll_value, mirrored, .. } => if let Some(node) = self.node_mut(id) {
                    node.scroll.horizontal = horizontal; node.scroll.shows_indicator = shows_indicator;
                    node.scroll.scroll_value = scroll_value; node.scroll.mirrored = mirrored;
                },
                MountItem::ScrollContent { id, size } => if let Some(node) = self.node_mut(id) { node.scroll.content = size; scrolls.push(id); },
                MountItem::ScrollBehavior { id, behavior } => if let Some(node) = self.node_mut(id) {
                    node.scroll.enabled = behavior.enabled; node.scroll.behavior = behavior; scrolls.push(id);
                },
                MountItem::ScrollBy { id, dx, dy } => {
                    if let Some(node) = self.node_mut(id) {
                        node.scroll.count = node.scroll.count.wrapping_add(1);
                        let (x, y) = (node.scroll.x + dx, node.scroll.y + dy);
                        self.scroll_to(id, x, y);
                    }
                }
                MountItem::StickyCells { id, cells } => if let Some(node) = self.node_mut(id) { node.scroll.sticky = cells; },
                MountItem::Keymap(keymap) => self.keymap = keymap,
                MountItem::ScreenConfig { id, config } => if let Some(node) = self.node_mut(id) { node.screen = *config; },
                MountItem::ModalConfig { id, config } => if let Some(node) = self.node_mut(id) { node.modal = config; },
                MountItem::Appearance { appearance } => self.dark = appearance.resolve(if self.dark { nsh_core::platform::ColorScheme::Dark } else { nsh_core::platform::ColorScheme::Light }) == nsh_core::platform::ColorScheme::Dark,
                MountItem::Animation(op) => self.animation.apply(op, &mut finished),
                MountItem::AnimationBindings { id, bindings } => if let Some(node) = self.node_mut(id) { node.bindings = bindings; },
                MountItem::Gestures { id, values } => if let Some(node) = self.node_mut(id) { node.gestures = values; },
                MountItem::Canvas { id, drawing } => {
                    let images: Vec<_> = drawing.images().map(|image| image.uri.to_string()).collect();
                    if let Some(node) = self.node_mut(id) { node.drawing = Some(drawing); }
                    for uri in images { self.request_image(uri, None, None); }
                    self.dirty_canvas(id);
                }
                MountItem::Image { id, source, resize_mode, .. } => {
                    if let Some(node) = self.node_mut(id) { node.image_source = source.clone(); node.resize_mode = resize_mode; node.image_raster = None; node.image_animation = None; }
                    if let Some(source) = source { self.request_image(source.uri.to_string(), Some(id), None); }
                }
                MountItem::Component { id, name } => {
                    let native = self.components.create(&name);
                    if let Some(node) = self.node_mut(id) { node.native = native; }
                    self.component_size(id);
                }
                MountItem::ComponentProps { id, props } => {
                    for (name, value) in props {
                        let response = self.node_mut(id).and_then(|node| node.native.as_mut()).map(|component| component.set_prop(&name, value.as_ref()));
                        if let Some(response) = response { self.component_response(id, response); }
                    }
                    self.component_size(id);
                }
                MountItem::Command { id, command } => self.command(id, command),
                MountItem::NativeProps { id, .. } => self.emit(id, EventKind::Error, EventPayload::Error { message: "This terminal surface presents web and video content through host components".to_owned() }),
                MountItem::Overflow { .. } | MountItem::LayoutDirection { .. } | MountItem::Refreshing { .. } => {}
            }
        }
        if !finished.is_empty() {
            self.events.push(SurfaceEvent::AnimationsFinished(finished));
        }
        scrolls.sort_unstable();
        scrolls.dedup();
        for id in scrolls {
            self.clamp_scroll(id);
        }
        self.reconcile_focus();
        for id in tracked {
            self.refresh_input_states(id);
        }
        self.update_canvas_args();
        self.schedule_graphics();
    }

    fn command(&mut self, id: NodeId, command: Command) {
        match command {
            Command::Focus | Command::AccessibilityFocus => self.set_focus(Some(id)),
            Command::Blur => self.set_focus(None),
            Command::SetSelection { start, end } => self.selection(id, start, end),
            Command::ScrollTo { x, y, .. } => self.scroll_to(id, x, y),
            Command::ScrollToEnd { .. } => {
                if let Some(node) = self.node(id) {
                    self.scroll_to(id, node.scroll.content.width, node.scroll.content.height);
                }
            }
            Command::Back => self.back(),
            Command::ExitApp => self.quit = true,
            Command::Dialog(dialog) => {
                self.dialog = Some(*dialog);
                self.dialog_selection = 0;
            }
            Command::Toast { text, long } => {
                self.toast = Some((text, self.now + if long { 5.0 } else { 2.5 }))
            }
            Command::Announce(text) => self.toast = Some((text, self.now + 5.0)),
            Command::SetClipboard(text) => {
                if self.access.allows_clipboard() {
                    self.clipboard = text;
                }
            }
            Command::ReadClipboard { id, has_only } => {
                let allowed = self.access.allows_clipboard();
                let mut fields = vec![
                    ("id", PayloadValue::Number(id as f64)),
                    (
                        "has",
                        PayloadValue::Bool(allowed && !self.clipboard.is_empty()),
                    ),
                ];
                if !has_only {
                    fields.push((
                        "text",
                        PayloadValue::Str(if allowed {
                            self.clipboard.clone()
                        } else {
                            String::new()
                        }),
                    ));
                }
                self.emit(ROOT_ID, EventKind::Clipboard, EventPayload::Map(fields));
            }
            Command::ImageInfo { id, uri } => self.request_image(uri, None, Some(id)),
            Command::Native { name, args } => {
                let response = self
                    .node_mut(id)
                    .and_then(|node| node.native.as_mut())
                    .map(|component| component.command(&name, &args));
                if let Some(response) = response {
                    self.component_response(id, response);
                }
                self.component_size(id);
            }
            Command::Desktop(command) => {
                if *command == nsh_core::desktop::DesktopCommand::Close {
                    self.quit = true;
                }
            }
            Command::StatusBar(_)
            | Command::SetStatusBar(_)
            | Command::LayoutAnimation(_)
            | Command::SetJsResponder { .. }
            | Command::ClearJsResponder => {}
            other => log::debug!("terminal command: {other:?}"),
        }
    }

    pub(crate) fn scroll_to(&mut self, id: NodeId, x: f32, y: f32) {
        let Some(node) = self.node_mut(id) else {
            return;
        };
        let inset = node.scroll.behavior.content_inset;
        let x = x.clamp(
            -inset.left,
            (node.scroll.content.width - node.frame.width + inset.right).max(-inset.left),
        );
        let y = y.clamp(
            -inset.top,
            (node.scroll.content.height - node.frame.height + inset.bottom).max(-inset.top),
        );
        node.scroll.x = x;
        node.scroll.y = y;
        let (count, value, horizontal) = (
            node.scroll.count,
            node.scroll.scroll_value,
            node.scroll.horizontal,
        );
        if let Some(value) = value {
            let mut finished = Vec::new();
            self.animation
                .set(value, if horizontal { x } else { y }, &mut finished);
            if !finished.is_empty() {
                self.events.push(SurfaceEvent::AnimationsFinished(finished));
            }
        }
        self.events.push(SurfaceEvent::Scroll(id, x, y, count));
    }

    fn clamp_scroll(&mut self, id: NodeId) {
        let Some(node) = self.node(id) else {
            return;
        };
        let inset = node.scroll.behavior.content_inset;
        let x = node.scroll.x.clamp(
            -inset.left,
            (node.scroll.content.width - node.frame.width + inset.right).max(-inset.left),
        );
        let y = node.scroll.y.clamp(
            -inset.top,
            (node.scroll.content.height - node.frame.height + inset.bottom).max(-inset.top),
        );
        if (x, y) != (node.scroll.x, node.scroll.y) {
            self.scroll_to(id, x, y);
        }
    }

    pub(crate) fn back(&mut self) {
        let ids = self.visible_ids();
        for id in ids.iter().rev().copied() {
            if let Some(node) = self.node(id)
                && node.kind == ViewKind::Stack
            {
                let screens: Vec<_> = node
                    .children
                    .iter()
                    .copied()
                    .filter(|&c| {
                        self.node(c)
                            .is_some_and(|n| n.kind == ViewKind::Screen && !n.popped)
                    })
                    .collect();
                if screens.len() > 1 {
                    let top = *screens.last().expect("nonempty screens");
                    if self.node(top).is_some_and(|n| n.screen.prevent_back) {
                        self.emit(top, EventKind::BackRequest, EventPayload::None);
                    } else {
                        if let Some(node) = self.node_mut(top) {
                            node.popped = true;
                        }
                        self.emit(top, EventKind::Dismiss, EventPayload::None);
                    }
                    self.reconcile_focus();
                    return;
                }
            }
        }
        if let Some(id) = ids
            .into_iter()
            .rev()
            .find(|&id| self.node(id).is_some_and(|n| n.kind == ViewKind::Modal))
        {
            let dismissible = self.node(id).is_some_and(|n| n.modal.dismissible);
            self.emit(id, EventKind::RequestClose, EventPayload::None);
            if dismissible {
                if let Some(node) = self.node_mut(id) {
                    node.popped = true;
                }
                self.emit(id, EventKind::Dismiss, EventPayload::None);
                self.reconcile_focus();
            }
        }
    }

    /// Advances native animations and polls completed image/canvas jobs.
    /// `now` is a monotonic timestamp in seconds.
    pub fn tick(&mut self, now: f64) -> bool {
        self.now = now;
        let mut finished = Vec::new();
        let mut changed = self.animation.tick(now, &mut finished);
        changed |= self.tick_motion();
        changed |= self.input_tick();
        if !finished.is_empty() {
            self.events.push(SurfaceEvent::AnimationsFinished(finished));
        }
        if self.toast.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.toast = None;
            changed = true;
        }
        changed |= self.poll_graphics();
        changed |= self.tick_images();
        self.update_canvas_args();
        self.schedule_graphics();
        changed
    }

    /// Whether an animation or a temporary overlay needs another frame.
    pub fn animating(&self) -> bool {
        self.animation.is_animating()
            || self.input_pending()
            || self.toast.is_some()
            || !self.canvas_pending.is_empty()
            || !self.pending_images.is_empty()
            || self.protocol.pending()
            || self.nodes.iter().flatten().any(|n| {
                n.motion.as_ref().is_some_and(|m| m.active())
                    || n.image_animation.as_ref().is_some_and(|a| {
                        !a.finished(((self.now - n.image_started).max(0.0) * 1000.0) as u64)
                    })
                    || n.drawing.as_ref().is_some_and(|drawing| {
                        drawing.images().any(|image| {
                            self.image_cache
                                .get(image.uri.as_ref())
                                .is_some_and(|cached| {
                                    !cached.animation.finished(
                                        ((self.now - cached.started).max(0.0) * 1000.0) as u64,
                                    )
                                })
                        })
                    })
            })
    }

    fn is_dying(&self, id: NodeId) -> bool {
        let mut current = Some(id);
        while let Some(id) = current {
            let Some(node) = self.node(id) else {
                break;
            };
            if node.motion.as_ref().is_some_and(|motion| motion.dying()) {
                return true;
            }
            if node.kind == ViewKind::Modal {
                break;
            }
            current = node.parent;
        }
        false
    }

    fn tick_motion(&mut self) -> bool {
        let mut due = Vec::new();
        let mut changed = false;
        for (id, node) in self.nodes.iter_mut().enumerate() {
            let Some(node) = node else {
                continue;
            };
            if let Some(motion) = &mut node.motion {
                changed |= motion.active();
                if motion.advance(self.now) {
                    due.push(id as NodeId);
                } else if !motion.active() {
                    node.motion = None;
                }
            }
        }
        for id in due {
            self.detach(id);
            let mut descendants = vec![id];
            let mut index = 0;
            while index < descendants.len() {
                if let Some(node) = self.node(descendants[index]) {
                    descendants.extend(node.children.iter().copied());
                }
                index += 1;
            }
            for id in descendants.into_iter().rev() {
                if self.node(id).is_some_and(|node| node.delete_pending) {
                    self.nodes[id as usize] = None;
                } else if let Some(node) = self.node_mut(id) {
                    node.motion = None;
                }
            }
        }
        if changed {
            self.reconcile_focus();
        }
        changed
    }

    pub(crate) fn animated_style(&self, id: NodeId) -> ViewStyle {
        let Some(node) = self.node(id) else {
            return ViewStyle::default();
        };
        let mut style = node.motion.as_ref().map_or_else(
            || (*node.style).clone(),
            |motion| {
                motion
                    .sample(node.frame, &node.style, self.now)
                    .style
                    .into_owned()
            },
        );
        let mut transform = Transform::IDENTITY;
        for (index, op) in style.transform.ops().iter().enumerate() {
            let value = node
                .bindings
                .iter()
                .find_map(|(prop, value)| {
                    (*prop == AnimationProp::Transform(index as u8))
                        .then(|| self.animation.eval(*value).number())
                })
                .unwrap_or(op.value);
            transform.push(op.kind, value);
        }
        style.transform = transform;
        for &(prop, value) in &node.bindings {
            let value = self.animation.eval(value);
            let color = match value {
                AnimationValue::Color(color) => Color(color),
                value => Color(value.number() as u32),
            };
            match prop {
                AnimationProp::Opacity => style.opacity = value.number(),
                AnimationProp::BackgroundColor => style.background_color = color,
                AnimationProp::BorderColor => style.borders.colors = [color; 4],
                AnimationProp::ShadowColor => style.shadow.color = color,
                AnimationProp::ShadowOpacity => style.shadow.opacity = value.number(),
                AnimationProp::ShadowRadius => style.shadow.radius = value.number(),
                _ => {}
            }
        }
        if let Some(motion) = &node.motion {
            style.opacity *= motion.appearance(self.now).0;
        }
        style
    }

    /// Nodes in drawing order, including the scenes below a modal.
    pub(crate) fn paint_ids(&self) -> Vec<NodeId> {
        let mut result = Vec::new();
        let mut modals = Vec::new();
        self.collect_layer(ROOT_ID, &mut result, &mut modals, true);
        let mut index = 0;
        while index < modals.len() {
            self.collect_layer(modals[index], &mut result, &mut modals, true);
            index += 1;
        }
        result
    }

    fn collect_layer(
        &self,
        id: NodeId,
        result: &mut Vec<NodeId>,
        modals: &mut Vec<NodeId>,
        layer_root: bool,
    ) {
        let Some(node) = self.node(id) else {
            return;
        };
        if node.popped || node.style.hidden {
            return;
        }
        if node.kind == ViewKind::Modal && !layer_root {
            modals.push(id);
            return;
        }
        result.push(id);
        let top = (node.kind == ViewKind::Stack)
            .then(|| {
                node.children.iter().rev().copied().find(|&child| {
                    self.node(child)
                        .is_some_and(|n| n.kind == ViewKind::Screen && !n.popped)
                })
            })
            .flatten();
        // Header slots are independent layout roots, drawn above screen content.
        let mut headers = Vec::new();
        for &child in &node.children {
            if top.is_some_and(|top| {
                top != child && self.node(child).is_some_and(|n| n.kind == ViewKind::Screen)
            }) {
                continue;
            }
            if node.kind == ViewKind::Screen
                && self
                    .node(child)
                    .is_some_and(|n| n.kind == ViewKind::HeaderItem)
            {
                headers.push(child);
                continue;
            }
            self.collect_layer(child, result, modals, false);
        }
        for child in headers {
            self.collect_layer(child, result, modals, false);
        }
    }

    fn collect_visible(&self, id: NodeId, result: &mut Vec<NodeId>) {
        let Some(node) = self.node(id) else {
            return;
        };
        if node.popped || node.style.hidden || self.is_dying(id) {
            return;
        }
        result.push(id);
        let top_screen = if node.kind == ViewKind::Stack {
            node.children.iter().rev().copied().find(|&child| {
                self.node(child)
                    .is_some_and(|n| n.kind == ViewKind::Screen && !n.popped)
            })
        } else {
            None
        };
        for &child in &node.children {
            if top_screen.is_some_and(|top| {
                top != child && self.node(child).is_some_and(|n| n.kind == ViewKind::Screen)
            }) {
                continue;
            }
            self.collect_visible(child, result);
        }
    }

    /// The newest modal is the root of the active input scope.
    pub(crate) fn visible_ids(&self) -> Vec<NodeId> {
        let ids = self.paint_ids();
        if let Some(modal) = ids
            .iter()
            .rev()
            .copied()
            .find(|&id| self.node(id).is_some_and(|n| n.kind == ViewKind::Modal))
        {
            let mut active = Vec::new();
            self.collect_visible(modal, &mut active);
            active
        } else {
            ids.into_iter().filter(|&id| !self.is_dying(id)).collect()
        }
    }

    fn presentation_frame(&self, id: NodeId) -> Option<Rect> {
        let node = self.node(id)?;
        let mut frame = node.motion.as_ref().map_or(node.frame, |motion| {
            motion.sample(node.frame, &node.style, self.now).frame
        });
        if node.kind == ViewKind::HeaderItem
            && let Some(screen) = node
                .parent
                .and_then(|id| self.node(id))
                .filter(|node| node.kind == ViewKind::Screen)
        {
            let height = screen.screen.header_height.max(self.metrics.height());
            frame.y = ((height - frame.height) / 2.0).max(0.0);
            let edge = self.metrics.width();
            frame.x = if screen.screen.right == Some(id) {
                (screen.frame.width - frame.width - edge).max(0.0)
            } else if screen.screen.title_view == Some(id) {
                ((screen.frame.width - frame.width) / 2.0).max(0.0)
            } else {
                edge
            };
        }
        Some(frame)
    }

    pub(crate) fn global_frame(&self, id: NodeId) -> Option<Rect> {
        let mut frame = self.presentation_frame(id)?;
        let mut current = id;
        loop {
            let node = self.node(current)?;
            let target_frame = self.presentation_frame(current)?;
            let style = self.animated_style(current);
            let mut m = style.transform.matrix(|_| None);
            if let Some(motion) = &node.motion {
                let extra = motion.appearance(self.now).1;
                m = m.mul(&nsh_core::Matrix::of(
                    TransformKind::TranslateY,
                    (extra.shift_y_fraction * target_frame.height) as f64,
                ));
                m = m.mul(&nsh_core::Matrix::of(
                    TransformKind::Rotate,
                    extra.rotate_degrees as f64,
                ));
                m = m.mul(&nsh_core::Matrix::of(
                    TransformKind::Scale,
                    extra.scale as f64,
                ));
            }
            let origin = (
                style.transform_origin.x.resolve(target_frame.width),
                style.transform_origin.y.resolve(target_frame.height),
            );
            let points = [
                (frame.x, frame.y),
                (frame.x + frame.width, frame.y),
                (frame.x, frame.y + frame.height),
                (frame.x + frame.width, frame.y + frame.height),
            ];
            let mut bounds = (
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            );
            for (x, y) in points {
                let x0 = (x - target_frame.x - origin.0) as f64;
                let y0 = (y - target_frame.y - origin.1) as f64;
                let w = x0 * m.0[3] + y0 * m.0[7] + m.0[15];
                let w = if w.abs() < f64::EPSILON { 1.0 } else { w };
                let x =
                    ((x0 * m.0[0] + y0 * m.0[4] + m.0[12]) / w) as f32 + target_frame.x + origin.0;
                let y =
                    ((x0 * m.0[1] + y0 * m.0[5] + m.0[13]) / w) as f32 + target_frame.y + origin.1;
                bounds.0 = bounds.0.min(x);
                bounds.1 = bounds.1.min(y);
                bounds.2 = bounds.2.max(x);
                bounds.3 = bounds.3.max(y);
            }
            frame = Rect {
                x: bounds.0,
                y: bounds.1,
                width: bounds.2 - bounds.0,
                height: bounds.3 - bounds.1,
            };
            if node.kind == ViewKind::Modal {
                break;
            }
            let Some(parent) = node.parent else {
                break;
            };
            let parent_node = self.node(parent)?;
            let sticky = parent_node
                .scroll
                .sticky
                .iter()
                .find(|sticky| sticky.cell == current)
                .map(|sticky| {
                    let offset = if parent_node.scroll.horizontal {
                        parent_node.scroll.x
                    } else {
                        parent_node.scroll.y
                    };
                    (offset - sticky.start).clamp(0.0, (sticky.end - sticky.start).max(0.0))
                })
                .unwrap_or(0.0);
            let parent_frame = self.presentation_frame(parent)?;
            let offset_x = target_frame.x - parent_node.scroll.x
                + if parent_node.scroll.horizontal {
                    sticky
                } else {
                    0.0
                };
            let child_x = if parent_node.scroll.mirrored {
                parent_frame.width - offset_x - target_frame.width
            } else {
                offset_x
            };
            frame.x += parent_frame.x + child_x - target_frame.x;
            frame.y += parent_frame.y - parent_node.scroll.y
                + if parent_node.scroll.horizontal {
                    0.0
                } else {
                    sticky
                };
            current = parent;
        }
        Some(frame)
    }

    pub(crate) fn clip_rect(&self, id: NodeId) -> Rect {
        let viewport = self.viewport();
        let mut clip = Rect {
            x: 0.0,
            y: 0.0,
            width: viewport.width,
            height: viewport.height,
        };
        if self
            .node(id)
            .is_some_and(|node| node.kind == ViewKind::Modal)
        {
            return clip;
        }
        let mut parent = self.node(id).and_then(|node| node.parent);
        while let Some(id) = parent {
            let Some(node) = self.node(id) else {
                break;
            };
            if (node.kind == ViewKind::Scroll || node.style.overflow_hidden)
                && let Some(frame) = self.global_frame(id)
            {
                clip = intersect(clip, frame);
            }
            if node.kind == ViewKind::Modal {
                break;
            }
            parent = node.parent;
        }
        clip
    }

    pub(crate) fn hit_test(&self, x: f32, y: f32) -> Option<NodeId> {
        self.visible_ids().into_iter().rev().find(|&id| {
            let Some(node) = self.node(id) else {
                return false;
            };
            if self.animated_style(id).opacity <= 0.0
                || node.props.pointer_events == PointerEvents::None
                || node.props.pointer_events == PointerEvents::BoxNone
            {
                return false;
            }
            let mut parent = node.parent;
            while let Some(id) = parent {
                let Some(ancestor) = self.node(id) else {
                    return false;
                };
                if matches!(
                    ancestor.props.pointer_events,
                    PointerEvents::None | PointerEvents::BoxOnly
                ) {
                    return false;
                }
                if ancestor.kind == ViewKind::Modal {
                    break;
                }
                parent = ancestor.parent;
            }
            let Some(mut frame) = self.global_frame(id) else {
                return false;
            };
            frame.x -= node.props.hit_slop.left;
            frame.y -= node.props.hit_slop.top;
            frame.width += node.props.hit_slop.left + node.props.hit_slop.right;
            frame.height += node.props.hit_slop.top + node.props.hit_slop.bottom;
            contains(intersect(frame, self.clip_rect(id)), x, y)
        })
    }

    fn dirty_canvas(&mut self, id: NodeId) {
        if self.node(id).is_some_and(|n| n.drawing.is_some()) && !self.canvas_dirty.contains(&id) {
            self.canvas_dirty.push(id);
        }
    }

    fn update_canvas_args(&mut self) {
        let mut dirty = Vec::new();
        for (id, node) in self.nodes.iter_mut().enumerate() {
            let Some(node) = node else {
                continue;
            };
            if node.drawing.is_none() {
                continue;
            }
            let args: Vec<_> = node
                .bindings
                .iter()
                .filter_map(|(prop, value)| match prop {
                    AnimationProp::CanvasArg(slot) => Some((*slot, self.animation.eval(*value))),
                    _ => None,
                })
                .collect();
            if args != node.canvas_args {
                node.canvas_args = args;
                dirty.push(id as NodeId);
            }
        }
        for id in dirty {
            self.dirty_canvas(id);
        }
    }

    fn schedule_graphics(&mut self) {
        let mut remaining = Vec::new();
        for id in std::mem::take(&mut self.canvas_dirty) {
            if self.canvas_pending.contains(&id) {
                remaining.push(id);
                continue;
            }
            let Some(node) = self.node(id) else {
                continue;
            };
            let Some(drawing) = node.drawing.clone() else {
                continue;
            };
            if node.frame.width <= 0.0 || node.frame.height <= 0.0 {
                continue;
            }
            let images = drawing
                .images()
                .filter_map(|image| {
                    self.image_cache.get(image.uri.as_ref()).map(|cached| {
                        (
                            image.uri.clone(),
                            cached.animation.frames[cached.frame].clone(),
                        )
                    })
                })
                .collect();
            self.graphics.canvas(
                id,
                drawing,
                node.canvas_args.clone(),
                node.frame.width,
                node.frame.height,
                images,
            );
            self.canvas_pending.push(id);
        }
        self.canvas_dirty = remaining;
    }

    fn request_image(&mut self, uri: String, node: Option<NodeId>, request: Option<u32>) {
        if let Some(raster) = self
            .image_cache
            .get(&uri)
            .map(|cached| cached.animation.clone())
        {
            if let Some(id) = node {
                self.install_image(id, &uri, raster.clone());
            }
            if let Some(id) = request {
                self.image_answer(id, Ok(&raster.frames[0]));
            }
            return;
        }
        let loading = self.pending_images.contains_key(&uri);
        let pending = self.pending_images.entry(uri.clone()).or_default();
        if let Some(id) = node {
            pending.views.push(id);
        }
        if let Some(id) = request {
            pending.requests.push(id);
        }
        if let Some(id) = node {
            self.emit(id, EventKind::LoadStart, EventPayload::None);
        }
        if !loading {
            self.graphics
                .image(uri, self.assets.clone(), self.access.clone());
        }
    }

    fn install_image(&mut self, id: NodeId, uri: &str, animation: Arc<AnimatedRaster>) {
        let now = self.now;
        if let Some(node) = self.node_mut(id)
            && node
                .image_source
                .as_ref()
                .is_some_and(|source| source.uri.as_ref() == uri)
        {
            let raster = animation.frames[0].clone();
            let payload = EventPayload::Image {
                width: raster.width as f32,
                height: raster.height as f32,
            };
            node.image_raster = Some(raster);
            node.image_animation = Some(animation);
            node.image_started = now;
            self.emit(id, EventKind::Load, payload);
            self.emit(id, EventKind::LoadEnd, EventPayload::None);
        }
    }

    fn image_answer(&mut self, id: u32, result: Result<&Raster, &str>) {
        let mut fields = vec![("id", PayloadValue::Number(id as f64))];
        match result {
            Ok(raster) => {
                fields.push(("width", PayloadValue::Number(raster.width as f64)));
                fields.push(("height", PayloadValue::Number(raster.height as f64)));
            }
            Err(error) => fields.push(("error", PayloadValue::Str(error.to_owned()))),
        }
        self.emit(ROOT_ID, EventKind::ImageInfo, EventPayload::Map(fields));
    }

    fn poll_graphics(&mut self) -> bool {
        let mut changed = false;
        while let Some(result) = self.graphics.poll() {
            changed = true;
            match result {
                crate::worker::GraphicsResult::Protocol(id, sequence, result) => {
                    self.protocol.complete(id, sequence, result)
                }
                crate::worker::GraphicsResult::Image(uri, result) => {
                    let PendingImage {
                        views: waiters,
                        requests,
                    } = self.pending_images.remove(&uri).unwrap_or_default();
                    match result {
                        Ok(raster) => {
                            let raster = Arc::new(raster);
                            // Decoded pixels are bounded independently from compressed assets.
                            let bytes: usize = self
                                .image_cache
                                .values()
                                .map(|cached| cached.animation.bytes())
                                .sum();
                            if bytes + raster.bytes() > 64 * 1024 * 1024 {
                                self.image_cache.clear();
                            }
                            self.image_cache.insert(
                                uri.clone(),
                                CachedImage {
                                    animation: raster.clone(),
                                    started: self.now,
                                    frame: 0,
                                },
                            );
                            for id in waiters {
                                self.install_image(id, &uri, raster.clone());
                            }
                            for id in requests {
                                self.image_answer(id, Ok(&raster.frames[0]));
                            }
                            let canvases: Vec<_> = self
                                .nodes
                                .iter()
                                .enumerate()
                                .filter_map(|(id, n)| {
                                    n.as_ref()
                                        .and_then(|n| n.drawing.as_ref())
                                        .map(|_| id as NodeId)
                                })
                                .collect();
                            for id in canvases {
                                self.dirty_canvas(id);
                            }
                        }
                        Err(error) => {
                            for id in waiters {
                                if self.node(id).is_some_and(|node| {
                                    node.image_source
                                        .as_ref()
                                        .is_some_and(|source| source.uri.as_ref() == uri)
                                }) {
                                    self.emit(
                                        id,
                                        EventKind::Error,
                                        EventPayload::Error {
                                            message: error.clone(),
                                        },
                                    );
                                    self.emit(id, EventKind::LoadEnd, EventPayload::None);
                                }
                            }
                            for id in requests {
                                self.image_answer(id, Err(&error));
                            }
                        }
                    }
                }
                crate::worker::GraphicsResult::Canvas(id, result) => {
                    self.canvas_pending.retain(|&pending| pending != id);
                    match result {
                        Ok(raster) => {
                            if let Some(node) = self.node_mut(id) {
                                node.canvas_raster = Some(Arc::new(raster));
                            }
                        }
                        Err(message) => {
                            self.emit(id, EventKind::Error, EventPayload::Error { message })
                        }
                    }
                }
            }
        }
        changed
    }

    fn tick_images(&mut self) -> bool {
        let mut changed = false;
        for node in self.nodes.iter_mut().flatten() {
            let Some(animation) = &node.image_animation else {
                continue;
            };
            let frame = &animation.frames
                [animation.frame_index(((self.now - node.image_started).max(0.0) * 1000.0) as u64)];
            if node
                .image_raster
                .as_ref()
                .is_none_or(|current| !Arc::ptr_eq(current, frame))
            {
                node.image_raster = Some(frame.clone());
                changed = true;
            }
        }
        let mut canvas_changed = false;
        for cached in self.image_cache.values_mut() {
            let frame = cached
                .animation
                .frame_index(((self.now - cached.started).max(0.0) * 1000.0) as u64);
            if cached.frame != frame {
                cached.frame = frame;
                canvas_changed = true;
            }
        }
        if canvas_changed {
            let ids: Vec<_> = self
                .nodes
                .iter()
                .enumerate()
                .filter_map(|(id, node)| {
                    node.as_ref()
                        .and_then(|n| n.drawing.as_ref())
                        .map(|_| id as NodeId)
                })
                .collect();
            for id in ids {
                self.dirty_canvas(id);
            }
        }
        changed || canvas_changed
    }
}

pub(crate) fn intersect(a: Rect, b: Rect) -> Rect {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    Rect {
        x,
        y,
        width: (a.x + a.width).min(b.x + b.width).max(x) - x,
        height: (a.y + a.height).min(b.y + b.height).max(y) - y,
    }
}

pub(crate) fn contains(rect: Rect, x: f32, y: f32) -> bool {
    x >= rect.x && y >= rect.y && x < rect.x + rect.width && y < rect.y + rect.height
}

#[cfg(test)]
mod tests {
    use nsh_core::{LayoutAnimation, LayoutCurve, LayoutProperty, LayoutTransition};

    use super::*;

    fn create(surface: &mut Surface, id: NodeId, kind: ViewKind, parent: NodeId, frame: Rect) {
        surface.apply(MountBatch {
            items: vec![
                MountItem::Create { id, kind },
                MountItem::Insert {
                    parent,
                    child: id,
                    before: None,
                },
                MountItem::Frame { id, frame },
            ],
        });
    }

    #[test]
    fn canvas_only_image_animation_keeps_embedding_awake_until_done_or_removed() {
        use nsh_core::canvas::{CanvasImage, CanvasOp, Drawing, Num};

        let mut surface = Surface::new(10, 5, CellMetrics::default());
        create(
            &mut surface,
            2,
            ViewKind::Canvas,
            ROOT_ID,
            Rect {
                width: 16.0,
                height: 16.0,
                ..Rect::default()
            },
        );
        let uri: Arc<str> = "asset:/animated.gif".into();
        let animation = Arc::new(AnimatedRaster {
            frames: vec![
                Arc::new(Raster {
                    width: 1,
                    height: 1,
                    rgba: vec![255, 0, 0, 255],
                }),
                Arc::new(Raster {
                    width: 1,
                    height: 1,
                    rgba: vec![0, 0, 255, 255],
                }),
            ],
            delays: vec![40, 60],
            plays: 1,
        });
        surface.image_cache.insert(
            uri.to_string(),
            CachedImage {
                animation,
                started: 1.0,
                frame: 0,
            },
        );
        // Install the retained drawing as after its first completed graphics
        // job: no mounted Image or pending work can keep the host awake.
        surface.node_mut(2).unwrap().drawing = Some(Arc::new(Drawing {
            ops: vec![CanvasOp::DrawImage {
                image: CanvasImage { uri: uri.clone() },
                source: None,
                dest: [0.0, 0.0, 16.0, 16.0].map(Num::Const),
            }],
            slots: Vec::new(),
        }));
        assert!(surface.pending_images.is_empty() && surface.canvas_pending.is_empty());
        assert!(surface.node(2).unwrap().image_animation.is_none());
        surface.now = 1.0;
        assert!(surface.animating());
        surface.now = 1.2;
        assert!(
            !surface.animating(),
            "a completed finite image should let the host idle"
        );

        surface.image_cache.get_mut(uri.as_ref()).unwrap().started = 2.0;
        surface.now = 2.01;
        assert!(surface.animating());
        surface.apply(MountBatch {
            items: vec![MountItem::Delete { id: 2 }],
        });
        assert!(surface.node(2).is_none());
        assert!(surface.image_cache.contains_key(uri.as_ref()));
        assert!(
            !surface.animating(),
            "cached images without live drawing consumers should let the host idle"
        );
    }

    #[test]
    fn shrinking_content_and_growing_viewports_clamp_reported_scrolls() {
        let mut surface = Surface::new(20, 10, CellMetrics::default());
        create(
            &mut surface,
            2,
            ViewKind::Scroll,
            ROOT_ID,
            Rect {
                width: 160.0,
                height: 80.0,
                ..Rect::default()
            },
        );
        surface.apply(MountBatch {
            items: vec![MountItem::ScrollContent {
                id: 2,
                size: Size::new(160.0, 800.0),
            }],
        });
        surface.scroll_to(2, 0.0, 600.0);
        surface.events.clear();
        surface.apply(MountBatch {
            items: vec![MountItem::ScrollContent {
                id: 2,
                size: Size::new(160.0, 112.0),
            }],
        });
        assert_eq!(surface.node(2).unwrap().scroll.y, 32.0);
        assert!(
            surface
                .events
                .iter()
                .any(|event| matches!(event, SurfaceEvent::Scroll(2, 0.0, 32.0, 0)))
        );
        surface.apply(MountBatch {
            items: vec![MountItem::Frame {
                id: 2,
                frame: Rect {
                    width: 160.0,
                    height: 160.0,
                    ..Rect::default()
                },
            }],
        });
        assert_eq!(surface.node(2).unwrap().scroll.y, 0.0);
    }

    #[test]
    fn header_slots_and_mirrored_cells_share_paint_and_hit_geometry() {
        let mut surface = Surface::new(20, 10, CellMetrics::default());
        create(
            &mut surface,
            2,
            ViewKind::Screen,
            ROOT_ID,
            Rect {
                width: 160.0,
                height: 160.0,
                ..Rect::default()
            },
        );
        create(
            &mut surface,
            4,
            ViewKind::HeaderItem,
            2,
            Rect {
                width: 24.0,
                height: 16.0,
                ..Rect::default()
            },
        );
        create(
            &mut surface,
            6,
            ViewKind::HeaderItem,
            2,
            Rect {
                width: 24.0,
                height: 16.0,
                ..Rect::default()
            },
        );
        surface.apply(MountBatch {
            items: vec![MountItem::ScreenConfig {
                id: 2,
                config: Box::new(ScreenConfig {
                    left: Some(4),
                    right: Some(6),
                    header_height: 32.0,
                    ..ScreenConfig::default()
                }),
            }],
        });
        assert_eq!(surface.global_frame(4).unwrap().x, 8.0);
        assert_eq!(surface.global_frame(6).unwrap().x, 128.0);
        assert_eq!(surface.hit_test(132.0, 12.0), Some(6));
        create(
            &mut surface,
            8,
            ViewKind::Scroll,
            ROOT_ID,
            Rect {
                y: 48.0,
                width: 160.0,
                height: 64.0,
                ..Rect::default()
            },
        );
        create(
            &mut surface,
            10,
            ViewKind::View,
            8,
            Rect {
                x: 32.0,
                width: 24.0,
                height: 32.0,
                ..Rect::default()
            },
        );
        create(
            &mut surface,
            12,
            ViewKind::View,
            10,
            Rect {
                x: 8.0,
                width: 8.0,
                height: 16.0,
                ..Rect::default()
            },
        );
        surface.node_mut(8).unwrap().scroll.mirrored = true;
        surface.node_mut(8).unwrap().scroll.x = 16.0;
        assert_eq!(surface.global_frame(10).unwrap().x, 120.0);
        assert_eq!(surface.global_frame(12).unwrap().x, 128.0);
        assert_eq!(surface.hit_test(129.0, 49.0), Some(12));
    }

    #[test]
    fn layout_motion_retains_direct_deletes_and_uses_presented_hit_frames() {
        let mut surface = Surface::new(30, 10, CellMetrics::default());
        create(
            &mut surface,
            2,
            ViewKind::View,
            ROOT_ID,
            Rect {
                width: 32.0,
                height: 32.0,
                ..Rect::default()
            },
        );
        let transition = LayoutTransition {
            curve: LayoutCurve::Linear,
            property: LayoutProperty::Opacity,
            duration_ms: 1000.0,
            delay_ms: 0.0,
        };
        surface.apply(MountBatch {
            items: vec![
                MountItem::LayoutAnimation(LayoutAnimation {
                    update: Some(transition),
                    ..LayoutAnimation::default()
                }),
                MountItem::Frame {
                    id: 2,
                    frame: Rect {
                        x: 80.0,
                        width: 32.0,
                        height: 32.0,
                        ..Rect::default()
                    },
                },
            ],
        });
        surface.tick(0.5);
        assert_eq!(surface.global_frame(2).unwrap().x, 40.0);
        assert_eq!(surface.hit_test(45.0, 8.0), Some(2));
        surface.apply(MountBatch {
            items: vec![
                MountItem::LayoutAnimation(LayoutAnimation {
                    delete: Some(transition),
                    ..LayoutAnimation::default()
                }),
                MountItem::Delete { id: 2 },
            ],
        });
        assert!(surface.node(2).is_some());
        assert!(!surface.visible_ids().contains(&2));
        surface.tick(1.0);
        assert!((surface.animated_style(2).opacity - 0.5).abs() < 0.001);
        surface.tick(1.6);
        assert!(surface.node(2).is_none());
        assert!(!surface.animating());
    }
}

#[cfg(test)]
mod image_ownership_tests {
    use std::time::{Duration, Instant};

    use super::*;

    struct Assets {
        ready: crossbeam_channel::Receiver<()>,
        png: Vec<u8>,
    }
    impl nsh_engine::Assets for Assets {
        fn read(&self, uri: &str) -> Result<Vec<u8>, String> {
            if uri == "asset:/old" {
                self.ready.recv().map_err(|e| e.to_string())?;
                Err("old request failed".into())
            } else {
                Ok(self.png.clone())
            }
        }
    }

    #[test]
    fn a_late_failure_does_not_reach_the_replacement_image() {
        use ::image::ImageEncoder as _;
        let mut png = Vec::new();
        ::image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[255, 0, 0, 255], 1, 1, ::image::ExtendedColorType::Rgba8)
            .unwrap();
        let (ready, wait) = crossbeam_channel::bounded(1);
        let mut surface = Surface::new(10, 5, CellMetrics::default());
        surface.set_assets(Some(Arc::new(Assets { ready: wait, png })));
        surface.apply(MountBatch {
            items: vec![
                MountItem::Create {
                    id: 2,
                    kind: ViewKind::Image,
                },
                MountItem::Insert {
                    parent: ROOT_ID,
                    child: 2,
                    before: None,
                },
                MountItem::Image {
                    id: 2,
                    source: Some(ImageSource {
                        uri: Arc::from("asset:/old"),
                        size: None,
                    }),
                    resize_mode: ResizeMode::Contain,
                    options: Default::default(),
                },
                MountItem::Image {
                    id: 2,
                    source: Some(ImageSource {
                        uri: Arc::from("asset:/new"),
                        size: None,
                    }),
                    resize_mode: ResizeMode::Contain,
                    options: Default::default(),
                },
            ],
        });
        ready.send(()).unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        while surface.node(2).unwrap().image_raster.is_none() {
            assert!(Instant::now() < end, "replacement image did not finish");
            surface.tick(0.0);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            !surface
                .events
                .iter()
                .any(|event| matches!(event, SurfaceEvent::Event(2, EventKind::Error, _)))
        );
        assert_eq!(
            surface
                .events
                .iter()
                .filter(|event| matches!(event, SurfaceEvent::Event(2, EventKind::LoadEnd, _)))
                .count(),
            1
        );
        assert_eq!(
            surface.node(2).unwrap().image_raster.as_ref().unwrap().rgba,
            [255, 0, 0, 255]
        );
    }
}
