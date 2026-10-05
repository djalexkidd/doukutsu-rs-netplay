use crate::common::{fix9_scale, interpolate_fix9_scale};
use crate::game::shared_game_state::SharedGameState;
use crate::game::stage::Stage;
use crate::util::rng::RNG;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum UpdateTarget {
    Player,
    NPC(u16),
    Boss(u16),
}

#[derive(Clone)]
pub struct Frame {
    pub x: i32,
    pub y: i32,
    pub prev_x: i32,
    pub prev_y: i32,
    pub update_target: UpdateTarget,
    pub target_x: i32,
    pub target_y: i32,
    pub wait: i32,
}

impl Frame {
    pub fn new() -> Frame {
        Frame {
            x: 0,
            y: 0,
            prev_x: 0,
            prev_y: 0,
            update_target: UpdateTarget::Player,
            target_x: 0,
            target_y: 0,
            wait: 16,
        }
    }

    pub fn xy_interpolated(&self, frame_time: f64) -> (f32, f32) {
        if self.prev_x == self.x && self.prev_y == self.y {
            return (fix9_scale(self.x), fix9_scale(self.y));
        }

        let x = interpolate_fix9_scale(self.prev_x, self.x, frame_time);
        let y = interpolate_fix9_scale(self.prev_y, self.y, frame_time);

        (x, y)
    }

    /// Adapt a deterministic simulation camera to the local viewport without changing it.
    pub fn for_viewport(&self, source: (f32, f32), destination: (f32, f32), map_pixels: (i32, i32)) -> Frame {
        fn axis(position: i32, source: f32, destination: f32, map: i32) -> i32 {
            let viewport = destination as i32;
            if map < viewport {
                -((viewport - map) * 0x200 / 2)
            } else {
                let offset = (viewport - source as i32) * 0x200 / 2;
                (position - offset).clamp(0, (map - viewport) * 0x200)
            }
        }
        let mut frame = self.clone();
        frame.x = axis(self.x, source.0, destination.0, map_pixels.0);
        frame.prev_x = axis(self.prev_x, source.0, destination.0, map_pixels.0);
        frame.y = axis(self.y, source.1, destination.1, map_pixels.1);
        frame.prev_y = axis(self.prev_y, source.1, destination.1, map_pixels.1);
        frame
    }

    pub fn immediate_update(&mut self, state: &mut SharedGameState, stage: &Stage) {
        let mut screen_width = state.canvas_size.0;
        if state.constants.is_switch && stage.map.width <= 54 {
            screen_width += 10.0; // hack for scrolling
        }

        let tile_size = state.tile_size.as_int();

        if (stage.map.width as usize).saturating_sub(1) * (tile_size as usize) < screen_width as usize {
            self.x = -(((screen_width as i32 - (stage.map.width as i32 - 1) * tile_size) * 0x200) / 2);
        } else {
            self.x = self.target_x - (screen_width as i32 * 0x200 / 2);

            if self.x < 0 {
                self.x = 0;
            }

            let max_x = (((stage.map.width as i32 - 1) * tile_size) - screen_width as i32) * 0x200;
            if self.x > max_x {
                self.x = max_x;
            }
        }

        if (stage.map.height as usize).saturating_sub(1) * (tile_size as usize) < state.canvas_size.1 as usize {
            self.y = -(((state.canvas_size.1 as i32 - (stage.map.height as i32 - 1) * tile_size) * 0x200) / 2);
        } else {
            self.y = self.target_y - (state.canvas_size.1 as i32 * 0x200 / 2);

            if self.y < 0 {
                self.y = 0;
            }

            let max_y = (((stage.map.height as i32 - 1) * tile_size) - state.canvas_size.1 as i32) * 0x200;
            if self.y > max_y {
                self.y = max_y;
            }
        }

        self.prev_x = self.x;
        self.prev_y = self.y;
    }

    pub fn update_position(&mut self, state: &mut SharedGameState, stage: &Stage) {
        let mut screen_width = state.canvas_size.0;
        if state.constants.is_switch && stage.map.width <= 54 {
            screen_width += 10.0;
        }

        if self.wait == 0 {
            // prevent zero division
            self.wait = 1;
        }

        let tile_size = state.tile_size.as_int();

        if (stage.map.width as usize).saturating_sub(1) * (tile_size as usize) < screen_width as usize {
            self.x = -(((screen_width as i32 - (stage.map.width as i32 - 1) * tile_size) * 0x200) / 2);
        } else {
            self.x += (self.target_x - (screen_width as i32 * 0x200 / 2) - self.x) / self.wait;

            if self.x < 0 {
                self.x = 0;
            }

            let max_x = (((stage.map.width as i32 - 1) * tile_size) - screen_width as i32) * 0x200;
            if self.x > max_x {
                self.x = max_x;
            }
        }

        if (stage.map.height as usize).saturating_sub(1) * (tile_size as usize) < state.canvas_size.1 as usize {
            self.y = -(((state.canvas_size.1 as i32 - (stage.map.height as i32 - 1) * tile_size) * 0x200) / 2);
        } else {
            self.y += (self.target_y - (state.canvas_size.1 as i32 * 0x200 / 2) - self.y) / self.wait;

            if self.y < 0 {
                self.y = 0;
            }

            let max_y = (((stage.map.height as i32 - 1) * tile_size) - state.canvas_size.1 as i32) * 0x200;
            if self.y > max_y {
                self.y = max_y;
            }
        }
    }

    pub fn update(&mut self, state: &mut SharedGameState, stage: &Stage) {
        self.update_position(state, stage);
        let intensity = state.settings.screen_shake_intensity.to_val();

        if state.super_quake_counter > 0 {
            state.super_quake_counter -= 1;

            let new_x = state.effect_rng.range(-0x300..0x300) * 5;
            let new_y = state.effect_rng.range(-0x300..0x300) * 3;

            self.x += (f64::from(new_x) * intensity).round() as i32;
            self.y += (f64::from(new_y) * intensity).round() as i32;
        }

        if state.quake_counter > 0 {
            state.quake_counter -= 1;

            let new_x = state.effect_rng.range(-0x300..0x300) as i32;
            let new_y = state.effect_rng.range(-0x300..0x300) as i32;

            self.x += (f64::from(new_x) * intensity).round() as i32;
            self.y += (f64::from(new_y) * intensity).round() as i32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_viewport_centers_small_rooms_and_limits_both_edges() {
        let mut camera = Frame::new();
        camera.x = 680 * 0x200;
        camera.prev_x = camera.x - 0x200;
        camera.y = 260 * 0x200;
        camera.prev_y = camera.y - 0x200;
        let view = camera.for_viewport((320.0, 240.0), (428.0, 240.0), (1000, 500));
        assert_eq!((view.x, view.prev_x), (572 * 0x200, 572 * 0x200));
        assert_eq!(view.y, 260 * 0x200);
        assert_eq!(camera.x, 680 * 0x200);
        camera.x = 0;
        camera.prev_x = 0;
        let view = camera.for_viewport((320.0, 240.0), (428.0, 240.0), (1000, 500));
        assert_eq!((view.x, view.prev_x), (0, 0));
        let view = camera.for_viewport((320.0, 240.0), (428.0, 240.0), (224, 160));
        assert_eq!((view.x, view.prev_x), (-102 * 0x200, -102 * 0x200));
        assert_eq!((view.y, view.prev_y), (-40 * 0x200, -40 * 0x200));
    }

    #[test]
    fn network_viewport_preserves_interpolation_between_simulation_frames() {
        let mut camera = Frame::new();
        camera.prev_x = 200 * 0x200;
        camera.x = 202 * 0x200;
        let view = camera.for_viewport((320.0, 240.0), (428.0, 240.0), (1000, 500));
        assert_eq!(view.prev_x, 146 * 0x200);
        assert_eq!(view.x, 148 * 0x200);
        assert_eq!(view.x - view.prev_x, camera.x - camera.prev_x);
    }
}
