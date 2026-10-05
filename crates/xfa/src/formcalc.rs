//! FormCalc (XFA 3.3 Part 1, "FormCalc Specification"): the default script language of XFA
//! forms. A lexer, a recursive-descent parser and a tree-walking evaluator over the scripting
//! object model ([`Host`]).
//!
//! Supported: numbers, strings and null; arithmetic, comparison and logical operators;
//! accessors (`a.b[2].c`, `Row[*].Amount`, `..Total`, `$`, `$form`, `$record`, `xfa.…`,
//! `_Row`), property reads and writes, method calls; `var`, `if/elseif/else`, `while`,
//! `for … upto/downto … step`, `foreach … in`, `func`, `break`, `continue`, `return`; and the
//! built-in functions of the arithmetic, date, logical and string families (`Sum`, `Round`,
//! `Date`, `Num2Date`, `Concat`, `Substr`, `HasValue`, `WordNum`, …). Not supported: the
//! financial functions and the URL functions (no network access).
//!
//! Scripts are untrusted: the parser bounds nesting, and evaluation bounds steps, recursion
//! and string sizes.

use std::collections::HashMap;
use std::rc::Rc;

use crate::dom::{self, N};
use crate::host::{Host, Obj, Val};
use crate::picture;

const MAX_SOURCE: usize = 1 << 20;
const MAX_NESTING: usize = 200;
const MAX_STEPS: u64 = 2_000_000;
const MAX_CALL_DEPTH: usize = 64;
const MAX_STRING: usize = 1 << 20;

// ── lexer ───────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Op(&'static str),
    Eof,
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    let at = |i: usize| chars.get(i).copied();
    while let Some(c) = at(i) {
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // Comments: `;` or `//` to the end of the line.
        if c == ';' || (c == '/' && at(i + 1) == Some('/')) {
            while at(i).is_some_and(|x| x != '\n') {
                i += 1;
            }
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && at(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let start = i;
            while at(i).is_some_and(|d| d.is_ascii_digit() || d == '.') {
                i += 1;
            }
            if at(i).is_some_and(|e| e == 'e' || e == 'E') && at(i + 1).is_some_and(|d| d.is_ascii_digit() || d == '-' || d == '+') {
                i += 2;
                while at(i).is_some_and(|d| d.is_ascii_digit()) {
                    i += 1;
                }
            }
            let text: String = chars.get(start..i).map(|s| s.iter().collect()).unwrap_or_default();
            out.push(Tok::Num(text.parse::<f64>().map_err(|_| format!("bad number {text}"))?));
            continue;
        }
        if c == '"' {
            i += 1;
            let mut s = String::new();
            loop {
                match at(i) {
                    None => return Err("a string is not closed".into()),
                    Some('"') if at(i + 1) == Some('"') => {
                        s.push('"');
                        i += 2;
                    }
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some('\\') if at(i + 1) == Some('u') => {
                        let hex: String = chars.get(i + 2..i + 6).map(|h| h.iter().collect()).unwrap_or_default();
                        match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                            Some(ch) => {
                                s.push(ch);
                                i += 6;
                            }
                            None => {
                                s.push('\\');
                                i += 1;
                            }
                        }
                    }
                    Some(ch) => {
                        s.push(ch);
                        i += 1;
                    }
                }
                if s.len() > MAX_STRING {
                    return Err("a string is too long".into());
                }
            }
            out.push(Tok::Str(s));
            continue;
        }
        let ident_start = |ch: char| ch.is_alphabetic() || ch == '_' || ch == '$' || ch == '#';
        if ident_start(c) || (c == '!' && at(i + 1).is_some_and(ident_start)) {
            let start = i;
            i += 1;
            while at(i).is_some_and(|d| d.is_alphanumeric() || d == '_' || d == '$') {
                i += 1;
            }
            out.push(Tok::Ident(chars.get(start..i).map(|s| s.iter().collect()).unwrap_or_default()));
            continue;
        }
        let two: String = chars.get(i..i + 2).map(|s| s.iter().collect()).unwrap_or_default();
        let op: &'static str = match two.as_str() {
            "<=" => "<=",
            ">=" => ">=",
            "<>" => "<>",
            "==" => "==",
            ".." => "..",
            ".*" => ".*",
            _ => match c {
                '=' => "=",
                '<' => "<",
                '>' => ">",
                '+' => "+",
                '-' => "-",
                '*' => "*",
                '/' => "/",
                '&' => "&",
                '|' => "|",
                '!' => "!",
                '(' => "(",
                ')' => ")",
                '[' => "[",
                ']' => "]",
                ',' => ",",
                '.' => ".",
                _ => return Err(format!("unexpected character '{c}'")),
            },
        };
        i += op.len();
        out.push(Tok::Op(op));
    }
    out.push(Tok::Eof);
    Ok(out)
}

// ── parser ──────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Index {
    One,
    All,
    At(Box<Expr>),
}

#[derive(Clone, Debug)]
enum Step {
    Name { name: String, index: Index, desc: bool },
    Call { name: String, args: Vec<Expr> },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Debug)]
enum Expr {
    Num(f64),
    Str(String),
    Null,
    Acc(Vec<Step>),
    Call(String, Vec<Expr>),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Assign(Box<Expr>, Box<Expr>),
    Var(String, Option<Box<Expr>>),
    If(Vec<(Expr, Vec<Expr>)>, Option<Vec<Expr>>),
    While(Box<Expr>, Vec<Expr>),
    For { var: String, start: Box<Expr>, end: Box<Expr>, down: bool, step: Option<Box<Expr>>, body: Vec<Expr> },
    Foreach { var: String, list: Vec<Expr>, body: Vec<Expr> },
    Func(String, Rc<(Vec<String>, Vec<Expr>)>),
    Break,
    Continue,
    Return(Option<Box<Expr>>),
    Block(Vec<Expr>),
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    depth: usize,
}

fn kw(t: &Tok, word: &str) -> bool {
    matches!(t, Tok::Ident(s) if s.eq_ignore_ascii_case(word))
}

const STOP_WORDS: &[&str] = &["then", "elseif", "else", "endif", "do", "endwhile", "endfor", "endfunc", "upto", "downto", "step", "in"];

impl Parser {
    fn peek(&self) -> &Tok {
        self.toks.get(self.pos).unwrap_or(&Tok::Eof)
    }

    fn peek_at(&self, k: usize) -> &Tok {
        self.toks.get(self.pos + k).unwrap_or(&Tok::Eof)
    }

    fn next(&mut self) -> Tok {
        let t = self.peek().clone();
        self.pos += 1;
        t
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Tok::Op(o) if *o == op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_op(&mut self, op: &str) -> Result<(), String> {
        if self.eat_op(op) { Ok(()) } else { Err(format!("expected '{op}' but found {:?}", self.peek())) }
    }

    fn eat_kw(&mut self, word: &str) -> bool {
        if kw(self.peek(), word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, word: &str) -> Result<(), String> {
        if self.eat_kw(word) { Ok(()) } else { Err(format!("expected '{word}' but found {:?}", self.peek())) }
    }

    fn ident(&mut self) -> Result<String, String> {
        match self.next() {
            Tok::Ident(s) => Ok(s),
            t => Err(format!("expected a name but found {t:?}")),
        }
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_NESTING { Err("the script is nested too deeply".into()) } else { Ok(()) }
    }

    /// Expressions until one of the stop words (or the end).
    fn list(&mut self, stops: &[&str]) -> Result<Vec<Expr>, String> {
        let mut out = Vec::new();
        loop {
            match self.peek() {
                Tok::Eof => break,
                Tok::Op(")") => break,
                t if stops.iter().any(|w| kw(t, w)) => break,
                _ => out.push(self.expr()?),
            }
        }
        Ok(out)
    }

    fn expr(&mut self) -> Result<Expr, String> {
        self.enter()?;
        let r = self.expr_inner();
        self.depth -= 1;
        r
    }

    fn expr_inner(&mut self) -> Result<Expr, String> {
        let t = self.peek().clone();
        if kw(&t, "var") {
            self.pos += 1;
            let name = self.ident()?;
            let init = if self.eat_op("=") { Some(Box::new(self.expr()?)) } else { None };
            return Ok(Expr::Var(name, init));
        }
        if kw(&t, "if") {
            self.pos += 1;
            let mut arms = Vec::new();
            self.expect_op("(")?;
            let c = self.expr()?;
            self.expect_op(")")?;
            self.expect_kw("then")?;
            arms.push((c, self.list(&["elseif", "else", "endif"])?));
            let mut otherwise = None;
            loop {
                if self.eat_kw("elseif") {
                    self.expect_op("(")?;
                    let c = self.expr()?;
                    self.expect_op(")")?;
                    self.expect_kw("then")?;
                    arms.push((c, self.list(&["elseif", "else", "endif"])?));
                } else if self.eat_kw("else") {
                    otherwise = Some(self.list(&["endif"])?);
                } else {
                    self.expect_kw("endif")?;
                    break;
                }
            }
            return Ok(Expr::If(arms, otherwise));
        }
        if kw(&t, "while") {
            self.pos += 1;
            self.expect_op("(")?;
            let c = self.expr()?;
            self.expect_op(")")?;
            self.expect_kw("do")?;
            let body = self.list(&["endwhile"])?;
            self.expect_kw("endwhile")?;
            return Ok(Expr::While(Box::new(c), body));
        }
        if kw(&t, "for") {
            self.pos += 1;
            let var = self.ident()?;
            self.expect_op("=")?;
            let start = self.expr()?;
            let down = if self.eat_kw("upto") {
                false
            } else if self.eat_kw("downto") {
                true
            } else {
                return Err("expected 'upto' or 'downto'".into());
            };
            let end = self.expr()?;
            let step = if self.eat_kw("step") { Some(Box::new(self.expr()?)) } else { None };
            self.expect_kw("do")?;
            let body = self.list(&["endfor"])?;
            self.expect_kw("endfor")?;
            return Ok(Expr::For { var, start: Box::new(start), end: Box::new(end), down, step, body });
        }
        if kw(&t, "foreach") {
            self.pos += 1;
            let var = self.ident()?;
            self.expect_kw("in")?;
            self.expect_op("(")?;
            let list = self.args()?;
            self.expect_kw("do")?;
            let body = self.list(&["endfor"])?;
            self.expect_kw("endfor")?;
            return Ok(Expr::Foreach { var, list, body });
        }
        if kw(&t, "func") {
            self.pos += 1;
            let name = self.ident()?;
            self.expect_op("(")?;
            let mut params = Vec::new();
            if !self.eat_op(")") {
                loop {
                    params.push(self.ident()?);
                    if self.eat_op(")") {
                        break;
                    }
                    self.expect_op(",")?;
                }
            }
            self.expect_kw("do")?;
            let body = self.list(&["endfunc"])?;
            self.expect_kw("endfunc")?;
            return Ok(Expr::Func(name.to_ascii_lowercase(), Rc::new((params, body))));
        }
        if kw(&t, "break") {
            self.pos += 1;
            return Ok(Expr::Break);
        }
        if kw(&t, "continue") {
            self.pos += 1;
            return Ok(Expr::Continue);
        }
        if kw(&t, "return") {
            self.pos += 1;
            let v = match self.peek() {
                Tok::Eof => None,
                t if STOP_WORDS.iter().any(|w| kw(t, w)) || kw(t, "endif") => None,
                _ => Some(Box::new(self.expr()?)),
            };
            return Ok(Expr::Return(v));
        }
        if kw(&t, "do") {
            self.pos += 1;
            let body = self.list(&["end"])?;
            self.expect_kw("end")?;
            return Ok(Expr::Block(body));
        }
        let lhs = self.or()?;
        if matches!(lhs, Expr::Acc(_)) && self.eat_op("=") {
            let rhs = self.expr()?;
            return Ok(Expr::Assign(Box::new(lhs), Box::new(rhs)));
        }
        Ok(lhs)
    }

    fn bin(&mut self, ops: &[(&str, BinOp)], words: &[(&str, BinOp)], next: fn(&mut Parser) -> Result<Expr, String>) -> Result<Expr, String> {
        let mut lhs = next(self)?;
        loop {
            let op = match self.peek() {
                Tok::Op(o) => ops.iter().find(|(s, _)| s == o).map(|(_, b)| *b),
                t => words.iter().find(|(w, _)| kw(t, w)).map(|(_, b)| *b),
            };
            let Some(op) = op else { break };
            self.pos += 1;
            self.enter()?;
            let rhs = next(self)?;
            self.depth -= 1;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn or(&mut self) -> Result<Expr, String> {
        self.bin(&[("|", BinOp::Or)], &[("or", BinOp::Or)], Parser::and)
    }

    fn and(&mut self) -> Result<Expr, String> {
        self.bin(&[("&", BinOp::And)], &[("and", BinOp::And)], Parser::equality)
    }

    fn equality(&mut self) -> Result<Expr, String> {
        self.bin(&[("==", BinOp::Eq), ("<>", BinOp::Ne)], &[("eq", BinOp::Eq), ("ne", BinOp::Ne)], Parser::relational)
    }

    fn relational(&mut self) -> Result<Expr, String> {
        self.bin(
            &[("<=", BinOp::Le), (">=", BinOp::Ge), ("<", BinOp::Lt), (">", BinOp::Gt)],
            &[("le", BinOp::Le), ("ge", BinOp::Ge), ("lt", BinOp::Lt), ("gt", BinOp::Gt)],
            Parser::additive,
        )
    }

    fn additive(&mut self) -> Result<Expr, String> {
        self.bin(&[("+", BinOp::Add), ("-", BinOp::Sub)], &[], Parser::multiplicative)
    }

    fn multiplicative(&mut self) -> Result<Expr, String> {
        self.bin(&[("*", BinOp::Mul), ("/", BinOp::Div)], &[], Parser::unary)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        self.enter()?;
        let r = if self.eat_op("-") {
            self.unary().map(|e| Expr::Neg(Box::new(e)))
        } else if self.eat_op("+") {
            self.unary()
        } else if self.eat_op("!") || self.eat_kw("not") {
            self.unary().map(|e| Expr::Not(Box::new(e)))
        } else {
            self.primary()
        };
        self.depth -= 1;
        r
    }

    fn args(&mut self) -> Result<Vec<Expr>, String> {
        let mut out = Vec::new();
        if self.eat_op(")") {
            return Ok(out);
        }
        loop {
            out.push(self.expr()?);
            if self.eat_op(")") {
                return Ok(out);
            }
            self.expect_op(",")?;
            if out.len() > 10_000 {
                return Err("too many arguments".into());
            }
        }
    }

    fn index(&mut self) -> Result<Index, String> {
        if self.eat_op("*") {
            self.expect_op("]")?;
            return Ok(Index::All);
        }
        let e = self.expr()?;
        self.expect_op("]")?;
        Ok(Index::At(Box::new(e)))
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Tok::Num(n) => Ok(Expr::Num(n)),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::Op("(") => {
                let body = self.list(&[])?;
                self.expect_op(")")?;
                Ok(match body.len() {
                    1 => body.into_iter().next().unwrap_or(Expr::Null),
                    _ => Expr::Block(body),
                })
            }
            Tok::Ident(name) => {
                if name.eq_ignore_ascii_case("null") {
                    return Ok(Expr::Null);
                }
                if name.eq_ignore_ascii_case("infinity") {
                    return Ok(Expr::Num(f64::INFINITY));
                }
                if name.eq_ignore_ascii_case("nan") {
                    return Ok(Expr::Num(f64::NAN));
                }
                // A function call: name(…) with nothing before it.
                if matches!(self.peek(), Tok::Op("(")) {
                    self.pos += 1;
                    let args = self.args()?;
                    return Ok(Expr::Call(name.to_ascii_lowercase(), args));
                }
                let mut steps = vec![Step::Name { name, index: Index::One, desc: false }];
                loop {
                    if self.eat_op("[") {
                        let idx = self.index()?;
                        if let Some(Step::Name { index, .. }) = steps.last_mut() {
                            *index = idx;
                        }
                        continue;
                    }
                    let desc = if self.eat_op("..") {
                        true
                    } else if self.eat_op(".") {
                        false
                    } else if self.eat_op(".*") {
                        steps.push(Step::Name { name: "*".into(), index: Index::All, desc: false });
                        continue;
                    } else {
                        break;
                    };
                    let name = self.ident()?;
                    if matches!(self.peek(), Tok::Op("(")) && !desc {
                        self.pos += 1;
                        let args = self.args()?;
                        steps.push(Step::Call { name, args });
                    } else {
                        steps.push(Step::Name { name, index: Index::One, desc });
                    }
                    if steps.len() > 256 {
                        return Err("the reference is too long".into());
                    }
                }
                // `name(` right after an accessor would be ambiguous: let the peek above decide.
                let _ = self.peek_at(0);
                Ok(Expr::Acc(steps))
            }
            t => Err(format!("unexpected {t:?}")),
        }
    }
}

fn parse(src: &str) -> Result<Vec<Expr>, String> {
    if src.len() > MAX_SOURCE {
        return Err("the script is too long".into());
    }
    let toks = lex(src)?;
    let mut p = Parser { toks, pos: 0, depth: 0 };
    let body = p.list(&[])?;
    if !matches!(p.peek(), Tok::Eof) {
        return Err(format!("unexpected {:?}", p.peek()));
    }
    Ok(body)
}

// ── evaluation ──────────────────────────────────────────────────────────────────────────────

/// A FormCalc value.
#[derive(Clone, Debug, PartialEq)]
enum V {
    Null,
    Num(f64),
    Str(String),
    Objs(Vec<Obj>),
}

enum Flow {
    Break,
    Continue,
    Return(V),
    Error(String),
}

type R<T> = Result<T, Flow>;

fn err<T>(m: impl Into<String>) -> R<T> {
    Err(Flow::Error(m.into()))
}

/// A user function: parameter names and body.
type FuncDef = Rc<(Vec<String>, Vec<Expr>)>;

struct Eval<'h> {
    host: &'h mut Host,
    this: N,
    scopes: Vec<HashMap<String, V>>,
    funcs: Vec<HashMap<String, FuncDef>>,
    steps: u64,
    calls: usize,
}

fn from_val(v: Val) -> V {
    match v {
        Val::Null => V::Null,
        Val::Num(n) => V::Num(n),
        Val::Str(s) => V::Str(s),
        Val::Bool(b) => V::Num(f64::from(u8::from(b))),
        Val::Obj(Obj::List(l)) => V::Objs(l),
        Val::Obj(o) => V::Objs(vec![o]),
    }
}

fn to_val(v: &V) -> Val {
    match v {
        V::Null => Val::Null,
        V::Num(n) => Val::Num(*n),
        V::Str(s) => Val::Str(s.clone()),
        V::Objs(l) if l.len() == 1 => l.first().cloned().map(Val::Obj).unwrap_or(Val::Null),
        V::Objs(l) => Val::Obj(Obj::List(l.clone())),
    }
}

fn parse_num(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok()
}

impl Eval<'_> {
    fn tick(&mut self) -> R<()> {
        self.steps += 1;
        if self.steps > MAX_STEPS { err("the script ran too long and was stopped") } else { Ok(()) }
    }

    /// The scalar a value stands for (objects give their value).
    fn scalar(&mut self, v: V) -> V {
        match v {
            V::Objs(l) => match l.first() {
                Some(o) => from_scalar(self.host.value_of(o)),
                None => V::Null,
            },
            other => other,
        }
    }

    fn num(&mut self, v: V) -> f64 {
        match self.scalar(v) {
            V::Null => 0.0,
            V::Num(n) => n,
            V::Str(s) => parse_num(&s).unwrap_or(0.0),
            V::Objs(_) => 0.0,
        }
    }

    fn text(&mut self, v: V) -> String {
        match self.scalar(v) {
            V::Null => String::new(),
            V::Num(n) => dom::number_text(n),
            V::Str(s) => s,
            V::Objs(_) => String::new(),
        }
    }

    fn truthy(&mut self, v: V) -> bool {
        let n = self.num(v);
        n != 0.0 && !n.is_nan()
    }

    fn var(&self, name: &str) -> Option<&V> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    fn var_mut(&mut self, name: &str) -> Option<&mut V> {
        self.scopes.iter_mut().rev().find_map(|s| s.get_mut(name))
    }

    fn block(&mut self, body: &[Expr]) -> R<V> {
        self.scopes.push(HashMap::new());
        self.funcs.push(HashMap::new());
        let mut last = V::Null;
        let mut r = Ok(());
        for e in body {
            match self.eval(e) {
                Ok(v) => last = v,
                Err(f) => {
                    r = Err(f);
                    break;
                }
            }
        }
        self.scopes.pop();
        self.funcs.pop();
        r.map(|_| last)
    }

    fn eval(&mut self, e: &Expr) -> R<V> {
        self.tick()?;
        match e {
            Expr::Num(n) => Ok(V::Num(*n)),
            Expr::Str(s) => Ok(V::Str(s.clone())),
            Expr::Null => Ok(V::Null),
            Expr::Acc(steps) => self.access(steps),
            Expr::Call(name, args) => self.call(name, args),
            Expr::Neg(x) => {
                let v = self.eval(x)?;
                Ok(match self.scalar(v) {
                    V::Null => V::Null,
                    v => V::Num(-self.num(v)),
                })
            }
            Expr::Not(x) => {
                let v = self.eval(x)?;
                Ok(match self.scalar(v) {
                    V::Null => V::Null,
                    v => V::Num(if self.truthy(v) { 0.0 } else { 1.0 }),
                })
            }
            Expr::Bin(op, a, b) => {
                // Logical operators short-circuit.
                if matches!(op, BinOp::And | BinOp::Or) {
                    let l = self.eval(a)?;
                    let lt = self.truthy(l);
                    if (*op == BinOp::And && !lt) || (*op == BinOp::Or && lt) {
                        return Ok(V::Num(if lt { 1.0 } else { 0.0 }));
                    }
                    let r = self.eval(b)?;
                    let rt = self.truthy(r);
                    return Ok(V::Num(if rt { 1.0 } else { 0.0 }));
                }
                let l = self.eval(a)?;
                let r = self.eval(b)?;
                self.binary(*op, l, r)
            }
            Expr::Assign(target, value) => {
                let v = self.eval(value)?;
                let v = self.scalar(v);
                let Expr::Acc(steps) = target.as_ref() else { return err("only names can be assigned") };
                self.assign(steps, v.clone())?;
                Ok(v)
            }
            Expr::Var(name, init) => {
                let v = match init {
                    Some(x) => {
                        let v = self.eval(x)?;
                        // Variables hold values, or the objects `Ref()` returned.
                        if matches!(&v, V::Objs(_)) && matches!(x.as_ref(), Expr::Call(f, _) if f == "ref") { v } else { self.scalar(v) }
                    }
                    None => V::Str(String::new()),
                };
                if let Some(s) = self.scopes.last_mut() {
                    s.insert(name.to_ascii_lowercase(), v.clone());
                }
                Ok(v)
            }
            Expr::If(arms, otherwise) => {
                for (c, body) in arms {
                    let cv = self.eval(c)?;
                    if self.truthy(cv) {
                        return self.block(body);
                    }
                }
                match otherwise {
                    Some(b) => self.block(b),
                    None => Ok(V::Null),
                }
            }
            Expr::While(c, body) => {
                let mut last = V::Null;
                loop {
                    let cv = self.eval(c)?;
                    if !self.truthy(cv) {
                        break;
                    }
                    match self.block(body) {
                        Ok(v) => last = v,
                        Err(Flow::Break) => break,
                        Err(Flow::Continue) => {}
                        Err(f) => return Err(f),
                    }
                }
                Ok(last)
            }
            Expr::For { var, start, end, down, step, body } => {
                let s = self.eval(start)?;
                let mut i = self.num(s);
                let e = self.eval(end)?;
                let end = self.num(e);
                let st = match step {
                    Some(x) => {
                        let v = self.eval(x)?;
                        self.num(v).abs()
                    }
                    None => 1.0,
                };
                if st == 0.0 || !st.is_finite() || !i.is_finite() {
                    return err("the loop step is not a usable number");
                }
                let name = var.to_ascii_lowercase();
                let mut last = V::Null;
                self.scopes.push(HashMap::new());
                let r = loop {
                    if (*down && i < end) || (!*down && i > end) {
                        break Ok(());
                    }
                    if let Some(s) = self.scopes.last_mut() {
                        s.insert(name.clone(), V::Num(i));
                    }
                    match self.block(body) {
                        Ok(v) => last = v,
                        Err(Flow::Break) => break Ok(()),
                        Err(Flow::Continue) => {}
                        Err(f) => break Err(f),
                    }
                    // The body may change the variable.
                    i = match self.var(&name).cloned() {
                        Some(v) => self.num(v),
                        None => i,
                    };
                    i = if *down { i - st } else { i + st };
                };
                self.scopes.pop();
                r.map(|_| last)
            }
            Expr::Foreach { var, list, body } => {
                let mut items = Vec::new();
                for a in list {
                    match self.eval(a)? {
                        V::Objs(l) => {
                            for o in l {
                                items.push(from_scalar(self.host.value_of(&o)));
                            }
                        }
                        v => items.push(v),
                    }
                }
                let name = var.to_ascii_lowercase();
                let mut last = V::Null;
                self.scopes.push(HashMap::new());
                let mut r = Ok(());
                for it in items {
                    if let Some(s) = self.scopes.last_mut() {
                        s.insert(name.clone(), it);
                    }
                    match self.block(body) {
                        Ok(v) => last = v,
                        Err(Flow::Break) => break,
                        Err(Flow::Continue) => {}
                        Err(f) => {
                            r = Err(f);
                            break;
                        }
                    }
                }
                self.scopes.pop();
                r.map(|_| last)
            }
            Expr::Func(name, def) => {
                if let Some(f) = self.funcs.last_mut() {
                    f.insert(name.clone(), def.clone());
                }
                Ok(V::Null)
            }
            Expr::Break => Err(Flow::Break),
            Expr::Continue => Err(Flow::Continue),
            Expr::Return(v) => {
                let v = match v {
                    Some(x) => {
                        let v = self.eval(x)?;
                        self.scalar(v)
                    }
                    None => V::Null,
                };
                Err(Flow::Return(v))
            }
            Expr::Block(body) => self.block(body),
        }
    }

    fn binary(&mut self, op: BinOp, l: V, r: V) -> R<V> {
        let (l, r) = (self.scalar(l), self.scalar(r));
        let both_null = l == V::Null && r == V::Null;
        match op {
            BinOp::Eq | BinOp::Ne => {
                let eq = match (&l, &r) {
                    (V::Null, V::Null) => true,
                    (V::Null, _) | (_, V::Null) => false,
                    (V::Str(a), V::Str(b)) => a == b,
                    _ => self.num(l.clone()) == self.num(r.clone()),
                };
                Ok(V::Num(if (op == BinOp::Eq) == eq { 1.0 } else { 0.0 }))
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let ord = match (&l, &r) {
                    (V::Str(a), V::Str(b)) => a.partial_cmp(b),
                    _ => self.num(l.clone()).partial_cmp(&self.num(r.clone())),
                };
                let Some(ord) = ord else { return Ok(V::Num(0.0)) };
                let t = match op {
                    BinOp::Lt => ord.is_lt(),
                    BinOp::Le => ord.is_le(),
                    BinOp::Gt => ord.is_gt(),
                    _ => ord.is_ge(),
                };
                Ok(V::Num(if t { 1.0 } else { 0.0 }))
            }
            _ if both_null => Ok(V::Null),
            BinOp::Add => Ok(V::Num(self.num(l) + self.num(r))),
            BinOp::Sub => Ok(V::Num(self.num(l) - self.num(r))),
            BinOp::Mul => Ok(V::Num(self.num(l) * self.num(r))),
            BinOp::Div => {
                let d = self.num(r);
                if d == 0.0 {
                    return err("division by zero");
                }
                Ok(V::Num(self.num(l) / d))
            }
            BinOp::And | BinOp::Or => Ok(V::Null),
        }
    }

    fn index_of(&mut self, idx: &Index) -> R<Option<Option<usize>>> {
        // None: all; Some(None): the default; Some(Some(i)): that one.
        Ok(match idx {
            Index::One => Some(None),
            Index::All => None,
            Index::At(e) => {
                let v = self.eval(e)?;
                let n = self.num(v);
                Some(Some(if n.is_finite() && n > 0.0 { n as usize } else { 0 }))
            }
        })
    }

    /// The first step: a variable, a special name, or a form object found by scope.
    fn first(&mut self, name: &str, idx: &Index) -> R<V> {
        if let Some(v) = self.var(&name.to_ascii_lowercase()).cloned() {
            return Ok(v);
        }
        let objs: Vec<Obj> = match name {
            "$" => vec![Obj::Node(self.this)],
            n if n.starts_with('$') || n == "xfa" => self.host.resolve(self.this, n),
            n if n.starts_with('!') => {
                let rest = n.trim_start_matches('!');
                self.host.resolve(self.this, &format!("xfa.datasets.{rest}"))
            }
            n => match self.host.lookup(self.this, n) {
                Some(o) => vec![o],
                None => return err(format!("{n} is not a known object or variable")),
            },
        };
        let which = self.index_of(idx)?;
        Ok(V::Objs(match (objs.first(), which) {
            (Some(Obj::Node(k)), None) => self.host.live.same_named(*k).into_iter().map(Obj::Node).collect(),
            (Some(Obj::Node(k)), Some(Some(i))) => self.host.live.same_named(*k).get(i).copied().map(Obj::Node).into_iter().collect(),
            _ => objs,
        }))
    }

    /// Children of `objs` named `name`, as a SOM step.
    fn children(&mut self, objs: &[Obj], name: &str, idx: &Index, desc: bool) -> R<Vec<Obj>> {
        let which = self.index_of(idx)?;
        let mut out = Vec::new();
        for o in objs {
            let som = match &which {
                None => format!("{name}[*]"),
                Some(Some(i)) => format!("{name}[{i}]"),
                Some(None) => name.to_string(),
            };
            let som = if desc { format!("..{som}") } else { som };
            out.extend(self.host.step(o, &som));
        }
        Ok(out)
    }

    fn access(&mut self, steps: &[Step]) -> R<V> {
        let Some(Step::Name { name, index, .. }) = steps.first() else { return err("bad reference") };
        let mut cur = self.first(name, index)?;
        let n = steps.len();
        for (k, step) in steps.iter().enumerate().skip(1) {
            let objs = match &cur {
                V::Objs(l) => l.clone(),
                _ => return err(format!("{name} has no properties")),
            };
            match step {
                Step::Name { name: s, index, desc } => {
                    let found = self.children(&objs, s, index, *desc)?;
                    if !found.is_empty() {
                        cur = V::Objs(found);
                        continue;
                    }
                    // A property of the (first) object.
                    let Some(o) = objs.first().cloned() else { return Ok(V::Null) };
                    match self.host.get(&o, s).map_err(Flow::Error)? {
                        Some(v) => cur = from_val(v),
                        None if k + 1 == n => return Ok(V::Null),
                        None => return err(format!("{s} is not a property of this object")),
                    }
                }
                Step::Call { name: m, args } => {
                    let mut vals = Vec::new();
                    for a in args {
                        let v = self.eval(a)?;
                        vals.push(to_val(&v));
                    }
                    let Some(o) = objs.first().cloned() else { return Ok(V::Null) };
                    let r = self.host.call(&o, m, &vals, self.this).map_err(Flow::Error)?;
                    cur = from_val(r);
                }
            }
        }
        Ok(cur)
    }

    fn assign(&mut self, steps: &[Step], v: V) -> R<()> {
        let val = to_val(&v);
        if steps.len() == 1 {
            let Some(Step::Name { name, index, .. }) = steps.first() else { return err("bad assignment") };
            if let Some(slot) = self.var_mut(&name.to_ascii_lowercase()) {
                *slot = v;
                return Ok(());
            }
            if let V::Objs(objs) = self.first(name, index)? {
                for o in objs {
                    self.host.set(&o, "rawValue", val.clone()).map_err(Flow::Error)?;
                }
            }
            return Ok(());
        }
        let (last, init) = match steps.split_last() {
            Some(x) => x,
            None => return err("bad assignment"),
        };
        let Step::Name { name: prop, index, desc } = last else { return err("a method call can't be assigned") };
        let base = self.access(init)?;
        let V::Objs(objs) = base else { return err("this can't be assigned") };
        let children = self.children(&objs, prop, index, *desc)?;
        if !children.is_empty() && children.iter().all(|c| matches!(c, Obj::Node(_))) {
            for c in children {
                self.host.set(&c, "rawValue", val.clone()).map_err(Flow::Error)?;
            }
            return Ok(());
        }
        for o in objs {
            self.host.set(&o, prop, val.clone()).map_err(Flow::Error)?;
        }
        Ok(())
    }

    /// All values of the arguments, objects expanded (for Sum, Avg, Count, Max, Min, Concat).
    fn values(&mut self, args: &[Expr]) -> R<Vec<V>> {
        let mut out = Vec::new();
        for a in args {
            match self.eval(a)? {
                V::Objs(l) => {
                    for o in l {
                        out.push(from_scalar(self.host.value_of(&o)));
                    }
                }
                v => out.push(v),
            }
        }
        Ok(out)
    }

    fn arg(&mut self, args: &[Expr], i: usize) -> R<V> {
        match args.get(i) {
            Some(a) => {
                let v = self.eval(a)?;
                Ok(self.scalar(v))
            }
            None => Ok(V::Null),
        }
    }

    fn call(&mut self, name: &str, args: &[Expr]) -> R<V> {
        if let Some(def) = self.funcs.iter().rev().find_map(|f| f.get(name)).cloned() {
            return self.user(&def, args);
        }
        let numbers = |e: &mut Self, vals: Vec<V>| -> Vec<f64> { vals.into_iter().filter(|v| *v != V::Null).map(|v| e.num(v)).collect() };
        Ok(match name {
            // Arithmetic.
            "abs" => self.num_fn(args, f64::abs)?,
            "ceil" => self.num_fn(args, f64::ceil)?,
            "floor" => self.num_fn(args, f64::floor)?,
            "round" => {
                let v = self.arg(args, 0)?;
                if v == V::Null {
                    return Ok(V::Null);
                }
                let x = self.num(v);
                let d = self.arg(args, 1)?;
                let digits = self.num(d).clamp(0.0, 12.0) as i32;
                let p = 10f64.powi(digits);
                // Round half away from zero, with a nudge for binary fractions (2.675 → 2.68).
                V::Num((x * p + x.signum() * 1e-9).round() / p)
            }
            "mod" => {
                let a = self.arg(args, 0)?;
                let b = self.arg(args, 1)?;
                let (a, b) = (self.num(a), self.num(b));
                if b == 0.0 {
                    return err("division by zero");
                }
                V::Num(a % b)
            }
            "sum" => {
                let v = self.values(args)?;
                let n = numbers(self, v);
                if n.is_empty() { V::Null } else { V::Num(n.iter().sum()) }
            }
            "avg" => {
                let v = self.values(args)?;
                let n = numbers(self, v);
                if n.is_empty() { V::Null } else { V::Num(n.iter().sum::<f64>() / n.len() as f64) }
            }
            "count" => {
                let v = self.values(args)?;
                V::Num(v.iter().filter(|x| **x != V::Null).count() as f64)
            }
            "max" | "min" => {
                let v = self.values(args)?;
                let n = numbers(self, v);
                let pick =
                    if name == "max" { n.iter().copied().fold(f64::NEG_INFINITY, f64::max) } else { n.iter().copied().fold(f64::INFINITY, f64::min) };
                if n.is_empty() { V::Null } else { V::Num(pick) }
            }
            // Logical.
            "choose" => {
                let i = self.arg(args, 0)?;
                let i = self.num(i);
                if i < 1.0 || !i.is_finite() {
                    return Ok(V::Str(String::new()));
                }
                let v = self.arg(args, i as usize)?;
                V::Str(self.text(v))
            }
            "exists" => {
                let found = match args.first() {
                    Some(Expr::Acc(s)) => matches!(self.access(s), Ok(V::Objs(l)) if !l.is_empty()),
                    Some(Expr::Str(s)) => !self.host.resolve(self.this, s).is_empty(),
                    _ => false,
                };
                V::Num(if found { 1.0 } else { 0.0 })
            }
            "hasvalue" => {
                let v = self.arg(args, 0)?;
                let has = match &v {
                    V::Null => false,
                    V::Str(s) => !s.trim().is_empty(),
                    _ => true,
                };
                V::Num(if has { 1.0 } else { 0.0 })
            }
            "oneof" => {
                let v = self.arg(args, 0)?;
                let mut hit = false;
                for i in 1..args.len() {
                    let o = self.arg(args, i)?;
                    if self.binary(BinOp::Eq, v.clone(), o)? == V::Num(1.0) {
                        hit = true;
                        break;
                    }
                }
                V::Num(if hit { 1.0 } else { 0.0 })
            }
            "within" => {
                let v = self.arg(args, 0)?;
                let lo = self.arg(args, 1)?;
                let hi = self.arg(args, 2)?;
                let a = self.binary(BinOp::Ge, v.clone(), lo)?;
                let b = self.binary(BinOp::Le, v, hi)?;
                V::Num(if a == V::Num(1.0) && b == V::Num(1.0) { 1.0 } else { 0.0 })
            }
            "ref" => match args.first() {
                Some(Expr::Acc(s)) => self.access(s)?,
                Some(other) => self.eval(other)?,
                None => V::Null,
            },
            "eval" => {
                let src = self.arg(args, 0)?;
                let src = self.text(src);
                let body = parse(&src).map_err(Flow::Error)?;
                self.block(&body)?
            }
            // Strings.
            "concat" => {
                let v = self.values(args)?;
                let mut s = String::new();
                for x in v {
                    s.push_str(&self.text(x));
                    if s.len() > MAX_STRING {
                        return err("the text is too long");
                    }
                }
                V::Str(s)
            }
            "len" => {
                let v = self.arg(args, 0)?;
                V::Num(self.text(v).chars().count() as f64)
            }
            "left" | "right" => {
                let v = self.arg(args, 0)?;
                let s: Vec<char> = self.text(v).chars().collect();
                let k = self.arg(args, 1)?;
                let k = (self.num(k).max(0.0) as usize).min(s.len());
                let part: String = if name == "left" { s.iter().take(k).collect() } else { s.iter().skip(s.len() - k).collect() };
                V::Str(part)
            }
            "substr" => {
                let v = self.arg(args, 0)?;
                let s: Vec<char> = self.text(v).chars().collect();
                let a = self.arg(args, 1)?;
                let b = self.arg(args, 2)?;
                let start = (self.num(a).max(1.0) as usize).saturating_sub(1);
                let count = self.num(b).max(0.0) as usize;
                V::Str(s.iter().skip(start).take(count).collect())
            }
            "lower" | "upper" | "ltrim" | "rtrim" => {
                let v = self.arg(args, 0)?;
                let s = self.text(v);
                V::Str(match name {
                    "lower" => s.to_lowercase(),
                    "upper" => s.to_uppercase(),
                    "ltrim" => s.trim_start().to_string(),
                    _ => s.trim_end().to_string(),
                })
            }
            "at" => {
                let a = self.arg(args, 0)?;
                let b = self.arg(args, 1)?;
                let (a, b) = (self.text(a), self.text(b));
                if b.is_empty() {
                    return Ok(V::Num(1.0));
                }
                V::Num(a.find(&b).map(|byte| a.get(..byte).map(|p| p.chars().count() + 1).unwrap_or(0)).unwrap_or(0) as f64)
            }
            "replace" => {
                let s = self.arg(args, 0)?;
                let f = self.arg(args, 1)?;
                let r = self.arg(args, 2)?;
                let (s, f, r) = (self.text(s), self.text(f), self.text(r));
                if f.is_empty() {
                    return Ok(V::Str(s));
                }
                let out = s.replace(&f, &r);
                if out.len() > MAX_STRING {
                    return err("the text is too long");
                }
                V::Str(out)
            }
            "space" => {
                let n = self.arg(args, 0)?;
                let n = (self.num(n).max(0.0) as usize).min(MAX_STRING);
                V::Str(" ".repeat(n))
            }
            "stuff" => {
                let s = self.arg(args, 0)?;
                let s: Vec<char> = self.text(s).chars().collect();
                let a = self.arg(args, 1)?;
                let b = self.arg(args, 2)?;
                let ins = self.arg(args, 3)?;
                let start = (self.num(a).max(1.0) as usize - 1).min(s.len());
                let del = (self.num(b).max(0.0) as usize).min(s.len() - start);
                let ins = self.text(ins);
                let mut out: String = s.iter().take(start).collect();
                out.push_str(&ins);
                out.extend(s.iter().skip(start + del));
                V::Str(out)
            }
            "str" => {
                let v = self.arg(args, 0)?;
                if v == V::Null {
                    return Ok(V::Null);
                }
                let x = self.num(v);
                let w = self.arg(args, 1)?;
                let p = self.arg(args, 2)?;
                let width = if args.len() > 1 { self.num(w).clamp(0.0, 1000.0) as usize } else { 10 };
                let prec = self.num(p).clamp(0.0, 15.0) as usize;
                let s = format!("{x:.prec$}");
                V::Str(if s.chars().count() > width { "*".repeat(width) } else { format!("{s:>width$}") })
            }
            "wordnum" => {
                let v = self.arg(args, 0)?;
                let x = self.num(v);
                let f = self.arg(args, 1)?;
                let f = self.num(f) as i64;
                V::Str(word_num(x, f))
            }
            "uuid" => V::Str("00000000-0000-0000-0000-000000000000".into()),
            "format" => {
                let p = self.arg(args, 0)?;
                let v = self.arg(args, 1)?;
                let (p, v) = (self.text(p), self.text(v));
                let pat = picture::date_pattern(&p).unwrap_or_else(|| p.clone());
                V::Str(picture::format(&v, &pat).unwrap_or(v))
            }
            "parse" => {
                let p = self.arg(args, 0)?;
                let v = self.arg(args, 1)?;
                let (p, v) = (self.text(p), self.text(v));
                let pat = picture::date_pattern(&p).unwrap_or_else(|| p.clone());
                V::Str(picture::parse(&v, &pat).unwrap_or(v))
            }
            "encode" | "decode" => {
                let v = self.arg(args, 0)?;
                V::Str(self.text(v))
            }
            // Dates.
            "date" => {
                let (y, m, d) = self.host.today;
                V::Num(days(y, m, d) as f64)
            }
            "isodate2num" => {
                let v = self.arg(args, 0)?;
                let s = self.text(v);
                match picture::parse_canonical(&s) {
                    Some((y, m, d)) => V::Num(days(y, m, d) as f64),
                    None => V::Num(0.0),
                }
            }
            "date2num" => {
                let v = self.arg(args, 0)?;
                let p = self.arg(args, 1)?;
                let (s, p) = (self.text(v), self.text(p));
                let pat = if p.is_empty() { "YYYY-MM-DD".to_string() } else { picture::date_pattern(&p).unwrap_or(p) };
                match picture::parse(&s, &pat).and_then(|c| picture::parse_canonical(&c)) {
                    Some((y, m, d)) => V::Num(days(y, m, d) as f64),
                    None => V::Num(0.0),
                }
            }
            "num2date" => {
                let v = self.arg(args, 0)?;
                let p = self.arg(args, 1)?;
                let n = self.num(v);
                let p = self.text(p);
                if n < 1.0 || !n.is_finite() {
                    return Ok(V::Str(String::new()));
                }
                let (y, m, d) = civil(n as i64);
                let canonical = format!("{y:04}-{m:02}-{d:02}");
                let pat = if p.is_empty() { "MMM D, YYYY".to_string() } else { picture::date_pattern(&p).unwrap_or(p) };
                V::Str(picture::format(&canonical, &pat).unwrap_or(canonical))
            }
            "datefmt" | "localdatefmt" => {
                let v = self.arg(args, 0)?;
                let style = if args.is_empty() { 0 } else { self.num(v) as i64 };
                V::Str(
                    match style {
                        1 => "M/D/YY",
                        3 => "MMMM D, YYYY",
                        4 => "EEEE, MMMM D, YYYY",
                        _ => "MMM D, YYYY",
                    }
                    .into(),
                )
            }
            "time" => V::Num(0.0),
            "time2num" | "isotime2num" => V::Num(0.0),
            "num2time" | "num2gmtime" | "timefmt" | "localtimefmt" => V::Str(String::new()),
            "unitvalue" => {
                let v = self.arg(args, 0)?;
                let s = self.text(v);
                V::Num(crate::template::measure(&s, "in").map(|pt| pt / 72.0).unwrap_or(0.0))
            }
            "unittype" => V::Str("in".into()),
            "get" | "post" | "put" => return err("network access is not available"),
            "apr" | "cterm" | "fv" | "ipmt" | "npv" | "pmt" | "ppmt" | "pv" | "rate" | "term" => {
                return err(format!("the financial function {name} is not supported"));
            }
            _ => return err(format!("{name} is not a known function")),
        })
    }

    fn num_fn(&mut self, args: &[Expr], f: fn(f64) -> f64) -> R<V> {
        let v = self.arg(args, 0)?;
        if v == V::Null {
            return Ok(V::Null);
        }
        let x = self.num(v);
        Ok(V::Num(f(x)))
    }

    fn user(&mut self, def: &Rc<(Vec<String>, Vec<Expr>)>, args: &[Expr]) -> R<V> {
        if self.calls >= MAX_CALL_DEPTH {
            return err("functions call each other too deeply");
        }
        let mut frame = HashMap::new();
        for (i, p) in def.0.iter().enumerate() {
            let v = self.arg(args, i)?;
            frame.insert(p.to_ascii_lowercase(), v);
        }
        self.calls += 1;
        self.scopes.push(frame);
        let r = self.block(&def.1);
        self.scopes.pop();
        self.calls -= 1;
        match r {
            Ok(v) => Ok(v),
            Err(Flow::Return(v)) => Ok(v),
            Err(f) => Err(f),
        }
    }
}

fn from_scalar(v: Val) -> V {
    match v {
        Val::Obj(_) => V::Null,
        other => from_val(other),
    }
}

/// Days since the FormCalc epoch (1900-01-01 is day 1).
pub fn days(y: i32, m: u32, d: u32) -> i64 {
    days_from_civil(y as i64, m as i64, d as i64) - days_from_civil(1900, 1, 1) + 1
}

/// The date of a FormCalc day number.
pub fn civil(n: i64) -> (i64, u32, u32) {
    let z = n - 1 + days_from_civil(1900, 1, 1) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days since 1970-01-01 (proleptic Gregorian), H. Hinnant's algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `WordNum`: 123.45 → "One Hundred Twenty-three" (f=1 adds "Dollars", f=2 adds cents).
fn word_num(x: f64, f: i64) -> String {
    if !x.is_finite() || !(0.0..1e15).contains(&x) {
        return "*".repeat(10);
    }
    const ONES: [&str; 20] = [
        "Zero",
        "One",
        "Two",
        "Three",
        "Four",
        "Five",
        "Six",
        "Seven",
        "Eight",
        "Nine",
        "Ten",
        "Eleven",
        "Twelve",
        "Thirteen",
        "Fourteen",
        "Fifteen",
        "Sixteen",
        "Seventeen",
        "Eighteen",
        "Nineteen",
    ];
    const TENS: [&str; 10] = ["", "", "Twenty", "Thirty", "Forty", "Fifty", "Sixty", "Seventy", "Eighty", "Ninety"];
    fn below_thousand(n: u64, out: &mut Vec<String>) {
        let h = n / 100;
        let r = n % 100;
        if h > 0 {
            out.push(format!("{} Hundred", ONES.get(h as usize).unwrap_or(&"")));
        }
        if r > 0 {
            if r < 20 {
                out.push(ONES.get(r as usize).unwrap_or(&"").to_string());
            } else if r.is_multiple_of(10) {
                out.push(TENS.get((r / 10) as usize).unwrap_or(&"").to_string());
            } else {
                out.push(format!("{}-{}", TENS.get((r / 10) as usize).unwrap_or(&""), ONES.get((r % 10) as usize).unwrap_or(&"").to_lowercase()));
            }
        }
    }
    fn words(n: u64) -> String {
        if n == 0 {
            return "Zero".into();
        }
        let scales = [(1_000_000_000_000u64, "Trillion"), (1_000_000_000, "Billion"), (1_000_000, "Million"), (1_000, "Thousand")];
        let mut out = Vec::new();
        let mut rest = n;
        for (s, name) in scales {
            if rest >= s {
                below_thousand(rest / s, &mut out);
                out.push(name.to_string());
                rest %= s;
            }
        }
        below_thousand(rest, &mut out);
        out.join(" ")
    }
    let whole = x.trunc() as u64;
    let cents = ((x - x.trunc()) * 100.0).round() as u64;
    match f {
        1 => format!("{} Dollars", words(whole)),
        2 => format!("{} Dollars And {} Cents", words(whole), words(cents)),
        _ => words(whole),
    }
}

/// Run a FormCalc script on `this`; returns the value of its last expression.
pub fn run(host: &mut Host, this: N, src: &str) -> Result<Val, String> {
    let body = parse(src)?;
    let mut e = Eval { host, this, scopes: vec![HashMap::new()], funcs: vec![HashMap::new()], steps: 0, calls: 0 };
    let r = e.block(&body);
    match r {
        Ok(v) | Err(Flow::Return(v)) => Ok(match v {
            V::Objs(l) => match l.first() {
                Some(o) => {
                    let v = e.host.value_of(o);
                    if matches!(v, Val::Obj(_)) { Val::Null } else { v }
                }
                None => Val::Null,
            },
            other => to_val(&other),
        }),
        Err(Flow::Break) | Err(Flow::Continue) => Ok(Val::Null),
        Err(Flow::Error(m)) => Err(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        assert_eq!(days(1900, 1, 1), 1);
        assert_eq!(civil(days(2026, 10, 5)), (2026, 10, 5));
        assert_eq!(civil(days(2000, 2, 29)), (2000, 2, 29));
    }

    #[test]
    fn words() {
        assert_eq!(word_num(123.0, 0), "One Hundred Twenty-three");
        assert_eq!(word_num(1_005.25, 2), "One Thousand Five Dollars And Twenty-five Cents");
    }

    #[test]
    fn parses_control_flow() {
        let src =
            "var t = 0\nfor i = 1 upto 3 do t = t + i endfor\nif (t == 6) then \"six\" elseif (t > 6) then \"more\" else \"less\" endif ; comment";
        assert!(parse(src).is_ok());
        assert!(parse("if (a then").is_err());
        let deep = format!("{}1{}", "(".repeat(5_000), ")".repeat(5_000));
        assert!(parse(&deep).is_err());
    }
}
