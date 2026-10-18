//! The install's game-text font encodings: characters <-> glyph bytes.
//!
//! `font.rs` renders an encoded stream by addressing the font grid directly
//! (`0x0C..=0xF7`), through the `0xF8`/`0xF9`/`0xFA` extended pages and a few
//! cursor controls. This module is the inverse direction: it turns the
//! characters of a language-file value into those bytes, and names the cell a
//! byte selects. The grids in `table` are generated from the same reference
//! map (`FONT.TIM`) that `font.rs` was verified against, so encoding and
//! rendering always agree.
//!
//! Two sheets ship with the games and each assigns the bytes its own way: the
//! 768-wide Japanese `FONT.TIM` (14px cells, kana through the extended pages)
//! and the 256-wide Western `fontus.tim` (8px cells, accented Latin letters
//! and the European punctuation). [`Script`] selects the map; a pack's font
//! sheet selects the script by its width, so the language files, the message
//! streams and the font always agree (see [`crate::text::Text`]).
//!
//! [`encode`] is greedy: a two-codepoint cell (`S.`) matches before its single
//! characters would, so text written with the ligature cells re-encodes to the
//! bytes the game uses. Characters the font has no cell for either map to a
//! documented substitute (each script's `SUBSTITUTIONS`: curly quotes, dashes,
//! middle dots, fullwidth ASCII) or encode as [`MISSING`], the reserved marker
//! [`crate::font::Font`] draws as a synthetic empty box so an unspellable
//! character is visible instead of silently dropped.

mod latin;
mod table;

/// Longest cell spelling, in characters.
pub const MAX_CELL_CHARS: usize = 2;

/// The bytes a character without a cell encodes to.
///
/// The pair is a marker, not a sheet address: [`crate::font::Font`] draws a
/// synthetic hollow box for it, so an unspellable character reads as a box
/// rather than as text in another language or as nothing at all.
///
/// `0xFA` addresses RIGHT rows 14..=17, so the operand `0xFF` (row 28) lies
/// past every page's table entries and past the 256-high sheet: the shipped
/// charsets cannot produce it, and nothing samples the sheet for it. Cells
/// that are real must not be reused here: the shipped sheet's `0x02..=0x0B`
/// cells are the controller-symbol glyphs (incl. the `0x0B` ▼ the message
/// machine draws for a page wait), and its one blank, unnamed cell,
/// `0xFA 0x3B`, is the blank lead-in the ja-JP table spells item `0x54`'s
/// name with.
pub const MISSING: [u8; 2] = [0xFA, 0xFF];

/// The game-text byte space a font sheet uses.
///
/// The sheet's width selects it ([`crate::font::FontMetrics`]): 768 or wider
/// is the Japanese sheet, 256 the Western one. A session's script is fixed by
/// the language it renders ([`crate::text::Text::script`]), and every stream
/// the session draws — language files, RDT text, UI labels — is encoded in
/// that one byte space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Script {
    /// The 768-wide Japanese sheet (`DATA/FONT.TIM`): 14px cells, kana and
    /// kanji through the `0xF8`/`0xF9`/`0xFA` pages.
    #[default]
    Japanese,
    /// The 256-wide Western sheet (`Data/fontus.tim`): 8px cells with the
    /// accented Latin letters and the European punctuation, all plain cells.
    Latin,
}

/// One sheet's character map: the encode tables, the authored substitutions
/// and the cell names the decode direction reports.
struct Charset {
    /// Single characters with their own cell, sorted by character.
    one: &'static [(char, &'static [u8])],
    /// Two-codepoint cells, sorted by spelling.
    two: &'static [(&'static str, &'static [u8])],
    /// Characters with no cell of their own, mapped to a spellable stand-in.
    substitutions: &'static [(char, &'static str)],
    /// Plain grid cell names, row-major, 18 columns.
    left: &'static [&'static str],
    /// The right page's cell names (`0xF9`/`0xFA`); empty for a single-page
    /// sheet.
    right: &'static [&'static str],
}

/// Characters with no cell of their own that localized text still uses on the
/// Japanese sheet.
///
/// Each maps to the closest cell the font draws; every target is directly
/// encodable and is not itself a substitution key. Sorted by character.
static JAPANESE_SUBSTITUTIONS: &[(char, &str)] = &[
    ('"', "”"),
    ('-', "ー"),
    ('_', "ー"),
    ('«', "“"),
    ('·', "・"),
    ('»', "”"),
    ('\u{2010}', "ー"),
    ('\u{2012}', "ー"),
    ('\u{2013}', "ー"),
    ('\u{2014}', "ー"),
    ('‘', "'"),
    ('’', "'"),
    ('„', "“"),
    ('‟', "“"),
    ('\u{2022}', "・"),
    ('\u{2027}', "・"),
    ('‹', "“"),
    ('›', "”"),
    ('\u{2212}', "ー"),
    ('\u{3000}', " "),
];

static JAPANESE: Charset = Charset {
    one: &table::ENCODE_ONE,
    two: &table::ENCODE_TWO,
    substitutions: JAPANESE_SUBSTITUTIONS,
    left: &table::LEFT,
    right: &table::RIGHT,
};

static LATIN: Charset = Charset {
    one: &latin::ENCODE_ONE,
    two: &latin::ENCODE_TWO,
    substitutions: &latin::SUBSTITUTIONS,
    left: &latin::LEFT,
    right: &[],
};

impl Script {
    /// The sheet's character map.
    fn charset(self) -> &'static Charset {
        match self {
            Script::Japanese => &JAPANESE,
            Script::Latin => &LATIN,
        }
    }
}

impl Charset {
    /// The bytes for one character, or `None` when the sheet has no cell.
    fn lookup(&self, ch: char) -> Option<&'static [u8]> {
        self.one
            .binary_search_by(|(key, _)| key.cmp(&ch))
            .ok()
            .map(|index| self.one[index].1)
    }

    /// The stand-in for a character the sheet cannot spell.
    fn substitute(&self, ch: char) -> Option<&'static str> {
        self.substitutions
            .binary_search_by(|(key, _)| key.cmp(&ch))
            .ok()
            .map(|index| self.substitutions[index].1)
    }
}

/// Encode `text` into font bytes, appending to `out`.
///
/// `text` is the literal part of a language-file value; tag and escape syntax
/// is the caller's job ([`crate::language`]). A character the font cannot
/// spell encodes as [`MISSING`].
pub fn encode(text: &str, script: Script, out: &mut Vec<u8>) {
    let charset = script.charset();
    let mut rest = text;
    while !rest.is_empty() {
        let first = rest.chars().next().expect("the string is not empty");
        let first_len = first.len_utf8();
        // A two-codepoint cell wins over the same characters spelled apart.
        if first_len < rest.len() {
            let second_len = rest[first_len..]
                .chars()
                .next()
                .expect("a second character")
                .len_utf8();
            let pair = &rest[..first_len + second_len];
            if let Ok(index) = charset.two.binary_search_by(|(key, _)| (*key).cmp(pair)) {
                out.extend_from_slice(charset.two[index].1);
                rest = &rest[first_len + second_len..];
                continue;
            }
        }
        encode_char(first, charset, out);
        rest = &rest[first_len..];
    }
}

/// Decode a game-text byte stream back into characters, appending to `out`.
///
/// The inverse of [`encode`] for every cell the sheet names. Stream controls
/// are not characters: `0x01`/`0x07` end the stream, `0x02` is a newline,
/// `0x03`..`0x06` and `0x08` skip their operands, `0xFB`/`0xFF` are spacing
/// controls, and [`MISSING`] decodes as `?` so an unspellable character stays
/// visible. A byte no cell names is skipped.
pub fn decode(bytes: &[u8], script: Script, out: &mut String) {
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        index += 1;
        match byte {
            0x01 | 0x07 => break,
            0x00 => out.push(' '),
            0x02 => out.push('\n'),
            0x03..=0x06 => index += 1,
            0x08 => index += 3,
            0xFB | 0xFF => {}
            0xF8..=0xFA => {
                let Some(&operand) = bytes.get(index) else {
                    break;
                };
                index += 1;
                if byte == 0xFA && operand == 0xFF {
                    out.push('?');
                } else if let Some(cell) = extended_cell(byte, operand, script) {
                    out.push_str(cell);
                }
            }
            _ => {
                if let Some(cell) = plain_cell(byte, script) {
                    out.push_str(cell);
                }
            }
        }
    }
}

/// The cell a plain glyph byte (`0x0C..=0xF7`) selects.
pub fn plain_cell(byte: u8, script: Script) -> Option<&'static str> {
    if byte >= 0xF8 {
        return None;
    }
    let index = usize::from(byte);
    let row = index / 18;
    let col = index % 18;
    script
        .charset()
        .left
        .get(row * 18 + col)
        .copied()
        .filter(|cell| !cell.is_empty())
}

/// The cell an extended glyph (`0xF8`/`0xF9`/`0xFA` plus its operand) selects.
///
/// The page byte follows `font.rs`: `0xF8` addresses the LEFT rows 13..15,
/// `0xF9` the RIGHT rows 0..15 and `0xFA` the RIGHT rows 14..17. The Western
/// sheet is a single page, so only its `0xF8` page has names.
pub fn extended_cell(page: u8, operand: u8, script: Script) -> Option<&'static str> {
    let index = usize::from(operand);
    let row = match page {
        0xF8 => index / 18 + 13,
        0xF9 => index / 18,
        0xFA => index / 18 + 14,
        _ => return None,
    };
    let charset = script.charset();
    let grid: &[&str] = if page == 0xF8 {
        charset.left
    } else {
        charset.right
    };
    grid.get(row * 18 + index % 18)
        .copied()
        .filter(|cell| !cell.is_empty())
}

/// Whether the font can spell `ch`, directly or through a substitution.
pub fn spellable(ch: char, script: Script) -> bool {
    let charset = script.charset();
    charset.lookup(ch).is_some()
        || charset.substitute(fold_fullwidth(ch)).is_some()
        || charset.lookup(fold_fullwidth(ch)).is_some()
}

/// Encode one character: the sheet's own cell, a fullwidth fold, or a
/// substitution (falling back to [`MISSING`]).
fn encode_char(ch: char, charset: &Charset, out: &mut Vec<u8>) {
    // A character the sheet spells itself wins over its fullwidth fold: the
    // Western sheet spells the seeds' fullwidth `ＳＴＡＲＳ` as the ligature
    // cells the original text uses, where the fold would spell the letters
    // apart.
    if let Some(bytes) = charset.lookup(ch) {
        out.extend_from_slice(bytes);
        return;
    }
    let folded = fold_fullwidth(ch);
    if let Some(target) = charset.substitute(folded) {
        for target_ch in target.chars() {
            match charset.lookup(target_ch) {
                Some(bytes) => out.extend_from_slice(bytes),
                None => out.extend_from_slice(&MISSING),
            }
        }
        return;
    }
    match charset.lookup(folded) {
        Some(bytes) => out.extend_from_slice(bytes),
        None => out.extend_from_slice(&MISSING),
    }
}

/// Fold a fullwidth ASCII character to its ASCII form; other characters pass
/// through.
fn fold_fullwidth(ch: char) -> char {
    if ('\u{FF01}'..='\u{FF5E}').contains(&ch) {
        // U+FF01..U+FF5E are the fullwidth forms of U+0021..U+007E.
        char::from_u32(ch as u32 - 0xFEE0).expect("an ASCII character")
    } else {
        ch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        encode(text, Script::Japanese, &mut out);
        out
    }

    fn latin(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        encode(text, Script::Latin, &mut out);
        out
    }

    #[test]
    fn decode_is_the_inverse_of_encode() {
        // Every string round-trips in a script that can spell it: the
        // Japanese sheet spells Latin letters, the Western one no kana.
        for (script, text) in [
            (Script::Japanese, "Combat Knife"),
            (Script::Japanese, "M. Room 1F"),
            (Script::Japanese, "セーブしない"),
            (Script::Japanese, "S.T.A.R.S."),
            (Script::Latin, "Combat Knife"),
            (Script::Latin, "M. Room 1F"),
            (Script::Latin, "S.T.A.R.S."),
            (Script::Latin, "a b  c"),
        ] {
            let mut bytes = Vec::new();
            encode(text, script, &mut bytes);
            let mut decoded = String::new();
            decode(&bytes, script, &mut decoded);
            assert_eq!(decoded, text, "{text:?} in {script:?}");
        }
    }

    #[test]
    fn decode_reads_controls_and_the_missing_marker() {
        // The `0x01` terminator ends the stream; `0x02` is a newline.
        let mut decoded = String::new();
        decode(&[0x1D, 0x1E, 0x01, 0x1F], Script::Latin, &mut decoded);
        assert_eq!(decoded, "AB");

        decoded.clear();
        decode(&[0x1D, 0x02, 0x1E], Script::Latin, &mut decoded);
        assert_eq!(decoded, "A\nB");

        // Colour/clear/scroll tags and their operands are not characters.
        decoded.clear();
        decode(
            &[0x05, 0x01, 0x1D, 0x03, 0x02, 0x1E],
            Script::Latin,
            &mut decoded,
        );
        assert_eq!(decoded, "AB");

        // The unspellable marker stays visible, spacing bytes vanish.
        decoded.clear();
        decode(
            &[0x1D, 0xFA, 0xFF, 0xFB, 0xFF, 0x1E],
            Script::Latin,
            &mut decoded,
        );
        assert_eq!(decoded, "A?B");

        // An extended cell decodes through its page.
        decoded.clear();
        decode(&[0xF9, 0x09], Script::Japanese, &mut decoded);
        assert_eq!(decoded, "館");
    }

    #[test]
    fn decode_tolerates_truncated_streams() {
        let mut decoded = String::new();
        decode(&[0xF9], Script::Japanese, &mut decoded);
        assert_eq!(decoded, "");
        decoded.clear();
        decode(&[], Script::Japanese, &mut decoded);
        assert_eq!(decoded, "");
    }

    #[test]
    fn encodes_plain_cells() {
        assert_eq!(encoded(" "), [0x00]);
        assert_eq!(encoded("0"), [0x0C]);
        assert_eq!(encoded("A"), [0x1D]);
        assert_eq!(encoded("S"), [0x2F]);
        assert_eq!(encoded("a"), [0x3D]);
        assert_eq!(encoded("あ"), [0x57]);
    }

    #[test]
    fn encodes_extended_cells() {
        assert_eq!(encoded("館"), [0xF9, 0x09]);
        assert_eq!(encoded("潟"), [0xFA, 0x33]);
        assert_eq!(encoded("「"), [0xF8, 0x10]);
        assert_eq!(encoded("」"), [0xF8, 0x11]);
    }

    #[test]
    fn greedy_pairs_win_over_singles() {
        assert_eq!(encoded("S."), [0xF8, 0x12]);
        assert_eq!(encoded("T."), [0xF8, 0x13]);
        assert_eq!(encoded("S.T."), [0xF8, 0x12, 0xF8, 0x13]);
        assert_eq!(encoded("ST"), [0x2F, 0x30]);
    }

    #[test]
    fn canonical_cells_win_for_shared_characters() {
        // The closing quote and the Latin full stop have cells the game uses.
        assert_eq!(encoded("”"), [0xF8, 0x1B]);
        assert_eq!(encoded("."), [0xF8, 0x1C]);
        // The reference aliases the Latin marks the font lacks.
        assert_eq!(encoded(","), encoded("、"));
        assert_eq!(encoded(";"), encoded("、"));
        assert_eq!(encoded("\\"), encoded("/"));
    }

    #[test]
    fn substitutes_characters_without_cells() {
        assert_eq!(encoded("\"Closet\""), encoded("”Closet”"));
        assert_eq!(encoded("“Closet\""), encoded("“Closet”"));
        assert_eq!(encoded("-"), encoded("ー"));
        assert_eq!(encoded("·"), encoded("・"));
        assert_eq!(encoded("„"), encoded("“"));
        assert_eq!(encoded("‟"), encoded("“"));
        assert_eq!(encoded("«"), encoded("“"));
        assert_eq!(encoded("»"), encoded("”"));
        assert_eq!(encoded("’"), encoded("'"));
        assert_eq!(encoded("　"), encoded(" "));
    }

    #[test]
    fn folds_fullwidth_ascii() {
        assert_eq!(encoded("ＳＴＡＲＳ"), encoded("STARS"));
        assert_eq!(encoded("１"), encoded("1"));
        // Folding does not invent glyphs the font lacks.
        assert_eq!(encoded("＋"), MISSING);
    }

    #[test]
    fn characters_without_cells_encode_as_the_missing_glyph() {
        assert_eq!(MISSING, [0xFA, 0xFF], "the pair is the reserved marker");
        assert_eq!(encoded("é"), MISSING);
        assert_eq!(encoded("З"), MISSING);
        assert_eq!(encoded("+"), MISSING);
        assert_eq!(encoded("aéa"), [0x3D, 0xFA, 0xFF, 0x3D]);
    }

    #[test]
    fn the_missing_marker_is_not_a_table_cell() {
        // Reserving a real cell would make the charset emit the box for a
        // spellable character. `0xFA 0x3B` in particular is a real stream
        // byte: the ja-JP table spells item 0x54's name with it.
        assert_eq!(
            extended_cell(MISSING[0], MISSING[1], Script::Japanese),
            None
        );
        assert_eq!(extended_cell(MISSING[0], MISSING[1], Script::Latin), None);
        assert_eq!(plain_cell(MISSING[0], Script::Japanese), None);
        assert_eq!(plain_cell(MISSING[0], Script::Latin), None);
        for script in [Script::Japanese, Script::Latin] {
            let charset = script.charset();
            for (ch, bytes) in charset.one.iter() {
                assert_ne!(*bytes, MISSING.as_slice(), "{ch:?} encodes as the marker");
            }
            for (cell, bytes) in charset.two.iter() {
                assert_ne!(*bytes, MISSING.as_slice(), "{cell:?} encodes as the marker");
            }
        }
    }

    #[test]
    fn plain_cell_names_the_grid() {
        assert_eq!(plain_cell(0x00, Script::Japanese), Some(" "));
        assert_eq!(plain_cell(0x1D, Script::Japanese), Some("A"));
        assert_eq!(plain_cell(0xFF, Script::Japanese), None); // beyond the grid
        assert_eq!(plain_cell(0x09, Script::Japanese), None); // an unnamed cell
        assert_eq!(plain_cell(0x18, Script::Latin), Some(","));
        assert_eq!(plain_cell(0x37, Script::Latin), Some("("));
        assert_eq!(plain_cell(0x65, Script::Latin), Some("é"));
        assert_eq!(plain_cell(0x90, Script::Latin), None); // a symbol row
        assert_eq!(plain_cell(0xFF, Script::Latin), None);
    }

    #[test]
    fn extended_cell_names_the_pages() {
        assert_eq!(extended_cell(0xF8, 0x12, Script::Japanese), Some("S."));
        assert_eq!(extended_cell(0xF8, 0x10, Script::Japanese), Some("「"));
        assert_eq!(extended_cell(0xF9, 0x00, Script::Japanese), Some("壊"));
        assert_eq!(extended_cell(0xFA, 0x33, Script::Japanese), Some("潟"));
        assert_eq!(extended_cell(0xFA, 0xFF, Script::Japanese), None);
        assert_eq!(extended_cell(0x00, 0x00, Script::Japanese), None);
        // The Western sheet is a single page: no right-page cells at all.
        assert_eq!(extended_cell(0xF9, 0x00, Script::Latin), None);
        assert_eq!(extended_cell(0xFA, 0x00, Script::Latin), None);
    }

    #[test]
    fn every_grid_cell_is_encodable() {
        for script in [Script::Japanese, Script::Latin] {
            let charset = script.charset();
            for cell in charset.left.iter().chain(charset.right.iter()) {
                if !cell.is_empty() {
                    let mut out = Vec::new();
                    encode(cell, script, &mut out);
                    assert_ne!(out, MISSING, "grid cell {cell:?} has no encoding");
                }
            }
        }
    }

    #[test]
    fn substitutions_are_sorted_and_their_targets_encodable() {
        for script in [Script::Japanese, Script::Latin] {
            let charset = script.charset();
            for (index, (ch, target)) in charset.substitutions.iter().enumerate() {
                if index > 0 {
                    assert!(
                        charset.substitutions[index - 1].0 < *ch,
                        "unsorted at {ch:?}"
                    );
                }
                assert!(!target.is_empty());
                for target_ch in target.chars() {
                    assert!(
                        charset
                            .substitutions
                            .binary_search_by(|(key, _)| key.cmp(&target_ch))
                            .is_err(),
                        "{ch:?} substitutes to the substitution key {target_ch:?}"
                    );
                    let mut out = Vec::new();
                    encode(&target_ch.to_string(), script, &mut out);
                    assert_ne!(out, MISSING, "{ch:?} -> {target:?} is not encodable");
                }
            }
        }
    }

    #[test]
    fn latin_encodes_the_official_usa_bytes() {
        // The bytes the USA release's own text tables and RDTs hold.
        assert_eq!(
            latin("COMBAT KNIFE"),
            [
                0x1F, 0x2B, 0x29, 0x1E, 0x1D, 0x30, 0x00, 0x27, 0x2A, 0x25, 0x22, 0x21
            ]
        );
        assert_eq!(latin("Beretta"), [0x1E, 0x41, 0x4E, 0x41, 0x50, 0x50, 0x3D]);
        assert_eq!(latin("café"), [0x3F, 0x3D, 0x42, 0x65]);
        // `S.T.A.R.S.` is the ligature cells, period included.
        assert_eq!(latin("S.T.A.R.S."), [0x74, 0x75, 0x76, 0x77, 0x74]);
        // The seeds spell it fullwidth; it maps to the same cells.
        assert_eq!(latin("ＳＴＡＲＳ"), [0x74, 0x75, 0x76, 0x77, 0x74]);
        // A plain "STARS" keeps the plain letters.
        assert_eq!(latin("STARS"), [0x2F, 0x30, 0x1D, 0x2E, 0x2F]);
    }

    #[test]
    fn latin_spells_the_slash_as_its_backslash_cell() {
        // The Western sheet has no `/` cell; the USA save screen's separators
        // are its backslash, so the substitute keeps localized text readable.
        assert_eq!(latin("/"), latin("\\"));
        assert_eq!(latin("/"), [0x38]);
        assert_eq!(latin("10/18"), latin("10\\18"));
        // The Japanese sheet spells both itself.
        assert_eq!(encoded("/"), encoded("\\"));
    }

    #[test]
    fn latin_punctuation_cells_differ_from_the_japanese_sheet() {
        assert_eq!(latin(","), [0x18]);
        assert_eq!(encoded(","), [0x17]);
        assert_eq!(latin(";"), [0x17]);
        // The Japanese sheet spells both comma and semicolon as its
        // ideographic comma.
        assert_eq!(encoded(";"), [0x17]);
        assert_eq!(latin("("), [0x37]);
        assert_eq!(encoded("("), [0xF8, 0x16]);
        // The Western sheet spells `+` and `=` where the Japanese one cannot.
        assert_eq!(latin("+"), [0x7D]);
        assert_eq!(encoded("+"), MISSING);
        assert_eq!(latin("="), [0x7E]);
        assert_eq!(encoded("="), MISSING);
        // The save screen's empty-slot dashes are the same byte on both.
        assert_eq!(latin("ー"), [0x3B]);
        assert_eq!(encoded("ー"), [0x3B]);
        assert_eq!(latin("-"), [0x3B]);
        assert_eq!(encoded("-"), [0x3B]);
    }

    #[test]
    fn latin_marks_what_the_western_sheet_cannot_spell() {
        // The original European releases' font has no Nordic, Iberian or
        // Cyrillic letters: those read as the missing-glyph box.
        assert_eq!(latin("ñ"), MISSING);
        assert_eq!(latin("å"), MISSING);
        assert_eq!(latin("З"), MISSING);
        assert_eq!(latin("café"), [0x3F, 0x3D, 0x42, 0x65]);
        assert_eq!(latin("naïve"), [0x4A, 0x3D, 0x69, 0x52, 0x41]);
        assert_eq!(
            latin("a\u{301}"),
            [0x3D, 0xFA, 0xFF],
            "a combining mark is not folded into its base letter"
        );
    }
}
