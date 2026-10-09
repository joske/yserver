//! `split verify`: the move changed placement only.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use quote::ToTokens;
use syn::{Expr, FnArg, ImplItemFn, Pat, Stmt, Visibility, visit_mut::VisitMut};

use crate::{
    apply::Manifest,
    scope::{self, Names},
    tree::{self, Leaf, Res, Source, Tree, dir_of, includes, join, tok},
};

pub struct Spec<'a> {
    pub manifest: Option<&'a Manifest>,
    pub old_root: String,
    pub new_root: String,
    pub module: String,
    pub delegate: bool,
}

/// Old leaf key → new leaf key, from the manifest's table or the identity.
pub fn table(spec: &Spec, before: &Tree) -> Res<BTreeMap<String, String>> {
    let Some(m) = spec.manifest else {
        return Ok(before.leaves.iter().map(|l| (l.okey(), l.key())).collect());
    };
    let text = std::fs::read_to_string(m.table_path())
        .map_err(|e| format!("{}: {e}", m.table_path().display()))?;
    text.lines()
        .map(|l| {
            l.split_once(" => ")
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .ok_or_else(|| format!("bad table line: {l}"))
        })
        .collect()
}

fn first_diff(a: &str, b: &str) -> String {
    let i = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    let lo = i.saturating_sub(60);
    let cut = |s: &str| s.get(lo..(i + 60).min(s.len())).unwrap_or("").to_string();
    format!("\n      before: …{}…\n      after:  …{}…", cut(a), cut(b))
}

fn allowed_vis(v: &str) -> bool {
    v == "pub (super)" || v.starts_with("pub (in ")
}

/// The trait body moved to an inherent fn and the trait method is now one
/// call `Self::<callee>(self, <params>)`: returns the callee name.
fn delegate_callee(old: &ImplItemFn, new: &ImplItemFn) -> Option<String> {
    let mut a = new.clone();
    a.block = old.block.clone();
    a.vis = Visibility::Inherited;
    let mut b = old.clone();
    b.vis = Visibility::Inherited;
    if tok(&a) != tok(&b) {
        return None;
    }
    let [Stmt::Expr(Expr::Call(call), None)] = new.block.stmts.as_slice() else {
        return None;
    };
    let Expr::Path(f) = &*call.func else {
        return None;
    };
    let segs: Vec<&syn::PathSegment> = f.path.segments.iter().collect();
    let [ty, callee] = segs.as_slice() else {
        return None;
    };
    if f.qself.is_some()
        || f.path.leading_colon.is_some()
        || ty.ident != "Self"
        || !ty.arguments.is_none()
        || !callee.arguments.is_none()
    {
        return None;
    }
    let params: Vec<String> = old
        .sig
        .inputs
        .iter()
        .map(|i| match i {
            FnArg::Receiver(_) => "self".to_string(),
            FnArg::Typed(pt) => match &*pt.pat {
                Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => {
                    pi.ident.to_string()
                }
                _ => String::new(),
            },
        })
        .collect();
    let args: Vec<String> = call
        .args
        .iter()
        .map(|e| match e {
            Expr::Path(p) if p.qself.is_none() => p
                .path
                .get_ident()
                .map(ToString::to_string)
                .unwrap_or_default(),
            _ => String::new(),
        })
        .collect();
    (params == args && params.iter().all(|p| !p.is_empty())).then(|| callee.ident.to_string())
}

/// Same attributes, qualifiers, generics, signature and body; only the name
/// and visibility may differ.
fn same_fn(old: &ImplItemFn, new: &ImplItemFn) -> bool {
    let canon = |f: &ImplItemFn| {
        let mut f = f.clone();
        f.sig.ident = old.sig.ident.clone();
        f.vis = Visibility::Inherited;
        tree::Commas.visit_impl_item_fn_mut(&mut f);
        tok(&f)
    };
    canon(old) == canon(new)
}

pub fn run(spec: &Spec, base: &dyn Source, head: &dyn Source, touched: &[String]) -> Res<bool> {
    let (info, errs) = check(spec, base, head, touched)?;
    for l in info {
        println!("{l}");
    }
    for e in &errs {
        println!("FAIL {e}");
    }
    println!(
        "{}",
        if errs.is_empty() {
            "verify: OK"
        } else {
            "verify: FAILED"
        }
    );
    Ok(errs.is_empty())
}

/// Summary lines and failures.
pub fn check(
    spec: &Spec,
    base: &dyn Source,
    head: &dyn Source,
    touched: &[String],
) -> Res<(Vec<String>, Vec<String>)> {
    let before = tree::load(base, &spec.old_root, &spec.module)?;
    let after = tree::load(head, &spec.new_root, &spec.module)?;
    let map = table(spec, &before)?;
    let mut errs: Vec<String> = Vec::new();

    let mut known: BTreeSet<&str> = before
        .files
        .iter()
        .chain(&after.files)
        .map(String::as_str)
        .collect();
    let extra_files: Vec<String> = spec
        .manifest
        .map(|m| vec![rel(&m.path), rel(&m.table_path())])
        .unwrap_or_default();
    known.extend(extra_files.iter().map(String::as_str));
    for t in touched {
        if !known.contains(t.as_str()) {
            errs.push(format!("touched file outside the moved tree: {t}"));
        }
    }

    // Path edits are keyed by old relative item key.
    let mut edits: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    if let Some(m) = spec.manifest {
        for e in m.path_edits.iter().filter(|e| !e.item.starts_with("mod ")) {
            let old = m.full("") + "::" + &e.item;
            let new = map
                .get(&old)
                .ok_or_else(|| format!("path edit: unknown item {}", e.item))?;
            edits
                .entry(new.clone())
                .or_default()
                .push((e.from.clone(), e.to.clone()));
        }
    }
    let vis_ok: BTreeMap<String, String> = spec
        .manifest
        .map(|m| {
            m.visibility
                .iter()
                .filter_map(|(k, v)| {
                    let old = format!("{}::{}", m.full(""), k);
                    map.get(&old).map(|n| (n.clone(), norm_vis(v)))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut groups: BTreeMap<String, (Vec<&Leaf>, Vec<&Leaf>)> = BTreeMap::new();
    for l in &before.leaves {
        match map.get(&l.okey()) {
            Some(n) => groups.entry(n.clone()).or_default().0.push(l),
            None => errs.push(format!("{}: not in the path table", l.okey())),
        }
    }
    for l in &after.leaves {
        groups.entry(l.key()).or_default().1.push(l);
    }

    let rx = Resolution::new(&before, &after, &map);
    let inc_macros = scope::including_macros(&before);
    let norm_tokens = |key: &str, l: &Leaf| {
        let mut t = l.tokens.clone();
        for (from, to) in edits.get(key).into_iter().flatten() {
            t = tree::edit_includes(&t, to, from).0;
        }
        t
    };
    let (mut matched, mut vis_changes, mut incl, mut delegated) = (0, 0, 0, 0);
    let mut delegations: Vec<(&Leaf, String)> = Vec::new();
    let mut extra: Vec<&Leaf> = Vec::new();
    for (key, (b, a)) in &groups {
        let mut a: Vec<(&Leaf, String)> = a.iter().map(|l| (*l, norm_tokens(key, l))).collect();
        let mut unpaired = Vec::new();
        for ob in b {
            match a
                .iter()
                .position(|(l, t)| *t == ob.tokens && l.ctx == ob.ctx)
            {
                Some(i) => {
                    let (na, _) = a.remove(i);
                    matched += 1;
                    errs.extend(rx.check(ob, na));
                    if na.vis != ob.vis {
                        if ob.vis.is_empty()
                            && allowed_vis(&na.vis)
                            && vis_ok.get(key) == Some(&na.vis)
                        {
                            vis_changes += 1;
                        } else {
                            errs.push(format!(
                                "{key}: visibility {:?} → {:?} not allowed by the manifest",
                                ob.vis, na.vis
                            ));
                        }
                    }
                    if na.comments != ob.comments {
                        errs.push(format!(
                            "{key}: comments changed\n      before: {:?}\n      after:  {:?}",
                            ob.comments, na.comments
                        ));
                    }
                    errs.extend(includes_ok(
                        spec,
                        base,
                        head,
                        key,
                        ob,
                        na,
                        &inc_macros,
                        &mut incl,
                    ));
                }
                None => unpaired.push(*ob),
            }
        }
        for ob in unpaired {
            if let Some(i) = a.iter().position(|(_, t)| *t == ob.tokens) {
                let (na, _) = a.remove(i);
                errs.push(format!(
                    "{key}: effective cfg/attributes changed\n      before: {:?}\n      after:  {:?}",
                    ob.ctx, na.ctx
                ));
            } else if let [(na, t)] = a.as_slice() {
                let callee = (spec.delegate && ob.owner.as_ref().is_some_and(|o| o.is_trait))
                    .then(|| {
                        ob.func
                            .as_ref()
                            .zip(na.func.as_ref())
                            .and_then(|(o, n)| delegate_callee(o, n))
                    })
                    .flatten();
                match callee {
                    Some(c) if na.ctx == ob.ctx => delegations.push((ob, c)),
                    _ => errs.push(format!(
                        "{key}: tokens changed ({}:{} → {}:{}){}",
                        ob.file,
                        ob.line,
                        na.file,
                        na.line,
                        first_diff(&ob.tokens, t)
                    )),
                }
                a.clear();
            } else {
                errs.push(format!(
                    "{key}: missing after the move ({}:{})",
                    ob.file, ob.line
                ));
            }
        }
        for (na, _) in a {
            extra.push(na);
        }
    }

    // Delegations: each marker must be claimed by the one inherent fn of that
    // name on the type, in the trait impl's generic context and effective cfg,
    // equal to the old trait fn but for name and visibility.
    let mut news = extra;
    for (old, callee) in delegations {
        let ofn = old.func.as_ref().expect("trait fn");
        let oo = old.owner.as_ref().expect("owner");
        let named: Vec<usize> = news
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                n.kind == "fn"
                    && n.name == callee
                    && n.owner
                        .as_ref()
                        .is_some_and(|o| !o.is_trait && o.self_ty == oo.self_ty)
            })
            .map(|(i, _)| i)
            .collect();
        let named_after = after
            .leaves
            .iter()
            .filter(|n| {
                n.kind == "fn"
                    && n.name == callee
                    && n.owner
                        .as_ref()
                        .is_some_and(|o| !o.is_trait && o.self_ty == oo.self_ty)
            })
            .count();
        let ok = match named.as_slice() {
            [i] if named_after == 1 => {
                let n = news[*i];
                n.ctx == old.ctx
                    && n.vis != "pub"
                    && n.owner.as_ref().is_some_and(|o| o.header == oo.inherent)
                    && n.func.as_ref().is_some_and(|f| same_fn(ofn, f))
            }
            _ => false,
        };
        if ok {
            news.remove(named[0]);
            delegated += 1;
        } else {
            errs.push(format!(
                "{}: forwards to Self::{callee}(..) but no inherent fn carries the old body ({} candidates)",
                old.okey(),
                named_after
            ));
        }
    }
    for n in news {
        errs.push(format!("{}: new leaf ({}:{})", n.key(), n.file, n.line));
    }

    // Every `use`, per module: only manifest lines may be added, none lost.
    let uses = |t: &Tree| -> BTreeMap<(String, String), usize> {
        let mut m = BTreeMap::new();
        for u in &t.uses {
            *m.entry((u.module.clone(), u.text())).or_default() += 1;
        }
        m
    };
    let mut allow: BTreeMap<(String, String), usize> = BTreeMap::new();
    if let Some(m) = spec.manifest {
        for x in &m.modules {
            for l in &x.lines {
                let u: syn::ItemUse =
                    syn::parse_str(l).map_err(|e| format!("manifest line {l:?}: {e}"))?;
                let mut v = Vec::new();
                tree::flatten_use(&u, &m.full(&x.name), &mut v);
                for u in v {
                    *allow.entry((u.module.clone(), u.text())).or_default() += 1;
                }
            }
        }
    }
    let (ub, ua) = (uses(&before), uses(&after));
    for (k, n) in &ua {
        let had = ub.get(k).copied().unwrap_or(0);
        if *n > had + allow.get(k).copied().unwrap_or(0) {
            let what = if k.1.starts_with("use") {
                "import"
            } else {
                "re-export"
            };
            errs.push(format!("{}: {what} `{}` not in the manifest", k.0, k.1));
        }
    }
    for (k, n) in &ub {
        if ua.get(k).copied().unwrap_or(0) < *n {
            errs.push(format!("{}: `{}` removed", k.0, k.1));
        }
    }

    // Comments outside leaves: none lost, none invented.
    let mut pool: BTreeMap<&str, i64> = BTreeMap::new();
    for c in &before.pool {
        *pool.entry(c).or_default() += 1;
    }
    let after_set: BTreeSet<&str> = after.pool.iter().map(String::as_str).collect();
    for c in &after.pool {
        *pool.entry(c).or_default() -= 1;
    }
    for (c, n) in &pool {
        if *n > 0 {
            errs.push(format!("comment lost: {c:?}"));
        } else if !before.pool.iter().any(|x| x == c) && after_set.contains(c) {
            errs.push(format!("comment added: {c:?}"));
        }
    }

    // Residual risk: names defined in more than one module may shadow.
    let mut defs: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for l in after.leaves.iter().filter(|l| l.owner.is_none()) {
        defs.entry(&l.name).or_default().insert(&l.module);
    }
    let shadow: Vec<String> = defs
        .iter()
        .filter(|(_, m)| m.len() > 1)
        .map(|(n, m)| format!("{n} in {m:?}"))
        .collect();

    let (lb, la) = (line_bag(base, &before.files), line_bag(head, &after.files));
    let removed: usize = lb
        .iter()
        .map(|(l, n)| n.saturating_sub(*la.get(l).unwrap_or(&0)))
        .sum();
    let added: usize = la
        .iter()
        .map(|(l, n)| n.saturating_sub(*lb.get(l).unwrap_or(&0)))
        .sum();

    let mut info = vec![
        format!(
            "leaves: {} before, {} after, {matched} identical; {vis_changes} manifest visibility changes; {incl} include paths same bytes; {delegated} delegations",
            before.leaves.len(),
            after.leaves.len()
        ),
        format!(
            "files: {} → {}; line smoke (info): -{removed} +{added} lines",
            before.files.len(),
            after.files.len()
        ),
    ];
    if !shadow.is_empty() {
        info.push(format!(
            "review, names defined in more than one module: {}",
            shadow.join("; ")
        ));
    }
    Ok((info, errs))
}

/// Include paths of a moved leaf resolve to the same bytes; `include!`,
/// non-literal paths and macros that include are refused once the leaf
/// changes directory, module or file. Leaves of files other than the split
/// root must still come from the same file.
fn includes_ok(
    spec: &Spec,
    base: &dyn Source,
    head: &dyn Source,
    key: &str,
    ob: &Leaf,
    na: &Leaf,
    inc_macros: &BTreeSet<String>,
    incl: &mut usize,
) -> Vec<String> {
    let mut errs = Vec::new();
    if ob.file != spec.old_root && na.file != ob.file {
        errs.push(format!("{key}: loaded from {} (was {})", na.file, ob.file));
    }
    let moved = dir_of(&ob.file) != dir_of(&na.file);
    let relocated = ob.file != na.file || ob.module != na.module;
    for (o, n) in includes(&ob.tokens).iter().zip(includes(&na.tokens)) {
        match (&o.lit, &n.lit) {
            _ if o.mac == "include" && relocated => {
                errs.push(format!("{key}: include! in a moved leaf is not supported"))
            }
            (Some(po), Some(pn)) => {
                let (fo, fn_) = (join(dir_of(&ob.file), po), join(dir_of(&na.file), pn));
                match (base.read(&fo), head.read(&fn_)) {
                    (Some(x), Some(y)) if x == y => *incl += 1,
                    _ => errs.push(format!(
                        "{key}: include {po:?} ({fo}) and {pn:?} ({fn_}) do not resolve to the same bytes"
                    )),
                }
            }
            _ if moved => errs.push(format!(
                "{key}: {}! with a non-literal path in a moved leaf is not supported",
                o.mac
            )),
            _ => {}
        }
    }
    if moved
        && let Some(m) = scope::invocations(&ob.tokens)
            .into_iter()
            .find(|m| inc_macros.contains(m))
    {
        errs.push(format!(
            "{key}: invokes {m}!, which includes a file, from a new directory"
        ));
    }
    errs
}

/// Macro and name resolution of both trees, comparable through the table.
struct Resolution<'a> {
    macros: [Vec<Vec<String>>; 2],
    names: [Names; 2],
    map: &'a BTreeMap<String, String>,
}

fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl<'a> Resolution<'a> {
    fn new(before: &Tree, after: &Tree, map: &'a BTreeMap<String, String>) -> Self {
        let old = |l: &Leaf| map.get(&l.okey()).cloned().unwrap_or_else(|| l.okey());
        let new = |l: &Leaf| l.key();
        let def_old = |l: &Leaf| format!("{} {:x}", old(l), hash(&l.tokens));
        let def_new = |l: &Leaf| format!("{} {:x}", new(l), hash(&l.tokens));
        Resolution {
            macros: [
                scope::macro_sites(before, &def_old),
                scope::macro_sites(after, &def_new),
            ],
            names: [Names::new(before, &old), Names::new(after, &new)],
            map,
        }
    }

    fn check(&self, ob: &Leaf, na: &Leaf) -> Vec<String> {
        let key = self.map.get(&ob.okey()).cloned().unwrap_or_default();
        let mut errs = Vec::new();
        let (mb, ma) = (&self.macros[0][ob.idx], &self.macros[1][na.idx]);
        if mb != ma {
            errs.push(format!(
                "{key}: macro resolution changed\n      before: {mb:?}\n      after:  {ma:?}"
            ));
        }
        let (names, rel) = scope::free_names(&ob.tokens);
        if rel && ob.module != na.module {
            errs.push(format!(
                "{key}: moved from {} to {} with a `self::`/`super::` path",
                ob.module, na.module
            ));
        }
        for (n, ns) in names {
            let (b, a) = (
                self.names[0].resolve(&ob.module, &n, ns),
                self.names[1].resolve(&na.module, &n, ns),
            );
            if b != a {
                errs.push(format!(
                    "{key}: name `{n}` resolves differently\n      before: {b:?}\n      after:  {a:?}"
                ));
            }
        }
        errs
    }
}

fn norm_vis(v: &str) -> String {
    syn::parse_str::<Visibility>(v)
        .map_or_else(|_| v.to_string(), |x| x.to_token_stream().to_string())
}

fn rel(p: &Path) -> String {
    let s = p.to_string_lossy().to_string();
    s.trim_start_matches("./").to_string()
}

fn line_bag(src: &dyn Source, files: &[String]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for f in files {
        let text = String::from_utf8_lossy(&src.read(f).unwrap_or_default()).to_string();
        for l in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            *m.entry(l.to_string()).or_default() += 1;
        }
    }
    m
}
