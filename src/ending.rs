//! The ending selection, film chain and post-film sequence data.
//!
//! The original picks one of seven ending rows from two scenario-flag bits
//! (the partner's survival and the second protagonist's) and the player
//! character, then plays a fixed film sequence: the pre-ending film (14), the
//! row's ending film (`id + 14`, ids 15-21), a congratulations film and, for
//! the rows whose descriptor sets the flag, the staff roll (22).
//!
//! After the films the RESULT screen (`ending_result_*`) reveals the row's
//! baked plates, the clear time and the save count; endings 6 and 7 then run
//! the rocket-launcher epilogue, and finally the next-cycle save carries the
//! unlocked items into a fresh card. [`EndStats`] carries the time, save count
//! and identity the screens read, and [`CreditEntry`]/[`CREDIT_ENTRIES`] are
//! the RESULT/epilogue plate layout table.

use crate::game::{self, GameState};

/// The pre-ending film every row shares.
pub const PRE_ENDING_ID: u8 = 14;
/// The staff-roll film the first three rows append.
pub const STAFF_ROLL_ID: u8 = 22;
/// The first ending film (`id + 14` for row 1).
pub const ENDING_FILM_BASE: u8 = 14;

/// The 30 Hz play-timer cutoff under which the clear grants the infinite
/// weapon: 4 hours (`EndingScreen.cpp:1052`, `0x69780` ticks).
pub const INFINITE_WEAPON_TIME_LIMIT: u32 = 0x69780;
/// 30 Hz ticks in one hour (the clear-time formatter's `0x1A5E0`).
pub const TICKS_PER_HOUR: u32 = 0x1A5E0;
/// 30 Hz ticks in one minute (`0x708`).
pub const TICKS_PER_MINUTE: u32 = 0x708;
/// 30 Hz ticks in one second (`0x1E`).
pub const TICKS_PER_SECOND: u32 = 0x1E;
/// The separator glyph the clear-time formatter writes (`EndingScreen.cpp:380`,
/// `0x5E`).
pub const CLEAR_TIME_SEPARATOR: u8 = 0x5E;

/// The RESULT-screen and next-cycle identity and counters.
///
/// Mirrors the reference's `g_gameTimerSnapshot`, `g_SavesCounter`,
/// `s_endingId` and the player character. The RESULT screen draws
/// [`EndStats::time_ticks`] and [`EndStats::save_count`]; the next-cycle grant
/// reads [`EndStats::under_time_limit`] and the no-save flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndStats {
    /// The selected ending row (1-7).
    pub ending_id: u8,
    /// The player character (`0` Chris, `1` Jill).
    pub character: u8,
    /// The play-timer snapshot in 30 Hz ticks.
    pub time_ticks: u32,
    /// The save counter (`g_SavesCounter`; one ahead of the player's saves).
    pub save_counter: u8,
}

impl EndStats {
    /// Read the stats from a live game state: the timer snapshot, the save
    /// counter and the character byte.
    pub fn from_state(state: &GameState, ending_id: u8, character: u8) -> Self {
        Self {
            ending_id,
            character: character & 1,
            time_ticks: state.play_timer(),
            save_counter: state.state_bytes[usize::from(game::STATE_BYTE_SAVES)],
        }
    }

    /// A debug stat for the `--ending`/`--ui` paths: a zero clear time and the
    /// given save counter (the shipped card starts at 1, a no-save run).
    pub fn debug(ending_id: u8, character: u8, save_counter: u8) -> Self {
        Self {
            ending_id,
            character: character & 1,
            time_ticks: 0,
            save_counter,
        }
    }

    /// The save count the RESULT screen prints: the counter minus one, clamped
    /// at zero (`EndingScreen.cpp:388-399`).
    pub fn save_count(&self) -> u8 {
        self.save_counter.saturating_sub(1)
    }

    /// Whether the run made no saves, which the reference detects by the
    /// counter still reading its initial one (`EndingScreen.cpp:426-434`).
    pub fn no_save(&self) -> bool {
        self.save_counter == 1
    }

    /// Whether the clear is under the infinite-weapon time limit.
    pub fn under_time_limit(&self) -> bool {
        self.time_ticks < INFINITE_WEAPON_TIME_LIMIT
    }

    /// The clear time split into `(hours, minutes, seconds)`.
    pub fn time_parts(&self) -> (u32, u32, u32) {
        let hours = self.time_ticks / TICKS_PER_HOUR;
        let minutes = (self.time_ticks % TICKS_PER_HOUR) / TICKS_PER_MINUTE;
        let seconds = (self.time_ticks % TICKS_PER_MINUTE) / TICKS_PER_SECOND;
        (hours, minutes, seconds)
    }

    /// The clear-time bytes the original's `sprintf` produces:
    /// `"%02d:%02d%1c%02d"` with the raw `0x5E` separator. These are ASCII
    /// bytes; the RESULT screen draws them through its own grid addressing (see
    /// [`crate::ui::ending::draw_grid_text`]).
    pub fn clear_time_bytes(&self) -> Vec<u8> {
        let (hours, minutes, seconds) = self.time_parts();
        let mut out = Vec::with_capacity(8);
        push_two_digits(&mut out, hours);
        out.push(b':');
        push_two_digits(&mut out, minutes);
        out.push(CLEAR_TIME_SEPARATOR);
        push_two_digits(&mut out, seconds);
        out
    }

    /// The save-count bytes the original's `sprintf("%02d", ...)` produces.
    pub fn save_count_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2);
        push_two_digits(&mut out, u32::from(self.save_count()));
        out
    }
}

/// Append `value` as at least two decimal ASCII digits (three or more when the
/// value needs them).
fn push_two_digits(out: &mut Vec<u8>, value: u32) {
    let text = value.to_string();
    for _ in text.len()..2 {
        out.push(b'0');
    }
    out.extend_from_slice(text.as_bytes());
}

/// The kind of draw a RESULT/epilogue credit slot performs
/// (`EndingScreen.cpp:57-60`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// A baked plate sprite (type 2), sampled from the `en0x.tim` page.
    Sprite,
    /// The clear-time string (type 3).
    ClearTime,
    /// The save-count string (type 5).
    SaveCount,
}

/// One RESULT/epilogue credit layout entry (`EndingScreen.cpp:119-140`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreditEntry {
    /// The slot's draw kind.
    pub kind: SlotKind,
    /// Screen X before the `-0xA0` centring.
    pub x: i16,
    /// Screen Y before the `-0x78` centring.
    pub y: i16,
    /// The plate's V cursor into the `en0x.tim` page.
    pub tex_v: u8,
    /// The plate's height in pixels.
    pub height: u8,
}

/// The RESULT/epilogue credit table (`s_creditEntries`, `EndingScreen.cpp:127`).
///
/// Entries 0-4 are the background layout (endings 4-7), 7-11 the plain layout
/// (endings 1-3); entries 5-6 are the epilogue's two captions.
pub const CREDIT_ENTRIES: [CreditEntry; 12] = [
    CreditEntry {
        kind: SlotKind::Sprite,
        x: 0x00,
        y: 0x30,
        tex_v: 0x00,
        height: 0x18,
    },
    CreditEntry {
        kind: SlotKind::ClearTime,
        x: 0x30,
        y: 0x50,
        tex_v: 0x00,
        height: 0x00,
    },
    CreditEntry {
        kind: SlotKind::Sprite,
        x: 0x00,
        y: 0x74,
        tex_v: 0x18,
        height: 0x18,
    },
    CreditEntry {
        kind: SlotKind::SaveCount,
        x: 0x48,
        y: 0x94,
        tex_v: 0x00,
        height: 0x00,
    },
    CreditEntry {
        kind: SlotKind::Sprite,
        x: 0x25,
        y: 0x66,
        tex_v: 0x30,
        height: 0x28,
    },
    CreditEntry {
        kind: SlotKind::Sprite,
        x: 0x20,
        y: 0x20,
        tex_v: 0x58,
        height: 0x30,
    },
    CreditEntry {
        kind: SlotKind::Sprite,
        x: 0x20,
        y: 0xC0,
        tex_v: 0x88,
        height: 0x20,
    },
    CreditEntry {
        kind: SlotKind::Sprite,
        x: 0x20,
        y: 0x16,
        tex_v: 0x00,
        height: 0x36,
    },
    CreditEntry {
        kind: SlotKind::ClearTime,
        x: 0x80,
        y: 0x58,
        tex_v: 0x00,
        height: 0x00,
    },
    CreditEntry {
        kind: SlotKind::Sprite,
        x: 0x20,
        y: 0x7A,
        tex_v: 0x36,
        height: 0x36,
    },
    CreditEntry {
        kind: SlotKind::SaveCount,
        x: 0x98,
        y: 0xB8,
        tex_v: 0x00,
        height: 0x00,
    },
    CreditEntry {
        kind: SlotKind::Sprite,
        x: 0x25,
        y: 0x66,
        tex_v: 0x30,
        height: 0x28,
    },
];

/// The RESULT layout base entry for a row: entries 0-4 for the background
/// rows, 7-11 for the plain rows (`EndingScreen.cpp:443`).
pub fn credit_base(background: bool) -> usize {
    if background { 0 } else { 7 }
}

/// The Jill layout shift on the background rows, in pixels (`0x9C0000 >> 16`).
pub const JILL_LAYOUT_SHIFT: i16 = 0x9C;

/// The packed item id of the rocket launcher (`ITEM_ROCKET_LAUNCHER`).
pub const ITEM_ROCKET_LAUNCHER: u8 = 0x0A;
/// The packed item id of the special key (`ITEM_SPECIAL_KEY`).
pub const ITEM_SPECIAL_KEY: u8 = 0x38;

/// One ending descriptor row.
///
/// `staff_roll` and `plate` drive the film chain; `epilogue` and `background`
/// drive the RESULT screen and its backdrop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndingRow {
    /// The row appends the staff-roll film (22).
    pub staff_roll: bool,
    /// The congratulations film group: true selects film 27, false the
    /// character group (24/25/26).
    pub plate: bool,
    /// The rocket-launcher epilogue applies to this row.
    pub epilogue: bool,
    /// The character backdrop `.pix` and `en07.tim` are used.
    pub background: bool,
}

/// The ending descriptor table, indexed by ending id. Row 0 is unused: the
/// selection never returns it.
pub const ROWS: [EndingRow; 8] = [
    EndingRow {
        staff_roll: false,
        plate: false,
        epilogue: false,
        background: false,
    },
    // 1 - Chris, both survivors.
    EndingRow {
        staff_roll: true,
        plate: true,
        epilogue: false,
        background: false,
    },
    // 2 - Jill, both survivors.
    EndingRow {
        staff_roll: true,
        plate: true,
        epilogue: false,
        background: false,
    },
    // 3 - one survivor plus the partner.
    EndingRow {
        staff_roll: true,
        plate: true,
        epilogue: false,
        background: false,
    },
    // 4 - Chris alone.
    EndingRow {
        staff_roll: false,
        plate: false,
        epilogue: false,
        background: true,
    },
    // 5 - Jill alone.
    EndingRow {
        staff_roll: false,
        plate: false,
        epilogue: false,
        background: true,
    },
    // 6 - Chris, no survivors.
    EndingRow {
        staff_roll: false,
        plate: false,
        epilogue: true,
        background: true,
    },
    // 7 - Jill, no survivors.
    EndingRow {
        staff_roll: false,
        plate: false,
        epilogue: true,
        background: true,
    },
];

/// The descriptor row for ending `id`, or `None` outside 1-7.
pub fn row(id: u8) -> Option<EndingRow> {
    ROWS.get(usize::from(id)).copied().filter(|_| id != 0)
}

/// Bank 1 (`g_ScenarioFlags2`) bit the original names
/// `SCENARIO2_FLAG_PARTNER_ALIVE`: the partner survived.
pub const SCENARIO2_FLAG_PARTNER_ALIVE: u8 = 0xC0;
/// Bank 1 bit the original names `SCENARIO2_FLAG_SECOND_SURVIVOR`.
pub const SCENARIO2_FLAG_SECOND_SURVIVOR: u8 = 0x4B;

/// Select the ending row (1-7) from the two scenario flags and the player.
///
/// The player always survives. `partner_alive` is the original's
/// `SCENARIO2_FLAG_PARTNER_ALIVE` bit (the partner: Rebecca on a Chris run,
/// Barry on a Jill run) and `second_survivor` its `SCENARIO2_FLAG_SECOND_SURVIVOR`
/// bit. The seven rows are:
///
/// | partner_alive | second_survivor | Chris | Jill |
/// |---------------|-----------------|-------|------|
/// | yes           | no              | 1     | 2    |
/// | yes           | yes             | 3     | 3    |
/// | no            | no              | 4     | 5    |
/// | no            | yes             | 6     | 7    |
pub fn select_id(partner_alive: bool, second_survivor: bool, character: u8) -> u8 {
    let chris = character & 1 == 0;
    match (partner_alive, second_survivor) {
        (false, false) => {
            if chris {
                4
            } else {
                5
            }
        }
        (false, true) => {
            if chris {
                6
            } else {
                7
            }
        }
        (true, false) => {
            if chris {
                1
            } else {
                2
            }
        }
        (true, true) => 3,
    }
}

/// The film order for ending `id`: the pre-ending film, the ending film, the
/// congratulations film and the staff roll when the row carries it.
///
/// The congratulations film is 27 when the row selects the plate group, the
/// character's 24/25 otherwise, and 26 for the infinite rocket launcher or a
/// second playthrough (which overrides the plate selection). An `id` outside
/// 1-7 yields an empty chain.
pub fn chain(
    id: u8,
    character: u8,
    second_playthrough: bool,
    has_infinite_launcher: bool,
) -> Vec<u8> {
    let Some(row) = row(id) else {
        return Vec::new();
    };
    let congratulations = if second_playthrough {
        26
    } else if row.plate {
        27
    } else if has_infinite_launcher {
        26
    } else {
        24 + (character & 1)
    };
    let mut films = vec![PRE_ENDING_ID, ENDING_FILM_BASE + id, congratulations];
    if row.staff_roll {
        films.push(STAFF_ROLL_ID);
    }
    films
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_id_covers_the_seven_rows() {
        // Partner alive and the second survivor flag clear: both survive.
        assert_eq!(select_id(true, false, 0), 1);
        assert_eq!(select_id(true, false, 1), 2);
        // Partner alive plus the second survivor: one survivor and the partner.
        assert_eq!(select_id(true, true, 0), 3);
        assert_eq!(select_id(true, true, 1), 3);
        // Partner gone, second survivor flag clear: alone.
        assert_eq!(select_id(false, false, 0), 4);
        assert_eq!(select_id(false, false, 1), 5);
        // Partner gone plus the second survivor: no survivors.
        assert_eq!(select_id(false, true, 0), 6);
        assert_eq!(select_id(false, true, 1), 7);
        // The character is masked to its low bit.
        assert_eq!(select_id(true, false, 2), 1);
        assert_eq!(select_id(true, false, 3), 2);
    }

    #[test]
    fn rows_match_the_descriptor_table() {
        assert!(row(0).is_none());
        assert!(row(8).is_none());
        for id in 1..=3 {
            let row = row(id).unwrap();
            assert!(row.staff_roll, "row {id} carries the staff roll");
            assert!(row.plate, "row {id} uses the plate group");
            assert!(!row.epilogue);
            assert!(!row.background);
        }
        for id in 4..=5 {
            let row = row(id).unwrap();
            assert!(!row.staff_roll && !row.plate && !row.epilogue && row.background);
        }
        for id in 6..=7 {
            let row = row(id).unwrap();
            assert!(!row.staff_roll && !row.plate && row.epilogue && row.background);
        }
    }

    #[test]
    fn chain_orders_every_row_and_flag_combination() {
        // The plate rows always end on 27 and append the staff roll.
        assert_eq!(chain(1, 0, false, false), vec![14, 15, 27, 22]);
        assert_eq!(chain(2, 1, false, false), vec![14, 16, 27, 22]);
        assert_eq!(chain(3, 0, false, false), vec![14, 17, 27, 22]);
        assert_eq!(chain(3, 1, false, true), vec![14, 17, 27, 22]);

        // The character rows pick 24/25 and never append the staff roll.
        assert_eq!(chain(4, 0, false, false), vec![14, 18, 24]);
        assert_eq!(chain(4, 1, false, false), vec![14, 18, 25]);
        assert_eq!(chain(5, 0, false, false), vec![14, 19, 24]);
        assert_eq!(chain(5, 1, false, false), vec![14, 19, 25]);
        assert_eq!(chain(6, 0, false, false), vec![14, 20, 24]);
        assert_eq!(chain(7, 1, false, false), vec![14, 21, 25]);

        // The infinite launcher selects 26 on the character rows.
        assert_eq!(chain(4, 0, false, true), vec![14, 18, 26]);
        assert_eq!(chain(7, 1, false, true), vec![14, 21, 26]);
        assert_eq!(chain(1, 0, false, true), vec![14, 15, 27, 22]);

        // A second playthrough overrides the plate selection to 26.
        assert_eq!(chain(1, 0, true, false), vec![14, 15, 26, 22]);
        assert_eq!(chain(4, 1, true, true), vec![14, 18, 26]);

        // Out-of-range ids yield no chain.
        assert!(chain(0, 0, false, false).is_empty());
        assert!(chain(8, 0, false, false).is_empty());
        assert!(chain(255, 0, false, false).is_empty());
    }

    #[test]
    fn stats_format_the_clear_time_and_save_count() {
        // 1h + 2min + 3s at 30 Hz.
        let stats = EndStats {
            ending_id: 1,
            character: 0,
            time_ticks: TICKS_PER_HOUR + 2 * TICKS_PER_MINUTE + 3 * TICKS_PER_SECOND,
            save_counter: 5,
        };
        assert_eq!(stats.time_parts(), (1, 2, 3));
        assert_eq!(stats.clear_time_bytes(), b"01:02\x5E03".to_vec());
        assert_eq!(stats.save_count(), 4);
        assert_eq!(stats.save_count_bytes(), b"04".to_vec());

        // Hours past 99 keep all their digits, like `%02d`.
        let long = EndStats {
            time_ticks: 100 * TICKS_PER_HOUR,
            ..stats
        };
        assert_eq!(long.time_parts().0, 100);
        assert_eq!(&long.clear_time_bytes()[..3], b"100");
    }

    #[test]
    fn stats_derive_the_no_save_and_time_limit_grants() {
        let no_save = EndStats::debug(6, 0, 1);
        assert!(no_save.no_save());
        assert_eq!(no_save.save_count(), 0);
        assert!(no_save.under_time_limit(), "zero ticks is under four hours");

        let saved = EndStats::debug(6, 0, 4);
        assert!(!saved.no_save());
        assert_eq!(saved.save_count(), 3);

        let slow = EndStats {
            time_ticks: INFINITE_WEAPON_TIME_LIMIT,
            ..saved
        };
        assert!(!slow.under_time_limit(), "the cutoff is exclusive");
    }

    #[test]
    fn credit_layout_matches_the_reference_table() {
        assert_eq!(credit_base(false), 7);
        assert_eq!(credit_base(true), 0);
        // The plain rows use the combined 0x36-high top plate and the second
        // 0x36 plate; the background rows use the split 0x18 plates.
        assert_eq!(CREDIT_ENTRIES[7].height, 0x36);
        assert_eq!(CREDIT_ENTRIES[9].tex_v, 0x36);
        assert_eq!(CREDIT_ENTRIES[0].height, 0x18);
        assert_eq!(CREDIT_ENTRIES[2].tex_v, 0x18);
        // The two epilogue captions live at 5/6.
        assert_eq!(CREDIT_ENTRIES[5].tex_v, 0x58);
        assert_eq!(CREDIT_ENTRIES[6].tex_v, 0x88);
        // Entry 4 (the ending-6 fifth plate) and entry 11 share the graphic.
        assert_eq!(CREDIT_ENTRIES[4].tex_v, CREDIT_ENTRIES[11].tex_v);
        assert_eq!(CREDIT_ENTRIES[4].height, CREDIT_ENTRIES[11].height);
    }
}
