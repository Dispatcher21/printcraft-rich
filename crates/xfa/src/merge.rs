//! Data merge (XFA 3.3 Part 1 §4 "Basic Data Binding"): pair template containers with data
//! groups and values, repeat subforms per `<occur>`, and give every node its SOM name.
//!
//! Supported: normal (`once`) binding by name with index-ordered matching, `none`, `global`, and
//! simple `dataRef` paths (`$.a.b`, `$record.a`, `$data.a`, `a[2]`). Not supported: data
//! descriptions, explicit data references with predicates, and scripts.

use std::collections::HashMap;

use crate::template::{self, Presence};
use crate::xml::{NodeId, Tree, local};

/// Most instances of one repeating subform.
const MAX_REPEAT: usize = 2_000;
/// Most instances in the form.
const MAX_INSTANCES: usize = 200_000;
const MAX_DEPTH: usize = 128;

/// A step of a data path: element name and index among same-named siblings.
pub type DataPath = Vec<(String, usize)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    Subform,
    Field,
    Draw,
    ExclGroup,
    Area,
}

#[derive(Clone, Debug)]
pub struct Inst {
    /// Template element.
    pub t: NodeId,
    pub tag: Tag,
    pub name: String,
    /// Full SOM expression, e.g. `form1[0].Page1[0].Name[0]`.
    pub som: String,
    /// Bound data node (path from the data root's parent, i.e. starting with the root group).
    pub data: Option<DataPath>,
    /// Field and exclusion group values, canonical (as the data holds them).
    pub value: String,
    pub children: Vec<usize>,
    pub presence: Presence,
}

/// The merged form: an arena of instances; index 0 is the root subform.
#[derive(Clone, Debug, Default)]
pub struct Form {
    pub insts: Vec<Inst>,
}

impl Form {
    pub fn get(&self, i: usize) -> Option<&Inst> {
        self.insts.get(i)
    }
}

/// The data DOM view for binding: the `<xfa:data>` element.
pub struct Data<'a> {
    pub tree: &'a Tree,
    pub root: Option<NodeId>,
}

impl Data<'_> {
    /// The node at `path` (from `<xfa:data>`).
    pub fn find(&self, path: &[(String, usize)]) -> Option<NodeId> {
        let mut at = self.root?;
        for (name, idx) in path {
            at = self.tree.elements(at).filter(|c| local(self.tree.name(*c).unwrap_or("")) == name).nth(*idx)?;
        }
        Some(at)
    }

    fn count(&self, path: &[(String, usize)], name: &str) -> usize {
        self.find(path).map(|n| self.tree.elements(n).filter(|c| self.tree.tag(*c) == name).count()).unwrap_or(0)
    }

    /// First descendant named `name` anywhere in the data (global binding).
    fn global(&self, name: &str) -> Option<DataPath> {
        let root = self.root?;
        let hit = self.tree.descendants(root).into_iter().find(|n| self.tree.tag(*n) == name)?;
        Some(self.path_of(hit))
    }

    fn path_of(&self, mut n: NodeId) -> DataPath {
        let mut out = Vec::new();
        while Some(n) != self.root {
            let Some(p) = self.tree.parent(n) else { break };
            let tag = self.tree.tag(n).to_string();
            let idx = self.tree.elements(p).filter(|c| self.tree.tag(*c) == tag).position(|c| c == n).unwrap_or(0);
            out.push((tag, idx));
            n = p;
            if out.len() > MAX_DEPTH {
                break;
            }
        }
        out.reverse();
        out
    }
}

/// The value a data node holds (multiple `<value>` children of a multi-select list join with
/// newlines).
pub fn data_value(tree: &Tree, n: NodeId) -> String {
    let kids: Vec<NodeId> = tree.elements(n).collect();
    if !kids.is_empty() && kids.iter().all(|k| tree.tag(*k) == "value") {
        return kids.iter().map(|k| tree.text(*k)).collect::<Vec<_>>().join("\n");
    }
    tree.text(n)
}

struct Ctx {
    path: DataPath,
    /// How many data children of each name earlier siblings bound.
    consumed: HashMap<String, usize>,
}

struct Merger<'a> {
    t: &'a Tree,
    data: Data<'a>,
    form: Form,
}

fn occur(t: &Tree, n: NodeId) -> (usize, usize, usize) {
    let Some(o) = t.child(n, "occur") else { return (1, 1, 1) };
    let num = |k: &str| t.attr(o, k).and_then(|v| v.trim().parse::<i64>().ok());
    let min = num("min").unwrap_or(1).clamp(0, MAX_REPEAT as i64) as usize;
    let max = match num("max") {
        Some(m) if m < 0 => MAX_REPEAT,
        Some(m) => (m as usize).clamp(min.max(1), MAX_REPEAT),
        None => min.max(1),
    };
    let initial = num("initial").map(|v| v.clamp(0, MAX_REPEAT as i64) as usize).unwrap_or(min).clamp(min, max);
    (min, max, initial)
}

fn bind_of(t: &Tree, n: NodeId) -> (String, Option<String>) {
    match t.child(n, "bind") {
        Some(b) => (t.attr(b, "match").unwrap_or("once").to_string(), t.attr(b, "ref").map(str::to_string)),
        None => ("once".to_string(), None),
    }
}

/// Resolve a simple `dataRef` expression against `ctx` (`None` for unsupported forms).
fn data_ref(ctx: &DataPath, data_root_name: Option<&str>, r: &str) -> Option<(DataPath, bool)> {
    let r = r.trim();
    let (mut path, rest) = if let Some(rest) = r.strip_prefix("$record").or_else(|| r.strip_prefix("$data")) {
        let mut p = DataPath::new();
        if let Some(root) = data_root_name {
            p.push((root.to_string(), 0));
        }
        (p, rest)
    } else if let Some(rest) = r.strip_prefix('$') {
        (ctx.clone(), rest)
    } else if let Some(rest) = r.strip_prefix("!") {
        (DataPath::new(), rest)
    } else {
        (ctx.clone(), r)
    };
    let mut all = false;
    for step in rest.split('.').filter(|s| !s.is_empty()) {
        let (name, idx) = match step.split_once('[') {
            Some((n, i)) => {
                let i = i.trim_end_matches(']').trim();
                if i == "*" {
                    all = true;
                    (n, 0)
                } else {
                    (n, i.parse::<usize>().ok()?)
                }
            }
            None => (step, 0),
        };
        if name.is_empty() || name.contains(['(', ')', '=', '<', '>']) {
            return None;
        }
        path.push((name.to_string(), idx));
        if path.len() > MAX_DEPTH {
            return None;
        }
    }
    Some((path, all))
}

impl Merger<'_> {
    fn push(&mut self, inst: Inst) -> Option<usize> {
        if self.form.insts.len() >= MAX_INSTANCES {
            return None;
        }
        self.form.insts.push(inst);
        Some(self.form.insts.len() - 1)
    }

    fn data_root_name(&self) -> Option<String> {
        self.data.root.and_then(|r| self.data.tree.elements(r).next()).map(|n| self.data.tree.tag(n).to_string())
    }

    fn value_at(&self, path: &DataPath) -> Option<String> {
        self.data.find(path).map(|n| data_value(self.data.tree, n))
    }

    fn default_value(&self, n: NodeId) -> String {
        self.t.child(n, "value").and_then(|v| template::value_text(self.t, v)).unwrap_or_default()
    }

    /// Merge the container children of `parent` into instances under `out`.
    fn children(&mut self, parent: NodeId, ctx: &mut Ctx, out: &mut Vec<usize>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let kids: Vec<NodeId> = self.t.elements(parent).collect();
        for c in kids {
            match self.t.tag(c) {
                "subform" => self.subform(c, ctx, out, depth),
                // A subform set only groups subforms: merge its members in place.
                "subformSet" => self.children(c, ctx, out, depth + 1),
                "area" => {
                    let mut inner = Vec::new();
                    self.children(c, ctx, &mut inner, depth + 1);
                    let inst = Inst {
                        t: c,
                        tag: Tag::Area,
                        name: self.t.attr(c, "name").unwrap_or("").to_string(),
                        som: String::new(),
                        data: None,
                        value: String::new(),
                        children: inner,
                        presence: template::presence(self.t, c),
                    };
                    if let Some(i) = self.push(inst) {
                        out.push(i);
                    }
                }
                "field" | "exclGroup" => self.field(c, ctx, out, depth),
                "draw" => {
                    let inst = Inst {
                        t: c,
                        tag: Tag::Draw,
                        name: self.t.attr(c, "name").unwrap_or("").to_string(),
                        som: String::new(),
                        data: None,
                        value: self.default_value(c),
                        children: Vec::new(),
                        presence: template::presence(self.t, c),
                    };
                    if let Some(i) = self.push(inst) {
                        out.push(i);
                    }
                }
                _ => {}
            }
        }
    }

    fn subform(&mut self, c: NodeId, ctx: &mut Ctx, out: &mut Vec<usize>, depth: usize) {
        let name = self.t.attr(c, "name").unwrap_or("").to_string();
        let (min, max, initial) = occur(self.t, c);
        let (mode, r) = bind_of(self.t, c);
        // (data path or None for transparent, how many instances)
        let mut plans: Vec<Option<DataPath>> = Vec::new();
        match mode.as_str() {
            "none" => plans.extend((0..initial).map(|_| None)),
            "dataRef" => match r.as_deref().and_then(|r| data_ref(&ctx.path, self.data_root_name().as_deref(), r)) {
                Some((p, true)) => {
                    let (last, parent) = match p.split_last() {
                        Some(((n, _), rest)) => (n.clone(), rest.to_vec()),
                        None => (String::new(), Vec::new()),
                    };
                    let n = self.data.count(&parent, &last).clamp(min, max);
                    plans.extend((0..n).map(|k| {
                        let mut q = parent.clone();
                        q.push((last.clone(), k));
                        Some(q)
                    }));
                }
                Some((p, false)) => plans.push(Some(p)),
                None => plans.extend((0..initial).map(|_| None)),
            },
            _ if name.is_empty() => plans.extend((0..initial).map(|_| None)),
            _ => {
                let start = ctx.consumed.get(&name).copied().unwrap_or(0);
                let available = self.data.count(&ctx.path, &name).saturating_sub(start);
                let n = if self.data.find(&ctx.path).is_some() { available.max(initial).clamp(min, max) } else { initial };
                // A subform that occurs once binds to the next unused group; repeats bind in order.
                for k in 0..n {
                    let mut p = ctx.path.clone();
                    p.push((name.clone(), start + k));
                    plans.push(Some(p));
                }
                ctx.consumed.insert(name.clone(), start + n);
            }
        }
        for plan in plans {
            let mut inner = Vec::new();
            match &plan {
                Some(p) => {
                    let mut sub = Ctx { path: p.clone(), consumed: HashMap::new() };
                    self.children(c, &mut sub, &mut inner, depth + 1);
                }
                // Unbound and unnamed subforms are transparent: their content binds in the
                // enclosing data group.
                None => self.children(c, ctx, &mut inner, depth + 1),
            }
            let inst = Inst {
                t: c,
                tag: Tag::Subform,
                name: name.clone(),
                som: String::new(),
                data: plan,
                value: String::new(),
                children: inner,
                presence: template::presence(self.t, c),
            };
            match self.push(inst) {
                Some(i) => out.push(i),
                None => return,
            }
        }
    }

    fn field(&mut self, c: NodeId, ctx: &mut Ctx, out: &mut Vec<usize>, depth: usize) {
        let name = self.t.attr(c, "name").unwrap_or("").to_string();
        let excl = self.t.tag(c) == "exclGroup";
        let (mode, r) = bind_of(self.t, c);
        let path = match mode.as_str() {
            "none" => None,
            _ if name.is_empty() && mode != "dataRef" => None,
            "global" => self.data.global(&name).or_else(|| {
                let mut p = ctx.path.clone();
                p.push((name.clone(), 0));
                Some(p)
            }),
            "dataRef" => r.as_deref().and_then(|r| data_ref(&ctx.path, self.data_root_name().as_deref(), r)).map(|(p, _)| p),
            _ => {
                let k = ctx.consumed.entry(name.clone()).or_insert(0);
                let mut p = ctx.path.clone();
                p.push((name.clone(), *k));
                *k += 1;
                Some(p)
            }
        };
        let mut value = path.as_ref().and_then(|p| self.value_at(p));
        let mut children = Vec::new();
        if excl {
            // Members are check buttons with no binding of their own; the group holds the value.
            let mut none = Ctx { path: Vec::new(), consumed: HashMap::new() };
            for m in self.t.elements(c).filter(|m| self.t.tag(*m) == "field").collect::<Vec<_>>() {
                let inst = Inst {
                    t: m,
                    tag: Tag::Field,
                    name: self.t.attr(m, "name").unwrap_or("").to_string(),
                    som: String::new(),
                    data: None,
                    value: self.default_value(m),
                    children: Vec::new(),
                    presence: template::presence(self.t, m),
                };
                if let Some(i) = self.push(inst) {
                    children.push(i);
                }
            }
            let _ = &mut none;
            if value.is_none() {
                // The group's initial value: the on-value of the member whose default is on.
                value = children.iter().filter_map(|i| self.form.insts.get(*i)).find_map(|m| {
                    let (on, _) = check_values(self.t, m.t);
                    (!m.value.is_empty() && m.value == on).then_some(on)
                });
            }
        }
        let _ = depth;
        let inst = Inst {
            t: c,
            tag: if excl { Tag::ExclGroup } else { Tag::Field },
            name,
            som: String::new(),
            data: path,
            value: value.unwrap_or_else(|| self.default_value(c)),
            children,
            presence: template::presence(self.t, c),
        };
        if let Some(i) = self.push(inst) {
            out.push(i);
        }
    }
}

/// A check button's on and off values (`<items>`: first on, second off; defaults "1" and "0").
pub fn check_values(t: &Tree, field: NodeId) -> (String, String) {
    let items: Vec<String> = t.child(field, "items").map(|i| t.elements(i).map(|e| t.text(e)).collect()).unwrap_or_default();
    let on = items.first().cloned().unwrap_or_else(|| "1".into());
    let off = items.get(1).cloned().unwrap_or_else(|| if items.is_empty() { "0".into() } else { String::new() });
    (on, off)
}

/// Assign SOM names: `name[i]` with `i` counting same-named siblings; unnamed subforms are
/// `#subform[i]`, unnamed fields `#field[i]`. Areas don't appear in SOM paths.
fn name_tree(form: &mut Form, i: usize, prefix: &str, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let kids = form.insts.get(i).map(|n| n.children.clone()).unwrap_or_default();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for k in flatten_areas(form, &kids, 0) {
        let Some(inst) = form.insts.get(k) else { continue };
        let key = match (inst.tag, inst.name.is_empty()) {
            (_, false) => inst.name.clone(),
            (Tag::Subform, true) => "#subform".into(),
            (Tag::Field, true) => "#field".into(),
            (Tag::ExclGroup, true) => "#exclGroup".into(),
            (Tag::Draw, true) => "#draw".into(),
            (Tag::Area, true) => "#area".into(),
        };
        let n = counts.entry(key.clone()).or_insert(0);
        let som = if prefix.is_empty() { format!("{key}[{n}]") } else { format!("{prefix}.{key}[{n}]") };
        *n += 1;
        if let Some(inst) = form.insts.get_mut(k) {
            inst.som = som.clone();
        }
        name_tree(form, k, &som, depth + 1);
    }
}

fn flatten_areas(form: &Form, kids: &[usize], depth: usize) -> Vec<usize> {
    let mut out = Vec::new();
    for k in kids {
        match form.insts.get(*k) {
            Some(inst) if inst.tag == Tag::Area && depth < MAX_DEPTH => out.extend(flatten_areas(form, &inst.children, depth + 1)),
            Some(_) => out.push(*k),
            None => {}
        }
    }
    out
}

/// Merge the template's root subform with the data. The returned form's instance 0 is the root.
pub fn merge(t: &Tree, root_subform: NodeId, data_tree: &Tree, data_root: Option<NodeId>) -> Form {
    let mut m = Merger { t, data: Data { tree: data_tree, root: data_root }, form: Form::default() };
    let name = t.attr(root_subform, "name").unwrap_or("").to_string();
    // The root subform binds to the data root group (whatever its name).
    let root_group = data_root.and_then(|r| data_tree.elements(r).next()).map(|g| data_tree.tag(g).to_string());
    let path: DataPath = match &root_group {
        Some(g) => vec![(g.clone(), 0)],
        None => vec![(if name.is_empty() { "form1".to_string() } else { name.clone() }, 0)],
    };
    m.form.insts.push(Inst {
        t: root_subform,
        tag: Tag::Subform,
        name: name.clone(),
        som: format!("{}[0]", if name.is_empty() { "#subform" } else { &name }),
        data: Some(path.clone()),
        value: String::new(),
        children: Vec::new(),
        presence: Presence::Visible,
    });
    let mut ctx = Ctx { path, consumed: HashMap::new() };
    let mut kids = Vec::new();
    m.children(root_subform, &mut ctx, &mut kids, 1);
    if let Some(r) = m.form.insts.get_mut(0) {
        r.children = kids;
    }
    let root_som = m.form.insts.first().map(|r| r.som.clone()).unwrap_or_default();
    name_tree(&mut m.form, 0, &root_som, 0);
    m.form
}

/// Merge the content of a page area (master page) for one page: fields there bind globally by
/// name, as Acrobat does for page headers and footers.
pub fn merge_page_area(t: &Tree, page_area: NodeId, data_tree: &Tree, data_root: Option<NodeId>, som: &str) -> Form {
    let mut m = Merger { t, data: Data { tree: data_tree, root: data_root }, form: Form::default() };
    m.form.insts.push(Inst {
        t: page_area,
        tag: Tag::Area,
        name: String::new(),
        som: som.to_string(),
        data: None,
        value: String::new(),
        children: Vec::new(),
        presence: Presence::Visible,
    });
    let mut ctx = Ctx { path: Vec::new(), consumed: HashMap::new() };
    let mut kids = Vec::new();
    m.children(page_area, &mut ctx, &mut kids, 1);
    // Without a data context, normal binding would invent paths: drop them.
    for i in kids.clone() {
        clear_unbound(&mut m.form, i, 0);
    }
    if let Some(r) = m.form.insts.get_mut(0) {
        r.children = kids;
    }
    name_tree(&mut m.form, 0, som, 0);
    m.form
}

fn clear_unbound(form: &mut Form, i: usize, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let kids = match form.insts.get_mut(i) {
        Some(inst) => {
            if inst.data.as_ref().is_some_and(|p| p.len() <= 1) {
                inst.data = None;
            }
            inst.children.clone()
        }
        None => return,
    };
    for k in kids {
        clear_unbound(form, k, depth + 1);
    }
}

/// The node at `path` under `<xfa:data>`, created (with any missing ancestors) if needed.
pub fn ensure_path(tree: &mut Tree, data_root: NodeId, path: &[(String, usize)]) -> NodeId {
    let mut at = data_root;
    for (name, idx) in path {
        let existing: Vec<NodeId> = tree.elements(at).filter(|c| tree.tag(*c) == name).collect();
        at = match existing.get(*idx) {
            Some(n) => *n,
            None => {
                let mut last = existing.last().copied();
                for _ in existing.len()..=*idx {
                    // Keep same-named siblings together, after the last of them.
                    let pos = last.and_then(|l| tree.children(at).iter().position(|c| *c == l)).map(|p| p + 1).unwrap_or(tree.children(at).len());
                    last = Some(tree.insert(at, pos, crate::xml::Kind::Element { name: name.clone(), attrs: Vec::new() }));
                }
                last.unwrap_or(at)
            }
        };
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Tree {
        Tree::parse(s).unwrap_or_default()
    }

    #[test]
    fn binds_by_name_and_repeats() {
        let t = parse(
            r#"<template><subform name="form1"><subform name="Page1"><field name="Name"/><field name="Name"/>
               <subform name="Row"><occur min="1" max="-1"/><field name="Item"/></subform><subform><field name="Loose"/></subform></subform></subform></template>"#,
        );
        let d = parse(
            r#"<xfa:data><form1><Page1><Name>Ann</Name><Name>Bob</Name><Row><Item>a</Item></Row><Row><Item>b</Item></Row><Row><Item>c</Item></Row><Loose>x</Loose></Page1></form1></xfa:data>"#,
        );
        let root = t.path(Tree::ROOT, &["template", "subform"]).unwrap_or(0);
        let data = d.child(Tree::ROOT, "data");
        let f = merge(&t, root, &d, data);
        let by_som: HashMap<&str, &str> = f.insts.iter().map(|i| (i.som.as_str(), i.value.as_str())).collect();
        assert_eq!(by_som.get("form1[0].Page1[0].Name[0]"), Some(&"Ann"));
        assert_eq!(by_som.get("form1[0].Page1[0].Name[1]"), Some(&"Bob"));
        assert_eq!(by_som.get("form1[0].Page1[0].Row[2].Item[0]"), Some(&"c"));
        assert_eq!(by_som.get("form1[0].Page1[0].#subform[0].Loose[0]"), Some(&"x"));
    }

    #[test]
    fn creates_missing_data() {
        let mut d = parse("<xfa:data/>");
        let root = d.child(Tree::ROOT, "data").unwrap_or(0);
        let n = ensure_path(&mut d, root, &[("form1".into(), 0), ("Row".into(), 2)]);
        d.set_text(n, "v");
        assert_eq!(d.to_string_of(root), "<xfa:data><form1><Row/><Row/><Row>v</Row></form1></xfa:data>");
    }

    #[test]
    fn data_refs() {
        let ctx = vec![("form1".to_string(), 0)];
        assert_eq!(data_ref(&ctx, Some("form1"), "$.a.b[2]"), Some((vec![("form1".into(), 0), ("a".into(), 0), ("b".into(), 2)], false)));
        assert_eq!(data_ref(&ctx, Some("form1"), "$record.x[*]").map(|p| p.1), Some(true));
        assert_eq!(data_ref(&ctx, Some("form1"), "$.a[x=1]"), None);
    }
}
