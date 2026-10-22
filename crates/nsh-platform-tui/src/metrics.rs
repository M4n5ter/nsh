//! The boundary between the shared layout's points and terminal cells.

use nsh_core::geometry::{Points, Size, px};

/// The virtual size of a terminal cell in Native Shell points.
///
/// The default is 8 × 16 points, which lets the same flex and grid layouts
/// retain their proportions in a terminal. The terminal controls the physical
/// font; these dimensions describe layout coordinates rather than font pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    width: Points,
    height: Points,
}

impl CellMetrics {
    /// Creates finite, strictly positive virtual cell dimensions.
    pub fn new(width: Points, height: Points) -> Result<Self, String> {
        if !width.is_finite()
            || width <= Points::ZERO
            || !height.is_finite()
            || height <= Points::ZERO
        {
            return Err("terminal cell dimensions must be finite and positive".to_owned());
        }
        Ok(Self { width, height })
    }

    /// Width of a terminal column in points.
    pub fn width(self) -> Points {
        self.width
    }

    /// Height of a terminal row in points.
    pub fn height(self) -> Points {
        self.height
    }

    /// First column whose center lies at or after a point edge.
    pub(crate) fn column_at(self, x: Points) -> f32 {
        (x / self.width - 0.5).ceil()
    }

    /// First row whose center lies at or after a point edge.
    pub(crate) fn row_at(self, y: Points) -> f32 {
        (y / self.height - 0.5).ceil()
    }

    /// Converts a terminal region to a viewport in shared layout coordinates.
    pub fn viewport(self, columns: u16, rows: u16) -> Size<Points> {
        Size::new(columns as f32 * self.width, rows as f32 * self.height)
    }
}

impl Default for CellMetrics {
    fn default() -> Self {
        Self {
            width: px(8.0),
            height: px(16.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_coordinates_preserve_terminal_proportions() {
        let metrics = CellMetrics::default();
        assert_eq!(metrics.viewport(120, 40), Size::new(px(960.0), px(640.0)));
        assert_eq!(
            CellMetrics::new(px(10.0), px(20.0)).unwrap().viewport(2, 3),
            Size::new(px(20.0), px(60.0))
        );
    }

    #[test]
    fn rejects_invalid_coordinate_scales() {
        for value in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(CellMetrics::new(px(value), px(16.0)).is_err());
            assert!(CellMetrics::new(px(8.0), px(value)).is_err());
        }
    }
}
