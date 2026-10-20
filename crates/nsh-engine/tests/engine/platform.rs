//! The OS the platform reports decides `Platform` and which platform-only
//! props (`ios={{…}}`, `android={{…}}`, `desktop={{…}}`, `web={{…}}`)
//! apply; on the web, the idiom decides between mobile and desktop.

use std::time::Duration;

use nsh_core::{ApproxTextEngine, MountBatch, MountItem, Size, geometry::px};
use nsh_engine::{Bundle, Engine, EngineConfig, Environment, Idiom, Os, Renderer};

use crate::ChannelSink;

const APP: &str = r#"import { mount, Text, View } from "nsh";
import { Platform } from "nsh/device";

mount(
  () => (
    <View
      ios={{ accessibilityLabel: "ios" }}
      android={{ accessibilityLabel: "android" }}
      desktop={{ accessibilityLabel: "desktop" }}
      web={{ accessibilityHint: "web" }}
    >
      <Text>{`${Platform.OS} ${Platform.isDesktop} ${Platform.select({ ios: "i", mobile: "m", desktop: "d" })} ${Platform.select({ native: "n", default: "-" })}`}</Text>
    </View>
  ),
);
"#;

/// The first frame's texts and accessibility labels and hints on `os`.
fn first_frame(os: Os, idiom: Idiom) -> (Vec<String>, Vec<String>) {
    frame_parts(first_batch(
        Environment {
            os,
            idiom,
            renderer: if os == Os::Web {
                Renderer::Web
            } else {
                Renderer::Native
            },
            ..Environment::default()
        },
        APP,
    ))
}

fn first_batch(environment: Environment, source: &str) -> MountBatch {
    let Environment {
        os,
        idiom,
        renderer,
        ..
    } = environment;
    let dir = std::env::temp_dir().join(format!(
        "nsh-platform-{}-{}-{idiom:?}-{renderer:?}",
        std::process::id(),
        os.name()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("App.tsx");
    std::fs::write(&entry, source).unwrap();
    let bundle = nsh_bundler::bundle(&nsh_bundler::BundleOptions {
        entry,
        minify: false,
        split: false,
        ui_sets: vec![nsh_base::registry().into()],
    })
    .unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    let config = EngineConfig {
        app: nsh_engine::App::Js(Bundle::source(bundle.code)),
        viewport: Size::new(px(320.0), px(480.0)),
        trace: false,
        network: None,
        websocket: None,
        filesystem: None,
        assets: None,
        environment,
        header_heights: (nsh_core::Points(44.0), nsh_core::Points(96.0)),
        limits: nsh_engine::Limits::default(),
        ui_sets: Vec::new(),
        runtime: None,
        data_dir: None,
        storage: None,
        clock: Default::default(),
    };
    let _engine = Engine::start(config, || ApproxTextEngine, ChannelSink(tx)).unwrap();
    rx.recv_timeout(Duration::from_secs(5))
        .expect("first frame")
}

fn frame_parts(batch: MountBatch) -> (Vec<String>, Vec<String>) {
    let mut texts = Vec::new();
    let mut labels = Vec::new();
    for item in &batch.items {
        match item {
            MountItem::Text { text, .. } => texts.push(text.text.clone()),
            MountItem::ViewProps { props, .. } => {
                labels.extend(props.accessibility.label.as_deref().map(str::to_owned));
                labels.extend(props.accessibility.hint.as_deref().map(str::to_owned));
            }
            _ => {}
        }
    }
    (texts, labels)
}

#[test]
fn terminal_environment_keeps_the_os_and_uses_terminal_interaction() {
    let app = r#"import { mount, Text, View } from "nsh";
import { Platform } from "nsh/device";
import { keymap } from "nsh/keymap";
import { Menu, isAvailable } from "nsh/desktop";
Menu.setApplicationMenu([]);
Menu.popup([]);
keymap.bind("mod+k", () => {});
mount(() => <View desktop={{ accessibilityLabel: "desktop" }}>
  <Text>{`${Platform.OS} ${Platform.renderer} ${Platform.isTerminal} ${Platform.isDesktop} ${isAvailable} ${keymap.label("mod+k")}`}</Text>
  <Text>{Platform.select({ terminal: "terminal", macos: "macos", desktop: "desktop", native: "native" })}</Text>
  <Text>{Platform.select({ macos: "macos", native: "native" })}</Text>
  <Text>{Platform.select({ mobile: "mobile", desktop: "desktop", default: "default" })}</Text>
</View>);
"#;
    let batch = first_batch(
        Environment {
            os: Os::MacOs,
            idiom: Idiom::Desktop,
            renderer: Renderer::Terminal,
            ..Environment::default()
        },
        app,
    );
    let keymap = batch
        .items
        .iter()
        .find_map(|item| match item {
            MountItem::Keymap(keymap) => Some(keymap),
            _ => None,
        })
        .expect("mounted keymap");
    let control_k = nsh_core::keymap::Keystroke::parse_with("ctrl+k", false).unwrap();
    assert!(keymap.find(control_k, false, false).consumed());
    assert!(batch.items.iter().any(|item| matches!(
        item,
        MountItem::Command {
            command: nsh_core::Command::Desktop(command),
            ..
        } if matches!(**command, nsh_core::desktop::DesktopCommand::SetMenu(_))
    )));
    assert_eq!(
        frame_parts(batch),
        (
            vec![
                "macos terminal true false true Ctrl+K".into(),
                "terminal".into(),
                "macos".into(),
                "default".into()
            ],
            Vec::new(),
        )
    );
}

#[test]
fn platform_props_and_select_follow_the_os() {
    let frame = |os, idiom, text: &str, labels: &[&str]| {
        assert_eq!(
            first_frame(os, idiom),
            (
                vec![text.to_owned()],
                labels.iter().map(|l| (*l).to_owned()).collect()
            ),
            "{os:?} {idiom:?}"
        );
    };
    frame(Os::Ios, Idiom::Phone, "ios false i n", &["ios"]);
    frame(Os::Android, Idiom::Phone, "android false m n", &["android"]);
    frame(Os::MacOs, Idiom::Desktop, "macos true d n", &["desktop"]);
    // `native` is not the web, as in React Native.
    frame(Os::Web, Idiom::Phone, "web false m -", &["web"]);
    frame(Os::Web, Idiom::Pad, "web false m -", &["web"]);
    // A desktop browser behaves as a desktop.
    frame(Os::Web, Idiom::Desktop, "web true d -", &["desktop", "web"]);
}
