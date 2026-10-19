//! ROOM SELECT: the pack's rooms in one column per stage.
//!
//! Up/down move inside a column and left/right between columns; both axes
//! scroll (three columns fit the panel). Each row shows the room's hex index
//! and its name clipped to the column; the footer spells the cursor row's full
//! `room3` id and name. Confirm jumps through the session's `debug_jump`, in
//! the RDT variant of the pending character when the pack carries it.

use std::collections::BTreeMap;

use super::{BODY_Y, PANEL_RIGHT, Pen, ROW_HEIGHT, Step, TEXT_LEFT};
use crate::pack::Pack;
use crate::render::Tint;
use crate::state::RoomId;
use crate::ui::UiInput;

/// One column per stage digit (1-7).
pub const STAGE_COLUMNS: usize = RoomId::MAX_STAGE as usize;
/// Columns the panel shows before it scrolls horizontally.
pub const VISIBLE_COLUMNS: usize = 3;
/// Rows a column shows before it scrolls vertically.
pub const VISIBLE_ROWS: usize = 8;
/// One column's width in pixels.
const COLUMN_WIDTH: i32 = (PANEL_RIGHT - TEXT_LEFT) / VISIBLE_COLUMNS as i32;
/// The column labels' language keys, by stage digit.
pub const STAGE_LABELS: [&str; STAGE_COLUMNS] = [
    "UI_DEBUG_STAGE_1",
    "UI_DEBUG_STAGE_2",
    "UI_DEBUG_STAGE_3",
    "UI_DEBUG_STAGE_4",
    "UI_DEBUG_STAGE_5",
    "UI_DEBUG_STAGE_6",
    "UI_DEBUG_STAGE_7",
];

/// One stage/room the pack carries and the RDT player variants it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomVariants {
    /// Stage digit (1-7).
    pub stage: u8,
    /// Room index.
    pub room: u8,
    /// The `player_flag` digits present, sorted.
    pub players: Vec<u8>,
}

impl RoomVariants {
    /// The room id in `preferred`'s variant when the pack carries it, else the
    /// lowest digit.
    pub fn id(&self, preferred: u8) -> RoomId {
        let player_flag = if self.players.contains(&preferred) {
            preferred
        } else {
            self.players[0]
        };
        RoomId {
            stage: self.stage,
            room: self.room,
            player_flag,
        }
    }
}

/// Enumerate the pack's `room/%04x.rdt` entries, sorted by stage/room with
/// every room's player variants collected.
pub fn room_variants(pack: &Pack) -> Vec<RoomVariants> {
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
            RoomVariants {
                stage,
                room,
                players,
            }
        })
        .collect()
}

/// Enumerate the pack's rooms deduped to one [`RoomId`] per room, picking
/// `preferred_player`'s RDT variant when the pack carries it and the lowest
/// digit otherwise.
pub fn room_entries(pack: &Pack, preferred_player: u8) -> Vec<RoomId> {
    room_variants(pack)
        .iter()
        .map(|room| room.id(preferred_player))
        .collect()
}

/// The ROOM SELECT tool's state.
pub struct RoomSelect {
    /// The rooms of stage `index + 1`.
    columns: Vec<Vec<RoomVariants>>,
    /// The session's room when the menu opened.
    current: (u8, u8),
    column: usize,
    rows: [usize; STAGE_COLUMNS],
    scroll_rows: [usize; STAGE_COLUMNS],
    scroll_column: usize,
}

impl RoomSelect {
    /// Split the pack's rooms into stage columns and put the cursor on
    /// `current`.
    pub fn new(pack: &Pack, current: RoomId) -> Self {
        let mut columns = vec![Vec::new(); STAGE_COLUMNS];
        for room in room_variants(pack) {
            if let Some(column) = columns.get_mut(usize::from(room.stage).wrapping_sub(1)) {
                column.push(room);
            }
        }
        let mut select = Self {
            columns,
            current: (current.stage, current.room),
            column: 0,
            rows: [0; STAGE_COLUMNS],
            scroll_rows: [0; STAGE_COLUMNS],
            scroll_column: 0,
        };
        let column = usize::from(current.stage).wrapping_sub(1);
        if let Some(row) = select
            .columns
            .get(column)
            .and_then(|rooms| rooms.iter().position(|room| room.room == current.room))
        {
            select.column = column;
            select.rows[column] = row;
            select.scroll_into_view();
        }
        select
    }

    /// The cursor's column (stage digit minus one).
    pub fn column(&self) -> usize {
        self.column
    }

    /// The cursor's row inside its column.
    pub fn row(&self) -> usize {
        self.rows[self.column]
    }

    /// The first column drawn.
    pub fn scroll_column(&self) -> usize {
        self.scroll_column
    }

    /// The first row drawn in the cursor's column.
    pub fn scroll_row(&self) -> usize {
        self.scroll_rows[self.column]
    }

    /// The room under the cursor (`None` in an empty column).
    pub fn selected(&self) -> Option<&RoomVariants> {
        self.columns[self.column].get(self.rows[self.column])
    }

    /// One tick of input: the jump target in `character`'s variant on confirm.
    pub(super) fn handle_input(&mut self, input: UiInput, character: u8) -> Step {
        if input.cancel {
            return Step::Back;
        }
        if input.left {
            self.column = (self.column + STAGE_COLUMNS - 1) % STAGE_COLUMNS;
        } else if input.right {
            self.column = (self.column + 1) % STAGE_COLUMNS;
        } else if input.up || input.down {
            let count = self.columns[self.column].len();
            if count > 0 {
                let row = &mut self.rows[self.column];
                *row = if input.up {
                    (*row + count - 1) % count
                } else {
                    (*row + 1) % count
                };
            }
        } else if input.confirm
            && let Some(room) = self.selected()
        {
            return Step::Event(super::DebugMenuEvent::Jump(room.id(character)));
        }
        self.scroll_into_view();
        Step::Stay
    }

    /// Keep the cursor's column and row inside the visible window.
    fn scroll_into_view(&mut self) {
        if self.column < self.scroll_column {
            self.scroll_column = self.column;
        } else if self.column >= self.scroll_column + VISIBLE_COLUMNS {
            self.scroll_column = self.column + 1 - VISIBLE_COLUMNS;
        }
        let row = self.rows[self.column];
        let scroll = &mut self.scroll_rows[self.column];
        if row < *scroll {
            *scroll = row;
        } else if row >= *scroll + VISIBLE_ROWS {
            *scroll = row + 1 - VISIBLE_ROWS;
        }
    }

    /// Draw the visible columns and the footer.
    pub(super) fn draw(&self, pen: &mut Pen) {
        let glyph_w = pen.font.metrics.glyph_w;
        for (slot, column) in (self.scroll_column..STAGE_COLUMNS)
            .take(VISIBLE_COLUMNS)
            .enumerate()
        {
            let x = TEXT_LEFT + slot as i32 * COLUMN_WIDTH;
            let label_tint = if column == self.column {
                Tint::Yellow
            } else {
                Tint::Grey
            };
            let label = pen.text.ui(STAGE_LABELS[column]);
            pen.fit(x, BODY_Y, label_tint, label, COLUMN_WIDTH - 4);
            let rooms = &self.columns[column];
            if rooms.is_empty() {
                let empty = pen.text.ui("UI_DEBUG_NO_ROOMS");
                pen.fit(x + 10, BODY_Y + 16, Tint::Grey, empty, COLUMN_WIDTH - 14);
                continue;
            }
            let scroll = self.scroll_rows[column];
            for (index, room) in rooms.iter().enumerate().skip(scroll).take(VISIBLE_ROWS) {
                let y = BODY_Y + 16 + (index - scroll) as i32 * ROW_HEIGHT;
                if column == self.column && index == self.rows[column] {
                    pen.cursor(x, y);
                }
                let current = (room.stage, room.room) == self.current;
                let tint = if current { Tint::Green } else { Tint::White };
                pen.ascii(x + 10, y, tint, &format!("{:02X}", room.room));
                let name_x = x + 10 + 2 * glyph_w + 4;
                if let Some(name) = pen.text.room_name(&room3(room)) {
                    let tint = if current { Tint::Green } else { Tint::Grey };
                    pen.fit(name_x, y, tint, name, x + COLUMN_WIDTH - 2 - name_x);
                }
            }
        }
        // The footer: the cursor row's full id and untruncated name.
        let footer_y = BODY_Y + 16 + VISIBLE_ROWS as i32 * ROW_HEIGHT + 4;
        if let Some(room) = self.selected() {
            let id = room3(room);
            pen.ascii(TEXT_LEFT, footer_y, Tint::Yellow, &id);
            if let Some(name) = pen.text.room_name(&id) {
                let x = TEXT_LEFT + 4 * glyph_w;
                pen.fit(x, footer_y, Tint::White, name, PANEL_RIGHT - x);
            }
        }
    }
}

/// The `room3` id (`"106"`) of a room.
fn room3(room: &RoomVariants) -> String {
    room.id(0).room3()
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

    fn press(f: impl FnOnce(&mut UiInput)) -> UiInput {
        let mut input = UiInput::default();
        f(&mut input);
        input
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

    fn column_pack() -> Pack {
        let mut paths: Vec<String> = (0..12u8)
            .map(|room| format!("room/1{room:02x}0.rdt"))
            .collect();
        paths.extend([
            "room/2000.rdt".into(),
            "room/2001.rdt".into(),
            "room/2011.rdt".into(),
        ]);
        paths.push("room/7000.rdt".into());
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        pack_with(&refs)
    }

    #[test]
    fn rooms_split_into_stage_columns_and_open_on_the_current_room() {
        let pack = column_pack();
        let select = RoomSelect::new(&pack, RoomId::parse("2010").unwrap());
        assert_eq!(select.columns[0].len(), 12);
        assert_eq!(select.columns[1].len(), 2);
        assert!(select.columns[2].is_empty());
        assert_eq!(select.columns[6].len(), 1);
        assert_eq!((select.column(), select.row()), (1, 1));
        let room = select.selected().unwrap();
        assert_eq!(
            (room.stage, room.room, room.players.clone()),
            (2, 1, vec![1])
        );
    }

    #[test]
    fn both_axes_wrap_and_scroll() {
        let pack = column_pack();
        let mut select = RoomSelect::new(&pack, RoomId::parse("1000").unwrap());
        assert_eq!((select.column(), select.row()), (0, 0));
        // Up wraps to the bottom of the twelve-room column and scrolls it.
        assert_eq!(select.handle_input(press(|i| i.up = true), 0), Step::Stay);
        assert_eq!(select.row(), 11);
        assert_eq!(select.scroll_row(), 11 + 1 - VISIBLE_ROWS);
        // Down wraps back to the top.
        select.handle_input(press(|i| i.down = true), 0);
        assert_eq!((select.row(), select.scroll_row()), (0, 0));
        // Right walks the stages; the fourth column scrolls the window.
        for _ in 0..3 {
            select.handle_input(press(|i| i.right = true), 0);
        }
        assert_eq!((select.column(), select.scroll_column()), (3, 1));
        // Left from the first column wraps to stage 7.
        for _ in 0..4 {
            select.handle_input(press(|i| i.left = true), 0);
        }
        assert_eq!((select.column(), select.scroll_column()), (6, 4));
        // The first column remembers its row.
        select.handle_input(press(|i| i.right = true), 0);
        assert_eq!((select.column(), select.row()), (0, 0));
    }

    #[test]
    fn confirm_jumps_in_the_requested_variant() {
        let pack = column_pack();
        let mut select = RoomSelect::new(&pack, RoomId::parse("2000").unwrap());
        let confirm = press(|i| i.confirm = true);
        // Room 200 has both variants: the character picks.
        assert_eq!(
            select.handle_input(confirm, 1),
            Step::Event(super::super::DebugMenuEvent::Jump(
                RoomId::parse("2001").unwrap()
            ))
        );
        assert_eq!(
            select.handle_input(confirm, 0),
            Step::Event(super::super::DebugMenuEvent::Jump(
                RoomId::parse("2000").unwrap()
            ))
        );
        // Room 201 only has the Jill variant.
        select.handle_input(press(|i| i.down = true), 0);
        assert_eq!(
            select.handle_input(confirm, 0),
            Step::Event(super::super::DebugMenuEvent::Jump(
                RoomId::parse("2011").unwrap()
            ))
        );
        // An empty column consumes confirm.
        select.handle_input(press(|i| i.right = true), 0);
        assert_eq!(select.handle_input(confirm, 0), Step::Stay);
        assert_eq!(
            select.handle_input(press(|i| i.cancel = true), 0),
            Step::Back
        );
    }
}
