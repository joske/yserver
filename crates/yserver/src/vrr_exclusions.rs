use std::{
    cell::RefCell,
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use regex::Regex;
use serde_json::Value;

const MAX_LIST_FILE_SIZE: u64 = 1_048_576;
const MAX_USER_HOME_CACHE_ENTRIES: usize = 64; // Client HOME values must not grow this cache without bound.
const YSERVER_EXCLUDED_PROCESSES: [&str; 4] =
    ["steamwebhelper", "Discord", "DiscordPTB", "DiscordCanary"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VrrExclusionSource {
    Environment,
    Yserver,
    NvidiaProfiles,
    MesaDriconf,
}

#[derive(Clone, Debug)]
pub struct VrrListRoots {
    pub nvidia_driver_dir: PathBuf,
    pub nvidia_etc_dir: PathBuf,
    pub mesa_system_dir: PathBuf,
    pub mesa_etc_file: PathBuf,
}

impl VrrListRoots {
    #[must_use]
    pub fn system() -> Self {
        Self {
            nvidia_driver_dir: PathBuf::from("/usr/share/nvidia"),
            nvidia_etc_dir: PathBuf::from("/etc/nvidia"),
            mesa_system_dir: PathBuf::from("/usr/share/drirc.d"),
            mesa_etc_file: PathBuf::from("/etc/drirc"),
        }
    }
}

pub struct VrrExclusions {
    nvidia: HashMap<String, bool>,
    nvidia_documents: Vec<Value>,
    mesa: Vec<MesaDriconfEntry>,
    user_lists: RefCell<HashMap<PathBuf, UserVrrLists>>,
}

struct UserVrrLists {
    nvidia: HashMap<String, bool>,
    mesa: Vec<MesaDriconfEntry>,
}

#[derive(Clone)]
struct MesaDriconfEntry {
    matcher: MesaApplicationMatcher,
    allowed: bool,
}

#[derive(Clone)]
enum MesaApplicationMatcher {
    Executable(String),
    ExecutableRegexp(Regex),
    ExecutableAndRegexp(String, Regex),
    Global,
    Never,
}

impl MesaApplicationMatcher {
    fn matches(&self, process_name: &str) -> bool {
        match self {
            Self::Executable(executable) => executable == process_name,
            Self::ExecutableRegexp(regex) => regex.is_match(process_name),
            Self::ExecutableAndRegexp(executable, regex) => {
                executable == process_name && regex.is_match(process_name)
            }
            Self::Global => true,
            Self::Never => false,
        }
    }
}

pub(crate) struct VrrExclusionDecision {
    pub(crate) process_name: Option<String>,
    pub(crate) source: Option<VrrExclusionSource>,
}

impl VrrExclusions {
    #[must_use]
    pub fn load(roots: &VrrListRoots) -> Self {
        let mut nvidia_paths = vec![roots.nvidia_etc_dir.join("nvidia-application-profiles-rc")];
        nvidia_paths.extend(files_in_dir(
            &roots
                .nvidia_etc_dir
                .join("nvidia-application-profiles-rc.d"),
        ));
        nvidia_paths.extend(files_named(&roots.nvidia_driver_dir, |name| {
            name.starts_with("nvidia-application-profiles-") && name.ends_with("-rc")
        }));
        let nvidia_documents = load_nvidia_documents(nvidia_paths);
        let nvidia = parse_nvidia_profiles(&nvidia_documents);

        let mesa_system_paths = files_named(&roots.mesa_system_dir, |name| name.ends_with(".conf"));
        if mesa_system_paths.is_empty() {
            log::warn!(
                "kms: vrr all requires Mesa's {} (Mesa DRI package: Arch/Alpine mesa, Debian/Ubuntu libgl1-mesa-dri, Fedora mesa-dri-drivers, FreeBSD mesa-dri, Gentoo media-libs/mesa)",
                roots
                    .mesa_system_dir
                    .join("00-mesa-defaults.conf")
                    .display()
            );
        }
        let mut mesa_paths = mesa_system_paths;
        mesa_paths.push(roots.mesa_etc_file.clone());
        let mesa = load_mesa_entries(mesa_paths);

        let nvidia_count = nvidia.values().filter(|allowed| !**allowed).count();
        let mesa_count = mesa.iter().filter(|entry| !entry.allowed).count();
        log::info!("kms: vrr exclusions nvidia={nvidia_count} mesa={mesa_count}");
        Self {
            nvidia,
            nvidia_documents,
            mesa,
            user_lists: RefCell::new(HashMap::new()),
        }
    }

    /// Return the list source that excludes this process, or `None` when it
    /// may use VRR. The server passes `/proc` in production and a synthetic
    /// proc tree in tests.
    #[must_use]
    pub fn excluded(&self, pid: u32, proc_root: &Path) -> Option<VrrExclusionSource> {
        self.evaluate(pid, proc_root).source
    }

    pub(crate) fn evaluate(&self, pid: u32, proc_root: &Path) -> VrrExclusionDecision {
        Self::evaluate_with_lists(Some(self), pid, proc_root)
    }

    #[cfg(not(test))]
    pub(crate) fn evaluate_with_lists(
        external_lists: Option<&Self>,
        pid: u32,
        proc_root: &Path,
    ) -> VrrExclusionDecision {
        Self::evaluate_with_lists_inner(external_lists, pid, proc_root)
    }

    #[cfg(test)]
    pub(crate) fn evaluate_with_lists(
        external_lists: Option<&Self>,
        pid: u32,
        proc_root: &Path,
    ) -> VrrExclusionDecision {
        test_support::record_process_read(proc_root);
        Self::evaluate_with_lists_inner(external_lists, pid, proc_root)
    }

    fn evaluate_with_lists_inner(
        external_lists: Option<&Self>,
        pid: u32,
        proc_root: &Path,
    ) -> VrrExclusionDecision {
        let process_dir = proc_root.join(pid.to_string());
        let environment = read_environment(&process_dir.join("environ"));
        let home = environment.get("HOME").map(PathBuf::from);
        let nvidia_environment = environment
            .get("__GL_VRR_ALLOWED")
            .and_then(|value| parse_bool(value));
        let mesa_environment = environment
            .get("adaptive_sync")
            .and_then(|value| parse_bool(value));
        let process_name = read_process_name(&process_dir);
        let source = if nvidia_environment == Some(false) || mesa_environment == Some(false) {
            Some(VrrExclusionSource::Environment)
        } else if nvidia_environment == Some(true) || mesa_environment == Some(true) {
            None
        } else if process_name
            .as_deref()
            .is_some_and(|name| YSERVER_EXCLUDED_PROCESSES.contains(&name))
        {
            Some(VrrExclusionSource::Yserver)
        } else {
            process_name.as_ref().and_then(|name| {
                external_lists.and_then(|lists| lists.list_exclusion(name, home.as_deref()))
            })
        };
        VrrExclusionDecision {
            process_name,
            source,
        }
    }

    fn list_exclusion(
        &self,
        process_name: &str,
        home: Option<&Path>,
    ) -> Option<VrrExclusionSource> {
        if let Some(home) = home
            && !self.user_lists.borrow().contains_key(home)
        {
            let lists = self.load_user_lists(home);
            let mut user_lists = self.user_lists.borrow_mut();
            if user_lists.len() >= MAX_USER_HOME_CACHE_ENTRIES {
                user_lists.clear();
            }
            user_lists.insert(home.to_path_buf(), lists);
        }
        let user_lists = self.user_lists.borrow();
        let selected_user_lists = home.and_then(|home| user_lists.get(home));
        let (nvidia, mesa) = selected_user_lists
            .map_or((&self.nvidia, self.mesa.as_slice()), |lists| {
                (&lists.nvidia, lists.mesa.as_slice())
            });
        if nvidia.get(process_name) == Some(&false) {
            Some(VrrExclusionSource::NvidiaProfiles)
        } else if mesa_excludes(mesa, process_name) {
            Some(VrrExclusionSource::MesaDriconf)
        } else {
            None
        }
    }

    fn load_user_lists(&self, home: &Path) -> UserVrrLists {
        let user_nvidia = home.join(".nv");
        let mut nvidia_paths = vec![user_nvidia.join("nvidia-application-profiles-rc")];
        nvidia_paths.extend(files_in_dir(
            &user_nvidia.join("nvidia-application-profiles-rc.d"),
        ));
        let mut nvidia_documents = load_nvidia_documents(nvidia_paths);
        nvidia_documents.extend(self.nvidia_documents.iter().cloned());

        let mut mesa = self.mesa.clone();
        mesa.extend(load_mesa_entries([home.join(".drirc")]));
        UserVrrLists {
            nvidia: parse_nvidia_profiles(&nvidia_documents),
            mesa,
        }
    }
}

fn mesa_excludes(entries: &[MesaDriconfEntry], process_name: &str) -> bool {
    entries
        .iter()
        .filter(|entry| entry.matcher.matches(process_name))
        .map(|entry| entry.allowed)
        .next_back()
        == Some(false)
}

fn files_in_dir(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    paths.sort();
    paths
}

fn files_named(directory: &Path, matches: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    files_in_dir(directory)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(&matches)
        })
        .collect()
}

fn read_optional(path: &Path, description: &str) -> Option<String> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => Some(file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            log::warn!("kms: cannot read {description} {}: {error}", path.display());
            None
        }
    }?;

    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            log::warn!("kms: cannot read {description} {}: {error}", path.display());
            return None;
        }
    };
    if !metadata.is_file() {
        log::warn!("kms: skipping non-regular {description} {}", path.display());
        return None;
    }
    if metadata.len() > MAX_LIST_FILE_SIZE {
        log::warn!(
            "kms: skipping oversized {description} {} ({} bytes; limit is {MAX_LIST_FILE_SIZE})",
            path.display(),
            metadata.len()
        );
        return None;
    }
    let mut bytes = Vec::new();
    if let Err(error) = file.take(MAX_LIST_FILE_SIZE + 1).read_to_end(&mut bytes) {
        if error.kind() != io::ErrorKind::NotFound {
            log::warn!("kms: cannot read {description} {}: {error}", path.display());
        }
        return None;
    }
    if bytes.len() as u64 > MAX_LIST_FILE_SIZE {
        log::warn!(
            "kms: skipping oversized {description} {} (grew beyond {MAX_LIST_FILE_SIZE} bytes while reading)",
            path.display()
        );
        return None;
    }
    match String::from_utf8(bytes) {
        Ok(contents) => Some(contents),
        Err(error) => {
            log::warn!("kms: cannot read {description} {}: {error}", path.display());
            None
        }
    }
}

fn load_nvidia_documents(paths: impl IntoIterator<Item = PathBuf>) -> Vec<Value> {
    let mut documents = Vec::new();
    for path in paths {
        if let Some(contents) = read_optional(&path, "NVIDIA application profile") {
            if let Some(document) = parse_nvidia_document(&contents) {
                documents.push(document);
            } else {
                log::warn!(
                    "kms: skipping malformed NVIDIA application profile {}",
                    path.display()
                );
            }
        }
    }
    documents
}

fn load_mesa_entries(paths: impl IntoIterator<Item = PathBuf>) -> Vec<MesaDriconfEntry> {
    let mut entries = Vec::new();
    for path in paths {
        if let Some(contents) = read_optional(&path, "Mesa driconf") {
            if let Some(file_entries) = parse_driconf(&contents) {
                entries.extend(file_entries);
            } else {
                log::warn!("kms: skipping malformed Mesa driconf {}", path.display());
            }
        }
    }
    entries
}

fn parse_nvidia_document(contents: &str) -> Option<Value> {
    let json = drop_hash_comment_lines(contents);
    let document: Value = serde_json::from_str(&json).ok()?;
    document.as_object()?;
    Some(document)
}

fn parse_nvidia_profiles(documents: &[Value]) -> HashMap<String, bool> {
    let mut profiles: HashMap<&str, &Value> = HashMap::new();
    for document in documents {
        let Some(items) = document.get("profiles").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            if let (Some(name), Some(profile)) = (
                item.get("name").and_then(Value::as_str),
                item.get("settings"),
            ) {
                profiles.entry(name).or_insert(profile);
            }
        }
    }

    let mut settings_by_process = HashMap::new();
    for document in documents {
        let Some(rules) = document.get("rules").and_then(Value::as_array) else {
            continue;
        };
        for rule in rules {
            let Some(process_name) = rule.get("pattern").and_then(procname_pattern) else {
                continue;
            };
            let Some(profile) = rule.get("profile") else {
                continue;
            };
            let settings = match profile {
                Value::String(name) => profiles.get(name.as_str()).copied(),
                Value::Object(object) => object.get("settings"),
                settings @ Value::Array(_) => Some(settings),
                _ => None,
            };
            let Some(allowed) =
                settings.and_then(|settings| setting_bool(settings, "GLVRRAllowed"))
            else {
                continue;
            };
            // NVIDIA applies earlier matching rules before later ones. The
            // first value for this key therefore wins across files and rules.
            settings_by_process
                .entry(process_name.to_owned())
                .or_insert(allowed);
        }
    }
    settings_by_process
}

fn procname_pattern(pattern: &Value) -> Option<&str> {
    match pattern {
        Value::String(name) => Some(name),
        Value::Object(pattern)
            if pattern.get("feature").and_then(Value::as_str) == Some("procname") =>
        {
            pattern.get("matches").and_then(Value::as_str)
        }
        _ => None,
    }
}

fn setting_bool(settings: &Value, key: &str) -> Option<bool> {
    match settings {
        Value::Array(entries) => {
            if entries.iter().any(Value::is_object) {
                entries.iter().find_map(|entry| {
                    let entry_key = entry
                        .get("key")
                        .or_else(|| entry.get("k"))
                        .and_then(Value::as_str);
                    (entry_key == Some(key)).then(|| {
                        entry
                            .get("value")
                            .or_else(|| entry.get("v"))
                            .and_then(value_bool)
                    })?
                })
            } else {
                entries.chunks_exact(2).find_map(|pair| {
                    (pair[0].as_str() == Some(key))
                        .then(|| value_bool(&pair[1]))
                        .flatten()
                })
            }
        }
        _ => None,
    }
}

fn value_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::Number(value) => match value.as_i64()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        },
        Value::String(value) => parse_bool(value),
        _ => None,
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "0" | "false" => Some(false),
        "1" | "true" => Some(true),
        _ => None,
    }
}

fn drop_hash_comment_lines(source: &str) -> String {
    source
        .lines()
        .map(|line| {
            let mut in_string = false;
            let mut escaped = false;
            for (index, character) in line.char_indices() {
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if character == '\\' {
                        escaped = true;
                    } else if character == '"' {
                        in_string = false;
                    }
                } else if character == '"' {
                    in_string = true;
                } else if character == '#' {
                    return &line[..index];
                }
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_environment(path: &Path) -> HashMap<String, String> {
    let Ok(contents) = fs::read(path) else {
        return HashMap::new();
    };
    contents
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let entry = std::str::from_utf8(entry).ok()?;
            let (key, value) = entry.split_once('=')?;
            matches!(key, "__GL_VRR_ALLOWED" | "adaptive_sync" | "HOME")
                .then(|| (key.to_owned(), value.to_owned()))
        })
        .collect()
}

fn read_process_name(process_dir: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;

    let cmdline = fs::read(process_dir.join("cmdline")).ok()?;
    let argv0 = cmdline.split(|byte| *byte == 0).next()?;
    let argv0 = std::str::from_utf8(argv0).ok()?;
    let argv_name = Path::new(argv0)
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned);
    let executable_text = fs::read_link(process_dir.join("exe"))
        .ok()
        .and_then(|executable| {
            std::str::from_utf8(executable.as_os_str().as_bytes())
                .ok()
                .map(ToOwned::to_owned)
        });
    executable_text
        .filter(|executable| argv0.starts_with(executable))
        .and_then(|executable| {
            Path::new(&executable)
                .file_name()
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
        })
        .or(argv_name)
}

fn parse_driconf(contents: &str) -> Option<Vec<MesaDriconfEntry>> {
    let document = roxmltree::Document::parse_with_options(
        contents,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..roxmltree::ParsingOptions::default()
        },
    )
    .ok()?;
    let mut entries = Vec::new();
    for application in document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "application")
    {
        // Mesa checks executable first, then only the first present secondary matcher.
        let executable = application.attribute("executable");
        let mut matcher = if let Some(pattern) = application.attribute("executable_regexp") {
            match Regex::new(pattern) {
                Ok(regex) => match executable {
                    Some(executable) => {
                        MesaApplicationMatcher::ExecutableAndRegexp(executable.to_owned(), regex)
                    }
                    None => MesaApplicationMatcher::ExecutableRegexp(regex),
                },
                Err(error) => {
                    log::warn!("kms: invalid Mesa executable_regexp {pattern:?}: {error}");
                    MesaApplicationMatcher::Never
                }
            }
        } else if application.attribute("blake3").is_some()
            || application.attribute("application_name_match").is_some()
        {
            MesaApplicationMatcher::Never
        } else if let Some(executable) = executable {
            MesaApplicationMatcher::Executable(executable.to_owned())
        } else {
            MesaApplicationMatcher::Global
        };
        // These attributes need client-side identity/version data. Entries
        // whose matcher is selected from them never apply in yserver.
        if application.attribute("application_versions").is_some() {
            matcher = MesaApplicationMatcher::Never;
        }
        let has_unknown_device_scope = application.ancestors().any(|ancestor| {
            ancestor.is_element()
                && ancestor.tag_name().name() == "device"
                && ancestor.attributes().next().is_some()
        });
        for option in application
            .descendants()
            .filter(|node| node.is_element() && node.tag_name().name() == "option")
        {
            if option.attribute("name") == Some("adaptive_sync")
                && let Some(value) = option.attribute("value").and_then(parse_bool)
            {
                if value && has_unknown_device_scope {
                    // An unknown scoped allow cannot lift an exclusion; fallback stays on fixed refresh.
                    continue;
                }
                entries.push(MesaDriconfEntry {
                    matcher: matcher.clone(),
                    allowed: value,
                });
            }
        }
    }
    Some(entries)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::VrrListRoots;
    use std::{
        cell::RefCell,
        collections::HashMap,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    thread_local! {
        static PROCESS_READ_COUNTS: RefCell<HashMap<PathBuf, usize>> = RefCell::new(HashMap::new());
    }

    pub(crate) struct TempTree(PathBuf);

    impl TempTree {
        pub(crate) fn new() -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("yserver-vrr-test-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp fixture root");
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn roots(tree: &TempTree) -> VrrListRoots {
        VrrListRoots {
            nvidia_driver_dir: tree.path().join("usr/share/nvidia"),
            nvidia_etc_dir: tree.path().join("etc/nvidia"),
            mesa_system_dir: tree.path().join("usr/share/drirc.d"),
            mesa_etc_file: tree.path().join("etc/drirc"),
        }
    }

    pub(crate) fn write(path: impl AsRef<Path>, contents: &str) {
        let path = path.as_ref();
        fs::create_dir_all(path.parent().expect("fixture path has parent"))
            .expect("create fixture parent");
        fs::write(path, contents).expect("write fixture file");
    }

    pub(crate) fn process_read_count(proc_root: &Path) -> usize {
        PROCESS_READ_COUNTS.with(|counts| counts.borrow().get(proc_root).copied().unwrap_or(0))
    }

    pub(super) fn record_process_read(proc_root: &Path) {
        PROCESS_READ_COUNTS.with(|counts| {
            *counts
                .borrow_mut()
                .entry(proc_root.to_path_buf())
                .or_default() += 1;
        });
    }

    pub(crate) fn process(
        proc_root: &Path,
        pid: u32,
        executable: &str,
        argv0: &str,
        environ: &[&str],
        home: Option<&Path>,
    ) {
        use std::os::unix::fs::symlink;

        let process_dir = proc_root.join(pid.to_string());
        let _ = fs::remove_dir_all(&process_dir);
        fs::create_dir_all(&process_dir).expect("create fake process directory");
        symlink(executable, process_dir.join("exe")).expect("create fake executable symlink");
        let mut cmdline = argv0.as_bytes().to_vec();
        cmdline.push(0);
        fs::write(process_dir.join("cmdline"), cmdline).expect("write fake command line");
        let mut environ: Vec<_> = environ.iter().map(|entry| (*entry).to_owned()).collect();
        if let Some(home) = home {
            environ.push(format!("HOME={}", home.display()));
        }
        fs::write(process_dir.join("environ"), environ.join("\0") + "\0")
            .expect("write fake environment");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        VrrExclusionSource, VrrExclusions, VrrListRoots,
        test_support::{TempTree, process, roots, write},
    };

    struct FifoTestLogger;

    impl log::Log for FifoTestLogger {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::Level::Warn
        }

        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                eprintln!("{}", record.args());
            }
        }

        fn flush(&self) {}
    }

    static FIFO_TEST_LOGGER: FifoTestLogger = FifoTestLogger;

    #[test]
    // Mutation: read user files from a fixed server-side root instead of the client's HOME.
    fn vrr_exclusions_user_files_follow_client_home() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let home_a = tree.path().join("home/alice");
        let home_b = tree.path().join("home/bob");
        let proc_root = tree.path().join("proc");
        write(
            home_a.join(".drirc"),
            r#"<driconf><application executable="foo"><option name="adaptive_sync" value="false"/></application></driconf>"#,
        );
        write(
            home_b.join(".nv/nvidia-application-profiles-rc"),
            r#"{"rules":[{"pattern":"foo","profile":["GLVRRAllowed",false]}]}"#,
        );
        process(
            &proc_root,
            101,
            "/usr/bin/foo",
            "/usr/bin/foo",
            &[],
            Some(&home_a),
        );
        process(
            &proc_root,
            102,
            "/usr/bin/foo",
            "/usr/bin/foo",
            &[],
            Some(&home_b),
        );
        process(&proc_root, 103, "/usr/bin/foo", "/usr/bin/foo", &[], None);
        let exclusions = VrrExclusions::load(&roots);

        assert_eq!(
            exclusions.excluded(101, &proc_root),
            Some(VrrExclusionSource::MesaDriconf),
            "HOME A's Mesa file excludes foo"
        );
        assert_eq!(
            exclusions.excluded(102, &proc_root),
            Some(VrrExclusionSource::NvidiaProfiles),
            "HOME B's NVIDIA file excludes foo"
        );
        assert_eq!(
            exclusions.excluded(103, &proc_root),
            None,
            "a process with no HOME uses only the system lists"
        );
    }

    #[test]
    // Mutation: drop the per-HOME user-list cache.
    fn vrr_exclusions_user_files_cached_per_home() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let home_a = tree.path().join("home/alice");
        let home_c = tree.path().join("home/carol");
        let proc_root = tree.path().join("proc");
        let drirc = home_a.join(".drirc");
        write(
            &drirc,
            r#"<driconf><application executable="foo"><option name="adaptive_sync" value="false"/></application></driconf>"#,
        );
        process(
            &proc_root,
            104,
            "/usr/bin/foo",
            "/usr/bin/foo",
            &[],
            Some(&home_a),
        );
        let exclusions = VrrExclusions::load(&roots);
        assert_eq!(
            exclusions.excluded(104, &proc_root),
            Some(VrrExclusionSource::MesaDriconf)
        );

        std::fs::remove_file(drirc).expect("remove HOME A driconf after its first read");
        process(
            &proc_root,
            105,
            "/usr/bin/foo",
            "/usr/bin/foo",
            &[],
            Some(&home_a),
        );
        process(
            &proc_root,
            106,
            "/usr/bin/foo",
            "/usr/bin/foo",
            &[],
            Some(&home_c),
        );

        assert_eq!(
            exclusions.excluded(105, &proc_root),
            Some(VrrExclusionSource::MesaDriconf),
            "a second process with HOME A sees its cached list"
        );
        assert_eq!(
            exclusions.excluded(106, &proc_root),
            None,
            "a new HOME is read independently"
        );
    }

    #[test]
    // Mutation: append user NVIDIA documents after the system documents.
    fn vrr_exclusions_user_nvidia_file_wins_over_system() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let home = tree.path().join("home/alice");
        let proc_root = tree.path().join("proc");
        write(
            roots.nvidia_etc_dir.join("nvidia-application-profiles-rc"),
            r#"{"rules":[{"pattern":"foo","profile":["GLVRRAllowed",false]}]}"#,
        );
        write(
            home.join(".nv/nvidia-application-profiles-rc"),
            r#"{"rules":[{"pattern":"foo","profile":["GLVRRAllowed",true]}]}"#,
        );
        process(
            &proc_root,
            107,
            "/usr/bin/foo",
            "/usr/bin/foo",
            &[],
            Some(&home),
        );
        let exclusions = VrrExclusions::load(&roots);

        assert_eq!(
            exclusions.excluded(107, &proc_root),
            None,
            "the first matching user rule allows VRR before the system rule"
        );
    }

    #[test]
    // Mutation: skip applications without a matching attribute.
    fn vrr_exclusions_mesa_global_application_applies() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        write(
            roots.mesa_system_dir.join("10-global.conf"),
            r#"<driconf><application name="all"><option name="adaptive_sync" value="false"/></application><application executable="mpv"><option name="adaptive_sync" value="true"/></application></driconf>"#,
        );
        process(&proc_root, 108, "/usr/bin/mpv", "/usr/bin/mpv", &[], None);
        process(&proc_root, 109, "/usr/bin/bar", "/usr/bin/bar", &[], None);
        let exclusions = VrrExclusions::load(&roots);

        assert_eq!(
            exclusions.excluded(108, &proc_root),
            None,
            "the later mpv entry overrides the earlier global false"
        );
        assert_eq!(
            exclusions.excluded(109, &proc_root),
            Some(VrrExclusionSource::MesaDriconf),
            "an application without a matcher applies globally"
        );
    }

    #[test]
    // Mutations: use full-match instead of Mesa's unanchored regex search; treat an invalid regex as global.
    fn vrr_exclusions_mesa_executable_regexp() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        let path = roots.mesa_system_dir.join("10-regex.conf");
        write(
            &path,
            r#"<driconf><application executable_regexp="pv"><option name="adaptive_sync" value="false"/></application></driconf>"#,
        );
        process(&proc_root, 110, "/usr/bin/mpv", "/usr/bin/mpv", &[], None);
        process(&proc_root, 111, "/usr/bin/vlc", "/usr/bin/vlc", &[], None);
        let unanchored = VrrExclusions::load(&roots);
        assert_eq!(
            unanchored.excluded(110, &proc_root),
            Some(VrrExclusionSource::MesaDriconf),
            "Mesa regex matching searches within the process name"
        );
        assert_eq!(unanchored.excluded(111, &proc_root), None);

        write(
            &path,
            r#"<driconf><application executable_regexp="^mp$"><option name="adaptive_sync" value="false"/></application></driconf>"#,
        );
        assert_eq!(
            VrrExclusions::load(&roots).excluded(110, &proc_root),
            None,
            "anchors prevent ^mp$ from matching mpv"
        );

        write(
            &path,
            r#"<driconf><application executable_regexp="["><option name="adaptive_sync" value="false"/></application><application executable="vlc"><option name="adaptive_sync" value="false"/></application></driconf>"#,
        );
        let invalid_regex = VrrExclusions::load(&roots);
        assert_eq!(
            invalid_regex.excluded(111, &proc_root),
            Some(VrrExclusionSource::MesaDriconf),
            "an invalid regex is ignored without losing later entries"
        );
        assert_eq!(
            invalid_regex.excluded(110, &proc_root),
            None,
            "an invalid regex does not apply globally to mpv"
        );
    }

    #[test]
    // Mutation: treat unevaluable application attributes as global matchers.
    fn vrr_exclusions_mesa_unevaluable_attributes_skip() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        write(
            roots.mesa_system_dir.join("10-unevaluable.conf"),
            r#"<driconf><application application_name_match=".*"><option name="adaptive_sync" value="false"/></application><application blake3="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"><option name="adaptive_sync" value="false"/></application><application application_versions="1"><option name="adaptive_sync" value="false"/></application></driconf>"#,
        );
        process(&proc_root, 112, "/usr/bin/foo", "/usr/bin/foo", &[], None);

        assert_eq!(
            VrrExclusions::load(&roots).excluded(112, &proc_root),
            None,
            "yserver cannot evaluate these client-side matching attributes"
        );
    }

    #[test]
    // Mutation: skip applications nested inside device elements with attributes.
    fn vrr_exclusions_mesa_device_scope_ignored() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        write(
            roots.mesa_system_dir.join("10-device.conf"),
            r#"<driconf><device driver="radeonsi"><application executable="foo"><option name="adaptive_sync" value="false"/></application></device></driconf>"#,
        );
        process(&proc_root, 113, "/usr/bin/foo", "/usr/bin/foo", &[], None);

        assert_eq!(
            VrrExclusions::load(&roots).excluded(113, &proc_root),
            Some(VrrExclusionSource::MesaDriconf),
            "unknown client driver scoping is ignored; fixed refresh is the safe error side for the union of both lists"
        );
    }

    #[test]
    // Mutation: restore line-start-only NVIDIA hash comment stripping.
    fn vrr_exclusions_nvidia_inline_hash_comment() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        write(
            roots
                .nvidia_driver_dir
                .join("nvidia-application-profiles-580-rc"),
            r##"{
              "rules": [
                {"pattern": "steam", "profile": ["GLVRRAllowed", false]}, # trailing comment
                {"pattern": {"feature": "procname", "matches": "a#b"}, "profile": ["GLVRRAllowed", false]},
                {"pattern": {"feature": "procname", "matches": "quote\"#tail"}, "profile": ["GLVRRAllowed", false]}
              ],
              "note": "escaped quote \"# remains in the string"
            }"##,
        );
        process(&proc_root, 114, "/usr/bin/steam", "steam", &[], None);
        process(&proc_root, 115, "/usr/bin/a#b", "/usr/bin/a#b", &[], None);
        process(
            &proc_root,
            116,
            "/usr/bin/quote\"#tail",
            "/usr/bin/quote\"#tail",
            &[],
            None,
        );
        let exclusions = VrrExclusions::load(&roots);

        for (pid, name) in [(114, "steam"), (115, "a#b"), (116, "quote\"#tail")] {
            assert_eq!(
                exclusions.excluded(pid, &proc_root),
                Some(VrrExclusionSource::NvidiaProfiles),
                "NVIDIA rule for {name} survives outside-string comments"
            );
        }
    }

    #[test]
    // Mutation: ignore Mesa adaptive_sync=false entries.
    fn vrr_exclusions_mesa_entry_excludes() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        write(
            roots.mesa_system_dir.join("10-app.conf"),
            r#"<driconf><device><application name="Browser" executable="firefox"><option name="adaptive_sync" value="false"/></application></device></driconf>"#,
        );
        process(
            &proc_root,
            21,
            "/usr/lib/firefox",
            "/usr/lib/firefox",
            &[],
            None,
        );

        assert_eq!(
            VrrExclusions::load(&roots).excluded(21, &proc_root),
            Some(VrrExclusionSource::MesaDriconf)
        );
    }

    #[test]
    // Mutation: revert to Document::parse, which rejects driconf internal DTDs.
    fn vrr_exclusions_mesa_entry_with_internal_dtd_excludes() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        write(
            roots.mesa_system_dir.join("10-firefox.conf"),
            r#"<?xml version="1.0" standalone="yes"?>
<!DOCTYPE driconf [
  <!ELEMENT driconf (device+)>
  <!ELEMENT device (application+)>
  <!ELEMENT application (option+)>
  <!ATTLIST application executable CDATA #REQUIRED>
  <!ELEMENT option EMPTY>
  <!ATTLIST option name CDATA #REQUIRED value CDATA #REQUIRED>
]>
<driconf><device><application executable="firefox"><option name="adaptive_sync" value="false"/></application></device></driconf>"#,
        );
        process(
            &proc_root,
            41,
            "/usr/lib/firefox",
            "/usr/lib/firefox",
            &[],
            None,
        );

        assert_eq!(
            VrrExclusions::load(&roots).excluded(41, &proc_root),
            Some(VrrExclusionSource::MesaDriconf)
        );
    }

    #[test]
    // Mutation: accept only object patterns and ignore NVIDIA string shorthand.
    fn vrr_exclusions_nvidia_procname_excludes() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        write(
            roots
                .nvidia_driver_dir
                .join("nvidia-application-profiles-580-rc"),
            r##"{
          "rules": [
            {"pattern":{"feature":"procname","matches":"firefox"},"profile":["GLVRRAllowed",false]},
            {"pattern":"steam","profile":["GLVRRAllowed",false]}
          ]
        }"##,
        );
        process(
            &proc_root,
            22,
            "/usr/lib/firefox",
            "/usr/lib/firefox",
            &[],
            None,
        );
        process(&proc_root, 23, "/home/user/steam", "steam", &[], None);
        let exclusions = VrrExclusions::load(&roots);

        assert_eq!(
            exclusions.excluded(22, &proc_root),
            Some(VrrExclusionSource::NvidiaProfiles)
        );
        assert_eq!(
            exclusions.excluded(23, &proc_root),
            Some(VrrExclusionSource::NvidiaProfiles)
        );
    }

    #[test]
    // Mutation: default an unlisted process to excluded.
    fn vrr_exclusions_unlisted_allowed() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        process(
            &proc_root,
            24,
            "/usr/bin/terminal",
            "/usr/bin/terminal",
            &[],
            None,
        );

        assert_eq!(VrrExclusions::load(&roots).excluded(24, &proc_root), None);
    }

    #[test]
    // Mutation: ignore later user files when resolving a name's setting.
    fn vrr_exclusions_later_file_lifts() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let home = tree.path().join("home/user");
        let proc_root = tree.path().join("proc");
        write(
            roots
                .nvidia_driver_dir
                .join("nvidia-application-profiles-580-rc"),
            r#"{"rules":[{"pattern":"firefox","profile":["GLVRRAllowed",false]}]}"#,
        );
        write(
            home.join(".nv/nvidia-application-profiles-rc"),
            r#"{"rules":[{"pattern":"firefox","profile":["GLVRRAllowed",true]}]}"#,
        );
        write(
            roots.mesa_system_dir.join("10-firefox.conf"),
            r#"<driconf><device><application executable="firefox"><option name="adaptive_sync" value="false"/></application></device></driconf>"#,
        );
        write(
            home.join(".drirc"),
            r#"<driconf><device><application executable="firefox"><option name="adaptive_sync" value="true"/></application></device></driconf>"#,
        );
        process(
            &proc_root,
            25,
            "/usr/lib/firefox",
            "/usr/lib/firefox",
            &[],
            Some(home.as_path()),
        );

        assert_eq!(VrrExclusions::load(&roots).excluded(25, &proc_root), None);
    }

    #[test]
    // Mutation: skip profiles defined in later NVIDIA files.
    fn vrr_exclusions_nvidia_named_profile_cross_file() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let home = tree.path().join("home/user");
        let proc_root = tree.path().join("proc");
        write(
            home.join(".nv/nvidia-application-profiles-rc"),
            r#"{"rules":[{"pattern":"firefox","profile":"deny"}]}"#,
        );
        write(
            roots
                .nvidia_driver_dir
                .join("nvidia-application-profiles-580-rc"),
            r#"{"profiles":[{"name":"deny","settings":["GLVRRAllowed",false]}]}"#,
        );
        process(
            &proc_root,
            37,
            "/usr/lib/firefox",
            "/usr/lib/firefox",
            &[],
            Some(home.as_path()),
        );

        assert_eq!(
            VrrExclusions::load(&roots).excluded(37, &proc_root),
            Some(VrrExclusionSource::NvidiaProfiles)
        );
    }

    #[test]
    // Mutation: let a lower-precedence NVIDIA rule override a user rule referencing a later profile.
    fn vrr_exclusions_nvidia_named_profile_user_override() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let home = tree.path().join("home/user");
        let proc_root = tree.path().join("proc");
        write(
            home.join(".nv/nvidia-application-profiles-rc"),
            r#"{"rules":[{"pattern":"firefox","profile":"allow"}]}"#,
        );
        write(
            roots
                .nvidia_driver_dir
                .join("nvidia-application-profiles-580-rc"),
            r#"{"profiles":[{"name":"allow","settings":["GLVRRAllowed",true]}]}"#,
        );
        write(
            roots
                .nvidia_driver_dir
                .join("nvidia-application-profiles-580.1-rc"),
            r#"{"rules":[{"pattern":"firefox","profile":["GLVRRAllowed",false]}]}"#,
        );
        process(
            &proc_root,
            38,
            "/usr/lib/firefox",
            "/usr/lib/firefox",
            &[],
            Some(home.as_path()),
        );

        assert_eq!(VrrExclusions::load(&roots).excluded(38, &proc_root), None);
    }

    #[test]
    // Mutation: let NVIDIA file entries override environment values.
    fn vrr_exclusions_environment_wins() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        write(
            roots
                .nvidia_driver_dir
                .join("nvidia-application-profiles-580-rc"),
            r#"{"rules":[{"pattern":"listed","profile":["GLVRRAllowed",false]}]}"#,
        );
        process(
            &proc_root,
            26,
            "/usr/bin/unlisted",
            "/usr/bin/unlisted",
            &["__GL_VRR_ALLOWED=0"],
            None,
        );
        process(
            &proc_root,
            27,
            "/usr/bin/listed",
            "/usr/bin/listed",
            &["__GL_VRR_ALLOWED=1"],
            None,
        );
        process(
            &proc_root,
            28,
            "/usr/bin/unlisted",
            "/usr/bin/unlisted",
            &["adaptive_sync=false"],
            None,
        );
        process(
            &proc_root,
            29,
            "/usr/bin/unlisted",
            "/usr/bin/unlisted",
            &["__GL_VRR_ALLOWED=1", "adaptive_sync=0"],
            None,
        );
        let exclusions = VrrExclusions::load(&roots);

        assert_eq!(
            exclusions.excluded(26, &proc_root),
            Some(VrrExclusionSource::Environment)
        );
        assert_eq!(exclusions.excluded(27, &proc_root), None);
        assert_eq!(
            exclusions.excluded(28, &proc_root),
            Some(VrrExclusionSource::Environment)
        );
        assert_eq!(
            exclusions.excluded(29, &proc_root),
            Some(VrrExclusionSource::Environment)
        );
    }

    #[test]
    // Mutation: split argv[0] on whitespace before applying the executable prefix rule.
    fn vrr_exclusions_process_name_like_mesa() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        process(
            &proc_root,
            30,
            "/opt/x/firefox",
            "/opt/x/firefox --flag",
            &[],
            None,
        );
        process(
            &proc_root,
            31,
            "/home/u/.steam/ubuntu12_32/steam",
            "steam",
            &[],
            None,
        );
        process(
            &proc_root,
            32,
            "/opt/x/other",
            "/usr/bin/unrelated",
            &[],
            None,
        );
        process(
            &proc_root,
            36,
            "/opt/x/firefox",
            "/opt/x/firefox-wrapper",
            &[],
            None,
        );
        process(
            &proc_root,
            37,
            "/opt/My Game",
            "/opt/My Game --flag",
            &[],
            None,
        );
        write(
            roots
                .nvidia_driver_dir
                .join("nvidia-application-profiles-580-rc"),
            r#"{"rules":[{"pattern":"firefox","profile":["GLVRRAllowed",false]},{"pattern":"steam","profile":["GLVRRAllowed",false]},{"pattern":"unrelated","profile":["GLVRRAllowed",false]},{"pattern":"My Game","profile":["GLVRRAllowed",false]}]}"#,
        );
        let exclusions = VrrExclusions::load(&roots);

        assert_eq!(
            exclusions.excluded(30, &proc_root),
            Some(VrrExclusionSource::NvidiaProfiles)
        );
        assert_eq!(
            exclusions.excluded(31, &proc_root),
            Some(VrrExclusionSource::NvidiaProfiles)
        );
        assert_eq!(
            exclusions.excluded(32, &proc_root),
            Some(VrrExclusionSource::NvidiaProfiles)
        );
        assert_eq!(
            exclusions.excluded(36, &proc_root),
            Some(VrrExclusionSource::NvidiaProfiles)
        );
        assert_eq!(
            exclusions.excluded(37, &proc_root),
            Some(VrrExclusionSource::NvidiaProfiles)
        );
    }

    #[test]
    // Mutation: abort loading all sources if one NVIDIA directory is missing or a Mesa file is malformed.
    fn vrr_exclusions_bad_files() {
        let tree = TempTree::new();
        let first_roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        write(
            first_roots.mesa_system_dir.join("10-broken.conf"),
            "<driconf><application>",
        );
        write(
            first_roots.mesa_etc_file.clone(),
            r#"<driconf><device><application executable="firefox"><option name="adaptive_sync" value="false"/></application></device></driconf>"#,
        );
        process(
            &proc_root,
            33,
            "/usr/lib/firefox",
            "/usr/lib/firefox",
            &[],
            None,
        );

        assert_eq!(
            VrrExclusions::load(&first_roots).excluded(33, &proc_root),
            Some(VrrExclusionSource::MesaDriconf)
        );

        let nvidia_tree = TempTree::new();
        let nvidia_roots = roots(&nvidia_tree);
        let nvidia_proc = nvidia_tree.path().join("proc");
        write(
            nvidia_roots.mesa_etc_file.clone(),
            r#"<driconf><device><application executable="steam"><option name="adaptive_sync" value="false"/></application></device></driconf>"#,
        );
        process(
            &nvidia_proc,
            34,
            "/usr/bin/steam",
            "/usr/bin/steam",
            &[],
            None,
        );
        assert_eq!(
            VrrExclusions::load(&nvidia_roots).excluded(34, &nvidia_proc),
            Some(VrrExclusionSource::MesaDriconf)
        );

        let comments_tree = TempTree::new();
        let comments_roots = roots(&comments_tree);
        let comments_proc = comments_tree.path().join("proc");
        write(
            comments_roots
                .nvidia_driver_dir
                .join("nvidia-application-profiles-580-rc"),
            "# Synthetic comment line\n{\"rules\":[{\"pattern\":\"steam\",\"profile\":[\"GLVRRAllowed\",false]}]}\n",
        );
        process(
            &comments_proc,
            35,
            "/usr/bin/steam",
            "/usr/bin/steam",
            &[],
            None,
        );
        assert_eq!(
            VrrExclusions::load(&comments_roots).excluded(35, &comments_proc),
            Some(VrrExclusionSource::NvidiaProfiles)
        );
    }

    // Maintainer reproductions for PR #298 review points 3 and 2 (verbatim from the PR thread,
    // except: the second writes /etc/drirc (roots.mesa_etc_file) instead of roots.home/.drirc,
    // since roots no longer carry a home after point 1, and pass `None` as the new HOME
    // argument of `process`).
    #[test]
    fn review_vrr_nvidia_inline_comment_preserves_exclusion() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        write(
            roots
                .nvidia_driver_dir
                .join("nvidia-application-profiles-580-rc"),
            r##"{
                "rules": [ {"pattern":"steam", "profile":["GLVRRAllowed",false]} ] # valid NVIDIA comment
            }"##,
        );
        process(&proc_root, 21, "/usr/bin/steam", "steam", &[], None);
        assert_eq!(
            VrrExclusions::load(&roots).excluded(21, &proc_root),
            Some(VrrExclusionSource::NvidiaProfiles)
        );
    }

    #[test]
    fn review_vrr_driconf_global_exclusion_is_honoured() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        write(
            &roots.mesa_etc_file,
            r#"<driconf><device><application name="all"><option name="adaptive_sync" value="false"/></application></device></driconf>"#,
        );
        process(&proc_root, 21, "/usr/bin/game", "/usr/bin/game", &[], None);
        assert_eq!(
            VrrExclusions::load(&roots).excluded(21, &proc_root),
            Some(VrrExclusionSource::MesaDriconf)
        );
    }

    #[test]
    // Mutation: let a matching executable bypass Mesa's executable_regexp condition.
    fn review_vrr_executable_match_does_not_bypass_regexp() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        write(
            &roots.mesa_etc_file,
            r#"<driconf><device>
                <application executable="game"><option name="adaptive_sync" value="false"/></application>
                <application executable="game" executable_regexp="^other$"><option name="adaptive_sync" value="true"/></application>
            </device></driconf>"#,
        );
        process(&proc_root, 202, "/usr/bin/game", "game", &[], None);
        assert_eq!(
            VrrExclusions::load(&roots).excluded(202, &proc_root),
            Some(VrrExclusionSource::MesaDriconf),
            "matching executable does not bypass Mesa's executable_regexp condition"
        );
    }

    #[test]
    // Mutation: allow a driver-scoped Mesa true entry to cancel a global exclusion.
    fn review_vrr_unknown_driver_allow_must_not_lift_global_exclusion() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        write(
            &roots.mesa_etc_file,
            r#"<driconf>
                <device><application executable="game"><option name="adaptive_sync" value="false"/></application></device>
                <device driver="radeonsi"><application executable="game"><option name="adaptive_sync" value="true"/></application></device>
            </driconf>"#,
        );
        process(&proc_root, 201, "/usr/bin/game", "game", &[], None);
        assert_eq!(
            VrrExclusions::load(&roots).excluded(201, &proc_root),
            Some(VrrExclusionSource::MesaDriconf),
            "an allow for an unidentified driver must not cancel a general exclusion"
        );
    }

    #[test]
    // Mutations: use executable alone; evaluate every present matcher instead of Mesa's first one.
    fn vrr_exclusions_mesa_matcher_chain_like_mesa() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        process(&proc_root, 203, "/usr/bin/game", "game", &[], None);
        let path = roots.mesa_etc_file.clone();
        let cases = [
            (
                r#"<application executable="game" blake3="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"><option name="adaptive_sync" value="false"/></application>"#,
                None,
                "unavailable blake3 after executable does not apply",
            ),
            (
                r#"<application executable="game" application_name_match="x"><option name="adaptive_sync" value="false"/></application>"#,
                None,
                "unavailable application_name_match after executable does not apply",
            ),
            (
                r#"<application executable="game" application_versions="1:2"><option name="adaptive_sync" value="false"/></application>"#,
                None,
                "unavailable application_versions independently prevents applying the entry",
            ),
            (
                r#"<application executable="other" executable_regexp="ga"><option name="adaptive_sync" value="false"/></application>"#,
                None,
                "executable mismatch prevents the regexp from applying",
            ),
            (
                r#"<application executable_regexp="ga" blake3="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"><option name="adaptive_sync" value="false"/></application>"#,
                Some(VrrExclusionSource::MesaDriconf),
                "regexp is evaluated and blake3 is ignored when both are present",
            ),
        ];
        for (application, expected, message) in cases {
            let contents = format!("<driconf><device>{application}</device></driconf>");
            write(&path, &contents);
            assert_eq!(
                VrrExclusions::load(&roots).excluded(203, &proc_root),
                expected,
                "{message}"
            );
        }
    }

    #[test]
    // Mutation: drop every true adaptive_sync entry inside a device element.
    fn vrr_exclusions_mesa_generic_allow_still_lifts() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        process(&proc_root, 204, "/usr/bin/game", "game", &[], None);
        write(
            &roots.mesa_etc_file,
            r#"<driconf>
                <device><application executable="game"><option name="adaptive_sync" value="false"/></application></device>
                <device><application executable="game"><option name="adaptive_sync" value="true"/></application></device>
            </driconf>"#,
        );
        assert_eq!(
            VrrExclusions::load(&roots).excluded(204, &proc_root),
            None,
            "a later generic-device allow lifts the general exclusion"
        );

        write(
            &roots.mesa_etc_file,
            r#"<driconf><device driver="radeonsi"><application executable="game"><option name="adaptive_sync" value="false"/></application></device></driconf>"#,
        );
        assert_eq!(
            VrrExclusions::load(&roots).excluded(204, &proc_root),
            Some(VrrExclusionSource::MesaDriconf),
            "an unknown driver scope remains an exclusion"
        );
    }

    #[test]
    // Mutation: remove O_NONBLOCK so opening a client-controlled FIFO blocks the evaluation thread.
    fn vrr_exclusions_list_file_fifo_is_skipped() {
        use std::{
            ffi::CString, os::unix::ffi::OsStrExt, process::Command, sync::mpsc, time::Duration,
        };

        let args: Vec<_> = std::env::args().collect();
        if let Some(index) = args
            .iter()
            .position(|argument| argument == "--fifo-log-child")
        {
            let tree = std::path::PathBuf::from(&args[index + 1]);
            log::set_logger(&FIFO_TEST_LOGGER).expect("install isolated FIFO test logger");
            log::set_max_level(log::LevelFilter::Warn);
            let roots = VrrListRoots {
                nvidia_driver_dir: tree.join("usr/share/nvidia"),
                nvidia_etc_dir: tree.join("etc/nvidia"),
                mesa_system_dir: tree.join("usr/share/drirc.d"),
                mesa_etc_file: tree.join("etc/drirc"),
            };
            assert_eq!(
                VrrExclusions::load(&roots).excluded(205, &tree.join("proc")),
                None,
                "a FIFO contributes no user exclusion"
            );
            return;
        }

        let tree = TempTree::new();
        let roots = roots(&tree);
        let home = tree.path().join("home/game");
        std::fs::create_dir_all(&home).expect("create client HOME");
        let fifo = CString::new(home.join(".drirc").as_os_str().as_bytes())
            .expect("FIFO path contains no NUL");
        // SAFETY: mkfifo only reads the nul-terminated path and creates this fixture FIFO.
        let result = unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) };
        assert_eq!(result, 0, "create client-controlled drirc FIFO");

        write(
            &roots.mesa_etc_file,
            r#"<driconf><device><application executable="system-app"><option name="adaptive_sync" value="false"/></application></device></driconf>"#,
        );
        let proc_root = tree.path().join("proc");
        process(&proc_root, 205, "/usr/bin/game", "game", &[], Some(&home));
        process(
            &proc_root,
            206,
            "/usr/bin/system-app",
            "system-app",
            &[],
            None,
        );

        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let exclusions = VrrExclusions::load(&roots);
            let result = (
                exclusions.excluded(205, &proc_root),
                exclusions.excluded(206, &proc_root),
            );
            let _ = sender.send(result);
        });
        let (home_result, system_result) = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("opening and skipping the HOME FIFO must complete promptly");
        assert_eq!(
            home_result, None,
            "FIFO contents do not add a user exclusion"
        );
        assert_eq!(
            system_result,
            Some(VrrExclusionSource::MesaDriconf),
            "a skipped user FIFO does not prevent system list entries from applying"
        );

        let output = Command::new(std::env::current_exe().expect("current test binary path"))
            .args([
                "--exact",
                "vrr_exclusions::tests::vrr_exclusions_list_file_fifo_is_skipped",
                "--nocapture",
                "--",
                "--fifo-log-child",
            ])
            .arg(tree.path())
            .output()
            .expect("run the isolated FIFO warning check");
        let output_text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "isolated FIFO warning check failed: {output_text}"
        );
        assert!(
            output_text.contains("kms: skipping non-regular Mesa driconf"),
            "the FIFO is skipped by the regular-file check: {output_text}"
        );
    }

    #[test]
    // Mutation: remove the 1 MiB list-file size cap.
    fn vrr_exclusions_list_file_over_cap_is_skipped() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let home = tree.path().join("home/game");
        let proc_root = tree.path().join("proc");
        process(&proc_root, 207, "/usr/bin/game", "game", &[], Some(&home));
        let prefix = r#"<driconf><device><application executable="game"><option name="adaptive_sync" value="false"/></application><!--"#;
        let suffix = "--></device></driconf>";
        let cap = 1_048_576usize;
        for (size, expected, message) in [
            (cap + 1, None, "a user file one byte over 1 MiB is skipped"),
            (
                cap - 1,
                Some(VrrExclusionSource::MesaDriconf),
                "a valid user file just under 1 MiB is applied",
            ),
        ] {
            let comment_len = size - prefix.len() - suffix.len();
            let contents = format!("{prefix}{}{suffix}", "x".repeat(comment_len));
            assert_eq!(contents.len(), size, "fixture has the requested byte size");
            write(home.join(".drirc"), &contents);
            assert_eq!(
                VrrExclusions::load(&roots).excluded(207, &proc_root),
                expected,
                "{message}"
            );
        }
    }

    #[test]
    // Mutation: remove the per-HOME cache bound.
    fn vrr_exclusions_home_cache_is_bounded() {
        let tree = TempTree::new();
        let roots = roots(&tree);
        let proc_root = tree.path().join("proc");
        let exclusions = VrrExclusions::load(&roots);
        for index in 0..65 {
            let home = tree.path().join(format!("home/{index}"));
            let pid = 300 + index;
            process(
                &proc_root,
                pid,
                "/usr/bin/unlisted",
                "unlisted",
                &[],
                Some(&home),
            );
            assert_eq!(
                exclusions.excluded(pid, &proc_root),
                None,
                "an unlisted client remains eligible while populating the HOME cache"
            );
        }
        assert!(
            exclusions.user_lists.borrow().len() <= 64,
            "the cache remains bounded after the 65th distinct HOME"
        );
    }
}
