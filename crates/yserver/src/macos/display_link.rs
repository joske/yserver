//! A CVDisplayLink ticking the surface's frame clock with the display's
//! refresh host time; its callback runs on the display link's own thread.

use std::{ffi::c_void, io, ptr, sync::Arc};

use crate::surface::clock::FrameClock;

type CVDisplayLinkRef = *mut c_void;
type CVReturn = i32;
type CVOptionFlags = u64;

/// `kCVTimeIsIndefinite`.
const CV_TIME_IS_INDEFINITE: i32 = 1;

#[repr(C)]
#[allow(dead_code)] // FFI layout only.
struct CVSMPTETime {
    subframes: i16,
    subframe_divisor: i16,
    counter: u32,
    kind: u32,
    flags: u32,
    hours: i16,
    minutes: i16,
    seconds: i16,
    frames: i16,
}

#[repr(C)]
#[allow(dead_code)] // FFI layout; only `host_time` is read.
struct CVTimeStamp {
    version: u32,
    video_time_scale: i32,
    video_time: i64,
    host_time: u64,
    rate_scalar: f64,
    video_refresh_period: i64,
    smpte_time: CVSMPTETime,
    flags: u64,
    reserved: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CVTime {
    time_value: i64,
    time_scale: i32,
    flags: i32,
}

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

type OutputCallback = unsafe extern "C" fn(
    CVDisplayLinkRef,
    *const CVTimeStamp,
    *const CVTimeStamp,
    CVOptionFlags,
    *mut CVOptionFlags,
    *mut c_void,
) -> CVReturn;

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVDisplayLinkCreateWithActiveCGDisplays(link: *mut CVDisplayLinkRef) -> CVReturn;
    fn CVDisplayLinkSetOutputCallback(
        link: CVDisplayLinkRef,
        callback: OutputCallback,
        user_info: *mut c_void,
    ) -> CVReturn;
    fn CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link: CVDisplayLinkRef) -> CVTime;
    fn CVDisplayLinkStart(link: CVDisplayLinkRef) -> CVReturn;
    fn CVDisplayLinkStop(link: CVDisplayLinkRef) -> CVReturn;
    fn CVDisplayLinkRelease(link: CVDisplayLinkRef);
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

/// What the callback reads: the clock and the host-time → ns ratio.
struct Context {
    clock: Arc<FrameClock>,
    numer: u64,
    denom: u64,
}

/// A running display link; stopped and released on drop.
pub(super) struct DisplayLink {
    link: CVDisplayLinkRef,
    context: *mut Context,
}

impl DisplayLink {
    /// Tick `clock` on every refresh of the active displays.
    ///
    /// # Errors
    ///
    /// CoreVideo could not create or start the link.
    pub(super) fn start(clock: Arc<FrameClock>) -> io::Result<Self> {
        let mut timebase = MachTimebaseInfo { numer: 1, denom: 1 };
        // SAFETY: writes the out-parameter.
        if unsafe { mach_timebase_info(&raw mut timebase) } != 0 || timebase.denom == 0 {
            timebase = MachTimebaseInfo { numer: 1, denom: 1 };
        }
        let mut link: CVDisplayLinkRef = ptr::null_mut();
        // SAFETY: writes the out-parameter.
        let created = unsafe { CVDisplayLinkCreateWithActiveCGDisplays(&raw mut link) };
        if created != 0 || link.is_null() {
            return Err(io::Error::other(format!(
                "CVDisplayLinkCreateWithActiveCGDisplays: {created}"
            )));
        }
        let context = Box::into_raw(Box::new(Context {
            clock: Arc::clone(&clock),
            numer: u64::from(timebase.numer),
            denom: u64::from(timebase.denom),
        }));
        // From here `Drop` releases the link and the context.
        let this = Self { link, context };
        // SAFETY: `context` lives until `Drop`, after the link stopped.
        let set = unsafe { CVDisplayLinkSetOutputCallback(link, tick, context.cast()) };
        if set != 0 {
            return Err(io::Error::other(format!(
                "CVDisplayLinkSetOutputCallback: {set}"
            )));
        }
        // SAFETY: a live link.
        let period = unsafe { CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link) };
        if period.flags & CV_TIME_IS_INDEFINITE == 0 && period.time_value > 0 {
            let mhz = i64::from(period.time_scale) * 1000 / period.time_value;
            clock.set_refresh_mhz(u32::try_from(mhz).unwrap_or(0));
        }
        // SAFETY: a live link with its callback set.
        let started = unsafe { CVDisplayLinkStart(link) };
        if started != 0 {
            return Err(io::Error::other(format!("CVDisplayLinkStart: {started}")));
        }
        log::info!("macos: display link at {} Hz", clock.refresh_hz());
        Ok(this)
    }
}

impl Drop for DisplayLink {
    fn drop(&mut self) {
        // SAFETY: stopping waits out a running callback; then nothing reads
        // the context.
        unsafe {
            CVDisplayLinkStop(self.link);
            CVDisplayLinkRelease(self.link);
            drop(Box::from_raw(self.context));
        }
    }
}

/// The display link's output callback: one tick at the refresh's host time.
unsafe extern "C" fn tick(
    _link: CVDisplayLinkRef,
    now: *const CVTimeStamp,
    _output_time: *const CVTimeStamp,
    _flags_in: CVOptionFlags,
    _flags_out: *mut CVOptionFlags,
    context: *mut c_void,
) -> CVReturn {
    // SAFETY: `context` is the `Context` `start` registered, alive until the
    // link stops; `now` is valid for the call.
    let (context, host_time) = unsafe { (&*context.cast::<Context>(), (*now).host_time) };
    let ns = u128::from(host_time) * u128::from(context.numer) / u128::from(context.denom);
    let ust_us = u64::try_from(ns / 1000).unwrap_or(u64::MAX);
    context.clock.tick(ust_us);
    0
}
