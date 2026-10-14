//! Wheel and finger scroll in v120 units (120 per click), turned into
//! press+release pairs of the synthetic scroll buttons (X buttons 4–7).

use std::collections::HashMap;

use yserver_core::{
    core_loop::{
        HostInputEvent, InputOrigin, SYNTH_SCROLL_DOWN, SYNTH_SCROLL_LEFT, SYNTH_SCROLL_RIGHT,
        SYNTH_SCROLL_UP,
    },
    xinput::InputSourceId,
};

/// One v120 click step. libinput emits high-resolution wheel deltas
/// in 120ths of a logical click; we accumulate fractional deltas and
/// fire a button press+release each time |accum| crosses this.
const V120_PER_CLICK: i32 = 120;

/// Sub-click scroll accumulators in v120 units, per source. High-resolution
/// wheel and finger/continuous scroll arrive as small v120 deltas that may
/// not add up to a full 120-unit click in one event. We bank the remainder
/// here and emit a button-4/5/6/7 press+release pair each time the absolute
/// accumulator crosses 120. Sign convention matches libinput: positive Y =
/// scroll down, positive X = scroll right.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScrollV120 {
    by_source: HashMap<InputSourceId, (i32, i32)>,
}

impl ScrollV120 {
    /// Accumulate a scroll delta and emit press+release pairs for any
    /// completed clicks. `dy_v120 > 0` → scroll-down (button 5);
    /// `dy_v120 < 0` → scroll-up (button 4). Horizontal axis maps to
    /// button 6 (left) / 7 (right). Mixed-axis events emit Y clicks
    /// first then X clicks within a single call.
    pub(crate) fn drain(
        &mut self,
        source_id: InputSourceId,
        dx_v120: i32,
        dy_v120: i32,
        time_ms: u32,
        out: &mut Vec<HostInputEvent>,
    ) {
        let (accum_x, accum_y) = self.by_source.entry(source_id).or_default();
        *accum_x = accum_x.saturating_add(dx_v120);
        *accum_y = accum_y.saturating_add(dy_v120);

        // Vertical first (more common; matches X11 button-4/5 priority).
        while *accum_y >= V120_PER_CLICK {
            *accum_y -= V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_DOWN, time_ms, source_id);
        }
        while *accum_y <= -V120_PER_CLICK {
            *accum_y += V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_UP, time_ms, source_id);
        }
        while *accum_x >= V120_PER_CLICK {
            *accum_x -= V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_RIGHT, time_ms, source_id);
        }
        while *accum_x <= -V120_PER_CLICK {
            *accum_x += V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_LEFT, time_ms, source_id);
        }
    }

    /// Discard `source_id`'s sub-click remainder.
    pub(crate) fn reset(&mut self, source_id: InputSourceId) {
        self.by_source.remove(&source_id);
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.by_source.is_empty()
    }
}

fn push_button_click(
    out: &mut Vec<HostInputEvent>,
    button: u16,
    time_ms: u32,
    source_id: InputSourceId,
) {
    out.push(HostInputEvent::PointerButton {
        origin: InputOrigin::Physical(source_id),
        button,
        pressed: true,
        time: time_ms,
    });
    out.push(HostInputEvent::PointerButton {
        origin: InputOrigin::Physical(source_id),
        button,
        pressed: false,
        time: time_ms,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SOURCE_ID: InputSourceId = InputSourceId(1);

    #[test]
    fn reset_discards_the_sub_click_remainder() {
        let mut scroll = ScrollV120::default();
        let mut out = Vec::new();
        scroll.drain(TEST_SOURCE_ID, 0, 60, 0, &mut out);
        scroll.reset(TEST_SOURCE_ID);
        assert!(scroll.is_empty());
        scroll.drain(TEST_SOURCE_ID, 0, 60, 0, &mut out);
        assert!(out.is_empty(), "60 + 60 across a reset is no click");
    }

    #[test]
    fn scroll_accumulates_subclick_v120() {
        let mut scroll = ScrollV120::default();
        let mut out = Vec::new();
        // 60 + 30 + 40 = 130 → one click; remainder 10 banked.
        scroll.drain(TEST_SOURCE_ID, 0, 60, 0, &mut out);
        assert!(out.is_empty(), "60 < 120, no emission yet");
        scroll.drain(TEST_SOURCE_ID, 0, 30, 0, &mut out);
        assert!(out.is_empty(), "60 + 30 = 90 < 120");
        scroll.drain(TEST_SOURCE_ID, 0, 40, 0, &mut out);
        assert_eq!(
            out.len(),
            2,
            "60 + 30 + 40 = 130 should emit one press+release pair"
        );
        assert!(matches!(
            out[0],
            HostInputEvent::PointerButton {
                button: SYNTH_SCROLL_DOWN,
                pressed: true,
                ..
            }
        ));
    }

    #[test]
    fn scroll_negative_v120_emits_scroll_up() {
        let mut scroll = ScrollV120::default();
        let mut out = Vec::new();
        scroll.drain(TEST_SOURCE_ID, 0, -120, 0, &mut out);
        assert_eq!(out.len(), 2);
        assert!(matches!(
            out[0],
            HostInputEvent::PointerButton {
                button: SYNTH_SCROLL_UP,
                pressed: true,
                ..
            }
        ));
    }

    #[test]
    fn scroll_multiple_clicks_in_one_event() {
        let mut scroll = ScrollV120::default();
        let mut out = Vec::new();
        // 480 v120 = exactly 4 clicks down.
        scroll.drain(TEST_SOURCE_ID, 0, 480, 0, &mut out);
        assert_eq!(out.len(), 8, "4 clicks × (press + release)");
        for chunk in out.chunks_exact(2) {
            assert!(matches!(
                chunk[0],
                HostInputEvent::PointerButton {
                    button: SYNTH_SCROLL_DOWN,
                    pressed: true,
                    ..
                }
            ));
            assert!(matches!(
                chunk[1],
                HostInputEvent::PointerButton {
                    button: SYNTH_SCROLL_DOWN,
                    pressed: false,
                    ..
                }
            ));
        }
    }

    #[test]
    fn scroll_horizontal_emits_buttons_6_7() {
        let mut scroll = ScrollV120::default();
        let mut out = Vec::new();
        scroll.drain(TEST_SOURCE_ID, 120, 0, 0, &mut out);
        assert!(matches!(
            out[0],
            HostInputEvent::PointerButton {
                button: SYNTH_SCROLL_RIGHT,
                pressed: true,
                ..
            }
        ));
        out.clear();
        scroll.drain(TEST_SOURCE_ID, -120, 0, 0, &mut out);
        assert!(matches!(
            out[0],
            HostInputEvent::PointerButton {
                button: SYNTH_SCROLL_LEFT,
                pressed: true,
                ..
            }
        ));
    }
}
