//! Main-thread terminal interaction. Keymaps, editing limits, focus scopes
//! and gesture decisions read mounted state without waiting for JavaScript.

use std::{ops::Range, time::Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use nsh_core::{
    EventKind, EventPayload, NodeId, PayloadValue, PointerEvents, ROOT_ID, SubmitBehavior,
    TouchEvent, TouchPhase, TouchPoint, ViewKind,
    keymap::{Key, KeyMatch, Keystroke, Modifiers},
    tailwind::state,
};

use crate::surface::{Surface, SurfaceEvent};

/// A focused input's native editing context for a host's completion,
/// autocorrection or autofill service. Terminal keys remain literal;
/// hosts decide which services to offer from these mounted hints.
#[derive(Clone, Debug, PartialEq)]
pub struct InputContext {
    /// The focused input.
    pub node: NodeId,
    /// Current text, redacted for secure inputs.
    pub text: Option<String>,
    /// Selected UTF-16 range, or the caret when empty.
    pub selection: Range<u32>,
    /// Native edit generation that a replacement must acknowledge.
    pub event_count: u32,
    /// Intended keyboard content.
    pub keyboard: nsh_core::KeyboardType,
    /// Capitalization hint for a host keyboard.
    pub auto_capitalize: nsh_core::AutoCapitalize,
    /// Whether the app requests correction suggestions.
    pub auto_correct: bool,
    /// Autofill content hint.
    pub auto_fill: nsh_core::AutoFill,
    /// The input hides its text; hosts keep automatic suggestions private.
    pub secure: bool,
    /// The mounted input currently accepts edits.
    pub editable: bool,
    /// The input accepts line breaks.
    pub multiline: bool,
    /// Maximum text length in UTF-16 units.
    pub max_length: Option<u32>,
}

/// A literal host-provided replacement. The surface checks its generation,
/// focus, editability, grapheme boundaries and mounted length limit before
/// reporting a native edit to the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputEdit {
    /// Input that the host inspected.
    pub node: NodeId,
    /// Native edit generation supplied by [`InputContext`].
    pub expected_event_count: u32,
    /// UTF-16 range to replace.
    pub selection: Range<u32>,
    /// Literal replacement, without automatic capitalization.
    pub replacement: String,
}

/// A terminal reports one mouse pointer. Its retained context also drives
/// long presses while the event loop is otherwise idle.
#[derive(Debug)]
pub(crate) struct Pointer {
    target: NodeId,
    pressable: Option<NodeId>,
    origin: (f32, f32),
    last: (f32, f32),
    started: Instant,
    moved: bool,
    long_fired: bool,
    pan_candidate: Option<NodeId>,
    pan: Option<NodeId>,
    pan_base: (f32, f32),
    native: Option<NodeId>,
    link: Option<NodeId>,
    scroll: Option<NodeId>,
    button: MouseButton,
    navigation: bool,
    navigation_claimed: bool,
}

impl Surface {
    /// Returns the focused input's hints and current generation. Secure
    /// fields expose their selection and hints while redacting text.
    pub fn input_context(&self) -> Option<InputContext> {
        if self.dialog.is_some() {
            return None;
        }
        let id = self.focus?;
        let node = self.node(id).filter(|node| node.kind == ViewKind::Input)?;
        let (start, end) = node.editor.selection();
        Some(InputContext {
            node: id,
            text: (!node.input.secure).then(|| node.editor.text().to_owned()),
            selection: start..end,
            event_count: node.editor.count(),
            keyboard: node.input.keyboard,
            auto_capitalize: node.input.auto_capitalize,
            auto_correct: node.input.auto_correct,
            auto_fill: node.input.auto_fill,
            secure: node.input.secure,
            editable: node.input.editable,
            multiline: node.input.multiline,
            max_length: node.input.max_length,
        })
    }

    /// Applies a completion, correction, paste or authorized autofill
    /// result only while its original input generation is still current.
    /// Returns whether the text or selection changed.
    pub fn apply_input_edit(&mut self, edit: InputEdit) -> bool {
        if self.dialog.is_some() || self.focus != Some(edit.node) {
            return false;
        }
        let Some(node) = self.node(edit.node) else {
            return false;
        };
        if node.kind != ViewKind::Input
            || !node.input.editable
            || node.editor.count() != edit.expected_event_count
            || edit.selection.start > edit.selection.end
            || !node.editor.is_boundary_utf16(edit.selection.start)
            || !node.editor.is_boundary_utf16(edit.selection.end)
        {
            return false;
        }
        let before = (node.editor.count(), node.editor.selection());
        self.edit_input(edit.node, |editor, config| {
            editor.replace_range(
                edit.selection.start,
                edit.selection.end,
                &edit.replacement,
                config,
            )
        });
        self.node(edit.node)
            .is_some_and(|node| (node.editor.count(), node.editor.selection()) != before)
    }

    /// Inserts text through the mounted capitalization policy for a host
    /// keyboard. Physical terminal keys and bracketed paste stay literal;
    /// an embedding host calls this when it provides keyboard assistance.
    pub fn type_text(&mut self, text: &str) -> bool {
        if self.dialog.is_some() {
            return false;
        }
        let Some(id) = self.focus.filter(|id| {
            self.node(*id)
                .is_some_and(|node| node.kind == ViewKind::Input && node.input.editable)
        }) else {
            return false;
        };
        let count = self.node(id).map_or(0, |node| node.editor.count());
        self.edit_input(id, |editor, config| editor.typed(text, config));
        self.node(id)
            .is_some_and(|node| node.editor.count() != count)
    }

    /// Handles an event in this surface's terminal coordinates. Embedded
    /// hosts translate mouse coordinates to the surface before forwarding.
    pub fn handle_event(&mut self, event: Event) -> bool {
        if self.service_event(&event) {
            return true;
        }
        match event {
            Event::Resize(columns, rows) => {
                self.resize(columns, rows);
                true
            }
            Event::Key(key) => self.key_event(key),
            Event::Mouse(mouse) => self.mouse_event(mouse),
            Event::Paste(text) => {
                if self.dialog.is_some() {
                    return true;
                }
                if let Some(id) = self.focus
                    && self.node(id).is_some_and(|node| node.native.is_some())
                    && self.native_event(id, &Event::Paste(text.clone()))
                {
                    return true;
                }
                let Some(id) = self
                    .focus
                    .filter(|id| self.node(*id).is_some_and(|n| n.kind == ViewKind::Input))
                else {
                    return false;
                };
                self.edit_input(id, |editor, config| editor.replace(&text, config));
                true
            }
            Event::FocusLost => {
                if let Some(id) = self.focus {
                    self.remember_focus(id);
                }
                self.set_focus(None);
                self.set_hover(None);
                self.cancel_pointer();
                true
            }
            Event::FocusGained => {
                self.reconcile_focus();
                true
            }
        }
    }

    pub(crate) fn set_focus(&mut self, id: Option<NodeId>) {
        let visible = self.visible_ids();
        let id = id.filter(|id| visible.contains(id) && self.focusable(*id));
        if self.focus == id {
            return;
        }
        let old = self.focus;
        let old_hover = self.hovered_path.clone();
        self.focus = id;
        if let Some(old) = old {
            self.emit_if(old, EventKind::Blur, EventPayload::None);
        }
        if let Some(id) = id {
            self.emit_if(id, EventKind::Focus, EventPayload::None);
        }
        self.report_states(old, self.pressed, &old_hover);
        if let Some(id) = id {
            self.reveal_focus(id);
        }
    }

    /// A batch may remove the focused field or cover it with a modal.
    /// Focus follows the active scope and returns when that scope closes.
    pub(crate) fn reconcile_focus(&mut self) {
        let visible = self.visible_ids();
        let displaced = self
            .focus
            .is_some_and(|id| !visible.contains(&id) || !self.focusable(id));
        if displaced {
            if let Some(id) = self.focus.filter(|id| self.node(*id).is_some()) {
                self.remember_focus(id);
            }
            self.set_focus(None);
        }
        self.autofocus_seen.retain(|id| {
            *id == ROOT_ID || self.nodes.get(*id as usize).is_some_and(Option::is_some)
        });
        let fresh_auto = visible.iter().copied().find(|id| {
            self.node(*id)
                .is_some_and(|n| n.kind == ViewKind::Input && n.input.auto_focus)
                && !self.autofocus_seen.contains(id)
                && self.focusable(*id)
        });
        for id in &visible {
            if self
                .node(*id)
                .is_some_and(|n| n.kind == ViewKind::Input && n.input.auto_focus)
                && !self.autofocus_seen.contains(id)
            {
                self.autofocus_seen.push(*id);
            }
        }
        if let Some(id) = fresh_auto {
            self.set_focus(Some(id));
        } else if self.focus.is_none() {
            let restore = self
                .focus_history
                .iter()
                .rposition(|id| visible.contains(id) && self.focusable(*id));
            if let Some(index) = restore {
                let id = self.focus_history.remove(index);
                self.set_focus(Some(id));
            } else if displaced || !self.autofocus_seen.contains(&ROOT_ID) {
                let first = visible.iter().copied().find(|id| self.focusable(*id));
                self.set_focus(first);
            }
        }
        if !self.autofocus_seen.contains(&ROOT_ID) {
            self.autofocus_seen.push(ROOT_ID);
        }
        if self
            .pointer
            .as_ref()
            .is_some_and(|p| !visible.contains(&p.target))
        {
            self.cancel_pointer();
        }
        if self.hovered.is_some_and(|id| !visible.contains(&id)) {
            self.set_hover(None);
        }
    }

    fn remember_focus(&mut self, id: NodeId) {
        self.focus_history.retain(|old| *old != id);
        self.focus_history.push(id);
        if self.focus_history.len() > 32 {
            self.focus_history.remove(0);
        }
    }

    pub(crate) fn input_value(&mut self, id: NodeId, text: String, count: u32) {
        if self
            .node_mut(id)
            .is_some_and(|n| n.editor.controlled(text, count))
        {
            self.report_selection(id);
        }
    }

    pub(crate) fn selection(&mut self, id: NodeId, start: u32, end: u32) {
        if self
            .node_mut(id)
            .is_some_and(|n| n.editor.set_selection(start, end))
        {
            self.report_selection(id);
        }
    }

    fn emit_if(&mut self, id: NodeId, kind: EventKind, payload: EventPayload) {
        let events = self.node(id).map(|n| n.events).or_else(|| {
            self.nodes
                .iter()
                .flatten()
                .filter_map(|n| n.text.as_ref())
                .flat_map(|(text, _, _)| &text.links)
                .find(|link| link.node == id)
                .map(|link| link.events)
        });
        if events.is_some_and(|events| events.contains(kind)) {
            self.emit(id, kind, payload);
        }
    }

    fn report_selection(&mut self, id: NodeId) {
        let Some(node) = self.node(id) else { return };
        let (start, end) = node.editor.selection();
        self.emit_if(
            id,
            EventKind::SelectionChange,
            EventPayload::Map(vec![
                ("start", PayloadValue::Number(start as f64)),
                ("end", PayloadValue::Number(end as f64)),
            ]),
        );
    }

    fn edit_input(
        &mut self,
        id: NodeId,
        edit: impl FnOnce(&mut crate::editor::Editor, &nsh_core::InputConfig) -> bool,
    ) {
        let Some(node) = self.node_mut(id) else {
            return;
        };
        let old_selection = node.editor.selection();
        let changed = edit(&mut node.editor, &node.input);
        let selection_changed = old_selection != node.editor.selection();
        if changed {
            let payload = EventPayload::Text {
                text: node.editor.text().to_owned(),
                count: node.editor.count(),
            };
            // The core needs every native edit for input measurement and
            // event counts, even without a JS change listener.
            self.emit(id, EventKind::ChangeText, payload);
        }
        if changed || selection_changed {
            self.report_selection(id);
        }
    }

    fn focusable(&self, id: NodeId) -> bool {
        let Some(node) = self.node(id) else {
            return false;
        };
        if node.style.hidden
            || node.props.accessibility.state.disabled
            || node.props.pointer_events == PointerEvents::None
        {
            return false;
        }
        if self.ancestors(id).into_iter().skip(1).any(|id| {
            self.node(id)
                .is_some_and(|n| n.props.pointer_events == PointerEvents::None)
        }) {
            return false;
        }
        node.kind == ViewKind::Input
            || node
                .text
                .as_ref()
                .is_some_and(|(text, _, _)| text.is_selectable())
            || (node.kind == ViewKind::Scroll && node.scroll.enabled)
            || node
                .native
                .as_ref()
                .is_some_and(|native| native.focusable())
            || node.media.is_some()
            || self.pressable(id)
    }

    fn pressable(&self, id: NodeId) -> bool {
        self.node(id).is_some_and(|n| {
            !n.props.accessibility.state.disabled
                && [
                    EventKind::Press,
                    EventKind::LongPress,
                    EventKind::PressIn,
                    EventKind::PressOut,
                ]
                .into_iter()
                .any(|e| n.events.contains(e))
        })
    }

    fn ancestors(&self, id: NodeId) -> Vec<NodeId> {
        let mut result = Vec::new();
        let mut next = Some(id);
        while let Some(id) = next {
            if result.contains(&id) {
                break;
            }
            result.push(id);
            next = self.node(id).and_then(|n| n.parent);
        }
        result
    }

    fn focus_next(&mut self, backwards: bool) -> bool {
        let stops: Vec<_> = self
            .visible_ids()
            .into_iter()
            .filter(|id| self.focusable(*id))
            .collect();
        if stops.is_empty() {
            return false;
        }
        let current = self
            .focus
            .and_then(|id| stops.iter().position(|stop| *stop == id));
        let next = match (current, backwards) {
            (Some(i), false) => (i + 1) % stops.len(),
            (Some(i), true) => (i + stops.len() - 1) % stops.len(),
            (None, false) => 0,
            (None, true) => stops.len() - 1,
        };
        let id = stops[next];
        self.set_focus(Some(id));
        if self.node(id).is_some_and(|n| n.kind == ViewKind::Input) {
            self.edit_input(id, |editor, _| {
                editor.select_all();
                false
            });
        }
        true
    }

    fn reveal_focus(&mut self, id: NodeId) {
        let ancestors = self.ancestors(id);
        for scroll in ancestors.into_iter().skip(1) {
            let Some(node) = self.node(scroll) else {
                continue;
            };
            if node.kind != ViewKind::Scroll || !node.scroll.enabled {
                continue;
            }
            let (x, y) = (node.scroll.x, node.scroll.y);
            let mirrored = node.scroll.mirrored;
            let viewport = (node.frame.width, node.frame.height);
            let (Some(matrix), Some(target)) =
                (self.local_to_surface(id), self.presentation_frame(id))
            else {
                continue;
            };
            let points = [
                (0.0, 0.0),
                (target.width, 0.0),
                (0.0, target.height),
                (target.width, target.height),
            ];
            let mut bounds = (
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            );
            for point in points {
                if let Some(point) = crate::geometry::project(&matrix, point.0, point.1)
                    && let Some(point) = self.surface_to_local(scroll, point.0, point.1)
                {
                    bounds.0 = bounds.0.min(point.0);
                    bounds.1 = bounds.1.min(point.1);
                    bounds.2 = bounds.2.max(point.0);
                    bounds.3 = bounds.3.max(point.1);
                }
            }
            if !bounds.0.is_finite() {
                continue;
            }
            let dx = if bounds.0 < 0.0 {
                bounds.0
            } else {
                (bounds.0 + (bounds.2 - bounds.0).min(viewport.0) - viewport.0).max(0.0)
            };
            let dy = if bounds.1 < 0.0 {
                bounds.1
            } else {
                (bounds.1 + (bounds.3 - bounds.1).min(viewport.1) - viewport.1).max(0.0)
            };
            if dx != 0.0 || dy != 0.0 {
                self.scroll_to(scroll, x + if mirrored { -dx } else { dx }, y + dy);
            }
        }
    }

    fn key_event(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let command = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('q') {
            self.quit = true;
            return true;
        }
        if self.dialog.is_some() {
            return self.dialog_key(key);
        }
        let editing = self.focus.is_some_and(|id| {
            self.node(id).is_some_and(|n| {
                n.kind == ViewKind::Input
                    || n.native.is_some()
                    || n.kind == ViewKind::WebView
                    || n.text
                        .as_ref()
                        .is_some_and(|(text, _, _)| text.is_selectable())
            })
        });
        if let Some(stroke) = keystroke(key) {
            match self
                .keymap
                .find(stroke, editing, key.kind == KeyEventKind::Repeat)
            {
                KeyMatch::Fire(id) => {
                    self.emit(
                        ROOT_ID,
                        EventKind::KeyCommand,
                        EventPayload::Value(PayloadValue::Number(id as f64)),
                    );
                    return true;
                }
                KeyMatch::Held => return true,
                KeyMatch::None => {}
            }
        }
        if let Some(id) = self.focus
            && self.node(id).is_some_and(|node| node.native.is_some())
            && self.native_event(id, &Event::Key(key))
        {
            return true;
        }
        if let Some(id) = self.focus
            && self.node(id).is_some_and(|node| node.media.is_some())
            && self.media_key(id, key)
        {
            return true;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
            && !command
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            return self.focus_next(
                key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
            );
        }
        if self
            .focus
            .is_some_and(|id| self.node(id).is_some_and(|n| n.kind == ViewKind::Input))
            && self.input_key(self.focus.unwrap_or(ROOT_ID), key)
        {
            return true;
        }
        if let Some(id) = self.focus
            && self.text_key(id, key)
        {
            return true;
        }
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Left && key.modifiers.contains(KeyModifiers::ALT))
        {
            self.back();
            return true;
        }
        if !command
            && !key.modifiers.contains(KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Enter | KeyCode::Char(' '))
            && let Some(id) = self.focus.filter(|id| self.pressable(*id))
        {
            if key.kind != KeyEventKind::Repeat {
                self.activate(id);
            }
            return true;
        }
        if self.scroll_key(key) {
            return true;
        }
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
            self.quit = true;
            return true;
        }
        false
    }

    fn input_key(&mut self, id: NodeId, key: KeyEvent) -> bool {
        let Some(node) = self.node(id) else {
            return false;
        };
        let config = node.input.clone();
        let command = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let lower = match key.code {
            KeyCode::Char(c) => Some(c.to_ascii_lowercase()),
            _ => None,
        };
        let columns = if config.multiline {
            self.input_hit_geometry(id)
                .map_or(1, |geometry| geometry.columns.max(1))
        } else {
            usize::MAX
        };
        if command {
            match lower {
                Some('a') => self.edit_input(id, |editor, _| {
                    editor.select_all();
                    false
                }),
                Some('c') => {
                    if !config.secure {
                        self.write_clipboard(node.editor.selected().to_owned());
                    }
                }
                Some('x') => {
                    if !config.secure {
                        self.write_clipboard(node.editor.selected().to_owned());
                        self.edit_input(id, |editor, config| {
                            if editor.selected_range().is_empty() {
                                false
                            } else {
                                editor.replace("", config)
                            }
                        });
                    }
                }
                Some('v') => {
                    self.request_paste(id);
                }
                Some('z') if shift => self.edit_input(id, |editor, config| editor.redo(config)),
                Some('z') => self.edit_input(id, |editor, config| editor.undo(config)),
                Some('y') => self.edit_input(id, |editor, config| editor.redo(config)),
                Some('w') => {
                    self.edit_input(id, |editor, config| editor.delete(true, true, config))
                }
                Some('k' | 'u') => {
                    let end = lower == Some('k');
                    self.edit_input(id, |editor, config| {
                        editor.home_end(end, false, true);
                        editor.replace("", config)
                    });
                }
                _ => return self.input_navigation(id, key, columns, config.secure),
            }
            return true;
        }
        match key.code {
            KeyCode::Enter => {
                self.key_press(id, "Enter".into());
                let submit = config.submit();
                if config.multiline && (shift || alt || submit == SubmitBehavior::Newline) {
                    self.edit_input(id, |editor, config| editor.replace("\n", config));
                } else {
                    if let Some(node) = self.node(id) {
                        let payload = EventPayload::Text {
                            text: node.editor.text().to_owned(),
                            count: node.editor.count(),
                        };
                        self.emit_if(id, EventKind::SubmitEditing, payload);
                    }
                    if submit == SubmitBehavior::BlurAndSubmit {
                        self.set_focus(None);
                    }
                }
                true
            }
            KeyCode::Backspace | KeyCode::Delete => {
                self.key_press(
                    id,
                    if key.code == KeyCode::Backspace {
                        "Backspace"
                    } else {
                        "Delete"
                    }
                    .into(),
                );
                self.edit_input(id, |editor, config| {
                    editor.delete(key.code == KeyCode::Backspace, alt, config)
                });
                true
            }
            KeyCode::Char(c) if !alt && !c.is_control() => {
                let text = c.to_string();
                self.key_press(id, text.clone());
                self.edit_input(id, |editor, config| editor.replace(&text, config));
                true
            }
            KeyCode::Insert if shift => {
                self.request_paste(id);
                true
            }
            _ => self.input_navigation(id, key, columns, config.secure),
        }
    }

    fn input_navigation(
        &mut self,
        id: NodeId,
        key: KeyEvent,
        columns: usize,
        secure: bool,
    ) -> bool {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let command = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
        let word = command || key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Left | KeyCode::Right => self.edit_input(id, |editor, _| {
                editor.horizontal(key.code == KeyCode::Right, word, shift);
                false
            }),
            KeyCode::Home | KeyCode::End => self.edit_input(id, |editor, _| {
                editor.home_end(key.code == KeyCode::End, command, shift);
                false
            }),
            KeyCode::Up | KeyCode::Down if columns != usize::MAX => {
                let alignments = self.node(id).map_or((0, 0), |node| {
                    let row = node.editor.cursor_cell(columns, secure).0;
                    let next = if key.code == KeyCode::Down {
                        row.saturating_add(1)
                    } else {
                        row.saturating_sub(1)
                    };
                    let offsets = crate::render::input_line_offsets(node, true, columns);
                    (
                        offsets.get(row).copied().unwrap_or(0),
                        offsets.get(next).copied().unwrap_or(0),
                    )
                });
                self.edit_input(id, |editor, _| {
                    editor.vertical(
                        key.code == KeyCode::Down,
                        shift,
                        columns,
                        secure,
                        alignments,
                    );
                    false
                })
            }
            KeyCode::Backspace | KeyCode::Delete if command => self
                .edit_input(id, |editor, config| {
                    editor.delete(key.code == KeyCode::Backspace, true, config)
                }),
            KeyCode::Insert if command => {
                if let Some(node) = self.node(id).filter(|n| !n.input.secure) {
                    self.write_clipboard(node.editor.selected().to_owned());
                }
            }
            KeyCode::Esc => {
                self.key_press(id, "Escape".into());
                self.set_focus(None);
                return false;
            }
            _ => return false,
        }
        true
    }

    fn key_press(&mut self, id: NodeId, text: String) {
        self.emit_if(
            id,
            EventKind::KeyPress,
            EventPayload::Map(vec![("key", PayloadValue::Str(text))]),
        );
    }

    fn text_key(&mut self, id: NodeId, key: KeyEvent) -> bool {
        let Some(node) = self.node(id) else {
            return false;
        };
        if !node
            .text
            .as_ref()
            .is_some_and(|(text, _, _)| text.is_selectable())
        {
            return false;
        }
        let command = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
        let extend = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('c' | 'C') if command => {
                let text = node
                    .editor
                    .selected()
                    .chars()
                    .filter(|c| {
                        *c != nsh_core::ATTACHMENT_CHAR
                            && (!c.is_control() || matches!(*c, '\n' | '\t'))
                    })
                    .collect();
                self.write_clipboard(text);
            }
            KeyCode::Char('a' | 'A') if command => {
                self.node_mut(id).unwrap().editor.select_all();
            }
            KeyCode::Left | KeyCode::Right => {
                self.node_mut(id).unwrap().editor.horizontal(
                    key.code == KeyCode::Right,
                    command || key.modifiers.contains(KeyModifiers::ALT),
                    extend,
                );
            }
            KeyCode::Home | KeyCode::End => {
                self.node_mut(id).unwrap().editor.home_end(
                    key.code == KeyCode::End,
                    command,
                    extend,
                );
            }
            KeyCode::Esc => {
                let cursor = node.editor.selection().1;
                self.node_mut(id)
                    .unwrap()
                    .editor
                    .set_selection(cursor, cursor);
                self.set_focus(None);
            }
            _ => return false,
        }
        true
    }

    fn activate(&mut self, id: NodeId) {
        self.set_pressed(Some(id));
        self.emit_if(id, EventKind::PressIn, EventPayload::None);
        self.emit_if(id, EventKind::Press, EventPayload::None);
        self.emit_if(id, EventKind::PressOut, EventPayload::None);
        self.set_pressed(None);
    }

    fn dialog_key(&mut self, key: KeyEvent) -> bool {
        let Some(dialog) = self.dialog.as_ref() else {
            return false;
        };
        let len = dialog.buttons.len().max(1);
        match key.code {
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.dialog_selection = (self.dialog_selection + len - 1) % len
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Down => {
                self.dialog_selection = (self.dialog_selection + 1) % len
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Up => {
                self.dialog_selection = (self.dialog_selection + len - 1) % len
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.answer_dialog(self.dialog_selection as i32),
            KeyCode::Char(c) if ('1'..='9').contains(&c) && (c as usize - '1' as usize) < len => {
                self.answer_dialog(c as i32 - '1' as i32)
            }
            KeyCode::Esc if dialog.cancelable => self.answer_dialog(-1),
            _ => {}
        }
        true
    }

    fn answer_dialog(&mut self, index: i32) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        self.dialog_selection = 0;
        self.emit(
            ROOT_ID,
            EventKind::Dialog,
            EventPayload::Map(vec![
                ("id", PayloadValue::Number(dialog.id as f64)),
                ("index", PayloadValue::Number(index as f64)),
            ]),
        );
    }

    fn scroll_ancestor(&self, start: Option<NodeId>) -> Option<NodeId> {
        let visible = self.visible_ids();
        let ancestors = start.map(|id| self.ancestors(id)).unwrap_or_default();
        ancestors
            .into_iter()
            .find(|id| {
                visible.contains(id)
                    && self
                        .node(*id)
                        .is_some_and(|n| n.kind == ViewKind::Scroll && n.scroll.enabled)
            })
            .or_else(|| {
                visible.into_iter().find(|id| {
                    self.node(*id)
                        .is_some_and(|n| n.kind == ViewKind::Scroll && n.scroll.enabled)
                })
            })
    }

    fn scroll_key(&mut self, key: KeyEvent) -> bool {
        if key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return false;
        }
        let Some(id) = self.scroll_ancestor(self.focus.or(self.hovered)) else {
            return false;
        };
        let Some(node) = self.node(id) else {
            return false;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('+' | '=' | '-' | '0'))
        {
            if !node.scroll.behavior.zooms() || !node.scroll.behavior.pinch {
                return false;
            }
            let Some(matrix) = self.local_to_surface(id) else {
                return false;
            };
            let Some(center) =
                crate::geometry::project(&matrix, node.frame.width * 0.5, node.frame.height * 0.5)
            else {
                return false;
            };
            if key.code == KeyCode::Char('0') {
                self.zoom_scroll(id, 1.0, None, true);
                return true;
            }
            return self.zoom_scroll_at(
                id,
                if key.code == KeyCode::Char('-') {
                    1.0 / 1.1
                } else {
                    1.1
                },
                center.0,
                center.1,
            );
        }
        let (x, y) = (node.scroll.x, node.scroll.y);
        let (width, height) = (node.frame.width, node.frame.height);
        let (content_w, content_h) = (
            node.scroll.content.width * node.scroll.physics.zoom,
            node.scroll.content.height * node.scroll.physics.zoom,
        );
        let horizontal = node.scroll.horizontal;
        let step_x = self.metrics.width() * 3.0 * if node.scroll.mirrored { -1.0 } else { 1.0 };
        let step_y = self.metrics.height();
        let (x, y) = match key.code {
            KeyCode::Up => (x, y - step_y),
            KeyCode::Down => (x, y + step_y),
            KeyCode::Left => (x - step_x, y),
            KeyCode::Right => (x + step_x, y),
            KeyCode::PageUp if horizontal => (x - width, y),
            KeyCode::PageDown if horizontal => (x + width, y),
            KeyCode::PageUp => (x, y - height),
            KeyCode::PageDown => (x, y + height),
            KeyCode::Home if horizontal => (0.0, y),
            KeyCode::End if horizontal => (content_w, y),
            KeyCode::Home => (x, 0.0),
            KeyCode::End => (x, content_h),
            _ => return false,
        };
        self.user_scroll(id, x, y);
        true
    }

    fn user_scroll(&mut self, id: NodeId, mut x: f32, mut y: f32) {
        if let Some(node) = self.node(id) {
            if node.scroll.horizontal {
                x = node
                    .scroll
                    .behavior
                    .snap_target(
                        node.scroll.x,
                        x,
                        (x - node.scroll.x).signum(),
                        node.frame.width,
                        node.scroll.content.width * node.scroll.physics.zoom,
                    )
                    .unwrap_or(x);
            } else {
                y = node
                    .scroll
                    .behavior
                    .snap_target(
                        node.scroll.y,
                        y,
                        (y - node.scroll.y).signum(),
                        node.frame.height,
                        node.scroll.content.height * node.scroll.physics.zoom,
                    )
                    .unwrap_or(y);
            }
        }
        self.scroll_to(id, x, y);
        if let Some(node) = self.node(id) {
            let payload = EventPayload::Scroll {
                x: node.scroll.x,
                y: node.scroll.y,
            };
            self.emit_if(id, EventKind::ScrollEnd, payload);
        }
    }

    fn mouse_event(&mut self, mouse: MouseEvent) -> bool {
        let x = (mouse.column as f32 + 0.5) * self.metrics.width();
        let y = (mouse.row as f32 + 0.5) * self.metrics.height();
        if self.dialog.is_some() {
            return self.dialog_mouse(mouse);
        }
        let hit = self.hit_test(x, y);
        self.set_hover(hit);
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) && self.navigation_click(x, y) {
            return true;
        }
        if let Some((id, button)) = self
            .pointer
            .as_ref()
            .and_then(|pointer| pointer.scroll.map(|id| (id, pointer.button)))
        {
            match mouse.kind {
                MouseEventKind::Drag(pressed) if pressed == button => {
                    self.move_scroll_drag(id, x, y);
                    if let Some(pointer) = &mut self.pointer {
                        pointer.last = (x, y);
                    }
                    return true;
                }
                MouseEventKind::Up(released) if released == button => {
                    if self
                        .pointer
                        .as_ref()
                        .is_some_and(|pointer| pointer.last != (x, y))
                    {
                        self.move_scroll_drag(id, x, y);
                    }
                    self.end_scroll_drag(id, false);
                    self.pointer = None;
                    return true;
                }
                MouseEventKind::Down(_) => self.cancel_pointer(),
                _ => {}
            }
        }
        if let MouseEventKind::Down(button) = mouse.kind
            && let Some(id) = self.scroll_ancestor(hit)
            && (button == MouseButton::Middle
                || (button == MouseButton::Left && self.scrollbar_hit(id, x, y)))
        {
            self.cancel_pointer();
            if self.begin_scroll_drag(id, x, y, button == MouseButton::Left) {
                self.pointer = Some(Pointer {
                    target: id,
                    pressable: None,
                    origin: (x, y),
                    last: (x, y),
                    started: Instant::now(),
                    moved: true,
                    long_fired: false,
                    pan_candidate: None,
                    pan: None,
                    pan_base: (0.0, 0.0),
                    native: None,
                    link: None,
                    scroll: Some(id),
                    button,
                    navigation: false,
                    navigation_claimed: false,
                });
                return true;
            }
        }
        let native = self.pointer.as_ref().and_then(|p| p.native).or_else(|| {
            hit.and_then(|id| {
                self.ancestors(id)
                    .into_iter()
                    .find(|id| self.node(*id).is_some_and(|n| n.native.is_some()))
            })
        });
        if let Some(id) = native {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) && self.focusable(id) {
                self.set_focus(Some(id));
            }
            let captured = self.pointer.as_ref().is_some_and(|p| p.native == Some(id));
            let handled = self.native_event(id, &Event::Mouse(mouse));
            if handled && mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                self.cancel_pointer();
                self.pointer = Some(Pointer {
                    target: id,
                    pressable: None,
                    origin: (x, y),
                    last: (x, y),
                    started: Instant::now(),
                    moved: false,
                    long_fired: false,
                    pan_candidate: None,
                    pan: None,
                    pan_base: (0.0, 0.0),
                    native: Some(id),
                    link: None,
                    scroll: None,
                    button: MouseButton::Left,
                    navigation: false,
                    navigation_claimed: false,
                });
            }
            if captured && mouse.kind == MouseEventKind::Up(MouseButton::Left) {
                self.pointer = None;
            }
            if handled || captured {
                return true;
            }
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.cancel_pointer();
                let Some(target) = hit else {
                    self.set_focus(None);
                    return false;
                };
                let path = self.ancestors(target);
                let link = path
                    .iter()
                    .copied()
                    .find_map(|id| self.text_link_at(id, x, y));
                let pressable =
                    link.or_else(|| path.iter().copied().find(|id| self.pressable(*id)));
                let field = path.iter().copied().find(|id| {
                    self.node(*id).is_some_and(|n| {
                        n.kind == ViewKind::Input
                            || n.text
                                .as_ref()
                                .is_some_and(|(text, _, _)| text.is_selectable())
                    })
                });
                if let Some(id) = field {
                    self.set_focus(Some(id));
                    self.drag_input = Some(id);
                    self.place_caret(id, x, y, mouse.modifiers.contains(KeyModifiers::SHIFT));
                } else if let Some(id) = pressable {
                    self.set_focus(Some(id));
                } else if let Some(id) = path
                    .iter()
                    .copied()
                    .find(|id| self.node(*id).is_some_and(|node| node.media.is_some()))
                {
                    self.set_focus(Some(id));
                } else {
                    self.set_focus(self.scroll_ancestor(Some(target)));
                }
                let pan_candidate = path.into_iter().find(|id| {
                    self.node(*id).is_some_and(|n| {
                        n.gestures.pan_x.is_some()
                            || n.gestures.pan_y.is_some()
                            || n.events.contains(EventKind::PanStart)
                            || n.events.contains(EventKind::PanEnd)
                            || n.props.pan_activation.is_some()
                    })
                });
                let pan_base = pan_candidate
                    .and_then(|id| self.node(id))
                    .map_or((0.0, 0.0), |n| {
                        (
                            n.gestures
                                .pan_x
                                .and_then(|id| self.animation.value(id))
                                .unwrap_or(0.0),
                            n.gestures
                                .pan_y
                                .and_then(|id| self.animation.value(id))
                                .unwrap_or(0.0),
                        )
                    });
                let navigation = self.navigation_pointer_down(x, y);
                self.pointer = Some(Pointer {
                    target,
                    pressable,
                    origin: (x, y),
                    last: (x, y),
                    started: Instant::now(),
                    moved: false,
                    long_fired: false,
                    pan_candidate,
                    pan: None,
                    pan_base,
                    native: None,
                    link,
                    scroll: None,
                    button: MouseButton::Left,
                    navigation,
                    navigation_claimed: false,
                });
                self.set_pressed(pressable);
                if let Some(id) = pressable {
                    self.emit_if(id, EventKind::PressIn, EventPayload::None);
                }
                self.raw_touch(TouchPhase::Start, x, y, None);
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.pointer.is_none() {
                    return false;
                }
                if self
                    .pointer
                    .as_ref()
                    .is_some_and(|pointer| pointer.navigation)
                    && self.navigation_pointer_drag(x, y)
                {
                    if !self
                        .pointer
                        .as_ref()
                        .is_some_and(|pointer| pointer.navigation_claimed)
                    {
                        self.raw_touch(TouchPhase::Cancel, x, y, None);
                        if let Some(id) =
                            self.pointer.as_ref().and_then(|pointer| pointer.pressable)
                        {
                            self.emit_if(id, EventKind::PressOut, EventPayload::None);
                        }
                        self.set_pressed(None);
                        self.drag_input = None;
                    }
                    if let Some(pointer) = &mut self.pointer {
                        pointer.navigation_claimed = true;
                        pointer.moved = true;
                        pointer.pressable = None;
                        pointer.last = (x, y);
                    }
                    return true;
                }
                if let Some(id) = self.drag_input {
                    self.place_caret(id, x, y, true);
                }
                self.drag_pointer(x, y);
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self
                    .pointer
                    .as_ref()
                    .is_some_and(|pointer| pointer.navigation)
                    && self.navigation_pointer_up(x, y)
                {
                    self.pointer = None;
                    self.drag_input = None;
                    self.set_pressed(None);
                    return true;
                }
                self.input_tick();
                if self.pointer.is_none() {
                    return false;
                }
                self.raw_touch(TouchPhase::End, x, y, None);
                let pointer = self.pointer.take().unwrap();
                if let Some(id) = pointer.pan {
                    self.pan_event(id, EventKind::PanEnd, &pointer, x, y);
                }
                if let Some(id) = pointer.pressable {
                    self.emit_if(id, EventKind::PressOut, EventPayload::None);
                    let released_inside = pointer.link.map_or_else(
                        || self.ancestors(hit.unwrap_or(ROOT_ID)).contains(&id),
                        |link| {
                            hit.is_some_and(|hit| {
                                self.ancestors(hit)
                                    .into_iter()
                                    .any(|id| self.text_link_at(id, x, y) == Some(link))
                            })
                        },
                    );
                    if !pointer.moved && !pointer.long_fired && released_inside {
                        self.emit_if(id, EventKind::Press, EventPayload::None);
                    }
                }
                self.set_pressed(None);
                self.drag_input = None;
                true
            }
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {
                if !mouse.modifiers.contains(KeyModifiers::CONTROL)
                    && let Some(media) = hit.and_then(|id| {
                        self.ancestors(id)
                            .into_iter()
                            .find(|id| self.node(*id).is_some_and(|node| node.media.is_some()))
                    })
                    && matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    )
                    && self.media_scroll(
                        media,
                        if mouse.kind == MouseEventKind::ScrollUp {
                            -3
                        } else {
                            3
                        },
                    )
                {
                    return true;
                }
                let Some(id) = self.scroll_ancestor(hit.or(self.focus)) else {
                    return false;
                };
                if mouse.modifiers.contains(KeyModifiers::CONTROL) {
                    return self.zoom_scroll_at(
                        id,
                        if matches!(
                            mouse.kind,
                            MouseEventKind::ScrollUp | MouseEventKind::ScrollRight
                        ) {
                            1.1
                        } else {
                            1.0 / 1.1
                        },
                        x,
                        y,
                    );
                }
                let Some(node) = self.node(id) else {
                    return false;
                };
                let (mut x, mut y) = (node.scroll.x, node.scroll.y);
                let horizontal =
                    node.scroll.horizontal || mouse.modifiers.contains(KeyModifiers::SHIFT);
                let physical_step =
                    self.metrics.width() * 3.0 * if node.scroll.mirrored { -1.0 } else { 1.0 };
                match mouse.kind {
                    MouseEventKind::ScrollLeft => x -= physical_step,
                    MouseEventKind::ScrollRight => x += physical_step,
                    MouseEventKind::ScrollUp if horizontal => x -= self.metrics.width() * 3.0,
                    MouseEventKind::ScrollDown if horizontal => x += self.metrics.width() * 3.0,
                    MouseEventKind::ScrollUp => y -= self.metrics.height() * 3.0,
                    MouseEventKind::ScrollDown => y += self.metrics.height() * 3.0,
                    _ => {}
                }
                self.user_scroll(id, x, y);
                true
            }
            MouseEventKind::Moved => hit.is_some(),
            _ => false,
        }
    }

    fn dialog_mouse(&mut self, mouse: MouseEvent) -> bool {
        let Some(dialog) = self.dialog.as_ref() else {
            return false;
        };
        let (area, buttons) =
            crate::render::dialog_geometry(self.columns, self.rows, dialog, self.dialog_selection);
        if let Some(index) = buttons.iter().find_map(|(index, rect)| {
            rect.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
                .then_some(*index)
        }) {
            self.dialog_selection = index;
            if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
                self.answer_dialog(index as i32);
            }
        } else if dialog.cancelable
            && mouse.kind == MouseEventKind::Up(MouseButton::Left)
            && !area.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
        {
            self.answer_dialog(-1);
        }
        true
    }

    fn navigation_click(&mut self, x: f32, y: f32) -> bool {
        let ids = self.visible_ids();
        if let Some(modal) = ids.iter().rev().copied().find(|id| {
            self.node(*id).is_some_and(|node| {
                node.kind == ViewKind::Modal
                    && !node.modal.transparent
                    && node.modal.presentation != nsh_core::ModalPresentation::FullScreen
            })
        }) && let (Some(point), Some(frame)) = (
            self.surface_to_local(modal, x, y),
            self.presentation_frame(modal),
        ) && (point.0 < 0.0
            || point.1 < 0.0
            || point.0 >= frame.width
            || point.1 >= frame.height)
        {
            self.cancel_pointer();
            self.navigation_request_close(modal);
            return true;
        }
        for id in ids.into_iter().rev() {
            let Some(node) = self.node(id) else { continue };
            if node.kind != ViewKind::Screen
                || !node.screen.header_shown
                || !node.screen.back_visible
                || node.screen.left.is_some()
                || !self.navigation_can_back(id)
            {
                continue;
            }
            let Some((x, y)) = self.surface_to_local(id, x, y) else {
                continue;
            };
            let width = self.navigation_back_title(id).map_or(4, |title| {
                unicode_width::UnicodeWidthStr::width(title.as_ref())
            });
            let end = (width + 3).min(19) as f32 * self.metrics.width();
            if x >= self.metrics.width()
                && x < end
                && y >= 0.0
                && y < self.navigation_header(id).bar_height
            {
                self.cancel_pointer();
                self.back();
                return true;
            }
        }
        false
    }

    fn place_caret(&mut self, id: NodeId, x: f32, y: f32, extend: bool) {
        if self.node(id).is_some_and(|n| n.kind == ViewKind::Text) {
            if let Some(offset) = self.text_offset_at(id, x, y)
                && let Some(node) = self.node_mut(id)
            {
                node.editor.move_to(offset, extend);
            }
            return;
        }
        let (Some(node), Some(geometry)) = (self.node(id), self.input_hit_geometry(id)) else {
            return;
        };
        let (x, y) = if self.composite_ancestor(id) {
            let Some(point) = self.surface_to_local(id, x, y) else {
                return;
            };
            point
        } else {
            (x, y)
        };
        let cols = geometry.columns.max(1);
        let rows = geometry.rows.max(1);
        let columns = if node.input.multiline {
            cols
        } else {
            usize::MAX
        };
        let (origin_row, origin_col) =
            crate::render::input_scroll(node, self.focus == Some(id), cols, rows);
        let row = ((y / self.metrics.height()).floor() as i64 - geometry.row).max(0) as usize
            + origin_row;
        let col = ((x / self.metrics.width()).floor() as i64 - geometry.column).max(0) as usize
            + origin_col;
        let offsets = crate::render::input_line_offsets(node, self.focus == Some(id), cols);
        let col = col.saturating_sub(offsets.get(row).copied().unwrap_or(0));
        let offset = node.editor.offset_at(row, col, columns, node.input.secure);
        self.edit_input(id, |editor, _| {
            editor.move_to(offset, extend);
            false
        });
    }

    fn input_hit_geometry(&self, id: NodeId) -> Option<crate::render::InputGeometry> {
        let node = self.node(id)?;
        if self.composite_ancestor(id) {
            let mut frame = self.presentation_frame(id)?;
            frame.x = 0.0;
            frame.y = 0.0;
            let area = ratatui::layout::Rect::new(
                0,
                0,
                (frame.width / self.metrics.width())
                    .ceil()
                    .clamp(1.0, u16::MAX as f32) as u16,
                (frame.height / self.metrics.height())
                    .ceil()
                    .clamp(1.0, u16::MAX as f32) as u16,
            );
            Some(crate::render::input_geometry(
                node,
                frame,
                self.metrics,
                area,
            ))
        } else {
            Some(crate::render::input_geometry(
                node,
                self.global_frame(id)?,
                self.metrics,
                ratatui::layout::Rect::new(0, 0, self.columns, self.rows),
            ))
        }
    }

    fn text_link_at(&self, id: NodeId, x: f32, y: f32) -> Option<NodeId> {
        let (run, x, y) = self.text_run_at(id, x, y)?;
        if x < run.x
            || x >= run.x + run.width as f32 * self.metrics.width()
            || y < run.y
            || y >= run.y + self.metrics.height()
        {
            return None;
        }
        let (text, _, _) = self.node(id)?.text.as_ref()?;
        text.link_at(run.start).map(|link| link.node)
    }

    fn text_offset_at(&self, id: NodeId, x: f32, y: f32) -> Option<u32> {
        let (run, x, _) = self.text_run_at(id, x, y)?;
        let (text, _, _) = self.node(id)?.text.as_ref()?;
        let right = x >= run.x + run.width as f32 * self.metrics.width() * 0.5;
        let rtl = text.writing_direction.unwrap_or(text.rtl);
        Some(if right != rtl { run.end } else { run.start })
    }

    fn text_run_at(
        &self,
        id: NodeId,
        x: f32,
        y: f32,
    ) -> Option<(&crate::text::TerminalRun, f32, f32)> {
        let node = self.node(id)?;
        let (_, layout, content) = node.text.as_ref()?;
        let layout = layout.downcast::<crate::text::TerminalLayout>()?;
        let (frame, x, y) = if self.composite_ancestor(id) {
            let (x, y) = self.surface_to_local(id, x, y)?;
            let mut frame = self.presentation_frame(id)?;
            frame.x = 0.0;
            frame.y = 0.0;
            (frame, x, y)
        } else {
            (self.global_frame(id)?, x, y)
        };
        let sx = if node.frame.width > 0.0 {
            frame.width / node.frame.width
        } else {
            1.0
        };
        let sy = if node.frame.height > 0.0 {
            frame.height / node.frame.height
        } else {
            1.0
        };
        let origin = (frame.x + content.x * sx, frame.y + content.y * sy);
        let column = (x / self.metrics.width()).floor();
        let row = (y / self.metrics.height()).floor();
        let run_position = |run: &crate::text::TerminalRun| {
            (
                ((origin.0 + run.x) / self.metrics.width()).floor(),
                ((origin.1 + run.y) / self.metrics.height()).floor(),
            )
        };
        let run = layout.runs.iter().min_by(|a, b| {
            let distance = |run: &crate::text::TerminalRun| {
                let (left, top) = run_position(run);
                let dy = (row - top).abs();
                let dx = (left - column)
                    .max(column - left - run.width.max(1) as f32 + 1.0)
                    .max(0.0);
                (dy, dx)
            };
            let (ay, ax) = distance(a);
            let (by, bx) = distance(b);
            ay.total_cmp(&by).then_with(|| ax.total_cmp(&bx))
        })?;
        let (left, top) = run_position(run);
        Some((
            run,
            run.x + (column - left + 0.5) * self.metrics.width(),
            run.y + (row - top + 0.5) * self.metrics.height(),
        ))
    }

    fn set_pressed(&mut self, pressed: Option<NodeId>) {
        if self.pressed == pressed {
            return;
        }
        let previous = self.pressed;
        let hover = self.hovered_path.clone();
        self.pressed = pressed;
        self.report_states(self.focus, previous, &hover);
    }

    fn set_hover(&mut self, target: Option<NodeId>) {
        let next: Vec<_> = target
            .map(|id| self.ancestors(id))
            .unwrap_or_default()
            .into_iter()
            .filter(|id| {
                self.node(*id).is_some_and(|n| {
                    n.tracked & state::HOVERED != 0
                        || n.events.contains(EventKind::HoverIn)
                        || n.events.contains(EventKind::HoverOut)
                })
            })
            .collect();
        let previous = std::mem::replace(&mut self.hovered_path, next);
        self.hovered = target;
        for id in &previous {
            if !self.hovered_path.contains(id) {
                self.emit_if(*id, EventKind::HoverOut, EventPayload::None);
            }
        }
        let entering: Vec<_> = self
            .hovered_path
            .iter()
            .rev()
            .copied()
            .filter(|id| !previous.contains(id))
            .collect();
        for id in entering {
            self.emit_if(id, EventKind::HoverIn, EventPayload::None);
        }
        self.report_states(self.focus, self.pressed, &previous);
    }

    fn interaction_states(
        id: NodeId,
        focus: Option<NodeId>,
        focus_chain: &[NodeId],
        pressed: Option<NodeId>,
        hover: &[NodeId],
    ) -> u8 {
        let mut states = 0;
        if focus == Some(id) {
            states |= state::FOCUSED;
        }
        if focus_chain.contains(&id) {
            states |= state::FOCUS_WITHIN;
        }
        if pressed == Some(id) {
            states |= state::PRESSED;
        }
        if hover.contains(&id) {
            states |= state::HOVERED;
        }
        states
    }

    fn report_states(
        &mut self,
        old_focus: Option<NodeId>,
        old_pressed: Option<NodeId>,
        old_hover: &[NodeId],
    ) {
        let old_chain = old_focus.map(|id| self.ancestors(id)).unwrap_or_default();
        let new_chain = self.focus.map(|id| self.ancestors(id)).unwrap_or_default();
        let mut affected = old_chain.clone();
        affected.extend_from_slice(&new_chain);
        affected.extend_from_slice(old_hover);
        affected.extend_from_slice(&self.hovered_path);
        affected.extend(old_pressed);
        affected.extend(self.pressed);
        affected.sort_unstable();
        affected.dedup();
        for id in affected {
            let tracked = self.node(id).map_or(0, |n| n.tracked);
            let before =
                Self::interaction_states(id, old_focus, &old_chain, old_pressed, old_hover)
                    & tracked;
            let after = Self::interaction_states(
                id,
                self.focus,
                &new_chain,
                self.pressed,
                &self.hovered_path,
            ) & tracked;
            if before != after {
                self.events.push(SurfaceEvent::States(id, after));
            }
        }
    }

    /// A newly mounted state mask must receive the current state even
    /// when no pointer or focus transition followed the batch.
    pub(crate) fn refresh_input_states(&mut self, id: NodeId) {
        let chain = self.focus.map(|id| self.ancestors(id)).unwrap_or_default();
        let tracked = self.node(id).map_or(0, |n| n.tracked);
        if tracked != 0 {
            let states =
                Self::interaction_states(id, self.focus, &chain, self.pressed, &self.hovered_path)
                    & tracked;
            self.events.push(SurfaceEvent::States(id, states));
        }
    }

    fn raw_touch(&mut self, phase: TouchPhase, x: f32, y: f32, activated: Option<NodeId>) {
        let Some(pointer) = self.pointer.as_ref() else {
            return;
        };
        let path: Vec<_> = self
            .ancestors(pointer.target)
            .into_iter()
            .filter(|id| {
                self.node(*id)
                    .is_some_and(|n| n.events.contains(EventKind::Touch))
            })
            .collect();
        if path.is_empty() {
            return;
        }
        let location = self
            .surface_to_local(pointer.target, x, y)
            .unwrap_or_default();
        let point = TouchPoint {
            id: 0,
            target: pointer.target,
            path,
            page_x: x,
            page_y: y,
            location_x: location.0,
            location_y: location.1,
            changed: true,
        };
        let event = TouchEvent {
            phase,
            timestamp: pointer.started.elapsed().as_secs_f64() * 1000.0,
            points: vec![point],
            activated,
        };
        self.emit(
            ROOT_ID,
            EventKind::Touch,
            EventPayload::Touch(Box::new(event)),
        );
    }

    fn drag_pointer(&mut self, x: f32, y: f32) {
        let Some(mut pointer) = self.pointer.take() else {
            return;
        };
        let (dx, dy) = (x - pointer.origin.0, y - pointer.origin.1);
        if dx.hypot(dy) > self.metrics.width().min(self.metrics.height()) {
            pointer.moved = true;
            self.set_pressed(None);
        }
        let mut activated = None;
        if pointer.pan.is_none()
            && self.drag_input.is_none()
            && let Some(id) = pointer.pan_candidate
        {
            let (dx, dy) = self.pan_delta(id, &pointer, x, y);
            let decision = self.node(id).map_or(Some(false), |n| {
                n.props.pan_activation.map_or_else(
                    || {
                        if dx.hypot(dy) > nsh_core::PanActivation::MIN_DISTANCE {
                            Some(true)
                        } else {
                            None
                        }
                    },
                    |gate| gate.decide(dx, dy),
                )
            });
            match decision {
                Some(true) => {
                    pointer.pan = Some(id);
                    activated = Some(id);
                    self.pan_event(id, EventKind::PanStart, &pointer, x, y);
                }
                Some(false) => pointer.pan_candidate = None,
                None => {}
            }
        }
        if let Some(id) = pointer.pan {
            let (dx, dy) = self.pan_delta(id, &pointer, x, y);
            let gestures = self.node(id).map(|n| n.gestures);
            if let Some(gestures) = gestures {
                let mut finished = Vec::new();
                if let Some(value) = gestures.pan_x {
                    self.animation
                        .set(value, pointer.pan_base.0 + dx, &mut finished);
                }
                if let Some(value) = gestures.pan_y {
                    self.animation
                        .set(value, pointer.pan_base.1 + dy, &mut finished);
                }
                if !finished.is_empty() {
                    self.events.push(SurfaceEvent::AnimationsFinished(finished));
                }
            }
        }
        pointer.last = (x, y);
        self.pointer = Some(pointer);
        self.raw_touch(TouchPhase::Move, x, y, activated);
    }

    fn pan_event(&mut self, id: NodeId, kind: EventKind, pointer: &Pointer, x: f32, y: f32) {
        let elapsed = pointer.started.elapsed().as_secs_f32().max(0.001);
        let (dx, dy) = self.pan_delta(id, pointer, x, y);
        let gestures = self.node(id).map(|n| n.gestures).unwrap_or_default();
        self.emit_if(
            id,
            kind,
            EventPayload::Pan {
                translation_x: dx,
                translation_y: dy,
                velocity_x: dx / elapsed,
                velocity_y: dy / elapsed,
                value_x: gestures.pan_x.map(|_| pointer.pan_base.0 + dx),
                value_y: gestures.pan_y.map(|_| pointer.pan_base.1 + dy),
            },
        );
    }

    fn pan_delta(&self, id: NodeId, pointer: &Pointer, x: f32, y: f32) -> (f32, f32) {
        match (
            self.surface_to_local(id, pointer.origin.0, pointer.origin.1),
            self.surface_to_local(id, x, y),
        ) {
            (Some(from), Some(to)) => (to.0 - from.0, to.1 - from.1),
            _ => (0.0, 0.0),
        }
    }

    fn cancel_pointer(&mut self) {
        if let Some(pointer) = self.pointer.as_ref().filter(|pointer| {
            pointer.scroll.is_none() && pointer.native.is_none() && !pointer.navigation_claimed
        }) {
            let (x, y) = pointer.last;
            self.raw_touch(TouchPhase::Cancel, x, y, None);
        }
        if let Some(pointer) = self.pointer.take() {
            if pointer.navigation {
                self.navigation_pointer_cancel();
            }
            if let Some(id) = pointer.scroll {
                self.end_scroll_drag(id, true);
            }
            if let Some(id) = pointer.pressable {
                self.emit_if(id, EventKind::PressOut, EventPayload::None);
            }
            if let Some(id) = pointer.pan {
                self.pan_event(
                    id,
                    EventKind::PanEnd,
                    &pointer,
                    pointer.last.0,
                    pointer.last.1,
                );
            }
        }
        self.set_pressed(None);
        self.drag_input = None;
    }

    pub(crate) fn input_pending(&self) -> bool {
        self.pointer.as_ref().is_some_and(|pointer| {
            !pointer.moved
                && !pointer.long_fired
                && pointer.pressable.is_some_and(|id| {
                    self.node(id)
                        .is_some_and(|n| n.events.contains(EventKind::LongPress))
                })
        })
    }

    pub(crate) fn input_tick(&mut self) -> bool {
        let Some(pointer) = self.pointer.as_ref() else {
            return false;
        };
        let Some(id) = pointer.pressable else {
            return false;
        };
        if pointer.moved || pointer.long_fired {
            return false;
        }
        let Some(node) = self.node(id) else {
            return false;
        };
        if !node.events.contains(EventKind::LongPress)
            || pointer.started.elapsed().as_secs_f32() * 1000.0
                < node.props.long_press_delay.unwrap_or(500.0)
        {
            return false;
        }
        if let Some(pointer) = self.pointer.as_mut() {
            pointer.long_fired = true;
        }
        self.emit_if(id, EventKind::LongPress, EventPayload::None);
        true
    }
}

fn keystroke(event: KeyEvent) -> Option<Keystroke> {
    let mut modifiers = Modifiers {
        ctrl: event.modifiers.contains(KeyModifiers::CONTROL),
        alt: event.modifiers.contains(KeyModifiers::ALT),
        shift: event.modifiers.contains(KeyModifiers::SHIFT) || event.code == KeyCode::BackTab,
        cmd: event.modifiers.contains(KeyModifiers::SUPER),
    };
    let key = match event.code {
        KeyCode::Char(c) => {
            let shifted = "~!@#$%^&*()_+{}|:\"<>?";
            let bases = "`1234567890-=[]\\;',./";
            let base = shifted
                .chars()
                .position(|s| s == c)
                .and_then(|i| bases.chars().nth(i));
            if base.is_some() || c.is_ascii_uppercase() {
                modifiers.shift = true;
            }
            Key::Char(base.unwrap_or(c.to_ascii_lowercase()))
        }
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Escape,
        KeyCode::Tab | KeyCode::BackTab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Insert => Key::Insert,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::F(n) => Key::F(n),
        _ => return None,
    };
    Some(Keystroke { modifiers, key })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_shortcuts_normalize_shifted_printable_keys() {
        let stroke = keystroke(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)).unwrap();
        assert_eq!(stroke.key, Key::Char('/'));
        assert!(stroke.modifiers.shift);
        let stroke = keystroke(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::CONTROL)).unwrap();
        assert_eq!(stroke.key, Key::Char('k'));
        assert!(stroke.modifiers.ctrl && stroke.modifiers.shift);
        let stroke = keystroke(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)).unwrap();
        assert_eq!(stroke.key, Key::Tab);
        assert!(stroke.modifiers.shift);
    }

    fn mask(events: &[EventKind]) -> nsh_core::EventMask {
        let mut mask = nsh_core::EventMask::default();
        for kind in events {
            mask.set(*kind, true);
        }
        mask
    }

    fn create(id: NodeId, kind: ViewKind, parent: NodeId, y: f32) -> Vec<nsh_core::MountItem> {
        use nsh_core::{MountItem, Rect};
        vec![
            MountItem::Create { id, kind },
            MountItem::Frame {
                id,
                frame: Rect {
                    x: 0.0,
                    y,
                    width: 160.0,
                    height: 32.0,
                },
            },
            MountItem::Insert {
                parent,
                child: id,
                before: None,
            },
        ]
    }

    fn form() -> Surface {
        use nsh_core::{MountBatch, MountItem};
        let mut surface = Surface::new(40, 12, crate::CellMetrics::default());
        let mut items = create(2, ViewKind::Input, ROOT_ID, 0.0);
        items.push(MountItem::InputContent {
            id: 2,
            content: nsh_core::Rect {
                x: 0.0,
                y: 0.0,
                width: 160.0,
                height: 32.0,
            },
        });
        items.extend(create(4, ViewKind::View, ROOT_ID, 48.0));
        items.push(MountItem::Events {
            id: 4,
            events: mask(&[EventKind::Press, EventKind::PressIn, EventKind::PressOut]),
        });
        surface.apply(MountBatch { items });
        surface.events.clear();
        surface
    }

    fn key(surface: &mut Surface, code: KeyCode, modifiers: KeyModifiers) {
        surface.handle_event(Event::Key(KeyEvent::new(code, modifiers)));
    }

    #[test]
    fn terminal_form_edits_and_activates_buttons_with_keyboard() {
        let mut surface = form();
        assert_eq!(surface.focused(), Some(2));
        surface.handle_event(Event::Paste("中文😀".into()));
        assert_eq!(surface.node(2).unwrap().editor.text(), "中文😀");
        assert!(surface.events.iter().any(|event| matches!(event, SurfaceEvent::Event(2, EventKind::ChangeText, EventPayload::Text { text, count: 1 }) if text == "中文😀")));
        surface.input_value(2, "stale".into(), 0);
        assert_eq!(surface.node(2).unwrap().editor.text(), "中文😀");
        key(&mut surface, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(surface.focused(), Some(4));
        surface.events.clear();
        key(&mut surface, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            surface
                .events
                .iter()
                .filter(|event| matches!(event, SurfaceEvent::Event(4, EventKind::Press, _)))
                .count(),
            1
        );
        let repeat =
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat);
        surface.handle_event(Event::Key(repeat));
        assert_eq!(
            surface
                .events
                .iter()
                .filter(|event| matches!(event, SurfaceEvent::Event(4, EventKind::Press, _)))
                .count(),
            1
        );
        key(&mut surface, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(surface.node(2).unwrap().editor.selected(), "中文😀");
    }

    #[test]
    fn modal_focus_scope_traps_tab_and_restores_background_focus() {
        use nsh_core::{MountBatch, MountItem};
        let mut surface = form();
        surface.set_focus(Some(4));
        let mut items = create(6, ViewKind::Modal, ROOT_ID, 0.0);
        items.extend(create(8, ViewKind::Input, 6, 0.0));
        surface.apply(MountBatch { items });
        assert_eq!(surface.focused(), Some(8));
        key(&mut surface, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(surface.focused(), Some(8));
        surface.set_focus(Some(4));
        assert_eq!(surface.focused(), None);
        surface.set_focus(Some(8));
        surface.apply(MountBatch {
            items: vec![MountItem::Remove { id: 6 }],
        });
        assert_eq!(surface.focused(), None);
        surface.tick(1.0);
        assert_eq!(surface.focused(), Some(4));
    }

    #[test]
    fn keymap_respects_editing_and_always_bindings() {
        use std::sync::Arc;

        use nsh_core::keymap::{KeyBinding, Keymap, When};
        let mut surface = form();
        let stroke = Keystroke::parse_with("ctrl+k", false).unwrap();
        surface.keymap = Arc::new(Keymap::new(vec![KeyBinding::new(
            99,
            stroke,
            When::Always,
            false,
        )]));
        key(&mut surface, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert!(surface.events.iter().any(|event| matches!(
            event,
            SurfaceEvent::Event(
                ROOT_ID,
                EventKind::KeyCommand,
                EventPayload::Value(PayloadValue::Number(99.0))
            )
        )));
        assert_eq!(surface.node(2).unwrap().editor.count(), 0);
        surface.keymap = Arc::new(Keymap::new(vec![KeyBinding::new(
            88,
            Keystroke::parse_with("a", false).unwrap(),
            When::Auto,
            false,
        )]));
        key(&mut surface, KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(surface.node(2).unwrap().editor.text(), "a");
        surface.keymap = Arc::new(Keymap::new(vec![KeyBinding::new(
            100,
            Keystroke::parse_with("ctrl+shift+q", false).unwrap(),
            When::Always,
            false,
        )]));
        key(
            &mut surface,
            KeyCode::Char('Q'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert!(!surface.should_quit());
        assert!(surface.events.iter().any(|event| matches!(
            event,
            SurfaceEvent::Event(
                ROOT_ID,
                EventKind::KeyCommand,
                EventPayload::Value(PayloadValue::Number(100.0))
            )
        )));
    }

    #[test]
    fn input_return_obeys_submit_and_multiline_rules() {
        let mut surface = form();
        let mut config = nsh_core::InputConfig {
            multiline: true,
            submit_behavior: Some(SubmitBehavior::Submit),
            ..Default::default()
        };
        surface.node_mut(2).unwrap().input = config.clone();
        surface.node_mut(2).unwrap().events = mask(&[EventKind::SubmitEditing]);
        surface.handle_event(Event::Paste("first".into()));
        key(&mut surface, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(surface.node(2).unwrap().editor.text(), "first");
        assert_eq!(surface.focused(), Some(2));
        assert!(
            surface
                .events
                .iter()
                .any(|event| matches!(event, SurfaceEvent::Event(2, EventKind::SubmitEditing, _)))
        );
        key(&mut surface, KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(surface.node(2).unwrap().editor.text(), "first\n");
        config.submit_behavior = Some(SubmitBehavior::BlurAndSubmit);
        surface.node_mut(2).unwrap().input = config;
        key(&mut surface, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(surface.focused(), None);
        surface.reconcile_focus();
        assert_eq!(surface.focused(), None);
    }

    #[test]
    fn disabled_controls_and_pointer_none_subtrees_are_skipped() {
        let mut surface = form();
        surface
            .node_mut(4)
            .unwrap()
            .props
            .accessibility
            .state
            .disabled = true;
        key(&mut surface, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(surface.focused(), Some(2));
        surface.node_mut(ROOT_ID).unwrap().props.pointer_events = PointerEvents::None;
        surface.reconcile_focus();
        assert_eq!(surface.focused(), None);
    }

    #[test]
    fn dialog_blocks_form_and_honors_cancelable() {
        use nsh_core::{Dialog, DialogButton};
        let mut surface = form();
        surface.dialog = Some(Dialog {
            id: 12,
            title: Some("Choose".into()),
            message: None,
            buttons: vec![
                DialogButton {
                    text: "First".into(),
                    style: Default::default(),
                },
                DialogButton {
                    text: "Second".into(),
                    style: Default::default(),
                },
            ],
            cancelable: false,
            sheet: false,
        });
        key(&mut surface, KeyCode::Esc, KeyModifiers::NONE);
        assert!(surface.dialog.is_some());
        surface.handle_event(Event::Paste("blocked".into()));
        assert_eq!(surface.node(2).unwrap().editor.text(), "");
        key(&mut surface, KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(surface.dialog_selection, 1);
        key(&mut surface, KeyCode::Enter, KeyModifiers::NONE);
        assert!(surface.dialog.is_none());
        assert!(surface.events.iter().any(|event| matches!(event, SurfaceEvent::Event(ROOT_ID, EventKind::Dialog, EventPayload::Map(fields)) if fields.contains(&("index", PayloadValue::Number(1.0))))));
    }

    #[test]
    fn wheel_scrolls_native_state_and_preserves_core_adjustment_count() {
        use nsh_core::{MountBatch, MountItem, Size};
        let mut surface = Surface::new(40, 12, crate::CellMetrics::default());
        let mut items = create(2, ViewKind::Scroll, ROOT_ID, 0.0);
        items.push(MountItem::ScrollContent {
            id: 2,
            size: Size::new(160.0, 1000.0),
        });
        surface.apply(MountBatch { items });
        surface.node_mut(2).unwrap().scroll.count = 3;
        surface.events.clear();
        surface.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(surface.node(2).unwrap().scroll.y, 48.0);
        assert!(
            surface
                .events
                .iter()
                .any(|event| matches!(event, SurfaceEvent::Scroll(2, 0.0, 48.0, 3)))
        );
        let scroll = &mut surface.node_mut(2).unwrap().scroll;
        scroll.content.width = 1000.0;
        scroll.horizontal = true;
        scroll.mirrored = true;
        scroll.x = 100.0;
        let wheel = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 2,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        surface.handle_event(wheel(MouseEventKind::ScrollRight));
        assert_eq!(surface.node(2).unwrap().scroll.x, 76.0);
        surface.handle_event(wheel(MouseEventKind::ScrollDown));
        assert_eq!(surface.node(2).unwrap().scroll.x, 100.0);
        key(&mut surface, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(surface.node(2).unwrap().scroll.x, 124.0);
        key(&mut surface, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(surface.node(2).unwrap().scroll.x, 0.0);
        key(&mut surface, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(surface.node(2).unwrap().scroll.x, 840.0);
    }

    #[test]
    fn password_copy_and_paste_do_not_expose_secret_to_clipboard() {
        let mut surface = form();
        surface.node_mut(2).unwrap().input.secure = true;
        surface.handle_event(Event::Paste("secret".into()));
        surface.node_mut(2).unwrap().editor.select_all();
        surface.clipboard = "public".into();
        key(&mut surface, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(surface.clipboard, "public");
        assert!(!surface.should_quit());
    }

    #[test]
    fn selectable_text_uses_shaped_runs_for_drag_copy_and_inline_links() {
        use std::sync::Arc;

        use nsh_core::{
            AttributedText, MountBatch, MountItem, Rect, ResolvedTextStyle, Size, TextEngine,
            TextLink,
        };
        let mut surface = Surface::new(40, 12, crate::CellMetrics::default());
        let mut text = AttributedText {
            selectable: Some(true),
            ..Default::default()
        };
        text.push("Hello中", ResolvedTextStyle::default());
        text.links.push(TextLink {
            start: 2,
            end: 5,
            node: 10,
            events: mask(&[EventKind::Press]),
        });
        let engine = crate::TerminalTextEngine::default();
        let prepared = engine.prepare(&text);
        let layout = engine.layout(&prepared, Size::new(160.0, 32.0));
        let mut items = create(2, ViewKind::Text, ROOT_ID, 0.0);
        items.push(MountItem::Text {
            id: 2,
            text: Arc::new(text),
            layout,
            content: Rect {
                x: 0.0,
                y: 0.0,
                width: 160.0,
                height: 32.0,
            },
        });
        surface.apply(MountBatch { items });
        let mouse = |kind, column| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        surface.handle_event(mouse(MouseEventKind::Down(MouseButton::Left), 1));
        surface.handle_event(mouse(MouseEventKind::Drag(MouseButton::Left), 4));
        surface.handle_event(mouse(MouseEventKind::Up(MouseButton::Left), 4));
        assert_eq!(surface.node(2).unwrap().editor.selected(), "llo");
        key(&mut surface, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(surface.clipboard, "llo");
        surface.events.clear();
        surface.handle_event(mouse(MouseEventKind::Down(MouseButton::Left), 3));
        surface.handle_event(mouse(MouseEventKind::Up(MouseButton::Left), 3));
        assert!(
            surface
                .events
                .iter()
                .any(|event| matches!(event, SurfaceEvent::Event(10, EventKind::Press, _)))
        );
    }

    #[test]
    fn native_long_press_suppresses_click_and_drag_reports_raw_touches_and_pan() {
        use nsh_core::{GestureValues, MountBatch, MountItem};
        let mut surface = form();
        surface.node_mut(4).unwrap().events = mask(&[
            EventKind::Press,
            EventKind::LongPress,
            EventKind::Touch,
            EventKind::PanStart,
            EventKind::PanEnd,
        ]);
        surface.node_mut(4).unwrap().props.long_press_delay = Some(0.0);
        let mouse = |kind, column| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row: 3,
                modifiers: KeyModifiers::NONE,
            })
        };
        surface.handle_event(mouse(MouseEventKind::Down(MouseButton::Left), 1));
        assert!(surface.input_tick());
        assert!(!surface.input_pending());
        surface.handle_event(mouse(MouseEventKind::Up(MouseButton::Left), 1));
        assert!(
            surface
                .events
                .iter()
                .any(|event| matches!(event, SurfaceEvent::Event(4, EventKind::LongPress, _)))
        );
        assert!(
            !surface
                .events
                .iter()
                .any(|event| matches!(event, SurfaceEvent::Event(4, EventKind::Press, _)))
        );
        surface.apply(MountBatch {
            items: vec![
                MountItem::Animation(nsh_animation::AnimationOp::CreateValue {
                    node: 11,
                    value: 5.0,
                }),
                MountItem::Gestures {
                    id: 4,
                    values: GestureValues {
                        pan_x: Some(11),
                        ..Default::default()
                    },
                },
            ],
        });
        surface.events.clear();
        surface.handle_event(mouse(MouseEventKind::Down(MouseButton::Left), 1));
        surface.handle_event(mouse(MouseEventKind::Drag(MouseButton::Left), 5));
        assert_eq!(surface.animation.value(11), Some(37.0));
        surface.handle_event(mouse(MouseEventKind::Up(MouseButton::Left), 5));
        assert!(
            surface
                .events
                .iter()
                .any(|event| matches!(event, SurfaceEvent::Event(4, EventKind::PanStart, _)))
        );
        assert!(
            surface
                .events
                .iter()
                .any(|event| matches!(event, SurfaceEvent::Event(4, EventKind::PanEnd, _)))
        );
        assert!(surface.events.iter().any(|event| matches!(event,SurfaceEvent::Event(ROOT_ID,EventKind::Touch,EventPayload::Touch(touch)) if touch.phase == TouchPhase::Move && touch.activated == Some(4))));
    }

    #[test]
    fn host_edits_guard_generation_graphemes_focus_and_secure_context() {
        let mut surface = form();
        surface.handle_event(Event::Paste("a😀b".into()));
        let context = surface.input_context().unwrap();
        assert_eq!(context.text.as_deref(), Some("a😀b"));
        assert!(!surface.apply_input_edit(InputEdit {
            node: 2,
            expected_event_count: context.event_count,
            selection: 1..2,
            replacement: "split".into()
        }));
        assert!(surface.apply_input_edit(InputEdit {
            node: 2,
            expected_event_count: context.event_count,
            selection: 1..3,
            replacement: "中".into()
        }));
        assert_eq!(surface.node(2).unwrap().editor.text(), "a中b");
        assert!(!surface.apply_input_edit(InputEdit {
            node: 2,
            expected_event_count: context.event_count,
            selection: 0..1,
            replacement: "stale".into()
        }));
        surface.node_mut(2).unwrap().input.max_length = Some(3);
        let current = surface.input_context().unwrap();
        assert!(!surface.apply_input_edit(InputEdit {
            node: 2,
            expected_event_count: current.event_count,
            selection: 0..1,
            replacement: "😀".into()
        }));
        assert_eq!(
            surface.input_context().unwrap().selection,
            current.selection
        );
        surface.node_mut(2).unwrap().input.secure = true;
        assert_eq!(surface.input_context().unwrap().text, None);
        let count = surface.node(2).unwrap().editor.count();
        surface.set_focus(Some(4));
        assert!(!surface.apply_input_edit(InputEdit {
            node: 2,
            expected_event_count: count,
            selection: 0..3,
            replacement: "background".into()
        }));
        surface.set_focus(Some(2));
        surface.selection(2, 3, 3);
        let input = &mut surface.node_mut(2).unwrap().input;
        input.secure = false;
        input.auto_capitalize = nsh_core::AutoCapitalize::Characters;
        input.max_length = Some(5);
        assert!(surface.type_text("xy"));
        assert_eq!(surface.node(2).unwrap().editor.text(), "a中bXY");
        key(&mut surface, KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(surface.node(2).unwrap().editor.text(), "a中bXY");
    }

    #[test]
    fn transformed_inputs_report_local_caret_touch_and_pan_coordinates() {
        let mut surface = form();
        let origin = nsh_core::TransformOrigin {
            x: nsh_core::OriginLen::Px(0.0),
            y: nsh_core::OriginLen::Px(0.0),
            z: 0.0,
        };
        let node = surface.node_mut(2).unwrap();
        node.style.transform_origin = origin;
        node.style
            .transform
            .push(nsh_core::TransformKind::Scale, 2.0);
        node.editor.controlled("abcd".into(), 0);
        node.events = mask(&[EventKind::Touch]);
        surface.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(surface.node(2).unwrap().editor.selection(), (2, 2));
        assert!(surface.events.iter().any(|event|matches!(event,SurfaceEvent::Event(ROOT_ID,EventKind::Touch,EventPayload::Touch(touch)) if touch.phase==TouchPhase::Start && (touch.points[0].location_x-18.0).abs()<0.001 && (touch.points[0].location_y-4.0).abs()<0.001)));
        surface.cancel_pointer();
        let node = surface.node_mut(4).unwrap();
        node.style.transform_origin = origin;
        node.style
            .transform
            .push(nsh_core::TransformKind::Scale, 2.0);
        node.events = mask(&[EventKind::PanStart, EventKind::PanEnd]);
        node.gestures.pan_x = Some(11);
        surface.animation.apply(
            nsh_animation::AnimationOp::CreateValue {
                node: 11,
                value: 5.0,
            },
            &mut Vec::new(),
        );
        let mouse = |kind, column| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row: 3,
                modifiers: KeyModifiers::NONE,
            })
        };
        surface.handle_event(mouse(MouseEventKind::Down(MouseButton::Left), 1));
        surface.handle_event(mouse(MouseEventKind::Drag(MouseButton::Left), 5));
        assert!((surface.animation.value(11).unwrap() - 21.0).abs() < 0.001);
    }

    #[test]
    fn native_header_back_click_honors_prevent_back_before_javascript() {
        use nsh_core::{MountBatch, MountItem};
        let mut surface = Surface::new(40, 12, crate::CellMetrics::default());
        let mut items = create(2, ViewKind::Stack, ROOT_ID, 0.0);
        items.extend(create(4, ViewKind::Screen, 2, 0.0));
        items.extend(create(6, ViewKind::Screen, 2, 0.0));
        items.push(MountItem::ScreenConfig {
            id: 6,
            config: Box::new(nsh_core::ScreenConfig {
                prevent_back: true,
                ..Default::default()
            }),
        });
        surface.apply(MountBatch { items });
        surface.tick(1.0);
        surface.events.clear();
        surface.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(
            surface
                .events
                .iter()
                .any(|event| matches!(event, SurfaceEvent::Event(6, EventKind::BackRequest, _)))
        );
        assert!(!surface.node(6).unwrap().popped);
    }
}
