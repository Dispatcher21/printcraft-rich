//! A small, lenient XML tree for XFA packets: parse, query, edit and serialize back.
//!
//! XFA needs only the XML 1.0 core: elements, attributes, text, CDATA, comments, processing
//! instructions and a doctype (kept verbatim). Namespace prefixes stay part of the names, so a
//! round trip writes back exactly the prefixes and declarations it read. Every input is
//! untrusted: depth and node counts are capped and nothing here panics.

use crate::XfaError;

/// Deepest element nesting accepted.
const MAX_DEPTH: usize = 256;
/// Most nodes in one tree.
const MAX_NODES: usize = 4_000_000;

/// A node index in a [`Tree`].
pub type NodeId = usize;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// The invisible document node that holds the top-level nodes.
    Document,
    Element {
        name: String,
        attrs: Vec<(String, String)>,
    },
    Text(String),
    CData(String),
    Comment(String),
    /// A processing instruction or doctype, written back verbatim (without the delimiters).
    Pi(String),
    Doctype(String),
}

#[derive(Clone, Debug)]
pub struct Node {
    pub kind: Kind,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
}

#[derive(Clone, Debug)]
pub struct Tree {
    nodes: Vec<Node>,
}

/// The part of a qualified name after the prefix.
pub fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

impl Default for Tree {
    fn default() -> Self {
        Tree { nodes: vec![Node { kind: Kind::Document, parent: None, children: Vec::new() }] }
    }
}

impl Tree {
    pub const ROOT: NodeId = 0;

    pub fn parse(text: &str) -> Result<Tree, XfaError> {
        Parser { s: text, pos: 0, tree: Tree::default() }.run()
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        self.nodes.get(id).map(|n| n.children.as_slice()).unwrap_or(&[])
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes.get(id).and_then(|n| n.parent)
    }

    /// The qualified name of an element.
    pub fn name(&self, id: NodeId) -> Option<&str> {
        match &self.nodes.get(id)?.kind {
            Kind::Element { name, .. } => Some(name),
            _ => None,
        }
    }

    /// The local name of an element (`""` for other nodes).
    pub fn tag(&self, id: NodeId) -> &str {
        self.name(id).map(local).unwrap_or("")
    }

    pub fn is_element(&self, id: NodeId) -> bool {
        self.name(id).is_some()
    }

    pub fn attr(&self, id: NodeId, key: &str) -> Option<&str> {
        match &self.nodes.get(id)?.kind {
            Kind::Element { attrs, .. } => attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()),
            _ => None,
        }
    }

    /// An attribute by local name (any prefix).
    pub fn attr_local(&self, id: NodeId, key: &str) -> Option<&str> {
        match &self.nodes.get(id)?.kind {
            Kind::Element { attrs, .. } => attrs.iter().find(|(k, _)| local(k) == key && !k.starts_with("xmlns")).map(|(_, v)| v.as_str()),
            _ => None,
        }
    }

    pub fn attrs(&self, id: NodeId) -> &[(String, String)] {
        match self.nodes.get(id).map(|n| &n.kind) {
            Some(Kind::Element { attrs, .. }) => attrs,
            _ => &[],
        }
    }

    pub fn set_attr(&mut self, id: NodeId, key: &str, value: &str) {
        if let Some(Node { kind: Kind::Element { attrs, .. }, .. }) = self.nodes.get_mut(id) {
            match attrs.iter_mut().find(|(k, _)| k == key) {
                Some(a) => a.1 = value.to_string(),
                None => attrs.push((key.to_string(), value.to_string())),
            }
        }
    }

    pub fn remove_attr(&mut self, id: NodeId, key: &str) {
        if let Some(Node { kind: Kind::Element { attrs, .. }, .. }) = self.nodes.get_mut(id) {
            attrs.retain(|(k, _)| k != key);
        }
    }

    /// Element children.
    pub fn elements(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.children(id).iter().copied().filter(|c| self.is_element(*c))
    }

    /// The first element child with local name `tag`.
    pub fn child(&self, id: NodeId, tag: &str) -> Option<NodeId> {
        self.elements(id).find(|c| self.tag(*c) == tag)
    }

    /// The first element at the end of a path of local names.
    pub fn path(&self, id: NodeId, tags: &[&str]) -> Option<NodeId> {
        tags.iter().try_fold(id, |at, t| self.child(at, t))
    }

    /// The concatenated text and CDATA of a node and its descendants.
    pub fn text(&self, id: NodeId) -> String {
        let mut out = String::new();
        let mut stack = vec![id];
        let mut steps = 0usize;
        while let Some(n) = stack.pop() {
            steps += 1;
            if steps > MAX_NODES {
                break;
            }
            match self.nodes.get(n).map(|n| &n.kind) {
                Some(Kind::Text(t)) | Some(Kind::CData(t)) => out.push_str(t),
                Some(Kind::Element { .. }) | Some(Kind::Document) => stack.extend(self.children(n).iter().rev()),
                _ => {}
            }
        }
        out
    }

    /// Replace a node's children with one text node (or none for an empty value).
    pub fn set_text(&mut self, id: NodeId, value: &str) {
        let old = self.nodes.get(id).map(|n| n.children.clone()).unwrap_or_default();
        for c in old {
            if let Some(n) = self.nodes.get_mut(c) {
                n.parent = None;
            }
        }
        if let Some(n) = self.nodes.get_mut(id) {
            n.children.clear();
        }
        if !value.is_empty() {
            self.append(id, Kind::Text(value.to_string()));
        }
    }

    /// Append a new node under `parent`; returns its id.
    pub fn append(&mut self, parent: NodeId, kind: Kind) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node { kind, parent: Some(parent), children: Vec::new() });
        if let Some(p) = self.nodes.get_mut(parent) {
            p.children.push(id);
        }
        id
    }

    /// Insert a new node under `parent` at child position `at` (clamped).
    pub fn insert(&mut self, parent: NodeId, at: usize, kind: Kind) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node { kind, parent: Some(parent), children: Vec::new() });
        if let Some(p) = self.nodes.get_mut(parent) {
            let at = at.min(p.children.len());
            p.children.insert(at, id);
        }
        id
    }

    /// Append a new element; returns its id.
    pub fn append_element(&mut self, parent: NodeId, name: &str) -> NodeId {
        self.append(parent, Kind::Element { name: name.to_string(), attrs: Vec::new() })
    }

    /// Detach `id` from its parent (the node stays in the arena, unreachable).
    pub fn detach(&mut self, id: NodeId) {
        if let Some(p) = self.parent(id)
            && let Some(pn) = self.nodes.get_mut(p)
        {
            pn.children.retain(|c| *c != id);
        }
        if let Some(n) = self.nodes.get_mut(id) {
            n.parent = None;
        }
    }

    /// Copy the subtree at `src` of `from` under `parent` here; returns the copy's id.
    pub fn copy_from(&mut self, from: &Tree, src: NodeId, parent: NodeId) -> Option<NodeId> {
        let kind = from.nodes.get(src)?.kind.clone();
        let top = self.append(parent, kind);
        let mut stack: Vec<(NodeId, NodeId, usize)> = vec![(src, top, 0)];
        while let Some((s, d, depth)) = stack.pop() {
            if depth > MAX_DEPTH || self.nodes.len() > MAX_NODES {
                break;
            }
            for c in from.children(s) {
                if let Some(n) = from.nodes.get(*c) {
                    let nd = self.append(d, n.kind.clone());
                    stack.push((*c, nd, depth + 1));
                }
            }
        }
        Some(top)
    }

    /// Copy the subtree at `src` under `parent` in this same tree; returns the copy's id.
    pub fn copy_within(&mut self, src: NodeId, parent: NodeId) -> Option<NodeId> {
        // Collect the subtree first (kinds with their parent's position in the list), so the
        // copy never reads nodes it is adding.
        let mut nodes: Vec<(Kind, Option<usize>)> = Vec::new();
        let mut stack: Vec<(NodeId, Option<usize>, usize)> = vec![(src, None, 0)];
        while let Some((n, up, depth)) = stack.pop() {
            let Some(node) = self.nodes.get(n) else { continue };
            if depth > MAX_DEPTH || nodes.len().saturating_add(self.nodes.len()) > MAX_NODES {
                break;
            }
            nodes.push((node.kind.clone(), up));
            let me = nodes.len() - 1;
            stack.extend(node.children.iter().rev().map(|c| (*c, Some(me), depth + 1)));
        }
        let mut made: Vec<NodeId> = Vec::with_capacity(nodes.len());
        for (kind, up) in nodes {
            let p = up.and_then(|i| made.get(i).copied()).unwrap_or(parent);
            made.push(self.append(p, kind));
        }
        made.first().copied()
    }

    /// List `child` among `parent`'s children too, without moving it: the child keeps its own
    /// parent. Shallow copies of template containers share their container children this way.
    pub fn share_child(&mut self, parent: NodeId, child: NodeId) {
        if child < self.nodes.len()
            && let Some(p) = self.nodes.get_mut(parent)
        {
            p.children.push(child);
        }
    }

    /// Every element in document order below `id` (excluding `id`), depth-limited.
    pub fn descendants(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack: Vec<(NodeId, usize)> = self.children(id).iter().rev().map(|c| (*c, 1)).collect();
        while let Some((n, depth)) = stack.pop() {
            if !self.is_element(n) || depth > MAX_DEPTH {
                continue;
            }
            out.push(n);
            stack.extend(self.children(n).iter().rev().map(|c| (*c, depth + 1)));
        }
        out
    }

    /// Serialize a node and its subtree (the document node writes its children).
    pub fn write(&self, id: NodeId, out: &mut String) {
        enum Step {
            Open(NodeId, usize),
            Close(NodeId),
        }
        let mut stack = vec![Step::Open(id, 0)];
        while let Some(step) = stack.pop() {
            match step {
                Step::Close(n) => {
                    if let Some(name) = self.name(n) {
                        out.push_str("</");
                        out.push_str(name);
                        out.push('>');
                    }
                }
                Step::Open(n, depth) => {
                    let Some(node) = self.nodes.get(n) else { continue };
                    if depth > MAX_DEPTH {
                        continue;
                    }
                    match &node.kind {
                        Kind::Document => stack.extend(node.children.iter().rev().map(|c| Step::Open(*c, depth + 1))),
                        Kind::Element { name, attrs } => {
                            out.push('<');
                            out.push_str(name);
                            for (k, v) in attrs {
                                out.push(' ');
                                out.push_str(k);
                                out.push_str("=\"");
                                escape(v, true, out);
                                out.push('"');
                            }
                            if node.children.is_empty() {
                                out.push_str("/>");
                            } else {
                                out.push('>');
                                stack.push(Step::Close(n));
                                stack.extend(node.children.iter().rev().map(|c| Step::Open(*c, depth + 1)));
                            }
                        }
                        Kind::Text(t) => escape(t, false, out),
                        Kind::CData(t) => {
                            // "]]>" can't appear inside a CDATA section: split it.
                            out.push_str("<![CDATA[");
                            out.push_str(&t.replace("]]>", "]]]]><![CDATA[>"));
                            out.push_str("]]>");
                        }
                        Kind::Comment(t) => {
                            out.push_str("<!--");
                            out.push_str(t);
                            out.push_str("-->");
                        }
                        Kind::Pi(t) => {
                            out.push_str("<?");
                            out.push_str(t);
                            out.push_str("?>");
                        }
                        Kind::Doctype(t) => {
                            out.push_str("<!DOCTYPE");
                            out.push_str(t);
                            out.push('>');
                        }
                    }
                }
            }
        }
    }

    pub fn to_string_of(&self, id: NodeId) -> String {
        let mut s = String::new();
        self.write(id, &mut s);
        s
    }
}

fn escape(s: &str, attr: bool, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            '\n' if attr => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' if attr => out.push_str("&#9;"),
            _ => out.push(c),
        }
    }
}

/// Resolve character and predefined entity references; unknown ones stay as written.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(rest.get(..i).unwrap_or(""));
        let after = rest.get(i..).unwrap_or("");
        let end = after.find(';').filter(|e| *e <= 12);
        let decoded = end.and_then(|e| {
            let ent = after.get(1..e)?;
            let c = match ent {
                "lt" => '<',
                "gt" => '>',
                "amp" => '&',
                "quot" => '"',
                "apos" => '\'',
                _ if ent.starts_with("#x") || ent.starts_with("#X") => char::from_u32(u32::from_str_radix(ent.get(2..)?, 16).ok()?)?,
                _ if ent.starts_with('#') => char::from_u32(ent.get(1..)?.parse().ok()?)?,
                _ => return None,
            };
            Some((c, e + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = after.get(len..).unwrap_or("");
            }
            None => {
                out.push('&');
                rest = after.get(1..).unwrap_or("");
            }
        }
    }
    out.push_str(rest);
    out
}

/// An element's name, its attributes, and whether it was self-closing.
type StartTag = (String, Vec<(String, String)>, bool);

struct Parser<'a> {
    s: &'a str,
    pos: usize,
    tree: Tree,
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        self.s.get(self.pos..).unwrap_or("")
    }

    fn err<T>(&self, m: &str) -> Result<T, XfaError> {
        Err(XfaError::Xml(format!("{m} at byte {}", self.pos)))
    }

    /// Advance past `pat`, returning the text before it.
    fn until(&mut self, pat: &str) -> Result<String, XfaError> {
        match self.rest().find(pat) {
            Some(i) => {
                let t = self.rest().get(..i).unwrap_or("").to_string();
                self.pos += i + pat.len();
                Ok(t)
            }
            None => self.err(&format!("missing {pat:?}")),
        }
    }

    fn run(mut self) -> Result<Tree, XfaError> {
        // A byte order mark is not content.
        if self.rest().starts_with('\u{feff}') {
            self.pos += '\u{feff}'.len_utf8();
        }
        let mut open: Vec<NodeId> = vec![Tree::ROOT];
        while self.pos < self.s.len() {
            if self.tree.nodes.len() > MAX_NODES {
                return self.err("too many XML nodes");
            }
            let parent = *open.last().unwrap_or(&Tree::ROOT);
            let whole = self.s;
            let rest = whole.get(self.pos..).unwrap_or("");
            if !rest.starts_with('<') {
                let end = rest.find('<').unwrap_or(rest.len());
                let raw = rest.get(..end).unwrap_or("");
                self.pos += end;
                self.tree.append(parent, Kind::Text(unescape(raw)));
                continue;
            }
            if rest.starts_with("<!--") {
                self.pos += 4;
                let t = self.until("-->")?;
                self.tree.append(parent, Kind::Comment(t));
            } else if rest.starts_with("<![CDATA[") {
                self.pos += 9;
                let t = self.until("]]>")?;
                self.tree.append(parent, Kind::CData(t));
            } else if rest.starts_with("<?") {
                self.pos += 2;
                let t = self.until("?>")?;
                // The XML declaration is regenerated by whoever writes a whole document.
                if !(parent == Tree::ROOT && t.starts_with("xml ")) {
                    self.tree.append(parent, Kind::Pi(t));
                }
            } else if rest.starts_with("<!DOCTYPE") {
                self.pos += 9;
                // An internal subset may hold '>' inside brackets.
                let mut depth = 0i32;
                let mut end = None;
                for (i, c) in self.rest().char_indices() {
                    match c {
                        '[' => depth += 1,
                        ']' => depth -= 1,
                        '>' if depth <= 0 => {
                            end = Some(i);
                            break;
                        }
                        _ => {}
                    }
                }
                let Some(e) = end else { return self.err("unterminated doctype") };
                let t = self.rest().get(..e).unwrap_or("").to_string();
                self.pos += e + 1;
                self.tree.append(parent, Kind::Doctype(t));
            } else if rest.starts_with("</") {
                self.pos += 2;
                let name = self.until(">")?;
                let name = name.trim();
                // Lenient: close up to the matching element; ignore a stray end tag.
                if let Some(k) = open.iter().rposition(|n| self.tree.name(*n) == Some(name)) {
                    open.truncate(k);
                }
            } else {
                self.pos += 1;
                let (name, attrs, empty) = self.start_tag()?;
                let id = self.tree.append(parent, Kind::Element { name, attrs });
                if !empty {
                    if open.len() > MAX_DEPTH {
                        return self.err("XML nested too deeply");
                    }
                    open.push(id);
                }
            }
        }
        Ok(self.tree)
    }

    /// Parse after '<' up to and including '>'.
    fn start_tag(&mut self) -> Result<StartTag, XfaError> {
        let bytes = self.s.as_bytes();
        let start = self.pos;
        while let Some(b) = bytes.get(self.pos) {
            if b.is_ascii_whitespace() || *b == b'>' || *b == b'/' {
                break;
            }
            self.pos += 1;
        }
        let name = self.s.get(start..self.pos).unwrap_or("").to_string();
        if name.is_empty() {
            return self.err("empty element name");
        }
        let mut attrs = Vec::new();
        loop {
            while bytes.get(self.pos).is_some_and(|b| b.is_ascii_whitespace()) {
                self.pos += 1;
            }
            match bytes.get(self.pos) {
                None => return self.err("unterminated start tag"),
                Some(b'>') => {
                    self.pos += 1;
                    return Ok((name, attrs, false));
                }
                Some(b'/') => {
                    self.pos += 1;
                    if bytes.get(self.pos) == Some(&b'>') {
                        self.pos += 1;
                    }
                    return Ok((name, attrs, true));
                }
                Some(_) => {
                    let ks = self.pos;
                    while let Some(b) = bytes.get(self.pos) {
                        if b.is_ascii_whitespace() || *b == b'=' || *b == b'>' || *b == b'/' {
                            break;
                        }
                        self.pos += 1;
                    }
                    let key = self.s.get(ks..self.pos).unwrap_or("").to_string();
                    while bytes.get(self.pos).is_some_and(|b| b.is_ascii_whitespace()) {
                        self.pos += 1;
                    }
                    if bytes.get(self.pos) != Some(&b'=') {
                        // An attribute without a value (not XML, but harmless).
                        if key.is_empty() {
                            self.pos += 1;
                        } else {
                            attrs.push((key, String::new()));
                        }
                        continue;
                    }
                    self.pos += 1;
                    while bytes.get(self.pos).is_some_and(|b| b.is_ascii_whitespace()) {
                        self.pos += 1;
                    }
                    let value = match bytes.get(self.pos) {
                        Some(q @ (b'"' | b'\'')) => {
                            let q = if *q == b'"' { "\"" } else { "'" };
                            self.pos += 1;
                            self.until(q)?
                        }
                        _ => {
                            let vs = self.pos;
                            while let Some(b) = bytes.get(self.pos) {
                                if b.is_ascii_whitespace() || *b == b'>' {
                                    break;
                                }
                                self.pos += 1;
                            }
                            self.s.get(vs..self.pos).unwrap_or("").to_string()
                        }
                    };
                    attrs.push((key, unescape(&value)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let src = r#"<?xml version="1.0"?><a x="1 &amp; 2"><!--c--><b>t&lt;</b><c/><![CDATA[<raw>]]></a>"#;
        let t = Tree::parse(src).unwrap_or_default();
        let a = t.child(Tree::ROOT, "a").unwrap_or(Tree::ROOT);
        assert_eq!(t.attr(a, "x"), Some("1 & 2"));
        assert_eq!(t.text(a), "t<<raw>");
        assert_eq!(t.to_string_of(a), r#"<a x="1 &amp; 2"><!--c--><b>t&lt;</b><c/><![CDATA[<raw>]]></a>"#);
    }

    #[test]
    fn hostile_input_does_not_panic() {
        for s in ["<", "<a", "<a b='", "</x>", "<a><b></a>", "&#xFFFFFFFF;", "<!DOCTYPE [", "<a>\u{e9}&#233;</a>", "<é/>"] {
            let _ = Tree::parse(s);
        }
        let deep = "<a>".repeat(10_000);
        assert!(Tree::parse(&deep).is_err());
    }

    #[test]
    fn prefixes() {
        let t = Tree::parse(r#"<xfa:data xmlns:xfa="u"><x xfa:dataNode="dataGroup"/></xfa:data>"#).unwrap_or_default();
        let d = t.child(Tree::ROOT, "data").unwrap_or(Tree::ROOT);
        assert_eq!(t.name(d), Some("xfa:data"));
        let x = t.child(d, "x").unwrap_or(Tree::ROOT);
        assert_eq!(t.attr_local(x, "dataNode"), Some("dataGroup"));
    }
}
