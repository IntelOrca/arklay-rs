//! CONTROLS: the rebind screen.
//!
//! One row per [`Action`] lists its bindings, then a RESET TO DEFAULTS row.
//! Up/down pick the row and left/right the binding slot, the slot past the
//! last binding being the "add" slot. Confirm captures the next input of any
//! device into the slot (replacing a binding, or appending on the add slot);
//! START clears the slot's binding. A source bound to more than one action is
//! drawn red and the status line names the other actions. Every change is
//! reported to the engine, which applies it at once and saves it to
//! `arklay.toml`.
//!
//! A capture takes the next press whatever it is (Escape included), so it can
//! only be abandoned by waiting it out ([`CAPTURE_TICKS`]) or with F1, which
//! closes the whole menu. A change that would leave an action the screen
//! itself needs ([`Action::is_essential`]) without a keyboard key of its own
//! is refused ([`Bindings::lockout`]), so the screen can never lock itself
//! out.

use super::{BODY_Y, PANEL_RIGHT, Pen, ROW_HEIGHT, Step, TEXT_LEFT};
use crate::controls::{ACTIONS, Action, Bindings};
use crate::render::Tint;
use crate::ui::UiInput;

/// Rows the list shows before it scrolls.
pub const VISIBLE_ROWS: usize = 8;
/// The list's rows: every action, then RESET TO DEFAULTS.
const ROWS: usize = ACTIONS.len() + 1;
/// The status line's baseline, under the list.
const STATUS_Y: i32 = BODY_Y + VISIBLE_ROWS as i32 * ROW_HEIGHT + 4;
/// How long a capture waits for an input, in menu ticks (~5 s at the 16 ms
/// menu rate).
pub const CAPTURE_TICKS: u32 = 300;

/// A one-shot note the status line shows until the next input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    /// The captured input is already bound to this action.
    Duplicate,
    /// The change would leave an essential action without a key of its own.
    KeepOne,
    /// The captured input has no name it could be saved under.
    Unsaveable,
    /// The bindings were reset to the defaults.
    Reset,
}

impl Note {
    fn key(self) -> &'static str {
        match self {
            Note::Duplicate => "UI_CONTROLS_DUPLICATE",
            Note::KeepOne => "UI_CONTROLS_KEEP_ONE",
            Note::Unsaveable => "UI_CONTROLS_UNSAVEABLE",
            Note::Reset => "UI_CONTROLS_RESET_DONE",
        }
    }
}

/// The rebind screen's state over its own copy of the bindings.
#[derive(Debug, Clone)]
pub struct ControlsEditor {
    bindings: Bindings,
    row: usize,
    slot: usize,
    top: usize,
    /// Ticks left in an armed capture.
    capture: Option<u32>,
    note: Option<Note>,
}

impl ControlsEditor {
    /// Open on the first row over `bindings`.
    pub fn new(bindings: &Bindings) -> Self {
        Self {
            bindings: bindings.clone(),
            row: 0,
            slot: 0,
            top: 0,
            capture: None,
            note: None,
        }
    }

    /// The edited bindings.
    pub fn bindings(&self) -> &Bindings {
        &self.bindings
    }

    /// The selected `(row, slot)`.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.slot)
    }

    /// Whether a capture is waiting for an input.
    pub fn capturing(&self) -> bool {
        self.capture.is_some()
    }

    /// The status note on show.
    pub fn note(&self) -> Option<Note> {
        self.note
    }

    /// The selected row's action (`None` on RESET TO DEFAULTS).
    fn action(&self) -> Option<Action> {
        ACTIONS.get(self.row).copied()
    }

    /// One tick of input. Returns [`Step::Stay`] or [`Step::Back`], plus
    /// whether the bindings changed.
    pub(super) fn handle_input(&mut self, input: UiInput) -> (Step, bool) {
        if let Some(left) = self.capture {
            return (Step::Stay, self.tick_capture(input, left));
        }
        if input.any {
            self.note = None;
        }
        if input.cancel {
            return (Step::Back, false);
        }
        let slots = self
            .action()
            .map_or(0, |action| self.bindings.get(action).len());
        if input.up {
            self.move_row(ROWS - 1);
        } else if input.down {
            self.move_row(1);
        } else if input.left {
            self.slot = self.slot.saturating_sub(1);
        } else if input.right {
            self.slot = (self.slot + 1).min(slots);
        } else if input.confirm {
            if self.action().is_some() {
                self.capture = Some(CAPTURE_TICKS);
            } else {
                let changed = self.bindings != Bindings::default();
                self.bindings = Bindings::default();
                self.note = Some(Note::Reset);
                return (Step::Stay, changed);
            }
        } else if input.start
            && let Some(action) = self.action()
            && self.slot < slots
        {
            let mut next = self.bindings.clone();
            next.unbind(action, self.slot);
            if next.lockout().is_some() {
                self.note = Some(Note::KeepOne);
            } else {
                self.bindings = next;
                self.slot = self.slot.min(slots - 1);
                return (Step::Stay, true);
            }
        }
        (Step::Stay, false)
    }

    /// One tick of an armed capture: bind the first captured input, or count
    /// down to giving up.
    fn tick_capture(&mut self, input: UiInput, left: u32) -> bool {
        let Some(action) = self.action() else {
            self.capture = None;
            return false;
        };
        let Some(source) = input.captured else {
            self.capture = left.checked_sub(1).filter(|&left| left > 0);
            return false;
        };
        self.capture = None;
        let mut next = self.bindings.clone();
        self.note = if !source.round_trips() {
            Some(Note::Unsaveable)
        } else if !next.bind(action, self.slot, source) {
            Some(Note::Duplicate)
        } else if next.lockout().is_some() {
            Some(Note::KeepOne)
        } else {
            self.bindings = next;
            return true;
        };
        false
    }

    /// Step the row by `delta` (mod the row count), keeping it on screen and
    /// the slot on the new row's "add" slot or before.
    fn move_row(&mut self, delta: usize) {
        self.row = (self.row + delta) % ROWS;
        if self.row < self.top {
            self.top = self.row;
        } else if self.row >= self.top + VISIBLE_ROWS {
            self.top = self.row + 1 - VISIBLE_ROWS;
        }
        let slots = self
            .action()
            .map_or(0, |action| self.bindings.get(action).len());
        self.slot = self.slot.min(slots);
    }

    /// Draw the visible rows and the status line.
    pub(super) fn draw(&self, pen: &mut Pen) {
        let glyph_w = pen.font.metrics.glyph_w;
        let value_x = TEXT_LEFT + 11 * glyph_w;
        for row in self.top..(self.top + VISIBLE_ROWS).min(ROWS) {
            let y = BODY_Y + (row - self.top) as i32 * ROW_HEIGHT;
            if row == self.row {
                pen.cursor(TEXT_LEFT - 16, y);
            }
            let Some(&action) = ACTIONS.get(row) else {
                pen.bytes(TEXT_LEFT, y, Tint::White, pen.text.ui("UI_CONTROLS_RESET"));
                continue;
            };
            pen.fit(
                TEXT_LEFT,
                y,
                Tint::White,
                pen.text.ui(action.label()),
                value_x - TEXT_LEFT - glyph_w,
            );
            self.draw_slots(pen, action, row == self.row, value_x, y);
        }
        self.draw_status(pen);
    }

    /// One row's bindings, each tinted: yellow under the cursor, red when
    /// bound to another action too. The cursor row scrolls so its selected
    /// slot stays visible, and ends in the "+" add slot.
    fn draw_slots(&self, pen: &mut Pen, action: Action, selected: bool, x: i32, y: i32) {
        let sources = self.bindings.get(action);
        let mut labels: Vec<(Vec<u8>, Tint)> = sources
            .iter()
            .enumerate()
            .map(|(slot, &source)| {
                let tint = if selected && slot == self.slot {
                    Tint::Yellow
                } else if !self.bindings.conflicts(action, source).is_empty() {
                    Tint::Red
                } else {
                    Tint::Grey
                };
                (encode(pen, &source.label()), tint)
            })
            .collect();
        if selected {
            let tint = if self.slot == sources.len() {
                Tint::Yellow
            } else {
                Tint::Grey
            };
            labels.push((encode(pen, "+"), tint));
        }
        let gap = pen.font.metrics.glyph_w;
        let budget = PANEL_RIGHT - x;
        let width = |labels: &[(Vec<u8>, Tint)]| -> i32 {
            labels
                .iter()
                .map(|(bytes, _)| pen.font.advance(pen.tight, bytes) + gap)
                .sum()
        };
        let mut first = 0;
        if selected {
            while first < self.slot && width(&labels[first..=self.slot]) > budget {
                first += 1;
            }
        }
        let mut pen_x = x;
        for (bytes, tint) in &labels[first..] {
            let room = PANEL_RIGHT - pen_x;
            if room <= 0 {
                break;
            }
            pen.fit(pen_x, y, *tint, bytes, room);
            pen_x += pen.font.advance(pen.tight, bytes) + gap;
        }
    }

    /// The status line: the capture prompt, a one-shot note, or the selected
    /// binding's conflicts.
    fn draw_status(&self, pen: &mut Pen) {
        let width = PANEL_RIGHT - TEXT_LEFT;
        if let Some(left) = self.capture {
            let mut line = pen.text.ui("UI_CONTROLS_PRESS").to_vec();
            line.extend(encode(pen, &format!(" {}", left.div_ceil(60))));
            pen.fit(TEXT_LEFT, STATUS_Y, Tint::Yellow, &line, width);
            return;
        }
        if let Some(note) = self.note {
            pen.fit(
                TEXT_LEFT,
                STATUS_Y,
                Tint::Green,
                pen.text.ui(note.key()),
                width,
            );
            return;
        }
        let Some(action) = self.action() else {
            return;
        };
        let Some(&source) = self.bindings.get(action).get(self.slot) else {
            return;
        };
        let others = self.bindings.conflicts(action, source);
        if others.is_empty() {
            return;
        }
        let mut line = pen.text.ui("UI_CONTROLS_ALSO").to_vec();
        for (index, other) in others.iter().enumerate() {
            line.extend(encode(pen, if index == 0 { " " } else { ", " }));
            line.extend_from_slice(pen.text.ui(other.label()));
        }
        pen.fit(TEXT_LEFT, STATUS_Y, Tint::Red, &line, width);
    }
}

/// `text` in the pack script's encoding.
fn encode(pen: &Pen, text: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    crate::charset::encode(text, pen.text.script(), &mut bytes);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controls::Source;

    /// Whether `source` is a gamepad input (for the device mix).
    fn is_pad(source: Source) -> bool {
        matches!(source, Source::PadButton(_) | Source::PadAxis(..))
    }

    fn press(f: impl FnOnce(&mut UiInput)) -> UiInput {
        let mut input = UiInput {
            any: true,
            ..UiInput::default()
        };
        f(&mut input);
        input
    }

    /// The input a captured source produces (any action edges included).
    fn captured(source: Source) -> UiInput {
        press(|i| i.captured = Some(source))
    }

    fn go_to(editor: &mut ControlsEditor, action: Action) {
        while editor.action() != Some(action) {
            editor.handle_input(press(|i| i.down = true));
        }
    }

    #[test]
    fn capture_appends_on_the_add_slot_and_replaces_elsewhere() {
        let mut editor = ControlsEditor::new(&Bindings::default());
        go_to(&mut editor, Action::Aim);
        let defaults = Bindings::default().get(Action::Aim).len();
        for _ in 0..10 {
            editor.handle_input(press(|i| i.right = true));
        }
        assert_eq!(editor.cursor().1, defaults, "right stops on the add slot");
        // Confirm arms the capture; that same press is not captured.
        let start = press(|i| {
            i.confirm = true;
            i.captured = Some(Source::Mouse(1));
        });
        assert_eq!(editor.handle_input(start), (Step::Stay, false));
        assert!(editor.capturing());
        // A tick with no input keeps waiting.
        assert_eq!(editor.handle_input(press(|_| {})), (Step::Stay, false));
        assert_eq!(
            editor.handle_input(captured(Source::Mouse(3))),
            (Step::Stay, true)
        );
        assert!(!editor.capturing());
        assert_eq!(
            editor.bindings().get(Action::Aim).last(),
            Some(&Source::Mouse(3))
        );
        assert!(
            editor
                .bindings()
                .get(Action::Aim)
                .iter()
                .any(|&s| is_pad(s))
        );
        // Slot 0 replaced in place.
        editor.handle_input(press(|i| i.left = true));
        editor.handle_input(press(|i| i.left = true));
        editor.handle_input(press(|i| i.left = true));
        assert_eq!(editor.cursor().1, 0);
        editor.handle_input(press(|i| i.confirm = true));
        assert_eq!(
            editor.handle_input(captured(Source::Wheel(crate::controls::Wheel::Up))),
            (Step::Stay, true)
        );
        assert_eq!(
            editor.bindings().get(Action::Aim)[0],
            Source::Wheel(crate::controls::Wheel::Up)
        );
        assert_eq!(editor.bindings().get(Action::Aim).len(), defaults + 1);
    }

    #[test]
    fn a_duplicate_capture_changes_nothing_and_a_capture_times_out() {
        let mut editor = ControlsEditor::new(&Bindings::default());
        let up = Bindings::default().get(Action::Up)[0];
        editor.handle_input(press(|i| i.confirm = true));
        assert_eq!(editor.handle_input(captured(up)), (Step::Stay, false));
        assert_eq!(editor.note(), Some(Note::Duplicate));
        editor.handle_input(press(|i| i.confirm = true));
        for _ in 0..CAPTURE_TICKS {
            assert!(editor.capturing());
            editor.handle_input(UiInput::default());
        }
        assert!(!editor.capturing(), "the capture gives up");
        assert_eq!(editor.bindings(), &Bindings::default());
    }

    #[test]
    fn clearing_keeps_one_essential_binding_and_reset_restores_defaults() {
        let mut editor = ControlsEditor::new(&Bindings::default());
        go_to(&mut editor, Action::Confirm);
        // Space clears; Return, the last key, is refused (pad:a alone would
        // strand a keyboard player).
        assert_eq!(
            editor.handle_input(press(|i| i.start = true)),
            (Step::Stay, true)
        );
        assert_eq!(
            editor.handle_input(press(|i| i.start = true)),
            (Step::Stay, false)
        );
        assert_eq!(editor.note(), Some(Note::KeepOne));
        assert!(
            editor
                .bindings()
                .get(Action::Confirm)
                .iter()
                .any(|s| matches!(s, Source::Key(_)))
        );
        // A non-essential action can be cleared completely.
        go_to(&mut editor, Action::Fire);
        while !editor.bindings().get(Action::Fire).is_empty() {
            assert_eq!(
                editor.handle_input(press(|i| i.start = true)),
                (Step::Stay, true)
            );
        }
        // RESET TO DEFAULTS is the last row, above the first.
        editor.row = 0;
        editor.handle_input(press(|i| i.up = true));
        assert_eq!(editor.action(), None);
        assert_eq!(
            editor.handle_input(press(|i| i.confirm = true)),
            (Step::Stay, true)
        );
        assert_eq!(editor.bindings(), &Bindings::default());
        assert_eq!(editor.note(), Some(Note::Reset));
        assert_eq!(
            editor.handle_input(press(|i| i.cancel = true)),
            (Step::Back, false)
        );
    }

    #[test]
    fn a_capture_that_would_lock_out_or_cannot_be_saved_is_refused() {
        use sdl3_sys::scancode::{SDL_SCANCODE_ESCAPE, SDL_SCANCODE_X};
        let mut editor = ControlsEditor::new(&Bindings::default());
        go_to(&mut editor, Action::Confirm);
        // Space -> Escape still leaves Return as confirm's own key.
        editor.handle_input(press(|i| i.confirm = true));
        let escape = Source::key(SDL_SCANCODE_ESCAPE);
        assert_eq!(editor.handle_input(captured(escape)), (Step::Stay, true));
        // Return -> X would leave confirm only on cancel's keys.
        editor.handle_input(press(|i| i.right = true));
        editor.handle_input(press(|i| i.confirm = true));
        let x = Source::key(SDL_SCANCODE_X);
        assert_eq!(editor.handle_input(captured(x)), (Step::Stay, false));
        assert_eq!(editor.note(), Some(Note::KeepOne));
        assert_eq!(editor.bindings().lockout(), None);
        // SDL's second `Return` scancode would reload as the first.
        editor.handle_input(press(|i| i.confirm = true));
        assert_eq!(
            editor.handle_input(captured(Source::Key(158))),
            (Step::Stay, false)
        );
        assert_eq!(editor.note(), Some(Note::Unsaveable));
    }

    #[test]
    fn the_list_scrolls_to_keep_the_cursor_visible() {
        let mut editor = ControlsEditor::new(&Bindings::default());
        for _ in 0..VISIBLE_ROWS {
            editor.handle_input(press(|i| i.down = true));
        }
        assert_eq!(editor.top, 1);
        editor.handle_input(press(|i| i.down = true));
        editor.handle_input(press(|i| i.down = true));
        editor.handle_input(press(|i| i.down = true));
        editor.handle_input(press(|i| i.down = true));
        editor.handle_input(press(|i| i.down = true));
        assert_eq!(editor.row, 0, "wraps");
        assert_eq!(editor.top, 0);
    }
}
