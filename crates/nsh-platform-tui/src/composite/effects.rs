//! Ordered CSS filters over straight RGBA compositor layers.
//!
//! Spatial effects use premultiplied samples so hidden colors cannot bleed
//! into translucent edges. A bounded working grid and a separable Gaussian
//! keep large layers and radii from making the terminal worker unbounded.

use nsh_core::{Color, Filter, Points};

use crate::image::Raster;

const MAX_WORK_PIXELS: usize = 256 * 1024;
const MAX_SIGMA: f64 = 16.0;
const MAX_KERNEL_RADIUS: usize = 64;

/// Applies every function in order; spatial lengths are points scaled to
/// the raster's pixels. Output remains clipped to the supplied raster.
pub(super) fn apply(raster: &mut Raster, filters: &[Filter], scale: f32) {
    if dimensions(raster).is_none() {
        return;
    }
    for filter in filters {
        match *filter {
            Filter::Blur(radius) => {
                if let Some(radius) = scaled(radius, scale) {
                    blur(raster, radius);
                }
            }
            Filter::DropShadow {
                offset_x,
                offset_y,
                blur,
                color,
            } => {
                if let (Some(x), Some(y), Some(radius)) = (
                    scaled(offset_x, scale),
                    scaled(offset_y, scale),
                    scaled(blur, scale),
                ) {
                    // Match the native backends: shadow blur is twice its
                    // Gaussian deviation, unlike Filter::Blur's deviation.
                    shadow(raster, x, y, radius * 0.5, color);
                }
            }
            _ => {
                if let Some(matrix) = filter.color_matrix()
                    && matrix.iter().all(|n| n.is_finite())
                {
                    color_matrix(raster, &matrix);
                }
            }
        }
    }
}

/// Gaussian standard deviation in raster pixels, with transparent samples
/// beyond the input and output clipped to its existing dimensions.
pub(super) fn blur(raster: &mut Raster, radius: f32) {
    if !radius.is_finite() || radius <= 0.0 || dimensions(raster).is_none() {
        return;
    }
    let field = Field::blurred(raster, f64::from(radius));
    for (i, pixel) in raster.rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = (i % raster.width as usize) as f32 + 0.5;
        let y = (i / raster.width as usize) as f32 + 0.5;
        write_straight(pixel, field.sample(x, y));
    }
}

fn dimensions(raster: &Raster) -> Option<(usize, usize)> {
    let (w, h) = (raster.width as usize, raster.height as usize);
    (w != 0 && h != 0 && w.checked_mul(h)?.checked_mul(4)? == raster.rgba.len()).then_some((w, h))
}

fn scaled(length: Points, scale: f32) -> Option<f32> {
    if !length.is_finite() || !scale.is_finite() || scale < 0.0 {
        return None;
    }
    Some(
        (f64::from(length.0) * f64::from(scale)).clamp(-f64::from(f32::MAX), f64::from(f32::MAX))
            as f32,
    )
}

fn color_matrix(raster: &mut Raster, matrix: &[f32; 20]) {
    for pixel in raster.rgba.as_chunks_mut::<4>().0 {
        let input = pixel.map(|n| n as f32 / 255.0);
        for (row, channel) in pixel.iter_mut().enumerate() {
            let coefficients = &matrix[row * 5..row * 5 + 5];
            let value = coefficients[..4]
                .iter()
                .zip(input)
                .fold(coefficients[4], |value, (coefficient, sample)| {
                    value + coefficient * sample
                });
            *channel = byte(value);
        }
    }
}

fn shadow(raster: &mut Raster, dx: f32, dy: f32, sigma: f32, color: Color) {
    if color.is_transparent() {
        return;
    }
    let field = Field::blurred(raster, f64::from(sigma.max(0.0)));
    let (r, g, b, alpha) = color.to_f64();
    let color = [r as f32, g as f32, b as f32];
    for (i, pixel) in raster.rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = (i % raster.width as usize) as f32 + 0.5 - dx;
        let y = (i / raster.width as usize) as f32 + 0.5 - dy;
        let behind = field.sample(x, y)[3] * alpha as f32;
        let source = pixel[3] as f32 / 255.0;
        let behind = behind * (1.0 - source);
        let mut out = [0.0; 4];
        for k in 0..3 {
            out[k] = pixel[k] as f32 / 255.0 * source + color[k] * behind;
        }
        out[3] = source + behind;
        write_straight(pixel, out);
    }
}

fn byte(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn write_straight(pixel: &mut [u8], sample: [f32; 4]) {
    let alpha = sample[3].clamp(0.0, 1.0);
    if alpha <= 0.0 {
        pixel.fill(0);
        return;
    }
    for k in 0..3 {
        pixel[k] = byte(sample[k] / alpha);
    }
    pixel[3] = byte(alpha);
}

/// A premultiplied image with a transparent convolution halo. Keeping the
/// halo lets offset shadows sample blurred pixels beyond the source box.
struct Field {
    width: usize,
    height: usize,
    x_scale: f32,
    y_scale: f32,
    x_halo: usize,
    y_halo: usize,
    pixels: Vec<[f32; 4]>,
}

impl Field {
    fn blurred(raster: &Raster, sigma: f64) -> Self {
        let (source_w, source_h) = (raster.width as usize, raster.height as usize);
        let mut reduction = (sigma / MAX_SIGMA)
            .max(((source_w * source_h) as f64 / MAX_WORK_PIXELS as f64).sqrt())
            .max(1.0);
        let (w, h, x_kernel, y_kernel) = loop {
            let w = (source_w as f64 / reduction).ceil().max(1.0) as usize;
            let h = (source_h as f64 / reduction).ceil().max(1.0) as usize;
            let x_kernel = kernel(sigma * w as f64 / source_w as f64);
            let y_kernel = kernel(sigma * h as f64 / source_h as f64);
            let padded = (w + 2 * (x_kernel.len() - 1)) * (h + 2 * (y_kernel.len() - 1));
            if padded <= MAX_WORK_PIXELS {
                break (w, h, x_kernel, y_kernel);
            }
            reduction *= 2.0;
        };
        let (x_halo, y_halo) = (x_kernel.len() - 1, y_kernel.len() - 1);
        let (width, height) = (w + 2 * x_halo, h + 2 * y_halo);
        let mut pixels = vec![[0.0; 4]; width * height];
        let (x_ratio, y_ratio) = (source_w as f64 / w as f64, source_h as f64 / h as f64);
        // Exact area averaging keeps thin strokes and isolated alpha when
        // a large effect needs a lower resolution working image.
        for y in 0..h {
            let (top, bottom) = (y as f64 * y_ratio, (y + 1) as f64 * y_ratio);
            for x in 0..w {
                let (left, right) = (x as f64 * x_ratio, (x + 1) as f64 * x_ratio);
                let mut sample = [0.0; 4];
                for sy in top.floor() as usize..(bottom.ceil() as usize).min(source_h) {
                    let wy = (bottom.min((sy + 1) as f64) - top.max(sy as f64)) / y_ratio;
                    for sx in left.floor() as usize..(right.ceil() as usize).min(source_w) {
                        let wx = (right.min((sx + 1) as f64) - left.max(sx as f64)) / x_ratio;
                        let pixel = &raster.rgba[(sy * source_w + sx) * 4..][..4];
                        let alpha = pixel[3] as f32 / 255.0;
                        let weight = (wx * wy) as f32;
                        for k in 0..3 {
                            sample[k] += pixel[k] as f32 / 255.0 * alpha * weight;
                        }
                        sample[3] += alpha * weight;
                    }
                }
                pixels[(y + y_halo) * width + x + x_halo] = sample;
            }
        }
        let mut scratch = vec![[0.0; 4]; pixels.len()];
        convolve(&pixels, &mut scratch, width, height, &x_kernel, true);
        convolve(&scratch, &mut pixels, width, height, &y_kernel, false);
        Self {
            width,
            height,
            x_scale: w as f32 / source_w as f32,
            y_scale: h as f32 / source_h as f32,
            x_halo,
            y_halo,
            pixels,
        }
    }

    /// `x` and `y` are pixel-center coordinates in the original raster.
    fn sample(&self, x: f32, y: f32) -> [f32; 4] {
        let x = x * self.x_scale - 0.5 + self.x_halo as f32;
        let y = y * self.y_scale - 0.5 + self.y_halo as f32;
        if x < -1.0 || y < -1.0 || x >= self.width as f32 || y >= self.height as f32 {
            return [0.0; 4];
        }
        let (ix, iy) = (x.floor() as isize, y.floor() as isize);
        let (fx, fy) = (x - x.floor(), y - y.floor());
        let mut out = [0.0; 4];
        for (py, wy) in [(iy, 1.0 - fy), (iy + 1, fy)] {
            for (px, wx) in [(ix, 1.0 - fx), (ix + 1, fx)] {
                if px >= 0 && py >= 0 && px < self.width as isize && py < self.height as isize {
                    let sample = self.pixels[py as usize * self.width + px as usize];
                    for k in 0..4 {
                        out[k] += sample[k] * wx * wy;
                    }
                }
            }
        }
        out
    }
}

/// Positive half of a normalized Gaussian, truncated at four deviations.
/// When even a one-pixel axis has a larger deviation, the continuous
/// Gaussian normalization is accurate and keeps its alpha attenuation.
fn kernel(sigma: f64) -> Vec<f32> {
    if sigma <= 0.0 {
        return vec![1.0];
    }
    let radius = ((4.0 * sigma).ceil() as usize).min(MAX_KERNEL_RADIUS);
    let weights: Vec<f64> = (0..=radius)
        .map(|i| (-0.5 * (i as f64 / sigma).powi(2)).exp())
        .collect();
    let total = if sigma > MAX_SIGMA {
        (2.0 * std::f64::consts::PI).sqrt() * sigma
    } else {
        weights[0] + 2.0 * weights[1..].iter().sum::<f64>()
    };
    weights.into_iter().map(|v| (v / total) as f32).collect()
}

fn convolve(
    source: &[[f32; 4]],
    dest: &mut [[f32; 4]],
    width: usize,
    height: usize,
    kernel: &[f32],
    horizontal: bool,
) {
    let (lines, len, step, stride) = if horizontal {
        (height, width, 1, width)
    } else {
        (width, height, width, 1)
    };
    for line in 0..lines {
        let start = line * stride;
        for at in 0..len {
            let center = source[start + at * step];
            let mut sum = center.map(|value| value * kernel[0]);
            for (offset, weight) in kernel.iter().enumerate().skip(1) {
                if offset <= at {
                    let sample = source[start + (at - offset) * step];
                    for k in 0..4 {
                        sum[k] += sample[k] * weight;
                    }
                }
                if at + offset < len {
                    let sample = source[start + (at + offset) * step];
                    for k in 0..4 {
                        sum[k] += sample[k] * weight;
                    }
                }
            }
            dest[start + at * step] = sum;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raster(width: u32, pixels: &[[u8; 4]]) -> Raster {
        Raster {
            width,
            height: pixels.len() as u32 / width,
            rgba: pixels.iter().flatten().copied().collect(),
        }
    }

    #[test]
    fn gaussian_edges_keep_color_without_a_transparent_matte() {
        let mut image = raster(3, &[[255, 0, 0, 255], [0, 0, 255, 0], [0, 0, 255, 0]]);
        blur(&mut image, 1.0);
        assert_eq!(image.width, 3);
        assert_eq!(image.height, 1);
        assert_eq!(&image.rgba[..4], &[255, 0, 0, 41]);
        assert_eq!(&image.rgba[4..8], &[255, 0, 0, 25]);
        assert_eq!(&image.rgba[8..12], &[255, 0, 0, 5]);
    }

    #[test]
    fn color_functions_apply_in_order_and_opacity_preserves_straight_rgb() {
        let mut first = raster(1, &[[200, 100, 50, 128]]);
        apply(
            &mut first,
            &[Filter::Brightness(0.5), Filter::Invert(1.0)],
            1.0,
        );
        assert_eq!(first.rgba, [155, 205, 230, 128]);
        let mut reversed = raster(1, &[[200, 100, 50, 128]]);
        apply(
            &mut reversed,
            &[Filter::Invert(1.0), Filter::Brightness(0.5)],
            1.0,
        );
        assert_eq!(reversed.rgba, [28, 78, 103, 128]);
        apply(&mut first, &[Filter::Opacity(0.5)], 1.0);
        assert_eq!(first.rgba, [155, 205, 230, 64]);
    }

    #[test]
    fn color_filters_cannot_cross_a_blur_in_the_chain() {
        let mut pixels = [[0, 0, 0, 255]; 81];
        pixels[40] = [255, 0, 0, 255];
        let mut before = raster(9, &pixels);
        let mut after = before.clone();
        apply(
            &mut before,
            &[Filter::Brightness(2.0), Filter::Blur(Points(1.0))],
            1.0,
        );
        apply(
            &mut after,
            &[Filter::Blur(Points(1.0)), Filter::Brightness(2.0)],
            1.0,
        );
        assert_eq!(&before.rgba[160..164], &[41, 0, 0, 255]);
        assert_eq!(&after.rgba[160..164], &[82, 0, 0, 255]);
    }

    #[test]
    fn shadow_is_an_offset_alpha_mask_behind_the_source() {
        let mut image = raster(3, &[[255, 0, 0, 255], [0; 4], [0; 4]]);
        apply(
            &mut image,
            &[Filter::DropShadow {
                offset_x: Points(0.5),
                offset_y: Points(0.0),
                blur: Points(0.0),
                color: Color::rgba(0, 0, 255, 128),
            }],
            2.0,
        );
        assert_eq!(image.rgba, [255, 0, 0, 255, 0, 0, 255, 128, 0, 0, 0, 0]);
    }

    #[test]
    fn a_shifted_blur_includes_the_halo_outside_the_source_bounds() {
        let mut image = raster(3, &[[255, 0, 0, 128], [0; 4], [0; 4]]);
        apply(
            &mut image,
            &[Filter::DropShadow {
                offset_x: Points(1.0),
                offset_y: Points(0.0),
                blur: Points(2.0),
                color: Color::rgba(0, 0, 255, 255),
            }],
            1.0,
        );
        assert_eq!(&image.rgba[..4], &[243, 0, 12, 134]);
        assert_eq!(&image.rgba[4..8], &[0, 0, 255, 20]);
        assert_eq!(&image.rgba[8..12], &[0, 0, 255, 12]);
    }

    #[test]
    fn large_radii_are_bounded_and_invalid_spatial_values_are_ignored() {
        let mut image = raster(1, &[[255, 0, 0, 255]]);
        let original = image.rgba.clone();
        apply(&mut image, &[Filter::Blur(Points(f32::NAN))], 1.0);
        apply(&mut image, &[Filter::Blur(Points(1.0))], f32::INFINITY);
        blur(&mut image, 0.0);
        assert_eq!(image.rgba, original);
        let field = Field::blurred(&image, f64::from(f32::MAX));
        assert!(field.pixels.len() <= MAX_WORK_PIXELS);
        assert!(field.pixels.iter().flatten().all(|v| v.is_finite()));
        blur(&mut image, f32::MAX);
        assert_eq!(image.rgba[3], 0);
    }
}
