//! Data-driven libinput property descriptor table.
//!
//! Single source of truth for the libinput XInput property surface.
//! Each [`PropDescriptor`] row pairs a libinput property name (as listed
//! in `/usr/include/xorg/libinput-properties.h`) with its X11 type
//! (`Bool`/`Card32`/`Float`), wire format (8 or 32), value cardinality
//! ([`ValueKind`]), access mode ([`Access`]), and — for writable rows —
//! the [`Binding`] that maps to a libinput setter.
//!
//! Used by:
//!   * `xinput::seed_pointer_properties` — iterates the table to populate
//!     each pointer facet from a [`LibinputConfigSnapshot`] (T2/T6).
//!   * `core_loop::process_request` XI2 / XI1 property dispatch arms —
//!     validates incoming writes ([`validate_value`]) and decodes them
//!     to [`DeviceConfigChange`] ([`decode_change`]) for the backend's
//!     `apply_device_config` hook (T3).
//!
//! [`LibinputConfigSnapshot`]: crate::core_loop::message::LibinputConfigSnapshot

/// A typed device-config write target (one variant per libinput setter).
///
/// Produced by [`decode_change`] after a successful [`validate_value`]
/// pass; consumed by `Backend::start_device_config` to write through to
/// the live libinput device. `PartialEq` only (no `Eq`) because
/// `AccelSpeed` carries an `f32`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceConfigChange {
    /// `libinput Tapping Enabled` (Scalar Bool).
    Tap(bool),
    /// `libinput Tapping Drag Enabled` (Scalar Bool).
    TapDrag(bool),
    /// `libinput Tapping Drag Lock Enabled` (Scalar Bool).
    TapDragLock(bool),
    /// `libinput Tapping Button Mapping Enabled` (OneHot, 2 slots).
    /// `0 = LRM` (default), `1 = LMR` (left-handed swap).
    TapButtonMap(u8),
    /// `libinput Natural Scrolling Enabled` (Scalar Bool).
    NaturalScroll(bool),
    /// `libinput Disable While Typing Enabled` (Scalar Bool).
    Dwt(bool),
    /// `libinput Left Handed Enabled` (Scalar Bool).
    LeftHanded(bool),
    /// `libinput Middle Emulation Enabled` (Scalar Bool).
    MiddleEmulation(bool),
    /// `libinput Scroll Method Enabled` (OneHotOrNone, 3 slots).
    /// `Some(0) = two-finger`, `Some(1) = edge`, `Some(2) = button`,
    /// `None = scrolling disabled`.
    ScrollMethod(Option<u8>),
    /// `libinput Click Method Enabled` (OneHotOrNone, 2 slots).
    /// `Some(0) = button-areas`, `Some(1) = clickfinger`,
    /// `None = clicks disabled`.
    ClickMethod(Option<u8>),
    /// `libinput Send Events Mode Enabled` (BitFlags, 2 bits).
    /// `bit0 = disabled`, `bit1 = disabled-on-external-mouse`.
    SendEvents(u8),
    /// `libinput Accel Speed` (Scalar Float).
    AccelSpeed(f32),
    /// `libinput Accel Profile Enabled` (OneHotOrNone, 3 slots).
    /// `Some(0) = adaptive`, `Some(1) = flat`, `None = profile disabled`.
    /// Index 2 (custom) is rejected at decode-time with
    /// [`DeviceConfigError::Invalid`] — the workspace doesn't yet
    /// enable the `libinput_1_23` feature that exposes the custom
    /// curve setter.
    AccelProfile(Option<u8>),
    /// `libinput Button Scrolling Button` (Scalar Card32).
    ScrollButton(u32),
    /// `libinput Button Scrolling Button Lock Enabled` (Scalar Bool).
    ScrollButtonLock(bool),
}

/// Why a config apply failed → mapped to an X error by the dispatch layer.
/// `Unsupported` → BadMatch, `Invalid` → BadValue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceConfigError {
    /// Setting not supported on this device.
    Unsupported,
    /// Value out of range / not a legal one-hot / wrong byte count.
    Invalid,
    /// The runtime input source is no longer bound to a live libinput handle.
    SourceGone,
}

/// Opaque identity for one input-thread configuration application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceConfigToken(pub u64);

/// Whether a backend applied a source configuration synchronously or
/// submitted it to an asynchronous owner such as the KMS input thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceConfigStart {
    Applied,
    Pending(DeviceConfigToken),
}

/// X11 property value type for a descriptor row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XiValType {
    /// `INTEGER` atom, format 8 (one byte per element).
    Bool,
    /// `CARDINAL` atom, format 32 (one u32 per element).
    Card32,
    /// `FLOAT` (runtime-interned) atom, format 32 (one f32 per element).
    Float,
}

/// Value-cardinality + validation rule for a property's data block.
///
/// Element count is implied by the format:
///   * format 8 → `data.len()` elements.
///   * format 32 → `data.len() / 4` elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// Exactly one element (1 byte for Bool, 4 bytes for Card32/Float).
    Scalar,
    /// Exactly one of `n` Bool slots set, rest zero.
    OneHot { n: u8 },
    /// At most one of `n` Bool slots set (all-zero allowed), written as
    /// `min..=n` bytes.
    ///
    /// `min` is the narrowest wire width a client may send; a shorter
    /// value than `n` is zero-padded to `n` by [`normalize_value`]. It
    /// exists because `xf86-input-libinput` gates each property on its
    /// own width and one of them accepts a *range*:
    ///
    /// ```text
    /// LibinputSetPropertyAccelProfile   src/xf86libinput.c:4622
    ///   val->size < 2 || val->size > 3        -> min 2, n 3
    /// LibinputSetPropertyScrollMethods  :4884  val->size != 3
    /// LibinputSetPropertyClickMethod    :4988  val->size != 2
    /// ```
    ///
    /// `Accel Profile Enabled` is the only one, because it is the only
    /// property whose width *grew* upstream — the third "custom" slot
    /// arrived with libinput 1.23, so the driver kept accepting the
    /// two-slot form that the installed client ecosystem still emits
    /// (see `mate-settings-daemon`'s two-item write). Every other
    /// property is exact-width there and must stay exact-width here:
    /// zero-padding e.g. a one-byte `Scroll Method Enabled` would turn
    /// it into "two-finger on, edge off, button off", a configuration
    /// the client never expressed.
    ///
    /// Set `min == n` for an exact-width property.
    OneHotOrNone { n: u8, min: u8 },
    /// Any subset of `n` Bool slots set (each byte non-zero ⇔ bit set).
    BitFlags { n: u8 },
}

/// Whether clients may modify a property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Writable via XIChangeProperty / XChangeDeviceProperty.
    ReadWrite,
    /// Read-only companion (`…Default`, `…Available`).
    ReadOnly,
}

/// Which libinput config a writable descriptor maps to.
///
/// One variant per libinput setter; T3 will switch on this in the
/// dispatch layer to call the matching `libinput_device_config_*_set_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    Tap,
    TapDrag,
    TapDragLock,
    TapButtonMap,
    NaturalScroll,
    Dwt,
    LeftHanded,
    MiddleEmulation,
    ScrollMethod,
    ClickMethod,
    SendEvents,
    AccelSpeed,
    AccelProfile,
    ScrollButton,
    ScrollButtonLock,
}

/// One row of the libinput property descriptor table.
pub struct PropDescriptor {
    /// Property name as listed in `libinput-properties.h`. Verbatim.
    pub name: &'static str,
    /// X11 property type atom kind.
    pub val: XiValType,
    /// Wire format: 8 for Bool, 32 for Card32/Float.
    pub format: u8,
    /// Per-row validation rule.
    pub kind: ValueKind,
    /// Read-only or read-write.
    pub access: Access,
    /// `Some(_)` for writable rows; `None` for read-only companions.
    pub binding: Option<Binding>,
}

/// The full libinput descriptor table.
///
/// Names verbatim from `/usr/include/xorg/libinput-properties.h`. Includes
/// every writable property, its `…Default` companion (ReadOnly with the
/// same `val`/`format`/`kind`, binding `None`), and the `…Available`
/// companion for method/profile/send-events groups (ReadOnly, kind
/// `BitFlags`).
///
/// `Accel Profile Enabled` is wire-width 3 (adaptive/flat/custom) to
/// match the Xorg driver; the custom slot is seeded zero and T3 rejects
/// writes that set it.
pub static DESCRIPTORS: &[PropDescriptor] = &[
    PropDescriptor {
        name: "libinput Tapping Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::Tap),
    },
    PropDescriptor {
        name: "libinput Tapping Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Tapping Drag Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::TapDrag),
    },
    PropDescriptor {
        name: "libinput Tapping Drag Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Tapping Drag Lock Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::TapDragLock),
    },
    PropDescriptor {
        name: "libinput Tapping Drag Lock Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Tapping Button Mapping Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHot { n: 2 },
        access: Access::ReadWrite,
        binding: Some(Binding::TapButtonMap),
    },
    PropDescriptor {
        name: "libinput Tapping Button Mapping Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHot { n: 2 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Natural Scrolling Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::NaturalScroll),
    },
    PropDescriptor {
        name: "libinput Natural Scrolling Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Disable While Typing Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::Dwt),
    },
    PropDescriptor {
        name: "libinput Disable While Typing Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Left Handed Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::LeftHanded),
    },
    PropDescriptor {
        name: "libinput Left Handed Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Middle Emulation Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::MiddleEmulation),
    },
    PropDescriptor {
        name: "libinput Middle Emulation Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Scroll Methods Available",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::BitFlags { n: 3 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Scroll Method Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHotOrNone { n: 3, min: 3 },
        access: Access::ReadWrite,
        binding: Some(Binding::ScrollMethod),
    },
    PropDescriptor {
        name: "libinput Scroll Method Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHotOrNone { n: 3, min: 3 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Click Methods Available",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::BitFlags { n: 2 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Click Method Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHotOrNone { n: 2, min: 2 },
        access: Access::ReadWrite,
        binding: Some(Binding::ClickMethod),
    },
    PropDescriptor {
        name: "libinput Click Method Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHotOrNone { n: 2, min: 2 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Send Events Modes Available",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::BitFlags { n: 2 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Send Events Mode Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::BitFlags { n: 2 },
        access: Access::ReadWrite,
        binding: Some(Binding::SendEvents),
    },
    PropDescriptor {
        name: "libinput Send Events Mode Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::BitFlags { n: 2 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Accel Speed",
        val: XiValType::Float,
        format: 32,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::AccelSpeed),
    },
    PropDescriptor {
        name: "libinput Accel Speed Default",
        val: XiValType::Float,
        format: 32,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Accel Profiles Available",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::BitFlags { n: 3 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Accel Profile Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHotOrNone { n: 3, min: 2 },
        access: Access::ReadWrite,
        binding: Some(Binding::AccelProfile),
    },
    PropDescriptor {
        name: "libinput Accel Profile Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHotOrNone { n: 3, min: 2 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Button Scrolling Button",
        val: XiValType::Card32,
        format: 32,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::ScrollButton),
    },
    PropDescriptor {
        name: "libinput Button Scrolling Button Default",
        val: XiValType::Card32,
        format: 32,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Button Scrolling Button Lock Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::ScrollButtonLock),
    },
    PropDescriptor {
        name: "libinput Button Scrolling Button Lock Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
];

/// Look up a descriptor by exact property name.
#[must_use]
pub fn descriptor_by_name(name: &str) -> Option<&'static PropDescriptor> {
    DESCRIPTORS.iter().find(|d| d.name == name)
}

/// Validate that `data` is a legal encoding for a property of `kind` at
/// the given wire `format` (8 or 32).
///
/// Returns [`DeviceConfigError::Invalid`] on any byte-count or
/// cardinality violation.
///
/// Rules:
///   * `Scalar` — `data.len() == format / 8` (1 byte for Bool, 4 for
///     Card32/Float).
///   * `OneHot { n }` — `format` MUST be 8; `data.len() == n` and exactly
///     one byte non-zero.
///   * `OneHotOrNone { n, min }` — `format` MUST be 8;
///     `min <= data.len() <= n` and at most one byte non-zero (all-zero
///     allowed). A short write is zero-padded to `n` by
///     [`normalize_value`] before decode. `min == n` for every property
///     except `Accel Profile Enabled` — see [`ValueKind::OneHotOrNone`]
///     for why that one accepts a range and the others must not.
///   * `BitFlags { n }` — `format` MUST be 8; `data.len() == n` (any
///     pattern of zero/non-zero bytes is legal).
///
/// Exact-width kinds reject an **empty** value implicitly (`0 != n`, and
/// `n >= 1` for every row in [`DESCRIPTORS`]). `OneHotOrNone` rejects it
/// via `min`, which is `>= 2` everywhere: an accepted `[]` would
/// zero-fill to a meaning-bearing all-zero write the client never
/// expressed (e.g. `ScrollMethod(None)` turns scrolling off).
///
/// # Errors
/// Returns [`DeviceConfigError::Invalid`] for any byte-count or
/// cardinality violation.
pub fn validate_value(kind: ValueKind, format: u8, data: &[u8]) -> Result<(), DeviceConfigError> {
    match kind {
        ValueKind::Scalar => {
            let expected = usize::from(format / 8);
            if data.len() != expected {
                return Err(DeviceConfigError::Invalid);
            }
            Ok(())
        }
        ValueKind::OneHot { n } => {
            if format != 8 || data.len() != usize::from(n) {
                return Err(DeviceConfigError::Invalid);
            }
            let nonzero = data.iter().filter(|b| **b != 0).count();
            if nonzero == 1 {
                Ok(())
            } else {
                Err(DeviceConfigError::Invalid)
            }
        }
        ValueKind::OneHotOrNone { n, min } => {
            debug_assert!(min >= 1 && min <= n, "OneHotOrNone min must be in 1..=n");
            if format != 8 || data.len() < usize::from(min) || data.len() > usize::from(n) {
                return Err(DeviceConfigError::Invalid);
            }
            let nonzero = data.iter().filter(|b| **b != 0).count();
            if nonzero <= 1 {
                Ok(())
            } else {
                Err(DeviceConfigError::Invalid)
            }
        }
        ValueKind::BitFlags { n } => {
            if format != 8 || data.len() != usize::from(n) {
                return Err(DeviceConfigError::Invalid);
            }
            Ok(())
        }
    }
}

/// Zero-pad a short multi-slot value to the descriptor's declared width
/// so every downstream consumer (decoders, the stored property) always
/// sees exactly `n` bytes. Caller MUST have run [`validate_value`] first.
///
/// Returns `data` untouched (borrowed, no allocation) for `Scalar` and
/// for already-full-width multi-slot values; returns a zero-padded
/// `n`-byte owned copy for a short multi-slot write.
#[must_use]
pub fn normalize_value(kind: ValueKind, data: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let n = match kind {
        ValueKind::Scalar => return std::borrow::Cow::Borrowed(data),
        ValueKind::OneHot { n } | ValueKind::OneHotOrNone { n, .. } | ValueKind::BitFlags { n } => {
            n
        }
    };
    let n = usize::from(n);
    if data.len() >= n {
        std::borrow::Cow::Borrowed(data)
    } else {
        let mut padded = data.to_vec();
        padded.resize(n, 0);
        std::borrow::Cow::Owned(padded)
    }
}

// ---------------------------------------------------------------------------
// Decode — wire bytes to typed `DeviceConfigChange`.
// ---------------------------------------------------------------------------

/// Decode an already-validated on-wire value into a typed
/// [`DeviceConfigChange`] for the given [`Binding`].
///
/// Caller MUST have called [`validate_value`] first; this function
/// assumes the byte count and cardinality constraints already hold and
/// only extracts the active index / mask / scalar value. The single
/// case it rejects beyond pure validation is
/// [`Binding::AccelProfile`] with the *custom* slot set (index 2),
/// since the workspace's libinput build doesn't expose the custom
/// profile setter — that returns [`DeviceConfigError::Invalid`] and
/// the dispatcher maps it to `BadValue`.
///
/// The "method/profile disabled" cases (all-zero `OneHotOrNone`) map
/// to the *inner* `None` on `ScrollMethod` / `ClickMethod` /
/// `AccelProfile`, NOT to a surrounding `Option`; every legal input
/// produces exactly one [`DeviceConfigChange`].
///
/// # Errors
///
/// Returns [`DeviceConfigError::Invalid`] only for the
/// `AccelProfile::custom` rejection above.
pub fn decode_change(
    binding: Binding,
    data: &[u8],
) -> Result<DeviceConfigChange, DeviceConfigError> {
    // Decode helpers — caller has already validated byte counts.
    let bool_scalar = |b: &[u8]| b.first().is_some_and(|v| *v != 0);
    let onehot_index = |b: &[u8]| b.iter().position(|v| *v != 0).map(|i| i as u8);
    let bitmask = |b: &[u8]| {
        b.iter().enumerate().fold(
            0u8,
            |acc, (i, v)| {
                if *v != 0 { acc | (1 << i) } else { acc }
            },
        )
    };
    let card32 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let float32 = |b: &[u8]| f32::from_le_bytes([b[0], b[1], b[2], b[3]]);

    Ok(match binding {
        Binding::Tap => DeviceConfigChange::Tap(bool_scalar(data)),
        Binding::TapDrag => DeviceConfigChange::TapDrag(bool_scalar(data)),
        Binding::TapDragLock => DeviceConfigChange::TapDragLock(bool_scalar(data)),
        Binding::TapButtonMap => {
            // OneHot { n: 2 } — validation already guaranteed exactly one slot set.
            let idx = onehot_index(data).unwrap_or(0);
            DeviceConfigChange::TapButtonMap(idx)
        }
        Binding::NaturalScroll => DeviceConfigChange::NaturalScroll(bool_scalar(data)),
        Binding::Dwt => DeviceConfigChange::Dwt(bool_scalar(data)),
        Binding::LeftHanded => DeviceConfigChange::LeftHanded(bool_scalar(data)),
        Binding::MiddleEmulation => DeviceConfigChange::MiddleEmulation(bool_scalar(data)),
        Binding::ScrollMethod => DeviceConfigChange::ScrollMethod(onehot_index(data)),
        Binding::ClickMethod => DeviceConfigChange::ClickMethod(onehot_index(data)),
        Binding::SendEvents => DeviceConfigChange::SendEvents(bitmask(data)),
        Binding::AccelSpeed => DeviceConfigChange::AccelSpeed(float32(data)),
        Binding::AccelProfile => {
            // OneHotOrNone { n: 3 } — slot 2 is the libinput-1.23 "custom"
            // profile, which we can't honor on the current workspace build.
            // Reject at decode-time so the dispatcher emits BadValue.
            if data.get(2).is_some_and(|b| *b != 0) {
                return Err(DeviceConfigError::Invalid);
            }
            DeviceConfigChange::AccelProfile(onehot_index(data))
        }
        Binding::ScrollButton => DeviceConfigChange::ScrollButton(card32(data)),
        Binding::ScrollButtonLock => DeviceConfigChange::ScrollButtonLock(bool_scalar(data)),
    })
}

// ---------------------------------------------------------------------------
// Value encoders — emit the on-wire byte layout for a descriptor row.
// ---------------------------------------------------------------------------

/// Encode a Bool scalar as a one-byte vector (`[0]` or `[1]`).
#[must_use]
pub fn encode_bool(value: bool) -> Vec<u8> {
    vec![u8::from(value)]
}

/// Encode a one-hot Bool group with `n` slots.
///
/// `idx = Some(i)` sets byte `i` to 1, rest 0. `idx = None` yields all
/// zeros (only legal for `OneHotOrNone` consumers).
///
/// # Panics
/// Panics in debug if `idx` is out of range; release truncates to `n`.
#[must_use]
pub fn encode_onehot(idx: Option<u8>, n: u8) -> Vec<u8> {
    let len = usize::from(n);
    let mut buf = vec![0u8; len];
    if let Some(i) = idx {
        debug_assert!(usize::from(i) < len, "encode_onehot: idx {i} out of {len}");
        if let Some(slot) = buf.get_mut(usize::from(i)) {
            *slot = 1;
        }
    }
    buf
}

/// Encode a bitflags Bool group: one byte per bit, byte = 1 if bit set.
#[must_use]
pub fn encode_bitflags(mask: u8, n: u8) -> Vec<u8> {
    (0..n).map(|i| u8::from(mask & (1 << i) != 0)).collect()
}

/// Encode a CARDINAL/32 scalar as 4 little-endian bytes.
#[must_use]
pub fn encode_card32(value: u32) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

/// Encode a FLOAT/32 scalar as 4 little-endian IEEE-754 bytes.
#[must_use]
pub fn encode_float(value: f32) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

/// Resolve the writable property descriptor affected by a confirmed
/// libinput change. This is used after a server reset, when the submitted
/// protocol request and its old atom ID have intentionally been discarded.
#[must_use]
pub fn descriptor_for_change(change: DeviceConfigChange) -> &'static PropDescriptor {
    let binding = match change {
        DeviceConfigChange::Tap(_) => Binding::Tap,
        DeviceConfigChange::TapDrag(_) => Binding::TapDrag,
        DeviceConfigChange::TapDragLock(_) => Binding::TapDragLock,
        DeviceConfigChange::TapButtonMap(_) => Binding::TapButtonMap,
        DeviceConfigChange::NaturalScroll(_) => Binding::NaturalScroll,
        DeviceConfigChange::Dwt(_) => Binding::Dwt,
        DeviceConfigChange::LeftHanded(_) => Binding::LeftHanded,
        DeviceConfigChange::MiddleEmulation(_) => Binding::MiddleEmulation,
        DeviceConfigChange::ScrollMethod(_) => Binding::ScrollMethod,
        DeviceConfigChange::ClickMethod(_) => Binding::ClickMethod,
        DeviceConfigChange::SendEvents(_) => Binding::SendEvents,
        DeviceConfigChange::AccelSpeed(_) => Binding::AccelSpeed,
        DeviceConfigChange::AccelProfile(_) => Binding::AccelProfile,
        DeviceConfigChange::ScrollButton(_) => Binding::ScrollButton,
        DeviceConfigChange::ScrollButtonLock(_) => Binding::ScrollButtonLock,
    };
    DESCRIPTORS
        .iter()
        .find(|descriptor| descriptor.binding == Some(binding))
        .expect("every DeviceConfigChange has a writable property descriptor")
}

/// Encode the canonical property value for a confirmed libinput change.
/// Typed format-32 values use little-endian bytes, matching XI property
/// storage after Task 7's frontend normalization.
#[must_use]
pub fn encode_change_value(change: DeviceConfigChange) -> Vec<u8> {
    match change {
        DeviceConfigChange::Tap(value)
        | DeviceConfigChange::TapDrag(value)
        | DeviceConfigChange::TapDragLock(value)
        | DeviceConfigChange::NaturalScroll(value)
        | DeviceConfigChange::Dwt(value)
        | DeviceConfigChange::LeftHanded(value)
        | DeviceConfigChange::MiddleEmulation(value)
        | DeviceConfigChange::ScrollButtonLock(value) => encode_bool(value),
        DeviceConfigChange::TapButtonMap(value) => encode_onehot(Some(value), 2),
        DeviceConfigChange::ScrollMethod(value) => encode_onehot(value, 3),
        DeviceConfigChange::ClickMethod(value) => encode_onehot(value, 2),
        DeviceConfigChange::SendEvents(mask) => encode_bitflags(mask, 2),
        DeviceConfigChange::AccelSpeed(value) => encode_float(value),
        DeviceConfigChange::AccelProfile(value) => encode_onehot(value, 3),
        DeviceConfigChange::ScrollButton(value) => encode_card32(value),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_onehot_requires_exactly_one() {
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[1, 0]).is_ok());
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[0, 1]).is_ok());
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[0, 0]).is_err()); // none
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[1, 1]).is_err()); // two
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[1]).is_err()); // short: exact-width kind
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[1, 0, 0]).is_err()); // too long
    }

    #[test]
    fn validate_onehotornone_allows_zero() {
        assert!(validate_value(ValueKind::OneHotOrNone { n: 3, min: 3 }, 8, &[0, 0, 0]).is_ok());
        assert!(validate_value(ValueKind::OneHotOrNone { n: 3, min: 3 }, 8, &[0, 1, 0]).is_ok());
        assert!(validate_value(ValueKind::OneHotOrNone { n: 3, min: 3 }, 8, &[1, 1, 0]).is_err());
    }

    #[test]
    fn validate_bitflags_allows_any_subset() {
        for v in [&[0u8, 0][..], &[1, 0][..], &[0, 1][..], &[1, 1][..]] {
            assert!(validate_value(ValueKind::BitFlags { n: 2 }, 8, v).is_ok());
        }
        assert!(validate_value(ValueKind::BitFlags { n: 2 }, 8, &[1]).is_err()); // short: exact-width kind
        assert!(validate_value(ValueKind::BitFlags { n: 2 }, 8, &[1, 0, 0]).is_err()); // too long
    }

    #[test]
    fn validate_value_accepts_short_multi_slot_write() {
        // The `Accel Profile Enabled` shape: n 3, min 2. A two-byte write
        // (what mate-settings-daemon emits) is accepted and zero-padded;
        // the driver's own gate is `val->size < 2 || val->size > 3`.
        let accel = ValueKind::OneHotOrNone { n: 3, min: 2 };
        assert!(validate_value(accel, 8, &[0, 1]).is_ok());
        assert!(validate_value(accel, 8, &[0, 1, 0]).is_ok());
        // Below `min` is rejected — the driver refuses a one-item write
        // too, and padding `[1]` would assert a slot the client never sent.
        assert!(validate_value(accel, 8, &[1]).is_err());
        // Longer than `n` is rejected.
        assert!(validate_value(accel, 8, &[0, 1, 0, 0]).is_err());
        // Cardinality rules are independent of width.
        assert!(validate_value(accel, 8, &[1, 1]).is_err());
        assert!(validate_value(accel, 8, &[0, 0]).is_ok()); // all-zero = None
        // `format != 8` is still rejected regardless of length.
        assert!(validate_value(accel, 32, &[0, 1]).is_err());

        // An exact-width `OneHotOrNone` (min == n) — Scroll/Click Method —
        // rejects the same short write the driver rejects.
        let exact = ValueKind::OneHotOrNone { n: 3, min: 3 };
        assert!(validate_value(exact, 8, &[0, 1]).is_err());
        assert!(validate_value(exact, 8, &[0, 1, 0]).is_ok());
        // `OneHot` and `BitFlags` have no ranged property upstream and
        // stay exact-width.
        assert!(validate_value(ValueKind::OneHot { n: 3 }, 8, &[0, 1]).is_err());
        assert!(validate_value(ValueKind::OneHot { n: 3 }, 8, &[0, 1, 0]).is_ok());
        assert!(validate_value(ValueKind::BitFlags { n: 3 }, 8, &[1]).is_err());
        assert!(validate_value(ValueKind::BitFlags { n: 3 }, 8, &[1, 0, 0]).is_ok());
    }

    #[test]
    fn validate_value_rejects_empty_multi_slot_write() {
        // Task 1b (review round S3): an empty write is meaning-bearing
        // once short writes are accepted — it would normalise to
        // all-zero and silently reprogram the device from a value the
        // client never expressed. All three multi-slot kinds reject it,
        // even `OneHot`, which already rejects `&[]` via the
        // cardinality check (0 non-zero slots) — pin the rule per-kind
        // rather than relying on that as an accident of the count.
        assert!(validate_value(ValueKind::OneHot { n: 3 }, 8, &[]).is_err());
        assert!(validate_value(ValueKind::OneHotOrNone { n: 3, min: 3 }, 8, &[]).is_err());
        assert!(validate_value(ValueKind::BitFlags { n: 3 }, 8, &[]).is_err());
    }

    #[test]
    fn normalize_value_zero_pads_short_multi_slot() {
        assert_eq!(
            normalize_value(ValueKind::OneHotOrNone { n: 3, min: 3 }, &[0, 1]).as_ref(),
            &[0, 1, 0]
        );
        let full = [0u8, 1, 0];
        assert!(matches!(
            normalize_value(ValueKind::OneHotOrNone { n: 3, min: 3 }, &full),
            std::borrow::Cow::Borrowed(_)
        ));
        let scalar = [1u8];
        assert!(matches!(
            normalize_value(ValueKind::Scalar, &scalar),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn normalize_value_zero_pads_onehot_and_bitflags() {
        // Task 3b (review round S4): Task 1's tests only covered
        // `OneHotOrNone` and `Scalar` here — pin `OneHot` and
        // `BitFlags` too, since all three multi-slot kinds share the
        // same relaxed `validate_value` upper bound.
        assert_eq!(
            normalize_value(ValueKind::OneHot { n: 3 }, &[0, 1]).as_ref(),
            &[0, 1, 0]
        );
        assert_eq!(
            normalize_value(ValueKind::BitFlags { n: 3 }, &[1]).as_ref(),
            &[1, 0, 0]
        );
    }

    #[test]
    fn normalize_then_decode_composes_to_flat_accel_profile() {
        // Task 3b (review round S4): the `normalize_value` →
        // `decode_change` composition — the actual pipeline the
        // dispatch layer runs — is asserted nowhere in Task 1's tests,
        // and `decode_change_maps_each_binding` never produces
        // `AccelProfile(Some(1))`, i.e. *flat*, the one value this
        // whole change exists to deliver to MATE/GNOME's 2-item write.
        let flat = normalize_value(ValueKind::OneHotOrNone { n: 3, min: 3 }, &[0, 1]);
        assert_eq!(
            decode_change(Binding::AccelProfile, &flat).unwrap(),
            DeviceConfigChange::AccelProfile(Some(1)),
            "[0, 1] normalises to [0, 1, 0] and decodes to flat"
        );

        let disabled = normalize_value(ValueKind::OneHotOrNone { n: 3, min: 3 }, &[0, 0]);
        assert_eq!(
            decode_change(Binding::AccelProfile, &disabled).unwrap(),
            DeviceConfigChange::AccelProfile(None),
            "[0, 0] normalises to [0, 0, 0] and decodes to None"
        );

        // A full-width 3-byte custom selection is still rejected —
        // normalisation runs before decode, so a short write can never
        // be mistaken for a custom (index 2) selection.
        let custom = normalize_value(ValueKind::OneHotOrNone { n: 3, min: 3 }, &[0, 0, 1]);
        assert!(decode_change(Binding::AccelProfile, &custom).is_err());
    }

    #[test]
    fn validate_scalar_float_is_four_bytes() {
        assert!(validate_value(ValueKind::Scalar, 32, &0.5f32.to_le_bytes()).is_ok());
        assert!(validate_value(ValueKind::Scalar, 32, &[0, 0, 0]).is_err());
    }

    #[test]
    fn validate_scalar_bool_is_one_byte() {
        assert!(validate_value(ValueKind::Scalar, 8, &[1]).is_ok());
        assert!(validate_value(ValueKind::Scalar, 8, &[0]).is_ok());
        assert!(validate_value(ValueKind::Scalar, 8, &[0, 0]).is_err());
    }

    #[test]
    fn descriptor_by_name_finds_known() {
        let d = descriptor_by_name("libinput Tapping Enabled").unwrap();
        assert_eq!(d.val, XiValType::Bool);
        assert_eq!(d.format, 8);
        assert_eq!(d.kind, ValueKind::Scalar);
        assert_eq!(d.access, Access::ReadWrite);
        assert_eq!(d.binding, Some(Binding::Tap));
    }

    #[test]
    fn descriptor_by_name_returns_none_for_unknown() {
        assert!(descriptor_by_name("not a libinput prop").is_none());
    }

    #[test]
    fn accel_profile_enabled_is_three_wide() {
        let d = descriptor_by_name("libinput Accel Profile Enabled").unwrap();
        // n 3, min 2 — mirrors LibinputSetPropertyAccelProfile's
        // `val->size < 2 || val->size > 3` (xf86libinput.c:4622). This is
        // the ONLY descriptor with min != n; the assertion below pins that
        // so a future row can't quietly widen its accept-set past the
        // driver's.
        assert_eq!(d.kind, ValueKind::OneHotOrNone { n: 3, min: 2 });
        for other in DESCRIPTORS
            .iter()
            .filter(|o| !o.name.starts_with("libinput Accel Profile Enabled"))
        {
            if let ValueKind::OneHotOrNone { n, min } = other.kind {
                assert_eq!(
                    min, n,
                    "{} must be exact-width: the libinput driver gates every \
                     property but Accel Profile Enabled on an exact val->size",
                    other.name,
                );
            }
        }
    }

    #[test]
    fn every_writable_has_default_companion() {
        for d in DESCRIPTORS.iter().filter(|d| d.access == Access::ReadWrite) {
            let default_name = format!("{} Default", d.name);
            // Some props use a slightly different default suffix; check
            // for at least one ReadOnly companion that mirrors the kind.
            let companion = DESCRIPTORS.iter().find(|c| {
                c.access == Access::ReadOnly
                    && c.binding.is_none()
                    && c.kind == d.kind
                    && c.format == d.format
                    && (c.name == default_name
                        || c.name == "libinput Tapping Button Mapping Default"
                            && d.name == "libinput Tapping Button Mapping Enabled")
            });
            assert!(
                companion.is_some(),
                "no Default companion for writable `{}`",
                d.name
            );
        }
    }

    #[test]
    fn encode_onehot_sets_correct_byte() {
        assert_eq!(encode_onehot(Some(0), 2), vec![1, 0]);
        assert_eq!(encode_onehot(Some(1), 2), vec![0, 1]);
        assert_eq!(encode_onehot(None, 3), vec![0, 0, 0]);
        assert_eq!(encode_onehot(Some(2), 3), vec![0, 0, 1]);
    }

    #[test]
    fn encode_bitflags_one_byte_per_bit() {
        assert_eq!(encode_bitflags(0b00, 2), vec![0, 0]);
        assert_eq!(encode_bitflags(0b01, 2), vec![1, 0]);
        assert_eq!(encode_bitflags(0b10, 2), vec![0, 1]);
        assert_eq!(encode_bitflags(0b11, 2), vec![1, 1]);
        assert_eq!(encode_bitflags(0b101, 3), vec![1, 0, 1]);
    }

    #[test]
    fn encode_float_is_little_endian_ieee754() {
        assert_eq!(encode_float(0.5), 0.5f32.to_le_bytes().to_vec());
        assert_eq!(encode_float(-1.0), (-1.0f32).to_le_bytes().to_vec());
    }

    #[test]
    fn decode_change_maps_each_binding() {
        use Binding::*;
        assert_eq!(
            decode_change(Tap, &[1]).unwrap(),
            DeviceConfigChange::Tap(true)
        );
        assert_eq!(
            decode_change(Tap, &[0]).unwrap(),
            DeviceConfigChange::Tap(false)
        );
        assert_eq!(
            decode_change(TapButtonMap, &[1, 0]).unwrap(),
            DeviceConfigChange::TapButtonMap(0)
        );
        assert_eq!(
            decode_change(TapButtonMap, &[0, 1]).unwrap(),
            DeviceConfigChange::TapButtonMap(1)
        );
        assert_eq!(
            decode_change(ScrollMethod, &[0, 1, 0]).unwrap(),
            DeviceConfigChange::ScrollMethod(Some(1))
        );
        assert_eq!(
            decode_change(ScrollMethod, &[0, 0, 0]).unwrap(),
            DeviceConfigChange::ScrollMethod(None)
        );
        assert_eq!(
            decode_change(ClickMethod, &[1, 0]).unwrap(),
            DeviceConfigChange::ClickMethod(Some(0))
        );
        assert_eq!(
            decode_change(SendEvents, &[1, 0]).unwrap(),
            DeviceConfigChange::SendEvents(0b01)
        );
        assert_eq!(
            decode_change(SendEvents, &[1, 1]).unwrap(),
            DeviceConfigChange::SendEvents(0b11)
        );
        assert_eq!(
            decode_change(AccelSpeed, &0.25f32.to_le_bytes()).unwrap(),
            DeviceConfigChange::AccelSpeed(0.25)
        );
        assert_eq!(
            decode_change(AccelProfile, &[1, 0, 0]).unwrap(),
            DeviceConfigChange::AccelProfile(Some(0))
        );
        assert_eq!(
            decode_change(AccelProfile, &[0, 0, 0]).unwrap(),
            DeviceConfigChange::AccelProfile(None)
        );
        // Accel profile custom (index 2) → Invalid, we can't honor it.
        assert!(decode_change(AccelProfile, &[0, 0, 1]).is_err());
        assert_eq!(
            decode_change(ScrollButton, &274u32.to_le_bytes()).unwrap(),
            DeviceConfigChange::ScrollButton(274)
        );
        assert_eq!(
            decode_change(ScrollButtonLock, &[1]).unwrap(),
            DeviceConfigChange::ScrollButtonLock(true)
        );
    }
}
