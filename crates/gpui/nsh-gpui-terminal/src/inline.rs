//! Full-viewport Sixel and iTerm2 images for an already owned alternate screen.
//!
//! The embedding driver negotiates image support, real viewport geometry and
//! mouse units independently. Sixel additionally needs a verified static display
//! mode (DECSDM 80) that the driver saves and restores through its sole reader
//! and writer. These codecs neither query stdin nor change that session mode.
//!
//! Each transaction erases only the owned alternate viewport, replaces its full
//! image and restores the cursor to its top left. Erase it again before leaving
//! that alternate screen; never apply this full-viewport contract to the user's
//! main screen. Neither protocol supplies a per-image success acknowledgement.
//! The sole output owner's complete write and flush receipt is the available
//! publication fact; encoding completion does not admit input or prove scanout.

use std::io::Write;

use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::{CandidateFrame, FrameToken, MAX_WIRE_BYTES, wire};

/// A negotiated image transport for the same complete native GPUI scene.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InlineProtocol {
    /// Palette-based Sixel; the terminal must clip a final partial six-row band.
    Sixel,
    /// OSC 1337 File-streaming PNG, sized to the actual viewport's character cells.
    /// Its payload may exceed generic OSC limits; the embedder must establish
    /// this protocol's streaming support on its actual terminal transport.
    Iterm2,
}

/// A complete bounded transaction awaiting the sole terminal writer's receipt.
#[derive(Debug)]
#[non_exhaustive]
pub struct InlineFrame {
    token: FrameToken,
    bytes: Vec<u8>,
}

impl InlineFrame {
    /// Encodes the actual GPU readback without rescaling its pixels or reducing
    /// the selected codec's colour palette.
    ///
    /// Run on the existing graphics worker, outside the output owner's IO lock.
    /// `synchronized` is an independently negotiated synchronized-update
    /// capability. All its controls count towards the complete 8 MiB wire limit.
    pub fn encode(
        frame: &CandidateFrame,
        protocol: InlineProtocol,
        columns: u16,
        rows: u16,
        synchronized: bool,
    ) -> Result<Self> {
        Self::with_limit(frame, protocol, columns, rows, synchronized, MAX_WIRE_BYTES)
    }

    fn with_limit(
        frame: &CandidateFrame,
        protocol: InlineProtocol,
        columns: u16,
        rows: u16,
        synchronized: bool,
        limit: usize,
    ) -> Result<Self> {
        ensure!(
            columns > 0 && rows > 0,
            "terminal cell dimensions must be nonzero"
        );
        let (width, height) = wire::dimensions(frame)?;
        let mut output = wire::FrameBytes::new(limit);
        if synchronized {
            output.write_all(b"\x1b[?2026h")?;
        }
        output.write_all(b"\x1b[H\x1b[2J")?;
        let suffix = if synchronized {
            b"\x1b[H\x1b[?2026l".as_slice()
        } else {
            b"\x1b[H".as_slice()
        };
        match protocol {
            InlineProtocol::Sixel => icy_sixel::sixel_encode_into(
                frame.pixels(),
                usize::try_from(width)?,
                usize::try_from(height)?,
                &icy_sixel::EncodeOptions::default(),
                &mut output,
            )?,
            InlineProtocol::Iterm2 => {
                let png = wire::png(frame, width, height)?;
                // Cell dimensions cover the same viewport even when a terminal
                // has a Retina preference that changes the meaning of "px".
                let header = format!(
                    "\x1b]1337;File=inline=1;size={};width={columns};height={rows};preserveAspectRatio=0:",
                    png.len()
                );
                let total = png
                    .len()
                    .checked_add(2)
                    .and_then(|n| (n / 3).checked_mul(4))
                    .and_then(|n| n.checked_add(header.len()))
                    .and_then(|n| n.checked_add(1 + suffix.len()))
                    .and_then(|n| n.checked_add(output.len()))
                    .ok_or_else(|| anyhow::anyhow!("terminal image wire size overflow"))?;
                ensure!(
                    total <= limit,
                    "terminal image exceeds its complete wire budget"
                );
                output.write_all(header.as_bytes())?;
                output.write_all(STANDARD.encode(&png).as_bytes())?;
                output.write_all(b"\x07")?;
            }
        }
        output.write_all(suffix)?;
        Ok(Self {
            token: frame.token(),
            bytes: output.into_vec(),
        })
    }

    /// The original draw's identity, still awaiting its actual output receipt.
    pub fn token(&self) -> FrameToken {
        self.token.clone()
    }

    /// Complete indivisible bytes, including any synchronized-update controls.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Clears only the complete alternate viewport already owned by the embedder.
///
/// Flush before leaving the alternate screen, then restore the driver's saved
/// DECSDM and input modes. This neither erases scrollback nor deletes Kitty ids.
pub fn cleanup() -> &'static [u8] {
    b"\x1b[H\x1b[2J"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> CandidateFrame {
        CandidateFrame {
            token: FrameToken {
                issuer: Some(std::sync::Arc::new(())),
                sequence: 7,
                viewport_generation: 2,
            },
            size: gpui::size(gpui::DevicePixels(2), gpui::DevicePixels(6)),
            pixels: [
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255, 255, 0, 0, 255,
                0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255, 255, 0, 0, 255, 0, 255, 0, 255,
                0, 0, 255, 255, 255, 255, 255, 255,
            ]
            .to_vec(),
        }
    }

    #[test]
    fn the_same_native_scene_survives_both_inline_transports() {
        let source = frame();
        for protocol in [InlineProtocol::Sixel, InlineProtocol::Iterm2] {
            let encoded = InlineFrame::encode(&source, protocol, 1, 1, false).unwrap();
            assert_eq!(encoded.token(), source.token());
            let payload = encoded
                .bytes()
                .strip_prefix(b"\x1b[H\x1b[2J")
                .unwrap()
                .strip_suffix(b"\x1b[H")
                .unwrap();
            let pixels = match protocol {
                InlineProtocol::Sixel => {
                    let decoded = icy_sixel::SixelImage::decode(payload).unwrap();
                    assert_eq!((decoded.width, decoded.height), (2, 6));
                    decoded.pixels
                }
                InlineProtocol::Iterm2 => {
                    let payload = std::str::from_utf8(payload).unwrap();
                    let (header, png) = payload
                        .strip_suffix('\x07')
                        .unwrap()
                        .split_once(':')
                        .unwrap();
                    let header = header
                        .strip_prefix("\x1b]1337;File=inline=1;size=")
                        .unwrap();
                    let (size, geometry) = header.split_once(';').unwrap();
                    assert_eq!(geometry, "width=1;height=1;preserveAspectRatio=0");
                    let png = STANDARD.decode(png).unwrap();
                    assert_eq!(size.parse::<usize>().unwrap(), png.len());
                    let decoded =
                        image::load_from_memory_with_format(&png, image::ImageFormat::Png)
                            .unwrap()
                            .into_rgba8();
                    assert_eq!(decoded.dimensions(), (2, 6));
                    decoded.into_raw()
                }
            };
            assert_eq!(pixels, source.pixels());
        }
    }

    #[test]
    fn whole_transaction_admission_includes_controls_and_rejects_inert_frames() {
        let source = frame();
        for protocol in [InlineProtocol::Sixel, InlineProtocol::Iterm2] {
            let encoded = InlineFrame::encode(&source, protocol, 1, 1, true).unwrap();
            assert!(encoded.bytes().starts_with(b"\x1b[?2026h\x1b[H\x1b[2J"));
            assert!(encoded.bytes().ends_with(b"\x1b[H\x1b[?2026l"));
            assert!(InlineFrame::with_limit(&source, protocol, 1, 1, true, 24).is_err());
            assert!(
                InlineFrame::encode(&CandidateFrame::default(), protocol, 1, 1, false).is_err()
            );
            assert!(InlineFrame::encode(&source, protocol, 0, 1, false).is_err());

            let without_sync = InlineFrame::encode(&source, protocol, 1, 1, false).unwrap();
            let exact = without_sync.bytes().len() + 16;
            assert!(InlineFrame::with_limit(&source, protocol, 1, 1, true, exact - 1).is_err());
            let admitted = InlineFrame::with_limit(&source, protocol, 1, 1, true, exact).unwrap();
            assert_eq!(admitted.bytes().len(), exact);
        }
    }

    #[test]
    fn iterm_file_streaming_roundtrips_a_scene_larger_than_generic_osc_limits() {
        let mut source = frame();
        source.size = gpui::size(gpui::DevicePixels(512), gpui::DevicePixels(512));
        let mut seed = 177u32;
        source.pixels = (0..512 * 512 * 4)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        let encoded = InlineFrame::encode(&source, InlineProtocol::Iterm2, 120, 40, true).unwrap();
        let payload = encoded
            .bytes()
            .strip_prefix(b"\x1b[?2026h\x1b[H\x1b[2J")
            .unwrap()
            .strip_suffix(b"\x1b[H\x1b[?2026l")
            .unwrap();
        assert!(payload.len() > 1_048_576);
        assert!(encoded.bytes().len() <= 8 * 1024 * 1024);
        let payload = std::str::from_utf8(payload).unwrap();
        let (header, png) = payload
            .strip_suffix('\x07')
            .unwrap()
            .split_once(':')
            .unwrap();
        let header = header
            .strip_prefix("\x1b]1337;File=inline=1;size=")
            .unwrap();
        let (size, geometry) = header.split_once(';').unwrap();
        assert_eq!(geometry, "width=120;height=40;preserveAspectRatio=0");
        let png = STANDARD.decode(png).unwrap();
        assert_eq!(size.parse::<usize>().unwrap(), png.len());
        let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .unwrap()
            .into_rgba8();
        assert_eq!(decoded.dimensions(), (512, 512));
        assert_eq!(decoded.as_raw(), source.pixels());
    }
}
