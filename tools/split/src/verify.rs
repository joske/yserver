//! `split verify`: the move changed placement only.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use quote::ToTokens;
use syn::{Expr, FnArg, ImplItemFn, Pat, Stmt, Visibility, visit_mut::VisitMut};

use crate::{
    apply::Manifest,
    scope::{self, Names, scope},
    tree::{self, Leaf, Res, Source, Tree, dir_of, includes, join, tok},
};

/// Committed manifests and path tables; never part of a moved tree.
const MANIFESTS: &str = "tools/split/manifests/";

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
        let manifest = t.starts_with(MANIFESTS)
            && (t.ends_with(".toml") || t.ends_with(".paths"))
            && !t[MANIFESTS.len()..].contains('/');
        if !known.contains(t.as_str()) && !manifest {
            errs.push(format!("touched file outside the moved tree: {t}"));
        }
    }

    // Path edits are keyed by old relative item key.
    let mut edits: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    if let Some(m) = spec.manifest {
        for e in m.path_edits.iter().filter(|e| !e.item.starts_with("mod ")) {
            let old = tree::child(&m.full(""), &e.item);
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
                    let old = tree::child(&m.full(""), k);
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

    let audited = |table: fn(&Manifest) -> &BTreeMap<String, String>| match spec.manifest {
        Some(m) => table(m)
            .iter()
            .map(|(k, why)| {
                let old = tree::child(&m.full(""), k);
                match map.get(&old) {
                    _ if why.trim().is_empty() => Err(format!("exception {k}: no reason given")),
                    Some(n) => Ok((n.clone(), why.clone())),
                    None => Err(format!("exception: unknown item {k}")),
                }
            })
            .collect::<Res<BTreeMap<String, String>>>(),
        None => Ok(BTreeMap::new()),
    };
    let exceptions = audited(|m| &m.exceptions)?;
    let locations = audited(|m| &m.locations)?;
    let log_targets = audited(|m| &m.log_targets)?;
    let crates = [
        scope::Crate::scan(base, &spec.old_root, &spec.module),
        scope::Crate::scan(head, &spec.new_root, &spec.module),
    ];
    let rx = Resolution::new(
        &before,
        &after,
        &map,
        [&exceptions, &locations, &log_targets],
        crates,
    );
    let inc_macros = scope::including_macros(&before);
    let btok: Vec<String> = before
        .leaves
        .iter()
        .map(|l| rx.commas(0, l, &l.tokens))
        .collect();
    let norm_tokens = |key: &str, l: &Leaf| {
        let mut t = l.tokens.clone();
        for (from, to) in edits.get(key).into_iter().flatten() {
            t = tree::edit_includes(&t, to, from).0;
        }
        rx.commas(1, l, &t)
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
                .position(|(l, t)| *t == btok[ob.idx] && l.ctx == ob.ctx)
            {
                Some(i) => {
                    let (na, _) = a.remove(i);
                    matched += 1;
                    errs.extend(rx.check(ob, na, ob.owner.as_ref().map(|o| o.header.as_str())));
                    errs.extend(rx.location(ob, na));
                    let (so, sn) = (scope(&ob.vis, &ob.module), scope(&na.vis, &na.module));
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
                    if !scope::within(&sn, &so) {
                        errs.push(format!("{key}: visibility widened from {so} to {sn}"));
                    }
                    let (cb, ca) = (
                        mod_chain(&before, &ob.module),
                        mod_chain(&after, &na.module),
                    );
                    if cb != ca {
                        errs.push(format!(
                            "{key}: enclosing module visibility changed\n      before: {cb:?}\n      after:  {ca:?}"
                        ));
                    }
                    let notes = |l: &Leaf| l.owner.as_ref().map(|o| o.notes.clone());
                    if notes(ob) != notes(na) {
                        errs.push(format!(
                            "{key}: impl head comments changed\n      before: {:?}\n      after:  {:?}",
                            notes(ob),
                            notes(na)
                        ));
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
            if let Some(i) = a.iter().position(|(_, t)| *t == btok[ob.idx]) {
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
                        first_diff(&btok[ob.idx], t)
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
            let n = news.remove(named[0]);
            delegated += 1;
            errs.extend(rx.check(old, n, Some(&oo.inherent)));
            errs.extend(rx.location(old, n));
            errs.extend(rx.self_ty(old, &oo.self_ty));
            if n.comments != old.comments {
                errs.push(format!(
                    "{}: comments changed in the delegated helper\n      before: {:?}\n      after:  {:?}",
                    old.okey(),
                    old.comments,
                    n.comments
                ));
            }
            errs.extend(includes_ok(
                spec,
                base,
                head,
                &old.okey(),
                old,
                n,
                &inc_macros,
                &mut incl,
            ));
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
    let used = rx.used.borrow();
    for (i, what) in ["exception", "location exception", "log target exception"]
        .iter()
        .enumerate()
    {
        for k in rx.exceptions[i].keys().filter(|k| !used[i].contains(*k)) {
            errs.push(format!("{k}: {what} not needed"));
        }
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

    // Comments and wrapper docs outside leaves and impl heads: same multiset.
    let mut pool: BTreeMap<&str, i64> = BTreeMap::new();
    for c in &before.pool {
        *pool.entry(c).or_default() += 1;
    }
    for c in &after.pool {
        *pool.entry(c).or_default() -= 1;
    }
    for (c, n) in &pool {
        match n.signum() {
            1 => errs.push(format!("comment lost ({n}x): {c:?}")),
            -1 => errs.push(format!("comment added ({}x): {c:?}", -n)),
            _ => {}
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
            "leaves: {} before, {} after, {matched} identical; {vis_changes} manifest visibility changes; {incl} include paths same bytes; {delegated} delegations; {} audited exceptions",
            before.leaves.len(),
            after.leaves.len(),
            used[0].len()
        ),
        format!(
            "files: {} → {}; line smoke (info): -{removed} +{added} lines",
            before.files.len(),
            after.files.len()
        ),
    ];
    let [pos, modp, desc] = *rx.tests.borrow();
    info.push(format!(
        "location-sensitive: {pos} test leaves with line!/column!/file!/Location at a new position, {modp} with module_path!/log macros without target: in a new module (allowed in test code); {} audited location exceptions",
        used[1].len()
    ));
    info.push(format!(
        "log targets: {desc} leaves moved to descendant modules (prefix filters preserved); {} audited log target exceptions",
        used[2].len()
    ));
    info.push(NOT_COVERED.to_string());
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
#[allow(clippy::too_many_arguments)]
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

/// Printed with every verify: what an OK does not vouch for.
const NOT_COVERED: &str = "not covered (outside the guarantee): panic!/unwrap/expect and other panic locations; #[track_caller] fns of other crates (std's, dependencies'); std::any::type_name strings of moved types; diagnostic text std macros derive from their input; bindings created by macro expansion (only refused, not modelled); log target filters longer than the old module path (a directive naming the new child overrides the parent's) and the displayed target of leaves moved to a descendant module; tracing span/event metadata (no tracing in this repo)";

/// Macro and name resolution of both trees, comparable through the table.
struct Resolution<'a> {
    before: &'a Tree,
    macros: [Vec<scope::Site>; 2],
    names: [Names; 2],
    map: &'a BTreeMap<String, String>,
    /// New leaf key → audited reason: for moving macro invocations, for
    /// moving positional code, for moving implicit log targets out of
    /// their module's subtree.
    exceptions: [&'a BTreeMap<String, String>; 3],
    used: std::cell::RefCell<[BTreeSet<String>; 3]>,
    /// Macro names that are not positively std's in either crate.
    not_std: BTreeSet<String>,
    /// The crate around each tree.
    crates: [scope::Crate; 2],
    /// `#[track_caller]` fns that pass their caller's position on, and
    /// `use … as` renames, of both sides.
    tracked: BTreeSet<String>,
    aliases: BTreeMap<String, BTreeSet<String>>,
    /// Per side: modules with an item-position macro invocation that may
    /// bind a name.
    binding_calls: [BTreeSet<String>; 2],
    /// Test leaves moved with positional / module-sensitive code, and
    /// production leaves with implicit log targets moved to a descendant.
    tests: std::cell::RefCell<[usize; 3]>,
}

fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl<'a> Resolution<'a> {
    fn new(
        before: &'a Tree,
        after: &'a Tree,
        map: &'a BTreeMap<String, String>,
        exceptions: [&'a BTreeMap<String, String>; 3],
        crates: [scope::Crate; 2],
    ) -> Self {
        let old = |l: &Leaf| map.get(&l.okey()).cloned().unwrap_or_else(|| l.okey());
        let new = |l: &Leaf| l.key();
        let def_old = |l: &Leaf| format!("{} {:x}", old(l), hash(&l.tokens));
        let def_new = |l: &Leaf| format!("{} {:x}", new(l), hash(&l.tokens));
        let names = [Names::new(before, &old), Names::new(after, &new)];
        let mut aliases = crates[0].aliases.clone();
        for (a, o) in &crates[1].aliases {
            aliases
                .entry(a.clone())
                .or_default()
                .extend(o.iter().cloned());
        }
        let binding_calls = [0, 1].map(|side| {
            let t = [before, after][side];
            t.leaves
                .iter()
                .filter(|l| l.kind == "macro_call" && l.owner.is_none())
                .filter(|l| {
                    scope::stmt_calls(&l.tokens).iter().any(|p| {
                        !scope::inert(p, &crates[side], &Self::bound(&names[side], &l.module, p))
                    })
                })
                .map(|l| l.module.clone())
                .collect()
        });
        Resolution {
            before,
            macros: [
                scope::macro_sites(before, &def_old, &names[0]),
                scope::macro_sites(after, &def_new, &names[1]),
            ],
            names,
            map,
            exceptions,
            used: Default::default(),
            not_std: crates[0]
                .not_std
                .union(&crates[1].not_std)
                .cloned()
                .collect(),
            tracked: crates[0]
                .tracked
                .union(&crates[1].tracked)
                .cloned()
                .collect(),
            aliases,
            binding_calls,
            crates,
            tests: Default::default(),
        }
    }

    /// What an unqualified macro path is imported as in `module`.
    fn bound(names: &Names, module: &str, path: &[String]) -> BTreeSet<String> {
        match path {
            [one] => names.resolve(module, one, scope::MACRO),
            _ => BTreeSet::new(),
        }
    }

    /// Leaf `l` (tree `side`) is where a macro expansion may bind a name:
    /// the crate's enclosing modules, a module-level invocation in its
    /// module or an ancestor, or a statement/item-position invocation in
    /// the leaf, of a macro not known to be inert.
    fn exposed(&self, side: usize, l: &Leaf) -> bool {
        self.crates[side].outer_binds
            || self.binding_calls[side]
                .iter()
                .any(|m| scope::under(&l.module, m))
            || scope::stmt_calls(&l.tokens).iter().any(|p| {
                !scope::inert(
                    p,
                    &self.crates[side],
                    &Self::bound(&self.names[side], &l.module, p),
                )
            })
    }

    fn diff(&self, key: &str, what: &str, ob: &Leaf, na: &Leaf, n: &str, ns: u8) -> Option<String> {
        let (b, a) = (
            self.names[0].resolve(&ob.module, n, ns),
            self.names[1].resolve(&na.module, n, ns),
        );
        (b != a).then(|| {
            format!("{key}: name `{n}`{what} resolves differently\n      before: {b:?}\n      after:  {a:?}")
        })
    }

    /// `na` (or a delegated helper) means what `ob` meant: macro sites, free
    /// names of the body, of `header` and of the macro expansions it reaches,
    /// relative paths, and the traits in scope.
    fn check(&self, ob: &Leaf, na: &Leaf, header: Option<&str>) -> Vec<String> {
        let key = self.map.get(&ob.okey()).cloned().unwrap_or_default();
        let mut errs = Vec::new();
        let (sb, sa) = (&self.macros[0][ob.idx], &self.macros[1][na.idx]);
        if sb.text != sa.text {
            errs.push(format!(
                "{key}: macro resolution changed\n      before: {:?}\n      after:  {:?}",
                sb.text, sa.text
            ));
        }
        let mut bodies = vec![ob.tokens.clone()];
        bodies.extend(header.map(str::to_string));
        let expansions: Vec<String> = sb
            .defs
            .iter()
            .flat_map(|d| scope::macro_defs(&self.before.leaves[*d].tokens))
            .flat_map(|(_, t)| t)
            .collect();
        for (i, t) in bodies.iter().chain(&expansions).enumerate() {
            let what = if i < bodies.len() {
                ""
            } else {
                " in a macro expansion"
            };
            let (names, rb, paths) = scope::free_names(t, &ob.module);
            let (_, ra, _) = scope::free_names(t, &na.module);
            if rb != ra {
                errs.push(format!(
                    "{key}: moved from {} to {} with a `self::`/`super::` path{what}, which now names another module; keep the module depth, or qualify the path in a separate reviewed preparatory commit",
                    ob.module, na.module
                ));
            }
            errs.extend(
                names
                    .iter()
                    .filter_map(|(n, ns)| self.diff(&key, what, ob, na, n, *ns)),
            );
            for p in &paths {
                let ns = scope::TYPE | scope::VALUE;
                let (b, a) = (
                    self.names[0].resolve_path(&ob.module, p, ns),
                    self.names[1].resolve_path(&na.module, p, ns),
                );
                if b != a {
                    errs.push(format!(
                        "{key}: path `{}`{what} resolves differently\n      before: {b:?}\n      after:  {a:?}",
                        p.join("::")
                    ));
                }
            }
        }
        let (tb, ta) = (
            self.names[0].traits(&ob.module),
            self.names[1].traits(&na.module),
        );
        if tb != ta {
            errs.push(format!(
                "{key}: traits in scope changed (method calls may dispatch elsewhere)\n      before: {tb:?}\n      after:  {ta:?}"
            ));
        }
        if ob.module != na.module && (sb.local || !sb.defs.is_empty()) {
            if self.exceptions[0].contains_key(&na.key()) {
                self.used.borrow_mut()[0].insert(na.key());
            } else {
                let what = if sb.local {
                    "defines and invokes a local macro_rules!".to_string()
                } else {
                    let names: BTreeSet<&str> = sb
                        .defs
                        .iter()
                        .map(|d| self.before.leaves[*d].name.as_str())
                        .collect();
                    format!("invokes macro_rules! {names:?}")
                };
                errs.push(format!(
                    "{key}: {what} and moves from {} to {}; expansions resolve at the call site, which is not modelled: keep the module, or list the item under [exceptions] with an audited reason",
                    ob.module, na.module
                ));
            }
        }
        errs
    }

    /// `tokens` of leaf `l` (tree `side`) with std macros' trailing commas
    /// dropped.
    fn commas(&self, side: usize, l: &Leaf, tokens: &str) -> String {
        let mut std = scope::std_macros(&l.tokens, &self.macros[side][l.idx], &self.not_std);
        if !std.is_empty() && self.exposed(side, l) {
            std.clear();
        }
        scope::std_commas(tokens, &std)
    }

    /// `na` (or a delegated helper) runs `line!`/`column!`/`file!`/
    /// `Location::caller` or a tracked fn (in its body or the macros it
    /// reaches) at a new position, or `module_path!`/a log macro without
    /// `target:` in a new module: counted in test code; elsewhere a new
    /// position needs a `[locations]` entry, a new module that is not a
    /// descendant of the old one (same crate) a `[log_targets]` entry, and a
    /// descendant is counted (log filters on the old prefix still match).
    fn location(&self, ob: &Leaf, na: &Leaf) -> Vec<String> {
        let key = self.map.get(&ob.okey()).cloned().unwrap_or_default();
        let at = |l: &Leaf| {
            let v: Vec<(String, usize, usize)> = l
                .sites
                .iter()
                .map(|(n, p)| (n.clone(), p.line, p.column))
                .collect();
            (l.file.clone(), v)
        };
        let (shifted, rehomed) = (at(ob) != at(na), ob.module != na.module);
        if !shifted && !rehomed {
            return Vec::new();
        }
        let texts: Vec<&str> = std::iter::once(ob.tokens.as_str())
            .chain(
                self.macros[0][ob.idx]
                    .defs
                    .iter()
                    .map(|d| self.before.leaves[*d].tokens.as_str()),
            )
            .collect();
        let bound = |n: &str| {
            self.names[0].resolve(&ob.module, n, scope::TYPE | scope::VALUE | scope::MACRO)
        };
        let cx = scope::Idents::new(&self.aliases, &self.tracked, &bound);
        let pos = if shifted {
            texts.iter().find_map(|t| scope::positional(t, &cx))
        } else {
            None
        };
        let modp = rehomed && texts.iter().any(|t| scope::module_sensitive(t, &cx) > 0);
        if scope::is_test(ob) {
            let mut t = self.tests.borrow_mut();
            t[0] += usize::from(pos.is_some());
            t[1] += usize::from(modp);
            return Vec::new();
        }
        let mut errs = Vec::new();
        let place = format!(
            "({}:{} {} → {}:{} {})",
            ob.file, ob.line, ob.module, na.file, na.line, na.module
        );
        if let Some(via) = pos {
            if self.exceptions[1].contains_key(&na.key()) {
                self.used.borrow_mut()[1].insert(na.key());
            } else {
                errs.push(format!(
                    "{key}: production code with line!/column!/file!/Location::caller/a #[track_caller] position (here {via}) moves {place}; the source position it reports changes: list the item under [locations] with an audited reason"
                ));
            }
        }
        if modp {
            let (cb, ca) = (&self.crates[0].name, &self.crates[1].name);
            let full = |c: &str, m: &str| {
                [c, m]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join("::")
            };
            if cb == ca && scope::under(&na.module, &ob.module) {
                self.tests.borrow_mut()[2] += 1;
            } else if self.exceptions[2].contains_key(&na.key()) {
                self.used.borrow_mut()[2].insert(na.key());
            } else {
                errs.push(format!(
                    "{key}: production code with module_path!/log macros without target: moves {place} out of its module's subtree ({} is not under {}); its log target changes: add explicit targets in a separate reviewed commit, or list the item under [log_targets] with an audited reason",
                    full(ca, &na.module),
                    full(cb, &ob.module)
                ));
            }
        }
        errs
    }

    /// The delegated helper's impl names the old impl's self type.
    fn self_ty(&self, old: &Leaf, ty: &str) -> Option<String> {
        let first = syn::parse_str::<syn::TypePath>(ty)
            .ok()
            .filter(|p| p.qself.is_none())
            .and_then(|p| p.path.segments.first().map(|s| s.ident.to_string()));
        let ok = first.is_some_and(|f| {
            let r = self.names[0].resolve(&old.module, &f, scope::TYPE);
            !r.is_empty() && r.iter().all(|x| !x.starts_with("glob "))
        });
        (!ok).then(|| {
            format!(
                "{}: self type `{ty}` of the delegation does not resolve to one item in the tree",
                old.okey()
            )
        })
    }
}

/// Resolved reach of every enclosing module that is not private.
fn mod_chain(t: &Tree, module: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for seg in module.split("::") {
        let parent = cur.clone();
        cur = if cur.is_empty() {
            seg.to_string()
        } else {
            format!("{cur}::{seg}")
        };
        if let Some(m) = t.mods.get(&cur).filter(|m| m.vis != "?") {
            let s = scope(&m.vis, &parent);
            if s != scope("", &parent) {
                out.push(s);
            }
        }
    }
    out
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
