//! Where the XKB data comes from. libxkbcommon's compiled-in root is a path
//! on the machine that built it, so a portable install ships the data in an
//! `xkb` directory next to `yserver.exe`.

use std::path::PathBuf;

use xkbcommon::xkb::{CONTEXT_NO_DEFAULT_INCLUDES, CONTEXT_NO_FLAGS, Context};

/// The context keymaps are compiled in: libxkbcommon's defaults when
/// `XKB_CONFIG_ROOT` is set, else the `xkb` directory next to the executable
/// if there is one.
///
/// An include path rather than setting `XKB_CONFIG_ROOT`: libxkbcommon reads
/// it with the CRT's `getenv`, whose table `std::env::set_var`
/// (`SetEnvironmentVariableW`) does not update.
pub(crate) fn context() -> Context {
    if let Some(root) = std::env::var_os("XKB_CONFIG_ROOT") {
        log::info!("xkb: data root {} (XKB_CONFIG_ROOT)", root.display());
        return Context::new(CONTEXT_NO_FLAGS);
    }
    let Some(root) = bundled_root() else {
        log::warn!(
            "xkb: no XKB_CONFIG_ROOT and no xkb directory next to the executable; \
             using libxkbcommon's compiled-in root"
        );
        return Context::new(CONTEXT_NO_FLAGS);
    };
    let mut ctx = Context::new(CONTEXT_NO_DEFAULT_INCLUDES);
    if ctx.include_path_append(&root) {
        log::info!("xkb: data root {} (next to the executable)", root.display());
    } else {
        // `include_path_append` takes UTF-8, and libxkbcommon opens it with
        // the ANSI `stat`: a path outside the code page fails here.
        log::error!("xkb: cannot use {} as the data root", root.display());
    }
    ctx
}

/// `<directory of yserver.exe>\xkb`, if it is a directory.
fn bundled_root() -> Option<PathBuf> {
    let root = std::env::current_exe().ok()?.parent()?.join("xkb");
    root.is_dir().then_some(root)
}
