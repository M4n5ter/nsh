use nsh_core::{MountItem, Points, ViewProps, geometry::size};

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
