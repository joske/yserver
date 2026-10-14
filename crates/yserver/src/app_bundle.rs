//! Process setup for a launch outside `just macos-run`: from an app bundle
//! (Finder, `open`) there is no terminal and no Vulkan environment. Runs
//! first in `main`, before any thread exists.

use std::{
    env,
    ffi::OsStr,
    fs::{self, File},
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

/// The Vulkan loader's manifest-list variables; either one set by the user
/// wins over self-discovery.
pub const ICD_VARS: [&str; 2] = ["VK_DRIVER_FILES", "VK_ICD_FILENAMES"];

/// Homebrew prefixes on Apple Silicon and Intel.
const HOMEBREW_PREFIXES: [&str; 2] = ["/opt/homebrew", "/usr/local"];

/// What [`prepare`] did, logged once the logger is up.
#[derive(Debug, Default)]
pub struct Startup {
    /// The bundle the executable runs from, if any.
    pub bundle: Option<PathBuf>,
    /// Where stderr goes, when launched from a bundle.
    pub log_file: Option<PathBuf>,
    /// Why stderr could not be redirected.
    pub log_error: Option<String>,
    /// The ICD manifest the loader was pointed at.
    pub icd: Option<PathBuf>,
}

impl Startup {
    /// Log what [`prepare`] did.
    pub fn log(&self) {
        if let Some(bundle) = &self.bundle {
            log::info!("yserver: launched from {}", bundle.display());
        }
        if let Some(error) = &self.log_error {
            log::warn!("yserver: no log file: {error}");
        }
        if let Some(icd) = &self.icd {
            log::info!("vulkan: VK_DRIVER_FILES={}", icd.display());
        }
    }
}

/// Redirect stdout and stderr to `~/Library/Logs/yserver.log` when running
/// from an app bundle, and point the Vulkan loader at the Homebrew
/// KosmicKrisp ICD when no manifest variable is set.
///
/// Must run before any thread is spawned: it sets an environment variable.
/// The loader reads a manifest path only from the environment; the API route
/// (`VK_LUNARG_direct_driver_loading`) takes an already-loaded driver, which
/// would mean re-implementing the manifest's `library_path` resolution.
#[must_use]
pub fn prepare() -> Startup {
    let mut startup = Startup {
        bundle: env::current_exe()
            .ok()
            .and_then(|exe| bundle_of(&exe).map(Path::to_path_buf)),
        ..Startup::default()
    };
    if startup.bundle.is_some() {
        match env::var_os("HOME").map(|home| log_path(Path::new(&home))) {
            Some(path) => match redirect_output(&path) {
                Ok(()) => startup.log_file = Some(path),
                Err(e) => startup.log_error = Some(format!("{}: {e}", path.display())),
            },
            None => startup.log_error = Some("HOME is not set".to_owned()),
        }
    }
    let user_set = ICD_VARS.iter().any(|var| env::var_os(var).is_some());
    if !user_set {
        let prefixes = homebrew_prefixes(option_env!("YSERVER_HOMEBREW_PREFIX"));
        startup.icd = find_icd(&prefixes, env::consts::ARCH, |path| path.is_file());
        if let Some(icd) = &startup.icd {
            // SAFETY: `prepare` runs first in `main`, before any thread
            // exists, so nothing reads the environment concurrently.
            unsafe { env::set_var(ICD_VARS[0], icd) };
        }
    }
    startup
}

/// The `*.app` bundle when `exe` is `<name>.app/Contents/MacOS/<binary>`.
#[must_use]
pub fn bundle_of(exe: &Path) -> Option<&Path> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name() == Some(OsStr::new("MacOS"))
        && contents.file_name() == Some(OsStr::new("Contents"))
        && bundle.extension() == Some(OsStr::new("app")))
    .then_some(bundle)
}

/// The log file under the home directory `home`.
#[must_use]
pub fn log_path(home: &Path) -> PathBuf {
    home.join("Library/Logs/yserver.log")
}

/// The Homebrew prefixes to search: the one `brew --prefix` gave at build
/// time first, then the standard ones, without duplicates.
#[must_use]
pub fn homebrew_prefixes(build_time: Option<&str>) -> Vec<PathBuf> {
    let mut prefixes: Vec<PathBuf> = Vec::new();
    for prefix in build_time
        .filter(|p| !p.is_empty())
        .into_iter()
        .chain(HOMEBREW_PREFIXES)
    {
        let prefix = PathBuf::from(prefix);
        if !prefixes.contains(&prefix) {
            prefixes.push(prefix);
        }
    }
    prefixes
}

/// The first KosmicKrisp ICD manifest for `arch` under `prefixes` that
/// `exists`.
#[must_use]
pub fn find_icd(
    prefixes: &[PathBuf],
    arch: &str,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let name = format!("kosmickrisp_mesa_icd.{arch}.json");
    prefixes
        .iter()
        .map(|prefix| prefix.join("share/vulkan/icd.d").join(&name))
        .find(|path| exists(path))
}

/// Point stdout and stderr at `path`, truncated (one log per launch, as
/// `just macos-run`'s `tee`). Stderr carries the logger and also Mesa's and
/// the loader's messages.
fn redirect_output(path: &Path) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = File::create(path)?;
    for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: both descriptors are valid; dup2 replaces `fd` atomically.
        if unsafe { libc::dup2(file.as_raw_fd(), fd) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_of_needs_app_contents_macos() {
        assert_eq!(
            bundle_of(Path::new(
                "/Applications/yserver.app/Contents/MacOS/yserver"
            )),
            Some(Path::new("/Applications/yserver.app"))
        );
        assert_eq!(
            bundle_of(Path::new("target/yserver.app/Contents/MacOS/yserver")),
            Some(Path::new("target/yserver.app"))
        );
        for exe in [
            "/Users/jos/yserver/target/release/yserver",
            "/Applications/yserver/Contents/MacOS/yserver",
            "/Applications/yserver.app/Contents/Resources/yserver",
            "/Applications/yserver.app/MacOS/yserver",
            "Contents/MacOS/yserver",
            "yserver",
        ] {
            assert_eq!(bundle_of(Path::new(exe)), None, "{exe}");
        }
    }

    #[test]
    fn log_path_is_in_library_logs() {
        assert_eq!(
            log_path(Path::new("/Users/jos")),
            PathBuf::from("/Users/jos/Library/Logs/yserver.log")
        );
    }

    #[test]
    fn homebrew_prefixes_put_build_time_first_without_duplicates() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(
            homebrew_prefixes(None),
            [p("/opt/homebrew"), p("/usr/local")]
        );
        assert_eq!(
            homebrew_prefixes(Some("")),
            [p("/opt/homebrew"), p("/usr/local")]
        );
        assert_eq!(
            homebrew_prefixes(Some("/usr/local")),
            [p("/usr/local"), p("/opt/homebrew")]
        );
        assert_eq!(
            homebrew_prefixes(Some("/Users/jos/brew")),
            [p("/Users/jos/brew"), p("/opt/homebrew"), p("/usr/local")]
        );
    }

    #[test]
    fn find_icd_takes_the_first_existing_manifest_for_arch() {
        let prefixes = homebrew_prefixes(None);
        let intel = PathBuf::from("/usr/local/share/vulkan/icd.d/kosmickrisp_mesa_icd.x86_64.json");
        let arm =
            PathBuf::from("/opt/homebrew/share/vulkan/icd.d/kosmickrisp_mesa_icd.aarch64.json");
        let both = |path: &Path| path == intel || path == arm;
        assert_eq!(
            find_icd(&prefixes, "aarch64", both).as_deref(),
            Some(arm.as_path())
        );
        assert_eq!(
            find_icd(&prefixes, "x86_64", both).as_deref(),
            Some(intel.as_path())
        );
        assert_eq!(find_icd(&prefixes, "aarch64", |_| false), None);
    }
}
