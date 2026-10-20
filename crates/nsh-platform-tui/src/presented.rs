//! Leases for ordinary press actions in a completed terminal presentation.
//!
//! A host publishes a snapshot only after its bytes have flushed. It may keep
//! that snapshot while unrelated work advances the scene: a lease is checked
//! against the current native route before entering its original capture.

use std::{collections::HashMap, sync::Arc};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use nsh_core::{Bounds, EventKind, Matrix, NodeId, Points, Size, ViewKind};

use crate::{CellMetrics, Surface, geometry::HitSnapshot};

/// An immutable sample of the native hit shapes and sealed press routes.
///
/// This contains no engine, component or scene owner. Publish it alongside the
/// terminal bytes it describes, after their successful flush, and use
/// [`Self::press_at`] on the input thread. An absent lease keeps the host's
/// existing exact-presentation input path; it does not make that input invalid.
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
    /// Samples immutable hit geometry and ordinary sealed press bindings.
    ///
    /// Taking a snapshot does not publish it or grant input authority. The
    /// embedding host publishes it only after flushing its corresponding frame.
    pub fn input_snapshot(&self) -> InputSnapshot {
        let geometry = self.hit_snapshot();
        let scope = geometry.visible_ids().iter().copied().find_map(|id| {
            self.node(id)
                .filter(|node| node.kind == ViewKind::Modal)
                .map(|node| (id, node.generation))
        });
        let blocked = self.dialog.is_some()
            || self.service_overlay_open()
            || (self.pointer.is_some() && self.presented_press.is_none())
            || self.hovered_path.iter().any(|&id| {
                self.node(id).is_some_and(|node| {
                    node.events.contains(EventKind::HoverIn)
                        || node.events.contains(EventKind::HoverOut)
                })
            });
        let mut owners = HashMap::new();
        let mut routes = HashMap::new();
        if !blocked {
            for &hit in geometry.visible_ids() {
                let path = geometry.ancestors(hit);
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
        }
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
