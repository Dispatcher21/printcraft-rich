//! The live form (XFA 3.3 Part 1 "Form DOM"): the merged form that scripts read and change.
//!
//! [`Live`] holds the XDP tree, the merged body form (and, after layout, one form per master
//! page), and what scripts changed:
//!
//! - **values** go into the instances and, for bound fields, straight into the data;
//! - **visibility** (`presence`) is kept on the instance;
//! - **other properties** (`access`, captions, colours, list items, …) are applied to a copy of
//!   the instance's template node, so layout and realize read them like any template property.
//!   Each change is also kept as an [`Op`] on the instance;
//! - **instances** of repeating subforms are added, removed and moved together with their data
//!   groups, so the data alone says how many rows there are.
//!
//! The ops are saved in the PDF ([`Live::save_state`]) and applied again when the form is next
//! read, so a section a script showed stays shown across edits and after reopening.

use std::collections::{HashMap, HashSet};

use printcraft_cos::{Dict, Document, Object, PdfString};
use printcraft_forms::FieldKind;

use crate::merge::{self, DataPath, Form, Inst, Key, Op, Tag};
use crate::packet::{self, Xdp};
use crate::template::{self, Presence};
use crate::xml::{Kind, NodeId, Tree, local};
use crate::{XfaError, picture, realize};

const MAX_DEPTH: usize = 128;
/// Most saved property changes read back.
const MAX_OPS: usize = 100_000;
/// Most instances a script can add in one run.
const MAX_ADDED: usize = 5_000;

/// An instance: which form (0 is the body, then master pages) and its index there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct N {
    pub form: usize,
    pub inst: usize,
}

impl N {
    pub const ROOT: N = N { form: 0, inst: 0 };
}

/// Property elements of containers (XFA template syntax), as opposed to attributes.
pub const PROPERTY_ELEMENTS: &[&str] = &[
    "arc",
    "assist",
    "bind",
    "boolean",
    "border",
    "button",
    "calculate",
    "caption",
    "checkButton",
    "choiceList",
    "color",
    "comb",
    "corner",
    "date",
    "dateTime",
    "dateTimeEdit",
    "decimal",
    "desc",
    "edge",
    "exData",
    "extras",
    "fill",
    "float",
    "font",
    "format",
    "image",
    "imageEdit",
    "integer",
    "items",
    "keep",
    "line",
    "linear",
    "margin",
    "message",
    "numericEdit",
    "occur",
    "para",
    "passwordEdit",
    "pattern",
    "picture",
    "radial",
    "rectangle",
    "signature",
    "solid",
    "speak",
    "stipple",
    "text",
    "textEdit",
    "time",
    "toolTip",
    "traversal",
    "ui",
    "validate",
    "value",
];

/// Leaf elements whose `value` is their text content.
const CONTENT_ELEMENTS: &[&str] =
    &["boolean", "date", "dateTime", "decimal", "exData", "float", "image", "integer", "message", "picture", "speak", "text", "time", "toolTip"];

pub fn is_content_element(tag: &str) -> bool {
    CONTENT_ELEMENTS.contains(&tag)
}

/// The form as scripts see it.
pub struct Live {
    pub xdp: Xdp,
    pub template: NodeId,
    /// The template's root subform.
    pub root: NodeId,
    /// `<xfa:data>`.
    pub data_root: NodeId,
    /// The body (0) and, after layout, the master page forms.
    pub forms: Vec<Form>,
    parents: Vec<Vec<Option<usize>>>,
    /// Detached element holding copies of template nodes that scripts changed.
    holder: NodeId,
    /// Copy → original template node.
    origin: HashMap<NodeId, NodeId>,
    /// Template nodes in document order.
    order: HashMap<NodeId, usize>,
    /// The data changed (the datasets packet must be written).
    pub data_changed: bool,
    /// Something layout depends on changed (visibility, properties, instances).
    pub layout_changed: bool,
    /// Field values changed since this was last cleared.
    pub changed: HashSet<N>,
    /// How many value writes happened (calculations stop when a pass makes none).
    pub writes: usize,
    added: usize,
}

/// Escape a state field (tabs, newlines and backslashes).
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "\\r")
}

fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

/// Drop "[0]" indexes so `a[0].b[1]` and `a.b[1]` compare equal.
pub fn normalize(som: &str) -> String {
    som.replace("[0]", "")
}

fn parse_presence(v: &str) -> Presence {
    match v {
        "invisible" => Presence::Invisible,
        "hidden" | "inactive" => Presence::Hidden,
        _ => Presence::Visible,
    }
}

pub fn presence_name(p: Presence) -> &'static str {
    match p {
        Presence::Visible => "visible",
        Presence::Invisible => "invisible",
        Presence::Hidden => "hidden",
    }
}

fn index_parents(form: &Form) -> Vec<Option<usize>> {
    let mut p = vec![None; form.insts.len()];
    for (i, inst) in form.insts.iter().enumerate() {
        for c in &inst.children {
            if let Some(x) = p.get_mut(*c) {
                *x = Some(i);
            }
        }
    }
    p
}

/// Format a number the way XFA stores it (no trailing zeros, no exponent).
pub fn number_text(v: f64) -> String {
    if !v.is_finite() {
        return String::new();
    }
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let s = format!("{v:.10}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

impl Live {
    /// Read the document's XFA form and merge it with its data.
    pub fn load(doc: &Document) -> Result<Live, XfaError> {
        let mut xdp = packet::read(doc)?;
        let template = xdp.template.ok_or(XfaError::NoTemplate)?;
        template::resolve_protos(&mut xdp.tree, template);
        let data_root = packet::data_root(&mut xdp);
        let root = xdp.tree.child(template, "subform").ok_or(XfaError::NoTemplate)?;
        let body = merge::merge(&xdp.tree, root, &xdp.tree, Some(data_root));
        let holder = xdp.tree.append_element(Tree::ROOT, "printcraft-copies");
        xdp.tree.detach(holder);
        let order = xdp.tree.descendants(template).into_iter().enumerate().map(|(i, n)| (n, i)).collect();
        let parents = vec![index_parents(&body)];
        Ok(Live {
            xdp,
            template,
            root,
            data_root,
            forms: vec![body],
            parents,
            holder,
            origin: HashMap::new(),
            order,
            data_changed: false,
            layout_changed: false,
            changed: HashSet::new(),
            writes: 0,
            added: 0,
        })
    }

    pub fn tree(&self) -> &Tree {
        &self.xdp.tree
    }

    pub fn inst(&self, n: N) -> Option<&Inst> {
        self.forms.get(n.form)?.get(n.inst)
    }

    fn inst_mut(&mut self, n: N) -> Option<&mut Inst> {
        self.forms.get_mut(n.form)?.insts.get_mut(n.inst)
    }

    pub fn parent(&self, n: N) -> Option<N> {
        self.parents.get(n.form)?.get(n.inst).copied().flatten().map(|inst| N { form: n.form, inst })
    }

    /// The original template node of a (possibly copied) one.
    pub fn orig(&self, t: NodeId) -> NodeId {
        self.origin.get(&t).copied().unwrap_or(t)
    }

    /// Recompute the parent index of `form` (after instances were added or removed).
    pub fn reindex(&mut self, form: usize) {
        if let Some(f) = self.forms.get(form) {
            let p = index_parents(f);
            match self.parents.get_mut(form) {
                Some(slot) => *slot = p,
                None => {
                    while self.parents.len() < form {
                        self.parents.push(Vec::new());
                    }
                    self.parents.push(p);
                }
            }
        }
    }

    /// Replace the master page forms (after layout).
    pub fn set_masters(&mut self, masters: Vec<Form>) {
        self.forms.truncate(1);
        self.parents.truncate(1);
        for m in masters {
            self.parents.push(index_parents(&m));
            self.forms.push(m);
        }
    }

    /// Children as SOM sees them (areas are transparent).
    pub fn kids(&self, n: N) -> Vec<N> {
        let Some(form) = self.forms.get(n.form) else { return Vec::new() };
        let Some(inst) = form.get(n.inst) else { return Vec::new() };
        merge::flatten_areas(form, &inst.children, 0).into_iter().map(|inst| N { form: n.form, inst }).collect()
    }

    /// Every instance of `form` reachable from its root, in document order.
    pub fn walk(&self, form: usize) -> Vec<N> {
        let mut out = Vec::new();
        let Some(f) = self.forms.get(form) else { return out };
        let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
        let mut seen = HashSet::new();
        while let Some((i, depth)) = stack.pop() {
            if depth > MAX_DEPTH || !seen.insert(i) {
                continue;
            }
            let Some(inst) = f.get(i) else { continue };
            out.push(N { form, inst: i });
            stack.extend(inst.children.iter().rev().map(|c| (*c, depth + 1)));
        }
        out
    }

    /// The instance's SOM key: its name, or `#subform`, `#field`, … when it has none.
    pub fn key(&self, n: N) -> String {
        match self.inst(n) {
            Some(i) if !i.name.is_empty() => i.name.clone(),
            Some(i) => match i.tag {
                Tag::Subform => "#subform".into(),
                Tag::Field => "#field".into(),
                Tag::ExclGroup => "#exclGroup".into(),
                Tag::Draw => "#draw".into(),
                Tag::Area => "#area".into(),
            },
            None => String::new(),
        }
    }

    /// Children named `name`, looking through unnamed subforms (which are transparent).
    pub fn named(&self, n: N, name: &str) -> Vec<N> {
        let mut out = Vec::new();
        self.named_into(n, name, &mut out, 0);
        out
    }

    fn named_into(&self, n: N, name: &str, out: &mut Vec<N>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        for k in self.kids(n) {
            let Some(inst) = self.inst(k) else { continue };
            if self.key(k) == name {
                out.push(k);
            } else if inst.name.is_empty() && inst.tag == Tag::Subform && !name.starts_with('#') {
                self.named_into(k, name, out, depth + 1);
            }
        }
    }

    /// Position among same-named siblings (`index` in scripts).
    pub fn index(&self, n: N) -> usize {
        let key = self.key(n);
        match self.parent(n) {
            Some(p) => self.kids(p).into_iter().filter(|k| self.key(*k) == key).position(|k| k == n).unwrap_or(0),
            None => 0,
        }
    }

    /// Same-named siblings, including `n`.
    pub fn same_named(&self, n: N) -> Vec<N> {
        let key = self.key(n);
        match self.parent(n) {
            Some(p) => self.kids(p).into_iter().filter(|k| self.key(*k) == key).collect(),
            None => vec![n],
        }
    }

    /// A `<variables><script name="…">` script object declared on `n`.
    pub fn script_object(&self, n: N, name: &str) -> Option<NodeId> {
        let t = self.tree();
        let v = t.child(self.inst(n)?.t, "variables")?;
        t.elements(v).find(|s| t.tag(*s) == "script" && t.attr(*s, "name") == Some(name))
    }

    /// The template node of a repeating subform named `name` under `parent` (for `_name` and
    /// `instanceManager`), and the instance that holds its instances.
    pub fn manager(&self, parent: N, name: &str) -> Option<(N, NodeId)> {
        if let Some(first) = self.named(parent, name).into_iter().find(|k| self.inst(*k).is_some_and(|i| i.tag == Tag::Subform)) {
            let t = self.orig(self.inst(first)?.t);
            return Some((self.parent(first).unwrap_or(parent), t));
        }
        // No instance right now: find the template under the parent's template.
        let t = self.tree();
        let pt = self.inst(parent)?.t;
        let mut stack: Vec<(NodeId, usize)> = t.elements(pt).map(|c| (c, 0)).collect();
        while let Some((c, depth)) = stack.pop() {
            if depth > 8 {
                continue;
            }
            match t.tag(c) {
                "subform" if t.attr(c, "name") == Some(name) => return Some((parent, self.orig(c))),
                "subformSet" => stack.extend(t.elements(c).map(|x| (x, depth + 1))),
                "subform" if t.attr(c, "name").is_none_or(str::is_empty) => stack.extend(t.elements(c).map(|x| (x, depth + 1))),
                _ => {}
            }
        }
        None
    }

    /// The instances of template subform `t` under `parent`, in order.
    pub fn instances(&self, parent: N, t: NodeId) -> Vec<N> {
        self.kids(parent).into_iter().filter(|k| self.inst(*k).is_some_and(|i| i.tag == Tag::Subform && self.orig(i.t) == t)).collect()
    }

    /// `<occur>` of a template subform: (min, max).
    pub fn occur(&self, t: NodeId) -> (usize, usize) {
        let tree = self.tree();
        let raw_max = tree.child(t, "occur").and_then(|o| tree.attr(o, "max")).and_then(|v| v.trim().parse::<i64>().ok());
        let (min, max, _) = merge::occur(tree, t);
        // Without an <occur>, a subform occurs exactly once.
        let max = if raw_max.is_none() && tree.child(t, "occur").is_none() { 1 } else { max };
        (min, max)
    }

    // ── values ──────────────────────────────────────────────────────────────────────────────

    /// What kind of value a field holds, from its `<value>` element.
    pub fn numeric(&self, n: N) -> bool {
        let Some(inst) = self.inst(n) else { return false };
        let t = self.tree();
        let Some(v) = t.child(inst.t, "value").and_then(|v| t.elements(v).next()) else {
            return t.path(inst.t, &["ui", "numericEdit"]).is_some();
        };
        matches!(t.tag(v), "integer" | "decimal" | "float" | "boolean")
    }

    /// A field's raw value: `None` when empty (null).
    pub fn raw(&self, n: N) -> Option<String> {
        let inst = self.inst(n)?;
        match inst.tag {
            Tag::Field | Tag::ExclGroup => (!inst.value.is_empty()).then(|| inst.value.clone()),
            Tag::Draw => {
                let t = self.tree();
                let v = t.child(inst.t, "value").and_then(|v| t.elements(v).next())?;
                let text = if t.tag(v) == "exData" { template::rich_text(t, v) } else { t.text(v) };
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        }
    }

    /// The value as the field shows it (`formattedValue`): dates in the display picture, list
    /// values as their display text.
    pub fn formatted(&self, n: N) -> String {
        let Some(raw) = self.raw(n) else { return String::new() };
        let Some(inst) = self.inst(n) else { return raw };
        let t = self.tree();
        if let Some(p) = realize::date_display(t, inst.t)
            && let Some(shown) = picture::format(&raw, &p)
        {
            return shown;
        }
        let items = self.items(n);
        if let Some((_, show)) = items.iter().find(|(s, _)| *s == raw) {
            return show.clone();
        }
        raw
    }

    /// Set a field's (or exclusion group's, or draw's) raw value. Bound fields write the data,
    /// and every other field bound to the same data node follows.
    pub fn set_raw(&mut self, n: N, value: Option<&str>) {
        let Some(inst) = self.inst(n) else { return };
        let tag = inst.tag;
        let mut v = value.unwrap_or("").to_string();
        if tag == Tag::Draw {
            let t = self.tree();
            let content = t.child(inst.t, "value").and_then(|x| t.elements(x).next()).map(|x| t.tag(x).to_string()).unwrap_or_else(|| "text".into());
            self.set_op(n, Op { path: vec!["value".into(), content], key: Key::Text, value: v });
            return;
        }
        if !matches!(tag, Tag::Field | Tag::ExclGroup) {
            return;
        }
        // Decimal and integer values are kept rounded as their value element says.
        let t = self.tree();
        if let Some(num) = t.child(inst.t, "value").and_then(|x| t.elements(x).next())
            && !v.is_empty()
            && let Ok(f) = v.trim().parse::<f64>()
        {
            match t.tag(num) {
                "integer" => v = number_text(f.round()),
                "decimal" => {
                    let digits = t.attr(num, "fracDigits").and_then(|d| d.parse::<i32>().ok()).unwrap_or(2);
                    if digits >= 0 {
                        let p = 10f64.powi(digits.min(15));
                        v = number_text((f * p).round() / p);
                    }
                }
                _ => {}
            }
        }
        let data = inst.data.clone();
        if inst.value == v {
            return;
        }
        if let Some(i) = self.inst_mut(n) {
            i.value = v.clone();
        }
        self.changed.insert(n);
        self.writes += 1;
        let Some(path) = data else { return };
        self.write_data(&path, &v);
        // Fields sharing the data node (global binding, the same name bound twice).
        let others: Vec<N> = self.walk(0).into_iter().filter(|k| *k != n && self.inst(*k).is_some_and(|i| i.data.as_ref() == Some(&path))).collect();
        for k in others {
            if let Some(i) = self.inst_mut(k)
                && i.value != v
            {
                i.value = v.clone();
                self.changed.insert(k);
            }
        }
    }

    /// Write a data value (creating the node and its ancestors when missing).
    pub fn write_data(&mut self, path: &DataPath, v: &str) {
        let current = merge::Data { tree: &self.xdp.tree, root: Some(self.data_root) }.find(path).map(|d| merge::data_value(&self.xdp.tree, d));
        if current.as_deref() == Some(v) || (current.is_none() && v.is_empty()) {
            return;
        }
        let node = merge::ensure_path(&mut self.xdp.tree, self.data_root, path);
        // Multi-select lists keep one <value> per selection.
        let values: Vec<&str> = v.split('\n').collect();
        let multi = values.len() > 1 || self.xdp.tree.elements(node).any(|c| self.xdp.tree.tag(c) == "value");
        if multi {
            self.xdp.tree.set_text(node, "");
            for one in values.iter().filter(|x| !x.is_empty()) {
                let e = self.xdp.tree.append_element(node, "value");
                self.xdp.tree.set_text(e, one);
            }
        } else {
            self.xdp.tree.set_text(node, v);
        }
        self.data_changed = true;
    }

    /// Choice list items as (save value, display text).
    pub fn items(&self, n: N) -> Vec<(String, String)> {
        self.inst(n).map(|i| realize::items(self.tree(), i.t)).unwrap_or_default()
    }

    pub fn set_items(&mut self, n: N, items: &[(String, String)]) {
        let value = items.iter().map(|(s, d)| format!("{}\t{}", esc(s), esc(d))).collect::<Vec<_>>().join("\n");
        self.set_op(n, Op { path: Vec::new(), key: Key::Items, value });
    }

    pub fn presence(&self, n: N) -> Presence {
        self.inst(n).map(|i| i.presence).unwrap_or(Presence::Visible)
    }

    pub fn set_presence(&mut self, n: N, v: &str) {
        self.set_op(n, Op { path: Vec::new(), key: Key::Attr("presence".into()), value: v.to_string() });
    }

    /// A property attribute at `path` below the instance's (possibly copied) template node.
    pub fn attr(&self, n: N, path: &[String], name: &str) -> Option<String> {
        let t = self.tree();
        let mut at = self.inst(n)?.t;
        for p in path {
            at = t.child(at, p)?;
        }
        if name == "presence" && path.is_empty() {
            return Some(presence_name(self.presence(n)).to_string());
        }
        t.attr(at, name).map(str::to_string)
    }

    /// The text of the element at `path` (`caption.value.text.value`, `assist.toolTip.value`).
    pub fn text_at(&self, n: N, path: &[String]) -> Option<String> {
        let t = self.tree();
        let mut at = self.inst(n)?.t;
        for p in path {
            at = t.child(at, p)?;
        }
        Some(if t.tag(at) == "exData" { template::rich_text(t, at) } else { t.text(at) })
    }

    /// Whether the element at `path` exists in the template (`isPropertySpecified`).
    pub fn has_element(&self, n: N, path: &[String]) -> bool {
        let t = self.tree();
        let Some(mut at) = self.inst(n).map(|i| i.t) else { return false };
        for p in path {
            match t.child(at, p) {
                Some(c) => at = c,
                None => return false,
            }
        }
        true
    }

    /// Record and apply a property change.
    pub fn set_op(&mut self, n: N, op: Op) {
        if self.apply_op(n, &op)
            && let Some(i) = self.inst_mut(n)
        {
            i.ops.retain(|o| !(o.path == op.path && o.key == op.key));
            if i.ops.len() < 1_000 {
                i.ops.push(op);
            }
        }
    }

    /// Apply a change; returns whether it applied.
    fn apply_op(&mut self, n: N, op: &Op) -> bool {
        if self.inst(n).is_none() {
            return false;
        }
        if op.path.is_empty() && op.key == Key::Attr("presence".into()) {
            let p = parse_presence(&op.value);
            if let Some(i) = self.inst_mut(n)
                && i.presence != p
            {
                i.presence = p;
                self.layout_changed = true;
            }
            return true;
        }
        let Some(mut at) = self.copy_on_write(n) else { return false };
        for step in &op.path {
            if step.is_empty() || step.contains(['<', '>', '&', '"', ' ']) {
                return false;
            }
            at = match self.xdp.tree.child(at, step) {
                Some(c) => c,
                None => self.xdp.tree.append_element(at, step),
            };
        }
        let tree = &mut self.xdp.tree;
        match &op.key {
            Key::Attr(a) => {
                if a.is_empty() || a.contains(['<', '>', '&', '"', ' ', '=']) {
                    return false;
                }
                tree.set_attr(at, a, &op.value);
            }
            Key::Text => tree.set_text(at, &op.value),
            Key::Items => {
                for old in tree.elements(at).filter(|c| tree.tag(*c) == "items").collect::<Vec<_>>() {
                    tree.detach(old);
                }
                let pairs: Vec<(String, String)> = op
                    .value
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| match l.split_once('\t') {
                        Some((s, d)) => (unesc(s), unesc(d)),
                        None => (unesc(l), unesc(l)),
                    })
                    .collect();
                let shown = tree.append_element(at, "items");
                for (_, d) in &pairs {
                    let e = tree.append_element(shown, "text");
                    tree.set_text(e, d);
                }
                if pairs.iter().any(|(s, d)| s != d) {
                    let saved = tree.append_element(at, "items");
                    tree.set_attr(saved, "save", "1");
                    tree.set_attr(saved, "presence", "hidden");
                    for (s, _) in &pairs {
                        let e = tree.append_element(saved, "text");
                        tree.set_text(e, s);
                    }
                }
            }
        }
        self.layout_changed = true;
        true
    }

    /// Give the instance its own copy of its template node (container children stay shared),
    /// so a property change affects only this instance.
    fn copy_on_write(&mut self, n: N) -> Option<NodeId> {
        let t = self.inst(n)?.t;
        if self.origin.contains_key(&t) {
            return Some(t);
        }
        let tree = &mut self.xdp.tree;
        let kind = tree.node(t)?.kind.clone();
        if !matches!(kind, Kind::Element { .. }) {
            return None;
        }
        let copy = tree.append(self.holder, kind);
        for c in tree.children(t).to_vec() {
            if tree.is_element(c) && (template::is_container(tree.tag(c)) || matches!(tree.tag(c), "pageSet" | "proto" | "variables")) {
                tree.share_child(copy, c);
            } else {
                tree.copy_within(c, copy);
            }
        }
        self.origin.insert(copy, t);
        if let Some(i) = self.inst_mut(n) {
            i.t = copy;
        }
        Some(copy)
    }

    // ── instances ───────────────────────────────────────────────────────────────────────────

    /// The data path of the nearest instance at or above `n` that has one.
    fn context_path(&self, mut n: N) -> DataPath {
        for _ in 0..MAX_DEPTH {
            if let Some(p) = self.inst(n).and_then(|i| i.data.clone()) {
                return p;
            }
            match self.parent(n) {
                Some(p) => n = p,
                None => break,
            }
        }
        Vec::new()
    }

    /// Insert an empty data element `name` at index `idx` among its same-named siblings under
    /// `prefix`, and shift the paths of later siblings' instances.
    fn insert_data(&mut self, prefix: &DataPath, name: &str, idx: usize) {
        let parent = merge::ensure_path(&mut self.xdp.tree, self.data_root, prefix);
        let tree = &mut self.xdp.tree;
        let same: Vec<NodeId> = tree.elements(parent).filter(|c| tree.tag(*c) == name).collect();
        let kids = tree.children(parent).to_vec();
        let pos = match same.get(idx) {
            Some(at) => kids.iter().position(|c| c == at).unwrap_or(kids.len()),
            None => same.last().and_then(|l| kids.iter().position(|c| c == l)).map(|p| p + 1).unwrap_or(kids.len()),
        };
        // Pad missing siblings so the new element lands at `idx`.
        let missing = idx.saturating_sub(same.len());
        for k in 0..=missing {
            tree.insert(parent, pos + k, Kind::Element { name: name.to_string(), attrs: Vec::new() });
        }
        self.shift_paths(prefix, name, idx, 1);
        self.data_changed = true;
    }

    /// Add `delta` to the index of data step `name` (at `prefix`) for indexes ≥ `from`.
    fn shift_paths(&mut self, prefix: &DataPath, name: &str, from: usize, delta: isize) {
        let depth = prefix.len();
        for form in &mut self.forms {
            for inst in &mut form.insts {
                let Some(p) = inst.data.as_mut() else { continue };
                if p.len() <= depth || p.get(..depth) != Some(prefix.as_slice()) {
                    continue;
                }
                if let Some(step) = p.get_mut(depth)
                    && step.0 == name
                    && step.1 >= from
                {
                    step.1 = step.1.saturating_add_signed(delta);
                }
            }
        }
    }

    /// `instanceManager.addInstance` / `insertInstance`: a new instance of template subform `t`
    /// under `parent` at position `at` (the end when `None`).
    pub fn add_instance(&mut self, parent: N, t: NodeId, at: Option<usize>) -> Result<N, String> {
        let existing = self.instances(parent, t);
        let (_, max) = self.occur(t);
        if existing.len() >= max {
            return Err(format!("no more than {max} instances are allowed"));
        }
        if self.added >= MAX_ADDED {
            return Err("too many instances were added".into());
        }
        let k = at.unwrap_or(existing.len()).min(existing.len());
        let tree = &self.xdp.tree;
        let name = tree.attr(t, "name").unwrap_or("").to_string();
        let (mode, _) = merge::bind_of(tree, t);
        let context = self.context_path(parent);
        // The data group: next to the existing instances' groups, else under the context.
        let plan: Option<DataPath> = if mode == "none" || (name.is_empty() && mode != "dataRef") {
            None
        } else {
            let sibling = existing.iter().find_map(|e| self.inst(*e).and_then(|i| i.data.clone()));
            match sibling {
                Some(p) => p.split_last().map(|((nm, first), prefix)| {
                    let mut q = prefix.to_vec();
                    // The first existing instance's group index plus the position.
                    let base = existing
                        .first()
                        .and_then(|e| self.inst(*e))
                        .and_then(|i| i.data.as_ref())
                        .and_then(|d| d.last())
                        .map(|s| s.1)
                        .unwrap_or(*first);
                    q.push((nm.clone(), base + k));
                    q
                }),
                None if mode == "once" || mode.is_empty() => {
                    let mut q = context.clone();
                    q.push((name.clone(), 0));
                    Some(q)
                }
                None => None,
            }
        };
        if let Some(p) = &plan
            && let Some(((nm, idx), prefix)) = p.split_last()
        {
            self.insert_data(&prefix.to_vec(), nm, *idx);
        }
        let new = {
            let tree = &self.xdp.tree;
            let form = self.forms.get_mut(parent.form).ok_or("no such form")?;
            merge::merge_one(tree, tree, Some(self.data_root), form, t, plan, context).ok_or("the form is too large")?
        };
        let pos = {
            let form = self.forms.get(parent.form).ok_or("no such form")?;
            let pi = form.get(parent.inst).ok_or("no such parent")?;
            match (k, existing.get(k.wrapping_sub(1)), existing.first()) {
                (0, _, Some(first)) => pi.children.iter().position(|c| *c == first.inst).unwrap_or(pi.children.len()),
                (_, Some(prev), _) => pi.children.iter().position(|c| *c == prev.inst).map(|p| p + 1).unwrap_or(pi.children.len()),
                _ => {
                    // No instance yet: before the first child that comes later in the template.
                    let mine = self.order.get(&t).copied().unwrap_or(usize::MAX);
                    pi.children
                        .iter()
                        .position(|c| form.get(*c).is_some_and(|ci| self.order.get(&self.orig(ci.t)).copied().unwrap_or(0) > mine))
                        .unwrap_or(pi.children.len())
                }
            }
        };
        if let Some(p) = self.forms.get_mut(parent.form).and_then(|f| f.insts.get_mut(parent.inst)) {
            let pos = pos.min(p.children.len());
            p.children.insert(pos, new);
        }
        self.renumber(parent);
        self.added += 1;
        self.layout_changed = true;
        Ok(N { form: parent.form, inst: new })
    }

    /// `instanceManager.removeInstance`.
    pub fn remove_instance(&mut self, parent: N, t: NodeId, index: usize) -> Result<(), String> {
        let existing = self.instances(parent, t);
        let (min, _) = self.occur(t);
        if existing.len() <= min {
            return Err(format!("at least {min} instances are required"));
        }
        let victim = *existing.get(index).ok_or_else(|| format!("there is no instance {index}"))?;
        if let Some(p) = self.forms.get_mut(parent.form).and_then(|f| f.insts.get_mut(parent.inst)) {
            p.children.retain(|c| *c != victim.inst);
        }
        if let Some(path) = self.inst(victim).and_then(|i| i.data.clone())
            && let Some(((nm, idx), prefix)) = path.split_last()
        {
            if let Some(node) = (merge::Data { tree: &self.xdp.tree, root: Some(self.data_root) }).find(&path) {
                self.xdp.tree.detach(node);
                self.data_changed = true;
            }
            // Its own instances' paths die with it; later siblings move up.
            let prefix = prefix.to_vec();
            if let Some(i) = self.inst_mut(victim) {
                i.data = None;
            }
            self.shift_paths(&prefix, nm, idx + 1, -1);
        }
        self.renumber(parent);
        self.layout_changed = true;
        Ok(())
    }

    /// `instanceManager.moveInstance`.
    pub fn move_instance(&mut self, parent: N, t: NodeId, from: usize, to: usize) -> Result<(), String> {
        let existing = self.instances(parent, t);
        let (Some(a), Some(_)) = (existing.get(from).copied(), existing.get(to)) else { return Err("there is no such instance".into()) };
        if from == to {
            return Ok(());
        }
        // The data groups move the same way (paths are rewritten through a permutation).
        let paths: Vec<Option<DataPath>> = existing.iter().map(|e| self.inst(*e).and_then(|i| i.data.clone())).collect();
        if let (Some(Some(pa)), Some(Some(pb))) = (paths.get(from), paths.get(to)) {
            let data = merge::Data { tree: &self.xdp.tree, root: Some(self.data_root) };
            if let (Some(na), Some(nb)) = (data.find(pa), data.find(pb))
                && let Some(dp) = self.xdp.tree.parent(na)
            {
                let tree = &mut self.xdp.tree;
                let kids = tree.children(dp).to_vec();
                let target = kids.iter().position(|c| *c == nb).unwrap_or(kids.len());
                let kind = tree.node(na).map(|x| x.kind.clone());
                if let Some(kind) = kind {
                    // Rebuild the moved group at its new place.
                    let at = if from < to { target + 1 } else { target };
                    let fresh = tree.insert(dp, at, kind);
                    for c in tree.children(na).to_vec() {
                        tree.copy_within(c, fresh);
                    }
                    tree.detach(na);
                }
                self.data_changed = true;
                // Remap indexes: from → to, the ones between shift by one.
                let (Some(((nm, _), prefix)), Some(ia), Some(ib)) = (pa.split_last(), pa.last().map(|s| s.1), pb.last().map(|s| s.1)) else {
                    return Ok(());
                };
                let (prefix, nm) = (prefix.to_vec(), nm.clone());
                let depth = prefix.len();
                for form in &mut self.forms {
                    for inst in &mut form.insts {
                        let Some(p) = inst.data.as_mut() else { continue };
                        if p.len() <= depth || p.get(..depth) != Some(prefix.as_slice()) {
                            continue;
                        }
                        if let Some(step) = p.get_mut(depth)
                            && step.0 == nm
                        {
                            let j = step.1;
                            step.1 = if j == ia {
                                ib
                            } else if ia < ib && j > ia && j <= ib {
                                j - 1
                            } else if ib < ia && j >= ib && j < ia {
                                j + 1
                            } else {
                                j
                            };
                        }
                    }
                }
            }
        }
        let holder = self.parent(a).unwrap_or(parent);
        if let Some(p) = self.forms.get_mut(holder.form).and_then(|f| f.insts.get_mut(holder.inst)) {
            let positions: Vec<usize> = existing.iter().filter_map(|e| p.children.iter().position(|c| *c == e.inst)).collect();
            let mut order: Vec<usize> = existing.iter().map(|e| e.inst).collect();
            let moved = order.remove(from);
            order.insert(to.min(order.len()), moved);
            for (slot, inst) in positions.iter().zip(order) {
                if let Some(c) = p.children.get_mut(*slot) {
                    *c = inst;
                }
            }
        }
        self.renumber(holder);
        self.layout_changed = true;
        Ok(())
    }

    /// Rename the subtree under `n` (SOM indexes) after its children changed.
    fn renumber(&mut self, n: N) {
        self.reindex(n.form);
        let prefix = self.inst(n).map(|i| i.som.clone()).unwrap_or_default();
        if let Some(form) = self.forms.get_mut(n.form) {
            merge::name_tree(form, n.inst, &prefix, 0);
        }
    }

    // ── fields ⇄ AcroForm ───────────────────────────────────────────────────────────────────

    /// The AcroForm field of every field instance in the body, by normalized SOM.
    pub fn field_map(&self) -> HashMap<String, N> {
        self.walk(0)
            .into_iter()
            .filter(|n| self.inst(*n).is_some_and(|i| matches!(i.tag, Tag::Field | Tag::ExclGroup) && !i.som.is_empty()))
            .filter_map(|n| self.inst(n).map(|i| (normalize(&i.som), n)))
            .collect()
    }

    /// A field's canonical value from its AcroForm field (`None`: not representable).
    pub fn value_from_acro(&self, n: N, f: &printcraft_forms::Field) -> Option<String> {
        let inst = self.inst(n)?;
        let tree = self.tree();
        let form = self.forms.get(n.form)?;
        Some(match (inst.tag, f.kind) {
            (Tag::ExclGroup, _) => crate::radio_value(tree, form, inst, f),
            (_, FieldKind::CheckBox) => {
                let (on, off) = merge::check_values(tree, inst.t);
                if f.value.first().is_some_and(|v| !v.is_empty() && v != "Off") { on } else { off }
            }
            (_, FieldKind::Text) => {
                let v = f.value.first().cloned().unwrap_or_default();
                match realize::date_display(tree, inst.t) {
                    Some(p) if !v.is_empty() => match picture::parse(&v, &p) {
                        // Stored as the data picture says (`<bind><picture>`), else canonical.
                        Some(iso) => crate::data_date_pattern(tree, inst.t).and_then(|dp| picture::format(&iso, &dp)).unwrap_or(iso),
                        None => v,
                    },
                    _ => v,
                }
            }
            (_, FieldKind::Combo | FieldKind::List) => f.value.join("\n"),
            _ => return None,
        })
    }

    /// The AcroForm value showing a field's raw value.
    pub fn acro_value(&self, n: N, kind: FieldKind) -> Option<printcraft_forms::FieldValue> {
        use printcraft_forms::FieldValue;
        let inst = self.inst(n)?;
        let tree = self.tree();
        Some(match (inst.tag, kind) {
            (Tag::ExclGroup, _) => {
                let on = inst
                    .children
                    .iter()
                    .filter_map(|m| self.forms.get(n.form)?.get(*m))
                    .map(|m| merge::check_values(tree, m.t).0)
                    .find(|on| *on == inst.value);
                FieldValue::Radio(on.map(|v| realize::on_name(&v)))
            }
            (_, FieldKind::CheckBox) => FieldValue::Check(!inst.value.is_empty() && inst.value == merge::check_values(tree, inst.t).0),
            (_, FieldKind::Text) => {
                let mut v = inst.value.clone();
                if let Some(p) = realize::date_display(tree, inst.t)
                    && let Some(shown) = picture::format(&v, &p)
                {
                    v = shown;
                }
                FieldValue::Text(v)
            }
            (_, FieldKind::Combo | FieldKind::List) => {
                FieldValue::Choice(inst.value.split('\n').filter(|v| !v.is_empty()).map(str::to_string).collect())
            }
            _ => return None,
        })
    }

    /// Take the AcroForm values into the form (and the data). Returns the fields whose value
    /// differs from what the form held, with their previous value.
    pub fn read_acroform(&mut self, doc: &Document) -> Vec<(N, Option<String>)> {
        let map = self.field_map();
        let mut out = Vec::new();
        for f in printcraft_forms::fields(doc) {
            let Some(n) = map.get(&normalize(&f.name)).copied() else { continue };
            let Some(v) = self.value_from_acro(n, &f) else { continue };
            let before = self.raw(n);
            let unchanged = match &before {
                Some(old) => *old == v || crate::same_date(old, &v),
                None => v.is_empty() || self.inst(n).is_some_and(|i| crate::is_off_value(self.tree(), i, &v)),
            };
            if !unchanged {
                // Keep the spelling the data already uses for dates.
                let v = match &before {
                    Some(old) if old.len() == 8 && picture::parse_canonical(old).is_some() && picture::parse_canonical(&v).is_some() => {
                        v.replace('-', "")
                    }
                    _ => v,
                };
                self.set_raw(n, Some(&v));
                out.push((n, before));
            }
        }
        out
    }

    /// Push changed values (and nothing else) into the AcroForm fields, read-only ones
    /// included (calculated fields usually are).
    pub fn write_values(&self, doc: &mut Document) -> Result<usize, XfaError> {
        use printcraft_forms::FieldValue;
        let map = self.field_map();
        let mut changes = Vec::new();
        for f in printcraft_forms::fields(doc) {
            let Some(n) = map.get(&normalize(&f.name)).copied() else { continue };
            let Some(v) = self.acro_value(n, f.kind) else { continue };
            let current = self.value_from_acro(n, &f);
            let raw = self.raw(n).unwrap_or_default();
            if current.as_deref() == Some(raw.as_str()) || current.as_deref().is_some_and(|c| crate::same_date(c, &raw)) {
                continue;
            }
            let value = match v {
                FieldValue::Text(t) => vec![t],
                FieldValue::Check(true) => vec!["On".to_string()],
                FieldValue::Check(false) | FieldValue::Radio(None) => Vec::new(),
                FieldValue::Radio(Some(s)) => vec![s],
                FieldValue::Choice(c) => c,
            };
            changes.push(printcraft_forms::FieldChange { name: f.name.clone(), value: Some(value), read_only: None, required: None, display: None });
        }
        if !changes.is_empty() {
            printcraft_forms::apply_script_changes(doc, &changes, &mut printcraft_forms::NoScripts).map_err(|e| XfaError::Pdf(e.to_string()))?;
        }
        Ok(changes.len())
    }

    // ── saving ──────────────────────────────────────────────────────────────────────────────

    /// The script changes, one per line: SOM, path, key, value.
    pub fn state_text(&self) -> String {
        let mut out = String::new();
        for n in self.walk(0) {
            let Some(inst) = self.inst(n) else { continue };
            for op in &inst.ops {
                let key = match &op.key {
                    Key::Attr(a) => format!("@{a}"),
                    Key::Text => "#text".to_string(),
                    Key::Items => "#items".to_string(),
                };
                out.push_str(&format!("{}\t{}\t{}\t{}\n", esc(&normalize(&inst.som)), esc(&op.path.join(".")), esc(&key), esc(&op.value)));
            }
        }
        out
    }

    /// Apply saved script changes (unknown instances are skipped).
    pub fn apply_state(&mut self, text: &str) {
        let map: HashMap<String, N> = self.walk(0).into_iter().filter_map(|n| self.inst(n).map(|i| (normalize(&i.som), n))).collect();
        for line in text.lines().take(MAX_OPS) {
            let parts: Vec<&str> = line.split('\t').collect();
            let [som, path, key, value] = parts.as_slice() else { continue };
            let Some(n) = map.get(&unesc(som)).copied() else { continue };
            let key = unesc(key);
            let key = match key.as_str() {
                "#text" => Key::Text,
                "#items" => Key::Items,
                a => match a.strip_prefix('@') {
                    Some(a) => Key::Attr(a.to_string()),
                    None => continue,
                },
            };
            let path: Vec<String> = unesc(path).split('.').filter(|s| !s.is_empty()).map(str::to_string).collect();
            self.set_op(n, Op { path, key, value: unesc(value) });
        }
        // Saved state isn't news.
        self.layout_changed = false;
    }

    /// Write the data (when it changed) and the script changes into the document.
    pub fn save(&mut self, doc: &mut Document) -> Result<(), XfaError> {
        if self.data_changed {
            packet::write_datasets(doc, &self.xdp)?;
            crate::drop_usage_rights(doc);
            self.data_changed = false;
        }
        let state = self.state_text();
        if state != read_state(doc).unwrap_or_default() {
            write_state(doc, &state)?;
        }
        Ok(())
    }
}

/// Where PrintCraft keeps script changes: the catalog's page-piece dictionary (ISO 32000-2
/// §14.5), which other applications ignore.
const PIECE: &[u8] = b"PrintCraft";

pub fn read_state(doc: &Document) -> Option<String> {
    let root = doc.root()?;
    let cat = doc.get(root);
    let pi = doc.resolve(cat.as_dict()?.get(b"PieceInfo")?);
    let app = doc.resolve(pi.as_dict()?.get(PIECE)?);
    let private = doc.resolve(app.as_dict()?.get(b"Private")?);
    let s = doc.resolve(private.as_dict()?.get(b"XFAState")?);
    match &*s {
        Object::String(s) => Some(s.to_text()),
        Object::Stream(st) => st.decoded_within(64 << 20).ok().map(|b| String::from_utf8_lossy(&b).into_owned()),
        _ => None,
    }
}

fn write_state(doc: &mut Document, text: &str) -> Result<(), XfaError> {
    let root = doc.root().ok_or_else(|| XfaError::Pdf("no document catalog".into()))?;
    let cat = doc.get(root).as_dict().cloned().unwrap_or_default();
    let mut pi = cat.get(b"PieceInfo").map(|p| doc.resolve(p)).and_then(|p| p.as_dict().cloned()).unwrap_or_default();
    let mut app = pi.get(PIECE).map(|p| doc.resolve(p)).and_then(|p| p.as_dict().cloned()).unwrap_or_default();
    let mut private = app.get(b"Private").map(|p| doc.resolve(p)).and_then(|p| p.as_dict().cloned()).unwrap_or_default();
    if text.is_empty() {
        private.remove(b"XFAState");
    } else {
        let r = doc.add(Object::Stream(printcraft_cos::Stream::flate(Dict::new(), text.as_bytes())));
        private.set(b"XFAState".to_vec(), Object::Ref(r));
    }
    // LastModified is required; the Info dictionary's date is the best we know here.
    let modified = doc.trailer().get(b"Info").map(|i| doc.resolve(i)).and_then(|i| i.as_dict().and_then(|d| d.get(b"ModDate").cloned()));
    app.set(b"LastModified".to_vec(), modified.unwrap_or_else(|| Object::String(PdfString::literal(b"D:20000101000000Z".to_vec()))));
    app.set(b"Private".to_vec(), Object::Dict(private));
    pi.set(PIECE.to_vec(), Object::Dict(app));
    doc.update_dict(root, |c| c.set(b"PieceInfo".to_vec(), Object::Dict(pi))).map_err(|e| XfaError::Pdf(e.to_string()))
}

/// Local name of an element, for matching SOM steps against data nodes.
pub fn data_name(tree: &Tree, n: NodeId) -> &str {
    local(tree.name(n).unwrap_or(""))
}
