//! printcraft-xfa — XFA forms (XML Forms Architecture 3.3; ISO 32000-2 §12.7.9).
//!
//! Many government and military forms are XFA forms made with LiveCycle Designer. Dynamic ones
//! (`/NeedsRendering true`) carry no real pages: every viewer but Acrobat shows a "Please wait…"
//! placeholder. This crate:
//!
//! - [`realize`]: lays out the XFA template merged with its data and writes real pages and
//!   AcroForm fields (named by their SOM expressions, e.g. `form1[0].Page1[0].Name[0]`) in place
//!   of the placeholder. The XFA stays, so Acrobat still re-renders the form from it.
//! - runs the form's scripts (JavaScript and FormCalc, [`script`]): `initialize`, calculations
//!   and `ready` events on open ([`realize_with`]); `change`, `click`, `exit`, validations and
//!   calculations when a value changes ([`after_edit`]) or a button is pressed ([`click`]).
//!   When scripts show or hide parts of the form or add or remove rows, the pages are laid
//!   out again.
//! - [`sync_datasets`]: before saving, copies the AcroForm field values into the XFA data
//!   (`datasets` packet). Acrobat reads values from there, so without this a form filled
//!   anywhere else opens empty in Acrobat. Works for static XFA forms too.
//!
//! Not supported: linked images and fonts, barcodes, splitting text across pages, and script
//! features that need a person or a server (dialogs that ask questions, web services).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::collections::{HashMap, HashSet};

use printcraft_cos::{Document, Object};

pub mod dom;
pub mod formcalc;
pub mod host;
pub mod js;
pub mod layout;
pub mod merge;
pub mod packet;
pub mod picture;
pub mod realize;
pub mod script;
pub mod template;
pub mod xml;

#[cfg(test)]
mod script_tests;
#[cfg(test)]
mod tests;

pub use host::Output;
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
    #[error("the form has no field named {0}")]
    NoField(String),
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

/// How scripts run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    /// Run the form's scripts (Preferences ▸ JavaScript turns them off).
    pub scripts: bool,
    /// Today's date (year, month, day), for `Date()` and similar.
    pub today: (i32, u32, u32),
}

impl Default for Options {
    fn default() -> Self {
        Options { scripts: true, today: (2000, 1, 1) }
    }
}

/// What a fill-in or a button press did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    /// What the scripts said or asked for (messages, errors, failed validations).
    pub output: Output,
    /// The pages were laid out again (scripts showed, hid, added or removed parts).
    pub relaid: bool,
    /// Field values the scripts changed (calculations).
    pub values: usize,
}

/// Lay out the live form: the body through its page areas, then each page's master page
/// content (as forms 1…, one per page).
fn lay_out(live: &mut dom::Live) -> Vec<layout::Page> {
    let (pages, masters) = {
        let tree = &live.xdp.tree;
        let root = live.root;
        let data = Some(live.data_root);
        let Some(body) = live.forms.first() else { return Vec::new() };
        let areas = layout::page_areas(tree, root);
        let first_w = areas.first().map(|a| {
            let size = layout::medium(tree, *a);
            layout::content_areas(tree, *a, size).first().map(|c| c[2]).unwrap_or(size.0)
        });
        let sizer = layout::Sizer { t: tree, form: body };
        let root_box = sizer.arrange(0, first_w.unwrap_or(576.0), 0);
        let mut pag = layout::Paginator::new(tree, body, areas.clone());
        pag.place_root(&root_box);
        let mut pages = pag.pages;
        let root_som = body.get(0).map(|r| r.som.clone()).unwrap_or_default();
        let mut masters = Vec::new();
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
            let fi = masters.len() + 1;
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
            masters.push(form);
        }
        (pages, masters)
    };
    live.set_masters(masters);
    pages
}

/// Which page each laid-out instance is on (its first page).
fn page_map(pages: &[layout::Page]) -> HashMap<dom::N, usize> {
    let mut m = HashMap::new();
    for (p, page) in pages.iter().enumerate() {
        for it in &page.items {
            m.entry(dom::N { form: it.form, inst: it.inst }).or_insert(p);
        }
    }
    m
}

/// Lay out with the scripts' help: master page scripts (page numbers) and `layout:ready`,
/// once more when those changed what layout depends on.
fn lay_out_scripted(engine: &mut script::Engine, first: bool) -> Vec<layout::Page> {
    let mut pages = Vec::new();
    for round in 0..2 {
        let laid = engine.host().map(|mut h| {
            let p = lay_out(&mut h.live);
            h.page_of = page_map(&p);
            h.page_count = p.len().max(1);
            h.live.layout_changed = false;
            p
        });
        pages = laid.unwrap_or_default();
        script::masters(engine);
        if first && round == 0 {
            engine.set_event("ready", None, "", "");
            engine.fire_global(0, "$layout", "ready");
        }
        if !engine.host().is_some_and(|h| h.live.layout_changed) {
            break;
        }
    }
    if first {
        engine.set_event("docReady", None, "", "");
        engine.fire_global(0, "$host", "docReady");
    }
    pages
}

/// The live form with PrintCraft's saved script changes applied.
fn load(doc: &Document) -> Result<dom::Live, XfaError> {
    let mut live = dom::Live::load(doc)?;
    if let Some(state) = dom::read_state(doc) {
        live.apply_state(&state);
    }
    Ok(live)
}

/// Lay out a dynamic XFA form and write it as real pages and AcroForm fields, running its
/// scripts (see [`realize_with`]).
pub fn realize(doc: &mut Document) -> Result<Report, XfaError> {
    realize_with(doc, &Options::default())
}

/// [`realize`] with options: when scripts run, `initialize`, the calculations and the `ready`
/// events run before the pages are written, and master page scripts after layout.
pub fn realize_with(doc: &mut Document, opts: &Options) -> Result<Report, XfaError> {
    let live = load(doc)?;
    let scripted = script::has_scripts(&live);
    let mut notes = Vec::new();
    let (mut live, output, pages) = if opts.scripts && scripted {
        let today = opts.today;
        let ran = script::on_thread(move || -> Result<(dom::Live, Output, Vec<layout::Page>), String> {
            let mut engine = script::Engine::new(host::Host::new(live, today))?;
            script::open(&mut engine);
            let pages = lay_out_scripted(&mut engine, true);
            let (live, out) = script::finish(engine).ok_or("the scripts' results were lost")?;
            Ok((live, out, pages))
        });
        match ran {
            Ok(Ok(r)) => r,
            Ok(Err(e)) | Err(e) => {
                notes.push(format!("the scripts could not run ({e})"));
                let mut live = load(doc)?;
                let pages = lay_out(&mut live);
                (live, Output::default(), pages)
            }
        }
    } else {
        let mut live = live;
        let pages = lay_out(&mut live);
        if scripted {
            notes.push("scripts not run (JavaScript is turned off)".into());
        }
        (live, Output::default(), pages)
    };
    if pages.is_empty() {
        return Err(XfaError::NoTemplate);
    }
    let mut report = realize::write(doc, &live.xdp.tree, &live.forms, &pages)?;
    live.save(doc)?;
    if !output.errors.is_empty() {
        notes.push(format!("{} script errors", output.errors.len()));
    }
    report.notes.extend(notes);
    report.output = output;
    Ok(report)
}

/// Run a static XFA form's opening scripts (`initialize`, calculations, `ready`) and show
/// their results in its AcroForm fields. Returns whether anything changed.
pub fn open_static(doc: &mut Document, opts: &Options) -> Result<Outcome, XfaError> {
    run_job(doc, opts, Job::Open)
}

/// After the user changed field values (`edited`: their AcroForm names): run those fields'
/// `change`, `click` and `exit` events and validations, then the calculations; show the
/// results, laying the pages out again when needed. With no names (a form reset, a script's
/// changes) only the calculations run. Also keeps the XFA data in step with the fields.
pub fn after_edit(doc: &mut Document, edited: &[String], opts: &Options) -> Result<Outcome, XfaError> {
    run_job(doc, opts, Job::Edit(edited.iter().map(|e| dom::normalize(e)).collect()))
}

/// Press button `field` (its AcroForm name): its `click` scripts (add a row, show a section,
/// …) run, then the calculations.
pub fn click(doc: &mut Document, field: &str, opts: &Options) -> Result<Outcome, XfaError> {
    run_job(doc, opts, Job::Click(dom::normalize(field)))
}

/// Whether field `field` has `click` scripts (so pressing it does something).
pub fn has_click(doc: &Document, field: &str) -> bool {
    let Ok(live) = load(doc) else { return false };
    let Some(n) = live.field_map().get(&dom::normalize(field)).copied().or_else(|| button_named(&live, field)) else { return false };
    ["click", "mouseUp", "mouseDown"].iter().any(|a| !script::event_scripts(&live, n, a, "$").is_empty())
}

/// Buttons aren't value fields: find them by SOM among all instances.
fn button_named(live: &dom::Live, field: &str) -> Option<dom::N> {
    let want = dom::normalize(field);
    live.walk(0).into_iter().find(|n| live.inst(*n).is_some_and(|i| dom::normalize(&i.som) == want))
}

enum Job {
    Open,
    Edit(HashSet<String>),
    Click(String),
}

fn run_job(doc: &mut Document, opts: &Options, job: Job) -> Result<Outcome, XfaError> {
    let k = kind(doc).ok_or(XfaError::NoXfa)?;
    let mut live = load(doc)?;
    let changed = live.read_acroform(doc);
    let scripted = opts.scripts && script::has_scripts(&live);
    if !scripted {
        if matches!(job, Job::Click(_)) {
            return Ok(Outcome::default());
        }
        live.save(doc)?;
        return Ok(Outcome::default());
    }
    let changes: Vec<script::Change> = match &job {
        Job::Edit(wanted) => changed
            .into_iter()
            .filter(|(n, _)| live.inst(*n).is_some_and(|i| wanted.contains(&dom::normalize(&i.som))))
            .map(|(n, before)| script::Change { field: n, after: live.raw(n), before })
            .collect(),
        _ => Vec::new(),
    };
    let target = match &job {
        Job::Click(name) => Some(button_named(&live, name).ok_or_else(|| XfaError::NoField(name.clone()))?),
        _ => None,
    };
    live.layout_changed = false;
    live.changed.clear();
    let dynamic = k == Kind::Dynamic;
    let today = opts.today;
    let open = matches!(job, Job::Open);
    let ran = script::on_thread(move || -> Result<(dom::Live, Output, Vec<layout::Page>), String> {
        let mut engine = script::Engine::new(host::Host::new(live, today))?;
        if open {
            script::open(&mut engine);
        } else if let Some(n) = target {
            script::click(&mut engine, n);
        } else {
            script::edit(&mut engine, &changes);
        }
        let relayout = dynamic && engine.host().is_some_and(|h| h.live.layout_changed);
        let pages = if relayout { lay_out_scripted(&mut engine, false) } else { Vec::new() };
        let (live, out) = script::finish(engine).ok_or("the scripts' results were lost")?;
        Ok((live, out, pages))
    });
    let (mut live, output, pages) = match ran {
        Ok(Ok(r)) => r,
        Ok(Err(e)) | Err(e) => {
            // Keep the data in step even when the scripts can't run.
            let mut live = load(doc)?;
            live.read_acroform(doc);
            live.save(doc)?;
            let mut output = Output::default();
            output.errors.push(format!("the form's scripts could not run: {e}"));
            return Ok(Outcome { output, relaid: false, values: 0 });
        }
    };
    let mut outcome = Outcome { output, relaid: false, values: live.changed.len() };
    if !pages.is_empty() {
        realize::write(doc, &live.xdp.tree, &live.forms, &pages)?;
        outcome.relaid = true;
    } else {
        outcome.values = live.write_values(doc)?;
        if !dynamic {
            apply_static(doc, &live)?;
        }
    }
    live.save(doc)?;
    Ok(outcome)
}

/// Show visibility and read-only changes in a static form's AcroForm fields (its pages are
/// fixed, so hidden parts become hidden widgets).
fn apply_static(doc: &mut Document, live: &dom::Live) -> Result<(), XfaError> {
    let map = live.field_map();
    for f in printcraft_forms::fields(doc) {
        let Some(n) = map.get(&dom::normalize(&f.name)).copied() else { continue };
        let mut hidden = false;
        let mut at = Some(n);
        for _ in 0..128 {
            match at {
                Some(a) => {
                    if live.presence(a) != template::Presence::Visible {
                        hidden = true;
                        break;
                    }
                    at = live.parent(a);
                }
                None => break,
            }
        }
        let readonly = matches!(live.attr(n, &[], "access").as_deref(), Some("readOnly" | "protected" | "nonInteractive"));
        for w in &f.widgets {
            let flags = doc.get(w.obj).as_dict().and_then(|d| d.int(b"F")).unwrap_or(4);
            let want = if hidden { flags | 2 } else { flags & !2 };
            if want != flags {
                doc.update_dict(w.obj, |d| d.set(b"F".to_vec(), Object::Int(want))).map_err(|e| XfaError::Pdf(e.to_string()))?;
            }
        }
        let ff = doc.get(f.obj).as_dict().and_then(|d| d.int(b"Ff")).unwrap_or(0);
        let want = if readonly { ff | 1 } else { ff & !1 };
        if want != ff && (readonly || live.inst(n).is_some_and(|i| !i.ops.is_empty())) {
            doc.update_dict(f.obj, |d| d.set(b"Ff".to_vec(), Object::Int(want))).map_err(|e| XfaError::Pdf(e.to_string()))?;
        }
    }
    Ok(())
}

/// Copy the AcroForm field values into the XFA data. Returns how many field values changed.
/// When any did, the datasets packet is rewritten and the usage-rights signature (`/Perms
/// /UR3`), which the change invalidates, is removed.
pub fn sync_datasets(doc: &mut Document) -> Result<usize, XfaError> {
    let mut live = load(doc)?;
    let changed = live.read_acroform(doc).len();
    if changed > 0 {
        live.save(doc)?;
    }
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
