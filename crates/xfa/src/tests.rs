use std::sync::Arc;

use printcraft_cos::{Document, SaveOptions, write_incremental};
use printcraft_forms::{FieldKind, FieldValue};

use super::*;

/// A synthetic dynamic XFA form in the style of a LiveCycle government form: a flowed root, a
/// letter page area, a positioned first page with a title, text, date, check box, radio group
/// and drop-down; a repeating table row; and a second page forced by a break.
pub(crate) const TEMPLATE: &str = r##"<template xmlns="http://www.xfa.org/schema/xfa-template/3.3/">
<subform name="form1" layout="tb" locale="en_US">
  <pageSet>
    <pageArea name="Page1" id="Page1">
      <contentArea x="0.25in" y="0.25in" w="8in" h="10.5in"/>
      <medium stock="letter" short="8.5in" long="11in"/>
      <draw name="Footer" x="0.25in" y="10.6in" w="8in" h="0.2in"><value><text>SYNTHETIC FORM 1, OCT 2026</text></value><font typeface="Arial" size="7pt"/></draw>
    </pageArea>
  </pageSet>
  <proto><field id="stdText" h="0.35in"><font typeface="Arial" size="9pt"/><ui><textEdit><border><edge/></border></textEdit></ui><margin topInset="1mm" leftInset="1mm"/></field></proto>
  <subform name="Page1" w="8in" h="5in">
    <draw name="Title" x="0" y="0" w="8in" h="0.4in"><value><text>REQUEST FOR LEAVE</text></value><font typeface="Arial" size="14pt" weight="bold"/><para hAlign="center"/></draw>
    <draw name="Rule" x="0" y="0.45in" w="8in" h="0"><value><line><edge thickness="1pt"/></line></value></draw>
    <field name="Name" x="0" y="0.6in" w="5in" use="#stdText">
      <caption reserve="1in"><value><text>1. NAME</text></value></caption>
      <assist><toolTip>Last, First MI</toolTip></assist>
    </field>
    <field name="Date" x="5.2in" y="0.6in" w="2.8in" h="0.35in">
      <ui><dateTimeEdit/></ui><format><picture>date{YYYYMMDD}</picture></format>
      <caption reserve="0.6in"><value><text>2. DATE</text></value></caption>
    </field>
    <field name="Agree" x="0" y="1.1in" w="3in" h="0.3in">
      <ui><checkButton size="10pt"/></ui><items><integer>1</integer><integer>0</integer></items>
      <caption placement="right" reserve="2.6in"><value><text>I certify this is correct</text></value></caption>
    </field>
    <exclGroup name="Type" x="0" y="1.5in" w="6in" h="0.3in" layout="lr-tb">
      <field name="Ordinary" w="2in" h="0.3in"><ui><checkButton shape="round"/></ui><items><text>ORD</text></items><caption placement="right" reserve="1.6in"><value><text>Ordinary</text></value></caption></field>
      <field name="Emergency" w="2in" h="0.3in"><ui><checkButton shape="round"/></ui><items><text>EMG</text></items><caption placement="right" reserve="1.6in"><value><text>Emergency</text></value></caption></field>
    </exclGroup>
    <field name="Branch" x="0" y="2in" w="3in" h="0.35in">
      <ui><choiceList/></ui>
      <items><text>Army</text><text>Navy</text></items>
      <items save="1" presence="hidden"><text>A</text><text>N</text></items>
    </field>
    <field name="Remarks" x="0" y="2.5in" w="8in" h="1in" access="readOnly"><ui><textEdit multiLine="1"/></ui><value><text>Default remark</text></value></field>
  </subform>
  <subform name="Itinerary" layout="tb" w="8in">
    <subform name="Row" layout="lr-tb" w="8in"><occur min="1" max="-1"/>
      <field name="Place" w="4in" h="0.3in" use="#stdText"/>
      <field name="Days" w="2in" h="0.3in" use="#stdText"/>
    </subform>
  </subform>
  <subform name="Page2" w="8in" h="2in">
    <breakBefore targetType="pageArea" startNew="1"/>
    <field name="Signature" x="0" y="0" w="4in" h="0.5in"><ui><signature/></ui></field>
    <draw name="Note" x="0" y="0.6in" w="8in" h="0.5in"><value><exData contentType="text/html"><body xmlns="http://www.w3.org/1999/xhtml"><p style="font-weight:bold">Privacy Act Statement</p></body></exData></value></draw>
  </subform>
</subform>
</template>"##;

pub(crate) const DATASETS: &str = r#"<xfa:datasets xmlns:xfa="http://www.xfa.org/schema/xfa-data/1.0/"><xfa:data><form1><Page1><Name>DOE, JANE A</Name><Date>20261004</Date><Type>EMG</Type><Branch>N</Branch></Page1><Itinerary><Row><Place>Fort Example</Place><Days>3</Days></Row><Row><Place>Home</Place><Days>10</Days></Row></Itinerary></form1></xfa:data></xfa:datasets>"#;

fn pdf(objs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

fn stream(data: &str) -> Vec<u8> {
    format!("<< /Length {} >>\nstream\n{data}\nendstream", data.len()).into_bytes()
}

/// A dynamic XFA PDF (placeholder page, `/NeedsRendering`, usage rights) built from packets.
pub(crate) fn xfa_pdf(template: &str, datasets: &str) -> Vec<u8> {
    let objs = vec![
        b"<< /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R /NeedsRendering true /Perms << /UR3 8 0 R >> >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 9 0 R >>".to_vec(),
        b"<< /Fields [] /XFA [(preamble) 5 0 R (template) 6 0 R (datasets) 7 0 R (postamble) 10 0 R] >>".to_vec(),
        stream(r#"<xdp:xdp xmlns:xdp="http://ns.adobe.com/xdp/">"#),
        stream(template),
        stream(datasets),
        b"<< /Type /Sig /Filter /Adobe.PPKLite /SubFilter /adbe.pkcs7.detached >>".to_vec(),
        stream("BT /F1 12 Tf 72 700 Td (Please wait...) Tj ET"),
        stream("</xdp:xdp>"),
    ];
    pdf(&objs)
}

pub(crate) fn open_doc(bytes: Vec<u8>) -> Document {
    open(bytes)
}

pub(crate) fn reopen_doc(doc: &Document) -> Document {
    reopen(doc)
}

fn open(bytes: Vec<u8>) -> Document {
    Document::open(Arc::new(bytes)).unwrap_or_else(|_| Document::new_empty())
}

fn reopen(doc: &Document) -> Document {
    let bytes = write_incremental(doc, &SaveOptions::default()).unwrap_or_default();
    assert!(hayro_syntax::Pdf::new(bytes.clone()).is_ok(), "hayro-syntax parses the output");
    open(bytes)
}

fn field<'a>(all: &'a [printcraft_forms::Field], name: &str) -> &'a printcraft_forms::Field {
    match all.iter().find(|f| f.name == name) {
        Some(f) => f,
        None => panic!("no field {name}; have {:?}", all.iter().map(|f| &f.name).collect::<Vec<_>>()),
    }
}

#[test]
fn dynamic_form_becomes_pages_and_fields() {
    let mut doc = open(xfa_pdf(TEMPLATE, DATASETS));
    assert_eq!(kind(&doc), Some(Kind::Dynamic));
    let report = realize(&mut doc).unwrap_or_default();
    assert_eq!(report.pages, 2, "{report:?}");
    let doc = reopen(&doc);
    let all = printcraft_forms::fields(&doc);
    let name = field(&all, "form1[0].Page1[0].Name[0]");
    assert_eq!(name.kind, FieldKind::Text);
    assert_eq!(name.value, vec!["DOE, JANE A".to_string()]);
    assert_eq!(name.tooltip.as_deref(), Some("Last, First MI"));
    // The prototype's font size reached the field.
    assert!(name.da.contains(" 9 Tf"), "{}", name.da);
    // The date shows through its display picture (here the same as the data).
    assert_eq!(field(&all, "form1[0].Page1[0].Date[0]").value, vec!["20261004".to_string()]);
    assert_eq!(field(&all, "form1[0].Page1[0].Agree[0]").kind, FieldKind::CheckBox);
    let ty = field(&all, "form1[0].Page1[0].Type[0]");
    assert_eq!(ty.kind, FieldKind::Radio);
    assert_eq!(ty.value, vec!["EMG".to_string()]);
    assert_eq!(ty.widgets.len(), 2);
    let branch = field(&all, "form1[0].Page1[0].Branch[0]");
    assert_eq!(branch.kind, FieldKind::Combo);
    assert_eq!(branch.value, vec!["N".to_string()]);
    assert_eq!(branch.display_value(), "Navy");
    let remarks = field(&all, "form1[0].Page1[0].Remarks[0]");
    assert!(remarks.read_only());
    assert_eq!(remarks.value, vec!["Default remark".to_string()]);
    // Both data rows produced a row instance.
    assert_eq!(field(&all, "form1[0].Itinerary[0].Row[1].Place[0]").value, vec!["Home".to_string()]);
    // The signature field landed on page 2 (the break).
    let sig = field(&all, "form1[0].Page2[0].Signature[0]");
    assert_eq!(sig.kind, FieldKind::Signature);
    assert_eq!(sig.widgets.first().and_then(|w| w.page), Some(1));
    // The XFA stays for Acrobat, and it still re-renders from it.
    assert!(packet::xfa_entry(&doc).is_some());
    assert_eq!(kind(&doc), Some(Kind::Dynamic));
}

#[test]
fn filled_values_reach_the_xfa_data() {
    let mut doc = open(xfa_pdf(TEMPLATE, DATASETS));
    assert!(realize(&mut doc).is_ok());
    let mut doc = reopen(&doc);
    // Nothing changed yet: nothing to write.
    assert_eq!(sync_datasets(&mut doc), Ok(0));
    let set = |doc: &mut Document, n: &str, v: FieldValue| assert!(printcraft_forms::set_value(doc, n, &v).is_ok(), "{n}");
    set(&mut doc, "form1[0].Page1[0].Name[0]", FieldValue::Text("ROE, RICHARD".into()));
    set(&mut doc, "form1[0].Page1[0].Date[0]", FieldValue::Text("20261225".into()));
    set(&mut doc, "form1[0].Page1[0].Agree[0]", FieldValue::Check(true));
    set(&mut doc, "form1[0].Page1[0].Type[0]", FieldValue::Radio(Some("ORD".into())));
    set(&mut doc, "form1[0].Page1[0].Branch[0]", FieldValue::Choice(vec!["A".into()]));
    set(&mut doc, "form1[0].Itinerary[0].Row[0].Days[0]", FieldValue::Text("4".into()));
    assert_eq!(sync_datasets(&mut doc), Ok(6));
    let doc = reopen(&doc);
    let xdp = packet::read(&doc).ok();
    let data = xdp.as_ref().and_then(|x| x.datasets.map(|d| x.tree.to_string_of(d))).unwrap_or_default();
    for want in [
        "<Name>ROE, RICHARD</Name>",
        "<Date>20261225</Date>",
        "<Agree>1</Agree>",
        "<Type>ORD</Type>",
        "<Branch>A</Branch>",
        "<Days>4</Days>",
        "<Place>Home</Place>",
    ] {
        assert!(data.contains(want), "{want} missing from {data}");
    }
    // The datasets packet stays a standalone XML fragment with its namespace.
    assert!(data.starts_with(r#"<xfa:datasets xmlns:xfa="#), "{data}");
    // The usage-rights signature no longer matches the document: it is gone.
    let root = doc.root().map(|r| doc.get(r));
    assert!(root.as_ref().and_then(|c| c.as_dict()).is_some_and(|c| !c.contains(b"Perms")));
    // Reopening re-realizes from the saved data.
    let mut again = doc.clone();
    assert!(realize(&mut again).is_ok());
    let all = printcraft_forms::fields(&again);
    assert_eq!(field(&all, "form1[0].Page1[0].Name[0]").value, vec!["ROE, RICHARD".to_string()]);
    assert_eq!(field(&all, "form1[0].Page1[0].Type[0]").value, vec!["ORD".to_string()]);
}

#[test]
fn display_pictures_convert_both_ways() {
    let template = TEMPLATE.replace("date{YYYYMMDD}", "date{MM/DD/YYYY}");
    let datasets = DATASETS.replace("<Date>20261004</Date>", "<Date>2026-10-04</Date>");
    let mut doc = open(xfa_pdf(&template, &datasets));
    assert!(realize(&mut doc).is_ok());
    let mut doc = reopen(&doc);
    let all = printcraft_forms::fields(&doc);
    assert_eq!(field(&all, "form1[0].Page1[0].Date[0]").value, vec!["10/04/2026".to_string()]);
    assert!(printcraft_forms::set_value(&mut doc, "form1[0].Page1[0].Date[0]", &FieldValue::Text("12/25/2026".into())).is_ok());
    assert_eq!(sync_datasets(&mut doc), Ok(1));
    let xdp = packet::read(&doc).ok();
    let data = xdp.as_ref().and_then(|x| x.datasets.map(|d| x.tree.to_string_of(d))).unwrap_or_default();
    assert!(data.contains("<Date>2026-12-25</Date>"), "{data}");
}

#[test]
fn missing_datasets_are_created() {
    // Same length, so the cross-reference offsets stay right.
    let bytes = String::from_utf8_lossy(&xfa_pdf(TEMPLATE, "")).replace("(datasets) 7 0 R ", &" ".repeat(17));
    let mut doc = open(bytes.into_bytes());
    assert!(realize(&mut doc).is_ok());
    let mut doc = reopen(&doc);
    assert!(printcraft_forms::set_value(&mut doc, "form1[0].Page1[0].Name[0]", &FieldValue::Text("X".into())).is_ok());
    assert_eq!(sync_datasets(&mut doc), Ok(1));
    let xdp = packet::read(&reopen(&doc)).ok();
    let data = xdp.as_ref().and_then(|x| x.datasets.map(|d| x.tree.to_string_of(d))).unwrap_or_default();
    assert!(data.contains("<form1><Page1><Name>X</Name></Page1></form1>"), "{data}");
}

#[test]
fn hostile_xfa_never_panics() {
    let cases = [
        String::new(),
        "<template>".into(),
        "<template><subform/></template>".into(),
        "<template><subform layout=\"tb\"><subform><occur min=\"99999999\" max=\"-1\"/><field w=\"-5in\" h=\"NaN\"/></subform></subform></template>".into(),
        format!("<template><subform>{}</subform></template>", "<subform layout=\"tb\">".repeat(300)),
        "<template><subform layout=\"tb\"><subform h=\"100in\"><breakBefore targetType=\"pageArea\" target=\"#nope\"/></subform><subform h=\"100in\"/></subform></template>".into(),
        "<template><subform><field use=\"#a\" id=\"a\"><ui><choiceList/></ui><items/><items save=\"1\"/></field><draw><value><image>!!!</image></value></draw></subform></template>".into(),
    ];
    for t in cases {
        let mut doc = open(xfa_pdf(&t, DATASETS));
        let _ = realize(&mut doc);
        let _ = sync_datasets(&mut doc);
    }
    // A document without XFA.
    let mut doc = Document::new_empty();
    assert_eq!(kind(&doc), None);
    assert_eq!(realize(&mut doc), Err(XfaError::NoXfa));
    assert_eq!(sync_datasets(&mut doc), Err(XfaError::NoXfa));
}

#[test]
fn page_overflow_breaks_pages() {
    // 60 rows of 0.3in can't fit on one 10.5in content area.
    let rows: String = (0..60).map(|i| format!("<Row><Place>P{i}</Place></Row>")).collect();
    let datasets = DATASETS.replace("<Row><Place>Fort Example</Place><Days>3</Days></Row>", &rows);
    let mut doc = open(xfa_pdf(TEMPLATE, &datasets));
    let report = realize(&mut doc).unwrap_or_default();
    assert!(report.pages >= 3, "{report:?}");
    let all = printcraft_forms::fields(&doc);
    let last = field(&all, "form1[0].Itinerary[0].Row[59].Place[0]");
    assert!(last.widgets.first().and_then(|w| w.page).is_some_and(|p| p >= 1));
}

/// Writes the synthetic form to `$XFA_FIXTURE_OUT` (for looking at it with `realize`):
/// `XFA_FIXTURE_OUT=/tmp/x.pdf cargo test -p printcraft-xfa write_fixture -- --ignored`.
#[test]
#[ignore]
fn write_fixture() {
    if let Ok(path) = std::env::var("XFA_FIXTURE_OUT") {
        assert!(std::fs::write(path, xfa_pdf(TEMPLATE, DATASETS)).is_ok());
    }
}

/// A static XFA form: real pages and an AcroForm mirror named by SOM, as LiveCycle saves them.
fn static_xfa_pdf() -> Vec<u8> {
    let template = r#"<template><subform name="form1"><pageSet><pageArea name="P"><contentArea w="8in" h="10in"/></pageArea></pageSet><subform name="Page1" w="8in" h="10in"><field name="Name" w="3in" h="0.3in"><ui><textEdit/></ui></field><field name="Ok" y="1in" w="0.3in" h="0.3in"><ui><checkButton/></ui><items><text>Y</text><text>N</text></items></field></subform></subform></template>"#;
    let datasets = r#"<xfa:datasets xmlns:xfa="http://www.xfa.org/schema/xfa-data/1.0/"><xfa:data><form1><Page1><Name>Old</Name></Page1></form1></xfa:data></xfa:datasets>"#;
    let objs = vec![
        b"<< /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Annots [10 0 R 11 0 R] >>".to_vec(),
        b"<< /Fields [8 0 R] /XFA [(template) 5 0 R (datasets) 6 0 R] /DA (/Helv 0 Tf 0 g) >>".to_vec(),
        stream(template),
        stream(datasets),
        b"null".to_vec(),
        b"<< /T (form1[0]) /Kids [9 0 R] >>".to_vec(),
        b"<< /T (Page1[0]) /Parent 8 0 R /Kids [10 0 R 11 0 R] >>".to_vec(),
        b"<< /Type /Annot /Subtype /Widget /FT /Tx /T (Name[0]) /Parent 9 0 R /V (Old) /Rect [36 700 252 722] /P 3 0 R >>".to_vec(),
        b"<< /Type /Annot /Subtype /Widget /FT /Btn /T (Ok[0]) /Parent 9 0 R /V /Off /AS /Off /Rect [36 640 58 662] /P 3 0 R /AP << /N << /1 7 0 R /Off 7 0 R >> >> >>".to_vec(),
    ];
    pdf(&objs)
}

#[test]
fn static_forms_save_values_into_the_xfa_data() {
    let mut doc = open(static_xfa_pdf());
    assert_eq!(kind(&doc), Some(Kind::Static));
    assert_eq!(sync_datasets(&mut doc), Ok(0));
    assert!(printcraft_forms::set_value(&mut doc, "form1[0].Page1[0].Name[0]", &FieldValue::Text("New".into())).is_ok());
    assert!(printcraft_forms::set_value(&mut doc, "form1[0].Page1[0].Ok[0]", &FieldValue::Check(true)).is_ok());
    assert_eq!(sync_datasets(&mut doc), Ok(2));
    let xdp = packet::read(&reopen(&doc)).ok();
    let data = xdp.as_ref().and_then(|x| x.datasets.map(|d| x.tree.to_string_of(d))).unwrap_or_default();
    // Acrobat's state name ("1") maps to the XFA on-value ("Y").
    assert!(data.contains("<Name>New</Name>") && data.contains("<Ok>Y</Ok>"), "{data}");
}
