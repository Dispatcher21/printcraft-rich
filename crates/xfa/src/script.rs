//! Running XFA scripts (XFA 3.3 Part 1 "Scripting" and "Events"): which scripts run when, in
//! which language, and the jobs the rest of the crate asks for (open, edit, click).
//!
//! - `<event activity="…">` scripts on a container (`ref="$"` or none) run for that container's
//!   events; `ref="$form"`, `"$layout"` and `"$host"` events run for the form's `ready`, the
//!   layout's `ready` and the host's `docReady`.
//! - `<calculate>` scripts give their field its value (the script's last expression); they
//!   run in document order until no value changes (at most [`MAX_PASSES`] passes).
//! - `<validate>` scripts return whether the value is acceptable.
//!
//! Scripts are JavaScript when their `contentType` says so, FormCalc otherwise. Server-side
//! scripts (`runAt="server"`) are skipped. Every script runs inside the limits of its language;
//! a failing script is reported and the rest go on, as in Acrobat.

use crate::dom::{Live, N};
use crate::formcalc;
use crate::host::{Host, Obj, Output, Pending, Val};
use crate::js::Runtime;
use crate::merge::Tag;
use crate::xml::{NodeId, Tree};

/// Calculation passes before giving up on a value that never settles.
pub const MAX_PASSES: usize = 10;
/// Most scripts one job runs.
const MAX_RUNS: usize = 200_000;
/// Most errors kept.
const MAX_ERRORS: usize = 200;
/// How deeply scripts may trigger each other (execEvent, new instances).
const MAX_NESTING: usize = 16;

/// A script's source and language.
#[derive(Clone, Debug, PartialEq)]
pub struct Src {
    pub js: bool,
    pub text: String,
}

fn src_of(t: &Tree, script: NodeId) -> Option<Src> {
    if t.attr(script, "runAt") == Some("server") {
        return None;
    }
    let text = t.text(script);
    if text.trim().is_empty() {
        return None;
    }
    let js = t.attr(script, "contentType").is_some_and(|c| c.contains("javascript"));
    Some(Src { js, text })
}

/// Which object an `<event ref>` targets.
fn target_of(r: Option<&str>) -> &'static str {
    match r.map(str::trim) {
        None | Some("") | Some("$") => "$",
        Some("$form") | Some("xfa.form") => "$form",
        Some("$layout") | Some("xfa.layout") => "$layout",
        Some("$host") | Some("xfa.host") => "$host",
        _ => "other",
    }
}

/// The scripts of `n` for `activity` on target `which` (`$` is the container itself).
pub fn event_scripts(live: &Live, n: N, activity: &str, which: &str) -> Vec<Src> {
    let Some(inst) = live.inst(n) else { return Vec::new() };
    let t = live.tree();
    t.elements(inst.t)
        .filter(|e| t.tag(*e) == "event" && t.attr(*e, "activity") == Some(activity) && target_of(t.attr(*e, "ref")) == which)
        .filter_map(|e| t.child(e, "script").and_then(|s| src_of(t, s)))
        .collect()
}

fn calculate_script(live: &Live, n: N) -> Option<Src> {
    let inst = live.inst(n)?;
    let t = live.tree();
    t.child(inst.t, "calculate").and_then(|c| t.child(c, "script")).and_then(|s| src_of(t, s))
}

fn validate_script(live: &Live, n: N) -> Option<(Src, String)> {
    let inst = live.inst(n)?;
    let t = live.tree();
    let v = t.child(inst.t, "validate")?;
    let src = t.child(v, "script").and_then(|s| src_of(t, s))?;
    let msg = t
        .child(v, "message")
        .and_then(|m| t.elements(m).find(|x| t.attr(*x, "name") == Some("scriptTest")).or_else(|| t.elements(m).next()))
        .map(|x| t.text(x))
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "The value is not valid.".to_string());
    Some((src, msg))
}

/// Whether the template has any script at all.
pub fn has_scripts(live: &Live) -> bool {
    let t = live.tree();
    t.descendants(live.template).into_iter().any(|n| t.tag(n) == "script" && src_of(t, n).is_some())
}

/// Runs scripts against one [`Host`].
pub struct Engine {
    rt: Runtime,
    runs: usize,
    nesting: usize,
    recalc: bool,
}

impl Engine {
    pub fn new(host: Host) -> Result<Engine, String> {
        let rt = Runtime::new(host, printcraft_js::Limits::default()).map_err(|e| e.to_string())?;
        Ok(Engine { rt, runs: 0, nesting: 0, recalc: false })
    }

    pub fn into_host(self) -> Option<Host> {
        self.rt.into_host()
    }

    fn with<T>(&self, f: impl FnOnce(&mut Host) -> T) -> Option<T> {
        self.rt.host().map(|mut h| f(&mut h))
    }

    fn live<T>(&self, f: impl FnOnce(&Live) -> T) -> Option<T> {
        self.rt.host().map(|h| f(&h.live))
    }

    /// Run one script on `n`; its result (`None`: undefined or failed).
    pub fn exec(&mut self, n: N, src: &Src) -> Option<Val> {
        if self.runs >= MAX_RUNS {
            return None;
        }
        self.runs += 1;
        let r = if src.js {
            self.rt.run(n, &src.text)
        } else {
            match self.rt.host() {
                Some(mut h) => formcalc::run(&mut h, n, &src.text).map(Some),
                None => Err("the form is busy".into()),
            }
        };
        let out = match r {
            Ok(v) => v,
            Err(e) => {
                self.with(|h| {
                    if h.out.errors.len() < MAX_ERRORS {
                        let som = h.live.inst(n).map(|i| i.som.clone()).unwrap_or_default();
                        h.out.errors.push(format!("{som}: {e}"));
                    }
                });
                None
            }
        };
        self.drain();
        out
    }

    /// Work scripts asked for (new instances' initialize, execEvent, recalculation).
    fn drain(&mut self) {
        if self.nesting >= MAX_NESTING {
            return;
        }
        self.nesting += 1;
        for _ in 0..10_000 {
            let Some(p) = self.with(|h| if h.pending.is_empty() { None } else { Some(h.pending.remove(0)) }).flatten() else { break };
            match p {
                Pending::Initialize(n) => self.initialize(n),
                Pending::Event(n, a) => self.fire(n, &a),
                Pending::Calculate => self.recalc = true,
            }
        }
        self.nesting -= 1;
    }

    /// Fire `activity` on `n` (its own events).
    pub fn fire(&mut self, n: N, activity: &str) {
        let scripts = self.live(|l| event_scripts(l, n, activity, "$")).unwrap_or_default();
        for s in scripts {
            self.exec(n, &s);
        }
    }

    /// Fire a form-, layout- or host-level event (`which`: `$form`, `$layout`, `$host`)
    /// wherever it is declared in `form`.
    pub fn fire_global(&mut self, form: usize, which: &str, activity: &str) {
        let nodes = self.live(|l| l.walk(form)).unwrap_or_default();
        for n in nodes {
            let scripts = self.live(|l| event_scripts(l, n, activity, which)).unwrap_or_default();
            for s in scripts {
                self.exec(n, &s);
            }
        }
    }

    /// `initialize` on `root` and everything below it, in document order.
    pub fn initialize(&mut self, root: N) {
        let nodes = self.live(|l| {
            let all = l.walk(root.form);
            if root.inst == 0 {
                return all;
            }
            // The subtree of `root`.
            let mut keep = Vec::new();
            for n in all {
                let mut at = Some(n);
                for _ in 0..128 {
                    match at {
                        Some(a) if a == root => {
                            keep.push(n);
                            break;
                        }
                        Some(a) => at = l.parent(a),
                        None => break,
                    }
                }
            }
            keep
        });
        for n in nodes.unwrap_or_default() {
            self.fire(n, "initialize");
        }
    }

    /// Run every calculation of `form` until the values settle.
    pub fn calculate_all(&mut self, form: usize) {
        if !self.with(|h| h.calculations).unwrap_or(true) {
            return;
        }
        for _ in 0..MAX_PASSES {
            self.recalc = false;
            let before = self.live(|l| l.writes).unwrap_or(0);
            let nodes: Vec<(N, Src)> = self
                .live(|l| {
                    l.walk(form)
                        .into_iter()
                        .filter(|n| l.inst(*n).is_some_and(|i| matches!(i.tag, Tag::Field | Tag::ExclGroup)))
                        .filter_map(|n| calculate_script(l, n).map(|s| (n, s)))
                        .collect()
                })
                .unwrap_or_default();
            if nodes.is_empty() {
                return;
            }
            for (n, s) in nodes {
                if let Some(v) = self.exec(n, &s) {
                    self.with(|h| {
                        let _ = h.set(&Obj::Node(n), "rawValue", v);
                    });
                }
            }
            let after = self.live(|l| l.writes).unwrap_or(0);
            if after == before && !self.recalc {
                return;
            }
        }
    }

    /// Run `n`'s validation; failures are recorded. Returns whether the value is valid.
    pub fn validate(&mut self, n: N) -> bool {
        let Some((src, msg)) = self.live(|l| validate_script(l, n)).flatten() else { return true };
        let ok = match self.exec(n, &src) {
            None => true,
            Some(Val::Null) => true,
            Some(Val::Bool(b)) => b,
            Some(Val::Num(x)) => x != 0.0,
            Some(Val::Str(s)) => s.trim().parse::<f64>().map(|x| x != 0.0).unwrap_or(!s.is_empty() && s != "false"),
            Some(Val::Obj(_)) => true,
        };
        if !ok {
            self.with(|h| {
                let som = h.live.inst(n).map(|i| i.som.clone()).unwrap_or_default();
                if h.out.invalid.len() < MAX_ERRORS {
                    h.out.invalid.push((som, msg.clone()));
                }
                if h.out.messages.len() < 1_000 {
                    h.out.messages.push(msg);
                }
            });
        }
        ok
    }

    /// The event the next scripts see.
    pub fn set_event(&mut self, name: &str, target: Option<N>, new_text: &str, prev_text: &str) {
        self.with(|h| {
            h.event.name = name.to_string();
            h.event.target = target;
            h.event.new_text = new_text.to_string();
            h.event.change = new_text.to_string();
            h.event.prev_text = prev_text.to_string();
        });
    }

    pub fn host(&self) -> Option<std::cell::RefMut<'_, Host>> {
        self.rt.host()
    }

    pub fn recalc_requested(&self) -> bool {
        self.recalc
    }
}

/// A value change the user made: the field, its value before and after.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub field: N,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// Run the events of the user's value changes, then the calculations.
pub fn edit(engine: &mut Engine, changes: &[Change]) {
    for c in changes {
        let n = c.field;
        let Some((tag, is_check, members, display_new, display_old)) = engine
            .live(|l| {
                let inst = l.inst(n)?;
                let is_check = l.tree().path(inst.t, &["ui", "checkButton"]).is_some();
                let display = |v: &Option<String>| -> String {
                    let raw = v.clone().unwrap_or_default();
                    l.items(n).into_iter().find(|(s, _)| *s == raw).map(|(_, d)| d).unwrap_or(raw)
                };
                Some((inst.tag, is_check, inst.children.clone(), display(&c.after), display(&c.before)))
            })
            .flatten()
        else {
            continue;
        };
        // During `change` the field still holds its old value; the new text is in the event.
        engine.with(|h| h.live.set_raw(n, c.before.as_deref()));
        engine.set_event("change", Some(n), &display_new, &display_old);
        engine.fire(n, "change");
        engine.with(|h| h.live.set_raw(n, c.after.as_deref()));
        if is_check {
            engine.set_event("click", Some(n), &display_new, &display_old);
            engine.fire(n, "click");
        }
        if tag == Tag::ExclGroup {
            // The selected button gets the click.
            let picked = engine.live(|l| {
                members
                    .iter()
                    .map(|m| N { form: n.form, inst: *m })
                    .find(|m| l.inst(*m).is_some_and(|mi| c.after.as_deref().is_some_and(|v| crate::merge::check_values(l.tree(), mi.t).0 == v)))
            });
            if let Some(Some(m)) = picked {
                engine.set_event("click", Some(m), &display_new, &display_old);
                engine.fire(m, "click");
            }
        }
        engine.set_event("exit", Some(n), &display_new, &display_old);
        engine.fire(n, "exit");
        engine.validate(n);
    }
    engine.calculate_all(0);
}

/// A button press: `click` (and `mouseUp`), then the calculations.
pub fn click(engine: &mut Engine, n: N) {
    engine.set_event("click", Some(n), "", "");
    engine.fire(n, "mouseDown");
    engine.fire(n, "mouseUp");
    engine.fire(n, "click");
    engine.calculate_all(0);
}

/// What opening runs before layout: `initialize` everywhere, the calculations, `form:ready`.
pub fn open(engine: &mut Engine) {
    engine.set_event("initialize", None, "", "");
    engine.initialize(N::ROOT);
    engine.calculate_all(0);
    engine.set_event("ready", None, "", "");
    engine.fire_global(0, "$form", "ready");
    if engine.recalc_requested() {
        engine.calculate_all(0);
    }
}

/// Master page scripts (page numbers), once their forms exist after layout.
pub fn masters(engine: &mut Engine) {
    let count = engine.live(|l| l.forms.len()).unwrap_or(1);
    for f in 1..count {
        engine.initialize(N { form: f, inst: 0 });
        engine.calculate_all(f);
    }
}

/// Run `job` on its own thread with a large stack (scripts recurse), as Acrobat JavaScript
/// does; on the web, in place.
pub fn on_thread<T: Send>(job: impl FnOnce() -> T + Send) -> Result<T, String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::scope(|scope| {
            let spawned = std::thread::Builder::new().name("printcraft-xfa".into()).stack_size(printcraft_js::SCRIPT_STACK).spawn_scoped(scope, job);
            match spawned {
                Ok(t) => t.join().map_err(|_| "the form's scripts stopped with an internal error".to_string()),
                Err(e) => Err(format!("the script engine could not start: {e}")),
            }
        })
    }
    #[cfg(target_arch = "wasm32")]
    Ok(job())
}

/// Collect a finished engine's output.
pub fn finish(engine: Engine) -> Option<(Live, Output)> {
    engine.into_host().map(|h| (h.live, h.out))
}
