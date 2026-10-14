//! Lightweight progress reporting for long-running command-line operations.
//!
//! One [`Progress`] reports a single bar for the whole operation, however many
//! internal phases it runs: [`Progress::begin`] starts a named phase and
//! [`Progress::end_phase`] finishes it, and the bar measures the completed
//! phases plus the fraction of the active one. A conversion that touches two
//! dozen asset categories therefore still draws one bar that fills from start to
//! finish, with the current phase named alongside it.
//!
//! On a terminal it redraws one line in place with carriage returns; when the
//! run ends [`Progress::finish`] (or the reporter's drop, on an early return)
//! erases it so the command's own output stands alone. Everywhere else it falls
//! back to periodic plain lines, so logs and pipes never see control
//! characters.
//!
//! The formatting helpers at the bottom of the module are pure and unit tested;
//! the reporter itself only keeps a handful of counters and one short item
//! string, so it stays cheap enough for per-entry updates.

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

/// Fixed-point scale of the overall progress fraction: the bar is full at
/// [`SCALE`]. Integer fixed point keeps the reporter allocation- and
/// float-free.
const SCALE: u64 = 10_000;

/// Tracks one overall bar across the phases of a run.
pub struct Progress {
    /// When this reporter was created; basis for [`Progress::elapsed`].
    started: Instant,
    /// Fixed prefix of the rendered line, e.g. `convert-game`.
    title: String,
    /// Total phases the run will report; set once they are known.
    phases_total: u64,
    /// Phases finished so far.
    phases_done: u64,
    /// Label of the active phase, e.g. `roomcut`.
    label: String,
    /// Total units in the active phase.
    total: u64,
    /// Units completed in the active phase.
    done: u64,
    /// Shortened name of the item currently being processed.
    item: String,
    /// When the last plain line was written (non-terminal mode).
    last_plain: Instant,
    /// Width of the last terminal line, used to pad over stale text.
    line_len: usize,
    /// Whether a phase is active.
    active: bool,
    /// Whether stderr is a terminal.
    tty: bool,
    /// Whether the terminal cursor is currently hidden by the bar.
    cursor_hidden: bool,
}

impl Progress {
    /// Create a reporter for `title` that will run `phases` phases; pass `0`
    /// when the count is not known yet and call [`Progress::set_phases`] before
    /// the first [`Progress::begin`].
    pub fn new(title: &str, phases: u64) -> Self {
        let now = Instant::now();
        Self {
            started: now,
            title: title.to_owned(),
            phases_total: phases,
            phases_done: 0,
            label: String::new(),
            total: 0,
            done: 0,
            item: String::new(),
            last_plain: now,
            line_len: 0,
            active: false,
            tty: std::io::stderr().is_terminal(),
            cursor_hidden: false,
        }
    }

    /// Set the total number of phases the run will report, once known.
    pub fn set_phases(&mut self, phases: u64) {
        self.phases_total = phases;
    }

    /// Begin a phase of `total` units, ending any previous phase.
    ///
    /// A zero-total phase is counted in the overall bar but draws nothing, so a
    /// category that resolved no files never flashes on screen.
    pub fn begin(&mut self, label: &str, total: u64) {
        if self.active {
            self.end_phase();
        }
        self.label.clear();
        self.label.push_str(label);
        self.total = total;
        self.done = 0;
        self.item.clear();
        self.last_plain = Instant::now();
        self.active = true;
        if total > 0 && self.tty {
            self.draw_tty();
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
        } else if self.total > 0
            && self.done < self.total
            && self.last_plain.elapsed() >= PLAIN_INTERVAL
        {
            self.draw_plain();
        }
    }

    /// Finish the active phase.
    ///
    /// The bar stays on its line; the caller's [`Progress::finish`] (or the
    /// reporter's drop) erases it once the run ends.
    pub fn end_phase(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        self.done = self.total;
        if self.phases_total > 0 && self.phases_done < self.phases_total {
            self.phases_done += 1;
        }
        if self.tty && self.total > 0 {
            self.draw_tty();
        }
    }

    /// End the active phase, settle the final line and erase it.
    ///
    /// A run whose declared phase count matched its phases has already finished
    /// by the last [`Progress::end_phase`]; this only settles the line when the
    /// count drifted. Either way the bar is removed so the command's own output
    /// (warnings, the final status line) stands alone.
    pub fn finish(&mut self) {
        if self.active {
            self.end_phase();
        }
        debug_assert_eq!(
            self.phases_done, self.phases_total,
            "the declared phase count drifted from the run"
        );
        if self.phases_total > 0 && self.phases_done < self.phases_total {
            self.phases_done = self.phases_total;
            self.done = self.total;
        }
        self.clear();
    }

    /// Erase the bar's line and restore the cursor, leaving the cursor where
    /// the run's own output can carry on. A no-op when nothing was drawn
    /// (redirected) or already cleared.
    fn clear(&mut self) {
        if !self.tty {
            return;
        }
        if self.line_len == 0 && !self.cursor_hidden {
            return;
        }
        let mut stderr = std::io::stderr().lock();
        if self.line_len > 0 {
            let _ = write!(stderr, "\r{:width$}\r", "", width = self.line_len);
            self.line_len = 0;
        }
        if self.cursor_hidden {
            let _ = write!(stderr, "\x1b[?25h");
            self.cursor_hidden = false;
        }
        let _ = stderr.flush();
    }

    /// Time since this reporter was created.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Number of phases finished so far.
    #[cfg(test)]
    fn phases_done(&self) -> u64 {
        self.phases_done
    }

    /// Name the current item for upcoming redraws.
    fn set_item(&mut self, item: &str) {
        self.item.clear();
        self.item.push_str(&short_item(item));
    }

    /// Overall progress in `0..=SCALE`: completed phases plus the active
    /// phase's fraction, spread over the phase count.
    fn overall_scaled(&self) -> u64 {
        if self.phases_total == 0 {
            return 0;
        }
        let base = u128::from(self.phases_done.min(self.phases_total)) * u128::from(SCALE)
            / u128::from(self.phases_total);
        let within = if self.total == 0 {
            0
        } else {
            u128::from(self.done.min(self.total)) * u128::from(SCALE)
                / u128::from(self.total)
                / u128::from(self.phases_total)
        };
        (base + within).min(u128::from(SCALE)) as u64
    }

    /// The full text of the terminal progress line: title, overall bar and
    /// percentage, the active phase, then the item name if it fits.
    fn render_tty(&self) -> String {
        let scaled = self.overall_scaled();
        let mut line = format!(
            "{}: {} {:>3}%",
            self.title,
            format_bar(scaled),
            scaled / 100,
        );
        self.push_context(&mut line);
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
        let scaled = self.overall_scaled();
        let elapsed = self.started.elapsed();
        let mut line = format!(
            "{}: {}% {}",
            self.title,
            scaled / 100,
            format_duration(elapsed),
        );
        self.push_context(&mut line);
        if !self.item.is_empty() {
            line.push(' ');
            line.push_str(&self.item);
        }
        line
    }

    /// Append the active phase to `line`, unless it just repeats the title (a
    /// single-phase run names the title as its own phase).
    fn push_context(&self, line: &mut String) {
        if !self.label.is_empty() && self.label != self.title {
            line.push(' ');
            line.push_str(&self.label);
        }
    }

    /// Redraw the progress line in place; only called when stderr is a TTY.
    fn draw_tty(&mut self) {
        let line = self.render_tty();
        let width = line.chars().count();
        // The cursor now sits past the padding, so the stale-text bound is the
        // padded width, not just this line's width; the next (possibly shorter)
        // line must clear all of it.
        let (padding, line_len) = erase_widths(self.line_len, width);
        let mut stderr = std::io::stderr().lock();
        if !self.cursor_hidden {
            // Hide the cursor while the bar owns the line, so it does not dance
            // over each redraw. Restored by `clear`.
            let _ = write!(stderr, "\x1b[?25l");
            self.cursor_hidden = true;
        }
        let _ = write!(stderr, "\r{line}");
        if padding > 0 {
            let _ = write!(stderr, "{:width$}", "", width = padding);
        }
        let _ = stderr.flush();
        self.line_len = line_len;
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

impl Drop for Progress {
    fn drop(&mut self) {
        // A run that returns early (an error) never reaches `finish`, so the
        // bar is erased here to keep the error message readable.
        self.clear();
    }
}

/// The padding and new stale-text bound for drawing a `width`-wide line over a
/// previous line of `previous` columns.
///
/// A line shorter than the previous one is padded out to `previous`; because
/// the cursor then sits at the padded width, that width (not the line's own) is
/// what the next frame must clear.
fn erase_widths(previous: usize, width: usize) -> (usize, usize) {
    let padding = previous.saturating_sub(width);
    (padding, width + padding)
}

/// The bar the terminal reporter draws for an overall fraction in
/// `0..=SCALE`, e.g. `[##########--------------]`, `BAR_WIDTH` characters wide
/// including both brackets.
fn format_bar(scaled: u64) -> String {
    let cells = BAR_WIDTH - 2;
    let filled = (u128::from(scaled.min(SCALE)) * cells as u128 / u128::from(SCALE)) as usize;
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
    fn bars_fill_proportionally_and_clamp_at_both_ends() {
        let empty: String = std::iter::repeat_n('-', BAR_WIDTH - 2).collect();
        let full: String = std::iter::repeat_n('#', BAR_WIDTH - 2).collect();
        assert_eq!(format_bar(0), format!("[{empty}]"));
        assert_eq!(
            format_bar(SCALE / 2),
            format!("[{}{}]", "#".repeat(11), "-".repeat(11))
        );
        assert_eq!(format_bar(SCALE), format!("[{full}]"));
        assert_eq!(format_bar(SCALE + 1), format!("[{full}]"));
    }

    #[test]
    fn redraws_keep_the_stale_text_bound() {
        // A shorter line pads out to the previous width, and the bound stays at
        // that padded width so the next frame still clears the whole tail.
        assert_eq!(erase_widths(74, 40), (34, 74));
        assert_eq!(erase_widths(74, 60), (14, 74));
        // A longer line needs no padding and raises the bound.
        assert_eq!(erase_widths(50, 60), (0, 60));
        assert_eq!(erase_widths(0, 40), (0, 40));
    }

    #[test]
    fn terminal_lines_spread_phases_over_one_bar() {
        let mut progress = Progress::new("convert-game", 4);
        progress.phases_done = 1;
        progress.label = "roomcut".to_string();
        progress.total = 10;
        progress.done = 5;
        progress.item = "roomcut/100_000.bmp".to_string();

        // (1 + 5/10) / 4 = 37.5%, rounded down to 37%.
        let line = progress.render_tty();
        assert!(line.starts_with("convert-game: "), "{line}");
        assert!(line.contains(" 37% roomcut "), "{line}");
        assert!(line.ends_with("roomcut/100_000.bmp"), "{line}");
    }

    #[test]
    fn a_single_phase_run_is_its_own_bar() {
        let mut progress = Progress::new("extract", 1);
        progress.label = "extract".to_string();
        progress.total = 10;
        progress.done = 3;

        let line = progress.render_tty();
        assert!(line.starts_with("extract: "), "{line}");
        assert!(line.contains(" 30%"), "{line}");
        assert!(
            !line.contains(" extract"),
            "the phase repeats the title: {line}"
        );
    }

    #[test]
    fn terminal_lines_stay_on_one_line() {
        let mut progress = Progress::new("convert-game", 30);
        progress.label = "roomcut".to_string();
        progress.total = 100;
        progress.done = 1;
        progress.item = "roomcut/".to_string() + &"nested/".repeat(20) + "100_000.bmp";

        let line = progress.render_tty();
        assert_eq!(line.chars().count(), TTY_MAX_CHARS, "{line}");
        assert!(line.ends_with("100_000.bmp"), "{line}");
        assert!(!line.chars().any(char::is_control), "{line}");
    }

    #[test]
    fn plain_lines_carry_the_phase_and_item() {
        let mut progress = Progress::new("convert-game", 10);
        progress.label = "room".to_string();
        progress.total = 10;
        progress.item = "room/1000.rdt".to_string();

        let line = progress.render_plain();
        assert!(line.starts_with("convert-game: 0% "), "{line}");
        assert!(line.ends_with("room room/1000.rdt"), "{line}");

        progress.done = 3;
        let line = progress.render_plain();
        assert!(line.contains(" 3% "), "{line}");
        assert!(line.contains(" room "), "{line}");

        progress.phases_done = 10;
        let line = progress.render_plain();
        assert!(line.starts_with("convert-game: 100% "), "{line}");
    }

    #[test]
    fn end_phase_counts_phases_and_clamps_at_the_total() {
        let mut progress = Progress::new("convert-game", 2);
        progress.begin("a", 4);
        progress.end_phase();
        assert_eq!(progress.phases_done(), 1);
        progress.begin("b", 4);
        progress.end_phase();
        assert_eq!(progress.phases_done(), 2);
        progress.begin("c", 4);
        progress.end_phase();
        assert_eq!(progress.phases_done(), 2, "never past the declared total");
    }

    #[test]
    fn empty_phases_still_advance_the_overall_bar() {
        let mut progress = Progress::new("convert-game", 2);
        progress.begin("empty", 0);
        progress.end_phase();
        assert_eq!(progress.phases_done(), 1);
        assert_eq!(progress.overall_scaled(), SCALE / 2);
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
