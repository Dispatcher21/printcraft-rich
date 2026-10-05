//! Lay out an XFA form as ordinary pages and fields: `realize IN.pdf OUT.pdf`.
//!
//! The output keeps the XFA (Acrobat still renders from it) and opens with filled-in pages in
//! any viewer.

use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(input), Some(output)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: realize IN.pdf OUT.pdf");
        std::process::exit(2);
    };
    let run = || -> Result<printcraft_xfa::Report, String> {
        let bytes = std::fs::read(input).map_err(|e| e.to_string())?;
        let mut doc = printcraft_cos::Document::open(Arc::new(bytes)).map_err(|e| e.to_string())?;
        let report = printcraft_xfa::realize(&mut doc).map_err(|e| e.to_string())?;
        let out = printcraft_cos::write_incremental(&doc, &printcraft_cos::SaveOptions::default()).map_err(|e| e.to_string())?;
        std::fs::write(output, out).map_err(|e| e.to_string())?;
        Ok(report)
    };
    match run() {
        Ok(r) => {
            println!("{} pages, {} fields", r.pages, r.fields);
            for n in r.notes {
                println!("note: {n}");
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
