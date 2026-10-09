use std::collections::BTreeMap;

use crate::{
    apply::{Manifest, PathEdit},
    tree::Source,
    verify::{Spec, check},
};

struct Mem(BTreeMap<String, String>);

impl Source for Mem {
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        self.0.get(path).map(|s| s.clone().into_bytes())
    }
}

fn mem(files: &[(&str, &str)]) -> Mem {
    Mem(files
        .iter()
        .map(|(p, t)| (p.to_string(), t.to_string()))
        .collect())
}

const BEFORE: &str = r#"
pub fn keep() -> u32 {
    // why one
    1
}

fn data() -> &'static str {
    include_str!("fixtures/x.txt")
}
"#;

const MOD_RS: &str =
    "mod inner;\npub use inner::*;\n\npub fn keep() -> u32 {\n    // why one\n    1\n}\n";

const TABLE: &str = "a::fn keep => a::fn keep\na::fn data => a::inner::fn data\n";

fn manifest(dir: &str, edits: Vec<PathEdit>) -> Manifest {
    let tmp = std::env::temp_dir().join(format!("split-test-{}-{dir}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("a.paths"), TABLE).unwrap();
    Manifest {
        source: "src/a.rs".into(),
        module: "a".into(),
        dir: "src/a".into(),
        table: "a.paths".into(),
        visibility: [("fn data".to_string(), "pub(super)".to_string())].into(),
        path_edits: edits,
        modules: vec![
            crate::apply::ModSpec {
                name: String::new(),
                lines: vec!["pub use inner::*;".into()],
                items: vec![],
            },
            crate::apply::ModSpec {
                name: "inner".into(),
                lines: vec!["use super::*;".into()],
                items: vec![],
            },
        ],
        path: tmp.join("a.toml"),
    }
}

fn run(m: &Manifest, inner: &str, mod_rs: &str, y: &str) -> Vec<String> {
    let base = mem(&[("src/a.rs", BEFORE), ("src/fixtures/x.txt", "X")]);
    let head = mem(&[
        ("src/a/mod.rs", mod_rs),
        ("src/a/inner.rs", inner),
        ("src/fixtures/x.txt", "X"),
        ("src/fixtures/y.txt", y),
    ]);
    let spec = Spec {
        manifest: Some(m),
        old_root: m.source.clone(),
        new_root: m.new_root(),
        module: m.module.clone(),
        delegate: false,
    };
    check(&spec, &base, &head, &[]).unwrap().1
}

fn edit(to: &str) -> Vec<PathEdit> {
    vec![PathEdit {
        item: "fn data".into(),
        from: "fixtures/x.txt".into(),
        to: to.into(),
    }]
}

const INNER: &str = "use super::*;\n\npub(super) fn data() -> &'static str {\n    include_str!(\"../fixtures/x.txt\")\n}\n";

#[test]
fn listed_path_edit_to_same_bytes_passes() {
    let m = manifest("ok", edit("../fixtures/x.txt"));
    assert_eq!(run(&m, INNER, MOD_RS, "X"), Vec::<String>::new());
}

#[test]
fn path_edit_to_other_bytes_fails() {
    let m = manifest("bytes", edit("../fixtures/y.txt"));
    let inner = INNER.replace("x.txt", "y.txt");
    let errs = run(&m, &inner, MOD_RS, "different");
    assert!(
        errs.iter()
            .any(|e| e.contains("do not resolve to the same bytes")),
        "{errs:?}"
    );
}

#[test]
fn unlisted_path_edit_fails() {
    let m = manifest("unlisted", vec![]);
    let errs = run(&m, INNER, MOD_RS, "X");
    assert!(
        errs.iter().any(|e| e.contains("tokens changed")),
        "{errs:?}"
    );
}

#[test]
fn dropped_comment_fails() {
    let m = manifest("comment", edit("../fixtures/x.txt"));
    let errs = run(&m, INNER, &MOD_RS.replace("    // why one\n", ""), "X");
    assert!(
        errs.iter().any(|e| e.contains("comments changed")),
        "{errs:?}"
    );
}

#[test]
fn unlisted_visibility_fails() {
    let m = manifest("vis", edit("../fixtures/x.txt"));
    let errs = run(
        &m,
        &INNER.replace("pub(super) fn", "pub(crate) fn"),
        MOD_RS,
        "X",
    );
    assert!(errs.iter().any(|e| e.contains("visibility")), "{errs:?}");
}

#[test]
fn unlisted_reexport_fails() {
    let m = manifest("reexport", edit("../fixtures/x.txt"));
    let errs = run(&m, INNER, &MOD_RS.replace("pub use", "pub(crate) use"), "X");
    assert!(errs.iter().any(|e| e.contains("re-export")), "{errs:?}");
}

const TRAIT_BEFORE: &str =
    "impl Backend for K {\n    fn f(&mut self, a: u32, b: u32) -> u32 {\n        a + b\n    }\n}\n";

fn same_file(before: &str, after: &str, delegate: bool) -> Vec<String> {
    let spec = Spec {
        manifest: None,
        old_root: "src/k.rs".into(),
        new_root: "src/k.rs".into(),
        module: "k".into(),
        delegate,
    };
    let base = mem(&[("src/k.rs", before)]);
    let head = mem(&[("src/k.rs", after)]);
    check(&spec, &base, &head, &[]).unwrap().1
}

fn delegate(after: &str) -> Vec<String> {
    same_file(TRAIT_BEFORE, after, true)
}

const DELEGATED: &str = "impl Backend for K {\n    fn f(&mut self, a: u32, b: u32) -> u32 {\n        Self::draw_f(self, a, b)\n    }\n}\n\nimpl K {\n    pub(super) fn draw_f(&mut self, a: u32, b: u32) -> u32 {\n        a + b\n    }\n}\n";

#[test]
fn delegate_forwarding_passes() {
    assert_eq!(delegate(DELEGATED), Vec::<String>::new());
}

#[test]
fn delegate_with_swapped_args_fails() {
    let errs = delegate(&DELEGATED.replace("draw_f(self, a, b)", "draw_f(self, b, a)"));
    assert!(!errs.is_empty());
}

#[test]
fn delegate_with_changed_body_fails() {
    let errs = delegate(&DELEGATED.replace("a + b", "a - b"));
    assert!(
        errs.iter()
            .any(|e| e.contains("no inherent fn carries the old body")),
        "{errs:?}"
    );
}

#[test]
fn one_tuple_comma_is_significant() {
    use syn::visit_mut::VisitMut;
    let canon = |s: &str| {
        let mut t: syn::Type = syn::parse_str(s).unwrap();
        crate::tree::Commas.visit_type_mut(&mut t);
        crate::tree::tok(&t)
    };
    assert_ne!(canon("(u8,)"), canon("(u8)"));
    assert_eq!(canon("(u8, u16,)"), canon("(u8, u16)"));
}

fn has(errs: &[String], needle: &str) {
    assert!(
        errs.iter().any(|e| e.contains(needle)),
        "{needle}: {errs:?}"
    );
}

const MACRO_COMMA: &str = "macro_rules! m {\n    ($a:expr, $b:expr,) => { 1 };\n    ($a:expr, $b:expr) => { 2 };\n}\n\nfn f() -> u32 {\n    m!(1, 2,)\n}\n";

#[test]
fn macro_invocation_comma_is_significant() {
    let after = MACRO_COMMA.replace("m!(1, 2,)", "m!(1, 2)");
    has(&same_file(MACRO_COMMA, &after, false), "tokens changed");
}

#[test]
fn macro_rule_comma_is_significant() {
    let after = MACRO_COMMA.replacen("$b:expr,)", "$b:expr)", 1);
    has(&same_file(MACRO_COMMA, &after, false), "tokens changed");
}

#[test]
fn attribute_comma_is_significant() {
    let before = "#[cfg(any(unix, windows,))]\nfn f() {}\n";
    let after = "#[cfg(any(unix, windows))]\nfn f() {}\n";
    has(&same_file(before, after, false), "tokens changed");
}

#[test]
fn layout_commas_are_not_significant() {
    let before = "fn f<T: Copy,>(a: T, b: (T, T,),) -> [T; 2] where T: Eq, {\n    let S { x, .. } = g(a, b.0,);\n    match x { 1 => {}, _ => (), }\n    [a, b.1,]\n}\n";
    let after = "fn f<T: Copy>(a: T, b: (T, T)) -> [T; 2] where T: Eq {\n    let S { x, .. } = g(a, b.0);\n    match x { 1 => {} _ => () }\n    [a, b.1]\n}\n";
    assert_eq!(same_file(before, after, false), Vec::<String>::new());
}

/// `src/a.rs` (module `a`) split into `src/a/mod.rs` + `src/a/inner.rs`;
/// `lines` are the manifest's (root, inner) use lines.
fn split(
    tag: &str,
    before: &str,
    files: &[(&str, &str)],
    table: &str,
    lines: [&[&str]; 2],
) -> Vec<String> {
    let tmp = std::env::temp_dir().join(format!("split-test-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("a.paths"), table).unwrap();
    let spec_of = |name: &str, l: &[&str]| crate::apply::ModSpec {
        name: name.into(),
        lines: l.iter().map(|s| s.to_string()).collect(),
        items: vec![],
    };
    let m = Manifest {
        source: "src/a.rs".into(),
        module: "a".into(),
        dir: "src/a".into(),
        table: "a.paths".into(),
        visibility: [("fn f".to_string(), "pub(super)".to_string())].into(),
        path_edits: vec![],
        modules: vec![spec_of("", lines[0]), spec_of("inner", lines[1])],
        path: tmp.join("a.toml"),
    };
    let spec = Spec {
        manifest: Some(&m),
        old_root: m.source.clone(),
        new_root: m.new_root(),
        module: m.module.clone(),
        delegate: false,
    };
    let base = mem(&[("src/a.rs", before)]);
    let head = mem(files);
    check(&spec, &base, &head, &[]).unwrap().1
}

#[test]
fn private_use_change_fails() {
    let before = "use std::cmp::min as pick;\n\nfn f() -> u32 {\n    pick(1, 2)\n}\n";
    let after = before.replace("min as pick", "max as pick");
    has(
        &same_file(before, &after, false),
        "use std :: cmp :: max as pick",
    );
}

const MACRO_ORDER: &str = "macro_rules! m {\n    () => { 1 };\n}\n\nfn a() -> u32 {\n    m!()\n}\n\nmacro_rules! m {\n    () => { 2 };\n}\n\nfn b() -> u32 {\n    m!()\n}\n";

#[test]
fn macro_definition_reordered_fails() {
    let after = "macro_rules! m {\n    () => { 1 };\n}\n\nmacro_rules! m {\n    () => { 2 };\n}\n\nfn a() -> u32 {\n    m!()\n}\n\nfn b() -> u32 {\n    m!()\n}\n";
    has(&same_file(MACRO_ORDER, after, false), "macro m");
}

#[test]
fn macro_scope_lost_by_mod_declaration_order_fails() {
    let before = "macro_rules! m {\n    () => { 1 };\n}\n\nfn f() -> u32 {\n    m!()\n}\n";
    let mod_rs = "mod inner;\npub use inner::*;\n\nmacro_rules! m {\n    () => { 1 };\n}\n";
    let inner = "use super::*;\n\npub(super) fn f() -> u32 {\n    m!()\n}\n";
    let table = "a::macro m => a::macro m\na::fn f => a::inner::fn f\n";
    let errs = split(
        "macro-scope",
        before,
        &[("src/a/mod.rs", mod_rs), ("src/a/inner.rs", inner)],
        table,
        [&["pub use inner::*;"], &["use super::*;"]],
    );
    has(&errs, "macro m");
}

const NR_BEFORE: &str = "mod other {\n    pub fn helper() -> u32 {\n        2\n    }\n}\n\nfn helper() -> u32 {\n    1\n}\n\nfn f() -> u32 {\n    helper()\n}\n";
const NR_MOD: &str = "mod inner;\npub use inner::*;\n\nmod other {\n    pub fn helper() -> u32 {\n        2\n    }\n}\n\nfn helper() -> u32 {\n    1\n}\n";
const NR_TABLE: &str = "a::other::fn helper => a::other::fn helper\na::fn helper => a::fn helper\na::fn f => a::inner::fn f\n";

#[test]
fn moved_item_gaining_a_shadowing_import_fails() {
    let inner =
        "use super::*;\nuse super::other::helper;\n\npub(super) fn f() -> u32 {\n    helper()\n}\n";
    let errs = split(
        "shadow",
        NR_BEFORE,
        &[("src/a/mod.rs", NR_MOD), ("src/a/inner.rs", inner)],
        NR_TABLE,
        [
            &["pub use inner::*;"],
            &["use super::*;", "use super::other::helper;"],
        ],
    );
    has(&errs, "helper");
}

#[test]
fn moved_item_gaining_a_glob_fails() {
    let inner =
        "use super::*;\nuse super::other::*;\n\npub(super) fn f() -> u32 {\n    helper()\n}\n";
    let errs = split(
        "glob",
        NR_BEFORE,
        &[("src/a/mod.rs", NR_MOD), ("src/a/inner.rs", inner)],
        NR_TABLE,
        [
            &["pub use inner::*;"],
            &["use super::*;", "use super::other::*;"],
        ],
    );
    has(&errs, "helper");
}

#[test]
fn moved_item_with_relative_path_fails() {
    let before = "fn f() -> u32 {\n    super::g()\n}\n";
    let mod_rs = "mod inner;\npub use inner::*;\n";
    let inner = "use super::*;\n\npub(super) fn f() -> u32 {\n    super::g()\n}\n";
    let errs = split(
        "relpath",
        before,
        &[("src/a/mod.rs", mod_rs), ("src/a/inner.rs", inner)],
        "a::fn f => a::inner::fn f\n",
        [&["pub use inner::*;"], &["use super::*;"]],
    );
    has(&errs, "`super::` path");
}

#[test]
fn clean_split_passes() {
    let inner = "use super::*;\n\npub(super) fn f() -> u32 {\n    helper()\n}\n";
    let errs = split(
        "clean",
        NR_BEFORE,
        &[("src/a/mod.rs", NR_MOD), ("src/a/inner.rs", inner)],
        NR_TABLE,
        [&["pub use inner::*;"], &["use super::*;"]],
    );
    assert_eq!(errs, Vec::<String>::new());
}

const DELEGATE_VIA_TRAIT: &str = "use crate::other::Other;\n\nimpl Backend for K {\n    fn f(&mut self, a: u32, b: u32) -> u32 {\n        a + b\n    }\n}\n";

#[test]
fn delegate_to_cfg_disabled_helper_fails() {
    let after = format!(
        "use crate::other::Other;\n\n{}",
        DELEGATED.replace(
            "    pub(super) fn draw_f",
            "    #[cfg(any())]\n    pub(super) fn draw_f"
        )
    );
    let errs = same_file(DELEGATE_VIA_TRAIT, &after, true);
    has(&errs, "no inherent fn carries the old body");
}

#[test]
fn delegate_by_method_call_fails() {
    let after = format!(
        "use crate::other::Other;\n\n{}",
        DELEGATED.replace("Self::draw_f(self, a, b)", "self.draw_f(a, b)")
    );
    let errs = same_file(DELEGATE_VIA_TRAIT, &after, true);
    has(&errs, "tokens changed");
}

#[test]
fn delegate_to_unsafe_helper_fails() {
    let errs = delegate(&DELEGATED.replace("pub(super) fn draw_f", "pub(super) unsafe fn draw_f"));
    has(&errs, "no inherent fn carries the old body");
}

#[test]
fn delegate_into_other_generic_context_fails() {
    let before = TRAIT_BEFORE.replace("impl Backend for K", "impl<T: Copy> Backend for K<T>");
    let after = DELEGATED
        .replace("impl Backend for K", "impl<T: Copy> Backend for K<T>")
        .replace("impl K {", "impl<T> K<T> {");
    has(
        &same_file(&before, &after, true),
        "no inherent fn carries the old body",
    );
}

#[test]
fn path_edit_does_not_cover_other_literals() {
    let before = "fn data() -> (&'static str, &'static str) {\n    (include_str!(\"fixtures/x.txt\"), \"fixtures/x.txt\")\n}\n";
    let mod_rs = "mod inner;\npub use inner::*;\n";
    let inner = "use super::*;\n\npub(super) fn data() -> (&'static str, &'static str) {\n    (include_str!(\"../fixtures/x.txt\"), \"../fixtures/x.txt\")\n}\n";
    let mut m = manifest("edit-scope", edit("../fixtures/x.txt"));
    m.table = "b.paths".into();
    std::fs::write(m.table_path(), "a::fn data => a::inner::fn data\n").unwrap();
    let base = mem(&[("src/a.rs", before), ("src/fixtures/x.txt", "X")]);
    let head = mem(&[
        ("src/a/mod.rs", mod_rs),
        ("src/a/inner.rs", inner),
        ("src/fixtures/x.txt", "X"),
    ]);
    let errs = check(&spec_of(&m), &base, &head, &[]).unwrap().1;
    has(&errs, "tokens changed");
}

fn spec_of(m: &Manifest) -> Spec<'_> {
    Spec {
        manifest: Some(m),
        old_root: m.source.clone(),
        new_root: m.new_root(),
        module: m.module.clone(),
        delegate: false,
    }
}

/// `src/k.rs` unchanged; `extra` files differ between base and head.
fn module_files(root: &str, extra: &[(&str, &str, &str)]) -> Vec<String> {
    let spec = Spec {
        manifest: None,
        old_root: "src/k.rs".into(),
        new_root: "src/k.rs".into(),
        module: "k".into(),
        delegate: false,
    };
    let mut b = vec![("src/k.rs", root)];
    let mut a = vec![("src/k.rs", root)];
    for (p, x, y) in extra {
        b.push((p, x));
        a.push((p, y));
    }
    check(&spec, &mem(&b), &mem(&a), &[]).unwrap().1
}

#[test]
fn path_attribute_on_inline_module_is_followed() {
    let root = "#[path = \"other\"]\nmod m {\n    mod x;\n}\n";
    let errs = module_files(
        root,
        &[
            (
                "src/k/m/x.rs",
                "fn x() -> u32 {\n    1\n}\n",
                "fn x() -> u32 {\n    1\n}\n",
            ),
            (
                "src/other/x.rs",
                "fn x() -> u32 {\n    1\n}\n",
                "fn x() -> u32 {\n    2\n}\n",
            ),
        ],
    );
    has(&errs, "tokens changed");
}

#[test]
fn path_attribute_inside_inline_module_is_relative_to_it() {
    let root = "mod m {\n    #[path = \"y.rs\"]\n    mod x;\n}\n";
    let errs = module_files(
        root,
        &[
            (
                "src/y.rs",
                "fn x() -> u32 {\n    1\n}\n",
                "fn x() -> u32 {\n    1\n}\n",
            ),
            (
                "src/k/m/y.rs",
                "fn x() -> u32 {\n    1\n}\n",
                "fn x() -> u32 {\n    2\n}\n",
            ),
        ],
    );
    has(&errs, "tokens changed");
}

fn moved_data(tag: &str, body: &str, moved: &str) -> Vec<String> {
    let before = format!("fn data() -> &'static str {{\n    {body}\n}}\n");
    let inner =
        format!("use super::*;\n\npub(super) fn data() -> &'static str {{\n    {moved}\n}}\n");
    let m = manifest(tag, edit("../fixtures/x.txt"));
    std::fs::write(m.table_path(), "a::fn data => a::inner::fn data\n").unwrap();
    let base = mem(&[("src/a.rs", &before), ("src/fixtures/x.txt", "X")]);
    let head = mem(&[
        ("src/a/mod.rs", "mod inner;\npub use inner::*;\n"),
        ("src/a/inner.rs", &inner),
        ("src/fixtures/x.txt", "X"),
        ("src/a/fixtures/x.txt", "Y"),
    ]);
    check(&spec_of(&m), &base, &head, &[]).unwrap().1
}

#[test]
fn moved_include_macro_fails() {
    let body = "include!(\"fixtures/x.txt\")";
    has(&moved_data("include", body, body), "include!");
}

#[test]
fn moved_dynamic_include_path_fails() {
    let body = "include_str!(concat!(\"fixtures/\", \"x.txt\"))";
    has(&moved_data("dynamic", body, body), "include");
}

#[test]
fn wrapper_attribute_order_is_significant() {
    let before = "#[allow(unused)]\n#[deny(unused)]\nmod m {\n    fn f() {}\n}\n";
    let after = "#[deny(unused)]\n#[allow(unused)]\nmod m {\n    fn f() {}\n}\n";
    has(
        &same_file(before, after, false),
        "effective cfg/attributes changed",
    );
}

#[test]
fn wrapper_attribute_nesting_is_significant() {
    let before = "#[allow(unused)]\nmod a {\n    #[deny(unused)]\n    mod b {\n        fn f() {}\n    }\n}\n";
    let after = "#[deny(unused)]\nmod a {\n    #[allow(unused)]\n    mod b {\n        fn f() {}\n    }\n}\n";
    has(
        &same_file(before, after, false),
        "effective cfg/attributes changed",
    );
}

#[test]
fn module_visibility_is_significant() {
    let before = "pub mod x {\n    pub fn f() {}\n}\n";
    let after = "mod x {\n    pub fn f() {}\n}\n";
    has(&same_file(before, after, false), "module visibility");
}

#[test]
fn relative_visibility_widened_by_relocation_fails() {
    let before = "mod m {\n    mod n {\n        pub(super) fn f() {}\n    }\n}\n";
    let mod_rs = "mod inner;\npub use inner::*;\n\nmod m {\n    mod n {}\n}\n";
    let inner = "use super::*;\n\npub(super) fn f() {}\n";
    let errs = split(
        "widen",
        before,
        &[("src/a/mod.rs", mod_rs), ("src/a/inner.rs", inner)],
        "a::m::n::fn f => a::inner::fn f\n",
        [&["pub use inner::*;"], &["use super::*;"]],
    );
    has(&errs, "widened");
}

#[test]
fn duplicated_comment_fails() {
    let before = "// note\nuse a::b;\nuse c::d;\n";
    let after = "// note\nuse a::b;\n// note\nuse c::d;\n";
    has(&same_file(before, after, false), "comment added");
}
