//! Throwaway painter state — bouncing rectangle + cursor follower.

use crate::input::InputEvent;

pub const RECT_SIZE: u16 = 60;
pub const CURSOR_SIZE: u16 = 4;

#[derive(Debug, Clone, Default)]
pub struct State {
    pub rect_x: f32,
    pub rect_y: f32,
    pub vel_x: f32,
    pub vel_y: f32,
    pub cursor_x: f32,
    pub cursor_y: f32,
}

pub fn update(state: &mut State, dt: f32, events: &[InputEvent], width: u16, height: u16) {
    state.rect_x += state.vel_x * dt;
    state.rect_y += state.vel_y * dt;

    let max_x = f32::from(width.saturating_sub(RECT_SIZE));
    let max_y = f32::from(height.saturating_sub(RECT_SIZE));

    if state.rect_x < 0.0 {
        state.rect_x = -state.rect_x;
        state.vel_x = state.vel_x.abs();
    } else if state.rect_x > max_x {
        state.rect_x = max_x - (state.rect_x - max_x);
        state.vel_x = -state.vel_x.abs();
    }
    if state.rect_y < 0.0 {
        state.rect_y = -state.rect_y;
        state.vel_y = state.vel_y.abs();
    } else if state.rect_y > max_y {
        state.rect_y = max_y - (state.rect_y - max_y);
        state.vel_y = -state.vel_y.abs();
    }

    let cmax_x = f32::from(width.saturating_sub(CURSOR_SIZE));
    let cmax_y = f32::from(height.saturating_sub(CURSOR_SIZE));
    for ev in events {
        if let InputEvent::PointerMotion { dx, dy, .. } = ev {
            state.cursor_x = (state.cursor_x + *dx as f32).clamp(0.0, cmax_x);
            state.cursor_y = (state.cursor_y + *dy as f32).clamp(0.0, cmax_y);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_advances_position_by_velocity() {
        let mut s = State {
            rect_x: 0.0,
            rect_y: 0.0,
            vel_x: 100.0,
            vel_y: 50.0,
            cursor_x: 0.0,
            cursor_y: 0.0,
        };
        update(&mut s, 0.1, &[], 1024, 768);
        assert!((s.rect_x - 10.0).abs() < 1e-3);
        assert!((s.rect_y - 5.0).abs() < 1e-3);
    }

    #[test]
    fn rect_bounces_off_right_edge() {
        let mut s = State {
            rect_x: 1020.0,
            rect_y: 0.0,
            vel_x: 100.0,
            vel_y: 0.0,
            cursor_x: 0.0,
            cursor_y: 0.0,
        };
        update(&mut s, 0.1, &[], 1024, 768);
        assert!(
            s.vel_x < 0.0,
            "velocity should flip negative on right-edge bounce"
        );
    }

    #[test]
    fn pointer_motion_moves_cursor() {
        let mut s = State::default();
        update(
            &mut s,
            0.0,
            &[InputEvent::PointerMotion {
                source_id: yserver_core::xinput::InputSourceId(1),
                dx: 5.0,
                dy: 3.0,
            }],
            1024,
            768,
        );
        assert!((s.cursor_x - 5.0).abs() < 1e-3);
        assert!((s.cursor_y - 3.0).abs() < 1e-3);
    }
}
