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

    fn list(&self, dir: &str) -> Vec<String> {
        self.0
            .keys()
            .filter(|k| dir.is_empty() || k.starts_with(&format!("{dir}/")))
            .cloned()
            .collect()
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
        root_form: crate::apply::RootForm::Dir,
        table: "a.paths".into(),
        target: None,
        visibility: [("fn data".to_string(), "pub(super)".to_string())].into(),
        path_edits: edits,
        exceptions: BTreeMap::new(),
        locations: BTreeMap::new(),
        log_targets: BTreeMap::new(),
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

const TRAIT_BEFORE: &str = "pub struct K;\n\nimpl Backend for K {\n    fn f(&mut self, a: u32, b: u32) -> u32 {\n        a + b\n    }\n}\n";

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

const DELEGATED: &str = "pub struct K;\n\nimpl Backend for K {\n    fn f(&mut self, a: u32, b: u32) -> u32 {\n        Self::draw_f(self, a, b)\n    }\n}\n\nimpl K {\n    pub(super) fn draw_f(&mut self, a: u32, b: u32) -> u32 {\n        a + b\n    }\n}\n";

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
    split_with(tag, before, files, table, lines, false)
}

fn split_with(
    tag: &str,
    before: &str,
    files: &[(&str, &str)],
    table: &str,
    lines: [&[&str]; 2],
    delegate: bool,
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
        target: None,
        visibility: [("fn f".to_string(), "pub(super)".to_string())].into(),
        root_form: crate::apply::RootForm::Dir,
        path_edits: vec![],
        exceptions: BTreeMap::new(),
        locations: BTreeMap::new(),
        log_targets: BTreeMap::new(),
        modules: vec![spec_of("", lines[0]), spec_of("inner", lines[1])],
        path: tmp.join("a.toml"),
    };
    let spec = Spec {
        delegate,
        ..self::spec_of(&m)
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

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("split-test-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

const HEAD_LINE: &str = "#\ttree=t\trev=r\tconfig=CFG\thost=h\n";

fn snapshot(dir: &std::path::Path, rows: &str) {
    for (cfg, _) in crate::testlist::CONFIGS {
        std::fs::write(
            dir.join(format!("{cfg}.tsv")),
            HEAD_LINE.replace("CFG", cfg).to_string() + rows,
        )
        .unwrap();
    }
}

#[test]
fn malformed_snapshot_row_fails() {
    let d = tmpdir("malformed");
    std::fs::write(
        d.join("default.tsv"),
        HEAD_LINE.replace("CFG", "default") + "c:lib\ta::t\n",
    )
    .unwrap();
    assert!(crate::testlist::read(&d.join("default.tsv"), "default").is_err());
}

#[test]
fn test_name_collision_fails() {
    let before = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn x() {}\n\n    mod inner {\n        #[test]\n        fn x() {}\n    }\n}\n";
    let m = manifest("collide", vec![]);
    std::fs::write(
        m.table_path(),
        "a::tests::fn x => a::tests::w::fn x\na::tests::inner::fn x => a::tests::w::fn x\n",
    )
    .unwrap();
    let (b, a) = (tmpdir("collide-b"), tmpdir("collide-a"));
    snapshot(
        &b,
        "c:lib\ta::tests::x\trun\nc:lib\ta::tests::inner::x\trun\n",
    );
    snapshot(&a, "c:lib\ta::tests::w::x\trun\n");
    let r = crate::testlist::verify(&spec_of(&m), &mem(&[("src/a.rs", before)]), "c:lib", &b, &a);
    assert!(!matches!(r, Ok(true)), "{r:?}");
}

#[test]
fn test_targets_from_cargo_layout() {
    let src = mem(&[
        (
            "crates/y/Cargo.toml",
            "[package]\nname = \"y-core\"\n\n[[bin]]\nname = \"tool\"\npath = \"src/tool/main.rs\"\n",
        ),
        ("crates/y/src/lib.rs", ""),
        ("crates/z/Cargo.toml", "[package]\nname = \"z\"\n"),
        ("crates/z/src/main.rs", ""),
    ]);
    let t = |f: &str| crate::crate_bin(&src, f).unwrap();
    assert_eq!(t("crates/y/src/resources.rs"), "y_core:lib");
    assert_eq!(
        t("crates/y/tests/render_acceptance.rs"),
        "render_acceptance:test"
    );
    assert_eq!(
        t("crates/y/tests/render_acceptance/main.rs"),
        "render_acceptance:test"
    );
    assert_eq!(
        t("crates/y/tests/render_acceptance/xkb.rs"),
        "render_acceptance:test"
    );
    assert_eq!(t("crates/y/src/tool/main.rs"), "tool:bin");
    assert_eq!(t("crates/y/src/bin/other.rs"), "other:bin");
    assert_eq!(t("crates/y/examples/demo.rs"), "demo:bin");
    assert_eq!(t("crates/z/src/state.rs"), "z:bin");
}

/// Runs `apply` on `files` in a scratch repo, then `verify` against them.
fn apply_and_verify(
    tag: &str,
    files: &[(&str, &str)],
    modules: [(&[&str], &[&str]); 2],
    edits: Vec<PathEdit>,
) -> Result<(Vec<String>, std::path::PathBuf), String> {
    apply_and_verify_as(tag, files, modules, edits, crate::apply::RootForm::Dir)
}

fn apply_and_verify_as(
    tag: &str,
    files: &[(&str, &str)],
    modules: [(&[&str], &[&str]); 2],
    edits: Vec<PathEdit>,
    root_form: crate::apply::RootForm,
) -> Result<(Vec<String>, std::path::PathBuf), String> {
    let repo = tmpdir(&format!("apply-{tag}"));
    let _ = std::fs::remove_dir_all(&repo);
    for (p, t) in files {
        let p = repo.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, t).unwrap();
    }
    let spec_of_mod = |name: &str, (items, lines): (&[&str], &[&str])| crate::apply::ModSpec {
        name: name.into(),
        lines: lines.iter().map(|s| s.to_string()).collect(),
        items: items.iter().map(|s| s.to_string()).collect(),
    };
    let m = Manifest {
        source: "src/a.rs".into(),
        module: "a".into(),
        dir: "src/a".into(),
        table: "a.paths".into(),
        target: None,
        visibility: [("fn f".to_string(), "pub(super)".to_string())].into(),
        root_form,
        path_edits: edits,
        exceptions: BTreeMap::new(),
        locations: BTreeMap::new(),
        log_targets: BTreeMap::new(),
        modules: vec![
            spec_of_mod("", modules[0]),
            spec_of_mod("inner", modules[1]),
        ],
        path: repo.join("a.toml"),
    };
    crate::apply::run(&m, &repo)?;
    let base = mem(files);
    let errs = check(&spec_of(&m), &base, &crate::tree::Disk(repo.clone()), &[])
        .unwrap()
        .1;
    Ok((errs, repo))
}

const WITH_CHILD: &str = "mod helpers;\n\nfn f() -> u32 {\n    helpers::g()\n}\n";
const HELPERS: &str = "pub fn g() -> u32 {\n    1\n}\n";

#[test]
fn apply_keeps_out_of_line_child_modules() {
    let (errs, _) = apply_and_verify(
        "child",
        &[("src/a.rs", WITH_CHILD), ("src/a/helpers.rs", HELPERS)],
        [
            (&["mod helpers"], &["pub use inner::*;"]),
            (&["fn f"], &["use super::*;"]),
        ],
        vec![],
    )
    .unwrap();
    assert_eq!(errs, Vec::<String>::new());
}

#[test]
fn apply_refuses_child_module_that_would_resolve_elsewhere() {
    let r = apply_and_verify(
        "child-moved",
        &[("src/a.rs", WITH_CHILD), ("src/a/helpers.rs", HELPERS)],
        [
            (&[], &["pub use inner::*;"]),
            (&["fn f", "mod helpers"], &["use super::*;"]),
        ],
        vec![],
    );
    assert!(
        r.as_ref().is_err_and(|e| e.contains("add a path edit")),
        "{r:?}"
    );
}

#[test]
fn apply_performs_path_edits() {
    let src = "#[path = \"other_tests.rs\"]\nmod other_tests;\n\nfn f() -> &'static str {\n    include_str!(\"fixtures/x.txt\")\n}\n";
    let edits = vec![
        PathEdit {
            item: "fn f".into(),
            from: "fixtures/x.txt".into(),
            to: "../fixtures/x.txt".into(),
        },
        PathEdit {
            item: "mod other_tests".into(),
            from: "other_tests.rs".into(),
            to: "../other_tests.rs".into(),
        },
    ];
    let (errs, repo) = apply_and_verify(
        "edits",
        &[
            ("src/a.rs", src),
            ("src/other_tests.rs", HELPERS),
            ("src/fixtures/x.txt", "X"),
        ],
        [
            (&["mod other_tests"], &["pub use inner::*;"]),
            (&["fn f"], &["use super::*;"]),
        ],
        edits,
    )
    .unwrap();
    assert_eq!(errs, Vec::<String>::new());
    let inner = std::fs::read_to_string(repo.join("src/a/inner.rs")).unwrap();
    assert!(
        inner.contains("include_str!(\"../fixtures/x.txt\")"),
        "{inner}"
    );
    let root = std::fs::read_to_string(repo.join("src/a/mod.rs")).unwrap();
    assert!(root.contains("#[path = \"../other_tests.rs\"]"), "{root}");
}

const EXPAND_BEFORE: &str = "mod other {\n    pub fn helper() -> u32 {\n        2\n    }\n}\n\nfn helper() -> u32 {\n    1\n}\n\nmacro_rules! m {\n    () => {\n        helper()\n    };\n}\n\nfn f() -> u32 {\n    m!()\n}\n";
const EXPAND_MOD: &str = "mod other {\n    pub fn helper() -> u32 {\n        2\n    }\n}\n\nfn helper() -> u32 {\n    1\n}\n\nmacro_rules! m {\n    () => {\n        helper()\n    };\n}\n\nmod inner;\npub use inner::*;\n";
const EXPAND_TABLE: &str = "a::other::fn helper => a::other::fn helper\na::fn helper => a::fn helper\na::macro m => a::macro m\na::fn f => a::inner::fn f\n";

#[test]
fn macro_expanding_in_a_new_module_is_refused() {
    let inner =
        "use super::*;\nuse super::other::helper;\n\npub(super) fn f() -> u32 {\n    m!()\n}\n";
    let errs = split(
        "expand",
        EXPAND_BEFORE,
        &[("src/a/mod.rs", EXPAND_MOD), ("src/a/inner.rs", inner)],
        EXPAND_TABLE,
        [
            &["pub use inner::*;"],
            &["use super::*;", "use super::other::helper;"],
        ],
    );
    has(&errs, "invokes macro_rules!");
}

#[test]
fn cfg_disabled_macro_masking_a_reorder_fails() {
    let before = "#[macro_use]\nmod p {\n    macro_rules! m {\n        () => { 1 };\n    }\n}\n\n#[macro_use]\nmod q {\n    macro_rules! m {\n        () => { 2 };\n    }\n}\n\n#[cfg(any())]\nmacro_rules! m {\n    () => { 3 };\n}\n\nfn f() -> u32 {\n    m!()\n}\n";
    let after = "#[macro_use]\nmod q {\n    macro_rules! m {\n        () => { 2 };\n    }\n}\n\n#[macro_use]\nmod p {\n    macro_rules! m {\n        () => { 1 };\n    }\n}\n\n#[cfg(any())]\nmacro_rules! m {\n    () => { 3 };\n}\n\nfn f() -> u32 {\n    m!()\n}\n";
    has(&same_file(before, after, false), "macro resolution changed");
}

#[test]
fn path_imported_macro_switched_by_an_import_fails() {
    let defs = "mod x {\n    macro_rules! pick {\n        () => { 1 };\n    }\n    pub(crate) use pick;\n}\n\nmod y {\n    macro_rules! pick {\n        () => { 2 };\n    }\n    pub(crate) use pick;\n}\n\nuse x::pick;\n";
    let before = format!("{defs}\nfn f() -> u32 {{\n    pick!()\n}}\n");
    let mod_rs = format!("mod inner;\npub use inner::*;\n\n{defs}");
    let inner =
        "use super::*;\nuse super::y::pick;\n\npub(super) fn f() -> u32 {\n    pick!()\n}\n";
    let errs = split(
        "macro-path",
        &before,
        &[("src/a/mod.rs", &mod_rs), ("src/a/inner.rs", inner)],
        "a::x::macro pick => a::x::macro pick\na::y::macro pick => a::y::macro pick\na::fn f => a::inner::fn f\n",
        [
            &["pub use inner::*;"],
            &["use super::*;", "use super::y::pick;"],
        ],
    );
    has(&errs, "macro resolution changed");
}

const TRAITS: &str = "mod ta {\n    pub trait A {\n        fn pick(&self) -> u32 {\n            1\n        }\n    }\n    impl A for u32 {}\n}\n\nmod tb {\n    pub trait B {\n        fn pick(&self) -> u32 {\n            2\n        }\n    }\n    impl B for u32 {}\n}\n\nuse ta::A as T;\n";
const TRAITS_TABLE: &str = "a::ta::trait A => a::ta::trait A\na::tb::trait B => a::tb::trait B\na::fn f => a::inner::fn f\n";

#[test]
fn trait_import_shadowed_in_destination_fails() {
    let before = format!("{TRAITS}\nfn f() -> u32 {{\n    0u32.pick()\n}}\n");
    let mod_rs = format!("mod inner;\npub use inner::*;\n\n{TRAITS}");
    let inner =
        "use super::*;\nuse super::tb::B as T;\n\npub(super) fn f() -> u32 {\n    0u32.pick()\n}\n";
    let errs = split(
        "trait-env",
        &before,
        &[("src/a/mod.rs", &mod_rs), ("src/a/inner.rs", inner)],
        TRAITS_TABLE,
        [
            &["pub use inner::*;"],
            &["use super::*;", "use super::tb::B as T;"],
        ],
    );
    has(&errs, "traits in scope changed");
}

const DELEGATE_SPLIT: &str = "use std::cmp::min as pick;\n\npub struct K;\n\nimpl Backend for K {\n    fn f(&mut self, a: u32, b: u32) -> u32 {\n        pick(a, b)\n    }\n}\n";
const FORWARDER: &str = "impl Backend for K {\n    fn f(&mut self, a: u32, b: u32) -> u32 {\n        Self::draw_f(self, a, b)\n    }\n}\n";
const FWD_TABLE: &str =
    "a::struct K => a::struct K\na::impl Backend for K::fn f => a::impl Backend for K::fn f\n";

#[test]
fn delegated_helper_under_another_import_fails() {
    let mod_rs = format!(
        "mod inner;\npub use inner::*;\n\nuse std::cmp::min as pick;\n\npub struct K;\n\n{FORWARDER}"
    );
    let inner = "use super::*;\nuse std::cmp::max as pick;\n\nimpl K {\n    pub(super) fn draw_f(&mut self, a: u32, b: u32) -> u32 {\n        pick(a, b)\n    }\n}\n";
    let errs = split_with(
        "delegate-names",
        DELEGATE_SPLIT,
        &[("src/a/mod.rs", &mod_rs), ("src/a/inner.rs", inner)],
        FWD_TABLE,
        [
            &["pub use inner::*;"],
            &["use super::*;", "use std::cmp::max as pick;"],
        ],
        true,
    );
    has(&errs, "name `pick` resolves differently");
}

#[test]
fn delegated_helper_on_another_modules_type_fails() {
    let other = "mod other {\n    pub struct K;\n}\n";
    let before = format!("{other}\n{DELEGATE_SPLIT}");
    let mod_rs = format!(
        "mod inner;\npub use inner::*;\n\n{other}\nuse std::cmp::min as pick;\n\npub struct K;\n\n{FORWARDER}"
    );
    let inner = "use super::*;\nuse super::other::K;\n\nimpl K {\n    pub(super) fn draw_f(&mut self, a: u32, b: u32) -> u32 {\n        pick(a, b)\n    }\n}\n";
    let errs = split_with(
        "delegate-type",
        &before,
        &[("src/a/mod.rs", &mod_rs), ("src/a/inner.rs", inner)],
        &format!("a::other::struct K => a::other::struct K\n{FWD_TABLE}"),
        [
            &["pub use inner::*;"],
            &["use super::*;", "use super::other::K;"],
        ],
        true,
    );
    has(&errs, "name `K` resolves differently");
}

#[test]
fn local_macro_moved_to_a_new_module_is_refused() {
    let before = format!(
        "macro_rules! m {{\n    () => {{ 0 }};\n}}\n\n{TRAITS}\nfn f() -> u32 {{\n    macro_rules! m {{\n        () => {{ 0u32.pick() }};\n    }}\n    m!()\n}}\n"
    );
    let mod_rs = format!(
        "macro_rules! m {{\n    () => {{ 0 }};\n}}\n\nmod inner;\npub use inner::*;\n\n{TRAITS}"
    );
    let inner = "use super::*;\n\npub(super) fn f() -> u32 {\n    macro_rules! m {\n        () => { 0u32.pick() };\n    }\n    m!()\n}\n";
    let errs = split(
        "local-macro",
        &before,
        &[("src/a/mod.rs", &mod_rs), ("src/a/inner.rs", inner)],
        &format!("a::macro m => a::macro m\n{TRAITS_TABLE}"),
        [&["pub use inner::*;"], &["use super::*;"]],
    );
    has(&errs, "local macro_rules!");
}

fn expand(tag: &str, inner_uses: &[&str], exceptions: &[(&str, &str)]) -> Vec<String> {
    let inner = format!(
        "{}\n\npub(super) fn f() -> u32 {{\n    m!()\n}}\n",
        inner_uses.join("\n")
    );
    let mut m = manifest(tag, vec![]);
    std::fs::write(m.table_path(), EXPAND_TABLE).unwrap();
    m.visibility = [("fn f".to_string(), "pub(super)".to_string())].into();
    m.modules[1].lines = inner_uses.iter().map(|s| s.to_string()).collect();
    m.exceptions = exceptions
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let base = mem(&[("src/a.rs", EXPAND_BEFORE)]);
    let head = mem(&[("src/a/mod.rs", EXPAND_MOD), ("src/a/inner.rs", &inner)]);
    check(&spec_of(&m), &base, &head, &[]).unwrap().1
}

#[test]
fn audited_macro_exception_passes() {
    let errs = expand(
        "exc-ok",
        &["use super::*;"],
        &[("fn f", "m! only calls helper")],
    );
    assert_eq!(errs, Vec::<String>::new());
}

#[test]
fn audited_macro_exception_still_checks_expansion_names() {
    let errs = expand(
        "exc-names",
        &["use super::*;", "use super::other::helper;"],
        &[("fn f", "m! only calls helper")],
    );
    has(
        &errs,
        "name `helper` in a macro expansion resolves differently",
    );
}

#[test]
fn unneeded_or_unexplained_exception_fails() {
    let a = &[("fn f", "audited")][..];
    let errs = expand(
        "exc-unused",
        &["use super::*;"],
        &[a[0], ("fn helper", "x")],
    );
    has(&errs, "exception not needed");
    let r = std::panic::catch_unwind(|| expand("exc-empty", &["use super::*;"], &[("fn f", " ")]));
    assert!(r.is_err());
}

#[test]
fn relative_path_refusal_names_the_remedy() {
    let before = "mod tests {\n    fn t() {\n        super::super::run();\n    }\n}\n";
    let mod_rs = "mod tests;\n";
    let inner = "use super::*;\n\nfn t() {\n    super::super::run();\n}\n";
    let errs = split(
        "relpath-tests",
        before,
        &[
            ("src/a/mod.rs", mod_rs),
            ("src/a/tests/mod.rs", "mod inner;\n"),
            ("src/a/tests/inner.rs", inner),
        ],
        "a::tests::fn t => a::tests::inner::fn t\n",
        [&[], &["use super::*;"]],
    );
    has(&errs, "keep the module depth, or qualify the path");
}

#[test]
fn relative_path_to_the_same_module_passes() {
    let before = "mod x {\n    pub fn g() {}\n}\n\nmod y {\n    fn f() {\n        super::x::g();\n    }\n}\n";
    let after = "mod x {\n    pub fn g() {}\n}\n\nmod z {\n    fn f() {\n        super::x::g();\n    }\n}\n";
    let spec = Spec {
        manifest: None,
        old_root: "src/k.rs".into(),
        new_root: "src/k.rs".into(),
        module: "k".into(),
        delegate: false,
    };
    let errs = check(
        &spec,
        &mem(&[("src/k.rs", before)]),
        &mem(&[("src/k.rs", after)]),
        &[],
    )
    .unwrap()
    .1;
    assert!(
        errs.iter().all(|e| !e.contains("`super::` path")),
        "{errs:?}"
    );
}

#[test]
fn cast_target_is_a_free_name() {
    let before = "type Pick = u8;\n\nfn f() -> u32 {\n    300 as Pick as u32\n}\n";
    let mod_rs = "mod inner;\npub use inner::*;\n\ntype Pick = u8;\n";
    let inner = "use super::*;\ntype Pick = u16;\n\npub(super) fn f() -> u32 {\n    300 as Pick as u32\n}\n";
    let errs = split(
        "cast",
        before,
        &[("src/a/mod.rs", mod_rs), ("src/a/inner.rs", inner)],
        "a::type Pick => a::type Pick\na::fn f => a::inner::fn f\n",
        [&["pub use inner::*;"], &["use super::*;"]],
    );
    has(&errs, "name `Pick` resolves differently");
}

fn fn_body(body: &str) -> String {
    format!("fn f(v: &[u8]) -> bool {{\n    {body}\n}}\n")
}

#[test]
fn closure_block_of_one_tail_expression_is_not_significant() {
    let before = fn_body("v.iter().any(|x| { *x == 1 && *x != 2 })");
    let after = fn_body("v.iter().any(|x| *x == 1 && *x != 2)");
    assert_eq!(same_file(&before, &after, false), Vec::<String>::new());
    let before = fn_body("v.iter().any(move |x| { g![x] })");
    let after = fn_body("v.iter().any(move |x| g![x])");
    assert_eq!(same_file(&before, &after, false), Vec::<String>::new());
}

#[test]
fn closure_block_of_another_shape_is_significant() {
    for block in [
        "|x| { let y = *x; y == 1 }",
        "|x| { *x == 1; }",
        "|x| unsafe { g(x) }",
        "|x| 'a: { g(x) }",
        "|x| #[allow(unused)] { g(x) }",
        "|x| { #![allow(unused)] g(x) }",
        "|x| { #[allow(unused)] g(x) }",
        "|x| { g! { x } }",
        "|x| async { g(x) }",
        "|x| { { g(x) } }",
    ] {
        let before = fn_body(&format!("v.iter().any({block})"));
        let after = fn_body("v.iter().any(|x| g(x))");
        has(&same_file(&before, &after, false), "tokens changed");
    }
    let before = fn_body("v.iter().any(|x| -> bool { g(x) })");
    has(
        &same_file(&before, &fn_body("v.iter().any(|x| g(x))"), false),
        "tokens changed",
    );
}

#[test]
fn match_arm_block_of_one_tail_expression_is_not_significant() {
    let before = fn_body("match v.first() { Some(x) => { (*x, 1) } None => { g![v] } }");
    let after = fn_body("match v.first() { Some(x) => (*x, 1), None => g![v], }");
    assert_eq!(same_file(&before, &after, false), Vec::<String>::new());
}

#[test]
fn match_arm_block_of_another_shape_is_significant() {
    for block in [
        "{ let y = *x; y }",
        "{ *x; }",
        "unsafe { g(x) }",
        "'a: { g(x) }",
        "#[allow(unused)] { g(x) }",
        "{ #![allow(unused)] g(x) }",
        "{ #[allow(unused)] g(x) }",
        "{ g! { x } }",
        "async { g(x) }",
        "{ { g(x) } }",
    ] {
        let before = fn_body(&format!(
            "match v.first() {{ Some(x) => {block}, None => 0 }}"
        ));
        let after = fn_body("match v.first() { Some(x) => g(x), None => 0 }");
        has(&same_file(&before, &after, false), "tokens changed");
    }
}

#[test]
fn std_macro_trailing_commas_are_not_significant() {
    let before = fn_body(
        "let w = vec![1, 2,]; assert_eq!(w, vec![1, 2,],); debug_assert!(w.len() == 2,); \
         let _ = format!(\"{}\", w.len(),); println!(\"{w:?}\",); \
         assert!(matches!(w[0], 1 | 2 if true,), \"{}\", 1,); matches!(w[1], Some(ref x),)",
    );
    let after = fn_body(
        "let w = vec![1, 2]; assert_eq!(w, vec![1, 2]); debug_assert!(w.len() == 2); \
         let _ = format!(\"{}\", w.len()); println!(\"{w:?}\"); \
         assert!(matches!(w[0], 1 | 2 if true), \"{}\", 1); matches!(w[1], Some(ref x))",
    );
    assert_eq!(same_file(&before, &after, false), Vec::<String>::new());
}

#[test]
fn trailing_comma_of_other_macros_is_significant() {
    for (before, after) in [
        ("m!(vec![1,])", "m!(vec![1])"),
        ("std::vec![1,]", "std::vec![1]"),
        (
            "vec![1,]; macro_rules! vec { ($a:expr,) => {}; }",
            "vec![1]; macro_rules! vec { ($a:expr,) => {}; }",
        ),
        ("my_assert!(true,)", "my_assert!(true)"),
    ] {
        has(
            &same_file(&fn_body(before), &fn_body(after), false),
            "tokens changed",
        );
    }
    let shadow = "macro_rules! vec {\n    ($a:expr,) => {\n        1\n    };\n}\n\n";
    has(
        &same_file(
            &format!("{shadow}{}", fn_body("vec![1,]")),
            &format!("{shadow}{}", fn_body("vec![1]")),
            false,
        ),
        "tokens changed",
    );
    let import = "use other::vec;\n\n";
    has(
        &same_file(
            &format!("{import}{}", fn_body("vec![1,]")),
            &format!("{import}{}", fn_body("vec![1]")),
            false,
        ),
        "tokens changed",
    );
}

#[test]
fn committed_manifests_are_not_outside_the_tree() {
    let spec = Spec {
        manifest: None,
        old_root: "src/k.rs".into(),
        new_root: "src/k.rs".into(),
        module: "k".into(),
        delegate: false,
    };
    let src = mem(&[("src/k.rs", "fn f() {}\n")]);
    let touched = |p: &str| check(&spec, &src, &src, &[p.to_string()]).unwrap().1;
    assert_eq!(
        touched("tools/split/manifests/k.toml"),
        Vec::<String>::new()
    );
    assert_eq!(
        touched("tools/split/manifests/k.paths"),
        Vec::<String>::new()
    );
    has(
        &touched("tools/split/manifests/k.rs"),
        "outside the moved tree",
    );
    has(&touched("src/other.rs"), "outside the moved tree");
}

#[test]
fn apply_writes_a_file_form_root() {
    let (errs, repo) = apply_and_verify_as(
        "file-form",
        &[("src/a.rs", WITH_CHILD), ("src/a/helpers.rs", HELPERS)],
        [
            (&["mod helpers"], &["pub use inner::*;"]),
            (&["fn f"], &["use super::*;"]),
        ],
        vec![],
        crate::apply::RootForm::File,
    )
    .unwrap();
    assert_eq!(errs, Vec::<String>::new());
    assert!(repo.join("src/a.rs").exists() && repo.join("src/a/inner.rs").exists());
    assert!(!repo.join("src/a/mod.rs").exists());
}

#[test]
fn layout_inside_std_macro_arguments_is_not_significant() {
    let before = fn_body(
        "let w = vec![(\"a\", g(1, &[2, 3,],), |x| { x },),]; \
         assert!(matches!(w[0], (\"a\", S { a, .. },),))",
    );
    let after = fn_body(
        "let w = vec![(\"a\", g(1, &[2, 3]), |x| x)]; \
         assert!(matches!(w[0], (\"a\", S { a, .. })))",
    );
    assert_eq!(same_file(&before, &after, false), Vec::<String>::new());
    has(
        &same_file(&fn_body("m!(g(1, 2,))"), &fn_body("m!(g(1, 2))"), false),
        "tokens changed",
    );
    has(
        &same_file(&fn_body("vec![(1,)]"), &fn_body("vec![(1)]"), false),
        "tokens changed",
    );
}

fn same_file_in(crate_files: &[(&str, &str)], before: &str, after: &str) -> Vec<String> {
    let spec = Spec {
        manifest: None,
        old_root: "src/k.rs".into(),
        new_root: "src/k.rs".into(),
        module: "k".into(),
        delegate: false,
    };
    let side = |k: &str| {
        let mut v = crate_files.to_vec();
        v.push(("src/k.rs", k));
        mem(&v)
    };
    check(&spec, &side(before), &side(after), &[]).unwrap().1
}

const CARGO: (&str, &str) = ("Cargo.toml", "[package]\nname = \"k\"\n");

#[test]
fn std_macro_name_from_elsewhere_in_the_crate_is_exact() {
    let (before, after) = (fn_body("vec![1,]"), fn_body("vec![1]"));
    assert_eq!(
        same_file_in(&[CARGO, ("src/lib.rs", "mod k;\n")], &before, &after),
        Vec::<String>::new()
    );
    for lib in [
        "#[macro_use]\nextern crate custom;\n\nmod k;\n",
        "#[cfg_attr(all(), macro_use)]\npub(crate) extern crate custom;\n\nmod k;\n",
        "mod k;\n\nmod other {\n    macro_rules! vec {\n        ($a:expr,) => {\n            1\n        };\n    }\n}\n",
    ] {
        has(
            &same_file_in(&[CARGO, ("src/lib.rs", lib)], &before, &after),
            "tokens changed",
        );
    }
}

#[test]
fn std_macro_name_imported_inside_the_leaf_is_exact() {
    for import in [
        "use other::vec;",
        "use other::{x, vec as vec};",
        "use other::*;",
    ] {
        has(
            &same_file(
                &fn_body(&format!("{{ {import} vec![1,] }}")),
                &fn_body(&format!("{{ {import} vec![1] }}")),
                false,
            ),
            "tokens changed",
        );
    }
}

#[test]
fn cfg_attr_on_a_wrapper_may_disable_a_macro() {
    let p = "#[macro_use]\nmod p {\n    macro_rules! m {\n        () => { 1 };\n    }\n}\n\n";
    let q = "#[macro_use]\nmod q {\n    macro_rules! m {\n        () => { 2 };\n    }\n}\n\n";
    let tail = "#[cfg_attr(all(), cfg(any()))]\n#[macro_use]\nmod disabled {\n    macro_rules! m {\n        () => { 3 };\n    }\n}\n\nfn f() -> u32 {\n    m!()\n}\n";
    has(
        &same_file(&format!("{p}{q}{tail}"), &format!("{q}{p}{tail}"), false),
        "macro resolution changed",
    );
}

const SWAP_BEFORE: &str = "mod one {\n    pub fn helper() -> u32 {\n        1\n    }\n}\n\nmod two {\n    pub fn helper() -> u32 {\n        2\n    }\n}\n\nuse one::helper;\n\nfn f() -> u32 {\n    BODY\n}\n";
const SWAP_TABLE: &str = "a::one::fn helper => a::two::fn helper\na::two::fn helper => a::one::fn helper\na::fn f => a::inner::fn f\n";

fn swap(tag: &str, body: &str, swapped: bool) -> Vec<String> {
    let before = SWAP_BEFORE.replace("BODY", body);
    let (x, y) = if swapped { ("2", "1") } else { ("1", "2") };
    let mod_rs = format!(
        "mod inner;\npub use inner::*;\n\nmod one {{\n    pub fn helper() -> u32 {{\n        {x}\n    }}\n}}\n\nmod two {{\n    pub fn helper() -> u32 {{\n        {y}\n    }}\n}}\n\nuse one::helper;\n"
    );
    let inner = format!("use super::*;\n\npub(super) fn f() -> u32 {{\n    {body}\n}}\n");
    let table = if swapped {
        SWAP_TABLE.to_string()
    } else {
        "a::one::fn helper => a::one::fn helper\na::two::fn helper => a::two::fn helper\na::fn f => a::inner::fn f\n".to_string()
    };
    split(
        tag,
        &before,
        &[("src/a/mod.rs", &mod_rs), ("src/a/inner.rs", &inner)],
        &table,
        [&["pub use inner::*;"], &["use super::*;"]],
    )
}

#[test]
fn import_referring_to_another_item_fails() {
    assert_eq!(swap("swap-ok", "helper()", false), Vec::<String>::new());
    has(
        &swap("swap-use", "helper()", true),
        "name `helper` resolves differently",
    );
    assert_eq!(
        swap("swap-path-ok", "one::helper()", false),
        Vec::<String>::new()
    );
    has(
        &swap("swap-path", "one::helper()", true),
        "path `one::helper` resolves differently",
    );
    let local = "{ use one::helper as h; h() }";
    assert_eq!(swap("swap-local-ok", local, false), Vec::<String>::new());
    has(
        &swap("swap-local", local, true),
        "path `one::helper` resolves differently",
    );
}

fn same_file_info(before: &str, after: &str) -> (Vec<String>, Vec<String>) {
    let spec = Spec {
        manifest: None,
        old_root: "src/k.rs".into(),
        new_root: "src/k.rs".into(),
        module: "k".into(),
        delegate: false,
    };
    check(
        &spec,
        &mem(&[("src/k.rs", before)]),
        &mem(&[("src/k.rs", after)]),
        &[],
    )
    .unwrap()
}

#[test]
fn positional_code_at_a_new_position_is_refused_outside_tests() {
    for body in [
        "line!()",
        "column!()",
        "file!().len() as u32",
        "std::panic::Location::caller().line()",
    ] {
        let before = format!("fn f() -> u32 {{\n    {body}\n}}\n");
        has(
            &same_file_info(&before, &format!("\n\n{before}")).1,
            "production code with line!",
        );
        assert_eq!(same_file_info(&before, &before).1, Vec::<String>::new());
    }
    let tracked =
        "#[track_caller]\nfn site() -> u32 {\n    std::panic::Location::caller().line()\n}\n\n";
    let before = format!("{tracked}fn f() -> u32 {{\n    site()\n}}\n");
    let after = format!("{tracked}\n\nfn f() -> u32 {{\n    site()\n}}\n");
    let errs = same_file_info(&before, &after).1;
    has(&errs, "fn f: production code with line!");
    assert!(errs.iter().all(|e| !e.contains("fn site")), "{errs:?}");
    let test = "#[cfg(test)]\nmod tests {\n    fn t() -> u32 {\n        line!()\n    }\n}\n";
    let (info, errs) = same_file_info(test, &format!("\n\n{test}"));
    assert_eq!(errs, Vec::<String>::new());
    has(&info, "1 test leaves with line!");
}

/// `fn f` moved from `a` (or `a::<from>`) to `a::inner`, with the
/// manifest's `[locations]` and `[log_targets]`.
fn moved_from(
    tag: &str,
    from: &str,
    body: &str,
    locations: &[(&str, &str)],
    log_targets: &[(&str, &str)],
) -> (Vec<String>, Vec<String>) {
    let mut m = manifest(tag, vec![]);
    let (old, before) = if from.is_empty() {
        (
            "a::fn f".to_string(),
            format!("fn f() {{\n    {body}\n}}\n"),
        )
    } else {
        (
            format!("a::{from}::fn f"),
            format!("mod {from} {{\n    pub(super) fn f() {{\n        {body}\n    }}\n}}\n"),
        )
    };
    std::fs::write(m.table_path(), format!("{old} => a::inner::fn f\n")).unwrap();
    m.visibility = [("fn f".to_string(), "pub(super)".to_string())].into();
    let table = |t: &[(&str, &str)]| {
        t.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    m.locations = table(locations);
    m.log_targets = table(log_targets);
    let inner = format!("use super::*;\n\npub(super) fn f() {{\n    {body}\n}}\n");
    let base = mem(&[("src/a.rs", &before)]);
    let head = mem(&[
        ("src/a/mod.rs", "mod inner;\npub use inner::*;\n"),
        ("src/a/inner.rs", &inner),
    ]);
    check(&spec_of(&m), &base, &head, &[]).unwrap()
}

fn moved_f(tag: &str, body: &str, locations: &[(&str, &str)]) -> Vec<String> {
    moved_from(tag, "", body, locations, &[]).1
}

const MODULE_SENSITIVE: [&str; 6] = [
    "let _ = module_path!();",
    "log::info!(\"x\");",
    "warn!(\"x {}\", 1);",
    "log::log!(log::Level::Info, \"x\");",
    "{ use log::info as note; note!(\"x\"); }",
    "{ use std::module_path as here; let _ = here!(); }",
];

#[test]
fn module_sensitive_code_moved_to_a_descendant_module_is_counted() {
    for body in MODULE_SENSITIVE {
        let (info, errs) = moved_from("modp-desc", "", body, &[], &[]);
        assert_eq!(errs, Vec::<String>::new(), "{body}");
        has(
            &info,
            "log targets: 1 leaves moved to descendant modules (prefix filters preserved)",
        );
    }
}

#[test]
fn module_sensitive_code_in_a_non_descendant_module_is_refused_outside_tests() {
    for body in MODULE_SENSITIVE {
        has(
            &moved_from("modp", "x", body, &[], &[]).1,
            "module_path!/log macros without target:",
        );
    }
    assert_eq!(
        moved_from(
            "modp-target",
            "x",
            "log::info!(target: \"a\", \"x\");",
            &[],
            &[]
        )
        .1,
        Vec::<String>::new()
    );
    assert_eq!(
        moved_from(
            "modp-exc",
            "x",
            "log::info!(\"x\");",
            &[],
            &[("x::fn f", "target change accepted")]
        )
        .1,
        Vec::<String>::new()
    );
    // A positional exception does not cover a log target change.
    has(
        &moved_from(
            "modp-wrong-table",
            "x",
            "log::info!(\"x\");",
            &[("x::fn f", "x")],
            &[],
        )
        .1,
        "module_path!/log macros without target:",
    );
    has(
        &moved_from("modp-unused", "x", "let _ = 1;", &[], &[("x::fn f", "x")]).1,
        "log target exception not needed",
    );
    has(
        &moved_f("pos-unused", "let _ = 1;", &[("fn f", "x")]),
        "location exception not needed",
    );
}

#[test]
fn std_macro_normalization_off_without_the_implicit_prelude() {
    let (before, after) = (fn_body("vec![1,]"), fn_body("vec![1]"));
    for lib in [
        "#![no_implicit_prelude]\nextern crate std;\n\nmod k;\n",
        "#![cfg_attr(all(), no_implicit_prelude)]\nextern crate std;\n\nmod k;\n",
        "mod k;\n\n#[no_implicit_prelude]\nmod other {}\n",
    ] {
        has(
            &same_file_in(&[CARGO, ("src/lib.rs", lib)], &before, &after),
            "tokens changed",
        );
    }
}

#[test]
fn std_macro_normalization_off_where_a_macro_may_expand_to_a_use() {
    // Codex's fixture: a macro-generated `use` binds `vec` to a macro whose
    // trailing comma matters.
    let import = "macro_rules! import {\n    () => {\n        use custom::vec;\n    };\n}\n\n";
    let lib = ("src/lib.rs", "mod k;\n");
    has(
        &same_file_in(
            &[CARGO, lib],
            &format!("{import}{}", fn_body("import!();\n    vec![1,]")),
            &format!("{import}{}", fn_body("import!();\n    vec![1]")),
        ),
        "tokens changed",
    );
    for (pre, body) in [
        ("", "custom::import!();\n    vec![1,]"),
        ("", "{ custom::import! {} }\n    vec![1,]"),
        ("", "#[allow(unused)]\n    import_ext!();\n    vec![1,]"),
        ("custom::import!();\n\n", "vec![1,]"),
        ("import!();\n\n", "vec![1,]"),
    ] {
        let before = format!("{pre}{}", fn_body(body));
        let after = before.replace("vec![1,]", "vec![1]");
        has(
            &same_file_in(&[CARGO, lib], &before, &after),
            "tokens changed",
        );
    }
    // An external item macro in an enclosing module outside the tree.
    has(
        &same_file_in(
            &[CARGO, ("src/lib.rs", "custom::import!();\n\nmod k;\n")],
            &fn_body("vec![1,]"),
            &fn_body("vec![1]"),
        ),
        "tokens changed",
    );
    // Known expression-only macros and in-tree macros without `use` keep it.
    let quiet = "macro_rules! quiet {\n    () => {\n        let _ = 1;\n    };\n}\n\n";
    for body in [
        "log::info!(\"x\");\n    vec![1,]",
        "assert!(true);\n    vec![1,]",
        "std::thread_local! {}\n    vec![1,]",
        "quiet!();\n    vec![1,]",
    ] {
        let before = format!("{quiet}{}", fn_body(body));
        let after = before.replace("vec![1,]", "vec![1]");
        assert_eq!(
            same_file_in(&[CARGO, lib], &before, &after),
            Vec::<String>::new(),
            "{body}"
        );
    }
}

#[test]
fn positional_code_is_found_through_aliases() {
    for before in [
        "use std::panic::Location as L;\n\nfn f() -> u32 {\n    L::caller().line()\n}\n",
        "use core::panic::Location as L;\n\nfn f() -> u32 {\n    L::caller().line()\n}\n",
        "mod m {\n    pub use std::panic::Location as Loc;\n}\n\nfn f() -> u32 {\n    m::Loc::caller().line()\n}\n",
        "use std::panic;\nuse panic::Location as P;\n\nfn f() -> u32 {\n    <P>::caller().line()\n}\n",
        "use std::line as here;\n\nfn f() -> u32 {\n    here!()\n}\n",
        "#[track_caller]\nfn site() -> u32 {\n    std::panic::Location::caller().line()\n}\n\nuse self::site as alias;\n\nfn f() -> u32 {\n    alias()\n}\n",
    ] {
        let after = before.replace("fn f()", "\n\nfn f()");
        let errs = same_file_info(before, &after).1;
        has(&errs, "fn f: production code with line!");
        assert!(errs.iter().all(|e| !e.contains("fn site")), "{errs:?}");
    }
}

#[test]
fn track_caller_fns_are_found_crate_wide() {
    let lib = ("src/lib.rs", "mod k;\nmod vk;\n");
    let before = fn_body("crate::vk::probe()");
    let after = format!("\n\n{before}");
    for vk in [
        "#[track_caller]\npub fn probe() -> u32 {\n    std::panic::Location::caller().line()\n}\n",
        "#[cfg_attr(debug_assertions, track_caller)]\npub fn probe() -> u32 {\n    std::panic::Location::caller().line()\n}\n",
        "#[track_caller]\npub fn probe() -> u32 {\n    inner()\n}\n\n#[track_caller]\nfn inner() -> u32 {\n    core::panic::Location::caller().line()\n}\n",
        "pub use self::deep::site as probe;\n\nmod deep {\n    #[track_caller]\n    pub fn site() -> u32 {\n        std::panic::Location::caller().line()\n    }\n}\n",
    ] {
        has(
            &same_file_in(&[CARGO, lib, ("src/vk.rs", vk)], &before, &after),
            "fn f: production code with line!",
        );
    }
    // Another workspace member's tracked fn, reached by path.
    let ws = [
        ("Cargo.toml", "[workspace]\nmembers = [\"k\", \"vk\"]\n"),
        ("k/Cargo.toml", "[package]\nname = \"k\"\n"),
        ("k/src/lib.rs", "mod k;\n"),
        ("vk/Cargo.toml", "[package]\nname = \"vk\"\n"),
        (
            "vk/src/lib.rs",
            "#[track_caller]\npub fn probe() -> u32 {\n    std::panic::Location::caller().line()\n}\n",
        ),
    ];
    let spec = Spec {
        manifest: None,
        old_root: "k/src/k.rs".into(),
        new_root: "k/src/k.rs".into(),
        module: "k".into(),
        delegate: false,
    };
    let side = |k: &str| {
        let mut v = ws.to_vec();
        v.push(("k/src/k.rs", k));
        mem(&v)
    };
    let before = fn_body("vk::probe()");
    let after = format!("\n\n{before}");
    has(
        &check(&spec, &side(&before), &side(&after), &[]).unwrap().1,
        "fn f: production code with line!",
    );
}

#[test]
fn split_root_declared_cfg_test_in_its_parent_file_is_test_code() {
    let mut m = manifest("cfg-test-root", vec![]);
    std::fs::write(m.table_path(), "a::fn f => a::inner::fn f\n").unwrap();
    m.visibility = BTreeMap::new();
    let lib = "#[cfg(test)]\nmod a;\n";
    let base = mem(&[
        ("src/lib.rs", lib),
        ("src/a.rs", "fn f() -> u32 {\n    line!()\n}\n"),
    ]);
    let head = mem(&[
        ("src/lib.rs", lib),
        ("src/a/mod.rs", "mod inner;\n"),
        (
            "src/a/inner.rs",
            "use super::*;\n\nfn f() -> u32 {\n    line!()\n}\n",
        ),
    ]);
    let (info, errs) = check(&spec_of(&m), &base, &head, &[]).unwrap();
    assert_eq!(errs, Vec::<String>::new());
    has(&info, "1 test leaves with line!");
    let plain = mem(&[
        ("src/lib.rs", "mod a;\n"),
        ("src/a.rs", "fn f() -> u32 {\n    line!()\n}\n"),
    ]);
    let plain_head = mem(&[
        ("src/lib.rs", "mod a;\n"),
        ("src/a/mod.rs", "mod inner;\n"),
        (
            "src/a/inner.rs",
            "use super::*;\n\nfn f() -> u32 {\n    line!()\n}\n",
        ),
    ]);
    has(
        &check(&spec_of(&m), &plain, &plain_head, &[]).unwrap().1,
        "fn f: production code with line!",
    );
}

#[test]
fn integration_test_root_splits_into_a_main_rs_directory() {
    let tmp = std::env::temp_dir().join(format!("split-test-{}-main-root", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("it.paths"),
        "fn helper => fn helper\nfn t => topic::fn t\n",
    )
    .unwrap();
    let text = "source = \"tests/it.rs\"\nmodule = \"\"\ndir = \"tests/it\"\nroot_form = \"main\"\ntable = \"it.paths\"\n\n[[modules]]\nname = \"\"\nitems = [\"fn helper\"]\n\n[[modules]]\nname = \"topic\"\nlines = [\"use super::*;\"]\nitems = [\"fn t\"]\n";
    std::fs::write(tmp.join("it.toml"), text).unwrap();
    let m = Manifest::load(&tmp.join("it.toml")).unwrap();
    assert_eq!(m.new_root(), "tests/it/main.rs");
    let before =
        "fn helper() -> u32 {\n    1\n}\n\n#[test]\nfn t() {\n    assert_eq!(helper(), 1);\n}\n";
    let base = mem(&[("tests/it.rs", before)]);
    let head = mem(&[
        (
            "tests/it/main.rs",
            "mod topic;\n\nfn helper() -> u32 {\n    1\n}\n",
        ),
        (
            "tests/it/topic.rs",
            "use super::*;\n\n#[test]\nfn t() {\n    assert_eq!(helper(), 1);\n}\n",
        ),
    ]);
    assert_eq!(
        check(&spec_of(&m), &base, &head, &[]).unwrap().1,
        Vec::<String>::new()
    );
}

/// `split apply` with a manifest given as TOML text, in a scratch repo, then
/// `verify` of the result.
fn apply_toml(tag: &str, files: &[(&str, &str)], toml: &str) -> Result<Vec<String>, String> {
    let repo = tmpdir(&format!("apply-toml-{tag}"));
    let _ = std::fs::remove_dir_all(&repo);
    for (p, t) in files {
        let p = repo.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, t).unwrap();
    }
    std::fs::write(repo.join("m.toml"), toml).unwrap();
    let m = Manifest::load(&repo.join("m.toml")).unwrap();
    crate::apply::run(&m, &repo)?;
    let errs = check(
        &spec_of(&m),
        &mem(files),
        &crate::tree::Disk(repo.clone()),
        &[],
    )
    .unwrap()
    .1;
    let inner = std::fs::read_to_string(repo.join("src/x/a/inner.rs")).unwrap();
    assert!(!inner.contains(") pub("), "{inner}");
    Ok(errs)
}

const REPLACE_SRC: &str =
    "pub(super) fn f() -> u32 {\n    1\n}\n\npub fn g() -> u32 {\n    f()\n}\n";

fn replace_manifest(vis: &str) -> String {
    format!(
        "source = \"src/x/a.rs\"\nmodule = \"x::a\"\ndir = \"src/x/a\"\ntable = \"a.paths\"\n\n[visibility]\n\"fn f\" = \"{vis}\"\n\n[[modules]]\nname = \"\"\nlines = [\"pub(in crate::x) use inner::*;\"]\nitems = [\"fn g\"]\n\n[[modules]]\nname = \"inner\"\nlines = [\"use super::*;\"]\nitems = [\"fn f\"]\n"
    )
}

#[test]
fn apply_replaces_an_existing_visibility() {
    let errs = apply_toml(
        "replace-vis",
        &[("src/x/a.rs", REPLACE_SRC)],
        &replace_manifest("pub(in crate::x)"),
    )
    .unwrap();
    assert_eq!(errs, Vec::<String>::new());
}

#[test]
fn replaced_visibility_that_widens_fails() {
    let errs = apply_toml(
        "replace-vis-widen",
        &[("src/x/a.rs", REPLACE_SRC)],
        &replace_manifest("pub(in crate)"),
    )
    .unwrap();
    has(&errs, "widened");
}

#[test]
fn unlisted_change_of_an_existing_visibility_fails() {
    let tmp = tmpdir("replace-vis-unlisted");
    std::fs::write(
        tmp.join("a.paths"),
        "x::a::fn f => x::a::inner::fn f\nx::a::fn g => x::a::fn g\n",
    )
    .unwrap();
    let toml = replace_manifest("pub(in crate::x)")
        .replace("[visibility]\n\"fn f\" = \"pub(in crate::x)\"\n", "");
    std::fs::write(tmp.join("m.toml"), toml).unwrap();
    let m = Manifest::load(&tmp.join("m.toml")).unwrap();
    let head = mem(&[
        (
            "src/x/a/mod.rs",
            "mod inner;\npub(in crate::x) use inner::*;\n\npub fn g() -> u32 {\n    f()\n}\n",
        ),
        (
            "src/x/a/inner.rs",
            "use super::*;\n\npub(in crate::x) fn f() -> u32 {\n    1\n}\n",
        ),
    ]);
    let errs = check(
        &spec_of(&m),
        &mem(&[("src/x/a.rs", REPLACE_SRC)]),
        &head,
        &[],
    )
    .unwrap()
    .1;
    has(&errs, "not allowed by the manifest");
}

#[test]
fn import_of_a_moved_fn_through_the_root_glob_is_not_a_trait() {
    let before = "fn f() -> u32 {\n    1\n}\n\nmod t {\n    use super::f;\n\n    fn g() -> u32 {\n        f()\n    }\n}\n";
    let mod_rs = "mod inner;\nuse inner::*;\n\nmod t {\n    use super::f;\n\n    fn g() -> u32 {\n        f()\n    }\n}\n";
    let inner = "use super::*;\n\npub(super) fn f() -> u32 {\n    1\n}\n";
    let errs = split(
        "glob-import-not-trait",
        before,
        &[("src/a/mod.rs", mod_rs), ("src/a/inner.rs", inner)],
        "a::fn f => a::inner::fn f\na::t::fn g => a::t::fn g\n",
        [&["use inner::*;"], &["use super::*;"]],
    );
    assert_eq!(errs, Vec::<String>::new());
}

#[test]
fn import_of_a_moved_trait_through_the_root_glob_still_counts() {
    let before =
        "trait f {}\n\nmod t {\n    use super::f;\n\n    fn g() -> u32 {\n        1\n    }\n}\n";
    let mod_rs = "mod inner;\nuse inner::*;\n\nmod t {\n    fn g() -> u32 {\n        1\n    }\n}\n";
    let inner = "use super::*;\n\npub(super) trait f {}\n";
    let errs = split(
        "glob-import-trait",
        before,
        &[("src/a/mod.rs", mod_rs), ("src/a/inner.rs", inner)],
        "a::trait f => a::inner::trait f\na::t::fn g => a::t::fn g\n",
        [&["use inner::*;"], &["use super::*;"]],
    );
    has(&errs, "traits in scope");
}
