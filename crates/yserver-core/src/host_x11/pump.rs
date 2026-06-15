//! Host X11 connection setup.
//!
//! Pre-Phase-6.3 this module owned the *second* X11 connection ynest
//! used: the input pump's read side, alongside `HostX11Backend`'s
//! request stream. Phase 6.3 Step 4 ("Big Flip") merges those into one
//! — the host event read path now lives on the backend's dispatcher
//! thread; Step 6 then deletes the `HostInputPumpHandle` compat wrapper
//! entirely so call sites in `nested.rs` go through the `Backend` trait
//! directly.
//!
//! Setup-time helpers live here too: `connect_to_host`, `XAuthority`,
//! `read_setup_reply`. `HostX11Backend::open_from_env` calls them through
//! `pub(super)` re-exports so it doesn't have to duplicate the wire
//! decoding.

use std::{
    env, fs,
    io::{self, ErrorKind, Read, Write},
    net::TcpStream,
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
};

use super::{pad4, padded_len, read_i16, read_u16, read_u32, write_u16};

const MIT_MAGIC_COOKIE: &str = "MIT-MAGIC-COOKIE-1";

/// Wraps either a Unix-domain socket (the default `/tmp/.X11-unix/Xn`
/// transport) or a TCP socket (used when `DISPLAY` declares a
/// non-empty, non-"unix" hostname — most commonly `localhost:N` from
/// SSH X11 forwarding). `Read`, `Write`, and `AsRawFd` delegate to
/// the inner type so the rest of the host-X11 code can stay
/// transport-agnostic.
///
/// Note: SCM_RIGHTS-based fd-passing (used by MIT-SHM `AttachFd`)
/// only works on the Unix variant. The TCP variant simply can't
/// forward fd-mode shm to the host server; that path stays
/// fd-passing-disabled when the host connection is TCP.
pub enum HostStream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Read for HostStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            HostStream::Unix(s) => s.read(buf),
            HostStream::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for HostStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            HostStream::Unix(s) => s.write(buf),
            HostStream::Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            HostStream::Unix(s) => s.flush(),
            HostStream::Tcp(s) => s.flush(),
        }
    }
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            HostStream::Unix(s) => s.write_all(buf),
            HostStream::Tcp(s) => s.write_all(buf),
        }
    }
}

impl AsRawFd for HostStream {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            HostStream::Unix(s) => s.as_raw_fd(),
            HostStream::Tcp(s) => s.as_raw_fd(),
        }
    }
}

pub(super) fn connect_to_host() -> io::Result<HostStream> {
    let display = env::var("DISPLAY").map_err(|_| {
        io::Error::new(
            ErrorKind::NotFound,
            "DISPLAY is not set for host X11 backend",
        )
    })?;
    let (host, display_number) = parse_display(&display)?;

    let auth = XAuthority::load(display_number).unwrap_or_default();
    let mut stream = match host.as_deref() {
        // Empty host or "unix" → AF_UNIX at /tmp/.X11-unix/Xn.
        None | Some("") | Some("unix") => {
            let socket_path = format!("/tmp/.X11-unix/X{display_number}");
            HostStream::Unix(UnixStream::connect(socket_path)?)
        }
        // Otherwise TCP at host:(6000+display_number). Most commonly
        // localhost:6010 for SSH X11 forwarding (DISPLAY=localhost:10.0).
        Some(host) => {
            let port: u16 = 6000u16.checked_add(display_number).ok_or_else(|| {
                io::Error::new(
                    ErrorKind::InvalidInput,
                    "DISPLAY number too large for TCP port",
                )
            })?;
            HostStream::Tcp(TcpStream::connect((host, port))?)
        }
    };
    write_setup_request(&mut stream, auth.as_ref())?;
    Ok(stream)
}

/// Decode a raw 32-byte X11 event header into a `HostEvent`. Returns
/// `None` for event types this layer ignores (synthetic flag is masked
/// off via the bit-7 strip; the rest of the cases mirror the previous
/// inline decode in `HostInputPump::read_event`).
///
/// Phase 6.3 Step 4 makes this reusable from `HostX11Backend`'s
/// dispatcher thread so that the merged main connection can fan host
/// events out to the sink without going through a separate socket /
/// thread (Xnest's classic pattern, but simplified to one connection).
pub(super) fn decode_host_event(event: &[u8; 32]) -> Option<HostEvent> {
    let event_type = event[0] & 0x7f;
    match event_type {
        2 | 3 => Some(HostEvent::Key(HostKeyEvent {
            pressed: event_type == 2,
            keycode: event[1],
            time: read_u32(&event[4..8]),
            root_x: read_i16(&event[20..22]),
            root_y: read_i16(&event[22..24]),
            event_x: read_i16(&event[24..26]),
            event_y: read_i16(&event[26..28]),
            state: read_u16(&event[28..30]),
        })),
        4..=6 => {
            let kind = match event_type {
                4 => PointerEventKind::ButtonPress,
                5 => PointerEventKind::ButtonRelease,
                _ => PointerEventKind::MotionNotify,
            };
            Some(HostEvent::Pointer(HostPointerEvent {
                kind,
                host_xid: read_u32(&event[12..16]), // event window
                detail: event[1],
                time: read_u32(&event[4..8]),
                root_x: read_i16(&event[20..22]),
                root_y: read_i16(&event[22..24]),
                event_x: read_i16(&event[24..26]),
                event_y: read_i16(&event[26..28]),
                state: read_u16(&event[28..30]),
                crossing_mode: 0,
                // Pointer events (Button/Motion) have `child` at bytes
                // 16..20 on the X11 wire (xcb_motion_notify_event_t
                // .child / etc.); pass it through so downstream
                // encoders that want the host server's computed child
                // can use it directly.
                child: read_u32(&event[16..20]),
            }))
        }
        7 | 8 => {
            let kind = if event_type == 7 {
                PointerEventKind::EnterNotify
            } else {
                PointerEventKind::LeaveNotify
            };
            // Crossing wire layout: detail at byte 1, child at 16..20,
            // mode at byte 30.
            Some(HostEvent::Pointer(HostPointerEvent {
                kind,
                host_xid: read_u32(&event[12..16]),
                detail: event[1],
                time: read_u32(&event[4..8]),
                root_x: read_i16(&event[20..22]),
                root_y: read_i16(&event[22..24]),
                event_x: read_i16(&event[24..26]),
                event_y: read_i16(&event[26..28]),
                state: read_u16(&event[28..30]),
                crossing_mode: event[30],
                child: read_u32(&event[16..20]),
            }))
        }
        12 => {
            let host_xid = read_u32(&event[4..8]);
            let x = read_u16(&event[8..10]);
            let y = read_u16(&event[10..12]);
            let width = read_u16(&event[12..14]);
            let height = read_u16(&event[14..16]);
            let count = read_u16(&event[16..18]);
            log::trace!(
                "host dispatch: Expose host_xid=0x{host_xid:x} x={x} y={y} w={width} h={height} count={count}",
            );
            Some(HostEvent::Expose(HostExposeEvent {
                host_xid,
                x,
                y,
                width,
                height,
                count,
            }))
        }
        22 => Some(HostEvent::Configure(HostConfigureEvent {
            host_xid: read_u32(&event[8..12]),
            x: read_i16(&event[16..18]),
            y: read_i16(&event[18..20]),
            width: read_u16(&event[20..22]),
            height: read_u16(&event[22..24]),
        })),
        17 => Some(HostEvent::Closed),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug)]
pub enum HostEvent {
    Key(HostKeyEvent),
    Pointer(HostPointerEvent),
    Expose(HostExposeEvent),
    Configure(HostConfigureEvent),
    Closed,
}

#[derive(Clone, Copy, Debug)]
pub struct HostExposeEvent {
    pub host_xid: u32,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub count: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct HostConfigureEvent {
    pub host_xid: u32,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerEventKind {
    ButtonPress,
    ButtonRelease,
    MotionNotify,
    EnterNotify,
    LeaveNotify,
}

#[derive(Clone, Copy, Debug)]
pub struct HostPointerEvent {
    pub kind: PointerEventKind,
    pub host_xid: u32,
    pub detail: u8,
    pub time: u32,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state: u16,
    /// X11 crossing mode: 0=NotifyNormal, 1=NotifyGrab, 2=NotifyUngrab.
    /// Only meaningful when `kind` is `EnterNotify`/`LeaveNotify`.
    pub crossing_mode: u8,
    /// X11 EnterNotify/LeaveNotify `child` field, as computed by the
    /// producer per X11 spec (see `crossings::CrossingEvent::child`).
    /// 0 = X11 `None`. The encoder layer uses this directly for
    /// Crossing events rather than the propagation walk's child,
    /// since spec-correct child semantics aren't a function of the
    /// fanout walk.
    pub child: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct HostKeyEvent {
    pub pressed: bool,
    pub keycode: u8,
    pub time: u32,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct PointerPosition {
    pub same_screen: bool,
    pub win_x: i16,
    pub win_y: i16,
    pub mask: u16,
    /// Backend-side event window under the cursor when known. KMS can
    /// answer this from its live window-under-cursor walk; host-X11
    /// leaves it unset because the proxied `QueryPointer` path only
    /// snapshots coordinates and mask today.
    pub host_xid: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostSubwindowConfig {
    pub x: Option<i16>,
    pub y: Option<i16>,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub border_width: Option<u16>,
    pub sibling: Option<u32>,
    pub stack_mode: Option<u8>,
}

#[derive(Clone, Debug, Default)]
struct XAuthority {
    name: Vec<u8>,
    data: Vec<u8>,
}

impl XAuthority {
    fn load(display_number: u16) -> io::Result<Option<Self>> {
        let path = env::var_os("XAUTHORITY")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".Xauthority")))
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "no Xauthority path"))?;

        let bytes = fs::read(path)?;
        let display_number = display_number.to_string();
        let mut cursor = 0;
        let mut fallback = None;

        while cursor < bytes.len() {
            let Some(_family) = read_be_u16_record(&bytes, &mut cursor) else {
                break;
            };
            let Some(address) = read_record_field(&bytes, &mut cursor) else {
                break;
            };
            let Some(number) = read_record_field(&bytes, &mut cursor) else {
                break;
            };
            let Some(name) = read_record_field(&bytes, &mut cursor) else {
                break;
            };
            let Some(data) = read_record_field(&bytes, &mut cursor) else {
                break;
            };

            if name == MIT_MAGIC_COOKIE.as_bytes() && number == display_number.as_bytes() {
                let auth = Self { name, data };
                if address.is_empty() {
                    return Ok(Some(auth));
                }
                fallback = Some(auth);
            }
        }

        Ok(fallback)
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct HostSetup {
    pub(super) resource_id_base: u32,
    pub(super) root: u32,
    pub(super) root_visual: u32,
    pub(super) root_depth: u8,
    pub(super) white_pixel: u32,
    pub(super) black_pixel: u32,
    /// First TrueColor visual at depth 32 with a non-zero alpha mask, if
    /// the host advertises one. Used to forward CreateWindow with our
    /// `ARGB_VISUAL` so the host produces an ARGB drawable instead of a
    /// 24-bit one. `None` = host has no ARGB visual; we fall back to
    /// CopyFromParent in that case.
    pub(super) argb_visual: Option<u32>,
}

/// Parse `DISPLAY` into `(host, display_number)`.
///
/// Examples:
/// * `:0` → `(None, 0)` — Unix-socket transport.
/// * `:10.0` → `(None, 10)` — Unix socket, screen 0 (stripped).
/// * `unix:0` → `(Some("unix"), 0)` — explicit Unix.
/// * `localhost:10.0` → `(Some("localhost"), 10)` — TCP at localhost:6010
///   (SSH X11-forwarding sets this).
fn parse_display(display: &str) -> io::Result<(Option<String>, u16)> {
    let (host_part, number_part) = match display.rsplit_once(':') {
        Some((h, n)) => (h, n),
        None => ("", display),
    };
    let number = number_part.split('.').next().unwrap_or(number_part);
    let display_number = number.parse::<u16>().map_err(|err| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("unsupported DISPLAY value {display:?}: {err}"),
        )
    })?;
    let host = if host_part.is_empty() {
        None
    } else {
        Some(host_part.to_string())
    };
    Ok((host, display_number))
}

fn write_setup_request(stream: &mut HostStream, auth: Option<&XAuthority>) -> io::Result<()> {
    let (name, data) = auth
        .map(|auth| (auth.name.as_slice(), auth.data.as_slice()))
        .unwrap_or((&[][..], &[][..]));

    let mut out = Vec::new();
    out.push(b'l');
    out.push(0);
    write_u16(&mut out, 11);
    write_u16(&mut out, 0);
    write_u16(&mut out, name.len() as u16);
    write_u16(&mut out, data.len() as u16);
    write_u16(&mut out, 0);
    out.extend_from_slice(name);
    pad4(&mut out);
    out.extend_from_slice(data);
    pad4(&mut out);
    stream.write_all(&out)
}

pub(super) fn read_setup_reply(stream: &mut HostStream) -> io::Result<HostSetup> {
    let mut header = [0; 8];
    stream.read_exact(&mut header)?;
    if header[0] != 1 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("host X11 setup failed with status {}", header[0]),
        ));
    }

    let length = u16::from_le_bytes([header[6], header[7]]) as usize * 4;
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    if body.len() < 40 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "host X11 setup body is too short",
        ));
    }

    let resource_id_base = read_u32(&body[4..8]);
    let vendor_len = read_u16(&body[16..18]) as usize;
    let roots_len = body[20] as usize;
    let pixmap_formats_len = body[21] as usize;
    if roots_len == 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "host X11 server has no roots",
        ));
    }

    let screen_offset = 32 + padded_len(vendor_len) + pixmap_formats_len * 8;
    if body.len() < screen_offset + 40 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "host X11 screen body is too short",
        ));
    }

    let screen = &body[screen_offset..];
    let allowed_depths_len = screen[39] as usize;
    let argb_visual = scan_for_argb_visual(&body, screen_offset + 40, allowed_depths_len);

    Ok(HostSetup {
        resource_id_base,
        root: read_u32(&screen[0..4]),
        root_visual: read_u32(&screen[32..36]),
        root_depth: screen[38],
        white_pixel: read_u32(&screen[8..12]),
        black_pixel: read_u32(&screen[12..16]),
        argb_visual,
    })
}

/// Walk the screen's depth-list looking for a depth-32 TrueColor visual
/// with a non-zero alpha mask. Each depth record is `depth(1) pad(1)
/// visuals_len(2) pad(4)` followed by `visuals_len * 24` bytes of
/// VisualType records (visual_id(4) class(1) bits_per_rgb(1)
/// colormap_entries(2) red(4) green(4) blue(4) pad(4)).
///
/// Returns `None` if the host has no such visual or the body is too
/// short to parse cleanly. `pad(4)` after the per-depth header is part
/// of the X11 protocol layout — see Protocol Reference, Setup section.
fn scan_for_argb_visual(body: &[u8], mut off: usize, depth_count: usize) -> Option<u32> {
    for _ in 0..depth_count {
        if body.len() < off + 8 {
            return None;
        }
        let depth = body[off];
        let visuals_len = read_u16(&body[off + 2..off + 4]) as usize;
        off += 8;
        for _ in 0..visuals_len {
            if body.len() < off + 24 {
                return None;
            }
            let visual_id = read_u32(&body[off..off + 4]);
            let class = body[off + 4];
            let red_mask = read_u32(&body[off + 8..off + 12]);
            let green_mask = read_u32(&body[off + 12..off + 16]);
            let blue_mask = read_u32(&body[off + 16..off + 20]);
            // Approximate "alpha mask present": for a 32-bit TrueColor
            // visual the host does not actually expose the alpha mask in
            // the setup reply (X11 only exposes R/G/B). We infer ARGB by
            // depth=32 + class=TrueColor + the standard 8-bit RGB layout.
            let standard_argb_layout =
                red_mask == 0x00ff_0000 && green_mask == 0x0000_ff00 && blue_mask == 0x0000_00ff;
            if depth == 32 && class == 4 && standard_argb_layout {
                return Some(visual_id);
            }
            off += 24;
        }
    }
    None
}

// Phase 6.3 Step 4: the helpers `select_pointer_events_on_container`
// and `select_keyboard_events` were removed when `HostInputPump`
// stopped owning a connection. The merged event-mask now lives on
// `HostX11Backend`'s `CONTAINER_EVENT_MASK` (set at CreateWindow time)
// and `update_host_event_mask` for everything past container init.

fn read_be_u16_record(bytes: &[u8], cursor: &mut usize) -> Option<u16> {
    let end = *cursor + 2;
    let value = u16::from_be_bytes(bytes.get(*cursor..end)?.try_into().ok()?);
    *cursor = end;
    Some(value)
}

fn read_record_field(bytes: &[u8], cursor: &mut usize) -> Option<Vec<u8>> {
    let len = read_be_u16_record(bytes, cursor)? as usize;
    let end = *cursor + len;
    let value = bytes.get(*cursor..end)?.to_vec();
    *cursor = end;
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::scan_for_argb_visual;

    /// `(visual_id, class, red_mask, green_mask, blue_mask)`.
    type VisualRec = (u32, u8, u32, u32, u32);
    type DepthRec<'a> = (u8, &'a [VisualRec]);

    /// Build a single-screen depth list with the given (depth, visuals)
    /// records. Returns the byte buffer + the offset at which the depth
    /// list starts (matches the layout `read_setup_reply` hands to
    /// `scan_for_argb_visual`).
    fn build_depth_list(records: &[DepthRec<'_>]) -> (Vec<u8>, usize) {
        let mut body = Vec::new();
        let depth_offset = 0;
        for (depth, visuals) in records {
            body.push(*depth);
            body.push(0); // pad
            body.extend_from_slice(&(visuals.len() as u16).to_le_bytes());
            body.extend_from_slice(&[0; 4]); // pad
            for (vid, class, red, green, blue) in *visuals {
                body.extend_from_slice(&vid.to_le_bytes());
                body.push(*class);
                body.push(8); // bits_per_rgb
                body.extend_from_slice(&256u16.to_le_bytes()); // colormap_entries
                body.extend_from_slice(&red.to_le_bytes());
                body.extend_from_slice(&green.to_le_bytes());
                body.extend_from_slice(&blue.to_le_bytes());
                body.extend_from_slice(&[0; 4]); // pad
            }
        }
        (body, depth_offset)
    }

    #[test]
    fn scan_for_argb_visual_picks_depth_32_truecolor_with_8bit_rgb() {
        let (body, off) = build_depth_list(&[
            (24, &[(0x21, 4, 0x00ff_0000, 0x0000_ff00, 0x0000_00ff)]),
            (32, &[(0x42, 4, 0x00ff_0000, 0x0000_ff00, 0x0000_00ff)]),
        ]);
        assert_eq!(scan_for_argb_visual(&body, off, 2), Some(0x42));
    }

    #[test]
    fn scan_for_argb_visual_skips_non_true_color_at_depth_32() {
        let (body, off) =
            build_depth_list(&[(32, &[(0x42, 5, 0x00ff_0000, 0x0000_ff00, 0x0000_00ff)])]);
        assert_eq!(scan_for_argb_visual(&body, off, 1), None);
    }

    #[test]
    fn scan_for_argb_visual_returns_none_when_no_depth_32() {
        let (body, off) =
            build_depth_list(&[(24, &[(0x21, 4, 0x00ff_0000, 0x0000_ff00, 0x0000_00ff)])]);
        assert_eq!(scan_for_argb_visual(&body, off, 1), None);
    }

    #[test]
    fn scan_for_argb_visual_returns_none_on_truncated_body() {
        // Header claims a depth-32 visual but the body cuts off early.
        let mut body = Vec::new();
        body.push(32);
        body.push(0);
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&[0; 4]);
        body.extend_from_slice(&[0; 12]); // less than the 24-byte VisualType
        assert_eq!(scan_for_argb_visual(&body, 0, 1), None);
    }
}
