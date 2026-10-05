//! Scripted XFA forms: calculations, show/hide, adding and removing rows, page numbers,
//! script objects, validations, and hostile scripts. Synthetic forms only.

use printcraft_cos::Document;
use printcraft_forms::{FieldValue, fields};

use super::tests::{open_doc, reopen_doc, xfa_pdf};
use super::*;

/// A purchase-request style form with the script patterns LiveCycle forms use.
const TEMPLATE: &str = r##"<template xmlns="http://www.xfa.org/schema/xfa-template/3.3/">
<subform name="form1" layout="tb" locale="en_US">
  <variables>
    <script name="Util" contentType="application/x-javascript">
      function twice(x) { return x * 2; }
      var label = "Total x2";
    </script>
  </variables>
  <pageSet>
    <pageArea name="Page1">
      <contentArea x="0.25in" y="0.25in" w="8in" h="10in"/>
      <medium stock="letter" short="8.5in" long="11in"/>
      <field name="PageNo" x="6in" y="10.5in" w="2in" h="0.25in">
        <ui><textEdit/></ui>
        <calculate><script contentType="application/x-javascript">xfa.layout.page(this) + " of " + xfa.layout.pageCount()</script></calculate>
      </field>
    </pageArea>
  </pageSet>
  <subform name="Header" w="8in" h="1.6in">
    <field name="Today" x="0" y="0" w="2in" h="0.3in">
      <ui><dateTimeEdit/></ui><value><date/></value><format><picture>date{MM/DD/YYYY}</picture></format>
      <event activity="initialize"><script>if (HasValue($) == 0) then $ = Num2Date(Date(), "YYYY-MM-DD") endif</script></event>
    </field>
    <field name="HasDeps" x="0" y="0.4in" w="3in" h="0.3in">
      <ui><checkButton/></ui><items><integer>1</integer><integer>0</integer></items>
      <event activity="click"><script contentType="application/x-javascript">Deps.presence = (this.rawValue == 1) ? "visible" : "hidden";</script></event>
    </field>
    <field name="Kind" x="3.5in" y="0.4in" w="2in" h="0.3in">
      <ui><choiceList/></ui><items><text>Supplies</text><text>Travel</text></items><items save="1" presence="hidden"><text>S</text><text>T</text></items>
      <event activity="change"><script contentType="application/x-javascript">Detail.rawValue = "Chose " + xfa.event.newText;</script></event>
    </field>
    <field name="Detail" x="0" y="0.8in" w="5in" h="0.3in"><ui><textEdit/></ui><bind match="none"/></field>
    <field name="Hello" x="5.5in" y="0.8in" w="2in" h="0.3in"><ui><button/></ui><caption><value><text>Hello</text></value></caption>
      <event activity="click"><script contentType="application/x-javascript">xfa.host.messageBox("Hello from the form", "Greeting");</script></event>
    </field>
  </subform>
  <subform name="Deps" w="8in" h="0.5in" presence="hidden">
    <field name="DepName" x="0" y="0" w="4in" h="0.3in"><ui><textEdit/></ui></field>
  </subform>
  <subform name="Items" layout="tb" w="8in">
    <subform name="Row" layout="lr-tb" w="8in"><occur min="1" max="5"/>
      <field name="Qty" w="1in" h="0.3in"><ui><numericEdit/></ui><value><integer/></value>
        <validate><script contentType="application/x-javascript">this.rawValue == null || this.rawValue &gt;= 0</script>
          <message><text name="scriptTest">Quantity can't be negative.</text></message></validate>
      </field>
      <field name="Price" w="1in" h="0.3in"><ui><numericEdit/></ui><value><decimal fracDigits="2"/></value></field>
      <field name="Amount" w="1.5in" h="0.3in" access="readOnly"><ui><numericEdit/></ui><value><decimal fracDigits="2"/></value>
        <calculate><script contentType="application/x-javascript">Qty.rawValue * Price.rawValue</script></calculate>
      </field>
    </subform>
    <field name="AddRow" w="1in" h="0.3in"><ui><button/></ui><caption><value><text>Add</text></value></caption>
      <event activity="click"><script contentType="application/x-javascript">_Row.addInstance(1);</script></event>
    </field>
    <field name="RemoveRow" w="1in" h="0.3in"><ui><button/></ui><caption><value><text>Remove</text></value></caption>
      <event activity="click"><script>_Row.removeInstance(_Row.count - 1)</script></event>
    </field>
  </subform>
  <subform name="Totals" w="8in" h="1in">
    <field name="Total" x="0" y="0" w="2in" h="0.3in" access="readOnly"><ui><numericEdit/></ui><value><decimal fracDigits="2"/></value>
      <calculate><script>Sum(Items.Row[*].Amount)</script></calculate>
    </field>
    <field name="Double" x="2.5in" y="0" w="2in" h="0.3in"><ui><numericEdit/></ui><value><decimal fracDigits="2"/></value><bind match="none"/>
      <calculate><script contentType="application/x-javascript">Util.twice(Total.rawValue)</script></calculate>
    </field>
    <field name="Words" x="0" y="0.4in" w="8in" h="0.3in"><ui><textEdit/></ui><bind match="none"/>
      <calculate><script>if (Total &gt; 0) then Concat(WordNum(Total), " (", Count(Items.Row[*].Qty), " rows)") else "none" endif</script></calculate>
    </field>
  </subform>
</subform>
</template>"##;

const DATA: &str = r#"<xfa:datasets xmlns:xfa="http://www.xfa.org/schema/xfa-data/1.0/"><xfa:data><form1><Header><HasDeps>0</HasDeps></Header><Items><Row><Qty>2</Qty><Price>10.50</Price></Row><Row><Qty>1</Qty><Price>4</Price></Row></Items></form1></xfa:data></xfa:datasets>"#;

const OPTS: Options = Options { scripts: true, today: (2026, 10, 5) };

fn value(doc: &Document, name: &str) -> Option<String> {
    fields(doc).into_iter().find(|f| f.name == name).map(|f| f.value.first().cloned().unwrap_or_default())
}

fn names(doc: &Document) -> Vec<String> {
    fields(doc).into_iter().map(|f| f.name).collect()
}

fn opened() -> Document {
    let mut doc = open_doc(xfa_pdf(TEMPLATE, DATA));
    let report = realize_with(&mut doc, &OPTS);
    assert!(report.as_ref().is_ok_and(|r| r.output.errors.is_empty()), "{report:?}");
    doc
}

#[test]
fn opening_runs_initialize_and_calculations() {
    let doc = opened();
    assert_eq!(value(&doc, "form1[0].Items[0].Row[0].Amount[0]").as_deref(), Some("21"));
    assert_eq!(value(&doc, "form1[0].Items[0].Row[1].Amount[0]").as_deref(), Some("4"));
    assert_eq!(value(&doc, "form1[0].Totals[0].Total[0]").as_deref(), Some("25"));
    assert_eq!(value(&doc, "form1[0].Totals[0].Double[0]").as_deref(), Some("50"));
    assert_eq!(value(&doc, "form1[0].Totals[0].Words[0]").as_deref(), Some("Twenty-five (2 rows)"));
    // initialize filled today's date, shown in the display picture.
    assert_eq!(value(&doc, "form1[0].Header[0].Today[0]").as_deref(), Some("10/05/2026"));
    // The master page's page number.
    assert_eq!(value(&doc, "form1[0].#pageSet[0].Page1[0].PageNo[0]").as_deref(), Some("1 of 1"));
    // The hidden section has no fields until it's shown.
    assert!(!names(&doc).iter().any(|n| n.contains("DepName")));
    // The calculated values went into the data.
    let live = dom::Live::load(&doc).ok();
    let data = live.as_ref().map(|l| l.tree().to_string_of(l.data_root)).unwrap_or_default();
    assert!(data.contains("<Amount>21</Amount>"), "{data}");
}

#[test]
fn editing_a_value_recalculates() {
    let mut doc = opened();
    let qty = "form1[0].Items[0].Row[0].Qty[0]";
    assert!(printcraft_forms::set_value(&mut doc, qty, &FieldValue::Text("3".into())).is_ok());
    let out = after_edit(&mut doc, &[qty.to_string()], &OPTS);
    assert!(out.as_ref().is_ok_and(|o| !o.relaid && o.values >= 2 && o.output.errors.is_empty()), "{out:?}");
    assert_eq!(value(&doc, "form1[0].Items[0].Row[0].Amount[0]").as_deref(), Some("31.5"));
    assert_eq!(value(&doc, "form1[0].Totals[0].Total[0]").as_deref(), Some("35.5"));
    assert_eq!(value(&doc, "form1[0].Totals[0].Double[0]").as_deref(), Some("71"));
}

#[test]
fn a_check_box_shows_a_section_and_it_stays_shown() {
    let mut doc = opened();
    let cb = "form1[0].Header[0].HasDeps[0]";
    assert!(printcraft_forms::set_value(&mut doc, cb, &FieldValue::Check(true)).is_ok());
    let out = after_edit(&mut doc, &[cb.to_string()], &OPTS);
    assert!(out.as_ref().is_ok_and(|o| o.relaid), "{out:?}");
    assert!(names(&doc).iter().any(|n| n == "form1[0].Deps[0].DepName[0]"), "{:?}", names(&doc));
    // Filling another field keeps it shown (the change is saved with the document).
    let dep = "form1[0].Deps[0].DepName[0]";
    assert!(printcraft_forms::set_value(&mut doc, dep, &FieldValue::Text("Sam".into())).is_ok());
    assert!(after_edit(&mut doc, &[dep.to_string()], &OPTS).is_ok());
    let mut again = reopen_doc(&doc);
    assert!(realize_with(&mut again, &OPTS).is_ok());
    assert_eq!(value(&again, dep).as_deref(), Some("Sam"));
    // And unchecking hides it again.
    assert!(printcraft_forms::set_value(&mut again, cb, &FieldValue::Check(false)).is_ok());
    assert!(after_edit(&mut again, &[cb.to_string()], &OPTS).is_ok_and(|o| o.relaid));
    assert!(!names(&again).iter().any(|n| n.contains("DepName")));
}

#[test]
fn buttons_add_and_remove_rows() {
    let mut doc = opened();
    assert!(has_click(&doc, "form1[0].Items[0].AddRow[0]"));
    assert!(!has_click(&doc, "form1[0].Items[0].Row[0].Qty[0]"));
    let out = click(&mut doc, "form1[0].Items[0].AddRow[0]", &OPTS);
    assert!(out.as_ref().is_ok_and(|o| o.relaid && o.output.errors.is_empty()), "{out:?}");
    assert!(names(&doc).iter().any(|n| n == "form1[0].Items[0].Row[2].Qty[0]"), "{:?}", names(&doc));
    // The new row is in the data, so it survives saving and reopening.
    let qty = "form1[0].Items[0].Row[2].Qty[0]";
    assert!(printcraft_forms::set_value(&mut doc, qty, &FieldValue::Text("4".into())).is_ok());
    let price = "form1[0].Items[0].Row[2].Price[0]";
    assert!(printcraft_forms::set_value(&mut doc, price, &FieldValue::Text("2.5".into())).is_ok());
    assert!(after_edit(&mut doc, &[qty.to_string(), price.to_string()], &OPTS).is_ok());
    assert_eq!(value(&doc, "form1[0].Totals[0].Total[0]").as_deref(), Some("35"));
    let mut again = reopen_doc(&doc);
    assert!(realize_with(&mut again, &OPTS).is_ok());
    assert_eq!(value(&again, "form1[0].Items[0].Row[2].Amount[0]").as_deref(), Some("10"));
    // FormCalc removes the last row; the total follows.
    let out = click(&mut again, "form1[0].Items[0].RemoveRow[0]", &OPTS);
    assert!(out.as_ref().is_ok_and(|o| o.relaid && o.output.errors.is_empty()), "{out:?}");
    assert!(!names(&again).iter().any(|n| n.contains("Row[2]")));
    assert_eq!(value(&again, "form1[0].Totals[0].Total[0]").as_deref(), Some("25"));
    // The maximum (5) is enforced: three more adds work, the fourth reports an error.
    for _ in 0..3 {
        assert!(click(&mut again, "form1[0].Items[0].AddRow[0]", &OPTS).is_ok_and(|o| o.output.errors.is_empty()));
    }
    let over = click(&mut again, "form1[0].Items[0].AddRow[0]", &OPTS);
    assert!(over.as_ref().is_ok_and(|o| o.output.errors.iter().any(|e| e.contains("no more than 5"))), "{over:?}");
}

#[test]
fn change_events_messages_and_validation() {
    let mut doc = opened();
    let kind = "form1[0].Header[0].Kind[0]";
    assert!(printcraft_forms::set_value(&mut doc, kind, &FieldValue::Choice(vec!["T".into()])).is_ok());
    assert!(after_edit(&mut doc, &[kind.to_string()], &OPTS).is_ok());
    assert_eq!(value(&doc, "form1[0].Header[0].Detail[0]").as_deref(), Some("Chose Travel"));
    let hello = click(&mut doc, "form1[0].Header[0].Hello[0]", &OPTS);
    assert!(hello.as_ref().is_ok_and(|o| o.output.messages == vec!["Greeting: Hello from the form".to_string()]), "{hello:?}");
    let qty = "form1[0].Items[0].Row[1].Qty[0]";
    assert!(printcraft_forms::set_value(&mut doc, qty, &FieldValue::Text("-2".into())).is_ok());
    let out = after_edit(&mut doc, &[qty.to_string()], &OPTS);
    assert!(
        out.as_ref().is_ok_and(|o| o.output.invalid.iter().any(|(f, m)| f.ends_with("Row[1].Qty[0]") && m == "Quantity can't be negative.")),
        "{out:?}"
    );
}

#[test]
fn scripts_off_still_lays_out_and_syncs() {
    let mut doc = open_doc(xfa_pdf(TEMPLATE, DATA));
    let off = Options { scripts: false, ..OPTS };
    let report = realize_with(&mut doc, &off);
    assert!(report.as_ref().is_ok_and(|r| r.notes.iter().any(|n| n.contains("scripts not run"))), "{report:?}");
    assert_eq!(value(&doc, "form1[0].Items[0].Row[0].Amount[0]").as_deref(), Some(""));
    let qty = "form1[0].Items[0].Row[0].Qty[0]";
    assert!(printcraft_forms::set_value(&mut doc, qty, &FieldValue::Text("7".into())).is_ok());
    assert!(after_edit(&mut doc, &[qty.to_string()], &off).is_ok());
    assert_eq!(value(&doc, "form1[0].Items[0].Row[0].Amount[0]").as_deref(), Some(""));
    let live = dom::Live::load(&doc).ok();
    assert!(live.is_some_and(|l| l.tree().to_string_of(l.data_root).contains("<Qty>7</Qty>")));
}

/// Scripts that loop forever, recurse without end, or throw are stopped and reported; the
/// form still opens.
#[test]
fn hostile_scripts_are_stopped() {
    let template = r##"<template><subform name="form1" layout="tb">
      <pageSet><pageArea name="P"><contentArea x="0" y="0" w="8in" h="10in"/><medium short="8.5in" long="11in"/></pageArea></pageSet>
      <subform name="S" w="8in" h="3in">
        <field name="A" x="0" y="0" w="2in" h="0.3in"><calculate><script contentType="application/x-javascript">while (true) {}</script></calculate></field>
        <field name="B" x="0" y="0.5in" w="2in" h="0.3in"><calculate><script contentType="application/x-javascript">function f(n) { return f(n + 1); } f(0)</script></calculate></field>
        <field name="C" x="0" y="1in" w="2in" h="0.3in"><calculate><script>while (1) do endwhile</script></calculate></field>
        <field name="D" x="0" y="1.5in" w="2in" h="0.3in"><event activity="initialize"><script contentType="application/x-javascript">throw "boom";</script></event></field>
        <field name="E" x="0" y="2in" w="2in" h="0.3in"><calculate><script>func f(n) do f(n + 1) endfunc f(1)</script></calculate></field>
      </subform></subform></template>"##;
    let mut doc = open_doc(xfa_pdf(template, ""));
    let report = realize_with(&mut doc, &OPTS);
    assert!(report.as_ref().is_ok_and(|r| r.fields >= 5 && r.output.errors.len() >= 5), "{report:?}");
}

#[test]
fn formcalc_built_ins() {
    let template = r##"<template><subform name="form1" layout="tb">
      <pageSet><pageArea name="P"><contentArea x="0" y="0" w="8in" h="10in"/><medium short="8.5in" long="11in"/></pageArea></pageSet>
      <subform name="S" w="8in" h="6in">
        <field name="N" x="0" y="0" w="2in" h="0.3in"><value><text>  Hello World  </text></value></field>
        <field name="R1" x="0" y="0.5in" w="4in" h="0.3in"><calculate><script>Concat(Upper(Left(Ltrim(N), 5)), "-", Len(Rtrim(Ltrim(N))), "-", Substr("abcdef", 2, 3))</script></calculate></field>
        <field name="R2" x="0" y="1in" w="4in" h="0.3in"><calculate><script>var t = 0
for i = 1 upto 10 step 3 do t = t + i endfor
Round(Avg(t, 4, null) / 3, 2)</script></calculate></field>
        <field name="R3" x="0" y="1.5in" w="4in" h="0.3in"><calculate><script>Num2Date(Date2Num("10/05/2026", "MM/DD/YYYY") + 30, "YYYY-MM-DD")</script></calculate></field>
        <field name="R4" x="0" y="2in" w="4in" h="0.3in"><calculate><script>if (Oneof("b", "a", "b") and Within(5, 1, 10) and not HasValue(" ")) then Choose(2, "x", "y") else "no" endif</script></calculate></field>
        <field name="R5" x="0" y="2.5in" w="4in" h="0.3in"><calculate><script>func sq(x) do x * x endfunc
Str(sq(3) + Mod(10, 4) + Abs(-1.5), 6, 1)</script></calculate></field>
        <field name="R6" x="0" y="3in" w="4in" h="0.3in"><calculate><script>Replace(At("abcabc", "ca") == 3, "1", "yes")</script></calculate></field>
        <field name="R7" x="0" y="3.5in" w="4in" h="0.3in"><calculate><script>R1.presence</script></calculate></field>
      </subform></subform></template>"##;
    let mut doc = open_doc(xfa_pdf(template, ""));
    let report = realize_with(&mut doc, &OPTS);
    assert!(report.as_ref().is_ok_and(|r| r.output.errors.is_empty()), "{report:?}");
    let v = |n: &str| value(&doc, &format!("form1[0].S[0].{n}[0]")).unwrap_or_default();
    assert_eq!(v("R1"), "HELLO-11-bcd");
    assert_eq!(v("R2"), "4.33");
    assert_eq!(v("R3"), "2026-11-04");
    assert_eq!(v("R4"), "y");
    assert_eq!(v("R5"), "  12.5");
    assert_eq!(v("R6"), "yes");
    assert_eq!(v("R7"), "visible");
}

/// Patterns from a real leave form (all synthetic here): check boxes sharing one data value,
/// unbound fields, pop-up pick lists, keystroke filters, `execEvent("calculate")`, a text
/// variable, Acrobat's `event.target.getField(…).signatureValidate()` and `util.printd`.
const LEAVE: &str = r##"<template xmlns="http://www.xfa.org/schema/xfa-template/3.3/">
<subform name="form1" layout="tb" restoreState="auto">
  <variables><text name="remLeave">Leave remark</text>
    <script name="fn" contentType="application/x-javascript">function unlock(a, b, sig) { sig.access = (a.rawValue != null &amp;&amp; b.rawValue == 2) ? "open" : "protected"; }</script>
  </variables>
  <pageSet><pageArea name="P"><contentArea x="0.25in" y="0.25in" w="8in" h="10in"/><medium short="8.5in" long="11in"/></pageArea></pageSet>
  <subform name="Page1" w="8in" h="6in">
    <field name="NAME" x="0" y="0" w="3in" h="0.3in"><ui><textEdit/></ui><bind match="none"/>
      <event activity="exit"><script contentType="application/x-javascript">fn.unlock(NAME, ORDINARY, SIG);</script></event></field>
    <field name="SSN" x="3.5in" y="0" w="2in" h="0.3in"><ui><textEdit/></ui><bind match="none"/>
      <event activity="change"><script contentType="application/x-javascript">var s = String(xfa.event.change), o = ""; for (var i = 0; i &lt; s.length; i++) { if ("0123456789".indexOf(s.charAt(i)) != -1) o += s.charAt(i); } xfa.event.change = o;</script></event></field>
    <field name="RANK" x="0" y="0.5in" w="2in" h="0.3in"><ui><textEdit/></ui><bind match="none"/>
      <event activity="click"><script contentType="application/x-javascript">this.rawValue = app.popUpMenuEx({cName:"Sergeant",cReturn:"SGT"},{cName:"More",oSubMenu:[{cName:"Captain",cReturn:"CPT"}]});</script></event></field>
    <field name="ORDINARY" x="0" y="1in" w="0.2in" h="0.2in"><ui><checkButton/></ui><value><text>0</text></value><items><text>2</text><text>0</text></items><bind match="dataRef" ref="$.group1"/>
      <event activity="click"><script contentType="application/x-javascript">if (this.rawValue == 2) { REMARKS.rawValue = remLeave.value; } fn.unlock(NAME, ORDINARY, SIG);</script></event></field>
    <field name="TDY" x="1in" y="1in" w="0.2in" h="0.2in"><ui><checkButton/></ui><value><text>0</text></value><items><text>4</text><text>0</text></items><bind match="dataRef" ref="$.group1"/></field>
    <field name="REMARKS" x="0" y="1.5in" w="6in" h="0.3in"><ui><textEdit/></ui><bind match="none"/></field>
    <field name="DAYS" x="0" y="2in" w="1in" h="0.3in"><ui><numericEdit/></ui><bind match="none"/>
      <event activity="exit"><script contentType="application/x-javascript">TO.execEvent("calculate");</script></event></field>
    <field name="TO" x="2in" y="2in" w="1in" h="0.3in" access="readOnly"><ui><textEdit/></ui><bind match="none"/>
      <calculate override="disabled"><script contentType="application/x-javascript">DAYS.rawValue == null ? null : "in " + DAYS.rawValue + " days"</script></calculate></field>
    <field name="SIG" x="0" y="3in" w="3in" h="0.5in" access="protected"><ui><signature/></ui>
      <event activity="preSign"><script contentType="application/x-javascript">STAMP.rawValue = util.printd("yyyymmdd", new Date(2026, 9, 5)); STAMP.access = "readOnly";</script></event></field>
    <field name="STAMP" x="3.5in" y="3in" w="2in" h="0.3in"><ui><textEdit/></ui><bind match="none"/></field>
    <field name="STATE" x="0" y="4in" w="2in" h="0.3in"><ui><textEdit/></ui><bind match="none"/>
      <event activity="initialize"><script contentType="application/x-javascript">this.rawValue = String(event.target.getField("form1[0].Page1[0].SIG[0]").signatureValidate());</script></event></field>
  </subform>
</subform>
</template>"##;

const LEAVE_DATA: &str = r#"<xfa:datasets xmlns:xfa="http://www.xfa.org/schema/xfa-data/1.0/"><xfa:data><form1><Page1><group1>4</group1></Page1></form1></xfa:data></xfa:datasets>"#;

fn fill(doc: &mut Document, name: &str, v: FieldValue) -> Outcome {
    let full = format!("form1[0].Page1[0].{name}[0]");
    assert!(printcraft_forms::set_value(doc, &full, &v).is_ok(), "{full}");
    match after_edit(doc, &[full], &OPTS) {
        Ok(o) => o,
        Err(e) => panic!("{name}: {e}"),
    }
}

fn get(doc: &Document, name: &str) -> Option<String> {
    value(doc, &format!("form1[0].Page1[0].{name}[0]"))
}

fn read_only(doc: &Document, name: &str) -> bool {
    fields(doc).into_iter().find(|f| f.name == format!("form1[0].Page1[0].{name}[0]")).is_some_and(|f| f.read_only())
}

#[test]
fn leave_form_patterns() {
    let mut doc = open_doc(xfa_pdf(LEAVE, LEAVE_DATA));
    let r = realize_with(&mut doc, &OPTS);
    assert!(r.as_ref().is_ok_and(|r| r.output.errors.is_empty()), "{r:?}");
    assert_eq!(get(&doc, "STATE").as_deref(), Some("0"), "unsigned");
    assert!(read_only(&doc, "SIG"), "protected until filled");
    // The data says TDY (4); checking ORDINARY moves the shared value to 2.
    assert_eq!(get(&doc, "TDY").as_deref(), Some("4"));
    fill(&mut doc, "NAME", FieldValue::Text("DOE, JOHN".into()));
    let o = fill(&mut doc, "ORDINARY", FieldValue::Check(true));
    assert!(o.output.errors.is_empty(), "{o:?}");
    assert_eq!(get(&doc, "TDY").as_deref(), Some(""), "unchecked");
    assert_eq!(get(&doc, "REMARKS").as_deref(), Some("Leave remark"));
    assert!(!read_only(&doc, "SIG"), "unlocked once filled");
    // Keystroke filter: only digits stay.
    fill(&mut doc, "SSN", FieldValue::Text("123-45-6789".into()));
    assert_eq!(get(&doc, "SSN").as_deref(), Some("123456789"));
    // execEvent("calculate") from another field's exit.
    fill(&mut doc, "DAYS", FieldValue::Text("15".into()));
    assert_eq!(get(&doc, "TO").as_deref(), Some("in 15 days"));
    // A pick list asks, then fills (submenus flatten to "More > Captain").
    let rank = "form1[0].Page1[0].RANK[0]";
    let ask = click(&mut doc, rank, &OPTS);
    let items = ask.as_ref().ok().and_then(|o| o.output.ask.clone()).unwrap_or_default();
    assert_eq!(items, vec![("Sergeant".to_string(), "SGT".to_string()), ("More > Captain".to_string(), "CPT".to_string())]);
    assert_eq!(get(&doc, "RANK").as_deref(), Some(""), "nothing changes until answered");
    assert!(click_answering(&mut doc, rank, &[Some("CPT".into())], &OPTS).is_ok());
    assert_eq!(get(&doc, "RANK").as_deref(), Some("CPT"));
    // preSign stamps the date and locks it.
    assert!(before_sign(&mut doc, "form1[0].Page1[0].SIG[0]", &OPTS).is_ok());
    assert_eq!(get(&doc, "STAMP").as_deref(), Some("20261005"));
    assert!(read_only(&doc, "STAMP"));
    // Unbound values (NAME, SSN, RANK, …) survive saving and reopening: they're kept in the
    // form state packet.
    let mut again = reopen_doc(&doc);
    assert!(realize_with(&mut again, &OPTS).is_ok());
    for (f, v) in [("NAME", "DOE, JOHN"), ("SSN", "123456789"), ("RANK", "CPT"), ("REMARKS", "Leave remark"), ("STAMP", "20261005")] {
        assert_eq!(get(&again, f).as_deref(), Some(v), "{f}");
    }
    let live = dom::Live::load(&again).ok();
    let form = live.as_ref().and_then(|l| l.xdp.form.map(|f| l.tree().to_string_of(f))).unwrap_or_default();
    assert!(form.contains(r#"<field name="NAME"><value override="1"><text>DOE, JOHN</text></value></field>"#), "{form}");
}
