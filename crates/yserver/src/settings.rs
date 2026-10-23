//! The app's settings (`docs/superpowers/specs/2026-10-23-macos-settings-design.md`):
//! values under string keys in a [`Store`] (`NSUserDefaults` on macOS),
//! merged under argv at launch by [`crate::launch::apply_settings`].

use crate::launch::ScaleOption;

pub const KEY_MODE: &str = "mode";
pub const KEY_EMULATE_BUTTONS: &str = "emulate_buttons";
pub const KEY_KEY_EQUIVALENTS: &str = "key_equivalents";
pub const KEY_SYNC_CLIPBOARD: &str = "sync_clipboard";
pub const KEY_SCALE: &str = "scale";

/// Where the settings live; a missing or malformed value reads as `None`.
pub trait Store {
    fn string(&self, key: &str) -> Option<String>;
    fn bool(&self, key: &str) -> Option<bool>;
    fn set_string(&mut self, key: &str, value: &str);
    fn set_bool(&mut self, key: &str, value: bool);
}

/// Rootless or one window for the root; applies at the next launch.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Rootless,
    Window,
}

impl Mode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "rootless" => Some(Self::Rootless),
            "window" => Some(Self::Window),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Rootless => "rootless",
            Self::Window => "window",
        }
    }
}

/// The settings that apply immediately, read on the main thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Live {
    /// Option-click is button 2, Command-click button 3.
    pub emulate_buttons: bool,
    /// Command keys go to the app menu first (Cmd-Q, Cmd-H, Cmd-,).
    pub key_equivalents: bool,
    /// CLIPBOARD and the Mac pasteboard are bridged.
    pub sync_clipboard: bool,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            emulate_buttons: true,
            key_equivalents: true,
            sync_clipboard: true,
        }
    }
}

/// Every stored setting.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub mode: Mode,
    pub scale: ScaleOption,
    pub live: Live,
}

impl Settings {
    /// The stored values, defaults for the missing or malformed ones.
    pub fn load(store: &impl Store) -> Self {
        let defaults = Self::default();
        let flag = |key, default| store.bool(key).unwrap_or(default);
        Self {
            mode: store
                .string(KEY_MODE)
                .and_then(|v| Mode::parse(&v))
                .unwrap_or(defaults.mode),
            scale: store
                .string(KEY_SCALE)
                .and_then(|v| crate::launch::parse_scale(&v).ok())
                .unwrap_or(defaults.scale),
            live: Live {
                emulate_buttons: flag(KEY_EMULATE_BUTTONS, defaults.live.emulate_buttons),
                key_equivalents: flag(KEY_KEY_EQUIVALENTS, defaults.live.key_equivalents),
                sync_clipboard: flag(KEY_SYNC_CLIPBOARD, defaults.live.sync_clipboard),
            },
        }
    }

    /// Write every value.
    pub fn save(&self, store: &mut impl Store) {
        store.set_string(KEY_MODE, self.mode.as_str());
        store.set_string(KEY_SCALE, &scale_str(self.scale));
        store.set_bool(KEY_EMULATE_BUTTONS, self.live.emulate_buttons);
        store.set_bool(KEY_KEY_EQUIVALENTS, self.live.key_equivalents);
        store.set_bool(KEY_SYNC_CLIPBOARD, self.live.sync_clipboard);
    }
}

fn scale_str(scale: ScaleOption) -> String {
    match scale {
        ScaleOption::Auto => "auto".to_owned(),
        ScaleOption::Factor(n) => n.to_string(),
    }
}

/// Which launch-time settings a command-line flag fixed for this run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SetByArgv {
    /// `--rooted` or `--rootless`.
    pub mode: bool,
    /// `--scale`.
    pub scale: bool,
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use super::*;

    /// A store of plain strings, as the property list keeps them.
    #[derive(Debug, Default, Clone, PartialEq, Eq)]
    pub(crate) struct MapStore(pub(crate) HashMap<String, String>);

    impl Store for MapStore {
        fn string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }

        fn bool(&self, key: &str) -> Option<bool> {
            self.0.get(key).and_then(|v| v.parse().ok())
        }

        fn set_string(&mut self, key: &str, value: &str) {
            self.0.insert(key.to_owned(), value.to_owned());
        }

        fn set_bool(&mut self, key: &str, value: bool) {
            self.0.insert(key.to_owned(), value.to_string());
        }
    }

    pub(crate) fn store(pairs: &[(&str, &str)]) -> MapStore {
        MapStore(
            pairs
                .iter()
                .map(|&(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        )
    }

    #[test]
    fn an_empty_store_gives_the_spec_defaults() {
        let s = Settings::load(&MapStore::default());
        assert_eq!(s.mode, Mode::Rootless);
        assert_eq!(s.scale, ScaleOption::Auto);
        assert!(s.live.emulate_buttons);
        assert!(s.live.key_equivalents);
        assert!(s.live.sync_clipboard);
    }

    #[test]
    fn stored_values_are_read_and_malformed_ones_default() {
        let s = Settings::load(&store(&[
            (KEY_MODE, "window"),
            (KEY_SCALE, "2"),
            (KEY_EMULATE_BUTTONS, "false"),
            (KEY_KEY_EQUIVALENTS, "false"),
            (KEY_SYNC_CLIPBOARD, "false"),
        ]));
        assert_eq!(s.mode, Mode::Window);
        assert_eq!(s.scale, ScaleOption::Factor(2));
        assert_eq!(
            s.live,
            Live {
                emulate_buttons: false,
                key_equivalents: false,
                sync_clipboard: false
            }
        );
        let s = Settings::load(&store(&[
            (KEY_MODE, "fullscreen"),
            (KEY_SCALE, "3"),
            (KEY_EMULATE_BUTTONS, "maybe"),
        ]));
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let s = Settings {
            mode: Mode::Window,
            scale: ScaleOption::Factor(1),
            live: Live {
                emulate_buttons: false,
                key_equivalents: true,
                sync_clipboard: false,
            },
        };
        let mut st = MapStore::default();
        s.save(&mut st);
        assert_eq!(Settings::load(&st), s);
    }
}
