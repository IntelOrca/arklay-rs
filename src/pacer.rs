//! When the interactive loop presents a frame.
//!
//! The simulation steps on its own fixed-tick accumulator; [`FramePacer`]
//! only decides, after each pass of the loop, whether to present now and how
//! far between the last two ticks that frame sits. Nothing it computes feeds
//! the simulation.
//!
//! - At or below the tick rate (the default 30 fps, and every 16 ms
//!   non-gameplay screen up to 62 fps) a frame is presented after every pass
//!   that ran a tick, showing that tick exactly.
//! - Above it, frames are presented on a wall-clock deadline every
//!   `1 / frame_rate` seconds, each with `alpha` = the time since the last
//!   tick over the tick interval, so at 120 fps a 30 Hz tick gets four frames
//!   and a rate that is not a multiple (75) still presents on time without
//!   ever skipping a tick. A deadline missed by more than a frame (a hitch, or
//!   vsync holding the loop to a slower display) restarts from now rather than
//!   bursting to catch up.

/// The longest the loop sleeps at once, so events and audio stay serviced.
pub const MAX_WAIT_NS: u64 = 4_000_000;

/// What the loop should do after a pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pace {
    /// Present a frame; `alpha` is the tween position between the previous
    /// and the current tick, or `None` to draw the current tick exactly.
    Present { alpha: Option<f32> },
    /// Nothing is due; sleep this many nanoseconds (at most [`MAX_WAIT_NS`]).
    Wait(u64),
}

/// The presentation clock of one interactive loop.
#[derive(Debug, Clone)]
pub struct FramePacer {
    rate: u32,
    /// The next present deadline while frames outpace ticks.
    next_ns: Option<u64>,
    /// Whether a tick ran since the last present.
    ticked: bool,
}

impl FramePacer {
    /// A pacer presenting `rate` frames per second (at least 1).
    pub fn new(rate: u32) -> Self {
        Self {
            rate: rate.max(1),
            next_ns: None,
            ticked: true,
        }
    }

    /// The configured frames per second.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Change the rate; the next pass re-anchors the deadline.
    pub fn set_rate(&mut self, rate: u32) {
        self.rate = rate.max(1);
        self.next_ns = None;
    }

    /// Whether frames come faster than ticks `tick_ms` apart, i.e. whether
    /// presented frames tween.
    pub fn tweens(&self, tick_ms: f64) -> bool {
        f64::from(self.rate) * tick_ms > 1000.0 + 1e-6
    }

    /// Note that the loop ran a simulation tick.
    pub fn ticked(&mut self) {
        self.ticked = true;
    }

    /// Present now regardless of the deadline, drawing the current tick
    /// exactly; the next deadline re-anchors.
    pub fn force(&mut self) -> Pace {
        self.ticked = false;
        self.next_ns = None;
        Pace::Present { alpha: None }
    }

    /// Decide this pass: `accumulator` is the unspent time (ms) since the
    /// last tick, `tick_ms` the current tick interval.
    pub fn poll(&mut self, now_ns: u64, accumulator: f64, tick_ms: f64) -> Pace {
        let until_tick = ((tick_ms - accumulator).max(0.0) * 1e6) as u64;
        if !self.tweens(tick_ms) {
            self.next_ns = None;
            if std::mem::take(&mut self.ticked) {
                return Pace::Present { alpha: None };
            }
            return Pace::Wait(until_tick.clamp(1, MAX_WAIT_NS));
        }
        let frame_ns = 1_000_000_000 / u64::from(self.rate);
        let due = *self.next_ns.get_or_insert(now_ns);
        if now_ns >= due {
            let next = due + frame_ns;
            self.next_ns = Some(if next <= now_ns {
                now_ns + frame_ns
            } else {
                next
            });
            self.ticked = false;
            let alpha = (accumulator / tick_ms).clamp(0.0, 1.0) as f32;
            return Pace::Present { alpha: Some(alpha) };
        }
        Pace::Wait((due - now_ns).min(until_tick).clamp(1, MAX_WAIT_NS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK_MS: f64 = 1000.0 / 30.0;

    /// Run the loop for one simulated second at 1 ms passes (0 to 999 ms);
    /// return the presented frames and the ticks.
    fn simulate(rate: u32, tick_ms: f64) -> (Vec<Option<f32>>, u32) {
        let mut pacer = FramePacer::new(rate);
        let mut accumulator = 0.0;
        let mut frames = Vec::new();
        let mut ticks = 0;
        for ms in 0..1000u64 {
            if ms > 0 {
                accumulator += 1.0;
            }
            while accumulator >= tick_ms {
                accumulator -= tick_ms;
                ticks += 1;
                pacer.ticked();
            }
            if let Pace::Present { alpha } = pacer.poll(ms * 1_000_000, accumulator, tick_ms) {
                frames.push(alpha);
            }
        }
        (frames, ticks)
    }

    #[test]
    fn the_default_rate_presents_each_tick_exactly() {
        let (frames, ticks) = simulate(30, TICK_MS);
        // The first pass presents the boot frame, then one per tick.
        assert_eq!(frames.len() as u32, ticks + 1);
        assert!(frames.iter().all(Option::is_none));
    }

    #[test]
    fn the_ui_tick_is_never_dropped_below_its_own_rate() {
        let (frames, ticks) = simulate(30, 16.0);
        assert_eq!(frames.len() as u32, ticks + 1);
    }

    #[test]
    fn higher_rates_present_on_time_and_tween() {
        for rate in [60, 75, 120, 144, 165] {
            let (frames, ticks) = simulate(rate, TICK_MS);
            // The 30th tick falls due at 1000 ms, just past the window.
            assert_eq!(ticks, 29);
            // Deadlines advance by the exact period, so even with 1 ms passes
            // a period that is not a whole millisecond keeps the rate.
            assert_eq!(frames.len() as u32, rate, "{rate} fps");
            assert!(
                frames
                    .iter()
                    .all(|alpha| alpha.is_some_and(|a| (0.0..=1.0).contains(&a)))
            );
        }
        // At 120 every tick interval holds four frames, a quarter apart.
        let (frames, _) = simulate(120, TICK_MS);
        let alphas: Vec<f32> = frames[4..8].iter().map(|a| a.unwrap()).collect();
        for pair in alphas.windows(2) {
            assert!((pair[1] - pair[0] - 0.25).abs() < 0.05, "{alphas:?}");
        }
    }

    #[test]
    fn a_missed_deadline_restarts_instead_of_bursting() {
        let mut pacer = FramePacer::new(120);
        assert!(matches!(pacer.poll(0, 0.0, TICK_MS), Pace::Present { .. }));
        // A 100 ms hitch: one frame, then the next deadline is a frame away.
        assert!(matches!(
            pacer.poll(100_000_000, 0.0, TICK_MS),
            Pace::Present { .. }
        ));
        assert!(matches!(
            pacer.poll(101_000_000, 0.0, TICK_MS),
            Pace::Wait(_)
        ));
    }

    #[test]
    fn force_presents_exactly_and_re_anchors_the_deadline() {
        let mut pacer = FramePacer::new(120);
        assert!(matches!(pacer.poll(0, 0.0, TICK_MS), Pace::Present { .. }));
        assert!(matches!(pacer.poll(1_000_000, 0.0, TICK_MS), Pace::Wait(_)));
        assert_eq!(pacer.force(), Pace::Present { alpha: None });
        // The next poll starts a new schedule at once.
        assert!(matches!(
            pacer.poll(2_000_000, 0.0, TICK_MS),
            Pace::Present { alpha: Some(_) }
        ));
    }

    #[test]
    fn a_rate_change_mid_run_takes_effect_at_once() {
        let mut pacer = FramePacer::new(30);
        pacer.poll(0, 0.0, TICK_MS);
        assert!(matches!(pacer.poll(1_000_000, 1.0, TICK_MS), Pace::Wait(_)));
        pacer.set_rate(120);
        assert!(pacer.tweens(TICK_MS));
        assert!(matches!(
            pacer.poll(2_000_000, 2.0, TICK_MS),
            Pace::Present { alpha: Some(_) }
        ));
        pacer.set_rate(30);
        pacer.ticked();
        assert_eq!(
            pacer.poll(3_000_000, 0.0, TICK_MS),
            Pace::Present { alpha: None }
        );
    }

    #[test]
    fn switching_to_the_ui_tick_presents_each_tick_exactly() {
        // At 60 fps the 33 ms gameplay tick tweens, but the 16 ms UI tick is
        // faster than the frame rate: every tick is presented exactly.
        let mut pacer = FramePacer::new(60);
        assert!(pacer.tweens(TICK_MS));
        assert!(!pacer.tweens(16.0));
        pacer.poll(0, 0.0, TICK_MS);
        pacer.ticked();
        assert_eq!(
            pacer.poll(1_000_000, 0.0, 16.0),
            Pace::Present { alpha: None }
        );
        assert!(matches!(pacer.poll(2_000_000, 1.0, 16.0), Pace::Wait(_)));
        // And back: the deadline re-anchors and frames tween again.
        assert!(matches!(
            pacer.poll(3_000_000, 2.0, TICK_MS),
            Pace::Present { alpha: Some(_) }
        ));
    }

    #[test]
    fn waits_are_capped() {
        let mut pacer = FramePacer::new(30);
        pacer.poll(0, 0.0, TICK_MS);
        assert_eq!(pacer.poll(0, 0.0, TICK_MS), Pace::Wait(MAX_WAIT_NS));
    }
}
