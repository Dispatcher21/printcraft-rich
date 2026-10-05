//! XFA forms in the real shell (egui_kittest): a field whose click script shows a pop-up
//! menu (as rank and leave-type pick lists do), and a button that adds a row.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use printcraft_ui_egui::PrintCraftApp;

/// A synthetic dynamic XFA form: a rank field with an `app.popUpMenuEx` pick list and a
/// table with an Add Row button.
fn xfa_pdf() -> Vec<u8> {
    let template = r##"<template xmlns="http://www.xfa.org/schema/xfa-template/3.3/"><subform name="form1" layout="tb">
      <pageSet><pageArea name="P"><contentArea x="0.25in" y="0.25in" w="4in" h="5in"/><medium short="4.5in" long="5.5in"/></pageArea></pageSet>
      <subform name="Head" w="4in" h="1in">
        <field name="RANK" x="0" y="0" w="2in" h="0.4in"><ui><textEdit/></ui><bind match="none"/>
          <event activity="click"><script contentType="application/x-javascript">
            var r = app.popUpMenuEx({cName:"Specialist",cReturn:"SPC"},{cName:"Sergeant",cReturn:"SGT"},{cName:"-"},{cName:"Captain",cReturn:"CPT"});
            this.rawValue = r;</script></event></field>
      </subform>
      <subform name="Items" layout="tb" w="4in">
        <subform name="Row" layout="lr-tb" w="4in"><occur min="1" max="5"/>
          <field name="Item" w="3in" h="0.4in"><ui><textEdit/></ui></field>
        </subform>
        <field name="AddRow" w="1in" h="0.4in"><ui><button/></ui><caption><value><text>Add</text></value></caption>
          <event activity="click"><script contentType="application/x-javascript">_Row.addInstance(1);</script></event></field>
      </subform>
    </subform></template>"##;
    let datasets = r#"<xfa:datasets xmlns:xfa="http://www.xfa.org/schema/xfa-data/1.0/"><xfa:data><form1/></xfa:data></xfa:datasets>"#;
    let stream = |d: &str| format!("<< /Length {} >>\nstream\n{d}\nendstream", d.len()).into_bytes();
    let objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R /NeedsRendering true >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 324 396] >>".to_vec(),
        b"<< /Fields [] /XFA [(preamble) 5 0 R (template) 6 0 R (datasets) 7 0 R (postamble) 8 0 R] >>".to_vec(),
        stream(r#"<xdp:xdp xmlns:xdp="http://ns.adobe.com/xdp/">"#),
        stream(template),
        stream(datasets),
        stream("</xdp:xdp>"),
    ];
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

fn harness() -> Harness<'static, PrintCraftApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(|_cc| {
        let mut app = PrintCraftApp::new();
        app.open_bytes("xfa.pdf", None, xfa_pdf()).unwrap();
        app.set_option("left", "closed").unwrap();
        app.set_option("zoom", "150").unwrap();
        app
    });
    for _ in 0..60 {
        h.run_steps(2);
        if !h.state().render_pending() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    h
}

fn value(h: &Harness<'static, PrintCraftApp>, name: &str) -> Option<Vec<String>> {
    let s = h.state();
    s.session.get(s.views[0].id).unwrap().form.iter().find(|f| f.name == name).map(|f| f.value.clone())
}

fn click_field(h: &mut Harness<'static, PrintCraftApp>, name: &str) {
    let p = {
        let s = h.state();
        let doc = s.session.get(s.views[0].id).unwrap();
        let f = doc.form.iter().find(|f| f.name == name).unwrap();
        printcraft_ui_egui::forms_ui::field_screen_rect(&s.views[0], &doc.info, f, 0).expect("on screen").center()
    };
    h.hover_at(p);
    h.run_steps(1);
    h.drag_at(p);
    h.run_steps(1);
    h.drop_at(p);
    h.run_steps(3);
}

#[test]
fn a_pick_list_field_shows_its_menu_and_fills_the_pick() {
    let mut h = harness();
    let rank = "form1[0].Head[0].RANK[0]";
    click_field(&mut h, rank);
    let menu = h.state().xfa_menu.clone().expect("the pick list opens");
    assert_eq!(menu.items.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(), ["Specialist", "Sergeant", "Captain"]);
    h.get_by_label("Sergeant").click();
    h.run_steps(4);
    assert!(h.state().xfa_menu.is_none());
    assert_eq!(value(&h, rank), Some(vec!["SGT".to_string()]));
    // Escape closes the menu and changes nothing.
    click_field(&mut h, rank);
    assert!(h.state().xfa_menu.is_some());
    h.key_press(egui::Key::Escape);
    h.run_steps(3);
    assert!(h.state().xfa_menu.is_none());
    assert_eq!(value(&h, rank), Some(vec!["SGT".to_string()]));
}

#[test]
fn an_xfa_button_adds_a_row() {
    let mut h = harness();
    assert!(value(&h, "form1[0].Items[0].Row[1].Item[0]").is_none());
    click_field(&mut h, "form1[0].Items[0].AddRow[0]");
    h.run_steps(4);
    assert!(value(&h, "form1[0].Items[0].Row[1].Item[0]").is_some(), "a second row");
    assert_eq!(h.state().session.get(h.state().views[0].id).unwrap().can_undo(), Some("Press AddRow"));
}
