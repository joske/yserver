//! The settings in `NSUserDefaults.standardUserDefaults` (the bundle id's
//! domain when run from the bundle).

use objc2::rc::Retained;
use objc2_foundation::{NSString, NSUserDefaults};

use crate::settings::{Settings, Store};

/// The standard user defaults as a settings store.
pub(super) struct UserDefaults(Retained<NSUserDefaults>);

impl UserDefaults {
    pub(super) fn standard() -> Self {
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
