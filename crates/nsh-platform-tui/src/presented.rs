//! Leases for native input owners in a completed terminal presentation.
//!
//! A host publishes a snapshot only after its bytes have flushed. It may keep
//! that snapshot while unrelated work advances the scene: a lease is checked
//! against the current native route before entering its original capture.

use std::{collections::HashMap, sync::Arc};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use nsh_core::{Bounds, EventKind, Matrix, NodeId, Points, ScrollBehavior, Size, ViewKind};

use crate::{CellMetrics, Surface, geometry::HitSnapshot};

/// An immutable sample of native hit shapes, sealed presses and scroll owners.
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
    wheel_owners: HashMap<NodeId, NodeId>,
    wheel_routes: HashMap<NodeId, WheelRoute>,
}

#[derive(Clone, Debug, PartialEq)]
struct PressRoute {
    target: NodeId,
    incarnation: u64,
    binding: u64,
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
    horizontal: bool,
    mirrored: bool,
    behavior: Box<ScrollBehavior>,
}

/// The original native Scroll owner at one cell of a completed presentation.
///
/// This retains a native view incarnation, not an application callback. Its
/// queued onScroll notification still reports that view's current offset.
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

/// The original sealed press route at one cell of a completed presentation.
///
/// Keep this same lease for its left-button Down, Drag and Up. The native
/// surface checks its identity and the host must additionally validate
/// [`Self::binding`] in the engine before dispatching its resulting callback.
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
    /// The JS event owner's sealed action binding, to validate before app code.
    pub fn binding(&self) -> u64 {
        self.route.binding
    }

    /// The native activation owner, rather than a text child that was hit.
    pub fn target(&self) -> NodeId {
        self.route.target
    }

    fn same_capture(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.instance, &other.instance)
            && self.route.target == other.route.target
            && self.route.incarnation == other.route.incarnation
            && self.route.binding == other.route.binding
            && self.viewport == other.viewport
            && self.metrics == other.metrics
            && self.scope == other.scope
            && self.origin == other.origin
    }
}

impl InputSnapshot {
    /// Resolves the native Scroll ancestor of the presented cell's actual hit.
    ///
    /// Native components, media, captures and immediate application callbacks
    /// keep the embedding's exact-presentation path. Selectable text is ordinary
    /// scroll content. This does not use the focused or first available scroll.
    pub fn wheel_at(&self, column: u16, row: u16) -> Option<WheelLease> {
        if column >= self.viewport.0 || row >= self.viewport.1 {
            return None;
        }
        let (x, y) = (
            (f32::from(column) + 0.5) * self.metrics.width().0,
            (f32::from(row) + 0.5) * self.metrics.height().0,
        );
        let hit = self.geometry.hit_test(x, y)?;
        let route = self.wheel_routes.get(self.wheel_owners.get(&hit)?)?.clone();
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
    /// Opaque routes, such as native components, selections, gestures, system
    /// overlays and focus callbacks, intentionally have no press lease.
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
    /// Samples immutable hit geometry, sealed presses and native scroll owners.
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
                if self.pointer.is_none()
                    && !path.iter().any(|&id| self.opaque_wheel_node(id))
                    && let Some(target) = path.iter().copied().find(|&id| {
                        self.node(id).is_some_and(|node| {
                            node.kind == ViewKind::Scroll && node.scroll.enabled
                        })
                    })
                {
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        wheel_routes.entry(target)
                        && let Some(route) = self.wheel_route(target, &geometry)
                    {
                        entry.insert(route);
                    }
                    if wheel_routes.contains_key(&target) {
                        wheel_owners.insert(hit, target);
                    }
                }
                if path.iter().any(|&id| self.opaque_press_node(id)) {
                    continue;
                }
                let Some(target) = path.iter().copied().find(|&id| self.pressable(id)) else {
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
                let Some((matrix, size)) = geometry.geometry(target) else {
                    continue;
                };
                let Some(clip) = geometry.clip_rect(target) else {
                    continue;
                };
                owners.insert(hit, target);
                routes.entry(target).or_insert(PressRoute {
                    target,
                    incarnation: node.generation,
                    binding,
                    matrix,
                    size,
                    clip,
                });
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
                    node.events.contains(EventKind::HoverIn)
                        || node.events.contains(EventKind::HoverOut)
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
                .any(|kind| node.events.contains(kind))
        })
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
            horizontal: node.scroll.horizontal,
            mirrored: node.scroll.mirrored,
            behavior: node.scroll.behavior.clone(),
        })
    }

    /// Delivers ordinary wheel detents only to their original native Scroll.
    ///
    /// Content growth and that owner's own offset may advance while output is
    /// pending. A changed incarnation, ancestry, viewport, scope, geometry or
    /// scroll policy rejects the lease. A newly hit nested Scroll cannot take
    /// the old input. Ctrl+wheel zoom and application callback routes keep the
    /// embedding's exact-presentation handler.
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
        if self.input_scope(&current) != lease.scope
            || self.wheel_route(route.target, &current).as_ref() != Some(route)
        {
            return None;
        }
        let (x, y) = (
            (f32::from(mouse.column) + 0.5) * self.metrics.width().0,
            (f32::from(mouse.row) + 0.5) * self.metrics.height().0,
        );
        let hit = current.hit_test(x, y)?;
        let path = current.ancestors(hit);
        if !path.contains(&route.target) || path.iter().any(|&id| self.opaque_wheel_node(id)) {
            return None;
        }
        // Both hover paths exclude callbacks; retain native feedback and the
        // next keyboard scroll target without changing this wheel's owner.
        self.set_hover(Some(hit));
        Some(self.scroll_wheel_owner(route.target, mouse, steps))
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
            .any(|kind| node.events.contains(kind))
            || node.gestures.pan_x.is_some()
            || node.gestures.pan_y.is_some()
            || node.props.pan_activation.is_some()
            || (node.kind == ViewKind::Screen
                && node.screen.gesture_enabled
                && self.navigation_can_back(id))
    }

    /// Handles an ordinary left press only while its presented route still fits.
    ///
    /// `None` rejects a stale or unsupported route without entering its mouse
    /// callbacks. `Some(changed)` is the original native handler's result. A
    /// successful Down captures the same route for Drag and Up. Color feedback
    /// may change; geometry changes conservatively cancel this leased gesture.
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
                    && node.props.input_action == Some(lease.binding())
            });
        if !identity_matches {
            self.reject_presented_press(lease);
            return None;
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
            if self.sealed_pointer_owner() != Some(lease.target()) {
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
            || self.sealed_pointer_owner() != Some(lease.target())
            || current.routes.get(&lease.target()) != Some(&lease.route)
        {
            self.reject_presented_press(lease);
            return None;
        }
        // Generic native mouse dispatch may enter a newly hit component or
        // hover responder before it reaches capture handling. Both Drag and
        // Up must still resolve to this same ordinary route at their point.
        if !current
            .press_at(mouse.column, mouse.row)
            .is_some_and(|offered| {
                offered.route.target == lease.target()
                    && offered.route.incarnation == lease.route.incarnation
                    && offered.route.binding == lease.binding()
            })
        {
            self.reject_presented_press(lease);
            return None;
        }
        let changed = self.handle_event(crossterm::event::Event::Mouse(mouse));
        if matches!(mouse.kind, MouseEventKind::Up(_)) {
            self.presented_press = None;
        }
        Some(changed)
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
