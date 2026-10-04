/// Test the swept path of a projectile against the bubble, including fast shots.
pub fn shot_hits_bubble(px: i32, py: i32, x: i32, y: i32, bx: i32, by: i32) -> bool {
    let (dx, dy) = (x as f64 - px as f64, y as f64 - py as f64);
    let (rx, ry) = (bx as f64 - px as f64, by as f64 - py as f64);
    let length = dx * dx + dy * dy;
    let t = if length == 0.0 { 0.0 } else { ((rx * dx + ry * dy) / length).clamp(0.0, 1.0) };
    let (ex, ey) = (rx - dx * t, ry - dy * t);
    ex * ex + ey * ey <= (13.0 * 512.0_f64).powi(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn swept_shots_pop_bubbles_without_hitting_distant_players() {
        assert!(shot_hits_bubble(-20000, 0, 20000, 0, 0, 0));
        assert!(shot_hits_bubble(0, 0, 0, 0, 0, 0));
        assert!(!shot_hits_bubble(-20000, 9000, 20000, 9000, 0, 0));
        assert!(!shot_hits_bubble(-30000, 0, -20000, 0, 0, 0));
    }
}
