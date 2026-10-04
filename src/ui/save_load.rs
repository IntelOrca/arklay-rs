//! The save/load screen.
//!
//! The screen drives the original's per-tick state machine: scan the eight
//! `savedat*.dat` slots, navigate the eight rows plus the exit row with a
//! blinking cursor, confirm an overwrite in save mode, write the block and
//! play the typed reveal (`name/count/location`, two bytes per step), or load
//! the chosen slot. It serves both modes: the title's load path opens it in
//! load mode, and a room typewriter opens it in save mode over the frozen
//! gameplay frame.
//!
//! Column positions are the Japanese glyph grid's: the save-count digits at
//! `countX 0x6F`, the location at `locX 0x99`, the header at `0x61` and the
//! slot cursor one glyph cell left of the text at `0x29`. The reveal string is
//! assembled exactly like the original's: three name cells, a separator, the
//! two count digits, a separator and the eight-cell location name.

use anyhow::Result;

use crate::bmp;
use crate::font::{Font, Tint};
use crate::game::InventoryItem;
use crate::items;
use crate::render::Framebuffer;
use crate::save::{self, PLAYER_SLOT_COUNT, SAVE_SLOT_COUNT, SaveFile, SaveSlotInfo};
use crate::state::Image;

use super::{Screen, ScreenAction, ScreenResult, UiContext, UiInput};

/// Horizontal position of the screen header.
const HEADER_X: i32 = 0x61;
/// Vertical position of the header row.
const HEADER_Y: i32 = 13;
/// Horizontal position of a slot row's text.
const ROW_X: i32 = 55;
/// Vertical position of the first slot row.
const ROW_Y: i32 = 45;
/// Vertical pitch of the rows.
const ROW_HEIGHT: i32 = 16;
/// Horizontal position of the save-count digits.
const COUNT_X: i32 = 0x6F;
/// Horizontal position of the location name.
const LOC_X: i32 = 0x99;
/// Horizontal position of the slot cursor.
const CURSOR_X: i32 = 0x29;
/// Horizontal position of the overwrite-confirm cursor.
const CONFIRM_X: i32 = 118;
/// Vertical position of the confirm cursor.
const CONFIRM_Y: i32 = 209;
/// Cursor step between YES and NO (four Japanese glyph cells).
const CONFIRM_STRIDE: i32 = 56;
/// Horizontal position of the overwrite prompt and error lines.
const PROMPT_X: i32 = 49;
/// Vertical position of the overwrite prompt.
const PROMPT_Y: i32 = 193;
/// Vertical position of the error lines.
const ERROR_Y: i32 = 209;
/// Second error line, one row below the first.
const ERROR_Y2: i32 = 225;
/// The exit row's index (one past the slot rows).
const EXIT_ROW: usize = SAVE_SLOT_COUNT;
/// Vertical position of the exit row.
const EXIT_Y: i32 = ROW_Y + SAVE_SLOT_COUNT as i32 * ROW_HEIGHT;
/// Bytes of the three-cell character name the reveal copies.
const NAME_LEN: usize = 6;
/// Bytes of the eight-cell location name (terminator included).
const LOC_LEN: usize = 17;
/// Bytes of the assembled reveal string.
const REVEAL_LEN: usize = NAME_LEN + 8 + LOC_LEN;
/// Ticks between cursor-blink toggles.
const BLINK_TICKS: u8 = 5;
/// Ticks between reveal steps.
const REVEAL_TICKS: u8 = 5;
/// The reveal slice grows two bytes per step.
const REVEAL_STEP: usize = 2;
/// Black-overlay step of the screen fade-in and fade-out.
const FADE_STEP: u8 = 8;
/// Faster black-overlay step once the screen leaves.
const FADE_OUT_STEP: u8 = 16;
/// Font glyph byte of the slash separator (cell 2 of row 2).
const SEPARATOR: u8 = 0x38;
/// Font glyph byte of the blinking cursor sprite (cell 2 of row 2).
const CURSOR_GLYPH: u8 = 0x26;

/// Which mode the screen runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveLoadMode {
    /// Save mode: writes the block and plays the reveal.
    Save,
    /// Load mode: returns the chosen slot.
    Load,
}

/// A completed save: the slot that was written and whether the flow consumed
/// an ink ribbon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveOutcome {
    /// Zero-based slot that was written.
    pub slot: usize,
    /// Whether one ink ribbon must be consumed from the live game state.
    pub ink_ribbon: bool,
}

/// Which part of the screen is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Cursor navigation and slot selection.
    Idle,
    /// The save-mode overwrite confirmation.
    ConfirmOverwrite,
    /// The typed reveal of a just-written slot.
    SaveAnim,
    /// A failed write, waiting for a key.
    Error,
    /// Fade to black, then report [`SaveLoadScreen::exit`].
    FadeOut,
}

/// The in-memory block a save-mode screen writes.
struct PendingSave {
    file: SaveFile,
    ink_ribbon: bool,
}

/// The typed reveal of a just-written slot.
struct Reveal {
    /// The assembled `name/count/location` bytes.
    bytes: Vec<u8>,
    /// How many bytes of `bytes` are currently revealed.
    length: usize,
    /// Ticks until the next step.
    timer: u8,
}

impl Reveal {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            length: REVEAL_STEP,
            timer: REVEAL_TICKS,
        }
    }

    /// Whether the reveal has shown everything the original shows. The last
    /// printed slice is `REVEAL_LEN - 1` bytes: the final pass's check steps
    /// past the string before it draws the terminator-only slice.
    fn finished(&self) -> bool {
        self.length + REVEAL_STEP + REVEAL_STEP > REVEAL_LEN + 1
    }
}

/// The save/load screen's state.
pub struct SaveLoadScreen {
    mode: SaveLoadMode,
    background: Option<Image>,
    slots: [Option<SaveSlotInfo>; SAVE_SLOT_COUNT],
    /// The block a save-mode screen writes.
    pending: Option<PendingSave>,
    /// `0..SAVE_SLOT_COUNT` are the slot rows, [`EXIT_ROW`] is the exit row.
    cursor: usize,
    stage: Stage,
    blink_timer: u8,
    blink_state: bool,
    /// `0` YES, `1` NO in the overwrite confirmation.
    confirm_choice: u8,
    reveal: Option<Reveal>,
    outcome: Option<SaveOutcome>,
    exit: Option<ScreenAction>,
    fade: u8,
    ticks: u32,
}

impl SaveLoadScreen {
    /// A load-mode screen.
    pub fn load() -> Self {
        Self::new(SaveLoadMode::Load, None)
    }

    /// A save-mode screen writing `file`; `ink_ribbon` makes the write consume
    /// one ribbon from the block.
    pub fn save(file: SaveFile, ink_ribbon: bool) -> Self {
        Self::new(SaveLoadMode::Save, Some(PendingSave { file, ink_ribbon }))
    }

    fn new(mode: SaveLoadMode, pending: Option<PendingSave>) -> Self {
        Self {
            mode,
            background: None,
            slots: [None; SAVE_SLOT_COUNT],
            pending,
            cursor: 0,
            stage: Stage::Idle,
            blink_timer: BLINK_TICKS,
            blink_state: false,
            confirm_choice: 0,
            reveal: None,
            outcome: None,
            exit: None,
            fade: 255,
            ticks: 0,
        }
    }

    /// Which mode the screen runs in.
    pub fn mode(&self) -> SaveLoadMode {
        self.mode
    }

    /// The row the cursor is on (`0..8` slot, `8` exit).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The current slot scan.
    pub fn slots(&self) -> &[Option<SaveSlotInfo>; SAVE_SLOT_COUNT] {
        &self.slots
    }

    /// The assembled reveal bytes of the last save, if any.
    pub fn reveal(&self) -> Option<&[u8]> {
        self.reveal.as_ref().map(|reveal| reveal.bytes.as_slice())
    }

    /// How many reveal bytes are drawn at the moment.
    pub fn reveal_len(&self) -> usize {
        self.reveal.as_ref().map_or(0, |reveal| reveal.length)
    }

    /// The completed save, once the screen left through a write.
    pub fn take_outcome(&mut self) -> Option<SaveOutcome> {
        self.outcome.take()
    }

    /// Move the cursor, wrapping through the exit row.
    fn move_cursor(&mut self, delta: i32) {
        let rows = (SAVE_SLOT_COUNT + 1) as i32;
        self.cursor = (self.cursor as i32 + delta).rem_euclid(rows) as usize;
        self.blink_state = false;
        self.blink_timer = BLINK_TICKS;
    }

    fn tick_blink(&mut self) {
        if self.blink_timer == 0 {
            self.blink_state = !self.blink_state;
            self.blink_timer = BLINK_TICKS;
        } else {
            self.blink_timer -= 1;
        }
    }

    /// Confirm the current row.
    fn confirm(&mut self, cx: &UiContext<'_>) {
        if self.cursor == EXIT_ROW {
            self.exit = Some(match self.mode {
                SaveLoadMode::Save => ScreenAction::Resume,
                SaveLoadMode::Load => ScreenAction::Title,
            });
            self.stage = Stage::FadeOut;
            return;
        }
        let occupied = self.slots.get(self.cursor).copied().flatten().is_some();
        match self.mode {
            SaveLoadMode::Load => {
                // An empty slot refuses.
                if occupied {
                    self.exit = Some(ScreenAction::LoadGame { slot: self.cursor });
                    self.stage = Stage::FadeOut;
                }
            }
            SaveLoadMode::Save => {
                if occupied {
                    self.confirm_choice = 0;
                    self.blink_state = true;
                    self.blink_timer = BLINK_TICKS;
                    self.stage = Stage::ConfirmOverwrite;
                } else {
                    self.begin_save(cx);
                }
            }
        }
    }

    /// Leave the screen through cancel: back to gameplay in save mode, back to
    /// the title in load mode.
    fn cancel(&mut self) {
        self.exit = Some(match self.mode {
            SaveLoadMode::Save => ScreenAction::Resume,
            SaveLoadMode::Load => ScreenAction::Title,
        });
        self.stage = Stage::FadeOut;
    }

    /// Write the pending block to the cursor's slot and start the reveal.
    fn begin_save(&mut self, cx: &UiContext<'_>) {
        let Some(pending) = self.pending.as_ref() else {
            self.stage = Stage::Error;
            return;
        };
        let mut file = pending.file.clone();
        if pending.ink_ribbon {
            consume_snapshot_ribbon(&mut file);
        }
        match save::save(cx.save_dir, self.cursor, &file) {
            Ok(()) => {
                self.slots[self.cursor] = Some(SaveSlotInfo::from_file(&file));
                self.outcome = Some(SaveOutcome {
                    slot: self.cursor,
                    ink_ribbon: pending.ink_ribbon,
                });
                let name = cx
                    .text
                    .map(|text| {
                        text.save()
                            .char_name(usize::from(file.character & 1))
                            .to_vec()
                    })
                    .unwrap_or_default();
                let location = cx
                    .text
                    .map(|text| {
                        text.save()
                            .location(location_index(file.stage, file.room))
                            .to_vec()
                    })
                    .unwrap_or_default();
                self.reveal = Some(Reveal::new(build_reveal(&name, file.saves, &location)));
                self.stage = Stage::SaveAnim;
            }
            Err(err) => {
                eprintln!("warning: failed to save slot {}: {err:#}", self.cursor + 1);
                self.stage = Stage::Error;
            }
        }
    }
}

impl Default for SaveLoadScreen {
    fn default() -> Self {
        Self::load()
    }
}

impl Screen for SaveLoadScreen {
    fn open(&mut self, cx: &mut UiContext<'_>) -> Result<()> {
        self.background = match cx.pack.read("ui/type00.bmp") {
            Ok(bytes) => match bmp::decode(bytes) {
                Ok(image) => Some(image),
                Err(err) => {
                    eprintln!("warning: invalid save/load background: {err:#}");
                    None
                }
            },
            Err(err) => {
                eprintln!("warning: missing save/load background: {err:#}");
                None
            }
        };
        self.slots = save::scan_slots(cx.save_dir);
        self.cursor = 0;
        self.stage = Stage::Idle;
        self.blink_timer = BLINK_TICKS;
        self.blink_state = false;
        self.confirm_choice = 0;
        self.reveal = None;
        self.outcome = None;
        self.exit = None;
        self.fade = 255;
        self.ticks = 0;
        Ok(())
    }

    fn update(&mut self, cx: &UiContext<'_>, input: UiInput) -> ScreenResult {
        self.ticks = self.ticks.saturating_add(1);
        if self.stage == Stage::FadeOut {
            self.fade = self.fade.saturating_add(FADE_OUT_STEP);
            if self.fade == 255 {
                return ScreenResult::Done(self.exit.take().unwrap_or(ScreenAction::Resume));
            }
            return ScreenResult::Continue;
        }
        self.fade = self.fade.saturating_sub(FADE_STEP);
        self.tick_blink();
        match self.stage {
            Stage::Idle => {
                if input.up {
                    self.move_cursor(-1);
                }
                if input.down {
                    self.move_cursor(1);
                }
                if input.confirm {
                    self.confirm(cx);
                } else if input.cancel {
                    self.cancel();
                }
            }
            Stage::ConfirmOverwrite => {
                if input.left {
                    self.confirm_choice = 0;
                    self.blink_state = false;
                    self.blink_timer = BLINK_TICKS;
                }
                if input.right {
                    self.confirm_choice = 1;
                    self.blink_state = false;
                    self.blink_timer = BLINK_TICKS;
                }
                if input.confirm {
                    if self.confirm_choice == 1 {
                        self.stage = Stage::Idle;
                        self.blink_state = false;
                        self.blink_timer = BLINK_TICKS;
                    } else {
                        self.begin_save(cx);
                    }
                } else if input.cancel {
                    self.stage = Stage::Idle;
                    self.blink_state = false;
                    self.blink_timer = BLINK_TICKS;
                }
            }
            Stage::SaveAnim => {
                let Some(reveal) = self.reveal.as_mut() else {
                    self.exit = Some(ScreenAction::Resume);
                    self.stage = Stage::FadeOut;
                    return ScreenResult::Continue;
                };
                if reveal.timer == 0 {
                    if reveal.finished() {
                        self.exit = Some(ScreenAction::Resume);
                        self.stage = Stage::FadeOut;
                    } else {
                        reveal.length += REVEAL_STEP;
                        reveal.timer = REVEAL_TICKS;
                    }
                } else {
                    reveal.timer -= 1;
                }
            }
            Stage::Error => {
                if input.confirm || input.cancel || input.any {
                    self.stage = Stage::Idle;
                    self.blink_state = false;
                    self.blink_timer = BLINK_TICKS;
                }
            }
            Stage::FadeOut => {}
        }
        ScreenResult::Continue
    }

    fn draw(&mut self, cx: &UiContext<'_>, framebuffer: &mut Framebuffer) {
        match &self.background {
            Some(background) => framebuffer.blit(background),
            None => {
                framebuffer.clear();
                framebuffer.fill_rect(
                    [0, 0, framebuffer.width as i32, framebuffer.height as i32],
                    [0, 0, 0, 255],
                );
            }
        }
        let (Some(font), Some(text)) = (cx.font, cx.text) else {
            self.draw_cursor(
                framebuffer,
                cx.font,
                CURSOR_X,
                ROW_Y + self.cursor as i32 * ROW_HEIGHT,
            );
            return;
        };
        let save_strings = text.save();
        let mode = match self.mode {
            SaveLoadMode::Save => 0,
            SaveLoadMode::Load => 1,
        };

        // The header is the original's formatted path: no controller-symbol
        // remap, so the suffix's M stays the letter M.
        font.draw_text_plain(
            framebuffer,
            HEADER_X,
            HEADER_Y,
            Tint::White,
            2,
            save_strings.header(mode),
        );
        font.draw_text_plain(
            framebuffer,
            HEADER_X,
            HEADER_Y,
            Tint::White,
            2,
            &save_strings.header_suffix,
        );

        for (index, slot) in self.slots.iter().enumerate() {
            let y = ROW_Y + index as i32 * ROW_HEIGHT;
            if self.stage == Stage::SaveAnim && self.cursor == index {
                continue;
            }
            match slot {
                Some(info) => {
                    font.draw_text(
                        framebuffer,
                        ROW_X,
                        y,
                        Tint::White,
                        2,
                        &save_strings.filled_slot,
                    );
                    font.draw_text(
                        framebuffer,
                        ROW_X,
                        y,
                        Tint::White,
                        2,
                        save_strings.char_name(usize::from(info.character & 1)),
                    );
                    let count = count_bytes(info.saves);
                    font.draw_text(framebuffer, COUNT_X, y, Tint::White, 2, &count);
                    font.draw_text(
                        framebuffer,
                        LOC_X,
                        y,
                        Tint::White,
                        2,
                        save_strings.location(location_index(info.stage, info.room)),
                    );
                }
                None => font.draw_text(
                    framebuffer,
                    ROW_X,
                    y,
                    Tint::White,
                    2,
                    &save_strings.empty_slot,
                ),
            }
        }

        font.draw_text(
            framebuffer,
            ROW_X,
            EXIT_Y,
            Tint::White,
            2,
            save_strings.exit(mode),
        );
        font.draw_text(
            framebuffer,
            ROW_X,
            EXIT_Y,
            Tint::White,
            2,
            &save_strings.exit_suffix,
        );

        if let Some(reveal) = &self.reveal {
            let end = reveal.length.min(reveal.bytes.len());
            font.draw_text(
                framebuffer,
                ROW_X,
                ROW_Y + self.cursor as i32 * ROW_HEIGHT,
                Tint::White,
                2,
                &reveal.bytes[..end],
            );
        }

        if self.stage == Stage::ConfirmOverwrite {
            font.draw_text(
                framebuffer,
                PROMPT_X,
                PROMPT_Y,
                Tint::White,
                2,
                &save_strings.overwrite_prompt,
            );
            font.draw_text(
                framebuffer,
                CONFIRM_X,
                CONFIRM_Y,
                Tint::White,
                2,
                &save_strings.yes_no,
            );
        }
        if self.stage == Stage::Error {
            font.draw_text(
                framebuffer,
                PROMPT_X,
                ERROR_Y,
                Tint::White,
                2,
                save_strings.error(0),
            );
            font.draw_text(
                framebuffer,
                PROMPT_X,
                ERROR_Y2,
                Tint::White,
                2,
                save_strings.error(1),
            );
        }

        let row_y = ROW_Y + self.cursor as i32 * ROW_HEIGHT;
        if self.stage == Stage::ConfirmOverwrite {
            // The slot cursor stays visible while the confirm cursor blinks.
            self.draw_cursor(framebuffer, cx.font, CURSOR_X, row_y);
            if !self.blink_state {
                self.draw_cursor(
                    framebuffer,
                    cx.font,
                    CONFIRM_X + i32::from(self.confirm_choice) * CONFIRM_STRIDE,
                    CONFIRM_Y,
                );
            }
        } else if !self.blink_state {
            self.draw_cursor(framebuffer, cx.font, CURSOR_X, row_y);
        }
    }

    fn fade(&self) -> u8 {
        self.fade
    }
}

impl SaveLoadScreen {
    /// Draw the blinking row cursor.
    fn draw_cursor(&self, framebuffer: &mut Framebuffer, font: Option<&Font>, x: i32, y: i32) {
        let Some(font) = font else {
            framebuffer.fill_rect([x, y, 8, 12], [255, 255, 255, 255]);
            return;
        };
        font.draw_text(framebuffer, x, y, Tint::White, 2, &[CURSOR_GLYPH]);
    }
}

/// The original's `GetSaveLocationIndex`: the stage digit picks a location
/// name, with the main hall, the mansion storeroom and the courtyard path
/// overriding it. The save block stores the engine's 1-based stage.
pub fn location_index(stage: u8, room: u8) -> usize {
    let zero = stage.saturating_sub(1) % 5;
    let mut index = usize::from(zero);
    if zero == 0 {
        if room == 0x06 {
            index = 1;
        }
        if index == 0 && room == 0x18 {
            index = 5;
        }
    }
    if zero == 2 && room == 0x07 {
        index = 6;
    }
    index
}

/// The save-count digits as the original's `%02d` line: tens, units, each in
/// its own cell with a `0xFB` pad.
fn count_bytes(saves: u8) -> [u8; 4] {
    let saves = saves % 100;
    [0x0C + (saves / 10) % 10, 0xFB, 0x0C + saves % 10, 0xFB]
}

/// Assemble the reveal string: name, separator, count digits, separator,
/// location.
fn build_reveal(name: &[u8], saves: u8, location: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(REVEAL_LEN);
    push_padded(&mut bytes, name, NAME_LEN);
    bytes.push(SEPARATOR);
    bytes.push(0xFB);
    bytes.extend_from_slice(&count_bytes(saves)[..2]);
    bytes.extend_from_slice(&count_bytes(saves)[2..4]);
    bytes.push(SEPARATOR);
    bytes.push(0xFB);
    push_padded(&mut bytes, location, LOC_LEN);
    bytes
}

/// Extend `out` with `src` up to `len` bytes, zero-padding a short source.
fn push_padded(out: &mut Vec<u8>, src: &[u8], len: usize) {
    let taken = src.len().min(len);
    out.extend_from_slice(&src[..taken]);
    out.resize(out.len() + len - taken, 0);
}

/// Remove one ink ribbon from the block the screen is about to write, the way
/// the original's `use_room_action_item` does before assembling the file.
fn consume_snapshot_ribbon(file: &mut SaveFile) {
    let Some(index) = file
        .player_slots
        .iter()
        .position(|stack| stack.id == items::ITEM_INK_RIBBONS && stack.quantity > 0)
    else {
        return;
    };
    file.used_item = items::ITEM_INK_RIBBONS;
    file.player_slots[index].quantity -= 1;
    if file.player_slots[index].quantity == 0 {
        for read in index + 1..PLAYER_SLOT_COUNT {
            file.player_slots[read - 1] = file.player_slots[read];
        }
        file.player_slots[PLAYER_SLOT_COUNT - 1] = InventoryItem::default();
        file.total_held = file.total_held.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    fn neutral() -> UiInput {
        UiInput::default()
    }

    fn context(dir: &Path) -> UiContext<'_> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "arklay-load-test-{}-{serial}.akpak",
            std::process::id()
        ));
        let mut writer = crate::pack::PackWriter::new();
        writer.add("unused", Vec::new()).unwrap();
        writer.write(&path).unwrap();
        let pack = crate::pack::Pack::open(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        // The context borrows the directory and a leaked pack; both live long
        // enough for one test.
        let pack: &'static crate::pack::Pack = Box::leak(Box::new(pack));
        UiContext {
            pack,
            save_dir: dir,
            font: None,
            text: None,
            ticks: 0,
        }
    }

    /// Self-deleting temporary directory unique to this process and label.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("arklay-load-{}-{label}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn run_to_result(
        screen: &mut SaveLoadScreen,
        cx: &UiContext<'_>,
        input: UiInput,
        ticks: u32,
    ) -> ScreenResult {
        let mut result = screen.update(cx, input);
        for _ in 0..ticks {
            if result != ScreenResult::Continue {
                return result;
            }
            result = screen.update(cx, neutral());
        }
        result
    }

    #[test]
    fn empty_slots_refuse_and_exit_returns_to_title() {
        let dir = TempDir::new("empty");
        let mut screen = SaveLoadScreen::load();
        let mut cx = context(&dir.path);
        screen.open(&mut cx).unwrap();
        assert_eq!(screen.cursor(), 0, "the original starts on slot 1");
        assert!(screen.slots().iter().all(Option::is_none));

        screen.confirm(&cx);
        assert_eq!(screen.stage, Stage::Idle, "an empty slot refuses");

        screen.cursor = EXIT_ROW;
        screen.confirm(&cx);
        assert_eq!(screen.stage, Stage::FadeOut);
        let result = run_to_result(&mut screen, &cx, neutral(), 64);
        assert_eq!(result, ScreenResult::Done(ScreenAction::Title));
    }

    #[test]
    fn load_mode_cancel_returns_to_title_and_confirm_returns_the_slot() {
        let dir = TempDir::new("load");
        let file = SaveFile {
            character: 1,
            stage: 1,
            room: 1,
            ..SaveFile::default()
        };
        save::save(&dir.path, 2, &file).unwrap();

        let mut screen = SaveLoadScreen::load();
        let mut cx = context(&dir.path);
        screen.open(&mut cx).unwrap();
        assert_eq!(screen.cursor(), 0);

        // An empty slot refuses.
        screen.confirm(&cx);
        assert_eq!(screen.stage, Stage::Idle);

        screen.cursor = 2;
        screen.confirm(&cx);
        assert_eq!(screen.stage, Stage::FadeOut);
        let result = run_to_result(&mut screen, &cx, neutral(), 64);
        assert_eq!(
            result,
            ScreenResult::Done(ScreenAction::LoadGame { slot: 2 })
        );

        screen.open(&mut cx).unwrap();
        screen.update(
            &cx,
            UiInput {
                cancel: true,
                ..neutral()
            },
        );
        let result = run_to_result(&mut screen, &cx, neutral(), 64);
        assert_eq!(result, ScreenResult::Done(ScreenAction::Title));
    }

    #[test]
    fn overwrite_confirm_yes_and_no_flow() {
        let dir = TempDir::new("overwrite");
        save::save(&dir.path, 0, &SaveFile::default()).unwrap();
        let mut screen = SaveLoadScreen::save(SaveFile::default(), false);
        let mut cx = context(&dir.path);
        screen.open(&mut cx).unwrap();

        // Confirm the occupied slot: the dialog opens with YES selected and
        // the cursor hidden.
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(screen.stage, Stage::ConfirmOverwrite);
        assert_eq!(screen.confirm_choice, 0);
        assert!(screen.blink_state);

        // NO with the right arrow, confirm: back to navigation, nothing saved.
        screen.update(
            &cx,
            UiInput {
                right: true,
                ..neutral()
            },
        );
        assert_eq!(screen.confirm_choice, 1);
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(screen.stage, Stage::Idle);
        assert!(screen.take_outcome().is_none());

        // Cancel out of the dialog.
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(screen.stage, Stage::ConfirmOverwrite);
        screen.update(
            &cx,
            UiInput {
                cancel: true,
                ..neutral()
            },
        );
        assert_eq!(screen.stage, Stage::Idle);

        // Confirm the overwrite with YES: the write starts and the outcome is
        // recorded. (The default choice is YES.)
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(screen.stage, Stage::SaveAnim);
        assert_eq!(
            screen.take_outcome(),
            Some(SaveOutcome {
                slot: 0,
                ink_ribbon: false
            })
        );
        assert!(save::read_slot_info(&dir.path, 0).is_some());
    }

    #[test]
    fn saving_an_empty_slot_skips_the_confirm_and_consumes_the_ribbon() {
        let dir = TempDir::new("save-empty");
        let mut file = SaveFile {
            character: 0,
            stage: 1,
            room: 0,
            saves: 4,
            total_held: 1,
            ..SaveFile::default()
        };
        file.player_slots[0] = InventoryItem {
            id: items::ITEM_INK_RIBBONS,
            quantity: 1,
        };
        let mut screen = SaveLoadScreen::save(file, true);
        let mut cx = context(&dir.path);
        screen.open(&mut cx).unwrap();

        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(screen.stage, Stage::SaveAnim);
        assert_eq!(
            screen.take_outcome(),
            Some(SaveOutcome {
                slot: 0,
                ink_ribbon: true
            }),
            "the outcome is available while the reveal runs"
        );

        // The block was written without the ribbon and the slot rescanned.
        let written = save::load(&dir.path, 0).unwrap();
        assert_eq!(written.total_held, 0);
        assert_eq!(written.player_slots[0].id, 0);
        assert_eq!(written.used_item, items::ITEM_INK_RIBBONS);
        assert!(screen.slots()[0].is_some());

        // The reveal starts two bytes long and finishes back in gameplay.
        assert_eq!(screen.reveal_len(), REVEAL_STEP);
        let result = run_to_result(&mut screen, &cx, neutral(), 600);
        assert_eq!(result, ScreenResult::Done(ScreenAction::Resume));
    }

    #[test]
    fn the_reveal_grows_two_bytes_per_step_and_names_the_location() {
        let dir = TempDir::new("reveal");
        let file = SaveFile {
            character: 1,
            stage: 3,
            room: 7,
            saves: 7,
            total_held: 0,
            ..SaveFile::default()
        };
        let mut screen = SaveLoadScreen::save(file, false);
        let mut cx = context(&dir.path);
        screen.open(&mut cx).unwrap();
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(screen.stage, Stage::SaveAnim);

        let reveal = screen.reveal().unwrap().to_vec();
        assert_eq!(reveal.len(), REVEAL_LEN);
        assert_eq!(reveal[6], 0x38, "name/location separator");
        assert_eq!(reveal[7], 0xFB);
        assert_eq!(reveal[8], 0x0C, "count tens digit 0");
        assert_eq!(reveal[9], 0xFB);
        assert_eq!(reveal[10], 0x0C + 7, "count units digit 7");
        assert_eq!(reveal[11], 0xFB);
        assert_eq!(reveal[12], 0x38, "count/location separator");
        assert_eq!(reveal[13], 0xFB);
        assert!(
            reveal[14..].iter().all(|&byte| byte == 0),
            "the test pack has no save strings, so the location is blank-padded"
        );

        // The assembled layout against real-shaped strings.
        let location = [0xF9u8, 0x09, 0x00, 0xFB, 0x01];
        let name = [0xDBu8, 0xFB, 0xCF, 0xFB, 0x00, 0xFB, 0x01];
        let assembled = build_reveal(&name, 7, &location);
        assert_eq!(&assembled[..6], &name[..6]);
        assert_eq!(
            &assembled[6..14],
            &[0x38, 0xFB, 0x0C, 0xFB, 0x13, 0xFB, 0x38, 0xFB]
        );
        assert_eq!(&assembled[14..19], &location);
        assert_eq!(assembled.len(), REVEAL_LEN);
        assert!(assembled[19..].iter().all(|&byte| byte == 0));

        // Steps advance every five ticks.
        let mut lengths = vec![screen.reveal_len()];
        for _ in 0..8 {
            for _ in 0..REVEAL_TICKS + 1 {
                screen.update(&cx, neutral());
            }
            if screen.reveal_len() != *lengths.last().unwrap() {
                lengths.push(screen.reveal_len());
            }
        }
        assert_eq!(&lengths[..4], &[2, 4, 6, 8]);
        assert!(
            lengths.iter().all(|length| length % 2 == 0),
            "the reveal always grows by two bytes"
        );
        assert!(
            lengths.iter().all(|&length| length <= REVEAL_LEN),
            "the reveal never runs past the string"
        );
    }

    #[test]
    fn count_digits_and_location_names_follow_the_original_tables() {
        assert_eq!(count_bytes(0), [0x0C, 0xFB, 0x0C, 0xFB]);
        assert_eq!(count_bytes(7), [0x0C, 0xFB, 0x13, 0xFB]);
        assert_eq!(count_bytes(42), [0x10, 0xFB, 0x0E, 0xFB]);
        assert_eq!(count_bytes(107), [0x0C, 0xFB, 0x13, 0xFB]);

        // Mansion 1F, main hall, storeroom, courtyard path, guardhouse and lab.
        assert_eq!(location_index(1, 0), 0);
        assert_eq!(location_index(1, 0x06), 1);
        assert_eq!(location_index(1, 0x18), 5);
        assert_eq!(location_index(2, 0), 1);
        assert_eq!(location_index(3, 0x07), 6);
        assert_eq!(location_index(3, 0), 2);
        assert_eq!(location_index(4, 0), 3);
        assert_eq!(location_index(5, 0), 4);
        assert_eq!(location_index(6, 0), 0);
    }

    #[test]
    fn save_mode_cancel_returns_to_gameplay_without_writing() {
        let dir = TempDir::new("save-cancel");
        let mut screen = SaveLoadScreen::save(SaveFile::default(), true);
        let mut cx = context(&dir.path);
        screen.open(&mut cx).unwrap();

        screen.update(
            &cx,
            UiInput {
                cancel: true,
                ..neutral()
            },
        );
        let result = run_to_result(&mut screen, &cx, neutral(), 64);
        assert_eq!(result, ScreenResult::Done(ScreenAction::Resume));
        assert!(screen.take_outcome().is_none());
        assert!(save::read_slot_info(&dir.path, 0).is_none());
    }

    #[test]
    fn a_failed_write_shows_the_error_line_until_a_key() {
        let base = TempDir::new("save-error");
        // A file where the save directory should be makes the write fail.
        let dir = base.path.join("saves");
        std::fs::write(&dir, b"not a directory").unwrap();
        let mut screen = SaveLoadScreen::save(SaveFile::default(), false);
        let mut cx = context(&dir);
        screen.open(&mut cx).unwrap();
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(screen.stage, Stage::Error);
        assert!(screen.take_outcome().is_none());

        screen.update(
            &cx,
            UiInput {
                any: true,
                ..neutral()
            },
        );
        assert_eq!(screen.stage, Stage::Idle);
    }

    #[test]
    fn the_jpn_columns_match_the_documented_layout() {
        assert_eq!(COUNT_X, 0x6F);
        assert_eq!(LOC_X, 0x99);
        assert_eq!(HEADER_X, 0x61);
        assert_eq!(CURSOR_X, 0x29);
        assert_eq!(REVEAL_LEN, 31);
        assert_eq!(CONFIRM_STRIDE, 4 * 14);
    }
}
