//! Shared admission for actual GPU pixels and fallible image encoders.

use std::io::{self, Write};

use anyhow::{Result, ensure};
use image::{
    ExtendedColorType, ImageEncoder,
    codecs::png::{CompressionType, FilterType, PngEncoder},
};

use crate::{CandidateFrame, MAX_WIRE_BYTES};

pub(crate) fn dimensions(frame: &CandidateFrame) -> Result<(u32, u32)> {
    let bytes = crate::window::rgba_len(frame.size)?;
    ensure!(
        frame.token.sequence != 0 && frame.pixels.len() == bytes,
        "inert or malformed terminal RGBA frame"
    );
    Ok((
        u32::try_from(frame.size.width.0)?,
        u32::try_from(frame.size.height.0)?,
    ))
}

pub(crate) fn png(frame: &CandidateFrame, width: u32, height: u32) -> Result<Vec<u8>> {
    let mut output = FrameBytes::new(MAX_WIRE_BYTES);
    PngEncoder::new_with_quality(&mut output, CompressionType::Fast, FilterType::Adaptive)
        .write_image(&frame.pixels, width, height, ExtendedColorType::Rgba8)?;
    Ok(output.into_vec())
}

/// The encoder itself must fail before allocating an oversized wire string.
pub(crate) struct FrameBytes {
    bytes: Vec<u8>,
    limit: usize,
}

impl FrameBytes {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.bytes
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }
}

impl Write for FrameBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other(
                "terminal image exceeds its pending budget",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
