//! Realize a laid-out XFA form as ordinary PDF: page content streams for the static parts
//! (draws, borders, captions) and AcroForm fields, named by their SOM expressions, for every
//! XFA field. Any viewer can then show and fill the form; [`crate::sync_datasets`] writes the
//! values back into the XFA data so Acrobat sees them too.

use std::collections::HashMap;

use printcraft_cos::{Dict, Document, ObjRef, Object, PdfString, Stream};
use printcraft_forms::{FieldKind, flags};

use crate::layout::{self, Item, Page, Place};
use crate::merge::{Form, Inst, Tag};
use crate::template::{self, Border, Edge, Family, Font, HAlign, Insets, Presence, VAlign};
use crate::xml::{NodeId, Tree};
use crate::{XfaError, picture};

/// What realizing produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub pages: usize,
    pub fields: usize,
    /// Things the form uses that were not reproduced (scripts, unsupported widgets).
    pub notes: Vec<String>,
}

fn n(v: f64) -> String {
    printcraft_forms::appearance::fmt(if v.is_finite() { v } else { 0.0 })
}

fn rgb(c: [f64; 3]) -> String {
    format!("{} {} {}", n(c[0]), n(c[1]), n(c[2]))
}

/// The standard font for a family and style, and its resource name.
fn std_font(f: &Font) -> (&'static str, &'static str) {
    match (f.family, f.bold, f.italic) {
        (Family::Sans, false, false) => ("F1", "Helvetica"),
        (Family::Sans, true, false) => ("F2", "Helvetica-Bold"),
        (Family::Sans, false, true) => ("F3", "Helvetica-Oblique"),
        (Family::Sans, true, true) => ("F4", "Helvetica-BoldOblique"),
        (Family::Serif, false, false) => ("F5", "Times-Roman"),
        (Family::Serif, true, false) => ("F6", "Times-Bold"),
        (Family::Serif, false, true) => ("F7", "Times-Italic"),
        (Family::Serif, true, true) => ("F8", "Times-BoldItalic"),
        (Family::Mono, false, false) => ("F9", "Courier"),
        (Family::Mono, true, false) => ("F10", "Courier-Bold"),
        (Family::Mono, false, true) => ("F11", "Courier-Oblique"),
        (Family::Mono, true, true) => ("F12", "Courier-BoldOblique"),
    }
}

/// A page's content stream under construction (top-left coordinates converted on the way out).
struct Canvas {
    h: f64,
    out: String,
    fonts: HashMap<&'static str, &'static str>,
    images: Vec<(String, ObjRef)>,
}

impl Canvas {
    fn y(&self, y: f64) -> f64 {
        self.h - y
    }

    fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, c: [f64; 3]) {
        self.out.push_str(&format!("{} rg {} {} {} {} re f\n", rgb(c), n(x), n(self.y(y + h)), n(w), n(h)));
    }

    fn line(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, e: &Edge) {
        if !e.visible || e.thickness <= 0.0 {
            return;
        }
        let dash = if e.dashed { "[3 2] 0 d " } else { "" };
        self.out.push_str(&format!(
            "{} RG {}{} w {} {} m {} {} l S [] 0 d\n",
            rgb(e.color),
            dash,
            n(e.thickness),
            n(x0),
            n(self.y(y0)),
            n(x1),
            n(self.y(y1))
        ));
    }

    fn border(&mut self, x: f64, y: f64, w: f64, h: f64, b: &Border) {
        if let Some(c) = b.fill {
            self.fill_rect(x, y, w, h, c);
        }
        let [top, right, bottom, left] = &b.edges;
        // Edges are centred on the box outline ("even" handedness).
        self.line(x, y, x + w, y, top);
        self.line(x + w, y, x + w, y + h, right);
        self.line(x, y + h, x + w, y + h, bottom);
        self.line(x, y, x, y + h, left);
    }

    fn ellipse(&mut self, x: f64, y: f64, w: f64, h: f64, e: &Edge, fill: Option<[f64; 3]>) {
        let (rx, ry) = (w / 2.0, h / 2.0);
        let (cx, cy) = (x + rx, self.y(y + ry));
        let (kx, ky) = (0.5523 * rx, 0.5523 * ry);
        let path = format!(
            "{} {} m {} {} {} {} {} {} c {} {} {} {} {} {} c {} {} {} {} {} {} c {} {} {} {} {} {} c",
            n(cx + rx),
            n(cy),
            n(cx + rx),
            n(cy + ky),
            n(cx + kx),
            n(cy + ry),
            n(cx),
            n(cy + ry),
            n(cx - kx),
            n(cy + ry),
            n(cx - rx),
            n(cy + ky),
            n(cx - rx),
            n(cy),
            n(cx - rx),
            n(cy - ky),
            n(cx - kx),
            n(cy - ry),
            n(cx),
            n(cy - ry),
            n(cx + kx),
            n(cy - ry),
            n(cx + rx),
            n(cy - ky),
            n(cx + rx),
            n(cy)
        );
        if let Some(c) = fill {
            self.out.push_str(&format!("{} rg {path} f\n", rgb(c)));
        }
        if e.visible && e.thickness > 0.0 {
            self.out.push_str(&format!("{} RG {} w {path} S\n", rgb(e.color), n(e.thickness)));
        }
    }

    /// Text in a box (top-left `x`,`y`, size `w`×`h`), wrapped and aligned.
    fn text(&mut self, text: &str, r: [f64; 4], font: &Font, para: &template::Para, wrap: bool) {
        let [x, y, w, h] = r;
        if text.trim().is_empty() {
            return;
        }
        let avail = (w - para.left - para.right).max(1.0);
        let lines: Vec<String> = if wrap { template::wrap(text, font, avail) } else { text.lines().map(str::to_string).collect() };
        let lh = if para.line_height > 0.0 { para.line_height } else { font.size * 1.15 };
        let total = lines.len() as f64 * lh;
        let top = match para.v {
            VAlign::Top => y,
            VAlign::Middle => y + (h - total) / 2.0,
            VAlign::Bottom => y + h - total,
        };
        let (res, base) = std_font(font);
        self.fonts.insert(res, base);
        self.out.push_str(&format!("BT /{res} {} Tf {} rg\n", n(font.size), rgb(font.color)));
        for (i, line) in lines.iter().enumerate().take(5_000) {
            let tw = template::text_width(line, font);
            let lx = match para.h {
                HAlign::Left => x + para.left + if i == 0 { para.indent } else { 0.0 },
                HAlign::Center => x + para.left + (avail - tw) / 2.0,
                HAlign::Right => x + w - para.right - tw,
            };
            // Baseline: the line's top plus the ascent (about 0.8 of the size for the base 14).
            let baseline = top + i as f64 * lh + (lh - font.size) / 2.0 + font.size * 0.8;
            self.out.push_str(&format!("1 0 0 1 {} {} Tm ", n(lx), n(self.y(baseline))));
            let bytes = printcraft_fonts::literal(&printcraft_fonts::win_ansi(line));
            self.out.push_str(&String::from_utf8_lossy(&bytes));
            self.out.push_str(" Tj\n");
            if font.underline {
                self.out.push_str(&format!(
                    "ET {} RG 0.5 w {} {} m {} {} l S BT /{res} {} Tf {} rg\n",
                    rgb(font.color),
                    n(lx),
                    n(self.y(baseline + 1.5)),
                    n(lx + tw),
                    n(self.y(baseline + 1.5)),
                    n(font.size),
                    rgb(font.color)
                ));
            }
        }
        self.out.push_str("ET\n");
    }

    fn image(&mut self, name: String, r: ObjRef, x: f64, y: f64, w: f64, h: f64) {
        self.out.push_str(&format!("q {} 0 0 {} {} {} cm /{name} Do Q\n", n(w), n(h), n(x), n(self.y(y + h))));
        self.images.push((name, r));
    }
}

fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ if c.is_ascii_whitespace() => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Width, height and component count of a JPEG (from its first SOF marker).
fn jpeg_info(d: &[u8]) -> Option<(u32, u32, u8)> {
    if d.get(0..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut i = 2usize;
    while i + 4 <= d.len() {
        if *d.get(i)? != 0xFF {
            i += 1;
            continue;
        }
        let marker = *d.get(i + 1)?;
        if marker == 0xFF {
            i += 1;
            continue;
        }
        let len = u16::from_be_bytes([*d.get(i + 2)?, *d.get(i + 3)?]) as usize;
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let h = u16::from_be_bytes([*d.get(i + 5)?, *d.get(i + 6)?]) as u32;
            let w = u16::from_be_bytes([*d.get(i + 7)?, *d.get(i + 8)?]) as u32;
            let c = *d.get(i + 9)?;
            return (w > 0 && h > 0).then_some((w, h, c));
        }
        i = i.checked_add(2 + len)?;
    }
    None
}

/// An image XObject from embedded image data (JPEG or PNG).
fn image_xobject(doc: &mut Document, data: &[u8]) -> Option<(ObjRef, u32, u32)> {
    let mut d = Dict::new();
    d.set(b"Type".to_vec(), Object::name("XObject"));
    d.set(b"Subtype".to_vec(), Object::name("Image"));
    d.set(b"BitsPerComponent".to_vec(), Object::Int(8));
    if let Some((w, h, c)) = jpeg_info(data) {
        d.set(b"Width".to_vec(), Object::Int(w as i64));
        d.set(b"Height".to_vec(), Object::Int(h as i64));
        d.set(
            b"ColorSpace".to_vec(),
            Object::name(match c {
                1 => "DeviceGray",
                4 => "DeviceCMYK",
                _ => "DeviceRGB",
            }),
        );
        if c == 4 {
            // Adobe-style CMYK JPEGs are stored inverted.
            d.set(b"Decode".to_vec(), Object::Array([1, 0, 1, 0, 1, 0, 1, 0].iter().map(|v| Object::Int(*v)).collect()));
        }
        d.set(b"Filter".to_vec(), Object::name("DCTDecode"));
        let r = doc.add(Object::Stream(Stream::from_raw(d, data.to_vec())));
        return Some((r, w, h));
    }
    let decoder = png::Decoder::new(std::io::Cursor::new(data));
    let mut reader = decoder.read_info().ok()?;
    let (w, h) = (reader.info().width, reader.info().height);
    if w == 0 || h == 0 || (w as u64) * (h as u64) > 40_000_000 {
        return None;
    }
    let mut buf = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buf).ok()?;
    let bytes = buf.get(..frame.buffer_size())?;
    let (ct, depth) = (frame.color_type, frame.bit_depth);
    if depth != png::BitDepth::Eight {
        return None;
    }
    let (rgb_bytes, alpha): (Vec<u8>, Option<Vec<u8>>) = match ct {
        png::ColorType::Rgb => (bytes.to_vec(), None),
        png::ColorType::Rgba => {
            (bytes.chunks(4).flat_map(|p| p.iter().take(3).copied()).collect(), Some(bytes.chunks(4).filter_map(|p| p.get(3).copied()).collect()))
        }
        png::ColorType::Grayscale => (bytes.iter().flat_map(|g| [*g, *g, *g]).collect(), None),
        png::ColorType::GrayscaleAlpha => {
            (bytes.chunks(2).flat_map(|p| [p.first().copied().unwrap_or(0); 3]).collect(), Some(bytes.chunks(2).filter_map(|p| p.get(1).copied()).collect()))
        }
        png::ColorType::Indexed => return None,
    };
    d.set(b"Width".to_vec(), Object::Int(w as i64));
    d.set(b"Height".to_vec(), Object::Int(h as i64));
    d.set(b"ColorSpace".to_vec(), Object::name("DeviceRGB"));
    if let Some(a) = alpha {
        let mut sd = Dict::new();
        sd.set(b"Type".to_vec(), Object::name("XObject"));
        sd.set(b"Subtype".to_vec(), Object::name("Image"));
        sd.set(b"Width".to_vec(), Object::Int(w as i64));
        sd.set(b"Height".to_vec(), Object::Int(h as i64));
        sd.set(b"ColorSpace".to_vec(), Object::name("DeviceGray"));
        sd.set(b"BitsPerComponent".to_vec(), Object::Int(8));
        let s = doc.add(Object::Stream(Stream::flate(sd, &a)));
        d.set(b"SMask".to_vec(), Object::Ref(s));
    }
    let r = doc.add(Object::Stream(Stream::flate(d, &rgb_bytes)));
    Some((r, w, h))
}

/// The kind of AcroForm field an XFA `<ui>` becomes.
#[derive(Clone, Debug, PartialEq)]
enum Ui {
    Text {
        multiline: bool,
        password: bool,
        comb: Option<usize>,
    },
    Check {
        round: bool,
        size: f64,
    },
    Choice {
        list: bool,
        editable: bool,
        multi: bool,
    },
    Button,
    Signature,
    /// Image and barcode fields: drawn as their frame only.
    Static,
}

fn ui_of(t: &Tree, field: NodeId) -> (Ui, Option<NodeId>) {
    let ui = t.child(field, "ui");
    let w = ui.and_then(|u| t.elements(u).find(|c| !matches!(t.tag(*c), "picture" | "extras")));
    let Some(w) = w else { return (Ui::Text { multiline: false, password: false, comb: None }, None) };
    let kind = match t.tag(w) {
        "textEdit" => Ui::Text {
            multiline: t.attr(w, "multiLine") == Some("1"),
            password: false,
            comb: t.child(w, "comb").and_then(|c| t.attr(c, "numberOfCells")).and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 0 && *n < 1000),
        },
        "passwordEdit" => Ui::Text { multiline: false, password: true, comb: None },
        "numericEdit" | "dateTimeEdit" => Ui::Text {
            multiline: false,
            password: false,
            comb: t.child(w, "comb").and_then(|c| t.attr(c, "numberOfCells")).and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 0 && *n < 1000),
        },
        "checkButton" => {
            Ui::Check { round: t.attr(w, "shape") == Some("round"), size: template::attr_measure(t, w, "size").filter(|s| *s > 0.0).unwrap_or(10.0) }
        }
        "choiceList" => {
            let open = t.attr(w, "open").unwrap_or("userInput");
            Ui::Choice {
                list: matches!(open, "always" | "multiSelect"),
                editable: open == "userInput" && t.attr(w, "textEntry") == Some("1"),
                multi: open == "multiSelect",
            }
        }
        "button" => Ui::Button,
        "signature" => Ui::Signature,
        _ => Ui::Static,
    };
    (kind, Some(w))
}

/// Choice items: (save value, display text).
fn items(t: &Tree, field: NodeId) -> Vec<(String, String)> {
    let lists: Vec<NodeId> = t.elements(field).filter(|c| t.tag(*c) == "items").collect();
    let read = |l: NodeId| -> Vec<String> { t.elements(l).map(|e| t.text(e)).take(10_000).collect() };
    match lists.as_slice() {
        [] => Vec::new(),
        [one] => read(*one).into_iter().map(|v| (v.clone(), v)).collect(),
        [a, b, ..] => {
            // The list with save="1" holds the values stored in the data.
            let (save, show) = if t.attr(*b, "save") == Some("1") || t.attr(*a, "presence") == Some("hidden") { (*b, *a) } else { (*a, *b) };
            let (s, d) = (read(save), read(show));
            d.into_iter().enumerate().map(|(i, show)| (s.get(i).cloned().unwrap_or_else(|| show.clone()), show)).collect()
        }
    }
}

/// The display picture of a date field (`<format><picture>`), as a date pattern.
pub fn date_display(t: &Tree, field: NodeId) -> Option<String> {
    let pic = t.path(field, &["format", "picture"]).map(|p| t.text(p))?;
    picture::date_pattern(&pic).or_else(|| {
        // A bare pattern with no category.
        let p = pic.trim();
        (p.contains('Y') && p.contains('M') && p.contains('D') && !p.contains('{')).then(|| p.to_string())
    })
}

/// The AcroForm field tree under construction.
struct Acro {
    roots: Vec<ObjRef>,
    /// Non-terminal nodes by SOM prefix: (object, kids).
    nodes: HashMap<String, (ObjRef, Vec<ObjRef>)>,
    order: Vec<String>,
    /// Widgets of check boxes and radio buttons: their on-state name.
    on_states: HashMap<ObjRef, String>,
    /// Push buttons and signatures, redrawn by the forms crate once they exist.
    redraw: Vec<String>,
    count: usize,
}

/// SOM path split into parts (names can't contain '.', indexes can't either).
pub fn som_parts(som: &str) -> Vec<&str> {
    som.split('.').filter(|p| !p.is_empty()).collect()
}

impl Acro {
    /// Attach a terminal field (or radio group) at `som`, creating its parents.
    fn attach(&mut self, doc: &mut Document, som: &str, field: ObjRef) {
        let parts = som_parts(som);
        let mut parent: Option<String> = None;
        for k in 1..parts.len() {
            let prefix = parts.get(..k).unwrap_or(&[]).join(".");
            if !self.nodes.contains_key(&prefix) {
                let r = doc.add(Object::Null);
                self.nodes.insert(prefix.clone(), (r, Vec::new()));
                self.order.push(prefix.clone());
                match &parent {
                    Some(p) => {
                        if let Some((_, kids)) = self.nodes.get_mut(p) {
                            kids.push(r);
                        }
                    }
                    None => self.roots.push(r),
                }
            }
            parent = Some(prefix);
        }
        let leaf = parts.last().copied().unwrap_or("field");
        let parent_ref = parent.as_ref().and_then(|p| self.nodes.get(p)).map(|(r, _)| *r);
        let _ = doc.update_dict(field, |d| {
            d.set(b"T".to_vec(), PdfString::text(leaf));
            if let Some(p) = parent_ref {
                d.set(b"Parent".to_vec(), Object::Ref(p));
            }
        });
        match parent.and_then(|p| self.nodes.get_mut(&p)) {
            Some((_, kids)) => kids.push(field),
            None => self.roots.push(field),
        }
        self.count += 1;
    }

    fn finish(&self, doc: &mut Document) {
        for prefix in &self.order {
            let Some((r, kids)) = self.nodes.get(prefix) else { continue };
            let parts = som_parts(prefix);
            let mut d = Dict::new();
            d.set(b"T".to_vec(), PdfString::text(parts.last().copied().unwrap_or("")));
            d.set(b"Kids".to_vec(), Object::Array(kids.iter().map(|k| Object::Ref(*k)).collect()));
            if parts.len() > 1 {
                let up = parts.get(..parts.len() - 1).unwrap_or(&[]).join(".");
                if let Some((p, _)) = self.nodes.get(&up) {
                    d.set(b"Parent".to_vec(), Object::Ref(*p));
                }
            }
            doc.set(*r, Object::Dict(d));
        }
    }
}

/// A PDF name for an on-state (never "Off").
fn on_name(v: &str) -> String {
    let v: String = v.chars().filter(|c| !c.is_control()).collect();
    if v.is_empty() || v == "Off" { "On".into() } else { v }
}

struct Ctx<'a> {
    t: &'a Tree,
    forms: &'a [Form],
    page_refs: Vec<ObjRef>,
    acro: Acro,
    /// Per page: widget annotations.
    annots: Vec<Vec<ObjRef>>,
    notes: HashMap<&'static str, usize>,
    image_n: usize,
    /// Radio group members' boxes on the current page, keyed by (form, group instance).
    member_boxes: HashMap<(usize, usize), Vec<Item>>,
    /// Each exclusion group member's group: (form, member) → group.
    group_of: HashMap<(usize, usize), usize>,
}

impl Ctx<'_> {
    fn inst(&self, it: &Item) -> Option<&Inst> {
        self.forms.get(it.form)?.get(it.inst)
    }

    fn note(&mut self, what: &'static str) {
        *self.notes.entry(what).or_insert(0) += 1;
    }

    fn widget(&mut self, doc: &mut Document, page: usize, rect: [f64; 4]) -> Dict {
        let mut d = Dict::new();
        d.set(b"Type".to_vec(), Object::name("Annot"));
        d.set(b"Subtype".to_vec(), Object::name("Widget"));
        d.set(b"Rect".to_vec(), Object::Array(rect.iter().map(|v| Object::Real(if v.is_finite() { *v } else { 0.0 })).collect()));
        if let Some(p) = self.page_refs.get(page) {
            d.set(b"P".to_vec(), Object::Ref(*p));
        }
        d.set(b"F".to_vec(), Object::Int(4));
        let _ = doc;
        d
    }

    fn draw_item(&mut self, doc: &mut Document, c: &mut Canvas, page: usize, it: &Item) {
        let Some(inst) = self.inst(it).cloned() else { return };
        if inst.presence != Presence::Visible {
            return;
        }
        let t = self.t;
        let node = inst.t;
        match inst.tag {
            Tag::Subform | Tag::Area | Tag::ExclGroup => {
                if let Some(b) = t.child(node, "border").and_then(|b| template::border(t, b)) {
                    c.border(it.x, it.y, it.w, it.h, &b);
                }
                if inst.tag == Tag::ExclGroup {
                    self.radio_group(doc, c, page, it, &inst);
                }
            }
            Tag::Draw => self.draw(doc, c, it, node),
            Tag::Field => {
                // Members of an exclusion group are handled with their group.
                if !self.group_of.contains_key(&(it.form, it.inst)) {
                    self.field(doc, c, page, it, &inst);
                }
            }
        }
    }

    fn draw(&mut self, doc: &mut Document, c: &mut Canvas, it: &Item, node: NodeId) {
        let t = self.t;
        if let Some(b) = t.child(node, "border").and_then(|b| template::border(t, b)) {
            c.border(it.x, it.y, it.w, it.h, &b);
        }
        let m = template::margin(t, node);
        let (x, y, w, h) = (it.x + m.left, it.y + m.top, (it.w - m.left - m.right).max(0.0), (it.h - m.top - m.bottom).max(0.0));
        let Some(value) = t.child(node, "value") else { return };
        let Some(v) = t.elements(value).next() else { return };
        match t.tag(v) {
            "line" => {
                let e = t.child(v, "edge").map(|e| template::edge_of(t, e)).unwrap_or(Edge {
                    visible: true,
                    thickness: 0.5,
                    color: [0.0; 3],
                    dashed: false,
                });
                if w < 0.01 || h < 0.01 {
                    c.line(x, y, x + w, y + h, &e);
                } else if t.attr(v, "slope") == Some("/") {
                    c.line(x, y + h, x + w, y, &e);
                } else {
                    c.line(x, y, x + w, y + h, &e);
                }
            }
            "rectangle" => {
                if let Some(b) = template::border(t, v) {
                    c.border(x, y, w, h, &b);
                }
            }
            "arc" => {
                let e = t.child(v, "edge").map(|e| template::edge_of(t, e)).unwrap_or(Edge {
                    visible: true,
                    thickness: 0.5,
                    color: [0.0; 3],
                    dashed: false,
                });
                let fill =
                    t.child(v, "fill").map(|f| t.child(f, "color").and_then(|cc| t.attr(cc, "value")).and_then(template::color).unwrap_or([1.0; 3]));
                c.ellipse(x, y, w, h, &e, fill);
            }
            "image" => {
                let data = base64(&t.text(v));
                match data.and_then(|d| image_xobject(doc, &d)) {
                    Some((r, pw, ph)) => {
                        // Default aspect "fit": as large as fits, proportional, top-left.
                        let (iw, ih) = match t.attr(v, "aspect").unwrap_or("fit") {
                            "none" => (w, h),
                            _ => {
                                let k = (w / pw as f64).min(h / ph as f64);
                                (pw as f64 * k, ph as f64 * k)
                            }
                        };
                        self.image_n += 1;
                        c.image(format!("Im{}", self.image_n), r, x, y, iw, ih);
                    }
                    None => self.note("images not shown (linked or unsupported format)"),
                }
            }
            "exData" => {
                let text = template::rich_text(t, v);
                let font = template::rich_style(t, v, &layout::value_font(t, node));
                c.text(&text, [x, y, w, h], &font, &template::para(t, node), true);
            }
            _ => {
                let text = t.text(v);
                c.text(&text, [x, y, w, h], &layout::value_font(t, node), &template::para(t, node), true);
            }
        }
    }

    /// Split a field's box into its caption and content rectangles (top-left x, y, w, h).
    fn regions(&self, it: &Item, node: NodeId) -> ([f64; 4], Option<([f64; 4], layout::Caption)>) {
        let m = template::margin(self.t, node);
        let (x, y, w, h) = (it.x + m.left, it.y + m.top, (it.w - m.left - m.right).max(0.0), (it.h - m.top - m.bottom).max(0.0));
        let Some(cap) = layout::caption(self.t, node) else { return ([x, y, w, h], None) };
        let r = cap.reserve.max(0.0);
        let (content, region) = match cap.place {
            Place::Left => ([x + r.min(w), y, (w - r).max(0.0), h], [x, y, r.min(w), h]),
            Place::Right => ([x, y, (w - r).max(0.0), h], [x + (w - r).max(0.0), y, r.min(w), h]),
            Place::Top => ([x, y + r.min(h), w, (h - r).max(0.0)], [x, y, w, r.min(h)]),
            Place::Bottom => ([x, y, w, (h - r).max(0.0)], [x, y + (h - r).max(0.0), w, r.min(h)]),
            Place::Inline => ([x, y, w, h], [x, y, w, h]),
        };
        (content, Some((region, cap)))
    }

    fn caption(&self, c: &mut Canvas, region: [f64; 4], cap: &layout::Caption) {
        let m = cap.margin;
        let mut para = cap.para;
        // A caption beside a one-line field with no para of its own lines up with the field's
        // middle (how Designer's default objects look); XFA's default is top.
        let one_line = region[3] < cap.font.size * 1.2 * 2.5;
        if para == template::Para::default() && one_line && matches!(cap.place, Place::Left | Place::Right) {
            para.v = VAlign::Middle;
        }
        c.text(
            &cap.text,
            [region[0] + m.left, region[1] + m.top, (region[2] - m.left - m.right).max(0.0), (region[3] - m.top - m.bottom).max(0.0)],
            &cap.font,
            &para,
            true,
        );
    }

    fn to_pdf_rect(c: &Canvas, r: [f64; 4]) -> [f64; 4] {
        [r[0], c.y(r[1] + r[3]), r[0] + r[2], c.y(r[1])]
    }

    fn field(&mut self, doc: &mut Document, c: &mut Canvas, page: usize, it: &Item, inst: &Inst) {
        let t = self.t;
        let node = inst.t;
        if let Some(b) = t.child(node, "border").and_then(|b| template::border(t, b)) {
            c.border(it.x, it.y, it.w, it.h, &b);
        }
        let (content, cap) = self.regions(it, node);
        if let Some((region, cap)) = &cap {
            self.caption(c, *region, cap);
        }
        let (ui, ui_el) = ui_of(t, node);
        // The widget's own frame (`<ui><textEdit><border>`), drawn on the page.
        let ui_border = ui_el.and_then(|u| t.child(u, "border")).and_then(|b| template::border(t, b));
        let font = layout::value_font(t, node);
        let para = template::para(t, node);
        if inst.som.is_empty() {
            return;
        }
        let access = t.attr(node, "access").unwrap_or("open");
        let mut ff: u32 = if matches!(access, "readOnly" | "protected" | "nonInteractive") { flags::READ_ONLY } else { 0 };
        if t.child(node, "validate").and_then(|v| t.attr(v, "nullTest")) == Some("error") {
            ff |= flags::REQUIRED;
        }
        let tooltip = t.path(node, &["assist", "toolTip"]).map(|n| t.text(n)).filter(|s| !s.trim().is_empty());
        let da = format!(
            "/{} {} Tf {} rg",
            match font.family {
                Family::Sans => "Helv",
                Family::Serif => "TiRo",
                Family::Mono => "Cour",
            },
            n(font.size),
            rgb(font.color)
        );
        let q = match para.h {
            HAlign::Left => 0,
            HAlign::Center => 1,
            HAlign::Right => 2,
        };
        match ui {
            Ui::Text { multiline, password, comb } => {
                if let Some(b) = &ui_border {
                    c.border(content[0], content[1], content[2], content[3], b);
                }
                let mut d = self.widget(doc, page, Self::to_pdf_rect(c, content));
                d.set(b"FT".to_vec(), Object::name("Tx"));
                let mut value = inst.value.clone();
                if let Some(p) = date_display(t, node)
                    && let Some(shown) = picture::format(&value, &p)
                {
                    value = shown;
                }
                if multiline {
                    ff |= flags::MULTILINE;
                }
                if password {
                    ff |= flags::PASSWORD;
                }
                let max_chars =
                    t.path(node, &["value", "text"]).and_then(|v| t.attr(v, "maxChars")).and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 0);
                if let Some(cells) = comb {
                    ff |= flags::COMB;
                    d.set(b"MaxLen".to_vec(), Object::Int(cells as i64));
                } else if let Some(mc) = max_chars {
                    d.set(b"MaxLen".to_vec(), Object::Int(mc as i64));
                }
                if !value.is_empty() {
                    d.set(b"V".to_vec(), PdfString::text(&value));
                }
                self.finish_terminal(doc, page, d, ff, &da, q, tooltip.as_deref(), &inst.som);
            }
            Ui::Choice { list, editable, multi } => {
                if let Some(b) = &ui_border {
                    c.border(content[0], content[1], content[2], content[3], b);
                }
                let mut d = self.widget(doc, page, Self::to_pdf_rect(c, content));
                d.set(b"FT".to_vec(), Object::name("Ch"));
                if !list {
                    ff |= flags::COMBO;
                    if editable {
                        ff |= flags::EDIT;
                    }
                }
                if multi {
                    ff |= flags::MULTI_SELECT;
                }
                let opts = items(t, node);
                let pairs = opts.iter().any(|(s, d)| s != d);
                d.set(
                    b"Opt".to_vec(),
                    Object::Array(
                        opts.iter()
                            .map(|(s, show)| {
                                if pairs {
                                    Object::Array(vec![Object::String(PdfString::text(s)), Object::String(PdfString::text(show))])
                                } else {
                                    Object::String(PdfString::text(show))
                                }
                            })
                            .collect(),
                    ),
                );
                let values: Vec<&str> = inst.value.split('\n').filter(|v| !v.is_empty()).collect();
                match values.as_slice() {
                    [] => {}
                    [one] => d.set(b"V".to_vec(), PdfString::text(one)),
                    many => d.set(b"V".to_vec(), Object::Array(many.iter().map(|v| Object::String(PdfString::text(v))).collect())),
                }
                self.finish_terminal(doc, page, d, ff, &da, q, tooltip.as_deref(), &inst.som);
            }
            Ui::Check { round, size } => {
                let rect = check_rect(content, size, &para);
                let b = ui_border.clone().unwrap_or_else(default_box);
                if round {
                    c.ellipse(rect[0], rect[1], rect[2], rect[3], &b.edges[0], b.fill);
                } else {
                    c.border(rect[0], rect[1], rect[2], rect[3], &b);
                }
                let (on, _) = crate::merge::check_values(t, node);
                let state = on_name(&on);
                let checked = !inst.value.is_empty() && inst.value == on;
                let mut d = self.widget(doc, page, Self::to_pdf_rect(c, rect));
                d.set(b"FT".to_vec(), Object::name("Btn"));
                let cur = if checked { state.clone() } else { "Off".to_string() };
                d.set(b"V".to_vec(), Object::Name(cur.clone().into_bytes()));
                d.set(b"AS".to_vec(), Object::Name(cur.into_bytes()));
                let r = self.finish_terminal(doc, page, d, ff, "/ZaDb 0 Tf 0 g", 0, tooltip.as_deref(), &inst.som);
                self.acro.on_states.insert(r, state);
            }
            Ui::Button => {
                let b = ui_border.clone().or_else(|| t.child(node, "border").and_then(|b| template::border(t, b)));
                if let Some(b) = &b {
                    c.border(content[0], content[1], content[2], content[3], b);
                }
                if let Some((_, cap)) = &cap
                    && matches!(cap.place, Place::Inline)
                {
                    // A button's caption sits on the button.
                }
                let mut d = self.widget(doc, page, Self::to_pdf_rect(c, content));
                d.set(b"FT".to_vec(), Object::name("Btn"));
                ff |= flags::PUSH_BUTTON;
                let mut mk = Dict::new();
                if let Some((_, cap)) = &cap {
                    mk.set(b"CA".to_vec(), PdfString::text(&cap.text));
                }
                d.set(b"MK".to_vec(), Object::Dict(mk));
                self.finish_terminal(doc, page, d, ff, &da, 1, tooltip.as_deref(), &inst.som);
                self.note("buttons without their scripts");
            }
            Ui::Signature => {
                // Signature fields always show where to sign.
                let b = ui_border.clone().unwrap_or_else(default_box);
                c.border(content[0], content[1], content[2], content[3], &b);
                let mut d = self.widget(doc, page, Self::to_pdf_rect(c, content));
                d.set(b"FT".to_vec(), Object::name("Sig"));
                self.finish_terminal(doc, page, d, 0, &da, 0, tooltip.as_deref(), &inst.som);
                self.acro.redraw.push(inst.som.clone());
            }
            Ui::Static => {
                if let Some(b) = &ui_border {
                    c.border(content[0], content[1], content[2], content[3], b);
                }
                self.note("image or barcode fields shown as empty frames");
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_terminal(
        &mut self,
        doc: &mut Document,
        page: usize,
        mut d: Dict,
        ff: u32,
        da: &str,
        q: i64,
        tooltip: Option<&str>,
        som: &str,
    ) -> ObjRef {
        if ff != 0 {
            d.set(b"Ff".to_vec(), Object::Int(ff as i64));
        }
        d.set(b"DA".to_vec(), PdfString::literal(da.as_bytes().to_vec()));
        if q != 0 {
            d.set(b"Q".to_vec(), Object::Int(q));
        }
        if let Some(tt) = tooltip {
            d.set(b"TU".to_vec(), PdfString::text(tt.trim()));
        }
        let r = doc.add(Object::Dict(d));
        self.acro.attach(doc, som, r);
        if let Some(a) = self.annots.get_mut(page) {
            a.push(r);
        }
        r
    }

    fn radio_group(&mut self, doc: &mut Document, c: &mut Canvas, page: usize, it: &Item, inst: &Inst) {
        if inst.som.is_empty() {
            return;
        }
        let t = self.t;
        let Some(form) = self.forms.get(it.form) else { return };
        let members: Vec<usize> = inst.children.clone();
        let mut group = Dict::new();
        group.set(b"FT".to_vec(), Object::name("Btn"));
        let access = t.attr(inst.t, "access").unwrap_or("open");
        let mut ff = flags::RADIO;
        if matches!(access, "readOnly" | "protected" | "nonInteractive") {
            ff |= flags::READ_ONLY;
        }
        group.set(b"Ff".to_vec(), Object::Int(ff as i64));
        group.set(b"DA".to_vec(), PdfString::literal(b"/ZaDb 0 Tf 0 g".to_vec()));
        if let Some(tt) = t.path(inst.t, &["assist", "toolTip"]).map(|n| t.text(n)).filter(|s| !s.trim().is_empty()) {
            group.set(b"TU".to_vec(), PdfString::text(tt.trim()));
        }
        let mut selected = None;
        let mut kids = Vec::new();
        let boxes = self.member_boxes.get(&(it.form, it.inst)).cloned().unwrap_or_default();
        for (k, m) in members.iter().enumerate() {
            let Some(member) = form.get(*m).cloned() else { continue };
            if member.presence != Presence::Visible {
                continue;
            }
            let Some(mi) = boxes.get(k).cloned() else { continue };
            let node = member.t;
            let (content, cap) = self.regions(&mi, node);
            if let Some(b) = t.child(node, "border").and_then(|b| template::border(t, b)) {
                c.border(mi.x, mi.y, mi.w, mi.h, &b);
            }
            if let Some((region, cap)) = &cap {
                self.caption(c, *region, cap);
            }
            let (ui, ui_el) = ui_of(t, node);
            let (round, size) = match ui {
                Ui::Check { round, size } => (round, size),
                _ => (true, 10.0),
            };
            let rect = check_rect(content, size, &template::para(t, node));
            let b = ui_el.and_then(|u| t.child(u, "border")).and_then(|b| template::border(t, b)).unwrap_or_else(default_box);
            if round {
                c.ellipse(rect[0], rect[1], rect[2], rect[3], &b.edges[0], b.fill);
            } else {
                c.border(rect[0], rect[1], rect[2], rect[3], &b);
            }
            let (on, _) = crate::merge::check_values(t, node);
            let state = on_name(&on);
            let is_on = !inst.value.is_empty() && inst.value == on;
            if is_on {
                selected = Some(state.clone());
            }
            let mut w = self.widget(doc, page, Self::to_pdf_rect(c, rect));
            w.set(b"AS".to_vec(), Object::Name(if is_on { state.clone().into_bytes() } else { b"Off".to_vec() }));
            if let Some(tt) = t.path(node, &["assist", "toolTip"]).map(|n| t.text(n)).filter(|s| !s.trim().is_empty()) {
                w.set(b"TU".to_vec(), PdfString::text(tt.trim()));
            }
            let r = doc.add(Object::Dict(w));
            self.acro.on_states.insert(r, state);
            if let Some(a) = self.annots.get_mut(page) {
                a.push(r);
            }
            kids.push(r);
        }
        if kids.is_empty() {
            return;
        }
        group.set(b"V".to_vec(), Object::Name(selected.unwrap_or_else(|| "Off".into()).into_bytes()));
        group.set(b"Kids".to_vec(), Object::Array(kids.iter().map(|k| Object::Ref(*k)).collect()));
        let g = doc.add(Object::Dict(group));
        for k in &kids {
            let _ = doc.update_dict(*k, |d| d.set(b"Parent".to_vec(), Object::Ref(g)));
        }
        self.acro.attach(doc, &inst.som, g);
    }
}

fn default_box() -> Border {
    let e = Edge { visible: true, thickness: 0.5, color: [0.0; 3], dashed: false };
    Border { edges: [e; 4], fill: None, radius: 0.0 }
}

/// Where a check box sits in its content area (size `s`, aligned by the field's para).
fn check_rect(content: [f64; 4], s: f64, para: &template::Para) -> [f64; 4] {
    let s = s.min(content[2].max(4.0)).min(content[3].max(4.0)).max(4.0);
    let x = match para.h {
        HAlign::Left => content[0],
        HAlign::Center => content[0] + (content[2] - s) / 2.0,
        HAlign::Right => content[0] + content[2] - s,
    };
    let y = match para.v {
        VAlign::Top => content[1],
        VAlign::Bottom => content[1] + content[3] - s,
        VAlign::Middle => content[1] + (content[3] - s) / 2.0,
    };
    // Designer centres check boxes vertically unless told otherwise.
    let y = if para == &template::Para::default() { content[1] + (content[3] - s) / 2.0 } else { y };
    [x, y, s, s]
}

/// Insets helper for callers that want the content box of an item.
pub fn inset(it: &Item, m: Insets) -> [f64; 4] {
    [it.x + m.left, it.y + m.top, (it.w - m.left - m.right).max(0.0), (it.h - m.top - m.bottom).max(0.0)]
}

/// Write `pages` into `doc`: replace the page tree, build the AcroForm fields. `forms[0]` is
/// the body form; master-page forms follow.
pub fn write(doc: &mut Document, t: &Tree, forms: &[Form], pages: &[Page]) -> Result<Report, XfaError> {
    let root = doc.root().ok_or_else(|| XfaError::Pdf("no document catalog".into()))?;
    let catalog = doc.get(root).as_dict().cloned().ok_or_else(|| XfaError::Pdf("no document catalog".into()))?;
    let pages_ref = match catalog.reference(b"Pages") {
        Some(r) => r,
        None => {
            let r = doc.add(Object::Null);
            doc.update_dict(root, |c| c.set(b"Pages".to_vec(), Object::Ref(r))).map_err(|e| XfaError::Pdf(e.to_string()))?;
            r
        }
    };
    let page_refs: Vec<ObjRef> = pages.iter().map(|_| doc.add(Object::Null)).collect();
    let mut ctx = Ctx {
        t,
        forms,
        page_refs: page_refs.clone(),
        acro: Acro { roots: Vec::new(), nodes: HashMap::new(), order: Vec::new(), on_states: HashMap::new(), redraw: Vec::new(), count: 0 },
        annots: vec![Vec::new(); pages.len()],
        notes: HashMap::new(),
        image_n: 0,
        member_boxes: HashMap::new(),
        group_of: HashMap::new(),
    };
    for (fi, form) in forms.iter().enumerate() {
        for (gi, g) in form.insts.iter().enumerate() {
            if g.tag == Tag::ExclGroup {
                for m in &g.children {
                    ctx.group_of.insert((fi, *m), gi);
                }
            }
        }
    }
    for (pi, page) in pages.iter().enumerate() {
        // Radio group members' boxes, in member order, for their group.
        ctx.member_boxes.clear();
        for it in &page.items {
            if let Some(g) = ctx.group_of.get(&(it.form, it.inst)).copied() {
                ctx.member_boxes.entry((it.form, g)).or_default().push(it.clone());
            }
        }
        // Keep member boxes aligned with member order (only visible members are emitted).
        for ((f, g), boxes) in ctx.member_boxes.iter_mut() {
            if let Some(group) = forms.get(*f).and_then(|form| form.get(*g)) {
                let order: Vec<usize> = group
                    .children
                    .iter()
                    .copied()
                    .filter(|m| forms.get(*f).and_then(|form| form.get(*m)).is_some_and(|i| i.presence == Presence::Visible))
                    .collect();
                boxes.sort_by_key(|b| order.iter().position(|m| *m == b.inst).unwrap_or(usize::MAX));
            }
        }
        let mut canvas = Canvas { h: page.height, out: String::new(), fonts: HashMap::new(), images: Vec::new() };
        for it in &page.items {
            ctx.draw_item(doc, &mut canvas, pi, it);
        }
        let mut fonts = Dict::new();
        let mut used: Vec<(&str, &str)> = canvas.fonts.iter().map(|(k, v)| (*k, *v)).collect();
        used.sort();
        for (res, base) in used {
            let mut f = Dict::new();
            f.set(b"Type".to_vec(), Object::name("Font"));
            f.set(b"Subtype".to_vec(), Object::name("Type1"));
            f.set(b"BaseFont".to_vec(), Object::name(base));
            f.set(b"Encoding".to_vec(), Object::name("WinAnsiEncoding"));
            fonts.set(res.as_bytes().to_vec(), Object::Dict(f));
        }
        let mut res = Dict::new();
        res.set(b"Font".to_vec(), Object::Dict(fonts));
        if !canvas.images.is_empty() {
            let mut xo = Dict::new();
            for (name, r) in &canvas.images {
                xo.set(name.as_bytes().to_vec(), Object::Ref(*r));
            }
            res.set(b"XObject".to_vec(), Object::Dict(xo));
        }
        let contents = doc.add(Object::Stream(Stream::flate(Dict::new(), canvas.out.as_bytes())));
        let mut pd = Dict::new();
        pd.set(b"Type".to_vec(), Object::name("Page"));
        pd.set(b"Parent".to_vec(), Object::Ref(pages_ref));
        pd.set(b"MediaBox".to_vec(), Object::Array([0.0, 0.0, page.width, page.height].iter().map(|v| Object::Real(*v)).collect()));
        pd.set(b"Resources".to_vec(), Object::Dict(res));
        pd.set(b"Contents".to_vec(), Object::Ref(contents));
        let annots = ctx.annots.get(pi).cloned().unwrap_or_default();
        if !annots.is_empty() {
            pd.set(b"Annots".to_vec(), Object::Array(annots.iter().map(|a| Object::Ref(*a)).collect()));
            pd.set(b"Tabs".to_vec(), Object::name("R"));
        }
        if let Some(r) = page_refs.get(pi) {
            doc.set(*r, Object::Dict(pd));
        }
    }
    let mut pages_dict = Dict::new();
    pages_dict.set(b"Type".to_vec(), Object::name("Pages"));
    pages_dict.set(b"Kids".to_vec(), Object::Array(page_refs.iter().map(|r| Object::Ref(*r)).collect()));
    pages_dict.set(b"Count".to_vec(), Object::Int(page_refs.len() as i64));
    doc.set(pages_ref, Object::Dict(pages_dict));
    ctx.acro.finish(doc);
    // The AcroForm: our fields, the standard fonts, and the XFA kept as it was.
    let (_, af_ref, mut af) = crate::packet::acroform_ref(doc).ok_or(XfaError::NoXfa)?;
    af.set(b"Fields".to_vec(), Object::Array(ctx.acro.roots.iter().map(|r| Object::Ref(*r)).collect()));
    af.set(b"DA".to_vec(), PdfString::literal(b"/Helv 0 Tf 0 g".to_vec()));
    let mut fonts = Dict::new();
    for (name, base) in [("Helv", "Helvetica"), ("TiRo", "Times-Roman"), ("Cour", "Courier"), ("ZaDb", "ZapfDingbats")] {
        let mut f = Dict::new();
        f.set(b"Type".to_vec(), Object::name("Font"));
        f.set(b"Subtype".to_vec(), Object::name("Type1"));
        f.set(b"BaseFont".to_vec(), Object::name(base));
        if base != "ZapfDingbats" {
            f.set(b"Encoding".to_vec(), Object::name("WinAnsiEncoding"));
        }
        fonts.set(name.as_bytes().to_vec(), Object::Dict(f));
    }
    let mut dr = Dict::new();
    dr.set(b"Font".to_vec(), Object::Dict(fonts));
    af.set(b"DR".to_vec(), Object::Dict(dr));
    af.remove(b"NeedAppearances");
    match af_ref {
        Some(r) => doc.set(r, Object::Dict(af)),
        None => doc.update_dict(root, |c| c.set(b"AcroForm".to_vec(), Object::Dict(af))).map_err(|e| XfaError::Pdf(e.to_string()))?,
    }
    // Appearances, now that every field is in the tree.
    let all = printcraft_forms::fields(doc);
    for f in &all {
        for w in &f.widgets {
            let ap = match f.kind {
                FieldKind::Text | FieldKind::Combo | FieldKind::List => {
                    let s = printcraft_forms::appearance::field_appearance(doc, f, w, &f.value);
                    let r = doc.add(Object::Stream(s));
                    let mut d = Dict::new();
                    d.set(b"N".to_vec(), Object::Ref(r));
                    d
                }
                FieldKind::CheckBox | FieldKind::Radio => {
                    let on = ctx.acro.on_states.get(&w.obj).cloned().unwrap_or_else(|| "On".into());
                    printcraft_forms::appearance::check_box_states(doc, w, f.kind, &on)
                }
                FieldKind::PushButton | FieldKind::Signature => continue,
            };
            doc.update_dict(w.obj, |d| d.set(b"AP".to_vec(), Object::Dict(ap))).map_err(|e| XfaError::Pdf(e.to_string()))?;
        }
    }
    for name in all.iter().filter(|f| matches!(f.kind, FieldKind::PushButton | FieldKind::Signature)).map(|f| f.name.clone()).collect::<Vec<_>>() {
        let _ = printcraft_forms::redraw_field(doc, &name);
    }
    let mut notes: Vec<String> = ctx.notes.iter().map(|(k, v)| format!("{v} {k}")).collect();
    notes.sort();
    Ok(Report { pages: pages.len(), fields: ctx.acro.count, notes })
}
