//! printcraft-xfa — XFA forms (XML Forms Architecture 3.3; ISO 32000-2 §12.7.9).
//!
//! Many government and military forms are XFA forms made with LiveCycle Designer. Dynamic ones
//! (`/NeedsRendering true`) carry no real pages: every viewer but Acrobat shows a "Please wait…"
//! placeholder. This crate:
//!
//! - [`realize`]: lays out the XFA template merged with its data and writes real pages and
//!   AcroForm fields (named by their SOM expressions, e.g. `form1[0].Page1[0].Name[0]`) in place
//!   of the placeholder. The XFA stays, so Acrobat still re-renders the form from it.
//! - [`sync_datasets`]: before saving, copies the AcroForm field values into the XFA data
//!   (`datasets` packet). Acrobat reads values from there, so without this a form filled
//!   anywhere else opens empty in Acrobat. Works for static XFA forms too.
//!
//! Not supported: scripts (JavaScript/FormCalc calculations, show/hide logic, adding rows at
//! run time), linked images and fonts, barcodes, and splitting text across pages.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::collections::HashMap;

use printcraft_cos::{Document, Object};
use printcraft_forms::FieldKind;

pub mod layout;
pub mod merge;
pub mod packet;
pub mod picture;
pub mod realize;
pub mod template;
pub mod xml;

#[cfg(test)]
mod tests;

pub use realize::Report;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum XfaError {
    #[error("the document has no XFA form")]
    NoXfa,
    #[error("the XFA form has no template")]
    NoTemplate,
    #[error("the XFA XML is malformed: {0}")]
    Xml(String),
    #[error("{0}")]
    Pdf(String),
}

/// How a document uses XFA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The pages are placeholders; the form must be laid out from the template.
    Dynamic,
    /// Real pages and AcroForm fields mirror the XFA form.
    Static,
}

/// Whether the document has an XFA form, and which kind.
pub fn kind(doc: &Document) -> Option<Kind> {
    packet::xfa_entry(doc)?;
    let (_, _, af) = packet::acroform_ref(doc)?;
    let catalog = doc.root().map(|r| doc.get(r));
    let needs = catalog
        .as_ref()
        .and_then(|c| c.as_dict())
        .and_then(|c| c.get(b"NeedsRendering"))
        .map(|o| doc.resolve(o))
        .is_some_and(|o| matches!(*o, Object::Bool(true)));
    let no_fields = af.get(b"Fields").map(|f| doc.resolve(f)).and_then(|f| f.as_array().map(Vec::is_empty)).unwrap_or(true);
    Some(if needs || no_fields { Kind::Dynamic } else { Kind::Static })
}

/// The template's root subform.
fn root_subform(tree: &xml::Tree, template: xml::NodeId) -> Option<xml::NodeId> {
    tree.child(template, "subform")
}

/// Merge and lay out the form: body plus one master-page form per page.
fn build(xdp: &mut packet::Xdp) -> Result<(Vec<merge::Form>, Vec<layout::Page>, usize), XfaError> {
    let template = xdp.template.ok_or(XfaError::NoTemplate)?;
    template::resolve_protos(&mut xdp.tree, template);
    let tree = &xdp.tree;
    let root = root_subform(tree, template).ok_or(XfaError::NoTemplate)?;
    let data = xdp.datasets.and_then(|d| tree.child(d, "data"));
    let body = merge::merge(tree, root, tree, data);
    let areas = layout::page_areas(tree, root);
    let first_w = areas.first().map(|a| {
        let size = layout::medium(tree, *a);
        layout::content_areas(tree, *a, size).first().map(|c| c[2]).unwrap_or(size.0)
    });
    let sizer = layout::Sizer { t: tree, form: &body };
    let root_box = sizer.arrange(0, first_w.unwrap_or(576.0), 0);
    let mut pag = layout::Paginator::new(tree, &body, areas.clone());
    pag.place_root(&root_box);
    let mut pages = pag.pages;
    // Master pages: each page's page area content, laid out at its absolute position.
    let mut forms = vec![body];
    let root_som = forms.first().and_then(|f| f.get(0)).map(|r| r.som.clone()).unwrap_or_default();
    let mut area_count: HashMap<xml::NodeId, usize> = HashMap::new();
    for page in &mut pages {
        let Some(a) = page.page_area else { continue };
        let k = area_count.entry(a).or_insert(0);
        let name = tree.attr(a, "name").filter(|n| !n.is_empty()).unwrap_or("#pageArea");
        let som = format!("{root_som}.#pageSet[0].{name}[{k}]");
        *k += 1;
        let form = merge::merge_page_area(tree, a, tree, data, &som);
        let sizer = layout::Sizer { t: tree, form: &form };
        let mut items = Vec::new();
        let fi = forms.len();
        for c in form.get(0).map(|r| r.children.clone()).unwrap_or_default() {
            if form.get(c).is_some_and(|i| i.presence == template::Presence::Hidden) {
                continue;
            }
            let b = sizer.arrange(c, page.width, 0);
            let (x, y) = {
                let n = form.get(c).map(|i| i.t).unwrap_or(0);
                (template::attr_measure(tree, n, "x").unwrap_or(0.0), template::attr_measure(tree, n, "y").unwrap_or(0.0))
            };
            layout::emit_all(&b, x, y, fi, &mut items, 0);
        }
        // Master page content goes under the body.
        items.append(&mut page.items);
        page.items = items;
        forms.push(form);
    }
    let scripts = tree.descendants(template).iter().filter(|n| tree.tag(**n) == "script" && !tree.text(**n).trim().is_empty()).count();
    Ok((forms, pages, scripts))
}

/// Lay out a dynamic XFA form and write it as real pages and AcroForm fields.
pub fn realize(doc: &mut Document) -> Result<Report, XfaError> {
    let mut xdp = packet::read(doc)?;
    let (forms, pages, scripts) = build(&mut xdp)?;
    if pages.is_empty() {
        return Err(XfaError::NoTemplate);
    }
    let mut report = realize::write(doc, &xdp.tree, &forms, &pages)?;
    if scripts > 0 {
        report.notes.push(format!("{scripts} scripts not run (calculations and show/hide logic)"));
    }
    Ok(report)
}

/// Drop "[0]" indexes so `a[0].b[1]` and `a.b[1]` compare equal.
fn normalize(som: &str) -> String {
    som.replace("[0]", "")
}

/// Copy the AcroForm field values into the XFA data. Returns how many data values changed.
/// When any did, the datasets packet is rewritten and the usage-rights signature (`/Perms
/// /UR3`), which the change invalidates, is removed.
pub fn sync_datasets(doc: &mut Document) -> Result<usize, XfaError> {
    let mut xdp = packet::read(doc)?;
    let template = xdp.template.ok_or(XfaError::NoTemplate)?;
    template::resolve_protos(&mut xdp.tree, template);
    let data_root = packet::data_root(&mut xdp);
    let tree = &xdp.tree;
    let root = root_subform(tree, template).ok_or(XfaError::NoTemplate)?;
    let mut forms = vec![merge::merge(tree, root, tree, Some(data_root))];
    let root_som = forms.first().and_then(|f| f.get(0)).map(|r| r.som.clone()).unwrap_or_default();
    for a in layout::page_areas(tree, root) {
        let name = tree.attr(a, "name").filter(|n| !n.is_empty()).unwrap_or("#pageArea");
        forms.push(merge::merge_page_area(tree, a, tree, Some(data_root), &format!("{root_som}.#pageSet[0].{name}[0]")));
    }
    let acro: HashMap<String, printcraft_forms::Field> = printcraft_forms::fields(doc).into_iter().map(|f| (normalize(&f.name), f)).collect();
    // (data path, new canonical value)
    let mut writes: Vec<(merge::DataPath, String)> = Vec::new();
    for form in &forms {
        for inst in &form.insts {
            let (Some(path), true) = (&inst.data, matches!(inst.tag, merge::Tag::Field | merge::Tag::ExclGroup)) else { continue };
            let Some(f) = acro.get(&normalize(&inst.som)) else { continue };
            let value = match (inst.tag, f.kind) {
                (merge::Tag::ExclGroup, _) => radio_value(tree, form, inst, f),
                (_, FieldKind::CheckBox) => {
                    let (on, off) = merge::check_values(tree, inst.t);
                    if f.value.first().is_some_and(|v| !v.is_empty() && v != "Off") { on } else { off }
                }
                (_, FieldKind::Text) => {
                    let v = f.value.first().cloned().unwrap_or_default();
                    match realize::date_display(tree, inst.t) {
                        Some(p) if !v.is_empty() => match picture::parse(&v, &p) {
                            // Stored as the data picture says (`<bind><picture>`), else canonical.
                            Some(iso) => data_date_pattern(tree, inst.t).and_then(|dp| picture::format(&iso, &dp)).unwrap_or(iso),
                            None => v,
                        },
                        _ => v,
                    }
                }
                (_, FieldKind::Combo | FieldKind::List) => f.value.join("\n"),
                _ => continue,
            };
            let current = merge::Data { tree, root: Some(data_root) }.find(path).map(|n| merge::data_value(tree, n));
            // Keep the spelling the data already uses for dates (`YYYYMMDD` vs `YYYY-MM-DD`).
            let value = match &current {
                Some(old)
                    if old.len() == 8
                        && picture::parse_canonical(old).is_some()
                        && value.len() == 10
                        && picture::parse_canonical(&value).is_some() =>
                {
                    value.replace('-', "")
                }
                _ => value,
            };
            let unchanged = match &current {
                Some(old) => *old == value || same_date(old, &value),
                // No data yet: only a value other than the template's default is news.
                None => value == inst.value || value.is_empty() || (inst.value.is_empty() && is_off_value(tree, inst, &value)),
            };
            if !unchanged {
                writes.push((path.clone(), value));
            }
        }
    }
    if writes.is_empty() {
        return Ok(0);
    }
    let changed = writes.len();
    for (path, value) in writes {
        let node = merge::ensure_path(&mut xdp.tree, data_root, &path);
        xdp.tree.set_text(node, &value);
    }
    packet::write_datasets(doc, &xdp)?;
    drop_usage_rights(doc);
    Ok(changed)
}

/// A date stored in another canonical spelling (`20261004` vs `2026-10-04`) is the same date.
fn same_date(a: &str, b: &str) -> bool {
    matches!((picture::parse_canonical(a), picture::parse_canonical(b)), (Some(x), Some(y)) if x == y)
}

/// The data picture of a field (`<bind><picture>date{…}</picture></bind>`).
fn data_date_pattern(tree: &xml::Tree, field: xml::NodeId) -> Option<String> {
    picture::date_pattern(&tree.text(tree.path(field, &["bind", "picture"])?))
}

/// A check box's off value: an untouched box with no data isn't a change.
fn is_off_value(tree: &xml::Tree, inst: &merge::Inst, value: &str) -> bool {
    inst.tag == merge::Tag::Field && tree.path(inst.t, &["ui", "checkButton"]).is_some() && merge::check_values(tree, inst.t).1 == value
}

/// The value of an exclusion group: the on-value of the member whose widget is selected.
fn radio_value(tree: &xml::Tree, form: &merge::Form, group: &merge::Inst, f: &printcraft_forms::Field) -> String {
    let Some(state) = f.value.first().filter(|v| !v.is_empty() && *v != "Off") else { return String::new() };
    let members: Vec<&merge::Inst> = group.children.iter().filter_map(|m| form.get(*m)).collect();
    let ons: Vec<String> = members.iter().map(|m| merge::check_values(tree, m.t).0).collect();
    if let Some(on) = ons.iter().find(|on| realize_on(on) == *state) {
        return on.clone();
    }
    // Acrobat's static mirrors may name states by position: the k-th widget is the k-th member.
    if let Some(k) = f.widgets.iter().position(|w| w.on_state.as_deref() == Some(state.as_str()))
        && let Some(on) = ons.get(k)
    {
        return on.clone();
    }
    state.clone()
}

fn realize_on(v: &str) -> String {
    let v: String = v.chars().filter(|c| !c.is_control()).collect();
    if v.is_empty() || v == "Off" { "On".into() } else { v }
}

/// Remove the usage-rights signature (`/Perms /UR3`) a changed document no longer satisfies.
fn drop_usage_rights(doc: &mut Document) {
    let Some(root) = doc.root() else { return };
    let perms = doc.get(root).as_dict().and_then(|c| c.get(b"Perms").cloned());
    let Some(perms) = perms else { return };
    let mut p = doc.resolve(&perms).as_dict().cloned().unwrap_or_default();
    if p.remove(b"UR3").is_none() && p.remove(b"UR").is_none() {
        return;
    }
    let _ = doc.update_dict(root, |c| {
        if p.is_empty() {
            c.remove(b"Perms");
        } else {
            c.set(b"Perms".to_vec(), Object::Dict(p));
        }
    });
}
