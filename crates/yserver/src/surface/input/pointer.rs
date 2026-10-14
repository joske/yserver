//! Window-system pointer positions and scroll deltas to host input.

use yserver_core::core_loop::HostInputEvent;

use super::devices::{POINTER, pointer_origin};
use crate::scroll_v120::ScrollV120;

/// v120 per point of precise (trackpad) scroll: the KMS finger-scroll
/// factor, 15 points per click.
const V120_PER_POINT: f64 = 8.0;
/// v120 per line of wheel scroll.
const V120_PER_LINE: f64 = 120.0;

/// A pointer position in view points, origin bottom-left.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RawPointer {
    pub(crate) x_pt: f64,
    pub(crate) y_pt: f64,
    pub(crate) view_h_pt: f64,
    /// X pixels per point.
    pub(crate) scale: f64,
    /// The event's motion delta in points, y down.
    pub(crate) dx_pt: f64,
    pub(crate) dy_pt: f64,
}

/// The X pixel under `raw` on a `fb`-sized screen, clamped to it.
pub(crate) fn x_position(raw: &RawPointer, fb: (u16, u16)) -> (i32, i32) {
    let clamp = |v: f64, size: u16| {
        let max = f64::from(size.saturating_sub(1));
        #[allow(clippy::cast_possible_truncation)]
        let px = v.floor().clamp(0.0, max) as i32;
        px
    };
    (
        clamp(raw.x_pt * raw.scale, fb.0),
        clamp((raw.view_h_pt - raw.y_pt) * raw.scale, fb.1),
    )
}

/// An absolute motion to `raw`'s X pixel.
pub(crate) fn motion(raw: &RawPointer, fb: (u16, u16), time: u32) -> HostInputEvent {
    let (x, y) = x_position(raw, fb);
    #[allow(clippy::cast_possible_truncation)]
    let delta = |d: f64| (d * raw.scale).round() as i32;
    HostInputEvent::PointerMotion {
        origin: pointer_origin(),
        x,
        y,
        time,
        relative: false,
        dx: delta(raw.dx_pt),
        dy: delta(raw.dy_pt),
        motion_delta: None,
    }
}

/// A gesture or momentum phase of a scroll event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollPhase {
    None,
    Began,
    Changed,
    Ended,
    Cancelled,
    Other,
}

impl ScrollPhase {
    fn ends(self) -> bool {
        matches!(self, Self::Ended | Self::Cancelled)
    }
}

/// A scroll event. Positive `dy` scrolls up (button 4), positive `dx` left.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RawScroll {
    pub(crate) dx: f64,
    pub(crate) dy: f64,
    /// Points (trackpad, Magic Mouse) rather than wheel lines.
    pub(crate) precise: bool,
    pub(crate) phase: ScrollPhase,
    pub(crate) momentum: ScrollPhase,
}

/// Scroll deltas to wheel clicks through the shared v120 accumulator.
#[derive(Debug, Default)]
pub(crate) struct Scroller {
    clicks: ScrollV120,
    /// The sub-v120 remainder per axis.
    frac: (f64, f64),
}

impl Scroller {
    pub(crate) fn scroll(&mut self, raw: RawScroll, time: u32, out: &mut Vec<HostInputEvent>) {
        let per_unit = if raw.precise {
            V120_PER_POINT
        } else {
            V120_PER_LINE
        };
        // libinput's sign: positive v120 scrolls down / right.
        self.frac.0 -= raw.dx * per_unit;
        self.frac.1 -= raw.dy * per_unit;
        let whole = (self.frac.0.trunc(), self.frac.1.trunc());
        self.frac.0 -= whole.0;
        self.frac.1 -= whole.1;
        #[allow(clippy::cast_possible_truncation)]
        self.clicks
            .drain(POINTER, whole.0 as i32, whole.1 as i32, time, out);
        if raw.momentum == ScrollPhase::None && raw.phase.ends() {
            // Fingers lifted: as libinput's scroll stop.
            self.reset();
            out.push(HostInputEvent::PointerScrollStop {
                origin: pointer_origin(),
                time,
            });
        } else if raw.momentum.ends() {
            self.reset();
        }
    }

    fn reset(&mut self) {
        self.clicks.reset(POINTER);
        self.frac = (0.0, 0.0);
    }
}

#[cfg(test)]
mod tests {
    use yserver_core::core_loop::{
        HostInputEvent, SYNTH_SCROLL_DOWN, SYNTH_SCROLL_LEFT, SYNTH_SCROLL_UP,
    };

    use super::{RawPointer, RawScroll, ScrollPhase, Scroller, motion, x_position};

    fn at(x_pt: f64, y_pt: f64, scale: f64) -> RawPointer {
        RawPointer {
            x_pt,
            y_pt,
            view_h_pt: 500.0,
            scale,
            dx_pt: 0.0,
            dy_pt: 0.0,
        }
    }

    #[test]
    fn points_become_x_pixels_with_y_flipped() {
        let fb = (1600, 1000);
        assert_eq!(x_position(&at(10.0, 490.0, 1.0), fb), (10, 10));
        assert_eq!(x_position(&at(10.0, 490.0, 2.0), fb), (20, 20));
        assert_eq!(x_position(&at(10.25, 489.75, 2.0), fb), (20, 20), "floor");
        assert_eq!(x_position(&at(0.0, 500.0, 2.0), fb), (0, 0), "top-left");
        assert_eq!(x_position(&at(800.0, 0.0, 2.0), fb), (1599, 999), "edge");
        assert_eq!(
            x_position(&at(-30.0, 600.0, 2.0), fb),
            (0, 0),
            "outside clamps"
        );
        assert_eq!(x_position(&at(900.0, -40.0, 2.0), fb), (1599, 999));
    }

    #[test]
    fn one_x_pixel_per_point_or_two_points_per_x_pixel() {
        let fb = (1600, 1000);
        // One X pixel per point (`--scale auto`).
        assert_eq!(x_position(&at(10.75, 489.25, 1.0), fb), (10, 10), "floor");
        assert_eq!(x_position(&at(1599.5, 0.0, 1.0), fb), (1599, 500));
        assert_eq!(x_position(&at(1700.0, -600.0, 1.0), fb), (1599, 999));
        // Two points per X pixel (`--scale 2` on a 1x screen).
        assert_eq!(x_position(&at(10.0, 490.0, 0.5), fb), (5, 5));
        assert_eq!(x_position(&at(11.0, 489.0, 0.5), fb), (5, 5), "floor");
        assert_eq!(x_position(&at(0.0, 500.0, 0.5), fb), (0, 0), "top-left");
        assert_eq!(x_position(&at(499.0, 0.0, 0.5), fb), (249, 250));
        assert_eq!(
            x_position(&at(-30.0, 600.0, 0.5), fb),
            (0, 0),
            "outside clamps"
        );
    }

    #[test]
    fn motion_is_absolute_with_scaled_deltas() {
        let mut raw = at(10.0, 490.0, 2.0);
        raw.dx_pt = 1.5;
        raw.dy_pt = -2.0;
        let HostInputEvent::PointerMotion {
            x,
            y,
            relative,
            dx,
            dy,
            motion_delta,
            ..
        } = motion(&raw, (1600, 1000), 5)
        else {
            panic!("a motion");
        };
        assert_eq!((x, y, relative), (20, 20, false));
        assert_eq!((dx, dy, motion_delta), (3, -4, None));
    }

    fn scroll(s: &mut Scroller, raw: RawScroll) -> Vec<(u16, bool)> {
        let mut out = Vec::new();
        s.scroll(raw, 0, &mut out);
        out.iter()
            .map(|ev| match ev {
                HostInputEvent::PointerButton {
                    button, pressed, ..
                } => (*button, *pressed),
                HostInputEvent::PointerScrollStop { .. } => (0, false),
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    fn raw(dy: f64, precise: bool, phase: ScrollPhase, momentum: ScrollPhase) -> RawScroll {
        RawScroll {
            dx: 0.0,
            dy,
            precise,
            phase,
            momentum,
        }
    }

    #[test]
    fn three_wheel_lines_are_three_clicks_up() {
        let mut s = Scroller::default();
        let events = scroll(
            &mut s,
            raw(3.0, false, ScrollPhase::None, ScrollPhase::None),
        );
        assert_eq!(
            events,
            [(SYNTH_SCROLL_UP, true), (SYNTH_SCROLL_UP, false)].repeat(3)
        );
        let down = scroll(
            &mut s,
            raw(-1.0, false, ScrollPhase::None, ScrollPhase::None),
        );
        assert_eq!(
            down,
            [(SYNTH_SCROLL_DOWN, true), (SYNTH_SCROLL_DOWN, false)]
        );
    }

    #[test]
    fn two_precise_half_clicks_make_one_click() {
        let mut s = Scroller::default();
        let changed = raw(-7.5, true, ScrollPhase::Changed, ScrollPhase::None);
        assert!(scroll(&mut s, changed).is_empty(), "7.5 points: 60 v120");
        assert_eq!(
            scroll(&mut s, changed),
            [(SYNTH_SCROLL_DOWN, true), (SYNTH_SCROLL_DOWN, false)]
        );
    }

    #[test]
    fn horizontal_precise_scroll_left() {
        let mut s = Scroller::default();
        let mut left = raw(0.0, true, ScrollPhase::Changed, ScrollPhase::None);
        left.dx = 15.0;
        assert_eq!(
            scroll(&mut s, left),
            [(SYNTH_SCROLL_LEFT, true), (SYNTH_SCROLL_LEFT, false)]
        );
    }

    #[test]
    fn lifting_the_fingers_stops_and_resets() {
        let mut s = Scroller::default();
        let half = raw(-7.5, true, ScrollPhase::Changed, ScrollPhase::None);
        scroll(&mut s, half);
        let ended = raw(0.0, true, ScrollPhase::Ended, ScrollPhase::None);
        assert_eq!(scroll(&mut s, ended), [(0, false)], "one stop, no click");
        assert!(scroll(&mut s, half).is_empty(), "the half click was reset");
    }

    #[test]
    fn only_an_ended_or_cancelled_gesture_stops() {
        let mut s = Scroller::default();
        for phase in [ScrollPhase::Began, ScrollPhase::Changed, ScrollPhase::Other] {
            assert!(scroll(&mut s, raw(0.0, true, phase, ScrollPhase::None)).is_empty());
        }
        let cancelled = raw(0.0, true, ScrollPhase::Cancelled, ScrollPhase::None);
        assert_eq!(scroll(&mut s, cancelled), [(0, false)]);
    }

    #[test]
    fn momentum_end_resets_without_a_stop() {
        let mut s = Scroller::default();
        let coast = raw(-7.5, true, ScrollPhase::None, ScrollPhase::Changed);
        assert!(scroll(&mut s, coast).is_empty());
        let end = raw(0.0, true, ScrollPhase::None, ScrollPhase::Ended);
        assert!(scroll(&mut s, end).is_empty(), "no stop for momentum");
        assert!(scroll(&mut s, coast).is_empty(), "the half click was reset");
    }
}
