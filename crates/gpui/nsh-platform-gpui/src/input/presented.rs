//! Native input paired with an externally presented GPUI frame.
//!
//! Sampling grants no authority. The embedding pairs this snapshot with the
//! exact pixels from the same draw and retains it until queued inputs release
//! that frame. Only the sole output writer publishes the paired frame token.

use std::rc::Weak;

use gpui::{MouseButton, PlatformInput, ScrollDelta};
use nsh_core::{Bounds, EdgeInsets, ScrollBehavior, input::laid_out};

use super::*;

mod keyboard;

/// A main-thread input source that does not keep its app alive.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct InputSource {
    surface: Weak<Surface>,
}

/// Immutable native hit geometry, focus and original view identities from one draw.
///
/// Keep this on the GPUI thread. Input readers carry the embedding's published
/// frame token; they never resolve coordinates against the live scene.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct InputSnapshot {
    frame: Option<Rc<Frame>>,
}

/// What the original native input route permits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputDisposition {
    /// The original native owner handled the input, including an edge clamp.
    Handled,
    /// An opaque route needs the embedding's exact-presentation GPUI path.
    ExactFallback,
    /// The original owner or presentation became invalid. Never fall back.
    Rejected,
}

struct Frame {
    source: Weak<Surface>,
    load: u64,
    viewport: (f32, f32),
    scale: f32,
    origin: (f32, f32),
    scope: NshView,
    blocked: bool,
    root: HitNode,
    editor: Option<NativeOwner>,
    keyboard: Option<keyboard::Lease>,
}

#[derive(Clone, PartialEq)]
struct NativeOwner {
    owner: NshView,
    ancestry: Vec<NshView>,
}

impl NativeOwner {
    fn sample(owner: NshView) -> Self {
        Self {
            ancestry: chain(&owner),
            owner,
        }
    }

    fn matches(&self, surface: &Surface) -> bool {
        surface.view(self.owner.id()).as_ref() == Some(&self.owner)
            && chain(&self.owner) == self.ancestry
            && self.owner.attached()
    }
}

/// Shared by live and sampled hit testing, so clipping and pointer policy
/// cannot drift between resolving a presentation and validating its owner.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PickGeometry {
    pub(super) pointer_events: PointerEvents,
    pub(super) clips: bool,
    size: (f32, f32),
    slop: EdgeInsets,
    overflow: Option<Bounds<Points>>,
    laid_out: bool,
}

impl PickGeometry {
    pub(super) fn sample(state: &ViewState) -> Option<Self> {
        if state.style.hidden
            || state.concealed
            || state.props.pointer_events == PointerEvents::None
            || state.live.as_ref().is_some_and(|live| live.dying())
        {
            return None;
        }
        Some(Self {
            pointer_events: state.props.pointer_events,
            clips: state.clips(),
            size: (state.frame.size.width.0, state.frame.size.height.0),
            slop: state.props.hit_slop,
            overflow: state.overflow,
            laid_out: laid_out(state.kind, state.id),
        })
    }

    pub(super) fn inside(&self, x: f32, y: f32) -> bool {
        x >= 0.0 && y >= 0.0 && x < self.size.0 && y < self.size.1
    }

    pub(super) fn owns(&self, x: f32, y: f32) -> bool {
        x >= -self.slop.left.0
            && y >= -self.slop.top.0
            && x < self.size.0 + self.slop.right.0
            && y < self.size.1 + self.slop.bottom.0
    }

    pub(super) fn children(&self, x: f32, y: f32) -> bool {
        if self.clips {
            self.inside(x, y)
        } else {
            !self.laid_out
                || self.owns(x, y)
                || self.overflow.is_some_and(|rect| {
                    x >= rect.origin.x.0
                        && y >= rect.origin.y.0
                        && x < rect.origin.x.0 + rect.size.width.0
                        && y < rect.origin.y.0 + rect.size.height.0
                })
        }
    }
}

struct HitNode {
    view: NshView,
    shape: PickGeometry,
    opaque_press: bool,
    opaque_wheel: bool,
    binding: Option<u64>,
    scroll: Option<(bool, bool, ScrollBehavior)>,
    thumb: Option<(Mat4, crate::scroll::ThumbHit)>,
    route: Vec<RouteNode>,
    children: Vec<(Mat4, HitNode)>,
}

#[derive(Clone, PartialEq)]
struct RouteNode {
    view: NshView,
    matrix: Mat4,
    shape: PickGeometry,
}

#[derive(Clone)]
pub(super) struct PressLease {
    route: Vec<RouteNode>,
    binding: u64,
    scope: NshView,
    load: u64,
    viewport: (f32, f32),
    scale: f32,
    origin: (f32, f32),
}

impl PressLease {
    pub(super) fn binding_for(&self, view: &NshView) -> Option<u64> {
        (self.route[0].view == *view).then_some(self.binding)
    }
}

impl HitNode {
    fn sample(surface: &Surface, view: &NshView) -> Option<Self> {
        let state = view.state();
        let shape = PickGeometry::sample(&state)?;
        let opaque_wheel = opaque(surface, view, false);
        let opaque_press = opaque(surface, view, true);
        let binding = (!opaque_press
            && focus_safe(surface, view)
            && state.events.contains(EventKind::Press)
            && !state.events.contains(EventKind::LongPress)
            && !state.props.accessibility.state.disabled)
            .then_some(state.props.input_action)
            .flatten();
        let scroll = state
            .scroll
            .as_ref()
            .filter(|scroll| scroll.behavior.enabled)
            .map(|scroll| (scroll.horizontal, scroll.mirrored, scroll.behavior.clone()));
        let children = view.with_children(|children| {
            children
                .iter()
                .filter_map(|child| {
                    let child_state = child.state();
                    let (x, y) = state.child_origin(child_state.frame, child_state.shift);
                    let matrix = child_state
                        .transform()
                        .unwrap_or_default()
                        .then(&Mat4::new().translate(x, y));
                    drop(child_state);
                    Some((matrix, Self::sample(surface, child)?))
                })
                .collect()
        });
        drop(state);
        let route = if binding.is_some() || scroll.is_some() {
            route(view)?
        } else {
            Vec::new()
        };
        let thumb = if let Some(thumb) = crate::scroll::thumb_hit(view) {
            Some((view.to_root()?, thumb))
        } else {
            None
        };
        Some(Self {
            view: view.clone(),
            shape,
            opaque_press,
            opaque_wheel,
            binding,
            scroll,
            thumb,
            route,
            children,
        })
    }

    fn path(&self, x: f32, y: f32) -> Option<Vec<&Self>> {
        let own = || {
            (self.shape.pointer_events != PointerEvents::BoxNone && self.shape.owns(x, y))
                .then(|| vec![self])
        };
        if self.shape.children(x, y) && self.shape.pointer_events != PointerEvents::BoxOnly {
            for (matrix, child) in self.children.iter().rev() {
                if let Some((x, y)) = matrix.unproject(x, y)
                    && let Some(mut path) = child.path(x, y)
                {
                    path.push(self);
                    return Some(path);
                }
            }
        }
        own()
    }
}

fn route(view: &NshView) -> Option<Vec<RouteNode>> {
    chain(view)
        .into_iter()
        .map(|view| {
            let matrix = view.to_root()?;
            let shape = PickGeometry::sample(&view.state())?;
            Some(RouteNode {
                view,
                matrix,
                shape,
            })
        })
        .collect()
}

fn opaque(surface: &Surface, view: &NshView, press: bool) -> bool {
    let state = view.state();
    state.opacity() <= 0.0
        || !state.natives.is_empty()
        || matches!(
            state.kind,
            ViewKind::Input | ViewKind::Native | ViewKind::WebView | ViewKind::Video
        )
        || state.events.contains(EventKind::HoverIn)
        || state.events.contains(EventKind::HoverOut)
        || state.events.contains(EventKind::Touch)
        || state.events.contains(EventKind::PanStart)
        || state.events.contains(EventKind::PanEnd)
        || (press && state.events.contains(EventKind::LongPress))
        || (press && state.events.contains(EventKind::Focus))
        || !gesture_values(surface, view, state.id).is_empty()
        || state.props.pan_activation.is_some()
        || (press && (state.props.selection_area || crate::text::selectable(&state)))
        || state
            .text
            .as_ref()
            .is_some_and(|text| !text.text.links.is_empty())
        || (!press
            && [
                EventKind::ScrollBeginDrag,
                EventKind::ScrollEndDrag,
                EventKind::ScrollEnd,
                EventKind::MomentumScrollBegin,
                EventKind::MomentumScrollEnd,
            ]
            .into_iter()
            .any(|event| state.events.contains(event)))
}

fn focus_safe(surface: &Surface, view: &NshView) -> bool {
    let current = crate::text_input::focused_view(surface);
    current.as_ref() == Some(view)
        || (!view.state().events.contains(EventKind::Focus)
            && current.is_none_or(|view| !view.state().events.contains(EventKind::Blur)))
}

fn blocked(surface: &Surface) -> bool {
    crate::chrome::menu_open(surface) || crate::system::has_dialog(surface)
}

impl InputSource {
    pub(crate) fn new(surface: &Rc<Surface>) -> Self {
        Self {
            surface: Rc::downgrade(surface),
        }
    }

    /// Samples the native scene during the embedding's actual draw callback.
    /// The caller pairs it with that draw's pixels before publishing either.
    pub fn sample(&self) -> Option<InputSnapshot> {
        let surface = self.surface.upgrade()?;
        Some(InputSnapshot {
            frame: Some(Rc::new(Frame {
                source: self.surface.clone(),
                load: surface.load_generation(),
                viewport: surface.size(),
                scale: surface.environment.get().pixel_ratio,
                origin: surface.origin.get(),
                scope: crate::navigation::focus_scope(&surface),
                blocked: blocked(&surface),
                root: HitNode::sample(&surface, &surface.root)?,
                editor: crate::text_input::focused(&surface)
                    .filter(|view| {
                        focusable(&surface, view)
                            && crate::text_input::wants_text(&surface)
                            && crate::text_input::window_active(&surface)
                    })
                    .map(NativeOwner::sample),
                keyboard: keyboard::Lease::sample(&surface),
            })),
        })
    }

    /// Cancels this surface's leased native capture without invoking handlers.
    /// Call when the embedding loses its pointer or retires its own capture;
    /// an unrelated rejected frame does not authorize cancelling it.
    pub fn cancel_capture(&self) {
        if let Some(surface) = self.surface.upgrade() {
            cancel(&surface);
        }
    }

    /// Ends this surface's NSH-owned Mouse capture after a physical release,
    /// leave, focus loss or resize, without invoking application callbacks.
    ///
    /// This is independent of frame admission: the embedding must own that
    /// physical mouse. It cancels the existing owner, never a newly hit view.
    /// Native hover is cleared until the next real move. Registered GPUI
    /// components retain their own GPUI capture lifecycle.
    pub fn cancel_pointer(&self) {
        let Some(surface) = self.surface.upgrade() else {
            return;
        };
        surface.input.presented_press.borrow_mut().take();
        if let Some(mut pointer) = take(&surface, Key::Mouse) {
            if let Some(item) = pointer.item.take() {
                // The capturing built-ins are Input/Text, Dialog and video
                // controls. Their Cancel only clears native drag state.
                item.send(
                    &surface,
                    PointerKind::Cancel,
                    (pointer.x, pointer.y),
                    1,
                    pointer.modifiers,
                );
            }
            release_pressed(&surface, &mut pointer);
            if let Mode::Thumb(view, _) | Mode::Scroll(view, _) = pointer.mode {
                crate::scroll::cancel_pointer(&surface, &view);
            }
            if pointer.reporting {
                // Surface::stop clears pointers before another load starts,
                // so this is still the engine that owns this contact id.
                surface.control(|control| control.cancel_touch(pointer.id));
            }
        }
        surface.input_pressed.set(0);
        let at = surface.input.last.take();
        for view in surface.input.hovered.take() {
            set_state(&surface, &view, HOVERED, false);
        }
        let hover_item = surface.input.hover_item.borrow_mut().take();
        if let Some(item) = hover_item
            && let Some(at) = at
        {
            // Leave only resets the hosted built-ins' native hover state.
            item.send(&surface, PointerKind::Leave, at, 0, Modifiers::default());
        }
    }

    /// Whether the original focused Input still accepts native text editing.
    ///
    /// Use immediately before this AppView's existing InputHandler text or paste
    /// entry, with no intervening event dispatch or pump. This does not permit
    /// a KeyDown callback, keymap command or pointer action. Text, selection,
    /// scroll and size may change while the same editor and scope keep focus.
    pub fn editor_matches(&self, snapshot: &InputSnapshot) -> bool {
        let Some(surface) = self.surface.upgrade() else {
            return false;
        };
        let Some(frame) = snapshot.frame.as_deref() else {
            return false;
        };
        let Some(editor) = &frame.editor else {
            return false;
        };
        Weak::ptr_eq(&self.surface, &frame.source)
            && !frame.blocked
            && !blocked(&surface)
            && surface.load_generation() == frame.load
            && crate::navigation::focus_scope(&surface) == frame.scope
            && crate::text_input::focused_view(&surface).as_ref() == Some(&editor.owner)
            && editor.matches(&surface)
            && focusable(&surface, &editor.owner)
            && crate::text_input::wants_text(&surface)
            && crate::text_input::window_active(&surface)
    }

    /// Delivers input against its original externally presented snapshot.
    ///
    /// The embedding must first resolve its published frame token, rejecting
    /// unknown or retired tokens. `ExactFallback` alone permits GPUI dispatch,
    /// after the embedding's additional exact-frame check. A rejected lease
    /// never reinterprets its coordinates. Discrete wheels remain on their
    /// original enabled Scroll even at an edge or after content grows. Native
    /// keys stay with their original focus owner and keymap registration,
    /// using its current editor text without requiring unchanged pixels.
    pub fn dispatch(&self, snapshot: &InputSnapshot, event: &PlatformInput) -> InputDisposition {
        let Some(surface) = self.surface.upgrade() else {
            return InputDisposition::Rejected;
        };
        let Some(frame) = snapshot.frame.as_deref() else {
            return InputDisposition::Rejected;
        };
        if !Weak::ptr_eq(&self.surface, &frame.source) {
            return InputDisposition::Rejected;
        }
        if matches!(event, PlatformInput::KeyDown(_) | PlatformInput::KeyUp(_)) {
            return keyboard::dispatch(&surface, frame, event);
        }
        if surface.load_generation() != frame.load
            || surface.size() != frame.viewport
            || surface.environment.get().pixel_ratio != frame.scale
            || surface.origin.get() != frame.origin
            || crate::navigation::focus_scope(&surface) != frame.scope
            || blocked(&surface) != frame.blocked
        {
            if surface
                .input
                .presented_press
                .borrow()
                .as_ref()
                .is_some_and(|lease| {
                    lease.load == frame.load
                        && lease.scope == frame.scope
                        && lease.viewport == frame.viewport
                        && lease.scale == frame.scale
                        && lease.origin == frame.origin
                })
            {
                cancel(&surface);
            }
            return InputDisposition::Rejected;
        }
        if frame.blocked {
            return InputDisposition::ExactFallback;
        }
        match event {
            PlatformInput::ScrollWheel(event) => self.wheel(&surface, frame, event),
            PlatformInput::MouseDown(event) if event.button == MouseButton::Left => self.pointer(
                &surface,
                frame,
                PointerKind::Down,
                event.position,
                &event.modifiers,
                event.click_count as u32,
            ),
            PlatformInput::MouseMove(event) if surface.input.presented_press.borrow().is_some() => {
                if event.pressed_button != Some(MouseButton::Left) {
                    cancel(&surface);
                    return InputDisposition::Handled;
                }
                self.pointer(
                    &surface,
                    frame,
                    PointerKind::Move,
                    event.position,
                    &event.modifiers,
                    0,
                )
            }
            PlatformInput::MouseUp(event)
                if event.button == MouseButton::Left
                    && surface.input.presented_press.borrow().is_some() =>
            {
                self.pointer(
                    &surface,
                    frame,
                    PointerKind::Up,
                    event.position,
                    &event.modifiers,
                    event.click_count as u32,
                )
            }
            PlatformInput::MouseDown(_) | PlatformInput::MouseExited(_) => {
                cancel(&surface);
                InputDisposition::ExactFallback
            }
            _ => InputDisposition::ExactFallback,
        }
    }

    fn pointer(
        &self,
        surface: &Rc<Surface>,
        frame: &Frame,
        kind: PointerKind,
        position: gpui::Point<gpui::Pixels>,
        modifiers: &gpui::Modifiers,
        clicks: u32,
    ) -> InputDisposition {
        let (x, y) = (
            f32::from(position.x) - frame.origin.0,
            f32::from(position.y) - frame.origin.1,
        );
        let captured = surface.input.presented_press.borrow().clone();
        if kind == PointerKind::Down
            && captured.is_none()
            && !surface.input.pointers.borrow().is_empty()
        {
            return InputDisposition::ExactFallback;
        }
        let candidate = if kind == PointerKind::Down {
            frame.root.path(x, y).and_then(|path| {
                (!path.iter().any(|node| node.opaque_press)).then_some(())?;
                (!path.iter().any(|node| {
                    node.thumb.as_ref().is_some_and(|(matrix, thumb)| {
                        matrix
                            .unproject(x, y)
                            .is_some_and(|(x, y)| thumb.contains(x, y))
                    })
                }))
                .then_some(())?;
                let owner = path.into_iter().find(|node| node.binding.is_some())?;
                Some(PressLease {
                    route: owner.route.clone(),
                    binding: owner.binding?,
                    scope: frame.scope.clone(),
                    load: frame.load,
                    viewport: frame.viewport,
                    scale: frame.scale,
                    origin: frame.origin,
                })
            })
        } else {
            captured
        };
        let Some(lease) = candidate else {
            if kind == PointerKind::Down {
                cancel(surface);
            }
            return InputDisposition::ExactFallback;
        };
        let owner = &lease.route[0].view;
        let live = pick(&surface.root, x, y, true);
        if lease.scope != crate::navigation::focus_scope(surface)
            || lease.load != surface.load_generation()
            || lease.viewport != surface.size()
            || lease.scale != surface.environment.get().pixel_ratio
            || lease.origin != surface.origin.get()
            || !owner.attached()
            || route(owner).as_ref() != Some(&lease.route)
            || surface.view(owner.id()).as_ref() != Some(owner)
            || owner.state().props.input_action != Some(lease.binding)
            || !focus_safe(surface, owner)
            || live.as_ref().is_none_or(|hit| {
                hit.item.is_some()
                    || chain(&hit.view)
                        .iter()
                        .any(|view| opaque(surface, view, true))
                    || chain(&hit.view).iter().any(|view| {
                        let (x, y) = local(view, x, y);
                        crate::scroll::grabs_thumb(view, x, y)
                    })
                    || chain(&hit.view)
                        .into_iter()
                        .find(|view| {
                            let state = view.state();
                            has_press(state.events) && !state.props.accessibility.state.disabled
                        })
                        .is_none_or(|view| {
                            view != *owner || view.state().props.input_action != Some(lease.binding)
                        })
            })
            || surface.input.hover_item.borrow().is_some()
        {
            cancel(surface);
            return InputDisposition::Rejected;
        }
        surface.input.last.set(Some((x, y)));
        match kind {
            PointerKind::Down => {
                cancel(surface);
                crate::text::window_pressed(surface, x, y);
                crate::selection_area::window_pressed(surface, x, y);
                *surface.input.presented_press.borrow_mut() = Some(lease);
                down(
                    surface,
                    Key::Mouse,
                    x,
                    y,
                    surface.clock() * 1000.0,
                    clicks,
                    crate::keys::modifiers(modifiers),
                );
                surface.input_pressed.set(surface.input_pressed.get() | 1);
            }
            PointerKind::Move => motion(surface, Key::Mouse, x, y, surface.clock() * 1000.0),
            PointerKind::Up => {
                up(
                    surface,
                    Key::Mouse,
                    Some((x, y)),
                    surface.clock() * 1000.0,
                    false,
                );
                surface.input.presented_press.borrow_mut().take();
                surface.input_pressed.set(surface.input_pressed.get() & !1);
            }
            _ => unreachable!(),
        }
        InputDisposition::Handled
    }

    fn wheel(
        &self,
        surface: &Rc<Surface>,
        frame: &Frame,
        event: &gpui::ScrollWheelEvent,
    ) -> InputDisposition {
        let ScrollDelta::Lines(delta) = event.delta else {
            return InputDisposition::ExactFallback;
        };
        if event.modifiers.control
            || !surface.input.pointers.borrow().is_empty()
            || surface.input.hover_item.borrow().is_some()
            || surface.input.wheel.borrow().is_some()
            || surface.input.pinching()
            || surface.input.zoom.borrow().is_some()
        {
            return InputDisposition::ExactFallback;
        }
        let (x, y) = (
            f32::from(event.position.x) - frame.origin.0,
            f32::from(event.position.y) - frame.origin.1,
        );
        let (mut dx, mut dy) = (
            -delta.x * crate::events::LINE,
            -delta.y * crate::events::LINE,
        );
        if dx == 0.0 && event.modifiers.shift {
            (dx, dy) = (dy, dx);
        }
        if surface
            .input
            .latch
            .borrow()
            .as_ref()
            .is_some_and(|(view, horizontal, last)| {
                latch_holds(
                    *horizontal,
                    *last,
                    dx.abs() > dy.abs(),
                    surface.clock() * 1000.0,
                ) && shown(view)
            })
        {
            return InputDisposition::ExactFallback;
        }
        let Some(path) = frame.root.path(x, y) else {
            return InputDisposition::Rejected;
        };
        if path.iter().any(|node| node.opaque_wheel) {
            return InputDisposition::ExactFallback;
        }
        let horizontal = dx.abs() > dy.abs();
        let along = path
            .iter()
            .any(|node| node.scroll.as_ref().is_some_and(|s| s.0 == horizontal));
        let Some(owner) = path.into_iter().find(|node| {
            node.scroll
                .as_ref()
                .is_some_and(|scroll| !along || scroll.0 == horizontal)
        }) else {
            return InputDisposition::ExactFallback;
        };
        let view = &owner.view;
        let live = pick(&surface.root, x, y, true);
        let policy = view
            .state()
            .scroll
            .as_ref()
            .map(|scroll| (scroll.horizontal, scroll.mirrored, scroll.behavior.clone()));
        if !view.attached()
            || route(view).as_ref() != Some(&owner.route)
            || surface.view(view.id()).as_ref() != Some(view)
            || policy != owner.scroll
            || live.is_none_or(|hit| {
                hit.item.is_some()
                    || chain(&hit.view)
                        .iter()
                        .any(|view| opaque(surface, view, false))
                    || !chain(&hit.view).contains(view)
            })
        {
            return InputDisposition::Rejected;
        }
        surface.input.last.set(Some((x, y)));
        crate::scroll::wheel(surface, view, dx, dy);
        InputDisposition::Handled
    }
}

fn cancel(surface: &Surface) {
    if surface.input.presented_press.borrow_mut().take().is_some() {
        if let Some(mut pointer) = take(surface, Key::Mouse) {
            release_pressed(surface, &mut pointer);
        }
        surface.input_pressed.set(surface.input_pressed.get() & !1);
    }
}

#[cfg(test)]
mod tests;
