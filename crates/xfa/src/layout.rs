//! Layout (XFA 3.3 Part 1 §8 "Layout for Growable Objects", §9 "Dynamic Forms"): size every
//! instance, arrange containers (positioned, top-to-bottom, left-to-right wrapping, rows and
//! tables), then flow the root subform through the page areas' content areas, breaking pages
//! where content overflows or a break asks for it.
//!
//! Simplifications: positioned containers never split; a flowed container splits only between
//! its children; text is not split across pages; `keep` and leaders/trailers are ignored.

use crate::merge::{Form, Tag};
use crate::template::{self, Font, Insets, Presence};
use crate::xml::{NodeId, Tree};

const MAX_DEPTH: usize = 128;
/// Most pages produced.
pub const MAX_PAGES: usize = 2_000;

/// A sized instance with its children placed relative to its top-left corner.
#[derive(Clone, Debug)]
pub struct Boxed {
    pub inst: usize,
    pub w: f64,
    pub h: f64,
    pub kids: Vec<(f64, f64, Boxed)>,
}

/// One placed instance on a page (top-left origin, y down, points).
#[derive(Clone, Debug)]
pub struct Item {
    /// Which form: 0 for the body, else a master-page form index + 1.
    pub form: usize,
    pub inst: usize,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Clone, Debug)]
pub struct Page {
    pub width: f64,
    pub height: f64,
    pub page_area: Option<NodeId>,
    pub items: Vec<Item>,
}

/// Page size of a `<medium>` (default US Letter portrait).
pub fn medium(t: &Tree, page_area: NodeId) -> (f64, f64) {
    let Some(m) = t.child(page_area, "medium") else { return (612.0, 792.0) };
    let (mut w, mut h) = match t.attr(m, "stock").unwrap_or("letter").to_ascii_lowercase().as_str() {
        "legal" => (612.0, 1008.0),
        "a4" => (595.28, 841.89),
        "a3" => (841.89, 1190.55),
        "a5" => (419.53, 595.28),
        "tabloid" | "ledger" => (792.0, 1224.0),
        "executive" => (522.0, 756.0),
        _ => (612.0, 792.0),
    };
    if let (Some(s), Some(l)) = (template::attr_measure(t, m, "short"), template::attr_measure(t, m, "long"))
        && s > 1.0
        && l > 1.0
    {
        (w, h) = (s.min(l), s.max(l));
    }
    if t.attr(m, "orientation") == Some("landscape") {
        (w, h) = (h, w);
    }
    (w.clamp(36.0, 14_400.0), h.clamp(36.0, 14_400.0))
}

/// Content areas of a page area: (x, y, w, h).
pub fn content_areas(t: &Tree, page_area: NodeId, size: (f64, f64)) -> Vec<[f64; 4]> {
    let areas: Vec<[f64; 4]> = t
        .elements(page_area)
        .filter(|c| t.tag(*c) == "contentArea")
        .map(|c| {
            let g = |k| template::attr_measure(t, c, k).unwrap_or(0.0);
            let w = template::attr_measure(t, c, "w").unwrap_or(size.0);
            let h = template::attr_measure(t, c, "h").unwrap_or(size.1);
            [g("x"), g("y"), w.max(1.0), h.max(1.0)]
        })
        .collect();
    if areas.is_empty() { vec![[18.0, 18.0, size.0 - 36.0, size.1 - 36.0]] } else { areas }
}

/// Every page area, in document order (nested page sets flattened).
pub fn page_areas(t: &Tree, root_subform: NodeId) -> Vec<NodeId> {
    let Some(ps) = t.child(root_subform, "pageSet") else { return Vec::new() };
    t.descendants(ps).into_iter().filter(|n| t.tag(*n) == "pageArea").collect()
}

pub struct Sizer<'a> {
    pub t: &'a Tree,
    pub form: &'a Form,
}

/// The font an instance draws its value with (inherited fonts are not part of XFA: each
/// element defaults on its own).
pub fn value_font(t: &Tree, n: NodeId) -> Font {
    template::font(t, n, &Font::default())
}

/// Caption geometry: placement and reserved size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Left,
    Right,
    Top,
    Bottom,
    Inline,
}

pub struct Caption {
    pub text: String,
    pub font: Font,
    pub place: Place,
    /// Reserved extent along the placement axis (points).
    pub reserve: f64,
    pub para: template::Para,
    pub margin: Insets,
}

pub fn caption(t: &Tree, field: NodeId) -> Option<Caption> {
    let c = t.child(field, "caption")?;
    if matches!(template::presence(t, c), Presence::Hidden | Presence::Invisible) {
        return None;
    }
    let value = t.child(c, "value");
    let rich = value.and_then(|v| t.child(v, "exData"));
    let text = value.and_then(|v| template::value_text(t, v)).unwrap_or_default();
    let base = template::font(t, c, &value_font(t, field));
    let font = match rich {
        Some(r) => template::rich_style(t, r, &base),
        None => base,
    };
    let place = match t.attr(c, "placement") {
        Some("right") => Place::Right,
        Some("top") => Place::Top,
        Some("bottom") => Place::Bottom,
        Some("inline") => Place::Inline,
        _ => Place::Left,
    };
    let margin = template::margin(t, c);
    let reserve = match template::attr_measure(t, c, "reserve").filter(|r| *r > 0.0) {
        Some(r) => r,
        None if text.is_empty() => 0.0,
        None => match place {
            Place::Left | Place::Right => {
                text.lines().map(|l| template::text_width(l, &font)).fold(0.0, f64::max) + font.size * 0.5 + margin.left + margin.right
            }
            Place::Top | Place::Bottom => text.lines().count().max(1) as f64 * font.size * 1.2 + margin.top + margin.bottom,
            Place::Inline => 0.0,
        },
    };
    Some(Caption { text, font, place, reserve, para: template::para(t, c), margin })
}

impl Sizer<'_> {
    fn geom(&self, n: NodeId, key: &str) -> Option<f64> {
        template::attr_measure(self.t, n, key).filter(|v| v.is_finite())
    }

    /// Natural height of a field or draw's content for width `w`.
    fn leaf_height(&self, i: usize, w: f64) -> f64 {
        let Some(inst) = self.form.get(i) else { return 0.0 };
        let n = inst.t;
        let m = template::margin(self.t, n);
        let font = value_font(self.t, n);
        let line = font.size * 1.2;
        let mut h = m.top + m.bottom;
        let cap = if inst.tag == Tag::Field { caption(self.t, n) } else { None };
        let cap_w = cap.as_ref().filter(|c| matches!(c.place, Place::Left | Place::Right)).map_or(0.0, |c| c.reserve);
        let text = match inst.tag {
            Tag::Draw => self.t.child(n, "value").and_then(|v| template::value_text(self.t, v)).unwrap_or_default(),
            _ => inst.value.clone(),
        };
        let avail = (w - m.left - m.right - cap_w).max(1.0);
        let lines = if text.is_empty() { 1 } else { template::wrap(&text, &font, avail).len().max(1) };
        h += lines as f64 * line;
        if let Some(c) = cap.filter(|c| matches!(c.place, Place::Top | Place::Bottom)) {
            h += c.reserve;
        }
        h
    }

    fn leaf_width(&self, i: usize) -> f64 {
        let Some(inst) = self.form.get(i) else { return 0.0 };
        let n = inst.t;
        let m = template::margin(self.t, n);
        let font = value_font(self.t, n);
        let text = match inst.tag {
            Tag::Draw => self.t.child(n, "value").and_then(|v| template::value_text(self.t, v)).unwrap_or_default(),
            _ => inst.value.clone(),
        };
        let tw = text.lines().map(|l| template::text_width(l, &font)).fold(0.0, f64::max);
        let cap = caption(self.t, n).filter(|c| matches!(c.place, Place::Left | Place::Right)).map_or(0.0, |c| c.reserve);
        tw + m.left + m.right + cap + 2.0
    }

    /// Clamp to `minX`/`maxX`.
    fn clamp(&self, n: NodeId, v: f64, min: &str, max: &str) -> f64 {
        let mut v = v;
        if let Some(lo) = self.geom(n, min) {
            v = v.max(lo);
        }
        if let Some(hi) = self.geom(n, max).filter(|h| *h > 0.0) {
            v = v.min(hi);
        }
        v.max(0.0)
    }

    /// Lay out instance `i` given the width its parent offers (for growable widths).
    pub fn arrange(&self, i: usize, avail_w: f64, depth: usize) -> Boxed {
        let empty = Boxed { inst: i, w: 0.0, h: 0.0, kids: Vec::new() };
        let Some(inst) = self.form.get(i) else { return empty };
        if depth > MAX_DEPTH {
            return empty;
        }
        let n = inst.t;
        match inst.tag {
            Tag::Field | Tag::Draw => {
                let w = self.geom(n, "w").unwrap_or_else(|| self.clamp(n, self.leaf_width(i).min(avail_w.max(1.0)), "minW", "maxW"));
                let h = self.geom(n, "h").unwrap_or_else(|| self.clamp(n, self.leaf_height(i, w), "minH", "maxH"));
                Boxed { inst: i, w, h, kids: Vec::new() }
            }
            Tag::Subform | Tag::ExclGroup | Tag::Area => self.container(i, avail_w, depth),
        }
    }

    fn container(&self, i: usize, avail_w: f64, depth: usize) -> Boxed {
        let Some(inst) = self.form.get(i) else { return Boxed { inst: i, w: 0.0, h: 0.0, kids: Vec::new() } };
        let n = inst.t;
        let m = template::margin(self.t, n);
        let layout = self.t.attr(n, "layout").unwrap_or("position");
        let fixed_w = self.geom(n, "w");
        let inner_w = (fixed_w.unwrap_or(avail_w) - m.left - m.right).max(0.0);
        let kids: Vec<usize> = inst.children.iter().copied().filter(|k| self.form.get(*k).is_some_and(|c| c.presence != Presence::Hidden)).collect();
        let mut placed: Vec<(f64, f64, Boxed)> = Vec::new();
        let (mut content_w, mut content_h) = (0.0f64, 0.0f64);
        match layout {
            "tb" | "table" => {
                let cols = if layout == "table" { self.columns(n) } else { Vec::new() };
                let mut y = 0.0;
                for k in kids {
                    let b = if layout == "table" && !cols.is_empty() { self.row(k, &cols, depth + 1) } else { self.arrange(k, inner_w, depth + 1) };
                    let x = self.flow_x(k, b.w, inner_w);
                    content_w = content_w.max(x + b.w);
                    y += self.space(k).0;
                    let h = b.h;
                    placed.push((m.left + x, m.top + y, b));
                    y += h + self.space(k).1;
                }
                content_h = y;
            }
            "row" => {
                let cols = self.columns(n);
                let b = self.row_cells(&kids, &cols, inner_w, depth + 1);
                content_w = b.iter().map(|(x, _, c)| x + c.w).fold(0.0, f64::max);
                content_h = b.iter().map(|(_, _, c)| c.h).fold(0.0, f64::max);
                placed.extend(b.into_iter().map(|(x, y, c)| (m.left + x, m.top + y, c)));
            }
            "lr-tb" | "rl-tb" => {
                let (mut x, mut y, mut row_h) = (0.0f64, 0.0f64, 0.0f64);
                let mut row: Vec<(f64, f64, Boxed)> = Vec::new();
                let rtl = layout == "rl-tb";
                let flush = |row: &mut Vec<(f64, f64, Boxed)>, placed: &mut Vec<(f64, f64, Boxed)>, used: f64| {
                    for (bx, by, b) in row.drain(..) {
                        let bx = if rtl { inner_w - used + bx } else { bx };
                        placed.push((m.left + bx, m.top + by, b));
                    }
                };
                for k in kids {
                    let b = self.arrange(k, inner_w, depth + 1);
                    if x > 0.0 && x + b.w > inner_w + 0.01 {
                        flush(&mut row, &mut placed, x);
                        y += row_h;
                        x = 0.0;
                        row_h = 0.0;
                    }
                    content_w = content_w.max(x + b.w);
                    row_h = row_h.max(b.h);
                    let w = b.w;
                    row.push((x, y, b));
                    x += w;
                }
                flush(&mut row, &mut placed, x);
                content_h = y + row_h;
            }
            _ => {
                // Positioned: x/y place each child's anchor point.
                for k in kids {
                    let b = self.arrange(k, inner_w, depth + 1);
                    let (x, y) = self.anchored(k, b.w, b.h);
                    content_w = content_w.max(x + b.w);
                    content_h = content_h.max(y + b.h);
                    placed.push((m.left + x, m.top + y, b));
                }
            }
        }
        let w = fixed_w.unwrap_or_else(|| {
            let natural = if matches!(layout, "tb" | "table" | "lr-tb" | "rl-tb") && self.geom(n, "minW").is_none() && self.geom(n, "maxW").is_none()
            {
                // Flowed containers without a width take what their parent offers.
                avail_w.max(content_w + m.left + m.right)
            } else {
                content_w + m.left + m.right
            };
            self.clamp(n, natural, "minW", "maxW")
        });
        let h = self.geom(n, "h").unwrap_or_else(|| self.clamp(n, content_h + m.top + m.bottom, "minH", "maxH"));
        Boxed { inst: i, w, h, kids: placed }
    }

    /// Space above and below a flowed child (its para's spaceAbove/Below for draws).
    fn space(&self, k: usize) -> (f64, f64) {
        self.form.get(k).map(|c| template::para(self.t, c.t)).map_or((0.0, 0.0), |p| (p.above, p.below))
    }

    /// A child in a tb flow honours its x offset only when it has an explicit one.
    fn flow_x(&self, k: usize, _w: f64, _avail: f64) -> f64 {
        self.form.get(k).and_then(|c| self.geom(c.t, "x")).unwrap_or(0.0).max(0.0)
    }

    fn anchored(&self, k: usize, w: f64, h: f64) -> (f64, f64) {
        let Some(c) = self.form.get(k) else { return (0.0, 0.0) };
        let x = self.geom(c.t, "x").unwrap_or(0.0);
        let y = self.geom(c.t, "y").unwrap_or(0.0);
        let (fx, fy) = match self.t.attr(c.t, "anchorType").unwrap_or("topLeft") {
            "topCenter" => (0.5, 0.0),
            "topRight" => (1.0, 0.0),
            "middleLeft" => (0.0, 0.5),
            "middleCenter" => (0.5, 0.5),
            "middleRight" => (1.0, 0.5),
            "bottomLeft" => (0.0, 1.0),
            "bottomCenter" => (0.5, 1.0),
            "bottomRight" => (1.0, 1.0),
            _ => (0.0, 0.0),
        };
        (x - fx * w, y - fy * h)
    }

    fn columns(&self, n: NodeId) -> Vec<f64> {
        self.t
            .attr(n, "columnWidths")
            .map(|s| s.split_whitespace().map(|c| template::measure(c, "in").unwrap_or(-1.0)).take(1_000).collect())
            .unwrap_or_default()
    }

    /// A table row (a subform whose own layout is "row") laid out with the table's columns.
    fn row(&self, k: usize, cols: &[f64], depth: usize) -> Boxed {
        let Some(inst) = self.form.get(k) else { return Boxed { inst: k, w: 0.0, h: 0.0, kids: Vec::new() } };
        if inst.tag != Tag::Subform || self.t.attr(inst.t, "layout") != Some("row") {
            return self.arrange(k, cols.iter().filter(|c| **c > 0.0).sum(), depth);
        }
        let kids: Vec<usize> = inst.children.iter().copied().filter(|c| self.form.get(*c).is_some_and(|c| c.presence != Presence::Hidden)).collect();
        let total: f64 = cols.iter().filter(|c| **c > 0.0).sum();
        let cells = self.row_cells(&kids, cols, total, depth + 1);
        let w = cells.iter().map(|(x, _, c)| x + c.w).fold(0.0, f64::max);
        let h = cells.iter().map(|(_, _, c)| c.h).fold(0.0, f64::max);
        let h = self.geom(inst.t, "h").unwrap_or_else(|| self.clamp(inst.t, h, "minH", "maxH"));
        // Cells stretch to the row height.
        let cells = cells
            .into_iter()
            .map(|(x, y, mut c)| {
                c.h = c.h.max(h);
                (x, y, c)
            })
            .collect();
        Boxed { inst: k, w, h, kids: cells }
    }

    fn row_cells(&self, kids: &[usize], cols: &[f64], avail: f64, depth: usize) -> Vec<(f64, f64, Boxed)> {
        let mut out = Vec::new();
        let mut x = 0.0;
        let mut col = 0usize;
        for k in kids {
            let span = self.form.get(*k).and_then(|c| self.t.attr(c.t, "colSpan")).and_then(|s| s.parse::<i64>().ok()).unwrap_or(1);
            let width = if cols.is_empty() {
                None
            } else if span < 0 {
                Some(cols.iter().skip(col).filter(|c| **c > 0.0).sum::<f64>())
            } else {
                let span = span.max(1) as usize;
                Some(cols.iter().skip(col).take(span).filter(|c| **c > 0.0).sum::<f64>())
            };
            col = col.saturating_add(span.max(1) as usize);
            let mut b = self.arrange(*k, width.unwrap_or(avail), depth);
            if let Some(w) = width.filter(|w| *w > 0.0) {
                b.w = w;
            }
            let w = b.w;
            out.push((x, 0.0, b));
            x += w;
        }
        out
    }
}

/// Break requests of a container: (before, before target, after, after target).
#[derive(Clone, Debug, Default)]
pub struct Breaks {
    pub before: Option<BreakTo>,
    pub after: Option<BreakTo>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BreakTo {
    ContentArea,
    PageArea(Option<String>),
}

pub fn breaks(t: &Tree, n: NodeId) -> Breaks {
    let mut b = Breaks::default();
    let target = |s: Option<&str>| s.map(|v| v.trim().trim_start_matches('#').to_string()).filter(|v| !v.is_empty());
    for (tag, before) in [("breakBefore", true), ("breakAfter", false)] {
        for el in t.elements(n).filter(|c| t.tag(*c) == tag) {
            let to = match t.attr(el, "targetType").unwrap_or("auto") {
                "pageArea" => Some(BreakTo::PageArea(target(t.attr(el, "target")))),
                "contentArea" => Some(BreakTo::ContentArea),
                _ => None,
            };
            if to.is_some() {
                if before {
                    b.before = to;
                } else {
                    b.after = to;
                }
            }
        }
    }
    // XFA 2.x `<break>`.
    if let Some(el) = t.child(n, "break") {
        let kind = |k: &str, tk: &str| match t.attr(el, k) {
            Some("pageArea" | "pageEven" | "pageOdd") => Some(BreakTo::PageArea(target(t.attr(el, tk)))),
            Some("contentArea") => Some(BreakTo::ContentArea),
            _ => None,
        };
        if b.before.is_none() {
            b.before = kind("before", "beforeTarget");
        }
        if b.after.is_none() {
            b.after = kind("after", "afterTarget");
        }
    }
    b
}

/// Flow the body into pages.
pub struct Paginator<'a> {
    t: &'a Tree,
    form: &'a Form,
    areas: Vec<NodeId>,
    pub pages: Vec<Page>,
    /// Index into `areas` of the current page's page area.
    area_idx: usize,
    /// How many times each page area has been used.
    used: Vec<usize>,
    content: Vec<[f64; 4]>,
    content_idx: usize,
    cursor: f64,
    pending: Option<BreakTo>,
}

impl<'a> Paginator<'a> {
    pub fn new(t: &'a Tree, form: &'a Form, areas: Vec<NodeId>) -> Self {
        let used = vec![0; areas.len()];
        let mut p =
            Paginator { t, form, areas, pages: Vec::new(), area_idx: 0, used, content: Vec::new(), content_idx: 0, cursor: 0.0, pending: None };
        p.start_page(None, true);
        p
    }

    fn occur_max(&self, a: NodeId) -> Option<usize> {
        let o = self.t.child(a, "occur")?;
        let m: i64 = self.t.attr(o, "max")?.trim().parse().ok()?;
        (m >= 0).then_some(m as usize)
    }

    fn find_area(&self, target: &str) -> Option<usize> {
        self.areas.iter().position(|a| self.t.attr(*a, "id") == Some(target) || self.t.attr(*a, "name") == Some(target))
    }

    fn start_page(&mut self, target: Option<&str>, first: bool) {
        if self.pages.len() >= MAX_PAGES {
            // Stay on the last page: overflow piles up there rather than looping forever.
            self.cursor = 0.0;
            return;
        }
        if !self.areas.is_empty() {
            let idx = match target.and_then(|t| self.find_area(t)) {
                Some(i) => i,
                None if first => 0,
                None => {
                    let cur = self.area_idx;
                    let exhausted =
                        self.areas.get(cur).and_then(|a| self.occur_max(*a)).is_some_and(|m| self.used.get(cur).copied().unwrap_or(0) >= m);
                    if exhausted && cur + 1 < self.areas.len() { cur + 1 } else { cur }
                }
            };
            self.area_idx = idx;
            if let Some(u) = self.used.get_mut(idx) {
                *u += 1;
            }
        }
        let area = self.areas.get(self.area_idx).copied();
        let size = area.map(|a| medium(self.t, a)).unwrap_or((612.0, 792.0));
        self.content = area.map(|a| content_areas(self.t, a, size)).unwrap_or_else(|| vec![[18.0, 18.0, size.0 - 36.0, size.1 - 36.0]]);
        self.content_idx = 0;
        self.cursor = 0.0;
        self.pages.push(Page { width: size.0, height: size.1, page_area: area, items: Vec::new() });
    }

    fn next_area(&mut self, to: &BreakTo) {
        match to {
            BreakTo::ContentArea if self.content_idx + 1 < self.content.len() => {
                self.content_idx += 1;
                self.cursor = 0.0;
            }
            BreakTo::ContentArea => self.start_page(None, false),
            BreakTo::PageArea(target) => self.start_page(target.as_deref(), false),
        }
    }

    fn area(&self) -> [f64; 4] {
        self.content.get(self.content_idx).copied().unwrap_or([0.0, 0.0, 612.0, 792.0])
    }

    /// Place the root subform's box.
    pub fn place_root(&mut self, root: &Boxed) {
        let flowed = self.form.get(root.inst).is_some_and(|r| matches!(self.t.attr(r.t, "layout"), Some("tb" | "table" | "lr-tb" | "rl-tb")));
        if flowed {
            // The root's own margins and decorations are not drawn per page.
            for (x, _, k) in &root.kids {
                self.place(k, *x, 0);
            }
        } else {
            self.place(root, 0.0, 0);
        }
    }

    fn place(&mut self, b: &Boxed, x: f64, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let Some(inst) = self.form.get(b.inst) else { return };
        let br = breaks(self.t, inst.t);
        if let Some(to) = self.pending.take() {
            self.next_area(&to);
        }
        if let Some(to) = &br.before
            && (self.cursor > 0.0 || matches!(to, BreakTo::PageArea(Some(_))) && !self.page_is_target(to))
        {
            self.next_area(to);
        }
        let area = self.area();
        if self.cursor + b.h > area[3] + 0.01 {
            let splittable = inst.tag == Tag::Subform
                && matches!(self.t.attr(inst.t, "layout"), Some("tb" | "table"))
                && !b.kids.is_empty()
                && !keep_intact(self.t, inst.t);
            if splittable {
                let top = template::margin(self.t, inst.t).top;
                self.cursor += top.min(b.h);
                let mut last_y = None::<f64>;
                for (kx, ky, k) in &b.kids {
                    // Keep the gaps between flowed children.
                    if let Some(prev_end) = last_y {
                        self.cursor += (ky - prev_end).max(0.0);
                    }
                    last_y = Some(ky + k.h);
                    self.place(k, x + kx, depth + 1);
                }
                if let Some(to) = br.after {
                    self.pending = Some(to);
                }
                return;
            }
            if self.cursor > 0.0 {
                self.next_area(&BreakTo::ContentArea);
            }
        }
        let area = self.area();
        let (ox, oy) = (area[0] + x, area[1] + self.cursor);
        self.emit(b, ox, oy, 0);
        self.cursor += b.h;
        if let Some(to) = br.after {
            self.pending = Some(to);
        }
    }

    fn page_is_target(&self, to: &BreakTo) -> bool {
        match to {
            BreakTo::PageArea(Some(t)) => self.cursor <= 0.0 && self.find_area(t) == Some(self.area_idx),
            _ => false,
        }
    }

    fn emit(&mut self, b: &Boxed, x: f64, y: f64, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        if let Some(p) = self.pages.last_mut() {
            p.items.push(Item { form: 0, inst: b.inst, x, y, w: b.w, h: b.h });
        }
        for (kx, ky, k) in &b.kids {
            self.emit(k, x + kx, y + ky, depth + 1);
        }
    }
}

fn keep_intact(t: &Tree, n: NodeId) -> bool {
    t.child(n, "keep").and_then(|k| t.attr(k, "intact")).is_some_and(|v| v == "contentArea" || v == "pageArea")
}

/// Emit a box tree at an absolute position (master pages).
pub fn emit_all(b: &Boxed, x: f64, y: f64, form: usize, out: &mut Vec<Item>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    out.push(Item { form, inst: b.inst, x, y, w: b.w, h: b.h });
    for (kx, ky, k) in &b.kids {
        emit_all(k, x + kx, y + ky, form, out, depth + 1);
    }
}
