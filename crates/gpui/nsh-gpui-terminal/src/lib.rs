//! A terminal platform for the same GPUI scene used by NSH desktop apps.
//!
//! The embedding host owns terminal input, protocol negotiation and output.
//! Drawing produces a [`CandidateFrame`]; only the sole output owner's receipt
//! may call [`TerminalPlatform::mark_presented`]. GPU completion is not terminal
//! presentation, and input still needs the NSH surface's original owner lease.
//!
//! This crate creates no terminal reader or writer and no application runtime.
//! Embed the existing `nsh_platform_gpui::AppView` using the host's EngineConfig.

#![cfg(target_os = "linux")]

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

/// The maximum tightly packed RGBA allocation for one offscreen scene.
pub const MAX_PIXEL_BYTES: usize = 64 * 1024 * 1024;

/// The maximum complete per-frame wire transaction, including ANSI controls.
pub const MAX_WIRE_BYTES: usize = 8 * 1024 * 1024;

/// An authentic draw's identity in one terminal viewport generation.
///
/// The default token is inert. A token is issued only after a complete GPU
/// readback, and does not itself grant input authority or prove output flush.
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
    /// The identity to pair with this draw's native NSH input snapshot.
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
