//! The F2/F3 gameplay speed control.
//!
//! The speed is a multiplier on how many fixed ticks the simulation runs per
//! wall-clock second. It starts at the original's own rate (1x) and each press
//! moves it by [`STEP`] within [`MIN`]..=[`MAX`]: F2 slows down, F3 speeds up.
//!
//! Nothing else about the simulation changes. The interactive loop divides the
//! current screen's tick interval by the multiplier, so the same ticks run in
//! the same order, only more of them per wall second. The frame pacer keeps
//! presenting frames on the wall-clock deadline it already had.

/// The original's own rate: one tick per tick interval.
pub const MIN: f64 = 1.0;
/// The fastest rate F3 reaches.
pub const MAX: f64 = 3.0;
/// The change one F2/F3 press makes.
pub const STEP: f64 = 0.5;

/// The interactive loop's speed multiplier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Speed(f64);

impl Default for Speed {
    fn default() -> Self {
        Self(MIN)
    }
}

impl Speed {
    /// The multiplier: the simulation ticks this many times per wall-clock
    /// tick interval.
    pub fn multiplier(self) -> f64 {
        self.0
    }

    /// Move `delta` steps faster (positive, F3) or slower (negative, F2),
    /// clamped to [`MIN`]..=[`MAX`]. Returns whether the multiplier changed,
    /// so the caller only reports real changes.
    pub fn step(&mut self, delta: i32) -> bool {
        let steps = ((MAX - MIN) / STEP).round() as i32;
        let step = ((self.0 - MIN) / STEP).round() as i32;
        let next = MIN + f64::from((step + delta).clamp(0, steps)) * STEP;
        let changed = next != self.0;
        self.0 = next;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_speed_is_the_original_rate() {
        let speed = Speed::default();
        assert_eq!(speed.multiplier(), 1.0);
    }

    #[test]
    fn f3_steps_up_to_the_cap_and_stops() {
        let mut speed = Speed::default();
        assert!(speed.step(1));
        assert_eq!(speed.multiplier(), 1.5);
        assert!(speed.step(1));
        assert_eq!(speed.multiplier(), 2.0);
        assert!(speed.step(1));
        assert!(speed.step(1));
        assert_eq!(speed.multiplier(), MAX);
        // At the cap nothing changes and no report is made.
        assert!(!speed.step(1));
        assert_eq!(speed.multiplier(), MAX);
    }

    #[test]
    fn f2_steps_down_to_the_floor_and_stops() {
        let mut speed = Speed::default();
        // Already at the floor: the first F2 does nothing.
        assert!(!speed.step(-1));
        assert_eq!(speed.multiplier(), MIN);
        speed.step(1);
        speed.step(1);
        assert_eq!(speed.multiplier(), 2.0);
        assert!(speed.step(-1));
        assert_eq!(speed.multiplier(), 1.5);
        assert!(speed.step(-1));
        assert_eq!(speed.multiplier(), MIN);
        assert!(!speed.step(-1));
    }

    #[test]
    fn big_deltas_stay_in_range() {
        let mut speed = Speed::default();
        speed.step(100);
        assert_eq!(speed.multiplier(), MAX);
        speed.step(-100);
        assert_eq!(speed.multiplier(), MIN);
    }

    #[test]
    fn the_multiplier_divides_the_tick_interval() {
        let mut speed = Speed::default();
        speed.step(1);
        let tick_ms = 1000.0 / 30.0;
        assert!((tick_ms / speed.multiplier() - tick_ms * 2.0 / 3.0).abs() < 1e-9);
    }
}
