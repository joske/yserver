//! The UI thread's half: one Win32 window per top-level, from the core's
//! events (spec §4.5.1). Window procedures only queue work for the shell
//! ([`on_ui`]): Win32 calls the shell makes send messages back into them.

use std::{
    cell::RefCell,
    collections::HashMap,
    ptr,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use windows_sys::Win32::{
    Foundation::{HWND, POINT, RECT},
    UI::{
        Input::KeyboardAndMouse::SetActiveWindow,
        WindowsAndMessaging::{
            DestroyWindow, GW_HWNDNEXT, GW_HWNDPREV, GWL_STYLE, GWLP_HWNDPARENT,
            GetForegroundWindow, GetTopWindow, GetWindow, HWND_BOTTOM, HWND_NOTOPMOST, HWND_TOP,
            HWND_TOPMOST, IDYES, IsIconic, IsZoomed, MB_ICONWARNING, MB_YESNO, MessageBoxW,
            PostMessageW, SC_MOVE, SC_SIZE, SW_HIDE, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE,
            SW_SHOWNA, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
            SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
            WINDOW_EX_STYLE, WINDOW_STYLE, WM_SYSCOMMAND, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW,
            WS_POPUP,
        },
    },
};
use yserver_core::rootless::{
    Geometry, GeometryFields, HostRequest, HostSerial, InfoFields, ResizeEdge, RootlessEvent,
    ToplevelId, ToplevelKind, WindowInfo, WmRequest,
};

use super::{
    HwndTable,
    window::{self, WindowData},
};
use crate::{
    rootless::{
        host::FrameEvent,
        native::{
            geometry::{GeometrySync, HostSerials},
            seat::{CloseAction, CloseTracker, FocusTracker, XStacking, restack_requests},
            space::{GlobalSpace, below_title_bar},
            style::{
                NativeFlags, NativeLevel, NetStateSync, content_limits, level_for, native_style,
            },
        },
    },
    surface::{link::ShellLink, ui::CursorImage},
    windows::{input::ShellInput, scale},
};

/// The shell's queued work is waiting (`on_ui`).
pub(super) const WM_RUN: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 1;

thread_local! {
    static SHELL: RefCell<Option<RootlessShell>> = const { RefCell::new(None) };
    /// The shell's input, for the window procedures while the shell is busy.
    static INPUT: RefCell<Option<Rc<ShellInput>>> = const { RefCell::new(None) };
}

type Job = Box<dyn FnOnce(&mut RootlessShell) + Send>;

/// Work for the shell, from any thread.
static JOBS: Mutex<Vec<Job>> = Mutex::new(Vec::new());
/// The message window `WM_RUN` goes to.
static MESSAGE_WINDOW: AtomicUsize = AtomicUsize::new(0);

/// Set up the shell on the UI thread.
pub(super) fn install(
    space: GlobalSpace,
    factor: u32,
    link: Arc<ShellLink>,
    hwnds: Arc<HwndTable>,
    input: Rc<ShellInput>,
    message_window: HWND,
) {
    MESSAGE_WINDOW.store(message_window as usize, Ordering::Release);
    INPUT.set(Some(Rc::clone(&input)));
    SHELL.set(Some(RootlessShell {
        space,
        factor,
        windows: HashMap::new(),
        link,
        hwnds,
        input,
        focus: FocusTracker::default(),
        serials: HostSerials::default(),
        stacking: XStacking::default(),
    }));
}

/// Drop the shell (the message loop ended).
pub(super) fn uninstall() {
    SHELL.set(None);
    INPUT.set(None);
}

/// The shell's input.
pub(super) fn input() -> Option<Rc<ShellInput>> {
    INPUT.with_borrow(Clone::clone)
}

/// Run `f` on the shell now, unless the shell is busy (a window procedure
/// called from inside one of its Win32 calls).
pub(super) fn now<R>(f: impl FnOnce(&mut RootlessShell) -> R) -> Option<R> {
    SHELL.with(|shell| {
        let mut shell = shell.try_borrow_mut().ok()?;
        shell.as_mut().map(f)
    })
}

/// Run `f` on the shell, from the UI thread's message loop.
pub(super) fn on_ui(f: impl FnOnce(&mut RootlessShell) + Send + 'static) {
    JOBS.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(Box::new(f));
    let hwnd = MESSAGE_WINDOW.load(Ordering::Acquire);
    // SAFETY: posting is thread-safe; a destroyed window just fails.
    unsafe { PostMessageW(hwnd as HWND, WM_RUN, 0, 0) };
}

/// `WM_RUN`: run the queued work; while the shell is busy it waits for
/// another turn.
pub(super) fn run_jobs() {
    let jobs = std::mem::take(
        &mut *JOBS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    let mut jobs = jobs.into_iter();
    for job in jobs.by_ref() {
        let mut job = Some(job);
        let ran = now(|shell| (job.take().expect("not yet run"))(shell));
        if ran.is_none() {
            if let Some(job) = job {
                let rest: Vec<Job> = std::iter::once(job).chain(jobs).collect();
                let mut queued = JOBS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let later = std::mem::replace(&mut *queued, rest);
                queued.extend(later);
                drop(queued);
                let hwnd = MESSAGE_WINDOW.load(Ordering::Acquire);
                // SAFETY: as in `on_ui`.
                unsafe { PostMessageW(hwnd as HWND, WM_RUN, 0, 0) };
            }
            return;
        }
    }
}

/// A borderless window's place before it went fullscreen.
#[derive(Clone, Copy)]
struct Windowed {
    style: WINDOW_STYLE,
    frame: RECT,
}

/// One top-level's native window.
struct NativeWindow {
    hwnd: HWND,
    kind: ToplevelKind,
    info: WindowInfo,
    border: u16,
    /// X's geometry, against which native changes are reported.
    sync: GeometrySync,
    /// `_NET_WM_STATE` as last reported.
    net: NetStateSync,
    close: CloseTracker,
    /// Fullscreen: the window it was.
    fullscreen: Option<Windowed>,
    /// Put at the bottom for `_NET_WM_STATE_BELOW`; Win32 has no level
    /// under the normal one to read back.
    below: bool,
    /// The transient parent that owns this window.
    owner: Option<ToplevelId>,
    /// A Managed window without a transient parent: its first frame
    /// brings it to the foreground.
    activates: bool,
}

pub(super) struct RootlessShell {
    space: GlobalSpace,
    /// Device pixels per X pixel per axis, everywhere (one global scale).
    factor: u32,
    windows: HashMap<ToplevelId, NativeWindow>,
    link: Arc<ShellLink>,
    hwnds: Arc<HwndTable>,
    input: Rc<ShellInput>,
    focus: FocusTracker,
    serials: HostSerials,
    /// X's stacking of our windows (§6.4).
    stacking: XStacking,
}

/// A force-kill to confirm (decision 12), asked outside the shell.
pub(super) struct KillAsk {
    id: ToplevelId,
    hwnd: HWND,
    title: String,
    deletes: bool,
    link: Arc<ShellLink>,
}

impl KillAsk {
    /// Ask the user; on Yes, the only request that kills.
    pub(super) fn run(self) {
        let why = if self.deletes {
            "It has not closed the window."
        } else {
            "It does not support closing a window."
        };
        let text: Vec<u16> = format!(
            "Kill the X client of \"{}\"?\n\n{why} Killing it closes all of its windows.",
            self.title
        )
        .encode_utf16()
        .chain([0])
        .collect();
        // SAFETY: a live window and NUL-terminated strings; the modal loop
        // re-enters the window procedures, which only queue work.
        let answer = unsafe {
            MessageBoxW(
                self.hwnd,
                text.as_ptr(),
                windows_sys::core::w!("yserver"),
                MB_YESNO | MB_ICONWARNING,
            )
        };
        if answer == IDYES {
            self.link.request(HostRequest::ForceKill { id: self.id });
        }
    }
}

/// X content + border on both sides.
fn outer_extent(g: &Geometry) -> (u16, u16) {
    (
        g.width.saturating_add(g.border.saturating_mul(2)),
        g.height.saturating_add(g.border.saturating_mul(2)),
    )
}

/// `rect`'s top-left corner.
fn top_left(rect: RECT) -> POINT {
    POINT {
        x: rect.left,
        y: rect.top,
    }
}

/// `WM_NORMAL_HINTS` as the outer size limits of `hwnd`, whose X pixels
/// are `factor`x`factor` device pixels; its client size snaps to whole X
/// pixels.
fn apply_limits(hwnd: HWND, info: &WindowInfo, border: u16, factor: u32) {
    let Some(data) = window::data(hwnd) else {
        return;
    };
    let (ws, ex) = window::styles(hwnd);
    let dpi = scale::window_dpi(hwnd);
    let limits = content_limits(&info.size_hints, border);
    let factor = i32::try_from(factor.max(1)).unwrap_or(1);
    let outer = |(w, h): (u16, u16)| {
        let frame = window::frame_rect(
            RECT {
                left: 0,
                top: 0,
                right: i32::from(w).saturating_mul(factor),
                bottom: i32::from(h).saturating_mul(factor),
            },
            ws,
            ex,
            dpi,
        );
        (frame.right - frame.left, frame.bottom - frame.top)
    };
    data.min.set(limits.min.map(outer));
    data.max.set(limits.max.map(outer));
    let hints = &info.size_hints;
    let side = |v: u32| i32::try_from(v).unwrap_or(i32::MAX).saturating_mul(factor);
    let bw = 2 * i32::from(border) * factor;
    let increments = hints.inc.map(|(w, h)| {
        let (base_w, base_h) = hints.base.or(hints.min).unwrap_or((0, 0));
        window::Increments {
            base: (
                side(base_w).saturating_add(bw),
                side(base_h).saturating_add(bw),
            ),
            inc: (side(w), side(h)),
        }
    });
    data.increments
        .set(increments.or((factor > 1).then_some(window::Increments {
            base: (0, 0),
            inc: (factor, factor),
        })));
}

/// `content` moved down so a title bar of `ws`/`ex` stays below the top of
/// the work area of its monitor (XQuartz, spec §4.0).
fn below_title(content: RECT, ws: WINDOW_STYLE, ex: WINDOW_EX_STYLE) -> RECT {
    let dpi = scale::dpi_at(content);
    let title_bar = content.top - window::frame_rect(content, ws, ex, dpi).top;
    let Some((_, work)) = window::monitor_at(top_left(content)) else {
        return content;
    };
    window::screen(below_title_bar(
        window::native(content),
        f64::from(title_bar),
        window::native(work),
    ))
}

/// Set `hwnd`'s client area to `content`, keeping its place in the order.
fn set_content(hwnd: HWND, content: RECT) {
    let (ws, ex) = window::styles(hwnd);
    let frame = window::frame_rect(content, ws, ex, scale::window_dpi(hwnd));
    // SAFETY: our window; `WM_WINDOWPOSCHANGED` only queues work.
    unsafe {
        SetWindowPos(
            hwnd,
            ptr::null_mut(),
            frame.left,
            frame.top,
            frame.right - frame.left,
            frame.bottom - frame.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

fn set_title(hwnd: HWND, title: &str) {
    let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
    // SAFETY: a live window and a NUL-terminated string.
    unsafe { SetWindowTextW(hwnd, title.as_ptr()) };
}

/// `WMSZ_*` of an X resize edge, for `SC_SIZE`.
fn size_edge(edge: ResizeEdge) -> usize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        WMSZ_BOTTOM, WMSZ_BOTTOMLEFT, WMSZ_BOTTOMRIGHT, WMSZ_LEFT, WMSZ_RIGHT, WMSZ_TOP,
        WMSZ_TOPLEFT, WMSZ_TOPRIGHT,
    };
    (match edge {
        ResizeEdge::TopLeft => WMSZ_TOPLEFT,
        ResizeEdge::Top => WMSZ_TOP,
        ResizeEdge::TopRight => WMSZ_TOPRIGHT,
        ResizeEdge::Right => WMSZ_RIGHT,
        ResizeEdge::BottomRight => WMSZ_BOTTOMRIGHT,
        ResizeEdge::Bottom => WMSZ_BOTTOM,
        ResizeEdge::BottomLeft => WMSZ_BOTTOMLEFT,
        ResizeEdge::Left => WMSZ_LEFT,
    }) as usize
}

impl RootlessShell {
    pub(super) fn apply(&mut self, events: &[RootlessEvent]) {
        for event in events {
            match event {
                RootlessEvent::Mapped {
                    id,
                    kind,
                    geometry: g,
                    depth,
                    info,
                    above,
                    ..
                } => {
                    self.stacking.place(*id, *above);
                    self.mapped(*id, *kind, g, *depth, info);
                }
                // `surface_released` frees it.
                RootlessEvent::Unmapped { id } => {
                    self.stacking.remove(*id);
                    if let Some(w) = self.windows.get(id) {
                        // SAFETY: our window.
                        unsafe { ShowWindow(w.hwnd, SW_HIDE) };
                    }
                }
                RootlessEvent::Restacked {
                    id,
                    above,
                    from_client,
                    ..
                } => {
                    self.stacking.place(*id, *above);
                    if *from_client {
                        self.order_natively(*id, *above);
                    }
                }
                RootlessEvent::InfoChanged { id, info, changed } => {
                    self.info_changed(*id, info, *changed);
                }
                RootlessEvent::Configured {
                    id,
                    geometry,
                    acked,
                    client_fields,
                } => self.configured(*id, geometry, *acked, *client_fields),
                RootlessEvent::WmRequest { id, request } => self.wm_request(*id, *request),
                // A popup is topmost; no owner relation.
                _ => {}
            }
        }
    }

    /// Create the window cloaked and set up; the first frame shows it.
    fn mapped(
        &mut self,
        id: ToplevelId,
        kind: ToplevelKind,
        g: &Geometry,
        _depth: u8,
        info: &WindowInfo,
    ) {
        let extent = outer_extent(g);
        let mut content = window::screen(self.space.to_native((g.x, g.y), extent));
        let style = native_style(kind, info);
        let (ws, ex) = window::window_style(&style);
        if style.titled {
            content = below_title(content, ws, ex);
        }
        // §6.1: the host places a window nobody positioned.
        let hints = &info.size_hints;
        let place = kind == ToplevelKind::Managed
            && style.titled
            && !hints.us_position
            && !hints.p_position
            && info.transient_for.is_none();
        let owner = style
            .child_of
            .and_then(|parent| self.windows.get(&parent))
            .map(|w| w.hwnd);
        let data = WindowData {
            id,
            popup: style.level == NativeLevel::PopUp,
            shown: std::cell::Cell::new(false),
            tracking: std::cell::Cell::new(false),
            min: std::cell::Cell::new(None),
            max: std::cell::Cell::new(None),
            increments: std::cell::Cell::new(None),
        };
        let hwnd = match window::create(&style, content, &info.title, data, owner, place) {
            Ok(hwnd) => hwnd,
            Err(e) => {
                log::warn!("windows: no window for {id:?}: {e}");
                return;
            }
        };
        if style.titled {
            apply_limits(hwnd, info, g.border, self.factor);
        }
        if style.level == NativeLevel::Below {
            // SAFETY: our window.
            unsafe {
                SetWindowPos(
                    hwnd,
                    HWND_BOTTOM,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        // Shown cloaked: a present to a hidden window may not reach the
        // screen (spec §4.5.1).
        // SAFETY: our window.
        unsafe { ShowWindow(hwnd, SW_SHOWNA) };
        // Where the content really is (ours above, or the system's own
        // placement): X follows it.
        let mut sync = GeometrySync::mapped(g);
        let (origin, actual) = self.space.to_x(window::native(window::content_rect(hwnd)));
        for request in sync.native_changed(id, origin, actual, &mut self.serials) {
            log::debug!("windows: {id:?} placed natively: {request:?}");
            self.link.request(request);
        }
        self.hwnds.insert(id, hwnd);
        self.windows.insert(
            id,
            NativeWindow {
                hwnd,
                kind,
                info: info.clone(),
                border: g.border,
                sync,
                net: NetStateSync::mapped(info.state),
                close: CloseTracker::default(),
                fullscreen: None,
                below: style.level == NativeLevel::Below,
                owner: owner.and(style.child_of),
                activates: kind == ToplevelKind::Managed && info.transient_for.is_none(),
            },
        );
        self.link.post_frame(FrameEvent::SetupDone { id });
    }

    /// X restacked `id` directly above `above` (`None`: the bottom of ours).
    fn order_natively(&self, id: ToplevelId, above: Option<ToplevelId>) {
        let Some(w) = self.windows.get(&id) else {
            return;
        };
        let after = match above.and_then(|a| self.windows.get(&a)) {
            Some(other) => {
                // SAFETY: a plain query of our window.
                let over = unsafe { GetWindow(other.hwnd, GW_HWNDPREV) };
                if over == w.hwnd {
                    return;
                }
                if over.is_null() { HWND_TOP } else { over }
            }
            None => {
                let lowest = self
                    .stacking
                    .bottom_to_top()
                    .iter()
                    .filter(|&&other| other != id)
                    .find_map(|other| self.windows.get(other));
                match lowest {
                    Some(lowest) => lowest.hwnd,
                    None => return,
                }
            }
        };
        // SAFETY: our window; `after` is a live window or `HWND_TOP`.
        unsafe {
            SetWindowPos(
                w.hwnd,
                after,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    /// The native order of our Managed windows is authoritative (§6.4):
    /// restack X to match.
    pub(super) fn sync_stacking(&mut self) {
        let managed = |w: &NativeWindow| w.kind == ToplevelKind::Managed;
        let mut native = Vec::new();
        // SAFETY: plain queries, top to bottom of the desktop's order.
        let mut hwnd = unsafe { GetTopWindow(ptr::null_mut()) };
        while !hwnd.is_null() {
            if let Some((id, _)) = self
                .windows
                .iter()
                .find(|(_, w)| w.hwnd == hwnd && managed(w))
            {
                native.push(*id);
            }
            // SAFETY: as above.
            hwnd = unsafe { GetWindow(hwnd, GW_HWNDNEXT) };
        }
        let x: Vec<ToplevelId> = self
            .stacking
            .bottom_to_top()
            .iter()
            .copied()
            .filter(|id| self.windows.get(id).is_some_and(managed))
            .collect();
        for (id, above) in restack_requests(&native, &x) {
            self.stacking.place(id, above);
            let serial = self.serials.next();
            log::debug!("windows: restack {id:?} above {above:?}");
            self.link
                .request(HostRequest::Restacked { id, above, serial });
        }
    }

    /// The first frame is on screen: show the window. A new Managed window
    /// without a transient parent comes to the foreground; transients and
    /// popups follow their app.
    pub(super) fn first_frame(&mut self, id: ToplevelId) {
        let Some(w) = self.windows.get(&id) else {
            return;
        };
        if let Some(data) = window::data(w.hwnd) {
            data.shown.set(true);
        }
        window::set_cloaked(w.hwnd, false);
        if w.kind != ToplevelKind::Managed {
            return;
        }
        let (hwnd, activates) = (w.hwnd, w.activates);
        // The state the client asked for before the map.
        let state = w.info.state;
        if state.fullscreen && native_style(w.kind, &w.info).fullscreen {
            self.set_fullscreen(id, true);
        } else if state.maximized_vert && state.maximized_horz {
            // SAFETY: our window.
            unsafe { ShowWindow(hwnd, SW_MAXIMIZE) };
        }
        log::debug!("windows: {id:?} shown");
        // SAFETY: our window. Windows may refuse the foreground, then the
        // taskbar flashes it; a transient is only activated within ours.
        unsafe {
            if activates {
                SetForegroundWindow(hwnd);
            } else {
                SetActiveWindow(hwnd);
            }
        }
    }

    fn info_changed(&mut self, id: ToplevelId, info: &WindowInfo, changed: InfoFields) {
        let Some(w) = self.windows.get_mut(&id) else {
            return;
        };
        if changed.contains(InfoFields::TITLE) {
            set_title(w.hwnd, &info.title);
        }
        if changed.contains(InfoFields::SIZE_HINTS) && native_style(w.kind, info).titled {
            apply_limits(w.hwnd, info, w.border, self.factor);
        }
        if changed.contains(InfoFields::STATE) {
            w.net.client_state(info.state);
        }
        w.info = info.clone();
        if changed.contains(InfoFields::TRANSIENT_FOR) {
            let parent = native_style(w.kind, info).child_of;
            if parent != w.owner {
                let hwnd = w.hwnd;
                let owner = parent.and_then(|p| self.windows.get(&p)).map(|p| p.hwnd);
                // SAFETY: our window; the owner is ours and live (or none).
                unsafe {
                    SetWindowLongPtrW(
                        hwnd,
                        GWLP_HWNDPARENT,
                        owner.unwrap_or(ptr::null_mut()) as isize,
                    );
                }
                if let Some(w) = self.windows.get_mut(&id) {
                    w.owner = owner.and(parent);
                }
            }
        }
    }

    /// §4.2.1 rule 4: apply exactly the client's fields natively.
    fn configured(
        &mut self,
        id: ToplevelId,
        g: &Geometry,
        acked: Option<HostSerial>,
        client_fields: GeometryFields,
    ) {
        let space = self.space;
        let Some(w) = self.windows.get_mut(&id) else {
            return;
        };
        w.border = g.border;
        if w.sync.configured(g, acked, client_fields).is_none() {
            return;
        }
        let (origin, extent) = w.sync.x();
        // Maximized, minimized or fullscreen, the system keeps the window
        // where it is; a client snapping to its size increments would fight
        // it for good. The restored window reports itself.
        // SAFETY: plain queries of our window.
        if w.fullscreen.is_some() || unsafe { IsZoomed(w.hwnd) != 0 || IsIconic(w.hwnd) != 0 } {
            log::debug!("windows: {id:?} keeps its native place against X's {origin:?} {extent:?}");
            return;
        }
        let mut content = window::screen(space.to_native(origin, extent));
        let (ws, ex) = window::styles(w.hwnd);
        if native_style(w.kind, &w.info).titled {
            content = below_title(content, ws, ex);
        }
        log::debug!("windows: {id:?} configured by X to {origin:?} {extent:?}");
        // `WM_WINDOWPOSCHANGED` reports any difference (title bar, the
        // system's limits) back to X.
        set_content(w.hwnd, content);
    }

    /// The native window moved or changed size (a drag, a resize, a
    /// maximize, our own `SetWindowPos`): X follows; equal to what X has is
    /// an echo. A minimized window keeps its X geometry.
    pub(super) fn frame_changed(&mut self, id: ToplevelId) {
        let space = self.space;
        let Some(w) = self.windows.get_mut(&id) else {
            return;
        };
        // SAFETY: a plain query of our window.
        if unsafe { IsIconic(w.hwnd) } == 0 {
            let (origin, extent) = space.to_x(window::native(window::content_rect(w.hwnd)));
            for request in w.sync.native_changed(id, origin, extent, &mut self.serials) {
                log::debug!("windows: {id:?} moved natively: {request:?}");
                self.link.request(request);
            }
        }
        self.report_state(id);
    }

    /// Read the window's state and report it when it differs.
    pub(super) fn report_state(&mut self, id: ToplevelId) {
        let Some(w) = self.windows.get_mut(&id) else {
            return;
        };
        let (_, ex) = window::styles(w.hwnd);
        let level = if w.kind != ToplevelKind::Managed {
            NativeLevel::PopUp
        } else if ex & WS_EX_TOPMOST != 0 {
            NativeLevel::Floating
        } else if w.below {
            NativeLevel::Below
        } else {
            NativeLevel::Normal
        };
        let flags = NativeFlags {
            // SAFETY: plain queries of our window.
            zoomed: w.fullscreen.is_none() && unsafe { IsZoomed(w.hwnd) } != 0,
            fullscreen: w.fullscreen.is_some(),
            // SAFETY: as above.
            minimized: unsafe { IsIconic(w.hwnd) } != 0,
            level,
        };
        if let Some(request) = w.net.native(id, flags) {
            log::debug!("windows: {id:?} state {request:?}");
            self.link.request(request);
        }
    }

    /// Fill the monitor without a frame, or go back to the window it was.
    fn set_fullscreen(&mut self, id: ToplevelId, on: bool) {
        let Some(w) = self.windows.get_mut(&id) else {
            return;
        };
        let hwnd = w.hwnd;
        match (on, w.fullscreen) {
            (true, None) => {
                let (style, ex) = window::styles(hwnd);
                let content = window::content_rect(hwnd);
                let Some((monitor, _)) = window::monitor_at(top_left(content)) else {
                    return;
                };
                w.fullscreen = Some(Windowed {
                    style,
                    frame: window::frame_rect(content, style, ex, scale::window_dpi(hwnd)),
                });
                // SAFETY: our window.
                unsafe {
                    if IsZoomed(hwnd) != 0 {
                        ShowWindow(hwnd, SW_RESTORE);
                    }
                    // Without the frame; `WS_VISIBLE` stays.
                    SetWindowLongPtrW(
                        hwnd,
                        GWL_STYLE,
                        ((style & !WS_OVERLAPPEDWINDOW) | WS_POPUP) as isize,
                    );
                    SetWindowPos(
                        hwnd,
                        HWND_TOP,
                        monitor.left,
                        monitor.top,
                        monitor.right - monitor.left,
                        monitor.bottom - monitor.top,
                        SWP_FRAMECHANGED,
                    );
                }
            }
            (false, Some(windowed)) => {
                w.fullscreen = None;
                let frame = windowed.frame;
                // SAFETY: our window.
                unsafe {
                    SetWindowLongPtrW(hwnd, GWL_STYLE, windowed.style as isize);
                    SetWindowPos(
                        hwnd,
                        ptr::null_mut(),
                        frame.left,
                        frame.top,
                        frame.right - frame.left,
                        frame.bottom - frame.top,
                        SWP_FRAMECHANGED | SWP_NOZORDER,
                    );
                }
            }
            _ => {}
        }
        self.report_state(id);
    }

    /// The close button: ask the client, the second time offer to kill it
    /// (decision 12); the kill is asked for outside the shell.
    pub(super) fn close_clicked(&mut self, id: ToplevelId) -> Option<KillAsk> {
        let w = self.windows.get_mut(&id)?;
        match w.close.clicked() {
            CloseAction::Request => {
                self.link.request(HostRequest::CloseRequested { id });
                None
            }
            CloseAction::ConfirmKill => Some(KillAsk {
                id,
                hwnd: w.hwnd,
                title: w.info.title.clone(),
                deletes: w.info.protocols.delete_window,
                link: Arc::clone(&self.link),
            }),
        }
    }

    /// A client asked the WM (§6.3); every state change comes back through
    /// `report_state`, never optimistically.
    fn wm_request(&mut self, id: ToplevelId, request: WmRequest) {
        let Some(w) = self.windows.get(&id) else {
            return;
        };
        let hwnd = w.hwnd;
        log::debug!("windows: {id:?} asks {request:?}");
        // The modal move and size loops run from the window procedure,
        // outside the shell.
        let post_command = |command: usize| {
            // SAFETY: our window; `DefWindowProcW` runs the loop.
            unsafe { PostMessageW(hwnd, WM_SYSCOMMAND, command, 0) };
        };
        match request {
            WmRequest::Activate => {
                // SAFETY: our window.
                unsafe {
                    if IsIconic(hwnd) != 0 {
                        ShowWindow(hwnd, SW_RESTORE);
                    }
                    SetForegroundWindow(hwnd);
                }
            }
            // As the close button's first click; never a kill.
            WmRequest::Close => self.link.request(HostRequest::CloseRequested { id }),
            WmRequest::Minimize => {
                // SAFETY: our window.
                unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
            }
            WmRequest::SetFullscreen(on) => self.set_fullscreen(id, on),
            WmRequest::SetMaximized(on) => {
                // SAFETY: our window.
                if on != (unsafe { IsZoomed(hwnd) } != 0) {
                    // SAFETY: our window.
                    unsafe { ShowWindow(hwnd, if on { SW_MAXIMIZE } else { SW_RESTORE }) };
                }
                self.report_state(id);
            }
            WmRequest::SetLayer(layer) => {
                let level = level_for(layer);
                let after = if level == NativeLevel::Floating {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                };
                // SAFETY: our window.
                unsafe {
                    SetWindowPos(
                        hwnd,
                        after,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                    if level == NativeLevel::Below {
                        SetWindowPos(
                            hwnd,
                            HWND_BOTTOM,
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                        );
                    }
                }
                if let Some(w) = self.windows.get_mut(&id) {
                    w.below = level == NativeLevel::Below;
                }
                self.report_state(id);
            }
            // `SC_MOVE`/`SC_SIZE` + 2 (`HTCAPTION`) or an edge: the mouse
            // drives the loop; plain, the keyboard does.
            WmRequest::BeginMove { button } => {
                self.input.release_capture();
                post_command(SC_MOVE as usize + if button == 0 { 0 } else { 2 });
            }
            WmRequest::BeginResize { edge, button } => {
                self.input.release_capture();
                post_command(SC_SIZE as usize + if button == 0 { 0 } else { size_edge(edge) });
            }
        }
    }

    /// `id`'s window is at another DPI: its outer limits take the new
    /// frame.
    pub(super) fn dpi_changed(&mut self, id: ToplevelId) {
        let Some(w) = self.windows.get(&id) else {
            return;
        };
        if native_style(w.kind, &w.info).titled {
            apply_limits(w.hwnd, &w.info, w.border, self.factor);
        }
    }

    /// The core's cursor.
    pub(super) fn set_cursor(&mut self, image: Option<CursorImage>) {
        let inside = self.input.pointer_inside();
        crate::windows::cursor::set_global(image.as_ref(), inside);
    }

    /// The core warped the pointer to root pixel `(x, y)`.
    pub(super) fn warp(&mut self, x: i32, y: i32) {
        let (sx, sy) = self.space.to_warp_point((x, y));
        log::debug!("windows: warp to root {x},{y} = screen {sx},{sy}");
        #[allow(clippy::cast_possible_truncation)]
        crate::windows::cursor::warp_screen(sx.round() as i32, sy.round() as i32);
    }

    /// Top-level `id`'s window was activated.
    pub(super) fn became_key(&mut self, id: ToplevelId) {
        if !self.windows.contains_key(&id) {
            return;
        }
        if let Some(request) = self.focus.became_key(id) {
            self.link.request(request);
        }
        self.input.focus_gained();
        self.sync_stacking();
    }

    /// A window was deactivated: decide on the next turn, when the new
    /// foreground window (ours or none) is known.
    pub(super) fn resigned_key(&mut self) {
        self.focus.resigned_key();
        on_ui(Self::settle);
    }

    fn settle(&mut self) {
        // SAFETY: a plain query.
        let foreground = unsafe { GetForegroundWindow() };
        let ours = self
            .windows
            .iter()
            .find(|(_, w)| w.hwnd == foreground && w.kind == ToplevelKind::Managed)
            .map(|(id, _)| *id);
        if let Some(request) = self.focus.settle(ours) {
            self.link.request(request);
        }
    }

    /// Another application became active: X loses focus, keys are released.
    pub(super) fn app_deactivated(&mut self) {
        if let Some(request) = self.focus.app_deactivated() {
            self.link.request(request);
        }
        self.input.focus_lost();
    }

    /// The core is done with `id`'s surface: free the window.
    pub(super) fn release(&mut self, id: ToplevelId) {
        let Some(w) = self.windows.remove(&id) else {
            return;
        };
        // Windows destroys owned windows with their owner; each of ours
        // goes at its own release.
        for owned in self.windows.values_mut().filter(|o| o.owner == Some(id)) {
            owned.owner = None;
            // SAFETY: our window.
            unsafe { SetWindowLongPtrW(owned.hwnd, GWLP_HWNDPARENT, 0) };
        }
        // SAFETY: our window.
        unsafe { DestroyWindow(w.hwnd) };
    }
}
