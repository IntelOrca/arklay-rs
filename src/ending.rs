//! The ending selection and film chain.
//!
//! The original picks one of seven ending rows from two scenario-flag bits
//! (the partner's survival and the second protagonist's) and the player
//! character, then plays a fixed film sequence: the pre-ending film (14), the
//! row's ending film (`id + 14`, ids 15-21), a congratulations film and, for
//! the rows whose descriptor sets the flag, the staff roll (22).
//!
//! The RESULT screen, the rocket-launcher epilogue and the next-cycle save are
//! outside this milestone: the chain is a pure function and the films are
//! reachable through the `--ending` debug entry, because a normal playthrough
//! would need the excluded combat and boss content.

/// The pre-ending film every row shares.
pub const PRE_ENDING_ID: u8 = 14;
/// The staff-roll film the first three rows append.
pub const STAFF_ROLL_ID: u8 = 22;
/// The first ending film (`id + 14` for row 1).
pub const ENDING_FILM_BASE: u8 = 14;

/// One ending descriptor row.
///
/// `staff_roll` and `plate` drive the film chain; `epilogue` and `background`
/// describe the RESULT screen and its backdrop, which this milestone does not
/// present, and are kept so the seven-row table stays a complete transcription.
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
}
