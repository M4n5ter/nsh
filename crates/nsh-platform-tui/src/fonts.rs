//! A surface's font catalog, shared by synchronous Canvas measurement and its
//! graphics worker. Registered app fonts never enter another surface's catalog.
//! Glyph placement is shaped once; the worker obtains outlines from those same
//! placements when drawing. System font discovery is lazy and instance-local.

use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use nsh_core::{
    FontVariant,
    canvas::CanvasFont,
    geometry::{Points, px},
};
use tiny_skia as sk;
use unicode_bidi::{BidiInfo, Level};
use unicode_segmentation::UnicodeSegmentation;

const MAX_REGISTERED_BYTES: usize = 64 << 20;
const FONT_CACHE_BYTES: usize = 32 << 20;
const SHAPE_CACHE_BYTES: usize = 8 << 20;
const BITMAP_CACHE_BYTES: usize = 32 << 20;

/// Font resources belonging to one embedded terminal surface.
#[derive(Clone, Default)]
pub(crate) struct FontCatalog(Arc<Catalog>);

#[derive(Default)]
struct Catalog {
    state: Mutex<Fonts>,
    revision: AtomicU64,
}

impl fmt::Debug for FontCatalog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FontCatalog")
            .field("revision", &self.revision())
            .finish_non_exhaustive()
    }
}

impl FontCatalog {
    /// A lock-free cache generation for the terminal loop.
    pub(crate) fn revision(&self) -> u64 {
        self.0.revision.load(Ordering::Acquire)
    }

    /// Registers TrueType/OpenType bytes for this surface, including collections.
    pub(crate) fn register(&self, data: &[u8]) -> Result<(), String> {
        ttf_parser::Face::parse(data, 0).map_err(|error| error.to_string())?;
        if ttf_parser::fonts_in_collection(data).unwrap_or(1) > 256 {
            return Err("a terminal font collection may contain at most 256 faces".into());
        }
        let mut fonts = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if fonts
            .registered_data
            .iter()
            .any(|source| source.as_slice() == data)
        {
            return Ok(());
        }
        let bytes = fonts
            .registered_data
            .iter()
            .map(|source| source.len())
            .sum::<usize>();
        if bytes.saturating_add(data.len()) > MAX_REGISTERED_BYTES {
            return Err("registered terminal fonts exceed the 64 MiB surface limit".into());
        }
        let data = Arc::new(data.to_vec());
        let ids = fonts
            .registered
            .load_font_source(fontdb::Source::Binary(data.clone()));
        if ids.is_empty() {
            return Err("the font has no usable named faces".into());
        }
        fonts.registered_data.push(data);
        fonts.choices.clear();
        fonts.fallback.clear();
        fonts.shapes.clear();
        fonts.shape_bytes = 0;
        self.0.revision.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// Shapes a line for both Canvas measurement and drawing.
    pub(crate) fn shape(&self, font: &CanvasFont, text: &str) -> Arc<ShapedText> {
        if text.is_empty() {
            return Arc::new(ShapedText {
                width: 0.0,
                ascent: 0.0,
                descent: 0.0,
                glyphs: Vec::new(),
            });
        }
        let mut fonts = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let key = ShapeKey::new(font, text);
        if let Some(cached) = fonts.shapes.get(&key) {
            return cached.clone();
        }
        let shaped = Arc::new(fonts.shape(font, text));
        let bytes = text
            .len()
            .saturating_add(shaped.glyphs.len() * std::mem::size_of::<Glyph>());
        if bytes <= SHAPE_CACHE_BYTES / 4 {
            if fonts.shapes.len() >= 256
                || fonts.shape_bytes.saturating_add(bytes) > SHAPE_CACHE_BYTES
            {
                fonts.shapes.clear();
                fonts.shape_bytes = 0;
            }
            fonts.shape_bytes += bytes;
            fonts.shapes.insert(key, shaped.clone());
        }
        shaped
    }

    pub(crate) fn measure(&self, font: &CanvasFont, text: &str) -> Points {
        px(self.shape(font, text).width)
    }
}

/// Glyph geometry in points, with y down and the alphabetic baseline at zero.
pub(crate) struct ShapedText {
    pub(crate) width: f32,
    pub(crate) ascent: f32,
    pub(crate) descent: f32,
    glyphs: Vec<Glyph>,
}

impl ShapedText {
    /// Paints retain color-font pixels and COLR v0 layers at the same positions
    /// as monochrome outlines; the caller applies its Canvas/scene transform.
    pub(crate) fn paints(&self) -> Vec<GlyphPaint> {
        let mut paints = Vec::new();
        let mut monochrome = sk::PathBuilder::new();
        for glyph in &self.glyphs {
            let transform =
                sk::Transform::from_row(glyph.scale, 0.0, 0.0, -glyph.scale, glyph.x, glyph.y);
            if let Some(art) = glyph.file.art(
                glyph.id,
                glyph.weight,
                glyph.size.ceil().clamp(1.0, 1024.0) as u16,
            ) {
                if let Some(path) = std::mem::take(&mut monochrome).finish() {
                    paints.push(GlyphPaint::Path(path, None));
                }
                match &*art {
                    GlyphArt::Bitmap(bitmap) => {
                        let scale = glyph.size / bitmap.pixels_per_em as f32;
                        paints.push(GlyphPaint::Image {
                            image: bitmap.image.clone(),
                            transform: sk::Transform::from_row(
                                scale,
                                0.0,
                                0.0,
                                scale,
                                glyph.x + bitmap.x as f32 * scale,
                                glyph.y - bitmap.y as f32 * scale,
                            ),
                            tint: bitmap.tint,
                        });
                    }
                    GlyphArt::Layers(layers) => {
                        for (path, color) in layers {
                            if let Some(path) = path.clone().transform(transform) {
                                paints.push(GlyphPaint::Path(path, *color));
                            }
                        }
                    }
                }
            } else {
                glyph
                    .file
                    .push_glyph(glyph.id, glyph.weight, transform, &mut monochrome);
            }
        }
        if let Some(path) = monochrome.finish() {
            paints.push(GlyphPaint::Path(path, None));
        }
        paints
    }
}

pub(crate) enum GlyphPaint {
    Path(sk::Path, Option<nsh_core::Color>),
    Image {
        image: Arc<sk::Pixmap>,
        transform: sk::Transform,
        tint: bool,
    },
}

struct Glyph {
    file: Arc<FontFile>,
    id: u16,
    x: f32,
    y: f32,
    scale: f32,
    weight: u16,
    size: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum FaceId {
    Registered(fontdb::ID),
    System(fontdb::ID),
}

#[derive(PartialEq, Eq, Hash)]
struct FontKey {
    family: Option<Arc<str>>,
    weight: u16,
    italic: bool,
}

impl FontKey {
    fn new(font: &CanvasFont) -> Self {
        Self {
            family: font.family.clone(),
            weight: font.weight,
            italic: font.italic,
        }
    }
}

#[derive(PartialEq, Eq, Hash)]
struct ShapeKey {
    font: FontKey,
    size: u32,
    variant: u8,
    text: String,
}

impl ShapeKey {
    fn new(font: &CanvasFont, text: &str) -> Self {
        Self {
            font: FontKey::new(font),
            size: font.size.0.to_bits(),
            variant: font.variant.0,
            text: text.to_owned(),
        }
    }
}

#[derive(Default)]
struct Fonts {
    registered: fontdb::Database,
    registered_data: Vec<Arc<Vec<u8>>>,
    system: Option<fontdb::Database>,
    files: HashMap<FaceId, Arc<FontFile>>,
    choices: HashMap<FontKey, FaceId>,
    fallback: HashMap<(FaceId, char), Option<FaceId>>,
    shapes: HashMap<ShapeKey, Arc<ShapedText>>,
    shape_bytes: usize,
}

impl Fonts {
    fn system(&mut self) -> &fontdb::Database {
        self.system.get_or_insert_with(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            for family in [
                "Arial",
                "Helvetica",
                "DejaVu Sans",
                "Noto Sans",
                "Liberation Sans",
            ] {
                if db
                    .query(&fontdb::Query {
                        families: &[fontdb::Family::Name(family)],
                        ..Default::default()
                    })
                    .is_some()
                {
                    db.set_sans_serif_family(family);
                    break;
                }
            }
            db
        })
    }

    fn resolve(&mut self, font: &CanvasFont) -> Option<FaceId> {
        let key = FontKey::new(font);
        if let Some(id) = self.choices.get(&key) {
            return Some(*id);
        }
        let family = font
            .family
            .as_deref()
            .map_or(fontdb::Family::SansSerif, fontdb::Family::Name);
        let registered_families = [family];
        let system_families = [family, fontdb::Family::SansSerif];
        let query = |families| fontdb::Query {
            families,
            weight: fontdb::Weight(font.weight),
            style: if font.italic {
                fontdb::Style::Italic
            } else {
                fontdb::Style::Normal
            },
            ..Default::default()
        };
        // Named app faces take precedence even when a same-family system face
        // was already used before registration.
        let registered = font
            .family
            .as_ref()
            .and_then(|_| self.registered.query(&query(&registered_families)))
            .map(FaceId::Registered);
        let id = registered
            .or_else(|| {
                let db = self.system();
                db.query(&query(&system_families))
                    .or_else(|| db.faces().next().map(|face| face.id))
                    .map(FaceId::System)
            })
            .or_else(|| {
                self.registered
                    .faces()
                    .next()
                    .map(|face| FaceId::Registered(face.id))
            })?;
        if self.choices.len() >= 256 {
            self.choices.clear();
        }
        self.choices.insert(key, id);
        Some(id)
    }

    fn database(&self, id: FaceId) -> Option<(&fontdb::Database, fontdb::ID)> {
        match id {
            FaceId::Registered(id) => Some((&self.registered, id)),
            FaceId::System(id) => Some((self.system.as_ref()?, id)),
        }
    }

    fn file(&mut self, id: FaceId) -> Option<Arc<FontFile>> {
        if let Some(file) = self.files.get(&id) {
            return Some(file.clone());
        }
        let (db, face) = self.database(id)?;
        let file = Arc::new(db.with_face_data(face, |data, index| FontFile {
            data: Arc::from(data),
            index,
            glyphs: Mutex::new(HashMap::new()),
            art: Mutex::new(ArtCache::default()),
        })?);
        if self.files.len() >= 64
            || self
                .files
                .values()
                .map(|font| font.data.len())
                .sum::<usize>()
                .saturating_add(file.data.len())
                > FONT_CACHE_BYTES
        {
            self.files.clear();
        }
        self.files.insert(id, file.clone());
        Some(file)
    }

    fn has(&self, id: FaceId, character: char) -> bool {
        self.database(id)
            .and_then(|(db, id)| {
                db.with_face_data(id, |data, index| {
                    ttf_parser::Face::parse(data, index)
                        .ok()
                        .and_then(|face| face.glyph_index(character))
                        .is_some()
                })
            })
            .unwrap_or(false)
    }

    fn for_character(&mut self, primary: FaceId, character: char) -> Option<FaceId> {
        if let Some(id) = self.fallback.get(&(primary, character)) {
            return *id;
        }
        let id = if self.has(primary, character) {
            Some(primary)
        } else {
            let registered = self
                .registered
                .faces()
                .map(|face| FaceId::Registered(face.id))
                .find(|&id| self.has(id, character));
            registered.or_else(|| {
                self.system();
                self.system
                    .as_ref()
                    .unwrap()
                    .faces()
                    .map(|face| FaceId::System(face.id))
                    .find(|&id| self.has(id, character))
            })
        };
        if self.fallback.len() >= 4096 {
            self.fallback.clear();
        }
        self.fallback.insert((primary, character), id);
        id
    }

    fn shape(&mut self, font: &CanvasFont, text: &str) -> ShapedText {
        let mut result = ShapedText {
            width: 0.0,
            ascent: 0.0,
            descent: 0.0,
            glyphs: Vec::new(),
        };
        let Some(primary) = self.resolve(font) else {
            return result;
        };
        // Canvas treats ASCII whitespace as spaces, rather than starting new
        // lines. The same normalization feeds synchronous measurement and draw.
        let text: String = text
            .chars()
            .map(|c| if c.is_ascii_whitespace() { ' ' } else { c })
            .collect();
        let bidi = BidiInfo::new(&text, None);
        let features = font_features(font.variant);
        let size = if font.size.is_finite() {
            font.size.0.max(0.1)
        } else {
            14.0
        };
        for paragraph in &bidi.paragraphs {
            let (levels, visual) = bidi.visual_runs(paragraph, paragraph.range.clone());
            for range in visual {
                let rtl = levels.get(range.start).is_some_and(Level::is_rtl);
                let mut runs: Vec<(FaceId, String)> = Vec::new();
                for grapheme in text[range].graphemes(true) {
                    let base = grapheme.chars().find(|c| !c.is_control()).unwrap_or(' ');
                    let id = self.for_character(primary, base).unwrap_or(primary);
                    if let Some((last, run)) = runs.last_mut()
                        && *last == id
                    {
                        run.push_str(grapheme);
                    } else {
                        runs.push((id, grapheme.to_owned()));
                    }
                }
                if rtl {
                    runs.reverse();
                }
                for (id, run) in runs {
                    let Some(file) = self.file(id) else {
                        continue;
                    };
                    let Some(mut face) = rustybuzz::Face::from_slice(&file.data, file.index) else {
                        continue;
                    };
                    face.set_variations(&[rustybuzz::Variation {
                        tag: ttf_parser::Tag::from_bytes(b"wght"),
                        value: font.weight as f32,
                    }]);
                    let mut buffer = rustybuzz::UnicodeBuffer::new();
                    buffer.push_str(&run);
                    buffer.guess_segment_properties();
                    buffer.set_direction(if rtl {
                        rustybuzz::Direction::RightToLeft
                    } else {
                        rustybuzz::Direction::LeftToRight
                    });
                    let shaped = rustybuzz::shape(&face, &features, buffer);
                    let scale = size / face.units_per_em() as f32;
                    result.ascent = result.ascent.max(face.ascender() as f32 * scale);
                    result.descent = result.descent.max(-(face.descender() as f32) * scale);
                    let mut y = 0.0;
                    for (glyph, position) in
                        shaped.glyph_infos().iter().zip(shaped.glyph_positions())
                    {
                        result.glyphs.push(Glyph {
                            file: file.clone(),
                            id: glyph.glyph_id as u16,
                            x: result.width + position.x_offset as f32 * scale,
                            y: -(y + position.y_offset as f32 * scale),
                            scale,
                            weight: font.weight,
                            size,
                        });
                        result.width += position.x_advance as f32 * scale;
                        y += position.y_advance as f32 * scale;
                    }
                }
            }
        }
        result
    }
}

struct FontFile {
    data: Arc<[u8]>,
    index: u32,
    glyphs: Mutex<HashMap<(u16, u16), Option<sk::Path>>>,
    art: Mutex<ArtCache>,
}

#[derive(Default)]
struct ArtCache {
    values: HashMap<(u16, u16, u16), Option<Arc<GlyphArt>>>,
    bytes: usize,
}

enum GlyphArt {
    Bitmap(BitmapGlyph),
    Layers(Vec<(sk::Path, Option<nsh_core::Color>)>),
}

struct BitmapGlyph {
    image: Arc<sk::Pixmap>,
    x: i16,
    y: i16,
    pixels_per_em: u16,
    tint: bool,
}

impl FontFile {
    fn art(&self, glyph: u16, weight: u16, size: u16) -> Option<Arc<GlyphArt>> {
        let mut cache = self.art.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(art) = cache.values.get(&(glyph, weight, size)) {
            return art.clone();
        }
        let art = (|| {
            let mut face = ttf_parser::Face::parse(&self.data, self.index).ok()?;
            let _ = face.set_variation(ttf_parser::Tag::from_bytes(b"wght"), weight as f32);
            if let Some(bitmap) = face.glyph_raster_image(ttf_parser::GlyphId(glyph), size) {
                if bitmap.pixels_per_em == 0 {
                    return None;
                }
                let tint = !matches!(
                    bitmap.format,
                    ttf_parser::RasterImageFormat::PNG
                        | ttf_parser::RasterImageFormat::BitmapPremulBgra32
                );
                return Some(GlyphArt::Bitmap(BitmapGlyph {
                    image: Arc::new(decode_bitmap(bitmap)?),
                    x: bitmap.x,
                    y: bitmap.y,
                    pixels_per_em: bitmap.pixels_per_em,
                    tint,
                }));
            }
            colr_v0_layers(&face, glyph).map(GlyphArt::Layers)
        })()
        .map(Arc::new);
        let bytes = art.as_ref().map_or(0, |art| match &**art {
            GlyphArt::Bitmap(bitmap) => bitmap.image.data().len(),
            GlyphArt::Layers(layers) => layers
                .iter()
                .map(|(path, _)| std::mem::size_of_val(path.points()))
                .sum(),
        });
        if cache.values.len() >= 256 || cache.bytes.saturating_add(bytes) > BITMAP_CACHE_BYTES {
            cache.values.clear();
            cache.bytes = 0;
        }
        cache.bytes += bytes;
        cache.values.insert((glyph, weight, size), art.clone());
        art
    }

    fn push_glyph(
        &self,
        glyph: u16,
        weight: u16,
        transform: sk::Transform,
        path: &mut sk::PathBuilder,
    ) {
        let mut glyphs = self.glyphs.lock().unwrap_or_else(|e| e.into_inner());
        if glyphs.len() >= 4096 {
            glyphs.clear();
        }
        let outline = glyphs.entry((glyph, weight)).or_insert_with(|| {
            let mut face = ttf_parser::Face::parse(&self.data, self.index).ok()?;
            let _ = face.set_variation(ttf_parser::Tag::from_bytes(b"wght"), weight as f32);
            let mut sink = Outline(sk::PathBuilder::new());
            let glyph = ttf_parser::GlyphId(glyph);
            if face.outline_glyph(glyph, &mut sink).is_none() {
                if face.glyph_raster_image(glyph, u16::MAX).is_some() || face.is_color_glyph(glyph)
                {
                    face.outline_glyph(face.glyph_index('\u{fffd}')?, &mut sink)?;
                } else {
                    return None;
                }
            }
            sink.0.finish()
        });
        if let Some(outline) = outline
            && let Some(placed) = outline.clone().transform(transform)
        {
            path.push_path(&placed);
        }
    }
}

fn decode_bitmap(bitmap: ttf_parser::RasterGlyphImage<'_>) -> Option<sk::Pixmap> {
    use ttf_parser::RasterImageFormat as F;
    let (width, height) = (bitmap.width as u32, bitmap.height as u32);
    if width == 0 || height == 0 || width > 1024 || height > 1024 {
        return None;
    }
    if bitmap.format == F::PNG {
        let mut reader = ::image::ImageReader::with_format(
            std::io::Cursor::new(bitmap.data),
            ::image::ImageFormat::Png,
        );
        let mut limits = ::image::Limits::default();
        limits.max_image_width = Some(1024);
        limits.max_image_height = Some(1024);
        limits.max_alloc = Some(4 << 20);
        reader.limits(limits);
        let image = reader.decode().ok()?.into_rgba8();
        let raster = crate::image::Raster {
            width: image.width(),
            height: image.height(),
            rgba: image.into_raw(),
        };
        return raster.pixmap();
    }
    let mut pixmap = sk::Pixmap::new(width, height)?;
    if bitmap.format == F::BitmapPremulBgra32 {
        let bytes = bitmap.data.get(..width as usize * height as usize * 4)?;
        for (pixel, channels) in pixmap.pixels_mut().iter_mut().zip(bytes.as_chunks::<4>().0) {
            let a = channels[3];
            *pixel = sk::PremultipliedColorU8::from_rgba(
                channels[2].min(a),
                channels[1].min(a),
                channels[0].min(a),
                a,
            )?;
        }
        return Some(pixmap);
    }
    let (bits, packed) = match bitmap.format {
        F::BitmapMono => (1, false),
        F::BitmapMonoPacked => (1, true),
        F::BitmapGray2 => (2, false),
        F::BitmapGray2Packed => (2, true),
        F::BitmapGray4 => (4, false),
        F::BitmapGray4Packed => (4, true),
        F::BitmapGray8 => (8, true),
        _ => return None,
    };
    let row_bits = if packed {
        width as usize * bits
    } else {
        (width as usize * bits).next_multiple_of(8)
    };
    let maximum = (1u16 << bits) - 1;
    for y in 0..height as usize {
        for x in 0..width as usize {
            let bit = y * row_bits + x * bits;
            let value = (bitmap.data.get(bit / 8)? >> (8 - bits - bit % 8)) as u16 & maximum;
            let alpha = (value * 255 / maximum) as u8;
            pixmap.pixels_mut()[y * width as usize + x] =
                sk::ColorU8::from_rgba(255, 255, 255, alpha).premultiply();
        }
    }
    Some(pixmap)
}

/// COLR v0 is a bounded list of solid-colored outline layers. COLR v1 paint
/// graphs and SVG glyph documents retain the replacement-outline fallback.
fn colr_v0_layers(
    face: &ttf_parser::Face<'_>,
    glyph: u16,
) -> Option<Vec<(sk::Path, Option<nsh_core::Color>)>> {
    let table = face
        .raw_face()
        .table(ttf_parser::Tag::from_bytes(b"COLR"))?;
    let palette = ttf_parser::cpal::Table::parse(
        face.raw_face()
            .table(ttf_parser::Tag::from_bytes(b"CPAL"))?,
    )?;
    let u16_at = |at: usize| {
        Some(u16::from_be_bytes(
            table.get(at..at.checked_add(2)?)?.try_into().ok()?,
        ))
    };
    let u32_at = |at: usize| {
        Some(u32::from_be_bytes(
            table.get(at..at.checked_add(4)?)?.try_into().ok()?,
        ))
    };
    if u16_at(0)? > 1 {
        return None;
    }
    let records = u16_at(2)? as usize;
    let base = u32_at(4)? as usize;
    let layers = u32_at(8)? as usize;
    let total = u16_at(12)? as usize;
    for record in 0..records {
        let at = base.checked_add(record.checked_mul(6)?)?;
        if u16_at(at)? != glyph {
            continue;
        }
        let first = u16_at(at + 2)? as usize;
        let count = u16_at(at + 4)? as usize;
        if count == 0 || count > 256 || first.checked_add(count)? > total {
            return None;
        }
        let mut result = Vec::with_capacity(count);
        for layer in first..first + count {
            let at = layers.checked_add(layer.checked_mul(4)?)?;
            let id = u16_at(at)?;
            let entry = u16_at(at + 2)?;
            let color = if entry == 0xffff {
                None
            } else {
                let c = palette.get(0, entry)?;
                Some(nsh_core::Color::rgba(c.red, c.green, c.blue, c.alpha))
            };
            let mut path = Outline(sk::PathBuilder::new());
            if face
                .outline_glyph(ttf_parser::GlyphId(id), &mut path)
                .is_some()
                && let Some(path) = path.0.finish()
            {
                result.push((path, color));
            }
        }
        return (!result.is_empty()).then_some(result);
    }
    None
}

pub(crate) fn tint_bitmap(image: &sk::Pixmap, color: nsh_core::Color) -> sk::Pixmap {
    let mut output = image.clone();
    let c = color.0;
    for pixel in output.pixels_mut() {
        let alpha = (pixel.alpha() as u16 * (c & 255) as u16 / 255) as u8;
        *pixel = sk::ColorU8::from_rgba((c >> 24) as u8, (c >> 16) as u8, (c >> 8) as u8, alpha)
            .premultiply();
    }
    output
}

struct Outline(sk::PathBuilder);

impl ttf_parser::OutlineBuilder for Outline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.0.quad_to(x1, y1, x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.0.cubic_to(x1, y1, x2, y2, x, y);
    }
    fn close(&mut self) {
        self.0.close();
    }
}

fn font_features(variant: FontVariant) -> Vec<rustybuzz::Feature> {
    [
        (FontVariant::SMALL_CAPS, "smcp"),
        (FontVariant::TABULAR_NUMS, "tnum"),
        (FontVariant::PROPORTIONAL_NUMS, "pnum"),
        (FontVariant::OLDSTYLE_NUMS, "onum"),
        (FontVariant::LINING_NUMS, "lnum"),
    ]
    .into_iter()
    .filter(|(bit, _)| variant.0 & bit != 0)
    .filter_map(|(_, feature)| feature.parse().ok())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    const PACIFICO: &[u8] =
        include_bytes!("../../../tests/compat/cases/layout/fonts/Pacifico-Regular.ttf");

    fn with_tables(additional: Vec<([u8; 4], Vec<u8>)>) -> Vec<u8> {
        let count = u16::from_be_bytes(PACIFICO[4..6].try_into().unwrap()) as usize;
        let mut tables: Vec<([u8; 4], Vec<u8>)> = (0..count)
            .map(|i| {
                let at = 12 + i * 16;
                let tag = PACIFICO[at..at + 4].try_into().unwrap();
                let offset =
                    u32::from_be_bytes(PACIFICO[at + 8..at + 12].try_into().unwrap()) as usize;
                let length =
                    u32::from_be_bytes(PACIFICO[at + 12..at + 16].try_into().unwrap()) as usize;
                (tag, PACIFICO[offset..offset + length].to_vec())
            })
            .filter(|(tag, _)| !additional.iter().any(|(extra, _)| tag == extra))
            .collect();
        tables.extend(additional);
        tables.sort_by_key(|(tag, _)| *tag);
        let count = tables.len();
        let mut result = vec![0; 12 + count * 16];
        result[..4].copy_from_slice(&PACIFICO[..4]);
        result[4..6].copy_from_slice(&(count as u16).to_be_bytes());
        let power = 1usize << (usize::BITS - count.leading_zeros() - 1);
        result[6..8].copy_from_slice(&((power * 16) as u16).to_be_bytes());
        result[8..10].copy_from_slice(&(power.ilog2() as u16).to_be_bytes());
        result[10..12].copy_from_slice(&(((count - power) * 16) as u16).to_be_bytes());
        for (i, (tag, body)) in tables.into_iter().enumerate() {
            let at = 12 + i * 16;
            result[at..at + 4].copy_from_slice(&tag);
            let offset = result.len() as u32;
            result[at + 8..at + 12].copy_from_slice(&offset.to_be_bytes());
            result[at + 12..at + 16].copy_from_slice(&(body.len() as u32).to_be_bytes());
            result.extend(body);
            while !result.len().is_multiple_of(4) {
                result.push(0);
            }
        }
        result
    }

    fn png() -> Vec<u8> {
        use ::image::ImageEncoder;
        let mut bytes = Vec::new();
        ::image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(
                &[255, 0, 0, 255, 0, 0, 255, 255],
                2,
                1,
                ::image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        bytes
    }

    fn shaped_a() -> u16 {
        let fonts = empty_catalog();
        fonts.register(PACIFICO).unwrap();
        // Pacifico's default contextual substitutions choose an alternate 'a'.
        // Color tables index shaped glyphs, rather than Unicode cmap entries.
        fonts
            .shape(&CanvasFont::parse("16px Pacifico").unwrap(), "a")
            .glyphs[0]
            .id
    }

    fn empty_catalog() -> FontCatalog {
        let catalog = FontCatalog::default();
        catalog.0.state.lock().unwrap().system = Some(fontdb::Database::new());
        catalog
    }

    #[test]
    fn registration_is_instance_local_and_invalidates_cached_fallbacks() {
        let first = empty_catalog();
        let second = empty_catalog();
        let font = CanvasFont::parse("24px Pacifico").unwrap();
        assert_eq!(first.measure(&font, "Native Shell"), Points::ZERO);
        first.register(PACIFICO).unwrap();
        let width = first.measure(&font, "Native Shell");
        assert!(width > Points::ZERO);
        assert_eq!(first.revision(), 1);
        assert_eq!(second.measure(&font, "Native Shell"), Points::ZERO);
        assert_eq!(second.revision(), 0);
        let shared = first.clone();
        assert_eq!(shared.measure(&font, "Native Shell"), width);
        shared.register(PACIFICO).unwrap();
        assert_eq!(first.revision(), 1);
    }

    #[test]
    fn shaped_advances_scale_with_canvas_size_and_have_drawable_outlines() {
        let fonts = empty_catalog();
        fonts.register(PACIFICO).unwrap();
        let small = CanvasFont::parse("20px Pacifico").unwrap();
        let large = CanvasFont::parse("40px Pacifico").unwrap();
        let shaped = fonts.shape(&small, "office AV");
        assert!(
            shaped
                .paints()
                .iter()
                .any(|paint| matches!(paint, GlyphPaint::Path(_, None)))
        );
        assert!((fonts.measure(&large, "office AV") - px(shaped.width * 2.0)).abs() < px(0.001));
        assert!(shaped.width > 0.0 && shaped.ascent > 0.0);
        assert!(fonts.register(b"invalid").is_err());
        assert_eq!(fonts.revision(), 1);
    }

    #[test]
    fn sbix_png_glyphs_keep_color_baseline_and_measured_advance() {
        let face = ttf_parser::Face::parse(PACIFICO, 0).unwrap();
        let glyph = shaped_a() as usize;
        let count = face.number_of_glyphs() as usize;
        let png = png();
        let payload = [
            0i16.to_be_bytes().as_slice(),
            1i16.to_be_bytes().as_slice(),
            b"png ",
            &png,
        ]
        .concat();
        let start = 4 + (count + 1) * 4;
        let mut strike = vec![0; start];
        strike[..2].copy_from_slice(&16u16.to_be_bytes());
        strike[2..4].copy_from_slice(&72u16.to_be_bytes());
        for i in 0..=count {
            strike[4 + i * 4..8 + i * 4].copy_from_slice(
                &((start + if i > glyph { payload.len() } else { 0 }) as u32).to_be_bytes(),
            );
        }
        strike.extend(payload);
        let mut sbix = [
            1u16.to_be_bytes().as_slice(),
            1u16.to_be_bytes().as_slice(),
            1u32.to_be_bytes().as_slice(),
            12u32.to_be_bytes().as_slice(),
        ]
        .concat();
        sbix.extend(strike);
        let data = with_tables(vec![(*b"sbix", sbix)]);
        let fonts = empty_catalog();
        fonts.register(&data).unwrap();
        let font = CanvasFont::parse("16px Pacifico").unwrap();
        let shaped = fonts.shape(&font, "a");
        let paints = shaped.paints();
        let GlyphPaint::Image {
            image,
            transform,
            tint,
        } = &paints[0]
        else {
            panic!("bitmap glyph should keep its image");
        };
        assert!(!tint);
        assert_eq!(image.pixel(0, 0).unwrap().red(), 255);
        assert_eq!(image.pixel(1, 0).unwrap().blue(), 255);
        assert_eq!((transform.tx, transform.ty), (0.0, -1.0));
        let original = empty_catalog();
        original.register(PACIFICO).unwrap();
        assert_eq!(px(shaped.width), original.measure(&font, "a"));
        let drawing = nsh_core::canvas::Drawing {
            ops: vec![
                nsh_core::canvas::CanvasOp::Font(font),
                nsh_core::canvas::CanvasOp::FillText {
                    text: "a".into(),
                    x: nsh_core::canvas::Num::Const(4.0),
                    y: nsh_core::canvas::Num::Const(20.0),
                    max_width: None,
                },
            ],
            ..nsh_core::canvas::Drawing::default()
        };
        let raster =
            crate::canvas::rasterize(&drawing, &[], px(32.0), px(32.0), &HashMap::new(), &fonts)
                .unwrap();
        assert_eq!(&raster.rgba[(19 * 32 + 4) * 4..][..4], [255, 0, 0, 255]);
        assert_eq!(&raster.rgba[(19 * 32 + 5) * 4..][..4], [0, 0, 255, 255]);
    }

    #[test]
    fn colr_v0_layers_use_palette_and_foreground_without_replacing_the_glyph() {
        let face = ttf_parser::Face::parse(PACIFICO, 0).unwrap();
        let (base, first, second) = (
            shaped_a(),
            face.glyph_index('A').unwrap().0,
            face.glyph_index('B').unwrap().0,
        );
        let colr = [
            0u16.to_be_bytes().as_slice(),
            1u16.to_be_bytes().as_slice(),
            14u32.to_be_bytes().as_slice(),
            20u32.to_be_bytes().as_slice(),
            2u16.to_be_bytes().as_slice(),
            base.to_be_bytes().as_slice(),
            0u16.to_be_bytes().as_slice(),
            2u16.to_be_bytes().as_slice(),
            first.to_be_bytes().as_slice(),
            0u16.to_be_bytes().as_slice(),
            second.to_be_bytes().as_slice(),
            0xffffu16.to_be_bytes().as_slice(),
        ]
        .concat();
        let cpal = [
            0u16.to_be_bytes().as_slice(),
            1u16.to_be_bytes().as_slice(),
            1u16.to_be_bytes().as_slice(),
            1u16.to_be_bytes().as_slice(),
            14u32.to_be_bytes().as_slice(),
            0u16.to_be_bytes().as_slice(),
            &[0, 0, 255, 255],
        ]
        .concat();
        let fonts = empty_catalog();
        fonts
            .register(&with_tables(vec![(*b"COLR", colr), (*b"CPAL", cpal)]))
            .unwrap();
        let paints = fonts
            .shape(&CanvasFont::parse("24px Pacifico").unwrap(), "a")
            .paints();
        assert_eq!(paints.len(), 2);
        assert!(matches!(
            &paints[0],
            GlyphPaint::Path(_, Some(nsh_core::Color(0xff0000ff)))
        ));
        assert!(matches!(&paints[1], GlyphPaint::Path(_, None)));
    }

    #[test]
    fn grayscale_and_bgra_glyph_decoders_preserve_alpha_and_reject_excessive_sizes() {
        let bitmap = ttf_parser::RasterGlyphImage {
            x: 0,
            y: 1,
            width: 3,
            height: 1,
            pixels_per_em: 16,
            format: ttf_parser::RasterImageFormat::BitmapMono,
            data: &[0b10100000],
        };
        let image = decode_bitmap(bitmap).unwrap();
        assert_eq!(
            image.pixels().iter().map(|p| p.alpha()).collect::<Vec<_>>(),
            [255, 0, 255]
        );
        assert!(
            decode_bitmap(ttf_parser::RasterGlyphImage {
                width: 1025,
                ..bitmap
            })
            .is_none()
        );
        let bitmap = ttf_parser::RasterGlyphImage {
            width: 1,
            format: ttf_parser::RasterImageFormat::BitmapPremulBgra32,
            data: &[0, 128, 0, 128],
            ..bitmap
        };
        let image = decode_bitmap(bitmap).unwrap();
        assert_eq!(
            (image.pixels()[0].green(), image.pixels()[0].alpha()),
            (128, 128)
        );
    }
}
