//! FLAGS: the flag editor.
//!
//! Thirteen groups mirror the reference editor's views
//! (`ref/src/game/DebugMenu.cpp` `s_dbgFlagViews`): one per bank, bank 5
//! split into its two dwords and bank 8 into its visited/map/file blocks. Each
//! row is one `(bank, selector)` bit addressed exactly like the scripts'
//! `ck`/`set` (MSB-first, [`GameState::flag_test`]/[`GameState::apply_flag`]),
//! with its selector, value and a name when one is known.
//!
//! The bit names are a port-only identifier table taken from the reference's
//! `s_dbgMsfNames`/`s_dbgMsf2Names`/`s_dbgMapNames`/visited groups and the
//! audited bits of `ref/docs/SCENARIO_FLAGS.md`; they are debug identifiers,
//! not translated text.

use super::{BODY_Y, PANEL_RIGHT, Pen, ROW_HEIGHT, Step, TEXT_LEFT};
use crate::game::{GameState, ROOM_FLAG_FILE_BASE, ROOM_FLAG_MAP_BASE, STAGE_ROOM_FLAG_OFFSETS};
use crate::render::Tint;
use crate::ui::UiInput;

/// Rows the panel shows before it scrolls.
pub const VISIBLE_ROWS: usize = 8;
/// The documents' first item id: file bit `i` is item `0x5F + i`.
const FILE_FIRST_ITEM: u8 = 0x5F;
/// The number of document bits.
const FILE_COUNT: u16 = 16;
/// The number of map bits.
const MAP_COUNT: u16 = 6;

/// How a group's rows are named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Names {
    /// No known names: raw indices only.
    None,
    /// A sparse `(selector, name)` table.
    Table(&'static [(u8, &'static str)]),
    /// A 32-entry table indexed by `selector - first`.
    Dword(&'static [&'static str; 32]),
    /// Room-visited bits: named by their stage room.
    Visited,
    /// Map bits.
    Maps,
    /// Document bits: named by their item.
    Files,
}

/// One editor group: a window of bits in one bank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlagGroup {
    /// The group label's language key.
    pub label: &'static str,
    /// The `ck`/`set` bank.
    pub bank: u8,
    /// The first selector.
    pub first: u8,
    /// The number of bits.
    pub count: u16,
    names: Names,
}

impl FlagGroup {
    /// The selector of row `row`.
    pub fn selector(&self, row: usize) -> u8 {
        self.first.wrapping_add(row as u8)
    }
}

/// The thirteen groups, in the reference editor's order (minus its unused
/// room-flag tail).
pub const GROUPS: [FlagGroup; 13] = [
    group(
        "UI_DEBUG_FLAGS_SCENARIO",
        0,
        0,
        128,
        Names::Table(SCENARIO_NAMES),
    ),
    group(
        "UI_DEBUG_FLAGS_SCENARIO2",
        1,
        0,
        256,
        Names::Table(SCENARIO2_NAMES),
    ),
    group("UI_DEBUG_FLAGS_LOCKS", 2, 0, 64, Names::None),
    group("UI_DEBUG_FLAGS_ENEMIES", 3, 0, 256, Names::None),
    group("UI_DEBUG_FLAGS_SYSTEM", 4, 0, 64, Names::None),
    group("UI_DEBUG_FLAGS_STATE", 5, 0, 32, Names::Dword(&MSF_NAMES)),
    group(
        "UI_DEBUG_FLAGS_STATE2",
        5,
        32,
        32,
        Names::Dword(&MSF2_NAMES),
    ),
    group("UI_DEBUG_FLAGS_MESSAGE", 6, 0, 32, Names::None),
    group("UI_DEBUG_FLAGS_ROOM_ITEMS", 7, 0, 256, Names::None),
    group(
        "UI_DEBUG_FLAGS_ROOM_VISITED",
        8,
        0,
        ROOM_FLAG_MAP_BASE as u16,
        Names::Visited,
    ),
    group(
        "UI_DEBUG_FLAGS_ROOM_MAPS",
        8,
        ROOM_FLAG_MAP_BASE,
        MAP_COUNT,
        Names::Maps,
    ),
    group(
        "UI_DEBUG_FLAGS_ROOM_FILES",
        8,
        ROOM_FLAG_FILE_BASE,
        FILE_COUNT,
        Names::Files,
    ),
    group("UI_DEBUG_FLAGS_ITEM_USE", 9, 0, 64, Names::None),
];

const fn group(label: &'static str, bank: u8, first: u8, count: u16, names: Names) -> FlagGroup {
    FlagGroup {
        label,
        bank,
        first,
        count,
        names,
    }
}

/// `g_main_state_flags` names by selector (the reference's `s_dbgMsfNames`).
const MSF_NAMES: [&str; 32] = [
    "SCREEN_REBUILD",
    "SCREEN_STANDALONE",
    "FADE_ACTIVE",
    "CONTINUE_GAME",
    "UNUSED_27",
    "ROOM_TRANSITION",
    "GAMEPLAY_ACTIVE",
    "PLAYER_DEAD",
    "CHAR_VARIANT",
    "OPTIONS_REQUEST",
    "UNUSED_21",
    "CAMERA_LOCK",
    "PANNING_RESET",
    "FMV_REQUEST",
    "VOICE_PLAYING",
    "INTENSITY_RAMP",
    "MENU_ACTIVE",
    "SCRIPT_ONLY_14",
    "MODE1_KEY_DEPLET",
    "MODE2_ITEMBOX",
    "MODE3_ITEM_VIEW",
    "MODE4_GOT_ITEM",
    "MODE5_MAP_ITEM",
    "PICKUP_SCREEN",
    "DOOR_TRANSITION",
    "OBJECT_PUSH",
    "CAMERA_REDRAW",
    "LADDER_DOWN",
    "SCRIPT_ONLY_03",
    "CAMERA_DEFER",
    "MIRROR_PLANE_X",
    "MIRROR_ENABLE",
];

/// `g_main_state_flags2` names by `selector - 32` (`s_dbgMsf2Names`).
const MSF2_NAMES: [&str; 32] = [
    "DEATH_VARIANT",
    "",
    "PLAYER_INITIALISED",
    "ATTRACT_DEMO",
    "COUNTDOWN_ACTIVE",
    "COSTUME_VARIANT",
    "ROOM_SPRITES_OFF",
    "DOOR_ANGLE_STEP",
    "SND_BUSY",
    "DOOR_TURN_PENDING",
    "SFX_BANK1_HALF",
    "",
    "PRESERVED_19",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "FADE_NO_DEPTH_CLMP",
    "SCREEN_BORDER",
    "SCREEN_SHAKE",
    "EFFECT_ZONE",
];

/// The audited `g_ScenarioFlags` bits (`ref/docs/SCENARIO_FLAGS.md`).
const SCENARIO_NAMES: &[(u8, &str)] = &[
    (0x00, "STAGE_VARIANT"),
    (0x10, "YAWN_BITE"),
    (0x13, "ITEM_13_GATE"),
    (0x16, "CHEMICAL_MIXED"),
    (0x1E, "PANEL_VARIANT_A"),
    (0x1F, "PANEL_VARIANT_B"),
    (0x20, "INTERACTIVE_SCREEN"),
    (0x21, "PANEL_SOLVED"),
    (0x29, "PLANT42_DEAD"),
    (0x2A, "ALT_OUTFIT"),
    (0x37, "WESKER_LATE_ANIM"),
    (0x47, "YAWN_SERUM"),
    (0x48, "PLANT42_OBJ_ACK"),
    (0x49, "SERUM1_OBJ_ACK"),
    (0x4A, "SERUM2_OBJ_ACK"),
    (0x5B, "MONSTER_PLANT_HITS"),
    (0x7A, "DC_BEST_ENDING"),
    (0x7B, "SECOND_PLAYTHROUGH"),
    (0x7C, "HAS_LOCKPICK"),
    (0x7D, "FADE_MENU_LATCH"),
    (0x7E, "ROCKET_UNLOCK"),
    (0x7F, "HAS_RADIO"),
];

/// The audited `g_ScenarioFlags2` bits (`ref/docs/SCENARIO_FLAGS.md`).
const SCENARIO2_NAMES: &[(u8, &str)] = &[
    (0x0B, "JILL_FIRST_RUN"),
    (0x22, "RADIO_TAB_GATE"),
    (0x23, "JILL_SERUM_OBJ1"),
    (0x24, "JILL_SERUM_OBJ2"),
    (0x2D, "CHRIS_SERUM_OBJ1"),
    (0x2E, "CHRIS_SERUM_OBJ2"),
    (0x38, "PLANT42_OBJ"),
    (0x43, "YAWN_POISONED"),
    (0x48, "BGM_207_A"),
    (0x4B, "SECOND_SURVIVOR"),
    (0x55, "BGM_207_B"),
    (0x5C, "BGM_207_C"),
    (0xC0, "PARTNER_ALIVE"),
];

/// The visited groups' labels by `(stage - 1) % 5` and the map names by map
/// index (`s_dbgVisitedGroups`, `s_dbgMapNames`).
const VISITED_GROUPS: [&str; 5] = [
    "MANSION 1F",
    "MANSION 2F",
    "COURTYARD",
    "GUARDHOUSE",
    "LABORATORY",
];
const MAP_NAMES: [&str; MAP_COUNT as usize] = [
    "MANSION 1F",
    "MANSION 2F",
    "COURTYARD",
    "UNDERGROUND",
    "GUARDHOUSE",
    "LABORATORY",
];

/// A bit's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagName {
    /// Nothing known.
    None,
    /// A fixed identifier.
    Static(&'static str),
    /// A room-visited bit: its visited-group label and the `room3` id of the
    /// group's first stage.
    Room(&'static str, String),
    /// A document bit: the document's item id.
    Item(u8),
}

/// The name of `group`'s selector `sel`.
pub fn flag_name(group: &FlagGroup, sel: u8) -> FlagName {
    match group.names {
        Names::None => FlagName::None,
        Names::Table(table) => table
            .iter()
            .find(|(bit, _)| *bit == sel)
            .map_or(FlagName::None, |(_, name)| FlagName::Static(name)),
        Names::Dword(table) => match table[usize::from(sel.wrapping_sub(group.first) & 31)] {
            "" => FlagName::None,
            name => FlagName::Static(name),
        },
        Names::Visited => {
            // The last group whose base is at or below the bit owns it.
            let Some(index) = (0..VISITED_GROUPS.len())
                .rev()
                .find(|&index| STAGE_ROOM_FLAG_OFFSETS[index] <= sel)
            else {
                return FlagName::None;
            };
            let room = sel - STAGE_ROOM_FLAG_OFFSETS[index];
            FlagName::Room(VISITED_GROUPS[index], format!("{}{room:02X}", index + 1))
        }
        Names::Maps => MAP_NAMES
            .get(usize::from(sel.wrapping_sub(group.first)))
            .map_or(FlagName::None, |name| FlagName::Static(name)),
        Names::Files => FlagName::Item(FILE_FIRST_ITEM + sel.wrapping_sub(group.first)),
    }
}

/// The FLAGS tool's state.
#[derive(Debug, Clone, Default)]
pub struct FlagEditor {
    group: usize,
    rows: [usize; GROUPS.len()],
    scroll: [usize; GROUPS.len()],
}

impl FlagEditor {
    /// The selected group's index into [`GROUPS`].
    pub fn group(&self) -> usize {
        self.group
    }

    /// The cursor row inside the selected group.
    pub fn row(&self) -> usize {
        self.rows[self.group]
    }

    /// The selected group and its cursor selector.
    pub fn selected(&self) -> (FlagGroup, u8) {
        let group = GROUPS[self.group];
        (group, group.selector(self.row()))
    }

    /// One tick of input: rows, groups and the set/clear/toggle edits.
    pub(super) fn handle_input(&mut self, input: UiInput, game: &mut GameState) -> Step {
        if input.cancel {
            return Step::Back;
        }
        let count = GROUPS.len();
        let (group, sel) = self.selected();
        let rows = usize::from(group.count);
        if input.left {
            self.group = (self.group + count - 1) % count;
        } else if input.right {
            self.group = (self.group + 1) % count;
        } else if input.up {
            self.rows[self.group] = (self.row() + rows - 1) % rows;
        } else if input.down {
            self.rows[self.group] = (self.row() + 1) % rows;
        } else if input.page_left {
            game.apply_flag(group.bank, sel, 0);
        } else if input.page_right {
            game.apply_flag(group.bank, sel, 1);
        } else if input.confirm {
            game.apply_flag(group.bank, sel, 2);
        }
        let row = self.rows[self.group];
        let scroll = &mut self.scroll[self.group];
        if row < *scroll {
            *scroll = row;
        } else if row >= *scroll + VISIBLE_ROWS {
            *scroll = row + 1 - VISIBLE_ROWS;
        }
        Step::Stay
    }

    /// Draw the group header and the visible bit rows.
    pub(super) fn draw(&self, pen: &mut Pen, game: &GameState) {
        let group = GROUPS[self.group];
        let header = pen.text.ui(group.label);
        pen.bytes(TEXT_LEFT, BODY_Y, Tint::Yellow, header);
        let position = format!("{}/{}", self.group + 1, GROUPS.len());
        let glyph_w = pen.font.metrics.glyph_w;
        pen.ascii(PANEL_RIGHT - 5 * glyph_w, BODY_Y, Tint::Grey, &position);
        let scroll = self.scroll[self.group];
        for row in (scroll..usize::from(group.count)).take(VISIBLE_ROWS) {
            let y = BODY_Y + 18 + (row - scroll) as i32 * ROW_HEIGHT;
            let sel = group.selector(row);
            let mut name_tint = Tint::White;
            if row == self.row() {
                pen.cursor(TEXT_LEFT - 16, y);
                name_tint = super::SELECTED_TINT;
            }
            let set = game.flag_test(group.bank, sel, false);
            pen.ascii(TEXT_LEFT, y, Tint::Grey, &format!("{sel:02X}"));
            pen.ascii(
                TEXT_LEFT + 3 * glyph_w,
                y,
                if set { Tint::Green } else { Tint::White },
                if set { "1" } else { "0" },
            );
            let name_x = TEXT_LEFT + 5 * glyph_w;
            let width = PANEL_RIGHT - name_x;
            match flag_name(&group, sel) {
                FlagName::None => {}
                FlagName::Static(name) => pen.ascii_fit(name_x, y, name_tint, name, width),
                FlagName::Room(label, room3) => {
                    let name = pen.text.room_name(&room3);
                    match name {
                        Some(name) => {
                            let mut line = Vec::new();
                            crate::charset::encode(
                                &format!("{room3} "),
                                pen.text.script(),
                                &mut line,
                            );
                            line.extend_from_slice(name);
                            pen.fit(name_x, y, name_tint, &line, width);
                        }
                        None => {
                            pen.ascii_fit(name_x, y, name_tint, &format!("{label} {room3}"), width)
                        }
                    }
                }
                FlagName::Item(item) => pen.item_name_fit(name_x, y, name_tint, item, width),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{BANK_SCENARIO2, MSF_CHAR_VARIANT, MSF2_COSTUME_VARIANT};

    fn press(f: impl FnOnce(&mut UiInput)) -> UiInput {
        let mut input = UiInput::default();
        f(&mut input);
        input
    }

    /// The raw byte/mask a selector addresses in a 32-byte bank, MSB-first per
    /// dword exactly like the reference's `DebugFlag_BitAddr` (the dword is
    /// stored little-endian, so its top byte is the last one).
    fn ref_byte_mask(sel: u8) -> (usize, u8) {
        let id = usize::from(sel);
        ((id >> 5) * 4 + 3 - ((id >> 3) & 3), 0x80 >> (id & 7))
    }

    #[test]
    fn groups_match_the_reference_views() {
        let sizes: Vec<(u8, u8, u16)> = GROUPS
            .iter()
            .map(|group| (group.bank, group.first, group.count))
            .collect();
        assert_eq!(
            sizes,
            [
                (0, 0, 128),
                (1, 0, 256),
                (2, 0, 64),
                (3, 0, 256),
                (4, 0, 64),
                (5, 0, 32),
                (5, 32, 32),
                (6, 0, 32),
                (7, 0, 256),
                (8, 0, 0x7C),
                (8, 0x7C, 6),
                (8, 0x82, 16),
                (9, 0, 64),
            ]
        );
    }

    #[test]
    fn edits_address_bits_msb_first_like_the_scripts() {
        let mut game = GameState::default();
        let mut editor = FlagEditor::default();
        // SCENARIO2 row 0xC0, the ending's partner bit.
        editor.handle_input(press(|i| i.right = true), &mut game);
        assert_eq!(editor.group(), 1);
        for _ in 0..0x40 {
            editor.handle_input(press(|i| i.up = true), &mut game);
        }
        assert_eq!(editor.selected().1, 0xC0);
        editor.handle_input(press(|i| i.page_left = true), &mut game);
        assert!(game.flag_test(BANK_SCENARIO2, 0xC0, false));
        let (byte, mask) = ref_byte_mask(0xC0);
        let raw = game.flags[1].bytes();
        assert_eq!(raw[byte], mask, "the reference's byte and mask");
        assert_eq!(raw.iter().filter(|byte| **byte != 0).count(), 1);
        // `]` clears, confirm toggles twice.
        editor.handle_input(press(|i| i.page_right = true), &mut game);
        assert!(!game.flag_test(BANK_SCENARIO2, 0xC0, false));
        editor.handle_input(press(|i| i.confirm = true), &mut game);
        assert!(game.flag_test(BANK_SCENARIO2, 0xC0, false));
        editor.handle_input(press(|i| i.confirm = true), &mut game);
        assert!(!game.flag_test(BANK_SCENARIO2, 0xC0, false));
    }

    #[test]
    fn the_state_groups_split_bank_five() {
        let mut game = GameState::default();
        let mut editor = FlagEditor {
            group: 5,
            ..FlagEditor::default()
        };
        for _ in 0..MSF_CHAR_VARIANT {
            editor.handle_input(press(|i| i.down = true), &mut game);
        }
        assert_eq!(
            flag_name(&GROUPS[5], editor.selected().1),
            FlagName::Static("CHAR_VARIANT")
        );
        editor.handle_input(press(|i| i.confirm = true), &mut game);
        assert!(game.flag_test(5, MSF_CHAR_VARIANT, false));
        // The msf2 group starts at selector 32.
        editor.handle_input(press(|i| i.right = true), &mut game);
        for _ in 0..5 {
            editor.handle_input(press(|i| i.down = true), &mut game);
        }
        let (group, sel) = editor.selected();
        assert_eq!(sel, MSF2_COSTUME_VARIANT);
        assert_eq!(flag_name(&group, sel), FlagName::Static("COSTUME_VARIANT"));
        assert_eq!(flag_name(&group, 0x21), FlagName::None);
    }

    #[test]
    fn the_message_group_routes_to_the_message_words() {
        let mut game = GameState::default();
        let mut editor = FlagEditor {
            group: 7,
            ..FlagEditor::default()
        };
        let before = game.message_flags;
        editor.handle_input(press(|i| i.page_left = true), &mut game);
        assert!(game.flag_test(6, 0, false));
        assert_eq!(game.message_flags, before, "selector 0 is the short word's");
        for _ in 0..16 {
            editor.handle_input(press(|i| i.down = true), &mut game);
        }
        editor.handle_input(press(|i| i.page_left = true), &mut game);
        assert_eq!(
            game.message_flags,
            before | 0x8000,
            "selector 16 is the main word's top bit"
        );
    }

    #[test]
    fn room_groups_are_named_by_room_map_and_document() {
        let visited = GROUPS[9];
        assert_eq!(
            flag_name(&visited, 0x06),
            FlagName::Room("MANSION 1F", "106".into())
        );
        assert_eq!(
            flag_name(&visited, 32),
            FlagName::Room("MANSION 2F", "200".into())
        );
        assert_eq!(
            flag_name(&visited, 100 + 3),
            FlagName::Room("LABORATORY", "503".into())
        );
        // Marking a room visited raises exactly the row the editor names.
        let mut game = GameState::default();
        game.mark_room_visited(crate::state::RoomId::parse("2030").unwrap());
        assert!(game.flag_test(8, 32 + 3, false));
        assert_eq!(
            flag_name(&GROUPS[10], 0x7C + 3),
            FlagName::Static("UNDERGROUND")
        );
        assert_eq!(flag_name(&GROUPS[11], 0x82 + 2), FlagName::Item(0x61));
        assert_eq!(
            flag_name(&GROUPS[0], 0x7B),
            FlagName::Static("SECOND_PLAYTHROUGH")
        );
        assert_eq!(flag_name(&GROUPS[3], 0x7B), FlagName::None);
    }

    #[test]
    fn rows_and_groups_wrap() {
        let mut game = GameState::default();
        let mut editor = FlagEditor::default();
        editor.handle_input(press(|i| i.left = true), &mut game);
        assert_eq!(editor.group(), GROUPS.len() - 1);
        editor.handle_input(press(|i| i.up = true), &mut game);
        assert_eq!(editor.row(), 63, "ITEM USE has 64 bits");
        assert_eq!(editor.scroll[editor.group], 64 - VISIBLE_ROWS);
        editor.handle_input(press(|i| i.right = true), &mut game);
        assert_eq!((editor.group(), editor.row()), (0, 0));
        assert_eq!(
            editor.handle_input(press(|i| i.cancel = true), &mut game),
            Step::Back
        );
    }
}
