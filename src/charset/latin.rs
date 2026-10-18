//! The 256-wide Western sheet's game-text map (`Data/fontus.tim`).
//!
//! The European and North American releases ship the same 256x256 4bpp sheet
//! the Japanese `FONT.TIM` replaces: the same 18-column game-text grid at
//! `v = 28`, but with 8px cells and a different byte assignment. Where the
//! Japanese sheet spends rows 3..7 on kana, this one spells the accented
//! Latin letters the European releases need (`ÄÖÜäöüßÀÂÈÉÊÏÎÔÙÛÇç`), and its
//! punctuation cells differ too: `,` is `0x18` here against `0x17` on the
//! Japanese sheet, `;` is `0x17` against `0x16`, and `(`, `\`, `)` sit at
//! `0x37..0x39` where the Japanese sheet has `[`, `/`, `]`.
//!
//! The sheet is a single page: the `0xF9`/`0xFA` extended escapes have no
//! cells here, and the only text the shipped USA/FRA/GER RDTs use is the
//! plain grid (they carry no `0xF8` escape at all). The `S.`/`T.`/`A.`/`R.`
//! ligature cells (which carry the trailing period) live in the plain grid at
//! `0x74..=0x77`, and the seeds spell `S.T.A.R.S.` as the fullwidth
//! `ＳＴＡＲＳ`, which maps to those same cells.
//!
//! The rows below are the decoded sheet: `LEFT` names each cell (rows 0..7
//! are the game-text grid, row 8 is the 14px controller-symbol row, and the
//! rest of the sheet is blank), `ENCODE_ONE`/`ENCODE_TWO` are the inverse
//! map, and `SUBSTITUTIONS` aliases the characters the sheet has no cell for.

/// plain byte b -> LEFT row b/18, col b%18; `""` marks a cell with no
/// Unicode spelling (the controller symbols draw as raw bytes).
#[rustfmt::skip]
pub(super) static LEFT: [&str; 288] = [
    // row 0 (the plain grid's first row; 0x02..=0x0B are the symbols)
    " ", "", "", "", "", "", "", "", "", "", "", "", "0", "1", "2", "3", "4", "5",
    // row 1
    "6", "7", "8", "9", ":", ";", ",", "”", "!", "?", "‽", "A", "B", "C", "D", "E", "F", "G",
    // row 2
    "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T", "U", "V", "W", "X", "Y",
    // row 3
    "Z", "(", "\\", ")", "'", "-", "·", "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k",
    // row 4
    "l", "m", "n", "o", "p", "q", "r", "s", "t", "u", "v", "w", "x", "y", "z", "Ä", "ä", "Ö",
    // row 5
    "ö", "Ü", "ü", "ß", "À", "à", "Â", "â", "È", "è", "É", "é", "Ê", "ê", "Ï", "ï", "Î", "î",
    // row 6
    "Ô", "ô", "Ù", "ù", "Û", "û", "Ç", "ç", "S.", "T.", "A.", "R.", "“", ".", "…", "–", "–", "+",
    // row 7
    "=", " ", " ", " ", " ", " ", " ", " ", " ", " ", " ", " ", " ", " ", " ", " ", " ", " ",
    // row 8 (the 14px controller-symbol row, no Unicode spelling)
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    // row 9
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    // row 10
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    // row 11
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    // row 12
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    // row 13
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    // row 14
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    // row 15
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
];

/// Character -> glyph bytes for the Western sheet's plain grid.
/// Sorted by character; every entry's byte is a plain grid cell
/// (`0x00..=0xF7`) the renderer samples at the 8px cell size.
#[rustfmt::skip]
pub(super) static ENCODE_ONE: [(char, &[u8]); 116] = [
    (' ', &[0x00]),
    ('!', &[0x1A]),
    ('"', &[0x19]),
    ('\'', &[0x3A]),
    ('(', &[0x37]),
    (')', &[0x39]),
    ('+', &[0x7D]),
    (',', &[0x18]),
    ('-', &[0x3B]),
    ('.', &[0x79]),
    ('0', &[0x0C]),
    ('1', &[0x0D]),
    ('2', &[0x0E]),
    ('3', &[0x0F]),
    ('4', &[0x10]),
    ('5', &[0x11]),
    ('6', &[0x12]),
    ('7', &[0x13]),
    ('8', &[0x14]),
    ('9', &[0x15]),
    (':', &[0x16]),
    (';', &[0x17]),
    ('=', &[0x7E]),
    ('?', &[0x1B]),
    ('A', &[0x1D]),
    ('B', &[0x1E]),
    ('C', &[0x1F]),
    ('D', &[0x20]),
    ('E', &[0x21]),
    ('F', &[0x22]),
    ('G', &[0x23]),
    ('H', &[0x24]),
    ('I', &[0x25]),
    ('J', &[0x26]),
    ('K', &[0x27]),
    ('L', &[0x28]),
    ('M', &[0x29]),
    ('N', &[0x2A]),
    ('O', &[0x2B]),
    ('P', &[0x2C]),
    ('Q', &[0x2D]),
    ('R', &[0x2E]),
    ('S', &[0x2F]),
    ('T', &[0x30]),
    ('U', &[0x31]),
    ('V', &[0x32]),
    ('W', &[0x33]),
    ('X', &[0x34]),
    ('Y', &[0x35]),
    ('Z', &[0x36]),
    ('\\', &[0x38]),
    ('a', &[0x3D]),
    ('b', &[0x3E]),
    ('c', &[0x3F]),
    ('d', &[0x40]),
    ('e', &[0x41]),
    ('f', &[0x42]),
    ('g', &[0x43]),
    ('h', &[0x44]),
    ('i', &[0x45]),
    ('j', &[0x46]),
    ('k', &[0x47]),
    ('l', &[0x48]),
    ('m', &[0x49]),
    ('n', &[0x4A]),
    ('o', &[0x4B]),
    ('p', &[0x4C]),
    ('q', &[0x4D]),
    ('r', &[0x4E]),
    ('s', &[0x4F]),
    ('t', &[0x50]),
    ('u', &[0x51]),
    ('v', &[0x52]),
    ('w', &[0x53]),
    ('x', &[0x54]),
    ('y', &[0x55]),
    ('z', &[0x56]),
    ('\u{b7}', &[0x3C]),
    ('\u{c0}', &[0x5E]),
    ('\u{c2}', &[0x60]),
    ('\u{c4}', &[0x57]),
    ('\u{c7}', &[0x72]),
    ('\u{c8}', &[0x62]),
    ('\u{c9}', &[0x64]),
    ('\u{ca}', &[0x66]),
    ('\u{ce}', &[0x6A]),
    ('\u{cf}', &[0x68]),
    ('\u{d4}', &[0x6C]),
    ('\u{d6}', &[0x59]),
    ('\u{d9}', &[0x6E]),
    ('\u{db}', &[0x70]),
    ('\u{dc}', &[0x5B]),
    ('\u{df}', &[0x5D]),
    ('\u{e0}', &[0x5F]),
    ('\u{e2}', &[0x61]),
    ('\u{e4}', &[0x58]),
    ('\u{e7}', &[0x73]),
    ('\u{e8}', &[0x63]),
    ('\u{e9}', &[0x65]),
    ('\u{ea}', &[0x67]),
    ('\u{ee}', &[0x6B]),
    ('\u{ef}', &[0x69]),
    ('\u{f4}', &[0x6D]),
    ('\u{f6}', &[0x5A]),
    ('\u{f9}', &[0x6F]),
    ('\u{fb}', &[0x71]),
    ('\u{fc}', &[0x5C]),
    ('\u{2013}', &[0x7B]),
    ('\u{201c}', &[0x78]),
    ('\u{201d}', &[0x19]),
    ('\u{2026}', &[0x7A]),
    ('\u{203d}', &[0x1C]),
    ('\u{ff21}', &[0x76]),
    ('\u{ff32}', &[0x77]),
    ('\u{ff33}', &[0x74]),
    ('\u{ff34}', &[0x75]),
];

/// Two-codepoint cells: the S.T.A.R.S. ligature cells, which carry the
/// trailing period. `S.` is what the text uses for "S.", and the
/// fullwidth `ＳＴＡＲＳ` the seeds spell maps to the same cells.
#[rustfmt::skip]
pub(super) static ENCODE_TWO: [(&str, &[u8]); 4] = [
    ("A.", &[0x76]),
    ("R.", &[0x77]),
    ("S.", &[0x74]),
    ("T.", &[0x75]),
];

/// Characters with no cell of their own that localized text still uses;
/// each maps to the closest cell the Western sheet draws. Sorted by
/// character; no target is itself a substitution key.
///
/// `/` is the load-bearing one: the Western sheet has no slash cell, but the
/// releases use one as the save screen's field separator — the USA art spells
/// it `\` (`SaveLoadScreen.cpp`'s `"     \  \"`) where the Japanese sheet
/// spells it `/` — so a `/` in Latin text draws the sheet's backslash.
#[rustfmt::skip]
pub(super) static SUBSTITUTIONS: [(char, &str); 19] = [
    ('/', "\\"),
    ('_', "-"),
    ('\u{ab}', "“"),
    ('\u{bb}', "”"),
    ('\u{2010}', "-"),
    ('\u{2011}', "-"),
    ('\u{2012}', "–"),
    ('\u{2014}', "–"),
    ('\u{2018}', "'"),
    ('\u{2019}', "'"),
    ('\u{201e}', "“"),
    ('\u{201f}', "“"),
    ('\u{2022}', "·"),
    ('\u{2027}', "·"),
    ('\u{2039}', "“"),
    ('\u{203a}', "”"),
    ('\u{2212}', "-"),
    ('\u{3000}', " "),
    ('\u{30fc}', "-"),
];
