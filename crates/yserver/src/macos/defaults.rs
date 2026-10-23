//! The settings in the `io.github.joske.yserver` defaults domain: the
//! standard user defaults when run from the bundle, the same domain by name
//! otherwise, so a Settings change made in a non-bundle run reaches the app.

use objc2::{AllocAnyThread, rc::Retained};
use objc2_foundation::{NSBundle, NSString, NSUserDefaults};

use crate::settings::{Settings, Store};

/// The standard user defaults as a settings store.
pub(super) struct UserDefaults(Retained<NSUserDefaults>);

impl UserDefaults {
    pub(super) fn standard() -> Self {
        const DOMAIN: &str = "io.github.joske.yserver";
        // A suite named after the main bundle's own id is not allowed; the
        // standard defaults already are that domain (as XQuartz does).
        let is_bundle = NSBundle::mainBundle()
            .bundleIdentifier()
            .is_some_and(|id| id.to_string() == DOMAIN);
        if !is_bundle
            && let Some(suite) = NSUserDefaults::initWithSuiteName(
                NSUserDefaults::alloc(),
                Some(&NSString::from_str(DOMAIN)),
            )
        {
            return Self(suite);
        }
        Self(NSUserDefaults::standardUserDefaults())
    }
}

impl Store for UserDefaults {
    fn string(&self, key: &str) -> Option<String> {
        self.0
            .stringForKey(&NSString::from_str(key))
            .map(|value| value.to_string())
    }

    fn bool(&self, key: &str) -> Option<bool> {
        let key = NSString::from_str(key);
        // `boolForKey:` reads a missing key as NO.
        self.0.objectForKey(&key).map(|_| self.0.boolForKey(&key))
    }

    fn set_string(&mut self, key: &str, value: &str) {
        let value = NSString::from_str(value);
        // SAFETY: a string is a property-list object.
        unsafe {
            self.0
                .setObject_forKey(Some(&value), &NSString::from_str(key))
        };
    }

    fn set_bool(&mut self, key: &str, value: bool) {
        self.0.setBool_forKey(value, &NSString::from_str(key));
    }
}

/// The stored settings, defaults for the missing ones.
pub(crate) fn stored_settings() -> Settings {
    Settings::load(&UserDefaults::standard())
}
