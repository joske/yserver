//! `split apply`: write the items of one file into a module tree per manifest.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use quote::ToTokens;
use serde::Deserialize;
use syn::{Item, ItemImpl, ItemMod};

use crate::tree::{self, Disk, Res, SrcFile, item_keys, item_parts, member_parts, owner, range};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Repo-relative file being split.
    pub source: String,
    /// Crate-relative module path of `source`.
    pub module: String,
    /// Repo-relative directory of the new tree.
    pub dir: String,
    /// Root file: `<dir>/mod.rs` (`dir`, the default) or `<dir>.rs` (`file`).
    #[serde(default)]
    pub root_form: RootForm,
    /// Old → new leaf path table written by `apply`, relative to the manifest.
    pub table: String,
    /// Test binary of `source` as `split test-list` names it (`name:lib`,
    /// `name:bin`, `name:test`), when the Cargo layout does not tell.
    #[serde(default)]
    pub target: Option<String>,
    /// Old item key → new visibility (only `pub(super)` / `pub(in …)`).
    #[serde(default)]
    pub visibility: BTreeMap<String, String>,
    #[serde(default)]
    pub path_edits: Vec<PathEdit>,
    /// Old item key → audited reason it may move macro invocations to
    /// another module (name and trait checks still apply).
    #[serde(default)]
    pub exceptions: BTreeMap<String, String>,
    pub modules: Vec<ModSpec>,
    #[serde(skip)]
    pub path: PathBuf,
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum RootForm {
    #[default]
    Dir,
    File,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModSpec {
    /// Relative to the split module; "" is the new `mod.rs`.
    pub name: String,
    /// Verbatim `use` lines emitted after the `mod` declarations.
    #[serde(default)]
    pub lines: Vec<String>,
    /// Item keys (see `split items`); `*` matches any run of characters.
    #[serde(default)]
    pub items: Vec<String>,
}

/// An include path literal edited by the move; both must resolve to the same
/// bytes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathEdit {
    pub item: String,
    pub from: String,
    pub to: String,
}

impl Manifest {
    pub fn load(path: &Path) -> Res<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut m: Manifest =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        m.path = path.to_path_buf();
        Ok(m)
    }

    pub fn table_path(&self) -> PathBuf {
        self.path
            .parent()
            .unwrap_or(Path::new(""))
            .join(&self.table)
    }

    pub fn new_root(&self) -> String {
        match self.root_form {
            RootForm::Dir => format!("{}/mod.rs", self.dir),
            RootForm::File => format!("{}.rs", self.dir),
        }
    }

    pub fn full(&self, rel: &str) -> String {
        if rel.is_empty() {
            self.module.clone()
        } else {
            tree::child(&self.module, rel)
        }
    }

    fn file_of(&self, name: &str) -> String {
        let has_children = self.modules.iter().any(|m| parent(&m.name) == Some(name));
        let p = name.replace("::", "/");
        match (name.is_empty(), has_children) {
            (true, _) => self.new_root(),
            (false, true) => format!("{}/{p}/mod.rs", self.dir),
            (false, false) => format!("{}/{p}.rs", self.dir),
        }
    }
}

fn parent(name: &str) -> Option<&str> {
    if name.is_empty() {
        None
    } else {
        Some(name.rsplit_once("::").map_or("", |(p, _)| p))
    }
}

pub fn glob(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !s.starts_with(first) || !s[first.len()..].ends_with(last) {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for p in &parts[1..parts.len() - 1] {
        match rest.find(p) {
            Some(i) => rest = &rest[i + p.len()..],
            None => return false,
        }
    }
    true
}

/// Display key of one item relative to its module (no ordinal).
fn base_key(item: &Item) -> String {
    match item {
        Item::Impl(i) => owner(i).header,
        Item::Mod(m) => format!("mod {}", m.ident),
        Item::Use(u) => format!("use {}", tree::tok(&u.tree).replace(' ', "")),
        _ => item_parts(item).map_or_else(|| "?".into(), |p| format!("{} {}", p.kind, p.name)),
    }
}

/// Appends `#n` to keys that occur more than once.
fn ordinals(keys: &mut [String]) {
    let mut total: BTreeMap<String, usize> = BTreeMap::new();
    for k in keys.iter() {
        *total.entry(k.clone()).or_default() += 1;
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for k in keys.iter_mut() {
        if total[k.as_str()] > 1 {
            let n = seen.entry(k.clone()).or_default();
            *n += 1;
            *k = format!("{k}#{n}");
        }
    }
}

struct Unit<'a> {
    key: String,
    /// Old module, relative ("" or a spread inline mod).
    module: String,
    start: usize,
    end: usize,
    blank: bool,
    vis_at: usize,
    depth: usize,
    wrap: Option<usize>,
    item: UnitItem<'a>,
}

enum UnitItem<'a> {
    Item(&'a Item),
    Member(&'a syn::ImplItem, &'a ItemImpl),
}

/// A spread `impl`: header text, and the trailer after its last member.
struct Wrap {
    head: (usize, usize),
    blank: bool,
    trailer: (usize, usize),
    last: usize,
}

/// A spread inline `mod`: becomes a file at the same module path.
struct SpreadMod {
    name: String,
    decl: (usize, usize),
    inner: Vec<(usize, usize)>,
    trailer: (usize, usize),
}

struct Src<'a> {
    f: &'a SrcFile,
    units: Vec<Unit<'a>>,
    wraps: Vec<Wrap>,
    mods: Vec<SpreadMod>,
    header_end: usize,
    trailer: (usize, usize),
    /// File lines inside multi-line tokens (literals): never reindented.
    frozen: BTreeSet<usize>,
}

/// Start of an item's text: its leading comments, from the start of their
/// line; whether a blank line precedes it.
fn trim_gap(text: &str, prev: usize, start: usize) -> (usize, bool) {
    let gap = &text[prev..start];
    let first = gap.find(|c: char| !c.is_whitespace()).unwrap_or(gap.len());
    let line_start = gap[..first].rfind('\n').map_or(0, |i| i + 1);
    let blank = gap[..first].matches('\n').count() >= 2;
    (prev + line_start, blank)
}

fn vis_at(f: &SrcFile, ts: proc_macro2::TokenStream) -> usize {
    ts.into_iter().next().map_or(0, |t| f.off(t.span().start()))
}

fn item_vis_at(f: &SrcFile, item: &Item) -> usize {
    let mut it = item.clone();
    match &mut it {
        Item::Const(i) => i.attrs.clear(),
        Item::Enum(i) => i.attrs.clear(),
        Item::Fn(i) => i.attrs.clear(),
        Item::Static(i) => i.attrs.clear(),
        Item::Struct(i) => i.attrs.clear(),
        Item::Trait(i) => i.attrs.clear(),
        Item::Type(i) => i.attrs.clear(),
        Item::Union(i) => i.attrs.clear(),
        _ => {}
    }
    vis_at(f, it.to_token_stream())
}

fn member_vis_at(f: &SrcFile, item: &syn::ImplItem) -> usize {
    let mut it = item.clone();
    match &mut it {
        syn::ImplItem::Const(i) => i.attrs.clear(),
        syn::ImplItem::Fn(i) => i.attrs.clear(),
        syn::ImplItem::Type(i) => i.attrs.clear(),
        _ => {}
    }
    vis_at(f, it.to_token_stream())
}

impl<'a> Src<'a> {
    fn build(f: &'a SrcFile, ast: &'a syn::File, m: &Manifest) -> Res<Self> {
        let header_end = ast
            .attrs
            .iter()
            .map(|a| range(f, a.to_token_stream()).1)
            .max()
            .unwrap_or(0);
        let mut s = Src {
            f,
            units: Vec::new(),
            wraps: Vec::new(),
            mods: Vec::new(),
            header_end,
            trailer: (0, 0),
            frozen: BTreeSet::new(),
        };
        let mut v = Vec::new();
        tree::spans(f.text.parse().map_err(|e| format!("lex: {e}"))?, &mut v);
        for (sp, multi) in v {
            if multi {
                s.frozen.extend(sp.start().line + 1..=sp.end().line);
            }
        }
        let pats: Vec<&str> = m
            .modules
            .iter()
            .flat_map(|x| x.items.iter().map(String::as_str))
            .collect();
        let hits = |k: &str| pats.iter().any(|p| glob(p, k));
        let last = s.items(&ast.items, "", header_end, &hits)?;
        s.trailer = (last, f.text.len());
        let mut keys: Vec<String> = s.units.iter().map(|u| u.key.clone()).collect();
        ordinals(&mut keys);
        for (u, k) in s.units.iter_mut().zip(keys) {
            u.key = k;
        }
        Ok(s)
    }

    /// Adds the units of `items`; returns the end of the last item.
    fn items(
        &mut self,
        items: &'a [Item],
        module: &str,
        mut prev: usize,
        hits: &dyn Fn(&str) -> bool,
    ) -> Res<usize> {
        let f = self.f;
        let prefix = if module.is_empty() {
            String::new()
        } else {
            format!("{module}::")
        };
        let depth = usize::from(!module.is_empty());
        let mut whole: Vec<String> = items
            .iter()
            .map(|i| format!("{prefix}{}", base_key(i)))
            .collect();
        ordinals(&mut whole);
        for (item, wkey) in items.iter().zip(whole) {
            let (s, e) = range(f, item.to_token_stream());
            let (start, blank) = trim_gap(&f.text, prev, s);
            match item {
                Item::Impl(i)
                    if module.is_empty() && i.items.iter().any(|ii| hits(&member_key(i, ii))) =>
                {
                    if hits(&wkey) {
                        return Err(format!("{wkey}: assigned both whole and per member"));
                    }
                    let open = f.off(i.brace_token.span.open().end());
                    let close = f.off(i.brace_token.span.close().end());
                    let w = self.wraps.len();
                    self.wraps.push(Wrap {
                        head: (start, open),
                        blank,
                        trailer: (0, close),
                        last: 0,
                    });
                    let mut p = open;
                    for ii in &i.items {
                        let (ms, me) = range(f, ii.to_token_stream());
                        let (mstart, mblank) = trim_gap(&f.text, p, ms);
                        self.units.push(Unit {
                            key: member_key(i, ii),
                            module: String::new(),
                            start: mstart,
                            end: me,
                            blank: mblank,
                            vis_at: member_vis_at(f, ii),
                            depth: 1,
                            wrap: Some(w),
                            item: UnitItem::Member(ii, i),
                        });
                        p = me;
                    }
                    self.wraps[w].trailer.0 = p;
                    self.wraps[w].last = self.units.len() - 1;
                }
                Item::Mod(
                    md @ ItemMod {
                        content: Some((brace, inner)),
                        ..
                    },
                ) if module.is_empty() && items_hit(inner, &format!("{}::", md.ident), hits) => {
                    if hits(&wkey) {
                        return Err(format!("{wkey}: assigned both whole and per member"));
                    }
                    let open = f.off(brace.span.open().end());
                    let inner_attrs = md
                        .attrs
                        .iter()
                        .filter(|a| matches!(a.style, syn::AttrStyle::Inner(_)))
                        .map(|a| range(f, a.to_token_stream()))
                        .collect::<Vec<_>>();
                    let body = inner_attrs.iter().map(|r| r.1).max().unwrap_or(open);
                    let last = self.items(inner, &md.ident.to_string(), body, hits)?;
                    self.mods.push(SpreadMod {
                        name: md.ident.to_string(),
                        decl: (start, f.off(md.ident.span().end())),
                        inner: inner_attrs,
                        trailer: (last, f.off(brace.span.close().start())),
                    });
                }
                _ => self.units.push(Unit {
                    key: wkey,
                    module: module.to_string(),
                    start,
                    end: e,
                    blank,
                    vis_at: item_vis_at(f, item),
                    depth,
                    wrap: None,
                    item: UnitItem::Item(item),
                }),
            }
            prev = e;
        }
        Ok(prev)
    }

    /// Unit text, with an inserted visibility and reindented to `depth`.
    fn text(&self, u: &Unit, vis: Option<&str>, depth: usize) -> String {
        let t = &self.f.text;
        let mut s = match vis {
            Some(v) => format!("{}{v} {}", &t[u.start..u.vis_at], &t[u.vis_at..u.end]),
            None => t[u.start..u.end].to_string(),
        };
        if depth != u.depth {
            let first = self.f.line_of(u.start);
            s = s
                .split('\n')
                .enumerate()
                .map(|(k, line)| {
                    if self.frozen.contains(&(first + k)) || line.trim().is_empty() {
                        line.to_string()
                    } else if depth < u.depth {
                        let n = 4 * (u.depth - depth);
                        let lead = line.len() - line.trim_start_matches(' ').len();
                        line[lead.min(n)..].to_string()
                    } else {
                        format!("{}{line}", " ".repeat(4 * (depth - u.depth)))
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        s
    }
}

/// `text` with the manifest's path edits for `key` applied to
/// `include_str!`/`include_bytes!` arguments and `#[path]` values.
fn edited(m: &Manifest, key: &str, text: String, done: &mut [usize]) -> Res<String> {
    let mut text = text;
    for (k, e) in m
        .path_edits
        .iter()
        .enumerate()
        .filter(|(_, e)| e.item == key)
    {
        let f = SrcFile::new(text.clone());
        let ts: proc_macro2::TokenStream = f.text.parse().map_err(|e| format!("{key}: {e}"))?;
        let mut spans = Vec::new();
        path_literals(ts, &e.from, &mut spans);
        let mut ranges: Vec<(usize, usize)> = spans
            .iter()
            .map(|s| (f.off(s.start()), f.off(s.end())))
            .collect();
        ranges.sort_unstable();
        for (a, b) in ranges.into_iter().rev() {
            text.replace_range(a..b, &format!("{:?}", e.to));
            done[k] += 1;
        }
    }
    Ok(text)
}

fn path_literals(ts: proc_macro2::TokenStream, from: &str, out: &mut Vec<proc_macro2::Span>) {
    use proc_macro2::TokenTree as T;
    let v: Vec<T> = ts.into_iter().collect();
    let lit = |t: Option<&T>| match t {
        Some(T::Literal(l)) => syn::parse_str::<syn::LitStr>(&l.to_string())
            .ok()
            .filter(|s| s.value() == from)
            .map(|_| l.span()),
        _ => None,
    };
    let punct = |t: Option<&T>, c: char| matches!(t, Some(T::Punct(p)) if p.as_char() == c);
    for (i, t) in v.iter().enumerate() {
        match t {
            T::Ident(id)
                if (id == "include_str" || id == "include_bytes") && punct(v.get(i + 1), '!') =>
            {
                if let Some(T::Group(g)) = v.get(i + 2) {
                    let inner: Vec<T> = g.stream().into_iter().collect();
                    if inner.len() == 1
                        && let Some(s) = lit(inner.first())
                    {
                        out.push(s);
                    }
                }
            }
            T::Group(g) => {
                let inner: Vec<T> = g.stream().into_iter().collect();
                let attr = g.delimiter() == proc_macro2::Delimiter::Bracket
                    && i > 0
                    && punct(v.get(i - 1), '#');
                match inner.as_slice() {
                    [T::Ident(id), eq, l] if attr && id == "path" && punct(Some(eq), '=') => {
                        out.extend(lit(Some(l)));
                    }
                    _ => path_literals(g.stream(), from, out),
                }
            }
            _ => {}
        }
    }
}

fn member_key(i: &ItemImpl, ii: &syn::ImplItem) -> String {
    let p = member_parts(ii);
    format!("{}::{} {}", owner(i).header, p.kind, p.name)
}

fn items_hit(items: &[Item], prefix: &str, hits: &dyn Fn(&str) -> bool) -> bool {
    items
        .iter()
        .any(|i| hits(&format!("{prefix}{}", base_key(i))))
}

/// `split items <file>`: the keys a manifest can assign.
pub fn list(path: &str) -> Res<()> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let f = SrcFile::new(text);
    let ast = syn::parse_file(&f.text).map_err(|e| format!("{path}: {e}"))?;
    let show = |indent: &str, key: &str, ts: proc_macro2::TokenStream| {
        let (s, e) = range(&f, ts);
        let (a, b) = (f.line_of(s), f.line_of(e));
        println!("{indent}{key}  L{a}-{b} ({})", b - a + 1);
    };
    let mut keys: Vec<String> = ast.items.iter().map(base_key).collect();
    ordinals(&mut keys);
    for (item, key) in ast.items.iter().zip(keys) {
        show("", &key, item.to_token_stream());
        match item {
            Item::Impl(i) => {
                for ii in &i.items {
                    show("    ", &member_key(i, ii), ii.to_token_stream());
                }
            }
            Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    let mut k: Vec<String> = inner
                        .iter()
                        .map(|i| format!("{}::{}", m.ident, base_key(i)))
                        .collect();
                    ordinals(&mut k);
                    for (i, key) in inner.iter().zip(k) {
                        show("    ", &key, i.to_token_stream());
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn run(m: &Manifest, repo: &Path) -> Res<()> {
    let text =
        std::fs::read_to_string(repo.join(&m.source)).map_err(|e| format!("{}: {e}", m.source))?;
    let f = SrcFile::new(text);
    let ast = syn::parse_file(&f.text).map_err(|e| format!("{}: {e}", m.source))?;
    let src = Src::build(&f, &ast, m)?;

    let names: BTreeSet<&str> = m.modules.iter().map(|x| x.name.as_str()).collect();
    if !names.contains("") {
        return Err("manifest needs a root module (name = \"\")".into());
    }
    for n in &names {
        if let Some(p) = parent(n)
            && !names.contains(p)
        {
            return Err(format!("module {n}: parent {p:?} is not in the manifest"));
        }
    }
    for sm in &src.mods {
        if !names.contains(sm.name.as_str()) {
            return Err(format!(
                "inline mod {} is spread but has no module entry",
                sm.name
            ));
        }
    }

    // Assignment: an exact key wins over globs; anything else must match once.
    let mut target: Vec<&str> = Vec::new();
    let mut errs = Vec::new();
    let mut used: BTreeSet<(&str, &str)> = BTreeSet::new();
    for u in &src.units {
        let exact: Vec<&ModSpec> = m
            .modules
            .iter()
            .filter(|x| x.items.contains(&u.key))
            .collect();
        let cands: Vec<(&ModSpec, &str)> = if exact.is_empty() {
            m.modules
                .iter()
                .flat_map(|x| {
                    x.items
                        .iter()
                        .filter(|p| glob(p, &u.key))
                        .map(move |p| (x, p.as_str()))
                })
                .collect()
        } else {
            exact.iter().map(|x| (*x, u.key.as_str())).collect()
        };
        let mods: BTreeSet<&str> = cands.iter().map(|(x, _)| x.name.as_str()).collect();
        match mods.len() {
            0 => errs.push(format!("unassigned: {}", u.key)),
            1 => {
                let t = *mods.iter().next().expect("one");
                let inside = t == u.module || t.starts_with(&format!("{}::", u.module));
                let into_spread = u.module.is_empty()
                    && src
                        .mods
                        .iter()
                        .any(|sm| t == sm.name || t.starts_with(&format!("{}::", sm.name)));
                if !(u.module.is_empty() || inside) || into_spread {
                    errs.push(format!(
                        "{}: cannot move from module {:?} to {t:?}",
                        u.key, u.module
                    ));
                }
                target.push(t);
                used.extend(cands.iter().map(|(x, p)| (x.name.as_str(), *p)));
            }
            _ => errs.push(format!("ambiguous: {} matches modules {mods:?}", u.key)),
        }
    }
    for x in &m.modules {
        for p in &x.items {
            if !used.contains(&(x.name.as_str(), p.as_str())) {
                errs.push(format!(
                    "manifest entry matches nothing: {:?} in {:?}",
                    p, x.name
                ));
            }
        }
    }
    for k in m.visibility.keys() {
        if !src.units.iter().any(|u| &u.key == k) {
            errs.push(format!("visibility entry matches no item: {k}"));
        }
    }
    if !errs.is_empty() {
        return Err(errs.join("\n"));
    }

    // Emit each module file.
    let t = &f.text;
    let mut written = Vec::new();
    let mut edits_done = vec![0; m.path_edits.len()];
    for spec in &m.modules {
        let mut out = String::new();
        let push = |out: &mut String, s: &str, blank: bool| {
            if !out.is_empty() {
                out.push_str(if blank { "\n\n" } else { "\n" });
            }
            out.push_str(s);
        };
        let spread = src.mods.iter().find(|sm| sm.name == spec.name);
        if spec.name.is_empty() && src.header_end > 0 {
            push(&mut out, t[..src.header_end].trim_end(), false);
        }
        if let Some(sm) = spread {
            for (a, b) in &sm.inner {
                push(&mut out, t[*a..*b].trim(), false);
            }
        }
        let mut first = true;
        for child in m
            .modules
            .iter()
            .filter(|x| parent(&x.name) == Some(&spec.name))
        {
            let leaf = child.name.rsplit("::").next().unwrap_or(&child.name);
            let decl = match src.mods.iter().find(|sm| sm.name == child.name) {
                Some(sm) => format!("{};", &t[sm.decl.0..sm.decl.1]),
                None => format!("mod {leaf};"),
            };
            push(&mut out, &decl, first);
            first = false;
        }
        let mut first = true;
        for l in &spec.lines {
            push(&mut out, l, first);
            first = false;
        }
        let mine: Vec<usize> = (0..src.units.len())
            .filter(|&i| target[i] == spec.name)
            .collect();
        for (n, &i) in mine.iter().enumerate() {
            let u = &src.units[i];
            let opens = u.wrap.is_some() && (n == 0 || src.units[mine[n - 1]].wrap != u.wrap);
            if let Some(w) = u.wrap.filter(|_| opens) {
                let wr = &src.wraps[w];
                push(&mut out, &t[wr.head.0..wr.head.1], wr.blank || n > 0);
                out.push('\n');
                out.push_str(&edited(
                    m,
                    &u.key,
                    src.text(u, m.visibility.get(&u.key).map(String::as_str), 1),
                    &mut edits_done,
                )?);
            } else {
                let depth = usize::from(u.wrap.is_some());
                let body = edited(
                    m,
                    &u.key,
                    src.text(u, m.visibility.get(&u.key).map(String::as_str), depth),
                    &mut edits_done,
                )?;
                push(&mut out, &body, u.blank);
            }
            if let Some(w) = u.wrap
                && (n + 1 == mine.len() || src.units[mine[n + 1]].wrap != Some(w))
            {
                let wr = &src.wraps[w];
                if i == wr.last {
                    out.push_str(&t[wr.trailer.0..wr.trailer.1]);
                } else {
                    out.push_str("\n}");
                }
            }
        }
        if let Some(sm) = spread {
            let tr = t[sm.trailer.0..sm.trailer.1].trim();
            if !tr.is_empty() {
                push(&mut out, tr, true);
            }
        }
        if spec.name.is_empty() {
            let tr = t[src.trailer.0..src.trailer.1].trim();
            if !tr.is_empty() {
                push(&mut out, tr, true);
            }
        }
        out.push('\n');
        written.push((m.file_of(&spec.name), out));
    }
    for (e, n) in m.path_edits.iter().zip(&edits_done) {
        if *n == 0 {
            return Err(format!(
                "path edit {:?} → {:?} matches no include or #[path] in {}",
                e.from, e.to, e.item
            ));
        }
    }
    for (u, t) in src.units.iter().zip(&target) {
        if let UnitItem::Item(Item::Mod(md @ ItemMod { content: None, .. })) = u.item {
            let file = |dir: &tree::Dir, attrs: &[syn::Attribute]| {
                dir.out_of_line(&md.ident.to_string(), tree::path_attr(attrs).as_deref())
                    .into_iter()
                    .map(|(p, _)| p)
                    .find(|p| repo.join(p).exists())
            };
            let old_dir = tree::Dir::of_file(&m.source, tree::Dir::root_rel(&m.source, &m.module));
            let old_dir = match u.module.as_str() {
                "" => old_dir,
                sm => old_dir.inline(sm, None),
            };
            let new_path = m.file_of(t);
            let new_dir = tree::Dir::of_file(&new_path, tree::Dir::root_rel(&new_path, "x"));
            let text = edited(
                m,
                &u.key,
                f.text[u.start..u.end].to_string(),
                &mut vec![0; m.path_edits.len()],
            )?;
            let new_attrs = syn::parse_str::<ItemMod>(text.trim())
                .map(|x| x.attrs)
                .unwrap_or_default();
            let (a, b) = (file(&old_dir, &md.attrs), file(&new_dir, &new_attrs));
            if a.is_none() || a != b {
                return Err(format!(
                    "{}: loads {a:?} from {}, would load {b:?} from {new_path}; add a path edit",
                    u.key, m.source
                ));
            }
        }
    }
    for (path, _) in &written {
        if repo.join(path).exists() && path != &m.source {
            return Err(format!("{path} already exists"));
        }
    }

    // Path table: every leaf of the source, old key → new key.
    let old = tree::load(&Disk(repo.to_path_buf()), &m.source, &m.module)?;
    let mut pairs = Vec::new();
    for (u, t) in src.units.iter().zip(&target) {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        match u.item {
            UnitItem::Item(Item::Mod(md @ ItemMod { content: None, .. })) => {
                let (from, to) = (
                    tree::child(&m.full(&u.module), &md.ident.to_string()),
                    tree::child(&m.full(t), &md.ident.to_string()),
                );
                for l in old
                    .leaves
                    .iter()
                    .filter(|l| l.module == from || l.module.starts_with(&format!("{from}::")))
                {
                    a.push(l.key());
                    b.push(format!("{to}{}", &l.key()[from.len()..]));
                }
            }
            UnitItem::Item(item) => {
                item_keys(item, &m.full(&u.module), &mut a);
                item_keys(item, &m.full(t), &mut b);
            }
            UnitItem::Member(ii, i) => {
                let p = member_parts(ii);
                let h = owner(i).header;
                a.push(tree::key(&m.full(""), Some(&h), p.kind, &p.name));
                b.push(tree::key(&m.full(t), Some(&h), p.kind, &p.name));
            }
        }
        pairs.extend(a.into_iter().zip(b));
    }
    if pairs.len() != old.leaves.len() {
        return Err(format!(
            "internal: {} unit leaves vs {} tree leaves",
            pairs.len(),
            old.leaves.len()
        ));
    }
    let mut table = String::new();
    for (l, (a, b)) in old.leaves.iter().zip(pairs) {
        if l.key() != a {
            return Err(format!("internal: leaf order {} vs {a}", l.key()));
        }
        table.push_str(&format!("{} => {b}\n", l.okey()));
    }

    std::fs::remove_file(repo.join(&m.source)).map_err(|e| format!("{}: {e}", m.source))?;
    for (path, text) in &written {
        let p = repo.join(path);
        std::fs::create_dir_all(p.parent().expect("dir")).map_err(|e| e.to_string())?;
        std::fs::write(&p, text).map_err(|e| format!("{path}: {e}"))?;
        println!("{path}: {} lines", text.lines().count());
    }
    std::fs::write(m.table_path(), table).map_err(|e| e.to_string())?;
    println!("{}: {} leaves", m.table_path().display(), old.leaves.len());
    Ok(())
}
