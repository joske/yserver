//! Source model shared by `apply` and `verify`: files, comments, leaf items.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::Command,
};

use proc_macro2::{LineColumn, Span, TokenStream, TokenTree};
use quote::ToTokens;
use syn::{
    Attribute, ImplItem, ImplItemFn, Item, ItemImpl, Visibility,
    punctuated::Punctuated,
    visit_mut::{self, VisitMut},
};

pub type Res<T> = Result<T, String>;

pub struct SrcFile {
    pub text: String,
    lines: Vec<usize>,
}

impl SrcFile {
    pub fn new(text: String) -> Self {
        let mut lines = vec![0];
        lines.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Self { text, lines }
    }

    /// Byte offset of a span position (`column` counts chars).
    pub fn off(&self, lc: LineColumn) -> usize {
        let start = self.lines[lc.line - 1];
        self.text[start..]
            .char_indices()
            .nth(lc.column)
            .map_or(self.text.len(), |(i, _)| start + i)
    }

    /// 1-based line of a byte offset.
    pub fn line_of(&self, off: usize) -> usize {
        self.lines.partition_point(|&s| s <= off)
    }
}

/// Every token span, group delimiters included. The second field is true for
/// a single token that spans lines (string literal, block doc comment).
pub fn spans(ts: TokenStream, out: &mut Vec<(Span, bool)>) {
    for tt in ts {
        match tt {
            TokenTree::Group(g) => {
                out.push((g.span_open(), false));
                out.push((g.span_close(), false));
                spans(g.stream(), out);
            }
            t => {
                let s = t.span();
                out.push((s, s.start().line < s.end().line));
            }
        }
    }
}

pub fn range(f: &SrcFile, ts: TokenStream) -> (usize, usize) {
    let mut v = Vec::new();
    spans(ts, &mut v);
    let s = v.iter().map(|(s, _)| f.off(s.start())).min().unwrap_or(0);
    let e = v.iter().map(|(s, _)| f.off(s.end())).max().unwrap_or(0);
    (s, e)
}

pub struct Comment {
    pub at: usize,
    pub text: String,
}

pub fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Plain comments are the non-whitespace text between tokens; one entry per
/// gap, whitespace collapsed.
pub fn comments(f: &SrcFile) -> Res<Vec<Comment>> {
    let ts: TokenStream = f.text.parse().map_err(|e| format!("lex: {e}"))?;
    let mut v = Vec::new();
    spans(ts, &mut v);
    let mut iv: Vec<(usize, usize)> = v
        .iter()
        .map(|(s, _)| (f.off(s.start()), f.off(s.end())))
        .collect();
    iv.sort_unstable();
    iv.push((f.text.len(), f.text.len()));
    let mut out = Vec::new();
    let mut cur = 0;
    for (s, e) in iv {
        if s > cur {
            let text = norm(&f.text[cur..s]);
            if !text.is_empty() {
                out.push(Comment { at: cur, text });
            }
        }
        cur = cur.max(e);
    }
    Ok(out)
}

pub fn tok(t: &impl ToTokens) -> String {
    t.to_token_stream().to_string()
}

/// Drops the trailing commas rustfmt adds or removes with line width, only in
/// syntax where they carry no meaning; 1-tuples, macro and attribute tokens
/// are left alone.
pub struct Commas;

fn trim<T, P>(p: &mut Punctuated<T, P>) {
    p.pop_punct();
}

fn trim_multi<T, P>(p: &mut Punctuated<T, P>) {
    if p.len() > 1 {
        p.pop_punct();
    }
}

macro_rules! trimmed {
    ($($visit:ident($ty:ty) $how:ident $($field:ident).+;)*) => {
        $(fn $visit(&mut self, i: &mut $ty) {
            $how(&mut i.$($field).+);
            visit_mut::$visit(self, i);
        })*
    };
}

impl VisitMut for Commas {
    trimmed! {
        visit_fields_named_mut(syn::FieldsNamed) trim named;
        visit_fields_unnamed_mut(syn::FieldsUnnamed) trim unnamed;
        visit_item_enum_mut(syn::ItemEnum) trim variants;
        visit_generics_mut(syn::Generics) trim params;
        visit_where_clause_mut(syn::WhereClause) trim predicates;
        visit_angle_bracketed_generic_arguments_mut(syn::AngleBracketedGenericArguments) trim args;
        visit_parenthesized_generic_arguments_mut(syn::ParenthesizedGenericArguments) trim inputs;
        visit_bound_lifetimes_mut(syn::BoundLifetimes) trim lifetimes;
        visit_expr_call_mut(syn::ExprCall) trim args;
        visit_expr_method_call_mut(syn::ExprMethodCall) trim args;
        visit_expr_array_mut(syn::ExprArray) trim elems;
        visit_expr_tuple_mut(syn::ExprTuple) trim_multi elems;
        visit_expr_struct_mut(syn::ExprStruct) trim fields;
        visit_expr_closure_mut(syn::ExprClosure) trim inputs;
        visit_pat_tuple_mut(syn::PatTuple) trim_multi elems;
        visit_pat_tuple_struct_mut(syn::PatTupleStruct) trim elems;
        visit_pat_struct_mut(syn::PatStruct) trim fields;
        visit_pat_slice_mut(syn::PatSlice) trim elems;
        visit_type_tuple_mut(syn::TypeTuple) trim_multi elems;
        visit_use_group_mut(syn::UseGroup) trim items;
    }

    fn visit_signature_mut(&mut self, i: &mut syn::Signature) {
        if i.variadic.is_none() {
            trim(&mut i.inputs);
        }
        visit_mut::visit_signature_mut(self, i);
    }

    fn visit_type_bare_fn_mut(&mut self, i: &mut syn::TypeBareFn) {
        if i.variadic.is_none() {
            trim(&mut i.inputs);
        }
        visit_mut::visit_type_bare_fn_mut(self, i);
    }

    fn visit_arm_mut(&mut self, i: &mut syn::Arm) {
        i.comma = None;
        visit_mut::visit_arm_mut(self, i);
    }

    fn visit_attribute_mut(&mut self, _: &mut Attribute) {}
}

/// Attributes that change meaning (cfg, lints, derives…): everything but
/// docs and `#[path]`, style-independent.
pub fn sem_attrs(attrs: &[Attribute]) -> impl Iterator<Item = String> + '_ {
    attrs
        .iter()
        .filter(|a| !a.path().is_ident("doc") && !a.path().is_ident("path"))
        .map(|a| tok(&a.meta))
}

pub fn doc_attrs(attrs: &[Attribute]) -> impl Iterator<Item = String> + '_ {
    attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .map(|a| tok(&a.meta))
}

fn take_vis(v: &mut Visibility) -> String {
    let s = tok(v);
    *v = Visibility::Inherited;
    s
}

pub struct Parts {
    pub kind: &'static str,
    pub name: String,
    pub vis: String,
    /// Tokens of the item with its visibility removed.
    pub tokens: String,
}

/// A module-level leaf; `None` for wrappers (`mod`, `impl`) and `use`.
pub fn item_parts(item: &Item) -> Option<Parts> {
    let mut it = item.clone();
    let (kind, name, vis) = match &mut it {
        Item::Fn(i) => ("fn", i.sig.ident.to_string(), take_vis(&mut i.vis)),
        Item::Const(i) => ("const", i.ident.to_string(), take_vis(&mut i.vis)),
        Item::Static(i) => ("static", i.ident.to_string(), take_vis(&mut i.vis)),
        Item::Type(i) => ("type", i.ident.to_string(), take_vis(&mut i.vis)),
        Item::Struct(i) => ("struct", i.ident.to_string(), take_vis(&mut i.vis)),
        Item::Enum(i) => ("enum", i.ident.to_string(), take_vis(&mut i.vis)),
        Item::Union(i) => ("union", i.ident.to_string(), take_vis(&mut i.vis)),
        Item::Trait(i) => ("trait", i.ident.to_string(), take_vis(&mut i.vis)),
        Item::TraitAlias(i) => ("trait", i.ident.to_string(), take_vis(&mut i.vis)),
        Item::ExternCrate(i) => ("extern_crate", i.ident.to_string(), take_vis(&mut i.vis)),
        Item::Macro(i) => match &i.ident {
            Some(id) => ("macro", id.to_string(), String::new()),
            None => ("macro_call", norm(&tok(&i.mac.path)), String::new()),
        },
        Item::ForeignMod(i) => ("extern", norm(&tok(&i.abi)), String::new()),
        Item::Mod(_) | Item::Impl(_) | Item::Use(_) => return None,
        _ => ("verbatim", String::new(), String::new()),
    };
    Some(Parts {
        kind,
        name,
        vis,
        tokens: {
            Commas.visit_item_mut(&mut it);
            tok(&it)
        },
    })
}

pub fn member_parts(item: &ImplItem) -> Parts {
    let mut it = item.clone();
    let (kind, name, vis) = match &mut it {
        ImplItem::Fn(i) => ("fn", i.sig.ident.to_string(), take_vis(&mut i.vis)),
        ImplItem::Const(i) => ("const", i.ident.to_string(), take_vis(&mut i.vis)),
        ImplItem::Type(i) => ("type", i.ident.to_string(), take_vis(&mut i.vis)),
        ImplItem::Macro(i) => ("macro_call", norm(&tok(&i.mac.path)), String::new()),
        _ => ("verbatim", String::new(), String::new()),
    };
    Commas.visit_impl_item_mut(&mut it);
    Parts {
        kind,
        name,
        vis,
        tokens: tok(&it),
    }
}

#[derive(Clone)]
pub struct Owner {
    pub header: String,
    pub self_ty: String,
    pub is_trait: bool,
}

pub fn owner(i: &ItemImpl) -> Owner {
    let mut h = i.clone();
    h.attrs.clear();
    h.items.clear();
    Commas.visit_item_impl_mut(&mut h);
    let header = tok(&h).trim_end_matches(['{', '}', ' ']).to_string();
    Owner {
        header,
        self_ty: tok(&i.self_ty),
        is_trait: i.trait_.is_some(),
    }
}

pub fn key(module: &str, owner: Option<&str>, kind: &str, name: &str) -> String {
    match owner {
        Some(o) => format!("{module}::{o}::{kind} {name}"),
        None => format!("{module}::{kind} {name}"),
    }
}

/// Leaf keys of one item, in walk order (inline mods and impls expanded).
pub fn item_keys(item: &Item, module: &str, out: &mut Vec<String>) {
    match item {
        Item::Mod(m) => {
            if let Some((_, inner)) = &m.content {
                let sub = format!("{module}::{}", m.ident);
                for i in inner {
                    item_keys(i, &sub, out);
                }
            }
        }
        Item::Impl(i) => {
            let o = owner(i);
            for ii in &i.items {
                let p = member_parts(ii);
                out.push(key(module, Some(&o.header), p.kind, &p.name));
            }
        }
        _ => {
            if let Some(p) = item_parts(item) {
                out.push(key(module, None, p.kind, &p.name));
            }
        }
    }
}

pub struct Leaf {
    pub module: String,
    pub owner: Option<Owner>,
    pub kind: &'static str,
    pub name: String,
    pub vis: String,
    pub tokens: String,
    /// Semantic attributes of every enclosing wrapper (effective cfg etc.).
    pub ctx: BTreeSet<String>,
    pub comments: Vec<String>,
    pub file: String,
    pub line: usize,
    /// Ordinal among leaves with the same key, when the key is not unique.
    pub ord: Option<usize>,
    pub func: Option<ImplItemFn>,
    /// Index in `Tree::leaves`.
    pub idx: usize,
}

impl Leaf {
    pub fn key(&self) -> String {
        key(
            &self.module,
            self.owner.as_ref().map(|o| o.header.as_str()),
            self.kind,
            &self.name,
        )
    }

    pub fn okey(&self) -> String {
        match self.ord {
            Some(n) => format!("{}#{n}", self.key()),
            None => self.key(),
        }
    }
}

/// One flattened `use` path: `a::{b, c as d}` gives `a::b` and `a::c as d`.
pub struct UseItem {
    pub module: String,
    pub vis: String,
    pub attrs: String,
    pub path: String,
    /// Bound name; `None` for globs and `_`.
    pub name: Option<String>,
}

impl UseItem {
    pub fn text(&self) -> String {
        format!("{}{} use {}", self.attrs, self.vis, self.path)
            .trim_start()
            .to_string()
    }
}

pub fn flatten_use(u: &syn::ItemUse, module: &str, out: &mut Vec<UseItem>) {
    fn walk(t: &syn::UseTree, prefix: &str, out: &mut Vec<(String, Option<String>)>) {
        match t {
            syn::UseTree::Path(p) => walk(&p.tree, &format!("{prefix}{} :: ", p.ident), out),
            syn::UseTree::Name(n) if n.ident == "self" => {
                let path = prefix.trim_end_matches(" :: ").to_string();
                let name = path.rsplit(" :: ").next().map(str::to_string);
                out.push((path, name));
            }
            syn::UseTree::Name(n) => {
                out.push((format!("{prefix}{}", n.ident), Some(n.ident.to_string())))
            }
            syn::UseTree::Rename(r) => out.push((
                format!("{prefix}{} as {}", r.ident, r.rename),
                (r.rename != "_").then(|| r.rename.to_string()),
            )),
            syn::UseTree::Glob(_) => out.push((format!("{prefix}*"), None)),
            syn::UseTree::Group(g) => g.items.iter().for_each(|i| walk(i, prefix, out)),
        }
    }
    let mut v = Vec::new();
    let lead = if u.leading_colon.is_some() { ":: " } else { "" };
    walk(&u.tree, lead, &mut v);
    let attrs: String = sem_attrs(&u.attrs).map(|a| format!("#[{a}] ")).collect();
    for (path, name) in v {
        out.push(UseItem {
            module: module.to_string(),
            vis: tok(&u.vis),
            attrs: attrs.clone(),
            path,
            name,
        });
    }
}

/// Textual walk order: module entry (with `#[macro_use]`), exit, leaf.
pub enum Ev {
    Enter(bool),
    Exit,
    Leaf(usize),
}

pub struct ModInfo {
    /// Visibility tokens of the declaration; `?` for the root.
    pub vis: String,
}

pub trait Source {
    fn read(&self, path: &str) -> Option<Vec<u8>>;
}

pub struct Disk(pub PathBuf);

impl Source for Disk {
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        std::fs::read(self.0.join(path)).ok()
    }
}

pub struct Git(pub String);

impl Source for Git {
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        let out = Command::new("git")
            .args(["show", &format!("{}:{path}", self.0)])
            .output()
            .ok()?;
        out.status.success().then_some(out.stdout)
    }
}

pub fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

/// Directory holding the out-of-line children of the module in `path`.
pub fn child_dir(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    if matches!(file, "mod.rs" | "lib.rs" | "main.rs") {
        dir_of(path).to_string()
    } else {
        path.trim_end_matches(".rs").to_string()
    }
}

pub fn join(dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn path_attr(attrs: &[Attribute]) -> Option<String> {
    attrs
        .iter()
        .find(|a| a.path().is_ident("path"))
        .and_then(|a| {
            let syn::Meta::NameValue(nv) = &a.meta else {
                return None;
            };
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            else {
                return None;
            };
            Some(s.value())
        })
}

#[derive(Default)]
pub struct Tree {
    pub leaves: Vec<Leaf>,
    pub uses: Vec<UseItem>,
    pub mods: BTreeMap<String, ModInfo>,
    pub events: Vec<Ev>,
    /// Every comment and wrapper doc attribute in the tree.
    pub pool: Vec<String>,
    pub files: Vec<String>,
}

struct FileCx<'a> {
    src: &'a dyn Source,
    path: &'a str,
    f: &'a SrcFile,
    cm: &'a [Comment],
}

/// Loads the module tree rooted at `root` (module path `module`), following
/// `mod x;` declarations.
pub fn load(src: &dyn Source, root: &str, module: &str) -> Res<Tree> {
    let mut t = Tree::default();
    t.mods
        .insert(module.to_string(), ModInfo { vis: "?".into() });
    t.file(src, root, module, &BTreeSet::new())?;
    let mut total: BTreeMap<String, usize> = BTreeMap::new();
    for l in &t.leaves {
        *total.entry(l.key()).or_default() += 1;
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for l in &mut t.leaves {
        let k = l.key();
        if total[&k] > 1 {
            let n = seen.entry(k).or_default();
            *n += 1;
            l.ord = Some(*n);
        }
    }
    Ok(t)
}

impl Tree {
    fn file(
        &mut self,
        src: &dyn Source,
        path: &str,
        module: &str,
        ctx: &BTreeSet<String>,
    ) -> Res<()> {
        let bytes = src.read(path).ok_or_else(|| format!("{path}: not found"))?;
        let f = SrcFile::new(String::from_utf8(bytes).map_err(|e| format!("{path}: {e}"))?);
        let ast = syn::parse_file(&f.text).map_err(|e| format!("{path}: {e}"))?;
        let cm = comments(&f)?;
        self.pool.extend(cm.iter().map(|c| c.text.clone()));
        self.pool.extend(doc_attrs(&ast.attrs));
        let mut ctx = ctx.clone();
        ctx.extend(sem_attrs(&ast.attrs));
        let start = ast
            .attrs
            .iter()
            .map(|a| range(&f, a.to_token_stream()).1)
            .max()
            .unwrap_or(0);
        let cx = FileCx {
            src,
            path,
            f: &f,
            cm: &cm,
        };
        self.files.push(path.to_string());
        self.items(&cx, &ast.items, module, &ctx, start, &child_dir(path))
    }

    fn items(
        &mut self,
        cx: &FileCx,
        items: &[Item],
        module: &str,
        ctx: &BTreeSet<String>,
        mut prev: usize,
        dir: &str,
    ) -> Res<()> {
        for item in items {
            let end = range(cx.f, item.to_token_stream()).1;
            match item {
                Item::Mod(m) => {
                    self.pool.extend(doc_attrs(&m.attrs));
                    let mut mctx = ctx.clone();
                    mctx.extend(sem_attrs(&m.attrs));
                    let sub = format!("{module}::{}", m.ident);
                    self.mods.insert(sub.clone(), ModInfo { vis: tok(&m.vis) });
                    let macro_use = m.attrs.iter().any(|a| a.path().is_ident("macro_use"));
                    self.events.push(Ev::Enter(macro_use));
                    if let Some((brace, inner)) = &m.content {
                        let open = cx.f.off(brace.span.open().end());
                        let subdir = format!("{dir}/{}", m.ident);
                        self.items(cx, inner, &sub, &mctx, open, &subdir)?;
                    } else {
                        let p = match path_attr(&m.attrs) {
                            Some(p) => join(dir_of(cx.path), &p),
                            None => [
                                format!("{dir}/{}.rs", m.ident),
                                format!("{dir}/{}/mod.rs", m.ident),
                            ]
                            .into_iter()
                            .find(|c| cx.src.read(c).is_some())
                            .ok_or_else(|| format!("{}: mod {} not found", cx.path, m.ident))?,
                        };
                        self.file(cx.src, &p, &sub, &mctx)?;
                    }
                    self.events.push(Ev::Exit);
                }
                Item::Impl(i) => {
                    self.pool.extend(doc_attrs(&i.attrs));
                    let mut ictx = ctx.clone();
                    ictx.extend(sem_attrs(&i.attrs));
                    let o = owner(i);
                    let mut p = cx.f.off(i.brace_token.span.open().end());
                    for ii in &i.items {
                        let e = range(cx.f, ii.to_token_stream()).1;
                        let func = match ii {
                            ImplItem::Fn(f) => Some(f.clone()),
                            _ => None,
                        };
                        self.leaf(
                            cx,
                            module,
                            Some(o.clone()),
                            member_parts(ii),
                            &ictx,
                            p,
                            e,
                            func,
                        );
                        p = e;
                    }
                }
                Item::Use(u) => flatten_use(u, module, &mut self.uses),
                _ => {
                    if let Some(p) = item_parts(item) {
                        self.leaf(cx, module, None, p, ctx, prev, end, None);
                    }
                }
            }
            prev = end;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn leaf(
        &mut self,
        cx: &FileCx,
        module: &str,
        owner: Option<Owner>,
        p: Parts,
        ctx: &BTreeSet<String>,
        from: usize,
        to: usize,
        func: Option<ImplItemFn>,
    ) {
        let comments = cx
            .cm
            .iter()
            .filter(|c| c.at >= from && c.at < to)
            .map(|c| c.text.clone())
            .collect();
        self.events.push(Ev::Leaf(self.leaves.len()));
        self.leaves.push(Leaf {
            module: module.to_string(),
            owner,
            kind: p.kind,
            name: p.name,
            vis: p.vis,
            tokens: p.tokens,
            ctx: ctx.clone(),
            comments,
            file: cx.path.to_string(),
            line: cx.f.line_of(from),
            ord: None,
            func,
            idx: self.leaves.len(),
        });
    }
}

/// `include_str!`/`include_bytes!`/`include!` string-literal arguments.
pub fn includes(tokens: &str) -> Vec<String> {
    let ts: TokenStream = tokens.parse().unwrap_or_default();
    let mut out = Vec::new();
    collect_includes(ts, &mut out);
    out
}

fn collect_includes(ts: TokenStream, out: &mut Vec<String>) {
    let tts: Vec<TokenTree> = ts.into_iter().collect();
    for (i, tt) in tts.iter().enumerate() {
        if let TokenTree::Group(g) = tt {
            collect_includes(g.stream(), out);
        }
        let TokenTree::Ident(id) = tt else { continue };
        if !matches!(
            id.to_string().as_str(),
            "include_str" | "include_bytes" | "include"
        ) {
            continue;
        }
        if let (Some(TokenTree::Punct(p)), Some(TokenTree::Group(g))) =
            (tts.get(i + 1), tts.get(i + 2))
            && p.as_char() == '!'
            && let Ok(lit) = syn::parse2::<syn::LitStr>(g.stream())
        {
            out.push(lit.value());
        }
    }
}
