//! Resolution guards over the in-tree model: visibility scopes, textual
//! `macro_rules!` scope, and what a leaf's free names bind to.
//!
//! Names: every module's bindings are its top-level leaves, child modules and
//! `use` names; `use x::*` of an in-tree module is followed (visibility and
//! local-over-glob shadowing applied), any other glob is an opaque marker that
//! only counts when nothing in the tree binds the name. A leaf's names are the
//! first segments of its paths, classified by shape (`a::` type, `A` type or
//! value, `a` value); fields, methods, macro names and bound names after a
//! keyword are skipped. Not modelled: preludes, extern crates, local `let`
//! bindings shadowing items, names inside macro input.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::{TokenStream, TokenTree};

use crate::tree::{Ev, Leaf, Tree};

pub fn parent(m: &str) -> &str {
    m.rsplit_once("::").map_or("", |(p, _)| p)
}

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

/// Per leaf: which `macro_rules!` each unqualified invocation reaches by
/// textual scope (with the macros that definition invokes in turn). `id`
/// names a definition leaf.
pub fn macro_sites(t: &Tree, id: &dyn Fn(&Leaf) -> String) -> Vec<Vec<String>> {
    let mut out = vec![Vec::new(); t.leaves.len()];
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
                for name in invocations(&l.tokens) {
                    let mut seen = BTreeSet::new();
                    let mut site = Vec::new();
                    reach(t, &visible, &name, id, &mut seen, &mut site);
                    out[*i].push(site.join(", "));
                }
            }
        }
    }
    out
}

fn reach(
    t: &Tree,
    visible: &[(String, usize)],
    name: &str,
    id: &dyn Fn(&Leaf) -> String,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<String>,
) {
    if !seen.insert(name.to_string()) {
        return;
    }
    match visible.iter().rev().find(|(n, _)| n == name) {
        Some((_, d)) => {
            out.push(format!("{name} = {}", id(&t.leaves[*d])));
            for inner in invocations(&t.leaves[*d].tokens) {
                reach(t, visible, &inner, id, seen, out);
            }
        }
        None => out.push(format!("{name} = (none in the tree)")),
    }
}

pub const TYPE: u8 = 1;
pub const VALUE: u8 = 2;

const SKIP_AFTER: [&str; 10] = [
    "fn", "struct", "enum", "union", "trait", "type", "mod", "as", "const", "static",
];

/// Free names of a leaf with their namespace mask; `true` if it has a path
/// starting with `self::`/`super::`.
pub fn free_names(tokens: &str) -> (BTreeSet<(String, u8)>, bool) {
    fn walk(v: &[TokenTree], out: &mut BTreeSet<(String, u8)>, rel: &mut bool) {
        for (i, t) in v.iter().enumerate() {
            let TokenTree::Ident(id) = t else {
                if let TokenTree::Group(g) = t {
                    walk(&g.stream().into_iter().collect::<Vec<_>>(), out, rel);
                }
                continue;
            };
            let s = id.to_string();
            let path_next = is_punct(v.get(i + 1), ':') && is_punct(v.get(i + 2), ':');
            if (s == "self" || s == "super") && path_next {
                let first = i < 2 || !(is_punct(v.get(i - 1), ':') && is_punct(v.get(i - 2), ':'));
                *rel |= first;
            }
            let prev = i.checked_sub(1).and_then(|j| v.get(j));
            let after_kw = matches!(prev, Some(TokenTree::Ident(p)) if SKIP_AFTER.contains(&p.to_string().as_str()));
            if is_keyword(&s)
                || after_kw
                || is_punct(prev, '.')
                || is_punct(prev, '\'')
                || (is_punct(prev, ':') && i >= 2 && is_punct(v.get(i - 2), ':'))
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
            out.insert((s, ns));
        }
    }
    let (mut out, mut rel) = (BTreeSet::new(), false);
    walk(&tts(tokens), &mut out, &mut rel);
    (out, rel)
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
}

enum Target {
    Module(String),
    Opaque(String),
}

/// Module namespaces of one tree.
pub struct Names {
    locals: BTreeMap<String, BTreeMap<String, Vec<Binding>>>,
    globs: BTreeMap<String, Vec<(Target, String)>>,
}

impl Names {
    pub fn new(t: &Tree, id: &dyn Fn(&Leaf) -> String) -> Self {
        let mut n = Names {
            locals: BTreeMap::new(),
            globs: BTreeMap::new(),
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
            let scope = scope(&l.vis, &l.module);
            bind(
                &l.module,
                &l.name,
                Binding {
                    ns,
                    id: id(l),
                    scope,
                },
            );
        }
        for (path, info) in &t.mods {
            if let Some((p, name)) = path.rsplit_once("::")
                && info.vis != "?"
            {
                let id = format!("mod {path}");
                bind(
                    p,
                    name,
                    Binding {
                        ns: TYPE,
                        id,
                        scope: scope(&info.vis, p),
                    },
                );
            }
        }
        for u in &t.uses {
            let scope = scope(&u.vis, &u.module);
            match &u.name {
                Some(name) => {
                    let id = format!("use {} in {}", u.path, u.module);
                    bind(
                        &u.module,
                        name,
                        Binding {
                            ns: TYPE | VALUE,
                            id,
                            scope,
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
                None => {}
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
}

fn glob_target(t: &Tree, module: &str, path: &str) -> Target {
    let segs: Vec<&str> = path.trim_end_matches(" :: *").split(" :: ").collect();
    let mut cur = match segs.first() {
        Some(&"crate") => Vec::new(),
        Some(&"self" | &"super") => split(module),
        Some(s) if t.mods.contains_key(&format!("{module}::{s}")) => split(module),
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
