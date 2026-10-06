//! Fixed-bucket frame-time instrumentation for `--stats`.
//!
//! [`Stats`] records one duration per fixed tick plus the phase totals for room
//! load, script/entity update, effect update and render, and the entities and
//! effects high-water marks. The histogram is a fixed array of power-of-two
//! nanosecond buckets, so recording a tick never allocates.
//!
//! The report is a stable, parseable line set printed at exit:
//!
//! ```text
//! stats ticks 600
//! stats tick_us min 120 avg 240 p95 510 max 3100
//! stats phase_us load 4200 update 118000 effect 9000 render 22000
//! stats high_water entities 5 effects 2
//! ```

use std::io::Write;
use std::time::Duration;

/// Power-of-two nanosecond buckets; bucket `i` covers `[2^i, 2^(i+1))`.
const BUCKETS: usize = 40;

/// One fixed-bucket duration histogram.
#[derive(Debug, Clone)]
pub struct Histogram {
    buckets: [u64; BUCKETS],
    count: u64,
    min_ns: u64,
    max_ns: u64,
    sum_ns: u128,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: [0; BUCKETS],
            count: 0,
            min_ns: 0,
            max_ns: 0,
            sum_ns: 0,
        }
    }
}

impl Histogram {
    /// Record one duration.
    pub fn record(&mut self, duration: Duration) {
        let ns = duration.as_nanos().min(u128::from(u64::MAX)) as u64;
        // A duration beyond the largest bucket still records: clamp it into the
        // last bucket instead of indexing past the end of the array.
        let index = bucket_index(ns).min(BUCKETS - 1);
        self.buckets[index] += 1;
        if self.count == 0 || ns < self.min_ns {
            self.min_ns = ns;
        }
        self.max_ns = self.max_ns.max(ns);
        self.sum_ns += u128::from(ns);
        self.count += 1;
    }

    /// Number of recorded durations.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Minimum recorded duration.
    pub fn min(&self) -> Duration {
        Duration::from_nanos(self.min_ns)
    }

    /// Maximum recorded duration.
    pub fn max(&self) -> Duration {
        Duration::from_nanos(self.max_ns)
    }

    /// Mean recorded duration.
    pub fn mean(&self) -> Duration {
        if self.count == 0 {
            return Duration::ZERO;
        }
        Duration::from_nanos((self.sum_ns / u128::from(self.count)) as u64)
    }

    /// The `p` percentile (0.0-1.0) as the upper edge of its bucket.
    ///
    /// Clamped between the mean and the maximum so the report always reads
    /// `min <= avg <= p95 <= max`.
    pub fn percentile(&self, p: f64) -> Duration {
        if self.count == 0 {
            return Duration::ZERO;
        }
        let rank = ((p * self.count as f64).ceil() as u64).max(1) - 1;
        let mut seen = 0u64;
        let mut edge_ns = 0u64;
        for (index, &count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen > rank {
                edge_ns = bucket_upper_ns(index);
                break;
            }
        }
        let lower = self.mean().as_nanos().min(u128::from(u64::MAX)) as u64;
        Duration::from_nanos(edge_ns.max(lower).min(self.max_ns))
    }

    /// The 95th percentile.
    pub fn p95(&self) -> Duration {
        self.percentile(0.95)
    }
}

/// The bucket that covers `ns`: floor(log2(ns)), saturating at 0 for 0.
fn bucket_index(ns: u64) -> usize {
    if ns == 0 {
        return 0;
    }
    (u64::BITS - 1 - ns.leading_zeros()) as usize
}

/// The exclusive upper edge of bucket `index`, in nanoseconds.
fn bucket_upper_ns(index: usize) -> u64 {
    if index >= 63 {
        return u64::MAX;
    }
    1u64 << (index + 1)
}

/// Accumulated phase durations.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PhaseTotals {
    /// Room load plus boot (init script, Lua room hook, camera).
    pub load: Duration,
    /// Script/entity/player update, from the tick's start to the effects.
    pub update: Duration,
    /// Effect pool update for the tick.
    pub effect: Duration,
    /// Frame render.
    pub render: Duration,
}

/// The `--stats` collector.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// Fixed ticks recorded.
    pub ticks: u64,
    /// Per-tick durations.
    pub tick: Histogram,
    /// Phase totals.
    pub phases: PhaseTotals,
    /// Highest number of active entities seen on a tick.
    pub entities_high: usize,
    /// Highest number of live effects seen on a tick.
    pub effects_high: usize,
}

impl Stats {
    /// A collector with nothing recorded.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one tick's total duration.
    pub fn record_tick(&mut self, duration: Duration) {
        self.tick.record(duration);
        self.ticks += 1;
    }

    /// Update the entities/effects high-water marks.
    pub fn observe_high_water(&mut self, entities: usize, effects: usize) {
        self.entities_high = self.entities_high.max(entities);
        self.effects_high = self.effects_high.max(effects);
    }

    /// Write the stable report line set.
    pub fn report(&self, out: &mut impl Write) -> std::io::Result<()> {
        let us = |duration: Duration| duration.as_micros().min(u128::from(u64::MAX)) as u64;
        writeln!(out, "stats ticks {}", self.ticks)?;
        writeln!(
            out,
            "stats tick_us min {} avg {} p95 {} max {}",
            us(self.tick.min()),
            us(self.tick.mean()),
            us(self.tick.p95()),
            us(self.tick.max())
        )?;
        writeln!(
            out,
            "stats phase_us load {} update {} effect {} render {}",
            us(self.phases.load),
            us(self.phases.update),
            us(self.phases.effect),
            us(self.phases.render)
        )?;
        writeln!(
            out,
            "stats high_water entities {} effects {}",
            self.entities_high, self.effects_high
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_histogram_reports_min_mean_p95_max_in_order() {
        let mut histogram = Histogram::default();
        for ns in [100u64, 200, 300, 400, 5_000, 1_000_000] {
            histogram.record(Duration::from_nanos(ns));
        }

        assert_eq!(histogram.count(), 6);
        assert_eq!(histogram.min(), Duration::from_nanos(100));
        assert_eq!(histogram.max(), Duration::from_nanos(1_000_000));
        assert_eq!(histogram.mean(), Duration::from_nanos(167_666));
        assert!(histogram.p95() >= histogram.mean());
        assert!(histogram.p95() <= histogram.max());
    }

    #[test]
    fn an_empty_histogram_reads_zero() {
        let histogram = Histogram::default();
        assert_eq!(histogram.count(), 0);
        assert_eq!(histogram.min(), Duration::ZERO);
        assert_eq!(histogram.mean(), Duration::ZERO);
        assert_eq!(histogram.p95(), Duration::ZERO);
        assert_eq!(histogram.max(), Duration::ZERO);
    }

    #[test]
    fn a_duration_beyond_the_largest_bucket_clamps_into_it() {
        let mut histogram = Histogram::default();
        histogram.record(Duration::from_nanos(u64::MAX));
        histogram.record(Duration::MAX);

        assert_eq!(histogram.count(), 2);
        assert_eq!(histogram.max(), Duration::from_nanos(u64::MAX));
        assert_eq!(histogram.buckets[BUCKETS - 1], 2);
        assert_eq!(histogram.buckets.iter().sum::<u64>(), 2);
        assert!(histogram.p95() <= histogram.max());
    }

    #[test]
    fn recording_ticks_never_allocates_histogram_storage() {
        let mut stats = Stats::new();
        for _ in 0..10_000 {
            stats.record_tick(Duration::from_micros(50));
        }
        assert_eq!(stats.ticks, 10_000);
        assert_eq!(stats.tick.count(), 10_000);
        assert_eq!(stats.tick.buckets.iter().sum::<u64>(), 10_000);
    }

    #[test]
    fn the_report_round_trips_through_text() {
        let mut stats = Stats::new();
        stats.phases.load = Duration::from_micros(4_000);
        stats.phases.update = Duration::from_micros(100_000);
        stats.phases.effect = Duration::from_micros(2_000);
        stats.phases.render = Duration::from_micros(20_000);
        for index in 0..100 {
            stats.record_tick(Duration::from_micros(200 + index));
        }
        stats.observe_high_water(3, 1);
        stats.observe_high_water(5, 2);

        let mut out = Vec::new();
        stats.report(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(lines[0].starts_with("stats ticks 100"), "{text}");
        assert!(lines[1].starts_with("stats tick_us min "), "{text}");
        assert!(lines[2].starts_with("stats phase_us load "), "{text}");
        assert!(
            lines[3].starts_with("stats high_water entities 5 effects 2"),
            "{text}"
        );
        let tick: Vec<&str> = lines[1].split_whitespace().collect();
        assert_eq!(tick[0], "stats");
        assert_eq!(tick[1], "tick_us");
        let min: u64 = tick[3].parse().unwrap();
        let avg: u64 = tick[5].parse().unwrap();
        let p95: u64 = tick[7].parse().unwrap();
        let max: u64 = tick[9].parse().unwrap();
        assert!(min <= avg && avg <= p95 && p95 <= max, "{text}");
        assert_eq!(min, 200);
        assert_eq!(max, 299);
    }
}
