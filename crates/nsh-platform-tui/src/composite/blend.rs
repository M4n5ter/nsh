//! CSS pixel blending in premultiplied sRGB, before terminal quantization.

use nsh_core::BlendMode;

/// Composites premultiplied `source` over `destination`, applying group opacity once.
pub(super) fn over(
    source: [f32; 4],
    destination: [f32; 4],
    opacity: f32,
    mode: BlendMode,
) -> [f32; 4] {
    let opacity = opacity.clamp(0.0, 1.0);
    let source = source.map(|component| component * opacity);
    let source_alpha = source[3];
    let destination_alpha = destination[3];
    if source_alpha == 0.0 {
        return destination;
    }
    if destination_alpha == 0.0 {
        return source;
    }

    // These CSS mix-blend-mode values select a compositing operator rather
    // than an overlap blend function, including their additive alpha.
    if matches!(mode, BlendMode::PlusLighter | BlendMode::PlusDarker) {
        let alpha = (source_alpha + destination_alpha).min(1.0);
        let mut result = [0.0; 4];
        for channel in 0..3 {
            result[channel] = match mode {
                BlendMode::PlusLighter => (source[channel] + destination[channel]).min(1.0),
                // The Level 2 draft's plus-darker equation has a known typo.
                // Use the Safari/WPT formula adopted in CSSWG issue #447:
                // https://github.com/w3c/fxtf-drafts/issues/447
                _ => {
                    alpha
                        - (source_alpha - source[channel] + destination_alpha
                            - destination[channel])
                            .min(1.0)
                }
            }
            .clamp(0.0, alpha);
        }
        result[3] = alpha;
        return result;
    }

    let alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
    if mode == BlendMode::Normal {
        return [
            source[0] + destination[0] * (1.0 - source_alpha),
            source[1] + destination[1] * (1.0 - source_alpha),
            source[2] + destination[2] * (1.0 - source_alpha),
            alpha,
        ];
    }

    // CSS blending uses straight colors only in the overlap. Keeping the
    // non-overlap contributions premultiplied preserves transparent edges.
    let source_color = std::array::from_fn(|channel| source[channel] / source_alpha);
    let destination_color = std::array::from_fn(|channel| destination[channel] / destination_alpha);
    let mixed = blend(source_color, destination_color, mode);
    let mut result = [0.0; 4];
    for channel in 0..3 {
        result[channel] = (source[channel] * (1.0 - destination_alpha)
            + destination[channel] * (1.0 - source_alpha)
            + source_alpha * destination_alpha * mixed[channel])
            .clamp(0.0, alpha);
    }
    result[3] = alpha;
    result
}

fn blend(source: [f32; 3], backdrop: [f32; 3], mode: BlendMode) -> [f32; 3] {
    match mode {
        BlendMode::Hue => set_luminosity(
            set_saturation(source, saturation(backdrop)),
            luminosity(backdrop),
        ),
        BlendMode::Saturation => set_luminosity(
            set_saturation(backdrop, saturation(source)),
            luminosity(backdrop),
        ),
        BlendMode::Color => set_luminosity(source, luminosity(backdrop)),
        BlendMode::Luminosity => set_luminosity(backdrop, luminosity(source)),
        _ => std::array::from_fn(|channel| blend_channel(source[channel], backdrop[channel], mode)),
    }
}

fn blend_channel(source: f32, backdrop: f32, mode: BlendMode) -> f32 {
    match mode {
        BlendMode::Normal => source,
        BlendMode::Multiply => source * backdrop,
        BlendMode::Screen => source + backdrop - source * backdrop,
        BlendMode::Overlay => hard_light(backdrop, source),
        BlendMode::Darken => source.min(backdrop),
        BlendMode::Lighten => source.max(backdrop),
        BlendMode::ColorDodge => {
            if backdrop == 0.0 {
                0.0
            } else if source == 1.0 {
                1.0
            } else {
                (backdrop / (1.0 - source)).min(1.0)
            }
        }
        BlendMode::ColorBurn => {
            if backdrop == 1.0 {
                1.0
            } else if source == 0.0 {
                0.0
            } else {
                1.0 - ((1.0 - backdrop) / source).min(1.0)
            }
        }
        BlendMode::HardLight => hard_light(source, backdrop),
        BlendMode::SoftLight => {
            if source <= 0.5 {
                backdrop - (1.0 - 2.0 * source) * backdrop * (1.0 - backdrop)
            } else {
                let highlight = if backdrop <= 0.25 {
                    ((16.0 * backdrop - 12.0) * backdrop + 4.0) * backdrop
                } else {
                    backdrop.sqrt()
                };
                backdrop + (2.0 * source - 1.0) * (highlight - backdrop)
            }
        }
        BlendMode::Difference => (backdrop - source).abs(),
        BlendMode::Exclusion => source + backdrop - 2.0 * source * backdrop,
        _ => unreachable!("nonseparable modes and operators are handled before channel blending"),
    }
}

fn hard_light(source: f32, backdrop: f32) -> f32 {
    if source <= 0.5 {
        2.0 * source * backdrop
    } else {
        1.0 - 2.0 * (1.0 - source) * (1.0 - backdrop)
    }
}

fn luminosity(color: [f32; 3]) -> f32 {
    0.3 * color[0] + 0.59 * color[1] + 0.11 * color[2]
}

fn saturation(color: [f32; 3]) -> f32 {
    color[0].max(color[1]).max(color[2]) - color[0].min(color[1]).min(color[2])
}

fn set_luminosity(color: [f32; 3], target: f32) -> [f32; 3] {
    let delta = target - luminosity(color);
    let mut result = color.map(|component| component + delta);
    let minimum = result[0].min(result[1]).min(result[2]);
    let maximum = result[0].max(result[1]).max(result[2]);
    // CSS ClipColor preserves weighted luminosity while bringing the color
    // into gamut, rather than clipping individual channels independently.
    if minimum < 0.0 {
        result =
            result.map(|component| target + (component - target) * target / (target - minimum));
    }
    if maximum > 1.0 {
        result = result
            .map(|component| target + (component - target) * (1.0 - target) / (maximum - target));
    }
    result.map(|component| component.clamp(0.0, 1.0))
}

fn set_saturation(color: [f32; 3], target: f32) -> [f32; 3] {
    let mut minimum = 0;
    let mut maximum = 0;
    for channel in 1..3 {
        if color[channel] < color[minimum] {
            minimum = channel;
        }
        if color[channel] > color[maximum] {
            maximum = channel;
        }
    }
    if color[maximum] == color[minimum] {
        return [0.0; 3];
    }
    let middle = 3 - minimum - maximum;
    let mut result = [0.0; 3];
    result[middle] = (color[middle] - color[minimum]) * target / (color[maximum] - color[minimum]);
    result[maximum] = target;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODES: [BlendMode; 18] = [
        BlendMode::Normal,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Overlay,
        BlendMode::Darken,
        BlendMode::Lighten,
        BlendMode::ColorDodge,
        BlendMode::ColorBurn,
        BlendMode::HardLight,
        BlendMode::SoftLight,
        BlendMode::Difference,
        BlendMode::Exclusion,
        BlendMode::Hue,
        BlendMode::Saturation,
        BlendMode::Color,
        BlendMode::Luminosity,
        BlendMode::PlusDarker,
        BlendMode::PlusLighter,
    ];

    fn close(actual: [f32; 4], expected: [f32; 4]) {
        for channel in 0..4 {
            assert!(
                (actual[channel] - expected[channel]).abs() < 0.000_01,
                "channel {channel}: {actual:?} != {expected:?}"
            );
        }
    }

    #[test]
    fn opaque_separable_reference_colors() {
        let source = [0.8, 0.2, 0.6, 1.0];
        let destination = [0.3, 0.7, 0.4, 1.0];
        for (mode, expected) in [
            (BlendMode::Normal, [0.8, 0.2, 0.6, 1.0]),
            (BlendMode::Multiply, [0.24, 0.14, 0.24, 1.0]),
            (BlendMode::Screen, [0.86, 0.76, 0.76, 1.0]),
            (BlendMode::Overlay, [0.48, 0.52, 0.48, 1.0]),
            (BlendMode::Darken, [0.3, 0.2, 0.4, 1.0]),
            (BlendMode::Lighten, [0.8, 0.7, 0.6, 1.0]),
            (BlendMode::ColorDodge, [1.0, 0.875, 1.0, 1.0]),
            (BlendMode::ColorBurn, [0.125, 0.0, 0.0, 1.0]),
            (BlendMode::HardLight, [0.72, 0.28, 0.52, 1.0]),
            (
                BlendMode::SoftLight,
                [0.448_633_55, 0.574, 0.446_491_1, 1.0],
            ),
            (BlendMode::Difference, [0.5, 0.5, 0.2, 1.0]),
            (BlendMode::Exclusion, [0.62, 0.62, 0.52, 1.0]),
            (BlendMode::PlusDarker, [0.1, 0.0, 0.0, 1.0]),
            (BlendMode::PlusLighter, [1.0, 0.9, 1.0, 1.0]),
        ] {
            close(over(source, destination, 1.0, mode), expected);
        }
    }

    #[test]
    fn nonseparable_reference_colors_preserve_css_luminosity() {
        let source = [0.8, 0.2, 0.6, 1.0];
        let backdrop = [0.3, 0.7, 0.4, 1.0];
        // Neither pair needs gamut clipping; the values follow the CSS
        // weighted luminosity (.3 R + .59 G + .11 B), rather than HSL.
        for (mode, expected) in [
            (
                BlendMode::Hue,
                [0.797_666_67, 0.397_666_66, 0.664_333_34, 1.0],
            ),
            (BlendMode::Saturation, [0.176_5, 0.776_5, 0.326_5, 1.0]),
            (BlendMode::Color, [0.923, 0.323, 0.723, 1.0]),
            (BlendMode::Luminosity, [0.177, 0.577, 0.277, 1.0]),
        ] {
            close(over(source, backdrop, 1.0, mode), expected);
        }
    }

    #[test]
    fn nonseparable_gamut_clipping_keeps_gray_levels() {
        let source = [0.0, 1.0, 0.0, 1.0];
        let backdrop = [0.9, 0.9, 0.9, 1.0];
        let color = over(source, backdrop, 1.0, BlendMode::Color);
        close(color, [0.756_097_56, 1.0, 0.756_097_56, 1.0]);
        assert!((luminosity([color[0], color[1], color[2]]) - 0.9).abs() < 0.000_01);
        close(over(source, backdrop, 1.0, BlendMode::Saturation), backdrop);
        close(
            over(
                [0.5, 0.5, 0.5, 1.0],
                [0.0, 0.0, 0.0, 1.0],
                1.0,
                BlendMode::Hue,
            ),
            [0.0, 0.0, 0.0, 1.0],
        );
    }

    #[test]
    fn partial_alpha_uses_straight_blend_only_in_overlap() {
        close(
            over(
                [0.4, 0.1, 0.3, 0.5],
                [0.15, 0.35, 0.2, 0.5],
                1.0,
                BlendMode::Multiply,
            ),
            [0.335, 0.26, 0.31, 0.75],
        );
        close(
            over(
                [0.4, 0.1, 0.3, 0.5],
                [0.15, 0.35, 0.2, 0.5],
                0.5,
                BlendMode::Multiply,
            ),
            [0.2425, 0.305, 0.255, 0.625],
        );
    }

    #[test]
    fn additive_operators_use_additive_alpha_and_darker_complements() {
        let source = [0.56, 0.14, 0.42, 0.7];
        let destination = [0.24, 0.56, 0.32, 0.8];
        close(
            over(source, destination, 1.0, BlendMode::PlusLighter),
            [0.8, 0.7, 0.74, 1.0],
        );
        close(
            over(source, destination, 1.0, BlendMode::PlusDarker),
            [0.3, 0.2, 0.24, 1.0],
        );
        for mode in [BlendMode::PlusLighter, BlendMode::PlusDarker] {
            close(
                over([0.4, 0.1, 0.3, 0.5], [0.1, 0.4, 0.2, 0.5], 0.5, mode),
                [0.3, 0.45, 0.35, 0.75],
            );
        }
    }

    #[test]
    fn transparent_inputs_and_zero_opacity_have_no_hidden_color() {
        let pixel = [0.2, 0.35, 0.1, 0.5];
        for mode in MODES {
            close(over([0.0; 4], pixel, 1.0, mode), pixel);
            close(over(pixel, [0.0; 4], 0.5, mode), [0.1, 0.175, 0.05, 0.25]);
            close(over(pixel, pixel, 0.0, mode), pixel);
            close(over([0.0; 4], [0.0; 4], 1.0, mode), [0.0; 4]);
        }
    }

    #[test]
    fn dodge_burn_endpoints_and_soft_light_polynomial_are_finite() {
        close(
            over(
                [1.0, 1.0, 0.0, 1.0],
                [0.0, 0.5, 0.5, 1.0],
                1.0,
                BlendMode::ColorDodge,
            ),
            [0.0, 1.0, 0.5, 1.0],
        );
        close(
            over(
                [0.0, 0.0, 1.0, 1.0],
                [1.0, 0.5, 0.5, 1.0],
                1.0,
                BlendMode::ColorBurn,
            ),
            [1.0, 0.0, 0.5, 1.0],
        );
        close(
            over(
                [1.0, 1.0, 1.0, 1.0],
                [0.0, 0.125, 0.25, 1.0],
                1.0,
                BlendMode::SoftLight,
            ),
            [0.0, 0.34375, 0.5, 1.0],
        );
    }

    #[test]
    fn every_mode_stays_premultiplied_at_alpha_and_color_endpoints() {
        for mode in MODES {
            for source_alpha in [0.0, 0.01, 0.5, 1.0] {
                for destination_alpha in [0.0, 0.01, 0.5, 1.0] {
                    for color in [[0.0, 1.0, 0.5], [1.0, 0.0, 0.0], [1.0; 3], [0.0; 3]] {
                        let source = [
                            color[0] * source_alpha,
                            color[1] * source_alpha,
                            color[2] * source_alpha,
                            source_alpha,
                        ];
                        let destination = [
                            color[2] * destination_alpha,
                            color[0] * destination_alpha,
                            color[1] * destination_alpha,
                            destination_alpha,
                        ];
                        let result = over(source, destination, 0.65, mode);
                        assert!((0.0..=1.0).contains(&result[3]), "{mode:?}: {result:?}");
                        for channel in &result[..3] {
                            assert!(
                                channel.is_finite()
                                    && *channel >= 0.0
                                    && *channel <= result[3] + f32::EPSILON,
                                "{mode:?}: {result:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}
