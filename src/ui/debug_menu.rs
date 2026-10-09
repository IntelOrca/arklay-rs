//! The port-only debug room-select overlay.
//!
//! This is a development aid the retail game never shipped. F1 while playing
//! freezes the room and opens a scrollable list of the rooms present in the
//! pack (`room/%04x.rdt`, deduped to the stage/room form and sorted). Up/down
//! move the cursor, confirm jumps to the selected room and F1 or cancel closes
//! the overlay. The overlay is always available (`--debug-menu` is accepted
//! for compatibility only). The engine drives the overlay like the pause menu
//! (it shares the session's frozen frame and performs the jump itself) rather
//! than running it as a boxed [`crate::ui::Screen`] modal.

use std::collections::BTreeMap;

use crate::font::Font;
use crate::pack::Pack;
use crate::render::{Framebuffer, Tint};
use crate::state::RoomId;

use super::UiInput;

/// Rows the panel shows before it scrolls.
pub const VISIBLE_ROWS: usize = 9;
/// One row's height in pixels (the font's glyph height).
const ROW_HEIGHT: i32 = crate::font::GLYPH_HEIGHT;
/// The dark panel's rectangle.
const PANEL: [i32; 4] = [24, 16, 272, 208];
/// Left edge of the list's cursor.
const CURSOR_X: i32 = 36;
/// Left edge of a row's room id.
const TEXT_X: i32 = 52;
/// First row's baseline.
const ROWS_Y: i32 = 48;
/// Cursor glyph from the shared font sheet.
const CURSOR_GLYPH: u8 = 0x26;

/// What one tick of the overlay decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugMenuEvent {
    /// The input was consumed inside the overlay.
    None,
    /// Close the overlay and resume the frozen room.
    Close,
    /// Load `RoomId` and place the player.
    Jump(RoomId),
}

/// The overlay's live state: the pack's rooms and the scroll cursor.
pub struct DebugMenu {
    rooms: Vec<RoomId>,
    /// Index of the session's current room, when the pack carries it.
    current: Option<usize>,
    cursor: usize,
    scroll: usize,
}

impl DebugMenu {
    /// Enumerate the pack's rooms and open on the current one.
    pub fn open(pack: &Pack, current: RoomId) -> Self {
        let rooms = room_entries(pack, current.player_flag);
        let index = rooms
            .iter()
            .position(|id| id.stage == current.stage && id.room == current.room);
        Self {
            rooms,
            current: index,
            cursor: index.unwrap_or(0),
            scroll: 0,
        }
    }

    /// The enumerated rooms, sorted and deduped.
    pub fn rooms(&self) -> &[RoomId] {
        &self.rooms
    }

    /// The cursor's index into [`Self::rooms`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// One tick of edge-triggered overlay input.
    pub fn handle_input(&mut self, input: UiInput) -> DebugMenuEvent {
        if input.debug_menu || input.cancel {
            return DebugMenuEvent::Close;
        }
        if self.rooms.is_empty() {
            return DebugMenuEvent::None;
        }
        if input.up {
            self.move_cursor(-1);
        } else if input.down {
            self.move_cursor(1);
        } else if input.confirm {
            return DebugMenuEvent::Jump(self.rooms[self.cursor]);
        }
        DebugMenuEvent::None
    }

    /// Move the cursor and keep it inside the visible window.
    fn move_cursor(&mut self, delta: i32) {
        let count = self.rooms.len() as i32;
        self.cursor = (self.cursor as i32 + delta).rem_euclid(count) as usize;
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + VISIBLE_ROWS {
            self.scroll = self.cursor + 1 - VISIBLE_ROWS;
        }
    }

    /// Draw the dark panel, the room list and the key hints. Without a font
    /// the panel still dims the frozen frame.
    pub fn draw(&mut self, framebuffer: &mut Framebuffer, font: Option<&Font>) {
        framebuffer.blend_black_rect(PANEL, 200);
        let Some(font) = font else {
            return;
        };
        let [panel_x, panel_y, _, _] = PANEL;
        let title = encode_ascii("DEBUG MENU");
        font.draw_text_plain(
            framebuffer,
            panel_x + 12,
            panel_y + 8,
            Tint::White,
            2,
            &title,
        );
        for (index, id) in self
            .rooms
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(VISIBLE_ROWS)
        {
            let y = ROWS_Y + (index - self.scroll) as i32 * ROW_HEIGHT;
            if index == self.cursor {
                font.draw_text_plain(framebuffer, CURSOR_X, y, Tint::White, 2, &[CURSOR_GLYPH]);
            }
            let text = encode_ascii(&id.room3());
            let tint = if Some(index) == self.current {
                Tint::Green
            } else {
                Tint::White
            };
            font.draw_text_plain(framebuffer, TEXT_X, y, tint, 2, &text);
        }
        let hints = ["UP/DOWN: MOVE", "ENTER: JUMP", "F1/ESC: CLOSE"];
        for (line, hint) in hints.iter().enumerate() {
            let bytes = encode_ascii(hint);
            font.draw_text_plain(
                framebuffer,
                panel_x + 12,
                panel_y + 164 + line as i32 * ROW_HEIGHT,
                Tint::Grey,
                2,
                &bytes,
            );
        }
    }
}

/// Enumerate the pack's `room/%04x.rdt` entries: sorted by stage/room and
/// deduped to one [`RoomId`] per room, picking `preferred_player`'s RDT
/// variant when the pack carries it and the lowest digit otherwise.
pub fn room_entries(pack: &Pack, preferred_player: u8) -> Vec<RoomId> {
    let mut rooms: BTreeMap<(u8, u8), Vec<u8>> = BTreeMap::new();
    for path in pack.paths() {
        let Some(hex) = path
            .strip_prefix("room/")
            .and_then(|name| name.strip_suffix(".rdt"))
        else {
            continue;
        };
        if hex.len() != 4 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(id) = RoomId::parse(hex) else {
            continue;
        };
        let players = rooms.entry((id.stage, id.room)).or_default();
        if !players.contains(&id.player_flag) {
            players.push(id.player_flag);
        }
    }
    rooms
        .into_iter()
        .map(|((stage, room), mut players)| {
            players.sort_unstable();
            let player_flag = if players.contains(&preferred_player) {
                preferred_player
            } else {
                players[0]
            };
            RoomId {
                stage,
                room,
                player_flag,
            }
        })
        .collect()
}

/// Encode one line of plain ASCII into the shared font's glyph bytes: letters
/// and digits use the font's ASCII grid, with the punctuation the overlays
/// need. The period uses the shipped sheet's own period cell (0x17); the USA
/// sheet's period index (0x9D) lands on a kana cell on the Japanese-width
/// sheet, so it would not stay legible.
pub fn encode_ascii(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| match c {
            'A'..='Z' => 0x1D + (c as u8 - b'A'),
            'a'..='z' => 0x3D + (c as u8 - b'a'),
            '0'..='9' => 0x0C + (c as u8 - b'0'),
            ' ' => 0x00,
            ':' => 0x16,
            '.' => 0x17,
            '-' => 0x3B,
            '/' => 0x38,
            _ => 0x1B,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::PackWriter;

    fn pack_with(paths: &[&str]) -> Pack {
        let mut writer = PackWriter::new();
        for path in paths {
            writer.add(path, vec![0u8; 4]).unwrap();
        }
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    fn ids(rooms: &[RoomId]) -> Vec<(u8, u8, u8)> {
        rooms
            .iter()
            .map(|id| (id.stage, id.room, id.player_flag))
            .collect()
    }

    #[test]
    fn room_entries_enumerate_sort_and_dedupe() {
        let pack = pack_with(&[
            "room/11c0.rdt",
            "room/1061.rdt",
            "room/1060.rdt",
            "room/1000.rdt",
            "room/zzzz.rdt",
            "room/100.rdt",
            "room/1060.txt",
            "bgm/013.wav",
        ]);
        // The current player's variant wins the dedupe.
        assert_eq!(
            ids(&room_entries(&pack, 1)),
            [(1, 0x00, 0), (1, 0x06, 1), (1, 0x1C, 0)]
        );
        assert_eq!(
            ids(&room_entries(&pack, 0)),
            [(1, 0x00, 0), (1, 0x06, 0), (1, 0x1C, 0)]
        );
    }

    #[test]
    fn room_entries_fall_back_to_the_lowest_variant() {
        let pack = pack_with(&["room/2053.rdt", "room/2052.rdt", "room/2050.rdt"]);
        assert_eq!(ids(&room_entries(&pack, 1)), [(2, 0x05, 0)]);
    }

    #[test]
    fn the_overlay_opens_on_the_current_room_and_wraps() {
        let pack = pack_with(&["room/1000.rdt", "room/1010.rdt", "room/1020.rdt"]);
        let current = RoomId::parse("1010").unwrap();
        let mut menu = DebugMenu::open(&pack, current);
        assert_eq!(menu.cursor(), 1, "opens on the current room");

        let up = UiInput {
            up: true,
            ..UiInput::default()
        };
        assert_eq!(menu.handle_input(up), DebugMenuEvent::None);
        assert_eq!(menu.cursor(), 0, "up wraps");
        assert_eq!(menu.handle_input(up), DebugMenuEvent::None);
        assert_eq!(menu.cursor(), 2, "and wraps again");

        let down = UiInput {
            down: true,
            ..UiInput::default()
        };
        assert_eq!(menu.handle_input(down), DebugMenuEvent::None);
        assert_eq!(menu.cursor(), 0);

        let confirm = UiInput {
            confirm: true,
            ..UiInput::default()
        };
        assert_eq!(
            menu.handle_input(confirm),
            DebugMenuEvent::Jump(RoomId::parse("1000").unwrap())
        );

        let cancel = UiInput {
            cancel: true,
            ..UiInput::default()
        };
        assert_eq!(menu.handle_input(cancel), DebugMenuEvent::Close);
        let f1 = UiInput {
            debug_menu: true,
            ..UiInput::default()
        };
        assert_eq!(menu.handle_input(f1), DebugMenuEvent::Close);
    }

    #[test]
    fn encode_ascii_uses_the_font_grid() {
        assert_eq!(encode_ascii("106"), [0x0D, 0x0C, 0x12]);
        assert_eq!(encode_ascii("A B"), [0x1D, 0x00, 0x1E]);
        assert_eq!(
            encode_ascii("F1/ESC: X"),
            [0x22, 0x0D, 0x38, 0x21, 0x2F, 0x1F, 0x16, 0x00, 0x34]
        );
        assert_eq!(encode_ascii("a z."), [0x3D, 0x00, 0x56, 0x17]);
    }
}
