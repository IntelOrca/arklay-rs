//! The install's 14x14 game-text font encoding: characters <-> glyph bytes.
//!
//! `font.rs` renders an encoded stream by addressing the font grid directly
//! (`0x0C..=0xF7`), through the `0xF8`/`0xF9`/`0xFA` extended pages and a few
//! cursor controls. This module is the inverse direction: it turns the
//! characters of a language-file value into those bytes, and names the cell a
//! byte selects. The grids in [`table`] are generated from the same reference
//! map (`FONT.TIM`) that `font.rs` was verified against, so encoding and
//! rendering always agree.
//!
//! [`encode`] is greedy: a two-codepoint cell (`S.`) matches before its single
//! characters would, so text written with the ligature cells re-encodes to the
//! bytes the game uses. Characters the font has no cell for either map to a
//! documented substitute ([`SUBSTITUTIONS`]: curly quotes, dashes, middle
//! dots, fullwidth ASCII) or encode as [`MISSING`], the cell
//! [`crate::font::Font::new`] paints as an empty box so an unspellable
//! character is visible instead of silently dropped.
//!
//! This is the Japanese font's map. A pack whose font sheet is the 256-wide
//! Western one uses a different byte assignment; a per-font charset lands with
//! the first pack that ships one.

mod table;

/// Longest cell spelling, in characters.
pub const MAX_CELL_CHARS: usize = 2;

/// The byte a character without a cell encodes to.
///
/// It selects LEFT row 0, column 11 — a blank cell in the shipped fonts — and
/// [`crate::font::Font::new`] paints that cell as a hollow box, so a missing
/// glyph reads as a box rather than as text in another language.
pub const MISSING: u8 = 0x0B;

/// Characters with no cell of their own that localized text still uses.
///
/// Each maps to the closest cell the font draws; every target is directly
/// encodable and is not itself a substitution key. Sorted by character.
static SUBSTITUTIONS: &[(char, &str)] = &[
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

/// Encode `text` into font bytes, appending to `out`.
///
/// `text` is the literal part of a language-file value; tag and escape syntax
/// is the caller's job ([`crate::language`]). A character the font cannot
/// spell encodes as [`MISSING`].
pub fn encode(text: &str, out: &mut Vec<u8>) {
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
            if let Ok(index) = table::ENCODE_TWO.binary_search_by(|(key, _)| (*key).cmp(pair)) {
                out.extend_from_slice(table::ENCODE_TWO[index].1);
                rest = &rest[first_len + second_len..];
                continue;
            }
        }
        encode_char(first, out);
        rest = &rest[first_len..];
    }
}

/// The cell a plain glyph byte (`0x0C..=0xF7`) selects.
pub fn plain_cell(byte: u8) -> Option<&'static str> {
    if byte >= 0xF8 {
        return None;
    }
    let index = usize::from(byte);
    let row = index / 18;
    let col = index % 18;
    table::LEFT
        .get(row * 18 + col)
        .copied()
        .filter(|cell| !cell.is_empty())
}

/// The cell an extended glyph (`0xF8`/`0xF9`/`0xFA` plus its operand) selects.
///
/// The page byte follows `font.rs`: `0xF8` addresses the LEFT rows 13..15,
/// `0xF9` the RIGHT rows 0..15 and `0xFA` the RIGHT rows 14..17.
pub fn extended_cell(page: u8, operand: u8) -> Option<&'static str> {
    let index = usize::from(operand);
    let row = match page {
        0xF8 => index / 18 + 13,
        0xF9 => index / 18,
        0xFA => index / 18 + 14,
        _ => return None,
    };
    let grid: &[&str] = if page == 0xF8 {
        &table::LEFT
    } else {
        &table::RIGHT
    };
    grid.get(row * 18 + index % 18)
        .copied()
        .filter(|cell| !cell.is_empty())
}

/// Whether the font can spell `ch`, directly or through a substitution.
pub fn spellable(ch: char) -> bool {
    let ch = fold_fullwidth(ch);
    SUBSTITUTIONS
        .binary_search_by(|(key, _)| key.cmp(&ch))
        .is_ok()
        || table::ENCODE_ONE
            .binary_search_by(|(key, _)| key.cmp(&ch))
            .is_ok()
}

/// Encode one character: fold fullwidth ASCII, apply a substitution, or use
/// the table directly (falling back to [`MISSING`]).
fn encode_char(ch: char, out: &mut Vec<u8>) {
    let ch = fold_fullwidth(ch);
    if let Ok(index) = SUBSTITUTIONS.binary_search_by(|(key, _)| key.cmp(&ch)) {
        for target in SUBSTITUTIONS[index].1.chars() {
            push_glyph(target, out);
        }
        return;
    }
    push_glyph(ch, out);
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

/// Push the encoding of one character: its cell, or the missing-glyph cell.
fn push_glyph(ch: char, out: &mut Vec<u8>) {
    match table::ENCODE_ONE.binary_search_by(|(key, _)| key.cmp(&ch)) {
        Ok(index) => out.extend_from_slice(table::ENCODE_ONE[index].1),
        Err(_) => out.push(MISSING),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        encode(text, &mut out);
        out
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
        assert_eq!(encoded("＋"), [MISSING]);
    }

    #[test]
    fn characters_without_cells_encode_as_the_missing_glyph() {
        assert_eq!(encoded("é"), [MISSING]);
        assert_eq!(encoded("З"), [MISSING]);
        assert_eq!(encoded("+"), [MISSING]);
        assert_eq!(encoded("aéa"), [0x3D, MISSING, 0x3D]);
    }

    #[test]
    fn plain_cell_names_the_grid() {
        assert_eq!(plain_cell(0x00), Some(" "));
        assert_eq!(plain_cell(0x1D), Some("A"));
        assert_eq!(plain_cell(0xFF), None); // beyond the grid
        assert_eq!(plain_cell(0x09), None); // an unnamed cell
    }

    #[test]
    fn extended_cell_names_the_pages() {
        assert_eq!(extended_cell(0xF8, 0x12), Some("S."));
        assert_eq!(extended_cell(0xF8, 0x10), Some("「"));
        assert_eq!(extended_cell(0xF9, 0x00), Some("壊"));
        assert_eq!(extended_cell(0xFA, 0x33), Some("潟"));
        assert_eq!(extended_cell(0xFA, 0xFF), None);
        assert_eq!(extended_cell(0x00, 0x00), None);
    }

    #[test]
    fn every_grid_cell_is_encodable() {
        for cell in table::LEFT.iter().chain(table::RIGHT.iter()) {
            if !cell.is_empty() {
                assert_ne!(
                    encoded(cell),
                    [MISSING],
                    "grid cell {cell:?} has no encoding"
                );
            }
        }
    }

    #[test]
    fn substitutions_are_sorted_and_their_targets_encodable() {
        for (index, (ch, target)) in SUBSTITUTIONS.iter().enumerate() {
            if index > 0 {
                assert!(SUBSTITUTIONS[index - 1].0 < *ch, "unsorted at {ch:?}");
            }
            assert!(!target.is_empty());
            for target_ch in target.chars() {
                assert!(
                    SUBSTITUTIONS
                        .binary_search_by(|(key, _)| key.cmp(&target_ch))
                        .is_err(),
                    "{ch:?} substitutes to the substitution key {target_ch:?}"
                );
                assert_ne!(
                    encoded(&target_ch.to_string()),
                    [MISSING],
                    "{ch:?} -> {target:?} is not encodable"
                );
            }
        }
    }
}
