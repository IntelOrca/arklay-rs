//! Per-stage voice-line name tables and pack path naming.
//!
//! Each 0-based stage selects one of five name rows through an eight-entry
//! selector: stages 5 and 6 reuse the rows of stages 0 and 1, and stage 7 has
//! no table at all. `xa_on` (0x1E) resolves its id through the current stage;
//! an empty record or an id past the end of the row is silent and the wait
//! flag is never raised, so a script cannot stall on a missing line.

/// One name row per stage table. Records are 7- or 8-character basenames;
/// empty records are silent slots (stage 4 ids 181/182 in the shipped data).
pub static ROWS: [&[&str]; 5] = [
    // Row 0: 134 records.
    &[
        "V001_00", "V001_01", "V001_02", "V001_03", "V001_04", "V001_05", "V001_06", "V003_00",
        "V003_01", "V004_00", "V004_01", "V004_02", "V004_03", "V004_04", "V004_05", "V004_06",
        "V004_07", "V004_08", "V004_09", "V004_0a", "V004_0b", "V004_0c", "V004_0d", "V004_0e",
        "V004_0f", "V101_00", "V101_01", "V101_02", "V101_03", "V101_04", "V102_00", "V102_01",
        "V102_02", "V102_03", "V102_04", "V102_05", "V102_06", "V102_07", "V102_08", "V102_09",
        "V102_0a", "V102_0b", "V102_0c", "V102_0d", "V102_0e", "V102_0a", "V102_10", "V102_0e",
        "V105_00", "V105_01", "V105_02", "V105_03", "V105_04", "V105_05", "V105_06", "V105_07",
        "V105_08", "V105_09", "V103_00", "V103_01", "V103_0a", "V103_0b", "V103_0c", "V103_0d",
        "V107_00", "V006_00", "V006_01", "V006_02", "V006_03", "V006_04", "V006_05", "V006_06",
        "V006_07", "V006_08", "V006_09", "V006_0a", "V006_0b", "V006_0c", "V006_0d", "V006_0e",
        "V006_0f", "V006_10", "V006_11", "V006_12", "V00C_00", "V00C_01", "V00C_10", "V00C_11",
        "V00C_12", "V00C_13", "V00C_14", "V00C_15", "V00C_16", "V00C_17", "V00C_18", "V00C_19",
        "V00C_1a", "V00C_1b", "V00C_1c", "V007_0d", "V00C_20", "V00C_21", "V00C_22", "V007_00",
        "V007_01", "V007_02", "V007_03", "V007_04", "V007_05", "V007_06", "V007_07", "V007_08",
        "V007_09", "V007_0a", "V007_0b", "V007_0c", "V007_0d", "V007_0e", "V007_0f", "V00B_00",
        "V00B_01", "V008_00", "V008_01", "V008_02", "V008_03", "V008_04", "V008_05", "V008_06",
        "VA04_00", "VA04_01", "V106_00", "V106_01", "V106_02", "VA00_00",
    ],
    // Row 1: 94 records.
    &[
        "V104_00", "V104_01", "V104_02", "V104_03", "V104_04", "V104_05", "V104_06", "V104_07",
        "V104_08", "V104_09", "V104_0a", "V00C_30", "V00C_31", "VA06_00", "VA07_00", "V008_10",
        "V008_11", "V008_12", "V008_13", "V008_14", "V008_15", "V008_16", "V008_17", "V008_18",
        "V008_19", "V108_00", "V108_01", "V108_02", "V108_03", "V108_04", "V108_05", "V108_06",
        "V108_07", "V108_08", "V106_00", "V106_01", "V106_02", "V005_00", "V005_01", "V005_02",
        "V005_03", "V005_04", "V005_05", "V005_06", "V005_07", "V005_08", "V005_09", "V005_0a",
        "V005_0b", "V005_0c", "V005_0d", "V005_0e", "V005_0f", "V005_10", "V005_11", "V005_12",
        "V005_13", "V005_14", "VA01_00", "VA01_01", "VA01_02", "VA01_03", "VA01_04", "VA01_05",
        "VA01_06", "VA01_07", "VA01_08", "VA01_09", "VA01_0a", "VA01_0b", "VA01_0c", "VA01_0d",
        "V10D_00", "V10D_01", "V10D_02", "V10D_03", "V10D_04", "V10D_05", "V10D_06", "V10D_07",
        "V10D_10", "V10D_11", "V10D_12", "V10D_13", "V10D_14", "V10D_15", "V10D_16", "V10D_17",
        "V10D_18", "V10D_19", "VB00_00", "VB00_01", "VB00_02", "VA00_00",
    ],
    // Row 2: 50 records.
    &[
        "V00D_00", "V00D_01", "V00D_02", "V00D_03", "V00D_04", "V00D_05", "V10F_00", "V10F_02",
        "V10F_04", "V10F_05", "V10F_06", "V10F_07", "V10F_08", "V10F_10", "V10F_12", "V10F_15",
        "V10F_16", "V10F_17", "V10F_18", "V10F_19", "V10F_1a", "V10F_1b", "V10E_00", "V10E_01",
        "V10E_02", "V10E_03", "V10E_04", "V10E_05", "V10E_06", "V10E_07", "V009_00", "V009_01",
        "V009_02", "V009_03", "V009_04", "V009_05", "V009_00", "VA05_01", "V009_02", "VA05_03",
        "V009_04", "VA05_05", "V016_05", "V11B_03", "VB00_30", "VB00_31a", "V110_00", "V110_01",
        "VA00_01", "V10E_08",
    ],
    // Row 3: 70 records.
    &[
        "V109_00", "V109_10", "V109_11", "V109_12", "V109_13", "V109_14", "V109_15", "V00A_00",
        "V00A_01", "V00A_10", "V00A_11", "V00A_12", "V00A_13", "V00A_14", "V00A_20", "V00A_21",
        "V00A_22", "V00A_23", "V00A_24", "V00A_25", "V00A_26", "V00A_27", "V00A_28", "V00A_29",
        "V00A_2a", "V00A_2b", "V00A_2c", "V10C_00", "V10C_01", "V10C_02", "V10C_03", "V10C_04",
        "V10C_08", "V10C_06", "V10C_07", "VA02_00", "VA02_01", "VA02_02", "VA02_03", "VA02_04",
        "VA02_05", "VA02_06", "VA02_07", "VA02_08", "VA02_09", "V10A_00", "V10A_01", "V10A_02",
        "V10A_03", "V10A_04", "V10A_05", "V10A_06", "V10A_07", "V10A_08", "V10A_09", "V10A_0a",
        "V10A_0b", "V10A_0c", "VA00_02", "VA08_00", "VA08_01", "VA08_02", "VA08_03", "VA08_04",
        "VA08_05", "VA08_06", "VA08_07", "VA08_08", "VA08_09", "VA08_0a",
    ],
    // Row 4: 187 records.
    &[
        "VA09_00", "VA09_01", "VA09_02", "VA09_03", "VA09_04", "VA09_05", "VA09_06", "VA09_07",
        "VA09_08", "VA09_09", "VA09_0a", "VA09_0b", "V118_00", "V118_01", "V00F_00", "V00F_01",
        "V00F_02", "V00F_10", "V00F_11", "V00F_12", "V11B_00", "V11B_01", "V11B_02", "V016_00",
        "V016_01", "V016_02", "V016_03", "V016_04", "V012_00", "V012_01", "V012_02", "V012_03",
        "V012_04", "V012_05", "V012_06", "V012_07", "V012_08", "V012_09", "V012_0a", "V012_0b",
        "V012_0c", "V012_0d", "V012_0e", "V012_0f", "V012_10", "V012_11", "V012_12", "V011_00",
        "V011_01", "V011_02", "V011_03", "V011_04", "V011_05", "V011_06", "V011_07", "V011_08",
        "V011_09", "V011_0a", "V015_00", "V015_01", "V015_02", "V015_03", "V015_04", "V015_05",
        "V015_06", "V015_07", "V119_00", "V119_01", "VA00_03", "VA00_04", "VA00_05", "V013_00",
        "V013_01", "V013_02", "V013_03", "V013_04", "V013_05", "V013_06", "V013_07", "V013_08",
        "V013_09", "VB00_11", "V016_06", "V11A_00", "V11A_01", "V11A_02", "V11A_03", "V11A_04",
        "V11A_05", "VA00_03", "VA00_04", "VA00_05", "V115_00", "V115_01", "V115_02", "V115_03",
        "V115_04", "V115_05", "V115_06", "V115_07", "V115_08", "V115_09", "V115_0a", "V115_0b",
        "V115_0c", "V115_0d", "V115_0e", "V115_0f", "V115_10", "V115_11", "V115_12", "V115_13",
        "V115_14", "V115_15", "V115_16", "V115_17", "V115_18", "V115_19", "V115_1a", "V115_1b",
        "V115_1c", "V115_1d", "V115_1e", "V115_1f", "V115_20", "V115_21", "V115_22", "VA03_00",
        "VA03_01", "VA03_02", "VA03_03", "VA03_04", "VA03_05", "VA03_06", "VA03_07", "VA03_08",
        "VA03_09", "VA03_0a", "VA03_0b", "VA03_0c", "VA03_0d", "VA03_0e", "V014_00", "V014_01",
        "V014_02", "V014_03", "V014_04", "V014_05", "V014_06", "V014_07", "V014_08", "V014_09",
        "V014_0a", "V112_10", "V112_11", "V112_12", "V112_13", "V112_14", "V112_15", "V112_16",
        "V117_00", "V117_01", "V117_02", "V117_03", "V117_04", "V117_05", "V117_10", "V117_11",
        "V117_12", "V117_13", "VB00_11", "VA09_0b", "V116_00", "V116_01", "V116_02", "V116_03",
        "V116_04", "V116_05", "V116_10", "VB00_10", "VB00_11", "", "", "VB00_20", "VB00_21",
        "VB00_22", "VB00_40",
    ],
];

/// Stage (0-based) to row in [`ROWS`]. Stages 5/6 share rows 0/1; the eighth
/// entry is the absent stage 7 table.
static ROW_FOR_STAGE: [Option<usize>; 8] = [
    Some(0),
    Some(1),
    Some(2),
    Some(3),
    Some(4),
    Some(0),
    Some(1),
    None,
];

/// The voice basename for `id` in 0-based `stage`, or `None` for an empty
/// record, an out-of-range id or a stage with no table.
pub fn name(stage: u8, id: u16) -> Option<&'static str> {
    let row = ROW_FOR_STAGE.get(usize::from(stage)).copied().flatten()?;
    let name = *ROWS[row].get(usize::from(id))?;
    (!name.is_empty()).then_some(name)
}

/// Pack path for a voice basename: `voice/{name}.wav`, lowercased.
pub fn pack_path(name: &str) -> String {
    format!("voice/{}.wav", name.to_ascii_lowercase())
}

/// Every non-empty name any row references, sorted and deduplicated.
pub fn referenced_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = ROWS
        .iter()
        .flat_map(|row| row.iter().copied())
        .filter(|name| !name.is_empty())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_lengths_match_the_shipped_tables() {
        assert_eq!(ROWS[0].len(), 134);
        assert_eq!(ROWS[1].len(), 94);
        assert_eq!(ROWS[2].len(), 50);
        assert_eq!(ROWS[3].len(), 70);
        assert_eq!(ROWS[4].len(), 187);
    }

    #[test]
    fn stage_selector_shares_and_nulls() {
        assert_eq!(name(0, 0), Some("V001_00"));
        assert_eq!(name(1, 0), Some("V104_00"));
        assert_eq!(name(2, 0), Some("V00D_00"));
        assert_eq!(name(3, 0), Some("V109_00"));
        assert_eq!(name(4, 0), Some("VA09_00"));
        assert_eq!(name(5, 0), name(0, 0));
        assert_eq!(name(6, 0), name(1, 0));
        assert_eq!(name(7, 0), None);
        assert_eq!(name(8, 0), None);
    }

    #[test]
    fn edge_ids_resolve_and_overruns_are_none() {
        assert_eq!(name(0, 133), Some("VA00_00"));
        assert_eq!(name(0, 134), None);
        assert_eq!(name(1, 93), Some("VA00_00"));
        assert_eq!(name(1, 94), None);
        assert_eq!(name(2, 49), Some("V10E_08"));
        assert_eq!(name(2, 50), None);
        assert_eq!(name(3, 69), Some("VA08_0a"));
        assert_eq!(name(3, 70), None);
        assert_eq!(name(4, 186), Some("VB00_40"));
        assert_eq!(name(4, 187), None);
        assert_eq!(name(4, u16::MAX), None);
    }

    #[test]
    fn empty_stage_four_records_are_none() {
        assert_eq!(name(4, 180), Some("VB00_11"));
        assert_eq!(name(4, 181), None);
        assert_eq!(name(4, 182), None);
        assert_eq!(name(4, 183), Some("VB00_20"));
    }

    #[test]
    fn eight_character_names_resolve() {
        assert_eq!(name(2, 45), Some("VB00_31a"));
        assert_eq!(pack_path("VB00_31a"), "voice/vb00_31a.wav");
    }

    #[test]
    fn pack_paths_are_lowercased_voice_entries() {
        assert_eq!(pack_path("V004_00"), "voice/v004_00.wav");
        assert_eq!(pack_path("VA09_0A"), "voice/va09_0a.wav");
    }

    #[test]
    fn referenced_names_are_sorted_and_unique() {
        let names = referenced_names();
        assert_eq!(names.len(), 517);
        assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(names.contains(&"V004_00"));
        assert!(names.contains(&"V110_00"));
        assert!(!names.contains(&""));
    }
}
