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
        modules: vec![crate::apply::ModSpec {
            name: String::new(),
            lines: vec!["pub use inner::*;".into()],
            items: vec![],
        }],
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

const DELEGATED: &str = "impl Backend for K {\n    fn f(&mut self, a: u32, b: u32) -> u32 {\n        self.draw_f(a, b)\n    }\n}\n\nimpl K {\n    pub(super) fn draw_f(&mut self, a: u32, b: u32) -> u32 {\n        a + b\n    }\n}\n";

#[test]
fn delegate_forwarding_passes() {
    assert_eq!(delegate(DELEGATED), Vec::<String>::new());
}

#[test]
fn delegate_with_swapped_args_fails() {
    let errs = delegate(&DELEGATED.replace("self.draw_f(a, b)", "self.draw_f(b, a)"));
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
