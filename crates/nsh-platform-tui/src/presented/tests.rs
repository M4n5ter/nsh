use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use nsh_core::{
    Bounds, Color, Command, EventKind, EventMask, MountBatch, MountItem, NodeId, Point, Points,
    ROOT_ID, Size, Transform, TransformKind, ViewKind, ViewProps, ViewStyle, geometry::px,
};

use crate::{CellMetrics, Surface, SurfaceEvent};

const CLOSE: NodeId = 3;
const CLOSE_ACTION: u64 = 101;

fn frame(x: f32, y: f32, width: f32, height: f32) -> Bounds<Points> {
    Bounds::new(Point::new(px(x), px(y)), Size::new(px(width), px(height)))
}

fn mount(id: NodeId, kind: ViewKind, bounds: Bounds<Points>) -> Vec<MountItem> {
    vec![
        MountItem::Create { id, kind },
        MountItem::Frame { id, frame: bounds },
        MountItem::Insert {
            parent: ROOT_ID,
            child: id,
            before: None,
        },
    ]
}

fn events(id: NodeId, kinds: &[EventKind]) -> MountItem {
    let mut mask = EventMask::default();
    for &kind in kinds {
        mask.set(kind, true);
    }
    MountItem::Events { id, events: mask }
}

fn action(id: NodeId, binding: u64) -> MountItem {
    MountItem::ViewProps {
        id,
        props: Box::new(ViewProps {
            input_action: Some(binding),
            ..Default::default()
        }),
    }
}

fn close_button() -> Vec<MountItem> {
    let mut items = mount(CLOSE, ViewKind::View, frame(8.0, 16.0, 64.0, 32.0));
    items.extend([
        events(
            CLOSE,
            &[EventKind::Press, EventKind::PressDown, EventKind::PressUp],
        ),
        action(CLOSE, CLOSE_ACTION),
    ]);
    items
}

fn surface() -> Surface {
    let mut surface = Surface::new(40, 12, CellMetrics::default());
    let mut items = mount(2, ViewKind::View, frame(0.0, 0.0, 320.0, 192.0));
    items.extend(close_button());
    surface.apply(MountBatch { items });
    surface.drain_events().for_each(drop);
    surface
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn down(column: u16, row: u16) -> MouseEvent {
    mouse(MouseEventKind::Down(MouseButton::Left), column, row)
}

fn up(column: u16, row: u16) -> MouseEvent {
    mouse(MouseEventKind::Up(MouseButton::Left), column, row)
}

fn presses(surface: &mut Surface) -> Vec<NodeId> {
    surface
        .drain_events()
        .filter_map(|event| match event {
            SurfaceEvent::Event(id, EventKind::Press, _) => Some(id),
            _ => None,
        })
        .collect()
}

#[test]
fn a_presented_close_survives_underlay_and_color_feedback_commits() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<crate::InputSnapshot>();
    send_sync::<crate::PressLease>();
    let mut surface = surface();
    let snapshot = surface.input_snapshot();
    let lease = snapshot.press_at(1, 1).expect("presented Close action");
    assert_eq!(lease.binding(), CLOSE_ACTION);
    assert_eq!(lease.target(), CLOSE);

    surface.apply(MountBatch {
        items: vec![MountItem::ViewStyle {
            id: 2,
            style: Box::new(ViewStyle {
                background_color: Color::rgba(20, 30, 40, 255),
                ..Default::default()
            }),
        }],
    });

    assert!(surface.handle_presented_mouse(down(1, 1), &lease).is_some());
    surface.apply(MountBatch {
        items: vec![MountItem::ViewStyle {
            id: CLOSE,
            style: Box::new(ViewStyle {
                background_color: Color::rgba(40, 50, 60, 255),
                ..Default::default()
            }),
        }],
    });
    assert!(surface.handle_presented_mouse(up(1, 1), &lease).is_some());
    assert_eq!(presses(&mut surface), [CLOSE]);
}

#[test]
fn pointer_focus_preserves_a_clipped_press_and_keyboard_focus_reveals_it() {
    let mut surface = Surface::new(40, 12, CellMetrics::default());
    let mut items = mount(2, ViewKind::Scroll, frame(0.0, 0.0, 160.0, 96.0));
    items.extend(close_button());
    items.extend([
        MountItem::ScrollContent {
            id: 2,
            size: Size::new(px(160.0), px(320.0)),
        },
        MountItem::Insert {
            parent: 2,
            child: CLOSE,
            before: None,
        },
        MountItem::Frame {
            id: CLOSE,
            frame: frame(8.0, 32.0, 128.0, 232.0),
        },
    ]);
    surface.apply(MountBatch { items });
    surface.drain_events().for_each(drop);
    let lease = surface
        .input_snapshot()
        .press_at(2, 2)
        .expect("visible part of a tall button");
    assert!(surface.handle_presented_mouse(down(2, 2), &lease).is_some());
    assert_eq!(surface.focused(), Some(CLOSE));
    assert_eq!(surface.node(2).unwrap().scroll.y, px(0.0));
    assert!(surface.handle_presented_mouse(up(2, 2), &lease).is_some());
    assert_eq!(presses(&mut surface), [CLOSE]);

    surface.handle_event(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    assert_eq!(surface.focused(), Some(2));
    surface.handle_event(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    assert_eq!(surface.focused(), Some(CLOSE));
    assert_eq!(surface.node(2).unwrap().scroll.y, px(32.0));
}

#[test]
fn changes_to_the_presented_action_or_its_hit_route_reject_the_old_press() {
    #[derive(Debug)]
    enum Change {
        Binding,
        RecreatedNode,
        Moved,
        Covered,
        Resized,
    }

    for change in [
        Change::Binding,
        Change::RecreatedNode,
        Change::Moved,
        Change::Covered,
        Change::Resized,
    ] {
        let mut surface = surface();
        let snapshot = surface.input_snapshot();
        let lease = snapshot.press_at(1, 1).expect("presented Close action");
        let admitted = matches!(change, Change::Binding);
        if admitted {
            assert!(surface.handle_presented_mouse(down(1, 1), &lease).is_some());
            surface.drain_events().for_each(drop);
        }
        let mut items = Vec::new();
        match change {
            Change::Binding => items.push(action(CLOSE, CLOSE_ACTION + 1)),
            Change::RecreatedNode => {
                items.push(MountItem::Delete { id: CLOSE });
                items.extend(close_button());
            }
            Change::Moved => items.push(MountItem::Frame {
                id: CLOSE,
                frame: frame(160.0, 16.0, 64.0, 32.0),
            }),
            Change::Covered => {
                items.extend(mount(4, ViewKind::View, frame(8.0, 16.0, 64.0, 32.0)));
                items.extend([events(4, &[EventKind::Press]), action(4, 202)]);
            }
            Change::Resized => surface.resize(41, 12),
        }
        surface.apply(MountBatch { items });

        if !admitted {
            assert_eq!(
                surface.handle_presented_mouse(down(1, 1), &lease),
                None,
                "{change:?}"
            );
        }
        assert_eq!(
            surface.handle_presented_mouse(up(1, 1), &lease),
            None,
            "{change:?}"
        );
        let callbacks: Vec<_> = surface
            .drain_events()
            .filter_map(|event| match event {
                SurfaceEvent::Event(
                    id,
                    kind @ (EventKind::Press | EventKind::PressUp | EventKind::PanEnd),
                    _,
                ) => Some((id, kind)),
                _ => None,
            })
            .collect();
        assert_eq!(
            callbacks,
            [],
            "{change:?}: no completion reaches a replacement owner"
        );
    }
}

#[test]
fn an_admitted_press_rejects_geometry_changes_even_when_release_stays_inside() {
    #[derive(Debug)]
    enum Change {
        Frame,
        Transform,
    }

    for change in [Change::Frame, Change::Transform] {
        let mut surface = surface();
        let snapshot = surface.input_snapshot();
        let lease = snapshot.press_at(1, 1).expect("presented Close action");
        assert!(surface.handle_presented_mouse(down(1, 1), &lease).is_some());
        let item = match change {
            Change::Frame => MountItem::Frame {
                id: CLOSE,
                frame: frame(9.0, 16.0, 64.0, 32.0),
            },
            Change::Transform => {
                let mut transform = Transform::IDENTITY;
                transform.push(TransformKind::TranslateX, 1.0).unwrap();
                MountItem::ViewStyle {
                    id: CLOSE,
                    style: Box::new(ViewStyle {
                        transform,
                        ..Default::default()
                    }),
                }
            }
        };
        surface.apply(MountBatch { items: vec![item] });

        // x=12 remains inside both shapes, but this presentation's geometry
        // no longer matches the frame that admitted the mouse-down.
        assert_eq!(
            surface.handle_presented_mouse(up(1, 1), &lease),
            None,
            "{change:?}"
        );
        assert!(presses(&mut surface).is_empty(), "{change:?}");
    }
}

#[test]
fn native_touch_focus_and_scroll_routes_do_not_claim_sealed_press_leases() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use crate::{ComponentContext, ComponentResponse, Components, NativeComponent};

    struct NativeLeaf(Arc<AtomicUsize>);

    impl NativeComponent for NativeLeaf {
        fn render(&self, _: &ComponentContext, _: &mut ratatui::buffer::Buffer) {}

        fn handle_event(&mut self, _: &Event, _: &ComponentContext) -> ComponentResponse {
            self.0.fetch_add(1, Ordering::SeqCst);
            ComponentResponse::handled()
        }
    }

    let mut touch = surface();
    touch.apply(MountBatch {
        items: vec![events(ROOT_ID, &[EventKind::Touch])],
    });
    assert!(
        touch.input_snapshot().press_at(1, 1).is_none(),
        "opaque touch responder"
    );

    let mut focus = surface();
    let mut items = mount(4, ViewKind::Input, frame(160.0, 16.0, 64.0, 32.0));
    items.extend([
        events(4, &[EventKind::Blur]),
        MountItem::Command {
            id: 4,
            command: Command::Focus,
        },
    ]);
    focus.apply(MountBatch { items });
    assert_eq!(focus.focused(), Some(4));
    focus.drain_events().for_each(drop);
    assert!(
        focus.input_snapshot().press_at(1, 1).is_none(),
        "activating Close would invoke the prior input's Blur callback"
    );
    focus.handle_event(Event::Mouse(down(1, 1)));
    focus.handle_event(Event::Mouse(up(1, 1)));
    let callbacks: Vec<_> = focus
        .drain_events()
        .filter_map(|event| match event {
            SurfaceEvent::Event(id, kind @ (EventKind::Blur | EventKind::Press), _) => {
                Some((id, kind))
            }
            _ => None,
        })
        .collect();
    assert_eq!(callbacks, [(4, EventKind::Blur), (CLOSE, EventKind::Press)]);

    let mut native = surface();
    let native_calls = Arc::new(AtomicUsize::new(0));
    let mut components = Components::default();
    components.register("test.leaf", {
        let native_calls = native_calls.clone();
        move || Box::new(NativeLeaf(native_calls.clone()))
    });
    native.set_components(components);
    let mut items = mount(4, ViewKind::Native, frame(8.0, 16.0, 64.0, 32.0));
    items.extend([
        MountItem::Component {
            id: 4,
            name: "test.leaf".into(),
        },
        events(4, &[EventKind::Press]),
        action(4, 202),
    ]);
    native.apply(MountBatch { items });
    assert!(
        native.input_snapshot().press_at(1, 1).is_none(),
        "host-native input"
    );

    native.apply(MountBatch {
        items: vec![MountItem::Frame {
            id: 4,
            frame: frame(160.0, 16.0, 64.0, 32.0),
        }],
    });
    let snapshot = native.input_snapshot();
    let lease = snapshot
        .press_at(1, 1)
        .expect("Close beside native component");
    assert!(native.handle_presented_mouse(down(1, 1), &lease).is_some());
    assert_eq!(
        native.handle_presented_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), 20, 1),
            &lease
        ),
        None
    );
    assert_eq!(
        native_calls.load(Ordering::SeqCst),
        0,
        "stale drag must not enter native B"
    );
    assert!(presses(&mut native).is_empty());
    assert_eq!(native.handle_presented_mouse(up(1, 1), &lease), None);
    assert_eq!(native_calls.load(Ordering::SeqCst), 0);
    assert!(
        presses(&mut native).is_empty(),
        "rejected drag cleared A's capture"
    );

    let mut scroll = surface();
    let mut items = mount(4, ViewKind::Scroll, frame(8.0, 16.0, 64.0, 32.0));
    items.extend([
        MountItem::ScrollContent {
            id: 4,
            size: Size::new(px(64.0), px(320.0)),
        },
        MountItem::Insert {
            parent: 4,
            child: CLOSE,
            before: None,
        },
        MountItem::Frame {
            id: CLOSE,
            frame: frame(0.0, 0.0, 64.0, 32.0),
        },
    ]);
    scroll.apply(MountBatch { items });
    assert!(
        scroll.input_snapshot().press_at(1, 1).is_some(),
        "ordinary child action"
    );
    assert!(
        scroll.input_snapshot().press_at(8, 1).is_none(),
        "native scrollbar drag"
    );
}
