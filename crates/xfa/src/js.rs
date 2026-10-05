//! XFA JavaScript on boa: form objects as JavaScript objects.
//!
//! Every scripting object ([`Obj`]) is a `Proxy`, so any property name reaches
//! [`Host::get`]/[`Host::set`] (`Page1.Total.rawValue`, `this.border.fill.color.value`) and
//! methods become functions calling [`Host::call`]. Scripts run as
//! `with (scope) { … }` with a scope proxy that resolves bare names the XFA way (children,
//! then ancestors), so `Total.rawValue = Qty.rawValue * Price.rawValue` works as in Acrobat.
//! JavaScript's own globals (`Math`, `Date`, …) win over form objects of the same name.
//!
//! The sandbox is the one Acrobat JavaScript uses (`printcraft-js`): no file, network or
//! timer access, bounded loops and recursion, and over-nested scripts are refused.

use std::cell::RefCell;
use std::collections::HashMap;

use boa_engine::object::builtins::{JsArray, JsProxyBuilder};
use boa_engine::object::{FunctionObjectBuilder, ObjectInitializer};
use boa_engine::property::Attribute;
use boa_engine::{Context, JsError, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction, Source, js_string};

use crate::dom::N;
use crate::host::{Host, Obj, Val};
use crate::xml::NodeId;

/// Most distinct objects one run hands to scripts.
const MAX_OBJECTS: usize = 500_000;

type Shared = RefCell<Host>;

/// Objects handed to scripts, by index (proxies keep the index).
#[derive(Default)]
struct Bridge {
    objs: Vec<Obj>,
    index: HashMap<Obj, usize>,
    proxies: HashMap<usize, JsObject>,
    scripts: HashMap<NodeId, JsObject>,
}

type SharedBridge = RefCell<Bridge>;

fn error(msg: impl Into<String>) -> JsError {
    JsNativeError::typ().with_message(msg.into()).into()
}

fn with_host<T>(ctx: &Context, f: impl FnOnce(&mut Host) -> T) -> JsResult<T> {
    let h = ctx.get_data::<Shared>().ok_or_else(|| error("the form is not available"))?;
    let mut h = h.try_borrow_mut().map_err(|_| error("the form is busy"))?;
    Ok(f(&mut h))
}

fn s(v: &str) -> JsValue {
    JsValue::from(JsString::from(v))
}

fn arg(args: &[JsValue], i: usize) -> JsValue {
    args.get(i).cloned().unwrap_or_default()
}

fn key_name(v: &JsValue, ctx: &mut Context) -> JsResult<Option<String>> {
    if v.is_symbol() {
        return Ok(None);
    }
    Ok(Some(v.to_string(ctx)?.to_std_string_escaped()))
}

/// The object a proxy target stands for.
fn target_obj(target: &JsValue, ctx: &mut Context) -> JsResult<(usize, Obj)> {
    let t = target.as_object().ok_or_else(|| error("not a form object"))?;
    let i = t.get(js_string!("__i"), ctx)?.as_number().ok_or_else(|| error("not a form object"))?;
    let i = if i.is_finite() && i >= 0.0 { i as usize } else { return Err(error("not a form object")) };
    let b = ctx.get_data::<SharedBridge>().ok_or_else(|| error("the form is not available"))?;
    let o = b.borrow().objs.get(i).cloned().ok_or_else(|| error("not a form object"))?;
    Ok((i, o))
}

/// The proxy for an object (one per object, so `===` works).
fn proxy(o: Obj, ctx: &mut Context) -> JsResult<JsValue> {
    let (i, cached) = {
        let b = ctx.get_data::<SharedBridge>().ok_or_else(|| error("the form is not available"))?;
        let mut b = b.borrow_mut();
        let i = match b.index.get(&o) {
            Some(i) => *i,
            None => {
                if b.objs.len() >= MAX_OBJECTS {
                    return Err(error("the script used too many form objects"));
                }
                b.objs.push(o.clone());
                let i = b.objs.len() - 1;
                b.index.insert(o, i);
                i
            }
        };
        (i, b.proxies.get(&i).cloned())
    };
    if let Some(p) = cached {
        return Ok(p.into());
    }
    let target = ObjectInitializer::new(ctx).property(js_string!("__i"), JsValue::from(i as f64), Attribute::empty()).build();
    let p: JsObject = JsProxyBuilder::new(target).get(trap_get).set(trap_set).has(trap_has).build(ctx)?.into();
    if let Some(b) = ctx.get_data::<SharedBridge>() {
        b.borrow_mut().proxies.insert(i, p.clone());
    }
    Ok(p.into())
}

/// A script value as JavaScript.
fn to_js(v: Val, ctx: &mut Context) -> JsResult<JsValue> {
    Ok(match v {
        Val::Null => JsValue::null(),
        Val::Num(n) => JsValue::from(n),
        Val::Str(t) => s(&t),
        Val::Bool(b) => JsValue::from(b),
        Val::Obj(Obj::List(l)) => {
            let mut items = Vec::with_capacity(l.len());
            for o in l {
                items.push(to_js(Val::Obj(o), ctx)?);
            }
            let a = JsArray::from_iter(items, ctx);
            let mk = ctx.global_object().get(js_string!("__mkList"), ctx)?;
            match mk.as_object() {
                Some(f) if f.is_callable() => f.call(&JsValue::undefined(), &[a.into()], ctx)?,
                _ => a.into(),
            }
        }
        Val::Obj(Obj::Script(n, node)) => script_object(n, node, ctx)?,
        Val::Obj(o) => proxy(o, ctx)?,
    })
}

/// A JavaScript value as a script value.
fn from_js(v: &JsValue, ctx: &mut Context) -> JsResult<Val> {
    if v.is_undefined() || v.is_null() {
        return Ok(Val::Null);
    }
    if let Some(n) = v.as_number() {
        return Ok(Val::Num(n));
    }
    if let Some(b) = v.as_boolean() {
        return Ok(Val::Bool(b));
    }
    if let Some(t) = v.as_string() {
        return Ok(Val::Str(t.to_std_string_escaped()));
    }
    if let Some(o) = v.as_object() {
        let i = o.get(js_string!("__i"), ctx)?;
        if i.as_number().is_some()
            && let Ok((_, obj)) = target_obj(&JsValue::from(o.clone()), ctx)
        {
            return Ok(Val::Obj(obj));
        }
    }
    Ok(Val::Str(v.to_string(ctx)?.to_std_string_escaped()))
}

fn method(i: usize, name: &str, ctx: &mut Context) -> JsValue {
    fn call(this: &JsValue, args: &[JsValue], captures: &(JsString, JsString), ctx: &mut Context) -> JsResult<JsValue> {
        let _ = this;
        let i = captures.0.to_std_string_escaped().parse::<usize>().map_err(|_| error("not a form object"))?;
        let name = captures.1.to_std_string_escaped();
        let o = {
            let b = ctx.get_data::<SharedBridge>().ok_or_else(|| error("the form is not available"))?;
            b.borrow().objs.get(i).cloned().ok_or_else(|| error("not a form object"))?
        };
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            vals.push(from_js(a, ctx)?);
        }
        let from = current(ctx);
        let r = with_host(ctx, |h| h.call(&o, &name, &vals, from))?.map_err(error)?;
        to_js(r, ctx)
    }
    let f = NativeFunction::from_copy_closure_with_captures(call, (JsString::from(i.to_string().as_str()), JsString::from(name)));
    FunctionObjectBuilder::new(ctx.realm(), f).name(JsString::from(name)).build().into()
}

/// The node of the running script.
fn current(ctx: &Context) -> N {
    ctx.get_data::<RefCell<Vec<N>>>().and_then(|s| s.borrow().last().copied()).unwrap_or(N::ROOT)
}

fn trap_get(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name) = key_name(&arg(args, 1), ctx)? else { return Ok(JsValue::undefined()) };
    let (i, o) = target_obj(&arg(args, 0), ctx)?;
    match name.as_str() {
        "__i" => return Ok(JsValue::from(i as f64)),
        "then" | "constructor" | "prototype" => return Ok(JsValue::undefined()),
        "toString" | "valueOf" | "toJSON" => {
            fn text(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
                Ok(s("[object XFAObject]"))
            }
            return Ok(FunctionObjectBuilder::new(ctx.realm(), NativeFunction::from_fn_ptr(text)).build().into());
        }
        _ => {}
    }
    if Host::is_method(&o, &name) {
        return Ok(method(i, &name, ctx));
    }
    let v = with_host(ctx, |h| h.get(&o, &name))?.map_err(error)?;
    match v {
        Some(v) => to_js(v, ctx),
        None => Ok(JsValue::undefined()),
    }
}

fn trap_set(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name) = key_name(&arg(args, 1), ctx)? else { return Ok(JsValue::from(true)) };
    let (_, o) = target_obj(&arg(args, 0), ctx)?;
    let v = from_js(&arg(args, 2), ctx)?;
    with_host(ctx, |h| h.set(&o, &name, v))?.map_err(error)?;
    Ok(JsValue::from(true))
}

fn trap_has(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name) = key_name(&arg(args, 1), ctx)? else { return Ok(JsValue::from(false)) };
    let (_, o) = target_obj(&arg(args, 0), ctx)?;
    if Host::is_method(&o, &name) {
        return Ok(JsValue::from(true));
    }
    let found = with_host(ctx, |h| h.get(&o, &name).ok().flatten().is_some())?;
    Ok(JsValue::from(found))
}

/// Scope proxies: `with (scope)` asks them for every bare identifier.
fn scope_has(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name) = key_name(&arg(args, 1), ctx)? else { return Ok(JsValue::from(false)) };
    if name.starts_with("__") || ctx.global_object().has_own_property(JsString::from(name.as_str()), ctx)? {
        return Ok(JsValue::from(false));
    }
    let (_, o) = target_obj(&arg(args, 0), ctx)?;
    let Obj::Node(from) = o else { return Ok(JsValue::from(false)) };
    let found = with_host(ctx, |h| h.lookup(from, &name).is_some())?;
    Ok(JsValue::from(found))
}

fn scope_get(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name) = key_name(&arg(args, 1), ctx)? else { return Ok(JsValue::undefined()) };
    let (_, o) = target_obj(&arg(args, 0), ctx)?;
    let Obj::Node(from) = o else { return Ok(JsValue::undefined()) };
    match with_host(ctx, |h| h.lookup(from, &name))? {
        Some(found) => to_js(Val::Obj(found), ctx),
        None => Ok(JsValue::undefined()),
    }
}

fn scope_set(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    // Assigning to a form object's name doesn't replace the object.
    Ok(JsValue::from(true))
}

/// The scope object for scripts on `n`.
fn scope(n: N, ctx: &mut Context) -> JsResult<JsValue> {
    // Scopes use their own registry slot (a node and its scope are different objects).
    let marker = Obj::List(vec![Obj::Node(n), Obj::Host]);
    let (i, cached) = {
        let b = ctx.get_data::<SharedBridge>().ok_or_else(|| error("the form is not available"))?;
        let mut b = b.borrow_mut();
        let i = match b.index.get(&marker) {
            Some(i) => *i,
            None => {
                b.objs.push(Obj::Node(n));
                let i = b.objs.len() - 1;
                b.index.insert(marker, i);
                i
            }
        };
        (i, b.proxies.get(&i).cloned())
    };
    if let Some(p) = cached {
        return Ok(p.into());
    }
    let target = ObjectInitializer::new(ctx).property(js_string!("__i"), JsValue::from(i as f64), Attribute::empty()).build();
    let p: JsObject = JsProxyBuilder::new(target).get(scope_get).set(scope_set).has(scope_has).build(ctx)?.into();
    if let Some(b) = ctx.get_data::<SharedBridge>() {
        b.borrow_mut().proxies.insert(i, p.clone());
    }
    Ok(p.into())
}

/// Names a script object's source declares at its top level (`function f`, `var x`).
fn declared(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let b = src.as_bytes();
    let mut i = 0;
    let word = |at: usize| -> String { src.get(at..).unwrap_or("").chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect() };
    while let Some(&c) = b.get(i) {
        match c {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'"' | b'\'' => {
                i += 1;
                while let Some(&d) = b.get(i) {
                    if d == b'\\' {
                        i += 1;
                    } else if d == c || d == b'\n' {
                        break;
                    }
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while b.get(i).is_some_and(|d| *d != b'\n') {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b.get(i) == Some(&b'*') && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 1;
            }
            _ if depth == 0 => {
                let prev_ok = i == 0 || b.get(i - 1).is_some_and(|p| !(p.is_ascii_alphanumeric() || *p == b'_' || *p == b'$'));
                if prev_ok {
                    for kw in ["function", "var", "let", "const"] {
                        if src.get(i..).is_some_and(|r| r.starts_with(kw)) && b.get(i + kw.len()).is_some_and(|x| x.is_ascii_whitespace()) {
                            let mut j = i + kw.len();
                            while b.get(j).is_some_and(|x| x.is_ascii_whitespace()) {
                                j += 1;
                            }
                            let w = word(j);
                            if !w.is_empty() && !out.contains(&w) && out.len() < 10_000 {
                                out.push(w);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// A `<variables>` script object, evaluated once per run.
fn script_object(n: N, node: NodeId, ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(o) = ctx.get_data::<SharedBridge>().and_then(|b| b.borrow().scripts.get(&node).cloned()) {
        return Ok(o.into());
    }
    let src = with_host(ctx, |h| h.live.tree().text(node))?;
    if let Some(why) = printcraft_js::refuse(&src) {
        return Err(error(why));
    }
    let names = declared(&src);
    let exports: Vec<String> = names.iter().map(|w| format!("{w}: (typeof {w} !== 'undefined' ? {w} : undefined)")).collect();
    let body = format!("{src}\n;return {{{}}};", exports.join(", "));
    let sc = scope(n, ctx)?;
    let run = ctx.global_object().get(js_string!("__module"), ctx)?;
    let f = run.as_object().filter(|f| f.is_callable()).ok_or_else(|| error("the script host is not set up"))?;
    let o = f.call(&JsValue::undefined(), &[sc, s(&body)], ctx)?;
    let obj = o.as_object().ok_or_else(|| error("the script object didn't load"))?;
    if let Some(b) = ctx.get_data::<SharedBridge>() {
        b.borrow_mut().scripts.insert(node, obj.clone());
    }
    Ok(obj.into())
}

const PRELUDE: &str = r#"
var __run = function (__t, __s, __src) { return (function () { with (__s) { return eval(__src); } }).call(__t); };
var __module = function (__s, __src) { return (function () { with (__s) { return (new Function("__s", "with (__s) {" + __src + "}"))(__s); } })(); };
var __mkList = function (a) { Object.defineProperty(a, "item", { value: function (i) { var v = this[i]; return v === undefined ? null : v; } }); return a; };
"#;

fn native(ctx: &mut Context, f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>) -> JsValue {
    FunctionObjectBuilder::new(ctx.realm(), NativeFunction::from_fn_ptr(f)).build().into()
}

fn alert(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let m = arg(args, 0);
    // app.alert takes a message or an object with cMsg.
    let text = match m.as_object() {
        Some(o) if !o.is_callable() => o.get(js_string!("cMsg"), ctx)?.to_string(ctx)?.to_std_string_escaped(),
        _ => m.to_string(ctx)?.to_std_string_escaped(),
    };
    with_host(ctx, |h| {
        if h.out.messages.len() < 1_000 {
            h.out.messages.push(text);
        }
    })?;
    Ok(JsValue::from(1))
}

fn println(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let text = arg(args, 0).to_string(ctx)?.to_std_string_escaped();
    with_host(ctx, |h| {
        if h.out.console.len() < 10_000 {
            h.out.console.push(text);
        }
    })?;
    Ok(JsValue::undefined())
}

fn nothing(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::null())
}

fn launch_url(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let u = arg(args, 0).to_string(ctx)?.to_std_string_escaped();
    with_host(ctx, |h| {
        if h.out.urls.len() < 100 {
            h.out.urls.push(u);
        }
    })?;
    Ok(JsValue::undefined())
}

/// A JavaScript runtime with the XFA objects installed.
pub struct Runtime {
    ctx: Context,
}

impl Runtime {
    pub fn new(host: Host, limits: printcraft_js::Limits) -> JsResult<Runtime> {
        let mut ctx = Context::default();
        ctx.runtime_limits_mut().set_loop_iteration_limit(limits.loop_iterations);
        ctx.runtime_limits_mut().set_recursion_limit(limits.recursion);
        ctx.insert_data(RefCell::new(host));
        ctx.insert_data(RefCell::new(Bridge::default()));
        ctx.insert_data(RefCell::new(Vec::<N>::new()));
        ctx.eval(Source::from_bytes(PRELUDE))?;
        let xfa = proxy(Obj::Xfa, &mut ctx)?;
        ctx.register_global_property(js_string!("xfa"), xfa, Attribute::all())?;
        let event = proxy(Obj::Event, &mut ctx)?;
        ctx.register_global_property(js_string!("event"), event, Attribute::all())?;
        let (alert_f, println_f, nothing_f, url_f) =
            (native(&mut ctx, alert), native(&mut ctx, println), native(&mut ctx, nothing), native(&mut ctx, launch_url));
        let app = ObjectInitializer::new(&mut ctx)
            .property(js_string!("alert"), alert_f, Attribute::all())
            .property(js_string!("beep"), nothing_f.clone(), Attribute::all())
            .property(js_string!("response"), nothing_f.clone(), Attribute::all())
            .property(js_string!("setTimeOut"), nothing_f.clone(), Attribute::all())
            .property(js_string!("setInterval"), nothing_f.clone(), Attribute::all())
            .property(js_string!("clearTimeOut"), nothing_f.clone(), Attribute::all())
            .property(js_string!("clearInterval"), nothing_f.clone(), Attribute::all())
            .property(js_string!("launchURL"), url_f, Attribute::all())
            .property(js_string!("viewerType"), s("Exchange-Pro"), Attribute::all())
            .property(js_string!("viewerVariation"), s("Full"), Attribute::all())
            .property(js_string!("viewerVersion"), JsValue::from(11), Attribute::all())
            .property(js_string!("platform"), s("WIN"), Attribute::all())
            .property(js_string!("language"), s("ENU"), Attribute::all())
            .build();
        ctx.register_global_property(js_string!("app"), app, Attribute::all())?;
        let console = ObjectInitializer::new(&mut ctx)
            .property(js_string!("println"), println_f, Attribute::all())
            .property(js_string!("show"), nothing_f.clone(), Attribute::all())
            .property(js_string!("clear"), nothing_f, Attribute::all())
            .build();
        ctx.register_global_property(js_string!("console"), console, Attribute::all())?;
        Ok(Runtime { ctx })
    }

    pub fn host(&self) -> Option<std::cell::RefMut<'_, Host>> {
        self.ctx.get_data::<Shared>().and_then(|h| h.try_borrow_mut().ok())
    }

    pub fn into_host(mut self) -> Option<Host> {
        self.ctx.remove_data::<Shared>().map(|h| h.into_inner())
    }

    /// Run `src` with `this` = `n`; the value of its last expression (`None`: undefined).
    pub fn run(&mut self, n: N, src: &str) -> Result<Option<Val>, String> {
        if let Some(why) = printcraft_js::refuse(src) {
            return Err(why);
        }
        if let Some(stack) = self.ctx.get_data::<RefCell<Vec<N>>>() {
            stack.borrow_mut().push(n);
        }
        let r = self.run_inner(n, src);
        if let Some(stack) = self.ctx.get_data::<RefCell<Vec<N>>>() {
            stack.borrow_mut().pop();
        }
        r.map_err(|e| e.to_string())
    }

    fn run_inner(&mut self, n: N, src: &str) -> JsResult<Option<Val>> {
        let ctx = &mut self.ctx;
        let this = proxy(Obj::Node(n), ctx)?;
        let sc = scope(n, ctx)?;
        let run = ctx.global_object().get(js_string!("__run"), ctx)?;
        let f = run.as_object().filter(|f| f.is_callable()).ok_or_else(|| error("the script host is not set up"))?;
        let r = f.call(&JsValue::undefined(), &[this, sc, s(src)], ctx)?;
        if r.is_undefined() {
            return Ok(None);
        }
        Ok(Some(from_js(&r, ctx)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_top_level_declarations() {
        let d = declared("function a(x) { var inner = 1; }\nvar b = 2; // var c\nconst  e = 'var f';\nfunction g(){}");
        assert_eq!(d, vec!["a", "b", "e", "g"]);
    }
}
