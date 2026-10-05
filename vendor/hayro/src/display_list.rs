//! PrintCraft patch: a recorded display list of one page.
//!
//! Interpreting a large engineering drawing (millions of path operators) takes seconds, and a
//! zoomed-in view renders the page as many tiles. Recording the device calls once and replaying
//! them per tile, skipping everything outside the tile, turns each tile into a fraction of the
//! interpretation cost.
//!
//! Recording is conservative: anything whose output depends on the device resolution in a way a
//! replay cannot reproduce (soft masks, stencil masks) makes [`DisplayList::record`] return
//! `None`, and callers fall back to [`crate::render`]. Lists that would exceed the memory
//! budget are given up on the same way.

use crate::renderer::Renderer;
use crate::{RenderCache, RenderSettings};
use hayro_interpret::font::Glyph;
use hayro_interpret::hayro_syntax::page::Page;
use hayro_interpret::util::{RectExt, TransformExt};
use hayro_interpret::{
    BlendMode, ClipPath, Context, Device, FillRule, GlyphDrawMode, Image, ImageData,
    InterpreterSettings, LumaData, Paint, PathDrawMode, SoftMask, StrokeProps, interpret_page,
};
use kurbo::{Affine, BezPath, Join, PathEl, Point, Rect, Shape};
use std::collections::HashMap;
use std::rc::Rc;
use vello_cpu::{Level, Pixmap, RenderMode};

const MOVE: u8 = 0;
const LINE: u8 = 1;
const QUAD: u8 = 2;
const CUBIC: u8 = 3;
const CLOSE: u8 = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Path,
    Rect,
    FillGlyph,
    StrokeGlyph,
    Image,
    PushClip(FillRule),
    PopClip,
    PushGroup,
    PopGroup,
    Blend,
}

/// One recorded device call. Indices point into the side tables of the list.
#[derive(Clone, Copy)]
struct Op {
    kind: Kind,
    /// Drawn while interpreting a Type 3 glyph procedure (the renderer treats those specially).
    type3: bool,
    /// Path span, glyph outline, image, group or blend mode index, depending on `kind`.
    item: u32,
    transform: u32,
    paint: u32,
    mode: u32,
    /// Conservative bounds in recording space (page pixels at scale 1).
    bbox: [f32; 4],
}

/// The recorded drawing of one page; see the module docs.
pub struct DisplayList<'a> {
    ops: Vec<Op>,
    verbs: Vec<u8>,
    points: Vec<[f32; 2]>,
    /// Per path: (first verb, first point).
    spans: Vec<(u32, u32)>,
    transforms: Vec<Affine>,
    paints: Vec<Paint<'a>>,
    modes: Vec<PathDrawMode>,
    outlines: Vec<Rc<BezPath>>,
    images: Vec<(ImageData, Option<LumaData>)>,
    groups: Vec<(f32, BlendMode)>,
    blends: Vec<BlendMode>,
    crop: BezPath,
    width: f32,
    height: f32,
    bytes: usize,
}

impl<'a> DisplayList<'a> {
    /// Record `page`, or `None` when it uses something a replay cannot reproduce or the list
    /// would take more than `budget` bytes.
    pub fn record(
        page: &'a Page<'a>,
        cache: &RenderCache<'a>,
        settings: &InterpreterSettings,
        budget: usize,
    ) -> Option<Self> {
        let (width, height) = page.render_dimensions();
        let initial = page.initial_transform(true).to_kurbo();
        let mut crop = page.intersected_crop_box().to_kurbo().to_path(0.1);
        crop.apply_affine(initial);
        let mut state = Context::new(
            initial,
            Rect::new(0.0, 0.0, width as f64, height as f64),
            &cache.interpreter_cache,
            page.xref(),
            settings.clone(),
        );
        let mut rec = Recorder {
            list: DisplayList {
                ops: Vec::new(),
                verbs: Vec::new(),
                points: Vec::new(),
                spans: Vec::new(),
                transforms: Vec::new(),
                paints: Vec::new(),
                modes: Vec::new(),
                outlines: Vec::new(),
                images: Vec::new(),
                groups: Vec::new(),
                blends: Vec::new(),
                crop,
                width,
                height,
                bytes: 0,
            },
            outline_ids: HashMap::new(),
            outline_cache: cache.outline_cache.clone(),
            type3: 0,
            failed: false,
            budget,
            paint_key: None,
        };
        interpret_page(page, &mut state, &mut rec);
        rec.update_bytes();
        if rec.failed || rec.list.bytes > budget {
            return None;
        }
        Some(rec.list)
    }

    /// Approximate memory held by the list, in bytes.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Number of recorded drawing operations.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Whether nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    fn path(&self, span: u32, out: &mut BezPath) {
        out.truncate(0);
        let i = span as usize;
        let Some(&(v0, p0)) = self.spans.get(i) else { return };
        let (v1, _) = self.spans.get(i + 1).copied().unwrap_or((self.verbs.len() as u32, 0));
        let pt = |k: usize| self.points.get(k).map(|p| Point::new(p[0] as f64, p[1] as f64)).unwrap_or_default();
        let mut k = p0 as usize;
        for v in self.verbs.get(v0 as usize..v1 as usize).unwrap_or(&[]) {
            match *v {
                MOVE => {
                    out.move_to(pt(k));
                    k += 1;
                }
                LINE => {
                    out.line_to(pt(k));
                    k += 1;
                }
                QUAD => {
                    out.quad_to(pt(k), pt(k + 1));
                    k += 2;
                }
                CUBIC => {
                    out.curve_to(pt(k), pt(k + 1), pt(k + 2));
                    k += 3;
                }
                _ => out.close_path(),
            }
        }
    }
}

struct Recorder<'a> {
    list: DisplayList<'a>,
    outline_ids: HashMap<u128, u32>,
    outline_cache: Rc<std::cell::RefCell<HashMap<u128, Rc<BezPath>>>>,
    type3: u32,
    failed: bool,
    budget: usize,
    /// RGBA8 of the last recorded solid paint, to share entries between consecutive ops.
    paint_key: Option<[u8; 4]>,
}

impl<'a> Recorder<'a> {
    fn update_bytes(&mut self) {
        let l = &self.list;
        let images: usize = l
            .images
            .iter()
            .map(|(i, a)| {
                let d = match i {
                    ImageData::Rgb(r) => r.data.len(),
                    ImageData::Luma(g) => g.data.len(),
                };
                d + a.as_ref().map(|a| a.data.len()).unwrap_or(0)
            })
            .sum();
        let outlines: usize = l.outlines.iter().map(|o| o.elements().len() * 56).sum();
        self.list.bytes = l.ops.capacity() * size_of::<Op>()
            + l.verbs.capacity()
            + l.points.capacity() * 8
            + l.spans.capacity() * 8
            + l.transforms.capacity() * 48
            + l.paints.capacity() * size_of::<Paint<'a>>()
            + l.modes.capacity() * size_of::<PathDrawMode>()
            + outlines
            + images;
        if self.list.bytes > self.budget {
            self.failed = true;
        }
    }

    fn push(&mut self, op: Op) {
        if self.failed {
            return;
        }
        self.list.ops.push(op);
        if self.list.ops.len() % 65536 == 0 {
            self.update_bytes();
        }
    }

    fn transform(&mut self, t: Affine) -> u32 {
        if self.list.transforms.last() != Some(&t) {
            self.list.transforms.push(t);
        }
        self.list.transforms.len().saturating_sub(1) as u32
    }

    fn paint(&mut self, p: &Paint<'a>) -> u32 {
        let key = match p {
            Paint::Color(c) => Some(c.to_rgba().to_rgba8()),
            Paint::Pattern(_) => None,
        };
        if key.is_none() || key != self.paint_key || self.list.paints.is_empty() {
            self.list.paints.push(p.clone());
        }
        self.paint_key = key;
        self.list.paints.len().saturating_sub(1) as u32
    }

    fn mode(&mut self, m: &PathDrawMode) -> u32 {
        let same = match (self.list.modes.last(), m) {
            (Some(PathDrawMode::Fill(a)), PathDrawMode::Fill(b)) => a == b,
            (Some(PathDrawMode::Stroke(a)), PathDrawMode::Stroke(b)) => same_stroke(a, b),
            _ => false,
        };
        if !same {
            self.list.modes.push(m.clone());
        }
        self.list.modes.len().saturating_sub(1) as u32
    }

    /// Store a path; returns its span index and its control-point bounds.
    fn store_path(&mut self, path: &BezPath) -> (u32, Rect) {
        let l = &mut self.list;
        l.spans.push((l.verbs.len() as u32, l.points.len() as u32));
        let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        let mut add = |l: &mut DisplayList<'a>, p: Point| {
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x);
            y1 = y1.max(p.y);
            l.points.push([p.x as f32, p.y as f32]);
        };
        for el in path.elements() {
            match *el {
                PathEl::MoveTo(p) => {
                    l.verbs.push(MOVE);
                    add(l, p);
                }
                PathEl::LineTo(p) => {
                    l.verbs.push(LINE);
                    add(l, p);
                }
                PathEl::QuadTo(a, b) => {
                    l.verbs.push(QUAD);
                    add(l, a);
                    add(l, b);
                }
                PathEl::CurveTo(a, b, c) => {
                    l.verbs.push(CUBIC);
                    add(l, a);
                    add(l, b);
                    add(l, c);
                }
                PathEl::ClosePath => l.verbs.push(CLOSE),
            }
        }
        let bounds = if x0 <= x1 && y0 <= y1 { Rect::new(x0, y0, x1, y1) } else { Rect::ZERO };
        (l.spans.len().saturating_sub(1) as u32, bounds)
    }

    fn outline(&mut self, o: &hayro_interpret::font::OutlineGlyph) -> u32 {
        use hayro_interpret::CacheKey;
        let id = o.identifier().cache_key();
        if let Some(&i) = self.outline_ids.get(&id) {
            return i;
        }
        let cached = self.outline_cache.borrow().get(&id).cloned();
        let path = cached.unwrap_or_else(|| {
            let p = Rc::new(o.outline());
            self.outline_cache.borrow_mut().insert(id, p.clone());
            p
        });
        self.list.outlines.push(path);
        let i = self.list.outlines.len().saturating_sub(1) as u32;
        self.outline_ids.insert(id, i);
        i
    }

    fn simple(&mut self, kind: Kind, item: u32) {
        self.push(Op { kind, type3: self.type3 > 0, item, transform: 0, paint: 0, mode: 0, bbox: [0.0; 4] });
    }
}

fn same_stroke(a: &StrokeProps, b: &StrokeProps) -> bool {
    a.line_width == b.line_width
        && a.line_cap == b.line_cap
        && a.line_join == b.line_join
        && a.miter_limit == b.miter_limit
        && a.dash_array == b.dash_array
        && a.dash_offset == b.dash_offset
}

/// How far a stroke can reach beyond its path, in user units.
fn stroke_reach(s: &StrokeProps) -> f64 {
    let half = (s.line_width.max(0.0) as f64) / 2.0;
    let join = if s.line_join == Join::Miter { (s.miter_limit as f64).clamp(1.5, 10.0) } else { 1.5 };
    half * join
}

fn bbox_of(r: Rect) -> [f32; 4] {
    [r.x0 as f32, r.y0 as f32, r.x1 as f32, r.y1 as f32]
}

impl<'a> Device<'a> for Recorder<'a> {
    fn set_soft_mask(&mut self, mask: Option<SoftMask<'a>>) {
        if mask.is_some() {
            self.failed = true;
        }
    }

    fn set_blend_mode(&mut self, blend_mode: BlendMode) {
        self.list.blends.push(blend_mode);
        let i = self.list.blends.len().saturating_sub(1) as u32;
        self.simple(Kind::Blend, i);
    }

    fn draw_path(&mut self, path: &BezPath, transform: Affine, paint: &Paint<'a>, draw_mode: &PathDrawMode) {
        if self.failed {
            return;
        }
        let (span, mut bounds) = self.store_path(path);
        if let PathDrawMode::Stroke(s) = draw_mode {
            bounds = bounds.inflate(stroke_reach(s), stroke_reach(s));
        }
        let op = Op {
            kind: Kind::Path,
            type3: self.type3 > 0,
            item: span,
            transform: self.transform(transform),
            paint: self.paint(paint),
            mode: self.mode(draw_mode),
            bbox: bbox_of(transform.transform_rect_bbox(bounds)),
        };
        self.push(op);
    }

    fn draw_rect(&mut self, rect: &Rect, transform: Affine, paint: &Paint<'a>, draw_mode: &PathDrawMode) {
        if self.failed {
            return;
        }
        let mut path = BezPath::new();
        path.move_to((rect.x0, rect.y0));
        path.line_to((rect.x1, rect.y1));
        let (span, _) = self.store_path(&path);
        let mut bounds = *rect;
        if let PathDrawMode::Stroke(s) = draw_mode {
            bounds = bounds.inflate(stroke_reach(s), stroke_reach(s));
        }
        let op = Op {
            kind: Kind::Rect,
            type3: self.type3 > 0,
            item: span,
            transform: self.transform(transform),
            paint: self.paint(paint),
            mode: self.mode(draw_mode),
            bbox: bbox_of(transform.transform_rect_bbox(bounds)),
        };
        self.push(op);
    }

    fn push_clip_path(&mut self, clip_path: &ClipPath) {
        if self.failed {
            return;
        }
        let (span, bounds) = self.store_path(&clip_path.path);
        self.push(Op {
            kind: Kind::PushClip(clip_path.fill),
            type3: self.type3 > 0,
            item: span,
            transform: 0,
            paint: 0,
            mode: 0,
            bbox: bbox_of(bounds),
        });
    }

    fn push_transparency_group(&mut self, opacity: f32, mask: Option<SoftMask<'a>>, blend_mode: BlendMode) {
        if mask.is_some() {
            self.failed = true;
            return;
        }
        self.list.groups.push((opacity, blend_mode));
        let i = self.list.groups.len().saturating_sub(1) as u32;
        self.simple(Kind::PushGroup, i);
    }

    fn draw_glyph(
        &mut self,
        glyph: &Glyph<'a>,
        transform: Affine,
        glyph_transform: Affine,
        paint: &Paint<'a>,
        draw_mode: &GlyphDrawMode,
    ) {
        if self.failed {
            return;
        }
        match (glyph, draw_mode) {
            (_, GlyphDrawMode::Invisible) => {}
            (Glyph::Outline(o), GlyphDrawMode::Fill) => {
                let item = self.outline(o);
                let t = transform * glyph_transform;
                let bounds = self.list.outlines.get(item as usize).map(|p| p.control_box()).unwrap_or(Rect::ZERO);
                let op = Op {
                    kind: Kind::FillGlyph,
                    type3: self.type3 > 0,
                    item,
                    transform: self.transform(t),
                    paint: self.paint(paint),
                    mode: 0,
                    bbox: bbox_of(t.transform_rect_bbox(bounds)),
                };
                self.push(op);
            }
            (Glyph::Outline(o), GlyphDrawMode::Stroke(s)) => {
                let item = self.outline(o);
                let bounds = self.list.outlines.get(item as usize).map(|p| p.control_box()).unwrap_or(Rect::ZERO);
                let bounds = glyph_transform.transform_rect_bbox(bounds).inflate(stroke_reach(s), stroke_reach(s));
                // Two consecutive entries: the text transform, then the glyph transform.
                self.list.transforms.push(transform);
                self.list.transforms.push(glyph_transform);
                let t = self.list.transforms.len().saturating_sub(2) as u32;
                let op = Op {
                    kind: Kind::StrokeGlyph,
                    type3: self.type3 > 0,
                    item,
                    transform: t,
                    paint: self.paint(paint),
                    mode: self.mode(&PathDrawMode::Stroke(s.clone())),
                    bbox: bbox_of(transform.transform_rect_bbox(bounds)),
                };
                self.push(op);
            }
            (Glyph::Type3(s), GlyphDrawMode::Fill) => {
                self.type3 += 1;
                s.interpret(self, transform, glyph_transform, paint);
                self.type3 -= 1;
            }
            (Glyph::Type3(s), GlyphDrawMode::Stroke(_)) => {
                // The renderer does not flag stroked Type 3 glyphs; mirror that.
                let outer = std::mem::take(&mut self.type3);
                s.interpret(self, transform, glyph_transform, paint);
                self.type3 = outer;
            }
        }
    }

    fn draw_image(&mut self, image: Image<'a, '_>, transform: Affine) {
        if self.failed {
            return;
        }
        let Image::Raster(r) = image else {
            // Stencil masks are decoded at the target resolution with the fill colour baked
            // in; not worth replaying.
            self.failed = true;
            return;
        };
        let mut decoded = None;
        r.with_rgba(|img, alpha| decoded = Some((img, alpha)), None);
        let Some((img, alpha)) = decoded else { return };
        let (w, h) = (img.width() as f64, img.height() as f64);
        let (sx, sy) = img.scale_factors();
        let t = transform * Affine::scale_non_uniform(sx as f64, sy as f64);
        let bbox = bbox_of(t.transform_rect_bbox(Rect::new(0.0, 0.0, w, h)));
        self.list.images.push((img, alpha));
        let item = self.list.images.len().saturating_sub(1) as u32;
        let op = Op { kind: Kind::Image, type3: self.type3 > 0, item, transform: self.transform(t), paint: 0, mode: 0, bbox };
        self.push(op);
        self.update_bytes();
    }

    fn pop_clip_path(&mut self) {
        self.simple(Kind::PopClip, 0);
    }

    fn pop_transparency_group(&mut self) {
        self.simple(Kind::PopGroup, 0);
    }
}

/// Render a recorded page with the given settings, like [`crate::render`] does from the PDF.
pub fn render_display_list<'a>(list: &DisplayList<'a>, cache: &RenderCache<'a>, settings: &RenderSettings) -> Pixmap {
    let (sx, sy) = (settings.x_scale as f64, settings.y_scale as f64);
    let m = Affine::translate((-settings.x_offset as f64, -settings.y_offset as f64)) * Affine::scale_non_uniform(sx, sy);
    let (pix_w, pix_h) = (
        settings.width.unwrap_or((list.width as f64 * sx).floor() as u16),
        settings.height.unwrap_or((list.height as f64 * sy).floor() as u16),
    );
    // The visible region in recording space, padded for the one-pixel minimum line width.
    let pad = 2.0 / sx.min(sy).max(1e-6);
    let view = m.inverse().transform_rect_bbox(Rect::new(0.0, 0.0, pix_w as f64, pix_h as f64)).inflate(pad, pad);
    let visible = |b: &[f32; 4]| {
        (b[0] as f64) <= view.x1 && (b[2] as f64) >= view.x0 && (b[1] as f64) <= view.y1 && (b[3] as f64) >= view.y0
    };

    let vc_settings = vello_cpu::RenderSettings { level: Level::new(), num_threads: 0, render_mode: RenderMode::OptimizeSpeed };
    let mut device = Renderer::new(pix_w, pix_h, vc_settings, cache);
    device.ctx.set_paint(settings.bg_color);
    device.ctx.fill_rect(&Rect::new(0.0, 0.0, pix_w as f64, pix_h as f64));
    let mut crop = list.crop.clone();
    crop.apply_affine(m);
    device.push_clip_path(&ClipPath { path: crop, fill: FillRule::NonZero });
    device.push_transparency_group(1.0, None, BlendMode::Normal);

    let mut scratch = BezPath::new();
    // Nesting depth inside a clip whose region is off the tile: everything there is invisible.
    let mut skip = 0usize;
    for op in &list.ops {
        let paint = || -> Option<Paint<'a>> {
            let mut p = list.paints.get(op.paint as usize)?.clone();
            if let Paint::Pattern(pat) = &mut p {
                pat.pre_concat_transform(m);
            }
            Some(p)
        };
        let t = || list.transforms.get(op.transform as usize).copied().unwrap_or_default();
        match op.kind {
            Kind::PushClip(fill) => {
                if skip > 0 || !visible(&op.bbox) {
                    skip += 1;
                    continue;
                }
                list.path(op.item, &mut scratch);
                scratch.apply_affine(m);
                device.push_clip_path(&ClipPath { path: std::mem::take(&mut scratch), fill });
            }
            Kind::PushGroup => {
                if skip > 0 {
                    skip += 1;
                    continue;
                }
                let (opacity, blend) = list.groups.get(op.item as usize).copied().unwrap_or((1.0, BlendMode::Normal));
                device.push_transparency_group(opacity, None, blend);
            }
            Kind::PopClip | Kind::PopGroup => {
                if skip > 0 {
                    skip -= 1;
                } else if op.kind == Kind::PopClip {
                    device.pop_clip_path();
                } else {
                    device.pop_transparency_group();
                }
            }
            Kind::Blend => {
                if let Some(b) = list.blends.get(op.item as usize) {
                    device.set_blend_mode(*b);
                }
            }
            _ if skip > 0 || !visible(&op.bbox) => {}
            Kind::Path => {
                let (Some(p), Some(mode)) = (paint(), list.modes.get(op.mode as usize)) else { continue };
                list.path(op.item, &mut scratch);
                device.in_type3_glyph = op.type3;
                device.draw_path(&scratch, m * t(), &p, mode);
            }
            Kind::Rect => {
                let (Some(p), Some(mode)) = (paint(), list.modes.get(op.mode as usize)) else { continue };
                list.path(op.item, &mut scratch);
                let r = scratch.control_box();
                device.in_type3_glyph = op.type3;
                device.draw_rect(&r, m * t(), &p, mode);
            }
            Kind::FillGlyph => {
                let (Some(p), Some(outline)) = (paint(), list.outlines.get(op.item as usize)) else { continue };
                device.in_type3_glyph = op.type3;
                device.fill_path(outline, m * t(), &p, FillRule::NonZero);
            }
            Kind::StrokeGlyph => {
                let (Some(p), Some(outline), Some(PathDrawMode::Stroke(s))) =
                    (paint(), list.outlines.get(op.item as usize), list.modes.get(op.mode as usize))
                else {
                    continue;
                };
                let g = list.transforms.get(op.transform as usize + 1).copied().unwrap_or_default();
                device.in_type3_glyph = op.type3;
                device.stroke_path(&(g * outline.as_ref().clone()), m * t(), &p, s, true);
            }
            Kind::Image => {
                let Some((img, alpha)) = list.images.get(op.item as usize) else { continue };
                device.in_type3_glyph = op.type3;
                device.ctx.set_paint_transform(Affine::IDENTITY);
                device.ctx.set_aliasing_threshold(Some(1));
                device.ctx.set_transform(m * t());
                device.draw_image(img.clone(), alpha.clone());
                device.ctx.set_aliasing_threshold(None);
            }
        }
    }
    device.in_type3_glyph = false;
    device.pop_transparency_group();
    device.pop_clip_path();

    let mut pixmap = Pixmap::new(pix_w, pix_h);
    let mut resources = vello_cpu::Resources::default();
    device.ctx.render_to_pixmap(&mut resources, &mut pixmap);
    pixmap
}
