//! Resolution guards over the in-tree model: visibility scopes, textual
//! `macro_rules!` scope, what a leaf's free names bind to, traits in scope.
//!
//! Names: every module's bindings are its top-level leaves, child modules and
//! `use` names (paths made absolute); `use x::*` of an in-tree module is
//! followed (visibility and local-over-glob shadowing applied), any other
//! glob is an opaque marker that only counts when nothing in the tree binds
//! the name. A leaf's names are the first segments of its paths (impl head
//! included), classified by shape (`a::` type, `A` type or value, `a`
//! value); fields, methods, macro names, `$` metavariables and names bound
//! after a keyword or in a `use … as` are skipped.
//!
//! Checked per moved leaf and per delegated helper, before against after:
//! those names; the textual macro chain of each invocation down to the first
//! definition active wherever the caller is (cfg'd ones listed), else the
//! macro's path binding; the free names in every arm of the reached
//! definitions, resolved at the caller; the module each `self::`/`super::`
//! path starts from; the traits in scope (named, `_` and glob imports,
//! imports of unknown kind and opaque globs as tokens); for delegations,
//! that the self type resolves to one in-tree item.
//!
//! Refused: moving a leaf that invokes an in-tree `macro_rules!`, or defines
//! and invokes its own, to another module, unless the manifest lists it under
//! `exceptions` with a reason (the checks above still apply).
//!
//! Not modelled: preludes, extern crates, local `let` and item shadowing,
//! names inside macro input, inherent-vs-trait method priority, macros named
//! by path (`a::m!`), `#[macro_export]`, cfg values.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::{TokenStream, TokenTree};

use crate::tree::{Ev, Leaf, Tree};

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

/// Whether a definition may be inactive where `caller` is compiled.
fn conditional(def: &Leaf, caller: &Leaf) -> bool {
    !def.ctx.cfg.is_subset(&caller.ctx.cfg)
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

/// Free names of a leaf with their namespace mask, and the module each
/// leading `self::`/`super::` path starts from when written in `module`.
pub fn free_names(tokens: &str, module: &str) -> (BTreeSet<(String, u8)>, Vec<String>) {
    struct W<'a> {
        module: &'a str,
        out: BTreeSet<(String, u8)>,
        rel: Vec<String>,
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
    };
    w.walk(&tts(tokens), false);
    (w.out, w.rel)
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
}

enum Target {
    Module(String),
    Opaque(String),
}

/// Module namespaces of one tree.
pub struct Names {
    locals: BTreeMap<String, BTreeMap<String, Vec<Binding>>>,
    globs: BTreeMap<String, Vec<(Target, String)>>,
    /// `use path as _`, per module: id and scope.
    anon: BTreeMap<String, Vec<(String, String)>>,
}

type TraitScope = (BTreeMap<String, BTreeSet<String>>, BTreeSet<String>);

impl Names {
    pub fn new(t: &Tree, id: &dyn Fn(&Leaf) -> String) -> Self {
        let mut n = Names {
            locals: BTreeMap::new(),
            globs: BTreeMap::new(),
            anon: BTreeMap::new(),
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
                        n.anon
                            .entry(u.module.clone())
                            .or_default()
                            .push((id, scope));
                    }
                }
            }
        }
        n
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
                    .map(|b| b.id.clone()),
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
                    bs.iter().filter(|b| b.tr).map(|b| b.id.clone()).collect(),
                );
            }
        }
        anon.extend(
            self.anon
                .get(m)
                .into_iter()
                .flatten()
                .filter(|(_, s)| admits(s, viewer))
                .map(|(id, _)| id.clone()),
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
