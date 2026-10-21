use nsh_core::{
    MountItem, Points, ViewProps,
    animation::{AnimationOp, AnimationProp},
    geometry::size,
};

use super::*;

#[cfg(target_os = "macos")]
const STEP: f32 = 30.0;
#[cfg(not(target_os = "macos"))]
const STEP: f32 = 48.0;

fn new_surface() -> Rc<Surface> {
    let surface = Surface::new(nsh_engine::Environment::default());
    surface.root.state_mut().frame = crate::geom::rect(0.0, 0.0, 400.0, 400.0);
    surface
}

fn view(surface: &Rc<Surface>, id: NodeId, parent: NodeId, kind: ViewKind) -> NshView {
    for item in [
        MountItem::Create { id, kind },
        MountItem::Frame {
            id,
            frame: crate::geom::rect(0.0, 0.0, 300.0, 300.0),
        },
        MountItem::Insert {
            parent,
            child: id,
            before: None,
        },
    ] {
        surface.mounter.borrow_mut().apply(surface, &item);
    }
    surface.view(id).unwrap()
}

fn wheel(dy: f32) -> PlatformInput {
    PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
        position: gpui::point(gpui::px(20.0), gpui::px(20.0)),
        delta: ScrollDelta::Lines(gpui::point(0.0, dy)),
        ..Default::default()
    })
}

fn click(down: bool) -> PlatformInput {
    let position = gpui::point(gpui::px(20.0), gpui::px(20.0));
    if down {
        PlatformInput::MouseDown(gpui::MouseDownEvent {
            button: MouseButton::Left,
            position,
            click_count: 1,
            ..Default::default()
        })
    } else {
        PlatformInput::MouseUp(gpui::MouseUpEvent {
            button: MouseButton::Left,
            position,
            click_count: 1,
            ..Default::default()
        })
    }
}

fn move_mouse(x: f32, y: f32) -> PlatformInput {
    PlatformInput::MouseMove(gpui::MouseMoveEvent {
        position: gpui::point(gpui::px(x), gpui::px(y)),
        pressed_button: Some(MouseButton::Left),
        ..Default::default()
    })
}

fn release_mouse(x: f32, y: f32) -> PlatformInput {
    PlatformInput::MouseUp(gpui::MouseUpEvent {
        button: MouseButton::Left,
        position: gpui::point(gpui::px(x), gpui::px(y)),
        click_count: 1,
        ..Default::default()
    })
}

fn ctrl_wheel(notches: f32) -> PlatformInput {
    PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
        position: gpui::point(gpui::px(20.0), gpui::px(20.0)),
        delta: ScrollDelta::Lines(gpui::point(0.0, notches * 48.0 / crate::events::LINE)),
        modifiers: gpui::Modifiers {
            control: true,
            ..Default::default()
        },
        ..Default::default()
    })
}

fn mount_animation(surface: &Rc<Surface>, item: MountItem) {
    if let MountItem::Animation(op) = &item {
        animation_changed(surface, op);
    }
    mount_animation_raw(surface, item);
}

fn mount_animation_raw(surface: &Rc<Surface>, item: MountItem) {
    surface.anim.borrow_mut().apply(&item);
    surface.mounter.borrow_mut().apply(surface, &item);
}

fn native_gestures() -> (Rc<Surface>, NshView, NshView) {
    let surface = new_surface();
    let owner = view(&surface, 2, ROOT_ID, ViewKind::View);
    let image = view(&surface, 4, owner.id(), ViewKind::View);
    bind_native_gestures(&surface, &owner, &image);
    (surface, owner, image)
}

fn bind_native_gestures(surface: &Rc<Surface>, owner: &NshView, image: &NshView) {
    {
        let mut state = image.state_mut();
        state.props.pointer_events = PointerEvents::None;
        state
            .style
            .transform
            .push(nsh_core::TransformKind::TranslateX, 0.0);
        state
            .style
            .transform
            .push(nsh_core::TransformKind::TranslateY, 0.0);
        state
            .style
            .transform
            .push(nsh_core::TransformKind::Scale, 1.0);
    }
    for (node, value) in [(101, 10.0), (102, -5.0), (103, 2.0)] {
        mount_animation(
            surface,
            MountItem::Animation(AnimationOp::CreateValue { node, value }),
        );
    }
    mount_animation(
        surface,
        MountItem::Gestures {
            id: owner.id(),
            values: GestureValues {
                pan_x: Some(101),
                pan_y: Some(102),
                pinch: Some(103),
                ..Default::default()
            },
        },
    );
    mount_animation(
        surface,
        MountItem::AnimationBindings {
            id: image.id(),
            bindings: vec![
                (AnimationProp::Transform(0), 101),
                (AnimationProp::Transform(1), 102),
                (AnimationProp::Transform(2), 103),
            ],
        },
    );
    surface.write_animations();
}

fn assert_pinch(image: &NshView, expected: f32) {
    let actual = image.state().anim.transform[2].unwrap();
    assert!(
        (actual - expected).abs() < 0.00001,
        "scale {actual}, expected {expected}"
    );
}

#[test]
fn presented_native_pan_and_ctrl_wheel_drive_the_original_values_across_redraws() {
    let (surface, owner, image) = native_gestures();
    let source = InputSource::new(&surface);
    let original = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&original, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(
        source.dispatch(&original, &move_mouse(70.0, 50.0)),
        InputDisposition::Handled
    );
    assert_eq!(
        surface.anim.borrow().pan_values(owner.id()),
        (Some(60.0), Some(25.0))
    );
    assert_eq!(image.state().anim.transform[..2], [Some(60.0), Some(25.0)]);

    // Growing content and the image's animated transform do not move its owner.
    image.state_mut().frame.size.height = Points(600.0);
    assert_eq!(
        source.dispatch(&original, &move_mouse(100.0, 80.0)),
        InputDisposition::Handled
    );
    assert_eq!(
        surface.anim.borrow().pan_values(owner.id()),
        (Some(90.0), Some(55.0))
    );
    assert_eq!(
        source.dispatch(&original, &release_mouse(100.0, 80.0)),
        InputDisposition::Handled
    );
    assert_eq!(image.state().anim.transform[..2], [Some(90.0), Some(55.0)]);

    assert_eq!(
        source.dispatch(&original, &ctrl_wheel(1.0)),
        InputDisposition::Handled
    );
    assert_pinch(&image, 2.2);
    assert_eq!(
        source.dispatch(&original, &ctrl_wheel(1.0)),
        InputDisposition::Handled
    );
    assert_pinch(&image, 2.42);
    assert_eq!(
        source.dispatch(&original, &ctrl_wheel(-1.0)),
        InputDisposition::Handled
    );
    assert_pinch(&image, 2.2);
}

#[test]
fn native_pan_rebinding_does_not_redirect_an_active_drag_to_replacement_values() {
    let (surface, owner, image) = native_gestures();
    let source = InputSource::new(&surface);
    let original = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&original, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(
        source.dispatch(&original, &move_mouse(70.0, 50.0)),
        InputDisposition::Handled
    );
    assert_eq!(image.state().anim.transform[..2], [Some(60.0), Some(25.0)]);
    for (node, value) in [(201, 100.0), (202, 200.0)] {
        mount_animation(
            &surface,
            MountItem::Animation(AnimationOp::CreateValue { node, value }),
        );
    }
    mount_animation(
        &surface,
        MountItem::Gestures {
            id: owner.id(),
            values: GestureValues {
                pan_x: Some(201),
                pan_y: Some(202),
                ..Default::default()
            },
        },
    );
    assert_eq!(
        source.dispatch(&original, &move_mouse(100.0, 80.0)),
        InputDisposition::Rejected
    );
    assert_eq!(
        surface.anim.borrow().pan_values(owner.id()),
        (Some(100.0), Some(200.0))
    );
    assert_eq!(image.state().anim.transform[..2], [Some(60.0), Some(25.0)]);
    assert!(surface.input.pointers.borrow().is_empty());

    let rebound = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&rebound, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(
        source.dispatch(&rebound, &move_mouse(50.0, 40.0)),
        InputDisposition::Handled
    );
    assert_eq!(
        surface.anim.borrow().pan_values(owner.id()),
        (Some(130.0), Some(220.0))
    );
    assert_eq!(
        source.dispatch(&rebound, &release_mouse(50.0, 40.0)),
        InputDisposition::Handled
    );
}

#[test]
fn native_pan_rejects_replaced_value_slots_even_when_the_descriptor_is_unchanged() {
    for release in [false, true] {
        let (surface, owner, image) = native_gestures();
        let source = InputSource::new(&surface);
        let original = source.sample().unwrap();
        let binding = surface.anim.borrow().gesture_binding(owner.id()).unwrap();
        assert_eq!(
            source.dispatch(&original, &click(true)),
            InputDisposition::Handled
        );
        assert_eq!(
            source.dispatch(&original, &move_mouse(70.0, 50.0)),
            InputDisposition::Handled
        );
        if release {
            mount_animation_raw(
                &surface,
                MountItem::Animation(AnimationOp::Release { node: 101 }),
            );
            assert!(surface.anim.borrow().gesture_binding(owner.id()).is_none());
        }
        // Bypass the ordinary mutation hook to prove graph birth identity
        // also revokes a stale lease after direct same-id graph replacement.
        mount_animation_raw(
            &surface,
            MountItem::Animation(AnimationOp::CreateValue {
                node: 101,
                value: 900.0,
            }),
        );
        assert_ne!(
            surface.anim.borrow().gesture_binding(owner.id()).unwrap(),
            binding
        );
        assert_eq!(
            source.dispatch(&original, &move_mouse(100.0, 80.0)),
            InputDisposition::Rejected
        );
        assert_eq!(
            surface.anim.borrow().pan_values(owner.id()),
            (Some(900.0), Some(25.0))
        );
        surface.write_animations();
        assert_eq!(image.state().anim.transform[..2], [Some(900.0), Some(25.0)]);
        assert!(surface.input.pointers.borrow().is_empty());

        let replacement = source.sample().unwrap();
        assert_eq!(
            source.dispatch(&replacement, &click(true)),
            InputDisposition::Handled
        );
        assert_eq!(
            source.dispatch(&replacement, &move_mouse(50.0, 40.0)),
            InputDisposition::Handled
        );
        assert_eq!(
            surface.anim.borrow().pan_values(owner.id()),
            (Some(930.0), Some(45.0))
        );
        assert_eq!(
            source.dispatch(&replacement, &release_mouse(50.0, 40.0)),
            InputDisposition::Handled
        );
    }
}

#[test]
fn callbacks_keep_native_gestures_exact_and_revoke_an_already_active_pan() {
    for event in [
        EventKind::Touch,
        EventKind::MouseMove,
        EventKind::HoverIn,
        EventKind::HoverOut,
        EventKind::HoverMove,
        EventKind::Focus,
        EventKind::Blur,
        EventKind::ContextMenu,
        EventKind::Press,
        EventKind::PressDown,
        EventKind::PressUp,
        EventKind::LongPress,
        EventKind::PanStart,
        EventKind::PanEnd,
        EventKind::PinchStart,
        EventKind::PinchEnd,
        EventKind::RotationStart,
        EventKind::RotationEnd,
    ] {
        let (surface, owner, image) = native_gestures();
        let source = InputSource::new(&surface);
        owner.state_mut().events.set(event, true);
        let opaque = source.sample().unwrap();
        assert_eq!(
            source.dispatch(&opaque, &click(true)),
            InputDisposition::ExactFallback,
            "{event:?}"
        );
        assert_eq!(
            source.dispatch(&opaque, &ctrl_wheel(1.0)),
            InputDisposition::ExactFallback,
            "{event:?}"
        );
        assert_eq!(
            surface.anim.borrow().pan_values(owner.id()),
            (Some(10.0), Some(-5.0))
        );
        assert_pinch(&image, 2.0);

        owner.state_mut().events.set(event, false);
        let original = source.sample().unwrap();
        assert_eq!(
            source.dispatch(&original, &click(true)),
            InputDisposition::Handled
        );
        assert_eq!(
            source.dispatch(&original, &move_mouse(70.0, 50.0)),
            InputDisposition::Handled
        );
        owner.state_mut().events.set(event, true);
        assert_eq!(
            source.dispatch(&original, &move_mouse(100.0, 80.0)),
            InputDisposition::Rejected,
            "{event:?}"
        );
        assert_eq!(
            surface.anim.borrow().pan_values(owner.id()),
            (Some(60.0), Some(25.0))
        );
        assert_eq!(image.state().anim.transform[..2], [Some(60.0), Some(25.0)]);
        assert!(surface.input.pointers.borrow().is_empty());
        assert_eq!(
            source.dispatch(&source.sample().unwrap(), &click(true)),
            InputDisposition::ExactFallback
        );
    }
}

#[test]
fn ctrl_wheel_rejects_rebound_and_recycled_pinch_values_without_scaling_the_replacement() {
    for rebind in [false, true] {
        let (surface, owner, image) = native_gestures();
        let source = InputSource::new(&surface);
        let original = source.sample().unwrap();
        assert_eq!(
            source.dispatch(&original, &ctrl_wheel(1.0)),
            InputDisposition::Handled
        );
        assert_pinch(&image, 2.2);
        let replacement_node = if rebind { 203 } else { 103 };
        if !rebind {
            mount_animation_raw(
                &surface,
                MountItem::Animation(AnimationOp::Release { node: 103 }),
            );
        }
        mount_animation_raw(
            &surface,
            MountItem::Animation(AnimationOp::CreateValue {
                node: replacement_node,
                value: 5.0,
            }),
        );
        if rebind {
            mount_animation(
                &surface,
                MountItem::Gestures {
                    id: owner.id(),
                    values: GestureValues {
                        pan_x: Some(101),
                        pan_y: Some(102),
                        pinch: Some(replacement_node),
                        ..Default::default()
                    },
                },
            );
            mount_animation(
                &surface,
                MountItem::AnimationBindings {
                    id: image.id(),
                    bindings: vec![(AnimationProp::Transform(2), replacement_node)],
                },
            );
        }
        surface.write_animations();
        assert_pinch(&image, 5.0);
        assert_eq!(
            source.dispatch(&original, &ctrl_wheel(1.0)),
            InputDisposition::Rejected
        );
        assert_pinch(&image, 5.0);
        assert!(surface.input.zoom.borrow().is_none());

        let replacement = source.sample().unwrap();
        assert_eq!(
            source.dispatch(&replacement, &ctrl_wheel(1.0)),
            InputDisposition::Handled
        );
        assert_pinch(&image, 5.5);
    }
}

#[test]
fn native_gestures_preserve_callback_boundaries_at_the_destination_and_old_hover() {
    for release in [false, true] {
        let (surface, owner, _) = native_gestures();
        let sibling = view(&surface, 6, ROOT_ID, ViewKind::View);
        sibling.state_mut().frame = crate::geom::rect(320.0, 0.0, 60.0, 300.0);
        sibling.state_mut().events.set(EventKind::HoverIn, true);
        let source = InputSource::new(&surface);
        let original = source.sample().unwrap();
        assert_eq!(
            source.dispatch(&original, &click(true)),
            InputDisposition::Handled
        );
        assert_eq!(
            source.dispatch(&original, &move_mouse(70.0, 50.0)),
            InputDisposition::Handled
        );
        let destination = if release {
            release_mouse(350.0, 20.0)
        } else {
            move_mouse(350.0, 20.0)
        };
        assert_eq!(
            source.dispatch(&original, &destination),
            InputDisposition::Rejected
        );
        assert_eq!(
            surface.anim.borrow().pan_values(owner.id()),
            (Some(60.0), Some(25.0))
        );
        assert!(surface.input.pointers.borrow().is_empty());
    }

    let (surface, _, image) = native_gestures();
    let sibling = view(&surface, 6, ROOT_ID, ViewKind::View);
    sibling.state_mut().frame = crate::geom::rect(320.0, 0.0, 60.0, 300.0);
    sibling.state_mut().events.set(EventKind::HoverOut, true);
    surface.input.hovered.borrow_mut().push(sibling);
    let source = InputSource::new(&surface);
    let original = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&original, &ctrl_wheel(1.0)),
        InputDisposition::ExactFallback
    );
    assert_pinch(&image, 2.0);
    surface.input.hovered.borrow_mut().clear();
    assert_eq!(
        source.dispatch(&original, &ctrl_wheel(1.0)),
        InputDisposition::Handled
    );
    assert_pinch(&image, 2.2);
}

#[test]
fn native_value_adjustments_retire_the_old_baseline_while_unrelated_writes_keep_it() {
    let (surface, owner, image) = native_gestures();
    let source = InputSource::new(&surface);
    let original = source.sample().unwrap();
    let binding = surface.anim.borrow().gesture_binding(owner.id()).unwrap();
    assert_eq!(
        source.dispatch(&original, &ctrl_wheel(1.0)),
        InputDisposition::Handled
    );
    assert_pinch(&image, 2.2);
    mount_animation(
        &surface,
        MountItem::Animation(AnimationOp::CreateValue {
            node: 999,
            value: 0.0,
        }),
    );
    mount_animation(
        &surface,
        MountItem::Animation(AnimationOp::Set {
            node: 999,
            value: 90.0,
        }),
    );
    assert!(surface.input.zoom.borrow().is_some());
    mount_animation(
        &surface,
        MountItem::Animation(AnimationOp::Adjust {
            node: 103,
            factor: 2.0,
            offset: 0.6,
        }),
    );
    surface.write_animations();
    assert_eq!(
        surface.anim.borrow().gesture_binding(owner.id()).unwrap(),
        binding
    );
    assert!(surface.input.zoom.borrow().is_none());
    assert_pinch(&image, 5.0);
    assert_eq!(
        source.dispatch(&original, &ctrl_wheel(1.0)),
        InputDisposition::Handled
    );
    assert_pinch(&image, 5.5);

    mount_animation(
        &surface,
        MountItem::Animation(AnimationOp::Set {
            node: 103,
            value: 5.0,
        }),
    );
    assert_eq!(
        source.dispatch(&original, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(
        source.dispatch(&original, &move_mouse(70.0, 50.0)),
        InputDisposition::Handled
    );
    assert_eq!(
        surface.anim.borrow().pan_values(owner.id()),
        (Some(60.0), Some(25.0))
    );
    mount_animation(
        &surface,
        MountItem::Animation(AnimationOp::Set {
            node: 999,
            value: 180.0,
        }),
    );
    assert_eq!(surface.input.pointers.borrow().len(), 1);
    mount_animation(
        &surface,
        MountItem::Animation(AnimationOp::Adjust {
            node: 101,
            factor: 1.0,
            offset: 5.0,
        }),
    );
    assert!(surface.input.pointers.borrow().is_empty());
    assert_eq!(
        surface.anim.borrow().pan_values(owner.id()),
        (Some(65.0), Some(25.0))
    );
    assert_eq!(
        surface.anim.borrow().gesture_binding(owner.id()).unwrap(),
        binding
    );
    assert_eq!(
        source.dispatch(&original, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(
        source.dispatch(&original, &move_mouse(50.0, 40.0)),
        InputDisposition::Handled
    );
    assert_eq!(
        surface.anim.borrow().pan_values(owner.id()),
        (Some(95.0), Some(45.0))
    );
    assert_eq!(
        source.dispatch(&original, &release_mouse(50.0, 40.0)),
        InputDisposition::Handled
    );
}

#[gpui::test]
fn idle_native_zoom_retires_without_delivering_a_new_pinch_end_callback(
    cx: &mut gpui::TestAppContext,
) {
    use std::time::Duration;

    use nsh_core::ui::{
        Root,
        prelude::{GestureEvent, Interactive, IntoElement, component, view as native_view},
    };

    thread_local! {
        static ENDED: RefCell<Option<crossbeam_channel::Sender<f32>>> = const { RefCell::new(None) };
    }
    #[component]
    fn Observer() -> impl IntoElement {
        let ended = ENDED.with(|ended| ended.borrow_mut().take().unwrap());
        native_view().on_pinch_end(move |event: &GestureEvent| {
            ended.send(event.value).unwrap();
        })
    }

    cx.executor().allow_parking();
    let (ended_tx, ended_rx) = crossbeam_channel::unbounded();
    let (surface, _app) = cx.update(|cx| {
        let app = crate::rt::init_test_app(cx);
        let surface = new_surface();
        *surface.text_system.borrow_mut() = Some(cx.text_system().clone());
        surface.start(
            nsh_engine::EngineConfig::new(Root::new::<Observer>(move || {
                ENDED.with(|ended| *ended.borrow_mut() = Some(ended_tx.clone()));
            })),
            false,
        );
        (surface, app)
    });
    let synchronize = || {
        let (tx, rx) = crossbeam_channel::bounded(1);
        surface.control(|control| {
            control.advance_clock(Duration::ZERO, move || tx.send(()).unwrap());
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        surface.drain();
    };
    synchronize();
    let owner = surface.root.with_children(|children| {
        children
            .iter()
            .find(|view| view.state().events.contains(EventKind::PinchEnd))
            .cloned()
            .unwrap()
    });
    owner.state_mut().frame = crate::geom::rect(0.0, 0.0, 300.0, 300.0);
    owner.state_mut().events.set(EventKind::PinchEnd, false);
    let image = view(&surface, 1000, owner.id(), ViewKind::View);
    bind_native_gestures(&surface, &owner, &image);
    let source = InputSource::new(&surface);
    let original = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&original, &ctrl_wheel(1.0)),
        InputDisposition::Handled
    );
    assert_pinch(&image, 2.2);
    assert!(surface.input.zoom.borrow().is_some());

    // The engine already has the listener; only its native admission mask
    // changes, reproducing a callback mounted while the idle timer is pending.
    owner.state_mut().events.set(EventKind::PinchEnd, true);
    cx.run_until_parked();
    cx.executor()
        .advance_clock(Duration::from_millis(ZOOM_IDLE + 1));
    cx.run_until_parked();
    synchronize();
    assert!(surface.input.zoom.borrow().is_none());
    assert_pinch(&image, 2.2);
    assert!(ended_rx.try_recv().is_err());
    assert_eq!(
        source.dispatch(&original, &ctrl_wheel(1.0)),
        InputDisposition::Rejected
    );
    assert_eq!(
        source.dispatch(&source.sample().unwrap(), &ctrl_wheel(1.0)),
        InputDisposition::ExactFallback
    );

    // The registered callback still receives an ordinary exact-route gesture.
    assert!(wheel_zoom(&surface, 20.0, 20.0, -48.0));
    assert_pinch(&image, 2.42);
    cx.run_until_parked();
    cx.executor()
        .advance_clock(Duration::from_millis(ZOOM_IDLE + 1));
    cx.run_until_parked();
    synchronize();
    let scale = ended_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        (scale - 1.1).abs() < 0.00001,
        "ordinary pinch ended at {scale}"
    );
    assert_pinch(&image, 2.42);
    surface.stop();
}

fn key_event(key: &str, down: bool) -> PlatformInput {
    let keystroke = gpui::Keystroke {
        key: key.to_owned(),
        key_char: None,
        modifiers: gpui::Modifiers::default(),
    };
    if down {
        PlatformInput::KeyDown(gpui::KeyDownEvent {
            keystroke,
            is_held: false,
            prefer_character_input: false,
        })
    } else {
        PlatformInput::KeyUp(gpui::KeyUpEvent { keystroke })
    }
}

#[test]
fn presented_wheels_keep_the_original_scroll_across_growth_and_a_new_nested_owner() {
    let surface = new_surface();
    let outer = view(&surface, 2, ROOT_ID, ViewKind::Scroll);
    outer.state_mut().scroll.as_mut().unwrap().content = size(Points(300.0), Points(1200.0));
    let mut events = outer.state().events;
    events.set(EventKind::ScrollDragStart, true);
    events.set(EventKind::ScrollDragEnd, true);
    surface.mounter.borrow_mut().apply(
        &surface,
        &MountItem::Events {
            id: outer.id(),
            events,
        },
    );
    let source = InputSource::new(&surface);
    let snapshot = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&snapshot, &wheel(-3.0)),
        InputDisposition::Handled
    );
    assert_eq!(outer.state().scroll.as_ref().unwrap().y, Points(STEP));
    assert_eq!(
        source.dispatch(&snapshot, &wheel(3.0)),
        InputDisposition::Handled
    );
    assert_eq!(outer.state().scroll.as_ref().unwrap().y, Points(0.0));

    outer.state_mut().scroll.as_mut().unwrap().content = size(Points(300.0), Points(1800.0));
    let nested = view(&surface, 4, 2, ViewKind::Scroll);
    nested.state_mut().scroll.as_mut().unwrap().content = size(Points(300.0), Points(900.0));
    assert_eq!(
        source.dispatch(&snapshot, &wheel(-3.0)),
        InputDisposition::Handled
    );
    assert_eq!(outer.state().scroll.as_ref().unwrap().y, Points(STEP));
    assert_eq!(nested.state().scroll.as_ref().unwrap().y, Points(0.0));
    assert_eq!(
        source.dispatch(&snapshot, &wheel(3.0)),
        InputDisposition::Handled
    );
    assert_eq!(outer.state().scroll.as_ref().unwrap().y, Points(0.0));
    // The edge is consumed by the same owner, rather than handed to a new hit.
    assert_eq!(
        source.dispatch(&snapshot, &wheel(3.0)),
        InputDisposition::Handled
    );
    assert_eq!(outer.state().scroll.as_ref().unwrap().y, Points(0.0));

    let pixels = PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
        position: gpui::point(gpui::px(20.0), gpui::px(20.0)),
        delta: ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(-3.0))),
        ..Default::default()
    });
    assert_eq!(
        source.dispatch(&snapshot, &pixels),
        InputDisposition::ExactFallback
    );
    assert_eq!(outer.state().scroll.as_ref().unwrap().y, Points(0.0));

    events.set(EventKind::ScrollEnd, true);
    surface.mounter.borrow_mut().apply(
        &surface,
        &MountItem::Events {
            id: outer.id(),
            events,
        },
    );
    assert_eq!(
        source.dispatch(&snapshot, &wheel(-3.0)),
        InputDisposition::Rejected
    );
    assert_eq!(
        source.dispatch(&source.sample().unwrap(), &wheel(-3.0)),
        InputDisposition::ExactFallback
    );
    assert_eq!(outer.state().scroll.as_ref().unwrap().y, Points(0.0));
}

#[test]
fn presented_wheels_keep_the_scroll_when_its_input_moves_under_the_pointer() {
    let surface = new_surface();
    let scroll = view(&surface, 2, ROOT_ID, ViewKind::Scroll);
    scroll.state_mut().scroll.as_mut().unwrap().content = size(Points(300.0), Points(1200.0));
    let editor = view(&surface, 4, scroll.id(), ViewKind::Input);
    editor.state_mut().frame = crate::geom::rect(0.0, STEP + 10.0, 200.0, 32.0);
    crate::text_input::value(&editor, "maka你好", 0);
    crate::text_input::focus_view(&surface, &editor);
    crate::text_input::set_selection(&editor, 1, 4);
    let source = InputSource::new(&surface);
    let snapshot = source.sample().unwrap();

    assert_eq!(pick(&surface.root, 20.0, 20.0, true).unwrap().view, scroll);
    assert_eq!(
        source.dispatch(&snapshot, &wheel(-3.0)),
        InputDisposition::Handled
    );
    assert_eq!(scroll.state().scroll.as_ref().unwrap().y, Points(STEP));
    assert_eq!(pick(&surface.root, 20.0, 20.0, true).unwrap().view, editor);
    // A new presentation begins on the field and keeps its ordinary route.
    assert_eq!(
        source.dispatch(&source.sample().unwrap(), &wheel(-3.0)),
        InputDisposition::ExactFallback
    );
    // An opaque callback still invalidates the old native scroll route.
    editor.state_mut().events.set(EventKind::MouseMove, true);
    assert_eq!(
        source.dispatch(&snapshot, &wheel(-3.0)),
        InputDisposition::Rejected
    );
    assert_eq!(scroll.state().scroll.as_ref().unwrap().y, Points(STEP));
    editor.state_mut().events.set(EventKind::MouseMove, false);

    // The rest of this original twelve-notch gesture stays on the Scroll,
    // even while its own field passes beneath the stationary pointer.
    for tick in 2..=12 {
        assert_eq!(
            source.dispatch(&snapshot, &wheel(-3.0)),
            InputDisposition::Handled,
            "original wheel notch {tick} was dropped"
        );
        assert_eq!(
            scroll.state().scroll.as_ref().unwrap().y,
            Points(STEP * tick as f32)
        );
    }
    assert_eq!(
        crate::text_input::focused_view(&surface),
        Some(editor.clone())
    );
    assert_eq!(
        crate::text_input::accessible_value(&editor)
            .map(|value| value.text)
            .as_deref(),
        Some("maka你好")
    );
    assert_eq!(
        crate::text_input::ime::selected_text_range(&surface)
            .unwrap()
            .range,
        1..4
    );
}

#[test]
fn presented_wheels_reject_recycled_geometry_scope_and_opaque_routes() {
    let surface = new_surface();
    let original = view(&surface, 2, ROOT_ID, ViewKind::Scroll);
    original.state_mut().scroll.as_mut().unwrap().content = size(Points(300.0), Points(1200.0));
    let source = InputSource::new(&surface);
    let old = source.sample().unwrap();
    surface
        .mounter
        .borrow_mut()
        .apply(&surface, &MountItem::Remove { id: 2 });
    let replacement = view(&surface, 2, ROOT_ID, ViewKind::Scroll);
    replacement.state_mut().scroll.as_mut().unwrap().content = size(Points(300.0), Points(1200.0));
    assert_eq!(
        source.dispatch(&old, &wheel(-3.0)),
        InputDisposition::Rejected
    );
    assert_eq!(replacement.state().scroll.as_ref().unwrap().y, Points(0.0));
    let fresh = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&fresh, &wheel(-3.0)),
        InputDisposition::Handled
    );
    assert_eq!(replacement.state().scroll.as_ref().unwrap().y, Points(STEP));

    replacement.state_mut().frame.size.width = Points(250.0);
    assert_eq!(
        source.dispatch(&fresh, &wheel(-3.0)),
        InputDisposition::Rejected
    );
    replacement.state_mut().frame.size.width = Points(300.0);
    replacement
        .state_mut()
        .events
        .set(EventKind::MomentumEnd, true);
    assert_eq!(
        source.dispatch(&fresh, &wheel(-3.0)),
        InputDisposition::Rejected
    );
    assert_eq!(
        source.dispatch(&source.sample().unwrap(), &wheel(-3.0)),
        InputDisposition::ExactFallback
    );
    replacement
        .state_mut()
        .events
        .set(EventKind::MomentumEnd, false);

    surface.resized(500.0, 400.0, 1.0);
    assert_eq!(
        source.dispatch(&fresh, &wheel(-3.0)),
        InputDisposition::Rejected
    );
    let current = source.sample().unwrap();
    let modal = view(&surface, 6, ROOT_ID, ViewKind::Modal);
    modal.state_mut().frame = crate::geom::rect(350.0, 350.0, 25.0, 25.0);
    assert_eq!(pick(&surface.root, 20.0, 20.0, true).unwrap().view.id(), 2);
    assert_eq!(
        source.dispatch(&current, &wheel(-3.0)),
        InputDisposition::Rejected
    );
    assert_eq!(replacement.state().scroll.as_ref().unwrap().y, Points(STEP));
}

#[test]
fn native_tooltip_watches_preserve_presented_presses_and_actual_listeners_reject_them() {
    use nsh_core::{ApproxTextEngine, Core, Op, Prop, Tag, overlay::TooltipContent};

    let surface = new_surface();
    let underlay = view(&surface, 4, ROOT_ID, ViewKind::View);
    let button = view(&surface, 2, ROOT_ID, ViewKind::View);
    let mut core = Core::new(ApproxTextEngine, size(Points(400.0), Points(400.0)));
    core.set_environment(nsh_core::ui::Environment {
        os: nsh_core::platform::Os::Linux,
        idiom: nsh_core::platform::Idiom::Desktop,
        ..Default::default()
    });
    for op in [
        Op::Create {
            id: 2,
            tag: Tag::View,
        },
        Op::Insert {
            parent: ROOT_ID,
            child: 2,
            before: None,
        },
        Op::Listen {
            id: 2,
            event: EventKind::Press,
            enabled: true,
        },
        Op::SetProp {
            id: 2,
            prop: Prop::InputAction(Some(7)),
        },
        Op::SetProp {
            id: 2,
            prop: Prop::Tooltip(TooltipContent::Text("Saves it".into())),
        },
    ] {
        core.apply(op).unwrap();
    }
    let mount_anchor = |core: &mut Core<ApproxTextEngine>| {
        let batch = core.commit().unwrap();
        for item in &batch.items {
            if matches!(
                item,
                MountItem::Events { id: 2, .. } | MountItem::ViewProps { id: 2, .. }
            ) {
                surface.mounter.borrow_mut().apply(&surface, item);
            }
        }
    };
    mount_anchor(&mut core);
    assert_eq!(button.state().props.input_action, Some(7));
    for kind in [EventKind::Touch, EventKind::HoverIn, EventKind::HoverOut] {
        assert!(button.state().events.contains(kind));
        assert!(!button.state().events.contains_listener(kind));
    }
    let source = InputSource::new(&surface);
    let snapshot = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&snapshot, &click(true)),
        InputDisposition::Handled
    );
    surface.mounter.borrow_mut().apply(
        &surface,
        &MountItem::Frame {
            id: underlay.id(),
            frame: crate::geom::rect(0.0, 0.0, 300.0, 600.0),
        },
    );
    assert_eq!(
        source.dispatch(&snapshot, &click(false)),
        InputDisposition::Handled
    );

    for kind in [EventKind::Touch, EventKind::HoverIn, EventKind::HoverOut] {
        assert_eq!(
            source.dispatch(&snapshot, &click(true)),
            InputDisposition::Handled
        );
        core.apply(Op::Listen {
            id: 2,
            event: kind,
            enabled: true,
        })
        .unwrap();
        mount_anchor(&mut core);
        assert_eq!(
            source.dispatch(&snapshot, &click(false)),
            InputDisposition::Rejected
        );
        assert!(surface.input.presented_press.borrow().is_none());
        assert!(surface.input.pointers.borrow().is_empty());
        assert_eq!(
            source.dispatch(&source.sample().unwrap(), &click(true)),
            InputDisposition::ExactFallback
        );
        core.apply(Op::Listen {
            id: 2,
            event: kind,
            enabled: false,
        })
        .unwrap();
        mount_anchor(&mut core);
    }
}

#[test]
fn sealed_press_rebinding_cancels_the_original_capture_without_releasing_into_its_replacement() {
    let surface = new_surface();
    let button = view(&surface, 2, ROOT_ID, ViewKind::View);
    {
        let mut state = button.state_mut();
        *state.props = ViewProps {
            input_action: Some(7),
            ..Default::default()
        };
        state.events.set(EventKind::Press, true);
        state.tracked = PRESSED;
    }
    let area = view(&surface, 4, ROOT_ID, ViewKind::View);
    area.state_mut().frame = crate::geom::rect(320.0, 0.0, 60.0, 300.0);
    area.state_mut().props.selection_area = true;
    crate::selection_area::down(&surface, &area, &area, (330.0, 20.0), 1, false);
    crate::selection_area::pointer(&surface, PointerKind::Up, 330.0, 20.0);
    assert_eq!(crate::selection_area::keyboard_owner(&surface), Some(area));
    let source = InputSource::new(&surface);
    let first = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&first, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(button.state().states, PRESSED);
    assert_eq!(crate::selection_area::keyboard_owner(&surface), None);
    button.state_mut().props.input_action = Some(8);
    assert_eq!(
        source.dispatch(&first, &click(false)),
        InputDisposition::Rejected
    );
    assert_eq!(button.state().states, 0);
    assert!(surface.input.pointers.borrow().is_empty());

    let rebound = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&rebound, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(button.state().states, PRESSED);
    assert_eq!(
        source.dispatch(&rebound, &click(false)),
        InputDisposition::Handled
    );
    assert_eq!(button.state().states, 0);
    let keyboard = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&keyboard, &key_event("enter", true)),
        InputDisposition::Handled
    );
    button.state_mut().props.input_action = Some(9);
    assert_eq!(
        source.dispatch(&keyboard, &key_event("enter", true)),
        InputDisposition::Rejected
    );
    // A foreign/default snapshot never acquires a press on this surface.
    assert_eq!(
        source.dispatch(&InputSnapshot::default(), &click(true)),
        InputDisposition::Rejected
    );
    assert_eq!(button.state().states, 0);
}

#[test]
fn sealed_press_preserves_mouse_move_listeners() {
    let event = EventKind::MouseMove;
    let surface = new_surface();
    let button = view(&surface, 2, ROOT_ID, ViewKind::View);
    button.state_mut().props.input_action = Some(7);
    button.state_mut().events.set(EventKind::Press, true);
    button.state_mut().tracked = PRESSED;
    let source = InputSource::new(&surface);

    // An existing listener must keep the down on the exact input path.
    button.state_mut().events.set(event, true);
    let snapshot = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&snapshot, &click(true)),
        InputDisposition::ExactFallback
    );
    assert!(surface.input.presented_press.borrow().is_none());

    // A listener added to the live route invalidates an already sealed press.
    button.state_mut().events.set(event, false);
    let snapshot = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&snapshot, &click(true)),
        InputDisposition::Handled
    );
    surface.root.state_mut().events.set(event, true);
    let moved = PlatformInput::MouseMove(gpui::MouseMoveEvent {
        position: gpui::point(gpui::px(100.0), gpui::px(20.0)),
        pressed_button: Some(MouseButton::Left),
        ..Default::default()
    });
    assert_eq!(
        source.dispatch(&snapshot, &moved),
        InputDisposition::Rejected
    );
    assert!(surface.input.presented_press.borrow().is_none());
    assert!(surface.input.pointers.borrow().is_empty());
    assert_eq!(button.state().states, 0);

    // Crossing into a listener must reject even when the press owner is unchanged.
    surface.root.state_mut().events.set(event, false);
    let child = view(&surface, 3, button.id(), ViewKind::View);
    child.state_mut().frame = crate::geom::rect(80.0, 0.0, 100.0, 100.0);
    child.state_mut().events.set(event, true);
    let snapshot = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&snapshot, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(
        source.dispatch(&snapshot, &moved),
        InputDisposition::Rejected
    );
    assert!(surface.input.presented_press.borrow().is_none());
    assert!(surface.input.pointers.borrow().is_empty());
    assert_eq!(button.state().states, 0);
}

#[test]
fn presented_inputs_reject_occlusion_reparenting_and_replaced_owners() {
    let surface = new_surface();
    let parent = view(&surface, 2, ROOT_ID, ViewKind::View);
    parent.state_mut().props.input_action = Some(7);
    parent.state_mut().events.set(EventKind::Press, true);
    let editor = view(&surface, 4, parent.id(), ViewKind::Input);
    surface.mounter.borrow_mut().apply(
        &surface,
        &MountItem::Input {
            id: editor.id(),
            config: Box::new(nsh_core::InputConfig {
                editable: false,
                ..Default::default()
            }),
        },
    );
    let source = InputSource::new(&surface);
    let original = source.sample().unwrap();
    let overlay = view(&surface, 6, ROOT_ID, ViewKind::View);
    assert_eq!(
        source.dispatch(&original, &click(true)),
        InputDisposition::Rejected
    );
    assert_eq!(crate::text_input::focused_view(&surface), None);
    surface
        .mounter
        .borrow_mut()
        .apply(&surface, &MountItem::Remove { id: overlay.id() });

    // Readonly editors still own native focus and selection, without giving a
    // containing press action the click or permitting text replacement.
    assert_eq!(
        source.dispatch(&original, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(
        crate::text_input::focused_view(&surface),
        Some(editor.clone())
    );
    assert!(!source.editor_matches(&source.sample().unwrap()));
    let native_move = crate::view::ItemPointer {
        kind: PointerKind::Move,
        x: 350.0,
        y: 20.0,
        clicks: 0,
        modifiers: Modifiers::default(),
    };
    assert!(crate::text_input::pointer(&surface, &editor, &native_move));
    let moved = PlatformInput::MouseMove(gpui::MouseMoveEvent {
        position: gpui::point(gpui::px(350.0), gpui::px(20.0)),
        pressed_button: Some(MouseButton::Left),
        ..Default::default()
    });
    parent.state_mut().events.set(EventKind::MouseMove, true);
    assert_eq!(
        source.dispatch(&original, &moved),
        InputDisposition::Rejected
    );
    assert!(!crate::text_input::pointer(&surface, &editor, &native_move));
    assert_eq!(
        crate::text_input::focused_view(&surface),
        Some(editor.clone())
    );
    parent.state_mut().events.set(EventKind::MouseMove, false);

    struct ForeignItem;
    impl crate::view::Hosted for ForeignItem {
        fn draw(&self, _: &mut crate::render::DrawCx<'_>, _: (f32, f32)) {}
    }
    let foreign: Rc<dyn crate::view::Hosted> = Rc::new(ForeignItem);
    editor.set_native(&foreign, crate::geom::rect(0.0, 0.0, 300.0, 300.0));
    assert_eq!(
        source.dispatch(&original, &click(true)),
        InputDisposition::Rejected
    );
    assert_eq!(
        source.dispatch(&source.sample().unwrap(), &click(true)),
        InputDisposition::ExactFallback
    );
    editor.remove_native(&foreign);

    // Arbitrary pointer and focus callbacks retain the exact-frame path.
    for event in [EventKind::Focus, EventKind::Touch, EventKind::MouseMove] {
        editor.state_mut().events.set(event, true);
        let snapshot = source.sample().unwrap();
        assert_eq!(
            source.dispatch(&snapshot, &click(true)),
            InputDisposition::ExactFallback
        );
        editor.state_mut().events.set(event, false);
    }

    let sibling = view(&surface, 10, ROOT_ID, ViewKind::View);
    sibling.state_mut().frame = crate::geom::rect(320.0, 0.0, 80.0, 300.0);
    let dragging = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&dragging, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(
        source.dispatch(&dragging, &moved),
        InputDisposition::Handled
    );
    sibling.state_mut().events.set(EventKind::MouseMove, true);
    assert_eq!(
        source.dispatch(&dragging, &moved),
        InputDisposition::Rejected
    );
    assert!(!crate::text_input::pointer(&surface, &editor, &native_move));
    sibling.state_mut().events.set(EventKind::MouseMove, false);

    sibling.set_native(&foreign, crate::geom::rect(0.0, 0.0, 80.0, 300.0));
    assert_eq!(
        source.dispatch(&dragging, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(
        source.dispatch(&dragging, &moved),
        InputDisposition::Rejected
    );
    assert!(!crate::text_input::pointer(&surface, &editor, &native_move));
    sibling.remove_native(&foreign);

    // Leaving an earlier hover owner may also invoke its arbitrary callback.
    assert_eq!(
        source.dispatch(&dragging, &click(true)),
        InputDisposition::Handled
    );
    sibling.state_mut().events.set(EventKind::HoverOut, true);
    set_hovered(&surface, vec![sibling.clone()]);
    let returned = PlatformInput::MouseMove(gpui::MouseMoveEvent {
        position: gpui::point(gpui::px(20.0), gpui::px(20.0)),
        pressed_button: Some(MouseButton::Left),
        ..Default::default()
    });
    assert_eq!(
        source.dispatch(&dragging, &returned),
        InputDisposition::Rejected
    );
    assert!(!crate::text_input::pointer(&surface, &editor, &native_move));
    sibling.state_mut().events.set(EventKind::HoverOut, false);
    set_hovered(&surface, Vec::new());

    let before_reparent = source.sample().unwrap();
    let another_parent = view(&surface, 8, ROOT_ID, ViewKind::View);
    surface.mounter.borrow_mut().apply(
        &surface,
        &MountItem::Insert {
            parent: another_parent.id(),
            child: editor.id(),
            before: None,
        },
    );
    assert_eq!(
        source.dispatch(&before_reparent, &click(true)),
        InputDisposition::Rejected
    );
    let reparented = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&reparented, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(
        source.dispatch(&reparented, &click(false)),
        InputDisposition::Handled
    );

    surface
        .mounter
        .borrow_mut()
        .apply(&surface, &MountItem::Remove { id: editor.id() });
    let replacement = view(&surface, editor.id(), another_parent.id(), ViewKind::Input);
    assert_eq!(
        source.dispatch(&reparented, &click(true)),
        InputDisposition::Rejected
    );
    let current = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&current, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(crate::text_input::focused_view(&surface), Some(replacement));
    assert_eq!(
        source.dispatch(&current, &click(false)),
        InputDisposition::Handled
    );
}

#[test]
fn resize_and_modal_changes_cancel_only_the_original_surfaces_native_capture() {
    let surface = new_surface();
    let button = view(&surface, 2, ROOT_ID, ViewKind::View);
    button.state_mut().props.input_action = Some(7);
    button.state_mut().events.set(EventKind::Press, true);
    button.state_mut().tracked = PRESSED;
    let source = InputSource::new(&surface);
    let first = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&first, &click(true)),
        InputDisposition::Handled
    );

    let other = new_surface();
    let foreign = InputSource::new(&other).sample().unwrap();
    assert_eq!(
        source.dispatch(&foreign, &click(false)),
        InputDisposition::Rejected
    );
    assert_eq!(button.state().states, PRESSED);
    surface.resized(500.0, 400.0, 1.0);
    assert_eq!(
        source.dispatch(&first, &click(false)),
        InputDisposition::Rejected
    );
    assert_eq!(button.state().states, 0);
    assert_eq!(surface.input_pressed.get(), 0);
    assert!(surface.input.pointers.borrow().is_empty());

    let resized = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&resized, &click(true)),
        InputDisposition::Handled
    );
    let modal = view(&surface, 4, ROOT_ID, ViewKind::Modal);
    modal.state_mut().frame = crate::geom::rect(350.0, 350.0, 25.0, 25.0);
    assert_eq!(pick(&surface.root, 20.0, 20.0, true).unwrap().view.id(), 2);
    assert_eq!(
        source.dispatch(&resized, &click(false)),
        InputDisposition::Rejected
    );
    assert_eq!(button.state().states, 0);
    assert_eq!(surface.input_pressed.get(), 0);
    assert!(surface.input.pointers.borrow().is_empty());
}

#[test]
fn native_thumb_over_a_full_width_button_never_acquires_a_button_lease() {
    let surface = new_surface();
    let scroll = view(&surface, 2, ROOT_ID, ViewKind::Scroll);
    scroll.state_mut().scroll.as_mut().unwrap().content = size(Points(300.0), Points(1200.0));
    let button = view(&surface, 4, 2, ViewKind::View);
    button.state_mut().props.input_action = Some(7);
    button.state_mut().events.set(EventKind::Press, true);
    button.state_mut().tracked = PRESSED;
    let source = InputSource::new(&surface);
    let ordinary = source.sample().unwrap();
    let at_thumb = PlatformInput::MouseDown(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: gpui::point(gpui::px(296.0), gpui::px(20.0)),
        click_count: 1,
        ..Default::default()
    });
    scroll.state_mut().scroll.as_mut().unwrap().indicator = true;
    assert_eq!(
        source.dispatch(&ordinary, &at_thumb),
        InputDisposition::Rejected
    );
    let with_thumb = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&with_thumb, &at_thumb),
        InputDisposition::ExactFallback
    );
    assert!(surface.input.pointers.borrow().is_empty());
    // The same snapshot's body remains an ordinary sealed Button.
    assert_eq!(
        source.dispatch(&with_thumb, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(
        source.dispatch(&with_thumb, &at_thumb),
        InputDisposition::ExactFallback
    );
    assert!(surface.input.pointers.borrow().is_empty());
    assert_eq!(button.state().states, 0);
    // The exact GPUI fallback now owns a thumb, rather than the previous
    // sealed Button. Cancelling a leased capture must leave that owner alone.
    pointer(
        &surface,
        PointerEvent {
            kind: PointerKind::Down,
            x: 296.0,
            y: 20.0,
            button: 0,
            pointer: 1,
            touch: false,
            pen: false,
            time: surface.clock(),
            modifiers: Modifiers::default(),
            clicks: 1,
        },
    );
    source.cancel_capture();
    assert_eq!(surface.input.pointers.borrow().len(), 1);
    assert!(
        matches!(&surface.input.pointers.borrow()[0].mode, Mode::Thumb(view, _) if *view == scroll)
    );
    pointer(
        &surface,
        PointerEvent {
            kind: PointerKind::Cancel,
            x: 296.0,
            y: 20.0,
            button: 0,
            pointer: 1,
            touch: false,
            pen: false,
            time: surface.clock(),
            modifiers: Modifiers::default(),
            clicks: 1,
        },
    );
    assert_eq!(
        source.dispatch(&with_thumb, &click(true)),
        InputDisposition::Handled
    );
    assert_eq!(
        source.dispatch(&with_thumb, &click(false)),
        InputDisposition::Handled
    );
}

#[test]
fn editor_text_lease_survives_its_own_edits_but_never_targets_a_different_focus_owner() {
    let surface = new_surface();
    let editor = view(&surface, 2, ROOT_ID, ViewKind::Input);
    crate::text_input::focus_view(&surface, &editor);
    let source = InputSource::new(&surface);
    let first = source.sample().unwrap();
    assert!(source.editor_matches(&first));
    crate::text_input::char_input(&surface, "maka");
    assert!(source.editor_matches(&first));
    crate::text_input::char_input(&surface, "🦀");
    assert_eq!(
        crate::text_input::accessible_value(&editor)
            .map(|value| value.text)
            .as_deref(),
        Some("maka🦀")
    );
    crate::text_input::window_focus(&surface, false);
    assert!(!source.editor_matches(&first));
    crate::text_input::window_focus(&surface, true);
    assert!(source.editor_matches(&first));
    surface.resized(500.0, 400.0, 1.0);
    editor.state_mut().frame.size.height = Points(200.0);
    assert!(
        source.editor_matches(&first),
        "text has an owner rather than pixel coordinates"
    );

    let other = view(&surface, 4, ROOT_ID, ViewKind::Input);
    crate::text_input::focus_view(&surface, &other);
    assert!(!source.editor_matches(&first));
    let second = source.sample().unwrap();
    assert!(source.editor_matches(&second));
    crate::text_input::char_input(&surface, "other");
    assert_eq!(
        crate::text_input::accessible_value(&other)
            .map(|value| value.text)
            .as_deref(),
        Some("other")
    );
    assert_eq!(
        crate::text_input::accessible_value(&editor)
            .map(|value| value.text)
            .as_deref(),
        Some("maka🦀")
    );

    let read_only = nsh_core::InputConfig {
        editable: false,
        ..Default::default()
    };
    surface.mounter.borrow_mut().apply(
        &surface,
        &MountItem::Input {
            id: 4,
            config: Box::new(read_only),
        },
    );
    assert!(!source.editor_matches(&second));
    crate::text_input::char_input(&surface, "must not append");
    assert_eq!(
        crate::text_input::accessible_value(&other)
            .map(|value| value.text)
            .as_deref(),
        Some("other")
    );

    surface
        .mounter
        .borrow_mut()
        .apply(&surface, &MountItem::Remove { id: 2 });
    let replacement = view(&surface, 2, ROOT_ID, ViewKind::Input);
    crate::text_input::focus_view(&surface, &replacement);
    assert!(
        !source.editor_matches(&first),
        "the same node id is a new editor"
    );
    let current = source.sample().unwrap();
    assert!(source.editor_matches(&current));
    crate::text_input::char_input(&surface, "new");
    assert_eq!(
        crate::text_input::accessible_value(&replacement)
            .map(|value| value.text)
            .as_deref(),
        Some("new")
    );
    assert_eq!(
        crate::text_input::accessible_value(&editor)
            .map(|value| value.text)
            .as_deref(),
        Some("maka🦀")
    );
}

#[test]
fn native_keys_keep_the_original_editor_through_redraws_and_release_after_tab() {
    let surface = new_surface();
    let editor = view(&surface, 2, ROOT_ID, ViewKind::Input);
    let next = view(&surface, 4, ROOT_ID, ViewKind::Input);
    crate::text_input::focus_view(&surface, &editor);
    let source = InputSource::new(&surface);
    let original = source.sample().unwrap();
    crate::text_input::char_input(&surface, "ab");
    surface.resized(500.0, 450.0, 1.0);
    editor.state_mut().frame.size.height = Points(180.0);
    assert_eq!(
        source.dispatch(&original, &key_event("backspace", true)),
        InputDisposition::Handled
    );
    assert_eq!(
        crate::text_input::accessible_value(&editor).unwrap().text,
        "a"
    );
    assert_eq!(
        source.dispatch(&original, &key_event("tab", true)),
        InputDisposition::Handled
    );
    assert_eq!(
        crate::text_input::focused_view(&surface),
        Some(next.clone())
    );
    assert_eq!(
        source.dispatch(&original, &key_event("tab", false)),
        InputDisposition::Handled
    );
    assert_eq!(
        source.dispatch(&original, &key_event("backspace", true)),
        InputDisposition::Rejected
    );
    assert_eq!(crate::text_input::accessible_value(&next).unwrap().text, "");

    // A route without native focus may pick scrolling under the live pointer.
    crate::text_input::blur_all(&surface);
    let scroll = view(&surface, 6, ROOT_ID, ViewKind::Scroll);
    scroll.state_mut().scroll.as_mut().unwrap().content = size(Points(300.0), Points(1200.0));
    surface.input.last.set(Some((20.0, 20.0)));
    let unfocused = source.sample().unwrap();
    assert_eq!(
        source.dispatch(&unfocused, &key_event("down", true)),
        InputDisposition::ExactFallback
    );
    assert_eq!(scroll.state().scroll.as_ref().unwrap().y, Points(0.0));
}

#[test]
fn physical_mouse_cleanup_ends_native_selection_thumb_and_hover_without_a_new_hit() {
    let surface = new_surface();
    let scroll = view(&surface, 2, ROOT_ID, ViewKind::Scroll);
    {
        let mut state = scroll.state_mut();
        let scroll = state.scroll.as_mut().unwrap();
        scroll.content = size(Points(300.0), Points(1200.0));
        scroll.indicator = true;
    }
    let editor = view(&surface, 4, 2, ViewKind::Input);
    editor.state_mut().frame.size.width = Points(200.0);
    track_states(&surface, &editor, PRESSED | HOVERED);
    crate::text_input::focus_view(&surface, &editor);
    crate::text_input::char_input(&surface, "maka");
    let source = InputSource::new(&surface);
    let mouse = |kind, x, y| {
        pointer(
            &surface,
            PointerEvent {
                kind,
                x,
                y,
                button: 0,
                pointer: 1,
                touch: false,
                pen: false,
                time: surface.clock(),
                modifiers: Modifiers::default(),
                clicks: 1,
            },
        );
    };
    mouse(PointerKind::Move, 20.0, 20.0);
    mouse(PointerKind::Down, 20.0, 20.0);
    assert_eq!(editor.state().states, PRESSED | HOVERED);
    assert_eq!(surface.input.pointers.borrow().len(), 1);
    source.cancel_pointer();
    assert!(surface.input.pointers.borrow().is_empty());
    assert_eq!(surface.input_pressed.get(), 0);
    assert_eq!(editor.state().states, 0);
    assert_eq!(surface.input.last.get(), None);
    let movement = ItemPointer {
        kind: PointerKind::Move,
        x: 50.0,
        y: 20.0,
        clicks: 1,
        modifiers: Modifiers::default(),
    };
    assert!(!crate::text_input::pointer(&surface, &editor, &movement));
    mouse(PointerKind::Down, 20.0, 20.0);
    assert!(crate::text_input::pointer(&surface, &editor, &movement));
    source.cancel_pointer();
    assert_eq!(
        crate::text_input::accessible_value(&editor)
            .map(|value| value.text)
            .as_deref(),
        Some("maka")
    );

    mouse(PointerKind::Down, 296.0, 20.0);
    assert!(matches!(
        &surface.input.pointers.borrow()[0].mode,
        Mode::Thumb(view, _) if *view == scroll
    ));
    source.cancel_pointer();
    assert!(surface.input.pointers.borrow().is_empty());
    crate::scroll::thumb_move(&surface, &scroll, 0.0, 10_000.0);
    assert_eq!(scroll.state().scroll.as_ref().unwrap().y, Points(0.0));
    mouse(PointerKind::Down, 296.0, 20.0);
    mouse(PointerKind::Move, 296.0, 10_020.0);
    assert_eq!(scroll.state().scroll.as_ref().unwrap().y, Points(900.0));
    source.cancel_pointer();
    assert!(surface.input.pointers.borrow().is_empty());
    assert_eq!(scroll.state().scroll.as_ref().unwrap().y, Points(900.0));
}
