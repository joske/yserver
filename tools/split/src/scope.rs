//! Resolution guards over the in-tree model: visibility scopes, textual
//! `macro_rules!` scope, what a leaf's free names bind to, traits in scope.
//!
//! Names: every module's bindings are its top-level leaves, child modules and
//! `use` names, a named import standing for the items its path reaches
//! through in-tree modules (its absolute spelling when it reaches none);
//! `use x::*` of an in-tree module is
//! followed (visibility and local-over-glob shadowing applied), any other
//! glob is an opaque marker that only counts when nothing in the tree binds
//! the name. A leaf's names are the first segments of its paths (impl head
//! included), classified by shape (`a::` type, `A` type or value, `a`
//! value); fields, methods, macro names, `$` metavariables and names bound
//! after a keyword or in a `use … as` are skipped. Every path of two or
//! more segments (`crate::`/`self::`/`super::` included) is also walked
//! through in-tree modules to the item it names.
//!
//! Checked per moved leaf and per delegated helper, before against after:
//! those names and paths, by the identity (mapped through the path table) of
//! the item they reach; the textual macro chain of each invocation down to
//! the first definition active wherever the caller is (ones under a cfg the
//! caller lacks or any `cfg_attr`, own or a wrapper's, listed), else the
//! macro's path binding; the free names in every arm of the reached
//! definitions, resolved at the caller; the module each `self::`/`super::`
//! path starts from; the traits in scope (named, `_` and glob imports,
//! imports of unknown kind and opaque globs as tokens); for delegations,
//! that the self type resolves to one in-tree item.
//!
//! Normalized before comparing tokens: the arguments of the
//! `STD_COMMA_MACROS` that are positively std's at every invocation in the
//! leaf (no binding in the tree, no import of the name in the leaf, no
//! `macro_rules!` of the name and no `#[macro_use] extern crate` anywhere in
//! the package), when they parse as the macro's grammar: trailing
//! comma dropped, and the same layout normalization as code outside macros;
//! other macros' input and attributes stay exact. Closure and match arm
//! bodies `{ e }` compare as `e` (see `tree::Commas`).
//!
//! Refused: moving a leaf that invokes an in-tree `macro_rules!`, or defines
//! and invokes its own, to another module, unless the manifest lists it under
//! `exceptions` with a reason (the checks above still apply). Production
//! code (not under `#[cfg(test)]`) whose value depends on where it is,
//! unless listed under `locations` with a reason: `line!`/`column!`/`file!`,
//! `Location::caller()` and calls of in-tree `#[track_caller]` fns that
//! pass it on, at a new position; `module_path!` and log macros without
//! `target:` (default target `module_path!()`) in a new module. Test code
//! is counted, not refused.
//!
//! Not modelled: preludes, extern crates, local `let` and item shadowing,
//! names inside macro input, inherent-vs-trait method priority, macros named
//! by path (`a::m!`), `#[macro_export]`, cfg values.
//!
//! Outside the equivalence guarantee: diagnostic text std macros derive
//! from their input (the stringified condition of `assert!(… |x| { x } …)`
//! in a panic message differs once the input is normalized), and panic
//! locations (`unwrap`, `panic!`, `#[track_caller]` callers) of moved code.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::{TokenStream, TokenTree};

use crate::tree::{Commas, Ev, Leaf, Source, Tree};

/// Absolute reach of a visibility written in `module`: `pub`, `crate`, or
/// `in <module path>`.
pub fn scope(vis: &str, module: &str) -> String {
    let Ok(v) = syn::parse_str::<syn::Visibility>(vis) else {
        return vis.to_string();
    };
    let path = match v {
        syn::Visibility::Public(_) => return "pub".into(),
        syn::Visibility::Inherited => module.to_string(),
        syn::Visibility::Restricted(r) => {
            let mut cur: Vec<String> = Vec::new();
            for (i, seg) in r.path.segments.iter().enumerate() {
                let s = seg.ident.to_string();
                match (i, s.as_str()) {
                    (0, "crate") => {}
                    (0, "self") => cur = split(module),
                    (_, "super") => {
                        if i == 0 {
                            cur = split(module);
                        }
                        cur.pop();
                    }
                    _ => cur.push(s),
                }
            }
            cur.join("::")
        }
    };
    if path.is_empty() {
        "crate".into()
    } else {
        format!("in {path}")
    }
}

fn split(m: &str) -> Vec<String> {
    m.split("::")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn under(m: &str, p: &str) -> bool {
    p.is_empty() || m == p || m.starts_with(&format!("{p}::"))
}

/// Whether `module` can see an item of scope `s`.
pub fn admits(s: &str, module: &str) -> bool {
    s.strip_prefix("in ").is_none_or(|p| under(module, p))
}

/// `new` reaches no further than `old`.
pub fn within(new: &str, old: &str) -> bool {
    match (new, old) {
        (_, "pub") => true,
        ("pub", _) => false,
        (_, "crate") => true,
        ("crate", _) => false,
        (n, o) => under(&n[3..], &o[3..]),
    }
}

fn tts(tokens: &str) -> Vec<TokenTree> {
    tokens
        .parse::<TokenStream>()
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn is_punct(t: Option<&TokenTree>, c: char) -> bool {
    matches!(t, Some(TokenTree::Punct(p)) if p.as_char() == c)
}

/// Unqualified macro invocations (`m!(..)`), in token order.
pub fn invocations(tokens: &str) -> Vec<String> {
    fn walk(v: &[TokenTree], out: &mut Vec<String>) {
        for (i, t) in v.iter().enumerate() {
            match t {
                TokenTree::Group(g) => walk(&g.stream().into_iter().collect::<Vec<_>>(), out),
                TokenTree::Ident(id)
                    if id != "macro_rules"
                        && is_punct(v.get(i + 1), '!')
                        && matches!(v.get(i + 2), Some(TokenTree::Group(_)))
                        && !(i > 0 && is_punct(v.get(i - 1), ':')) =>
                {
                    out.push(id.to_string());
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&tts(tokens), &mut out);
    out
}

/// std macros whose arguments are comma-separated expressions (format
/// arguments included) or `matches!`'s, and whose optional trailing comma
/// expands the same.
pub const STD_COMMA_MACROS: [&str; 20] = [
    "vec",
    "format",
    "format_args",
    "print",
    "println",
    "eprint",
    "eprintln",
    "write",
    "writeln",
    "panic",
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "unreachable",
    "todo",
    "unimplemented",
    "matches",
];

/// The `STD_COMMA_MACROS` a leaf invokes that are positively std's at every
/// invocation: no binding in the tree (neither textually nor by path), not
/// in `crate_not_std`, and not named by an import in the leaf itself.
pub fn std_macros(tokens: &str, site: &Site, crate_not_std: &BTreeSet<String>) -> BTreeSet<String> {
    let Some(local) = use_bound(tokens) else {
        return BTreeSet::new();
    };
    let (mut ok, mut not) = (BTreeSet::new(), BTreeSet::new());
    for (n, t) in invocations(tokens).into_iter().zip(&site.text) {
        if STD_COMMA_MACROS.contains(&n.as_str())
            && *t == format!("{n} = path {{}}")
            && !crate_not_std.contains(&n)
            && !local.contains(&n)
        {
            ok.insert(n);
        } else {
            not.insert(n);
        }
    }
    ok.difference(&not).cloned().collect()
}

/// Every ident in the `use` items inside `tokens` (function- or block-local
/// imports); `None` when one is a glob.
fn use_bound(tokens: &str) -> Option<BTreeSet<String>> {
    fn idents(v: &[TokenTree], out: &mut BTreeSet<String>) -> bool {
        v.iter().all(|t| match t {
            TokenTree::Ident(id) => {
                out.insert(id.to_string());
                true
            }
            TokenTree::Group(g) => idents(&g.stream().into_iter().collect::<Vec<_>>(), out),
            TokenTree::Punct(p) => p.as_char() != '*',
            TokenTree::Literal(_) => true,
        })
    }
    fn walk(v: &[TokenTree], out: &mut BTreeSet<String>) -> bool {
        let mut i = 0;
        while i < v.len() {
            match &v[i] {
                TokenTree::Ident(id) if id == "use" => {
                    let end = (i..v.len())
                        .find(|&j| is_punct(v.get(j), ';'))
                        .unwrap_or(v.len());
                    if !idents(&v[i + 1..end], out) {
                        return false;
                    }
                    i = end;
                }
                TokenTree::Group(g) if !walk(&g.stream().into_iter().collect::<Vec<_>>(), out) => {
                    return false;
                }
                _ => {}
            }
            i += 1;
        }
        true
    }
    let mut out = BTreeSet::new();
    walk(&tts(tokens), &mut out).then_some(out)
}

/// Macro names that are not positively std's anywhere in the crate holding
/// `root`: every `STD_COMMA_MACROS` when a `#[macro_use] extern crate` (or a
/// file that does not lex) is in the package, and every `macro_rules!` name
/// defined in it. The package is the directory of the nearest Cargo.toml
/// (the whole source without one); every `.rs` under it is scanned.
pub fn crate_not_std(src: &dyn Source, root: &str) -> BTreeSet<String> {
    fn walk(v: &[TokenTree], out: &mut BTreeSet<String>, macro_use: &mut bool) {
        let mut pending = false;
        for (i, t) in v.iter().enumerate() {
            match t {
                TokenTree::Group(g) if is_punct(i.checked_sub(1).and_then(|j| v.get(j)), '#') => {
                    pending |= g.stream().to_string().contains("macro_use");
                    walk(&g.stream().into_iter().collect::<Vec<_>>(), out, macro_use);
                    continue;
                }
                TokenTree::Group(g) => {
                    walk(&g.stream().into_iter().collect::<Vec<_>>(), out, macro_use);
                    if matches!(i.checked_sub(1).and_then(|j| v.get(j)), Some(TokenTree::Ident(p)) if p == "pub")
                    {
                        continue;
                    }
                }
                TokenTree::Ident(id) if id == "extern" => {
                    let krate = matches!(v.get(i + 1), Some(TokenTree::Ident(c)) if c == "crate");
                    *macro_use |= krate && pending;
                }
                TokenTree::Ident(id) if id == "macro_rules" && is_punct(v.get(i + 1), '!') => {
                    if let Some(TokenTree::Ident(n)) = v.get(i + 2) {
                        out.insert(n.to_string());
                    }
                }
                TokenTree::Ident(id) if id == "pub" => continue,
                TokenTree::Punct(p) if p.as_char() == '#' || p.as_char() == '!' => continue,
                _ => {}
            }
            pending = false;
        }
    }
    let mut dir = crate::tree::dir_of(root);
    while src.read(&crate::tree::join(dir, "Cargo.toml")).is_none() && !dir.is_empty() {
        dir = crate::tree::dir_of(dir);
    }
    let mut out = BTreeSet::new();
    let mut macro_use = false;
    for f in src.list(dir).iter().filter(|f| f.ends_with(".rs")) {
        let text = String::from_utf8_lossy(&src.read(f).unwrap_or_default()).to_string();
        match text.parse::<TokenStream>() {
            Ok(ts) => walk(
                &ts.into_iter().collect::<Vec<_>>(),
                &mut out,
                &mut macro_use,
            ),
            Err(_) => macro_use = true,
        }
    }
    if macro_use {
        out.extend(STD_COMMA_MACROS.iter().map(|s| (*s).to_string()));
    }
    out
}

/// The arguments of std macro `name`, when they parse as its grammar,
/// without their trailing comma and layout-normalized (`tree::Commas`).
fn std_args(name: &str, ts: TokenStream) -> Option<TokenStream> {
    use quote::ToTokens;
    use syn::{
        Token,
        parse::{ParseStream, Parser},
        visit_mut::VisitMut,
    };
    if name == "matches" {
        let (mut e, mut pat, mut guard) = (|p: ParseStream| {
            let e = p.parse::<syn::Expr>()?;
            p.parse::<Token![,]>()?;
            let pat = syn::Pat::parse_multi_with_leading_vert(p)?;
            let guard = if p.peek(Token![if]) {
                p.parse::<Token![if]>()?;
                Some(p.parse::<syn::Expr>()?)
            } else {
                None
            };
            if !p.is_empty() {
                p.parse::<Token![,]>()?;
            }
            Ok((e, pat, guard))
        })
        .parse2(ts)
        .ok()?;
        Commas.visit_expr_mut(&mut e);
        Commas.visit_pat_mut(&mut pat);
        let mut out = quote::quote!(#e, #pat);
        if let Some(g) = guard.as_mut() {
            Commas.visit_expr_mut(g);
            out.extend(quote::quote!(if #g));
        }
        return Some(out);
    }
    let mut args = syn::punctuated::Punctuated::<syn::Expr, Token![,]>::parse_terminated
        .parse2(ts)
        .ok()?;
    args.pop_punct();
    args.iter_mut().for_each(|e| Commas.visit_expr_mut(e));
    Some(args.to_token_stream())
}

/// `tokens` with the arguments of invocations of the `std` macros
/// normalized, outside attributes and other macros' input.
pub fn std_commas(tokens: &str, std: &BTreeSet<String>) -> String {
    fn walk(ts: TokenStream, std: &BTreeSet<String>) -> TokenStream {
        let v: Vec<TokenTree> = ts.into_iter().collect();
        let at = |i: usize, k: usize| i.checked_sub(k).and_then(|j| v.get(j));
        let ident = |i: usize, k: usize| match at(i, k) {
            Some(TokenTree::Ident(id)) => Some(id.to_string()),
            _ => None,
        };
        v.iter()
            .enumerate()
            .map(|(i, t)| {
                let TokenTree::Group(g) = t else {
                    return t.clone();
                };
                let called = ident(i, 2).filter(|n| is_punct(at(i, 1), '!') && !is_keyword(n));
                let opaque = is_punct(at(i, 1), '#')
                    || (is_punct(at(i, 1), '!') && is_punct(at(i, 2), '#'))
                    || ident(i, 3).as_deref() == Some("macro_rules");
                let stream = match called {
                    Some(n) if std.contains(&n) && !is_punct(at(i, 3), ':') => {
                        std_args(&n, g.stream()).map_or_else(|| g.stream(), |s| walk(s, std))
                    }
                    Some(_) => g.stream(),
                    None if opaque => g.stream(),
                    None => walk(g.stream(), std),
                };
                let mut ng = proc_macro2::Group::new(g.delimiter(), stream);
                ng.set_span(g.span());
                TokenTree::Group(ng)
            })
            .collect()
    }
    walk(tokens.parse().unwrap_or_default(), std).to_string()
}

/// Every `name!(..)` in `tokens`, qualified or not: name and arguments.
fn calls(tokens: &str) -> Vec<(String, TokenStream)> {
    fn walk(v: &[TokenTree], out: &mut Vec<(String, TokenStream)>) {
        for (i, t) in v.iter().enumerate() {
            match (t, v.get(i + 2)) {
                (TokenTree::Group(g), _) => walk(&g.stream().into_iter().collect::<Vec<_>>(), out),
                (TokenTree::Ident(id), Some(TokenTree::Group(g)))
                    if is_punct(v.get(i + 1), '!') =>
                {
                    out.push((id.to_string(), g.stream()));
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&tts(tokens), &mut out);
    out
}

/// `line!`, `column!`, `file!` in `tokens`, or a source position taken
/// from the caller: `Location::caller()` outside a `#[track_caller]` fn, or
/// a call of one of `tracked` (fns that pass their caller's position on).
pub fn positional(tokens: &str, tracked: &BTreeSet<String>) -> bool {
    fn walk(v: &[TokenTree], tracked: &BTreeSet<String>, own: bool) -> bool {
        v.iter().enumerate().any(|(i, t)| match t {
            TokenTree::Group(g) => walk(&g.stream().into_iter().collect::<Vec<_>>(), tracked, own),
            TokenTree::Ident(id) => {
                let s = id.to_string();
                let next = v.get(i + 1);
                (matches!(s.as_str(), "line" | "column" | "file")
                    && is_punct(next, '!')
                    && matches!(v.get(i + 2), Some(TokenTree::Group(_))))
                    || (!own
                        && s == "Location"
                        && is_punct(next, ':')
                        && matches!(v.get(i + 3), Some(TokenTree::Ident(c)) if c == "caller"))
                    || (!own
                        && tracked.contains(&s)
                        && (matches!(next, Some(TokenTree::Group(g)) if g.delimiter() == proc_macro2::Delimiter::Parenthesis)
                            || is_punct(next, ':')))
            }
            _ => false,
        })
    }
    let v = tts(tokens);
    walk(&v, tracked, has_attr(&v, "track_caller"))
}

/// The leading outer attributes of an item's tokens include `#[name]`.
fn has_attr(v: &[TokenTree], name: &str) -> bool {
    v.chunks(2)
        .take_while(|c| is_punct(c.first(), '#'))
        .any(|c| matches!(c.get(1), Some(TokenTree::Group(g)) if g.stream().to_string() == name))
}

/// Names of `#[track_caller]` fns that take `Location::caller()` or call
/// another such fn: their callers' positions reach their result.
pub fn tracked(trees: &[&Tree]) -> BTreeSet<String> {
    let fns: Vec<(&str, &str)> = trees
        .iter()
        .flat_map(|t| &t.leaves)
        .filter(|l| l.kind == "fn" && has_attr(&tts(&l.tokens), "track_caller"))
        .map(|l| (l.name.as_str(), l.tokens.as_str()))
        .collect();
    let mut set = BTreeSet::new();
    loop {
        let before = set.len();
        for (name, tokens) in &fns {
            let as_caller = tokens.replace("# [track_caller]", "");
            if positional(&as_caller, &set) {
                set.insert((*name).to_string());
            }
        }
        if set.len() == before {
            return set;
        }
    }
}

/// Log macros (`log`'s, qualified or not, whatever they resolve to).
pub const LOG_MACROS: [&str; 7] = [
    "trace",
    "debug",
    "info",
    "warn",
    "error",
    "log",
    "log_enabled",
];

/// `module_path!` and log macro calls without an explicit `target:` in
/// `tokens` (their default target is `module_path!()`).
pub fn module_sensitive(tokens: &str) -> usize {
    calls(tokens)
        .iter()
        .filter(|(n, args)| {
            n == "module_path"
                || (LOG_MACROS.contains(&n.as_str()) && !args.to_string().starts_with("target :"))
        })
        .count()
}

/// Test-only code: under `#[cfg(test)]`, or a `#[test]`/`#[cfg(test)]` item.
pub fn is_test(l: &Leaf) -> bool {
    let v = tts(&l.tokens);
    l.ctx.cfg.contains("cfg (test)") || has_attr(&v, "test") || has_attr(&v, "cfg (test)")
}

/// `macro_rules!` definitions anywhere in `tokens`: name and transcribers.
pub fn macro_defs(tokens: &str) -> Vec<(String, Vec<String>)> {
    fn walk(v: &[TokenTree], out: &mut Vec<(String, Vec<String>)>) {
        for (i, t) in v.iter().enumerate() {
            match (t, v.get(i + 2), v.get(i + 3)) {
                (TokenTree::Ident(id), Some(TokenTree::Ident(name)), Some(TokenTree::Group(g)))
                    if id == "macro_rules" && is_punct(v.get(i + 1), '!') =>
                {
                    let arms: Vec<TokenTree> = g.stream().into_iter().collect();
                    let bodies = (0..arms.len())
                        .filter(|&j| is_punct(arms.get(j), '=') && is_punct(arms.get(j + 1), '>'))
                        .filter_map(|j| match arms.get(j + 2) {
                            Some(TokenTree::Group(b)) => Some(b.stream().to_string()),
                            _ => None,
                        })
                        .collect();
                    out.push((name.to_string(), bodies));
                }
                (TokenTree::Group(g), ..) => {
                    walk(&g.stream().into_iter().collect::<Vec<_>>(), out);
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&tts(tokens), &mut out);
    out
}

/// What a leaf's macro invocations reach.
#[derive(Default)]
pub struct Site {
    /// Per invocation: the definitions by textual scope, from the innermost
    /// down to the first one active whenever the caller is, or the path
    /// binding when no definition is in textual scope.
    pub text: Vec<String>,
    /// Definition leaves reached, directly or through other macros.
    pub defs: BTreeSet<usize>,
    /// The leaf defines and invokes its own `macro_rules!`.
    pub local: bool,
}

/// Whether a definition may be inactive where `caller` is compiled: a cfg
/// the caller does not share, or any `cfg_attr` on it or a wrapper (it may
/// expand to a cfg).
fn conditional(def: &Leaf, caller: &Leaf) -> bool {
    !def.ctx.cfg.is_subset(&caller.ctx.cfg)
        || def
            .ctx
            .levels
            .iter()
            .flatten()
            .any(|a| a.starts_with("cfg_attr"))
        || def
            .tokens
            .split("macro_rules")
            .next()
            .is_some_and(|attrs| attrs.contains("# [cfg"))
}

/// Per leaf: which `macro_rules!` each unqualified invocation reaches by
/// textual scope, with the macros those definitions invoke in turn. `id`
/// names a definition leaf.
pub fn macro_sites(t: &Tree, id: &dyn Fn(&Leaf) -> String, names: &Names) -> Vec<Site> {
    let mut out: Vec<Site> = t.leaves.iter().map(|_| Site::default()).collect();
    let mut visible: Vec<(String, usize)> = Vec::new();
    let mut stack: Vec<(usize, bool)> = Vec::new();
    for ev in &t.events {
        match ev {
            Ev::Enter(keep) => stack.push((visible.len(), *keep)),
            Ev::Exit => {
                if let Some((n, false)) = stack.pop() {
                    visible.truncate(n);
                }
            }
            Ev::Leaf(i) => {
                let l = &t.leaves[*i];
                if l.kind == "macro" {
                    visible.push((l.name.clone(), *i));
                    continue;
                }
                let locals: BTreeSet<String> =
                    macro_defs(&l.tokens).into_iter().map(|(n, _)| n).collect();
                let site = &mut out[*i];
                for name in invocations(&l.tokens) {
                    if locals.contains(&name) {
                        site.local = true;
                        site.text.push(format!("{name} = local"));
                        continue;
                    }
                    let mut seen = BTreeSet::new();
                    let mut text = Vec::new();
                    let cx = Reach {
                        t,
                        visible: &visible,
                        caller: l,
                        id,
                        names,
                    };
                    cx.reach(&name, &mut seen, &mut text, &mut site.defs);
                    site.text.push(text.join(", "));
                }
            }
        }
    }
    out
}

struct Reach<'a> {
    t: &'a Tree,
    visible: &'a [(String, usize)],
    caller: &'a Leaf,
    id: &'a dyn Fn(&Leaf) -> String,
    names: &'a Names,
}

impl Reach<'_> {
    fn reach(
        &self,
        name: &str,
        seen: &mut BTreeSet<String>,
        out: &mut Vec<String>,
        defs: &mut BTreeSet<usize>,
    ) {
        if !seen.insert(name.to_string()) {
            return;
        }
        let mut chain = Vec::new();
        for (_, d) in self.visible.iter().rev().filter(|(n, _)| n == name) {
            chain.push(*d);
            if !conditional(&self.t.leaves[*d], self.caller) {
                break;
            }
        }
        if chain.is_empty() {
            let b = self.names.resolve(&self.caller.module, name, MACRO);
            out.push(format!("{name} = path {b:?}"));
            return;
        }
        for d in chain {
            let l = &self.t.leaves[d];
            let cond = if conditional(l, self.caller) {
                " (cfg)"
            } else {
                ""
            };
            out.push(format!("{name} = {}{cond}", (self.id)(l)));
            defs.insert(d);
            for inner in invocations(&l.tokens) {
                self.reach(&inner, seen, out, defs);
            }
        }
    }
}

pub const TYPE: u8 = 1;
pub const VALUE: u8 = 2;
pub const MACRO: u8 = 4;

const SKIP_AFTER: [&str; 7] = ["fn", "struct", "enum", "union", "trait", "type", "mod"];

type FreeNames = (BTreeSet<(String, u8)>, Vec<String>, BTreeSet<Vec<String>>);

/// Free names of a leaf with their namespace mask, the module each leading
/// `self::`/`super::` path starts from when written in `module`, and every
/// path of two or more segments (macro paths excepted).
pub fn free_names(tokens: &str, module: &str) -> FreeNames {
    struct W<'a> {
        module: &'a str,
        out: BTreeSet<(String, u8)>,
        rel: Vec<String>,
        paths: BTreeSet<Vec<String>>,
    }
    impl W<'_> {
        fn walk(&mut self, v: &[TokenTree], mut in_use: bool) {
            for (i, t) in v.iter().enumerate() {
                let TokenTree::Ident(id) = t else {
                    match t {
                        TokenTree::Group(g) => {
                            self.walk(&g.stream().into_iter().collect::<Vec<_>>(), in_use);
                        }
                        TokenTree::Punct(p) if p.as_char() == ';' => in_use = false,
                        _ => {}
                    }
                    continue;
                };
                let s = id.to_string();
                in_use |= s == "use";
                let path_next = is_punct(v.get(i + 1), ':') && is_punct(v.get(i + 2), ':');
                let after_path =
                    i >= 2 && is_punct(v.get(i - 1), ':') && is_punct(v.get(i - 2), ':');
                if (s == "self" || s == "super") && path_next && !after_path {
                    let mut m = split(self.module);
                    let mut j = i;
                    while matches!(v.get(j), Some(TokenTree::Ident(x)) if x == "super") {
                        m.pop();
                        j += 3;
                    }
                    self.rel.push(m.join("::"));
                }
                let prev = i.checked_sub(1).and_then(|j| v.get(j));
                let after_kw = matches!(prev, Some(TokenTree::Ident(p))
                    if SKIP_AFTER.contains(&p.to_string().as_str()) || (in_use && p == "as"));
                if path_next
                    && !after_path
                    && !after_kw
                    && !is_punct(prev, '.')
                    && !is_punct(prev, '$')
                    && (!is_keyword(&s) || matches!(s.as_str(), "crate" | "self" | "super"))
                {
                    let mut segs = vec![s.clone()];
                    let mut j = i;
                    while is_punct(v.get(j + 1), ':') && is_punct(v.get(j + 2), ':') {
                        let Some(TokenTree::Ident(n)) = v.get(j + 3) else {
                            break;
                        };
                        segs.push(n.to_string());
                        j += 3;
                    }
                    if segs.len() > 1 && !is_punct(v.get(j + 1), '!') {
                        self.paths.insert(segs);
                    }
                }
                if is_keyword(&s)
                    || after_kw
                    || is_punct(prev, '.')
                    || is_punct(prev, '\'')
                    || is_punct(prev, '$')
                    || after_path
                    || is_punct(v.get(i + 1), '!')
                    || (is_punct(v.get(i + 1), ':') && !path_next)
                {
                    continue;
                }
                let ns = if path_next {
                    TYPE
                } else if s.starts_with(char::is_uppercase) {
                    TYPE | VALUE
                } else {
                    VALUE
                };
                self.out.insert((s, ns));
            }
        }
    }
    let mut w = W {
        module,
        out: BTreeSet::new(),
        rel: Vec::new(),
        paths: BTreeSet::new(),
    };
    w.walk(&tts(tokens), false);
    (w.out, w.rel, w.paths)
}

fn is_keyword(s: &str) -> bool {
    matches!(
        s,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "union"
            | "macro_rules"
    )
}

struct Binding {
    ns: u8,
    id: String,
    scope: String,
    /// May name a trait: in-tree traits and imports not known to be anything
    /// else.
    tr: bool,
    /// A named import: the module it is written in and its path, resolved
    /// to the referenced items on lookup (`id`, its spelling, when that
    /// finds nothing).
    link: Option<(String, Vec<String>)>,
}

enum Target {
    Module(String),
    Opaque(String),
}

/// Module namespaces of one tree.
pub struct Names {
    locals: BTreeMap<String, BTreeMap<String, Vec<Binding>>>,
    globs: BTreeMap<String, Vec<(Target, String)>>,
    /// `use path as _`, per module.
    anon: BTreeMap<String, Vec<Binding>>,
    /// In-tree modules.
    mods: BTreeSet<String>,
    /// Imports being resolved (cycle guard).
    busy: std::cell::RefCell<BTreeSet<(String, Vec<String>)>>,
}

type TraitScope = (BTreeMap<String, BTreeSet<String>>, BTreeSet<String>);

impl Names {
    pub fn new(t: &Tree, id: &dyn Fn(&Leaf) -> String) -> Self {
        let mut n = Names {
            locals: BTreeMap::new(),
            globs: BTreeMap::new(),
            anon: BTreeMap::new(),
            mods: t.mods.keys().cloned().collect(),
            busy: Default::default(),
        };
        let mut bind = |m: &str, name: &str, b: Binding| {
            n.locals
                .entry(m.to_string())
                .or_default()
                .entry(name.to_string())
                .or_default()
                .push(b);
        };
        for l in t.leaves.iter().filter(|l| l.owner.is_none()) {
            let ns = match l.kind {
                "fn" | "const" | "static" => VALUE,
                "struct" => TYPE | VALUE,
                "enum" | "union" | "trait" | "type" | "extern_crate" => TYPE,
                _ => continue,
            };
            bind(
                &l.module,
                &l.name,
                Binding {
                    ns,
                    id: id(l),
                    scope: scope(&l.vis, &l.module),
                    tr: l.kind == "trait",
                    link: None,
                },
            );
        }
        for (path, info) in &t.mods {
            if info.vis != "?" {
                let (p, name) = path.rsplit_once("::").unwrap_or(("", path));
                bind(
                    p,
                    name,
                    Binding {
                        ns: TYPE,
                        id: format!("mod {path}"),
                        scope: scope(&info.vis, p),
                        tr: false,
                        link: None,
                    },
                );
            }
        }
        for u in &t.uses {
            let scope = scope(&u.vis, &u.module);
            match &u.name {
                Some(name) => {
                    let (id, tr) = use_target(t, &u.module, &u.path);
                    bind(
                        &u.module,
                        name,
                        Binding {
                            ns: TYPE | VALUE | MACRO,
                            id,
                            scope,
                            tr,
                            link: link(&u.module, &u.path),
                        },
                    );
                }
                None if u.path.ends_with('*') => {
                    let target = glob_target(t, &u.module, &u.path);
                    n.globs
                        .entry(u.module.clone())
                        .or_default()
                        .push((target, scope));
                }
                None => {
                    let (id, tr) = use_target(t, &u.module, &u.path);
                    if tr {
                        n.anon.entry(u.module.clone()).or_default().push(Binding {
                            ns: TYPE,
                            id,
                            scope,
                            tr,
                            link: link(&u.module, &u.path),
                        });
                    }
                }
            }
        }
        n
    }
    /// The items an import refers to, else its spelling.
    fn ids(&self, b: &Binding, ns: u8) -> BTreeSet<String> {
        let r = b
            .link
            .as_ref()
            .map(|(m, segs)| self.resolve_path(m, segs, ns))
            .unwrap_or_default();
        if r.is_empty() {
            BTreeSet::from([b.id.clone()])
        } else {
            r
        }
    }

    /// What path `segs` (last segment in namespaces `ns`) means to code in
    /// `module`: in-tree modules are walked segment by segment; the walk
    /// stops at anything else and keeps the rest as text. Empty when an
    /// in-tree module lacks the next segment.
    pub fn resolve_path(&self, module: &str, segs: &[String], ns: u8) -> BTreeSet<String> {
        let key = (module.to_string(), segs.to_vec());
        if segs.is_empty() || !self.busy.borrow_mut().insert(key.clone()) {
            return BTreeSet::new();
        }
        let at = |i: usize| if i + 1 < segs.len() { TYPE } else { ns };
        let mut i = 1;
        let mut cur = match segs[0].as_str() {
            "crate" => BTreeSet::from(["mod ".to_string()]),
            "self" => BTreeSet::from([format!("mod {module}")]),
            "super" => {
                let mut m = split(module);
                i = 0;
                while segs.get(i).is_some_and(|s| s == "super") {
                    m.pop();
                    i += 1;
                }
                BTreeSet::from([format!("mod {}", m.join("::"))])
            }
            s => self.resolve(module, s, at(0)),
        };
        while i < segs.len() {
            let [one] = Vec::from_iter(&cur)[..] else {
                break;
            };
            let Some(m) = one.strip_prefix("mod ") else {
                break;
            };
            if !self.mods.contains(m) {
                break;
            }
            let (f, o) = self.exported(m, &segs[i], at(i), module, &mut Vec::new());
            cur = if f.is_empty() { o } else { f };
            i += 1;
        }
        if i < segs.len() {
            let rest = segs[i..].join("::");
            cur = cur.into_iter().map(|c| format!("{c}::{rest}")).collect();
        }
        self.busy.borrow_mut().remove(&key);
        cur
    }

    /// What `name` (namespaces `ns`) means to code in `module`.
    pub fn resolve(&self, module: &str, name: &str, ns: u8) -> BTreeSet<String> {
        let (found, opaque) = self.exported(module, name, ns, module, &mut Vec::new());
        if found.is_empty() { opaque } else { found }
    }

    fn exported(
        &self,
        m: &str,
        name: &str,
        ns: u8,
        viewer: &str,
        path: &mut Vec<String>,
    ) -> (BTreeSet<String>, BTreeSet<String>) {
        let (mut found, mut opaque) = (BTreeSet::new(), BTreeSet::new());
        if path.iter().any(|p| p == m) {
            return (found, opaque);
        }
        let local: Vec<&Binding> = self
            .locals
            .get(m)
            .and_then(|x| x.get(name))
            .map(|v| v.iter().filter(|b| b.ns & ns != 0).collect())
            .unwrap_or_default();
        if !local.is_empty() {
            found.extend(
                local
                    .iter()
                    .filter(|b| admits(&b.scope, viewer))
                    .flat_map(|b| self.ids(b, ns)),
            );
            return (found, opaque);
        }
        path.push(m.to_string());
        for (target, s) in self.globs.get(m).into_iter().flatten() {
            if !admits(s, viewer) {
                continue;
            }
            match target {
                Target::Module(t) => {
                    let (f, o) = self.exported(t, name, ns, viewer, path);
                    found.extend(f);
                    opaque.extend(o);
                }
                Target::Opaque(p) => {
                    opaque.insert(p.clone());
                }
            }
        }
        path.pop();
        (found, opaque)
    }

    /// Traits whose methods code in `module` can call: named and `_`
    /// imports and local traits, through in-tree globs, locals shadowing
    /// glob names; opaque globs and imports of unknown kind count as tokens.
    pub fn traits(&self, module: &str) -> BTreeSet<String> {
        let (named, anon) = self.trait_scope(module, module, &mut Vec::new());
        named.into_values().flatten().chain(anon).collect()
    }

    fn trait_scope(&self, m: &str, viewer: &str, path: &mut Vec<String>) -> TraitScope {
        let (mut named, mut anon): TraitScope = Default::default();
        if path.iter().any(|p| p == m) {
            return (named, anon);
        }
        for (name, bs) in self.locals.get(m).into_iter().flatten() {
            let bs: Vec<&Binding> = bs
                .iter()
                .filter(|b| b.ns & TYPE != 0 && admits(&b.scope, viewer))
                .collect();
            if !bs.is_empty() {
                named.insert(
                    name.clone(),
                    bs.iter()
                        .filter(|b| b.tr)
                        .flat_map(|b| self.ids(b, TYPE))
                        .collect(),
                );
            }
        }
        anon.extend(
            self.anon
                .get(m)
                .into_iter()
                .flatten()
                .filter(|b| admits(&b.scope, viewer))
                .flat_map(|b| self.ids(b, TYPE)),
        );
        path.push(m.to_string());
        let mut globbed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (target, s) in self.globs.get(m).into_iter().flatten() {
            if !admits(s, viewer) {
                continue;
            }
            match target {
                Target::Module(t) => {
                    let (gn, ga) = self.trait_scope(t, viewer, path);
                    for (name, ids) in gn {
                        globbed.entry(name).or_default().extend(ids);
                    }
                    anon.extend(ga);
                }
                Target::Opaque(p) => {
                    anon.insert(p.clone());
                }
            }
        }
        path.pop();
        for (name, ids) in globbed {
            named.entry(name).or_insert(ids);
        }
        (named, anon)
    }
}

/// The module and segments of a named import of a relative or `crate::`
/// path; `None` for `::extern` paths.
fn link(module: &str, path: &str) -> Option<(String, Vec<String>)> {
    let p = path.split_once(" as ").map_or(path, |(p, _)| p);
    (!p.starts_with(":: ")).then(|| {
        (
            module.to_string(),
            p.split(" :: ").map(str::to_string).collect(),
        )
    })
}

/// A `use` path made absolute (`crate::…` for in-tree modules, the path
/// itself for extern crates, the path and module when it starts from another
/// import), and whether it may name a trait.
fn use_target(t: &Tree, module: &str, path: &str) -> (String, bool) {
    let (p, alias) = path
        .split_once(" as ")
        .map_or((path, String::new()), |(p, a)| (p, format!(" as {a}")));
    let segs: Vec<&str> = p.trim_start_matches(":: ").split(" :: ").collect();
    let mut cur = match segs[0] {
        _ if p.starts_with(":: ") => return (format!("use {path}"), true),
        "crate" => Vec::new(),
        "self" | "super" => split(module),
        s if t.mods.contains_key(&crate::tree::child(module, s)) => split(module),
        s if t
            .uses
            .iter()
            .any(|u| u.module == module && u.name.as_deref() == Some(s))
            || t.leaves
                .iter()
                .any(|l| l.owner.is_none() && l.module == module && l.name == s) =>
        {
            return (format!("use {path} in {module}"), true);
        }
        _ => return (format!("use {path}"), true),
    };
    for (i, s) in segs.iter().enumerate() {
        match *s {
            "crate" | "self" if i == 0 => {}
            "super" => {
                cur.pop();
            }
            s => cur.push(s.to_string()),
        }
    }
    let name = cur.pop().unwrap_or_default();
    let parent = cur.join("::");
    let item = crate::tree::child(&parent, &name);
    let kinds: Vec<&str> = t
        .leaves
        .iter()
        .filter(|l| l.owner.is_none() && l.module == parent && l.name == name)
        .map(|l| l.kind)
        .collect();
    let tr = if kinds.is_empty() {
        !t.mods.contains_key(&item)
    } else {
        kinds.contains(&"trait")
    };
    (format!("use crate::{item}{alias}"), tr)
}

fn glob_target(t: &Tree, module: &str, path: &str) -> Target {
    let segs: Vec<&str> = path.trim_end_matches(" :: *").split(" :: ").collect();
    let mut cur = match segs.first() {
        Some(&"crate") => Vec::new(),
        Some(&"self" | &"super") => split(module),
        Some(s) if t.mods.contains_key(&crate::tree::child(module, s)) => split(module),
        _ => return Target::Opaque(format!("glob {path} in {module}")),
    };
    for (i, s) in segs.iter().enumerate() {
        match *s {
            "crate" | "self" if i == 0 => {}
            "super" => {
                cur.pop();
            }
            s => cur.push(s.to_string()),
        }
    }
    let target = cur.join("::");
    if t.mods.contains_key(&target) {
        Target::Module(target)
    } else {
        Target::Opaque(format!("glob {path} in {module}"))
    }
}

/// Names of `macro_rules!` in the tree that expand to an `include*!`,
/// directly or through another such macro.
pub fn including_macros(t: &Tree) -> BTreeSet<String> {
    let defs: Vec<(&str, Vec<String>, bool)> = t
        .leaves
        .iter()
        .filter(|l| l.kind == "macro")
        .map(|l| {
            let inc = !crate::tree::includes(&l.tokens).is_empty()
                || crate::tree::INCLUDES
                    .iter()
                    .any(|m| l.tokens.contains(&format!("{m} !")));
            (l.name.as_str(), invocations(&l.tokens), inc)
        })
        .collect();
    let mut set: BTreeSet<String> = BTreeSet::new();
    loop {
        let before = set.len();
        for (name, calls, inc) in &defs {
            if *inc || calls.iter().any(|c| set.contains(c)) {
                set.insert((*name).to_string());
            }
        }
        if set.len() == before {
            return set;
        }
    }
}
