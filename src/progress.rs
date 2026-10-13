//! Lightweight progress reporting for long-running command-line operations.
//!
//! [`Progress`] tracks one phase at a time: a label, a completed/total count,
//! the item currently being processed, and the phase's elapsed time. On a
//! terminal it redraws a single bar line in place with carriage returns;
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

/// Width of the bar the terminal reporter draws, brackets included.
const BAR_WIDTH: usize = 24;

/// Longest line the terminal reporter draws, leaving the last column of an
/// 80-column terminal free so the line never wraps (a wrapped line cannot be
/// redrawn in place with a carriage return).
const TTY_MAX_CHARS: usize = 79;

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

    /// The full text of the terminal progress line: label, bar, percentage,
    /// counts, ETA when known, then the item name if it fits.
    fn render_tty(&self) -> String {
        let elapsed = self.phase_started.elapsed();
        let mut line = format!(
            "{}: {} {:>3}% {}/{} {}",
            self.label,
            format_bar(self.done, self.total),
            format_percent(self.done, self.total),
            self.done,
            self.total,
            self.unit,
        );
        if let Some(eta) = estimate_remaining(elapsed, self.done, self.total) {
            line.push_str(" ETA ");
            line.push_str(&format_duration(eta));
        }
        if !self.item.is_empty() {
            let budget = TTY_MAX_CHARS.saturating_sub(line.chars().count() + 2);
            if budget >= 4 {
                line.push_str("  ");
                line.push_str(&elide_start(&self.item, budget));
            }
        }
        line
    }

    /// The full text of one plain progress line, for non-terminal outputs.
    fn render_plain(&self) -> String {
        let elapsed = self.phase_started.elapsed();
        let mut line = format!(
            "{}: {}/{} {} ({}%) {}",
            self.label,
            self.done,
            self.total,
            self.unit,
            format_percent(self.done, self.total),
            format_duration(elapsed),
        );
        if let Some(eta) = estimate_remaining(elapsed, self.done, self.total) {
            line.push_str(" ETA ");
            line.push_str(&format_duration(eta));
        }
        if !self.item.is_empty() {
            line.push(' ');
            line.push_str(&self.item);
        }
        line
    }

    /// Redraw the progress line in place; only called when stderr is a TTY.
    fn draw_tty(&mut self) {
        let line = self.render_tty();
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
        let line = self.render_plain();
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

/// The bar the terminal reporter draws, e.g. `[##########--------------]`,
/// `BAR_WIDTH` characters wide including both brackets.
///
/// A zero total counts as complete.
fn format_bar(done: u64, total: u64) -> String {
    let cells = BAR_WIDTH - 2;
    let filled = if total == 0 {
        cells
    } else {
        (u128::from(done.min(total)) * cells as u128 / u128::from(total)) as usize
    };
    let mut bar = String::with_capacity(BAR_WIDTH);
    bar.push('[');
    bar.extend(std::iter::repeat_n('#', filled));
    bar.extend(std::iter::repeat_n('-', cells - filled));
    bar.push(']');
    bar
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
    elide_start(item, ITEM_MAX_CHARS)
}

/// Shorten `item` to `max_chars` characters (never below the three of the
/// elision itself), keeping its end so the file name survives; shorter names
/// are returned unchanged.
fn elide_start(item: &str, max_chars: usize) -> String {
    let count = item.chars().count();
    if count <= max_chars {
        return item.to_owned();
    }
    let keep = max_chars.saturating_sub(3);
    let mut out = String::from("...");
    out.extend(item.chars().skip(count - keep));
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
    fn bars_fill_proportionally_and_clamp_at_both_ends() {
        let empty: String = std::iter::repeat_n('-', BAR_WIDTH - 2).collect();
        let full: String = std::iter::repeat_n('#', BAR_WIDTH - 2).collect();
        assert_eq!(format_bar(0, 10), format!("[{empty}]"));
        assert_eq!(
            format_bar(1, 2),
            format!("[{}{}]", "#".repeat(11), "-".repeat(11))
        );
        assert_eq!(format_bar(10, 10), format!("[{full}]"));
        assert_eq!(format_bar(20, 10), format!("[{full}]"));
        assert_eq!(
            format_bar(0, 0),
            format!("[{full}]"),
            "a zero total is done"
        );
    }

    #[test]
    fn terminal_lines_show_a_bar_and_a_known_eta() {
        let mut progress = Progress::new();
        progress.label = "room".to_string();
        progress.unit = "files";
        progress.total = 10;
        progress.done = 3;
        progress.set_item("room/1000.rdt");

        let line = progress.render_tty();
        assert_eq!(
            line,
            format!(
                "room: {}  30% 3/10 files ETA 0s  room/1000.rdt",
                format_bar(3, 10)
            ),
            "{line}"
        );
    }

    #[test]
    fn terminal_lines_stay_on_one_line() {
        let mut progress = Progress::new();
        progress.label = "roomcut".to_string();
        progress.unit = "cuts";
        progress.total = 100;
        progress.done = 1;
        progress.item = "roomcut/".to_string() + &"nested/".repeat(20) + "100_000.bmp";

        let line = progress.render_tty();
        assert_eq!(line.chars().count(), TTY_MAX_CHARS, "{line}");
        assert!(line.ends_with("100_000.bmp"), "{line}");
        assert!(!line.chars().any(char::is_control), "{line}");
    }

    #[test]
    fn plain_lines_drop_an_unknown_eta() {
        let mut progress = Progress::new();
        progress.label = "room".to_string();
        progress.unit = "files";
        progress.total = 10;
        progress.item = "room/1000.rdt".to_string();

        let line = progress.render_plain();
        assert!(line.starts_with("room: 0/10 files (0%) "), "{line}");
        assert!(line.ends_with(" room/1000.rdt"), "{line}");
        assert!(!line.contains("ETA"), "{line}");

        progress.done = 3;
        let line = progress.render_plain();
        assert!(line.contains("(30%)"), "{line}");
        assert!(line.contains(" ETA "), "{line}");

        progress.done = 10;
        let line = progress.render_plain();
        assert!(line.starts_with("room: 10/10 files (100%) "), "{line}");
        assert!(line.ends_with(" room/1000.rdt"), "{line}");
        assert!(!line.contains("ETA"), "{line}");
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
