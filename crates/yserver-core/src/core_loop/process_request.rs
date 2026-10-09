//! Single-threaded entry point for X11 request dispatch.
//!
//! `process_request` is the new home of the opcode `match` that today
//! still lives in `nested::handle_request`. Its signature is the one
//! D4 wires to `Message::Request` arms in `run_core` — every state
//! mutation goes through `&mut ServerState`, every backend call goes
//! through `&mut dyn Backend`, and every reply/event byte is pushed
//! out via `client_io::write_or_buffer` (no more
//! `Arc<Mutex<UnixStream>>` snapshots).
//!
//! ## Migration status
//!
//! D2 finished the additive part of the lift: every fanout helper
//! has a state-borrowing twin. D3 then moved every opcode dispatch
//! arm off `Arc<Mutex<...>>` and onto `&mut`-borrowed types. Every
//! arm `nested::handle_request` had — including all 11 extension
//! dispatchers (RANDR / MIT-SHM / RENDER / XKB / XI2 / XFIXES /
//! SHAPE / SYNC / DAMAGE / COMPOSITE / PRESENT) — has a
//! state-borrowing implementation here. The `nested::handle_request`
//! path is dead-code from D4 forward and gets retired in H1.

mod colormaps;
mod composite_damage;
mod drawing;
mod dri3;
mod focus_pointer;
mod fonts;
mod gc_pixmap_cursor;
mod glx;
mod grabs;
mod input_ctl;
mod misc;
mod present_ext;
mod props;
mod randr_ext;
mod redirect;
mod render;
mod saver_dpms;
mod selection;
mod shape_xfixes;
mod sync_ext;
mod vidmode;
mod windows;
mod xi;
mod xkb;
mod xshm;
mod xtest;

use colormaps::*;
use composite_damage::*;
use drawing::*;
use dri3::*;
pub(crate) use focus_pointer::*;
use fonts::*;
use gc_pixmap_cursor::*;
use glx::*;
pub(crate) use grabs::*;
use input_ctl::*;
use misc::*;
pub use present_ext::*;
pub(crate) use props::*;
pub use randr_ext::*;
pub(crate) use redirect::*;
use render::*;
pub(crate) use saver_dpms::*;
pub(crate) use selection::*;
pub(crate) use shape_xfixes::*;
pub(crate) use sync_ext::*;
use vidmode::*;
pub(crate) use windows::*;
pub(crate) use xi::*;
use xkb::*;
use xshm::*;
use xtest::*;

#[cfg(test)]
mod get_image_reply_tests;
#[cfg(test)]
mod largest_free_xid_gap_tests;
#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet},
    io,
    os::fd::OwnedFd,
};

use log::{debug, trace};
use yserver_protocol::x11::{self, AtomId, ClientId, RequestHeader, ResourceId, SequenceNumber};

#[cfg(test)]
use crate::core_loop::pointer_fanout::pointer_event_fanout_to_state;
use crate::{
    backend::{
        Backend, CrtcConfigApply, CrtcConfigToken, ModeSpec, OriginContext, params::FillState,
    },
    core_loop::{
        client_io::{self, WriteOutcome},
        damage_fanout::{
            accumulate_damage_border_to_state, accumulate_damage_clip_by_children_to_state,
            accumulate_damage_full_to_state, accumulate_damage_to_state,
            report_existing_damage_to_state,
        },
        fanout::{
            client_target_id, emit_visibility_unobscured_subtree_to_state,
            emit_window_event_to_state, emit_xi2_focus_event_to_state, fanout_event_to_clients,
            fanout_raw_event_to_clients, selection_owner_target_id, subscribers_by_id,
        },
        key_fanout::replay_frozen_key_to_focus,
        pointer_fanout::replay_frozen_pointer_event_to_state,
    },
    properties,
    resources::{
        BorderSource, COMPOSITE_OVERLAY_WINDOW, MapState, Pixmap, ROOT_WINDOW, ViewabilityDelta,
        Window,
    },
    server::{
        PendingPresentPixmap, PendingPresentRequest, ScreenSaverActive, ServerState, XI_FIRST_EVENT,
    },
    xinput::{
        XI_DEVICE_KEY_PRESS_OFFSET, XI_DEVICE_PROPERTY_NOTIFY_OFFSET, XI2_DEVICE_CHANGED_MASK,
        XI2_PROPERTY_EVENT_MASK,
    },
};

/// XI2 major opcode assigned by `extension_metadata("XInputExtension")`.
/// This matches the `XI2_MAJOR_OPCODE` constant in nested.rs and goes
/// away in H1 with that file.
const XI2_MAJOR_OPCODE: u8 = 137;
/// XInput extension first-error base (matches `nested.rs::XI2_FIRST_ERROR`).
/// `XI_BadDevice = 0`, so the wire `BadDevice` code is `XI2_FIRST_ERROR + 0`.
const XI2_FIRST_ERROR: u8 = 157;
const XFIXES_MAJOR_OPCODE: u8 = 140;
/// Core request major opcodes for the three resource-release requests
/// that must report an unresolvable XID. Values match this file's own
/// dispatch arms and `yserver_protocol::x11::request_lengths`
/// (`54 => FreePixmap`, `60 => FreeGC`, `95 => FreeCursor`).
const FREE_PIXMAP_OPCODE: u8 = 54;
const FREE_GC_OPCODE: u8 = 60;
const FREE_CURSOR_OPCODE: u8 = 95;
const XI2_SERVER_MAJOR_VERSION: u16 = 2;
const XI2_SERVER_MINOR_VERSION: u16 = 4;
/// Highest request numbers in the corresponding Xorg dispatch tables.
const RENDER_LAST_REQUEST: u8 = 36;
const RANDR_REQUEST_COUNT: u8 = 47;
/// RANDR's fixed first-error base from `extension_metadata("RANDR")`.
const RANDR_BAD_OUTPUT: u8 = 147;
const RANDR_BAD_CRTC: u8 = 147 + 1;
const RANDR_BAD_PROVIDER: u8 = 147 + 3;
/// XFIXES `BadRegion` and SYNC `BadFence` wire error codes. Their extension
/// bases are fixed by `nested::extension_metadata`; the resource-specific
/// errors are offsets 0 and 2 respectively.
const XFIXES_BAD_REGION: u8 = 163;
const SYNC_BAD_FENCE: u8 = 164 + 2;
/// Xorg `PresentAllOptions`: Async, Copy, UST, Suboptimal, AsyncMayTear.
const PRESENT_ALL_OPTIONS: u32 = 0x1f;
// No RANDR_BAD_LEASE: randr.h defines `BadRRLease = 4`, but Xorg references it
// nowhere — RRLeaseType keeps dix's default `errorValue = BadValue`, so a failed
// lease lookup reports BadValue. See the FreeLease arm.
const XINPUT_LAST_REQUEST: u8 = 61;
const XKB_LAST_REQUEST: u8 = 25;
/// XKB request minors (`X_kb*`).
const X_KB_USE_EXTENSION: u8 = 0;
const X_KB_SELECT_EVENTS: u8 = 1;
const X_KB_SET_MAP: u8 = 9;
const X_KB_SET_COMPAT_MAP: u8 = 11;
const X_KB_SET_INDICATOR_MAP: u8 = 14;
const X_KB_SET_NAMES: u8 = 18;
const X_KB_SET_GEOMETRY: u8 = 20;
/// FocusChangeMask
const FOCUS_CHANGE_MASK: u32 = 0x0020_0000;

/// Outcome of `process_request` for one request.
#[derive(Debug)]
pub enum RequestOutcome {
    /// Request handled to completion. Any reply or event bytes are
    /// already buffered/written via `client_io`.
    Handled,
    /// The peer's outbound buffer overflowed (or its socket is
    /// unrecoverable); the core should issue a `Message::ClientDisconnected`.
    Disconnect(ClientId),
    /// A backend operation is still running. The core must withhold this
    /// request's flow-control credit and park later requests from the same
    /// client until the token becomes ready.
    PendingCrtcConfig(PendingCrtcConfig),
    /// A recognized physical libinput property write. The core runner owns
    /// validation, source-targeted application, and commit ordering.
    PendingXiConfig(crate::core_loop::message::XiConfigRequest),
}

/// Dispatch one X11 request entirely on the core thread.
///
/// Every X11 core opcode (1-127) plus every extension dispatcher
/// (128 RANDR through 145 PRESENT) lives in this match. Unknown opcodes
/// return `BadRequest`, matching Xorg's `ProcBadRequest` dispatch entries.
#[allow(
    clippy::needless_pass_by_value,
    reason = "attached_fd is moved into the segment table on AttachFd; \
              passing by value keeps the OwnedFd ownership story simple"
)]
pub fn process_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
    attached_fd: Option<OwnedFd>,
) -> io::Result<RequestOutcome> {
    let origin = Some(OriginContext {
        client_id,
        nested_seq: sequence.0,
        opcode: header.opcode,
    });
    // Stamp the per-client sequence counter so any event fanouts
    // generated by this request encode the correct `seq` field.
    // Pre-F2 the legacy `nested::handle_client` did this after each
    // `handle_request`; run_core's path needs the same store, but
    // earlier — before any fanout helper reads the counter.
    if let Some(client) = state.clients.get(&client_id.0) {
        client
            .last_sequence
            .store(sequence.0, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(outcome) = reject_non_local_extension_request(state, client_id, sequence, header) {
        return outcome;
    }
    if !x11::request_lengths::validate_core_request_length(header.opcode, header.length_units) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            header.opcode,
        );
    }
    // Maximum request length: u16::MAX without BIG-REQUESTS,
    // MAX_BIG_REQUEST_UNITS with it (the value BigRequestsEnable advertises).
    let big_enabled = state
        .clients
        .get(&client_id.0)
        .is_some_and(|c| c.big_requests_enabled);
    let max_length_units = if big_enabled {
        x11::MAX_BIG_REQUEST_UNITS
    } else {
        u32::from(u16::MAX)
    };
    if header.length_units > max_length_units {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            header.opcode,
        );
    }
    // Phase F: variable-length opcodes need content-derived exact-length
    // validation (the spec's `length one less/greater than the minimum
    // required to contain the request` xts probes).
    if !x11::request_lengths::validate_exact_request_length(
        header.opcode,
        header.data,
        header.length_units,
        body,
    ) {
        let required =
            x11::request_lengths::exact_required_length(header.opcode, header.data, body);
        let preview_len = body.len().min(64);
        debug!(
            "exact-length BadLength: client={} seq={} opcode={} header.data={:#x} \
             length_units={} body.len={} required={:?} body_preview={:02x?}",
            client_id.0,
            sequence.0,
            header.opcode,
            header.data,
            header.length_units,
            body.len(),
            required,
            &body[..preview_len],
        );
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            header.opcode,
        );
    }
    // Value-mask validation: requests that carry a CW/GC/configure
    // value-mask must reject masks with unused bits set as BadValue.
    if let Some(bad) = x11::request_lengths::invalid_value_mask(header.opcode, body) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            bad,
            header.opcode,
        );
    }
    // Per-opcode scalar value-range validation (Group A: fixed-position
    // fields like grab modes, owner_events bool, CopyPlane single-bit).
    if let Some(bad) = x11::request_lengths::invalid_value(header.opcode, header.data, body) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            bad,
            header.opcode,
        );
    }
    let outcome = match header.opcode {
        // ── server scheduling grab ──
        36 => handle_grab_server(state, client_id, sequence),
        37 => handle_ungrab_server(state, client_id, sequence),
        // ── void requests with local state/backend handling ──
        96 => handle_recolor_cursor(state, backend, origin, client_id, sequence, body),
        102 => handle_change_keyboard_control(state, client_id, sequence, header, body),
        104 => handle_bell(state, client_id, sequence, header),
        105 => handle_change_pointer_control(state, client_id, sequence, header, body),
        107 => handle_set_screen_saver(state, client_id, sequence, header, body),
        115 => handle_force_screen_saver(state, backend, client_id, sequence, header),
        127 => log_void(client_id, sequence, "NoOperation"),
        // ── trivial replies (no state mutation, no body parsing) ──
        43 => handle_get_input_focus(state, client_id, sequence),
        44 => handle_query_keymap(state, client_id, sequence),
        103 => handle_get_keyboard_control(state, client_id, sequence),
        106 => handle_get_pointer_control(state, client_id, sequence),
        108 => handle_get_screen_saver(state, client_id, sequence),
        110 => handle_list_hosts(state, client_id, sequence),
        117 => handle_get_pointer_mapping(state, client_id, sequence),
        // ── replies backed by small or backend-owned state ──
        39 => handle_get_motion_events(state, client_id, sequence, body),
        51 => handle_set_font_path(state, backend, origin, client_id, sequence, header, body),
        52 => handle_get_font_path(state, backend, client_id, sequence),
        83 => handle_list_installed_colormaps(state, client_id, sequence, body),
        // ── GC dashes (multi-byte pattern; opcode 58 is its own request,
        //    distinct from the single-byte CreateGC/ChangeGC dash form). ──
        58 => handle_set_dashes(state, client_id, sequence, body),
        // ── colormap lifecycle (BadIDChoice on duplicate ID) ──
        78 => handle_create_colormap(state, client_id, sequence, body),
        79 => handle_free_colormap(state, client_id, sequence, body),
        80 => handle_copy_colormap_and_free(state, client_id, sequence, body),
        81 => handle_install_colormap(state, client_id, sequence, body),
        82 => handle_uninstall_colormap(state, client_id, sequence, body),
        // ── SetCloseDownMode: validate mode (0/1/2 valid) ──
        112 => handle_set_close_down_mode(state, client_id, sequence, header),
        // ── KillClient (AllTemporary or by resource owner) ──
        113 => handle_kill_client(state, backend, client_id, sequence, body),
        // ── pointer/modifier mapping (reply + MappingNotify fanout) ──
        116 => handle_set_pointer_mapping(state, client_id, sequence, header, body),
        118 => {
            handle_set_modifier_mapping(state, backend, origin, client_id, sequence, header, body)
        }
        // ── state-read replies (read state, no backend, no mutation) ──
        14 => handle_get_geometry(state, client_id, sequence, body),
        15 => handle_query_tree(state, client_id, sequence, body),
        16 => handle_intern_atom(state, client_id, sequence, header, body),
        21 => handle_list_properties(state, client_id, sequence, body),
        23 => handle_get_selection_owner(state, client_id, sequence, body),
        40 => handle_translate_coordinates(state, client_id, sequence, body),
        // ── grabs (pure state mutation on ServerState.{pointer,key}_grabs) ──
        26 => handle_grab_pointer(state, backend, client_id, sequence, header, body),
        27 => handle_ungrab_pointer(state, backend, client_id, sequence, body),
        28 => handle_grab_button(state, client_id, sequence, header, body),
        29 => handle_ungrab_button(state, client_id, sequence, header, body),
        30 => handle_change_active_pointer_grab(state, client_id, sequence, body),
        31 => handle_grab_keyboard(state, client_id, sequence, header, body),
        32 => handle_ungrab_keyboard(state, client_id, sequence, body),
        33 => handle_grab_key(state, client_id, sequence, header, body),
        34 => handle_ungrab_key(state, client_id, sequence, header, body),
        // ── font / size queries (pure state-read replies) ──
        47 => handle_query_font(state, client_id, sequence, body),
        48 => handle_query_text_extents(state, client_id, sequence, header, body),
        97 => handle_query_best_size(state, client_id, sequence, body),
        // ── properties (state mutation + PropertyNotify fanout) ──
        18 => handle_change_property(state, backend, client_id, sequence, header, body),
        19 => handle_delete_property(state, backend, client_id, sequence, body),
        20 => handle_get_property(state, backend, client_id, sequence, header, body),
        // ── backend-proxy replies (state-light, host-RPC) ──
        17 => handle_get_atom_name(state, backend, origin, client_id, sequence, body),
        38 => handle_query_pointer(state, backend, origin, client_id, sequence, body),
        41 => handle_warp_pointer(state, backend, origin, client_id, sequence, body),
        119 => handle_get_modifier_mapping(state, backend, origin, client_id, sequence),
        // ── selections + SendEvent (cross-client fanout) ──
        22 => handle_set_selection_owner(state, client_id, sequence, body),
        24 => handle_convert_selection(state, client_id, sequence, body),
        25 => handle_send_event(state, client_id, sequence, header, body),
        // ── fonts (state mutation + backend lifecycle) ──
        45 => handle_open_font(state, backend, origin, client_id, sequence, body),
        46 => handle_close_font(state, backend, origin, client_id, sequence, body),
        // ── color queries (no state, just protocol replies) ──
        84 => handle_alloc_color(state, client_id, sequence, body),
        85 => handle_alloc_named_color(state, client_id, sequence, body),
        86 => handle_alloc_color_cells(state, client_id, sequence, body),
        87 => handle_alloc_color_planes(state, client_id, sequence, body),
        88 => handle_free_colors(state, client_id, sequence, body),
        89 => handle_store_colors(state, client_id, sequence),
        90 => handle_store_named_color(state, client_id, sequence),
        91 => handle_query_colors(state, client_id, sequence, body),
        92 => handle_lookup_color(state, client_id, sequence, body),
        // ── keyboard mapping (server-wide MappingNotify + backend proxy) ──
        100 => handle_change_keyboard_mapping(state, backend, client_id, sequence, header, body),
        101 => handle_get_keyboard_mapping(state, backend, origin, client_id, sequence, body),
        // ── save-set + cursor lifecycle ──
        6 => handle_change_save_set(state, client_id, sequence, header, body),
        95 => handle_free_cursor(state, backend, origin, client_id, sequence, body),
        // ── pixmaps (state + backend lifecycle) ──
        53 => handle_create_pixmap(state, backend, origin, client_id, sequence, header, body),
        54 => handle_free_pixmap(state, backend, origin, client_id, sequence, body),
        // ── font listing (backend proxy with reply sequence rewrite) ──
        49 => handle_list_fonts(state, backend, origin, client_id, sequence, body),
        50 => handle_list_fonts_with_info(state, backend, origin, client_id, sequence, body),
        // ── GContext (pure state mutation) ──
        55 => handle_create_gc(state, client_id, sequence, body),
        56 => handle_change_gc(state, backend, origin, client_id, sequence, body),
        57 => handle_copy_gc(state, backend, origin, client_id, sequence, body),
        59 => handle_set_clip_rectangles(state, backend, origin, client_id, sequence, header, body),
        60 => handle_free_gc(state, backend, origin, client_id, sequence, body),
        // ── drawing (state read + backend RPC + damage) ──
        61 => handle_clear_area(state, backend, origin, client_id, sequence, header, body),
        62 => handle_copy_area(state, backend, origin, client_id, sequence, body),
        63 => handle_copy_plane(state, backend, origin, client_id, sequence, body),
        64 => handle_poly_point(state, backend, origin, client_id, sequence, header, body),
        65 => handle_poly_line(state, backend, origin, client_id, sequence, header, body),
        66 => handle_poly_segment(state, backend, origin, client_id, sequence, body),
        67 => handle_poly_rectangle(state, backend, origin, client_id, sequence, body),
        68 => handle_poly_arc(state, backend, origin, client_id, sequence, body),
        69 => handle_fill_poly(state, backend, origin, client_id, sequence, body),
        70 => handle_poly_fill_rectangle(state, backend, origin, client_id, sequence, body),
        71 => handle_poly_fill_arc(state, backend, origin, client_id, sequence, body),
        72 => handle_put_image(state, backend, origin, client_id, sequence, header, body),
        73 => handle_get_image(state, backend, origin, client_id, sequence, header, body),
        74 => handle_poly_text8(state, backend, origin, client_id, sequence, body),
        75 => handle_poly_text16(state, backend, origin, client_id, sequence, body),
        76 => handle_image_text8(state, backend, origin, client_id, sequence, header, body),
        77 => handle_image_text16(state, backend, origin, client_id, sequence, header, body),
        // ── focus + AllowEvents ──
        42 => handle_set_input_focus(state, client_id, sequence, header, body),
        35 => handle_allow_events(state, backend, client_id, sequence, header, body),
        // ── extension queries ──
        98 => handle_query_extension(state, backend, client_id, sequence, body),
        99 => handle_list_extensions(state, backend, client_id, sequence),
        // ── cursor creation ──
        93 => handle_create_cursor(state, backend, origin, client_id, sequence, body),
        94 => handle_create_glyph_cursor(state, backend, origin, client_id, sequence, body),
        // ── window queries / circulation ──
        3 => handle_get_window_attributes(state, client_id, sequence, body),
        13 => handle_circulate_window(state, backend, client_id, sequence, header, body),
        // ── extension extension-protocol arms (standalone, not full
        //    extension dispatchers) ──
        138 => handle_ge_request(state, client_id, sequence, header), // GE
        135 => handle_big_requests_request(state, client_id, sequence, header), // BIG-REQUESTS
        // ── unmap (window lifecycle) ──
        10 => handle_unmap_window(state, backend, origin, client_id, sequence, body),
        11 => handle_unmap_subwindows(state, backend, origin, client_id, sequence, body),
        // ── map (window lifecycle) ──
        8 => handle_map_window(state, backend, origin, client_id, sequence, body),
        9 => handle_map_subwindows(state, backend, origin, client_id, sequence, body),
        // ── destroy (window lifecycle) ──
        4 => handle_destroy_window(state, backend, origin, client_id, sequence, body),
        5 => handle_destroy_subwindows(state, backend, origin, client_id, sequence, body),
        // ── configure (window geometry/stacking) ──
        12 => handle_configure_window(state, backend, origin, client_id, sequence, body),
        // ── attributes ──
        2 => handle_change_window_attributes(state, backend, origin, client_id, sequence, body),
        // ── window create ──
        1 => handle_create_window(state, backend, origin, client_id, sequence, header, body),
        // ── reparent ──
        7 => handle_reparent_window(state, backend, origin, client_id, sequence, body),
        // ── XKB extension proxy ──
        136 => handle_xkb_request(state, backend, origin, client_id, sequence, header, body),
        // ── XI2 extension dispatcher ──
        137 => handle_xi2_request(state, backend, origin, client_id, sequence, header, body),
        // ── PRESENT extension dispatcher ──
        145 => handle_present_request(state, backend, origin, client_id, sequence, header, body),
        151 => handle_xinerama_request(state, client_id, sequence, header, body), // XINERAMA
        // ── DAMAGE extension dispatcher ──
        143 => handle_damage_request(state, backend, client_id, sequence, header, body),
        // ── MIT-SHM extension dispatcher ──
        130 => handle_mit_shm_request(
            state,
            backend,
            origin,
            client_id,
            sequence,
            header,
            body,
            attached_fd,
        ),
        // ── COMPOSITE extension dispatcher ──
        144 => handle_composite_request(state, backend, origin, client_id, sequence, header, body),
        // ── XFIXES extension dispatcher ──
        140 => dispatch_xfixes_request(state, backend, origin, client_id, sequence, header, body),
        // ── SHAPE extension dispatcher ──
        141 => handle_shape_request(state, backend, origin, client_id, sequence, header, body),
        // ── SYNC extension dispatcher ──
        142 => handle_sync_request(state, backend, client_id, sequence, header, body),
        // ── RANDR extension dispatcher ──
        128 => handle_randr_request(state, backend, client_id, sequence, header, body),
        // ── RENDER extension dispatcher ──
        133 => handle_render_request(state, backend, origin, client_id, sequence, header, body),
        // ── DPMS extension dispatcher ──
        134 => handle_dpms_request(state, backend, client_id, sequence, header, body),
        // ── XTEST extension dispatcher ──
        146 => handle_xtest_request(state, backend, client_id, sequence, header, body),
        // ── DRI3 extension dispatcher ──
        147 => handle_dri3_request(
            state,
            backend,
            client_id,
            sequence,
            header,
            body,
            attached_fd,
        ),
        // ── GLX extension dispatcher ──
        148 => handle_glx_request(state, backend, origin, client_id, sequence, header, body),
        // ── X-Resource (XRes) extension dispatcher ──
        149 => handle_x_resource_request(state, client_id, sequence, header, body),
        // ── MIT-SCREEN-SAVER extension dispatcher ──
        150 => handle_screen_saver_request(state, backend, client_id, sequence, header, body),
        // ── XC-MISC extension dispatcher ──
        152 => handle_xcmisc_request(state, client_id, sequence, header, body),
        // ── XFree86-VidModeExtension dispatcher ──
        153 => handle_xf86vidmode_request(state, backend, client_id, sequence, header, body),
        // ── RECORD extension dispatcher ──
        154 => crate::core_loop::record::handle_record_request(
            state, client_id, sequence, header, body,
        ),
        opcode => {
            debug!(
                "client {} #{} unknown opcode {} ({} bytes) -> BadRequest",
                client_id.0,
                sequence.0,
                opcode,
                body.len() + 4
            );
            emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                opcode,
            )
        }
    };
    backend.sync_floating_keyboard_states(state);
    outcome
}

/// Apply Xorg's per-client locality policy before an extension handler sees a
/// request.  Extensions remain advertised; remote clients receive the same
/// dispatch errors that they do from Xorg.
fn reject_non_local_extension_request(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
) -> Option<io::Result<RequestOutcome>> {
    let (is_local, fd_passing) = state
        .clients
        .get(&client_id.0)
        .map_or((true, true), |client| (client.is_local, client.fd_passing));

    // DRI3 needs a DESCRIPTOR, not merely a local peer — every one of its
    // requests either sends or receives one. So it is gated on
    // `fd_passing`, which is false for any TCP peer however local.
    //
    // This is finer-grained than Xorg, deliberately. There a loopback TCP
    // client is `client->local` (`xtransLocalClient`, os/access.c), so
    // DRI3 dispatch is allowed and the request fails later, at the
    // descriptor it cannot carry. Same refusal, worse diagnosis: the
    // client sees the extension work and then break. Refusing at the gate
    // reports `BadMatch` where the client already handles it.
    if header.opcode == 147 && !fd_passing {
        return Some(emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_MATCH,
            0,
            u16::from(header.data),
            147,
        ));
    }

    // MIT-SHM's two descriptor-bearing minors need the same treatment, and
    // for the same reason: `is_local` is an ADDRESS property, so a loopback
    // TCP peer is local and reaches them, then fails at the descriptor the
    // transport cannot carry. `Attach` is deliberately NOT here — it passes
    // a SysV shmid, not a descriptor, and stays available to a local TCP
    // client (see `tcp_tests.rs`). Neither is `QueryVersion`.
    //
    // The codes are Xorg's, per minor rather than uniform: `ProcShmAttachFd`
    // reaches `ReadFdFromClient` and returns `BadMatch` when it fails
    // (`Xext/shm.c:1163`), while `ProcShmCreateSegment` returns `BadAlloc`
    // when `WriteFdToClient` does (`Xext/shm.c:1323`). A client that handles
    // Xorg's answer handles ours.
    if header.opcode == 130 && !fd_passing {
        use yserver_protocol::x11::mit_shm;
        let code = match header.data {
            mit_shm::ATTACH_FD => Some(x11::error::BAD_MATCH),
            mit_shm::CREATE_SEGMENT => Some(x11::error::BAD_ALLOC),
            _ => None,
        };
        if let Some(code) = code {
            return Some(emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                code,
                0,
                u16::from(header.data),
                130,
            ));
        }
    }

    if is_local {
        return None;
    }

    match header.opcode {
        // dri3/dri3_request.c rejects every DRI3 request from a non-local
        // client, before inspecting its minor opcode or body.
        147 => Some(emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_MATCH,
            0,
            u16::from(header.data),
            147,
        )),
        // Xext/shm.c keeps QueryVersion available to remote clients but
        // rejects the descriptor- and shared-memory-bearing requests.
        130 if header.data != yserver_protocol::x11::mit_shm::QUERY_VERSION => {
            Some(emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                u16::from(header.data),
                130,
            ))
        }
        // Xext/vidmode.c lets its read-only requests through for remote
        // clients, but reports ClientNotLocal for known mutating minors and
        // BadRequest for unknown minors.
        153 => {
            use crate::nested::{XF86VIDMODE_FIRST_ERROR, XF86VIDMODE_MAJOR_OPCODE};
            use yserver_protocol::x11::xf86vidmode as x11vm;

            let code = match header.data {
                x11vm::QUERY_VERSION
                | x11vm::GET_MODE_LINE
                | x11vm::GET_MONITOR
                | x11vm::GET_ALL_MODE_LINES
                | x11vm::VALIDATE_MODE_LINE
                | x11vm::GET_VIEW_PORT
                | x11vm::GET_DOT_CLOCKS
                | x11vm::SET_CLIENT_VERSION
                | x11vm::GET_GAMMA
                | x11vm::GET_GAMMA_RAMP
                | x11vm::GET_GAMMA_RAMP_SIZE
                | x11vm::GET_PERMISSIONS => return None,
                x11vm::MOD_MODE_LINE
                | x11vm::SWITCH_MODE
                | x11vm::LOCK_MODE_SWITCH
                | x11vm::ADD_MODE_LINE
                | x11vm::DELETE_MODE_LINE
                | x11vm::SWITCH_TO_MODE
                | x11vm::SET_VIEW_PORT
                | x11vm::SET_GAMMA
                | x11vm::SET_GAMMA_RAMP => XF86VIDMODE_FIRST_ERROR + x11vm::CLIENT_NOT_LOCAL,
                _ => x11::error::BAD_REQUEST,
            };
            Some(emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                code,
                0,
                u16::from(header.data),
                XF86VIDMODE_MAJOR_OPCODE,
            ))
        }
        // Present intentionally has no locality gate in Xorg.
        _ => None,
    }
}

/// Special-case writer for MIT-SHM CreateSegment: sends a normal X11
/// reply alongside an SCM_RIGHTS file descriptor. This is the one path
/// where the lifted code reaches around `client_io::write_or_buffer`
/// because the kernel only delivers the fd if it accompanies inline
/// bytes — buffering would split them apart. Once the outbound buffer
/// is non-empty we can't insert an FD-carrying frame, so we drain
/// first.
fn send_reply_with_fd(
    client: &mut crate::server::ClientState,
    bytes: &[u8],
    fd: std::os::fd::RawFd,
) -> io::Result<()> {
    if !client.fd_passing {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file descriptor passing is unavailable on this transport",
        ));
    }
    // Drain whatever's pending so the SCM_RIGHTS frame lands in order.
    while !client.outbound.is_empty() {
        match client_io::drain_outbound(client)? {
            client_io::WriteOutcome::Done => break,
            client_io::WriteOutcome::WouldBlock => {
                std::thread::yield_now();
            }
            client_io::WriteOutcome::Disconnect => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "client outbound disconnect during fd send",
                ));
            }
        }
    }
    let writer_arc = client.writer.clone();
    let mut w = writer_arc
        .lock()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "client writer mutex poisoned"))?;
    let crate::transport::Transport::Unix(stream) = &mut *w else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file descriptor passing is unavailable on this transport",
        ));
    };
    crate::unix_fd::send_with_fd(stream, bytes, fd)
}

/// A window or pixmap of that id.
fn drawable_exists(state: &ServerState, id: ResourceId) -> bool {
    state.resources.window(id).is_some() || state.resources.pixmap(id).is_some()
}

#[derive(Debug, Clone, Copy)]
struct PresentDomainSelection {
    crtc_id: u32,
    crtc_epoch: u64,
    msc_offset: u64,
    raw_msc: u64,
    raw_ust: u64,
}

#[derive(Debug)]
struct CurrentVidModeOutput {
    output_id: u32,
    crtc_id: u32,
    connector: String,
    mode: yserver_protocol::x11::xf86vidmode::ModeLine,
}

fn motion_history_range(
    state: &ServerState,
    start: u32,
    stop: u32,
) -> Vec<crate::server::PointerMotionRecord> {
    let now = state.timestamp_now();
    let start = if start == 0 { now } else { start };
    let stop = if stop == 0 { now } else { stop.min(now) };
    if start > stop || start > now {
        return Vec::new();
    }
    state
        .pointer_motion_history
        .iter()
        .copied()
        .filter(|record| (start..=stop).contains(&record.time))
        .collect()
}

fn supported_pixmap_depth(depth: u8) -> bool {
    matches!(depth, 1 | 4 | 8 | 24 | 32)
}

/// Resolve a `Drawable` resource id to its kind + depth for error
/// validation. Mirrors Xorg's `dixLookupDrawable` shape: the four
/// outcomes the spec distinguishes.
enum DrawableLookup {
    InputOutputWindow { depth: u8 },
    InputOnlyWindow,
    Pixmap { depth: u8 },
    Missing,
}

fn drawable_lookup(state: &ServerState, id: ResourceId) -> DrawableLookup {
    if let Some(w) = state.resources.window(id) {
        return match w.class {
            crate::resources::WindowClass::InputOnly => DrawableLookup::InputOnlyWindow,
            // `CopyFromParent` / `Other` only show up pre-CreateWindow
            // resolution and shouldn't be reachable for a registered window;
            // treat them the same as the regular InputOutput case.
            crate::resources::WindowClass::InputOutput
            | crate::resources::WindowClass::CopyFromParent
            | crate::resources::WindowClass::Other(_) => {
                DrawableLookup::InputOutputWindow { depth: w.depth }
            }
        };
    }
    if let Some(p) = state.resources.pixmap(id) {
        return DrawableLookup::Pixmap { depth: p.depth };
    }
    DrawableLookup::Missing
}

/// Result of validating (drawable, gc) for a paint request. The
/// shape mirrors Xorg's `VALIDATE_DRAWABLE_AND_GC` macro: emit
/// BadDrawable, BadMatch (inputonly), BadGC, or BadMatch
/// (gc-drawable-depth) in that priority order.
///
/// `gc-drawable-screen` is not modelled — yserver has one screen
/// so the BadMatch-screen case is structurally unreachable, and
/// XTS marks the corresponding TPs UNSUPPORTED on a single-screen
/// server already.
fn validate_drawable_and_gc(
    state: &ServerState,
    drawable: ResourceId,
    gc: ResourceId,
) -> Result<(), (u8, u32)> {
    let target_depth = match drawable_lookup(state, drawable) {
        DrawableLookup::InputOutputWindow { depth } | DrawableLookup::Pixmap { depth } => depth,
        DrawableLookup::InputOnlyWindow => return Err((x11::error::BAD_MATCH, 0)),
        DrawableLookup::Missing => return Err((x11::error::BAD_DRAWABLE, drawable.0)),
    };
    let Some(gc_entry) = state.resources.gc(gc) else {
        return Err((x11::error::BAD_GC, gc.0));
    };
    // GC depth is fixed at CreateGC to the depth of the origin drawable
    // (X11 spec). Stored on the GC entry directly so it survives the
    // origin drawable being freed. `depth == 0` means "GC was created
    // before the depth field was tracked" — skip the check rather than
    // emit a spurious BadMatch.
    if gc_entry.depth != 0 && gc_entry.depth != target_depth {
        return Err((x11::error::BAD_MATCH, 0));
    }
    Ok(())
}

/// Sibling of [`validate_drawable_and_gc`] for paint ops that consume
/// a drawable WITHOUT a GC depth-match constraint — the `src` side of
/// `CopyPlane`, for instance, where any depth is acceptable because
/// the plane mask extracts a single bit from whatever the source is.
/// Still emits BadDrawable for missing and BadMatch for `InputOnly`
/// windows.
fn validate_drawable_only(state: &ServerState, drawable: ResourceId) -> Result<(), (u8, u32)> {
    match drawable_lookup(state, drawable) {
        DrawableLookup::InputOutputWindow { .. } | DrawableLookup::Pixmap { .. } => Ok(()),
        DrawableLookup::InputOnlyWindow => Err((x11::error::BAD_MATCH, 0)),
        DrawableLookup::Missing => Err((x11::error::BAD_DRAWABLE, drawable.0)),
    }
}

/// Sibling for ChangeGC / SetDashes / SetClipRectangles: BadGC on
/// missing id, no drawable involvement. Mirrors Xorg's `dixLookupGC`
/// gate (`dix/dispatch.c:1589,1639,1664`).
fn validate_gc_only(state: &ServerState, gc: ResourceId) -> Result<(), (u8, u32)> {
    if state.resources.gc(gc).is_some() {
        Ok(())
    } else {
        Err((x11::error::BAD_GC, gc.0))
    }
}

/// Encode an X11 protocol error and ship it to the client through
/// `client_io::write_or_buffer`. Mirrors `nested::emit_x11_error` but
/// operates on the new `&mut ClientState` plumbing.
fn emit_x11_error(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    code: u8,
    bad_value: u32,
    major_opcode: u8,
) -> io::Result<RequestOutcome> {
    emit_x11_error_with_minor(state, client_id, sequence, code, bad_value, 0, major_opcode)
}

pub(crate) fn emit_x11_error_with_minor(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    code: u8,
    bad_value: u32,
    minor_opcode: u16,
    major_opcode: u8,
) -> io::Result<RequestOutcome> {
    debug!(
        "emit_x11_error: client={} seq={} code={} bad_value=0x{:x} minor={} major_opcode={}",
        client_id.0, sequence.0, code, bad_value, minor_opcode, major_opcode
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_error(
        &mut buf,
        byte_order,
        sequence,
        code,
        bad_value,
        minor_opcode,
        major_opcode,
    )?;
    Ok(write_to_client(client, client_id, &buf))
}

fn log_void(
    client_id: ClientId,
    sequence: SequenceNumber,
    name: &str,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} {name}", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

fn xid_out_of_client_range(state: &ServerState, client_id: ClientId, xid: u32) -> bool {
    let Some(client) = state.clients.get(&client_id.0) else {
        return false;
    };
    let base = client.resource_id_base;
    let mask = client.resource_id_mask;
    (xid & !mask) != base
}

/// Compute the expected ZPixmap data length for `width × height` at
/// `depth`, returning `None` for unsupported depths or arithmetic
/// overflow. Mirrors `nested::zpixmap_expected_len`.
fn zpixmap_expected_len(width: u16, height: u16, depth: u8) -> Option<usize> {
    let stride_bytes = zpixmap_row_stride(width, depth)?;
    stride_bytes.checked_mul(usize::from(height))
}

/// True for a window that is not viewable (Xorg: not realized); false for pixmaps.
fn window_unviewable(state: &ServerState, id: ResourceId) -> bool {
    state
        .resources
        .window(id)
        .is_some_and(|w| w.map_state != crate::resources::MapState::Viewable)
}

/// Rectangle in destination-window-local coordinates, used by the
/// X11 CopyArea clipping pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CopyAreaSubRect {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

pub(crate) fn write_to_client(
    client: &mut crate::server::ClientState,
    client_id: ClientId,
    bytes: &[u8],
) -> RequestOutcome {
    crate::core_loop::fanout::record_outbound_telemetry(client_id, client.byte_order, bytes);
    match client_io::write_or_buffer(client, bytes) {
        Ok(WriteOutcome::Done | WriteOutcome::WouldBlock) => RequestOutcome::Handled,
        Ok(WriteOutcome::Disconnect) => RequestOutcome::Disconnect(client_id),
        Err(_) => RequestOutcome::Disconnect(client_id),
    }
}
