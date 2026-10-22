//! A terminal platform for the same GPUI scene used by NSH desktop apps.
//!
//! The embedding host owns terminal input, protocol negotiation and output.
//! Drawing produces a [`SubmittedFrame`] whose owned readback completes away
//! from the native input thread. Only the sole output owner's receipt
//! may call [`TerminalPlatform::mark_presented`]. GPU completion is not terminal
//! presentation. Input follows GPUI's ordinary native route independently.
//!
//! This crate creates no terminal reader or writer and no application runtime.
//! Embed the existing `nsh_platform_gpui::AppView` using the host's EngineConfig.

#![cfg(any(target_os = "linux", target_os = "macos", windows))]

mod dispatcher;
pub mod inline;
pub mod kitty;
mod platform;
mod window;
mod wire;

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

pub use platform::TerminalPlatform;

/// An authentic draw's identity in one terminal viewport generation.
///
/// The default token is inert. A token is issued after successful GPU
/// submission, and does not grant input authority or prove output flush.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct FrameToken {
    pub(crate) issuer: Option<Arc<()>>,
    pub(crate) sequence: u64,
    pub(crate) viewport_generation: u64,
}

impl FrameToken {
    /// The draw sequence; zero is the inert default.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The geometry generation, revoked immediately when the terminal resizes.
    pub fn viewport_generation(&self) -> u64 {
        self.viewport_generation
    }
}

impl PartialEq for FrameToken {
    fn eq(&self, other: &Self) -> bool {
        self.sequence == other.sequence
            && self.viewport_generation == other.viewport_generation
            && match (&self.issuer, &other.issuer) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Eq for FrameToken {}

impl Hash for FrameToken {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.issuer.as_ref().map(Arc::as_ptr).hash(state);
        self.sequence.hash(state);
        self.viewport_generation.hash(state);
    }
}

/// An authentic GPU submission whose readback belongs to the receiving host.
///
/// Move this ticket to a background executor. Completion retains the draw's
/// identity and pixels without sampling application state or authorizing input.
#[non_exhaustive]
pub struct SubmittedFrame {
    pub(crate) token: FrameToken,
    pub(crate) readback: gpui_wgpu::SubmittedReadback,
}

impl SubmittedFrame {
    /// The identity paired with the scene at its original GPU submission.
    pub fn token(&self) -> FrameToken {
        self.token.clone()
    }

    /// The submitted GPU target's actual dimensions.
    pub fn size(&self) -> gpui::Size<gpui::DevicePixels> {
        self.readback.size()
    }

    /// The tightly packed RGBA8 length produced by a successful completion.
    pub fn pixel_len(&self) -> usize {
        self.readback.pixel_len()
    }

    /// Waits for readback and copies RGBA pixels on the calling background thread.
    pub fn complete(self) -> anyhow::Result<CandidateFrame> {
        let size = self.readback.size();
        let pixels = self.readback.complete()?;
        Ok(CandidateFrame {
            token: self.token,
            size,
            pixels,
        })
    }
}

/// A completed GPU readback awaiting the terminal output owner's admission.
///
/// Pixels are tightly packed top-down RGBA8. A slow link may replace a pending
/// candidate; an already-started output transaction must finish indivisibly.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct CandidateFrame {
    pub(crate) token: FrameToken,
    pub(crate) size: gpui::Size<gpui::DevicePixels>,
    pub(crate) pixels: Vec<u8>,
}

impl CandidateFrame {
    /// The original draw's identity for its output transaction and diagnostics.
    pub fn token(&self) -> FrameToken {
        self.token.clone()
    }

    /// The actual device-pixel dimensions of the GPU target.
    pub fn size(&self) -> gpui::Size<gpui::DevicePixels> {
        self.size
    }

    /// The GPU-produced tightly packed RGBA8 bytes.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}
