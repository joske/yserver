//! Which window sizes a shell reports to the core: every size outside a
//! live resize, at most one per frame-clock tick during one, and the final
//! size when it ends.

/// A size in X pixels.
pub(crate) type Size = (u16, u16);

/// The smallest screen (and window content) offered.
pub(crate) const MIN_SCREEN: Size = (320, 200);

/// `points` at `scale` backing pixels per point, in whole pixels within
/// `1..=u16::MAX`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Clamped and floored.
pub(crate) fn to_px(points: f64, scale: f64) -> u16 {
    let px = (points * scale).floor();
    if px.is_nan() {
        return 1;
    }
    px.clamp(1.0, f64::from(u16::MAX)) as u16
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

    /// The window is `size` at the end of a request of the core: always
    /// posted (with the request's token).
    pub(crate) fn answered(&mut self, size: Size) -> Size {
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
    use super::{ResizeCoalescer, to_px};

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
    fn an_answered_request_counts_as_posted() {
        let mut c = ResizeCoalescer::new((1280, 800));
        assert_eq!(c.answered((1024, 768)), (1024, 768));
        assert_eq!(c.resized((1024, 768)), None);
        assert_eq!(c.answered((1024, 768)), (1024, 768), "even unchanged");
        assert_eq!(c.resized((1280, 800)), Some((1280, 800)));
    }

    #[test]
    fn a_start_size_other_than_the_cores_posts() {
        let mut c = ResizeCoalescer::new((3000, 2000));
        assert_eq!(c.resized((2560, 1500)), Some((2560, 1500)));
    }
}
