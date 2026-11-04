//! The settings as `key=value` lines in a text file, for a platform without
//! a settings store of its own (Windows: `%APPDATA%\yserver\settings.conf`).
//! Every change is written at once; keys this build does not know survive.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use super::Store;

/// The settings file and its values.
#[derive(Debug)]
pub struct FileStore {
    path: PathBuf,
    values: BTreeMap<String, String>,
}

impl FileStore {
    /// The values in `path`; none when it is missing or unreadable.
    #[must_use]
    pub fn open(path: PathBuf) -> Self {
        let values = match fs::read_to_string(&path) {
            Ok(text) => parse(&text),
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    log::warn!("settings: cannot read {}: {e}", path.display());
                }
                BTreeMap::new()
            }
        };
        Self { path, values }
    }

    /// Write every value, creating the directory: a whole new file renamed
    /// over the old one, so a reader never sees half of it.
    fn save(&self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let partial = self.path.with_extension("tmp");
        fs::write(&partial, to_text(&self.values))?;
        fs::rename(&partial, &self.path)
    }

    fn set(&mut self, key: &str, value: String) {
        self.values.insert(key.to_owned(), value);
        if let Err(e) = self.save() {
            log::warn!("settings: cannot write {}: {e}", self.path.display());
        }
    }

    /// Where the values live.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Store for FileStore {
    fn string(&self, key: &str) -> Option<String> {
        self.values.get(key).cloned()
    }

    fn bool(&self, key: &str) -> Option<bool> {
        self.values.get(key).and_then(|v| v.parse().ok())
    }

    fn set_string(&mut self, key: &str, value: &str) {
        self.set(key, value.to_owned());
    }

    fn set_bool(&mut self, key: &str, value: bool) {
        self.set(key, value.to_string());
    }
}

/// `key=value` lines, both sides trimmed; blank lines, `#` comments and
/// lines without `=` are skipped, and a later line wins.
fn parse(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .filter(|(key, _)| !key.is_empty())
        .collect()
}

/// One `key=value` line per value, sorted by key.
fn to_text(values: &BTreeMap<String, String>) -> String {
    values.iter().map(|(k, v)| format!("{k}={v}\n")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        launch::ScaleOption,
        settings::{KEY_MODE, KEY_SCALE, KEY_SYNC_CLIPBOARD, Live, Mode, Settings},
    };

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("yserver-settings-{tag}-{}", std::process::id()))
            .join("settings.conf")
    }

    #[test]
    fn lines_parse_to_trimmed_pairs() {
        let values = parse("# yserver\n\nmode = window\nscale=2\r\nnonsense\n=x\nmode=rootless\n");
        let expected: BTreeMap<String, String> = [("mode", "rootless"), ("scale", "2")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        assert_eq!(values, expected);
    }

    #[test]
    fn text_is_one_sorted_line_per_value() {
        let values = parse("sync_clipboard=false\nmode=window\n");
        assert_eq!(to_text(&values), "mode=window\nsync_clipboard=false\n");
        assert_eq!(parse(&to_text(&values)), values);
    }

    #[test]
    fn a_missing_file_reads_as_the_defaults() {
        let store = FileStore::open(temp_path("missing"));
        assert_eq!(Settings::load(&store), Settings::default());
    }

    #[test]
    fn every_change_is_written_and_unknown_keys_survive() {
        let path = temp_path("write");
        let dir = path.parent().unwrap().to_owned();
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "future_option=on\n").unwrap();

        let mut store = FileStore::open(path.clone());
        store.set_string(KEY_MODE, "window");
        store.set_bool(KEY_SYNC_CLIPBOARD, false);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "future_option=on\nmode=window\nsync_clipboard=false\n"
        );

        let settings = Settings::load(&FileStore::open(path.clone()));
        assert_eq!(settings.mode, Mode::Window);
        assert_eq!(settings.scale, ScaleOption::Auto);
        assert_eq!(
            settings.live,
            Live {
                sync_clipboard: false,
                ..Live::default()
            }
        );

        let mut store = FileStore::open(path.clone());
        store.set_string(KEY_SCALE, "1");
        let settings = Settings::load(&FileStore::open(path));
        assert_eq!(settings.scale, ScaleOption::Factor(1));
        assert_eq!(settings.mode, Mode::Window);
        fs::remove_dir_all(dir).unwrap();
    }
}
