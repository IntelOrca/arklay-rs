//! Lightweight progress reporting for long-running command-line operations.
//!
//! [`Progress`] tracks one phase at a time: a label, a completed/total count,
//! the item currently being processed, and the phase's elapsed time. On a
//! terminal it redraws a single line in place with carriage returns;
//! everywhere else it falls back to periodic plain lines, so logs and pipes
//! never see control characters.
//!
//! The ETA and formatting helpers at the bottom of the module are pure and
//! unit tested; the reporter itself only keeps a handful of counters and one
//! short item string, so it stays cheap enough for per-entry updates.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

/// How often a non-terminal reporter emits a plain progress line.
const PLAIN_INTERVAL: Duration = Duration::from_secs(2);

/// Longest item name rendered before its start is elided.
const ITEM_MAX_CHARS: usize = 44;

/// Tracks and renders the progress of one phase at a time.
pub struct Progress {
    /// When this reporter was created; basis for [`Progress::elapsed`].
    started: Instant,
    /// Label of the active phase, e.g. `roomcut`.
    label: String,
    /// Unit of the active phase, e.g. `cuts`.
    unit: &'static str,
    /// Total units in the active phase.
    total: u64,
    /// Units completed so far.
    done: u64,
    /// Shortened name of the item currently being processed.
    item: String,
    /// When the active phase began.
    phase_started: Instant,
    /// When the last plain line was written (non-terminal mode).
    last_plain: Instant,
    /// Width of the last terminal line, used to pad over stale text.
    line_len: usize,
    /// Whether a phase is active.
    active: bool,
    /// Whether stderr is a terminal.
    tty: bool,
}

impl Progress {
    /// Create a reporter; the overall clock starts now.
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            label: String::new(),
            unit: "items",
            total: 0,
            done: 0,
            item: String::new(),
            phase_started: now,
            last_plain: now,
            line_len: 0,
            active: false,
            tty: std::io::stderr().is_terminal(),
        }
    }

    /// Begin a phase of `total` `unit`s, ending any previous phase.
    pub fn begin(&mut self, label: &str, total: u64, unit: &'static str) {
        if self.active {
            self.end_phase();
        }
        self.label.clear();
        self.label.push_str(label);
        self.unit = unit;
        self.total = total;
        self.done = 0;
        self.item.clear();
        self.phase_started = Instant::now();
        self.last_plain = self.phase_started;
        self.line_len = 0;
        self.active = true;
        // Draw the zero line immediately so a long first item does not leave
        // the phase invisible until the first update or the plain interval.
        // Empty phases only render their final line from `end_phase`.
        if self.total > 0 {
            if self.tty {
                self.draw_tty();
            } else {
                self.draw_plain();
            }
        }
    }

    /// Advance the active phase by one item, naming the item just processed.
    pub fn advance(&mut self, item: &str) {
        self.set_item(item);
        self.advance_by(1);
    }

    /// Advance the active phase by `count` items, naming the batch.
    pub fn advance_named(&mut self, count: u64, item: &str) {
        self.set_item(item);
        self.advance_by(count);
    }

    /// Advance the active phase by `count` unnamed units (e.g. bytes).
    pub fn advance_by(&mut self, count: u64) {
        if !self.active {
            return;
        }
        self.done = self.done.saturating_add(count);
        if self.tty {
            self.draw_tty();
        } else if self.done < self.total && self.last_plain.elapsed() >= PLAIN_INTERVAL {
            // The final line is always emitted by `end_phase`.
            self.draw_plain();
        }
    }

    /// Finish the active phase, leaving its final line on screen.
    pub fn end_phase(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        if self.tty {
            self.draw_tty();
            let _ = writeln!(std::io::stderr());
        } else {
            self.draw_plain();
        }
    }

    /// Time since this reporter was created.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Name the current item for upcoming redraws.
    fn set_item(&mut self, item: &str) {
        self.item.clear();
        self.item.push_str(&short_item(item));
    }

    /// The full text of the current progress line.
    fn render(&self) -> String {
        let elapsed = self.phase_started.elapsed();
        let percent = format_percent(self.done, self.total);
        let eta = estimate_remaining(elapsed, self.done, self.total)
            .map_or_else(|| "--".to_owned(), format_duration);
        let mut line = format!(
            "{}: {}/{} {} ({percent}%) {} ETA {eta}",
            self.label,
            self.done,
            self.total,
            self.unit,
            format_duration(elapsed),
        );
        if !self.item.is_empty() {
            line.push(' ');
            line.push_str(&self.item);
        }
        line
    }

    /// Redraw the progress line in place; only called when stderr is a TTY.
    fn draw_tty(&mut self) {
        let line = self.render();
        let width = line.chars().count();
        let padding = self.line_len.saturating_sub(width);
        let mut stderr = std::io::stderr().lock();
        let _ = write!(stderr, "\r{line}");
        if padding > 0 {
            let _ = write!(stderr, "{:width$}", "", width = padding);
        }
        let _ = stderr.flush();
        self.line_len = width;
    }

    /// Write one plain progress line; only called when stderr is not a TTY.
    fn draw_plain(&mut self) {
        let line = self.render();
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{line}");
        let _ = stderr.flush();
        self.last_plain = Instant::now();
    }
}

impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}

/// Percentage of `total` completed, clamped to `0..=100`.
///
/// A zero total counts as complete.
fn format_percent(done: u64, total: u64) -> u64 {
    if done >= total {
        return 100;
    }
    (u128::from(done) * 100 / u128::from(total)) as u64
}

/// Format a duration as `45s`, `1m05s` or `1h01m01s`.
pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}m{:02}s", secs / 60, secs % 60);
    }
    format!("{}h{:02}m{:02}s", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// Estimate the time left at the current rate, or `None` when it cannot be
/// estimated (nothing done yet, already finished, or absurdly large).
fn estimate_remaining(elapsed: Duration, done: u64, total: u64) -> Option<Duration> {
    if done == 0 || done >= total {
        return None;
    }
    let per_unit = elapsed.as_secs_f64() / done as f64;
    Duration::try_from_secs_f64(per_unit * (total - done) as f64).ok()
}

/// Shorten a long item name to [`ITEM_MAX_CHARS`], keeping its end.
fn short_item(item: &str) -> String {
    let count = item.chars().count();
    if count <= ITEM_MAX_CHARS {
        return item.to_owned();
    }
    let mut out = String::from("...");
    out.extend(item.chars().skip(count - (ITEM_MAX_CHARS - 3)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_use_sensible_units() {
        assert_eq!(format_duration(Duration::ZERO), "0s");
        assert_eq!(format_duration(Duration::from_millis(999)), "0s");
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(60)), "1m00s");
        assert_eq!(format_duration(Duration::from_secs(605)), "10m05s");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h01m01s");
    }

    #[test]
    fn percentages_clamp_and_round_down() {
        assert_eq!(format_percent(0, 0), 100);
        assert_eq!(format_percent(0, 10), 0);
        assert_eq!(format_percent(1, 2), 50);
        assert_eq!(format_percent(2, 3), 66);
        assert_eq!(format_percent(3, 3), 100);
        assert_eq!(format_percent(4, 3), 100);
        assert_eq!(format_percent(u64::MAX - 1, u64::MAX), 99);
    }

    #[test]
    fn remaining_interpolates_the_current_rate() {
        assert_eq!(estimate_remaining(Duration::from_secs(10), 0, 100), None);
        assert_eq!(estimate_remaining(Duration::from_secs(10), 100, 100), None);
        assert_eq!(estimate_remaining(Duration::from_secs(10), 150, 100), None);
        let eta = estimate_remaining(Duration::from_secs(10), 50, 100).unwrap();
        assert!((eta.as_secs_f64() - 10.0).abs() < 0.001);
        let eta = estimate_remaining(Duration::from_secs(5), 1, 4).unwrap();
        assert!((eta.as_secs_f64() - 15.0).abs() < 0.001);
    }

    #[test]
    fn long_items_keep_their_file_name() {
        let path = "roomcut/600_123.bmp";
        assert_eq!(short_item(path), path);

        let long = "roomcut/very/deeply/nested/directory/name/600_123.bmp";
        let short = short_item(long);
        assert_eq!(short.chars().count(), ITEM_MAX_CHARS);
        assert!(short.starts_with("..."));
        assert!(short.ends_with("600_123.bmp"));

        let unicode = "é".repeat(100);
        let short = short_item(&unicode);
        assert_eq!(short.chars().count(), ITEM_MAX_CHARS);
    }
}
