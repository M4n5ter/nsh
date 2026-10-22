//! Reading anchors use the final shaped source lines, not recycled node ids.
//! The list keeps the one pin; this module only derives its geometry index.

use super::{Core, Key, Kind, LeafContext, TextEngine, content_box};
use crate::text_source::SourceGeometry;

struct Seat {
    key: Key,
    y: f32,
    clip: (f32, f32),
}

impl<M: TextEngine> Core<M> {
    /// Rebuilt on source/layout/cell changes. Wheel reports use the resulting
    /// line index and do not walk text nodes or reconstruct line geometry.
    pub(super) fn list_source_geometry(&mut self, key: Key) -> SourceGeometry {
        let list = self.nodes[key as usize].list.as_ref().expect("list");
        // A parent-viewport list contributes its texts to the existing outer
        // list's index; it must not become another reader/scroll owner.
        if list.horizontal || list.inverted || list.parent {
            return SourceGeometry::default();
        }
        let mut seats = Vec::new();
        for (cell, y) in self.source_cells(key) {
            self.source_seats(cell, y, (f32::NEG_INFINITY, f32::INFINITY), &mut seats);
        }
        let keys: Vec<_> = seats.iter().map(|seat| seat.key).collect();
        // This is the same cached text layout used by the final mount flush.
        // Taffy's final content box is already available even when a resized
        // cell has not yet queued descendant frame notifications.
        self.flush_texts(&keys, false);
        let mut geometry = SourceGeometry::default();
        for seat in seats {
            let Some(layout) = self.nodes[seat.key as usize].layout else {
                continue;
            };
            let Some(LeafContext::Text(cx)) = self.layout.get_node_context(layout) else {
                continue;
            };
            if let (Some(lines), Some((content, _))) = (&cx.source_geometry, &cx.laid_out) {
                geometry.append(lines, seat.y + content.origin.y.0, seat.clip);
            }
        }
        geometry.finish();
        geometry
    }

    /// Current ready cells in the same natural positions `update_list` uses.
    fn source_cells(&self, key: Key) -> Vec<(Key, f32)> {
        let node = &self.nodes[key as usize];
        let list = node.list.as_ref().expect("list");
        let inset = if list.parent {
            node.layout.map_or(0.0, |layout| {
                content_box(&self.rounded_layout(layout)).origin.y.0
            })
        } else {
            0.0
        };
        let mut rows: Vec<_> = list
            .slots
            .iter()
            .filter_map(|slot| slot.ready().map(|index| (index, slot.cell)))
            .collect();
        rows.sort_unstable_by_key(|&(index, _)| index);
        let blocks = list.blocks();
        let mut out = Vec::with_capacity(rows.len() + 3);
        let header = list.extras[0].cell;
        if header != u32::MAX {
            out.push((header, inset + blocks.header));
        }
        out.extend(
            rows.into_iter()
                .map(|(index, cell)| (cell, inset + list.item_at(index))),
        );
        let empty = list.extras[2].cell;
        if list.count == 0 && empty != u32::MAX {
            out.push((empty, inset + blocks.body));
        }
        let footer = list.extras[1].cell;
        if footer != u32::MAX {
            out.push((footer, inset + blocks.footer));
        }
        out
    }

    fn source_seats(&self, key: Key, y: f32, mut clip: (f32, f32), out: &mut Vec<Seat>) {
        let Some(node) = self.nodes.get(key as usize) else {
            return;
        };
        let Some(layout) = node.layout else { return };
        if node.kind.is_root() || self.displays_none(key) {
            return;
        }
        let frame = self.rounded_layout(layout);
        let parent_list = node.list.as_ref().is_some_and(|list| list.parent);
        if let Some(scroll) = &node.scroll
            && !parent_list
            && !scroll.horizontal
            && (node.kind == Kind::List
                || scroll.behavior.enabled
                || scroll.offset.1 != 0.0
                || !scroll.pending.is_empty())
        {
            // An independent vertical reader's offsets are not reflow of
            // its outer reader. Disabled wrapped code is not such an owner.
            return;
        }
        if node.view_style.overflow_hidden || node.scroll.is_some() {
            clip = (clip.0.max(y), clip.1.min(y + frame.size.height));
            if clip.0 >= clip.1 {
                return;
            }
        }
        if let Some(LeafContext::Text(cx)) = self.layout.get_node_context(layout) {
            if !cx.sources.is_empty() {
                out.push(Seat { key, y, clip });
            }
            // Styled nested texts belong to this flattened text block.
            // Inline view attachments have their own glyph placement rather
            // than a normal child-layout position.
            return;
        }
        if parent_list {
            for (cell, at) in self.source_cells(key) {
                self.source_seats(cell, y + at, clip, out);
            }
            return;
        }
        for &child in &node.children {
            let child_node = &self.nodes[child as usize];
            if let Some(child_layout) = child_node.layout {
                let child_y = self.rounded_layout(child_layout).location.y;
                self.source_seats(child, y + child_y, clip, out);
            }
        }
    }
}
