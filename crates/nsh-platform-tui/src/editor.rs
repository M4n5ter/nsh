//! Terminal input editing. Native edits use grapheme boundaries; the
//! engine and JavaScript see UTF-16 offsets and monotonically increasing
//! edit counts, as they do on the graphical backends.

use std::ops::Range;

use nsh_core::{InputConfig, KeyboardType};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const HISTORY_LIMIT: usize = 64;
const HISTORY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
struct Snapshot {
    text: String,
    anchor: usize,
    cursor: usize,
}

/// One input's retained text, selection and bounded undo history.
#[derive(Clone, Debug, Default)]
pub(crate) struct Editor {
    text: String,
    anchor: usize,
    cursor: usize,
    count: u32,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    goal_column: Option<usize>,
}

impl Editor {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    pub(crate) fn selected_range(&self) -> Range<usize> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor)
    }

    pub(crate) fn selected(&self) -> &str {
        &self.text[self.selected_range()]
    }

    pub(crate) fn selection(&self) -> (u32, u32) {
        let range = self.selected_range();
        (
            utf16_len(&self.text[..range.start]),
            utf16_len(&self.text[..range.end]),
        )
    }

    /// A stale controlled value must never erase text typed since its turn.
    pub(crate) fn controlled(&mut self, text: String, count: u32) -> bool {
        if count < self.count {
            return false;
        }
        self.assign_text(sanitize(&text, true))
    }

    /// Label selection uses the original UTF-16 offsets carried by the
    /// shaped layout, including characters hidden by the text renderer.
    pub(crate) fn label_text(&mut self, text: &str) -> bool {
        self.assign_text(text.to_owned())
    }

    fn assign_text(&mut self, text: String) -> bool {
        if text == self.text {
            return false;
        }
        let at_end = self.cursor == self.text.len();
        let old_cursor = utf16_len(&self.text[..self.cursor]);
        self.text = text;
        self.cursor = if at_end {
            self.text.len()
        } else {
            byte_at_utf16(&self.text, old_cursor)
        };
        self.anchor = self.cursor;
        self.goal_column = None;
        self.undo.clear();
        self.redo.clear();
        true
    }

    pub(crate) fn set_selection(&mut self, start: u32, end: u32) -> bool {
        let start = byte_at_utf16(&self.text, start);
        let end = byte_at_utf16(&self.text, end);
        self.select_bytes(start, end)
    }

    fn select_bytes(&mut self, anchor: usize, cursor: usize) -> bool {
        let changed = (self.anchor, self.cursor) != (anchor, cursor);
        self.anchor = anchor;
        self.cursor = cursor;
        self.goal_column = None;
        changed
    }

    pub(crate) fn select_all(&mut self) -> bool {
        self.select_bytes(0, self.text.len())
    }

    pub(crate) fn move_to(&mut self, offset: u32, extend: bool) -> bool {
        let offset = byte_at_utf16(&self.text, offset);
        self.select_bytes(if extend { self.anchor } else { offset }, offset)
    }

    pub(crate) fn horizontal(&mut self, forward: bool, word: bool, extend: bool) -> bool {
        let range = self.selected_range();
        let offset = if !extend && !range.is_empty() && !word {
            if forward { range.end } else { range.start }
        } else if word {
            self.word_boundary(self.cursor, forward)
        } else {
            self.boundary(self.cursor, forward)
        };
        self.select_bytes(if extend { self.anchor } else { offset }, offset)
    }

    pub(crate) fn home_end(&mut self, end: bool, document: bool, extend: bool) -> bool {
        let offset = if document {
            if end { self.text.len() } else { 0 }
        } else if end {
            self.text[self.cursor..]
                .find('\n')
                .map_or(self.text.len(), |i| self.cursor + i)
        } else {
            self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
        };
        self.select_bytes(if extend { self.anchor } else { offset }, offset)
    }

    pub(crate) fn vertical(
        &mut self,
        down: bool,
        extend: bool,
        columns: usize,
        secure: bool,
        alignments: (usize, usize),
    ) -> bool {
        let (row, column) = self.cursor_cell(columns, secure);
        let goal = self
            .goal_column
            .unwrap_or(column.saturating_add(alignments.0));
        let row = if down {
            row.saturating_add(1)
        } else {
            row.saturating_sub(1)
        };
        let offset = self.offset_at(row, goal.saturating_sub(alignments.1), columns, secure);
        let changed = self.move_to(offset, extend);
        self.goal_column = Some(goal);
        changed
    }

    fn boundary(&self, offset: usize, forward: bool) -> usize {
        if forward {
            self.text
                .grapheme_indices(true)
                .map(|(i, _)| i)
                .find(|&i| i > offset)
                .unwrap_or(self.text.len())
        } else {
            self.text
                .grapheme_indices(true)
                .map(|(i, _)| i)
                .take_while(|&i| i < offset)
                .last()
                .unwrap_or(0)
        }
    }

    fn word_boundary(&self, offset: usize, forward: bool) -> usize {
        if forward {
            self.text
                .unicode_word_indices()
                .map(|(i, word)| i + word.len())
                .find(|&i| i > offset)
                .unwrap_or(self.text.len())
        } else {
            self.text
                .unicode_word_indices()
                .map(|(i, _)| i)
                .take_while(|&i| i < offset)
                .last()
                .unwrap_or(0)
        }
    }

    pub(crate) fn replace(&mut self, incoming: &str, config: &InputConfig) -> bool {
        if !config.editable {
            return false;
        }
        let text = sanitize(incoming, config.multiline);
        let text: String = if config.keyboard == KeyboardType::Number {
            text.chars().filter(char::is_ascii_digit).collect()
        } else {
            text
        };
        // A rejected key or paste does not delete the current selection.
        if !incoming.is_empty() && text.is_empty() {
            return false;
        }
        let range = self.selected_range();
        let retained = utf16_len(&self.text) - utf16_len(&self.text[range.clone()]);
        let limit = config.max_length.map(|max| max.saturating_sub(retained));
        let mut units = 0u32;
        let fit: String = text
            .graphemes(true)
            .take_while(|g| {
                units = units.saturating_add(utf16_len(g));
                limit.is_none_or(|limit| units <= limit)
            })
            .collect();
        if !text.is_empty() && fit.is_empty() {
            return false;
        }
        if self.text[range.clone()] == fit {
            self.select_bytes(range.start + fit.len(), range.start + fit.len());
            return false;
        }
        self.record();
        self.text.replace_range(range.clone(), &fit);
        let cursor = range.start + fit.len();
        // Combining text inserted beside a cluster may merge with it.
        self.cursor = ceil_grapheme(&self.text, cursor);
        self.anchor = self.cursor;
        self.goal_column = None;
        self.count = self.count.saturating_add(1);
        true
    }

    pub(crate) fn delete(&mut self, backward: bool, word: bool, config: &InputConfig) -> bool {
        if !config.editable {
            return false;
        }
        if self.selected_range().is_empty() {
            let offset = if word {
                self.word_boundary(self.cursor, !backward)
            } else {
                self.boundary(self.cursor, !backward)
            };
            if offset == self.cursor {
                return false;
            }
            self.anchor = offset;
        }
        self.replace("", config)
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            anchor: self.anchor,
            cursor: self.cursor,
        }
    }

    fn record(&mut self) {
        let snapshot = self.snapshot();
        bounded_push(&mut self.undo, snapshot);
        self.redo.clear();
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.text = snapshot.text;
        self.anchor = snapshot.anchor;
        self.cursor = snapshot.cursor;
        self.goal_column = None;
        self.count = self.count.saturating_add(1);
    }

    pub(crate) fn undo(&mut self, config: &InputConfig) -> bool {
        if !config.editable {
            return false;
        }
        let Some(snapshot) = self.undo.pop() else {
            return false;
        };
        let current = self.snapshot();
        bounded_push(&mut self.redo, current);
        self.restore(snapshot);
        true
    }

    pub(crate) fn redo(&mut self, config: &InputConfig) -> bool {
        if !config.editable {
            return false;
        }
        let Some(snapshot) = self.redo.pop() else {
            return false;
        };
        let current = self.snapshot();
        bounded_push(&mut self.undo, current);
        self.restore(snapshot);
        true
    }

    #[cfg(test)]
    pub(crate) fn display(&self, secure: bool) -> String {
        if !secure {
            return self.text.clone();
        }
        self.text
            .graphemes(true)
            .map(|g| if g == "\n" { "\n" } else { "•" })
            .collect()
    }

    pub(crate) fn cursor_cell(&self, columns: usize, secure: bool) -> (usize, usize) {
        let mut result = (0, 0);
        self.walk_cells(columns, secure, |offset, row, column| {
            if offset <= self.cursor {
                result = (row, column)
            }
        });
        result
    }

    /// Finds the nearest caret boundary on a visual line, including tabs,
    /// wide glyphs, masked passwords and wrapped continuation lines.
    pub(crate) fn offset_at(&self, row: usize, column: usize, columns: usize, secure: bool) -> u32 {
        let mut closest = (usize::MAX, usize::MAX, 0);
        self.walk_cells(columns, secure, |offset, r, c| {
            let distance = (r.abs_diff(row), c.abs_diff(column), offset);
            if (distance.0, distance.1) < (closest.0, closest.1) {
                closest = distance;
            }
        });
        utf16_len(&self.text[..closest.2])
    }

    fn walk_cells(&self, columns: usize, secure: bool, mut visit: impl FnMut(usize, usize, usize)) {
        let columns = columns.max(1);
        let (mut row, mut column) = (0usize, 0usize);
        for (offset, grapheme) in self.text.grapheme_indices(true) {
            if grapheme == "\n" {
                visit(offset, row, column);
                row += 1;
                column = 0;
                continue;
            }
            let width = if secure {
                1
            } else if grapheme == "\t" {
                4 - column % 4
            } else {
                UnicodeWidthStr::width(grapheme)
            };
            if column > 0 && column.saturating_add(width) > columns {
                row += 1;
                column = 0;
            }
            visit(offset, row, column);
            column = column.saturating_add(width);
        }
        if column >= columns {
            row += column / columns;
            column %= columns;
        }
        visit(self.text.len(), row, column);
    }
}

fn bounded_push(history: &mut Vec<Snapshot>, snapshot: Snapshot) {
    if snapshot.text.len() > HISTORY_BYTES {
        history.clear();
        return;
    }
    history.push(snapshot);
    let mut bytes = history.iter().map(|s| s.text.len()).sum::<usize>();
    while history.len() > HISTORY_LIMIT || bytes > HISTORY_BYTES {
        bytes -= history.remove(0).text.len();
    }
}

fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count().min(u32::MAX as usize) as u32
}

fn byte_at_utf16(text: &str, offset: u32) -> usize {
    let mut units = 0u32;
    for (i, grapheme) in text.grapheme_indices(true) {
        let end = units.saturating_add(utf16_len(grapheme));
        if offset < end {
            return i;
        }
        units = end;
    }
    text.len()
}

fn ceil_grapheme(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .map(|(i, _)| i)
        .find(|&i| i >= offset)
        .unwrap_or(text.len())
}

/// Terminals must never receive application supplied escape sequences as
/// editing content. Newlines and tabs retain their editing meaning.
fn sanitize(text: &str, multiline: bool) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .chars()
        .filter_map(|c| match c {
            '\n' if !multiline => Some(' '),
            '\n' | '\t' => Some(c),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grapheme_editing_and_utf16_selection() {
        let mut editor = Editor::default();
        let config = InputConfig::default();
        assert!(editor.replace("a👨‍👩‍👧e\u{301}中", &config));
        assert_eq!(editor.selection(), (12, 12));
        assert!(editor.horizontal(false, false, false));
        assert_eq!(editor.selection(), (11, 11));
        assert!(editor.delete(true, false, &config));
        assert_eq!(editor.text(), "a👨‍👩‍👧中");
        assert!(editor.delete(true, false, &config));
        assert_eq!(editor.text(), "a中");
        editor.set_selection(1, 2);
        assert_eq!(editor.selected(), "中");
    }

    #[test]
    fn stale_controlled_values_and_history() {
        let mut editor = Editor::default();
        let config = InputConfig::default();
        editor.controlled("hello".into(), 0);
        editor.replace("中", &config);
        assert!(!editor.controlled("hello".into(), 0));
        assert_eq!(editor.text(), "hello中");
        assert!(editor.undo(&config));
        assert_eq!(editor.text(), "hello");
        assert!(editor.redo(&config));
        assert_eq!(editor.text(), "hello中");
        assert_eq!(editor.count(), 3);
        assert!(editor.controlled("server".into(), 3));
        assert!(!editor.undo(&config));
    }

    #[test]
    fn utf16_limit_does_not_split_clusters_or_erase_rejected_selection() {
        let mut editor = Editor::default();
        let config = InputConfig {
            max_length: Some(3),
            ..InputConfig::default()
        };
        editor.replace("a😀b", &config);
        assert_eq!(editor.text(), "a😀");
        editor.set_selection(0, 1);
        assert!(!editor.replace("🇨🇳", &config));
        assert_eq!(editor.text(), "a😀");
        assert_eq!(editor.selected(), "a");
        editor.replace("z", &config);
        assert_eq!(editor.text(), "z😀");
    }

    #[test]
    fn paste_sanitization_numeric_and_read_only() {
        let mut editor = Editor::default();
        let mut config = InputConfig::default();
        editor.replace("a\r\nb\x1b[31m\0", &config);
        assert_eq!(editor.text(), "a b[31m");
        editor.select_all();
        config.keyboard = KeyboardType::Number;
        editor.replace("a12-３", &config);
        assert_eq!(editor.text(), "12");
        config.editable = false;
        assert!(!editor.replace("3", &config));
        assert!(!editor.delete(true, false, &config));
        assert!(!editor.undo(&config));
    }

    #[test]
    fn visual_navigation_wide_glyphs_wrap_and_passwords() {
        let mut editor = Editor::default();
        let config = InputConfig {
            multiline: true,
            ..InputConfig::default()
        };
        editor.replace("a中b\nc", &config);
        assert_eq!(editor.cursor_cell(3, false), (2, 1));
        editor.set_selection(1, 1);
        assert_eq!(editor.cursor_cell(3, false), (0, 1));
        editor.vertical(true, false, 3, false, (0, 0));
        assert_eq!(editor.selection(), (3, 3));
        assert_eq!(editor.display(true), "•••\n•");
        assert_eq!(editor.offset_at(0, 2, 10, true), 2);
    }

    #[test]
    fn history_is_bounded_and_combining_insert_stays_on_boundary() {
        let mut editor = Editor::default();
        let config = InputConfig::default();
        for _ in 0..100 {
            editor.replace("x", &config);
        }
        assert_eq!(editor.undo.len(), HISTORY_LIMIT);
        editor.controlled("e".into(), editor.count());
        editor.replace("\u{301}", &config);
        assert_eq!(editor.selection(), (2, 2));
        editor.horizontal(false, false, false);
        assert_eq!(editor.selection(), (0, 0));
    }

    #[test]
    fn label_source_keeps_hidden_utf16_units_for_shaped_selection() {
        let mut editor = Editor::default();
        editor.label_text("a\x1bb");
        editor.set_selection(2, 3);
        assert_eq!(editor.selected(), "b");
        assert_eq!(editor.selection(), (2, 3));
        editor.controlled("a\x1bb".into(), 0);
        assert_eq!(editor.text(), "ab");
    }
}
