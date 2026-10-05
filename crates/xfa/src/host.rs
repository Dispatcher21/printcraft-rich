//! The scripting object model (XFA 3.3 Part 1 "Scripting Object Model" and the *LiveCycle
//! Designer Scripting Reference*'s public object list), shared by JavaScript and FormCalc.
//!
//! Scripts reach form objects ([`Obj`]) by SOM expressions and unqualified names, and read,
//! write and call their properties and methods through [`Host::get`], [`Host::set`] and
//! [`Host::call`]. Both languages convert their own values to and from [`Val`].

use std::collections::HashMap;

use crate::dom::{self, Live, N, PROPERTY_ELEMENTS, is_content_element};
use crate::merge::{self, Key, Op, Tag};
use crate::template::Presence;
use crate::xml::{NodeId, Tree};
use crate::{picture, realize};

const MAX_DEPTH: usize = 128;
/// Most objects a SOM expression yields.
const MAX_RESULTS: usize = 100_000;

/// A scripting object.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Obj {
    /// A form container (subform, field, exclusion group, draw, area).
    Node(N),
    /// A property element below a container's template node (`border.fill.color`).
    Prop(N, Vec<String>),
    /// The instance manager of template subform `.1` under container `.0`.
    Manager(N, NodeId),
    /// A data group or value.
    Data(NodeId),
    /// A `<variables><script name>` script object (JavaScript only), declared on `.0`.
    Script(N, NodeId),
    Host,
    Form,
    Layout,
    Event,
    Xfa,
    List(Vec<Obj>),
}

/// A script value.
#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Null,
    Num(f64),
    Str(String),
    Bool(bool),
    Obj(Obj),
}

impl Val {
    pub fn text(&self) -> String {
        match self {
            Val::Null => String::new(),
            Val::Num(n) => dom::number_text(*n),
            Val::Str(s) => s.clone(),
            Val::Bool(b) => if *b { "1" } else { "0" }.into(),
            Val::Obj(_) => String::new(),
        }
    }

    pub fn number(&self) -> f64 {
        match self {
            Val::Null | Val::Obj(_) => 0.0,
            Val::Num(n) => *n,
            Val::Bool(b) => f64::from(u8::from(*b)),
            Val::Str(s) => s.trim().parse::<f64>().ok().filter(|v| v.is_finite()).unwrap_or(0.0),
        }
    }
}

/// The event being handled (`xfa.event`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventInfo {
    pub name: String,
    pub target: Option<N>,
    pub change: String,
    pub new_text: String,
    pub prev_text: String,
}

/// What scripts asked of the viewer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Output {
    /// `xfa.host.messageBox` and `app.alert` messages.
    pub messages: Vec<String>,
    /// `console.println`.
    pub console: Vec<String>,
    /// Script errors, with the script's container.
    pub errors: Vec<String>,
    /// Failed validations: (field SOM, message).
    pub invalid: Vec<(String, String)>,
    /// `xfa.host.print` calls.
    pub prints: usize,
    /// `xfa.host.gotoURL` addresses.
    pub urls: Vec<String>,
    /// The field `setFocus` asked for (SOM).
    pub focus: Option<String>,
}

impl Output {
    pub fn absorb(&mut self, o: Output) {
        self.messages.extend(o.messages);
        self.console.extend(o.console);
        self.errors.extend(o.errors);
        self.invalid.extend(o.invalid);
        self.prints += o.prints;
        self.urls.extend(o.urls);
        if o.focus.is_some() {
            self.focus = o.focus;
        }
    }
}

/// Work a script asked for, done after it returns.
#[derive(Clone, Debug, PartialEq)]
pub enum Pending {
    /// Run `initialize` on a new instance and its descendants.
    Initialize(N),
    /// `execEvent(activity)`.
    Event(N, String),
    /// `execCalculate` / `recalculate`.
    Calculate,
}

/// Everything a running script can reach.
pub struct Host {
    pub live: Live,
    pub out: Output,
    pub event: EventInfo,
    /// 0-based page of laid-out instances (master page fields included).
    pub page_of: HashMap<N, usize>,
    pub page_count: usize,
    pub pending: Vec<Pending>,
    /// Today's date, for `Date()` and friends.
    pub today: (i32, u32, u32),
    pub calculations: bool,
}

fn class_name(tag: Tag) -> &'static str {
    match tag {
        Tag::Subform => "subform",
        Tag::Field => "field",
        Tag::Draw => "draw",
        Tag::ExclGroup => "exclGroup",
        Tag::Area => "area",
    }
}

/// A SOM index.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Idx {
    Default,
    Num(usize),
    All,
}

/// Split a SOM expression into steps; an empty step means "search descendants" (`..`).
fn steps(som: &str) -> Vec<(String, Idx)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    for c in som.trim().chars() {
        match c {
            '[' => {
                depth += 1;
                cur.push(c);
            }
            ']' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            '.' if depth == 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
        if out.len() > MAX_DEPTH {
            break;
        }
    }
    out.push(cur);
    out.into_iter()
        .map(|s| match s.split_once('[') {
            Some((name, rest)) => {
                let inner = rest.trim_end_matches(']').trim();
                let idx = match inner {
                    "*" => Idx::All,
                    // Relative indexes and predicates: the first match is the best guess.
                    i => i.parse::<usize>().map(Idx::Num).unwrap_or(Idx::Num(0)),
                };
                (name.trim().to_string(), idx)
            }
            None => (s.trim().to_string(), Idx::Default),
        })
        .collect()
}

fn pick(list: Vec<N>, idx: Idx) -> Vec<Obj> {
    match idx {
        Idx::Default => list.into_iter().take(1).map(Obj::Node).collect(),
        Idx::Num(i) => list.into_iter().nth(i).map(Obj::Node).into_iter().collect(),
        Idx::All => list.into_iter().map(Obj::Node).collect(),
    }
}

impl Host {
    pub fn new(live: Live, today: (i32, u32, u32)) -> Host {
        Host {
            live,
            out: Output::default(),
            event: EventInfo::default(),
            page_of: HashMap::new(),
            page_count: 1,
            pending: Vec::new(),
            today,
            calculations: true,
        }
    }

    fn tree(&self) -> &Tree {
        self.live.tree()
    }

    fn record(&self) -> Option<NodeId> {
        self.tree().elements(self.live.data_root).next()
    }

    fn datasets(&self) -> Option<NodeId> {
        self.live.xdp.datasets
    }

    // ── name resolution ─────────────────────────────────────────────────────────────────────

    /// An unqualified name, searched from `from` outward (its children, then each ancestor
    /// and its children), as XFA scripts resolve bare identifiers.
    pub fn lookup(&self, from: N, name: &str) -> Option<Obj> {
        if name.is_empty() {
            return None;
        }
        let mut at = Some(from);
        let mut steps = 0;
        while let Some(n) = at {
            if self.live.key(n) == name {
                return Some(Obj::Node(n));
            }
            if let Some(k) = self.live.named(n, name).first() {
                return Some(Obj::Node(*k));
            }
            if let Some(s) = self.live.script_object(n, name) {
                return Some(Obj::Script(n, s));
            }
            if let Some(rest) = name.strip_prefix('_')
                && let Some((p, t)) = self.live.manager(n, rest)
            {
                return Some(Obj::Manager(p, t));
            }
            at = self.live.parent(n);
            steps += 1;
            if steps > MAX_DEPTH {
                break;
            }
        }
        // Master page objects can name body objects too.
        if from.form != 0 { self.lookup(N::ROOT, name) } else { None }
    }

    /// All objects a SOM expression names, relative to `from`.
    pub fn resolve(&self, from: N, som: &str) -> Vec<Obj> {
        let mut st = steps(som).into_iter();
        let Some((first, idx)) = st.next() else { return Vec::new() };
        let mut desc = false;
        let mut cur: Vec<Obj> = match first.as_str() {
            "$" | "this" => vec![Obj::Node(from)],
            "$form" => vec![Obj::Form],
            "$data" => vec![Obj::Data(self.live.data_root)],
            "$record" => self.record().map(Obj::Data).into_iter().collect(),
            "$host" => vec![Obj::Host],
            "$layout" => vec![Obj::Layout],
            "$event" => vec![Obj::Event],
            "xfa" => vec![Obj::Xfa],
            "$template" | "$config" | "$connectionSet" | "$signature" => return Vec::new(),
            "" => {
                desc = true;
                vec![Obj::Node(from)]
            }
            name => match self.lookup(from, name) {
                Some(Obj::Node(k)) => match idx {
                    Idx::Default => vec![Obj::Node(k)],
                    i => pick(self.live.same_named(k), i),
                },
                Some(o) => vec![o],
                None => return Vec::new(),
            },
        };
        for (name, idx) in st {
            if name.is_empty() {
                desc = true;
                continue;
            }
            let mut next = Vec::new();
            for o in &cur {
                next.extend(self.child(o, &name, idx, desc));
                if next.len() > MAX_RESULTS {
                    break;
                }
            }
            desc = false;
            cur = next;
            if cur.is_empty() {
                break;
            }
        }
        cur
    }

    /// The children of `o` that step `name[idx]` selects.
    fn child(&self, o: &Obj, name: &str, idx: Idx, desc: bool) -> Vec<Obj> {
        match o {
            Obj::Node(n) => {
                if desc {
                    let all: Vec<N> = self.live.walk(n.form).into_iter().filter(|k| self.is_below(*k, *n) && self.live.key(*k) == name).collect();
                    return pick(all, idx);
                }
                let found = self.live.named(*n, name);
                if !found.is_empty() {
                    return pick(found, idx);
                }
                if let Some(rest) = name.strip_prefix('_')
                    && let Some((p, t)) = self.live.manager(*n, rest)
                {
                    return vec![Obj::Manager(p, t)];
                }
                if name == "instanceManager"
                    && let Some(m) = self.manager_of(*n)
                {
                    return vec![m];
                }
                if let Some(s) = self.live.script_object(*n, name) {
                    return vec![Obj::Script(*n, s)];
                }
                if PROPERTY_ELEMENTS.contains(&name) {
                    return vec![Obj::Prop(*n, vec![name.to_string()])];
                }
                Vec::new()
            }
            Obj::Form => {
                if self.live.key(N::ROOT) == name {
                    return vec![Obj::Node(N::ROOT)];
                }
                self.child(&Obj::Node(N::ROOT), name, idx, desc)
            }
            Obj::Xfa => match name {
                "form" => vec![Obj::Form],
                "host" => vec![Obj::Host],
                "layout" => vec![Obj::Layout],
                "event" => vec![Obj::Event],
                "datasets" => self.datasets().map(Obj::Data).into_iter().collect(),
                "record" => self.record().map(Obj::Data).into_iter().collect(),
                "data" => vec![Obj::Data(self.live.data_root)],
                _ => Vec::new(),
            },
            Obj::Data(d) => {
                let t = self.tree();
                let all: Vec<NodeId> = if desc {
                    t.descendants(*d).into_iter().filter(|c| dom::data_name(t, *c) == name).collect()
                } else {
                    t.elements(*d).filter(|c| dom::data_name(t, *c) == name).collect()
                };
                match idx {
                    Idx::Default => all.into_iter().take(1).map(Obj::Data).collect(),
                    Idx::Num(i) => all.into_iter().nth(i).map(Obj::Data).into_iter().collect(),
                    Idx::All => all.into_iter().map(Obj::Data).collect(),
                }
            }
            Obj::Prop(n, p) => {
                let mut p = p.clone();
                p.push(name.to_string());
                vec![Obj::Prop(*n, p)]
            }
            _ => Vec::new(),
        }
    }

    /// One SOM step from an object: `name`, `name[i]`, `name[*]`, or `..name` (descendants).
    pub fn step(&self, o: &Obj, step: &str) -> Vec<Obj> {
        let (desc, step) = match step.strip_prefix("..") {
            Some(rest) => (true, rest),
            None => (false, step),
        };
        let parsed = steps(step);
        let Some((name, idx)) = parsed.into_iter().next() else { return Vec::new() };
        self.child(o, &name, idx, desc)
    }

    fn is_below(&self, k: N, n: N) -> bool {
        let mut at = self.live.parent(k);
        for _ in 0..MAX_DEPTH {
            match at {
                Some(p) if p == n => return true,
                Some(p) => at = self.live.parent(p),
                None => return false,
            }
        }
        false
    }

    /// The instance manager of a subform instance.
    fn manager_of(&self, n: N) -> Option<Obj> {
        let inst = self.live.inst(n)?;
        if inst.tag != Tag::Subform {
            return None;
        }
        let t = self.live.orig(inst.t);
        Some(Obj::Manager(self.live.parent(n).unwrap_or(n), t))
    }

    // ── values ──────────────────────────────────────────────────────────────────────────────

    /// A field's raw value as a script sees it: null when empty, numbers for numeric fields.
    pub fn raw_val(&self, n: N) -> Val {
        match self.live.raw(n) {
            None => Val::Null,
            Some(s) if self.live.numeric(n) => s.trim().parse::<f64>().ok().filter(|v| v.is_finite()).map(Val::Num).unwrap_or(Val::Str(s)),
            Some(s) => Val::Str(s),
        }
    }

    /// The value an object stands for in an expression (FormCalc `Field`, `Field + 1`).
    pub fn value_of(&mut self, o: &Obj) -> Val {
        match o {
            Obj::Node(n) => self.raw_val(*n),
            Obj::Prop(..) => self.get(o, "value").ok().flatten().unwrap_or(Val::Null),
            Obj::Data(d) => {
                let s = merge::data_value(self.tree(), *d);
                if s.is_empty() { Val::Null } else { Val::Str(s) }
            }
            Obj::List(l) => l.first().cloned().map(|f| self.value_of(&f)).unwrap_or(Val::Null),
            _ => Val::Null,
        }
    }

    /// A value as a field's canonical value (`None` for null).
    pub fn canonical(&mut self, v: &Val) -> Option<String> {
        match v {
            Val::Null => None,
            Val::Obj(o) => {
                let o = o.clone();
                let inner = self.value_of(&o);
                if matches!(inner, Val::Obj(_)) { None } else { self.canonical(&inner) }
            }
            other => Some(other.text()),
        }
    }

    fn set_raw(&mut self, n: N, v: &Val) {
        let s = self.canonical(v);
        self.live.set_raw(n, s.as_deref().filter(|x| !x.is_empty()));
    }

    fn attr_or(&self, n: N, path: &[&str], name: &str, default: &str) -> String {
        let p: Vec<String> = path.iter().map(|s| s.to_string()).collect();
        self.live.attr(n, &p, name).unwrap_or_else(|| default.to_string())
    }

    fn op(&mut self, n: N, path: &[&str], key: Key, value: &str) {
        let path = path.iter().map(|s| s.to_string()).collect();
        self.live.set_op(n, Op { path, key, value: value.to_string() });
    }

    /// Reset fields to their template defaults (`xfa.host.resetData`).
    fn reset(&mut self, which: Option<Vec<N>>) {
        let all: Vec<N> = self.live.walk(0);
        let targets: Vec<N> = match which {
            None => all,
            Some(roots) => all.into_iter().filter(|k| roots.iter().any(|r| r == k || self.is_below(*k, *r))).collect(),
        };
        for n in targets {
            let Some(inst) = self.live.inst(n) else { continue };
            if !matches!(inst.tag, Tag::Field | Tag::ExclGroup) {
                continue;
            }
            let t = self.tree();
            let default = t.child(self.live.orig(inst.t), "value").and_then(|v| crate::template::value_text(t, v));
            self.live.set_raw(n, default.as_deref());
        }
    }

    // ── properties ──────────────────────────────────────────────────────────────────────────

    /// Read a property (`Ok(None)`: no such property).
    pub fn get(&mut self, o: &Obj, name: &str) -> Result<Option<Val>, String> {
        let s = |v: &str| Some(Val::Str(v.to_string()));
        Ok(match o {
            Obj::Node(n) => {
                let n = *n;
                let Some(inst) = self.live.inst(n) else { return Ok(None) };
                let (tag, data) = (inst.tag, inst.data.clone());
                match name {
                    "rawValue" => Some(self.raw_val(n)),
                    "formattedValue" | "editValue" => s(&self.live.formatted(n)),
                    "isNull" => Some(Val::Bool(self.live.raw(n).is_none())),
                    "name" => s(&inst.name),
                    "className" => s(class_name(tag)),
                    "somExpression" => s(&format!("xfa[0].form[0].{}", inst.som)),
                    "index" | "instanceIndex" => Some(Val::Num(self.live.index(n) as f64)),
                    "presence" => s(dom::presence_name(self.live.presence(n))),
                    "access" => s(&self.attr_or(n, &[], "access", "open")),
                    "relevant" => s(&self.attr_or(n, &[], "relevant", "")),
                    "locale" => s(&self.attr_or(n, &[], "locale", "en_US")),
                    "mandatory" => s(&self.attr_or(n, &["validate"], "nullTest", "disabled")),
                    "fillColor" => s(&self.attr_or(n, &["border", "fill", "color"], "value", "255,255,255")),
                    "borderColor" => s(&self.attr_or(n, &["border", "edge", "color"], "value", "0,0,0")),
                    "fontColor" => s(&self.attr_or(n, &["font", "fill", "color"], "value", "0,0,0")),
                    "borderWidth" => s(&self.attr_or(n, &["border", "edge"], "thickness", "0.5pt")),
                    "h" | "w" | "x" | "y" | "minH" | "minW" | "maxH" | "maxW" => s(&self.attr_or(n, &[], name, "0in")),
                    "validationMessage" => s(&self.live.text_at(n, &["validate".into(), "message".into(), "text".into()]).unwrap_or_default()),
                    "parent" => Some(Val::Obj(match self.live.parent(n) {
                        Some(p) => Obj::Node(p),
                        None => Obj::Form,
                    })),
                    "all" => Some(Val::Obj(Obj::List(self.live.same_named(n).into_iter().map(Obj::Node).collect()))),
                    "nodes" => Some(Val::Obj(Obj::List(self.live.kids(n).into_iter().map(Obj::Node).collect()))),
                    "length" => Some(Val::Num(self.live.items(n).len() as f64)),
                    "selectedIndex" => {
                        let raw = self.live.raw(n);
                        let i = self.live.items(n).iter().position(|(sv, _)| Some(sv) == raw.as_ref());
                        Some(Val::Num(i.map(|i| i as f64).unwrap_or(-1.0)))
                    }
                    "instanceManager" => self.manager_of(n).map(Val::Obj),
                    "dataNode" => {
                        data.and_then(|p| merge::Data { tree: self.tree(), root: Some(self.live.data_root) }.find(&p)).map(|d| Val::Obj(Obj::Data(d)))
                    }
                    "oneOfChild" => None,
                    _ => self.child(o, name, Idx::Default, false).into_iter().next().map(Val::Obj),
                }
            }
            Obj::Prop(n, path) => {
                let last = path.last().map(String::as_str).unwrap_or("");
                match name {
                    "value" if is_content_element(last) => s(&self.live.text_at(*n, path).unwrap_or_default()),
                    "className" => s(last),
                    "parent" => Some(Val::Obj(match path.len() {
                        0 | 1 => Obj::Node(*n),
                        k => Obj::Prop(*n, path.get(..k - 1).map(<[String]>::to_vec).unwrap_or_default()),
                    })),
                    "oneOfChild" => {
                        let t = self.tree();
                        let mut at = self.live.inst(*n).map(|i| i.t);
                        for p in path {
                            at = at.and_then(|a| t.child(a, p));
                        }
                        let first =
                            at.and_then(|a| t.elements(a).find(|c| !matches!(t.tag(*c), "extras" | "picture" | "border" | "margin" | "comb")));
                        first.map(|f| {
                            let mut p = path.clone();
                            p.push(t.tag(f).to_string());
                            Val::Obj(Obj::Prop(*n, p))
                        })
                    }
                    _ if PROPERTY_ELEMENTS.contains(&name) => {
                        let mut p = path.clone();
                        p.push(name.to_string());
                        Some(Val::Obj(Obj::Prop(*n, p)))
                    }
                    _ => s(&self.live.attr(*n, path, name).unwrap_or_else(|| default_attr(last, name).to_string())),
                }
            }
            Obj::Manager(p, t) => match name {
                "count" => Some(Val::Num(self.live.instances(*p, *t).len() as f64)),
                "min" => Some(Val::Num(self.live.occur(*t).0 as f64)),
                "max" => {
                    let raw = self.tree().child(*t, "occur").and_then(|o| self.tree().attr(o, "max")).and_then(|v| v.trim().parse::<f64>().ok());
                    Some(Val::Num(raw.unwrap_or(self.live.occur(*t).1 as f64)))
                }
                "name" => s(&format!("_{}", self.tree().attr(*t, "name").unwrap_or(""))),
                "className" => s("instanceManager"),
                _ => None,
            },
            Obj::Data(d) => {
                let d = *d;
                match name {
                    "value" => s(&merge::data_value(self.tree(), d)),
                    "name" => s(dom::data_name(self.tree(), d)),
                    "isNull" => Some(Val::Bool(merge::data_value(self.tree(), d).is_empty())),
                    "className" => s(if self.tree().elements(d).next().is_some() { "dataGroup" } else { "dataValue" }),
                    "nodes" => Some(Val::Obj(Obj::List(self.tree().elements(d).map(Obj::Data).collect()))),
                    "parent" => self.tree().parent(d).map(|p| Val::Obj(Obj::Data(p))),
                    _ => self.child(o, name, Idx::Default, false).into_iter().next().map(Val::Obj),
                }
            }
            Obj::Host => match name {
                "name" => s("Acrobat"),
                "appType" => s("Exchange-Pro"),
                "version" => s("11.0"),
                "variation" => s("Full"),
                "platform" => s(match std::env::consts::OS {
                    "windows" => "WIN",
                    "macos" => "MAC",
                    _ => "UNIX",
                }),
                "language" => s("en_US"),
                "numPages" => Some(Val::Num(self.page_count as f64)),
                "currentPage" => Some(Val::Num(0.0)),
                "title" => s(""),
                "validationsEnabled" => Some(Val::Bool(true)),
                "calculationsEnabled" => Some(Val::Bool(self.calculations)),
                "documentInBrowser" => Some(Val::Bool(false)),
                _ => None,
            },
            Obj::Event => {
                let e = &self.event;
                match name {
                    "change" => s(&e.change),
                    "newText" | "fullText" => s(&e.new_text),
                    "prevText" => s(&e.prev_text),
                    "name" => s(&e.name),
                    "target" => e.target.map(|t| Val::Obj(Obj::Node(t))),
                    "commitKey" | "selStart" | "selEnd" => Some(Val::Num(0.0)),
                    "modifier" | "shift" | "keyDown" | "reenter" => Some(Val::Bool(false)),
                    _ => None,
                }
            }
            Obj::Form => match name {
                "className" => s("form"),
                _ => self.child(o, name, Idx::Default, false).into_iter().next().map(Val::Obj),
            },
            Obj::Xfa => self.child(o, name, Idx::Default, false).into_iter().next().map(Val::Obj),
            Obj::List(l) => match name {
                "length" => Some(Val::Num(l.len() as f64)),
                _ => None,
            },
            Obj::Layout | Obj::Script(..) => None,
        })
    }

    /// Write a property. Read-only and unknown properties are ignored, as in Acrobat.
    pub fn set(&mut self, o: &Obj, name: &str, v: Val) -> Result<(), String> {
        match o {
            Obj::Node(n) => {
                let n = *n;
                let text = self.canonical(&v).unwrap_or_default();
                match name {
                    "rawValue" => self.set_raw(n, &v),
                    "formattedValue" | "editValue" => {
                        let t = self.tree();
                        let parsed = self.live.inst(n).and_then(|i| realize::date_display(t, i.t)).and_then(|p| picture::parse(&text, &p));
                        let display = self.live.items(n).into_iter().find(|(_, d)| *d == text).map(|(sv, _)| sv);
                        let value = parsed.or(display).unwrap_or(text);
                        self.live.set_raw(n, Some(&value).filter(|x| !x.is_empty()).map(String::as_str));
                    }
                    "presence" => {
                        if matches!(text.as_str(), "visible" | "invisible" | "hidden" | "inactive") {
                            self.live.set_presence(n, &text);
                        }
                    }
                    "access" => {
                        if matches!(text.as_str(), "open" | "readOnly" | "protected" | "nonInteractive") {
                            self.op(n, &[], Key::Attr("access".into()), &text);
                        }
                    }
                    "relevant" => self.op(n, &[], Key::Attr("relevant".into()), &text),
                    "mandatory" => {
                        if matches!(text.as_str(), "error" | "warning" | "disabled") {
                            self.op(n, &["validate"], Key::Attr("nullTest".into()), &text);
                        }
                    }
                    "fillColor" => {
                        if !self.live.has_element(n, &["border".into()]) {
                            // A new border shows only its fill.
                            self.op(n, &["border", "edge"], Key::Attr("presence".into()), "hidden");
                        }
                        self.op(n, &["border", "fill", "color"], Key::Attr("value".into()), &text);
                        self.op(n, &["border", "fill"], Key::Attr("presence".into()), "visible");
                    }
                    "borderColor" => self.op(n, &["border", "edge", "color"], Key::Attr("value".into()), &text),
                    "borderWidth" => self.op(n, &["border", "edge"], Key::Attr("thickness".into()), &text),
                    "fontColor" => self.op(n, &["font", "fill", "color"], Key::Attr("value".into()), &text),
                    "h" | "w" | "x" | "y" | "minH" | "minW" | "maxH" | "maxW" => self.op(n, &[], Key::Attr(name.into()), &text),
                    "validationMessage" => self.op(n, &["validate", "message", "text"], Key::Text, &text),
                    "selectedIndex" => {
                        let i = v.number();
                        let item = (i >= 0.0).then(|| self.live.items(n).get(i as usize).map(|(sv, _)| sv.clone())).flatten();
                        self.live.set_raw(n, item.as_deref());
                    }
                    _ => {}
                }
                Ok(())
            }
            Obj::Prop(n, path) => {
                let last = path.last().map(String::as_str).unwrap_or("");
                let text = self.canonical(&v).unwrap_or_default();
                let p: Vec<&str> = path.iter().map(String::as_str).collect();
                if name == "value" && is_content_element(last) {
                    self.op(*n, &p, Key::Text, &text);
                } else if !PROPERTY_ELEMENTS.contains(&name) && !matches!(name, "className" | "parent" | "oneOfChild") {
                    self.op(*n, &p, Key::Attr(name.to_string()), &text);
                }
                Ok(())
            }
            Obj::Manager(p, t) if name == "count" => self.set_instances(*p, *t, v.number()),
            Obj::Data(d) if name == "value" => {
                let text = self.canonical(&v).unwrap_or_default();
                let d = *d;
                let path = merge::Data { tree: self.tree(), root: Some(self.live.data_root) }.path_of(d);
                self.live.write_data(&path, &text);
                // Fields bound to this data node show the new value.
                for k in self.live.walk(0) {
                    if self.live.inst(k).is_some_and(|i| i.data.as_ref() == Some(&path)) {
                        self.live.set_raw(k, Some(&text).filter(|x| !x.is_empty()).map(String::as_str));
                    }
                }
                Ok(())
            }
            Obj::Host if name == "calculationsEnabled" => {
                self.calculations = v.number() != 0.0 || matches!(v, Val::Bool(true));
                Ok(())
            }
            Obj::Event => {
                let text = self.canonical(&v).unwrap_or_default();
                match name {
                    "change" => self.event.change = text,
                    "newText" => self.event.new_text = text,
                    _ => {}
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn set_instances(&mut self, p: N, t: NodeId, count: f64) -> Result<(), String> {
        if !count.is_finite() || count < 0.0 {
            return Err("the instance count must be a positive number".into());
        }
        let want = count as usize;
        let mut guard = 0;
        loop {
            let have = self.live.instances(p, t).len();
            guard += 1;
            if have == want || guard > 10_000 {
                break;
            }
            if have < want {
                let new = self.live.add_instance(p, t, None)?;
                self.pending.push(Pending::Initialize(new));
            } else {
                self.live.remove_instance(p, t, have - 1)?;
            }
        }
        self.pending.push(Pending::Calculate);
        Ok(())
    }

    /// Whether `name` is a method of `o` (for JavaScript, which needs a function object).
    pub fn is_method(o: &Obj, name: &str) -> bool {
        match o {
            Obj::Node(_) => matches!(
                name,
                "resolveNode"
                    | "resolveNodes"
                    | "execEvent"
                    | "execCalculate"
                    | "execInitialize"
                    | "execValidate"
                    | "clearItems"
                    | "addItem"
                    | "deleteItem"
                    | "setItems"
                    | "boundItem"
                    | "getDisplayItem"
                    | "getSaveItem"
                    | "getItemState"
                    | "setItemState"
                    | "isPropertySpecified"
                    | "getAttribute"
                    | "setAttribute"
                    | "setFocus"
                    | "getElement"
                    | "remerge"
            ),
            Obj::Manager(..) => matches!(name, "addInstance" | "insertInstance" | "removeInstance" | "setInstances" | "moveInstance"),
            Obj::Host => matches!(
                name,
                "messageBox"
                    | "response"
                    | "beep"
                    | "print"
                    | "resetData"
                    | "setFocus"
                    | "getFocus"
                    | "gotoURL"
                    | "importData"
                    | "exportData"
                    | "pageUp"
                    | "pageDown"
                    | "openList"
                    | "currentDateTime"
            ),
            Obj::Layout => matches!(
                name,
                "page" | "pageCount" | "absPage" | "absPageCount" | "relayout" | "pageSpan" | "sheet" | "sheetCount" | "h" | "w" | "x" | "y"
            ),
            Obj::Form => {
                matches!(name, "recalculate" | "execCalculate" | "execInitialize" | "execValidate" | "remerge" | "resolveNode" | "resolveNodes")
            }
            Obj::Xfa => matches!(name, "resolveNode" | "resolveNodes"),
            Obj::Data(_) => matches!(name, "resolveNode" | "resolveNodes"),
            Obj::List(_) => name == "item",
            _ => false,
        }
    }

    fn node_arg(&self, from: N, v: Option<&Val>) -> Option<N> {
        match v? {
            Val::Obj(Obj::Node(n)) => Some(*n),
            Val::Str(s) => match self.resolve(from, s).into_iter().next() {
                Some(Obj::Node(n)) => Some(n),
                _ => None,
            },
            _ => None,
        }
    }

    /// Call a method. `from` is the node of the running script (for relative SOM).
    pub fn call(&mut self, o: &Obj, name: &str, args: &[Val], from: N) -> Result<Val, String> {
        let arg = |i: usize| args.get(i).cloned().unwrap_or(Val::Null);
        let text = |h: &mut Host, i: usize| h.canonical(&arg(i)).unwrap_or_default();
        match o {
            Obj::Node(n) => {
                let n = *n;
                match name {
                    "resolveNode" => {
                        let som = text(self, 0);
                        Ok(self.resolve(n, &som).into_iter().next().map(Val::Obj).unwrap_or(Val::Null))
                    }
                    "resolveNodes" => {
                        let som = text(self, 0);
                        Ok(Val::Obj(Obj::List(self.resolve(n, &som))))
                    }
                    "execEvent" => {
                        let a = text(self, 0);
                        self.pending.push(Pending::Event(n, a));
                        Ok(Val::Null)
                    }
                    "execCalculate" => {
                        self.pending.push(Pending::Calculate);
                        Ok(Val::Null)
                    }
                    "execInitialize" => {
                        self.pending.push(Pending::Initialize(n));
                        Ok(Val::Null)
                    }
                    "execValidate" => Ok(Val::Bool(true)),
                    "clearItems" => {
                        self.live.set_items(n, &[]);
                        Ok(Val::Null)
                    }
                    "addItem" => {
                        let display = text(self, 0);
                        let save = if args.len() > 1 { text(self, 1) } else { display.clone() };
                        let mut items = self.live.items(n);
                        if items.len() < 10_000 {
                            items.push((save, display));
                        }
                        self.live.set_items(n, &items);
                        Ok(Val::Null)
                    }
                    "deleteItem" => {
                        let i = arg(0).number();
                        let mut items = self.live.items(n);
                        if i >= 0.0 && (i as usize) < items.len() {
                            items.remove(i as usize);
                            self.live.set_items(n, &items);
                            return Ok(Val::Bool(true));
                        }
                        Ok(Val::Bool(false))
                    }
                    "setItems" => {
                        let list = text(self, 0);
                        let cols = arg(1).number().max(1.0) as usize;
                        let parts: Vec<String> = list.split(',').map(|p| p.trim().to_string()).collect();
                        let items: Vec<(String, String)> = if cols >= 2 {
                            parts.chunks(2).map(|c| (c.get(1).cloned().unwrap_or_default(), c.first().cloned().unwrap_or_default())).collect()
                        } else {
                            parts.into_iter().map(|p| (p.clone(), p)).collect()
                        };
                        self.live.set_items(n, &items);
                        Ok(Val::Bool(true))
                    }
                    "boundItem" => {
                        let d = text(self, 0);
                        Ok(Val::Str(self.live.items(n).into_iter().find(|(_, x)| *x == d).map(|(sv, _)| sv).unwrap_or(d)))
                    }
                    "getDisplayItem" | "getSaveItem" => {
                        let i = arg(0).number();
                        let items = self.live.items(n);
                        let item = (i >= 0.0).then(|| items.get(i as usize)).flatten();
                        Ok(item.map(|(sv, d)| Val::Str(if name == "getSaveItem" { sv.clone() } else { d.clone() })).unwrap_or(Val::Null))
                    }
                    "getItemState" => {
                        let i = arg(0).number();
                        let items = self.live.items(n);
                        let raw = self.live.raw(n).unwrap_or_default();
                        let on = (i >= 0.0).then(|| items.get(i as usize)).flatten().is_some_and(|(sv, _)| raw.split('\n').any(|r| r == sv));
                        Ok(Val::Bool(on))
                    }
                    "setItemState" => {
                        let i = arg(0).number();
                        let on = arg(1).number() != 0.0 || matches!(arg(1), Val::Bool(true));
                        let items = self.live.items(n);
                        if let Some((sv, _)) = (i >= 0.0).then(|| items.get(i as usize)).flatten() {
                            let v = if on { Some(sv.clone()) } else { None };
                            self.live.set_raw(n, v.as_deref());
                        }
                        Ok(Val::Null)
                    }
                    "isPropertySpecified" => {
                        let p = text(self, 0);
                        Ok(Val::Bool(self.live.has_element(n, std::slice::from_ref(&p)) || self.live.attr(n, &[], &p).is_some()))
                    }
                    "getAttribute" => {
                        let a = text(self, 0);
                        Ok(self.get(o, &a)?.unwrap_or(Val::Null))
                    }
                    "setAttribute" => {
                        let (value, a) = (arg(0), text(self, 1));
                        self.set(o, &a, value)?;
                        Ok(Val::Null)
                    }
                    "getElement" => {
                        let e = text(self, 0);
                        Ok(Val::Obj(Obj::Prop(n, vec![e])))
                    }
                    "setFocus" => {
                        self.out.focus = self.live.inst(n).map(|i| i.som.clone());
                        Ok(Val::Null)
                    }
                    "remerge" => Ok(Val::Null),
                    _ => Err(format!("{name} is not a method of this object")),
                }
            }
            Obj::Manager(p, t) => {
                let (p, t) = (*p, *t);
                let r = match name {
                    "addInstance" => {
                        let new = self.live.add_instance(p, t, None)?;
                        self.pending.push(Pending::Initialize(new));
                        Val::Obj(Obj::Node(new))
                    }
                    "insertInstance" => {
                        let at = arg(0).number().max(0.0) as usize;
                        let new = self.live.add_instance(p, t, Some(at))?;
                        self.pending.push(Pending::Initialize(new));
                        Val::Obj(Obj::Node(new))
                    }
                    "removeInstance" => {
                        let i = arg(0).number().max(0.0) as usize;
                        self.live.remove_instance(p, t, i)?;
                        Val::Null
                    }
                    "setInstances" => {
                        self.set_instances(p, t, arg(0).number())?;
                        Val::Null
                    }
                    "moveInstance" => {
                        let (a, b) = (arg(0).number().max(0.0) as usize, arg(1).number().max(0.0) as usize);
                        self.live.move_instance(p, t, a, b)?;
                        Val::Null
                    }
                    _ => return Err(format!("{name} is not a method of instanceManager")),
                };
                self.pending.push(Pending::Calculate);
                Ok(r)
            }
            Obj::Host => match name {
                "messageBox" => {
                    let msg = text(self, 0);
                    let title = text(self, 1);
                    self.out.messages.push(if title.is_empty() { msg } else { format!("{title}: {msg}") });
                    // No one can answer here: OK, or Yes for Yes/No questions.
                    Ok(Val::Num(if matches!(arg(3).number() as i64, 2 | 3) { 4.0 } else { 1.0 }))
                }
                "response" => Ok(match arg(2) {
                    Val::Null => Val::Null,
                    v => Val::Str(v.text()),
                }),
                "print" => {
                    self.out.prints += 1;
                    Ok(Val::Null)
                }
                "resetData" => {
                    let which: Vec<N> = args.iter().filter_map(|a| self.node_arg(from, Some(a))).collect();
                    let list = if args.is_empty() || matches!(arg(0), Val::Null) { None } else { Some(which) };
                    self.reset(list);
                    self.pending.push(Pending::Calculate);
                    Ok(Val::Null)
                }
                "setFocus" => {
                    let target = self.node_arg(from, args.first());
                    self.out.focus = target.and_then(|t| self.live.inst(t)).map(|i| i.som.clone());
                    Ok(Val::Null)
                }
                "gotoURL" => {
                    let u = text(self, 0);
                    if self.out.urls.len() < 100 {
                        self.out.urls.push(u);
                    }
                    Ok(Val::Null)
                }
                "currentDateTime" => {
                    let (y, m, d) = self.today;
                    Ok(Val::Str(format!("{y:04}{m:02}{d:02}T000000")))
                }
                "beep" | "getFocus" | "importData" | "exportData" | "pageUp" | "pageDown" | "openList" => Ok(Val::Null),
                _ => Err(format!("xfa.host.{name} is not available")),
            },
            Obj::Layout => {
                let page = |h: &Host, v: Option<&Val>| h.node_arg(from, v).and_then(|k| h.page_of.get(&k).copied()).unwrap_or(0);
                Ok(match name {
                    "page" | "sheet" => Val::Num((page(self, args.first()) + 1) as f64),
                    "absPage" => Val::Num(page(self, args.first()) as f64),
                    "pageCount" | "absPageCount" | "sheetCount" => Val::Num(self.page_count as f64),
                    "pageSpan" => Val::Num(1.0),
                    "relayout" => {
                        self.live.layout_changed = true;
                        Val::Null
                    }
                    "h" | "w" | "x" | "y" => Val::Num(0.0),
                    _ => return Err(format!("xfa.layout.{name} is not available")),
                })
            }
            Obj::Form => match name {
                "recalculate" | "execCalculate" => {
                    self.pending.push(Pending::Calculate);
                    Ok(Val::Null)
                }
                "execInitialize" => {
                    self.pending.push(Pending::Initialize(N::ROOT));
                    Ok(Val::Null)
                }
                "execValidate" => Ok(Val::Bool(true)),
                "remerge" => Ok(Val::Null),
                "resolveNode" | "resolveNodes" => self.call(&Obj::Xfa, name, args, from),
                _ => Err(format!("xfa.form.{name} is not available")),
            },
            Obj::Xfa | Obj::Data(_) => {
                let som = text(self, 0);
                // Absolute expressions start at the form root; "xfa.…" and "$…" are handled
                // by the resolver.
                let base = match o {
                    Obj::Data(d) => {
                        let found = self.child(&Obj::Data(*d), &som, Idx::Default, false);
                        return Ok(match name {
                            "resolveNodes" => Val::Obj(Obj::List(found)),
                            _ => found.into_iter().next().map(Val::Obj).unwrap_or(Val::Null),
                        });
                    }
                    _ => N::ROOT,
                };
                let found = self.resolve(base, &som);
                Ok(match name {
                    "resolveNodes" => Val::Obj(Obj::List(found)),
                    "resolveNode" => found.into_iter().next().map(Val::Obj).unwrap_or(Val::Null),
                    _ => return Err(format!("{name} is not available")),
                })
            }
            Obj::List(l) if name == "item" => {
                let i = arg(0).number();
                Ok((i >= 0.0).then(|| l.get(i as usize).cloned()).flatten().map(Val::Obj).unwrap_or(Val::Null))
            }
            _ => Err(format!("{name} is not a method of this object")),
        }
    }

    /// The default presence when not specified, for `Prop` reads.
    pub fn presence_of(&self, n: N) -> Presence {
        self.live.presence(n)
    }
}

/// Defaults of commonly read property attributes.
fn default_attr(element: &str, attr: &str) -> &'static str {
    match (element, attr) {
        (_, "presence") => "visible",
        ("color", "value") => "0,0,0",
        ("font", "typeface") => "Courier",
        ("font", "size") => "10pt",
        ("para", "hAlign") => "left",
        ("para", "vAlign") => "top",
        ("caption", "placement") => "left",
        ("textEdit", "multiLine") => "0",
        _ => "",
    }
}
