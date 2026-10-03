//! libinput ↔ yserver config conversion.
//!
//! Split from `context.rs` so the libinput-config surface stays in a
//! focused module. Two public entry points:
//!
//! * [`gather`] — build a [`LibinputConfigSnapshot`] from the live
//!   `&input::Device` at `DeviceAdded` for touchpads. Each field's
//!   `available` / `current` / `default` triplet comes straight from
//!   the matching `config_*_is_available` / `config_*_*enabled` /
//!   `config_*_default_*enabled` libinput getter (or the closest
//!   equivalent when libinput doesn't expose an explicit availability
//!   probe).
//!
//! * [`apply`] — route a decoded [`DeviceConfigChange`] to the matching
//!   `config_*_set_*` setter on the live device, then collapse
//!   libinput's [`input::DeviceConfigError`] onto yserver's
//!   [`DeviceConfigError`]. Caller keys the live handle by source ID and
//!   reports an absent source as `SourceGone` (see
//!   [`super::Context::apply_device_config`]).
//!
//! The six bit-mapping helpers (`*_bit` / `send_events_*_mask`) are
//! private to this module — they encode the snapshot's slot order,
//! which is part of the X11 property wire layout and shouldn't leak
//! beyond the conversion boundary.
//!
//! All `input::*` enums are `#[non_exhaustive]`; the bit-mapping helpers
//! degrade unknown-future variants to "not in our snapshot mask" rather
//! than panic. The apply dispatch, in contrast, exhausts every validated
//! index — `decode_change` only emits values in `0..n` for each
//! one-hot / one-hot-or-none, so any other index is an invariant break
//! that should fail loud (`unreachable!`), not silently route to a
//! default variant.

use input::{
    AccelProfile, AsRaw, ClickMethod, Device, DragLockState, ScrollButtonLockState, ScrollMethod,
    SendEventsMode, TapButtonMap,
};
use yserver_core::{
    core_loop::message::{
        BitFlags2, BoolSetting, FloatSetting, LibinputConfigSnapshot, OneHot2, OneHot3, U32Setting,
    },
    xinput::libinput_props::{DeviceConfigChange, DeviceConfigError},
};

/// Map [`AccelProfile`] to the bit index used by the current/default
/// `accel_profile` slots (Adaptive=0, Flat=1). `Custom` remains out of
/// scope for writes on this build.
fn accel_profile_bit(profile: AccelProfile) -> Option<u8> {
    match profile {
        AccelProfile::Adaptive => Some(0),
        AccelProfile::Flat => Some(1),
        // `Custom` is only present under `libinput_1_23`; ignore so the
        // snapshot stays at 2 slots and the property table's
        // `OneHotOrNone { n: 2 }` validator stays well-defined.
        _ => None,
    }
}

/// Map [`ScrollMethod`] to the bit index used by the `scroll_method`
/// `OneHot3` slot. `NoScroll` is the "disabled" sentinel (no bit) and
/// returns `None`; the snapshot's `current`/`default` fields are `None`
/// in that case while `available_mask` ORs in the corresponding slot
/// for each concrete method libinput exposes on the device.
fn scroll_method_bit(method: ScrollMethod) -> Option<u8> {
    match method {
        ScrollMethod::NoScroll => None,
        ScrollMethod::TwoFinger => Some(0),
        ScrollMethod::Edge => Some(1),
        ScrollMethod::OnButtonDown => Some(2),
        // `ScrollMethod` is `#[non_exhaustive]`; any future libinput
        // method we don't know about is treated as "not in our 3-slot
        // mask", same as `NoScroll`.
        _ => None,
    }
}

/// Map [`ClickMethod`] to the bit index used by the `click_method`
/// `OneHot2` slot (ButtonAreas=0, Clickfinger=1). libinput has no
/// "disabled" sentinel exposed via this enum (the NONE value lives
/// only in the FFI layer), so this is total.
fn click_method_bit(method: ClickMethod) -> Option<u8> {
    match method {
        ClickMethod::ButtonAreas => Some(0),
        ClickMethod::Clickfinger => Some(1),
        // `ClickMethod` is `#[non_exhaustive]`; unknown future
        // variants stay out of our 2-slot mask.
        _ => None,
    }
}

/// Map [`TapButtonMap`] to the bit index used by the `tap_button_map`
/// `OneHot2` slot (LRM=0, LMR=1).
fn tap_button_map_bit(map: TapButtonMap) -> u8 {
    match map {
        TapButtonMap::LeftRightMiddle => 0,
        TapButtonMap::LeftMiddleRight => 1,
        // `TapButtonMap` is `#[non_exhaustive]`; future libinput
        // additions fall back to the LRM slot (libinput's default).
        _ => 0,
    }
}

/// Translate the `send_events` bitmask (bit0=Disabled, bit1=Disabled-
/// on-external-mouse) used over the X11 wire to libinput's
/// [`SendEventsMode`] flags. `Enabled` is libinput's "no bits" placeholder
/// and is naturally produced when the mask is zero.
fn send_events_from_mask(mask: u8) -> SendEventsMode {
    let mut out = SendEventsMode::empty();
    if mask & 0b01 != 0 {
        out |= SendEventsMode::DISABLED;
    }
    if mask & 0b10 != 0 {
        out |= SendEventsMode::DISABLED_ON_EXTERNAL_MOUSE;
    }
    out
}

/// Translate libinput's [`SendEventsMode`] back to the bitmask shape used
/// by the snapshot (bit0=Disabled, bit1=Disabled-on-external-mouse).
fn send_events_to_mask(mode: SendEventsMode) -> u8 {
    let mut out: u8 = 0;
    if mode.contains(SendEventsMode::DISABLED) {
        out |= 0b01;
    }
    if mode.contains(SendEventsMode::DISABLED_ON_EXTERNAL_MOUSE) {
        out |= 0b10;
    }
    out
}

/// Build a [`LibinputConfigSnapshot`] from the live libinput device. Called
/// at `DeviceAdded` for touchpads so the XI2 property registry surfaces
/// each knob's `available`/`current`/`default` triplet. Setters that
/// libinput considers unsupported on the device leave the corresponding
/// `*_is_available` flag `false`; current/default still report whatever
/// libinput hands back (typically the no-op defaults).
pub(crate) fn gather(dev: &Device) -> LibinputConfigSnapshot {
    // Tap: libinput uses `tap_finger_count > 0` as the availability gate.
    let tap_available = dev.config_tap_finger_count() > 0;
    let tap = BoolSetting {
        available: tap_available,
        current: dev.config_tap_enabled(),
        default: dev.config_tap_default_enabled(),
    };
    let tap_drag = BoolSetting {
        // Tap-and-drag tracks tap availability — libinput's drag setter
        // returns UNSUPPORTED on devices that don't expose tap at all.
        available: tap_available,
        current: dev.config_tap_drag_enabled(),
        default: dev.config_tap_default_drag_enabled(),
    };
    let tap_drag_lock = BoolSetting {
        available: tap_available,
        current: dev.config_tap_drag_lock_enabled(),
        // `config_tap_default_drag_lock_enabled` returns
        // `DragLockState`, not a bool; collapse `Disabled` → false,
        // any enabled variant (Timeout / Sticky) → true to match the
        // wire's Bool-scalar shape.
        default: !matches!(
            dev.config_tap_default_drag_lock_enabled(),
            DragLockState::Disabled
        ),
    };

    let natural_scroll = BoolSetting {
        available: dev.config_scroll_has_natural_scroll(),
        current: dev.config_scroll_natural_scroll_enabled(),
        default: dev.config_scroll_default_natural_scroll_enabled(),
    };
    let dwt = BoolSetting {
        available: dev.config_dwt_is_available(),
        current: dev.config_dwt_enabled(),
        default: dev.config_dwt_default_enabled(),
    };
    let left_handed = BoolSetting {
        available: dev.config_left_handed_is_available(),
        current: dev.config_left_handed(),
        default: dev.config_left_handed_default(),
    };
    let middle_emulation = BoolSetting {
        available: dev.config_middle_emulation_is_available(),
        current: dev.config_middle_emulation_enabled(),
        default: dev.config_middle_emulation_default_enabled(),
    };
    // Button-scroll lock availability isn't directly exposed by libinput;
    // it implies `ScrollMethod::OnButtonDown` is among the available
    // scroll methods. Mirror that.
    let scroll_methods_vec = dev.config_scroll_methods();
    let scroll_button_lock_available = scroll_methods_vec.contains(&ScrollMethod::OnButtonDown);
    let scroll_button_lock = BoolSetting {
        available: scroll_button_lock_available,
        current: matches!(
            dev.config_scroll_button_lock(),
            ScrollButtonLockState::Enabled
        ),
        default: matches!(
            dev.config_scroll_default_button_lock(),
            ScrollButtonLockState::Enabled,
        ),
    };

    let accel = FloatSetting {
        available: dev.config_accel_is_available(),
        current: dev.config_accel_speed() as f32,
        default: dev.config_accel_default_speed() as f32,
    };

    let scroll_button = U32Setting {
        // Same availability rule as scroll-button-lock: requires
        // `ScrollMethod::OnButtonDown` to be among supported methods.
        available: scroll_button_lock_available,
        current: dev.config_scroll_button(),
        default: dev.config_scroll_default_button(),
    };

    let tap_button_map = OneHot2 {
        available: tap_available,
        current: dev.config_tap_button_map().map(tap_button_map_bit),
        default: dev.config_tap_default_button_map().map(tap_button_map_bit),
    };

    // Scroll methods: build `available_mask` from the device's supported
    // set, then current/default by index. `NoScroll` is the "disabled"
    // sentinel — it occupies no bit and shows as `None` in current/default.
    let mut scroll_methods_mask: u8 = 0;
    for m in &scroll_methods_vec {
        if let Some(b) = scroll_method_bit(*m) {
            scroll_methods_mask |= 1 << b;
        }
    }
    let scroll_method = OneHot3 {
        available_mask: scroll_methods_mask,
        current: dev.config_scroll_method().and_then(scroll_method_bit),
        default: dev
            .config_scroll_default_method()
            .and_then(scroll_method_bit),
    };

    // Click methods: build `available_mask` over 2 slots
    // (ButtonAreas=0, Clickfinger=1). libinput's `Vec<ClickMethod>` is
    // a simple list.
    let click_methods_vec = dev.config_click_methods();
    let mut click_methods_mask: u8 = 0;
    for m in &click_methods_vec {
        if let Some(b) = click_method_bit(*m) {
            click_methods_mask |= 1 << b;
        }
    }
    let click_method = OneHot2 {
        // `OneHot2` has no `available_mask` — surface availability as
        // "the device supports any click method" so the X11
        // descriptor's `Available` companion is non-empty.
        available: click_methods_mask != 0,
        current: dev.config_click_method().and_then(click_method_bit),
        default: dev.config_click_default_method().and_then(click_method_bit),
    };

    // Accel profiles: keep current/default in the established two-slot
    // snapshot, but capture the actual supported profile mask separately.
    // The XI property remains three bytes wide; Custom stays unsupported
    // for writes on this build.
    // This getter predates libinput 1.23 and its raw bitmask also preserves
    // the custom capability even when input's safe enum wrapper omits that
    // variant under the workspace's older API feature set. `dev` owns a live
    // libinput_device handle for this call; the getter only reads its config.
    // Native C bits are Flat=0, Adaptive=1, Custom=2; XI wire slots are
    // Adaptive=0, Flat=1, Custom=2.
    let native_accel_profile_mask =
        unsafe { input::ffi::libinput_device_config_accel_get_profiles(dev.as_raw_mut()) };
    let accel_profile_available_mask = u8::from(
        native_accel_profile_mask
            & input::ffi::libinput_config_accel_profile_LIBINPUT_CONFIG_ACCEL_PROFILE_ADAPTIVE
            != 0,
    ) | (u8::from(
        native_accel_profile_mask
            & input::ffi::libinput_config_accel_profile_LIBINPUT_CONFIG_ACCEL_PROFILE_FLAT
            != 0,
    ) << 1)
        // LIBINPUT_CONFIG_ACCEL_PROFILE_CUSTOM = (1 << 2), introduced in
        // libinput 1.23; the getter itself is available in the 1.21 bindings.
        | (u8::from(native_accel_profile_mask & (1 << 2) != 0) << 2);
    let accel_profile_mask = accel_profile_available_mask & 0b011;
    let accel_profile = OneHot2 {
        available: accel_profile_mask != 0,
        current: dev.config_accel_profile().and_then(accel_profile_bit),
        default: dev
            .config_accel_default_profile()
            .and_then(accel_profile_bit),
    };

    // Send-events: bitflags over Disabled (0b01) +
    // Disabled-on-external-mouse (0b10). `config_send_events_modes`
    // returns the device's *available* modes (always includes ENABLED,
    // libinput's "no-bits" placeholder).
    let send_modes_available = dev.config_send_events_modes();
    let send_modes_current = dev.config_send_events_mode();
    // Default is libinput-defined; the FFI doesn't expose a
    // `get_default_mode` for the modes bitflag, so it always defaults
    // to ENABLED → mask 0.
    let send_events = BitFlags2 {
        available_mask: send_events_to_mask(send_modes_available),
        current_mask: send_events_to_mask(send_modes_current),
        default_mask: 0,
    };

    LibinputConfigSnapshot {
        tap,
        tap_drag,
        tap_drag_lock,
        natural_scroll,
        dwt,
        left_handed,
        middle_emulation,
        scroll_button_lock,
        accel,
        scroll_button,
        tap_button_map,
        scroll_method,
        click_method,
        accel_profile,
        accel_profile_available_mask,
        send_events,
    }
}

/// Route a decoded [`DeviceConfigChange`] to the matching `config_*_set_*`
/// setter on `dev`, mapping libinput's [`input::DeviceConfigError`] onto
/// yserver's [`DeviceConfigError`]. The caller (see
/// [`super::Context::apply_device_config`]) is responsible for keying
/// the live handle by devnode and short-circuiting absent-device writes.
///
/// One-hot index ranges are guaranteed by `decode_change` +
/// `validate_value` upstream — `unreachable!` arms make a future
/// invariant break fail loud rather than route to a default variant.
///
/// # Errors
///
/// Returns [`DeviceConfigError::Unsupported`] when libinput reports the
/// setting isn't available on this device, or [`DeviceConfigError::Invalid`]
/// when the value is out of range.
pub(crate) fn apply(dev: &mut Device, change: DeviceConfigChange) -> Result<(), DeviceConfigError> {
    use DeviceConfigChange as C;
    let status = match change {
        C::Tap(b) => dev.config_tap_set_enabled(b),
        C::TapDrag(b) => dev.config_tap_set_drag_enabled(b),
        C::TapDragLock(b) => dev.config_tap_set_drag_lock_enabled(if b {
            // Pre-1.27 builds have only `EnabledTimeout`; on
            // 1.27+ the user-facing toggle keeps "enabled" =
            // timeout mode (Sticky needs a separate property,
            // out of scope). Either way, this matches the
            // wire's Bool-scalar shape.
            DragLockState::EnabledTimeout
        } else {
            DragLockState::Disabled
        }),
        C::TapButtonMap(i) => dev.config_tap_set_button_map(if i == 0 {
            TapButtonMap::LeftRightMiddle
        } else {
            TapButtonMap::LeftMiddleRight
        }),
        C::NaturalScroll(b) => dev.config_scroll_set_natural_scroll_enabled(b),
        C::Dwt(b) => dev.config_dwt_set_enabled(b),
        C::LeftHanded(b) => dev.config_left_handed_set(b),
        C::MiddleEmulation(b) => dev.config_middle_emulation_set_enabled(b),
        C::ScrollMethod(opt) => dev.config_scroll_set_method(match opt {
            None => ScrollMethod::NoScroll,
            Some(0) => ScrollMethod::TwoFinger,
            Some(1) => ScrollMethod::Edge,
            Some(2) => ScrollMethod::OnButtonDown,
            // `validate_value` + `decode_change` produce only
            // 0..3 for `OneHotOrNone { n: 3 }`; anything else is
            // an invariant break upstream.
            Some(n) => {
                unreachable!("ScrollMethod index {n} should have been rejected by validate_value")
            }
        }),
        C::ClickMethod(opt) => match opt {
            // libinput has no public `None` variant on
            // `ClickMethod` (it's an FFI-only sentinel). Map the
            // "clicks disabled" decode to UNSUPPORTED — the
            // property's spec doesn't carry a real `None` write
            // path on devices we target, and clients should not
            // get a silent success here.
            None => Err(input::DeviceConfigError::Unsupported),
            Some(0) => dev.config_click_set_method(ClickMethod::ButtonAreas),
            Some(1) => dev.config_click_set_method(ClickMethod::Clickfinger),
            Some(n) => {
                unreachable!("ClickMethod index {n} should have been rejected by validate_value")
            }
        },
        C::SendEvents(mask) => dev.config_send_events_set_mode(send_events_from_mask(mask)),
        C::AccelSpeed(s) => dev.config_accel_set_speed(f64::from(s)),
        C::AccelProfile(opt) => dev.config_accel_set_profile(match opt {
            // `None` and `Some(0)` both map to Adaptive — there is no
            // "no profile" setter on libinput, so falling back to the
            // default profile is the closest legal write.
            None | Some(0) => AccelProfile::Adaptive,
            Some(1) => AccelProfile::Flat,
            // Custom (`Some(2)`) is rejected at `decode_change`, so it
            // can never reach here; nor can any other index.
            Some(n) => {
                unreachable!("AccelProfile index {n} should have been rejected by decode_change")
            }
        }),
        C::ScrollButton(b) => dev.config_scroll_set_button(b),
        C::ScrollButtonLock(b) => dev.config_scroll_set_button_lock(if b {
            ScrollButtonLockState::Enabled
        } else {
            ScrollButtonLockState::Disabled
        }),
    };
    match status {
        Ok(()) => Ok(()),
        Err(input::DeviceConfigError::Unsupported) => Err(DeviceConfigError::Unsupported),
        Err(input::DeviceConfigError::Invalid) => Err(DeviceConfigError::Invalid),
    }
}
