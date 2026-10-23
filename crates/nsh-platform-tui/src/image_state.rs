//! One mounted image's sources, asynchronous variant tickets and presentation.
//! Raw decoded data stays in the surface cache; node state holds shared display
//! variants and samples animation/fade timing from the same monotonic clock.

use std::sync::Arc;

use nsh_core::{
    ImageOptions, ImageSource, ResizeMode,
    access::Access,
    geometry::{Bounds, Points, Size, point, px},
};

use crate::image::{AnimatedRaster, Raster, fitted_frame};

/// Which source an asynchronous load or display variant belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageRole {
    Main,
    Placeholder,
}

/// Immutable identity and geometry for a load/blur completion. A source or
/// option change invalidates outstanding tickets without mutating shared data.
#[derive(Clone, Debug)]
pub(crate) struct ImageRequest {
    owner: Arc<()>,
    pub(crate) role: ImageRole,
    pub(crate) source: ImageSource,
    pub(crate) revision: u64,
    pub(crate) blur_radius: Points,
    pub(crate) size: Size<Points>,
    pub(crate) mode: ResizeMode,
}

impl PartialEq for ImageRequest {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
            && self.role == other.role
            && self.source == other.source
            && self.revision == other.revision
            && self.blur_radius == other.blur_radius
            && self.size == other.size
            && self.mode == other.mode
    }
}

impl ImageRequest {
    /// Source-pixel radii that produce the requested blur in local points,
    /// including anisotropic stretching and a declared natural image size.
    pub(crate) fn blur_radii(&self, image: &AnimatedRaster) -> (f32, f32) {
        let Some(raster) = image.frames.first() else {
            return (0.0, 0.0);
        };
        if self.blur_radius <= Points::ZERO {
            return (0.0, 0.0);
        }
        let natural = self
            .source
            .size
            .filter(|size| {
                size.width.is_finite()
                    && size.height.is_finite()
                    && size.width > Points::ZERO
                    && size.height > Points::ZERO
            })
            .unwrap_or_else(|| Size::new(px(raster.width as f32), px(raster.height as f32)));
        let size = if self.mode == ResizeMode::Center {
            natural
        } else {
            let frame = Bounds::new(
                point(Points::ZERO, Points::ZERO),
                Size::new(
                    if self.size.width > Points::ZERO {
                        self.size.width
                    } else {
                        natural.width
                    },
                    if self.size.height > Points::ZERO {
                        self.size.height
                    } else {
                        natural.height
                    },
                ),
            );
            if self.mode == ResizeMode::ScaleDown {
                self.mode
                    .fit(natural, frame.size, nsh_core::ObjectPosition::CENTER)
                    .size
            } else {
                fitted_frame(raster, frame, self.mode).size
            }
        };
        (
            self.blur_radius / size.width.max(px(f32::MIN_POSITIVE)) * raster.width as f32,
            self.blur_radius / size.height.max(px(f32::MIN_POSITIVE)) * raster.height as f32,
        )
    }
}

struct Loaded {
    animation: Arc<AnimatedRaster>,
    started: f64,
}

impl Loaded {
    fn index(&self, now: f64) -> usize {
        self.animation.frame_index(elapsed(self.started, now))
    }
    fn raster(&self, now: f64) -> &Arc<Raster> {
        &self.animation.frames[self.index(now)]
    }
    fn animating(&self, now: f64) -> bool {
        !self.animation.finished(elapsed(self.started, now))
    }
}

/// One already processed image layer. Placeholder alpha stays at one while
/// the loaded source fades over it, preserving the native image semantics.
#[derive(Clone, Copy)]
pub(crate) struct ImageLayer<'a> {
    pub(crate) raster: &'a Arc<Raster>,
    pub(crate) source: &'a ImageSource,
    pub(crate) opacity: f32,
}

#[derive(Default)]
pub(crate) struct ImagePresentation<'a> {
    pub(crate) placeholder: Option<ImageLayer<'a>>,
    pub(crate) main: Option<ImageLayer<'a>>,
}

/// Canonical state for a mounted Image; no terminal IO or decoding occurs here.
#[derive(Default)]
pub(crate) struct ImageState {
    // Allocated only for image nodes. Outstanding tickets retain this identity
    // after removal, so recycling a node ID cannot accept an old completion.
    owner: Option<Arc<()>>,
    source: Option<ImageSource>,
    mode: ResizeMode,
    options: ImageOptions,
    size: Size<Points>,
    revision: u64,
    main: Option<Loaded>,
    placeholder: Option<Loaded>,
    main_pending: bool,
    placeholder_pending: bool,
    fade_started: Option<f64>,
    observed: Option<(Option<usize>, Option<usize>, u32)>,
}

impl ImageState {
    pub(crate) fn mode(&self) -> ResizeMode {
        self.mode
    }

    pub(crate) fn position(&self) -> nsh_core::ObjectPosition {
        self.options.position
    }

    /// Updates mounted props and returns only currently needed asynchronous
    /// requests. The main source always precedes its default source.
    pub(crate) fn update(
        &mut self,
        source: Option<ImageSource>,
        mode: ResizeMode,
        mut options: ImageOptions,
        now: f64,
    ) -> Vec<ImageRequest> {
        options.blur_radius = positive_points(options.blur_radius);
        options.fade_ms = options.fade_ms.map(positive);
        if source.is_some() || options.placeholder.is_some() {
            self.owner.get_or_insert_with(|| Arc::new(()));
        }
        let changed = source != self.source;
        let placeholder_changed = options.placeholder != self.options.placeholder;
        let blur_changed = options.blur_radius != self.options.blur_radius;
        let geometry_changed = mode != self.mode && options.blur_radius > Points::ZERO;
        if changed || placeholder_changed || blur_changed || geometry_changed {
            self.revision = self.revision.wrapping_add(1);
        }
        if changed {
            self.main = None;
            self.main_pending = source.is_some();
            self.fade_started = None;
        }
        if placeholder_changed {
            self.placeholder = None;
        }
        self.source = source;
        self.mode = mode;
        self.options = options;
        if blur_changed || geometry_changed {
            self.main_pending = self.source.is_some();
        }
        if self.main.is_none() || self.fading(now) {
            if placeholder_changed || blur_changed || geometry_changed || self.placeholder.is_none()
            {
                self.placeholder_pending = self.options.placeholder.is_some();
            }
        } else {
            self.placeholder_pending = false;
        }
        if self.options.placeholder.is_none() {
            self.placeholder = None;
            self.placeholder_pending = false;
        }
        if self.source.is_some() && self.main.is_none() && !self.main_pending {
            self.main_pending = true;
            self.revision = self.revision.wrapping_add(1);
        }
        self.requests()
    }

    /// Image blur is specified in points; changing the fitted dimensions
    /// therefore needs a new pixel-space blur variant, while sharp images
    /// retain their shared raster through resize.
    pub(crate) fn resize(&mut self, size: Size<Points>) -> Vec<ImageRequest> {
        let size = Size::new(positive_points(size.width), positive_points(size.height));
        if self.size != size {
            self.size = size;
            if self.options.blur_radius > Points::ZERO {
                self.revision = self.revision.wrapping_add(1);
                self.main_pending = self.source.is_some();
                self.placeholder_pending = self.options.placeholder.is_some()
                    && (self.main.is_none() || self.fade_started.is_some());
            }
        }
        self.requests()
    }

    pub(crate) fn requests(&self) -> Vec<ImageRequest> {
        [
            (ImageRole::Main, self.main_pending, self.source.as_ref()),
            (
                ImageRole::Placeholder,
                self.placeholder_pending,
                self.options.placeholder.as_ref(),
            ),
        ]
        .into_iter()
        .filter_map(|(role, pending, source)| {
            pending
                .then_some(source)
                .flatten()
                .map(|source| ImageRequest {
                    owner: self
                        .owner
                        .as_ref()
                        .expect("pending image has an owner")
                        .clone(),
                    role,
                    source: source.clone(),
                    revision: self.revision,
                    blur_radius: self.options.blur_radius,
                    size: self.size,
                    mode: self.mode,
                })
        })
        .collect()
    }

    pub(crate) fn accepts(&self, request: &ImageRequest) -> bool {
        if request.revision != self.revision
            || !self
                .owner
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &request.owner))
        {
            return false;
        }
        let (pending, source) = match request.role {
            ImageRole::Main => (self.main_pending, self.source.as_ref()),
            ImageRole::Placeholder => (self.placeholder_pending, self.options.placeholder.as_ref()),
        };
        pending
            && source == Some(&request.source)
            && request.blur_radius == self.options.blur_radius
    }

    /// Progress belongs only to the current unfinished main source. Shared
    /// placeholder and stale source reads never emit Image.onProgress.
    pub(crate) fn accepts_progress(&self, uri: &str) -> bool {
        self.main_pending
            && self
                .source
                .as_ref()
                .is_some_and(|source| source.uri.as_ref() == uri)
    }

    pub(crate) fn install(
        &mut self,
        request: &ImageRequest,
        animation: Arc<AnimatedRaster>,
        now: f64,
    ) -> bool {
        self.install_inner(request, animation, now, true)
    }

    /// An immediate cache hit appears directly rather than replaying a load
    /// transition for pixels the host already held.
    pub(crate) fn install_cached(
        &mut self,
        request: &ImageRequest,
        animation: Arc<AnimatedRaster>,
        now: f64,
    ) -> bool {
        self.install_inner(request, animation, now, false)
    }

    fn install_inner(
        &mut self,
        request: &ImageRequest,
        animation: Arc<AnimatedRaster>,
        now: f64,
        waited: bool,
    ) -> bool {
        if !self.accepts(request) || animation.frames.is_empty() {
            return false;
        }
        match request.role {
            ImageRole::Main => {
                let replacing = self.main.is_some();
                let started = self.main.as_ref().map_or(now, |loaded| loaded.started);
                self.main = Some(Loaded { animation, started });
                self.main_pending = false;
                self.fade_started =
                    (!replacing && waited && self.fade_duration() > 0.0).then_some(now);
                if self.fade_started.is_none() {
                    self.placeholder = None;
                    self.placeholder_pending = false;
                }
            }
            ImageRole::Placeholder => {
                let started = self
                    .placeholder
                    .as_ref()
                    .map_or(now, |loaded| loaded.started);
                self.placeholder = Some(Loaded { animation, started });
                self.placeholder_pending = false;
            }
        }
        self.observed = None;
        true
    }

    pub(crate) fn fail(&mut self, request: &ImageRequest) -> bool {
        if !self.accepts(request) {
            return false;
        }
        match request.role {
            ImageRole::Main => {
                self.main_pending = false;
            }
            ImageRole::Placeholder => self.placeholder_pending = false,
        }
        true
    }

    /// Revokes denied pixels and tickets immediately. A permitted default
    /// source can remain as fallback, and a restored grant makes a missing
    /// source requestable again without changing its mounted metadata.
    pub(crate) fn revoke(&mut self, access: &Access) -> bool {
        let main_denied = self
            .source
            .as_ref()
            .is_some_and(|source| !access.allows_media(&source.uri));
        let placeholder_denied = self
            .options
            .placeholder
            .as_ref()
            .is_some_and(|source| !access.allows_media(&source.uri));
        let mut changed = false;
        if main_denied {
            changed |= self.main.take().is_some() || self.main_pending;
            self.main_pending = false;
            self.fade_started = None;
        } else if self.source.is_some() && self.main.is_none() && !self.main_pending {
            self.main_pending = true;
            changed = true;
        }
        if placeholder_denied {
            changed |= self.placeholder.take().is_some() || self.placeholder_pending;
            self.placeholder_pending = false;
        } else if self.options.placeholder.is_some()
            && self.main.is_none()
            && self.placeholder.is_none()
            && !self.placeholder_pending
        {
            self.placeholder_pending = true;
            changed = true;
        }
        if changed {
            self.revision = self.revision.wrapping_add(1);
            self.observed = None;
        }
        changed
    }

    pub(crate) fn presentation(&self, now: f64) -> ImagePresentation<'_> {
        let alpha = self.main_alpha(now);
        ImagePresentation {
            main: self
                .main
                .as_ref()
                .zip(self.source.as_ref())
                .map(|(loaded, source)| ImageLayer {
                    raster: loaded.raster(now),
                    source,
                    opacity: alpha,
                }),
            placeholder: if self.main.is_none() || alpha < 1.0 {
                self.placeholder
                    .as_ref()
                    .zip(self.options.placeholder.as_ref())
                    .map(|(loaded, source)| ImageLayer {
                        raster: loaded.raster(now),
                        source,
                        opacity: 1.0,
                    })
            } else {
                None
            },
        }
    }

    /// Advances the retained presentation and releases an obsolete placeholder
    /// after its fade. Returned changes request a redraw without JS work.
    pub(crate) fn advance(&mut self, now: f64) -> bool {
        let alpha = self.main_alpha(now);
        let observed = (
            self.main.as_ref().map(|loaded| loaded.index(now)),
            self.placeholder
                .as_ref()
                .filter(|_| self.main.is_none() || alpha < 1.0)
                .map(|loaded| loaded.index(now)),
            alpha.to_bits(),
        );
        let changed = self.observed.replace(observed) != Some(observed);
        if self.fade_started.is_some() && alpha >= 1.0 {
            self.fade_started = None;
            self.placeholder = None;
            self.placeholder_pending = false;
        }
        changed
    }

    pub(crate) fn animating(&self, now: f64) -> bool {
        self.fading(now)
            || if let Some(main) = &self.main {
                main.animating(now)
            } else {
                self.placeholder
                    .as_ref()
                    .is_some_and(|loaded| loaded.animating(now))
            }
    }

    fn fade_duration(&self) -> f64 {
        f64::from(self.options.fade_ms.unwrap_or(0.0)) / 1000.0
    }
    fn fading(&self, now: f64) -> bool {
        self.main.is_some() && self.main_alpha(now) < 1.0
    }
    fn main_alpha(&self, now: f64) -> f32 {
        if self.main.is_none() {
            return 0.0;
        }
        match (self.fade_started, self.fade_duration()) {
            (Some(started), duration) if duration > 0.0 => {
                ((now - started).max(0.0) / duration).clamp(0.0, 1.0) as f32
            }
            _ => 1.0,
        }
    }
}

fn elapsed(started: f64, now: f64) -> u64 {
    ((now - started).max(0.0) * 1000.0) as u64
}
fn positive_points(value: Points) -> Points {
    if value.is_finite() {
        value.max(Points::ZERO)
    } else {
        Points::ZERO
    }
}
fn positive(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(uri: &str) -> ImageSource {
        ImageSource {
            uri: uri.into(),
            scales: nsh_core::ImageScales::NONE,
            data: None,
            size: None,
        }
    }
    fn image(color: [u8; 4]) -> Arc<AnimatedRaster> {
        Arc::new(AnimatedRaster {
            frames: vec![Arc::new(Raster {
                width: 1,
                height: 1,
                rgba: color.to_vec(),
            })],
            delays: Vec::new(),
            plays: 1,
        })
    }

    #[test]
    fn default_source_survives_failure_and_late_old_source_cannot_replace_it() {
        let mut state = ImageState::default();
        let options = ImageOptions {
            placeholder: Some(source("asset:/default")),
            ..Default::default()
        };
        let requests = state.update(
            Some(source("asset:/first")),
            ResizeMode::Contain,
            options.clone(),
            0.0,
        );
        assert_eq!(requests[0].role, ImageRole::Main);
        assert_eq!(requests[1].role, ImageRole::Placeholder);
        let placeholder = image([0, 0, 255, 255]);
        assert!(state.install(&requests[1], placeholder.clone(), 0.0));
        assert!(state.fail(&requests[0]));
        assert!(state.presentation(1.0).main.is_none());
        assert!(Arc::ptr_eq(
            state.presentation(1.0).placeholder.unwrap().raster,
            &placeholder.frames[0]
        ));
        let next = state.update(
            Some(source("asset:/second")),
            ResizeMode::Contain,
            options,
            1.0,
        );
        assert!(state.accepts_progress("asset:/second"));
        assert!(!state.accepts_progress("asset:/first"));
        assert!(!state.install(&requests[0], image([255, 0, 0, 255]), 2.0));
        assert!(state.install(&next[0], image([0, 255, 0, 255]), 2.0));
        assert_eq!(
            state.presentation(2.0).main.unwrap().raster.rgba,
            [0, 255, 0, 255]
        );
    }

    #[test]
    fn loaded_source_fades_over_full_placeholder_and_cached_source_skips_fade() {
        let mut state = ImageState::default();
        let options = ImageOptions {
            placeholder: Some(source("asset:/default")),
            fade_ms: Some(200.0),
            ..Default::default()
        };
        let requests = state.update(
            Some(source("asset:/main")),
            ResizeMode::Cover,
            options.clone(),
            0.0,
        );
        state.install_cached(&requests[1], image([0, 0, 255, 255]), 0.0);
        state.install(&requests[0], image([255, 0, 0, 255]), 1.0);
        let middle = state.presentation(1.1);
        assert_eq!(middle.placeholder.unwrap().opacity, 1.0);
        assert!((middle.main.unwrap().opacity - 0.5).abs() < 0.001);
        assert!(state.animating(1.1));
        state.advance(1.3);
        assert!(state.presentation(1.3).placeholder.is_none());
        assert!(!state.animating(1.3));
        let request = state
            .update(
                Some(source("asset:/cached")),
                ResizeMode::Cover,
                options,
                2.0,
            )
            .remove(0);
        state.install_cached(&request, image([255, 255, 0, 255]), 2.0);
        assert_eq!(state.presentation(2.0).main.unwrap().opacity, 1.0);
        assert!(state.requests().is_empty());
    }

    #[test]
    fn blur_resize_rejects_stale_variants_and_uses_display_pixel_scale() {
        let mut state = ImageState::default();
        state.resize(Size::new(px(20.0), px(10.0)));
        let request = state
            .update(
                Some(source("asset:/main")),
                ResizeMode::Stretch,
                ImageOptions {
                    blur_radius: px(4.0),
                    ..Default::default()
                },
                0.0,
            )
            .remove(0);
        let raw = Arc::new(AnimatedRaster {
            frames: vec![Arc::new(Raster {
                width: 40,
                height: 40,
                rgba: vec![255; 40 * 40 * 4],
            })],
            delays: Vec::new(),
            plays: 1,
        });
        assert_eq!(request.blur_radii(&raw), (8.0, 16.0));
        let resized = state.resize(Size::new(px(40.0), px(20.0)));
        assert!(!state.accepts(&request));
        assert_eq!(resized[0].blur_radii(&raw), (4.0, 8.0));
        assert!(state.install(&resized[0], raw, 0.1));
        assert!(state.resize(Size::new(px(40.0), px(20.0))).is_empty());
    }

    #[test]
    fn animated_main_and_placeholder_advance_without_copying_frames() {
        let mut state = ImageState::default();
        let frames = vec![
            image([255, 0, 0, 255]).frames[0].clone(),
            image([0, 0, 255, 255]).frames[0].clone(),
        ];
        let animation = Arc::new(AnimatedRaster {
            frames: frames.clone(),
            delays: vec![40, 60],
            plays: 1,
        });
        let requests = state.update(
            Some(source("asset:/main")),
            ResizeMode::Cover,
            ImageOptions {
                placeholder: Some(source("asset:/default")),
                ..Default::default()
            },
            0.0,
        );
        state.install(&requests[1], animation.clone(), 0.0);
        assert!(state.animating(0.01));
        assert!(Arc::ptr_eq(
            state.presentation(0.05).placeholder.unwrap().raster,
            &frames[1]
        ));
        state.install(&requests[0], animation, 1.0);
        assert!(Arc::ptr_eq(
            state.presentation(1.05).main.unwrap().raster,
            &frames[1]
        ));
        assert!(!state.animating(1.2));
    }

    #[test]
    fn access_revocation_removes_denied_pixels_and_stale_tickets_then_allows_retry() {
        let mut state = ImageState::default();
        let options = ImageOptions {
            placeholder: Some(source("asset:/default")),
            fade_ms: Some(200.0),
            ..Default::default()
        };
        let requests = state.update(
            Some(source("https://images.example/main.png")),
            ResizeMode::Cover,
            options,
            0.0,
        );
        state.install(&requests[1], image([0, 0, 255, 255]), 0.0);
        state.install(&requests[0], image([255, 0, 0, 255]), 1.0);
        let denied = Access {
            declared: nsh_core::access::Capabilities::default(),
            granted: None,
        };
        assert!(state.revoke(&denied));
        assert!(state.presentation(1.1).main.is_none());
        assert_eq!(
            state.presentation(1.1).placeholder.unwrap().raster.rgba,
            [0, 0, 255, 255]
        );
        assert!(!state.accepts_progress("https://images.example/main.png"));
        assert!(!state.install(&requests[0], image([255, 0, 0, 255]), 1.2));
        assert!(state.revoke(&Access::all()));
        let restored = state.requests().remove(0);
        assert!(state.accepts(&restored));
        assert!(state.install_cached(&restored, image([0, 255, 0, 255]), 2.0));
        assert_eq!(
            state.presentation(2.0).main.unwrap().raster.rgba,
            [0, 255, 0, 255]
        );
    }

    #[test]
    fn a_recycled_node_cannot_accept_an_earlier_images_completion() {
        let mut removed = ImageState::default();
        let stale = removed
            .update(
                Some(source("asset:/same")),
                ResizeMode::Contain,
                ImageOptions::default(),
                0.0,
            )
            .remove(0);
        drop(removed);
        let mut replacement = ImageState::default();
        let current = replacement
            .update(
                Some(source("asset:/same")),
                ResizeMode::Contain,
                ImageOptions::default(),
                1.0,
            )
            .remove(0);
        assert_eq!(stale.revision, current.revision);
        assert_ne!(stale, current);
        assert!(!replacement.install(&stale, image([255, 0, 0, 255]), 2.0));
        assert!(replacement.install(&current, image([0, 255, 0, 255]), 2.0));
    }
}
