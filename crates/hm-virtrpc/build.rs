//! Generates Rust types, XDR codecs and typed client methods from libvirt's
//! `remote_protocol.x`.
//!
//! The parser covers the subset of RFC 4506 XDR that libvirt uses: consts,
//! typedefs, structs, enums, one discriminated union, fixed and variable
//! arrays, opaque data, strings and optional data (`*`). Codecs are emitted as
//! explicit per-field code, so `char`/`short` (4 bytes each on the wire,
//! like every XDR integer) never collide with opaque bytes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::path::PathBuf;

/// Constants the protocol file takes from libvirt's C headers.
const EXTERNAL_CONSTS: &[(&str, i64)] = &[
    ("VIR_UUID_BUFLEN", 16),
    ("VIR_SECURITY_MODEL_BUFLEN", 257),
    ("VIR_SECURITY_LABEL_BUFLEN", 4097),
    ("VIR_SECURITY_DOI_BUFLEN", 257),
    ("VIR_TYPED_PARAM_INT", 1),
    ("VIR_TYPED_PARAM_UINT", 2),
    ("VIR_TYPED_PARAM_LLONG", 3),
    ("VIR_TYPED_PARAM_ULLONG", 4),
    ("VIR_TYPED_PARAM_DOUBLE", 5),
    ("VIR_TYPED_PARAM_BOOLEAN", 6),
    ("VIR_TYPED_PARAM_STRING", 7),
];

fn main() {
    let path = "proto/remote_protocol.x";
    println!("cargo:rerun-if-changed={path}");
    let src = std::fs::read_to_string(path).expect("reading protocol file");
    let defs = Parser::new(tokenize(&src)).parse();
    let streams = stream_procedures(&src);
    let out = generate(&defs, &streams);
    let dest = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("remote_protocol.rs");
    std::fs::write(dest, out).expect("writing generated code");
}

// ---------------------------------------------------------------- lexing

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Num(i64),
    Punct(char),
}

fn tokenize(src: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let b = src.as_bytes();
    let mut i = 0;
    let mut line_start = true;
    while i < b.len() {
        let c = b[i] as char;
        if c == '\n' {
            line_start = true;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // `%` lines pass C code through rpcgen; irrelevant here.
        if line_start && c == '%' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        line_start = false;
        if src[i..].starts_with("/*") {
            i += src[i..].find("*/").expect("unterminated comment") + 2;
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            toks.push(Tok::Ident(src[start..i].to_owned()));
            continue;
        }
        if c.is_ascii_digit() || (c == '-' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let start = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            let text = &src[start..i];
            let (neg, digits) = text.strip_prefix('-').map_or((false, text), |t| (true, t));
            let v = if let Some(hex) = digits.strip_prefix("0x") {
                i64::from_str_radix(hex, 16).unwrap()
            } else {
                digits
                    .parse::<i64>()
                    .unwrap_or_else(|_| panic!("bad number {text}"))
            };
            toks.push(Tok::Num(if neg { -v } else { v }));
            continue;
        }
        toks.push(Tok::Punct(c));
        i += 1;
    }
    toks
}

// --------------------------------------------------------------- parsing

#[derive(Clone, Debug)]
enum Ty {
    Int,
    UInt,
    Hyper,
    UHyper,
    Bool,
    Float,
    Double,
    Char,
    UChar,
    Short,
    UShort,
    String(Option<Size>),
    OpaqueFixed(Size),
    OpaqueVar(Option<Size>),
    Named(String),
    Optional(Box<Ty>),
    ArrayFixed(Box<Ty>, Size),
    ArrayVar(Box<Ty>, Option<Size>),
}

/// Array bounds: a literal or a constant name.
#[derive(Clone, Debug)]
enum Size {
    Lit(i64),
    Const(String),
}

#[derive(Clone, Debug)]
struct Field {
    name: String,
    ty: Ty,
}

#[derive(Debug)]
enum Def {
    Const(String, Size),
    Typedef(Field),
    Struct(String, Vec<Field>),
    Enum(String, Vec<(String, Size)>),
    Union {
        name: String,
        disc: Field,
        arms: Vec<(Size, Option<Field>)>,
    },
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn new(toks: Vec<Tok>) -> Self {
        Self { toks, pos: 0 }
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Tok {
        let t = self
            .toks
            .get(self.pos)
            .cloned()
            .expect("unexpected end of input");
        self.pos += 1;
        t
    }

    fn ident(&mut self) -> String {
        match self.next() {
            Tok::Ident(s) => s,
            t => panic!("expected identifier, got {t:?} at token {}", self.pos),
        }
    }

    fn punct(&mut self, c: char) {
        match self.next() {
            Tok::Punct(p) if p == c => {}
            t => panic!("expected {c:?}, got {t:?} at token {}", self.pos),
        }
    }

    fn is_punct(&self, c: char) -> bool {
        matches!(self.peek(), Some(Tok::Punct(p)) if *p == c)
    }

    fn eat_punct(&mut self, c: char) -> bool {
        if self.is_punct(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn size(&mut self) -> Size {
        match self.next() {
            Tok::Num(n) => Size::Lit(n),
            Tok::Ident(s) => Size::Const(s),
            t => panic!("expected size, got {t:?}"),
        }
    }

    fn parse(mut self) -> Vec<Def> {
        let mut defs = Vec::new();
        while let Some(t) = self.peek().cloned() {
            let Tok::Ident(kw) = t else {
                panic!("unexpected {t:?} at top level")
            };
            self.pos += 1;
            match kw.as_str() {
                "const" => {
                    let name = self.ident();
                    self.punct('=');
                    let v = self.size();
                    self.punct(';');
                    defs.push(Def::Const(name, v));
                }
                "typedef" => {
                    let f = self.declaration().expect("typedef of void");
                    self.punct(';');
                    defs.push(Def::Typedef(f));
                }
                "struct" => {
                    let name = self.ident();
                    self.punct('{');
                    let mut fields = Vec::new();
                    while !self.eat_punct('}') {
                        fields.push(self.declaration().expect("void struct member"));
                        self.punct(';');
                    }
                    self.punct(';');
                    defs.push(Def::Struct(name, fields));
                }
                "enum" => {
                    let name = self.ident();
                    self.punct('{');
                    let mut members = Vec::new();
                    loop {
                        let m = self.ident();
                        self.punct('=');
                        members.push((m, self.size()));
                        if self.eat_punct('}') {
                            break;
                        }
                        self.punct(',');
                        if self.eat_punct('}') {
                            break;
                        }
                    }
                    self.punct(';');
                    defs.push(Def::Enum(name, members));
                }
                "union" => {
                    let name = self.ident();
                    assert_eq!(self.ident(), "switch");
                    self.punct('(');
                    let disc = self.declaration().expect("union discriminant");
                    self.punct(')');
                    self.punct('{');
                    let mut arms = Vec::new();
                    while !self.eat_punct('}') {
                        let kw = self.ident();
                        assert_eq!(kw, "case", "only case arms are supported");
                        let v = self.size();
                        self.punct(':');
                        let f = self.declaration();
                        self.punct(';');
                        arms.push((v, f));
                    }
                    self.punct(';');
                    defs.push(Def::Union { name, disc, arms });
                }
                other => panic!("unexpected top-level keyword {other}"),
            }
        }
        defs
    }

    /// One declaration; `None` for `void`.
    fn declaration(&mut self) -> Option<Field> {
        let first = self.ident();
        if first == "void" {
            return None;
        }
        if first == "opaque" || first == "string" {
            let name = self.ident();
            let ty = if self.eat_punct('[') {
                assert_eq!(first, "opaque", "fixed-size strings do not exist");
                let s = self.size();
                self.punct(']');
                Ty::OpaqueFixed(s)
            } else {
                self.punct('<');
                let s = if self.is_punct('>') {
                    None
                } else {
                    Some(self.size())
                };
                self.punct('>');
                if first == "opaque" {
                    Ty::OpaqueVar(s)
                } else {
                    Ty::String(s)
                }
            };
            return Some(Field { name, ty });
        }
        let base = match first.as_str() {
            "unsigned" => match self.peek() {
                Some(Tok::Ident(s)) if ["int", "hyper", "char", "short"].contains(&s.as_str()) => {
                    let s = self.ident();
                    match s.as_str() {
                        "int" => Ty::UInt,
                        "hyper" => Ty::UHyper,
                        "char" => Ty::UChar,
                        _ => Ty::UShort,
                    }
                }
                _ => Ty::UInt,
            },
            "int" => Ty::Int,
            "hyper" => Ty::Hyper,
            "bool" => Ty::Bool,
            "float" => Ty::Float,
            "double" => Ty::Double,
            "char" => Ty::Char,
            "short" => Ty::Short,
            _ => Ty::Named(first),
        };
        if self.eat_punct('*') {
            let name = self.ident();
            return Some(Field {
                name,
                ty: Ty::Optional(Box::new(base)),
            });
        }
        let name = self.ident();
        let ty = if self.eat_punct('[') {
            let s = self.size();
            self.punct(']');
            Ty::ArrayFixed(Box::new(base), s)
        } else if self.eat_punct('<') {
            let s = if self.is_punct('>') {
                None
            } else {
                Some(self.size())
            };
            self.punct('>');
            Ty::ArrayVar(Box::new(base), s)
        } else {
            base
        };
        Some(Field { name, ty })
    }
}

/// Procedures that carry a data stream, from their `@readstream` or
/// `@writestream` annotations. They need the stream API, not a plain call.
fn stream_procedures(src: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut annotated = false;
    for line in src.lines() {
        let l = line.trim();
        // Each procedure's annotations live in the comment block just above it.
        if l.starts_with("/**") {
            annotated = false;
        }
        if l.contains("@readstream") || l.contains("@writestream") {
            annotated = true;
        }
        if let Some(rest) = l.strip_prefix("REMOTE_PROC_")
            && let Some((name, _)) = rest.split_once(" = ")
        {
            if annotated {
                out.insert(name.to_owned());
            }
            annotated = false;
        }
    }
    out
}

// ------------------------------------------------------------ generation

const KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen",
];

fn field_name(n: &str) -> String {
    let snake = snake_case(n);
    if KEYWORDS.contains(&snake.as_str()) {
        format!("r#{snake}")
    } else {
        snake
    }
}

/// `callbackID` -> `callback_id`; already snake-case names pass through.
fn snake_case(n: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = n.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev_lower =
                i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            let prev_upper = i > 0 && chars[i - 1].is_ascii_uppercase();
            if i > 0 && (prev_lower || (prev_upper && next_lower)) && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `remote_nonnull_domain` -> `NonnullDomain`. Names that would shadow
/// standard types get a prefix (`remote_string` -> `RemoteString`).
fn type_name(n: &str) -> String {
    let base = n.strip_prefix("remote_").unwrap_or(n);
    let camel: String = base
        .split('_')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut c = s.chars();
            c.next()
                .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                .unwrap_or_default()
        })
        .collect();
    if ["String", "Option", "Vec", "Result", "Box"].contains(&camel.as_str()) {
        format!("Remote{camel}")
    } else {
        camel
    }
}

struct Gen<'a> {
    typedefs: HashMap<&'a str, &'a Ty>,
    enums: HashSet<&'a str>,
    consts: HashMap<String, i64>,
}

impl Gen<'_> {
    fn size(&self, s: &Size) -> i64 {
        match s {
            Size::Lit(n) => *n,
            Size::Const(c) => *self
                .consts
                .get(c)
                .unwrap_or_else(|| panic!("unknown constant {c}")),
        }
    }

    fn max(&self, s: &Option<Size>) -> String {
        s.as_ref().map_or("u32::MAX as usize".into(), |s| {
            format!("{}usize", self.size(s))
        })
    }

    fn rust_ty(&self, t: &Ty) -> String {
        match t {
            Ty::Int => "i32".into(),
            Ty::UInt => "u32".into(),
            Ty::Hyper => "i64".into(),
            Ty::UHyper => "u64".into(),
            Ty::Bool => "bool".into(),
            Ty::Float => "f32".into(),
            Ty::Double => "f64".into(),
            Ty::Char => "i8".into(),
            Ty::UChar => "u8".into(),
            Ty::Short => "i16".into(),
            Ty::UShort => "u16".into(),
            Ty::String(_) => "String".into(),
            Ty::OpaqueFixed(s) => format!("[u8; {}]", self.size(s)),
            Ty::OpaqueVar(_) => "Vec<u8>".into(),
            Ty::Named(n) if self.enums.contains(n.as_str()) => "i32".into(),
            Ty::Named(n) => type_name(n),
            Ty::Optional(inner) => format!("Option<{}>", self.rust_ty(inner)),
            Ty::ArrayFixed(inner, _) | Ty::ArrayVar(inner, _) => {
                format!("Vec<{}>", self.rust_ty(inner))
            }
        }
    }

    /// Statement encoding `v` (an expression of type `&T`) into `w`.
    fn enc(&self, t: &Ty, v: &str) -> String {
        let v = &format!("({v})");
        match t {
            Ty::Int | Ty::Char | Ty::Short => format!("xdr::put_i32(w, *{v} as i32);"),
            Ty::UInt | Ty::UChar | Ty::UShort => format!("xdr::put_u32(w, *{v} as u32);"),
            Ty::Hyper => format!("xdr::put_i64(w, *{v});"),
            Ty::UHyper => format!("xdr::put_u64(w, *{v});"),
            Ty::Bool => format!("xdr::put_bool(w, *{v});"),
            Ty::Float => format!("xdr::put_f32(w, *{v});"),
            Ty::Double => format!("xdr::put_f64(w, *{v});"),
            Ty::String(m) => format!("xdr::put_string(w, {v}, {})?;", self.max(m)),
            Ty::OpaqueFixed(_) => format!("xdr::put_opaque_fixed(w, &{v}[..]);"),
            Ty::OpaqueVar(m) => format!("xdr::put_opaque(w, {v}, {})?;", self.max(m)),
            Ty::Named(n) if self.enums.contains(n.as_str()) => format!("xdr::put_i32(w, *{v});"),
            Ty::Named(n) => match self.typedefs.get(n.as_str()) {
                Some(inner) => self.enc(inner, v),
                None => format!("{v}.encode(w)?;"),
            },
            Ty::Optional(inner) => format!(
                "match {v} {{ Some(x) => {{ xdr::put_bool(w, true); {} }} None => xdr::put_bool(w, false) }}",
                self.enc(inner, "x")
            ),
            Ty::ArrayFixed(inner, s) => format!(
                "{{ if {v}.len() != {n} {{ return Err(xdr::Error::Length({v}.len(), {n})); }} for x in {v}.iter() {{ {} }} }}",
                self.enc(inner, "x"),
                n = self.size(s)
            ),
            Ty::ArrayVar(inner, m) => format!(
                "{{ xdr::put_len(w, {v}.len(), {})?; for x in {v}.iter() {{ {} }} }}",
                self.max(m),
                self.enc(inner, "x")
            ),
        }
    }

    /// Expression decoding a value of type `t` from `r`.
    fn dec(&self, t: &Ty) -> String {
        match t {
            Ty::Int => "xdr::get_i32(r)?".into(),
            Ty::UInt => "xdr::get_u32(r)?".into(),
            Ty::Char => "xdr::get_i32(r)? as i8".into(),
            Ty::UChar => "xdr::get_u32(r)? as u8".into(),
            Ty::Short => "xdr::get_i32(r)? as i16".into(),
            Ty::UShort => "xdr::get_u32(r)? as u16".into(),
            Ty::Hyper => "xdr::get_i64(r)?".into(),
            Ty::UHyper => "xdr::get_u64(r)?".into(),
            Ty::Bool => "xdr::get_bool(r)?".into(),
            Ty::Float => "xdr::get_f32(r)?".into(),
            Ty::Double => "xdr::get_f64(r)?".into(),
            Ty::String(m) => format!("xdr::get_string(r, {})?", self.max(m)),
            Ty::OpaqueFixed(s) => format!("xdr::get_opaque_fixed::<{}>(r)?", self.size(s)),
            Ty::OpaqueVar(m) => format!("xdr::get_opaque(r, {})?", self.max(m)),
            Ty::Named(n) if self.enums.contains(n.as_str()) => "xdr::get_i32(r)?".into(),
            Ty::Named(n) => match self.typedefs.get(n.as_str()) {
                Some(inner) => self.dec(inner),
                None => format!("{}::decode(r)?", type_name(n)),
            },
            Ty::Optional(inner) => format!(
                "if xdr::get_bool(r)? {{ Some({}) }} else {{ None }}",
                self.dec(inner)
            ),
            Ty::ArrayFixed(inner, s) => {
                format!(
                    "{{ let mut v = Vec::with_capacity({n}); for _ in 0..{n} {{ v.push({}); }} v }}",
                    self.dec(inner),
                    n = self.size(s)
                )
            }
            Ty::ArrayVar(inner, m) => format!(
                "{{ let n = xdr::get_len(r, {})?; let mut v = Vec::with_capacity(n.min(1024)); for _ in 0..n {{ v.push({}); }} v }}",
                self.max(m),
                self.dec(inner)
            ),
        }
    }
}

fn generate(defs: &[Def], streams: &HashSet<String>) -> String {
    let mut consts: HashMap<String, i64> = EXTERNAL_CONSTS
        .iter()
        .map(|(k, v)| ((*k).to_owned(), *v))
        .collect();
    // Constants may refer to earlier constants.
    for d in defs {
        match d {
            Def::Const(n, Size::Lit(v)) => {
                consts.insert(n.clone(), *v);
            }
            Def::Const(n, Size::Const(c)) => {
                let v = *consts
                    .get(c)
                    .unwrap_or_else(|| panic!("unknown constant {c}"));
                consts.insert(n.clone(), v);
            }
            Def::Enum(_, members) => {
                for (m, s) in members {
                    let v = match s {
                        Size::Lit(v) => *v,
                        Size::Const(c) => consts[c],
                    };
                    consts.insert(m.clone(), v);
                }
            }
            _ => {}
        }
    }
    let g = Gen {
        typedefs: defs
            .iter()
            .filter_map(|d| match d {
                Def::Typedef(f) => Some((f.name.as_str(), &f.ty)),
                _ => None,
            })
            .collect(),
        enums: defs
            .iter()
            .filter_map(|d| match d {
                Def::Enum(n, _) => Some(n.as_str()),
                _ => None,
            })
            .collect(),
        consts,
    };

    let mut o =
        String::from("// @generated by build.rs from proto/remote_protocol.x; do not edit.\n\n");
    let mut structs = HashSet::new();

    for d in defs {
        match d {
            Def::Const(n, s) => {
                let _ = writeln!(o, "pub const {n}: i64 = {};", g.size(s));
            }
            Def::Typedef(f) => {
                let _ = writeln!(o, "pub type {} = {};", type_name(&f.name), g.rust_ty(&f.ty));
            }
            Def::Enum(name, members) => {
                let _ = writeln!(o, "/// Values of `{name}`.");
                let _ = writeln!(o, "pub mod {} {{", name.to_lowercase());
                for (m, s) in members {
                    let v = match s {
                        Size::Lit(v) => *v,
                        Size::Const(c) => g.consts[c],
                    };
                    let _ = writeln!(o, "    pub const {m}: i32 = {v};");
                }
                o.push_str("}\n");
            }
            Def::Struct(name, fields) => {
                structs.insert(name.clone());
                let tn = type_name(name);
                let _ = writeln!(
                    o,
                    "#[derive(Clone, Debug, Default, PartialEq)]\npub struct {tn} {{"
                );
                for f in fields {
                    let _ = writeln!(o, "    pub {}: {},", field_name(&f.name), g.rust_ty(&f.ty));
                }
                o.push_str("}\n");
                let _ = writeln!(o, "impl Xdr for {tn} {{");
                o.push_str("    fn encode(&self, w: &mut Vec<u8>) -> xdr::Result<()> {\n");
                for f in fields {
                    let _ = writeln!(
                        o,
                        "        {}",
                        g.enc(&f.ty, &format!("&self.{}", field_name(&f.name)))
                    );
                }
                o.push_str("        Ok(())\n    }\n");
                o.push_str("    fn decode(r: &mut xdr::Reader<'_>) -> xdr::Result<Self> {\n        Ok(Self {\n");
                for f in fields {
                    let _ = writeln!(o, "            {}: {},", field_name(&f.name), g.dec(&f.ty));
                }
                o.push_str("        })\n    }\n}\n");
            }
            Def::Union { name, disc, arms } => {
                let tn = type_name(name);
                let _ = writeln!(o, "#[derive(Clone, Debug, PartialEq)]\npub enum {tn} {{");
                for (case, f) in arms {
                    let variant = type_name(&match case {
                        Size::Const(c) => c.to_lowercase(),
                        Size::Lit(v) => format!("v{v}"),
                    });
                    match f {
                        Some(f) => {
                            let _ = writeln!(o, "    {variant}({}),", g.rust_ty(&f.ty));
                        }
                        None => {
                            let _ = writeln!(o, "    {variant},");
                        }
                    }
                }
                o.push_str("}\n");
                let first = arms.iter().find_map(|(case, f)| {
                    let variant = type_name(&match case {
                        Size::Const(c) => c.to_lowercase(),
                        Size::Lit(v) => format!("v{v}"),
                    });
                    f.as_ref()
                        .map(|_| format!("Self::{variant}(Default::default())"))
                });
                let _ = writeln!(
                    o,
                    "impl Default for {tn} {{ fn default() -> Self {{ {} }} }}",
                    first.expect("union with a valued arm")
                );
                let _ = writeln!(o, "impl Xdr for {tn} {{");
                o.push_str("    fn encode(&self, w: &mut Vec<u8>) -> xdr::Result<()> {\n        match self {\n");
                for (case, f) in arms {
                    let variant = type_name(&match case {
                        Size::Const(c) => c.to_lowercase(),
                        Size::Lit(v) => format!("v{v}"),
                    });
                    let n = g.size(case);
                    match f {
                        Some(f) => {
                            let _ = writeln!(
                                o,
                                "            Self::{variant}(x) => {{ xdr::put_i32(w, {n}); {} }}",
                                g.enc(&f.ty, "x")
                            );
                        }
                        None => {
                            let _ =
                                writeln!(o, "            Self::{variant} => xdr::put_i32(w, {n}),");
                        }
                    }
                }
                o.push_str("        }\n        Ok(())\n    }\n");
                assert!(
                    matches!(disc.ty, Ty::Int),
                    "only int discriminants are supported"
                );
                o.push_str("    fn decode(r: &mut xdr::Reader<'_>) -> xdr::Result<Self> {\n");
                o.push_str("        Ok(match xdr::get_i32(r)? {\n");
                for (case, f) in arms {
                    let variant = type_name(&match case {
                        Size::Const(c) => c.to_lowercase(),
                        Size::Lit(v) => format!("v{v}"),
                    });
                    let n = g.size(case);
                    match f {
                        Some(f) => {
                            let _ = writeln!(
                                o,
                                "            {n} => Self::{variant}({}),",
                                g.dec(&f.ty)
                            );
                        }
                        None => {
                            let _ = writeln!(o, "            {n} => Self::{variant},");
                        }
                    }
                }
                o.push_str("            d => return Err(xdr::Error::Discriminant(d)),\n        })\n    }\n}\n");
            }
        }
    }

    // Procedures: numbers, and typed client methods for plain calls.
    let procs: BTreeMap<i64, String> = defs
        .iter()
        .find_map(|d| match d {
            Def::Enum(n, m) if n == "remote_procedure" => Some(m),
            _ => None,
        })
        .expect("remote_procedure enum")
        .iter()
        .map(|(m, s)| {
            (
                g.size(s),
                m.strip_prefix("REMOTE_PROC_")
                    .expect("REMOTE_PROC_ prefix")
                    .to_owned(),
            )
        })
        .collect();

    o.push_str("\n/// Procedure numbers of the remote program.\npub mod proc {\n");
    for (n, name) in &procs {
        let _ = writeln!(o, "    pub const {name}: i32 = {n};");
    }
    o.push_str("}\n\nimpl crate::Client {\n");
    for (n, name) in &procs {
        if streams.contains(name) {
            continue;
        }
        let lower = name.to_lowercase();
        let args = format!("remote_{lower}_args");
        let ret = format!("remote_{lower}_ret");
        let has_args = structs.contains(&args);
        let has_ret = structs.contains(&ret);
        let param = if has_args {
            format!(", args: &{}", type_name(&args))
        } else {
            String::new()
        };
        let ret_ty = if has_ret {
            type_name(&ret)
        } else {
            "()".into()
        };
        let call_args = if has_args { "args" } else { "&()" };
        let _ = writeln!(
            o,
            "    /// `REMOTE_PROC_{name}` ({n}).\n    pub async fn {lower}(&self{param}) -> crate::Result<{ret_ty}> {{ self.call(proc::{name}, {call_args}).await }}"
        );
    }
    o.push_str("}\n");
    o
}
