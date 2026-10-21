//! Keys remain on their original native focus owner while unrelated pixels change.

use std::sync::Arc;

use nsh_core::keymap::{KeyMatch, Keymap};

use super::*;

pub(super) struct Lease {
    focus: Option<NativeOwner>,
    text: Option<NativeOwner>,
    area: Option<NativeOwner>,
    bindings: Option<Arc<Keymap>>,
    action: Option<u64>,
}

fn opaque_window(surface: &Surface) -> bool {
    blocked(surface)
        || crate::chrome::owns_window_keys(surface)
        || !surface.shortcuts.borrow().is_empty()
}

impl Lease {
    pub(super) fn sample(surface: &Surface) -> Option<Self> {
        if opaque_window(surface) || !crate::text_input::window_active(surface) {
            return None;
        }
        let focus = crate::text_input::focused_view(surface)
            .or_else(|| crate::native::focused_component(surface).and_then(|id| surface.view(id)));
        if focus.as_ref().is_some_and(|view| {
            matches!(
                view.kind(),
                ViewKind::Native | ViewKind::WebView | ViewKind::Video
            ) || !focusable(surface, view)
        }) {
            return None;
        }
        let action = focus
            .as_ref()
            .and_then(|view| view.state().props.input_action);
        Some(Self {
            focus: focus.map(NativeOwner::sample),
            text: crate::text::keyboard_owner(surface).map(NativeOwner::sample),
            area: crate::selection_area::keyboard_owner(surface).map(NativeOwner::sample),
            bindings: crate::keymap::snapshot(surface),
            action,
        })
    }

    fn matches(&self, surface: &Surface) -> bool {
        let current = Self::sample(surface);
        let Some(current) = current else { return false };
        self.focus == current.focus
            && self.text == current.text
            && self.area == current.area
            && [&self.focus, &self.text, &self.area]
                .into_iter()
                .flatten()
                .all(|owner| owner.matches(surface))
    }
}

pub(super) fn dispatch(
    surface: &Rc<Surface>,
    frame: &Frame,
    event: &PlatformInput,
) -> InputDisposition {
    if surface.load_generation() != frame.load {
        return InputDisposition::Rejected;
    }
    let (keystroke, down, repeat) = match event {
        PlatformInput::KeyDown(event) => (&event.keystroke, true, event.is_held),
        PlatformInput::KeyUp(event) => (&event.keystroke, false, false),
        _ => return InputDisposition::ExactFallback,
    };
    let Some(vk) = crate::keys::vk_of(&keystroke.key) else {
        return InputDisposition::ExactFallback;
    };
    let event = KeyEvent {
        vk,
        down,
        repeat,
        modifiers: crate::keys::modifiers(&keystroke.modifiers),
    };
    if !down {
        // Releasing custody never enters app callbacks. A preceding down may
        // itself have moved focus or opened a new scope; still release its key.
        key(surface, event);
        return InputDisposition::Handled;
    }
    if crate::navigation::focus_scope(surface) != frame.scope || blocked(surface) != frame.blocked {
        return InputDisposition::Rejected;
    }
    let Some(lease) = &frame.keyboard else {
        return InputDisposition::ExactFallback;
    };
    if !lease.matches(surface) {
        return InputDisposition::Rejected;
    }
    let path = surface.focus_path();
    let Some(binding) = crate::keymap::presented_match(
        surface,
        lease.bindings.as_deref(),
        &event,
        crate::text_input::focused(surface).is_some(),
        &path,
    ) else {
        return InputDisposition::Rejected;
    };
    if binding == KeyMatch::None && lease.focus.is_none() {
        // Without a focus owner, native scrolling picks under the live pointer.
        // That route still needs the embedding's exact-presentation guard.
        return InputDisposition::ExactFallback;
    }
    // A native editor consumes Return itself. Outside an editor, Enter/Space
    // may activate only the originally sealed press action of the focus owner.
    if binding == KeyMatch::None
        && matches!(vk, crate::keys::VK_RETURN | crate::keys::VK_SPACE)
        && let Some(owner) = &lease.focus
    {
        let state = owner.owner.state();
        if state.kind != ViewKind::Input && has_press(state.events) {
            if state.props.input_action != lease.action {
                return InputDisposition::Rejected;
            }
            if lease.action.is_none() {
                return InputDisposition::ExactFallback;
            }
        }
    }
    if key(surface, event) {
        InputDisposition::Handled
    } else {
        // Plain text still enters through the embedding's original-editor
        // InputHandler lease. Unknown routes retain the exact-frame boundary.
        InputDisposition::ExactFallback
    }
}
