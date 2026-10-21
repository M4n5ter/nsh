//! Popovers' native focus lifetime. Core supplies their logical parent,
//! because the native overlay views are siblings in the surface or modal.
//! Capture before a batch can run child autoFocus; reconcile its final
//! tree, so moving an overlay is not a close followed by another opening.

use std::rc::Rc;

use nsh_core::{Command, MountBatch, MountItem, NodeId, ViewKind};

use crate::{surface::Surface, view::NshView};

#[derive(Default)]
pub(crate) struct FocusReturns {
    popups: Vec<Popup>,
}

struct Popup {
    view: NshView,
    parent: NshView,
    restore: Option<NshView>,
}

pub(crate) struct BeforeBatch {
    focused: Option<NshView>,
    owner: Option<NshView>,
}

fn focused(surface: &Surface) -> Option<NshView> {
    crate::native::requested_focus(surface)
        .and_then(|id| surface.view(id))
        .or_else(|| crate::text_input::focused_view(surface))
        .or_else(|| crate::native::focused_component(surface).and_then(|id| surface.view(id)))
}

fn contains(parent: &NshView, child: &NshView) -> bool {
    let mut next = Some(child.clone());
    while let Some(view) = next {
        if view == *parent {
            return true;
        }
        next = view.parent_view();
    }
    false
}

fn current(surface: &Surface, view: &NshView) -> bool {
    surface.view(view.id()).as_ref() == Some(view)
}

fn candidate(item: &MountItem) -> Option<NodeId> {
    match item {
        MountItem::ViewProps { id, .. } | MountItem::ViewStyle { id, .. } => Some(*id),
        MountItem::Insert { child, .. } => Some(*child),
        _ => None,
    }
}

fn eligible(surface: &Surface, view: &NshView) -> bool {
    current(surface, view)
        && crate::input::focusable(surface, view)
        && !leaving(view)
        && contains(&crate::navigation::focus_scope(surface), view)
}

fn leaving(view: &NshView) -> bool {
    let mut next = Some(view.clone());
    while let Some(view) = next {
        if view.state().live.as_ref().is_some_and(|live| live.dying()) {
            return true;
        }
        next = view.parent_view();
    }
    false
}

fn popup_parent(surface: &Surface, view: &NshView) -> Option<NshView> {
    if !current(surface, view) || !crate::navigation::is_shown(view) || leaving(view) {
        return None;
    }
    let parent = surface.view(view.state().props.focus_return_parent?)?;
    // A logical parent can be a native sibling, but it must still be the
    // shown popup the core identified, or the real surface/modal root.
    if parent != surface.root
        && !(parent.kind() == ViewKind::Modal
            && current(surface, &parent)
            && crate::navigation::is_shown(&parent))
        && popup_parent(surface, &parent).is_none()
    {
        return None;
    }
    Some(parent)
}

fn owner<'a>(popups: &'a [Popup], view: &NshView) -> Option<&'a Popup> {
    let mut next = Some(view.clone());
    while let Some(view) = next {
        if let Some(popup) = popups.iter().find(|popup| popup.view == view) {
            return Some(popup);
        }
        next = view.parent_view();
    }
    None
}

fn within(popups: &[Popup], parent: &NshView, view: &NshView) -> bool {
    if contains(parent, view) {
        return true;
    }
    let mut next = owner(popups, view);
    while let Some(popup) = next {
        if popup.parent == *parent {
            return true;
        }
        next = popups.iter().find(|parent| parent.view == popup.parent);
    }
    false
}

fn restore_target<'a>(
    surface: &Surface,
    active: &[Popup],
    closing: &'a [Popup],
    mut returning: Option<&'a Popup>,
) -> Option<NshView> {
    while let Some(popup) = returning {
        if let Some(target) = &popup.restore {
            if let Some(parent) = owner(closing, target) {
                returning = Some(parent);
                continue;
            }
            if eligible(surface, target) {
                return Some(target.clone());
            }
        }
        // Deleted children no longer have native ancestry. The captured
        // logical parent still identifies the next owner to unwind.
        returning = closing.iter().find(|parent| parent.view == popup.parent);
        if returning.is_none()
            && active.iter().any(|parent| parent.view == popup.parent)
            && let Some(target) = crate::input::first_focus_stop(surface, &popup.parent)
            && eligible(surface, &target)
        {
            return Some(target);
        }
    }
    None
}

impl FocusReturns {
    pub(crate) fn clear(&mut self) {
        self.popups.clear();
    }

    fn reconcile(
        &mut self,
        surface: &Surface,
        batch: &MountBatch,
        before: BeforeBatch,
    ) -> Option<NshView> {
        let now = focused(surface);
        let mut closing = Vec::new();
        let mut retained = Vec::with_capacity(self.popups.len());
        for popup in self.popups.drain(..) {
            if popup_parent(surface, &popup.view).as_ref() == Some(&popup.parent) {
                retained.push(popup);
            } else {
                closing.push(popup);
            }
        }
        self.popups = retained;

        let previous_owner = before
            .owner
            .as_ref()
            .and_then(|view| closing.iter().find(|popup| popup.view == *view));
        let restore = match previous_owner {
            Some(owner) => restore_target(surface, &self.popups, &closing, Some(owner)),
            None => before.focused.clone(),
        };

        let mut opened = Vec::new();
        for item in &batch.items {
            let Some(view) = candidate(item).and_then(|id| surface.view(id)) else {
                continue;
            };
            if self.popups.iter().any(|popup| popup.view == view) {
                continue;
            }
            let Some(parent) = popup_parent(surface, &view) else {
                continue;
            };
            self.popups.push(Popup {
                view: view.clone(),
                parent,
                restore: restore.clone().filter(|target| !contains(&view, target)),
            });
            opened.push(view);
        }

        // New nested popups may have been configured before their parent.
        // Open the innermost last, independently of native sibling ancestry.
        opened.sort_by_key(|view| {
            let mut depth = 0;
            let mut next = self.popups.iter().find(|popup| popup.view == *view);
            while let Some(popup) = next {
                depth += 1;
                next = self
                    .popups
                    .iter()
                    .find(|parent| parent.view == popup.parent);
            }
            depth
        });
        for view in &opened {
            let index = self
                .popups
                .iter()
                .position(|popup| popup.view == *view)
                .unwrap();
            if opened.contains(&self.popups[index].parent) {
                self.popups[index].restore =
                    crate::input::first_focus_stop(surface, &self.popups[index].parent);
            }
        }
        let blurred = batch.items.iter().any(|item| {
            matches!(
                item,
                MountItem::Command {
                    command: Command::Blur,
                    ..
                }
            )
        });
        let explicit = blurred
            || batch.items.iter().any(|item| {
                matches!(
                    item,
                    MountItem::Command {
                        command: Command::Focus,
                        ..
                    }
                )
            });
        if !explicit {
            for view in opened.iter().rev() {
                if !contains(&crate::navigation::focus_scope(surface), view) {
                    continue;
                }
                // A child autoFocus already established the correct owner.
                // Keep its caret, including in a logical nested popup.
                if now
                    .as_ref()
                    .is_some_and(|field| within(&self.popups, view, field))
                {
                    return None;
                }
                if let Some(target) = crate::input::first_focus_stop(surface, view) {
                    return Some(target);
                }
            }
        }

        // A pointer press outside may have moved focus before the app's
        // close batch. Only the departing owner can give focus back.
        if blurred && now.is_none() {
            return None;
        }
        let returning = match &now {
            Some(view) => owner(&closing, view),
            None => previous_owner,
        };
        restore_target(surface, &self.popups, &closing, returning)
    }
}

/// Captures the real native owner before a child autoFocus or Delete.
/// Streamed children do not change a popup's lifetime. Only changed popup
/// roots and their mounted ancestors need a focus-return reconciliation.
pub(crate) fn before_batch(surface: &Surface, batch: &MountBatch) -> Option<BeforeBatch> {
    let state = surface.focus_returns.borrow();
    if !batch.items.iter().any(|item| {
        matches!(item, MountItem::ViewProps { props, .. } if props.focus_return_parent.is_some())
            || candidate(item)
                .and_then(|id| surface.view(id))
                .is_some_and(|view| {
                    view.state().props.focus_return_parent.is_some()
                        || state
                            .popups
                            .iter()
                            .any(|popup| contains(&view, &popup.view))
                })
            || match item {
                MountItem::Remove { id } | MountItem::Delete { id } => {
                    surface.view(*id).is_some_and(|view| {
                        state
                            .popups
                            .iter()
                            .any(|popup| contains(&view, &popup.view))
                    })
                }
                _ => false,
            }
    }) {
        return None;
    }
    let focused = focused(surface);
    let owner = focused
        .as_ref()
        .and_then(|view| owner(&state.popups, view))
        .map(|popup| popup.view.clone());
    Some(BeforeBatch { focused, owner })
}

pub(crate) fn after_batch(surface: &Rc<Surface>, batch: &MountBatch, before: Option<BeforeBatch>) {
    let Some(before) = before else { return };
    let target = surface
        .focus_returns
        .borrow_mut()
        .reconcile(surface, batch, before);
    if let Some(target) = target {
        crate::input::focus_view(surface, &target);
    }
}

/// Tab follows the popup that currently owns native focus. Pointer hit
/// testing and explicit focus outside it remain unrestricted.
pub(crate) fn scope(surface: &Surface) -> Option<NshView> {
    let state = surface.focus_returns.borrow();
    let active = |popup: &&Popup| {
        popup_parent(surface, &popup.view).as_ref() == Some(&popup.parent)
            && contains(&crate::navigation::focus_scope(surface), &popup.view)
    };
    let popup = match focused(surface) {
        Some(view) => owner(&state.popups, &view).filter(active),
        None => state.popups.iter().rev().find(active),
    };
    popup.map(|popup| popup.view.clone())
}
