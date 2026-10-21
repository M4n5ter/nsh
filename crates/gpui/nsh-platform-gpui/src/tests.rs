//! Tests in windows of GPUI's test platform (`TestAppContext`; feature
//! `shot`, which turns on GPUI's test support): a surface drawn by GPUI
//! and given GPUI's input, with the engine running on its own thread.

use std::{
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App as GpuiApp, Entity, Focusable, IntoElement, Styled, TestAppContext,
    VisualTestContext, Window, div,
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
        config.assets = assets.map(|assets| -> Arc<dyn nsh_engine::Assets> { assets.clone() });
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
