//! Tests in windows of GPUI's test platform (`TestAppContext`; feature
//! `shot`, which turns on GPUI's test support): a surface drawn by GPUI
//! and given GPUI's input, with the engine running on its own thread.

use std::{
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App as GpuiApp, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    Styled, TestAppContext, VisualTestContext, Window, div,
};

use crate::{App, AppOptions, AppView, Bundle, NativeComponent};

/// Host functions supplied through the embedding configuration.
#[nsh_core::ui::module]
pub(crate) struct EmbeddingHost;

#[nsh_core::ui::module]
impl EmbeddingHost {
    /// Identifies the registry supplied with the engine configuration.
    pub(crate) fn label() -> String {
        "configured host".to_owned()
    }
}

struct ImageAssets {
    png: Vec<u8>,
    reads: Mutex<Vec<String>>,
}

impl ImageAssets {
    fn new(width: u32, height: u32) -> Self {
        use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(
                &vec![255; (width * height * 4) as usize],
                width,
                height,
                ExtendedColorType::Rgba8,
            )
            .unwrap();
        Self {
            png,
            reads: Mutex::default(),
        }
    }
}

impl nsh_engine::Assets for ImageAssets {
    fn read(&self, uri: &str) -> Result<Vec<u8>, String> {
        self.reads.lock().unwrap().push(uri.to_owned());
        if uri == "asset:/same-preview.png" {
            Ok(self.png.clone())
        } else {
            Err(format!("unexpected asset: {uri}"))
        }
    }
}

const CONFIGURED_IMAGE: &str = r#"
import { Input, mount, signal, View } from "nsh";
import { windowSize } from "nsh/device";
import { Image } from "nsh/image";
import { nativeModule } from "nsh/native";

const { EmbeddingHost } = nativeModule<{ EmbeddingHost: { label(): string } }>("test/embedding");
const label = EmbeddingHost.label();
const policy = eval("'development policy'");
function App() {
  const loaded = signal("waiting");
  return (
    <View style={{ flex: 1, padding: 8 }}>
      <Image
        src="asset:/same-preview.png"
        onLoad={(e) => loaded.set(`${label}|${policy}|${e.width}x${e.height}`)}
        onError={(e) => loaded.set(e.message)}
        style={{ width: 32, height: 32 }}
      />
      <Input
        autoFocus
        value={`${loaded()}|${windowSize().width}x${windowSize().height}`}
        style={{ height: 30 }}
      />
    </View>
  );
}
mount(App);
"#;

const SCOPED_BUTTONS: &str = r#"
import { Input, mount, signal, Text, View } from "nsh";
import { Button, Dialog } from "nsh/base";
import { colorScheme } from "nsh/device";
import { keymap } from "nsh/keymap";
import { nativeComponent } from "nsh/native";

const FocusSquare = nativeComponent<{}, {}>("test.focus-square", { props: [], events: [] });

function App() {
  const log = signal<string[]>([]);
  const open = signal(false);
  const record = (event: string) => log.set([...log(), event]);
  let scope, a, b;
  keymap.bind("f6", () => record(`global:${keymap.focusedWithin(scope)}:${keymap.focusedWithin(a)}:${keymap.focusedWithin(b)}`), { when: "always" });
  return (
    <View style={{ flex: 1, gap: 8 }}>
      <Input testID="report" autoFocus value={`${log().join("|")}@${colorScheme()}`}
        onChange={() => record("unexpected edit")} style={{ height: 30 }} />
      <Button disabled onPress={() => record("disabled")}><Text>Disabled</Text></Button>
      <Button style={{ display: "none" }} onPress={() => record("hidden")}><Text>Hidden</Text></Button>
      <Button focusable={false} onPress={() => record("not tabbable")}><Text>No Tab stop</Text></Button>
      <View ref={(node) => {
        scope = node;
        keymap.bind("f6", () => record(`scope:${keymap.focusedWithin(scope)}:${keymap.focusedWithin(a)}:${keymap.focusedWithin(b)}`),
          { when: "always", within: node });
      }}>
        <Button testID="a" ref={(node) => a = node} onPress={() => record("A")}><Text>A</Text></Button>
      </View>
      <FocusSquare testID="native" style={{ width: 40, height: 24 }} />
      <Button testID="b" ref={(node) => b = node} onPress={() => record("B")}><Text>B</Text></Button>
      <Button testID="opener" onPress={() => { record("open"); open.set(true); }}><Text>Open dialog</Text></Button>
      <Dialog open={open()} animation="none" onOpen={() => record("shown")}
        onClose={() => record("closed")} onOpenChange={(next) => open.set(next)}>
        <Button testID="modal" onPress={() => record("modal")}><Text>Modal action</Text></Button>
      </Dialog>
    </View>
  );
}
mount(App);
"#;

struct FocusSquare {
    focus: Option<FocusHandle>,
}

impl NativeComponent for FocusSquare {
    fn render(&mut self, _: &mut Window, cx: &mut GpuiApp) -> Option<AnyElement> {
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        Some(
            div()
                .id("focus-square")
                .track_focus(&focus)
                .size_full()
                .into_any_element(),
        )
    }

    fn focus_handle(&self) -> Option<FocusHandle> {
        self.focus.clone()
    }

    fn set_prop(
        &mut self,
        _: &str,
        _: Option<&serde_json::Value>,
        _: &mut Window,
        _: &mut GpuiApp,
    ) {
    }
}

/// A native component that only draws a box.
struct Square;

impl NativeComponent for Square {
    fn render(&mut self, _: &mut Window, _: &mut GpuiApp) -> Option<AnyElement> {
        Some(div().size_full().into_any_element())
    }

    fn set_prop(
        &mut self,
        _: &str,
        _: Option<&serde_json::Value>,
        _: &mut Window,
        _: &mut GpuiApp,
    ) {
    }
}

/// A component and, after it, a focused field.
const FIELD_AFTER_COMPONENT: &str = r#"
import { Input, mount, signal, View } from "nsh";
import { nativeComponent } from "nsh/native";

const Square = nativeComponent<{}, {}>("test.square", { props: [], events: [] });

function App() {
  const text = signal("abcd");
  return (
    <View style={{ flex: 1, padding: 8, gap: 8 }}>
      <Square style={{ width: 40, height: 20 }} />
      <Input autoFocus value={text()} onChange={(t) => text.set(t)} style={{ height: 30 }} />
    </View>
  );
}
mount(App);
"#;

/// A focused field that logs its input method's compositions and sets
/// values while they are in progress: "held" on "ni" (applied once the
/// composition is cancelled), "stale" on "hao" (dropped, as "好" is
/// committed after it); the log becomes the text when "hao" ends.
const COMPOSING_FIELD: &str = r#"
import { Input, mount, signal, View } from "nsh";

function App() {
  const text = signal("abcd");
  const log: string[] = [];
  return (
    <View style={{ flex: 1, padding: 8 }}>
      <Input
        autoFocus
        value={text()}
        onChange={(t) => {
          log.push("change " + t);
          text.set(t);
        }}
        onCompositionChange={(e) => {
          log.push(e.composing ? "compose " + e.text : "end");
          if (e.text === "ni") text.set("held");
          if (e.text === "hao") text.set("stale");
          if (!e.composing && log.length > 2) text.set(log.join("|"));
        }}
        style={{ height: 30 }}
      />
    </View>
  );
}
mount(App);
"#;

/// The focused field's text.
fn field_text(view: &Entity<AppView>, cx: &mut VisualTestContext) -> Option<String> {
    view.read_with(cx, |view, _| {
        crate::text_input::focused(view.surface())
            .and_then(|field| crate::text_input::accessible_value(&field))
            .map(|value| value.text)
    })
}

/// The value the focused field holds until its composition ends.
fn field_pending(view: &Entity<AppView>, cx: &mut VisualTestContext) -> Option<String> {
    view.read_with(cx, |view, _| {
        crate::text_input::focused(view.surface())
            .and_then(|field| crate::text_input::pending_value(&field))
    })
}

/// Runs the window until the focused field holds `text` for after its
/// composition.
fn wait_for_pending(view: &Entity<AppView>, cx: &mut VisualTestContext, text: &str) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        let now = field_pending(view, cx);
        if now.as_deref() == Some(text) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the field holds {now:?}, not {text:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// An input method's step on the focused field.
fn ime(
    view: &Entity<AppView>,
    cx: &mut VisualTestContext,
    f: impl FnOnce(&Rc<crate::surface::Surface>),
) {
    view.update(cx, |view, _| f(view.surface()));
}

/// Runs the window (and waits for the engine's thread) until the focused
/// field reads `text`.
fn wait_for(view: &Entity<AppView>, cx: &mut VisualTestContext, text: &str) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        let now = field_text(view, cx);
        if now.as_deref() == Some(text) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the field reads {now:?}, not {text:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The tests in windows, one after the other in one app: the main
/// thread's queue (`crate::rt`) is made once per process, for the first
/// app, so every window's engine must post to that app.
#[gpui::test]
fn windows(cx: &mut TestAppContext) {
    // The engine (and the settings portal) run on threads of their own.
    cx.executor().allow_parking();
    keys_reach_the_fields_of_a_surface_showing_a_native_component(cx);
    compositions_are_reported_and_hold_values_until_they_end(cx);
    configured_assets_stay_with_their_original_load(cx);
    buttons_keep_native_focus_and_live_key_scopes(cx);
}

fn find_test_view(view: &crate::view::NshView, id: &str) -> Option<crate::view::NshView> {
    if view.state().props.test_id.as_deref() == Some(id) {
        return Some(view.clone());
    }
    view.with_children(|children| children.iter().find_map(|child| find_test_view(child, id)))
}

fn wait_for_report(view: &Entity<AppView>, cx: &mut VisualTestContext, expected: &str) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        let current = view.read_with(cx, |view, _| {
            find_test_view(&view.surface().root, "report")
                .and_then(|view| crate::text_input::accessible_value(&view))
                .map(|value| value.text)
        });
        if current.as_deref() == Some(expected) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "report {current:?}, expected {expected:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_button(view: &Entity<AppView>, cx: &mut VisualTestContext, id: &str) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        let focused = view.read_with(cx, |view, _| {
            let surface = view.surface();
            let generic = crate::text_input::focused_view(surface);
            let native = crate::native::focused_component(surface).and_then(|id| surface.view(id));
            let owns = if id == "native" {
                generic.is_none()
                    && native.is_some_and(|view| view.state().props.test_id.as_deref() == Some(id))
            } else {
                native.is_none()
                    && generic.is_some_and(|view| view.state().props.test_id.as_deref() == Some(id))
            };
            owns && crate::text_input::focused(surface).is_none()
                && !crate::text_input::wants_text(surface)
        });
        if focused {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "button {id} did not take native focus: {}",
            view.read_with(cx, |view, _| focus_diagnostic(view.surface()))
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn focus_diagnostic(surface: &crate::surface::Surface) -> String {
    fn nodes(view: &crate::view::NshView, out: &mut Vec<String>) {
        let s = view.state();
        out.push(format!(
            "{}:{:?}:test={:?}:disabled={}:hidden={}:focusable={:?}:press={}:input={}",
            s.id,
            s.kind,
            s.props.test_id,
            s.props.accessibility.state.disabled,
            s.style.hidden || s.concealed,
            s.props.focusable,
            s.events.contains(nsh_core::EventKind::Press),
            s.input.is_some(),
        ));
        drop(s);
        view.with_children(|children| {
            for child in children {
                nodes(child, out);
            }
        });
    }
    let generic = crate::text_input::focused_view(surface).map(|view| view.id());
    let native = crate::native::focused_component(surface);
    let mut tree = Vec::new();
    nodes(&surface.root, &mut tree);
    format!(
        "generic={generic:?} native={native:?} path={:?} tree={tree:?}",
        surface.focus_path()
    )
}

fn buttons_keep_native_focus_and_live_key_scopes(cx: &mut TestAppContext) {
    let code =
        nsh_bundler::compile_module("App.tsx", SCOPED_BUTTONS, &[nsh_base::registry().into()])
            .unwrap();
    let (view, cx) =
        cx.add_window_view(|window, cx| AppView::new(AppOptions::default(), window, cx));
    view.update(cx, |view, cx| {
        view.register_component(
            "test.focus-square",
            Rc::new(|_| Box::new(FocusSquare { focus: None })),
        );
        view.set_appearance(nsh_core::platform::Appearance::Light, cx);
        view.load(App::Js(Bundle::source(code.clone())));
    });
    cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
    wait_for(&view, cx, "@light");
    cx.simulate_keystrokes("tab");
    wait_for_button(&view, cx, "a");
    cx.simulate_keystrokes("x enter space f6");
    wait_for_report(&view, cx, "A|A|scope:true:true:false@light");
    cx.simulate_keystrokes("tab");
    wait_for_button(&view, cx, "native");
    cx.simulate_keystrokes("tab");
    wait_for_button(&view, cx, "b");
    cx.simulate_keystrokes("enter f6");
    wait_for_report(
        &view,
        cx,
        "A|A|scope:true:true:false|B|global:false:false:true@light",
    );
    cx.simulate_keystrokes("tab");
    wait_for_button(&view, cx, "opener");
    cx.simulate_keystrokes("enter");
    let mut report = "A|A|scope:true:true:false|B|global:false:false:true|open|shown".to_owned();
    wait_for_report(&view, cx, &format!("{report}@light"));
    wait_for_button(&view, cx, "modal");
    cx.simulate_keystrokes("tab");
    wait_for_button(&view, cx, "modal");
    cx.simulate_keystrokes("enter escape");
    report.push_str("|modal|closed");
    wait_for_report(&view, cx, &format!("{report}@light"));
    wait_for_button(&view, cx, "opener");
    cx.simulate_keystrokes("enter");
    report.push_str("|open|shown");
    wait_for_report(&view, cx, &format!("{report}@light"));
    wait_for_button(&view, cx, "modal");
    cx.simulate_keystrokes("escape");
    report.push_str("|closed");
    wait_for_report(&view, cx, &format!("{report}@light"));
    view.update(cx, |view, cx| {
        view.set_appearance(nsh_core::platform::Appearance::Dark, cx)
    });
    wait_for_report(&view, cx, &format!("{report}@dark"));
    cx.simulate_keystrokes("f6");
    report.push_str("|global:false:false:false");
    wait_for_report(&view, cx, &format!("{report}@dark"));
    view.update(cx, |view, _| view.load(App::Js(Bundle::source(code))));
    wait_for(&view, cx, "@dark");
    cx.simulate_keystrokes("tab f6");
    wait_for_report(&view, cx, "scope:true:true:false@dark");
    view.update(cx, |view, _| view.stop());
}

fn configured_assets_stay_with_their_original_load(cx: &mut TestAppContext) {
    let code = nsh_bundler::compile_module("App.tsx", CONFIGURED_IMAGE, &[]).unwrap();
    let (view, cx) =
        cx.add_window_view(|window, cx| AppView::new(AppOptions::default(), window, cx));
    let first = Arc::new(ImageAssets::new(2, 3));
    let second = Arc::new(ImageAssets::new(7, 5));
    for (assets, status) in [
        (Some(&first), "configured host|development policy|2x3"),
        (Some(&second), "configured host|development policy|7x5"),
        (None, "asset:/same-preview.png: no asset reader configured"),
    ] {
        let mut registry = nsh_core::ui::Registry::ui_set("test/embedding");
        registry.module::<EmbeddingHost>();
        let mut config = nsh_engine::EngineConfig::new(App::Js(
            Bundle::source(code.clone()).with_policy(nsh_engine::Policy::development()),
        ));
        config.assets = assets
            .cloned()
            .map(|assets| -> Arc<dyn nsh_engine::Assets> { assets });
        config.ui_sets = vec![registry];
        config.viewport = nsh_core::Size::new(nsh_core::Points(1.0), nsh_core::Points(1.0));
        config.environment.window = config.viewport;
        view.update(cx, |view, _| view.load_with_config(config));
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        let start = Instant::now();
        loop {
            cx.run_until_parked();
            let (width, height) = view.read_with(cx, |view, _| view.surface().size());
            let expected = format!("{status}|{width}x{height}");
            let now = field_text(&view, cx);
            if width > 1.0 && height > 1.0 && now.as_deref() == Some(expected.as_str()) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the image reports {now:?}, not {expected:?} on the actual GPUI surface"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    for assets in [&first, &second] {
        assert_eq!(
            *assets.reads.lock().unwrap(),
            ["asset:/same-preview.png"],
            "each provider is read once; the load with no reader must use neither old provider",
        );
    }
    view.update(cx, |view, _| view.stop());
}

fn keys_reach_the_fields_of_a_surface_showing_a_native_component(cx: &mut TestAppContext) {
    let code = nsh_bundler::compile_module("App.tsx", FIELD_AFTER_COMPONENT, &[]).unwrap();
    let (view, cx) =
        cx.add_window_view(|window, cx| AppView::new(AppOptions::default(), window, cx));
    view.update(cx, |view, _| {
        view.register_component("test.square", Rc::new(|_| Box::new(Square)));
        view.load(App::Js(Bundle::source(code)));
    });
    cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
    wait_for(&view, cx, "abcd");
    // Painting the component's element must not take the surface's key
    // listeners with it: deletions and moves come as key events.
    cx.simulate_keystrokes("end backspace");
    wait_for(&view, cx, "abc");
    cx.simulate_keystrokes("home delete");
    wait_for(&view, cx, "bc");
    cx.simulate_keystrokes("right shift-right backspace");
    wait_for(&view, cx, "b");
}

fn compositions_are_reported_and_hold_values_until_they_end(cx: &mut TestAppContext) {
    let code = nsh_bundler::compile_module("App.tsx", COMPOSING_FIELD, &[]).unwrap();
    let (view, cx) =
        cx.add_window_view(|window, cx| AppView::new(AppOptions::default(), window, cx));
    view.update(cx, |view, _| view.load(App::Js(Bundle::source(code))));
    cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
    wait_for(&view, cx, "abcd");
    cx.simulate_keystrokes("end");
    // The same composition twice is reported once; the value JS sets
    // meanwhile waits, the text keeps what the input method composes in.
    ime(&view, cx, |s| crate::text_input::mark(s, "ni"));
    ime(&view, cx, |s| crate::text_input::mark(s, "ni"));
    wait_for_pending(&view, cx, "held");
    assert_eq!(field_text(&view, cx).as_deref(), Some("abcd"));
    // Cancelled: the value held is applied as the composition ends.
    ime(&view, cx, |s| crate::text_input::mark(s, ""));
    assert_eq!(field_text(&view, cx).as_deref(), Some("held"));
    assert_eq!(field_pending(&view, cx), None);
    // Committed: the commit's `onChange` comes before the end, and the
    // value set before the commit is stale.
    ime(&view, cx, |s| crate::text_input::mark(s, "hao"));
    wait_for_pending(&view, cx, "stale");
    ime(&view, cx, |s| crate::text_input::char_input(s, "好"));
    assert_eq!(field_text(&view, cx).as_deref(), Some("held好"));
    wait_for(&view, cx, "compose ni|end|compose hao|change held好|end");
}
