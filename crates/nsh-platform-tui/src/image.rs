//! Bounded image decoding, asset reads and terminal block composition.
//!
//! IO and decoding run on the surface's workers. The painter only samples
//! already decoded pixels, preserving aspect ratio and composing alpha into
//! the buffer's existing foreground/background blocks. Each cell selects the
//! least-error half or quadrant block while retaining its two-color limit.

use std::{
    io::Read,
    path::PathBuf,
    sync::{Arc, OnceLock, Weak},
    time::{Duration, Instant},
};

use nsh_core::{
    Color, ResizeMode,
    access::Access,
    geometry::{Bounds, Points, Size, point, px},
};
use ratatui::{buffer::Buffer, layout::Rect as Area, style::Color as TerminalColor};
use tiny_skia as sk;

use crate::CellMetrics;

const MAX_BYTES: u64 = 64 << 20;
const MAX_REDIRECTS: usize = 20;
const MAX_PIXELS: u64 = 16 << 20;
const MAX_FRAMES: usize = 600;
const MAX_ANIMATION_BYTES: usize = 48 << 20;
const BLUR_CACHE_BYTES: usize = 64 << 20;
const BLUR_CACHE_ENTRIES: usize = 64;

/// Decoded straight RGBA pixels, shared between mounted images and canvases.
#[derive(Clone)]
pub(crate) struct Raster {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
}

/// Decoded frames and their timeline; every frame is already composited by
/// the image decoder, including GIF disposal and transparent frame regions.
#[derive(Debug, Clone)]
pub(crate) struct AnimatedRaster {
    pub(crate) frames: Vec<Arc<Raster>>,
    pub(crate) delays: Vec<u32>,
    /// Complete plays, with zero meaning an unlimited loop.
    pub(crate) plays: u32,
}

impl AnimatedRaster {
    pub(crate) fn animated(&self) -> bool {
        self.frames.len() > 1 && !self.delays.is_empty()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.frames.iter().map(|frame| frame.rgba.len()).sum()
    }

    /// Selects a frame from elapsed monotonic time; finite animations stay on
    /// their last frame after the final complete play.
    pub(crate) fn frame_index(&self, elapsed_ms: u64) -> usize {
        if !self.animated() {
            return 0;
        }
        let duration: u64 = self.delays.iter().map(|&ms| u64::from(ms)).sum();
        if duration == 0 {
            return 0;
        }
        if self.plays != 0 && elapsed_ms >= duration.saturating_mul(u64::from(self.plays)) {
            return self.frames.len() - 1;
        }
        let mut within = elapsed_ms % duration;
        for (index, &delay) in self.delays.iter().enumerate() {
            if within < u64::from(delay) {
                return index.min(self.frames.len() - 1);
            }
            within -= u64::from(delay);
        }
        self.frames.len() - 1
    }

    pub(crate) fn finished(&self, elapsed_ms: u64) -> bool {
        !self.animated()
            || (self.plays != 0
                && elapsed_ms
                    >= self
                        .delays
                        .iter()
                        .map(|&ms| u64::from(ms))
                        .sum::<u64>()
                        .saturating_mul(u64::from(self.plays)))
    }
}

impl std::fmt::Debug for Raster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Raster")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl Raster {
    /// Converts straight RGBA pixels to tiny-skia's premultiplied storage.
    pub(crate) fn pixmap(&self) -> Option<sk::Pixmap> {
        let count = (self.width as usize).checked_mul(self.height as usize)?;
        if self.rgba.len() != count.checked_mul(4)? {
            return None;
        }
        let mut p = sk::Pixmap::new(self.width, self.height)?;
        for (dst, src) in p.pixels_mut().iter_mut().zip(self.rgba.as_chunks::<4>().0) {
            *dst = sk::ColorU8::from_rgba(src[0], src[1], src[2], src[3]).premultiply();
        }
        Some(p)
    }

    /// Converts a completed canvas to straight RGBA storage.
    pub(crate) fn from_pixmap(p: sk::Pixmap) -> Self {
        let mut rgba = Vec::with_capacity(p.data().len());
        for pixel in p.pixels() {
            let c = pixel.demultiply();
            rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
        }
        Self {
            width: p.width(),
            height: p.height(),
            rgba,
        }
    }

    /// Bilinear sample as premultiplied RGBA in 0..1.
    pub(crate) fn sample(&self, x: f32, y: f32) -> [f32; 4] {
        let x = (x - 0.5).clamp(0.0, self.width.saturating_sub(1) as f32);
        let y = (y - 0.5).clamp(0.0, self.height.saturating_sub(1) as f32);
        let (ix, iy) = (x.floor() as u32, y.floor() as u32);
        let (fx, fy) = (x - x.floor(), y - y.floor());
        let mut out = [0.0; 4];
        for (py, wy) in [(iy, 1.0 - fy), ((iy + 1).min(self.height - 1), fy)] {
            for (px, wx) in [(ix, 1.0 - fx), ((ix + 1).min(self.width - 1), fx)] {
                let i = (py as usize * self.width as usize + px as usize) * 4;
                let a = self.rgba[i + 3] as f32 / 255.0;
                let weight = wx * wy;
                for (c, v) in out.iter_mut().take(3).enumerate() {
                    *v += self.rgba[i + c] as f32 / 255.0 * a * weight;
                }
                out[3] += a * weight;
            }
        }
        out
    }
}

/// Makes an immutable blurred display variant on a worker. Alpha is blurred
/// premultiplied, so transparent colored pixels cannot create edge fringes.
/// A sharp variant shares the complete original animation and every frame.
pub(crate) fn blur_animation(
    source: &Arc<AnimatedRaster>,
    radii: (f32, f32),
) -> Result<Arc<AnimatedRaster>, String> {
    if radii.0 <= 0.0 && radii.1 <= 0.0 {
        return Ok(source.clone());
    }
    if source.frames.is_empty()
        || source.frames.len() > MAX_FRAMES
        || (source.frames.len() > 1 && source.bytes() > MAX_ANIMATION_BYTES)
    {
        return Err("blur animation exceeds its frame memory limit".into());
    }
    let mut frames = Vec::with_capacity(source.frames.len());
    for frame in &source.frames {
        frames.push(Arc::new(blur_raster(frame, radii)?));
    }
    Ok(Arc::new(AnimatedRaster {
        frames,
        delays: source.delays.clone(),
        plays: source.plays,
    }))
}

struct BlurVariant {
    source: Weak<AnimatedRaster>,
    radii: (u32, u32),
    image: Arc<AnimatedRaster>,
    used: u64,
}

/// Shared worker-side variants. Weak source identities prevent allocator
/// address reuse from aliasing a previous decode, without pinning raw pixels
/// after the surface's raw-data cache releases them.
#[derive(Default)]
pub(crate) struct BlurCache {
    entries: Vec<BlurVariant>,
    clock: u64,
}

impl BlurCache {
    pub(crate) fn blur(
        &mut self,
        source: &Arc<AnimatedRaster>,
        radii: (f32, f32),
    ) -> Result<Arc<AnimatedRaster>, String> {
        if radii.0 <= 0.0 && radii.1 <= 0.0 {
            return Ok(source.clone());
        }
        self.clock = self.clock.wrapping_add(1);
        self.entries.retain(|entry| entry.source.strong_count() > 0);
        let key = (radii.0.to_bits(), radii.1.to_bits());
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.radii == key && entry.source.as_ptr() == Arc::as_ptr(source))
        {
            entry.used = self.clock;
            return Ok(entry.image.clone());
        }
        let image = blur_animation(source, radii)?;
        let bytes = image.bytes();
        if bytes > BLUR_CACHE_BYTES {
            return Ok(image);
        }
        while !self.entries.is_empty()
            && (self.entries.len() >= BLUR_CACHE_ENTRIES
                || self
                    .entries
                    .iter()
                    .map(|entry| entry.image.bytes())
                    .sum::<usize>()
                    .saturating_add(bytes)
                    > BLUR_CACHE_BYTES)
        {
            let oldest = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(index, _)| index)
                .expect("nonempty blur cache");
            self.entries.swap_remove(oldest);
        }
        self.entries.push(BlurVariant {
            source: Arc::downgrade(source),
            radii: key,
            image: image.clone(),
            used: self.clock,
        });
        Ok(image)
    }
}

fn blur_raster(source: &Raster, radii: (f32, f32)) -> Result<Raster, String> {
    let (width, height) = (source.width as usize, source.height as usize);
    let count = width
        .checked_mul(height)
        .ok_or("blur image dimensions overflow")?;
    if width == 0
        || height == 0
        || count as u64 > MAX_PIXELS
        || source.rgba.len()
            != count
                .checked_mul(4)
                .ok_or("blur image dimensions overflow")?
    {
        return Err("blur image exceeds its pixel limit or has malformed data".into());
    }
    let radius = |value: f32, dimension: usize| {
        if value.is_nan() || value <= 0.0 {
            0.0
        } else {
            value.min(dimension as f32 * 2.0)
        }
    };
    let rx = blur_boxes(radius(radii.0, width) * 0.5);
    let ry = blur_boxes(radius(radii.1, height) * 0.5);
    let mut pixels = source.rgba.clone();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        for c in 0..3 {
            pixel[c] = ((u32::from(pixel[c]) * u32::from(pixel[3]) + 127) / 255) as u8;
        }
    }
    let mut scratch = vec![0; pixels.len()];
    for (x, y) in rx.into_iter().zip(ry) {
        if x > 0 {
            box_blur(&pixels, &mut scratch, width, height, x, true);
            std::mem::swap(&mut pixels, &mut scratch);
        }
        if y > 0 {
            box_blur(&pixels, &mut scratch, width, height, y, false);
            std::mem::swap(&mut pixels, &mut scratch);
        }
    }
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = (u32::from(*channel) * 255 + alpha / 2)
                .checked_div(alpha)
                .unwrap_or(0)
                .min(255) as u8;
        }
    }
    Ok(Raster {
        width: source.width,
        height: source.height,
        rgba: pixels,
    })
}

/// Three odd box kernels approximate a Gaussian of the requested sigma.
fn blur_boxes(sigma: f32) -> [usize; 3] {
    if sigma <= 0.0 {
        return [0; 3];
    }
    let sigma = f64::from(sigma);
    let mut lower = (4.0 * sigma * sigma + 1.0).sqrt().floor() as usize;
    if lower.is_multiple_of(2) {
        lower = lower.saturating_sub(1);
    }
    let lower = lower.max(1);
    let count = ((12.0 * sigma * sigma - 3.0 * (lower * lower) as f64 - 12.0 * lower as f64 - 9.0)
        / (-4.0 * lower as f64 - 4.0))
        .round()
        .clamp(0.0, 3.0) as usize;
    std::array::from_fn(|i| {
        if i < count {
            lower / 2
        } else {
            (lower + 2) / 2
        }
    })
}

fn box_blur(
    source: &[u8],
    destination: &mut [u8],
    width: usize,
    height: usize,
    radius: usize,
    horizontal: bool,
) {
    let (lines, length, step) = if horizontal {
        (height, width, 4)
    } else {
        (width, height, width * 4)
    };
    let count = (radius * 2 + 1) as u64;
    for line in 0..lines {
        let base = if horizontal {
            line * width * 4
        } else {
            line * 4
        };
        let sample =
            |index: usize, c: usize| u64::from(source[base + index.min(length - 1) * step + c]);
        let mut sum = [0u64; 4];
        for (c, value) in sum.iter_mut().enumerate() {
            *value = sample(0, c) * (radius + 1) as u64;
            for index in 1..=radius {
                *value += sample(index, c);
            }
        }
        for index in 0..length {
            let out = base + index * step;
            for c in 0..4 {
                destination[out + c] = ((sum[c] + count / 2) / count) as u8;
                sum[c] = sum[c] + sample(index + radius + 1, c)
                    - sample(index.saturating_sub(radius), c);
            }
        }
    }
}

/// Decodes an encoded image with strict allocation and dimension limits.
pub(crate) fn decode(bytes: &[u8]) -> Result<Raster, String> {
    use image::ImageDecoder;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("image exceeds 64 MiB".into());
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(MAX_PIXELS * 4);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let (w, h) = decoder.dimensions();
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err("image exceeds 16 million pixels".into());
    }
    let orientation = decoder.orientation().ok();
    let mut image = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    let (width, height) = (image.width(), image.height());
    Ok(Raster {
        width,
        height,
        rgba: image.into_rgba8().into_raw(),
    })
}

fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(MAX_PIXELS * 4);
    limits
}

/// Decodes still images and bounded GIF/WebP animation timelines on a worker.
pub(crate) fn decode_animation(bytes: &[u8]) -> Result<AnimatedRaster, String> {
    use image::{AnimationDecoder, ImageDecoder, ImageFormat, metadata::LoopCount};
    if bytes.len() as u64 > MAX_BYTES {
        return Err("image exceeds 64 MiB".into());
    }
    let format = image::guess_format(bytes).map_err(|e| e.to_string())?;
    let cursor = std::io::Cursor::new(bytes);
    let (frames, loops, orientation) = match format {
        ImageFormat::Gif => {
            let mut decoder =
                image::codecs::gif::GifDecoder::new(cursor).map_err(|e| e.to_string())?;
            decoder
                .set_limits(decode_limits())
                .map_err(|e| e.to_string())?;
            let (w, h) = decoder.dimensions();
            if u64::from(w) * u64::from(h) > MAX_PIXELS {
                return Err("image exceeds 16 million pixels".into());
            }
            let loops = decoder.loop_count();
            (decoder.into_frames(), loops, None)
        }
        ImageFormat::WebP => {
            let mut decoder =
                image::codecs::webp::WebPDecoder::new(cursor).map_err(|e| e.to_string())?;
            decoder
                .set_limits(decode_limits())
                .map_err(|e| e.to_string())?;
            let (w, h) = decoder.dimensions();
            if u64::from(w) * u64::from(h) > MAX_PIXELS {
                return Err("image exceeds 16 million pixels".into());
            }
            if !decoder.has_animation() {
                return decode(bytes).map(still);
            }
            let loops = decoder.loop_count();
            let orientation = decoder.orientation().ok();
            (decoder.into_frames(), loops, orientation)
        }
        _ => return decode(bytes).map(still),
    };
    let plays = match loops {
        LoopCount::Infinite => 0,
        LoopCount::Finite(n) if format == ImageFormat::Gif => n.get().saturating_add(1),
        LoopCount::Finite(n) => n.get(),
    };
    let (mut decoded, mut delays, mut allocated) = (Vec::new(), Vec::new(), 0usize);
    for frame in frames.take(MAX_FRAMES) {
        let frame = frame.map_err(|e| e.to_string())?;
        let (n, d) = frame.delay().numer_denom_ms();
        let delay = match n / d.max(1) {
            ms if ms >= 20 => ms,
            _ => 100,
        };
        let buffer = frame.into_buffer();
        let (w, h) = buffer.dimensions();
        if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
            return Err("animation frame exceeds 16 million pixels".into());
        }
        let mut image = image::DynamicImage::ImageRgba8(buffer);
        if let Some(orientation) = orientation {
            image.apply_orientation(orientation);
        }
        let raster = Raster {
            width: image.width(),
            height: image.height(),
            rgba: image.into_rgba8().into_raw(),
        };
        allocated = allocated.saturating_add(raster.rgba.len());
        if allocated > MAX_ANIMATION_BYTES && !decoded.is_empty() {
            // Keep the first frame rather than a shortened, fast-playing
            // loop with different timing from the source.
            decoded.truncate(1);
            delays.clear();
            break;
        }
        decoded.push(Arc::new(raster));
        delays.push(delay);
    }
    if decoded.is_empty() {
        return Err("animation contains no frames".into());
    }
    if decoded.len() == 1 {
        delays.clear();
    }
    Ok(AnimatedRaster {
        frames: decoded,
        delays,
        plays,
    })
}

fn still(raster: Raster) -> AnimatedRaster {
    AnimatedRaster {
        frames: vec![Arc::new(raster)],
        delays: Vec::new(),
        plays: 1,
    }
}

/// Reads on a worker and reports bounded progress before decoding frames.
pub(crate) fn load_animation_with_progress(
    uri: &str,
    base: Option<&AssetBase>,
    access: &Access,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<AnimatedRaster, String> {
    read_bytes_with_progress(uri, base, access, progress).and_then(|bytes| decode_animation(&bytes))
}

/// Reads a source on a worker, bounded to 64 MiB.
pub(crate) fn read_bytes(
    uri: &str,
    base: Option<&AssetBase>,
    access: &Access,
) -> Result<Vec<u8>, String> {
    read_bytes_with_progress(uri, base, access, &mut |_, _| {})
}

/// Reports at most once per 100 ms while bytes arrive, followed by one final
/// complete report even when a response omits Content-Length.
pub(crate) fn read_bytes_with_progress(
    uri: &str,
    base: Option<&AssetBase>,
    access: &Access,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Vec<u8>, String> {
    if !access.allows_media(uri) {
        return Err(format!(
            "image `{uri}` is not granted by capabilities.network"
        ));
    }
    let location = resolve(uri, base)
        .ok_or_else(|| format!("unsupported image URI or missing asset base: {uri}"))?;
    let mut reports = Progress::new(progress);
    let mut result = read(&location, access, &mut |loaded, total| {
        reports.report(loaded, total, Instant::now())
    })?;
    if result.len() as u64 > MAX_BYTES {
        result.clear();
        return Err("image exceeds 64 MiB".into());
    }
    reports.finish(result.len() as u64);
    Ok(result)
}

struct Progress<'a> {
    output: &'a mut dyn FnMut(u64, u64),
    last: Option<Instant>,
    latest: (u64, u64),
    reported: Option<(u64, u64)>,
}

impl<'a> Progress<'a> {
    fn new(output: &'a mut dyn FnMut(u64, u64)) -> Self {
        Self {
            output,
            last: None,
            latest: (0, 0),
            reported: None,
        }
    }
    fn report(&mut self, loaded: u64, total: u64, now: Instant) {
        self.latest = (loaded, total);
        if self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_millis(100))
        {
            (self.output)(loaded, total);
            self.last = Some(now);
            self.reported = Some((loaded, total));
        }
    }
    fn finish(&mut self, loaded: u64) {
        let total = if self.latest.1 == 0 {
            loaded
        } else {
            self.latest.1
        };
        if self.reported != Some((loaded, total)) {
            (self.output)(loaded, total);
        }
    }
}

/// Where bundled (`asset:/…`) files are.
#[derive(Debug, Clone)]
pub enum AssetBase {
    /// Served over HTTP (the dev server): `http://host:port/assets/`.
    Http(String),
    /// A directory on disk.
    Directory(PathBuf),
}

/// Reads `asset:/…` files for the engine (fonts), on the engine thread.
pub(crate) struct AssetReader(pub(crate) Option<AssetBase>);

impl nsh_engine::Assets for AssetReader {
    fn read(&self, uri: &str) -> Result<Vec<u8>, String> {
        if !uri.starts_with("asset:/") {
            return Err(format!("not an asset URI: {uri}"));
        }
        match resolve(uri, self.0.as_ref()) {
            Some(location) => read(&location, &Access::default(), &mut |_, _| {}),
            None => Err(format!("{uri}: no asset location configured")),
        }
    }
}

/// Where an image's bytes come from.
#[derive(Debug, Clone, PartialEq)]
enum Location {
    /// `http(s)://`.
    Url(String),
    /// A file on disk.
    File(PathBuf),
    /// A `data:` URI (decoded on the worker).
    Data(String),
}

/// Resolves `uri` (`asset:/…` against `base`, `http(s)://`, `file:`, an
/// absolute path, `data:`).
fn resolve(uri: &str, base: Option<&AssetBase>) -> Option<Location> {
    if let Some(path) = uri.strip_prefix("asset:/") {
        let path = path.trim_start_matches('/');
        // An asset names a file inside its configured root.
        if path.split(['/', '\\']).any(|part| part == "..")
            || !std::path::Path::new(path).components().all(|part| {
                matches!(
                    part,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            })
        {
            return None;
        }
        return match base? {
            AssetBase::Http(base) => {
                let sep = if base.ends_with('/') { "" } else { "/" };
                Some(Location::Url(format!("{base}{sep}{path}")))
            }
            AssetBase::Directory(dir) => Some(Location::File(dir.join(path))),
        };
    }
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return Some(Location::Url(uri.to_owned()));
    }
    if uri.starts_with("data:") {
        return Some(Location::Data(uri.to_owned()));
    }
    if let Some(rest) = uri.strip_prefix("file:") {
        return file_uri(rest).map(Location::File);
    }
    absolute_path(uri).then(|| Location::File(uri.into()))
}

/// True for an absolute path of this system: `/…`; on Windows `C:\…`,
/// `C:/…` or a UNC path.
fn absolute_path(path: &str) -> bool {
    if cfg!(windows) {
        let b = path.as_bytes();
        let drive = b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'\\' | b'/');
        drive || path.starts_with("\\\\") || path.starts_with("//")
    } else {
        path.starts_with('/')
    }
}

/// The path of a `file:` URI (what follows `file:`): `///home/a%20b.png`
/// is `/home/a b.png`; on Windows `///C:/a.png` is `C:\a.png` and
/// `//server/share/a.png` is `\\server\share\a.png`.
fn file_uri(rest: &str) -> Option<PathBuf> {
    let decoded = String::from_utf8(percent_decode(rest.as_bytes())).ok()?;
    let path = match decoded.strip_prefix("//") {
        // `file:///…` (an empty host) or `file://localhost/…`.
        Some(after) => match after.split_once('/') {
            Some(("" | "localhost", path)) => format!("/{path}"),
            Some((host, path)) if cfg!(windows) => format!("//{host}/{path}"),
            _ => return None,
        },
        // `file:/…`.
        None => decoded,
    };
    if cfg!(windows) {
        // `/C:/…` is `C:\…`; `//host/…` stays a UNC path.
        let path = match path.strip_prefix('/') {
            Some(p) if !p.starts_with('/') => p.to_owned(),
            _ => path,
        };
        let path = path.replace('/', "\\");
        absolute_path(&path).then(|| path.into())
    } else {
        absolute_path(&path).then(|| path.into())
    }
}

/// `%xx` escapes decoded; malformed ones stay as they are.
fn percent_decode(bytes: &[u8]) -> Vec<u8> {
    let hex = |b: u8| (b as char).to_digit(16);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (
            bytes[i],
            bytes.get(i + 1).and_then(|&b| hex(b)),
            bytes.get(i + 2).and_then(|&b| hex(b)),
        ) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// The bytes of a `data:` URI (base64 or percent-encoded).
fn data_uri(uri: &str) -> Result<Vec<u8>, String> {
    let rest = uri.strip_prefix("data:").ok_or("not a data URI")?;
    let (meta, data) = rest.split_once(',').ok_or("malformed data URI")?;
    // Bound encoded input as well as output, before whitespace filtering or
    // percent decoding allocates a second copy of an untrusted string.
    if data.len() as u64 > MAX_BYTES.saturating_mul(3) {
        return Err("data URI exceeds the image byte limit".into());
    }
    if meta.ends_with(";base64") {
        let encoded = data.bytes().filter(|c| !c.is_ascii_whitespace()).count();
        if encoded as u64 > MAX_BYTES.div_ceil(3) * 4 {
            return Err("data URI exceeds the image byte limit".into());
        }
    }
    nsh_core::image::decode_data_uri(uri).map(|(_, bytes)| bytes)
}

/// The HTTP agent all downloads share. It never follows redirects: each hop
/// is checked against the app's capabilities before it is requested.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .build()
            .into()
    })
}

/// GETs `url`, following redirects one hop at a time: a hop the app is not
/// granted is never requested.
fn get(url: &str, access: &Access) -> Result<ureq::http::Response<ureq::Body>, String> {
    let mut url = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        let response = agent()
            .get(&url)
            .call()
            .map_err(|e| format!("{url}: {e}"))?;
        let status = response.status().as_u16();
        if !matches!(status, 301 | 302 | 303 | 307 | 308) {
            if status >= 400 {
                return Err(format!("{url}: HTTP {status}"));
            }
            return Ok(response);
        }
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| format!("{url}: redirect without a Location"))?;
        url = access.media_redirect(&url, location)?;
    }
    Err(format!("{url}: too many redirects"))
}

/// Reads the bytes at `location` (blocking), calling `progress(loaded,
/// total)` (total 0: unknown) as a download goes.
fn read(
    location: &Location,
    access: &Access,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Vec<u8>, String> {
    match location {
        Location::File(path) => {
            let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err("image file exceeds 64 MiB".into());
            }
            Ok(bytes)
        }
        Location::Data(uri) => data_uri(uri),
        Location::Url(url) => {
            let mut response = get(url, access)?;
            let total = response.body().content_length().unwrap_or(0);
            // ureq's limit reports overflow once exactly that many bytes
            // have been read, so reserve one probe byte to accept the cap.
            let mut reader = response
                .body_mut()
                .with_config()
                .limit(MAX_BYTES + 1)
                .reader();
            let mut bytes = Vec::with_capacity(total.min(MAX_BYTES) as usize);
            let mut chunk = [0u8; 16 << 10];
            loop {
                let n = reader.read(&mut chunk).map_err(|e| format!("{url}: {e}"))?;
                if n == 0 {
                    break;
                }
                if bytes.len().saturating_add(n) as u64 > MAX_BYTES {
                    return Err("image exceeds 64 MiB".into());
                }
                bytes.extend_from_slice(&chunk[..n]);
                progress(bytes.len() as u64, total);
            }
            Ok(bytes)
        }
    }
}

/// The image's rectangle after applying its resize mode, before clipping.
pub(crate) fn fitted_frame(
    raster: &Raster,
    frame: Bounds<Points>,
    mode: ResizeMode,
) -> Bounds<Points> {
    let (iw, ih) = (px(raster.width as f32), px(raster.height as f32));
    let (width, height) = match mode {
        ResizeMode::Stretch => (frame.size.width, frame.size.height),
        ResizeMode::Center => (iw, ih),
        ResizeMode::Cover | ResizeMode::Contain | ResizeMode::ScaleDown => {
            let (sx, sy) = (frame.size.width / iw, frame.size.height / ih);
            let scale = if mode == ResizeMode::Cover {
                sx.max(sy)
            } else if mode == ResizeMode::ScaleDown {
                sx.min(sy).min(1.0)
            } else {
                sx.min(sy)
            };
            (iw * scale, ih * scale)
        }
    };
    Bounds::new(
        point(
            frame.origin.x + (frame.size.width - width) * 0.5,
            frame.origin.y + (frame.size.height - height) * 0.5,
        ),
        Size::new(width, height),
    )
}

/// Composes an image into two-color terminal blocks. `frame` and `clip`
/// use virtual points; `area` is the surface's destination in the buffer.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint(
    raster: &Raster,
    frame: Bounds<Points>,
    clip: Bounds<Points>,
    mode: ResizeMode,
    tint: Option<Color>,
    opacity: f32,
    metrics: CellMetrics,
    area: Area,
    buffer: &mut Buffer,
) {
    if raster.width == 0
        || raster.height == 0
        || raster.rgba.len() != raster.width as usize * raster.height as usize * 4
        || frame.size.width <= Points::ZERO
        || frame.size.height <= Points::ZERO
        || !opacity.is_finite()
        || opacity <= 0.0
    {
        return;
    }
    let (iw, ih) = (raster.width as f32, raster.height as f32);
    let dest = fitted_frame(raster, frame, mode);
    let (w, h) = (dest.size.width, dest.size.height);
    let left = frame
        .origin
        .x
        .max(clip.origin.x)
        .max(dest.origin.x)
        .max(Points::ZERO);
    let top = frame
        .origin
        .y
        .max(clip.origin.y)
        .max(dest.origin.y)
        .max(Points::ZERO);
    let right = (frame.origin.x + frame.size.width)
        .min(clip.origin.x + clip.size.width)
        .min(dest.origin.x + w);
    let bottom = (frame.origin.y + frame.size.height)
        .min(clip.origin.y + clip.size.height)
        .min(dest.origin.y + h);
    if ![left, top, right, bottom, w, h]
        .iter()
        .all(|n| n.is_finite())
        || right <= left
        || bottom <= top
    {
        return;
    }
    let (cw, ch) = (metrics.width(), metrics.height());
    let (x0, x1) = ((left / cw).floor() as u16, (right / cw).ceil() as u16);
    let (y0, y1) = ((top / ch).floor() as u16, (bottom / ch).ceil() as u16);
    let opacity = opacity.clamp(0.0, 1.0);
    let tinted = tint.map(|color| {
        let (r, g, b, a) = color.to_f64();
        [r as f32, g as f32, b as f32, a as f32]
    });
    for y in y0..y1.min(area.height) {
        for x in x0..x1.min(area.width) {
            let Some(cell) = buffer.cell_mut((area.x.saturating_add(x), area.y.saturating_add(y)))
            else {
                continue;
            };
            let mut samples = [[0.0f32; 4]; 4];
            // Visit each sample row once, retaining each quadrant's original
            // sample coordinates and accumulation order.
            for sy in 0..8 {
                let py = (y as f32 + (sy / 4) as f32 * 0.5 + ((sy % 4) as f32 + 0.5) / 8.0) * ch;
                if py < top || py >= bottom {
                    continue;
                }
                for sx in 0..4 {
                    let px =
                        (x as f32 + (sx / 2) as f32 * 0.5 + ((sx % 2) as f32 + 0.5) / 4.0) * cw;
                    if px < left || px >= right {
                        continue;
                    }
                    let mut color =
                        raster.sample((px - dest.origin.x) / w * iw, (py - dest.origin.y) / h * ih);
                    if let Some([r, g, b, a]) = tinted {
                        color = [
                            r * color[3] * a,
                            g * color[3] * a,
                            b * color[3] * a,
                            color[3] * a,
                        ];
                    }
                    let sampled = &mut samples[(sy / 4) * 2 + sx / 2];
                    for k in 0..4 {
                        sampled[k] += color[k] * opacity / 8.0;
                    }
                }
            }
            let mut underlay = None;
            let mut colors = [[0.0; 3]; 4];
            let mut painted = false;
            for quadrant in 0..4 {
                let sampled = samples[quadrant];
                if sampled[3] > 0.0 {
                    painted = true;
                }
                // An opaque sample replaces this quadrant completely; only
                // transparent or clipped content needs the existing cell.
                if sampled[3] == 1.0 {
                    colors[quadrant] = [
                        (sampled[0] * 255.0).clamp(0.0, 255.0),
                        (sampled[1] * 255.0).clamp(0.0, 255.0),
                        (sampled[2] * 255.0).clamp(0.0, 255.0),
                    ];
                } else {
                    let (mask, palette) = underlay.get_or_insert_with(|| {
                        (
                            block_mask(cell.symbol()).unwrap_or(0),
                            [rgb(cell.bg), rgb(cell.fg)],
                        )
                    });
                    let old = palette[usize::from(*mask & (1 << quadrant) != 0)];
                    for k in 0..3 {
                        colors[quadrant][k] = (sampled[k] * 255.0
                            + f32::from(old[k]) * (1.0 - sampled[3]))
                            .clamp(0.0, 255.0);
                    }
                }
            }
            if painted {
                let (symbol, fg, bg) = fit_block(colors);
                cell.set_symbol(symbol)
                    .set_fg(TerminalColor::Rgb(fg[0], fg[1], fg[2]))
                    .set_bg(TerminalColor::Rgb(bg[0], bg[1], bg[2]));
            }
        }
    }
}

const BLOCKS: [&str; 16] = [
    " ", "▘", "▝", "▀", "▖", "▌", "▞", "▛", "▗", "▚", "▐", "▜", "▄", "▙", "▟", "█",
];

/// Foreground quadrants in reading order, shared with backdrop rasterization.
pub(crate) fn block_mask(symbol: &str) -> Option<u8> {
    BLOCKS
        .iter()
        .position(|block| *block == symbol)
        .filter(|&i| i != 0)
        .map(|i| i as u8)
}

fn fit_block(samples: [[f32; 3]; 4]) -> (&'static str, [u8; 3], [u8; 3]) {
    #[inline(always)]
    fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
        [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
    }
    let [a, b, c, d] = samples;
    let (ab, ac, ad, bc, bd, cd) = (
        add(a, b),
        add(a, c),
        add(a, d),
        add(b, c),
        add(b, d),
        add(c, d),
    );
    let mut best = ("▀", [0; 3], [0; 3]);
    let mut error = f32::INFINITY;
    // A mask and its complement reconstruct the same two-color cell.
    // Prefer half blocks on ties, including uniform colors and horizontal edges.
    // These sums retain each group's original reading-order arithmetic while
    // avoiding repeated grouping and iterator work for every partition.
    for (mask, fg_sum, bg_sum, count) in [
        (3, ab, cd, 2.0),
        (5, ac, bd, 2.0),
        (1, a, add(bc, d), 1.0),
        (2, b, add(ac, d), 1.0),
        (4, c, add(ab, d), 1.0),
        (6, bc, ad, 2.0),
        (7, add(ab, c), d, 3.0),
    ] {
        let fg = [
            (fg_sum[0] / count).round() as u8,
            (fg_sum[1] / count).round() as u8,
            (fg_sum[2] / count).round() as u8,
        ];
        let bg_count = 4.0 - count;
        let bg = [
            (bg_sum[0] / bg_count).round() as u8,
            (bg_sum[1] / bg_count).round() as u8,
            (bg_sum[2] / bg_count).round() as u8,
        ];
        let mut candidate = 0.0;
        macro_rules! accumulate {
            ($sample:ident, $bit:expr) => {{
                let color = if mask & $bit != 0 { fg } else { bg };
                let r = $sample[0] - f32::from(color[0]);
                let g = $sample[1] - f32::from(color[1]);
                let b = $sample[2] - f32::from(color[2]);
                candidate += r * r;
                candidate += g * g;
                candidate += b * b;
            }};
        }
        accumulate!(a, 1);
        accumulate!(b, 2);
        accumulate!(c, 4);
        accumulate!(d, 8);
        if candidate < error {
            best = (BLOCKS[mask as usize], fg, bg);
            error = candidate;
        }
        if error == 0.0 {
            break;
        }
    }
    best
}

/// Terminal colors as RGB, including the xterm 256-color palette.
pub(crate) fn rgb(color: TerminalColor) -> [u8; 3] {
    const ANSI: [[u8; 3]; 16] = [
        [0, 0, 0],
        [128, 0, 0],
        [0, 128, 0],
        [128, 128, 0],
        [0, 0, 128],
        [128, 0, 128],
        [0, 128, 128],
        [192, 192, 192],
        [128, 128, 128],
        [255, 0, 0],
        [0, 255, 0],
        [255, 255, 0],
        [0, 0, 255],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ];
    let index = match color {
        TerminalColor::Rgb(r, g, b) => return [r, g, b],
        TerminalColor::Indexed(n) if n < 16 => n,
        TerminalColor::Indexed(n) if n < 232 => {
            let n = n - 16;
            let level = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
            return [level(n / 36), level(n / 6 % 6), level(n % 6)];
        }
        TerminalColor::Indexed(n) => return [8 + (n - 232) * 10; 3],
        TerminalColor::Black | TerminalColor::Reset => 0,
        TerminalColor::Red => 1,
        TerminalColor::Green => 2,
        TerminalColor::Yellow => 3,
        TerminalColor::Blue => 4,
        TerminalColor::Magenta => 5,
        TerminalColor::Cyan => 6,
        TerminalColor::Gray => 7,
        TerminalColor::DarkGray => 8,
        TerminalColor::LightRed => 9,
        TerminalColor::LightGreen => 10,
        TerminalColor::LightYellow => 11,
        TerminalColor::LightBlue => 12,
        TerminalColor::LightMagenta => 13,
        TerminalColor::LightCyan => 14,
        TerminalColor::White => 15,
    };
    ANSI[index as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raster() -> Raster {
        Raster {
            width: 1,
            height: 2,
            rgba: vec![255, 0, 0, 255, 0, 0, 255, 255],
        }
    }

    #[test]
    fn alpha_and_top_bottom_colors_compose_with_the_existing_buffer() {
        let area = Area::new(3, 2, 1, 1);
        let mut buffer = Buffer::empty(area);
        buffer[(3, 2)].set_bg(TerminalColor::Rgb(0, 255, 0));
        let rect = Bounds::new(point(px(0.0), px(0.0)), Size::new(px(8.0), px(16.0)));
        paint(
            &raster(),
            rect,
            rect,
            ResizeMode::Stretch,
            None,
            0.5,
            CellMetrics::default(),
            area,
            &mut buffer,
        );
        let cell = &buffer[(3, 2)];
        assert_eq!(cell.symbol(), "▀");
        let (TerminalColor::Rgb(tr, tg, tb), TerminalColor::Rgb(br, bg, bb)) = (cell.fg, cell.bg)
        else {
            panic!("RGB samples");
        };
        assert!(tr > tb && bb > br);
        assert!((127..=128).contains(&tg) && (127..=128).contains(&bg));
    }

    #[test]
    fn contain_leaves_letterbox_pixels_untouched() {
        let area = Area::new(0, 0, 3, 1);
        let mut buffer = Buffer::empty(area);
        buffer.content.iter_mut().for_each(|cell| {
            cell.set_symbol(".");
        });
        let rect = Bounds::new(point(px(0.0), px(0.0)), Size::new(px(24.0), px(16.0)));
        paint(
            &raster(),
            rect,
            rect,
            ResizeMode::Contain,
            None,
            1.0,
            CellMetrics::default(),
            area,
            &mut buffer,
        );
        assert_eq!(buffer[(0, 0)].symbol(), ".");
        assert_eq!(buffer[(1, 0)].symbol(), "▀");
        assert_eq!(buffer[(2, 0)].symbol(), ".");
    }

    #[test]
    fn clipping_preserves_the_other_half_of_a_cell() {
        let area = Area::new(0, 0, 1, 1);
        let mut buffer = Buffer::empty(area);
        buffer[(0, 0)].set_bg(TerminalColor::Rgb(10, 20, 30));
        let rect = Bounds::new(point(px(0.0), px(0.0)), Size::new(px(8.0), px(16.0)));
        let clip = Bounds::new(rect.origin, Size::new(rect.size.width, px(8.0)));
        paint(
            &raster(),
            rect,
            clip,
            ResizeMode::Stretch,
            Some(Color::BLACK),
            1.0,
            CellMetrics::default(),
            area,
            &mut buffer,
        );
        assert_eq!(buffer[(0, 0)].bg, TerminalColor::Rgb(10, 20, 30));
        assert_eq!(buffer[(0, 0)].fg, TerminalColor::Rgb(0, 0, 0));
    }

    #[test]
    fn decoding_checks_limits_and_premultiplication_roundtrips() {
        assert!(decode(b"bad image").is_err());
        let raster = Raster {
            width: 1,
            height: 1,
            rgba: vec![200, 100, 50, 128],
        };
        let out = Raster::from_pixmap(raster.pixmap().unwrap());
        for (a, b) in out.rgba.iter().zip(raster.rgba.iter()) {
            assert!((*a as i16 - *b as i16).abs() <= 1);
        }
    }

    #[test]
    fn assets_cannot_traverse_outside_their_root() {
        let base = AssetBase::Directory("/tmp/app/assets".into());
        assert!(resolve("asset:/../secret", Some(&base)).is_none());
        assert!(resolve("asset:/images/logo.png", Some(&base)).is_some());
        assert_eq!(data_uri("data:image/png;base64,YQ==").unwrap(), b"a");
    }

    #[test]
    fn raw_data_uris_use_the_shared_decoder_and_report_completed_reads() {
        for (uri, expected) in [
            ("data:image/png;base64,YQ==", b"a".as_slice()),
            ("data:;base64,_-8 \n", &[0xff, 0xef]),
            ("data:application/octet-stream,%00%2B%ff%zz", b"\0+\xff%zz"),
        ] {
            let mut progress = Vec::new();
            let bytes =
                read_bytes_with_progress(uri, None, &Access::default(), &mut |loaded, total| {
                    progress.push((loaded, total))
                })
                .unwrap();
            assert_eq!(bytes, expected);
            assert_eq!(progress, [(expected.len() as u64, expected.len() as u64)]);
        }
        assert!(read_bytes("data:;base64,a*b", None, &Access::default()).is_err());
        assert!(read_bytes("data:image/png;nsh-key=missing", None, &Access::default()).is_err());
        assert!(read_bytes("blob:revoked", None, &Access::default()).is_err());
    }

    #[test]
    fn blur_preserves_alpha_color_and_animation_timing_without_mutating_shared_pixels() {
        let mut pixels = [255, 0, 0, 0].repeat(7);
        pixels[12..16].copy_from_slice(&[0, 0, 255, 255]);
        let source = Arc::new(AnimatedRaster {
            frames: vec![
                Arc::new(Raster {
                    width: 7,
                    height: 1,
                    rgba: pixels.clone(),
                }),
                Arc::new(Raster {
                    width: 7,
                    height: 1,
                    rgba: pixels.clone(),
                }),
            ],
            delays: vec![40, 80],
            plays: 2,
        });
        assert!(Arc::ptr_eq(
            &blur_animation(&source, (0.0, 0.0)).unwrap(),
            &source
        ));
        let blurred = blur_animation(&source, (4.0, 0.0)).unwrap();
        assert_eq!(blurred.delays, source.delays);
        assert_eq!(blurred.plays, source.plays);
        assert_eq!(source.frames[0].rgba, pixels);
        assert!(!Arc::ptr_eq(&blurred.frames[0], &source.frames[0]));
        assert!(
            blurred.frames[0].rgba[7] > 0,
            "the central alpha must spread to a neighboring pixel"
        );
        for pixel in blurred.frames[0].rgba.as_chunks::<4>().0 {
            assert_eq!(
                pixel[0], 0,
                "transparent red must not bleed into the blurred blue"
            );
            if pixel[3] > 0 {
                assert_eq!(pixel[2], 255);
            }
        }
        assert_eq!(blurred.frame_index(40), 1);
    }

    #[test]
    fn blur_is_anisotropic_and_validates_dimensions_before_allocating() {
        let mut data = [0, 0, 0, 255].repeat(49);
        data[(3 * 7 + 3) * 4..][..4].copy_from_slice(&[255, 255, 255, 255]);
        let source = Raster {
            width: 7,
            height: 7,
            rgba: data,
        };
        let horizontal = blur_raster(&source, (4.0, 0.0)).unwrap();
        assert!(horizontal.rgba[(3 * 7 + 2) * 4] > 0);
        assert_eq!(horizontal.rgba[(2 * 7 + 3) * 4], 0);
        assert!(
            blur_raster(
                &Raster {
                    width: 8193,
                    height: 8193,
                    rgba: Vec::new()
                },
                (4.0, 4.0)
            )
            .is_err()
        );
    }

    #[test]
    fn blur_cache_shares_variants_and_releases_dead_source_entries() {
        let source = Arc::new(AnimatedRaster {
            frames: vec![Arc::new(Raster {
                width: 3,
                height: 1,
                rgba: vec![255, 0, 0, 255, 0, 0, 255, 255, 255, 0, 0, 255],
            })],
            delays: Vec::new(),
            plays: 1,
        });
        let mut cache = BlurCache::default();
        let first = cache.blur(&source, (2.0, 2.0)).unwrap();
        let shared = cache.blur(&source, (2.0, 2.0)).unwrap();
        assert!(Arc::ptr_eq(&first, &shared));
        assert!(Arc::ptr_eq(
            &cache.blur(&source, (0.0, 0.0)).unwrap(),
            &source
        ));
        assert_eq!(cache.entries.len(), 1);
        let replacement = Arc::new((*source).clone());
        drop(source);
        let different = cache.blur(&replacement, (2.0, 2.0)).unwrap();
        assert!(!Arc::ptr_eq(&first, &different));
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn worker_http_progress_is_throttled_and_finishes_with_the_full_byte_count() {
        use std::{io::Write as _, net::TcpListener};
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}/image", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
                assert!(header.len() < 8192);
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 131072\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            stream.write_all(&[7; 65536]).unwrap();
            stream.flush().unwrap();
            std::thread::sleep(Duration::from_millis(120));
            stream.write_all(&[9; 65536]).unwrap();
        });
        let mut events = Vec::new();
        let bytes =
            read_bytes_with_progress(&url, None, &Access::default(), &mut |loaded, total| {
                events.push((loaded, total, Instant::now()))
            })
            .unwrap();
        server.join().unwrap();
        assert_eq!(bytes.len(), 131072);
        assert!(events.len() >= 2);
        assert_eq!(events.last().map(|e| (e.0, e.1)), Some((131072, 131072)));
        // The complete report may follow an intermediate report immediately.
        for pair in events[..events.len() - 1].windows(2) {
            assert!(pair[1].2.duration_since(pair[0].2) >= Duration::from_millis(95));
            assert!(pair[1].0 > pair[0].0);
        }
    }

    #[test]
    fn unknown_length_progress_reports_a_final_known_total() {
        let mut values = Vec::new();
        {
            let mut output = |loaded, total| values.push((loaded, total));
            let mut progress = Progress::new(&mut output);
            let now = Instant::now();
            progress.report(10, 0, now);
            progress.report(20, 0, now + Duration::from_millis(50));
            progress.report(30, 0, now + Duration::from_millis(100));
            progress.finish(35);
        }
        assert_eq!(values, vec![(10, 0), (30, 0), (35, 35)]);
    }

    fn animated_gif() -> Vec<u8> {
        use image::{
            Delay, Frame, Rgba, RgbaImage,
            codecs::gif::{GifEncoder, Repeat},
        };
        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            encoder.set_repeat(Repeat::Finite(1)).unwrap();
            for color in [[255, 0, 0, 255], [0, 0, 255, 255]] {
                encoder
                    .encode_frame(Frame::from_parts(
                        RgbaImage::from_pixel(2, 2, Rgba(color)),
                        0,
                        0,
                        Delay::from_numer_denom_ms(40, 1),
                    ))
                    .unwrap();
            }
        }
        bytes
    }

    #[test]
    fn animated_gif_decodes_distinct_pixels_and_respects_finite_loop_timing() {
        let animation = decode_animation(&animated_gif()).unwrap();
        assert!(animation.animated());
        assert_eq!(animation.frames.len(), 2);
        assert_eq!(&animation.frames[0].rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(&animation.frames[1].rgba[..4], &[0, 0, 255, 255]);
        assert_eq!(animation.delays, vec![40, 40]);
        assert_eq!(animation.plays, 2);
        assert_eq!(animation.bytes(), 32);
        assert_eq!(animation.frame_index(39), 0);
        assert_eq!(animation.frame_index(40), 1);
        assert_eq!(animation.frame_index(80), 0);
        assert_eq!(animation.frame_index(120), 1);
        assert_eq!(animation.frame_index(200), 1);
        assert!(!animation.finished(159));
        assert!(animation.finished(160));
    }

    #[test]
    fn infinite_animations_keep_playing_on_the_same_monotonic_timeline() {
        let mut animation = decode_animation(&animated_gif()).unwrap();
        animation.plays = 0;
        assert_eq!(animation.frame_index(80_000), 0);
        assert_eq!(animation.frame_index(80_040), 1);
        assert!(!animation.finished(u64::MAX));
    }

    #[test]
    fn animated_webp_decodes_distinct_frames_with_its_own_loop_semantics() {
        fn chunk(tag: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut out = tag.to_vec();
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(body);
            if !body.len().is_multiple_of(2) {
                out.push(0);
            }
            out
        }
        // Wrap independently encoded lossless frame bitstreams in the WebP
        // animation container (2x2 canvas, two complete plays).
        let mut body = b"WEBP".to_vec();
        body.extend(chunk(b"VP8X", &[2, 0, 0, 0, 1, 0, 0, 1, 0, 0]));
        body.extend(chunk(b"ANIM", &[0, 0, 0, 0, 2, 0]));
        for (color, delay) in [([255, 0, 0, 255], 40u32), ([0, 0, 255, 255], 80u32)] {
            let mut still = Vec::new();
            image::codecs::webp::WebPEncoder::new_lossless(&mut still)
                .encode(&color.repeat(4), 2, 2, image::ExtendedColorType::Rgba8)
                .unwrap();
            let mut frame = vec![0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0];
            frame.extend_from_slice(&delay.to_le_bytes()[..3]);
            frame.push(2);
            frame.extend_from_slice(&still[12..]);
            body.extend(chunk(b"ANMF", &frame));
        }
        let mut encoded = b"RIFF".to_vec();
        encoded.extend_from_slice(&(body.len() as u32).to_le_bytes());
        encoded.extend(body);
        let animation = decode_animation(&encoded).unwrap();
        assert_eq!(animation.frames.len(), 2);
        assert_eq!(&animation.frames[0].rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(&animation.frames[1].rgba[..4], &[0, 0, 255, 255]);
        assert_eq!(animation.delays, vec![40, 80]);
        // WebP counts complete plays, whereas GIF counts repetitions.
        assert_eq!(animation.plays, 2);
        assert_eq!(animation.frame_index(120), 0);
        assert_eq!(animation.frame_index(160), 1);
        assert!(!animation.finished(239));
        assert!(animation.finished(240));
    }
}
