//! Describe an XFA form: its scripts (by event and language) and what opening it runs:
//! `inspect IN.pdf [--scripts] [--find NAME] [--grep TEXT] [--vars]`.

use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(input) = args.get(1) else {
        eprintln!("usage: inspect IN.pdf [--scripts] [--find NAME] [--grep TEXT] [--vars]");
        std::process::exit(2);
    };
    let show = args.iter().any(|a| a == "--scripts");
    let run = || -> Result<(), String> {
        let bytes = std::fs::read(input).map_err(|e| e.to_string())?;
        let doc = printcraft_cos::Document::open(Arc::new(bytes)).map_err(|e| e.to_string())?;
        println!("kind: {:?}", printcraft_xfa::kind(&doc));
        let live = printcraft_xfa::dom::Live::load(&doc).map_err(|e| e.to_string())?;
        let t = live.tree();
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        for n in t.descendants(live.template) {
            if t.tag(n) != "script" || t.text(n).trim().is_empty() {
                continue;
            }
            let lang = if t.attr(n, "contentType").is_some_and(|c| c.contains("javascript")) { "js" } else { "formcalc" };
            let parent = t.parent(n);
            let what = match parent.map(|p| t.tag(p)) {
                Some("event") => format!(
                    "event:{}{}",
                    parent.and_then(|p| t.attr(p, "activity")).unwrap_or("?"),
                    parent.and_then(|p| t.attr(p, "ref")).map(|r| format!("@{r}")).unwrap_or_default()
                ),
                Some(other) => other.to_string(),
                None => "?".into(),
            };
            *counts.entry(format!("{what} ({lang})")).or_default() += 1;
            if show {
                let owner = parent.and_then(|p| t.parent(p)).map(|o| t.attr(o, "name").unwrap_or("").to_string()).unwrap_or_default();
                println!("--- {owner} {what} ({lang})\n{}", t.text(n).trim());
            }
        }
        if let Some(i) = args.iter().position(|a| a == "--find") {
            let want = args.get(i + 1).cloned().unwrap_or_default();
            for n in t.descendants(live.template).into_iter().filter(|n| t.attr(*n, "name") == Some(want.as_str())) {
                let p = t.parent(n).map(|p| t.tag(p).to_string()).unwrap_or_default();
                println!("found under <{p}>: {}", t.to_string_of(n).chars().take(600).collect::<String>());
            }
        }
        if let Some(i) = args.iter().position(|a| a == "--grep") {
            let want = args.get(i + 1).cloned().unwrap_or_default();
            for n in t.descendants(live.template).into_iter().filter(|n| matches!(t.tag(*n), "draw" | "field")) {
                let x = t.to_string_of(n);
                if x.contains(&want) {
                    println!("match <{}>: {}", t.tag(n), x.chars().take(1400).collect::<String>());
                }
            }
        }
        if args.iter().any(|a| a == "--vars") {
            for v in t.descendants(live.template).into_iter().filter(|n| t.tag(*n) == "variables") {
                println!("variables: {}", t.to_string_of(v).chars().take(1500).collect::<String>());
            }
        }
        for (k, v) in counts {
            println!("{v:5}  {k}");
        }
        let mut d = doc.clone();
        let r = printcraft_xfa::realize_with(&mut d, &printcraft_xfa::Options { scripts: true, today: (2026, 10, 5) }).map_err(|e| e.to_string())?;
        println!("pages {} fields {} notes {:?}", r.pages, r.fields, r.notes);
        for m in &r.output.messages {
            println!("message: {m}");
        }
        for e in &r.output.errors {
            println!("error: {e}");
        }
        Ok(())
    };
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
