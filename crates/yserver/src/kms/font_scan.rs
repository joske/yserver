//! Windows stand-in for fontconfig: the installed faces, found by
//! opening every file in the system font directories with FreeType.

use std::path::{Path, PathBuf};

// fontconfig's values, so the catalog's XLFD mapping applies unchanged.
const FC_WEIGHT_REGULAR: i32 = 80;
const FC_WEIGHT_BOLD: i32 = 200;
const FC_SLANT_ROMAN: i32 = 0;
const FC_SLANT_ITALIC: i32 = 100;
const FC_PROPORTIONAL: i32 = 0;
const FC_MONO: i32 = 100;

/// What fontconfig's `monospace` resolves to here, in preference order.
const MONOSPACE: &[&str] = &["Consolas", "Cascadia Mono", "Lucida Console", "Courier New"];

pub(crate) struct ScannedFace {
    pub(crate) path: PathBuf,
    pub(crate) index: isize,
    pub(crate) family: String,
    pub(crate) weight: i32,
    pub(crate) slant: i32,
    pub(crate) spacing: i32,
}

pub(crate) struct FontScan {
    faces: Vec<ScannedFace>,
}

impl FontScan {
    pub(crate) fn new() -> Option<Self> {
        let library = freetype::Library::init().ok()?;
        let mut faces = Vec::new();
        for dir in font_dirs() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            files.sort();
            for path in files {
                scan_file(&library, &path, &mut faces);
            }
        }
        Some(Self { faces })
    }

    pub(crate) fn faces(&self) -> &[ScannedFace] {
        &self.faces
    }

    /// fontconfig's `font_match` for a family chained with `monospace`:
    /// the face of the first family present that best fits `style`.
    pub(crate) fn find(&self, family: &str, style: Option<&str>) -> Option<&ScannedFace> {
        let style = style.unwrap_or("").to_ascii_lowercase();
        let bold = style.contains("bold");
        let italic = style.contains("italic") || style.contains("oblique");
        let chain = std::iter::once(family)
            .filter(|f| !f.eq_ignore_ascii_case("monospace"))
            .chain(MONOSPACE.iter().copied());
        for fam in chain {
            let in_family = || {
                self.faces
                    .iter()
                    .filter(|f| f.family.eq_ignore_ascii_case(fam))
            };
            let fit = in_family()
                .find(|f| {
                    (f.weight >= FC_WEIGHT_BOLD) == bold && (f.slant != FC_SLANT_ROMAN) == italic
                })
                .or_else(|| in_family().next());
            if fit.is_some() {
                return fit;
            }
        }
        self.faces.first()
    }
}

fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let windir =
        std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    dirs.push(windir.join("Fonts"));
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(Path::new(&local).join(r"Microsoft\Windows\Fonts"));
    }
    dirs
}

fn scan_file(library: &freetype::Library, path: &Path, out: &mut Vec<ScannedFace>) {
    let mut index = 0;
    loop {
        let Ok(face) = library.new_face(path, index) else {
            return;
        };
        if let Some(family) = face.family_name() {
            let flags = face.style_flags();
            out.push(ScannedFace {
                path: path.to_path_buf(),
                index,
                family,
                weight: if flags.contains(freetype::face::StyleFlag::BOLD) {
                    FC_WEIGHT_BOLD
                } else {
                    FC_WEIGHT_REGULAR
                },
                slant: if flags.contains(freetype::face::StyleFlag::ITALIC) {
                    FC_SLANT_ITALIC
                } else {
                    FC_SLANT_ROMAN
                },
                spacing: if face.is_fixed_width() {
                    FC_MONO
                } else {
                    FC_PROPORTIONAL
                },
            });
        }
        index += 1;
        if index >= isize::from(face.num_faces()) {
            return;
        }
    }
}
