//! Presentation-only spatialization in network games. Positions use engine fixed-point units.
/// Keep nearby effects at their original volume, then fade them smoothly out at 480 pixels.
pub(super) fn gains(listener: (i32, i32), source: (i32, i32)) -> [f32; 2] {
    let dx = (source.0 as f64 - listener.0 as f64) / 512.0;
    let dy = (source.1 as f64 - listener.1 as f64) / 512.0;
    let distance = dx.hypot(dy);
    let fade = ((distance - 32.0) / 448.0).clamp(0.0, 1.0);
    let volume = (1.0 - fade).powi(2) as f32;
    let pan = (dx.signum() * (dx.abs() - 32.0).max(0.0) / 160.0).clamp(-1.0, 1.0) as f32;
    [volume * (1.0 - pan.max(0.0)), volume * (1.0 + pan.min(0.0))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearby_sounds_keep_their_volume_and_distant_sounds_fade_on_both_axes() {
        assert_eq!(gains((0, 0), (0, 0)), [1.0, 1.0]);
        assert_eq!(gains((0, 0), (16 * 512, 0)), [1.0, 1.0]);
        let near = gains((0, 0), (0, 80 * 512));
        let far = gains((0, 0), (0, 240 * 512));
        assert!(far[0] < near[0] && near[0] < 1.0);
        assert_eq!(gains((0, 0), (480 * 512, 0)), [0.0, 0.0]);
        assert_eq!(gains((0, 0), (0, 480 * 512)), [0.0, 0.0]);
        assert_eq!(gains((i32::MIN, 0), (i32::MAX, 0)), [0.0, 0.0]);
    }

    #[test]
    fn moving_the_listener_reverses_the_stereo_balance() {
        let right = gains((0, 0), (100 * 512, 0));
        let left = gains((200 * 512, 0), (100 * 512, 0));
        assert!(right[1] > right[0]);
        assert_eq!(right, [left[1], left[0]]);
    }
}
