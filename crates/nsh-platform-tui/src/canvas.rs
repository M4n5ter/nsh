//! Canvas raster replay in virtual points, reduced to terminal half blocks by
//! the image painter. Geometry, compositing, gradients and shadows follow the
//! same tiny-skia replay model as the GPUI backend. Font outlines are shaped
//! with rustybuzz from the surface's registered and system font catalog.

use std::{
    collections::HashMap,
    f64::consts::{PI, TAU},
    rc::Rc,
    sync::Arc,
};

use nsh_core::{
    Color,
    animation::AnimationValue,
    canvas::{
        CanvasFont, CanvasGradient, CanvasOp, CanvasTextAlign, CanvasTextBaseline, Composite,
        Drawing, FillRule, GradientShape, LineCap, LineJoin, Num, Paint, PathOp, arc_sweep,
        fit_radii,
    },
    geometry::{Bounds, Corners, Points, Radians, ScaledPixels, size},
};
use tiny_skia as sk;

use crate::{
    fonts::{FontCatalog, GlyphPaint, tint_bitmap},
    image::Raster,
};

const GRADIENT_STEPS: usize = 1024;
const MAX_SIGMA: f64 = 256.0;
const MAX_SIDE: u32 = 8192;
const MAX_PIXELS: u64 = 8 << 20;

/// Replays one canvas into straight RGBA pixels. Call on a worker, caching the
/// result until drawing, native slots, size or image dependencies change.
pub(crate) fn rasterize(
    drawing: &Drawing,
    args: &[(u16, AnimationValue)],
    width: Points,
    height: Points,
    images: &HashMap<Arc<str>, Arc<Raster>>,
    fonts: &FontCatalog,
) -> Result<Raster, String> {
    if !width.is_finite() || !height.is_finite() || width <= Points::ZERO || height <= Points::ZERO
    {
        return Err("canvas has no finite positive size".into());
    }
    let (w, h) = (width.ceil().0 as u32, height.ceil().0 as u32);
    if w > MAX_SIDE || h > MAX_SIDE || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(
            "canvas exceeds the terminal raster limit (8192 per side, 8 million pixels)".into(),
        );
    }
    let mut used_images = HashMap::new();
    for image in drawing.images() {
        if !used_images.contains_key(&image.uri)
            && let Some(raster) = images.get(&image.uri)
            && let Some(pixmap) = raster.pixmap()
        {
            used_images.insert(image.uri.clone(), Arc::new(pixmap));
        }
    }
    let base = Affine::scale(1.0, 1.0);
    let mut replay = Replay {
        args,
        images: &used_images,
        fonts,
        size: (w, h),
        base,
        state: State::new(base),
        stack: Vec::new(),
        path: PathBuf::default(),
        pixmap: sk::Pixmap::new(w, h),
    };
    for op in &drawing.ops {
        replay.op(op);
    }
    replay
        .pixmap
        .map(Raster::from_pixmap)
        .ok_or_else(|| "canvas raster allocation failed".into())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct IRect {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl IRect {
    /// The pixels the rectangle touches; `None` if it is empty or not
    /// finite.
    fn round_out(x0: f32, y0: f32, x1: f32, y1: f32) -> Option<IRect> {
        let far = 1.0e8;
        if !(x0.abs() < far && y0.abs() < far && x1.abs() < far && y1.abs() < far) {
            return None;
        }
        let r = IRect {
            x0: x0.floor() as i32,
            y0: y0.floor() as i32,
            x1: x1.ceil() as i32,
            y1: y1.ceil() as i32,
        };
        (r.x1 > r.x0 && r.y1 > r.y0).then_some(r)
    }

    fn width(&self) -> i32 {
        self.x1 - self.x0
    }

    fn height(&self) -> i32 {
        self.y1 - self.y0
    }

    fn intersect(&self, o: &IRect) -> Option<IRect> {
        let r = IRect {
            x0: self.x0.max(o.x0),
            y0: self.y0.max(o.y0),
            x1: self.x1.min(o.x1),
            y1: self.y1.min(o.y1),
        };
        (r.x1 > r.x0 && r.y1 > r.y0).then_some(r)
    }
}

/// Canvas' drawing state (`save` / `restore`).
#[derive(Clone)]
struct State {
    /// User space to the pixmap's coordinates.
    transform: Affine,
    fill: Paint,
    stroke: Paint,
    line_width: f64,
    cap: LineCap,
    join: LineJoin,
    miter: f64,
    dash: Arc<[f32]>,
    dash_offset: f64,
    alpha: f64,
    composite: Composite,
    /// Color, blur, offset x, offset y.
    shadow: (Color, f64, f64, f64),
    font: CanvasFont,
    align: CanvasTextAlign,
    baseline: CanvasTextBaseline,
    /// The transform is not invertible (`scale(0, 1)`): nothing draws
    /// until `setTransform` or `restore`.
    singular: bool,
    /// The clip (pixmap coordinates), and its bounds.
    clip: Option<(Rc<sk::Mask>, [f32; 4])>,
}

impl State {
    fn new(transform: Affine) -> Self {
        State {
            transform,
            fill: Paint::Color(Color::BLACK),
            stroke: Paint::Color(Color::BLACK),
            line_width: 1.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter: 10.0,
            dash: Arc::new([]),
            dash_offset: 0.0,
            alpha: 1.0,
            composite: Composite::SourceOver,
            shadow: (Color::TRANSPARENT, 0.0, 0.0, 0.0),
            font: CanvasFont::default(),
            align: CanvasTextAlign::Start,
            baseline: CanvasTextBaseline::Alphabetic,
            singular: false,
            clip: None,
        }
    }
}

/// How a shape is painted.
#[derive(Clone, Copy)]
enum Mode {
    Fill(FillRule),
    Stroke,
}

/// What a shape is filled with.
enum Source {
    Solid(sk::Color),
    /// Pixels over part of the pixmap (gradients), from `(x, y)`.
    Pixels(sk::Pixmap, i32, i32),
    /// An image: its pixels, the transform from them to the pixmap and the
    /// alpha.
    Image(Arc<sk::Pixmap>, sk::Transform, f32),
}

struct Replay<'a> {
    fonts: &'a FontCatalog,
    args: &'a [(u16, AnimationValue)],
    images: &'a HashMap<Arc<str>, Arc<sk::Pixmap>>,
    size: (u32, u32),
    /// The view's coordinates to the pixmap's.
    base: Affine,
    state: State,
    stack: Vec<State>,
    /// The current path, in pixmap coordinates.
    path: PathBuf,
    pixmap: Option<sk::Pixmap>,
}

impl<'a> Replay<'a> {
    fn n(&self, n: Num) -> f64 {
        n.get(|s| {
            self.args
                .iter()
                .find(|(slot, _)| *slot == s)
                .map_or(0.0, |(_, value)| value.number())
        }) as f64
    }

    fn color(&self, paint: &Paint) -> Option<Color> {
        match paint {
            Paint::Color(c) => Some(*c),
            Paint::Slot(s) => match self
                .args
                .iter()
                .find(|(slot, _)| slot == s)
                .map(|(_, value)| *value)
            {
                Some(AnimationValue::Color(c)) => Some(Color(c)),
                _ => Some(Color::TRANSPARENT),
            },
            Paint::Gradient(_) => None,
        }
    }

    fn paint(&self, mode: Mode) -> &Paint {
        match mode {
            Mode::Fill(_) => &self.state.fill,
            Mode::Stroke => &self.state.stroke,
        }
    }

    /// Multiplies the transform by `m` (canvas `transform`).
    fn concat(&mut self, m: Affine) {
        if self.state.singular {
            return;
        }
        let next = self.state.transform.then(&m);
        if next.invert().is_some() {
            self.state.transform = next;
        } else {
            self.state.singular = true;
        }
    }

    fn op(&mut self, op: &CanvasOp) {
        match op {
            CanvasOp::Save => self.stack.push(self.state.clone()),
            CanvasOp::Restore => {
                if let Some(state) = self.stack.pop() {
                    self.state = state;
                }
            }
            CanvasOp::Translate(x, y) => self.concat(Affine::translate(self.n(*x), self.n(*y))),
            CanvasOp::Scale(x, y) => self.concat(Affine::scale(self.n(*x), self.n(*y))),
            CanvasOp::Rotate(a) => self.concat(Affine::rotate(self.n(*a))),
            CanvasOp::Transform(m) => {
                let [a, b, c, d, e, f] = m.map(|v| self.n(v));
                self.concat(Affine { a, b, c, d, e, f });
            }
            CanvasOp::SetTransform(m) => {
                let [a, b, c, d, e, f] = m.map(|v| self.n(v));
                let next = self.base.then(&Affine { a, b, c, d, e, f });
                self.state.singular = next.invert().is_none();
                if !self.state.singular {
                    self.state.transform = next;
                }
            }
            CanvasOp::BeginPath => self.path.clear(),
            CanvasOp::Path(p) => {
                if !self.state.singular {
                    let mut path = std::mem::take(&mut self.path);
                    self.path_op(&mut path, p);
                    self.path = path;
                }
            }
            CanvasOp::Fill(path, rule) => self.shape(path.as_deref(), Mode::Fill(*rule)),
            CanvasOp::Stroke(path) => self.shape(path.as_deref(), Mode::Stroke),
            CanvasOp::Clip(path, rule) => self.clip(path.as_deref(), *rule),
            CanvasOp::FillRect(r) => self.rect(r, Mode::Fill(FillRule::NonZero)),
            CanvasOp::StrokeRect(r) => self.rect(r, Mode::Stroke),
            CanvasOp::ClearRect(r) => self.clear_rect(r),
            CanvasOp::FillStyle(p) => self.state.fill = p.clone(),
            CanvasOp::StrokeStyle(p) => self.state.stroke = p.clone(),
            CanvasOp::LineWidth(w) => {
                let w = self.n(*w);
                // Canvas ignores zero, negative and non-finite widths.
                if w > 0.0 && w.is_finite() {
                    self.state.line_width = w;
                }
            }
            CanvasOp::LineCap(cap) => self.state.cap = *cap,
            CanvasOp::LineJoin(join) => self.state.join = *join,
            CanvasOp::MiterLimit(m) => self.state.miter = *m as f64,
            CanvasOp::LineDash(d) => self.state.dash = d.iter().map(|value| value.0).collect(),
            CanvasOp::LineDashOffset(o) => self.state.dash_offset = self.n(*o),
            CanvasOp::GlobalAlpha(a) => {
                let a = self.n(*a);
                if (0.0..=1.0).contains(&a) {
                    self.state.alpha = a;
                }
            }
            CanvasOp::Composite(m) => self.state.composite = *m,
            CanvasOp::ShadowColor(color) => self.state.shadow.0 = *color,
            CanvasOp::ShadowBlur(b) => self.state.shadow.1 = b.0 as f64,
            CanvasOp::ShadowOffset(offset) => {
                self.state.shadow.2 = offset.x.0 as f64;
                self.state.shadow.3 = offset.y.0 as f64;
            }
            CanvasOp::Font(f) => {
                // The shorthand keeps the numeric variant, as in browsers.
                let variant = self.state.font.variant;
                self.state.font = f.clone();
                self.state.font.variant = variant;
            }
            CanvasOp::FontVariantNumeric(v) => self.state.font.variant = *v,
            CanvasOp::TextAlign(a) => self.state.align = *a,
            CanvasOp::TextBaseline(b) => self.state.baseline = *b,
            CanvasOp::FillText {
                text,
                x,
                y,
                max_width,
            } => {
                let mode = Mode::Fill(FillRule::NonZero);
                self.text_op(text, self.n(*x), self.n(*y), *max_width, mode)
            }
            CanvasOp::StrokeText {
                text,
                x,
                y,
                max_width,
            } => self.text_op(text, self.n(*x), self.n(*y), *max_width, Mode::Stroke),
            CanvasOp::DrawImage {
                image,
                source,
                dest,
            } => self.image(&image.uri, *source, dest),
        }
    }

    // --- Paths ---

    /// Adds `op` (in user space) to `path` (in pixmap coordinates).
    fn path_op(&self, path: &mut PathBuf, op: &PathOp) {
        let m = self.state.transform;
        let n = |v: Num| self.n(v);
        match op {
            PathOp::MoveTo(x, y) => path.move_to(m.map((n(*x), n(*y)))),
            PathOp::LineTo(x, y) => path.line_to(m.map((n(*x), n(*y)))),
            PathOp::QuadTo(v) => {
                let [cx, cy, x, y] = v.map(n);
                path.ensure(m.map((cx, cy)));
                let p0 = self.user_point(path).unwrap_or((cx, cy));
                let [c1x, c1y, c2x, c2y] = quad_to_cubic(p0, (cx, cy), (x, y));
                path.cubic_to(m.map((c1x, c1y)), m.map((c2x, c2y)), m.map((x, y)));
            }
            PathOp::CubicTo(v) => {
                let [c1x, c1y, c2x, c2y, x, y] = v.map(n);
                path.ensure(m.map((c1x, c1y)));
                path.cubic_to(m.map((c1x, c1y)), m.map((c2x, c2y)), m.map((x, y)));
            }
            PathOp::Arc {
                x,
                y,
                radius,
                start,
                end,
                ccw,
            } => {
                let r = n(*radius);
                if r < 0.0 {
                    return;
                }
                let start = n(*start);
                let sweep =
                    arc_sweep(Radians(start as f32), Radians(n(*end) as f32), *ccw).0 as f64;
                let unit = m.then(&Affine::translate(n(*x), n(*y)).then(&Affine::scale(r, r)));
                path.arc(&unit, start, sweep);
            }
            PathOp::ArcTo(v) => {
                let [x1, y1, x2, y2, r] = v.map(n);
                self.arc_to(path, x1, y1, x2, y2, r);
            }
            PathOp::Ellipse {
                geometry,
                start,
                end,
                ccw,
            } => {
                let [x, y, rx, ry, rotation] = geometry.map(n);
                if rx < 0.0 || ry < 0.0 {
                    return;
                }
                let start = n(*start);
                let sweep =
                    arc_sweep(Radians(start as f32), Radians(n(*end) as f32), *ccw).0 as f64;
                let unit = m.then(
                    &Affine::translate(x, y)
                        .then(&Affine::rotate(rotation))
                        .then(&Affine::scale(rx, ry)),
                );
                path.arc(&unit, start, sweep);
            }
            PathOp::Rect(v) => {
                let [x, y, w, h] = v.map(n);
                path.move_to(m.map((x, y)));
                path.line_to(m.map((x + w, y)));
                path.line_to(m.map((x + w, y + h)));
                path.line_to(m.map((x, y + h)));
                path.close();
            }
            PathOp::RoundRect(rect, radii) => {
                let [x, y, w, h] = rect.map(n);
                let [tl, tr, br, bl] = radii.map(|r| Points(n(r) as f32));
                let fitted = fit_radii(
                    size(Points(w as f32), Points(h as f32)),
                    Corners {
                        top_left: tl,
                        top_right: tr,
                        bottom_right: br,
                        bottom_left: bl,
                    },
                );
                let [tl, tr, br, bl] = [
                    fitted.top_left,
                    fitted.top_right,
                    fitted.bottom_right,
                    fitted.bottom_left,
                ]
                .map(|r| f64::from(r.0));
                path.move_to(m.map((x + tl, y)));
                self.arc_to(path, x + w, y, x + w, y + h, tr);
                self.arc_to(path, x + w, y + h, x, y + h, br);
                self.arc_to(path, x, y + h, x, y, bl);
                self.arc_to(path, x, y, x + w, y, tl);
                path.close();
                path.move_to(m.map((x, y)));
            }
            PathOp::Close => path.close(),
        }
    }

    /// The path's current point in user space.
    fn user_point(&self, path: &PathBuf) -> Option<Pt> {
        let p = path.current?;
        Some(self.state.transform.invert()?.map(p))
    }

    /// Canvas `arcTo` (user space).
    fn arc_to(&self, path: &mut PathBuf, x1: f64, y1: f64, x2: f64, y2: f64, r: f64) {
        let m = self.state.transform;
        let Some(p0) = self.user_point(path) else {
            path.move_to(m.map((x1, y1)));
            return;
        };
        match arc_to_geometry(p0, (x1, y1), (x2, y2), r) {
            Some(ArcTo {
                center: (cx, cy),
                start,
                sweep,
            }) => {
                let unit = m.then(&Affine::translate(cx, cy).then(&Affine::scale(r, r)));
                path.arc(&unit, start, sweep);
            }
            None => path.line_to(m.map((x1, y1))),
        }
    }

    /// `path` (the current path when `None`) in pixmap coordinates.
    fn shape_path(&self, path: Option<&[PathOp]>) -> Option<sk::Path> {
        match path {
            None => self.path.to_skia(),
            Some(ops) => {
                let mut own = PathBuf::default();
                for op in ops {
                    self.path_op(&mut own, op);
                }
                own.to_skia()
            }
        }
    }

    /// The area `path` (pixmap coordinates) covers when painted by `mode`,
    /// and its fill rule.
    fn coverage(&self, path: sk::Path, mode: Mode) -> Option<(sk::Path, sk::FillRule)> {
        match mode {
            Mode::Fill(rule) => Some((path, fill_rule(rule))),
            Mode::Stroke => {
                let s = &self.state;
                let to_user = s.transform.invert()?;
                let mut user = path.transform(to_user.skia())?;
                // Curves are flattened finely enough for the pixmap's pixels.
                let k = s.transform.scale_factor().max(1e-3) as f32;
                if let Some((dashes, offset)) = dash_pattern(&s.dash, s.dash_offset) {
                    let dash = sk::StrokeDash::new(dashes, offset)?;
                    user = user.dash(&dash, k)?;
                }
                let stroke = sk::Stroke {
                    width: s.line_width as f32,
                    miter_limit: s.miter as f32,
                    line_cap: match s.cap {
                        LineCap::Butt => sk::LineCap::Butt,
                        LineCap::Round => sk::LineCap::Round,
                        LineCap::Square => sk::LineCap::Square,
                    },
                    // tiny-skia bevels joins past the miter limit, as canvas
                    // does.
                    line_join: match s.join {
                        LineJoin::Miter => sk::LineJoin::Miter,
                        LineJoin::Round => sk::LineJoin::Round,
                        LineJoin::Bevel => sk::LineJoin::Bevel,
                    },
                    dash: None,
                };
                let outline = user.stroke(&stroke, k)?;
                Some((
                    outline.transform(s.transform.skia())?,
                    sk::FillRule::Winding,
                ))
            }
        }
    }

    /// `fill` / `stroke` of the current path or of `path`.
    fn shape(&mut self, path: Option<&[PathOp]>, mode: Mode) {
        if self.state.singular {
            return;
        }
        if let Some(path) = self.shape_path(path) {
            self.paint_path(path, mode);
        }
    }

    /// The rectangle `r` (user space) in pixmap coordinates.
    fn rect_path(&self, r: [f64; 4]) -> Option<sk::Path> {
        let [x, y, w, h] = r;
        let m = self.state.transform;
        let mut p = PathBuf::default();
        p.move_to(m.map((x, y)));
        p.line_to(m.map((x + w, y)));
        p.line_to(m.map((x + w, y + h)));
        p.line_to(m.map((x, y + h)));
        p.close();
        p.to_skia()
    }

    /// `fillRect` / `strokeRect` (the current path stays).
    fn rect(&mut self, r: &[Num; 4], mode: Mode) {
        if self.state.singular {
            return;
        }
        if let Some(path) = self.rect_path(r.map(|v| self.n(v))) {
            self.paint_path(path, mode);
        }
    }

    /// `clearRect`: the rectangle (inside the clip) becomes transparent.
    fn clear_rect(&mut self, r: &[Num; 4]) {
        if self.state.singular {
            return;
        }
        let Some(path) = self.rect_path(r.map(|v| self.n(v))) else {
            return;
        };
        let clip = self.state.clip.clone();
        let Some(pixmap) = self.pixmap.as_mut() else {
            return;
        };
        let paint = sk::Paint {
            blend_mode: sk::BlendMode::Clear,
            ..sk::Paint::default()
        };
        pixmap.fill_path(
            &path,
            &paint,
            sk::FillRule::Winding,
            sk::Transform::identity(),
            clip.as_ref().map(|c| &*c.0),
        );
    }

    /// `clip`: intersects the clip with the current path or `path`.
    fn clip(&mut self, path: Option<&[PathOp]>, rule: FillRule) {
        if self.state.singular {
            return;
        }
        let (w, h) = self.size;
        let path = self.shape_path(path);
        let old = self.state.clip.take();
        let mut mask = match &old {
            Some((mask, _)) => (**mask).clone(),
            None => match sk::Mask::new(w, h) {
                Some(mut mask) => {
                    mask.data_mut().fill(255);
                    mask
                }
                None => return,
            },
        };
        let bounds = match &path {
            Some(path) => {
                mask.intersect_path(path, fill_rule(rule), true, sk::Transform::identity());
                let b = path.bounds();
                [b.left(), b.top(), b.right(), b.bottom()]
            }
            // An empty path clips everything away.
            None => {
                mask.clear();
                [0.0; 4]
            }
        };
        let bounds = match &old {
            Some((_, old)) => intersect(old, &bounds),
            None => bounds,
        };
        self.state.clip = Some((Rc::new(mask), bounds));
    }

    // --- Painting ---

    /// Fills or strokes `path` (pixmap coordinates) with the state's
    /// paint, alpha, composite operation and shadow.
    fn paint_path(&mut self, path: sk::Path, mode: Mode) {
        let Some((area, rule)) = self.coverage(path, mode) else {
            return;
        };
        let Some(source) = self.source(self.paint(mode).clone(), &area, rule) else {
            return;
        };
        self.emit(area.bounds(), &|pixmap, to, mask, blend| {
            fill(pixmap, &area, rule, &source, to, blend, mask);
        });
    }

    /// What `paint` fills `area` with, at the state's alpha; `None` if it
    /// draws nothing.
    fn source(&self, paint: Paint, area: &sk::Path, rule: sk::FillRule) -> Option<Source> {
        let alpha = self.state.alpha as f32;
        if let Some(color) = self.color(&paint) {
            let (r, g, b, a) = color.to_f64();
            let c = sk::Color::from_rgba(r as f32, g as f32, b as f32, a as f32 * alpha)?;
            // Copy/source-in and the other destructive modes also act where
            // their source is transparent.
            return (c.alpha() > 0.0 || destructive(self.state.composite))
                .then_some(Source::Solid(c));
        }
        let Paint::Gradient(gradient) = paint else {
            return None;
        };
        let Some(pixels) = self.clipped(area.bounds()) else {
            // Destructive composite modes act on the destination even when
            // their source is wholly outside the visible raster.
            return Some(Source::Solid(sk::Color::TRANSPARENT));
        };
        // Only the pixels the area covers are worked out (a stroke covers
        // little of its bounds).
        let mut covered = sk::Mask::new(pixels.width() as u32, pixels.height() as u32)?;
        let to_pixels = sk::Transform::from_translate(-pixels.x0 as f32, -pixels.y0 as f32);
        covered.fill_path(area, rule, true, to_pixels);
        let to_user = self.state.transform.invert()?;
        let gradient = gradient_pixels(&gradient, &to_user, pixels, &covered, alpha)?;
        Some(Source::Pixels(gradient, pixels.x0, pixels.y0))
    }

    /// The pixels of the pixmap within `bounds` and the clip.
    fn clipped(&self, bounds: sk::Rect) -> Option<IRect> {
        let (w, h) = self.size;
        let mut r = [bounds.left(), bounds.top(), bounds.right(), bounds.bottom()];
        if let Some((_, clip)) = &self.state.clip {
            r = intersect(&r, clip);
        }
        pixels_within(r, (w, h))
    }

    fn casts_shadow(&self) -> bool {
        let (color, blur, ox, oy) = self.state.shadow;
        !color.is_transparent() && (blur > 0.0 || ox != 0.0 || oy != 0.0)
    }

    /// The state's shadow offset, in pixmap pixels (shadows are in the
    /// view's coordinates, not the transform's).
    fn shadow_offset(&self) -> (f64, f64) {
        let (_, _, ox, oy) = self.state.shadow;
        let base = self.base;
        (base.a * ox + base.c * oy, base.b * ox + base.d * oy)
    }

    /// Draws what `draw` paints within `bounds` (pixmap coordinates) with
    /// the state's composite operation, clip and shadow. `draw` paints in
    /// pixmap coordinates moved by the transform it is given.
    fn emit(&mut self, bounds: sk::Rect, draw: Draw<'_>) {
        let shadow = if self.casts_shadow() {
            let (color, blur, _, _) = self.state.shadow;
            let sigma = (blur / 2.0 * self.base.scale_factor()).clamp(0.0, MAX_SIGMA);
            Some((color, sigma, self.shadow_offset()))
        } else {
            None
        };
        let mut extent = bounds;
        if let Some((_, sigma, (dx, dy))) = shadow {
            let grow = (3.0 * sigma) as f32;
            let (dx, dy) = (dx as f32, dy as f32);
            if let Some(r) = sk::Rect::from_ltrb(
                bounds.left() + dx.min(0.0) - grow,
                bounds.top() + dy.min(0.0) - grow,
                bounds.right() + dx.max(0.0) + grow,
                bounds.bottom() + dy.max(0.0) + grow,
            ) {
                extent = r;
            }
        }
        let composite = self.state.composite;
        let clip = self.state.clip.clone();
        let mask = clip.as_ref().map(|c| &*c.0);
        let (w, h) = self.size;
        let Some(pixmap) = self.pixmap.as_mut() else {
            return;
        };
        if composite == Composite::SourceOver && shadow.is_none() {
            draw(
                pixmap,
                sk::Transform::identity(),
                mask,
                sk::BlendMode::SourceOver,
            );
            return;
        }
        // The shadow and the shape combine with the pixmap as one layer: of
        // what they cover, or of all of it for the operations that change
        // the pixmap where the shape is not.
        let all = IRect {
            x0: 0,
            y0: 0,
            x1: w as i32,
            y1: h as i32,
        };
        let rect = if destructive(composite) {
            Some(all)
        } else {
            let r = [extent.left(), extent.top(), extent.right(), extent.bottom()];
            pixels_within(r, (w, h))
        };
        let Some(rect) = rect else {
            return;
        };
        let Some(mut layer) = sk::Pixmap::new(rect.width() as u32, rect.height() as u32) else {
            return;
        };
        let to_layer = sk::Transform::from_translate(-rect.x0 as f32, -rect.y0 as f32);
        draw(&mut layer, to_layer, None, sk::BlendMode::SourceOver);
        if let Some((color, sigma, offset)) = shadow {
            let area = shadow_area(bounds, rect, sigma);
            let shadow = area.and_then(|area| shadow_pixels(&layer, area, sigma, color));
            if let Some((shadow, x, y)) = shadow {
                layer.draw_pixmap(
                    0,
                    0,
                    shadow.as_ref(),
                    &sk::PixmapPaint {
                        blend_mode: sk::BlendMode::DestinationOver,
                        quality: sk::FilterQuality::Bilinear,
                        ..sk::PixmapPaint::default()
                    },
                    sk::Transform::from_translate(
                        x as f32 + offset.0 as f32,
                        y as f32 + offset.1 as f32,
                    ),
                    None,
                );
            }
        }
        pixmap.draw_pixmap(
            rect.x0,
            rect.y0,
            layer.as_ref(),
            &sk::PixmapPaint {
                blend_mode: blend_mode(composite),
                ..sk::PixmapPaint::default()
            },
            sk::Transform::identity(),
            mask,
        );
    }

    // --- Text ---

    fn text_op(&mut self, text: &Arc<str>, x: f64, y: f64, max_width: Option<Points>, mode: Mode) {
        if text.is_empty() || self.state.singular {
            return;
        }
        let shaped = self.fonts.shape(&self.state.font, text);
        let (width, ascent, descent) = (
            shaped.width as f64,
            shaped.ascent as f64,
            shaped.descent as f64,
        );
        let dx = match self.state.align {
            CanvasTextAlign::Start | CanvasTextAlign::Left => 0.0,
            CanvasTextAlign::Center => -width / 2.0,
            CanvasTextAlign::End | CanvasTextAlign::Right => -width,
        };
        let down = match self.state.baseline {
            CanvasTextBaseline::Alphabetic => 0.0,
            CanvasTextBaseline::Top => ascent,
            CanvasTextBaseline::Hanging => ascent * 0.8,
            CanvasTextBaseline::Middle => (ascent - descent) / 2.0,
            CanvasTextBaseline::Ideographic | CanvasTextBaseline::Bottom => -descent,
        };
        let squeeze = match max_width {
            Some(m) if !m.is_finite() || m <= Points::ZERO => return,
            Some(m) if (m.0 as f64) < width && width > 0.0 => m.0 as f64 / width,
            _ => 1.0,
        };
        let place = self.state.transform.then(
            &Affine::translate(x, y + down)
                .then(&Affine::scale(squeeze, 1.0))
                .then(&Affine::translate(dx, 0.0)),
        );
        for paint in shaped.paints() {
            match paint {
                GlyphPaint::Path(path, color) => {
                    if let Some(path) = path.transform(place.skia()) {
                        let previous = self.state.fill.clone();
                        if let Some(mut color) = color
                            && matches!(mode, Mode::Fill(_))
                        {
                            if let Some(foreground) = self.color(&previous) {
                                color.0 = (color.0 & !255)
                                    | ((color.0 & 255) * (foreground.0 & 255) / 255);
                            }
                            self.state.fill = Paint::Color(color);
                        }
                        self.paint_path(path, mode);
                        self.state.fill = previous;
                    }
                }
                GlyphPaint::Image {
                    image,
                    transform,
                    tint,
                } => {
                    let foreground = self.color(self.paint(mode)).unwrap_or(Color::BLACK);
                    let image = if tint {
                        Arc::new(tint_bitmap(&image, foreground))
                    } else {
                        image
                    };
                    let glyph = Affine {
                        a: transform.sx as f64,
                        b: transform.ky as f64,
                        c: transform.kx as f64,
                        d: transform.sy as f64,
                        e: transform.tx as f64,
                        f: transform.ty as f64,
                    };
                    let transform = place.then(&glyph).skia();
                    let Some(rect) =
                        sk::Rect::from_xywh(0.0, 0.0, image.width() as f32, image.height() as f32)
                    else {
                        continue;
                    };
                    let Some(path) = sk::PathBuilder::from_rect(rect).transform(transform) else {
                        continue;
                    };
                    let alpha = self.state.alpha as f32
                        * if tint {
                            1.0
                        } else {
                            (foreground.0 & 255) as f32 / 255.0
                        };
                    let source = Source::Image(image, transform, alpha);
                    self.emit(path.bounds(), &|pixmap, to, mask, blend| {
                        fill(
                            pixmap,
                            &path,
                            sk::FillRule::Winding,
                            &source,
                            to,
                            blend,
                            mask,
                        )
                    });
                }
            }
        }
    }

    // --- Images ---

    fn image(&mut self, uri: &str, source: Option<Bounds<ScaledPixels>>, dest: &[Num; 4]) {
        if self.state.singular {
            return;
        }
        let Some(image) = self.images.get(uri).cloned() else {
            return;
        };
        let (iw, ih) = (image.width() as f64, image.height() as f64);
        let [mut sx, mut sy, mut sw, mut sh] = source.map_or([0.0, 0.0, iw, ih], |s| {
            [s.origin.x, s.origin.y, s.size.width, s.size.height].map(|value| f64::from(value.0))
        });
        if sw < 0.0 {
            sx += sw;
            sw = -sw;
        }
        if sh < 0.0 {
            sy += sh;
            sh = -sh;
        }
        if sw == 0.0 || sh == 0.0 {
            return;
        }
        let [mut dx, mut dy, dw, dh] = dest.map(|v| self.n(v));
        // Missing sizes are the source's.
        let mut dw = if dw.is_nan() { sw } else { dw };
        let mut dh = if dh.is_nan() { sh } else { dh };
        // Negative extents grow toward the origin without mirroring pixels.
        if dw < 0.0 {
            dx += dw;
            dw = -dw;
        }
        if dh < 0.0 {
            dy += dh;
            dh = -dh;
        }
        if dw == 0.0 || dh == 0.0 {
            return;
        }
        if ![sx, sy, sw, sh, dx, dy, dw, dh]
            .iter()
            .all(|n| n.is_finite())
        {
            return;
        }
        // Source pixels outside the image trim the corresponding destination
        // rather than extending its nearest border pixel.
        let (left, top, right, bottom) = (
            sx.max(0.0),
            sy.max(0.0),
            (sx + sw).min(iw),
            (sy + sh).min(ih),
        );
        if right <= left || bottom <= top {
            return;
        }
        dx += (left - sx) * dw / sw;
        dy += (top - sy) * dh / sh;
        dw *= (right - left) / sw;
        dh *= (bottom - top) / sh;
        (sx, sy, sw, sh) = (left, top, right - left, bottom - top);
        // A cropped sprite gets its own sampler boundary, preventing colors
        // from neighboring sprites bleeding into the scaled edge.
        let crop = sk::IntRect::from_ltrb(
            left.floor() as i32,
            top.floor() as i32,
            right.ceil() as i32,
            bottom.ceil() as i32,
        );
        let image = if let Some(crop) = crop
            && (crop.width() != image.width() || crop.height() != image.height())
        {
            let Some(cropped) = image.clone_rect(crop) else {
                return;
            };
            sx -= crop.x() as f64;
            sy -= crop.y() as f64;
            Arc::new(cropped)
        } else {
            image
        };
        let Some(path) = self.rect_path([dx, dy, dw, dh]) else {
            return;
        };
        // The image's pixels to user space, then to the pixmap.
        let to_dest = Affine::translate(dx, dy)
            .then(&Affine::scale(dw / sw, dh / sh))
            .then(&Affine::translate(-sx, -sy));
        let transform = self.state.transform.then(&to_dest).skia();
        let source = Source::Image(image, transform, self.state.alpha as f32);
        self.emit(path.bounds(), &|pixmap, to, mask, blend| {
            fill(
                pixmap,
                &path,
                sk::FillRule::Winding,
                &source,
                to,
                blend,
                mask,
            );
        });
    }
}

/// What paints a shape: on a pixmap, through a transform from pixmap
/// coordinates, inside a mask, with a blend mode.
type Draw<'a> = &'a dyn Fn(&mut sk::Pixmap, sk::Transform, Option<&sk::Mask>, sk::BlendMode);

fn fill(
    pixmap: &mut sk::Pixmap,
    path: &sk::Path,
    rule: sk::FillRule,
    source: &Source,
    to: sk::Transform,
    blend_mode: sk::BlendMode,
    mask: Option<&sk::Mask>,
) {
    let mut paint = sk::Paint {
        blend_mode,
        ..sk::Paint::default()
    };
    match source {
        Source::Solid(c) => paint.set_color(*c),
        Source::Pixels(pixels, x, y) => {
            paint.shader = sk::Pattern::new(
                pixels.as_ref(),
                sk::SpreadMode::Pad,
                sk::FilterQuality::Nearest,
                1.0,
                sk::Transform::from_translate(*x as f32, *y as f32),
            );
        }
        Source::Image(image, transform, alpha) => {
            // Drawn smaller than its pixels, bicubic keeps it from aliasing.
            let (sx, sy) = transform.get_scale();
            let quality = if sx.min(sy) < 0.75 {
                sk::FilterQuality::Bicubic
            } else {
                sk::FilterQuality::Bilinear
            };
            paint.shader = sk::Pattern::new(
                image.as_ref().as_ref(),
                sk::SpreadMode::Pad,
                quality,
                *alpha,
                *transform,
            );
        }
    }
    // The transform moves the shader along with the path.
    pixmap.fill_path(path, &paint, rule, to, mask);
}

fn fill_rule(rule: FillRule) -> sk::FillRule {
    match rule {
        FillRule::NonZero => sk::FillRule::Winding,
        FillRule::EvenOdd => sk::FillRule::EvenOdd,
    }
}

/// The pixels of a `size` pixmap that the rectangle `[x0, y0, x1, y1]`
/// (which may be far larger) touches.
fn pixels_within(r: [f32; 4], size: (u32, u32)) -> Option<IRect> {
    let (w, h) = (size.0 as f32, size.1 as f32);
    let c = |v: f32, max: f32| if v.is_nan() { 0.0 } else { v.clamp(0.0, max) };
    IRect::round_out(c(r[0], w), c(r[1], h), c(r[2], w), c(r[3], h))
}

fn intersect(a: &[f32; 4], b: &[f32; 4]) -> [f32; 4] {
    [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ]
}

/// Canvas dashes in tiny-skia's form (an even count: odd lists repeat);
/// `None` for a solid line (no dashes, or all zero: canvas draws solid).
fn dash_pattern(dash: &[f32], offset: f64) -> Option<(Vec<f32>, f32)> {
    if !dash.iter().any(|d| *d > 0.0) || dash.iter().any(|d| !d.is_finite() || *d < 0.0) {
        return None;
    }
    let mut d = dash.to_vec();
    if d.len() % 2 == 1 {
        d.extend_from_slice(dash);
    }
    Some((d, offset as f32))
}

/// Operations that change the destination where the source is
/// transparent (bounded by the clip).
fn destructive(c: Composite) -> bool {
    matches!(
        c,
        Composite::SourceIn
            | Composite::SourceOut
            | Composite::DestinationIn
            | Composite::DestinationAtop
            | Composite::Copy
    )
}

/// Canvas' composite operations as tiny-skia's blend modes.
fn blend_mode(c: Composite) -> sk::BlendMode {
    use Composite::*;
    match c {
        SourceOver => sk::BlendMode::SourceOver,
        SourceIn => sk::BlendMode::SourceIn,
        SourceOut => sk::BlendMode::SourceOut,
        SourceAtop => sk::BlendMode::SourceAtop,
        DestinationOver => sk::BlendMode::DestinationOver,
        DestinationIn => sk::BlendMode::DestinationIn,
        DestinationOut => sk::BlendMode::DestinationOut,
        DestinationAtop => sk::BlendMode::DestinationAtop,
        Lighter => sk::BlendMode::Plus,
        Copy => sk::BlendMode::Source,
        Xor => sk::BlendMode::Xor,
        Multiply => sk::BlendMode::Multiply,
        Screen => sk::BlendMode::Screen,
        Overlay => sk::BlendMode::Overlay,
        Darken => sk::BlendMode::Darken,
        Lighten => sk::BlendMode::Lighten,
        ColorDodge => sk::BlendMode::ColorDodge,
        ColorBurn => sk::BlendMode::ColorBurn,
        HardLight => sk::BlendMode::HardLight,
        SoftLight => sk::BlendMode::SoftLight,
        Difference => sk::BlendMode::Difference,
        Exclusion => sk::BlendMode::Exclusion,
        Hue => sk::BlendMode::Hue,
        Saturation => sk::BlendMode::Saturation,
        Color => sk::BlendMode::Color,
        Luminosity => sk::BlendMode::Luminosity,
    }
}

// --- Shadows -----------------------------------------------------------------

/// The pixels whose shadow (blurred with standard deviation `sigma`) the
/// layer `layer` (pixmap pixels) needs, of a shape within `bounds`, in the
/// layer's coordinates: the shape grown by the blur's spread, inside the
/// layer grown likewise (the shape is drawn only in the layer); `None`
/// when that is nothing or too much to blur.
fn shadow_area(bounds: sk::Rect, layer: IRect, sigma: f64) -> Option<IRect> {
    let grow = (3.0 * sigma).ceil() as i32;
    let around = IRect {
        x0: -grow,
        y0: -grow,
        x1: layer.width() + grow,
        y1: layer.height() + grow,
    };
    let (x, y) = (layer.x0 as f32, layer.y0 as f32);
    let shape = [
        (bounds.left() - x).max(around.x0 as f32) - grow as f32,
        (bounds.top() - y).max(around.y0 as f32) - grow as f32,
        (bounds.right() - x).min(around.x1 as f32) + grow as f32,
        (bounds.bottom() - y).min(around.y1 as f32) + grow as f32,
    ];
    let area = IRect::round_out(shape[0], shape[1], shape[2], shape[3])?.intersect(&around)?;
    if area.width() as i64 * area.height() as i64 > 2 * MAX_PIXELS as i64 {
        log::debug!("canvas: a shadow too large to blur is left out");
        return None;
    }
    Some(area)
}

/// The shadow of `shape` within `area`: its alpha blurred with standard
/// deviation `sigma` and tinted `color`, as a pixmap to draw at `(x, y)`.
fn shadow_pixels(
    shape: &sk::Pixmap,
    area: IRect,
    sigma: f64,
    color: Color,
) -> Option<(sk::Pixmap, i32, i32)> {
    let (w, h) = (area.width() as usize, area.height() as usize);
    let mut alpha = vec![0u8; w * h];
    let (sw, sh) = (shape.width() as i32, shape.height() as i32);
    let pixels = shape.pixels();
    for y in area.y0.max(0)..area.y1.min(sh) {
        for x in area.x0.max(0)..area.x1.min(sw) {
            let p = pixels[(y * sw + x) as usize];
            alpha[(y - area.y0) as usize * w + (x - area.x0) as usize] = p.alpha();
        }
    }
    if sigma > 0.0 {
        blur(&mut alpha, w, h, sigma);
    }
    let (r, g, b, a) = color.to_f64();
    let mut out = sk::Pixmap::new(w as u32, h as u32)?;
    for (p, &m) in out.pixels_mut().iter_mut().zip(&alpha) {
        let a = a * m as f64 / 255.0;
        if a > 0.0 {
            let c = |v: f64| (v * a * 255.0).round() as u8;
            let alpha = (a * 255.0).round() as u8;
            *p = sk::PremultipliedColorU8::from_rgba(c(r), c(g), c(b), alpha)
                .unwrap_or(sk::PremultipliedColorU8::TRANSPARENT);
        }
    }
    Some((out, area.x0, area.y0))
}

/// The sizes of three box blurs that together come close to a Gaussian of
/// standard deviation `sigma`.
fn box_sizes(sigma: f64) -> [usize; 3] {
    let n = 3.0;
    let ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut lower = ideal.floor() as usize;
    if lower.is_multiple_of(2) {
        lower = lower.saturating_sub(1);
    }
    let lower = lower.max(1);
    let l = lower as f64;
    let m = ((12.0 * sigma * sigma - n * l * l - 4.0 * n * l - 3.0 * n) / (-4.0 * l - 4.0))
        .round()
        .clamp(0.0, 3.0) as usize;
    [0, 1, 2].map(|i| if i < m { lower } else { lower + 2 })
}

/// Blurs `alpha` (`w` × `h`) with three box blurs each way.
pub(crate) fn blur(alpha: &mut [u8], w: usize, h: usize, sigma: f64) {
    let mut tmp = vec![0u8; alpha.len()];
    for size in box_sizes(sigma) {
        let r = size / 2;
        if r == 0 {
            continue;
        }
        box_pass(alpha, &mut tmp, w, h, r, true);
        box_pass(&tmp, alpha, w, h, r, false);
    }
}

/// One box blur of radius `r` along rows (`horizontal`) or columns, from
/// `src` into `dst`; outside is transparent.
fn box_pass(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize, horizontal: bool) {
    let (lines, len, step, stride) = if horizontal {
        (h, w, 1, w)
    } else {
        (w, h, w, 1)
    };
    if len == 0 {
        return;
    }
    let width = (2 * r + 1) as u32;
    for line in 0..lines {
        let start = line * stride;
        let at = |i: usize| src[start + i * step] as u32;
        let mut sum: u32 = (0..=r.min(len - 1)).map(at).sum();
        for i in 0..len {
            dst[start + i * step] = ((sum + width / 2) / width) as u8;
            if i + r + 1 < len {
                sum += at(i + r + 1);
            }
            if i >= r {
                sum -= at(i - r);
            }
        }
    }
}

// --- Gradients ---------------------------------------------------------------

/// A gradient's colors at `GRADIENT_STEPS` offsets, premultiplied and
/// multiplied by `alpha`.
fn gradient_table(g: &CanvasGradient, alpha: f32) -> Vec<[f32; 4]> {
    let premultiplied = |c: Color| {
        let (r, g, b, a) = c.to_f64();
        let a = a as f32 * alpha;
        [r as f32 * a, g as f32 * a, b as f32 * a, a]
    };
    let stops = &g.stops;
    let (first, last) = (stops[0], stops[stops.len() - 1]);
    (0..GRADIENT_STEPS)
        .map(|i| {
            let t = i as f32 / (GRADIENT_STEPS - 1) as f32;
            if t <= first.0.0 {
                return premultiplied(first.1);
            }
            for w in stops.windows(2) {
                let ((o0, c0), (o1, c1)) = (w[0], w[1]);
                let (o0, o1) = (o0.0, o1.0);
                if t < o1 {
                    let f = if o1 > o0 { (t - o0) / (o1 - o0) } else { 1.0 };
                    let (a, b) = (premultiplied(c0), premultiplied(c1));
                    return [0, 1, 2, 3].map(|k| a[k] + (b[k] - a[k]) * f);
                }
            }
            premultiplied(last.1)
        })
        .collect()
}

/// Where a gradient is at a point of user space: 0..1 (padded past), or
/// `None` where it paints nothing (outside a radial gradient's cone).
fn gradient_offset(shape: &GradientShape, (x, y): Pt) -> Option<f64> {
    match *shape {
        GradientShape::Linear { start, end } => {
            let [x0, y0, x1, y1] = [start.x, start.y, end.x, end.y].map(|v| v.0);
            let (dx, dy) = ((x1 - x0) as f64, (y1 - y0) as f64);
            let len = dx * dx + dy * dy;
            Some(((x - x0 as f64) * dx + (y - y0 as f64) * dy) / len)
        }
        GradientShape::Radial {
            start,
            start_radius,
            end,
            end_radius,
        } => {
            let circles = [start.x, start.y, start_radius, end.x, end.y, end_radius];
            radial_offset(circles.map(|v| f64::from(v.0)), (x, y))
        }
        GradientShape::Conic {
            start_angle,
            center,
        } => {
            let (angle, cx, cy) = (start_angle.0, center.x.0, center.y.0);
            let a = (y - cy as f64).atan2(x - cx as f64) - angle as f64;
            Some((a / TAU).rem_euclid(1.0))
        }
    }
}

/// Canvas' two-circle radial gradient at `p`: the largest `t` whose circle
/// (centers and radii interpolated) passes through `p` with a radius of 0
/// or more.
fn radial_offset([x0, y0, r0, x1, y1, r1]: [f64; 6], (px, py): Pt) -> Option<f64> {
    let (cdx, cdy, dr) = (x1 - x0, y1 - y0, r1 - r0);
    let (pdx, pdy) = (px - x0, py - y0);
    let a = cdx * cdx + cdy * cdy - dr * dr;
    let b = pdx * cdx + pdy * cdy + r0 * dr;
    let c = pdx * pdx + pdy * pdy - r0 * r0;
    let ok = |t: f64| r0 + t * dr >= 0.0;
    if a.abs() < 1e-9 {
        if b.abs() < 1e-12 {
            return None;
        }
        let t = c / (2.0 * b);
        return ok(t).then_some(t);
    }
    let disc = b * b - a * c;
    if disc < 0.0 {
        return None;
    }
    let s = disc.sqrt();
    let (t1, t2) = ((b + s) / a, (b - s) / a);
    let (hi, lo) = if t1 > t2 { (t1, t2) } else { (t2, t1) };
    if ok(hi) {
        Some(hi)
    } else if ok(lo) {
        Some(lo)
    } else {
        None
    }
}

/// The gradient's pixels over `area` of the pixmap (`to_user` maps the
/// pixmap to user space) where `covered` (the size of `area`) is not
/// empty, at `alpha`; `None` if it paints nothing.
fn gradient_pixels(
    g: &CanvasGradient,
    to_user: &Affine,
    area: IRect,
    covered: &sk::Mask,
    alpha: f32,
) -> Option<sk::Pixmap> {
    if g.stops.is_empty() {
        return None;
    }
    match g.shape {
        // Canvas paints nothing along a gradient of no length...
        GradientShape::Linear { start, end } if start == end => return None,
        // ... or between two equal circles.
        GradientShape::Radial {
            start,
            start_radius,
            end,
            end_radius,
        } if start == end && start_radius == end_radius => {
            return None;
        }
        _ => {}
    }
    let table = gradient_table(g, alpha);
    let last = (GRADIENT_STEPS - 1) as f64;
    let mut pixmap = sk::Pixmap::new(area.width() as u32, area.height() as u32)?;
    let w = area.width() as usize;
    let byte = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    let pixels = pixmap.pixels_mut();
    let covered = covered.data();
    for y in 0..area.height() {
        let py = (area.y0 + y) as f64 + 0.5;
        for x in 0..area.width() {
            if covered[y as usize * w + x as usize] == 0 {
                continue;
            }
            let p = to_user.map(((area.x0 + x) as f64 + 0.5, py));
            let Some(t) = gradient_offset(&g.shape, p) else {
                continue;
            };
            let [r, g, b, a] = table[(t.clamp(0.0, 1.0) * last).round() as usize].map(byte);
            pixels[y as usize * w + x as usize] =
                sk::PremultipliedColorU8::from_rgba(r.min(a), g.min(a), b.min(a), a)
                    .unwrap_or(sk::PremultipliedColorU8::TRANSPARENT);
        }
    }
    Some(pixmap)
}

// --- Geometry ----------------------------------------------------------------

/// A point.
type Pt = (f64, f64);

/// A 2D affine transform, canvas' matrix [a c e; b d f].
#[derive(Debug, Clone, Copy, PartialEq)]
struct Affine {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Affine {
    fn translate(x: f64, y: f64) -> Affine {
        Affine {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: x,
            f: y,
        }
    }

    fn scale(x: f64, y: f64) -> Affine {
        Affine {
            a: x,
            b: 0.0,
            c: 0.0,
            d: y,
            e: 0.0,
            f: 0.0,
        }
    }

    /// Clockwise in canvas' y-down coordinates.
    fn rotate(angle: f64) -> Affine {
        let (s, c) = angle.sin_cos();
        Affine {
            a: c,
            b: s,
            c: -s,
            d: c,
            e: 0.0,
            f: 0.0,
        }
    }

    /// `m` applied first, then `self` (canvas `transform(m)`).
    fn then(&self, m: &Affine) -> Affine {
        Affine {
            a: self.a * m.a + self.c * m.b,
            b: self.b * m.a + self.d * m.b,
            c: self.a * m.c + self.c * m.d,
            d: self.b * m.c + self.d * m.d,
            e: self.a * m.e + self.c * m.f + self.e,
            f: self.b * m.e + self.d * m.f + self.f,
        }
    }

    fn map(&self, (x, y): Pt) -> Pt {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    fn det(&self) -> f64 {
        self.a * self.d - self.b * self.c
    }

    /// The larger of the scales of its axes.
    fn scale_factor(&self) -> f64 {
        self.a.hypot(self.b).max(self.c.hypot(self.d))
    }

    /// The inverse; `None` if it is singular or not finite.
    fn invert(&self) -> Option<Affine> {
        let det = self.det();
        if !det.is_finite() || det.abs() < 1e-12 || !self.e.is_finite() || !self.f.is_finite() {
            return None;
        }
        Some(Affine {
            a: self.d / det,
            b: -self.b / det,
            c: -self.c / det,
            d: self.a / det,
            e: (self.c * self.f - self.d * self.e) / det,
            f: (self.b * self.e - self.a * self.f) / det,
        })
    }

    fn skia(&self) -> sk::Transform {
        sk::Transform::from_row(
            self.a as f32,
            self.b as f32,
            self.c as f32,
            self.d as f32,
            self.e as f32,
            self.f as f32,
        )
    }
}

/// A path segment, in pixmap coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Seg {
    Move(Pt),
    Line(Pt),
    Cubic(Pt, Pt, Pt),
    Close,
}

/// Canvas' current path: subpaths kept as they were added.
#[derive(Default)]
struct PathBuf {
    segments: Vec<Seg>,
    /// The current point (`None`: no subpath).
    current: Option<Pt>,
    /// Where the current subpath started.
    start: Pt,
}

impl PathBuf {
    fn clear(&mut self) {
        self.segments.clear();
        self.current = None;
    }

    fn move_to(&mut self, p: Pt) {
        self.segments.push(Seg::Move(p));
        self.current = Some(p);
        self.start = p;
    }

    /// Starts a subpath at `p` when there is none (curves start at their
    /// first control point, as canvas specifies).
    fn ensure(&mut self, p: Pt) {
        if self.current.is_none() {
            self.move_to(p);
        }
    }

    fn line_to(&mut self, p: Pt) {
        if self.current.is_none() {
            return self.move_to(p);
        }
        self.segments.push(Seg::Line(p));
        self.current = Some(p);
    }

    fn cubic_to(&mut self, c1: Pt, c2: Pt, p: Pt) {
        self.ensure(c1);
        self.segments.push(Seg::Cubic(c1, c2, p));
        self.current = Some(p);
    }

    /// Closes the subpath; the next one starts where it started.
    fn close(&mut self) {
        if self.current.is_some() {
            self.segments.push(Seg::Close);
            self.current = Some(self.start);
        }
    }

    /// An arc of the unit circle that `unit` maps into the pixmap, of
    /// `sweep` radians (negative: counterclockwise) from `start`, joined to
    /// the subpath by a line.
    fn arc(&mut self, unit: &Affine, start: f64, sweep: f64) {
        let p0 = unit.map((start.cos(), start.sin()));
        if self.current.is_some() {
            self.line_to(p0);
        } else {
            self.move_to(p0);
        }
        arc_cubics(start, sweep, |c1, c2, p| {
            self.cubic_to(unit.map(c1), unit.map(c2), unit.map(p));
        });
    }

    /// The path as tiny-skia's; `None` when it has nothing to paint.
    fn to_skia(&self) -> Option<sk::Path> {
        let n = self.segments.len();
        let mut b = sk::PathBuilder::with_capacity(n, n * 3);
        let f = |p: Pt| (p.0 as f32, p.1 as f32);
        // A segment after a close starts a subpath where the closed one
        // started.
        let mut start = (0.0, 0.0);
        let mut open = false;
        for seg in &self.segments {
            match *seg {
                Seg::Move(p) => {
                    start = f(p);
                    b.move_to(start.0, start.1);
                    open = true;
                }
                Seg::Line(p) => {
                    if !open {
                        b.move_to(start.0, start.1);
                        open = true;
                    }
                    let (x, y) = f(p);
                    b.line_to(x, y);
                }
                Seg::Cubic(c1, c2, p) => {
                    if !open {
                        b.move_to(start.0, start.1);
                        open = true;
                    }
                    let ((ax, ay), (bx, by), (x, y)) = (f(c1), f(c2), f(p));
                    b.cubic_to(ax, ay, bx, by, x, y);
                }
                Seg::Close => {
                    b.close();
                    open = false;
                }
            }
        }
        b.finish()
    }
}

/// The cubics of an arc of the unit circle of `sweep` radians from
/// `start`, at most a quarter turn each: `f(c1, c2, end)`.
fn arc_cubics(start: f64, sweep: f64, mut f: impl FnMut(Pt, Pt, Pt)) {
    if sweep == 0.0 || !sweep.is_finite() {
        return;
    }
    let n = (sweep.abs() / std::f64::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let step = sweep / n as f64;
    let k = 4.0 / 3.0 * (step / 4.0).tan();
    let mut a0 = start;
    for _ in 0..n {
        let a1 = a0 + step;
        let (s0, c0) = a0.sin_cos();
        let (s1, c1) = a1.sin_cos();
        f(
            (c0 - k * s0, s0 + k * c0),
            (c1 + k * s1, s1 - k * c1),
            (c1, s1),
        );
        a0 = a1;
    }
}

/// The inner control points of the cubic equal to a quadratic curve.
fn quad_to_cubic(p0: Pt, c: Pt, p: Pt) -> [f64; 4] {
    [
        p0.0 + 2.0 / 3.0 * (c.0 - p0.0),
        p0.1 + 2.0 / 3.0 * (c.1 - p0.1),
        p.0 + 2.0 / 3.0 * (c.0 - p.0),
        p.1 + 2.0 / 3.0 * (c.1 - p.1),
    ]
}

/// The arc of `arcTo`.
#[derive(Debug, PartialEq)]
struct ArcTo {
    center: Pt,
    start: f64,
    sweep: f64,
}

/// The arc tangent to the lines `p0`–`p1` and `p1`–`p2` (canvas `arcTo`);
/// `None` when it degenerates to a line to `p1`.
fn arc_to_geometry(p0: Pt, p1: Pt, p2: Pt, r: f64) -> Option<ArcTo> {
    let (ux, uy) = (p0.0 - p1.0, p0.1 - p1.1);
    let (vx, vy) = (p2.0 - p1.0, p2.1 - p1.1);
    let (lu, lv) = (ux.hypot(uy), vx.hypot(vy));
    if r <= 0.0 || !r.is_finite() || lu < 1e-9 || lv < 1e-9 {
        return None;
    }
    let (ux, uy, vx, vy) = (ux / lu, uy / lu, vx / lv, vy / lv);
    if (ux * vy - uy * vx).abs() < 1e-9 {
        // Collinear.
        return None;
    }
    let half = (ux * vx + uy * vy).clamp(-1.0, 1.0).acos() / 2.0;
    // From p1 to the tangent points, and to the center along the bisector.
    let tangent = r / half.tan();
    let (bx, by) = (ux + vx, uy + vy);
    let lb = bx.hypot(by);
    let to_center = r / half.sin();
    let center = (p1.0 + bx / lb * to_center, p1.1 + by / lb * to_center);
    let t1 = (p1.0 + ux * tangent, p1.1 + uy * tangent);
    let t2 = (p1.0 + vx * tangent, p1.1 + vy * tangent);
    let start = (t1.1 - center.1).atan2(t1.0 - center.0);
    let end = (t2.1 - center.1).atan2(t2.0 - center.0);
    // The short way round: the arc between the tangent points is under a
    // half turn.
    let mut sweep = end - start;
    if sweep > PI {
        sweep -= TAU;
    } else if sweep < -PI {
        sweep += TAU;
    }
    Some(ArcTo {
        center,
        start,
        sweep,
    })
}

#[cfg(test)]
mod tests {
    use nsh_core::geometry::{point, px};

    use super::*;
    fn close(a: Pt, b: Pt) -> bool {
        (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6
    }
    const IDENTITY: Affine = Affine {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };
    const RED: Color = Color(0xff0000ff);
    const BLUE: Color = Color(0x0000ffff);
    fn c(v: f32) -> Num {
        Num::Const(v)
    }
    fn rect(x: f32, y: f32, w: f32, h: f32) -> [Num; 4] {
        [x, y, w, h].map(c)
    }
    fn raster(ops: Vec<CanvasOp>) -> Raster {
        rasterize(
            &Drawing {
                ops,
                ..Drawing::default()
            },
            &[],
            px(32.0),
            px(32.0),
            &HashMap::new(),
            &FontCatalog::default(),
        )
        .unwrap()
    }
    fn at(r: &Raster, x: usize, y: usize) -> [u8; 4] {
        r.rgba[(y * r.width as usize + x) * 4..][..4]
            .try_into()
            .unwrap()
    }

    #[test]
    fn path_coordinates_survive_later_transforms_and_saved_clips_restore() {
        let image = raster(vec![
            CanvasOp::FillStyle(Paint::Color(BLUE)),
            CanvasOp::FillRect(rect(0.0, 0.0, 32.0, 32.0)),
            CanvasOp::BeginPath,
            CanvasOp::Path(PathOp::Rect(rect(0.0, 0.0, 16.0, 32.0))),
            CanvasOp::Save,
            CanvasOp::Clip(None, FillRule::NonZero),
            CanvasOp::Translate(c(16.0), c(0.0)),
            CanvasOp::FillStyle(Paint::Color(RED)),
            // The path was built before translate and stays on the left.
            CanvasOp::Fill(None, FillRule::NonZero),
            CanvasOp::Restore,
            CanvasOp::ClearRect(rect(24.0, 0.0, 8.0, 32.0)),
        ]);
        assert_eq!(at(&image, 8, 16), [255, 0, 0, 255]);
        assert_eq!(at(&image, 20, 16), [0, 0, 255, 255]);
        assert_eq!(at(&image, 28, 16), [0, 0, 0, 0]);
    }

    #[test]
    fn animated_geometry_and_color_slots_are_replayed_natively() {
        let drawing = Drawing {
            ops: vec![
                CanvasOp::FillStyle(Paint::Slot(2)),
                CanvasOp::FillRect([Num::Slot(0), c(0.0), c(8.0), c(8.0)]),
            ],
            ..Drawing::default()
        };
        let image = rasterize(
            &drawing,
            &[
                (0, AnimationValue::Number(16.0)),
                (2, AnimationValue::Color(RED.0)),
            ],
            px(32.0),
            px(32.0),
            &HashMap::new(),
            &FontCatalog::default(),
        )
        .unwrap();
        assert_eq!(at(&image, 1, 1), [0, 0, 0, 0]);
        assert_eq!(at(&image, 18, 2), [255, 0, 0, 255]);
    }

    #[test]
    fn transparent_copy_clears_the_destination_inside_the_clip() {
        let image = raster(vec![
            CanvasOp::FillStyle(Paint::Color(BLUE)),
            CanvasOp::FillRect(rect(0.0, 0.0, 32.0, 32.0)),
            CanvasOp::BeginPath,
            CanvasOp::Path(PathOp::Rect(rect(0.0, 0.0, 16.0, 32.0))),
            CanvasOp::Clip(None, FillRule::NonZero),
            CanvasOp::Composite(Composite::Copy),
            CanvasOp::FillStyle(Paint::Color(Color::TRANSPARENT)),
            CanvasOp::FillRect(rect(0.0, 0.0, 4.0, 4.0)),
        ]);
        assert_eq!(at(&image, 8, 16), [0, 0, 0, 0]);
        assert_eq!(at(&image, 20, 16), [0, 0, 255, 255]);
    }

    #[test]
    fn linear_radial_and_conic_gradients_keep_their_geometry() {
        let gradient = |shape| {
            Arc::new(CanvasGradient {
                shape,
                stops: vec![
                    (nsh_core::geometry::percentage(0.0), RED),
                    (nsh_core::geometry::percentage(1.0), BLUE),
                ],
            })
        };
        let linear = raster(vec![
            CanvasOp::FillStyle(Paint::Gradient(gradient(GradientShape::Linear {
                start: point(px(0.0), px(0.0)),
                end: point(px(32.0), px(0.0)),
            }))),
            CanvasOp::FillRect(rect(0.0, 0.0, 32.0, 32.0)),
        ]);
        assert!(at(&linear, 1, 16)[0] > at(&linear, 30, 16)[0]);
        let radial = raster(vec![
            CanvasOp::FillStyle(Paint::Gradient(gradient(GradientShape::Radial {
                start: point(px(16.0), px(16.0)),
                start_radius: px(0.0),
                end: point(px(16.0), px(16.0)),
                end_radius: px(16.0),
            }))),
            CanvasOp::FillRect(rect(0.0, 0.0, 32.0, 32.0)),
        ]);
        assert!(at(&radial, 16, 16)[0] > at(&radial, 30, 16)[0]);
        let conic = raster(vec![
            CanvasOp::FillStyle(Paint::Gradient(gradient(GradientShape::Conic {
                start_angle: Radians(0.0),
                center: point(px(16.0), px(16.0)),
            }))),
            CanvasOp::FillRect(rect(0.0, 0.0, 32.0, 32.0)),
        ]);
        assert!(at(&conic, 30, 18)[0] > at(&conic, 30, 14)[0]);
    }

    #[test]
    fn stroke_dash_and_shadow_render_the_recorded_shape() {
        let image = raster(vec![
            CanvasOp::StrokeStyle(Paint::Color(RED)),
            CanvasOp::LineWidth(c(4.0)),
            CanvasOp::LineDash(vec![px(4.0), px(4.0)].into()),
            CanvasOp::ShadowColor(BLUE),
            CanvasOp::ShadowBlur(px(2.0)),
            CanvasOp::ShadowOffset(point(px(0.0), px(8.0))),
            CanvasOp::BeginPath,
            CanvasOp::Path(PathOp::MoveTo(c(0.0), c(8.0))),
            CanvasOp::Path(PathOp::LineTo(c(32.0), c(8.0))),
            CanvasOp::Stroke(None),
        ]);
        assert!(at(&image, 2, 8)[0] > 200);
        assert!(at(&image, 6, 8)[3] < 20);
        assert!(at(&image, 2, 16)[2] > 200);
    }

    #[test]
    fn draw_image_obeys_source_crop_and_transform() {
        let uri: Arc<str> = "data:test".into();
        let source = Raster {
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 0, 255, 255],
        };
        let drawing = Drawing {
            ops: vec![
                CanvasOp::Translate(c(8.0), c(8.0)),
                CanvasOp::DrawImage {
                    image: nsh_core::canvas::CanvasImage { uri: uri.clone() },
                    source: Some(Bounds::new(
                        point(ScaledPixels(1.0), ScaledPixels(0.0)),
                        size(ScaledPixels(1.0), ScaledPixels(1.0)),
                    )),
                    dest: rect(0.0, 0.0, 8.0, 8.0),
                },
            ],
            ..Drawing::default()
        };
        let image = rasterize(
            &drawing,
            &[],
            px(32.0),
            px(32.0),
            &HashMap::from([(uri, Arc::new(source))]),
            &FontCatalog::default(),
        )
        .unwrap();
        assert_eq!(at(&image, 1, 1), [0, 0, 0, 0]);
        assert_eq!(at(&image, 8, 12), [0, 0, 255, 255]);
        assert_eq!(at(&image, 14, 12), [0, 0, 255, 255]);
    }

    #[test]
    fn draw_image_clips_outside_source_pixels_and_negative_sizes_do_not_flip() {
        let uri: Arc<str> = "data:test".into();
        let source = Raster {
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 0, 255, 255],
        };
        let drawing = Drawing {
            ops: vec![CanvasOp::DrawImage {
                image: nsh_core::canvas::CanvasImage { uri: uri.clone() },
                source: Some(Bounds::new(
                    point(ScaledPixels(-1.0), ScaledPixels(0.0)),
                    size(ScaledPixels(2.0), ScaledPixels(1.0)),
                )),
                dest: rect(16.0, 0.0, -16.0, 8.0),
            }],
            ..Drawing::default()
        };
        let image = rasterize(
            &drawing,
            &[],
            px(32.0),
            px(32.0),
            &HashMap::from([(uri, Arc::new(source))]),
            &FontCatalog::default(),
        )
        .unwrap();
        assert_eq!(at(&image, 1, 4), [0, 0, 0, 0]);
        assert_eq!(at(&image, 12, 4), [255, 0, 0, 255]);
        assert_eq!(at(&image, 20, 4), [0, 0, 0, 0]);
    }

    #[test]
    fn excessive_or_nonfinite_canvases_return_an_error_before_allocating() {
        for (w, h) in [
            (f32::NAN, 10.0),
            (10.0, f32::INFINITY),
            (-1.0, 10.0),
            (8193.0, 10.0),
            (4000.0, 4000.0),
        ] {
            assert!(
                rasterize(
                    &Drawing::default(),
                    &[],
                    px(w),
                    px(h),
                    &HashMap::new(),
                    &FontCatalog::default()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn registered_canvas_measurement_matches_alignment_of_drawn_glyphs() {
        let fonts = FontCatalog::default();
        fonts
            .register(include_bytes!(
                "../../../tests/compat/cases/layout/fonts/Pacifico-Regular.ttf"
            ))
            .unwrap();
        let engine =
            crate::TerminalTextEngine::with_fonts(crate::CellMetrics::default(), fonts.clone());
        let font = CanvasFont::parse("24px Pacifico").unwrap();
        let text: Arc<str> = "AV office".into();
        let advance = nsh_core::canvas::measure_line(&engine, &font, &text);
        let draw = |alignment, x| {
            rasterize(
                &Drawing {
                    ops: vec![
                        CanvasOp::Font(font.clone()),
                        CanvasOp::TextAlign(alignment),
                        CanvasOp::FillText {
                            text: text.clone(),
                            x: c(x),
                            y: c(48.0),
                            max_width: None,
                        },
                    ],
                    ..Drawing::default()
                },
                &[],
                px(192.0),
                px(64.0),
                &HashMap::new(),
                &fonts,
            )
            .unwrap()
        };
        let left = draw(CanvasTextAlign::Left, 20.0);
        let right = draw(CanvasTextAlign::Right, 160.0);
        let first_ink = |r: &Raster| {
            (0..r.width)
                .find(|&x| (0..r.height).any(|y| r.rgba[((y * r.width + x) * 4 + 3) as usize] > 0))
                .unwrap() as f32
        };
        assert!(((first_ink(&right) - first_ink(&left)) - (140.0 - advance.0)).abs() <= 1.1);
        assert_eq!(advance.0, fonts.shape(&font, &text).width);
    }
    #[test]
    fn transforms_compose_like_canvas() {
        // translate then scale: points scale first.
        let m = Affine::translate(10.0, 20.0).then(&Affine::scale(2.0, 3.0));
        assert_eq!(m.map((1.0, 1.0)), (12.0, 23.0));
        // rotate a quarter turn clockwise (y down).
        let r = Affine::rotate(std::f64::consts::FRAC_PI_2);
        assert!(close(r.map((1.0, 0.0)), (0.0, 1.0)));
        let inv = m.then(&r).invert().unwrap();
        assert!(close(inv.map(m.then(&r).map((3.0, -4.0))), (3.0, -4.0)));
        assert_eq!(Affine::scale(0.0, 1.0).invert(), None);
        assert_eq!(m.det(), 6.0);
        assert_eq!(m.scale_factor(), 3.0);
        // tiny-skia maps points as the matrix does.
        let mut p = [sk::Point::from_xy(1.0, 1.0)];
        m.skia().map_points(&mut p);
        assert_eq!((p[0].x, p[0].y), (12.0, 23.0));
        assert_eq!(IDENTITY.then(&m), m);
    }

    #[test]
    fn arc_to_rounds_a_right_angle() {
        // Along y = 0 to the corner (10, 0), then down.
        let a = arc_to_geometry((0.0, 0.0), (10.0, 0.0), (10.0, 10.0), 2.0).unwrap();
        assert!(close(a.center, (8.0, 2.0)));
        // From straight above the center, a quarter turn clockwise.
        assert!((a.start + std::f64::consts::FRAC_PI_2).abs() < 1e-9);
        assert!((a.sweep - std::f64::consts::FRAC_PI_2).abs() < 1e-9);
        // Collinear points and zero radii are lines.
        assert_eq!(
            arc_to_geometry((0.0, 0.0), (5.0, 0.0), (10.0, 0.0), 2.0),
            None
        );
        assert_eq!(
            arc_to_geometry((0.0, 0.0), (5.0, 0.0), (5.0, 5.0), 0.0),
            None
        );
    }

    #[test]
    fn quads_become_cubics() {
        let c = quad_to_cubic((0.0, 0.0), (3.0, 3.0), (6.0, 0.0));
        assert_eq!(c, [2.0, 2.0, 4.0, 2.0]);
    }

    #[test]
    fn arcs_are_quarter_turn_cubics_on_the_circle() {
        let mut ends = Vec::new();
        arc_cubics(0.0, PI, |c1, _, p| ends.push((c1, p)));
        assert_eq!(ends.len(), 2);
        assert!(close(ends[0].1, (0.0, 1.0)));
        assert!(close(ends[1].1, (-1.0, 0.0)));
        // The first control point is tangent to the circle at the start.
        assert!(ends[0].0.0 > 0.99 && ends[0].0.1 > 0.5);
        // Counterclockwise sweeps go the other way.
        let mut ends = Vec::new();
        arc_cubics(0.0, -std::f64::consts::FRAC_PI_2, |_, _, p| ends.push(p));
        assert_eq!(ends.len(), 1);
        assert!(close(ends[0], (0.0, -1.0)));
        // The midpoint of a quarter cubic is within 0.03% of the radius.
        let mut mid = None;
        arc_cubics(0.0, std::f64::consts::FRAC_PI_2, |c1, c2, p| {
            let m = (
                0.125 + 0.375 * c1.0 + 0.375 * c2.0 + 0.125 * p.0,
                0.375 * c1.1 + 0.375 * c2.1 + 0.125 * p.1,
            );
            mid = Some(m);
        });
        let m = mid.unwrap();
        assert!((m.0.hypot(m.1) - 1.0).abs() < 3e-4);
        let mut none = 0;
        arc_cubics(1.0, 0.0, |_, _, _| none += 1);
        assert_eq!(none, 0);
    }

    #[test]
    fn paths_follow_canvas_rules() {
        let mut p = PathBuf::default();
        assert!(p.to_skia().is_none());
        // A line without a subpath starts one.
        p.line_to((1.0, 1.0));
        assert_eq!(p.segments, vec![Seg::Move((1.0, 1.0))]);
        p.line_to((2.0, 1.0));
        p.close();
        // After closing, the current point is the subpath's start.
        assert_eq!(p.current, Some((1.0, 1.0)));
        // An arc joins the subpath with a line to its start.
        let unit = Affine::translate(10.0, 0.0).then(&Affine::scale(5.0, 5.0));
        p.arc(&unit, 0.0, std::f64::consts::FRAC_PI_2);
        assert_eq!(p.segments[3], Seg::Line((15.0, 0.0)));
        match p.segments[4] {
            Seg::Cubic(_, _, end) => assert!(close(end, (10.0, 5.0))),
            s => panic!("{s:?}"),
        }
        // As tiny-skia's: the segment after the close starts a subpath at
        // (1, 1).
        let path = p.to_skia().unwrap();
        let points = path.points();
        assert_eq!((points[2].x, points[2].y), (1.0, 1.0));
        assert_eq!((points[3].x, points[3].y), (15.0, 0.0));
        p.clear();
        assert!(p.segments.is_empty() && p.current.is_none());
        // Curves start at their first control point.
        p.cubic_to((3.0, 3.0), (4.0, 4.0), (5.0, 5.0));
        assert_eq!(p.segments[0], Seg::Move((3.0, 3.0)));
    }
}
