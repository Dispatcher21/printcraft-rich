//! Reading the XFA template (XFA 3.3 Part 1 §2–§8): prototype resolution, measurements, and the
//! style properties (font, para, margin, border, caption) the layout and drawing need.

use std::collections::{HashMap, HashSet};

use crate::xml::{NodeId, Tree};

/// Longest prototype chain followed.
const MAX_PROTO_DEPTH: usize = 16;

/// Container elements that can hold other containers or be placed.
pub fn is_container(tag: &str) -> bool {
    matches!(tag, "subform" | "field" | "draw" | "exclGroup" | "area" | "subformSet")
}

/// Expand `use="#id"` / `usehref="#id"` references (XFA §"Prototypes"): the referencing element
/// takes every attribute and property it doesn't set itself from the prototype, recursively.
pub fn resolve_protos(tree: &mut Tree, template: NodeId) {
    let mut ids: HashMap<String, NodeId> = HashMap::new();
    for n in tree.descendants(template) {
        if let Some(id) = tree.attr(n, "id") {
            ids.entry(id.to_string()).or_insert(n);
        }
    }
    let mut done: HashSet<NodeId> = HashSet::new();
    // Nodes appended while expanding are visited too: the work list grows as it goes.
    let mut work: Vec<NodeId> = tree.descendants(template);
    work.reverse();
    let mut budget = 2_000_000usize;
    while let Some(n) = work.pop() {
        budget = budget.saturating_sub(1);
        if budget == 0 {
            break;
        }
        expand(tree, n, &ids, &mut done, 0);
        // Children copied from a prototype need their own references expanded.
        for c in tree.elements(n).collect::<Vec<_>>().into_iter().rev() {
            if !done.contains(&c) && reference(tree, c).is_some() {
                work.push(c);
            }
        }
    }
}

fn reference(tree: &Tree, n: NodeId) -> Option<String> {
    let r = tree.attr(n, "use").filter(|v| !v.is_empty()).or_else(|| tree.attr(n, "usehref").filter(|v| !v.is_empty()))?;
    // Only same-document id references ("#id"); external fragments are not fetched.
    let id = r.rsplit('#').next().unwrap_or(r);
    (r.contains('#') && !id.is_empty()).then(|| id.to_string())
}

fn expand(tree: &mut Tree, n: NodeId, ids: &HashMap<String, NodeId>, done: &mut HashSet<NodeId>, depth: usize) {
    if depth > MAX_PROTO_DEPTH || !done.insert(n) {
        return;
    }
    let Some(id) = reference(tree, n) else { return };
    let Some(&proto) = ids.get(&id) else { return };
    if proto == n {
        return;
    }
    // The prototype's own references first.
    expand(tree, proto, ids, done, depth + 1);
    merge(tree, n, proto, depth);
    tree.remove_attr(n, "use");
    tree.remove_attr(n, "usehref");
}

/// Take from `proto` what `n` lacks.
fn merge(tree: &mut Tree, n: NodeId, proto: NodeId, depth: usize) {
    if depth > MAX_PROTO_DEPTH * 4 {
        return;
    }
    for (k, v) in tree.attrs(proto).to_vec() {
        if matches!(k.as_str(), "id" | "name" | "use" | "usehref") || tree.attr(n, &k).is_some() {
            continue;
        }
        tree.set_attr(n, &k, &v);
    }
    let theirs: Vec<NodeId> = tree.elements(proto).collect();
    let mut by_tag: Vec<(String, Vec<NodeId>)> = Vec::new();
    for c in theirs {
        let t = tree.tag(c).to_string();
        match by_tag.iter_mut().find(|(k, _)| *k == t) {
            Some((_, v)) => v.push(c),
            None => by_tag.push((t, vec![c])),
        }
    }
    for (tag, protos) in by_tag {
        let mine: Vec<NodeId> = tree.elements(n).filter(|c| tree.tag(*c) == tag).collect();
        if mine.is_empty() {
            for p in protos {
                copy_within(tree, p, n);
            }
        } else if let ([m], [p]) = (mine.as_slice(), protos.as_slice())
            && !is_container(&tag)
        {
            merge(tree, *m, *p, depth + 1);
        }
    }
}

/// Copy a subtree of `tree` under `parent` (in the same tree).
fn copy_within(tree: &mut Tree, src: NodeId, parent: NodeId) {
    // Collect the subtree first (as kinds with their parent's position in the list), so the
    // copy never reads nodes it is adding.
    let mut nodes: Vec<(crate::xml::Kind, Option<usize>)> = Vec::new();
    let mut stack: Vec<(NodeId, Option<usize>, usize)> = vec![(src, None, 0)];
    while let Some((n, up, depth)) = stack.pop() {
        let Some(node) = tree.node(n) else { continue };
        if depth > MAX_PROTO_DEPTH * 16 || nodes.len() > 1_000_000 {
            break;
        }
        nodes.push((node.kind.clone(), up));
        let me = nodes.len() - 1;
        stack.extend(node.children.iter().rev().map(|c| (*c, Some(me), depth + 1)));
    }
    let mut made: Vec<NodeId> = Vec::with_capacity(nodes.len());
    for (kind, up) in nodes {
        let p = up.and_then(|i| made.get(i).copied()).unwrap_or(parent);
        made.push(tree.append(p, kind));
    }
}

/// A measurement in points (`"1.5in"`, `"10mm"`, `"12pt"`, …; bare numbers use `default_unit`).
pub fn measure(s: &str, default_unit: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = (s.get(..split)?.trim(), s.get(split..)?.trim());
    let v: f64 = num.parse().ok()?;
    let unit = if unit.is_empty() { default_unit } else { unit };
    let k = match unit {
        "in" => 72.0,
        "pt" => 1.0,
        "mm" => 72.0 / 25.4,
        "cm" => 72.0 / 2.54,
        "mp" => 0.001,
        "em" => 10.0,
        "%" => return None,
        _ => return None,
    };
    let p = v * k;
    p.is_finite().then_some(p)
}

pub fn attr_measure(tree: &Tree, n: NodeId, key: &str) -> Option<f64> {
    measure(tree.attr(n, key)?, "in")
}

/// Font size and other point-default measurements.
pub fn attr_pt(tree: &Tree, n: NodeId, key: &str) -> Option<f64> {
    measure(tree.attr(n, key)?, "pt")
}

/// `"r,g,b"` (0–255) to 0–1 components.
pub fn color(v: &str) -> Option<[f64; 3]> {
    let parts: Vec<f64> = v.split(',').filter_map(|p| p.trim().parse::<f64>().ok()).collect();
    match parts.as_slice() {
        [r, g, b, ..] => Some([(r / 255.0).clamp(0.0, 1.0), (g / 255.0).clamp(0.0, 1.0), (b / 255.0).clamp(0.0, 1.0)]),
        _ => None,
    }
}

fn color_of(tree: &Tree, n: Option<NodeId>) -> Option<[f64; 3]> {
    tree.attr(tree.child(n?, "color")?, "value").and_then(color)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Family {
    Sans,
    Serif,
    Mono,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Font {
    pub family: Family,
    pub size: f64,
    pub bold: bool,
    pub italic: bool,
    pub color: [f64; 3],
    pub underline: bool,
    /// Horizontal scale (`fontHorizontalScale`, 1.0 = normal width).
    pub hscale: f64,
}

impl Default for Font {
    fn default() -> Self {
        // XFA's default font: 10pt Courier is the spec default, but designers always set one;
        // a sans face reads closer to what forms intend when it's missing.
        Font { family: Family::Sans, size: 10.0, bold: false, italic: false, color: [0.0; 3], underline: false, hscale: 1.0 }
    }
}

pub fn family_of(typeface: &str) -> Family {
    let t = typeface.to_ascii_lowercase();
    if t.contains("courier") || t.contains("mono") || t.contains("consol") || t.contains("ocr") {
        Family::Mono
    } else if t.contains("times")
        || t.contains("serif") && !t.contains("sans")
        || t.contains("garamond")
        || t.contains("georgia")
        || t.contains("minion")
        || t.contains("palatino")
        || t.contains("book")
    {
        Family::Serif
    } else {
        Family::Sans
    }
}

/// The `<font>` of an element, over `base`.
pub fn font(tree: &Tree, n: NodeId, base: &Font) -> Font {
    let mut f = base.clone();
    let Some(el) = tree.child(n, "font") else { return f };
    if let Some(t) = tree.attr(el, "typeface") {
        f.family = family_of(t);
    }
    if let Some(s) = attr_pt(tree, el, "size").filter(|s| *s > 0.0 && *s < 1000.0) {
        f.size = s;
    }
    if let Some(w) = tree.attr(el, "weight") {
        f.bold = w == "bold";
    }
    if let Some(p) = tree.attr(el, "posture") {
        f.italic = p == "italic";
    }
    if let Some(u) = tree.attr(el, "underline") {
        f.underline = u != "0";
    }
    if let Some(c) = color_of(tree, tree.child(el, "fill")) {
        f.color = c;
    }
    if let Some(h) = tree.attr(el, "fontHorizontalScale").and_then(percent) {
        f.hscale = h;
    }
    f
}

/// `"83%"` → 0.83 (kept within 10%–400%).
fn percent(v: &str) -> Option<f64> {
    let x = v.trim().trim_end_matches('%').trim().parse::<f64>().ok()?;
    (x.is_finite() && x > 0.0).then(|| (x / 100.0).clamp(0.1, 4.0))
}

/// Approximate advance of `s` in `font`.
pub fn text_width(s: &str, font: &Font) -> f64 {
    font.hscale.clamp(0.1, 4.0)
        * match font.family {
            Family::Mono => s.chars().count() as f64 * 0.6 * font.size,
            Family::Serif => printcraft_fonts::helvetica_width(s, font.size) * if font.bold { 0.97 } else { 0.92 },
            Family::Sans => printcraft_fonts::helvetica_width(s, font.size) * if font.bold { 1.06 } else { 1.0 },
        }
}

/// Greedy line breaking for `font` within `width`.
/// Line breaks as `\n`: forms also break lines with a bare carriage return (`&#13;`) and
/// the Unicode line and paragraph separators.
pub fn breaks(text: &str) -> String {
    text.replace("\r\n", "\n").replace(['\r', '\u{2028}', '\u{2029}'], "\n")
}

pub fn wrap(text: &str, font: &Font, width: f64) -> Vec<String> {
    let mut lines = Vec::new();
    let text = breaks(text);
    for para in text.split('\n') {
        let para = para.trim_end_matches('\r');
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if text_width(&candidate, font) <= width || line.is_empty() && text_width(word, font) <= width {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            for ch in word.chars() {
                if !line.is_empty() && text_width(&format!("{line}{ch}"), font) > width {
                    lines.push(std::mem::take(&mut line));
                }
                line.push(ch);
            }
        }
        lines.push(line);
        if lines.len() > 10_000 {
            break;
        }
    }
    lines
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Insets {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

pub fn margin(tree: &Tree, n: NodeId) -> Insets {
    let Some(m) = tree.child(n, "margin") else { return Insets::default() };
    let g = |k| attr_measure(tree, m, k).unwrap_or(0.0).max(0.0);
    Insets { top: g("topInset"), right: g("rightInset"), bottom: g("bottomInset"), left: g("leftInset") }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VAlign {
    Top,
    Middle,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Para {
    pub h: HAlign,
    pub v: VAlign,
    pub left: f64,
    pub right: f64,
    pub above: f64,
    pub below: f64,
    pub indent: f64,
    /// Line height in points (0 = from the font).
    pub line_height: f64,
}

impl Default for Para {
    fn default() -> Self {
        Para { h: HAlign::Left, v: VAlign::Top, left: 0.0, right: 0.0, above: 0.0, below: 0.0, indent: 0.0, line_height: 0.0 }
    }
}

pub fn para(tree: &Tree, n: NodeId) -> Para {
    let mut p = Para::default();
    let Some(el) = tree.child(n, "para") else { return p };
    p.h = match tree.attr(el, "hAlign") {
        Some("center") => HAlign::Center,
        Some("right") => HAlign::Right,
        _ => HAlign::Left,
    };
    p.v = match tree.attr(el, "vAlign") {
        Some("middle") => VAlign::Middle,
        Some("bottom") => VAlign::Bottom,
        _ => VAlign::Top,
    };
    let g = |k| attr_measure(tree, el, k).unwrap_or(0.0);
    p.left = g("marginLeft").max(0.0);
    p.right = g("marginRight").max(0.0);
    p.above = g("spaceAbove").max(0.0);
    p.below = g("spaceBelow").max(0.0);
    p.indent = g("textIndent");
    p.line_height = attr_pt(tree, el, "lineHeight").unwrap_or(0.0).max(0.0);
    p
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edge {
    pub visible: bool,
    pub thickness: f64,
    pub color: [f64; 3],
    pub dashed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Border {
    /// Top, right, bottom, left.
    pub edges: [Edge; 4],
    pub fill: Option<[f64; 3]>,
    /// Corner radius (points).
    pub radius: f64,
}

pub fn edge_of(tree: &Tree, e: NodeId) -> Edge {
    let visible = !matches!(tree.attr(e, "presence"), Some("hidden" | "invisible" | "inactive"));
    let thickness = attr_measure(tree, e, "thickness").unwrap_or(0.5 / 72.0 * 72.0).clamp(0.0, 72.0);
    let thickness = if tree.attr(e, "thickness").is_none() { 0.5 } else { thickness };
    let color = color_of(tree, Some(e)).unwrap_or([0.0; 3]);
    let dashed = matches!(tree.attr(e, "stroke"), Some("dashed" | "dotted" | "dashDot" | "dashDotDot"));
    Edge { visible, thickness, color, dashed }
}

/// A `<border>` (or a draw's `<rectangle>`): edges in XFA order (missing ones repeat the last).
pub fn border(tree: &Tree, el: NodeId) -> Option<Border> {
    if matches!(tree.attr(el, "presence"), Some("hidden" | "invisible" | "inactive")) {
        return None;
    }
    let edges: Vec<Edge> = tree.elements(el).filter(|c| tree.tag(*c) == "edge").map(|e| edge_of(tree, e)).collect();
    // A border with no edge element has one default edge (visible, 0.5pt black).
    let default = Edge { visible: true, thickness: 0.5, color: [0.0; 3], dashed: false };
    let pick = |i: usize| edges.get(i).or(edges.last()).copied().unwrap_or(default);
    let fill = tree.child(el, "fill").and_then(|f| {
        if matches!(tree.attr(f, "presence"), Some("hidden" | "invisible" | "inactive")) {
            return None;
        }
        Some(color_of(tree, Some(f)).unwrap_or([1.0; 3]))
    });
    let radius = tree.child(el, "corner").and_then(|c| attr_measure(tree, c, "radius")).unwrap_or(0.0).max(0.0);
    Some(Border { edges: [pick(0), pick(1), pick(2), pick(3)], fill, radius })
}

/// An element's visibility (`presence`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Visible,
    /// Takes space but is not drawn.
    Invisible,
    /// Takes no space and is not drawn.
    Hidden,
}

pub fn presence(tree: &Tree, n: NodeId) -> Presence {
    match tree.attr(n, "presence") {
        Some("invisible") => Presence::Invisible,
        Some("hidden" | "inactive") => Presence::Hidden,
        _ => Presence::Visible,
    }
}

/// Plain text of a value-like element (`<text>`, `<exData>` rich text, numbers, dates).
pub fn value_text(tree: &Tree, value: NodeId) -> Option<String> {
    let el = tree.elements(value).next()?;
    match tree.tag(el) {
        "exData" => Some(rich_text(tree, el)),
        "text" | "integer" | "decimal" | "float" | "date" | "time" | "dateTime" | "boolean" => Some(tree.text(el)),
        _ => None,
    }
}

/// Rich text (XHTML) flattened to paragraphs separated by newlines.
pub fn rich_text(tree: &Tree, n: NodeId) -> String {
    let mut out = String::new();
    let mut stack: Vec<(NodeId, bool)> = tree.children(n).iter().rev().map(|c| (*c, false)).collect();
    let mut steps = 0usize;
    while let Some((c, closing)) = stack.pop() {
        steps += 1;
        if steps > 1_000_000 {
            break;
        }
        if closing {
            if !out.ends_with('\n') && !out.is_empty() {
                out.push('\n');
            }
            continue;
        }
        match tree.node(c).map(|n| &n.kind) {
            Some(crate::xml::Kind::Text(t)) | Some(crate::xml::Kind::CData(t)) => {
                // Source formatting whitespace collapses, as in HTML.
                let mut last_space = out.ends_with(' ') || out.ends_with('\n') || out.is_empty();
                for ch in t.chars() {
                    if ch.is_whitespace() {
                        if !last_space {
                            out.push(' ');
                            last_space = true;
                        }
                    } else {
                        out.push(ch);
                        last_space = false;
                    }
                }
            }
            Some(crate::xml::Kind::Element { .. }) => {
                let tag = tree.tag(c);
                if tag == "br" {
                    out.push('\n');
                    continue;
                }
                let block = matches!(tag, "p" | "div" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "tr");
                if block {
                    stack.push((c, true));
                }
                stack.extend(tree.children(c).iter().rev().map(|k| (*k, false)));
            }
            _ => {}
        }
    }
    out.lines().map(str::trim_end).collect::<Vec<_>>().join("\n").trim_matches('\n').to_string()
}

/// The style of rich text drawn in one font: the first size set, and the weight and
/// horizontal scale that cover most of the text (a bold lead-in like "Note:" doesn't make
/// the whole paragraph bold).
pub fn rich_style(tree: &Tree, n: NodeId, base: &Font) -> Font {
    let mut f = base.clone();
    let mut set_size = false;
    for d in tree.descendants(n) {
        let Some(style) = tree.attr(d, "style") else { continue };
        for decl in style.split(';') {
            let Some((k, v)) = decl.split_once(':') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "font-size" if !set_size => {
                    if let Some(s) = measure(v, "pt").filter(|s| *s > 0.0 && *s < 1000.0) {
                        f.size = s;
                        set_size = true;
                    }
                }
                "font-family" => f.family = family_of(v),
                "font-style" => f.italic = v == "italic",
                "color" => {
                    if let Some(c) = v.strip_prefix('#').filter(|h| h.len() == 6) {
                        let p = |i: usize| c.get(i..i + 2).and_then(|x| u8::from_str_radix(x, 16).ok()).map(|b| b as f64 / 255.0);
                        if let (Some(r), Some(g), Some(b)) = (p(0), p(2), p(4)) {
                            f.color = [r, g, b];
                        }
                    }
                }
                _ => {}
            }
        }
    }
    // Weight and scale by how much text each covers (styles inherit down the tree).
    let (mut bold_chars, mut plain_chars) = (0usize, 0usize);
    let mut scales: Vec<(f64, usize)> = Vec::new();
    let mut stack: Vec<(NodeId, bool, f64, usize)> = vec![(n, base.bold, base.hscale, 0)];
    while let Some((at, bold, hs, depth)) = stack.pop() {
        if depth > 64 {
            continue;
        }
        let (mut bold, mut hs) = (bold, hs);
        if matches!(tree.tag(at), "b" | "strong") {
            bold = true;
        }
        if let Some(style) = tree.attr(at, "style") {
            for decl in style.split(';') {
                let Some((k, v)) = decl.split_once(':') else { continue };
                match k.trim() {
                    "font-weight" => bold = v.trim() == "bold" || v.trim().parse::<u32>().is_ok_and(|w| w >= 600),
                    "xfa-font-horizontal-scale" => hs = percent(v).unwrap_or(hs),
                    _ => {}
                }
            }
        }
        for c in tree.children(at) {
            match tree.node(*c).map(|x| &x.kind) {
                Some(crate::xml::Kind::Text(t)) | Some(crate::xml::Kind::CData(t)) => {
                    let k = t.chars().filter(|ch| !ch.is_whitespace()).count();
                    if bold {
                        bold_chars += k
                    } else {
                        plain_chars += k
                    }
                    match scales.iter_mut().find(|(s, _)| (*s - hs).abs() < 1e-6) {
                        Some(e) => e.1 += k,
                        None => scales.push((hs, k)),
                    }
                }
                Some(crate::xml::Kind::Element { .. }) => stack.push((*c, bold, hs, depth + 1)),
                _ => {}
            }
        }
    }
    if bold_chars + plain_chars > 0 {
        f.bold = bold_chars > plain_chars;
    }
    if let Some((s, _)) = scales.iter().max_by_key(|(_, k)| *k) {
        f.hscale = *s;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measurements() {
        assert_eq!(measure("1in", "in"), Some(72.0));
        assert_eq!(measure("25.4mm", "in"), Some(72.0));
        assert_eq!(measure("12pt", "in"), Some(12.0));
        assert_eq!(measure("2", "in"), Some(144.0));
        assert_eq!(measure("abc", "in"), None);
        assert_eq!(measure("", "in"), None);
    }

    #[test]
    fn prototypes_fill_in_missing_properties() {
        let src = r##"<template><proto><field id="p" w="2in"><font typeface="Courier" size="9pt"/><ui><textEdit/></ui></field></proto>
            <subform name="s"><field name="a" use="#p"><font size="12pt"/></field></subform></template>"##;
        let mut t = Tree::parse(src).unwrap_or_default();
        let tpl = t.child(Tree::ROOT, "template").unwrap_or(0);
        resolve_protos(&mut t, tpl);
        let a = t.path(tpl, &["subform", "field"]).unwrap_or(0);
        assert_eq!(t.attr(a, "w"), Some("2in"));
        assert_eq!(t.attr(a, "name"), Some("a"));
        let f = font(&t, a, &Font::default());
        assert_eq!(f.size, 12.0);
        assert_eq!(f.family, Family::Mono);
        assert!(t.path(a, &["ui", "textEdit"]).is_some());
    }

    #[test]
    fn self_referencing_prototypes_terminate() {
        let src = r##"<template><subform id="a" use="#b"/><subform id="b" use="#a"/><x use="#x" id="x"/></template>"##;
        let mut t = Tree::parse(src).unwrap_or_default();
        resolve_protos(&mut t, 1);
    }

    #[test]
    fn rich_text_paragraphs() {
        let t = Tree::parse(r#"<exData><body><p>Line <span style="font-weight:bold">one</span></p><p>two<br/>three</p></body></exData>"#)
            .unwrap_or_default();
        let e = t.child(Tree::ROOT, "exData").unwrap_or(0);
        assert_eq!(rich_text(&t, e), "Line one\ntwo\nthree");
        // One bold word doesn't make the paragraph bold; mostly bold text is.
        assert!(!rich_style(&t, e, &Font::default()).bold);
        let t = Tree::parse(r#"<exData><body><p style="font-weight:bold;xfa-font-horizontal-scale:80%">Mostly bold <span style="font-weight:normal">x</span></p></body></exData>"#)
            .unwrap_or_default();
        let e = t.child(Tree::ROOT, "exData").unwrap_or(0);
        let f = rich_style(&t, e, &Font::default());
        assert!(f.bold);
        assert!((f.hscale - 0.8).abs() < 1e-9);
        assert!(text_width("abc", &f) < text_width("abc", &Font { bold: true, ..Font::default() }));
    }

    #[test]
    fn carriage_returns_break_lines() {
        assert_eq!(wrap("a\rb\r\nc", &Font::default(), 500.0), vec!["a", "b", "c"]);
    }
}
