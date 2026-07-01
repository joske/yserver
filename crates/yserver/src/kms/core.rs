//! `KmsCore` — protocol-bookkeeping state shared between
//! `KmsBackend` (v1) and `KmsBackendV2` (v2, lands in Stage 1b).
//!
//! Per the rendering-model-v2 spec (`docs/superpowers/specs/
//! 2026-05-15-rendering-model-v2.md` § "KmsCore scope — narrowly
//! drawn"), this module owns **only** protocol state that
//! describes what the X11 protocol says exists (XID maps, window
//! metadata stripped of storage, fonts as logical entities, etc.).
//! It does **not** own Vulkan images, GPU pipelines, scanout BOs,
//! or anything keyed by `vk::*` types — those stay in the
//! backend-specific structs.
//!
//! The litmus test: a hypothetical all-CPU yserver backend would
//! need every field here; it would not need anything in
//! `KmsBackend`'s LEAVE-column fields.
//!
//! Several types and helpers that were previously private to
//! `kms::backend` have moved here too, with widened visibility
//! (`pub(crate)`) so `KmsBackend`'s rendering code can still
//! reach them.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
};

use yserver_core::{
    backend::{
        ClipState, FillState, GcFunction, PixmapHandle, SubwindowMode,
        params::{ArcMode, CapStyle, JoinStyle, LineStyle},
    },
    host_x11::{HostPointerEvent, HostXidMap},
};
use yserver_protocol::x11::{
    CharInfo as ProtocolCharInfo, FontMetrics, FontPropValue, ResourceId, xfixes,
};

use crate::kms::cpu_types::{PictTransform, Rectangle16, Repeat};

// ───────────────────────────────────────────────────────────────
// `Send` newtype wrappers around `!Send` third-party types.
// The kms backend is driven from a single core thread, so manual
// `Send` impls are sound — but the wrapper makes that explicit.
// ───────────────────────────────────────────────────────────────

/// Newtype wrapper around `freetype::Face`.
/// `repr(transparent)` is required so `RefCell::as_ptr` can be safely cast
/// from `*mut FreetypeFace` to `*mut freetype::Face` in `render_text_string`.
/// SAFETY: All access is on the single-threaded core thread.
/// Single-threaded context makes this sound. `Face` contains raw pointers
/// and `Rc<Vec<u8>>` by default, both `!Send`.
#[repr(transparent)]
pub struct FreetypeFace(#[allow(dead_code)] pub freetype::Face);
// SAFETY: see doc comment above.
unsafe impl Send for FreetypeFace {}

/// Newtype wrapper around `xkb::Context`.
/// SAFETY: All access is on the single-threaded core thread.
/// The raw pointer in xkbcommon is not `Send`, but the C library is thread-safe.
pub struct XkbContext(pub xkbcommon::xkb::Context);
// SAFETY: see doc comment above.
unsafe impl Send for XkbContext {}

/// Newtype wrapper around `xkb::Keymap`.
/// SAFETY: All access is on the single-threaded core thread.
pub struct XkbKeymap(pub xkbcommon::xkb::Keymap);
// SAFETY: see doc comment above.
unsafe impl Send for XkbKeymap {}

/// Newtype wrapper around `xkb::State`.
/// SAFETY: All access is on the single-threaded core thread.
pub struct XkbState(pub xkbcommon::xkb::State);
// SAFETY: see doc comment above.
unsafe impl Send for XkbState {}

/// The resolved-component keyboard layout (rules/model/layout/variant/
/// options) the server currently has compiled. Stored so GetNames can
/// report an honest `symbolsName` and so a layout change can be diffed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XkbRmlvo {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: Option<String>,
}

impl Default for XkbRmlvo {
    fn default() -> Self {
        Self {
            rules: "evdev".into(),
            model: "pc105".into(),
            layout: "us".into(),
            variant: String::new(),
            options: None,
        }
    }
}

/// Pure RMLVO resolution: explicit `layout_arg` (from `-layout`) wins,
/// then the env-provided value, else the Xorg-style default. Rules/model
/// default to evdev/pc105.
pub(crate) fn resolve_rmlvo_from(
    rules_env: Option<String>,
    model_env: Option<String>,
    layout_env: Option<String>,
    variant_env: Option<String>,
    options_env: Option<String>,
    layout_arg: Option<String>,
) -> XkbRmlvo {
    XkbRmlvo {
        rules: rules_env
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "evdev".into()),
        model: model_env
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "pc105".into()),
        layout: layout_arg
            .filter(|s| !s.is_empty())
            .or(layout_env.filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "us".into()),
        variant: variant_env.unwrap_or_default(),
        options: options_env.filter(|s| !s.is_empty()),
    }
}

/// Read `XKB_DEFAULT_*` from the environment + the `-layout` arg.
pub(crate) fn resolve_startup_rmlvo(layout_arg: Option<String>) -> XkbRmlvo {
    resolve_rmlvo_from(
        std::env::var("XKB_DEFAULT_RULES").ok(),
        std::env::var("XKB_DEFAULT_MODEL").ok(),
        std::env::var("XKB_DEFAULT_LAYOUT").ok(),
        std::env::var("XKB_DEFAULT_VARIANT").ok(),
        std::env::var("XKB_DEFAULT_OPTIONS").ok(),
        layout_arg,
    )
}

// ───────────────────────────────────────────────────────────────
// Font protocol state (FontLoader + FontState + helpers).
// Pure protocol-domain: resolves X11 font names to FreeType faces
// via fontconfig. No Vulkan / GPU types reach in here.
// ───────────────────────────────────────────────────────────────

pub(crate) struct FontState {
    #[allow(dead_code)]
    pub(crate) handle: u32,
    pub(crate) face: RefCell<FreetypeFace>,
    pub(crate) metrics: FontMetrics,
    pub(crate) char_info_cache: HashMap<char, ProtocolCharInfo>,
    pub(crate) glyph_span_cache: RefCell<HashMap<char, Vec<Rectangle16>>>,
}

/// Resolves X11 font names (aliases like `fixed`, XLFDs like
/// `-adobe-helvetica-bold-r-*-*-12-*-...`, or family names) to a filesystem
/// path via fontconfig, then opens the file with FreeType.
///
/// `catalog` is the list of XLFDs we advertise via ListFonts /
/// ListFontsWithInfo. It is built once at init time by enumerating
/// fontconfig's installed-font set and synthesising one XLFD per
/// (face × pixel-size × charset) combination. Every entry resolves
/// back through `open_font`, so the LFWI metrics path can return real
/// FreeType metrics for any name we hand out.
/// One font-path directory: parsed `fonts.dir` (+ optional
/// `fonts.alias`). Names are kept verbatim (matching is
/// case-insensitive per the X11 spec).
#[derive(Debug, Clone)]
pub(crate) struct FontDir {
    /// fonts.dir entries in file order: (font name, glyph file path).
    pub(crate) entries: Vec<(String, std::path::PathBuf)>,
    /// fonts.alias entries: (alias, target font name or pattern).
    pub(crate) aliases: Vec<(String, String)>,
}

impl FontDir {
    /// Parse `<dir>/fonts.dir` (required) and `<dir>/fonts.alias`
    /// (optional). fonts.dir: first line = entry count, then
    /// `<file> <name>` per line (name may contain spaces — split at
    /// the FIRST space). fonts.alias: `<alias> <name>` with optional
    /// double quotes around either; `!` starts a comment line.
    pub(crate) fn load(dir: &std::path::Path) -> io::Result<Self> {
        let dir_listing = std::fs::read_to_string(dir.join("fonts.dir"))?;
        let mut entries = Vec::new();
        for line in dir_listing.lines().skip(1) {
            let line = line.trim_end();
            if let Some((file, name)) = line.split_once(' ') {
                if file.is_empty() || name.is_empty() {
                    continue;
                }
                entries.push((name.to_string(), dir.join(file)));
            }
        }
        let mut aliases = Vec::new();
        if let Ok(alias_text) = std::fs::read_to_string(dir.join("fonts.alias")) {
            for line in alias_text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('!') {
                    continue;
                }
                let (alias, rest) = match split_alias_token(line) {
                    Some(pair) => pair,
                    None => continue,
                };
                if let Some((target, _)) = split_alias_token(rest) {
                    aliases.push((alias.to_string(), target.to_string()));
                }
            }
        }
        Ok(Self { entries, aliases })
    }
}

/// Pull the next token off a fonts.alias line: either a
/// double-quoted string (quotes stripped) or a whitespace-delimited
/// word. Returns (token, remainder).
fn split_alias_token(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    if let Some(stripped) = s.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some((&stripped[..end], &stripped[end + 1..]))
    } else {
        match s.split_once(char::is_whitespace) {
            Some((tok, rest)) => Some((tok, rest)),
            None => Some((s, "")),
        }
    }
}

/// Case-insensitive `*`/`?` glob for X11 font name patterns.
/// Greedy backtracking matcher — font names and patterns are short.
pub(crate) fn font_pattern_matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let n: Vec<char> = name.to_ascii_lowercase().chars().collect();
    fn rec(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|k| rec(&p[1..], &n[k..])),
            Some('?') => !n.is_empty() && rec(&p[1..], &n[1..]),
            Some(c) => n.first() == Some(c) && rec(&p[1..], &n[1..]),
        }
    }
    rec(&p, &n)
}

/// Resolution outcome for a font name against the font path.
pub(crate) enum FontResolution {
    /// Matched a fonts.dir entry: open this file. `entry_name` is the
    /// matched fonts.dir name (used for XLFD-derived sizing of
    /// scalable files; PCF strikes ignore it).
    File {
        path: std::path::PathBuf,
        entry_name: String,
    },
    /// Matched the built-ins element (alias set or fontconfig
    /// catalog): resolve via fontconfig.
    BuiltIn,
}

pub(crate) struct FontLoader {
    pub(crate) library: freetype::Library,
    pub(crate) fc: fontconfig::Fontconfig,
    pub(crate) catalog: Vec<String>,
    /// Current font path, element order significant. Elements are
    /// directories (with fonts.dir) or the literal "built-ins".
    pub(crate) font_path: Vec<String>,
    /// Parsed dirs parallel to `font_path` (`None` = "built-ins").
    pub(crate) path_dirs: Vec<Option<FontDir>>,
}

/// Built-in alias names that always resolve via fontconfig — the
/// compatibility layer that keeps `fixed`/`cursor` working no matter
/// what the font path holds (mirrors Xorg's built-ins FPE).
pub(crate) const BUILTIN_ALIASES: &[&str] = &["fixed", "cursor", "nil2"];

impl FontLoader {
    pub(crate) fn new() -> io::Result<Self> {
        let fc = fontconfig::Fontconfig::new()
            .ok_or_else(|| io::Error::other("fontconfig init failed"))?;
        let catalog = build_font_catalog(&fc);
        log::info!("font catalog: {} XLFDs from fontconfig", catalog.len());
        let mut loader = Self {
            library: freetype::Library::init()
                .map_err(|e| io::Error::other(format!("freetype init failed: {e:?}")))?,
            fc,
            catalog,
            font_path: Vec::new(),
            path_dirs: Vec::new(),
        };
        let default = Self::default_font_path();
        // Default path elements are pre-vetted (fonts.dir checked) —
        // failure here would be a TOCTOU race; fall back to built-ins.
        if loader.set_font_path(&default).is_err() {
            let _ = loader.set_font_path(&["built-ins".to_string()]);
        }
        Ok(loader)
    }

    /// Xorg-style default font path filtered to dirs that actually
    /// carry a fonts.dir on this system, with "built-ins" always last.
    pub(crate) fn default_font_path() -> Vec<String> {
        const CANDIDATES: &[&str] = &[
            // Arch/Fedora layout (/usr/share/fonts/<subdir>) and the
            // Debian/Ubuntu layout (/usr/share/fonts/X11/<subdir>, where
            // xfonts-base/75dpi/100dpi install). The filter below drops
            // whichever set lacks a fonts.dir, so both distros work.
            "/usr/share/fonts/misc",
            "/usr/share/fonts/X11/misc",
            "/usr/share/fonts/TTF",
            "/usr/share/fonts/OTF",
            "/usr/share/fonts/Type1",
            "/usr/share/fonts/X11/Type1",
            "/usr/share/fonts/100dpi",
            "/usr/share/fonts/X11/100dpi",
            "/usr/share/fonts/75dpi",
            "/usr/share/fonts/X11/75dpi",
        ];
        let mut path: Vec<String> = CANDIDATES
            .iter()
            .filter(|d| std::path::Path::new(d).join("fonts.dir").is_file())
            .map(|d| (*d).to_string())
            .collect();
        path.push("built-ins".to_string());
        path
    }

    /// Validate and install a new font path. Every element must be
    /// "built-ins" or a directory with a readable fonts.dir; on any
    /// invalid element the old path is kept and Err carries the bad
    /// element (handler → BadValue). Empty list resets to default
    /// (Xorg SetFontPath semantics).
    pub(crate) fn set_font_path(&mut self, paths: &[String]) -> Result<(), String> {
        let effective: Vec<String> = if paths.is_empty() {
            Self::default_font_path()
        } else {
            paths.to_vec()
        };
        let mut dirs: Vec<Option<FontDir>> = Vec::with_capacity(effective.len());
        for el in &effective {
            if el == "built-ins" {
                dirs.push(None);
                continue;
            }
            match FontDir::load(std::path::Path::new(el)) {
                Ok(d) => dirs.push(Some(d)),
                Err(e) => {
                    log::debug!("SetFontPath: rejecting {el:?}: {e}");
                    return Err(el.clone());
                }
            }
        }
        self.font_path = effective;
        self.path_dirs = dirs;
        Ok(())
    }

    /// Resolve a font name/pattern against the font path in element
    /// order: exact fonts.dir name match (case-insensitive), alias
    /// match (recursive, ≤20 hops — Xorg dixfonts.c aliascount), then
    /// wildcard match against fonts.dir names; "built-ins" matches
    /// the alias set or the fontconfig catalog. None = BadName.
    pub(crate) fn resolve(&self, name: &str) -> Option<FontResolution> {
        self.resolve_inner(name, 20)
    }

    fn resolve_inner(&self, name: &str, hops: u8) -> Option<FontResolution> {
        if hops == 0 {
            return None;
        }
        for (el, dir) in self.font_path.iter().zip(&self.path_dirs) {
            let Some(dir) = dir else {
                // "built-ins": alias set, then catalog XLFD match.
                if BUILTIN_ALIASES.iter().any(|a| a.eq_ignore_ascii_case(name))
                    || self
                        .catalog
                        .iter()
                        .any(|entry| font_pattern_matches(name, entry))
                {
                    return Some(FontResolution::BuiltIn);
                }
                let _ = el;
                continue;
            };
            if let Some((entry_name, path)) = dir
                .entries
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(n, p)| (n.clone(), p.clone()))
            {
                return Some(FontResolution::File { path, entry_name });
            }
            if let Some((_, target)) = dir
                .aliases
                .iter()
                .find(|(a, _)| a.eq_ignore_ascii_case(name))
            {
                return self.resolve_inner(target, hops - 1);
            }
            if let Some((entry_name, path)) = dir
                .entries
                .iter()
                .find(|(n, _)| font_pattern_matches(name, n))
                .map(|(n, p)| (n.clone(), p.clone()))
            {
                return Some(FontResolution::File { path, entry_name });
            }
        }
        None
    }

    /// All fonts.dir/alias names on the current path matching
    /// `pattern`, in path order — feed for ListFonts ahead of the
    /// built-ins catalog. Aliases are reported by their alias name.
    pub(crate) fn path_font_names(&self, pattern: &str) -> Vec<String> {
        let mut out = Vec::new();
        for dir in self.path_dirs.iter().flatten() {
            for (name, _) in &dir.entries {
                if font_pattern_matches(pattern, name) {
                    out.push(name.clone());
                }
            }
            for (alias, _) in &dir.aliases {
                if font_pattern_matches(pattern, alias) {
                    out.push(alias.clone());
                }
            }
        }
        out
    }

    pub(crate) fn is_xlfd_pattern(name: &str) -> bool {
        name.starts_with('-')
    }

    /// Resolve a bare alias catalog entry ("fixed", "cursor", "nil2")
    /// to a full XLFD built from the resolved face's REAL metrics.
    ///
    /// `XCreateFontSet` clients need a charset to bind: libX11's XLC
    /// reads the XLFD from the `ListFontsWithInfo` reply name (or the
    /// `FONT` property), takes registry-encoding from the last two
    /// fields, and `OpenFont`s that name verbatim (traced against
    /// Xephyr — `tools/fontset-trace-xephyr.sh`). A bare alias name
    /// has no charset → the C-locale ISO8859-1 set can't bind → NULL
    /// fontset → e16 exits silently. The synthesized name round-trips
    /// through [`Self::open_font`]: `parse_xlfd` recovers the alias as
    /// the family (falling through fontconfig's monospace chain) and
    /// the pixel size.
    pub(crate) fn alias_to_xlfd(alias: &str, metrics: &FontMetrics) -> String {
        let px = i32::from(metrics.font_ascent)
            .saturating_add(i32::from(metrics.font_descent))
            .max(1);
        // XLFD AVERAGE_WIDTH is in tenths of pixels.
        let avg_w = i32::from(metrics.max_bounds.character_width).max(1) * 10;
        let spacing = if metrics.min_bounds.character_width == metrics.max_bounds.character_width {
            "c"
        } else {
            "p"
        };
        format!(
            "-misc-{alias}-medium-r-normal--{px}-{}-75-75-{spacing}-{avg_w}-iso8859-1",
            px * 10,
        )
    }

    /// Pull (family, style, pixel_size) hints out of an XLFD pattern.
    /// XLFD field indices after splitting on '-' (leading '-' produces an
    /// empty 0th element):
    ///   1=foundry 2=family 3=weight 4=slant 5=setwidth 6=addstyle
    ///   7=pixelsize 8=pointsize 9=resx 10=resy 11=spacing 12=avgwidth
    /// "*" or empty fields are treated as wildcards.
    pub(crate) fn parse_xlfd(name: &str) -> (Option<String>, Option<String>, Option<u32>) {
        let parts: Vec<&str> = name.split('-').collect();
        let take = |i: usize| -> Option<String> {
            parts
                .get(i)
                .filter(|s| !s.is_empty() && **s != "*")
                .map(|s| (*s).to_string())
        };
        let family = take(2);
        let weight = take(3);
        let slant = take(4);
        let style = match (weight.as_deref(), slant.as_deref()) {
            (None, Some("i" | "o")) => Some("Italic".to_string()),
            (Some(w), Some("i" | "o")) => Some(format!("{w} Italic")),
            (Some(w), _) => Some(w.to_string()),
            (None, _) => None,
        };
        let px = parts
            .get(7)
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|&s| s > 0);
        (family, style, px)
    }

    /// Open a font by name against the current font path. Resolution
    /// order: fonts.dir/alias path elements first, then "built-ins"
    /// (alias set + fontconfig catalog). `ErrorKind::NotFound` when
    /// nothing on the path matches — the request layer turns that
    /// into BadName (no silent substitution).
    pub(crate) fn open_font(
        &self,
        name: &str,
    ) -> io::Result<(freetype::Face, FontMetrics, HashMap<char, ProtocolCharInfo>)> {
        match self.resolve(name) {
            Some(FontResolution::File { path, entry_name }) => {
                self.open_font_file(&path, &entry_name)
            }
            Some(FontResolution::BuiltIn) => self.open_font_builtin(name),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no font on path matches {name:?}"),
            )),
        }
    }

    /// Open a fonts.dir-resolved glyph file (PCF/BDF/scalable —
    /// FreeType auto-detects). Bitmap faces select their (single)
    /// strike; scalable files honour the matched entry name's XLFD
    /// pixel size.
    fn open_font_file(
        &self,
        path: &std::path::Path,
        entry_name: &str,
    ) -> io::Result<(freetype::Face, FontMetrics, HashMap<char, ProtocolCharInfo>)> {
        let face = self
            .library
            .new_face(path, 0)
            .map_err(|e| io::Error::other(format!("freetype new_face({path:?}): {e:?}")))?;
        // PCF/BDF faces without a recognized registry (the xts test
        // fonts carry no CHARSET properties) get NO active charmap
        // from FreeType — FT_Get_First_Char then iterates nothing
        // and load_char can't map codes. Select the first charmap
        // explicitly; PCF files expose exactly one.
        if face.raw().charmap.is_null() && face.num_charmaps() > 0 {
            let cm = face.get_charmap(0);
            let _ = face.set_charmap(&cm);
        }
        if face.is_scalable() {
            let px = if Self::is_xlfd_pattern(entry_name) {
                Self::parse_xlfd(entry_name).2
            } else {
                None
            };
            if let Some(px) = px {
                let _ = face.set_pixel_sizes(0, px);
            } else {
                let _ = face.set_char_size(12 << 6, 12 << 6, 96, 96);
            }
        } else {
            // Bitmap strike (PCF/BDF): exactly one size per file in
            // X11 font dirs.
            let _ = face.select_size(0);
        }
        let pcf = pcf_file_info(path);
        // Ink metrics iff the PCF carries the table; BDF files always
        // (Xorg's bdf reader computes ink unconditionally); scalable
        // faces use cell metrics (old behavior).
        let use_ink = match &pcf {
            Some(info) => info.has_ink_metrics,
            None => !face.is_scalable(),
        };
        let (mut metrics, char_cache) = compute_font_metrics(&face, use_ink);
        // PCF authoritative overrides — Xorg's QueryFont serves
        // these from the file's tables, not from re-derivation:
        // default_char (encodings header), min/max_bounds + font
        // ascent/descent (accelerators, ink variants when present).
        if let Some(pcf) = pcf {
            if let Some(default_char) = pcf.default_char {
                metrics.default_char = default_char;
            }
            // min/max_bounds intentionally NOT taken from the accel
            // table: R6+ servers fold them from per-char metrics,
            // excluding all-zero entries (xtfont3/4's #if
            // XT_X_RELEASE==6 expectations) — compute_font_metrics
            // does exactly that.
            if let Some(a) = pcf.font_ascent {
                metrics.font_ascent = a;
            }
            if let Some(d) = pcf.font_descent {
                metrics.font_descent = d;
            }
        }
        Ok((face, metrics, char_cache))
    }

    fn open_font_builtin(
        &self,
        name: &str,
    ) -> io::Result<(freetype::Face, FontMetrics, HashMap<char, ProtocolCharInfo>)> {
        // Resolve the X11 font name to a file path via fontconfig. We can't
        // rely on the high-level `Fontconfig::find`: when the requested family
        // doesn't exist, fontconfig falls back to the *system default* (often
        // a proportional sans-serif), which makes xterm/wmaker render with
        // wrong metrics. Build the pattern ourselves and chain "monospace" as
        // a secondary family — fontconfig prefers the first listed family but
        // falls through the chain before reaching its system default.
        let (family, style, xlfd_px) = if Self::is_xlfd_pattern(name) {
            Self::parse_xlfd(name)
        } else {
            (Some(name.to_string()), None, None)
        };
        let query_family = family.as_deref().unwrap_or("monospace");

        let cfamily = std::ffi::CString::new(query_family)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "font name has nul"))?;

        // fontconfig 0.11 wraps these calls in `Result`; map the
        // error into this fn's io::Result. Matching behaviour is
        // unchanged — `font_match` still runs config+default substitute
        // internally.
        let fc_err = |e: fontconfig::FontconfigError| io::Error::other(format!("fontconfig: {e}"));
        let mut pat = fontconfig::Pattern::new(&self.fc).map_err(fc_err)?;
        pat.add_string(fontconfig::FC_FAMILY, &cfamily)
            .map_err(fc_err)?;
        if query_family != "monospace" {
            pat.add_string(fontconfig::FC_FAMILY, c"monospace")
                .map_err(fc_err)?;
        }
        let cstyle_storage;
        if let Some(style) = style.as_deref() {
            cstyle_storage = std::ffi::CString::new(style)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "font style has nul"))?;
            pat.add_string(fontconfig::FC_STYLE, &cstyle_storage)
                .map_err(fc_err)?;
        }
        let matched = pat.font_match().map_err(fc_err)?;
        let path = matched.filename().map_err(|_| {
            io::Error::new(io::ErrorKind::NotFound, format!("font not found: {name}"))
        })?;
        let face_index: isize = matched.face_index().unwrap_or(0) as isize;
        let face = self
            .library
            .new_face(path, face_index)
            .map_err(|e| io::Error::other(format!("freetype new_face({path}) failed: {e:?}")))?;

        // Honour XLFD PIXEL_SIZE if specified; otherwise default to 12pt @ 96dpi.
        if let Some(px) = xlfd_px {
            let _ = face.set_pixel_sizes(0, px);
        } else {
            let _ = face.set_char_size(12 << 6, 12 << 6, 96, 96);
        }
        let (metrics, char_cache) = compute_font_metrics(&face, false);
        Ok((face, metrics, char_cache))
    }
}

/// Per-char QueryFont metrics. Mirrors Xorg's table choice:
/// - `use_ink` (PCF with an INK_METRICS table, or BDF — Xorg's bdf
///   reader always computes ink): CharInfo = the inked bounding box
///   scanned off the monochrome bitmap (equals bdftopcf's ink
///   computation). Blank-but-existing glyphs (xtfont1 char 0) keep
///   their advance width with zero ink extents.
/// - otherwise (PCF without ink table, scalable faces): CharInfo =
///   the glyph cell (METRICS table / BBX shape — xtfont0's all-zero
///   10×10 C002 still reports rb 10, asc 10).
fn compute_char_info(face: &freetype::Face, ch: char, use_ink: bool) -> ProtocolCharInfo {
    let glyph_idx = ch as usize;
    let flags = if use_ink {
        freetype::face::LoadFlag::RENDER | freetype::face::LoadFlag::TARGET_MONO
    } else {
        freetype::face::LoadFlag::RENDER
    };
    let _ = face.load_char(glyph_idx, flags);
    let glyph = face.glyph();
    let bitmap = glyph.bitmap();
    let metrics = glyph.metrics();
    let width = (metrics.horiAdvance >> 6) as i16;

    if !use_ink {
        let left_side_bearing = (metrics.horiBearingX >> 6) as i16;
        let right_side_bearing = left_side_bearing + bitmap.width() as i16;
        let ascent = (metrics.horiBearingY >> 6) as i16;
        let descent = (bitmap.rows() as i16) - ascent;
        return ProtocolCharInfo {
            left_side_bearing,
            right_side_bearing,
            character_width: width,
            ascent,
            descent,
            attributes: 0,
        };
    }

    let w = bitmap.width() as usize;
    let h = bitmap.rows() as usize;
    let pitch = bitmap.pitch();
    let buf = bitmap.buffer();
    let mono = matches!(bitmap.pixel_mode(), Ok(freetype::bitmap::PixelMode::Mono));
    let set_at = |row: usize, col: usize| -> bool {
        let row_start = if pitch >= 0 {
            row * pitch as usize
        } else {
            (h - 1 - row) * (pitch as isize).unsigned_abs()
        };
        if mono {
            buf.get(row_start + (col >> 3)).copied().unwrap_or(0) & (0x80 >> (col & 7)) != 0
        } else {
            buf.get(row_start + col).copied().unwrap_or(0) >= 128
        }
    };
    // Ink bounding box in bitmap-local coords: (r0, r1, c0, c1).
    let mut ink: Option<(usize, usize, usize, usize)> = None;
    for row in 0..h {
        for col in 0..w {
            if set_at(row, col) {
                ink = Some(match ink {
                    None => (row, row, col, col),
                    Some((r0, r1, c0, c1)) => (r0.min(row), r1.max(row), c0.min(col), c1.max(col)),
                });
            }
        }
    }
    let Some((r0, r1, c0, c1)) = ink else {
        // No ink: advance only (X11 "exists" iff any field nonzero,
        // so a blank space-like glyph still exists via its width).
        return ProtocolCharInfo {
            left_side_bearing: 0,
            right_side_bearing: 0,
            character_width: width,
            ascent: 0,
            descent: 0,
            attributes: 0,
        };
    };
    let origin_x = glyph.bitmap_left() as i16; // bearing of bitmap col 0
    let top = glyph.bitmap_top() as i16; // rows above baseline of row 0
    ProtocolCharInfo {
        left_side_bearing: origin_x + c0 as i16,
        right_side_bearing: origin_x + c1 as i16 + 1,
        character_width: width,
        ascent: top - r0 as i16,
        descent: (r1 as i16 + 1) - top,
        attributes: 0,
    }
}

/// PCF file facts FreeType doesn't expose.
struct PcfFileInfo {
    /// BDF_ENCODINGS header default_char — bdftopcf moves
    /// DEFAULT_CHAR out of the property table.
    default_char: Option<u16>,
    /// Whether the file carries an INK_METRICS table (Xorg serves
    /// per-char ink metrics iff present; cell metrics otherwise).
    has_ink_metrics: bool,
    /// Accelerator-table font bounds. Xorg's QueryFont reply takes
    /// min/max_bounds from here (the INK variants when the
    /// accelerator format carries them), not from re-derivation.
    min_bounds: Option<ProtocolCharInfo>,
    max_bounds: Option<ProtocolCharInfo>,
    font_ascent: Option<i16>,
    font_descent: Option<i16>,
}

/// Parse the PCF table directory for [`PcfFileInfo`]. Returns None
/// for non-PCF/compressed/odd files.
fn pcf_file_info(path: &std::path::Path) -> Option<PcfFileInfo> {
    const PCF_ACCELERATORS: u32 = 1 << 1;
    const PCF_INK_METRICS: u32 = 1 << 4;
    const PCF_BDF_ENCODINGS: u32 = 1 << 5;
    const PCF_BDF_ACCELERATORS: u32 = 1 << 8;
    const PCF_ACCEL_W_INKBOUNDS: u32 = 0x0000_0100;
    let data = std::fs::read(path).ok()?;
    if data.get(0..4)? != b"\x01fcp" {
        return None;
    }
    let count = u32::from_le_bytes(data.get(4..8)?.try_into().ok()?);
    let mut info = PcfFileInfo {
        default_char: None,
        has_ink_metrics: false,
        min_bounds: None,
        max_bounds: None,
        font_ascent: None,
        font_descent: None,
    };
    let read_i16 = |off: usize, big: bool| -> Option<i16> {
        let raw: [u8; 2] = data.get(off..off + 2)?.try_into().ok()?;
        Some(if big {
            i16::from_be_bytes(raw)
        } else {
            i16::from_le_bytes(raw)
        })
    };
    let read_i32 = |off: usize, big: bool| -> Option<i32> {
        let raw: [u8; 4] = data.get(off..off + 4)?.try_into().ok()?;
        Some(if big {
            i32::from_be_bytes(raw)
        } else {
            i32::from_le_bytes(raw)
        })
    };
    // Uncompressed metrics entry: lsb, rsb, width, ascent, descent
    // (i16 each) + attributes (u16) = 12 bytes.
    let read_metrics = |off: usize, big: bool| -> Option<ProtocolCharInfo> {
        Some(ProtocolCharInfo {
            left_side_bearing: read_i16(off, big)?,
            right_side_bearing: read_i16(off + 2, big)?,
            character_width: read_i16(off + 4, big)?,
            ascent: read_i16(off + 6, big)?,
            descent: read_i16(off + 8, big)?,
            attributes: 0,
        })
    };
    let mut accel_off: Option<usize> = None;
    let mut bdf_accel_off: Option<usize> = None;
    for i in 0..count as usize {
        let base = 8 + 16 * i;
        let ttype = u32::from_le_bytes(data.get(base..base + 4)?.try_into().ok()?);
        let off = u32::from_le_bytes(data.get(base + 12..base + 16)?.try_into().ok()?) as usize;
        match ttype {
            PCF_INK_METRICS => info.has_ink_metrics = true,
            PCF_ACCELERATORS => accel_off = Some(off),
            PCF_BDF_ACCELERATORS => bdf_accel_off = Some(off),
            PCF_BDF_ENCODINGS => {
                let fmt = u32::from_le_bytes(data.get(off..off + 4)?.try_into().ok()?);
                let big = fmt & (1 << 2) != 0; // PCF_BYTE_MASK → MSB first
                // Encodings header: min/max byte2, min/max byte1,
                // default_char — five i16s after the format word.
                info.default_char = read_i16(off + 12, big).and_then(|v| u16::try_from(v).ok());
            }
            _ => {}
        }
    }
    // BDF_ACCELERATORS (post-encoding recompute) wins over the
    // plain table, matching Xorg's pcfReadFont preference.
    if let Some(off) = bdf_accel_off.or(accel_off) {
        let fmt = u32::from_le_bytes(data.get(off..off + 4)?.try_into().ok()?);
        let big = fmt & (1 << 2) != 0;
        // Layout after format: 8 flag/pad bytes, fontAscent i32,
        // fontDescent i32, maxOverlap i32, minbounds (12),
        // maxbounds (12), then ink variants when the accel format
        // has PCF_ACCEL_W_INKBOUNDS.
        info.font_ascent = read_i32(off + 12, big).and_then(|v| i16::try_from(v).ok());
        info.font_descent = read_i32(off + 16, big).and_then(|v| i16::try_from(v).ok());
        let bounds_off = off + 24;
        let (min_off, max_off) = if fmt & PCF_ACCEL_W_INKBOUNDS != 0 {
            (bounds_off + 24, bounds_off + 36)
        } else {
            (bounds_off, bounds_off + 12)
        };
        info.min_bounds = read_metrics(min_off, big);
        info.max_bounds = read_metrics(max_off, big);
    }
    Some(info)
}

/// FFI for FreeType's BDF/PCF property accessor — freetype-sys
/// doesn't bind it. Same libfreetype the bound calls use.
mod ft_bdf {
    use freetype::freetype_sys::{FT_Face, FT_Int};
    use std::os::raw::{c_char, c_int, c_long, c_ulong};

    pub const BDF_PROPERTY_TYPE_ATOM: c_int = 1;
    pub const BDF_PROPERTY_TYPE_INTEGER: c_int = 2;
    pub const BDF_PROPERTY_TYPE_CARDINAL: c_int = 3;

    #[repr(C)]
    pub union BdfPropertyValue {
        pub atom: *const c_char,
        pub integer: c_long,
        pub cardinal: c_ulong,
    }

    #[repr(C)]
    pub struct BdfPropertyRec {
        pub type_: c_int,
        pub u: BdfPropertyValue,
    }

    unsafe extern "C" {
        pub fn FT_Get_BDF_Property(
            face: FT_Face,
            prop_name: *const c_char,
            aproperty: *mut BdfPropertyRec,
        ) -> FT_Int;
    }
}

/// Best-effort read of the standard BDF/PCF property set off a face.
/// Empty for faces without embedded properties (scalables) — callers
/// fall back to XLFD-synthesized properties.
fn read_bdf_properties(face: &freetype::Face) -> Vec<(String, FontPropValue)> {
    // The classic property names XTS / legacy clients interrogate.
    const NAMES: &[&str] = &[
        "FOUNDRY",
        "FAMILY_NAME",
        "WEIGHT_NAME",
        "SLANT",
        "SETWIDTH_NAME",
        "ADD_STYLE_NAME",
        "PIXEL_SIZE",
        "POINT_SIZE",
        "RESOLUTION_X",
        "RESOLUTION_Y",
        "SPACING",
        "AVERAGE_WIDTH",
        "CHARSET_REGISTRY",
        "CHARSET_ENCODING",
        "FONT",
        "FONT_ASCENT",
        "FONT_DESCENT",
        "DEFAULT_CHAR",
        "COPYRIGHT",
        "MIN_SPACE",
        "NORM_SPACE",
        "MAX_SPACE",
        "END_SPACE",
        "SUPERSCRIPT_X",
        "SUPERSCRIPT_Y",
        "SUBSCRIPT_X",
        "SUBSCRIPT_Y",
        "UNDERLINE_POSITION",
        "UNDERLINE_THICKNESS",
        "ITALIC_ANGLE",
        "X_HEIGHT",
        "QUAD_WIDTH",
        "WEIGHT",
        "RESOLUTION",
        "CAP_HEIGHT",
    ];
    let mut out = Vec::new();
    let raw_face = face.raw() as *const _ as freetype::freetype_sys::FT_Face;
    for name in NAMES {
        let Ok(cname) = std::ffi::CString::new(*name) else {
            continue;
        };
        let mut prop = ft_bdf::BdfPropertyRec {
            type_: 0,
            u: ft_bdf::BdfPropertyValue { cardinal: 0 },
        };
        // SAFETY: face outlives the call; FT_Get_BDF_Property only
        // reads the face and fills `prop` on success (returns 0).
        let err = unsafe { ft_bdf::FT_Get_BDF_Property(raw_face, cname.as_ptr(), &mut prop) };
        if err != 0 {
            continue;
        }
        let value = match prop.type_ {
            ft_bdf::BDF_PROPERTY_TYPE_ATOM => {
                // SAFETY: ATOM type guarantees a NUL-terminated string
                // owned by the face.
                let s = unsafe {
                    let ptr = prop.u.atom;
                    if ptr.is_null() {
                        continue;
                    }
                    std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
                };
                FontPropValue::Str(s)
            }
            // SAFETY: tag-checked union reads.
            ft_bdf::BDF_PROPERTY_TYPE_INTEGER => {
                FontPropValue::Int(unsafe { prop.u.integer } as i32)
            }
            ft_bdf::BDF_PROPERTY_TYPE_CARDINAL => {
                FontPropValue::Card(unsafe { prop.u.cardinal } as u32)
            }
            _ => continue,
        };
        out.push(((*name).to_string(), value));
    }
    out
}

/// Compute QueryFont metrics from the face's REAL charmap coverage
/// (FT charcode iteration), not an assumed ASCII range. `char_infos`
/// is ordered with zeroed entries for missing codes (X11
/// "nonexistent character" = all-zero metrics).
///
/// Bitmap faces with codes above 255 use the 2-byte matrix model:
/// min/max_byte1 rows × min/max_byte2 columns, row-major (the xts
/// 2-byte xtfonts have NO row 0 at all). Scalable faces stay capped
/// at the single-byte range — fontconfig-backed unicode faces would
/// otherwise produce 64K-entry CharInfo grids on every QueryFont.
fn compute_font_metrics(
    face: &freetype::Face,
    use_ink: bool,
) -> (FontMetrics, HashMap<char, ProtocolCharInfo>) {
    let cap: u32 = if face.is_scalable() { 0xFF } else { 0xFFFF };
    let mut present: Vec<u32> = face
        .chars()
        .map(|(code, _gindex)| code as u32)
        .filter(|&c| c <= cap)
        .collect();
    present.sort_unstable();
    // Faces whose charmap is empty or entirely above the cap (symbol
    // maps): fall back to the ASCII probe range so QueryFont still
    // carries plausible bounds.
    if present.is_empty() {
        present = (0x20..=0x7E).collect();
    }
    let min_code = *present.first().unwrap_or(&0x20);
    let max_code = *present.last().unwrap_or(&0x7E);
    let present_set: HashSet<u32> = present.iter().copied().collect();

    // Grid shape: 1-byte fonts are one row (byte1 = 0, byte2 =
    // min..=max code); 2-byte fonts span byte1 rows × byte2 columns.
    let two_byte = max_code > 0xFF;
    let (min_byte1, max_byte1, min_byte2, max_byte2) = if two_byte {
        let min_b2 = present.iter().map(|c| c & 0xFF).min().unwrap_or(0);
        let max_b2 = present.iter().map(|c| c & 0xFF).max().unwrap_or(0xFF);
        (min_code >> 8, max_code >> 8, min_b2, max_b2)
    } else {
        (0, 0, min_code, max_code)
    };

    let mut char_info_cache = HashMap::new();
    let mut min_bounds = ProtocolCharInfo {
        left_side_bearing: i16::MAX,
        right_side_bearing: i16::MAX,
        character_width: i16::MAX,
        ascent: i16::MAX,
        descent: i16::MAX,
        attributes: 0,
    };
    let mut max_bounds = ProtocolCharInfo {
        left_side_bearing: i16::MIN,
        right_side_bearing: i16::MIN,
        character_width: i16::MIN,
        ascent: i16::MIN,
        descent: i16::MIN,
        attributes: 0,
    };

    let rows = max_byte1 - min_byte1 + 1;
    let cols = max_byte2 - min_byte2 + 1;
    let mut char_infos: Vec<ProtocolCharInfo> = Vec::with_capacity((rows * cols) as usize);
    let mut all_chars_exist = true;
    for b1 in min_byte1..=max_byte1 {
        for b2 in min_byte2..=max_byte2 {
            let code = (b1 << 8) | b2;
            let ch = char::from_u32(code);
            let exists = ch.is_some() && present_set.contains(&code);
            if !exists {
                // Nonexistent char: all-zero metrics per X11.
                char_infos.push(ProtocolCharInfo::default());
                all_chars_exist = false;
                continue;
            }
            let ch = ch.expect("checked above");
            let ci = compute_char_info(face, ch, use_ink);
            // Encoded-but-all-zero glyphs (xtfont3's DWIDTH-0 chars)
            // still EXIST (all_chars_exist unaffected — Xorg counts
            // encoding presence) but are excluded from the bounds
            // fold (R6 FontComputeInfoAccelerators semantics).
            if ci == ProtocolCharInfo::default() {
                char_infos.push(ci);
                char_info_cache.insert(ch, ci);
                continue;
            }
            min_bounds.left_side_bearing = min_bounds.left_side_bearing.min(ci.left_side_bearing);
            max_bounds.left_side_bearing = max_bounds.left_side_bearing.max(ci.left_side_bearing);
            min_bounds.right_side_bearing =
                min_bounds.right_side_bearing.min(ci.right_side_bearing);
            max_bounds.right_side_bearing =
                max_bounds.right_side_bearing.max(ci.right_side_bearing);
            min_bounds.character_width = min_bounds.character_width.min(ci.character_width);
            max_bounds.character_width = max_bounds.character_width.max(ci.character_width);
            min_bounds.ascent = min_bounds.ascent.min(ci.ascent);
            max_bounds.ascent = max_bounds.ascent.max(ci.ascent);
            min_bounds.descent = min_bounds.descent.min(ci.descent);
            max_bounds.descent = max_bounds.descent.max(ci.descent);
            char_infos.push(ci);
            char_info_cache.insert(ch, ci);
        }
    }
    if char_info_cache.is_empty() {
        min_bounds = ProtocolCharInfo::default();
        max_bounds = ProtocolCharInfo::default();
    }

    let named_properties = read_bdf_properties(face);
    // FONT_ASCENT/FONT_DESCENT properties are the authoritative
    // overall line metrics for bitmap fonts (can exceed the glyph
    // bound extremes); fall back to bounds-derived values.
    let prop_i16 = |name: &str| -> Option<i16> {
        named_properties.iter().find_map(|(n, v)| {
            (n == name).then(|| match v {
                FontPropValue::Card(c) => i16::try_from(*c).ok(),
                FontPropValue::Int(i) => i16::try_from(*i).ok(),
                FontPropValue::Str(_) => None,
            })?
        })
    };
    // FONT_ASCENT/FONT_DESCENT properties first; bitmap faces then
    // fall back to the PCF accelerator values FreeType surfaces as
    // size metrics (bdftopcf moves these OUT of the property table);
    // last resort = glyph-bound extremes. Scalable faces keep the
    // bounds-derived values (FT face ascender is typically larger
    // than the ASCII ink extremes — don't shift xterm row heights).
    let bitmap_face = !face.is_scalable();
    let sm = face.size_metrics().filter(|_| bitmap_face);
    let font_ascent = prop_i16("FONT_ASCENT")
        .or_else(|| sm.map(|m| (m.ascender >> 6) as i16))
        .unwrap_or(max_bounds.ascent);
    let font_descent = prop_i16("FONT_DESCENT")
        .or_else(|| sm.map(|m| (-m.descender >> 6) as i16))
        .unwrap_or(max_bounds.descent);
    let default_char = named_properties
        .iter()
        .find_map(|(n, v)| {
            (n == "DEFAULT_CHAR").then(|| match v {
                FontPropValue::Card(c) => u16::try_from(*c).ok(),
                FontPropValue::Int(i) => u16::try_from(*i).ok(),
                FontPropValue::Str(_) => None,
            })?
        })
        .unwrap_or_else(|| {
            if bitmap_face {
                // PCF: default_char lives in the encodings table —
                // open_font_file overrides from the file; 0 = "no
                // default" (Xorg's initial value).
                0
            } else if present_set.contains(&0x20) {
                0x20
            } else {
                u16::try_from(min_code).unwrap_or(0)
            }
        });

    let metrics = FontMetrics {
        min_bounds,
        max_bounds,
        min_char_or_byte2: u16::try_from(min_byte2).unwrap_or(0),
        max_char_or_byte2: u16::try_from(max_byte2).unwrap_or(0xFF),
        default_char,
        draw_direction: 0, // LeftToRight
        min_byte1: u8::try_from(min_byte1).unwrap_or(0),
        max_byte1: u8::try_from(max_byte1).unwrap_or(0),
        all_chars_exist,
        font_ascent,
        font_descent,
        properties: Vec::new(),
        named_properties,
        char_infos,
    };
    (metrics, char_info_cache)
}

/// Map a fontconfig integer `weight` (`FC_WEIGHT_*`) to the X11 XLFD
/// weight name. Buckets follow the canonical fontconfig→XLFD mapping
/// used by Xft and traditional X font path synthesis.
pub(crate) fn xlfd_weight(w: i32) -> &'static str {
    match w {
        ..=49 => "thin",
        50..=74 => "light",
        75..=89 => "book",
        90..=139 => "medium",
        140..=189 => "demibold",
        190..=209 => "bold",
        _ => "black",
    }
}

/// Map a fontconfig integer `slant` to the single-letter XLFD slant.
pub(crate) fn xlfd_slant(s: i32) -> &'static str {
    match s {
        100 => "i", // italic
        110 => "o", // oblique
        _ => "r",   // roman
    }
}

/// Map a fontconfig integer `spacing` to the single-letter XLFD spacing.
pub(crate) fn xlfd_spacing(s: i32) -> &'static str {
    match s {
        100 => "m", // monospaced
        110 => "c", // charcell
        _ => "p",   // proportional
    }
}

/// Make a fontconfig string safe to drop into a single XLFD field:
/// dashes are field separators in XLFDs, so any embedded `-` is
/// replaced with a space. XLFD is case-insensitive but conventionally
/// lowercase.
pub(crate) fn sanitize_xlfd_field(s: &str) -> String {
    s.replace('-', " ").to_lowercase()
}

/// Enumerate the installed font set via fontconfig and synthesise one
/// XLFD per (face × pixel-size × charset) combination. Every entry is
/// a real font that `FontLoader::open_font` can resolve back to a
/// `FreeType::Face` — there are no stub XLFDs.
///
/// Charsets are limited to `iso8859-1` and `iso10646-1` because those
/// two are the universal subset every scalable font supports through
/// FreeType's char-by-char lookup. Locale-specific charsets (jisx*,
/// gb2312*, ksc*) need real fonts on disk to satisfy properly; rather
/// than stub them, we let libXt warn "Missing charset X" and proceed
/// with the iso* coverage that's guaranteed real.
pub(crate) fn build_font_catalog(fc: &fontconfig::Fontconfig) -> Vec<String> {
    const PIXEL_SIZES: &[u32] = &[8, 10, 12, 14, 16, 18, 24];
    const CHARSETS: &[&str] = &["iso8859-1", "iso10646-1"];

    // Aliases the loader handles directly without an XLFD parse pass.
    let mut entries: Vec<String> = vec!["fixed".into(), "cursor".into(), "nil2".into()];

    // fontconfig 0.11 wraps construction/queries in `Result`; this fn
    // is infallible, so on any setup failure fall back to just the
    // alias entries rather than panicking at boot.
    let Ok(pat) = fontconfig::Pattern::new(fc) else {
        return entries;
    };
    let Ok(mut objs) = fontconfig::ObjectSet::new(fc) else {
        return entries;
    };
    for obj in [
        fontconfig::FC_FAMILY,
        fontconfig::FC_FOUNDRY,
        fontconfig::FC_WEIGHT,
        fontconfig::FC_SLANT,
        fontconfig::FC_SPACING,
    ] {
        let _ = objs.add(obj);
    }
    let Ok(set) = fontconfig::list_fonts(&pat, Some(&objs)) else {
        return entries;
    };

    let mut seen: HashSet<(String, String, i32, i32, i32)> = HashSet::new();

    for font in set.iter() {
        let Ok(family) = font.get_string(fontconfig::FC_FAMILY) else {
            continue;
        };
        let foundry = font.get_string(fontconfig::FC_FOUNDRY).unwrap_or("misc");
        let weight = font.get_int(fontconfig::FC_WEIGHT).unwrap_or(80);
        let slant = font.get_int(fontconfig::FC_SLANT).unwrap_or(0);
        let spacing = font.get_int(fontconfig::FC_SPACING).unwrap_or(0);

        let key = (
            family.to_lowercase(),
            foundry.to_lowercase(),
            weight,
            slant,
            spacing,
        );
        if !seen.insert(key) {
            continue;
        }

        let foundry_x = sanitize_xlfd_field(foundry);
        let family_x = sanitize_xlfd_field(family);
        let weight_x = xlfd_weight(weight);
        let slant_x = xlfd_slant(slant);
        let spacing_x = xlfd_spacing(spacing);

        for &px in PIXEL_SIZES {
            // Average width estimate: ~0.6 × pixel size, in 1/10 px.
            // Approximation only — clients filter by name pattern, not
            // by this field, and QueryFont returns the real widths.
            let avg_width = (px * 6).clamp(1, 999);
            for &charset in CHARSETS {
                entries.push(format!(
                    "-{foundry_x}-{family_x}-{weight_x}-{slant_x}-normal--{px}-{}-75-75-{spacing_x}-{avg_width}-{charset}",
                    px * 10,
                ));
            }
        }
    }

    entries
}

// ───────────────────────────────────────────────────────────────
// RENDER glyphset protocol state.
// Glyphset records keyed by host xid; the glyph bytes are CPU-side
// (uploaded later into v1's `glyph_atlas` or v2's `RenderEngine`).
// ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlyphSetFormat {
    A8,
    A1,
    /// ARGB32 source glyphs (Cairo's default for modern GTK3 themes
    /// using subpixel / colour-emoji rendering). On `AddGlyphs` we
    /// extract the alpha channel into a densely-packed A8 buffer and
    /// store the glyph as if it had been uploaded in A8 format — the
    /// downstream atlas + text pipeline path is identical from there
    /// on. Subpixel coverage detail and emoji colour are lost; glyph
    /// shape is preserved, which is enough for grayscale text.
    Argb32,
    Other,
}

pub(crate) struct StoredGlyph {
    pub(crate) width: u16,
    pub(crate) height: u16,
    /// RENDER wire field: top-left of bitmap relative to glyph origin.
    /// This is the *negative* of FreeType's bitmap_left.
    /// Draw at pen_x - x, pen_y - y.
    pub(crate) x: i16,
    pub(crate) y: i16,
    pub(crate) x_off: i16,
    /// Vertical pen advance. Parsed from wire for fidelity but unused —
    /// horizontal-text rendering only advances the x pen between glyphs.
    #[allow(dead_code)]
    pub(crate) y_off: i16,
    /// Row-major A8 bytes, densely packed (no per-row padding).
    pub(crate) pixels: Vec<u8>,
    pub(crate) format: GlyphSetFormat,
}

pub(crate) struct GlyphSetState {
    pub(crate) format: GlyphSetFormat,
    pub(crate) glyphs: HashMap<u32, StoredGlyph>,
}

// ───────────────────────────────────────────────────────────────
// RENDER picture records (CPU-only, Stage 3b).
//
// Per the v2 spec § "KmsCore scope", picture *records* (the
// protocol's idea of a picture: op, src/dst xid, transform,
// filter, clip, repeat, alpha-map) live in `KmsCore`. The Vk
// pipeline / sampler / image-view side belongs to
// `RenderEngine`'s `picture_paint` map. v1 keeps its own
// `KmsBackend.pictures: HashMap<u32, PictureState>` (which
// carries Vk-typed gradients) — these `PictureRecord` types
// are v2-only.
// ───────────────────────────────────────────────────────────────

/// One stop in a gradient. `pos` is X11 fixed-point 16.16;
/// colour channels are 16-bit straight (non-premultiplied) per
/// the X RENDER wire format. v2's `RenderEngine` premultiplies
/// at gradient-LUT build time. Layout-compatible with
/// `crate::kms::vk::gradient::Stop` (a deliberate mirror so the
/// engine can `mem::transmute`-equivalent the slice on
/// conversion; never actually transmuted but a layout-check
/// `#[allow(unused)]` guard sits next to the v1 type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GradientStop {
    pub(crate) pos: i32,
    pub(crate) r: u16,
    pub(crate) g: u16,
    pub(crate) b: u16,
    pub(crate) a: u16,
}

/// Stage 3b: protocol record of an X RENDER Picture. CPU-only;
/// every variant is `Copy`-friendly except for the small `Vec`s
/// (clip rects, gradient stops).
#[allow(
    dead_code,
    reason = "Gradient endpoint + stops fields are consumed by Stage 3c \
              when RenderEngine builds the gradient LUT"
)]
#[derive(Debug, Clone)]
pub(crate) enum PictureRecord {
    /// Picture wraps a Drawable (window or pixmap). Composite ops
    /// read pixels from the backing drawable's storage; paint ops
    /// (Composite as dst, FillRectangles, etc.) write into it.
    Drawable {
        /// XID of the backing window or pixmap.
        host_xid: u32,
        /// Drawable-space origin of the wrapped surface.
        /// Window-backed pictures need this to translate external
        /// region geometry into picture-local coordinates.
        drawable_origin: (i16, i16),
        /// X11 `RENDER_PICTFORMAT` ID requested at `CreatePicture`.
        /// Captures the client's *declared* sampling intent — e.g.
        /// marco creates a Picture wrapping a depth-24 backing with
        /// a 24-bit xRGB format, but a depth-32 backing with ARGB32.
        /// The drawable's depth alone is not enough to distinguish
        /// "no alpha" from "real alpha" cases (rare but real for
        /// X RENDER source-format flexibility). Currently
        /// instrumentation-only — the engine still chooses
        /// force-opaque via drawable depth; PictFormat-aware
        /// sampling lands as a follow-on once we have data showing
        /// where the depth-based heuristic diverges.
        pict_format: u32,
        /// Optional clip rectangles (set by
        /// `RenderSetPictureClipRectangles`). Stored in dst-coord
        /// space — the clip-origin has already been folded in.
        clip: Option<Vec<Rectangle16>>,
        clip_x: i16,
        clip_y: i16,
        repeat: Repeat,
        alpha_map: Option<u32>,
        alpha_x: i16,
        alpha_y: i16,
        component_alpha: bool,
        transform: Option<PictTransform>,
        /// X RENDER `RENDER_FILTER` enum. `Nearest` is the only
        /// honoured filter in Stage 3 (per spec § "Out of scope");
        /// `Bilinear`/`Convolution` parse + store but the
        /// `RenderEngine` ignores them at draw time.
        filter: PictureFilter,
        graphics_exposure: bool,
        subwindow_mode: u8,
        poly_edge: u8,
        poly_mode: u8,
    },
    /// `RenderCreateSolidFill` source. The X RENDER wire colour
    /// is **already premultiplied** (per the protocol spec, and
    /// confirmed by rendercheck `main.c:337-345`); v2 stores it
    /// as-is so the pipeline's sampler reads exactly what the
    /// client sent.
    SolidFill {
        premul: [f32; 4],
        repeat: Repeat,
        component_alpha: bool,
    },
    /// `RenderCreateLinearGradient` source. Stops are kept
    /// straight (non-premultiplied) — the v2 RenderEngine
    /// premultiplies when building the gradient LUT in Stage 3c.
    LinearGradient {
        /// Endpoints in X11 fixed-point 16.16 (`p1`, `p2`).
        p1: (i32, i32),
        p2: (i32, i32),
        stops: Vec<GradientStop>,
        repeat: Repeat,
        transform: Option<PictTransform>,
    },
    /// `RenderCreateRadialGradient` source. `(cx, cy, r)` for
    /// both the inner and outer circles, X11 fixed-point.
    RadialGradient {
        inner: (i32, i32, i32),
        outer: (i32, i32, i32),
        stops: Vec<GradientStop>,
        repeat: Repeat,
        transform: Option<PictTransform>,
    },
}

/// X RENDER picture filter selector. The protocol carries the
/// filter as a byte-string name; v2 maps the standard names into
/// this enum at request time. Unknown filters degrade to
/// `Nearest` per Stage 3's "only Nearest is honoured" rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PictureFilter {
    #[default]
    Nearest,
    Bilinear,
    /// Server-known convolution kernel selector. The kernel bytes
    /// live in the request's value-list; v2 doesn't apply them
    /// (Stage 5 territory), but parsing them as `Convolution`
    /// keeps the picture-record round-trip honest.
    Convolution,
}

impl PictureRecord {
    /// Construct a default `PictureRecord::Drawable` against
    /// `host_xid`. All optional / typed fields take their X RENDER
    /// protocol defaults; subsequent `RenderChangePicture` /
    /// `RenderSetPicture*` calls mutate them.
    pub(crate) fn drawable_default(host_xid: u32, pict_format: u32) -> Self {
        PictureRecord::Drawable {
            host_xid,
            drawable_origin: (0, 0),
            pict_format,
            clip: None,
            clip_x: 0,
            clip_y: 0,
            repeat: Repeat::None,
            alpha_map: None,
            alpha_x: 0,
            alpha_y: 0,
            component_alpha: false,
            transform: None,
            filter: PictureFilter::Nearest,
            graphics_exposure: false,
            subwindow_mode: 0,
            poly_edge: 0,
            poly_mode: 0,
        }
    }

    /// Convenience: the backing drawable host xid for a `Drawable`
    /// variant, or `None` for the source-only variants
    /// (SolidFill / Gradient). Used by `render_free_picture` to
    /// decide whether to call `DrawableStore::decref`.
    pub(crate) fn drawable_host_xid(&self) -> Option<u32> {
        if let PictureRecord::Drawable { host_xid, .. } = self {
            Some(*host_xid)
        } else {
            None
        }
    }
}

// ───────────────────────────────────────────────────────────────
// COMPOSITE redirect alias bookkeeping.
//
// `AliasRegistry::decref` returns `true` when the refcount hit zero,
// signalling the caller (backend-specific) that the underlying
// storage can be released. This struct only owns the protocol-side
// refcount + (width, height, depth) snapshot; it doesn't touch
// storage handles itself.
// ───────────────────────────────────────────────────────────────

/// One entry in the [`AliasRegistry`]: tracks the refcount on a
/// `Composite::NameWindowPixmap` backing pixmap, plus the
/// width/height/depth snapshot the alias was created against.
///
/// Refcount sources, per the L2 spec:
///
///   1. Active redirect on the matching window
///      (`Window.redirected_backing`).
///   2. Each live `NameWindowPixmap` alias on this backing.
///
/// The backing drops when refcount reaches zero — Unredirect
/// without aliases (1 → 0), DestroyWindow with surviving aliases
/// (1 → 0 from owner; aliases keep it alive), final FreePixmap of
/// the last alias (after Unredirect or Destroy).
#[allow(dead_code, reason = "width/height/depth consumed by B.6d on resize")]
#[derive(Clone, Copy, Debug)]
pub struct AliasEntry {
    pub refcount: u32,
    pub width: u16,
    pub height: u16,
    pub depth: u8,
}

/// Refcounted backing-pixmap registry on the KMS backend. Keyed by
/// the backing's `host_xid` (the `PixmapHandle` the protocol layer
/// sees through `Window.redirected_backing.host_pixmap`).
/// L2 plan B.3.
#[derive(Default, Debug)]
pub struct AliasRegistry {
    entries: HashMap<u32, AliasEntry>,
}

#[allow(
    dead_code,
    reason = "get/decref/len/is_empty consumed by B.6c (Unredirect teardown) + B.10c (overlay-demote quiescence); B.6d uses the width/height fields for resize check"
)]
impl AliasRegistry {
    /// Add the initial entry for a backing pixmap. Caller seeds the
    /// `refcount` (typically 1 for the redirect-activation hold).
    pub fn insert(&mut self, host_pixmap: PixmapHandle, entry: AliasEntry) {
        self.entries.insert(host_pixmap.as_raw(), entry);
    }

    /// Refcount-only lookup; returns `None` if the backing isn't
    /// tracked (e.g. the redirect was torn down).
    #[must_use]
    pub fn get(&self, host_pixmap: PixmapHandle) -> Option<&AliasEntry> {
        self.entries.get(&host_pixmap.as_raw())
    }

    /// Update the logical geometry metadata for a live backing
    /// without touching its refcount.
    pub fn update_geometry(
        &mut self,
        host_pixmap: PixmapHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) {
        if let Some(e) = self.entries.get_mut(&host_pixmap.as_raw()) {
            e.width = width;
            e.height = height;
            e.depth = depth;
        }
    }

    /// Bump the refcount. Silently no-ops on an unknown handle — the
    /// caller is expected to have inserted the entry first
    /// (`Composite::NameWindowPixmap`'s ordering guarantees that).
    pub fn incref(&mut self, host_pixmap: PixmapHandle) {
        if let Some(e) = self.entries.get_mut(&host_pixmap.as_raw()) {
            e.refcount = e.refcount.saturating_add(1);
        }
    }

    /// Drop one reference. Returns `true` if the entry was removed
    /// (refcount reached zero). Returns `false` on an unknown handle
    /// or when refs remain — caller uses the return value to decide
    /// whether to release the underlying Vulkan image.
    pub fn decref(&mut self, host_pixmap: PixmapHandle) -> bool {
        let key = host_pixmap.as_raw();
        let Some(entry) = self.entries.get_mut(&key) else {
            return false;
        };
        entry.refcount = entry.refcount.saturating_sub(1);
        if entry.refcount == 0 {
            self.entries.remove(&key);
            true
        } else {
            false
        }
    }

    /// Number of tracked backings. Mostly for test introspection.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True iff no backings are tracked. Used by overlay-demotion
    /// quiescence checks (B.10c).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ───────────────────────────────────────────────────────────────
// The shared `KmsCore` struct.
// ───────────────────────────────────────────────────────────────

/// Protocol-bookkeeping state shared between `KmsBackend` (v1) and
/// `KmsBackendV2` (v2). Owns *what the protocol says exists*, not
/// *how the backend stores it*.
pub(crate) struct KmsCore {
    // XID / ID maps
    pub(crate) xid_map: HostXidMap,
    pub(crate) next_host_xid: u32,
    pub(crate) window_id: u32,
    pub(crate) root_visual_xid: u32,
    pub(crate) top_level_order: Vec<u32>,

    // Keyboard protocol state
    #[allow(dead_code)]
    pub(crate) xkb_context: XkbContext,
    pub(crate) xkb_keymap: XkbKeymap,
    pub(crate) xkb_state: XkbState,
    // Read by GetNames to derive `symbolsName` from the active RMLVO.
    pub(crate) xkb_rmlvo: XkbRmlvo,
    /// Authoritative active keyboard group (0..=3). Tracked server-side
    /// (set by `XkbLatchLockState`), NOT derived from the master
    /// `xkb_state`. Stamped into the core-event `state` bits 13-14
    /// (`XkbGroupForCoreState`) and the XI2 `xXIGroupInfo` quartet.
    pub(crate) locked_group: u8,
    /// Cooked X11 keycodes currently pressed. Maintained in the key path
    /// so suspend can synthesize a release for each (xkbcommon::State
    /// cannot enumerate down keys).
    pub(crate) down_keys: HashSet<u8>,

    // Font protocol state
    pub(crate) font_loader: FontLoader,
    pub(crate) fonts: HashMap<u32, FontState>,

    // Root background metadata (paint into root storage is
    // backend-specific; the metadata that *what* the root
    // background should be lives here)
    pub(crate) bg_pixel: Option<u32>,
    pub(crate) bg_pixmap: Option<PixmapHandle>,

    // Pointer protocol state
    pub(crate) cursor_x: f32,
    pub(crate) cursor_y: f32,
    pub(crate) active_cursor: Option<u32>,
    pub(crate) button_mask: u16,
    pub(crate) prev_pointer_window: Option<u32>,
    pub(crate) pending_pointer_events: Vec<HostPointerEvent>,

    // Default GC state (the in-progress GC values feeding paint paths)
    pub(crate) current_font: Option<u32>,
    pub(crate) current_function: GcFunction,
    pub(crate) current_plane_mask: u32,
    pub(crate) current_foreground: u32,
    pub(crate) current_background: u32,
    pub(crate) current_fill: FillState,
    pub(crate) current_clip: ClipState,
    // Default: `ClipByChildren` per X11 spec § GC. When set,
    // core drawing ops on a window dst must exclude every
    // mapped child window's area (Stage 4d Manual-redirect
    // fix — relevant because v2 collapses an entire redirected
    // subtree into one backing pixmap, so parent paint can no
    // longer "miss" its own children by virtue of separate
    // storage).
    pub(crate) current_subwindow_mode: SubwindowMode,
    // Stroke state: snapshotted from the resolved DrawState on every
    // apply_draw_state call so the poly_line / poly_segment /
    // poly_rectangle / poly_arc dispatch sites can honour the GC's
    // line_width / line_style / cap_style / join_style / dashes /
    // dash_offset without re-resolving the GC.
    pub(crate) current_line_width: u16,
    pub(crate) current_line_style: LineStyle,
    pub(crate) current_cap_style: CapStyle,
    pub(crate) current_join_style: JoinStyle,
    pub(crate) current_dashes: Vec<u8>,
    pub(crate) current_dash_offset: u16,
    pub(crate) current_arc_mode: ArcMode,

    // SHAPE extension: per-window shape regions keyed by host XID.
    // None entry = no shape (full rectangle). Some(vec![]) = empty region.
    pub(crate) shape_bounding: HashMap<u32, Vec<xfixes::RegionRect>>, // kind=0
    pub(crate) shape_clip: HashMap<u32, Vec<xfixes::RegionRect>>,     // kind=1
    pub(crate) shape_input: HashMap<u32, Vec<xfixes::RegionRect>>,    // kind=2

    // COMPOSITE redirect records
    pub(crate) alias_registry: AliasRegistry,
    /// `host_window → backing_pixmap_handle` map populated by
    /// `allocate_redirected_backing`; `name_window_pixmap` looks
    /// up the matching backing here. Cleared when a redirect is
    /// torn down.
    pub(crate) host_window_to_backing: HashMap<u32, PixmapHandle>,
    /// Stage 4d — Composite Overlay Window refcount.
    ///
    /// Live count of outstanding `XComposite::GetOverlayWindow`
    /// holds. Bumped on every GET, decremented on every RELEASE;
    /// when it hits zero the backend drops the underlying COW
    /// storage. Protocol-side bookkeeping per the v2 plan
    /// §"`KmsCore` scope — narrowly drawn": this is metadata (a
    /// counter), not a storage handle. The `DrawableId` of the
    /// allocated COW storage lives on `KmsBackendV2.cow_id`.
    ///
    /// See Stage 4 plan §4d "Composite Overlay Window (COW) as
    /// first-class scene entry" + spec lines 453, 459, 470 for
    /// the protocol/storage split.
    pub(crate) cow_refcount: u32,

    // RENDER glyphset source data (atlas storage is backend-specific)
    pub(crate) glyphsets: HashMap<u32, GlyphSetState>,

    // RENDER picture records (Stage 3b, v2-only). v1 carries its
    // own `KmsBackend.pictures` with Vk-typed gradient state;
    // these are CPU-only and v2 reads/writes them directly.
    pub(crate) pictures: HashMap<u32, PictureRecord>,
}

impl KmsCore {
    /// Build the shared protocol-bookkeeping state for a real
    /// backend. Initialises the XKB context+keymap+state, the
    /// FontLoader (fontconfig + freetype), and seeds the root
    /// xid map entry.
    ///
    /// Cursor position seeds at the centre of `(fb_w, fb_h)` to
    /// match the input thread's initial pointer position
    /// (`LibinputThreadState::new`) — without this match, the
    /// first synthetic Enter event after window-map can carry a
    /// `(0, 0)` location, which trips GTK's gesture-drag anchor.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` on FontLoader init failure or XKB
    /// keymap construction failure.
    pub(crate) fn new(fb_w: u16, fb_h: u16, layout_arg: Option<String>) -> io::Result<Self> {
        let xkb_context = XkbContext(xkbcommon::xkb::Context::new(
            xkbcommon::xkb::CONTEXT_NO_FLAGS,
        ));
        let rmlvo = resolve_startup_rmlvo(layout_arg);
        // Try the resolved RMLVO; on failure fall back to a guaranteed-valid
        // us map AND reset the stored RMLVO so xkb_rmlvo always matches the
        // keymap actually compiled (coherence — a later GetNames reads it).
        let (keymap, rmlvo) = match xkbcommon::xkb::Keymap::new_from_names(
            &xkb_context.0,
            &rmlvo.rules,
            &rmlvo.model,
            &rmlvo.layout,
            &rmlvo.variant,
            rmlvo.options.clone(),
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        ) {
            Some(km) => (km, rmlvo),
            None => {
                log::warn!("xkb: startup RMLVO {rmlvo:?} failed to compile; falling back to us");
                let fallback = XkbRmlvo::default();
                let km = xkbcommon::xkb::Keymap::new_from_names(
                    &xkb_context.0,
                    &fallback.rules,
                    &fallback.model,
                    &fallback.layout,
                    &fallback.variant,
                    fallback.options.clone(),
                    xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
                )
                .ok_or_else(|| io::Error::other("failed to create xkb keymap"))?;
                (km, fallback)
            }
        };
        let xkb_state = XkbState(xkbcommon::xkb::State::new(&keymap));
        let xkb_keymap = XkbKeymap(keymap);

        let mut xid_map: HostXidMap = HashMap::new();
        xid_map.insert(0x0000_0001u32, ResourceId(0x0000_0100));

        Ok(Self {
            xid_map,
            next_host_xid: 0x0040_0000,
            window_id: 1,
            root_visual_xid: 0x21,
            top_level_order: Vec::new(),
            xkb_context,
            xkb_keymap,
            xkb_state,
            xkb_rmlvo: rmlvo,
            locked_group: 0,
            down_keys: HashSet::new(),
            font_loader: FontLoader::new()?,
            fonts: HashMap::new(),
            bg_pixel: None,
            bg_pixmap: None,
            cursor_x: f32::from(fb_w) / 2.0,
            cursor_y: f32::from(fb_h) / 2.0,
            active_cursor: None,
            button_mask: 0,
            prev_pointer_window: None,
            pending_pointer_events: Vec::new(),
            current_font: None,
            current_function: GcFunction::Copy,
            current_plane_mask: u32::MAX,
            current_foreground: 0,
            current_background: 0x00ff_ffff,
            current_fill: FillState::Solid,
            current_clip: ClipState::None,
            current_subwindow_mode: SubwindowMode::ClipByChildren,
            current_line_width: 0,
            current_line_style: LineStyle::Solid,
            current_cap_style: CapStyle::Butt,
            current_join_style: JoinStyle::Miter,
            current_dashes: vec![4, 4],
            current_dash_offset: 0,
            current_arc_mode: ArcMode::PieSlice,
            shape_bounding: HashMap::new(),
            shape_clip: HashMap::new(),
            shape_input: HashMap::new(),
            alias_registry: AliasRegistry::default(),
            host_window_to_backing: HashMap::new(),
            cow_refcount: 0,
            glyphsets: HashMap::new(),
            pictures: HashMap::new(),
        })
    }

    /// Headless test seed. Mirrors `new` but panics on init
    /// failure (FontLoader / XKB) since test fixtures shouldn't
    /// be running without these. Uses an 800×600 cursor centre
    /// matching `KmsBackend::for_tests`'s synthetic output.
    #[doc(hidden)]
    #[must_use]
    pub(crate) fn for_tests() -> Self {
        let xkb_context = XkbContext(xkbcommon::xkb::Context::new(
            xkbcommon::xkb::CONTEXT_NO_FLAGS,
        ));
        let keymap = xkbcommon::xkb::Keymap::new_from_names(
            &xkb_context.0,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .or_else(|| {
            xkbcommon::xkb::Keymap::new_from_names(
                &xkb_context.0,
                "",
                "",
                "",
                "",
                None,
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        })
        .expect("test xkb keymap");
        let xkb_state = XkbState(xkbcommon::xkb::State::new(&keymap));
        let xkb_keymap = XkbKeymap(keymap);

        Self {
            xid_map: HostXidMap::new(),
            next_host_xid: 0x0040_0000,
            window_id: 1,
            root_visual_xid: 0x21,
            top_level_order: Vec::new(),
            xkb_context,
            xkb_keymap,
            xkb_state,
            xkb_rmlvo: XkbRmlvo::default(),
            locked_group: 0,
            down_keys: HashSet::new(),
            font_loader: FontLoader::new().expect("test font loader"),
            fonts: HashMap::new(),
            bg_pixel: None,
            bg_pixmap: None,
            cursor_x: 400.0,
            cursor_y: 300.0,
            active_cursor: None,
            button_mask: 0,
            prev_pointer_window: None,
            pending_pointer_events: Vec::new(),
            current_font: None,
            current_function: GcFunction::Copy,
            current_plane_mask: u32::MAX,
            current_foreground: 0,
            current_background: 0x00ff_ffff,
            current_fill: FillState::Solid,
            current_clip: ClipState::None,
            current_subwindow_mode: SubwindowMode::ClipByChildren,
            current_line_width: 0,
            current_line_style: LineStyle::Solid,
            current_cap_style: CapStyle::Butt,
            current_join_style: JoinStyle::Miter,
            current_dashes: vec![4, 4],
            current_dash_offset: 0,
            current_arc_mode: ArcMode::PieSlice,
            shape_bounding: HashMap::new(),
            shape_clip: HashMap::new(),
            shape_input: HashMap::new(),
            alias_registry: AliasRegistry::default(),
            host_window_to_backing: HashMap::new(),
            cow_refcount: 0,
            glyphsets: HashMap::new(),
            pictures: HashMap::new(),
        }
    }

    /// Allocate the next host-XID. Monotonic counter starting at
    /// `0x0040_0000`. Wraps around at `u32::MAX` (in practice the
    /// counter never exceeds a few million in any real session).
    pub(crate) fn next_host_xid(&mut self) -> u32 {
        self.next_host_xid = self
            .next_host_xid
            .checked_add(1)
            .expect("host xid counter overflow");
        self.next_host_xid
    }

    /// Recompile the keyboard map from a new RMLVO and swap it in.
    ///
    /// Returns `Some((min_keycode, max_keycode))` of the new map on a
    /// successful change, or `None` if compilation failed (the old map is
    /// kept) or the RMLVO is byte-identical to the active one (no-op).
    ///
    /// A fresh `xkb_state` is built, then every physically-held key in
    /// `down_keys` is re-applied to it. The layout switch is typically
    /// triggered by a *hotkey* still held at swap time; without re-applying,
    /// the new state's modifier/group tracking diverges from the physical
    /// keyboard (the stuck-modifier class seen on the VT path).
    ///
    /// LIMITATION (deliberate): *locked* state (Caps Lock, a locked group)
    /// is reset by the rebuild — only physically-held keys are re-asserted.
    /// We do NOT restore locked mods/group via `update_mask`: xkbcommon
    /// documents `update_mask` as the lossy slave/wire entry point that
    /// "must not be used to update the master state" and "should not be used
    /// together" with `update_key`. Mixing it in would risk an incoherent
    /// master state for subsequent real key events. This matches the
    /// VT-acquire path's fresh-state behavior.
    pub(crate) fn recompile_keymap(&mut self, rmlvo: &XkbRmlvo) -> Option<(u8, u8)> {
        if *rmlvo == self.xkb_rmlvo {
            return None;
        }
        let keymap = xkbcommon::xkb::Keymap::new_from_names(
            &self.xkb_context.0,
            &rmlvo.rules,
            &rmlvo.model,
            &rmlvo.layout,
            &rmlvo.variant,
            rmlvo.options.clone(),
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )?;
        let (min_kc, max_kc) = crate::kms::xkb::clamped_keycode_bounds(&keymap);
        let mut new_state = xkbcommon::xkb::State::new(&keymap);
        for kc in &self.down_keys {
            new_state.update_key(
                xkbcommon::xkb::Keycode::new(u32::from(*kc)),
                xkbcommon::xkb::KeyDirection::Down,
            );
        }
        self.xkb_state = XkbState(new_state);
        self.xkb_keymap = XkbKeymap(keymap);
        self.xkb_rmlvo = rmlvo.clone();
        log::info!(
            "xkb: recompiled keymap -> rules={} model={} layout={} variant={:?} options={:?}",
            rmlvo.rules,
            rmlvo.model,
            rmlvo.layout,
            rmlvo.variant,
            rmlvo.options
        );
        Some((min_kc, max_kc))
    }
}

#[cfg(test)]
mod xkb_rmlvo_tests {
    use super::*;

    #[test]
    fn recompile_keymap_changes_keysym() {
        // External ground truth: physical Y key (keycode 29, "AD06") is `y`
        // on the us layout, `z` on de.
        let mut core = KmsCore::for_tests();
        let us_sym = core
            .xkb_state
            .0
            .key_get_one_sym(xkbcommon::xkb::Keycode::new(29));
        assert_eq!(us_sym, xkbcommon::xkb::keysyms::KEY_y.into());

        let changed = core.recompile_keymap(&XkbRmlvo {
            rules: "evdev".into(),
            model: "pc105".into(),
            layout: "de".into(),
            variant: String::new(),
            options: None,
        });
        assert!(changed.is_some(), "de keymap must compile");

        let de_sym = core
            .xkb_state
            .0
            .key_get_one_sym(xkbcommon::xkb::Keycode::new(29));
        assert_eq!(de_sym, xkbcommon::xkb::keysyms::KEY_z.into());
    }

    #[test]
    fn recompile_keymap_reconciles_held_keys() {
        // Shift_L is evdev keycode 50. Seed it as physically held, then
        // recompile to de and prove the held Shift survives the swap.
        let mut core = KmsCore::for_tests();
        core.down_keys.insert(50);

        let changed = core.recompile_keymap(&XkbRmlvo {
            rules: "evdev".into(),
            model: "pc105".into(),
            layout: "de".into(),
            variant: String::new(),
            options: None,
        });
        assert!(changed.is_some(), "de keymap must compile");

        assert!(
            core.xkb_state
                .0
                .mod_name_is_active("Shift", xkbcommon::xkb::STATE_MODS_EFFECTIVE),
            "held Shift must survive the keymap swap"
        );
    }

    #[test]
    fn resolve_rmlvo_prefers_inputs_over_defaults() {
        // Explicit values win; missing fall back to evdev/pc105/us.
        let r = resolve_rmlvo_from(None, None, Some("be".into()), None, None, None);
        assert_eq!(r.rules, "evdev");
        assert_eq!(r.model, "pc105");
        assert_eq!(r.layout, "be");

        // -layout arg overrides env layout (Xorg cmdline precedence).
        let r2 = resolve_rmlvo_from(None, None, Some("be".into()), None, None, Some("de".into()));
        assert_eq!(r2.layout, "de");

        // Nothing set -> us default.
        let r3 = resolve_rmlvo_from(None, None, None, None, None, None);
        assert_eq!(r3.layout, "us");

        // Empty strings are treated as unset.
        let r4 = resolve_rmlvo_from(
            Some(String::new()),
            None,
            Some(String::new()),
            None,
            None,
            None,
        );
        assert_eq!(r4.rules, "evdev");
        assert_eq!(r4.layout, "us");
    }
}

#[cfg(test)]
mod font_tests {
    use super::*;

    fn write_test_font_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("yserver-font-test-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // Minimal BDF — FreeType reads it like a PCF for our purposes.
        let bdf = "STARTFONT 2.1\nFONT testfont0\nSIZE 13 100 100\n\
                   FONTBOUNDINGBOX 10 10 0 0\nSTARTPROPERTIES 3\n\
                   DEFAULT_CHAR 1\nFONT_ASCENT 10\nFONT_DESCENT 0\nENDPROPERTIES\n\
                   CHARS 2\n\
                   STARTCHAR C001\nENCODING 1\nSWIDTH 570 0\nDWIDTH 10 0\n\
                   BBX 10 10 0 0\nBITMAP\nFFC0\nFFC0\nFFC0\nFFC0\nFFC0\nFFC0\nFFC0\nFFC0\nFFC0\nFFC0\nENDCHAR\n\
                   STARTCHAR C003\nENCODING 3\nSWIDTH 285 0\nDWIDTH 5 0\n\
                   BBX 5 5 0 0\nBITMAP\nF8\nF8\nF8\nF8\nF8\nENDCHAR\n\
                   ENDFONT\n";
        std::fs::write(dir.join("testfont0.bdf"), bdf).unwrap();
        std::fs::write(
            dir.join("fonts.dir"),
            "2\ntestfont0.bdf testfont0\ntestfont0.bdf -vsw-testfont-bold-r-normal--13-130-75-75-m-70-iso8859-1\n",
        )
        .unwrap();
        std::fs::write(dir.join("fonts.alias"), "myalias testfont0\n! comment\n").unwrap();
        dir
    }

    #[test]
    fn fonts_dir_parse_and_alias() {
        let dir = write_test_font_dir("parse");
        let fd = FontDir::load(&dir).unwrap();
        assert_eq!(fd.entries.len(), 2);
        assert_eq!(fd.entries[0].0, "testfont0");
        assert_eq!(
            fd.aliases,
            vec![("myalias".to_string(), "testfont0".to_string())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn font_pattern_glob() {
        assert!(font_pattern_matches("xtfont*", "xtfont0"));
        assert!(font_pattern_matches("XTFONT0", "xtfont0")); // case-insensitive
        assert!(font_pattern_matches(
            "-vsw-*-bold-r-*",
            "-vsw-testfont-bold-r-normal--13-130-75-75-m-70-iso8859-1"
        ));
        assert!(!font_pattern_matches("xtfont?", "xtfont"));
        assert!(!font_pattern_matches("nope", "xtfont0"));
    }

    #[test]
    fn resolution_order_and_bad_name() {
        let dir = write_test_font_dir("resolve");
        let mut loader = FontLoader::new().unwrap();
        loader
            .set_font_path(&[dir.to_string_lossy().into_owned(), "built-ins".into()])
            .unwrap();
        // exact name
        assert!(matches!(
            loader.resolve("testfont0"),
            Some(FontResolution::File { .. })
        ));
        // alias hop
        assert!(matches!(
            loader.resolve("MYALIAS"),
            Some(FontResolution::File { .. })
        ));
        // XLFD wildcard against fonts.dir name
        assert!(matches!(
            loader.resolve("-vsw-testfont-bold-r-*"),
            Some(FontResolution::File { .. })
        ));
        // built-ins alias survives any path
        assert!(matches!(
            loader.resolve("fixed"),
            Some(FontResolution::BuiltIn)
        ));
        // unknown bare name → None → BadName at the request layer
        assert!(loader.resolve("definitely-not-a-font").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_font_path_rejects_bad_dir_and_keeps_old() {
        let mut loader = FontLoader::new().unwrap();
        let before = loader.font_path.clone();
        let err = loader
            .set_font_path(&["/no-such-path-name".to_string()])
            .unwrap_err();
        assert_eq!(err, "/no-such-path-name");
        assert_eq!(loader.font_path, before, "old path kept on failure");
    }

    #[test]
    fn open_font_file_metrics_from_charmap() {
        let dir = write_test_font_dir("metrics");
        let mut loader = FontLoader::new().unwrap();
        loader
            .set_font_path(&[dir.to_string_lossy().into_owned()])
            .unwrap();
        let (_face, metrics, cache) = loader.open_font("testfont0").unwrap();
        // Coverage: encodings 1 and 3; 2 missing → zero CharInfo.
        assert_eq!(metrics.min_char_or_byte2, 1);
        assert_eq!(metrics.max_char_or_byte2, 3);
        assert!(!metrics.all_chars_exist);
        assert_eq!(metrics.char_infos.len(), 3);
        assert_eq!(metrics.char_infos[0].character_width, 10);
        assert_eq!(
            metrics.char_infos[1].character_width, 0,
            "missing char = zero metrics"
        );
        assert_eq!(metrics.char_infos[2].character_width, 5);
        assert_eq!(metrics.default_char, 1, "DEFAULT_CHAR property honored");
        assert_eq!(metrics.font_ascent, 10);
        assert_eq!(metrics.font_descent, 0);
        assert!(cache.contains_key(&char::from_u32(1).unwrap()));
        // BDF properties present (best-effort)
        assert!(
            metrics
                .named_properties
                .iter()
                .any(|(n, _)| n == "FONT_ASCENT")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_font_unknown_is_not_found() {
        let loader = FontLoader::new().unwrap();
        let err = loader.open_font("xtfont-nonexistent").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}

#[cfg(test)]
mod pcf_tests {
    use super::*;

    /// Real compiled PCF regression: FreeType's PCF driver leaves no
    /// charmap selected for registry-less fonts (the xts xtfonts), so
    /// metrics came out as the empty-coverage fallback (32..126,
    /// all-zero bounds) on the first vng run. Uses the xts build tree
    /// when present; skips silently otherwise.
    #[test]
    fn open_real_pcf_has_charmap_coverage() {
        let path = std::path::Path::new("/home/jos/Projects/xts/xts5/fonts");
        if !path.join("fonts.dir").is_file() {
            eprintln!("skipping: xts build-fresh fonts not present");
            return;
        }
        let mut loader = FontLoader::new().unwrap();
        loader
            .set_font_path(&[path.to_string_lossy().into_owned()])
            .unwrap();
        let (_face, metrics, cache) = loader.open_font("xtfont0").unwrap();
        // Ground truth = the compiled-in XTS expectation
        // (xts5/fonts/xtfont0.c): encodings 1..=68 (sparse),
        // default_char 0 (PCF encodings table), FONT_ASCENT 20 /
        // FONT_DESCENT 3 (PCF accelerators via FT size metrics —
        // bdftopcf strips both from the property table), and CELL
        // per-char metrics (no INK_METRICS table in this file).
        assert_eq!(metrics.min_char_or_byte2, 1, "PCF charmap coverage");
        assert_eq!(metrics.max_char_or_byte2, 68);
        assert_eq!(metrics.font_ascent, 20);
        assert_eq!(metrics.font_descent, 3);
        assert_eq!(metrics.default_char, 0);
        assert!(!metrics.all_chars_exist, "encodings are sparse");
        assert!(!cache.is_empty(), "glyphs must load through the charmap");
        let c1 = cache.get(&char::from_u32(1).unwrap()).expect("char 1");
        assert_eq!(c1.character_width, 10, "10x10 block glyph");
        // C002: all-blank bitmap, BBX 10x10, advance 2 — cell
        // metrics (rb 10, asc 10), NOT zero ink.
        let c2 = cache.get(&char::from_u32(2).unwrap()).expect("char 2");
        assert_eq!(
            (c2.right_side_bearing, c2.character_width, c2.ascent),
            (10, 2, 10)
        );
    }

    /// xtfont1 HAS an INK_METRICS table → per-char CharInfo must be
    /// the ink extents (blank char 0 = zero ink + advance 7).
    #[test]
    fn open_real_pcf_serves_ink_metrics_when_table_present() {
        let path = std::path::Path::new("/home/jos/Projects/xts/xts5/fonts");
        if !path.join("fonts.dir").is_file() {
            eprintln!("skipping: xts fonts not present");
            return;
        }
        let mut loader = FontLoader::new().unwrap();
        loader
            .set_font_path(&[path.to_string_lossy().into_owned()])
            .unwrap();
        let (_face, metrics, cache) = loader.open_font("xtfont1").unwrap();
        assert_eq!(metrics.default_char, 0);
        assert_eq!(metrics.font_ascent, 10);
        let c0 = cache.get(&char::from_u32(0).unwrap()).expect("char 0");
        assert_eq!(
            (c0.right_side_bearing, c0.character_width, c0.ascent),
            (0, 7, 0),
            "blank glyph: zero ink, advance kept"
        );
        let c1 = cache.get(&char::from_u32(1).unwrap()).expect("char 1");
        assert_eq!(
            (c1.right_side_bearing, c1.character_width, c1.ascent),
            (6, 7, 5),
            "ink-cropped extents (PCF INK_METRICS parity)"
        );
    }

    /// 2-byte matrix font (xtfont2: encodings 0x2121..0x307E, no row
    /// 0): metrics must use the byte1×byte2 grid, not a flat ≤255
    /// range (which filtered ALL codes out and produced the empty
    /// ASCII fallback — the XDrawString16 28→10 regression).
    #[test]
    fn open_real_two_byte_pcf_has_matrix_coverage() {
        let path = std::path::Path::new("/home/jos/Projects/xts/xts5/fonts");
        if !path.join("fonts.dir").is_file() {
            eprintln!("skipping: xts build-fresh fonts not present");
            return;
        }
        let mut loader = FontLoader::new().unwrap();
        loader
            .set_font_path(&[path.to_string_lossy().into_owned()])
            .unwrap();
        let (_face, metrics, cache) = loader.open_font("xtfont2").unwrap();
        assert_eq!(metrics.min_byte1, 0x21, "first row");
        assert_eq!(metrics.max_byte1, 0x30, "last row");
        assert!(metrics.min_char_or_byte2 >= 0x21);
        assert!(metrics.max_char_or_byte2 <= 0x7E);
        let rows = usize::from(metrics.max_byte1 - metrics.min_byte1) + 1;
        let cols = usize::from(metrics.max_char_or_byte2 - metrics.min_char_or_byte2) + 1;
        assert_eq!(metrics.char_infos.len(), rows * cols, "row-major grid");
        assert!(
            cache.contains_key(&char::from_u32(0x2121).unwrap()),
            "first 2-byte glyph loads through the charmap"
        );
    }
}

#[cfg(test)]
mod xtfont_probe {
    use super::*;

    #[test]
    fn probe_all_xtfonts() {
        let path = std::path::Path::new("/home/jos/Projects/xts/xts5/fonts");
        if !path.join("fonts.dir").is_file() {
            return;
        }
        let mut loader = FontLoader::new().unwrap();
        loader
            .set_font_path(&[path.to_string_lossy().into_owned()])
            .unwrap();
        for i in 0..=6 {
            let name = format!("xtfont{i}");
            match loader.open_font(&name) {
                Ok((_f, m, _c)) => {
                    eprintln!(
                        "{name}: byte1 {}..{} byte2 {}..{} default {} ascent {} descent {} nprops {} all_exist {}",
                        m.min_byte1,
                        m.max_byte1,
                        m.min_char_or_byte2,
                        m.max_char_or_byte2,
                        m.default_char,
                        m.font_ascent,
                        m.font_descent,
                        m.named_properties.len(),
                        m.all_chars_exist
                    );
                    eprintln!(
                        "  props: {:?}",
                        m.named_properties
                            .iter()
                            .map(|(n, _)| n.as_str())
                            .collect::<Vec<_>>()
                    );
                    let b = &m.min_bounds;
                    eprintln!(
                        "  minb: {} {} {} {} {}",
                        b.left_side_bearing,
                        b.right_side_bearing,
                        b.character_width,
                        b.ascent,
                        b.descent
                    );
                    let b = &m.max_bounds;
                    eprintln!(
                        "  maxb: {} {} {} {} {}",
                        b.left_side_bearing,
                        b.right_side_bearing,
                        b.character_width,
                        b.ascent,
                        b.descent
                    );
                    for (idx, ci) in m.char_infos.iter().take(5).enumerate() {
                        eprintln!(
                            "  char[{}]: lb {} rb {} w {} asc {} desc {}",
                            idx,
                            ci.left_side_bearing,
                            ci.right_side_bearing,
                            ci.character_width,
                            ci.ascent,
                            ci.descent
                        );
                    }
                }
                Err(e) => eprintln!("{name}: ERR {e}"),
            }
        }
    }
}
