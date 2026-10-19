//! The boundary between the shared layout's points and terminal cells.

use nsh_core::Size;

/// The virtual size of a terminal cell in Native Shell points.
///
/// The default is 8 × 16 points, which lets the same flex and grid layouts
/// retain their proportions in a terminal. The terminal controls the physical
/// font; these dimensions describe layout coordinates rather than font pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    width: f32,
    height: f32,
}

impl CellMetrics {
    /// Creates finite, strictly positive virtual cell dimensions.
    pub fn new(width: f32, height: f32) -> Result<Self, String> {
        if !width.is_finite() || width <= 0.0 || !height.is_finite() || height <= 0.0 {
            return Err("terminal cell dimensions must be finite and positive".to_owned());
        }
        Ok(Self { width, height })
    }

    /// Width of a terminal column in points.
    pub fn width(self) -> f32 {
        self.width
    }

    /// Height of a terminal row in points.
    pub fn height(self) -> f32 {
        self.height
    }

    /// Converts a terminal region to a viewport in shared layout coordinates.
    pub fn viewport(self, columns: u16, rows: u16) -> Size {
        Size::new(columns as f32 * self.width, rows as f32 * self.height)
    }
}

impl Default for CellMetrics {
    fn default() -> Self {
        Self {
            width: 8.0,
            height: 16.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_coordinates_preserve_terminal_proportions() {
        let metrics = CellMetrics::default();
        assert_eq!(metrics.viewport(120, 40), Size::new(960.0, 640.0));
        assert_eq!(
            CellMetrics::new(10.0, 20.0).unwrap().viewport(2, 3),
            Size::new(20.0, 60.0)
        );
    }

    #[test]
    fn rejects_invalid_coordinate_scales() {
        for value in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(CellMetrics::new(value, 16.0).is_err());
            assert!(CellMetrics::new(8.0, value).is_err());
        }
    }
}
