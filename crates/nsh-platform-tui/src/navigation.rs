//! Screen and modal presentation without a JavaScript round trip.
//!
//! A departing root keeps its retained subtree until the transition ends.
//! Layout targets stay unchanged: presentation shifts and opacity are sampled
//! by both drawing and input. Modal contents are laid out at the card's size.

use std::sync::Arc;

use nsh_core::{
    EdgeInsets, EventKind, EventPayload, ModalAnimation, ModalConfig, ModalPresentation,
    MountBatch, MountItem, NodeId, PayloadValue, Rect, ScreenAnimation, Size, ViewKind,
};

use crate::surface::{Surface, SurfaceEvent};

const SLIDE_SECONDS: f64 = 0.3;
const FADE_SECONDS: f64 = 0.2;
const PARALLAX: f32 = 0.3;
const DIM: f32 = 0.12;

/// Per-surface navigation records. Dense node slots hold only running tracks.
#[derive(Default)]
pub(crate) struct Navigation {
    tracks: Vec<Option<Track>>,
    stacks: Vec<(NodeId, NodeId)>,
    modals: Vec<NodeId>,
    reports: Vec<Option<(Size, EdgeInsets)>>,
    headers: Vec<Option<Option<f32>>>,
    moving: Vec<NodeId>,
    presentations: Vec<NodeId>,
    gesture: Option<Gesture>,
}

impl Navigation {
    fn track(&self, id: NodeId) -> Option<&Track> {
        self.tracks.get(id as usize)?.as_ref()
    }

    fn track_mut(&mut self, id: NodeId) -> Option<&mut Track> {
        self.tracks.get_mut(id as usize)?.as_mut()
    }

    fn set_track(&mut self, id: NodeId, track: Option<Track>) {
        if track.is_none() && self.tracks.len() <= id as usize {
            return;
        }
        self.tracks
            .resize_with(self.tracks.len().max(id as usize + 1), || None);
        self.tracks[id as usize] = track;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Movement {
    Horizontal,
    Vertical,
    Fade,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Completion {
    Enter,
    Restore,
    Detach,
    Pop,
}

impl Completion {
    fn exiting(self) -> bool {
        matches!(self, Self::Detach | Self::Pop)
    }
}

struct Track {
    movement: Movement,
    position: Position,
    below: Option<NodeId>,
    completion: Completion,
    modal: bool,
}

impl Track {
    fn fraction(&self, now: f64) -> f32 {
        self.position.value(now).clamp(0.0, 1.0)
    }
}

enum Position {
    Timed {
        from: f32,
        to: f32,
        started: f64,
        duration: f64,
    },
    Interactive(f32),
}

impl Position {
    fn value(&self, now: f64) -> f32 {
        match *self {
            Self::Interactive(value) => value,
            Self::Timed {
                from,
                to,
                started,
                duration,
            } => {
                let progress = if duration <= 0.0 {
                    1.0
                } else {
                    ((now - started) / duration).clamp(0.0, 1.0)
                };
                let remaining = 1.0 - progress;
                let eased = (1.0 - remaining * remaining * remaining) as f32;
                from + (to - from) * eased
            }
        }
    }

    fn finished(&self, now: f64) -> bool {
        match *self {
            Self::Interactive(_) => false,
            Self::Timed {
                started, duration, ..
            } => now - started >= duration,
        }
    }
}

struct Gesture {
    id: NodeId,
    below: Option<NodeId>,
    origin: (f32, f32),
    last: (f32, f32),
    started: f64,
    last_time: f64,
    velocity: f32,
    extent: f32,
    modal: bool,
    claimed: bool,
}

/// Positions of the compact button row and optional collapsing title row.
pub(crate) struct HeaderLayout {
    pub(crate) height: f32,
    pub(crate) bar_height: f32,
    pub(crate) title_y: f32,
    pub(crate) large: bool,
}

impl Surface {
    /// Protects complete outgoing trees before descendants can be released
    /// earlier in the batch than their screen or modal.
    pub(crate) fn navigation_prepare(&mut self, batch: &MountBatch) {
        self.navigation.presentations.clear();
        self.navigation.moving = batch
            .items
            .iter()
            .filter_map(|item| match item {
                MountItem::Insert { child, .. } => Some(*child),
                _ => None,
            })
            .collect();
        let mut roots = Vec::new();
        for item in &batch.items {
            let id = match item {
                MountItem::Remove { id } | MountItem::Delete { id } => *id,
                _ => continue,
            };
            if self
                .node(id)
                .is_some_and(|node| matches!(node.kind, ViewKind::Screen | ViewKind::Modal))
            {
                roots.push(id);
            }
        }
        // Outer roots retain their children rather than running each nested
        // navigation controller's exit independently.
        roots.sort_by_key(|&id| self.navigation_depth(id));
        roots.dedup();
        for id in roots {
            self.navigation_removing(id);
        }
    }

    fn navigation_depth(&self, id: NodeId) -> usize {
        let mut depth = 0;
        let mut current = self.node(id).and_then(|node| node.parent);
        while let Some(id) = current {
            depth += 1;
            current = self.node(id).and_then(|node| node.parent);
        }
        depth
    }

    fn navigation_attached(&self, id: NodeId) -> bool {
        let mut current = Some(id);
        while let Some(id) = current {
            if id == nsh_core::ROOT_ID {
                return true;
            }
            let Some(node) = self.node(id) else {
                return false;
            };
            if node.popped || node.style.hidden {
                return false;
            }
            current = node.parent;
        }
        false
    }

    /// Cancels an app removal when it moves or reinserts the same root.
    pub(crate) fn navigation_reinserted(&mut self, id: NodeId) {
        let exiting = self
            .navigation
            .track(id)
            .is_some_and(|track| track.completion.exiting());
        let presenting = self.node(id).is_some_and(|node| {
            node.kind == ViewKind::Modal && (exiting || node.popped || node.parent.is_none())
        });
        if presenting {
            self.navigation.presentations.retain(|&modal| modal != id);
            self.navigation.presentations.push(id);
            self.navigation.modals.retain(|&modal| modal != id);
        }
        if exiting {
            self.navigation.set_track(id, None);
        }
        if let Some(node) = self.node_mut(id) {
            node.popped = false;
            node.delete_pending = false;
        }
    }

    /// Whether this node must stay mounted for its own or an ancestor's exit.
    pub(crate) fn navigation_retains(&self, id: NodeId) -> bool {
        let mut current = Some(id);
        while let Some(id) = current {
            if self
                .navigation
                .track(id)
                .is_some_and(|track| track.completion.exiting())
            {
                return true;
            }
            current = self.node(id).and_then(|node| node.parent);
        }
        false
    }

    /// Starts an exit if this is a visible navigation root, otherwise inherits
    /// an ancestor's exit. The caller defers both Remove and Delete while true.
    pub(crate) fn navigation_removing(&mut self, id: NodeId) -> bool {
        if self.navigation.moving.contains(&id) {
            return false;
        }
        if self.navigation_retains(id) {
            if let Some(track) = self.navigation.track_mut(id)
                && track.completion == Completion::Pop
            {
                track.completion = Completion::Detach;
            }
            return true;
        }
        let Some(node) = self.node(id) else {
            return false;
        };
        if node.parent.is_none() || node.popped {
            return false;
        }
        let modal = node.kind == ViewKind::Modal;
        if !modal && node.kind != ViewKind::Screen {
            return false;
        }
        let below = if modal {
            None
        } else {
            let Some(parent) = node.parent else {
                return false;
            };
            if self.navigation_top(parent) != Some(id) {
                return false;
            }
            self.navigation_previous(id)
        };
        let motion = if modal {
            modal_motion(node.modal.animation, self.reduce_motion)
        } else {
            screen_motion(node.screen.animation, self.reduce_motion)
        };
        let from = self
            .navigation
            .track(id)
            .map_or(0.0, |track| track.fraction(self.now));
        let Some((movement, duration)) = motion else {
            self.navigation.set_track(id, None);
            self.navigation_complete_subtree(id);
            if modal && self.navigation.modals.contains(&id) {
                self.navigation.modals.retain(|&modal| modal != id);
                self.emit(id, EventKind::Dismiss, EventPayload::None);
            }
            return false;
        };
        self.navigation.set_track(
            id,
            Some(Track {
                movement,
                position: Position::Timed {
                    from,
                    to: 1.0,
                    started: self.now,
                    duration: duration * (1.0 - from) as f64,
                },
                below,
                completion: Completion::Detach,
                modal,
            }),
        );
        true
    }

    /// Applies the final state of a commit, after configs, frames and children
    /// have arrived. First screens appear directly; subsequent pushes animate.
    pub(crate) fn navigation_reconcile(&mut self) {
        self.navigation.moving.clear();
        let stacks: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(id, node)| {
                let node = node.as_ref()?;
                (node.kind == ViewKind::Stack && self.navigation_attached(id as NodeId))
                    .then_some(id as NodeId)
            })
            .collect();
        let mut next_stacks = Vec::new();
        for stack in stacks {
            let Some(top) = self.navigation_top(stack) else {
                continue;
            };
            let old = self
                .navigation
                .stacks
                .iter()
                .find(|(id, _)| *id == stack)
                .map(|(_, top)| *top);
            if let Some(old) = old.filter(|&old| old != top)
                && !self.navigation_retains(old)
                && self.node(old).is_some_and(|node| !node.popped)
                && self.navigation.track(top).is_none()
                && let Some((movement, duration)) = self
                    .node(top)
                    .and_then(|node| screen_motion(node.screen.animation, self.reduce_motion))
            {
                self.navigation.set_track(
                    top,
                    Some(Track {
                        movement,
                        position: Position::Timed {
                            from: 1.0,
                            to: 0.0,
                            started: self.now,
                            duration,
                        },
                        below: self
                            .node(old)
                            .filter(|node| node.parent == Some(stack))
                            .map(|_| old),
                        completion: Completion::Enter,
                        modal: false,
                    }),
                );
            }
            next_stacks.push((stack, top));
        }
        self.navigation.stacks = next_stacks;

        let mut modals: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(id, node)| {
                let node = node.as_ref()?;
                (node.kind == ViewKind::Modal
                    && self.navigation_attached(id as NodeId)
                    && !node.popped
                    && !self.navigation_retains(id as NodeId))
                .then_some(id as NodeId)
            })
            .collect();
        modals.sort_by_key(|id| {
            self.navigation
                .presentations
                .iter()
                .position(|modal| modal == id)
                .unwrap_or(usize::MAX)
        });
        for &id in &modals {
            if self.navigation.modals.contains(&id) {
                continue;
            }
            self.navigation.modals.push(id);
            let config = self.node(id).expect("modal collected above").modal;
            if let Some((movement, duration)) = modal_motion(config.animation, self.reduce_motion) {
                self.navigation.set_track(
                    id,
                    Some(Track {
                        movement,
                        position: Position::Timed {
                            from: 1.0,
                            to: 0.0,
                            started: self.now,
                            duration,
                        },
                        below: None,
                        completion: Completion::Enter,
                        modal: true,
                    }),
                );
            } else {
                self.emit(id, EventKind::Show, EventPayload::None);
            }
        }
        let mut dismissed = Vec::new();
        let retained_modals: Vec<_> = self
            .navigation
            .modals
            .iter()
            .copied()
            .filter(|&id| self.navigation_retains(id))
            .collect();
        self.navigation.modals.retain(|id| {
            let alive = modals.contains(id) || retained_modals.contains(id);
            if !alive {
                dismissed.push(*id);
            }
            alive
        });
        for id in dismissed {
            self.navigation.set_track(id, None);
            self.emit(id, EventKind::Dismiss, EventPayload::None);
        }
        self.navigation_report_geometry();
    }

    fn navigation_report_geometry(&mut self) {
        let roots: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(id, node)| {
                let node = node.as_ref()?;
                (self.navigation_attached(id as NodeId)
                    && matches!(node.kind, ViewKind::Screen | ViewKind::Modal)
                    && !node.popped)
                    .then_some(id as NodeId)
            })
            .collect();
        for id in roots {
            let node = self.node(id).expect("navigation root collected above");
            let (size, insets, header) = if node.kind == ViewKind::Modal {
                let frame = self.modal_frame(id, node.frame);
                (
                    Size::new(frame.width, frame.height),
                    EdgeInsets::default(),
                    None,
                )
            } else {
                let size = node
                    .parent
                    .and_then(|id| self.node(id))
                    .map_or(Size::new(node.frame.width, node.frame.height), |parent| {
                        Size::new(parent.frame.width, parent.frame.height)
                    });
                let config = &node.screen;
                let height = if config.header_shown {
                    self.metrics.height() * if config.large_title { 2.0 } else { 1.0 }
                } else {
                    0.0
                };
                let under =
                    config.header_shown && (config.large_title || config.header_transparent);
                let insets = EdgeInsets {
                    top: if under { height } else { 0.0 },
                    ..EdgeInsets::default()
                };
                let header = Some((config.header_shown && !under).then_some(height));
                (size, insets, header)
            };
            if let Some(header) = header {
                self.navigation
                    .headers
                    .resize(self.navigation.headers.len().max(id as usize + 1), None);
                if self.navigation.headers[id as usize] != Some(header) {
                    self.navigation.headers[id as usize] = Some(header);
                    self.events.push(SurfaceEvent::ScreenHeader(id, header));
                }
            }
            if size.width <= 0.0 || size.height <= 0.0 {
                continue;
            }
            self.navigation
                .reports
                .resize(self.navigation.reports.len().max(id as usize + 1), None);
            let old = self.navigation.reports[id as usize];
            if old == Some((size, insets)) {
                continue;
            }
            self.navigation.reports[id as usize] = Some((size, insets));
            self.events.push(SurfaceEvent::RootSize(id, size, insets));
            if old.is_none_or(|(_, old)| old != insets) {
                let value = |value: f32| PayloadValue::Number(value as f64);
                self.emit(
                    id,
                    EventKind::Insets,
                    EventPayload::Map(vec![
                        ("top", value(insets.top)),
                        ("right", value(insets.right)),
                        ("bottom", value(insets.bottom)),
                        ("left", value(insets.left)),
                    ]),
                );
            }
        }
    }

    /// Advances the presentation clock and reports lifecycle completions once.
    pub(crate) fn navigation_tick(&mut self) -> bool {
        let mut due = Vec::new();
        let mut active = false;
        for (id, track) in self.navigation.tracks.iter().enumerate() {
            let Some(track) = track else { continue };
            active = true;
            if track.position.finished(self.now) {
                due.push(id as NodeId);
            }
        }
        due.sort_by_key(|&id| self.navigation_depth(id));
        for id in due {
            let Some(track) = self.navigation.tracks[id as usize].take() else {
                continue;
            };
            match track.completion {
                Completion::Enter if track.modal => {
                    if self.navigation_attached(id) && !self.navigation_retains(id) {
                        self.emit(id, EventKind::Show, EventPayload::None);
                    }
                }
                Completion::Enter | Completion::Restore => {}
                Completion::Detach => {
                    if track.modal {
                        self.navigation.modals.retain(|&modal| modal != id);
                        self.emit(id, EventKind::Dismiss, EventPayload::None);
                    }
                    self.navigation_complete_subtree(id);
                    self.finish_retained_removal(id);
                }
                Completion::Pop => {
                    if track.modal {
                        self.navigation.modals.retain(|&modal| modal != id);
                    }
                    self.navigation_complete_subtree(id);
                    self.emit(id, EventKind::Dismiss, EventPayload::None);
                }
            }
        }
        if active {
            self.reconcile_focus();
        }
        active
    }

    fn navigation_complete_subtree(&mut self, root: NodeId) {
        let mut descendants = self
            .node(root)
            .map_or_else(Vec::new, |node| node.children.clone());
        let mut index = 0;
        while index < descendants.len() {
            if let Some(node) = self.node(descendants[index]) {
                descendants.extend(node.children.iter().copied());
            }
            index += 1;
        }
        for &id in &descendants {
            let native_pop = self
                .navigation
                .track(id)
                .is_some_and(|track| track.completion == Completion::Pop);
            let modal = self.navigation.modals.contains(&id);
            self.navigation.set_track(id, None);
            if modal {
                self.navigation.modals.retain(|&modal| modal != id);
            }
            if modal || native_pop {
                self.emit(id, EventKind::Dismiss, EventPayload::None);
            }
            if let Some(report) = self.navigation.reports.get_mut(id as usize) {
                *report = None;
            }
            if let Some(header) = self.navigation.headers.get_mut(id as usize) {
                *header = None;
            }
        }
        self.navigation
            .stacks
            .retain(|(id, _)| !descendants.contains(id));
        if self
            .navigation
            .gesture
            .as_ref()
            .is_some_and(|gesture| gesture.id == root || descendants.contains(&gesture.id))
        {
            self.navigation.gesture = None;
        }
    }

    /// Whether the terminal needs another presentation frame.
    pub(crate) fn navigation_animating(&self) -> bool {
        self.navigation.tracks.iter().any(Option::is_some)
    }

    /// Orders pending modal layers by when they were presented. Apply to
    /// the unpainted suffix again after discovering a nested modal: its
    /// presentation may precede another pending sibling's. Retained exits
    /// keep their original position until the departure finishes.
    pub(crate) fn navigation_order_modals(&self, modals: &mut [NodeId]) {
        modals.sort_by_key(|id| {
            self.navigation
                .modals
                .iter()
                .position(|modal| modal == id)
                .unwrap_or(usize::MAX)
        });
    }

    /// The local presentation shift and opacity on a root or underlying screen.
    pub(crate) fn navigation_values(&self, id: NodeId) -> (f32, f32, f32) {
        let mut result = (0.0, 0.0, 1.0);
        if let Some(track) = self.navigation.track(id) {
            let position = track.fraction(self.now);
            match track.movement {
                Movement::Horizontal => {
                    result.0 += self.node(id).map_or(0.0, |node| node.frame.width) * position
                }
                Movement::Vertical => {
                    let frame = self
                        .node(id)
                        .map_or(Rect::default(), |node| self.modal_frame(id, node.frame));
                    result.1 += (self.viewport().height - frame.y).max(0.0) * position;
                }
                Movement::Fade => result.2 *= 1.0 - position,
            }
        }
        // Only screens can be another track's underlying route. Ordinary
        // nodes are sampled repeatedly while walking their ancestors; a
        // modal with a high id must not make each sample scan its dense slots.
        if !self
            .node(id)
            .is_some_and(|node| node.kind == ViewKind::Screen)
        {
            return result;
        }
        for track in self
            .navigation
            .tracks
            .iter()
            .flatten()
            .filter(|track| track.below == Some(id))
        {
            if track.movement == Movement::Horizontal {
                let covered = 1.0 - track.fraction(self.now);
                result.0 -= self.node(id).map_or(0.0, |node| node.frame.width) * PARALLAX * covered;
                result.2 *= 1.0 - DIM * covered;
            }
        }
        result
    }

    /// A sheet's backdrop fades independently from its sliding content.
    pub(crate) fn navigation_scrim_opacity(&self, id: NodeId) -> f32 {
        self.navigation
            .track(id)
            .map_or(1.0, |track| 1.0 - track.fraction(self.now))
    }

    /// The top screen and a transition's departing/underlying screen paint.
    pub(crate) fn navigation_draws_screen(&self, stack: NodeId, id: NodeId) -> bool {
        self.navigation_top(stack) == Some(id)
            || self.navigation.track(id).is_some()
            || self
                .navigation
                .tracks
                .iter()
                .flatten()
                .any(|track| track.below == Some(id))
    }

    /// Departing trees retain their pixels, but leave the active input scope.
    pub(crate) fn navigation_accepts(&self, id: NodeId) -> bool {
        if self.navigation_retains(id) {
            return false;
        }
        let mut current = Some(id);
        while let Some(id) = current {
            let Some(node) = self.node(id) else {
                return false;
            };
            if node.popped {
                return false;
            }
            if node.kind == ViewKind::Screen
                && let Some(stack) = node.parent
                && self
                    .node(stack)
                    .is_some_and(|node| node.kind == ViewKind::Stack)
                && self.navigation_top(stack) != Some(id)
            {
                return false;
            }
            current = node.parent;
        }
        true
    }

    fn navigation_top(&self, stack: NodeId) -> Option<NodeId> {
        self.node(stack)?
            .children
            .iter()
            .rev()
            .copied()
            .find(|&id| {
                self.node(id)
                    .is_some_and(|node| node.kind == ViewKind::Screen && !node.popped)
                    && !self.navigation_retains(id)
            })
    }

    fn navigation_previous(&self, screen: NodeId) -> Option<NodeId> {
        let stack = self.node(screen)?.parent?;
        let mut previous = None;
        for &id in &self.node(stack)?.children {
            if id == screen {
                return previous;
            }
            if self
                .node(id)
                .is_some_and(|node| node.kind == ViewKind::Screen && !node.popped)
                && !self.navigation_retains(id)
            {
                previous = Some(id);
            }
        }
        None
    }

    /// Whether a screen has an earlier route for its native back button.
    pub(crate) fn navigation_can_back(&self, screen: NodeId) -> bool {
        self.navigation_previous(screen).is_some()
    }

    /// The previous screen supplies the label of the button leading back to it.
    pub(crate) fn navigation_back_title(&self, screen: NodeId) -> Option<Arc<str>> {
        self.node(self.navigation_previous(screen)?)?
            .screen
            .back_title
            .clone()
    }

    /// Goes back in the innermost active stack, then asks its modal to close.
    /// Mounted preventBack/dismissible props decide before JS receives events.
    pub(crate) fn navigation_back(&mut self) {
        let ids = self.visible_ids();
        let screen = ids.iter().rev().copied().find(|&id| {
            self.node(id)
                .is_some_and(|node| node.kind == ViewKind::Screen)
                && self.navigation_can_back(id)
        });
        if let Some(screen) = screen {
            if self
                .node(screen)
                .is_some_and(|node| node.screen.prevent_back)
            {
                self.emit(screen, EventKind::BackRequest, EventPayload::None);
            } else {
                self.navigation_pop(screen, None);
            }
            return;
        }
        if let Some(modal) = ids.into_iter().rev().find(|&id| {
            self.node(id)
                .is_some_and(|node| node.kind == ViewKind::Modal)
        }) {
            self.navigation_request_close(modal);
        }
    }

    /// A scrim click or sheet gesture asks a controlled modal to close. A
    /// dismissible modal also closes natively, then reports completion.
    pub(crate) fn navigation_request_close(&mut self, id: NodeId) {
        if !self
            .node(id)
            .is_some_and(|node| node.kind == ViewKind::Modal && !node.popped)
        {
            return;
        }
        self.emit(id, EventKind::RequestClose, EventPayload::None);
        if self.node(id).is_some_and(|node| node.modal.dismissible) {
            self.navigation_pop(id, None);
        }
    }

    fn navigation_pop(&mut self, id: NodeId, position: Option<f32>) {
        let Some(node) = self.node(id) else { return };
        if node.popped {
            return;
        }
        let modal = node.kind == ViewKind::Modal;
        let below = if modal {
            None
        } else {
            self.navigation_previous(id)
        };
        let configured = if modal {
            modal_motion(node.modal.animation, self.reduce_motion)
        } else {
            screen_motion(node.screen.animation, self.reduce_motion)
        };
        let motion = if position.is_some() {
            // An interactive drag continues in its current direction even
            // when the configured noninteractive transition is a fade.
            self.navigation
                .track(id)
                .map(|track| {
                    let duration = if track.movement == Movement::Fade {
                        FADE_SECONDS
                    } else {
                        SLIDE_SECONDS
                    };
                    (track.movement, duration)
                })
                .or(configured)
        } else {
            configured
        };
        let from = position.unwrap_or_else(|| {
            self.navigation
                .track(id)
                .map_or(0.0, |track| track.fraction(self.now))
        });
        if let Some(node) = self.node_mut(id) {
            node.popped = true;
        }
        match motion {
            Some((movement, duration)) => self.navigation.set_track(
                id,
                Some(Track {
                    movement,
                    position: Position::Timed {
                        from,
                        to: 1.0,
                        started: self.now,
                        duration: (duration * (1.0 - from) as f64).max(0.01),
                    },
                    below,
                    completion: Completion::Pop,
                    modal,
                }),
            ),
            None => {
                self.navigation.set_track(id, None);
                self.navigation_complete_subtree(id);
                if modal {
                    self.navigation.modals.retain(|&modal| modal != id);
                }
                self.emit(id, EventKind::Dismiss, EventPayload::None);
            }
        }
        self.reconcile_focus();
    }

    /// The card's target geometry. Transparent presentations cover the viewport.
    pub(crate) fn modal_frame(&self, id: NodeId, target: Rect) -> Rect {
        let Some(node) = self.node(id).filter(|node| node.kind == ViewKind::Modal) else {
            return target;
        };
        modal_rect(
            node.modal,
            self.viewport(),
            self.metrics.width(),
            self.metrics.height(),
        )
    }

    /// Header buttons occupy one row; a large title gets a second row that
    /// collapses as its nearest vertical content scrolls underneath.
    pub(crate) fn navigation_header(&self, id: NodeId) -> HeaderLayout {
        let Some(node) = self.node(id) else {
            return HeaderLayout {
                height: 0.0,
                bar_height: 0.0,
                title_y: 0.0,
                large: false,
            };
        };
        if !node.screen.header_shown {
            return HeaderLayout {
                height: 0.0,
                bar_height: 0.0,
                title_y: 0.0,
                large: false,
            };
        }
        let bar_height = self.metrics.height();
        let extra = if node.screen.large_title {
            (bar_height - self.navigation_scroll_offset(id)).clamp(0.0, bar_height)
        } else {
            0.0
        };
        HeaderLayout {
            height: bar_height + extra,
            bar_height,
            title_y: extra,
            large: extra >= bar_height * 0.5,
        }
    }

    fn navigation_scroll_offset(&self, id: NodeId) -> f32 {
        let mut pending = self
            .node(id)
            .map_or_else(Vec::new, |node| node.children.clone());
        let mut index = 0;
        while index < pending.len() {
            let Some(node) = self.node(pending[index]) else {
                index += 1;
                continue;
            };
            if node.kind == ViewKind::Scroll && !node.scroll.horizontal {
                return (node.scroll.y + node.scroll.behavior.content_inset.top).max(0.0);
            }
            if !matches!(
                node.kind,
                ViewKind::HeaderItem | ViewKind::Screen | ViewKind::Modal
            ) {
                pending.extend(node.children.iter().copied());
            }
            index += 1;
        }
        0.0
    }

    /// Starts a candidate; presses still belong to the app until a directional
    /// drag passes a cell. Screen gestures respect the mounted back gates.
    pub(crate) fn navigation_pointer_down(&mut self, x: f32, y: f32) -> bool {
        self.navigation.gesture = None;
        if self.dialog.is_some() {
            return false;
        }
        let ids = self.visible_ids();
        let modal = ids.iter().rev().copied().find(|&id| {
            self.node(id)
                .is_some_and(|node| node.kind == ViewKind::Modal)
        });
        if let Some(id) = modal
            && let Some(node) = self.node(id)
            && node.modal.dismissible
            && self.navigation.track(id).is_none()
            && !node.modal.transparent
            && node.modal.presentation != ModalPresentation::FullScreen
        {
            let frame = self.modal_frame(id, node.frame);
            if contains(frame, x, y) && y < frame.y + self.metrics.height() {
                self.navigation.gesture = Some(Gesture {
                    id,
                    below: None,
                    origin: (x, y),
                    last: (x, y),
                    started: self.now,
                    last_time: self.now,
                    velocity: 0.0,
                    extent: (self.viewport().height - frame.y).max(self.metrics.height()),
                    modal: true,
                    claimed: false,
                });
                return true;
            }
        }
        for id in ids.into_iter().rev() {
            let Some(node) = self.node(id).filter(|node| node.kind == ViewKind::Screen) else {
                continue;
            };
            if !node.screen.gesture_enabled
                || node.screen.prevent_back
                || self.navigation.track(id).is_some()
            {
                continue;
            }
            let Some(below) = self.navigation_previous(id) else {
                continue;
            };
            let Some(frame) = self.global_frame(id) else {
                continue;
            };
            if contains(frame, x, y) && x < frame.x + self.metrics.width() * 3.0 {
                self.navigation.gesture = Some(Gesture {
                    id,
                    below: Some(below),
                    origin: (x, y),
                    last: (x, y),
                    started: self.now,
                    last_time: self.now,
                    velocity: 0.0,
                    extent: frame.width.max(self.metrics.width()),
                    modal: false,
                    claimed: false,
                });
                return true;
            }
        }
        false
    }

    /// Returns true once navigation owns this drag and the app's press cancels.
    pub(crate) fn navigation_pointer_drag(&mut self, x: f32, y: f32) -> bool {
        let Some(mut gesture) = self.navigation.gesture.take() else {
            return false;
        };
        let (dx, dy) = (x - gesture.origin.0, y - gesture.origin.1);
        let (distance, other, cell) = if gesture.modal {
            (dy, dx.abs(), self.metrics.height())
        } else {
            (dx, dy.abs(), self.metrics.width())
        };
        if !gesture.claimed {
            if other > distance.max(cell) || distance < -cell {
                return false;
            }
            if distance < cell {
                self.navigation.gesture = Some(gesture);
                return false;
            }
            gesture.claimed = true;
        }
        let last = if gesture.modal {
            gesture.last.1
        } else {
            gesture.last.0
        };
        let current = if gesture.modal { y } else { x };
        let elapsed = self.now - gesture.last_time;
        if elapsed > 0.0 {
            gesture.velocity = (current - last) / elapsed as f32;
        }
        gesture.last = (x, y);
        gesture.last_time = self.now;
        let position = (distance / gesture.extent).clamp(0.0, 1.0);
        self.navigation.set_track(
            gesture.id,
            Some(Track {
                movement: if self.reduce_motion {
                    Movement::Fade
                } else if gesture.modal {
                    Movement::Vertical
                } else {
                    Movement::Horizontal
                },
                position: Position::Interactive(position),
                below: gesture.below,
                completion: Completion::Restore,
                modal: gesture.modal,
            }),
        );
        self.navigation.gesture = Some(gesture);
        true
    }

    /// Completes a directional gesture or returns to the current route.
    pub(crate) fn navigation_pointer_up(&mut self, x: f32, y: f32) -> bool {
        let Some(gesture) = self.navigation.gesture.take() else {
            return false;
        };
        if !gesture.claimed {
            return false;
        }
        let distance = if gesture.modal {
            y - gesture.origin.1
        } else {
            x - gesture.origin.0
        };
        let position = (distance / gesture.extent).clamp(0.0, 1.0);
        let elapsed = (self.now - gesture.started).max(0.001) as f32;
        let velocity = if self.now - gesture.last_time < 0.1 {
            gesture.velocity
        } else {
            distance / elapsed
        };
        if position >= 0.35 || (position > 0.05 && velocity > gesture.extent * 0.5) {
            if gesture.modal {
                self.emit(gesture.id, EventKind::RequestClose, EventPayload::None);
            }
            self.navigation_pop(gesture.id, Some(position));
        } else {
            self.navigation_restore(gesture.id, position);
        }
        true
    }

    fn navigation_restore(&mut self, id: NodeId, position: f32) {
        if let Some(track) = self.navigation.track_mut(id) {
            track.position = Position::Timed {
                from: position,
                to: 0.0,
                started: self.now,
                duration: FADE_SECONDS,
            };
            track.completion = Completion::Restore;
        }
    }

    /// Losing pointer capture restores an interactive route without dismissing.
    pub(crate) fn navigation_pointer_cancel(&mut self) {
        if let Some(gesture) = self.navigation.gesture.take()
            && gesture.claimed
        {
            let position = self
                .navigation
                .track(gesture.id)
                .map_or(0.0, |track| track.fraction(self.now));
            self.navigation_restore(gesture.id, position);
        }
    }
}

fn screen_motion(animation: ScreenAnimation, reduce_motion: bool) -> Option<(Movement, f64)> {
    match animation {
        ScreenAnimation::None => None,
        ScreenAnimation::Fade => Some((Movement::Fade, FADE_SECONDS)),
        ScreenAnimation::Default if reduce_motion => Some((Movement::Fade, FADE_SECONDS)),
        ScreenAnimation::Default => Some((Movement::Horizontal, SLIDE_SECONDS)),
    }
}

fn modal_motion(animation: ModalAnimation, reduce_motion: bool) -> Option<(Movement, f64)> {
    match animation {
        ModalAnimation::None => None,
        ModalAnimation::Fade => Some((Movement::Fade, FADE_SECONDS)),
        ModalAnimation::Slide if reduce_motion => Some((Movement::Fade, FADE_SECONDS)),
        ModalAnimation::Slide => Some((Movement::Vertical, SLIDE_SECONDS)),
    }
}

fn modal_rect(config: ModalConfig, viewport: Size, cell_width: f32, cell_height: f32) -> Rect {
    let full = Rect {
        x: 0.0,
        y: 0.0,
        width: viewport.width,
        height: viewport.height,
    };
    if config.transparent || config.presentation == ModalPresentation::FullScreen {
        return full;
    }
    let (max_width, max_height) = match config.presentation {
        ModalPresentation::FullScreen => return full,
        ModalPresentation::PageSheet => (720.0, f32::INFINITY),
        ModalPresentation::FormSheet => (540.0, 620.0),
    };
    let margin_x = (viewport.width * 0.05)
        .min(cell_width * 4.0)
        .max(cell_width)
        .min(viewport.width * 0.25);
    let margin_y = (viewport.height * 0.05)
        .min(cell_height * 2.0)
        .max(cell_height)
        .min(viewport.height * 0.25);
    let snap = |value: f32, unit: f32| (value / unit).floor().max(1.0) * unit;
    let width = snap(
        (viewport.width - margin_x * 2.0).min(max_width).max(0.0),
        cell_width,
    )
    .min(viewport.width);
    let height = snap(
        (viewport.height - margin_y * 2.0).min(max_height).max(0.0),
        cell_height,
    )
    .min(viewport.height);
    Rect {
        x: ((viewport.width - width) / (2.0 * cell_width)).floor() * cell_width,
        y: ((viewport.height - height) / (2.0 * cell_height)).floor() * cell_height,
        width,
        height,
    }
}

fn contains(frame: Rect, x: f32, y: f32) -> bool {
    x >= frame.x && y >= frame.y && x < frame.x + frame.width && y < frame.y + frame.height
}

#[cfg(test)]
mod tests {
    use nsh_core::{ROOT_ID, ScreenConfig};

    use super::*;
    use crate::CellMetrics;

    fn frame() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 640.0,
        }
    }

    fn stack() -> Surface {
        let mut surface = Surface::new(100, 40, CellMetrics::default());
        surface.apply(MountBatch {
            items: vec![
                MountItem::Create {
                    id: 2,
                    kind: ViewKind::Stack,
                },
                MountItem::Insert {
                    parent: ROOT_ID,
                    child: 2,
                    before: None,
                },
                MountItem::Frame {
                    id: 2,
                    frame: frame(),
                },
                MountItem::Create {
                    id: 4,
                    kind: ViewKind::Screen,
                },
                MountItem::Insert {
                    parent: 2,
                    child: 4,
                    before: None,
                },
                MountItem::Frame {
                    id: 4,
                    frame: frame(),
                },
            ],
        });
        surface.drain_events().for_each(drop);
        surface
    }

    fn push(surface: &mut Surface, animation: ScreenAnimation) {
        surface.apply(MountBatch {
            items: vec![
                MountItem::Create {
                    id: 6,
                    kind: ViewKind::Screen,
                },
                MountItem::ScreenConfig {
                    id: 6,
                    config: Box::new(ScreenConfig {
                        animation,
                        ..ScreenConfig::default()
                    }),
                },
                MountItem::Insert {
                    parent: 2,
                    child: 6,
                    before: None,
                },
                MountItem::Frame {
                    id: 6,
                    frame: frame(),
                },
            ],
        });
    }

    fn events(surface: &mut Surface, kind: EventKind) -> Vec<NodeId> {
        surface
            .drain_events()
            .filter_map(|event| match event {
                SurfaceEvent::Event(id, event, _) if event == kind => Some(id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_push_moves_the_new_screen_and_retains_the_previous_during_motion() {
        let mut surface = stack();
        assert!(!surface.navigation_animating());
        push(&mut surface, ScreenAnimation::Default);
        assert_eq!(surface.navigation_values(6).0, 800.0);
        assert!(surface.navigation_draws_screen(2, 4));
        surface.tick(0.15);
        assert!((surface.navigation_values(6).0 - 100.0).abs() < 0.01);
        assert!(surface.navigation_values(4).0 < 0.0);
        surface.tick(0.31);
        assert_eq!(surface.navigation_values(6), (0.0, 0.0, 1.0));
        assert!(!surface.navigation_draws_screen(2, 4));
        assert!(!surface.navigation_animating());
    }

    #[test]
    fn mounted_back_gates_apply_before_any_javascript_response() {
        let mut surface = stack();
        push(&mut surface, ScreenAnimation::None);
        surface.node_mut(6).unwrap().screen.prevent_back = true;
        surface.navigation_back();
        assert!(!surface.node(6).unwrap().popped);
        assert_eq!(events(&mut surface, EventKind::BackRequest), vec![6]);
        assert!(!surface.navigation_pointer_down(0.0, 80.0));
        surface.node_mut(6).unwrap().screen.prevent_back = false;
        surface.navigation_back();
        assert!(surface.node(6).unwrap().popped);
        assert_eq!(events(&mut surface, EventKind::Dismiss), vec![6]);
    }

    #[test]
    fn native_pop_stays_visible_until_exit_finishes_and_reports_once() {
        let mut surface = stack();
        push(&mut surface, ScreenAnimation::Default);
        surface.tick(0.31);
        surface.navigation_back();
        assert!(surface.node(6).unwrap().popped);
        assert!(surface.navigation_draws_screen(2, 6));
        assert!(!surface.navigation_accepts(6));
        assert!(events(&mut surface, EventKind::Dismiss).is_empty());
        surface.tick(0.62);
        assert_eq!(events(&mut surface, EventKind::Dismiss), vec![6]);
        surface.tick(1.0);
        assert!(events(&mut surface, EventKind::Dismiss).is_empty());
        assert!(surface.node(6).is_some());
    }

    #[test]
    fn descendant_deletes_before_their_screen_keep_pixels_until_exit() {
        let mut surface = stack();
        push(&mut surface, ScreenAnimation::Default);
        surface.tick(0.31);
        surface.apply(MountBatch {
            items: vec![
                MountItem::Create {
                    id: 8,
                    kind: ViewKind::View,
                },
                MountItem::Insert {
                    parent: 6,
                    child: 8,
                    before: None,
                },
            ],
        });
        surface.apply(MountBatch {
            items: vec![MountItem::Delete { id: 8 }, MountItem::Delete { id: 6 }],
        });
        assert!(surface.node(8).is_some());
        assert!(surface.node(6).is_some());
        assert!(!surface.navigation_accepts(8));
        surface.tick(0.62);
        assert!(surface.node(8).is_none());
        assert!(surface.node(6).is_none());
        assert_eq!(surface.node(2).unwrap().children, vec![4]);
    }

    #[test]
    fn moving_a_screen_does_not_preserve_children_removed_in_the_same_commit() {
        let mut surface = stack();
        push(&mut surface, ScreenAnimation::Default);
        surface.tick(0.31);
        surface.apply(MountBatch {
            items: vec![
                MountItem::Create {
                    id: 8,
                    kind: ViewKind::View,
                },
                MountItem::Insert {
                    parent: 6,
                    child: 8,
                    before: None,
                },
            ],
        });
        surface.apply(MountBatch {
            items: vec![
                MountItem::Remove { id: 8 },
                MountItem::Remove { id: 6 },
                MountItem::Insert {
                    parent: 2,
                    child: 6,
                    before: None,
                },
            ],
        });
        assert_eq!(surface.node(8).unwrap().parent, None);
        assert_eq!(surface.node(6).unwrap().parent, Some(2));
        assert!(!surface.navigation_retains(6));
    }

    #[test]
    fn modal_lifecycle_follows_finite_fades_and_dismissible_controls() {
        let mut surface = stack();
        surface.apply(MountBatch {
            items: vec![
                MountItem::Create {
                    id: 8,
                    kind: ViewKind::Modal,
                },
                MountItem::ModalConfig {
                    id: 8,
                    config: ModalConfig {
                        animation: ModalAnimation::Fade,
                        ..ModalConfig::default()
                    },
                },
                MountItem::Insert {
                    parent: ROOT_ID,
                    child: 8,
                    before: None,
                },
                MountItem::Frame {
                    id: 8,
                    frame: frame(),
                },
            ],
        });
        assert!(events(&mut surface, EventKind::Show).is_empty());
        surface.tick(0.1);
        assert!(surface.navigation_values(8).2 > 0.5);
        surface.tick(0.21);
        assert_eq!(events(&mut surface, EventKind::Show), vec![8]);
        surface.navigation_request_close(8);
        assert!(!surface.node(8).unwrap().popped);
        assert_eq!(events(&mut surface, EventKind::RequestClose), vec![8]);
        surface.node_mut(8).unwrap().modal.dismissible = true;
        surface.navigation_request_close(8);
        assert!(surface.node(8).unwrap().popped);
        surface.tick(0.42);
        assert_eq!(events(&mut surface, EventKind::Dismiss), vec![8]);
    }

    #[test]
    fn reopened_earlier_sibling_is_the_top_modal_and_active_scope() {
        let mut surface = stack();
        surface.apply(MountBatch {
            items: vec![
                MountItem::Create {
                    id: 8,
                    kind: ViewKind::Modal,
                },
                MountItem::Create {
                    id: 10,
                    kind: ViewKind::Modal,
                },
                MountItem::ModalConfig {
                    id: 8,
                    config: ModalConfig {
                        animation: ModalAnimation::None,
                        ..ModalConfig::default()
                    },
                },
                MountItem::ModalConfig {
                    id: 10,
                    config: ModalConfig {
                        animation: ModalAnimation::None,
                        ..ModalConfig::default()
                    },
                },
                MountItem::Insert {
                    parent: nsh_core::ROOT_ID,
                    child: 10,
                    before: None,
                },
            ],
        });
        for _ in 0..2 {
            surface.apply(MountBatch {
                items: vec![MountItem::Insert {
                    parent: nsh_core::ROOT_ID,
                    child: 8,
                    before: Some(10),
                }],
            });
            let layers: Vec<_> = surface
                .paint_ids()
                .into_iter()
                .filter(|&id| {
                    surface
                        .node(id)
                        .is_some_and(|node| node.kind == ViewKind::Modal)
                })
                .collect();
            assert_eq!(layers, vec![10, 8]);
            assert_eq!(surface.visible_ids().first(), Some(&8));
            assert!(!surface.visible_ids().contains(&10));
            surface.apply(MountBatch {
                items: vec![MountItem::Remove { id: 8 }],
            });
            assert_eq!(surface.visible_ids().first(), Some(&10));
        }
        surface.apply(MountBatch {
            items: vec![MountItem::Insert {
                parent: nsh_core::ROOT_ID,
                child: 8,
                before: Some(10),
            }],
        });
        surface.apply(MountBatch {
            items: vec![MountItem::ModalConfig {
                id: 8,
                config: ModalConfig {
                    animation: ModalAnimation::Fade,
                    ..ModalConfig::default()
                },
            }],
        });
        surface.apply(MountBatch {
            items: vec![MountItem::Remove { id: 8 }],
        });
        assert_eq!(surface.paint_ids().last(), Some(&8));
        assert!(surface.visible_ids().is_empty());
        surface.tick(0.21);
        assert_eq!(surface.visible_ids().first(), Some(&10));
    }

    #[test]
    fn a_nested_modal_opened_before_a_sibling_stays_below_that_sibling() {
        let mut surface = stack();
        surface.apply(MountBatch {
            items: vec![
                MountItem::Create {
                    id: 8,
                    kind: ViewKind::Modal,
                },
                MountItem::Create {
                    id: 12,
                    kind: ViewKind::Modal,
                },
                MountItem::ModalConfig {
                    id: 8,
                    config: ModalConfig {
                        animation: ModalAnimation::None,
                        ..ModalConfig::default()
                    },
                },
                MountItem::ModalConfig {
                    id: 12,
                    config: ModalConfig {
                        animation: ModalAnimation::None,
                        ..ModalConfig::default()
                    },
                },
                MountItem::Insert {
                    parent: nsh_core::ROOT_ID,
                    child: 8,
                    before: None,
                },
                MountItem::Insert {
                    parent: 8,
                    child: 12,
                    before: None,
                },
            ],
        });
        surface.apply(MountBatch {
            items: vec![
                MountItem::Create {
                    id: 10,
                    kind: ViewKind::Modal,
                },
                MountItem::ModalConfig {
                    id: 10,
                    config: ModalConfig {
                        animation: ModalAnimation::None,
                        ..ModalConfig::default()
                    },
                },
                MountItem::Insert {
                    parent: nsh_core::ROOT_ID,
                    child: 10,
                    before: None,
                },
            ],
        });
        let layers: Vec<_> = surface
            .paint_ids()
            .into_iter()
            .filter(|&id| {
                surface
                    .node(id)
                    .is_some_and(|node| node.kind == ViewKind::Modal)
            })
            .collect();
        assert_eq!(layers, vec![8, 12, 10]);
        assert_eq!(surface.visible_ids().first(), Some(&10));
        assert!(!surface.visible_ids().contains(&12));
    }

    #[test]
    fn modal_insert_order_and_reopening_during_exit_both_bring_the_last_to_front() {
        let mut surface = stack();
        surface.apply(MountBatch {
            items: vec![
                MountItem::Create {
                    id: 8,
                    kind: ViewKind::Modal,
                },
                MountItem::Create {
                    id: 10,
                    kind: ViewKind::Modal,
                },
                MountItem::ModalConfig {
                    id: 8,
                    config: ModalConfig {
                        animation: ModalAnimation::None,
                        ..ModalConfig::default()
                    },
                },
                MountItem::ModalConfig {
                    id: 10,
                    config: ModalConfig {
                        animation: ModalAnimation::None,
                        ..ModalConfig::default()
                    },
                },
                MountItem::Insert {
                    parent: nsh_core::ROOT_ID,
                    child: 10,
                    before: None,
                },
                MountItem::Insert {
                    parent: nsh_core::ROOT_ID,
                    child: 8,
                    before: Some(10),
                },
            ],
        });
        assert_eq!(surface.navigation.modals, vec![10, 8]);
        assert_eq!(surface.visible_ids().first(), Some(&8));
        surface.apply(MountBatch {
            items: vec![MountItem::ModalConfig {
                id: 10,
                config: ModalConfig {
                    animation: ModalAnimation::Fade,
                    ..ModalConfig::default()
                },
            }],
        });
        surface.apply(MountBatch {
            items: vec![MountItem::Remove { id: 10 }],
        });
        assert!(surface.navigation_retains(10));
        assert_eq!(surface.visible_ids().first(), Some(&8));
        surface.apply(MountBatch {
            items: vec![MountItem::Insert {
                parent: nsh_core::ROOT_ID,
                child: 10,
                before: Some(8),
            }],
        });
        assert!(!surface.navigation_retains(10));
        assert_eq!(surface.navigation.modals, vec![8, 10]);
        assert_eq!(surface.visible_ids().first(), Some(&10));
        surface.tick(0.21);
        assert_eq!(surface.paint_ids().last(), Some(&10));
    }

    #[test]
    fn sheet_geometry_is_cell_aligned_and_transparency_uses_the_viewport() {
        let page = ModalConfig {
            presentation: ModalPresentation::PageSheet,
            ..ModalConfig::default()
        };
        let viewport = Size::new(1200.0, 800.0);
        let frame = modal_rect(page, viewport, 8.0, 16.0);
        assert_eq!(frame.width, 720.0);
        assert!(frame.x > 0.0 && frame.y > 0.0);
        assert_eq!(frame.y % 16.0, 0.0);
        let form = modal_rect(
            ModalConfig {
                presentation: ModalPresentation::FormSheet,
                ..page
            },
            viewport,
            8.0,
            16.0,
        );
        assert!(form.width <= 540.0 && form.height <= 620.0);
        assert_eq!(
            modal_rect(
                ModalConfig {
                    transparent: true,
                    ..page
                },
                viewport,
                8.0,
                16.0
            ),
            Rect {
                x: 0.0,
                y: 0.0,
                width: 1200.0,
                height: 800.0
            }
        );
        let small = modal_rect(page, Size::new(8.0, 16.0), 8.0, 16.0);
        assert!(small.width > 0.0 && small.height > 0.0);
    }

    #[test]
    fn reduced_motion_uses_a_fade_and_none_is_immediate() {
        assert_eq!(
            screen_motion(ScreenAnimation::Default, true),
            Some((Movement::Fade, FADE_SECONDS))
        );
        assert_eq!(
            modal_motion(ModalAnimation::Slide, true),
            Some((Movement::Fade, FADE_SECONDS))
        );
        assert_eq!(screen_motion(ScreenAnimation::None, true), None);
        let mut surface = stack();
        surface.reduce_motion = true;
        push(&mut surface, ScreenAnimation::Default);
        assert_eq!(surface.navigation_values(6), (0.0, 0.0, 0.0));
        surface.tick(0.21);
        assert!(!surface.navigation_animating());
    }

    #[test]
    fn interactive_back_can_cancel_then_complete_without_app_callbacks() {
        let mut surface = stack();
        push(&mut surface, ScreenAnimation::Default);
        surface.tick(0.31);
        assert!(surface.navigation_pointer_down(0.0, 80.0));
        surface.tick(0.5);
        assert!(surface.navigation_pointer_drag(80.0, 80.0));
        surface.tick(1.0);
        assert!(surface.navigation_pointer_up(80.0, 80.0));
        assert!(!surface.node(6).unwrap().popped);
        surface.tick(1.21);
        assert_eq!(surface.navigation_values(6), (0.0, 0.0, 1.0));
        assert!(surface.navigation_pointer_down(0.0, 80.0));
        surface.tick(1.5);
        assert!(surface.navigation_pointer_drag(400.0, 80.0));
        assert!(surface.navigation_pointer_up(400.0, 80.0));
        assert!(surface.node(6).unwrap().popped);
        surface.tick(1.8);
        assert_eq!(events(&mut surface, EventKind::Dismiss), vec![6]);
    }

    #[test]
    fn large_title_collapses_without_moving_header_buttons() {
        let mut surface = stack();
        surface.node_mut(4).unwrap().screen.large_title = true;
        surface.apply(MountBatch {
            items: vec![
                MountItem::Create {
                    id: 8,
                    kind: ViewKind::Scroll,
                },
                MountItem::Insert {
                    parent: 4,
                    child: 8,
                    before: None,
                },
            ],
        });
        let large = surface.navigation_header(4);
        assert_eq!(
            (large.height, large.bar_height, large.title_y, large.large),
            (32.0, 16.0, 16.0, true)
        );
        surface.node_mut(8).unwrap().scroll.y = 16.0;
        let compact = surface.navigation_header(4);
        assert_eq!(
            (
                compact.height,
                compact.bar_height,
                compact.title_y,
                compact.large
            ),
            (16.0, 16.0, 0.0, false)
        );
    }
}
