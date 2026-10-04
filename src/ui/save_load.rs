//! The load-screen picker.
//!
//! This is the slice-4 seam for `Title -> SaveLoad(load) -> Game`: it scans
//! `savedat1.dat`..`savedat8.dat`, lists the occupied slots and loads the
//! chosen one. The full save/load screen (typed reveal, overwrite confirm,
//! save-count digits, location names) is the save screen's own slice and
//! replaces this layout; the column constants below are already the ones that
//! screen uses (`countX 0x6F`, `locX 0x99`, header `0x61`, cursor `0x29`).

use anyhow::Result;

use crate::bmp;
use crate::font::Tint;
use crate::render::Framebuffer;
use crate::save::{self, SAVE_SLOT_COUNT, SaveSlotInfo};
use crate::state::Image;

use super::{Screen, ScreenAction, ScreenResult, UiContext, UiInput};

/// Horizontal position of the screen header (and the slot text).
const HEADER_X: i32 = 0x61;
/// Vertical position of the header row.
const HEADER_Y: i32 = 32;
/// Vertical position of the first slot row.
const FIRST_ROW_Y: i32 = 56;
/// Vertical pitch of the rows.
const ROW_HEIGHT: i32 = 16;
/// Horizontal position of the cursor mark.
const CURSOR_X: i32 = 0x29;
/// Width of the cursor mark.
const CURSOR_W: i32 = 8;
/// Height of the cursor mark.
const CURSOR_H: i32 = 12;
/// The exit row's index (one past the slot rows).
const EXIT_ROW: usize = SAVE_SLOT_COUNT;

/// Which part of the screen is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    FadeOut,
}

/// The load screen's state.
pub struct SaveLoadScreen {
    background: Option<Image>,
    slots: [Option<SaveSlotInfo>; SAVE_SLOT_COUNT],
    /// `0..SAVE_SLOT_COUNT` are the slot rows, [`EXIT_ROW`] is the exit row.
    cursor: usize,
    fade: u8,
    ticks: u32,
    stage: Stage,
    exit: Option<ScreenAction>,
}

impl SaveLoadScreen {
    /// An empty screen; call [`Screen::open`] to scan the save directory.
    pub fn new() -> Self {
        Self {
            background: None,
            slots: [None; SAVE_SLOT_COUNT],
            cursor: 0,
            fade: 255,
            ticks: 0,
            stage: Stage::Idle,
            exit: None,
        }
    }

    /// The row the cursor is on (`0..8` slot, `8` exit).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Move the cursor, wrapping through the exit row.
    fn move_cursor(&mut self, delta: i32) {
        let rows = (SAVE_SLOT_COUNT + 1) as i32;
        self.cursor = (self.cursor as i32 + delta).rem_euclid(rows) as usize;
    }

    /// The first occupied slot, if any.
    fn first_filled(&self) -> Option<usize> {
        self.slots.iter().position(Option::is_some)
    }

    /// Confirm the current row.
    fn confirm(&mut self) {
        if self.cursor == EXIT_ROW {
            self.exit = Some(ScreenAction::Title);
            self.stage = Stage::FadeOut;
            return;
        }
        if self.slots.get(self.cursor).copied().flatten().is_none() {
            // An empty slot refuses, exactly like the title's disabled LOAD.
            return;
        }
        self.exit = Some(ScreenAction::LoadGame { slot: self.cursor });
        self.stage = Stage::FadeOut;
    }
}

impl Default for SaveLoadScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen for SaveLoadScreen {
    fn open(&mut self, cx: &mut UiContext<'_>) -> Result<()> {
        self.background = match cx.pack.read("ui/type00.bmp") {
            Ok(bytes) => match bmp::decode(bytes) {
                Ok(image) => Some(image),
                Err(err) => {
                    eprintln!("warning: invalid load background: {err:#}");
                    None
                }
            },
            Err(err) => {
                eprintln!("warning: missing load background: {err:#}");
                None
            }
        };
        self.slots = save::scan_slots(cx.save_dir);
        self.cursor = self.first_filled().unwrap_or(EXIT_ROW);
        self.fade = 255;
        self.ticks = 0;
        self.stage = Stage::Idle;
        self.exit = None;
        Ok(())
    }

    fn update(&mut self, _cx: &UiContext<'_>, input: UiInput) -> ScreenResult {
        self.ticks = self.ticks.saturating_add(1);
        match self.stage {
            Stage::Idle => {
                self.fade = self.fade.saturating_sub(8);
                if input.up {
                    self.move_cursor(-1);
                }
                if input.down {
                    self.move_cursor(1);
                }
                if input.confirm {
                    self.confirm();
                } else if input.cancel {
                    self.exit = Some(ScreenAction::Title);
                    self.stage = Stage::FadeOut;
                }
            }
            Stage::FadeOut => {
                self.fade = self.fade.saturating_add(16);
                if self.fade == 255 {
                    return ScreenResult::Done(self.exit.take().unwrap_or(ScreenAction::Title));
                }
            }
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
            self.draw_cursor(framebuffer);
            return;
        };
        let save = text.save();
        font.draw_text(
            framebuffer,
            HEADER_X,
            HEADER_Y,
            Tint::White,
            2,
            save.header(1),
        );
        let characters = [save.char_name(0), save.char_name(1)];
        for (index, slot) in self.slots.iter().enumerate() {
            let Some(info) = slot else {
                continue;
            };
            let name = characters[usize::from(info.character & 1)];
            let y = FIRST_ROW_Y + index as i32 * ROW_HEIGHT;
            font.draw_text(framebuffer, HEADER_X, y, Tint::White, 2, name);
        }
        let y = FIRST_ROW_Y + EXIT_ROW as i32 * ROW_HEIGHT;
        font.draw_text(framebuffer, HEADER_X, y, Tint::White, 2, save.exit(1));
        self.draw_cursor(framebuffer);
    }

    fn fade(&self) -> u8 {
        self.fade
    }
}

impl SaveLoadScreen {
    /// Draw the blinking row cursor.
    fn draw_cursor(&self, framebuffer: &mut Framebuffer) {
        let y = FIRST_ROW_Y + self.cursor as i32 * ROW_HEIGHT;
        let color = if (self.ticks & 0x10) == 0 {
            [255, 255, 255, 255]
        } else {
            [128, 128, 128, 255]
        };
        framebuffer.fill_rect([CURSOR_X, y, CURSOR_W, CURSOR_H], color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn neutral() -> UiInput {
        UiInput::default()
    }

    fn context(dir: &std::path::Path) -> UiContext<'_> {
        let path =
            std::env::temp_dir().join(format!("arklay-load-test-{}.akpak", std::process::id()));
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

    #[test]
    fn empty_slots_refuse_and_exit_returns_to_title() {
        let dir =
            std::env::temp_dir().join(format!("arklay-load-test-{}-empty", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut screen = SaveLoadScreen::new();
        let mut cx = context(&dir);
        screen.open(&mut cx).unwrap();
        assert_eq!(screen.cursor(), EXIT_ROW, "no saves starts on EXIT");

        screen.move_cursor(-1);
        assert_eq!(screen.cursor(), SAVE_SLOT_COUNT - 1);
        screen.confirm();
        assert_eq!(screen.stage, Stage::Idle, "an empty slot refuses");

        screen.cursor = EXIT_ROW;
        screen.confirm();
        assert_eq!(screen.stage, Stage::FadeOut);
        let mut result = ScreenResult::Continue;
        for _ in 0..64 {
            result = screen.update(&cx, neutral());
            if result != ScreenResult::Continue {
                break;
            }
        }
        assert_eq!(result, ScreenResult::Done(ScreenAction::Title));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_occupied_slot_loads() {
        let dir =
            std::env::temp_dir().join(format!("arklay-load-test-{}-filled", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = crate::save::SaveFile {
            character: 1,
            ..crate::save::SaveFile::default()
        };
        crate::save::save(&dir, 2, &file).unwrap();

        let mut screen = SaveLoadScreen::new();
        let mut cx = context(&dir);
        screen.open(&mut cx).unwrap();
        assert_eq!(screen.cursor(), 2, "the cursor starts on the only save");

        screen.confirm();
        assert_eq!(screen.stage, Stage::FadeOut);
        let mut result = ScreenResult::Continue;
        for _ in 0..64 {
            result = screen.update(&cx, neutral());
            if result != ScreenResult::Continue {
                break;
            }
        }
        assert_eq!(
            result,
            ScreenResult::Done(ScreenAction::LoadGame { slot: 2 })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
