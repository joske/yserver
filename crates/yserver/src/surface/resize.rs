//! Which window sizes a shell reports to the core: every size outside a
//! live resize, at most one per frame-clock tick during one, and the final
//! size when it ends. Also the `--scale` arithmetic between points, X
//! pixels and backing pixels.

use crate::launch::ScaleOption;

/// A size in X pixels.
pub(crate) type Size = (u16, u16);

/// The smallest screen (and window content) offered.
pub(crate) const MIN_SCREEN: Size = (320, 200);

/// `backing` (backing pixels per point), or 1 when the window system gave
/// no usable value.
fn usable_backing(backing: f64) -> f64 {
    if backing.is_finite() && backing > 0.0 {
        backing
    } else {
        1.0
    }
}

/// Backing pixels per X pixel per axis (the slot → swapchain copy factor)
/// for `option` on a window of `backing` backing pixels per point.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Within 1..=8.
pub(crate) fn copy_factor(option: ScaleOption, backing: f64) -> u32 {
    match option {
        ScaleOption::Factor(factor) => factor.max(1),
        ScaleOption::Auto => usable_backing(backing).round().clamp(1.0, 8.0) as u32,
    }
}

/// X pixels per point for `option` on a window of `backing` backing pixels
/// per point.
pub(crate) fn x_per_point(option: ScaleOption, backing: f64) -> f64 {
    usable_backing(backing) / f64::from(copy_factor(option, backing))
}

/// The physical size of `size` X pixels at `xpp` X pixels per point, on a
/// display of `display_mm` millimetres over `display_pt` points; `(0, 0)`
/// (unknown) when the display does not say.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Clamped and rounded.
pub(crate) fn output_mm(
    size: Size,
    xpp: f64,
    display_mm: (f64, f64),
    display_pt: (f64, f64),
) -> (u32, u32) {
    let usable = |v: f64| v.is_finite() && v > 0.0;
    if ![xpp, display_mm.0, display_mm.1, display_pt.0, display_pt.1]
        .into_iter()
        .all(usable)
    {
        return (0, 0);
    }
    let mm = |px: u16, mm: f64, pt: f64| {
        (f64::from(px) / xpp * mm / pt)
            .round()
            .clamp(0.0, f64::from(u32::MAX)) as u32
    };
    (
        mm(size.0, display_mm.0, display_pt.0),
        mm(size.1, display_mm.1, display_pt.1),
    )
}

/// `points` at `scale` X pixels per point, in whole pixels within
/// `1..=u16::MAX`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Clamped and floored.
pub(crate) fn to_px(points: f64, scale: f64) -> u16 {
    let px = (points * scale).floor();
    if px.is_nan() {
        return 1;
    }
    px.clamp(1.0, f64::from(u16::MAX)) as u16
}

/// Logical pixels per inch on Windows: 100% display scaling.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) const BASE_DPI: u32 = 96;

/// `--scale` on a window system whose window coordinates are device
/// pixels (Windows, per-monitor DPI aware): a logical pixel (`BASE_DPI`)
/// is the point, the monitor's DPI over `BASE_DPI` the backing scale.
/// Every window size, position and pointer coordinate is in device
/// pixels, and one X pixel covers `factor`x`factor` of them, so the
/// mapping is whole device pixels over the copy factor; the X pixels per
/// point follow from the backing scale exactly as on macOS.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PixelScale {
    /// Device pixels per logical pixel.
    pub(crate) backing: f64,
    /// X pixels per logical pixel.
    pub(crate) xpp: f64,
    /// Device pixels per X pixel per axis.
    pub(crate) factor: u32,
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
impl PixelScale {
    /// `option` on a monitor of `dpi` (0: unknown, as `BASE_DPI`).
    pub(crate) fn new(option: ScaleOption, dpi: u32) -> Self {
        let backing = if dpi == 0 {
            1.0
        } else {
            f64::from(dpi) / f64::from(BASE_DPI)
        };
        Self {
            backing,
            xpp: x_per_point(option, backing),
            factor: copy_factor(option, backing),
        }
    }

    /// A device-pixel coordinate in X pixels, unfloored (the pointer).
    pub(crate) fn to_x_coord(self, device: f64) -> f64 {
        device / f64::from(self.factor)
    }

    /// `device` pixels (a window's client area) in whole X pixels, within
    /// `1..=u16::MAX`; a remainder below one X pixel is not shown.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Clamped.
    pub(crate) fn to_x(self, device: (i32, i32)) -> Size {
        let px = |v: i32| (v / self.factor as i32).clamp(1, i32::from(u16::MAX)) as u16;
        (px(device.0), px(device.1))
    }

    /// `size` X pixels in device pixels.
    pub(crate) fn to_device(self, size: Size) -> (i32, i32) {
        let px = |v: u16| i32::from(v).saturating_mul(self.factor as i32);
        (px(size.0), px(size.1))
    }

    /// `size` X pixels in millimetres on a monitor of `display_mm` over
    /// `display_device` device pixels; `(0, 0)` when it does not say.
    pub(crate) fn mm(
        &self,
        size: Size,
        display_mm: (f64, f64),
        display_device: (f64, f64),
    ) -> (u32, u32) {
        output_mm(
            size,
            1.0 / f64::from(self.factor),
            display_mm,
            display_device,
        )
    }
}

/// The window's content size changes in, the sizes to post out.
#[derive(Debug)]
pub(crate) struct ResizeCoalescer {
    /// The window's latest size.
    seen: Size,
    /// The latest size posted to the core.
    posted: Size,
    live: bool,
    /// A tick passed since the last post of this live resize.
    ticked: bool,
}

impl ResizeCoalescer {
    /// The core starts at `posted`.
    pub(crate) fn new(posted: Size) -> Self {
        Self {
            seen: posted,
            posted,
            live: false,
            ticked: true,
        }
    }

    /// The window is now `size`; returns the size to post now.
    pub(crate) fn resized(&mut self, size: Size) -> Option<Size> {
        self.seen = size;
        if size == self.posted || (self.live && !self.ticked) {
            return None;
        }
        self.post()
    }

    /// The latest size posted.
    pub(crate) fn posted(&self) -> Size {
        self.posted
    }

    /// A size that goes out now, changed or not: the answer to a core
    /// request (with its token), or the size under a new scale.
    pub(crate) fn post_now(&mut self, size: Size) -> Size {
        self.seen = size;
        self.posted = size;
        size
    }

    /// A size waits for the next tick.
    pub(crate) fn waiting(&self) -> bool {
        self.live && self.seen != self.posted
    }

    /// A frame-clock tick; returns the size that waited for it.
    pub(crate) fn tick(&mut self) -> Option<Size> {
        self.ticked = true;
        if self.waiting() { self.post() } else { None }
    }

    pub(crate) fn live_started(&mut self) {
        self.live = true;
        self.ticked = true;
    }

    /// The live resize ended; returns the final size if not yet posted.
    pub(crate) fn live_ended(&mut self) -> Option<Size> {
        self.live = false;
        self.ticked = true;
        if self.seen == self.posted {
            return None;
        }
        self.post()
    }

    fn post(&mut self) -> Option<Size> {
        self.posted = self.seen;
        self.ticked = false;
        Some(self.seen)
    }
}

#[cfg(test)]
mod tests {
    use super::{PixelScale, ResizeCoalescer, copy_factor, output_mm, to_px, x_per_point};
    use crate::launch::ScaleOption;

    #[test]
    fn pixels_are_floored_and_clamped() {
        assert_eq!(to_px(640.0, 2.0), 1280);
        assert_eq!(to_px(511.75, 2.0), 1023);
        assert_eq!(to_px(100.4, 1.0), 100);
        assert_eq!(to_px(0.2, 2.0), 1);
        assert_eq!(to_px(-5.0, 2.0), 1);
        assert_eq!(to_px(40_000.0, 2.0), u16::MAX);
        assert_eq!(to_px(f64::NAN, 2.0), 1);
    }

    #[test]
    fn auto_scale_is_one_x_pixel_per_point() {
        for (backing, factor) in [(1.0, 1), (2.0, 2), (3.0, 3)] {
            assert_eq!(copy_factor(ScaleOption::Auto, backing), factor);
            assert_eq!(x_per_point(ScaleOption::Auto, backing), 1.0);
        }
        assert_eq!(copy_factor(ScaleOption::Auto, 1.5), 2, "rounded");
        assert_eq!(x_per_point(ScaleOption::Auto, 1.5), 0.75);
        for bad in [0.0, -2.0, f64::NAN, f64::INFINITY] {
            assert_eq!(copy_factor(ScaleOption::Auto, bad), 1, "{bad}");
            assert_eq!(x_per_point(ScaleOption::Auto, bad), 1.0, "{bad}");
        }
    }

    #[test]
    fn a_fixed_scale_keeps_its_factor_on_every_backing() {
        let one = ScaleOption::Factor(1);
        let two = ScaleOption::Factor(2);
        assert_eq!(copy_factor(one, 2.0), 1);
        assert_eq!(x_per_point(one, 2.0), 2.0, "X px = backing px");
        assert_eq!(x_per_point(one, 1.0), 1.0);
        assert_eq!(copy_factor(two, 1.0), 2);
        assert_eq!(x_per_point(two, 1.0), 0.5, "an X px is 2x2 points");
        assert_eq!(x_per_point(two, 2.0), 1.0);
    }

    #[test]
    fn millimetres_follow_the_points_the_size_covers() {
        // 0.2 mm per point on both axes.
        let (mm, pt) = ((288.0, 180.0), (1440.0, 900.0));
        assert_eq!(output_mm((1280, 800), 1.0, mm, pt), (256, 160));
        assert_eq!(output_mm((1280, 800), 2.0, mm, pt), (128, 80));
        assert_eq!(output_mm((1280, 800), 0.5, mm, pt), (512, 320));
        // 194.898 and 187.238 mm.
        let (mm, pt) = ((286.5, 179.0), (1470.0, 956.0));
        assert_eq!(output_mm((1000, 1000), 1.0, mm, pt), (195, 187));
        assert_eq!(output_mm((1280, 800), 1.0, (0.0, 0.0), pt), (0, 0));
        assert_eq!(output_mm((1280, 800), 1.0, mm, (0.0, 956.0)), (0, 0));
        assert_eq!(output_mm((1280, 800), f64::NAN, mm, pt), (0, 0));
    }

    /// Windows display scaling 100/125/150/200% is 96/120/144/192 DPI,
    /// backing 1, 1.25, 1.5, 2 device pixels per logical pixel. Auto rounds
    /// the backing (half away from zero, as macOS's Auto): 1, 1, 2, 2; a
    /// fixed scale keeps its factor. X pixels per logical pixel are the
    /// backing over the factor.
    #[test]
    fn pixel_scale_factor_and_x_per_logical_pixel() {
        let cases = [
            // (dpi, option, factor, xpp)
            (96, ScaleOption::Auto, 1, 1.0),
            (96, ScaleOption::Factor(1), 1, 1.0),
            (96, ScaleOption::Factor(2), 2, 0.5),
            (120, ScaleOption::Auto, 1, 1.25),
            (120, ScaleOption::Factor(1), 1, 1.25),
            (120, ScaleOption::Factor(2), 2, 0.625),
            (144, ScaleOption::Auto, 2, 0.75),
            (144, ScaleOption::Factor(1), 1, 1.5),
            (144, ScaleOption::Factor(2), 2, 0.75),
            (192, ScaleOption::Auto, 2, 1.0),
            (192, ScaleOption::Factor(1), 1, 2.0),
            (192, ScaleOption::Factor(2), 2, 1.0),
            // 175%: 1.75 rounds to 2.
            (168, ScaleOption::Auto, 2, 0.875),
        ];
        for (dpi, option, factor, xpp) in cases {
            let s = PixelScale::new(option, dpi);
            assert_eq!(s.factor, factor, "{dpi} dpi {option:?}");
            assert_eq!(s.xpp, xpp, "{dpi} dpi {option:?}");
        }
        assert_eq!(PixelScale::new(ScaleOption::Auto, 0).factor, 1, "unknown");
        assert_eq!(PixelScale::new(ScaleOption::Auto, 0).xpp, 1.0, "unknown");
    }

    /// A window of 1024x768 logical pixels is 1024x768 device pixels at
    /// 100%, 1280x960 at 125%, 1536x1152 at 150%, 2048x1536 at 200%; the X
    /// screen is that over the factor.
    #[test]
    fn pixel_scale_window_sizes() {
        let cases = [
            // (dpi, device client area, option, X size)
            (96, (1024, 768), ScaleOption::Auto, (1024, 768)),
            (96, (1024, 768), ScaleOption::Factor(1), (1024, 768)),
            (96, (1024, 768), ScaleOption::Factor(2), (512, 384)),
            (120, (1280, 960), ScaleOption::Auto, (1280, 960)),
            (120, (1280, 960), ScaleOption::Factor(1), (1280, 960)),
            (120, (1280, 960), ScaleOption::Factor(2), (640, 480)),
            (144, (1536, 1152), ScaleOption::Auto, (768, 576)),
            (144, (1536, 1152), ScaleOption::Factor(1), (1536, 1152)),
            (144, (1536, 1152), ScaleOption::Factor(2), (768, 576)),
            (192, (2048, 1536), ScaleOption::Auto, (1024, 768)),
            (192, (2048, 1536), ScaleOption::Factor(1), (2048, 1536)),
            (192, (2048, 1536), ScaleOption::Factor(2), (1024, 768)),
        ];
        for (dpi, device, option, x) in cases {
            let s = PixelScale::new(option, dpi);
            assert_eq!(s.to_x(device), x, "{dpi} dpi {option:?}");
            assert_eq!(s.to_device(x), device, "{dpi} dpi {option:?}");
        }
        let two = PixelScale::new(ScaleOption::Auto, 144);
        // 1537 / 2 = 768.5: the last device column shows nothing.
        assert_eq!(two.to_x((1537, 1153)), (768, 576));
        assert_eq!(two.to_x((1, 0)), (1, 1), "at least one X pixel");
        assert_eq!(two.to_x((-4, 200_000)), (1, u16::MAX));
        // `--geometry 1024x768` at 150% Auto: 2048x1536 device pixels,
        // 1365.33 logical pixels wide, as 1024 / 0.75 points on macOS.
        assert_eq!(two.to_device((1024, 768)), (2048, 1536));
    }

    #[test]
    fn pixel_scale_pointer_coordinates() {
        let two = PixelScale::new(ScaleOption::Factor(2), 192);
        // Device pixels 300 and 301 are both X pixel 150.
        assert_eq!(two.to_x_coord(300.0), 150.0);
        assert_eq!(two.to_x_coord(301.0), 150.5);
        assert_eq!(two.to_x_coord(-3.0), -1.5, "captured, left of the window");
        let one = PixelScale::new(ScaleOption::Auto, 120);
        assert_eq!(one.to_x_coord(301.0), 301.0);
    }

    #[test]
    fn pixel_scale_millimetres() {
        // A 2000x1000 device-pixel monitor of 500x250 mm: 0.25 mm per
        // device pixel. 1024x768 X pixels at factor 2 cover 2048x1536
        // device pixels: 512x384 mm; at factor 1, 256x192 mm.
        let (mm, device) = ((500.0, 250.0), (2000.0, 1000.0));
        let two = PixelScale::new(ScaleOption::Auto, 192);
        assert_eq!(two.mm((1024, 768), mm, device), (512, 384));
        let one = PixelScale::new(ScaleOption::Factor(1), 192);
        assert_eq!(one.mm((1024, 768), mm, device), (256, 192));
        assert_eq!(one.mm((1024, 768), (0.0, 0.0), device), (0, 0), "unknown");
    }

    #[test]
    fn outside_a_live_resize_every_change_posts_once() {
        let mut c = ResizeCoalescer::new((1024, 768));
        assert_eq!(c.resized((1024, 768)), None, "unchanged");
        assert_eq!(c.resized((1280, 800)), Some((1280, 800)));
        assert_eq!(c.resized((1280, 800)), None, "already posted");
        assert_eq!(c.resized((1440, 900)), Some((1440, 900)));
        assert!(!c.waiting());
        assert_eq!(c.tick(), None);
    }

    #[test]
    fn a_live_resize_posts_at_most_once_per_tick() {
        let mut c = ResizeCoalescer::new((800, 600));
        c.live_started();
        assert_eq!(c.resized((810, 600)), Some((810, 600)), "first change");
        assert_eq!(c.resized((820, 600)), None);
        assert_eq!(c.resized((830, 600)), None);
        assert!(c.waiting());
        assert_eq!(c.tick(), Some((830, 600)), "the latest size");
        assert_eq!(c.resized((840, 600)), None, "posted this tick");
        assert_eq!(c.tick(), Some((840, 600)));
        assert_eq!(c.tick(), None, "nothing waits");
        assert_eq!(c.resized((850, 600)), Some((850, 600)), "a tick passed");
    }

    #[test]
    fn the_end_of_a_live_resize_posts_the_final_size_once() {
        let mut c = ResizeCoalescer::new((800, 600));
        c.live_started();
        assert_eq!(c.resized((900, 700)), Some((900, 700)));
        assert_eq!(c.resized((950, 720)), None);
        assert_eq!(c.live_ended(), Some((950, 720)));
        assert!(!c.waiting());
        assert_eq!(c.tick(), None);

        c.live_started();
        assert_eq!(c.resized((960, 720)), Some((960, 720)));
        assert_eq!(c.live_ended(), None, "the final size was posted");
    }

    #[test]
    fn a_size_back_to_the_posted_one_needs_no_post() {
        let mut c = ResizeCoalescer::new((800, 600));
        c.live_started();
        assert_eq!(c.resized((900, 600)), Some((900, 600)));
        assert_eq!(c.resized((950, 600)), None);
        assert_eq!(c.resized((900, 600)), None);
        assert!(!c.waiting());
        assert_eq!(c.tick(), None);
        assert_eq!(c.live_ended(), None);
    }

    #[test]
    fn a_size_posted_now_counts_as_posted() {
        let mut c = ResizeCoalescer::new((1280, 800));
        assert_eq!(c.post_now((1024, 768)), (1024, 768));
        assert_eq!(c.posted(), (1024, 768));
        assert_eq!(c.resized((1024, 768)), None);
        assert_eq!(c.post_now((1024, 768)), (1024, 768), "even unchanged");
        assert_eq!(c.resized((1280, 800)), Some((1280, 800)));
    }

    #[test]
    fn a_size_posted_now_mid_live_resize_ends_the_wait() {
        let mut c = ResizeCoalescer::new((800, 600));
        c.live_started();
        assert_eq!(c.resized((900, 600)), Some((900, 600)));
        assert_eq!(c.resized((950, 600)), None);
        assert!(c.waiting());
        assert_eq!(c.post_now((950, 600)), (950, 600));
        assert!(!c.waiting());
        assert_eq!(c.tick(), None);
        assert_eq!(c.live_ended(), None);
    }

    #[test]
    fn a_start_size_other_than_the_cores_posts() {
        let mut c = ResizeCoalescer::new((3000, 2000));
        assert_eq!(c.resized((2560, 1500)), Some((2560, 1500)));
    }
}
