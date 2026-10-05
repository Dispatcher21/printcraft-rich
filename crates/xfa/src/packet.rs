//! The XFA packets of a PDF (`/AcroForm /XFA`, ISO 32000-2 §12.7.9; XFA 3.3 §"XDP"): read them as
//! one XDP tree, and write a new `datasets` packet back.

use printcraft_cos::{Dict, Document, ObjRef, Object, PdfString, Stream};

use crate::XfaError;
use crate::xml::{Kind, NodeId, Tree};

/// Largest XFA packet set read (decoded bytes).
const MAX_XDP: usize = 64 << 20;

/// Where the XFA lives in the PDF.
#[derive(Clone, Debug)]
pub(crate) enum Layout {
    /// One stream holding the whole XDP document.
    Single(ObjRef),
    /// `[name stream name stream …]`, possibly an indirect array.
    Array { array: Option<ObjRef>, parts: Vec<(String, Object)> },
}

/// The XDP document and its main packets.
pub struct Xdp {
    pub tree: Tree,
    /// The `<template>` element.
    pub template: Option<NodeId>,
    /// The `<xfa:datasets>` element.
    pub datasets: Option<NodeId>,
    pub(crate) layout: Layout,
}

pub(crate) fn acroform_ref(doc: &Document) -> Option<(ObjRef, Option<ObjRef>, Dict)> {
    let root = doc.root()?;
    let af = doc.get(root).as_dict()?.get(b"AcroForm")?.clone();
    let r = af.as_ref();
    let d = doc.resolve(&af).as_dict()?.clone();
    Some((root, r, d))
}

/// The `/XFA` entry, if the document has one.
pub fn xfa_entry(doc: &Document) -> Option<Object> {
    let (_, _, af) = acroform_ref(doc)?;
    let x = af.get(b"XFA")?.clone();
    match &*doc.resolve(&x) {
        Object::Stream(_) | Object::Array(_) => Some(x),
        _ => None,
    }
}

fn stream_text(doc: &Document, o: &Object, budget: &mut usize) -> Result<String, XfaError> {
    let obj = doc.resolve(o);
    let Object::Stream(s) = &*obj else { return Ok(String::new()) };
    let data = s.decoded_within(*budget).map_err(|e| XfaError::Pdf(e.to_string()))?;
    *budget = budget.saturating_sub(data.len());
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// Read the XFA packets as one XDP tree.
pub fn read(doc: &Document) -> Result<Xdp, XfaError> {
    let entry = xfa_entry(doc).ok_or(XfaError::NoXfa)?;
    let mut budget = MAX_XDP;
    let (text, layout) = match (&entry, &*doc.resolve(&entry)) {
        (Object::Ref(r), Object::Stream(_)) => (stream_text(doc, &entry, &mut budget)?, Layout::Single(*r)),
        (_, Object::Array(items)) => {
            let mut parts = Vec::new();
            let mut text = String::new();
            for pair in items.chunks(2) {
                let [name, stream] = pair else { continue };
                let name = match &*doc.resolve(name) {
                    Object::String(s) => s.to_text(),
                    _ => continue,
                };
                text.push_str(&stream_text(doc, stream, &mut budget)?);
                parts.push((name, stream.clone()));
            }
            (text, Layout::Array { array: entry.as_ref(), parts })
        }
        _ => return Err(XfaError::NoXfa),
    };
    let tree = Tree::parse(&text)?;
    let xdp = tree.elements(Tree::ROOT).find(|e| tree.tag(*e) == "xdp");
    // Some writers omit the xdp wrapper and concatenate the packets at the top level.
    let top = xdp.unwrap_or(Tree::ROOT);
    let template = tree.child(top, "template");
    let datasets = tree.child(top, "datasets");
    Ok(Xdp { tree, template, datasets, layout })
}

/// The `<xfa:data>` element (created, with a datasets packet, when missing).
pub fn data_root(xdp: &mut Xdp) -> NodeId {
    let tree = &mut xdp.tree;
    let datasets = match xdp.datasets {
        Some(d) => d,
        None => {
            let top = tree.elements(Tree::ROOT).find(|e| tree.tag(*e) == "xdp").unwrap_or(Tree::ROOT);
            let d = tree.append_element(top, "xfa:datasets");
            tree.set_attr(d, "xmlns:xfa", "http://www.xfa.org/schema/xfa-data/1.0/");
            xdp.datasets = Some(d);
            d
        }
    };
    match tree.child(datasets, "data") {
        Some(d) => d,
        None => {
            let prefix = tree.name(datasets).and_then(|n| n.split_once(':')).map(|(p, _)| p.to_string());
            let name = prefix.map(|p| format!("{p}:data")).unwrap_or_else(|| "data".into());
            // Data comes before any data description.
            tree.insert(datasets, 0, Kind::Element { name, attrs: Vec::new() })
        }
    }
}

/// The datasets packet as a standalone XML fragment, with the namespace declaration of its
/// prefix even when the original declared it on the `xdp` element.
fn datasets_text(xdp: &Xdp, datasets: NodeId) -> String {
    let mut tree = xdp.tree.clone();
    if let Some(name) = tree.name(datasets).map(str::to_string) {
        let decl = match name.split_once(':') {
            Some((p, _)) => format!("xmlns:{p}"),
            None => "xmlns".to_string(),
        };
        if tree.attr(datasets, &decl).is_none() {
            // Find the declaration in scope, else the XFA data namespace.
            let mut at = tree.parent(datasets);
            let mut value = None;
            while let Some(a) = at {
                if let Some(v) = tree.attr(a, &decl) {
                    value = Some(v.to_string());
                    break;
                }
                at = tree.parent(a);
            }
            tree.set_attr(datasets, &decl, value.as_deref().unwrap_or("http://www.xfa.org/schema/xfa-data/1.0/"));
        }
    }
    tree.to_string_of(datasets)
}

fn xml_stream(text: &str) -> Stream {
    Stream::flate(Dict::new(), text.as_bytes())
}

/// Write the (edited) datasets packet back into the PDF.
pub fn write_datasets(doc: &mut Document, xdp: &Xdp) -> Result<(), XfaError> {
    let datasets = xdp.datasets.ok_or(XfaError::NoXfa)?;
    match &xdp.layout {
        Layout::Single(r) => {
            let mut text = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
            xdp.tree.write(Tree::ROOT, &mut text);
            doc.set(*r, Object::Stream(xml_stream(&text)));
        }
        Layout::Array { array, parts } => {
            let text = datasets_text(xdp, datasets);
            let mut parts = parts.clone();
            match parts.iter().position(|(n, _)| n == "datasets") {
                Some(i) => match parts.get(i).and_then(|(_, o)| o.as_ref()) {
                    Some(r) => doc.set(r, Object::Stream(xml_stream(&text))),
                    None => {
                        let r = doc.add(Object::Stream(xml_stream(&text)));
                        if let Some(p) = parts.get_mut(i) {
                            p.1 = Object::Ref(r);
                        }
                    }
                },
                None => {
                    let r = doc.add(Object::Stream(xml_stream(&text)));
                    // Before the closing packets, as Acrobat orders them.
                    let at = parts.iter().position(|(n, _)| n == "postamble" || n == "xmpmeta" || n == "</xdp:xdp>").unwrap_or(parts.len());
                    parts.insert(at, ("datasets".to_string(), Object::Ref(r)));
                }
            }
            let value = Object::Array(parts.into_iter().flat_map(|(n, o)| [Object::String(PdfString::literal(n.into_bytes())), o]).collect());
            match array {
                Some(r) => doc.set(*r, value),
                None => set_xfa_entry(doc, value)?,
            }
        }
    }
    Ok(())
}

fn set_xfa_entry(doc: &mut Document, value: Object) -> Result<(), XfaError> {
    let (root, af_ref, mut af) = acroform_ref(doc).ok_or(XfaError::NoXfa)?;
    af.set(b"XFA".to_vec(), value);
    match af_ref {
        Some(r) => doc.set(r, Object::Dict(af)),
        None => doc.update_dict(root, |c| c.set(b"AcroForm".to_vec(), Object::Dict(af))).map_err(|e| XfaError::Pdf(e.to_string()))?,
    }
    Ok(())
}
