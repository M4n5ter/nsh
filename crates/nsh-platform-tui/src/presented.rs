//! Leases for native input owners in a completed terminal presentation.
//!
//! A host publishes a snapshot only after its bytes have flushed. It may keep
//! that snapshot while unrelated work advances the scene: a lease is checked
//! against the current native route before entering its original capture.

use std::{collections::HashMap, sync::Arc};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use nsh_core::{
    Bounds, EventKind, Matrix, NodeId, PanActivation, Points, ScrollBehavior, Size, TextLayout,
    ViewKind, animation::GestureBinding, overlay::HintObservation,
};

use crate::{
    CellMetrics, Surface,
    geometry::{HitSnapshot, project, unproject},
    input::FocusSource,
    render::InputGeometry,
};

/// Native hit shapes, sealed actions, value-only gestures and scroll owners.
///
/// This contains no engine, component or scene owner. Publish it alongside the
/// terminal bytes it describes, after their successful flush, and use
/// [`Self::press_at`] or [`Self::wheel_at`] on the input thread. An absent lease
/// keeps the host's existing exact-presentation path; it does not invalidate
/// that input.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct InputSnapshot {
    instance: Arc<()>,
    geometry: HitSnapshot,
    viewport: (u16, u16),
    metrics: CellMetrics,
    scope: Option<(NodeId, u64)>,
    blocked: bool,
    owners: HashMap<NodeId, NodeId>,
    routes: HashMap<NodeId, PressRoute>,
    // Some(horizontal) selects a scroll axis; None retains native Ctrl+wheel Pinch.
    wheel_owners: HashMap<(NodeId, Option<bool>), NodeId>,
    wheel_routes: HashMap<(NodeId, bool), WheelRoute>,
}

#[derive(Clone, Debug, PartialEq)]
enum PointerTarget {
    Action(u64),
    Input(InputContact),
    Text(TextContact),
    ScrollFocus,
    Pan {
        binding: GestureBinding,
        activation: Option<PanActivation>,
    },
}

#[derive(Clone, Debug)]
struct TextContact {
    revision: u64,
    layout: TextLayout,
    content: Bounds<Points>,
}

impl PartialEq for TextContact {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision
            && Arc::ptr_eq(&self.layout.0, &other.layout.0)
            && self.content == other.content
    }
}

#[derive(Clone, Debug, PartialEq)]
struct InputContact {
    revision: u64,
    geometry: InputGeometry,
    local: bool,
    scroll: (usize, usize),
    line_offsets: Vec<usize>,
    wrap: usize,
    secure: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct PressRoute {
    target: NodeId,
    incarnation: u64,
    owner: PointerTarget,
    ancestry: Vec<(NodeId, u64)>,
    matrix: Matrix,
    size: Size<Points>,
    clip: Bounds<Points>,
}

#[derive(Clone, Debug, PartialEq)]
struct WheelRoute {
    target: NodeId,
    ancestry: Vec<(NodeId, u64)>,
    matrix: Matrix,
    size: Size<Points>,
    clip: Bounds<Points>,
    policy: WheelPolicy,
}

#[derive(Clone, Debug, PartialEq)]
enum WheelPolicy {
    Scroll {
        horizontal: bool,
        mirrored: bool,
        behavior: Box<ScrollBehavior>,
    },
    Pinch(GestureBinding),
}

/// The native Scroll or value-only Pinch owner of a completed presentation.
///
/// This retains a native view incarnation, not an application callback. Its
/// queued onScroll notification still reports a scroll view's current offset.
/// A default lease is inert and authorizes no surface.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct WheelLease {
    instance: Arc<()>,
    route: Option<WheelRoute>,
    viewport: (u16, u16),
    metrics: CellMetrics,
    scope: Option<(NodeId, u64)>,
    origin: (u16, u16),
}

/// A sealed action, native selection/focus or value-only Pan in a presentation.
///
/// Keep this same lease for its left-button Down, Drag and Up. The native
/// surface checks its identity. An action's [`Self::binding`] must additionally
/// be validated in the engine before entering app code; a native Pan has no
/// binding and never authorizes an application input callback.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct PressLease {
    instance: Arc<()>,
    route: PressRoute,
    viewport: (u16, u16),
    metrics: CellMetrics,
    scope: Option<(NodeId, u64)>,
    origin: (u16, u16),
}

impl PressLease {
    /// The sealed app action binding, or `None` for a native selection, focus or Pan.
    pub fn binding(&self) -> Option<u64> {
        match &self.route.owner {
            PointerTarget::Action(binding) => Some(*binding),
            PointerTarget::Input(_)
            | PointerTarget::Text(_)
            | PointerTarget::ScrollFocus
            | PointerTarget::Pan { .. } => None,
        }
    }

    /// Whether the original native owner accepts left-button drag continuation.
    /// Application actions and a plain scroll-focus contact accept only Down/Up.
    pub fn allows_drag(&self) -> bool {
        match &self.route.owner {
            PointerTarget::Action(_) | PointerTarget::ScrollFocus => false,
            PointerTarget::Input(_) | PointerTarget::Text(_) | PointerTarget::Pan { .. } => true,
        }
    }

    fn is_native_pan(&self) -> bool {
        matches!(self.route.owner, PointerTarget::Pan { .. })
    }

    /// The native activation owner, rather than a text child that was hit.
    pub fn target(&self) -> NodeId {
        self.route.target
    }

    fn same_capture(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.instance, &other.instance)
            && self.route.target == other.route.target
            && self.route.incarnation == other.route.incarnation
            && self.route.owner == other.route.owner
            && self.viewport == other.viewport
            && self.metrics == other.metrics
            && self.scope == other.scope
            && self.origin == other.origin
    }
}

impl InputSnapshot {
    /// Resolves a wheel's original Scroll or Ctrl+wheel native Pinch owner.
    ///
    /// Native components, media, captures and immediate application callbacks
    /// keep the embedding's exact-presentation path. Selectable text is ordinary
    /// scroll content. This does not use the focused or first available scroll.
    pub fn wheel_at(&self, mouse: MouseEvent) -> Option<WheelLease> {
        let (column, row) = (mouse.column, mouse.row);
        if column >= self.viewport.0 || row >= self.viewport.1 {
            return None;
        }
        let (x, y) = (
            (f32::from(column) + 0.5) * self.metrics.width().0,
            (f32::from(row) + 0.5) * self.metrics.height().0,
        );
        let hit = self.geometry.hit_test(x, y)?;
        let control = mouse
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL);
        let axis = (!control).then(|| Surface::horizontal_wheel(mouse));
        let owner = self.wheel_owners.get(&(hit, axis))?;
        let route = self.wheel_routes.get(&(*owner, control))?.clone();
        Some(WheelLease {
            instance: self.instance.clone(),
            route: Some(route),
            viewport: self.viewport,
            metrics: self.metrics,
            scope: self.scope,
            origin: (column, row),
        })
    }

    /// Resolves an ordinary left press at the terminal cell's native center.
    ///
    /// Built-in inputs, selectable labels and plain scroll content retain their
    /// native selection or focus owner. Links, native components, scrollbars,
    /// callback-capable gestures, overlays and focus callbacks use exact input.
    pub fn press_at(&self, column: u16, row: u16) -> Option<PressLease> {
        if self.blocked || column >= self.viewport.0 || row >= self.viewport.1 {
            return None;
        }
        let (x, y) = (
            (f32::from(column) + 0.5) * self.metrics.width().0,
            (f32::from(row) + 0.5) * self.metrics.height().0,
        );
        let hit = self.geometry.hit_test(x, y)?;
        if self
            .geometry
            .ancestors(hit)
            .into_iter()
            .any(|id| self.geometry.scrollbar_hit(id, x, y))
        {
            return None;
        }
        let route = self.routes.get(self.owners.get(&hit)?)?.clone();
        Some(PressLease {
            instance: self.instance.clone(),
            route,
            viewport: self.viewport,
            metrics: self.metrics,
            scope: self.scope,
            origin: (column, row),
        })
    }
}

impl Surface {
    /// Samples hit geometry, sealed actions and native scroll/gesture owners.
    ///
    /// Taking a snapshot does not publish it or grant input authority. The
    /// embedding host publishes it only after flushing its corresponding frame.
    pub fn input_snapshot(&self) -> InputSnapshot {
        let geometry = self.hit_snapshot();
        let scope = self.input_scope(&geometry);
        let blocked = self.input_blocked();
        let mut owners = HashMap::new();
        let mut routes = HashMap::new();
        let mut wheel_owners = HashMap::new();
        let mut wheel_routes = HashMap::new();
        if !blocked {
            for &hit in geometry.visible_ids() {
                let path = geometry.ancestors(hit);
                if self.pointer.is_none() && !path.iter().any(|&id| self.opaque_wheel_node(id)) {
                    for horizontal in [false, true] {
                        let Some(target) = self.wheel_scroll_target(path.clone(), horizontal)
                        else {
                            continue;
                        };
                        if let std::collections::hash_map::Entry::Vacant(entry) =
                            wheel_routes.entry((target, false))
                            && let Some(route) = self.wheel_route(target, &geometry)
                        {
                            entry.insert(route);
                        }
                        if wheel_routes.contains_key(&(target, false)) {
                            wheel_owners.insert((hit, Some(horizontal)), target);
                        }
                    }
                }
                if self.pointer.is_none()
                    && !path.iter().any(|&id| self.opaque_native_gesture_node(id))
                    && !path.iter().any(|&id| {
                        self.node(id).is_some_and(|node| {
                            node.kind == ViewKind::Scroll
                                && node.scroll.enabled
                                && node.scroll.behavior.pinch
                                && node.scroll.behavior.zooms()
                        })
                    })
                    && let Some(target) = path.iter().copied().find(|&id| {
                        self.node(id)
                            .is_some_and(|node| node.gestures.pinch.is_some())
                    })
                    && let Some(route) = self.pinch_route(target, &geometry)
                {
                    wheel_routes.entry((target, true)).or_insert(route);
                    wheel_owners.insert((hit, None), target);
                }
                if !path.iter().any(|&id| self.opaque_native_gesture_node(id))
                    && self.native_gesture_focus_safe(&path)
                    && let Some(target) = path.iter().copied().find(|&id| {
                        self.node(id).is_some_and(|node| {
                            node.gestures.pan_x.is_some()
                                || node.gestures.pan_y.is_some()
                                || node.props.pan_activation.is_some()
                        })
                    })
                    && let Some(node) = self.node(target)
                    && let Some(binding) = self.animation.gesture_binding(node.gestures)
                    && (binding.values.pan_x.is_some() || binding.values.pan_y.is_some())
                    && let Some(route) = self.press_route(
                        target,
                        PointerTarget::Pan {
                            binding,
                            activation: node.props.pan_activation,
                        },
                        &geometry,
                    )
                {
                    owners.insert(hit, target);
                    routes.entry(target).or_insert(route);
                    continue;
                }
                if let Some(route) = self.selection_route(hit, &geometry) {
                    owners.insert(hit, hit);
                    routes.entry(hit).or_insert(route);
                    continue;
                }
                if path.iter().any(|&id| self.opaque_press_node(id)) {
                    continue;
                }
                let Some(target) = path.iter().copied().find(|&id| self.pressable(id)) else {
                    if let Some(target) = path.iter().copied().find(|&id| {
                        self.node(id).is_some_and(|node| {
                            node.kind == ViewKind::Scroll && node.scroll.enabled
                        })
                    }) && self.native_contact_safe(target, hit, &geometry)
                        && let Some(route) =
                            self.press_route(target, PointerTarget::ScrollFocus, &geometry)
                    {
                        owners.insert(hit, target);
                        routes.entry(target).or_insert(route);
                    }
                    continue;
                };
                let Some(node) = self.node(target) else {
                    continue;
                };
                let Some(binding) = node.props.input_action else {
                    continue;
                };
                if !node.events.contains(EventKind::Press)
                    || node.events.contains(EventKind::LongPress)
                    || (self.focus != Some(target)
                        && (node.events.contains(EventKind::Focus)
                            || self.focus.is_some_and(|id| {
                                self.node(id)
                                    .is_some_and(|node| node.events.contains(EventKind::Blur))
                            })))
                {
                    continue;
                }
                if let Some(route) =
                    self.press_route(target, PointerTarget::Action(binding), &geometry)
                {
                    owners.insert(hit, target);
                    routes.entry(target).or_insert(route);
                }
            }
        }
        InputSnapshot {
            instance: self.input_instance.clone(),
            geometry,
            viewport: (self.columns, self.rows),
            metrics: self.metrics,
            scope,
            blocked,
            owners,
            routes,
            wheel_owners,
            wheel_routes,
        }
    }

    fn input_scope(&self, geometry: &HitSnapshot) -> Option<(NodeId, u64)> {
        geometry.visible_ids().iter().copied().find_map(|id| {
            self.node(id)
                .filter(|node| node.kind == ViewKind::Modal)
                .map(|node| (id, node.generation))
        })
    }

    fn input_blocked(&self) -> bool {
        self.dialog.is_some()
            || self.service_overlay_open()
            || (self.pointer.is_some() && self.presented_press.is_none())
            || self.hovered_path.iter().any(|&id| {
                self.node(id).is_some_and(|node| {
                    node.events.contains_listener(EventKind::HoverIn)
                        || node.events.contains_listener(EventKind::HoverOut)
                })
            })
    }

    fn opaque_wheel_node(&self, id: NodeId) -> bool {
        self.node(id).is_none_or(|node| {
            node.native.is_some()
                || node.media.is_some()
                || matches!(
                    node.kind,
                    ViewKind::Native | ViewKind::WebView | ViewKind::Video
                )
                || [
                    EventKind::HoverIn,
                    EventKind::HoverOut,
                    EventKind::ScrollEnd,
                    EventKind::MomentumEnd,
                ]
                .into_iter()
                .any(|kind| node.events.contains_listener(kind))
        })
    }

    fn opaque_native_gesture_node(&self, id: NodeId) -> bool {
        self.node(id).is_none_or(|node| {
            matches!(
                node.kind,
                ViewKind::Input | ViewKind::Native | ViewKind::WebView | ViewKind::Video
            ) || node.native.is_some()
                || node.media.is_some()
                || node
                    .text
                    .as_ref()
                    .is_some_and(|(text, _, _)| text.is_selectable() || !text.links.is_empty())
                || [
                    EventKind::Touch,
                    EventKind::HoverIn,
                    EventKind::HoverOut,
                    EventKind::PressDown,
                    EventKind::PressUp,
                    EventKind::Press,
                    EventKind::LongPress,
                    EventKind::Focus,
                    EventKind::PanStart,
                    EventKind::PanEnd,
                    EventKind::PinchStart,
                    EventKind::PinchEnd,
                    EventKind::RotationStart,
                    EventKind::RotationEnd,
                ]
                .into_iter()
                .any(|kind| node.events.contains_listener(kind))
                || (node.kind == ViewKind::Screen
                    && node.screen.gesture_enabled
                    && self.navigation_can_back(id))
        })
    }

    fn native_gesture_focus_safe(&self, path: &[NodeId]) -> bool {
        let target = path.iter().copied().find(|&id| {
            self.node(id)
                .is_some_and(|node| node.kind == ViewKind::Scroll && node.scroll.enabled)
        });
        target.is_none()
            || target == self.focus
            || !self.focus.is_some_and(|id| {
                self.node(id)
                    .is_some_and(|node| node.events.contains_listener(EventKind::Blur))
            })
    }

    fn press_route(
        &self,
        target: NodeId,
        owner: PointerTarget,
        geometry: &HitSnapshot,
    ) -> Option<PressRoute> {
        let node = self.node(target)?;
        let (matrix, size) = geometry.geometry(target)?;
        let ancestry = geometry
            .ancestors(target)
            .into_iter()
            .map(|id| {
                let node = self.node(id)?;
                (self.animated_style(id).opacity > 0.0).then_some((id, node.generation))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(PressRoute {
            target,
            incarnation: node.generation,
            owner,
            ancestry,
            matrix,
            size,
            clip: geometry.clip_rect(target)?,
        })
    }

    fn native_contact_safe(&self, target: NodeId, hit: NodeId, geometry: &HitSnapshot) -> bool {
        self.focusable(target)
            && !(self.focus != Some(target)
                && self.focus.is_some_and(|id| {
                    self.node(id)
                        .is_some_and(|node| node.events.contains(EventKind::Blur))
                }))
            && !geometry.ancestors(hit).into_iter().any(|id| {
                self.node(id).is_none_or(|node| {
                    (id != target && self.opaque_native_gesture_node(id))
                        || node.native.is_some()
                        || node.media.is_some()
                        || node.gestures.pan_x.is_some()
                        || node.gestures.pan_y.is_some()
                        || node.props.pan_activation.is_some()
                        || [EventKind::Touch, EventKind::HoverIn, EventKind::HoverOut]
                            .into_iter()
                            .any(|kind| node.events.contains_listener(kind))
                        || [
                            EventKind::PressDown,
                            EventKind::PressUp,
                            EventKind::Press,
                            EventKind::LongPress,
                            EventKind::Focus,
                            EventKind::Blur,
                            EventKind::SelectionChange,
                        ]
                        .into_iter()
                        .any(|kind| node.events.contains(kind))
                })
            })
    }

    fn selection_route(&self, target: NodeId, geometry: &HitSnapshot) -> Option<PressRoute> {
        let node = self.node(target)?;
        match node.kind {
            ViewKind::Input => {}
            ViewKind::Text
                if node
                    .text
                    .as_ref()
                    .is_some_and(|(text, _, _)| text.is_selectable() && text.links.is_empty()) => {}
            _ => return None,
        }
        if !self.native_contact_safe(target, target, geometry) {
            return None;
        }
        if node.kind == ViewKind::Text {
            let (_, layout, content) = node.text.as_ref()?;
            return self.press_route(
                target,
                PointerTarget::Text(TextContact {
                    revision: node.editor.revision(),
                    layout: layout.clone(),
                    content: *content,
                }),
                geometry,
            );
        }
        let input = self.input_hit_geometry(target)?;
        let columns = input.columns.max(1);
        let focused = self.focus == Some(target);
        let contact = InputContact {
            revision: node.editor.revision(),
            local: self.composite_ancestor(target),
            scroll: crate::render::input_scroll(node, focused, columns, input.rows.max(1)),
            line_offsets: crate::render::input_line_offsets(node, focused, columns),
            wrap: if node.input.multiline {
                columns
            } else {
                usize::MAX
            },
            secure: node.input.secure,
            geometry: input,
        };
        self.press_route(target, PointerTarget::Input(contact), geometry)
    }

    fn wheel_route(&self, target: NodeId, geometry: &HitSnapshot) -> Option<WheelRoute> {
        let node = self.node(target)?;
        if node.kind != ViewKind::Scroll
            || !node.scroll.enabled
            || !geometry.visible_ids().contains(&target)
        {
            return None;
        }
        let ancestry = geometry
            .ancestors(target)
            .into_iter()
            .map(|id| {
                let node = self.node(id)?;
                (self.animated_style(id).opacity > 0.0 && !self.opaque_wheel_node(id))
                    .then_some((id, node.generation))
            })
            .collect::<Option<Vec<_>>>()?;
        let (matrix, size) = geometry.geometry(target)?;
        Some(WheelRoute {
            target,
            ancestry,
            matrix,
            size,
            clip: geometry.clip_rect(target)?,
            policy: WheelPolicy::Scroll {
                horizontal: node.scroll.horizontal,
                mirrored: node.scroll.mirrored,
                behavior: node.scroll.behavior.clone(),
            },
        })
    }

    fn pinch_route(&self, target: NodeId, geometry: &HitSnapshot) -> Option<WheelRoute> {
        let node = self.node(target)?;
        node.gestures.pinch?;
        let binding = self.animation.gesture_binding(node.gestures)?;
        let ancestry = geometry
            .ancestors(target)
            .into_iter()
            .map(|id| {
                let node = self.node(id)?;
                (self.animated_style(id).opacity > 0.0 && !self.opaque_native_gesture_node(id))
                    .then_some((id, node.generation))
            })
            .collect::<Option<Vec<_>>>()?;
        let (matrix, size) = geometry.geometry(target)?;
        Some(WheelRoute {
            target,
            ancestry,
            matrix,
            size,
            clip: geometry.clip_rect(target)?,
            policy: WheelPolicy::Pinch(binding),
        })
    }

    /// Delivers wheel detents only to their original Scroll or native Pinch.
    ///
    /// Content growth and that owner's own offset may advance while output is
    /// pending. A changed incarnation, ancestry, viewport, scope, geometry or
    /// scroll policy rejects the lease. A newly hit nested Scroll cannot take
    /// the old input. A value-only Ctrl+wheel Pinch additionally pins its live
    /// animation value births. Application input callbacks and scroll zoom
    /// retain the embedding's exact-presentation handler.
    pub fn handle_presented_wheel(
        &mut self,
        mouse: MouseEvent,
        steps: u16,
        lease: &WheelLease,
    ) -> Option<u16> {
        let route = lease.route.as_ref()?;
        if steps == 0
            || !matches!(
                mouse.kind,
                MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight
            )
            || mouse
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
                != matches!(route.policy, WheelPolicy::Pinch(_))
            || !Arc::ptr_eq(&self.input_instance, &lease.instance)
            || (self.columns, self.rows) != lease.viewport
            || self.metrics != lease.metrics
            || (mouse.column, mouse.row) != lease.origin
            || self.input_blocked()
            || self.pointer.is_some()
        {
            return None;
        }
        let current = self.hit_snapshot();
        let offered = match &route.policy {
            WheelPolicy::Scroll { .. } => self.wheel_route(route.target, &current),
            WheelPolicy::Pinch(_) => self.pinch_route(route.target, &current),
        };
        if self.input_scope(&current) != lease.scope || offered.as_ref() != Some(route) {
            return None;
        }
        let (x, y) = (
            (f32::from(mouse.column) + 0.5) * self.metrics.width().0,
            (f32::from(mouse.row) + 0.5) * self.metrics.height().0,
        );
        let hit = current.hit_test(x, y)?;
        let path = current.ancestors(hit);
        if !path.contains(&route.target)
            || path.iter().any(|&id| match &route.policy {
                WheelPolicy::Scroll { .. } => self.opaque_wheel_node(id),
                WheelPolicy::Pinch(_) => self.opaque_native_gesture_node(id),
            })
            || (matches!(route.policy, WheelPolicy::Pinch(_))
                && path.iter().any(|&id| {
                    self.node(id).is_some_and(|node| {
                        node.kind == ViewKind::Scroll
                            && node.scroll.enabled
                            && node.scroll.behavior.pinch
                            && node.scroll.behavior.zooms()
                    })
                }))
        {
            return None;
        }
        // Both hover paths exclude callbacks; retain native feedback and the
        // next keyboard scroll target without changing this wheel's owner.
        self.set_hover(Some(hit));
        Some(match &route.policy {
            WheelPolicy::Scroll { .. } => self.scroll_wheel_owner(route.target, mouse, steps),
            WheelPolicy::Pinch(_) => self.pinch_wheel(route.target, mouse.kind, steps),
        })
    }

    fn opaque_press_node(&self, id: NodeId) -> bool {
        let Some(node) = self.node(id) else {
            return true;
        };
        matches!(
            node.kind,
            ViewKind::Input | ViewKind::Native | ViewKind::WebView | ViewKind::Video
        ) || node.native.is_some()
            || node.media.is_some()
            || node
                .text
                .as_ref()
                .is_some_and(|(text, _, _)| text.is_selectable() || !text.links.is_empty())
            || [
                EventKind::Touch,
                EventKind::HoverIn,
                EventKind::HoverOut,
                EventKind::PanStart,
                EventKind::PanEnd,
            ]
            .into_iter()
            .any(|kind| node.events.contains_listener(kind))
            || node.gestures.pan_x.is_some()
            || node.gestures.pan_y.is_some()
            || node.props.pan_activation.is_some()
            || (node.kind == ViewKind::Screen
                && node.screen.gesture_enabled
                && self.navigation_can_back(id))
    }

    /// Handles a sealed action, native input or Pan from its presented route.
    ///
    /// `None` rejects a stale or unsupported route without entering its mouse
    /// callbacks. `Some(changed)` is the original native handler's result. A
    /// successful Down captures the same route for Drag and Up. Color feedback
    /// may change; action/Pan geometry changes cancel the capture. A plain
    /// input may move while its original local contact remains unoccluded;
    /// selection uses its presented text layout and unchanged content identity.
    /// Native captures may leave the original viewport while dragging.
    pub fn handle_presented_mouse(
        &mut self,
        mouse: MouseEvent,
        lease: &PressLease,
    ) -> Option<bool> {
        if !matches!(
            mouse.kind,
            MouseEventKind::Down(MouseButton::Left)
                | MouseEventKind::Drag(MouseButton::Left)
                | MouseEventKind::Up(MouseButton::Left)
        ) {
            return None;
        }
        let current = self.input_snapshot();
        let identity_matches = Arc::ptr_eq(&self.input_instance, &lease.instance)
            && current.viewport == lease.viewport
            && current.metrics == lease.metrics
            && current.scope == lease.scope
            && !current.blocked
            && self.node(lease.target()).is_some_and(|node| {
                node.generation == lease.route.incarnation
                    && match &lease.route.owner {
                        PointerTarget::Action(binding) => node.props.input_action == Some(*binding),
                        PointerTarget::Input(contact) => {
                            node.kind == ViewKind::Input
                                && node.editor.revision() == contact.revision
                        }
                        PointerTarget::Text(contact) => {
                            node.kind == ViewKind::Text
                                && node.editor.revision() == contact.revision
                        }
                        PointerTarget::ScrollFocus => {
                            node.kind == ViewKind::Scroll && node.scroll.enabled
                        }
                        PointerTarget::Pan {
                            binding,
                            activation,
                        } => {
                            node.props.pan_activation == *activation
                                && self.animation.gesture_binding(node.gestures).as_ref()
                                    == Some(binding)
                        }
                    }
            });
        if !identity_matches {
            self.reject_presented_press(lease);
            return None;
        }
        match &lease.route.owner {
            PointerTarget::Input(_) => return self.handle_presented_input(mouse, lease, &current),
            PointerTarget::Text(_) | PointerTarget::ScrollFocus => {
                return self.handle_presented_reading(mouse, lease, &current);
            }
            PointerTarget::Action(_) | PointerTarget::Pan { .. } => {}
        }
        if matches!(mouse.kind, MouseEventKind::Down(_)) {
            let offered = current.press_at(mouse.column, mouse.row);
            if (mouse.column, mouse.row) != lease.origin
                || !offered.as_ref().is_some_and(|offered| {
                    offered.same_capture(lease) && offered.route == lease.route
                })
            {
                self.reject_presented_press(lease);
                return None;
            }
            if self.presented_press.is_some() {
                self.clear_sealed_pointer();
            }
            let changed = self.handle_event(crossterm::event::Event::Mouse(mouse));
            let owner = if lease.is_native_pan() {
                self.native_pan_owner()
            } else {
                self.sealed_pointer_owner()
            };
            if owner != Some(lease.target()) {
                self.clear_sealed_pointer();
                return None;
            }
            self.presented_press = Some(lease.clone());
            return Some(changed);
        }
        if !self
            .presented_press
            .as_ref()
            .is_some_and(|captured| captured.same_capture(lease))
            || (if lease.is_native_pan() {
                self.native_pan_owner()
            } else {
                self.sealed_pointer_owner()
            }) != Some(lease.target())
            || current.routes.get(&lease.target()) != Some(&lease.route)
        {
            self.reject_presented_press(lease);
            return None;
        }
        // Generic native mouse dispatch may enter a newly hit component or
        // hover responder before it reaches capture handling. Both Drag and
        // Up must still resolve to this same ordinary route at their point.
        let (column, row) = if lease.is_native_pan() {
            lease.origin
        } else {
            (mouse.column, mouse.row)
        };
        if !current.press_at(column, row).is_some_and(|offered| {
            offered.route.target == lease.target()
                && offered.route.incarnation == lease.route.incarnation
                && offered.route.owner == lease.route.owner
        }) {
            self.reject_presented_press(lease);
            return None;
        }
        if lease.is_native_pan() {
            if matches!(mouse.kind, MouseEventKind::Drag(_)) {
                let x = (f32::from(mouse.column) + 0.5) * self.metrics.width().0;
                let y = (f32::from(mouse.row) + 0.5) * self.metrics.height().0;
                // This original native capture may leave the viewport. Do not
                // route its continuation through a newly hit hosted component.
                self.drag_pointer(x, y, false);
            } else {
                self.clear_sealed_pointer();
            }
            return Some(true);
        }
        let changed = self.handle_event(crossterm::event::Event::Mouse(mouse));
        if matches!(mouse.kind, MouseEventKind::Up(_)) {
            self.presented_press = None;
        }
        Some(changed)
    }

    fn handle_presented_reading(
        &mut self,
        mouse: MouseEvent,
        lease: &PressLease,
        current: &InputSnapshot,
    ) -> Option<bool> {
        let down = matches!(mouse.kind, MouseEventKind::Down(_));
        if current.routes.get(&lease.target()) != Some(&lease.route)
            || !current
                .press_at(lease.origin.0, lease.origin.1)
                .is_some_and(|offered| offered.same_capture(lease))
            || (down && (mouse.column, mouse.row) != lease.origin)
            || (!down
                && !self
                    .presented_press
                    .as_ref()
                    .is_some_and(|captured| captured.same_capture(lease)))
            || (matches!(mouse.kind, MouseEventKind::Drag(_)) && !lease.allows_drag())
        {
            self.reject_presented_press(lease);
            return None;
        }
        if matches!(mouse.kind, MouseEventKind::Up(_)) {
            self.clear_sealed_pointer();
            return Some(true);
        }
        if down {
            self.clear_sealed_pointer();
            self.set_hover(Some(lease.target()));
            self.set_focus_from(Some(lease.target()), FocusSource::Pointer);
            self.presented_press = Some(lease.clone());
        }
        if matches!(&lease.route.owner, PointerTarget::Text(_)) {
            self.drag_input = Some(lease.target());
            // This is the original immutable text layout, not a new hit or
            // another component encountered while selection leaves its bounds.
            self.place_caret(
                lease.target(),
                (f32::from(mouse.column) + 0.5) * self.metrics.width().0,
                (f32::from(mouse.row) + 0.5) * self.metrics.height().0,
                !down,
            );
        }
        Some(true)
    }

    fn handle_presented_input(
        &mut self,
        mouse: MouseEvent,
        lease: &PressLease,
        current: &InputSnapshot,
    ) -> Option<bool> {
        let PointerTarget::Input(contact) = &lease.route.owner else {
            return None;
        };
        let valid = current.routes.get(&lease.target()).is_some_and(|route| {
            matches!(&route.owner, PointerTarget::Input(now) if now.revision == contact.revision)
                && route.ancestry == lease.route.ancestry
                && {
                    // The same editor may move while an unrelated frame is in
                    // flight. Test its original local contact against its live
                    // clipping/occlusion; never hit a new owner at old screen
                    // coordinates or reinterpret that owner's caret.
                    let x = (f32::from(lease.origin.0) + 0.5) * self.metrics.width().0;
                    let y = (f32::from(lease.origin.1) + 0.5) * self.metrics.height().0;
                    unproject(&lease.route.matrix, x, y)
                        .and_then(|(x, y)| project(&route.matrix, x, y))
                        .and_then(|(x, y)| current.geometry.hit_test(x, y))
                        .is_some_and(|hit| hit == lease.target())
                }
        });
        let down = matches!(mouse.kind, MouseEventKind::Down(_));
        if !valid
            || (down && (mouse.column, mouse.row) != lease.origin)
            || (!down
                && (self.drag_input != Some(lease.target())
                    || !self
                        .presented_press
                        .as_ref()
                        .is_some_and(|captured| captured.same_capture(lease))))
        {
            self.reject_presented_press(lease);
            return None;
        }
        if matches!(mouse.kind, MouseEventKind::Up(_)) {
            self.clear_sealed_pointer();
            return Some(true);
        }
        let (contact, matrix) = if down {
            (contact, &lease.route.matrix)
        } else {
            let route = current.routes.get(&lease.target())?;
            let PointerTarget::Input(contact) = &route.owner else {
                return None;
            };
            // Down names the witnessed caret. A captured drag then follows
            // the same native editor's scrolling selection, just as ordinary
            // input does, without ever authorizing a different hit owner.
            (contact, &route.matrix)
        };
        let x = (f32::from(mouse.column) + 0.5) * self.metrics.width().0;
        let y = (f32::from(mouse.row) + 0.5) * self.metrics.height().0;
        let (x, y) = if contact.local {
            unproject(matrix, x, y)?
        } else {
            (x, y)
        };
        let row = ((y / self.metrics.height().0).floor() as i64 - contact.geometry.row).max(0)
            as usize
            + contact.scroll.0;
        let column = ((x / self.metrics.width().0).floor() as i64 - contact.geometry.column).max(0)
            as usize
            + contact.scroll.1;
        let column = column.saturating_sub(contact.line_offsets.get(row).copied().unwrap_or(0));
        let offset =
            self.node(lease.target())?
                .editor
                .offset_at(row, column, contact.wrap, contact.secure);
        if down {
            self.clear_sealed_pointer();
            self.set_hover(Some(lease.target()));
            let watches = current
                .geometry
                .ancestors(lease.target())
                .into_iter()
                .filter_map(|id| {
                    self.node(id).and_then(|node| {
                        node.events
                            .contains(EventKind::Touch)
                            .then(|| node.events.hint_watch())
                            .flatten()
                    })
                })
                .collect::<Vec<_>>();
            if !watches.is_empty() {
                self.events.push(crate::SurfaceEvent::HintObservation(
                    HintObservation::Press(watches),
                ));
            }
            self.set_focus_from(Some(lease.target()), FocusSource::Pointer);
            self.drag_input = Some(lease.target());
            self.presented_press = Some(lease.clone());
        }
        self.edit_input(lease.target(), |editor, _| {
            editor.move_to(offset, !down);
            false
        });
        Some(true)
    }

    pub(crate) fn presented_animation_changed(&mut self, op: &nsh_core::animation::AnimationOp) {
        let affected = self.presented_press.as_ref().is_some_and(|lease| {
            if let PointerTarget::Pan { binding, .. } = &lease.route.owner {
                self.animation.affects_gesture(op, binding)
            } else {
                false
            }
        });
        if affected {
            self.clear_sealed_pointer();
        }
    }

    fn reject_presented_press(&mut self, lease: &PressLease) {
        if self
            .presented_press
            .as_ref()
            .is_some_and(|captured| captured.same_capture(lease))
        {
            // The stored id may already denote a new binding or incarnation.
            // Clear local capture and its tracked state without dispatching
            // an old PressUp or another cancellation into the replacement
            // owner. The engine separately validates each sealed phase.
            self.clear_sealed_pointer();
        }
    }
}

#[cfg(test)]
mod tests;
