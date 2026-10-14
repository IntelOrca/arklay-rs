//! Voice-line pack path naming.
//!
//! `voice_play` (0x1E, the original's `xa_on`) requests a line by id; the
//! engine reads it from `voice/{row}_{id}.wav`, where `row` is the 1-based
//! table row the stage reads and `id` is the request id in two lowercase hex
//! digits. The 0-based stage selects its row through [`row_for_stage`]: rooms
//! in stages 6 and 7 reuse the stage 1 and 2 rows, so a stage-6 request for
//! id 9 reads `voice/01_09.wav`.
//!
//! The name tables themselves live in [`crate::voice_table`] and are read
//! only by `convert-game`. A request whose record is empty or past the row
//! end formats a path no entry matches, and the engine's missing-line
//! handling counts the miss without ever stalling the F7 wait.

/// Stage (0-based) to its table row. Stages 5 and 6 (rooms 6xxx/7xxx) reuse
/// rows 0 and 1; no shipped stage past 6 has a table.
static ROW_FOR_STAGE: [Option<usize>; 7] = [
    Some(0),
    Some(1),
    Some(2),
    Some(3),
    Some(4),
    Some(0),
    Some(1),
];

/// The table row a 0-based stage reads, or `None` for an unknown stage.
pub fn row_for_stage(stage: u8) -> Option<usize> {
    ROW_FOR_STAGE.get(usize::from(stage)).copied().flatten()
}

/// Pack path for a table record: `voice/{row+1:02}_{id:02x}.wav`.
pub fn entry_path(row: usize, id: u16) -> String {
    format!("voice/{:02}_{:02x}.wav", row + 1, id)
}

/// Pack path for a request in 0-based `stage`. A stage without a shipped
/// table (unreachable for a parsed [`crate::state::RoomId`]) formats under
/// its own 1-based number (`entry_path` takes a 0-based row), so the load
/// simply misses.
pub fn pack_path(stage: u8, id: u16) -> String {
    let row = row_for_stage(stage).unwrap_or(usize::from(stage));
    entry_path(row, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_selector_shares_rows_and_nulls() {
        assert_eq!(row_for_stage(0), Some(0));
        assert_eq!(row_for_stage(1), Some(1));
        assert_eq!(row_for_stage(2), Some(2));
        assert_eq!(row_for_stage(3), Some(3));
        assert_eq!(row_for_stage(4), Some(4));
        assert_eq!(row_for_stage(5), Some(0));
        assert_eq!(row_for_stage(6), Some(1));
        assert_eq!(row_for_stage(7), None);
        assert_eq!(row_for_stage(8), None);
    }

    #[test]
    fn pack_paths_are_lowercased_two_digit_hex_entries() {
        assert_eq!(pack_path(0, 9), "voice/01_09.wav");
        assert_eq!(pack_path(1, 0), "voice/02_00.wav");
        assert_eq!(pack_path(2, 46), "voice/03_2e.wav");
        assert_eq!(pack_path(4, 186), "voice/05_ba.wav");
        assert_eq!(pack_path(5, 9), "voice/01_09.wav", "stage 6 reads row 0");
        assert_eq!(pack_path(6, 0), "voice/02_00.wav", "stage 7 reads row 1");
    }

    #[test]
    fn empty_and_out_of_range_ids_still_format_a_path() {
        assert_eq!(pack_path(4, 181), "voice/05_b5.wav");
        assert_eq!(pack_path(0, 134), "voice/01_86.wav");
        assert_eq!(pack_path(0, u16::MAX), "voice/01_ffff.wav");
    }

    #[test]
    fn a_stage_without_a_table_formats_under_its_own_number() {
        assert_eq!(pack_path(7, 0), "voice/08_00.wav");
    }
}
