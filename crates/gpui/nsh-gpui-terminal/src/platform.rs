use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Instant,
};

use anyhow::{Result, anyhow, ensure};
use futures_channel::oneshot;
use gpui::{
    Action, ActivityGuard, AnyWindowHandle, BackgroundExecutor, ClipboardItem, CursorStyle,
    DevicePixels, DispatchEventResult, DummyKeyboardMapper, ForegroundExecutor, Keymap, Menu,
    MenuItem, OwnedMenu, PathPromptOptions, Platform, PlatformDisplay, PlatformHeadlessRenderer,
    PlatformInput, PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem,
    PlatformWindow, Size, Task, ThermalState, WindowAppearance, WindowParams, WindowVisibility,
};

use crate::{
    CandidateFrame, FrameToken,
    dispatcher::Dispatcher,
    window::{TerminalWindow, WindowRef, WindowState},
};

type Wake = Arc<dyn Fn() + Send + Sync>;
type PixelSink = Box<dyn FnMut(CandidateFrame)>;

#[derive(Default)]
struct Callbacks {
    quit: Option<Box<dyn FnMut() -> bool>>,
    reopen: Option<Box<dyn FnMut()>>,
    sleep: Option<Box<dyn FnMut()>>,
    wake: Option<Box<dyn FnMut()>>,
    displays: Option<Box<dyn FnMut()>>,
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    menu: Option<Box<dyn FnMut(&dyn Action)>>,
    open_menu: Option<Box<dyn FnMut()>>,
    validate_menu: Option<Box<dyn FnMut(&dyn Action) -> bool>>,
    keyboard: Option<Box<dyn FnMut()>>,
    thermal: Option<Box<dyn FnMut()>>,
}

/// One GPUI surface driven by an external terminal host's main-thread loop.
///
/// This owns no terminal input or output. Keep the `ApplicationHandle` returned
/// by `Application::run_embedded`, call [`Self::pump`] when woken, and send
/// candidates to the host's sole writer. A candidate is not a presentation.
pub struct TerminalPlatform {
    dispatcher: Arc<Dispatcher>,
    background: BackgroundExecutor,
    foreground: ForegroundExecutor,
    text: Arc<dyn PlatformTextSystem>,
    resources: RefCell<Option<(Box<dyn PlatformHeadlessRenderer>, PixelSink)>>,
    window: RefCell<Option<WindowRef>>,
    callbacks: RefCell<Callbacks>,
    started: Cell<bool>,
    pending_quit: Cell<bool>,
    quit: Cell<bool>,
    error: RefCell<Option<String>>,
    clipboard: RefCell<Option<ClipboardItem>>,
    clipboard_write: RefCell<Option<ClipboardItem>>,
    menus: RefCell<Vec<OwnedMenu>>,
    cursor: Cell<CursorStyle>,
    cursor_visible: Cell<bool>,
    wake: Wake,
}

impl TerminalPlatform {
    /// Creates a real text/scene platform. `wake` schedules the external loop;
    /// it must not enter GPUI synchronously from a callback or worker thread.
    pub fn new(
        renderer: Box<dyn PlatformHeadlessRenderer>,
        sink: Box<dyn FnMut(CandidateFrame)>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Rc<Self>> {
        let dispatcher = Dispatcher::new(wake.clone())?;
        Ok(Rc::new(Self {
            background: BackgroundExecutor::new(dispatcher.clone()),
            foreground: ForegroundExecutor::new(dispatcher.clone()),
            dispatcher,
            text: Arc::new(gpui_wgpu::CosmicTextSystem::new("DejaVu Sans")),
            resources: RefCell::new(Some((renderer, sink))),
            window: RefCell::new(None),
            callbacks: RefCell::new(Callbacks::default()),
            started: Cell::new(false),
            pending_quit: Cell::new(false),
            quit: Cell::new(false),
            error: RefCell::new(None),
            clipboard: RefCell::new(None),
            clipboard_write: RefCell::new(None),
            menus: RefCell::new(Vec::new()),
            cursor: Cell::new(CursorStyle::Arrow),
            cursor_visible: Cell::new(true),
            wake,
        }))
    }

    fn window(&self) -> Result<Rc<RefCell<WindowState>>> {
        self.window
            .borrow()
            .as_ref()
            .and_then(WindowRef::upgrade)
            .filter(|window| !window.borrow().is_closed())
            .ok_or_else(|| anyhow!("terminal GPUI window is not open"))
    }

    fn unsupported(&self, capability: &str) {
        self.error
            .borrow_mut()
            .get_or_insert_with(|| format!("terminal GPUI does not support {capability}"));
    }

    /// Runs at most 64 foreground tasks and one window frame on the creating
    /// thread. Pending work re-wakes the host. Terminal IO remains external;
    /// this first renderer performs synchronous GPU readback during a draw.
    pub fn pump(&self) -> bool {
        let mut worked = self.dispatcher.pump();
        if let Ok(window) = self.window() {
            worked |= WindowState::pump(&window);
        } else {
            if self.window.borrow_mut().take().is_some() {
                self.pending_quit.set(true);
            }
        }
        if self.pending_quit.replace(false) && !self.quit.get() {
            let callback = self.callbacks.borrow_mut().quit.take();
            let allowed = if let Some(mut callback) = callback {
                let allowed = callback();
                self.callbacks.borrow_mut().quit.get_or_insert(callback);
                allowed
            } else {
                true
            };
            self.quit.set(allowed);
            worked = true;
        }
        if self.has_pending() {
            (self.wake)();
        }
        worked
    }

    /// Whether an immediate pump is needed, independent of future deadlines.
    pub fn has_pending(&self) -> bool {
        self.dispatcher.has_pending()
            || self.pending_quit.get()
            || self
                .window()
                .is_ok_and(|window| window.borrow().has_pending())
    }

    /// Earliest delayed dispatcher task. Native GPUI futures also wake the
    /// host through the existing async-io timer reactor.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.dispatcher.next_deadline()
    }

    /// Whether GPUI has accepted a quit request on the main-thread pump.
    pub fn is_quit_requested(&self) -> bool {
        self.quit.get()
    }

    /// Returns the first pending rendering or unsupported-service error.
    pub fn take_error(&self) -> Option<String> {
        self.error
            .borrow_mut()
            .take()
            .or_else(|| self.window().ok()?.borrow_mut().take_error())
    }

    /// Changes the real pixel viewport. Old presentation receipts become
    /// invalid immediately; GPUI's resize callback runs on the next pump.
    pub fn resize(&self, size: Size<DevicePixels>) -> Result<()> {
        self.window()?.borrow_mut().resize(size)?;
        (self.wake)();
        Ok(())
    }

    /// Reports terminal focus tracking without changing native field focus.
    pub fn set_active(&self, active: bool) -> Result<()> {
        self.window()?.borrow_mut().set_active(active);
        (self.wake)();
        Ok(())
    }

    /// Delivers already-admitted native input through GPUI's actual callback.
    /// The external host must establish its original-owner authority first.
    pub fn dispatch_input(&self, input: PlatformInput) -> Result<DispatchEventResult> {
        let window = self.window()?;
        if matches!(input, PlatformInput::MouseMove(_)) {
            self.cursor_visible.set(true);
        }
        Ok(WindowState::dispatch_input(&window, input))
    }

    /// Exact-presentation transport for opaque input only. Ordinary press and
    /// wheel routes must use the native admission peer rather than this check.
    pub fn dispatch_input_exact(&self, token: FrameToken, input: PlatformInput) -> bool {
        let Ok(window) = self.window() else {
            return false;
        };
        if !window.borrow().has_exact_presentation(&token) {
            return false;
        }
        self.dispatch_input(input).is_ok()
    }

    /// Commits one already-admitted bracketed paste through the current GPUI
    /// input handler. The host verifies the original editor before calling.
    pub fn paste(&self, text: &str) -> Result<()> {
        WindowState::paste(&self.window()?, text);
        Ok(())
    }

    /// Records a receipt from the host's trusted sole writer. This confirms
    /// transport completion, not the authority of an event listener.
    pub fn mark_presented(&self, token: FrameToken) -> Result<()> {
        self.window()?.borrow_mut().mark_presented(token)
    }

    /// Last accepted writer receipt, if its viewport remains current.
    pub fn presented(&self) -> Option<FrameToken> {
        self.window().ok()?.borrow().published()
    }

    /// Requests the actual registered GPUI close path, respecting its veto.
    pub fn request_close(&self) -> Result<()> {
        WindowState::request_close(&self.window()?);
        Ok(())
    }

    /// A local clipboard write for the host to forward through its existing
    /// terminal clipboard transport. Reading does not query the OS clipboard.
    pub fn take_clipboard_write(&self) -> Option<ClipboardItem> {
        self.clipboard_write.borrow_mut().take()
    }

    /// Cancels foreground work on its owner thread. Call after dropping the
    /// embedded application; background work cannot delay terminal cleanup.
    pub fn shutdown(&self) {
        self.dispatcher.shutdown();
    }
}

impl Drop for TerminalPlatform {
    fn drop(&mut self) {
        self.dispatcher.shutdown();
    }
}

fn unsupported_reply<T>(capability: &str) -> oneshot::Receiver<Result<T>> {
    let (send, receive) = oneshot::channel();
    let _ = send.send(Err(anyhow!("terminal GPUI does not support {capability}")));
    receive
}

struct TerminalKeyboardLayout;

impl PlatformKeyboardLayout for TerminalKeyboardLayout {
    fn id(&self) -> &str {
        "terminal-decoded"
    }
    fn name(&self) -> &str {
        "Terminal decoded keys"
    }
}

impl Platform for TerminalPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.background.clone()
    }
    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground.clone()
    }
    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text.clone()
    }
    fn run(&self, launch: Box<dyn FnOnce()>) {
        if self.started.replace(true) {
            self.unsupported("launching the same platform twice");
            return;
        }
        launch();
        (self.wake)();
    }
    fn quit(&self) {
        self.pending_quit.set(true);
        (self.wake)();
    }
    fn restart(&self, _: Option<PathBuf>, _: Vec<OsString>) {
        self.unsupported("process restart");
    }
    fn activate(&self, _: bool) {
        let _ = self.set_active(true);
    }
    fn hide(&self) {
        if let Ok(window) = self.window() {
            window.borrow_mut().set_visibility(WindowVisibility::Hidden);
        }
        (self.wake)();
    }
    fn hide_other_apps(&self) {
        self.unsupported("hiding other applications");
    }
    fn unhide_other_apps(&self) {
        self.unsupported("showing other applications");
    }
    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        Vec::new()
    }
    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        None
    }
    fn on_displays_changed(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().displays = Some(callback);
    }
    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.window().ok().and_then(|window| {
            let window = window.borrow();
            window.active().then_some(window.handle)
        })
    }
    fn open_window(
        &self,
        handle: AnyWindowHandle,
        params: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        ensure!(
            self.window.borrow().is_none(),
            "terminal GPUI supports one embedded window"
        );
        let (renderer, sink) = self
            .resources
            .borrow_mut()
            .take()
            .ok_or_else(|| anyhow!("terminal renderer already belongs to a window"))?;
        let (window, weak) =
            TerminalWindow::new(handle, params, renderer, sink, self.wake.clone())?;
        *self.window.borrow_mut() = Some(weak);
        Ok(Box::new(window))
    }
    fn window_appearance(&self) -> WindowAppearance {
        WindowAppearance::Dark
    }
    fn open_url(&self, _: &str) {
        self.unsupported("opening external URLs");
    }
    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.callbacks.borrow_mut().open_urls = Some(callback);
    }
    fn register_url_scheme(&self, _: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow!("terminal GPUI cannot register URL schemes")))
    }
    fn prompt_for_paths(
        &self,
        _: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        unsupported_reply("native path prompts")
    }
    fn prompt_for_new_path(
        &self,
        _: &Path,
        _: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        unsupported_reply("native path prompts")
    }
    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }
    fn reveal_path(&self, _: &Path) {
        self.unsupported("native file reveal");
    }
    fn open_with_system(&self, _: &Path) {
        self.unsupported("opening external applications");
    }
    fn on_quit(&self, callback: Box<dyn FnMut() -> bool>) {
        self.callbacks.borrow_mut().quit = Some(callback);
    }
    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().reopen = Some(callback);
    }
    fn on_system_sleep(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().sleep = Some(callback);
    }
    fn on_system_wake(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().wake = Some(callback);
    }
    fn set_menus(&self, menus: Vec<Menu>, _: &Keymap) {
        *self.menus.borrow_mut() = menus.into_iter().map(|menu| menu.owned()).collect();
    }
    fn get_menus(&self) -> Option<Vec<OwnedMenu>> {
        Some(self.menus.borrow().clone())
    }
    fn set_dock_menu(&self, _: Vec<MenuItem>, _: &Keymap) {}
    fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>) {
        self.callbacks.borrow_mut().menu = Some(callback);
    }
    fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().open_menu = Some(callback);
    }
    fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        self.callbacks.borrow_mut().validate_menu = Some(callback);
    }
    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }
    fn on_thermal_state_change(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().thermal = Some(callback);
    }
    fn prevent_idle_sleep(&self, _: &str) -> Task<Result<ActivityGuard>> {
        Task::ready(Err(anyhow!("terminal GPUI cannot prevent OS sleep")))
    }
    fn app_path(&self) -> Result<PathBuf> {
        Ok(std::env::current_exe()?)
    }
    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        ensure!(
            Path::new(name).components().count() == 1,
            "auxiliary executable must be a file name"
        );
        let executable = std::env::current_exe()?;
        Ok(executable
            .parent()
            .ok_or_else(|| anyhow!("executable has no directory"))?
            .join(name))
    }
    fn set_cursor_style(&self, style: CursorStyle) {
        self.cursor.set(style);
    }
    fn hide_cursor_until_mouse_moves(&self) {
        self.cursor_visible.set(false);
    }
    fn is_cursor_visible(&self) -> bool {
        self.cursor_visible.get()
    }
    fn should_auto_hide_scrollbars(&self) -> bool {
        false
    }
    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        self.clipboard.borrow().clone()
    }
    fn write_to_clipboard(&self, item: ClipboardItem) {
        *self.clipboard.borrow_mut() = Some(item.clone());
        *self.clipboard_write.borrow_mut() = Some(item);
        (self.wake)();
    }
    fn read_from_primary(&self) -> Option<ClipboardItem> {
        None
    }
    fn write_to_primary(&self, _: ClipboardItem) {}
    fn write_credentials(&self, _: &str, _: &str, _: &[u8]) -> Task<Result<()>> {
        Task::ready(Err(anyhow!("terminal GPUI has no OS credential store")))
    }
    fn read_credentials(&self, _: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Err(anyhow!("terminal GPUI has no OS credential store")))
    }
    fn delete_credentials(&self, _: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow!("terminal GPUI has no OS credential store")))
    }
    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(TerminalKeyboardLayout)
    }
    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }
    fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().keyboard = Some(callback);
    }
}
