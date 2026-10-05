# printcraft-xfa

XFA forms (XML Forms Architecture 3.3; ISO 32000-2 §12.7.9). Layer L3; depends on
`printcraft-cos`, `printcraft-fonts` and `printcraft-forms`.

Government and military forms are often XFA forms made with LiveCycle Designer. Dynamic ones
(`/NeedsRendering true`) ship only a "Please wait…" placeholder page, so every viewer except
Acrobat shows that page instead of the form. This crate lays such forms out as real pages and
AcroForm fields, and writes filled-in values back into the XFA data so Acrobat sees them too.

## API

```rust
match printcraft_xfa::kind(&doc) {
    Some(Kind::Dynamic) => { let report = printcraft_xfa::realize(&mut doc)?; } // pages + fields
    Some(Kind::Static) => {}                                                      // already has both
    None => {}                                                                    // not XFA
}
// …fill fields with printcraft-forms…
let changed = printcraft_xfa::sync_datasets(&mut doc)?; // before saving
```

The engine does both on its own: `Session::open` realizes dynamic forms (see
`Document::xfa`), and `save_bytes` / `save_full_bytes` sync the data.

`cargo run -p printcraft-xfa --example realize -- in.pdf out.pdf` converts a file from the
command line.

## How it works

1. **Packets** (`packet`): the `/XFA` stream or `[name stream …]` array read as one XDP tree
   (`xml`, a small lenient XML tree that round-trips prefixes and declarations).
2. **Template** (`template`): prototypes (`use="#id"`) expanded; measurements, fonts, paras,
   margins, borders, captions and rich text read.
3. **Merge** (`merge`): template containers paired with data groups and values by name (normal
   binding), `global`, `none` and simple `dataRef` paths; repeating subforms per `<occur>` and
   the data; SOM names (`form1[0].Page1[0].Name[0]`, `#subform[0]` for unnamed ones).
4. **Layout** (`layout`): positioned, `tb`, `lr-tb`/`rl-tb`, `row` and `table` layouts; growable
   sizes; the root flows through page areas' content areas with overflow breaks, `breakBefore`/
   `breakAfter`/`break`, page-area `occur` limits, and master-page content on every page.
5. **Realize** (`realize`): one content stream per page (draws, lines, rectangles, arcs, JPEG/PNG
   images, captions, borders) and AcroForm fields for every field (text, multi-line, comb,
   password, date with its display picture, check boxes, radio groups from exclusion groups,
   drop-downs and lists with save/display items, buttons, signatures), with appearances. The
   page tree is replaced; the XFA stays (Acrobat keeps rendering from it).
6. **Sync** (`sync_datasets`): every bound field's AcroForm value goes into the data, in the
   data's own spelling (check box on/off values, radio on-values, choice save values, dates in
   canonical or `<bind><picture>` form). Unchanged values and untouched defaults are left alone.
   When anything changes, `/Perms /UR3` (Reader usage rights) is removed because the change
   invalidates it.

## Scripts

`realize_with`, `after_edit` and `click` run the form's scripts (JavaScript on the sandboxed
boa engine of `printcraft-js`, FormCalc on this crate's interpreter) against a live form model
(`dom`, `host`): `initialize`, calculations and `ready` events on open; `change`, `click`,
`exit`, validations and calculations while filling; button `click` scripts. Changes to
visibility, properties and instances lay the pages out again. Property changes are saved in
the catalog's `/PieceInfo /PrintCraft` so they survive reopening; row counts live in the data.

Scripts also get the Acrobat objects LiveCycle forms use: `event.target` (the document) with
`getField(…).signatureValidate()`, `app.alert`, `app.popUpMenuEx` (a pick list: the job stops
and reports the choices in `Output::ask`; `click_answering` finishes it), and `util.printd`.
`before_sign` runs a signature field's `preSign` scripts.

Fields that don't bind to data (`<bind match="none"/>`) keep their values in the `form`
packet, where Acrobat keeps them; PrintCraft reads and writes it (Acrobat's checksum is left as
it was). Once a form carries a signature its pages stay as signed: scripts still update values
and field locks, but nothing is laid out again.

## Not supported

- Splitting a text block across pages; `keep`, leaders and trailers; overflow targets.
- Linked (non-embedded) images and fonts; barcodes and image fields are drawn as frames.
- Number pictures (`num{…}`); dates use the canonical form when their picture isn't understood.
- FormCalc financial and URL functions; `messageBox` can't ask the user (answers OK / Yes).

Everything is untrusted input: depth, node, instance and page counts are capped, scripts run
with loop, step and recursion limits, and nothing panics (see the hostile-input tests).
