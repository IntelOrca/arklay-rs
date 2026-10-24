//! The Western game-text map: Classic REbirth's sheet (`font/latin.tim`).
//!
//! Classic REbirth draws every Western language with its own sheet,
//! `DATA/PBFONT0.TIM`, which ships here as `games/re1/font/latin.tim`: the
//! Japanese `FONT.TIM` left page's 14px cells, with the kana rows replaced by
//! accented Latin letters. Its byte assignment is the Western releases'
//! `fontus.tim` one for everything those sheets share (`,` is `0x18`, `;`
//! `0x17`, `Ä` `0x57`, the `S.`/`T.`/`A.`/`R.` ligatures `0x74..=0x77`), with
//! three moves — `(`/`)` at `0x05`/`0x06`, `«`/`»` at `0x09`/`0x0A`, and the
//! Japanese sheet's `[`, `/`, `]` at `0x37..=0x39` where `fontus.tim` has
//! `(`, `\`, `)` — and the Spanish, Nordic and other letters it adds at
//! `0x7F..=0xA3`. The tables follow CR's own `encoding.xml`.
//!
//! The sheet is a single page: the `0xF9`/`0xFA` extended escapes have no
//! cells here. `LEFT` names each plain cell, `ENCODE_ONE`/`ENCODE_TWO` are the
//! inverse map, `SUBSTITUTIONS` aliases the characters the sheet has no cell
//! for, and `WIDTHS` carries CR's proportional advances.

/// plain byte b -> LEFT row b/18, col b%18; `""` marks a cell with no
/// Unicode spelling (the `0x02`/`0x0B` symbols and the `0x80..=0x83` fill
/// pieces draw as raw bytes).
#[rustfmt::skip]
pub(super) static LEFT: [&str; 288] = [
    // row 0
    " ", "", "", "", "", "(", ")", "", "", "«", "»", "", "0", "1", "2", "3", "4", "5",
    // row 1
    "6", "7", "8", "9", ":", ";", ",", "”", "!", "?", "‽", "A", "B", "C", "D", "E", "F", "G",
    // row 2
    "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T", "U", "V", "W", "X", "Y",
    // row 3
    "Z", "[", "/", "]", "'", "-", "·", "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k",
    // row 4
    "l", "m", "n", "o", "p", "q", "r", "s", "t", "u", "v", "w", "x", "y", "z", "Ä", "ä", "Ö",
    // row 5
    "ö", "Ü", "ü", "ß", "À", "à", "Â", "â", "È", "è", "É", "é", "Ê", "ê", "Ï", "ï", "Î", "î",
    // row 6
    "Ô", "ô", "Ù", "ù", "Û", "û", "Ç", "ç", "S.", "T.", "A.", "R.", "“", ".", "…", "‒", "–", "+",
    // row 7
    "=", "&", "", "", "", "", "Ñ", "ñ", "Ë", "ë", "°", "ª", "Á", "á", "Í", "í", "Ó", "ó",
    // row 8
    "Ú", "ú", "¿", "¡", "Ì", "ì", "Ò", "ò", "$", "*", "„", "‟", "▲", "`", "Æ", "æ", "Ø", "ø",
    // row 9
    "Å", "å", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
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

/// Character -> glyph bytes for the sheet's plain grid.
/// Sorted by character; every entry's byte is a plain grid cell.
#[rustfmt::skip]
pub(super) static ENCODE_ONE: [(char, &[u8]); 154] = [
    (' ', &[0x00]),
    ('!', &[0x1A]),
    ('"', &[0x19]),
    ('$', &[0x98]),
    ('&', &[0x7F]),
    ('\'', &[0x3A]),
    ('(', &[0x05]),
    (')', &[0x06]),
    ('*', &[0x99]),
    ('+', &[0x7D]),
    (',', &[0x18]),
    ('-', &[0x3B]),
    ('.', &[0x79]),
    ('/', &[0x38]),
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
    ('[', &[0x37]),
    (']', &[0x39]),
    ('`', &[0x9D]),
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
    ('\u{a1}', &[0x93]),
    ('\u{aa}', &[0x89]),
    ('\u{ab}', &[0x09]),
    ('\u{b0}', &[0x88]),
    ('\u{b7}', &[0x3C]),
    ('\u{bb}', &[0x0A]),
    ('\u{bf}', &[0x92]),
    ('\u{c0}', &[0x5E]),
    ('\u{c1}', &[0x8A]),
    ('\u{c2}', &[0x60]),
    ('\u{c4}', &[0x57]),
    ('\u{c5}', &[0xA2]),
    ('\u{c6}', &[0x9E]),
    ('\u{c7}', &[0x72]),
    ('\u{c8}', &[0x62]),
    ('\u{c9}', &[0x64]),
    ('\u{ca}', &[0x66]),
    ('\u{cb}', &[0x86]),
    ('\u{cc}', &[0x94]),
    ('\u{cd}', &[0x8C]),
    ('\u{ce}', &[0x6A]),
    ('\u{cf}', &[0x68]),
    ('\u{d1}', &[0x84]),
    ('\u{d2}', &[0x96]),
    ('\u{d3}', &[0x8E]),
    ('\u{d4}', &[0x6C]),
    ('\u{d6}', &[0x59]),
    ('\u{d8}', &[0xA0]),
    ('\u{d9}', &[0x6E]),
    ('\u{da}', &[0x90]),
    ('\u{db}', &[0x70]),
    ('\u{dc}', &[0x5B]),
    ('\u{df}', &[0x5D]),
    ('\u{e0}', &[0x5F]),
    ('\u{e1}', &[0x8B]),
    ('\u{e2}', &[0x61]),
    ('\u{e4}', &[0x58]),
    ('\u{e5}', &[0xA3]),
    ('\u{e6}', &[0x9F]),
    ('\u{e7}', &[0x73]),
    ('\u{e8}', &[0x63]),
    ('\u{e9}', &[0x65]),
    ('\u{ea}', &[0x67]),
    ('\u{eb}', &[0x87]),
    ('\u{ec}', &[0x95]),
    ('\u{ed}', &[0x8D]),
    ('\u{ee}', &[0x6B]),
    ('\u{ef}', &[0x69]),
    ('\u{f1}', &[0x85]),
    ('\u{f2}', &[0x97]),
    ('\u{f3}', &[0x8F]),
    ('\u{f4}', &[0x6D]),
    ('\u{f6}', &[0x5A]),
    ('\u{f8}', &[0xA1]),
    ('\u{f9}', &[0x6F]),
    ('\u{fa}', &[0x91]),
    ('\u{fb}', &[0x71]),
    ('\u{fc}', &[0x5C]),
    ('\u{2012}', &[0x7B]),
    ('\u{2013}', &[0x7C]),
    ('\u{201c}', &[0x78]),
    ('\u{201d}', &[0x19]),
    ('\u{201e}', &[0x9A]),
    ('\u{201f}', &[0x9B]),
    ('\u{2026}', &[0x7A]),
    ('\u{203d}', &[0x1C]),
    ('\u{25b2}', &[0x9C]),
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
/// each maps to the closest cell the sheet draws. Sorted by character; no
/// target is itself a substitution key.
#[rustfmt::skip]
pub(super) static SUBSTITUTIONS: [(char, &str); 15] = [
    ('\\', "/"),
    ('_', "-"),
    ('\u{2010}', "-"),
    ('\u{2011}', "-"),
    ('\u{2014}', "–"),
    ('\u{2018}', "'"),
    ('\u{2019}', "'"),
    ('\u{2022}', "·"),
    ('\u{2027}', "·"),
    ('\u{2039}', "«"),
    ('\u{203a}', "»"),
    ('\u{2212}', "-"),
    ('\u{3000}', " "),
    ('\u{30fc}', "-"),
    ('\u{ff3f}', "-"),
];

/// Classic REbirth's per-glyph `(byte, width, indent)` for the cells whose
/// advance is not the default 8px: the cell draws shifted left by `indent`
/// and the cursor advances by `width`.
#[rustfmt::skip]
pub(super) static WIDTHS: [(u8, u8, u8); 47] = [
    (0x02, 14, 0),
    (0x05, 5, 3),
    (0x06, 5, 0),
    (0x09, 7, 6),
    (0x0A, 7, 1),
    (0x0B, 14, 0),
    (0x16, 4, 2),
    (0x17, 4, 2),
    (0x18, 4, 2),
    (0x19, 7, 0),
    (0x1A, 3, 2),
    (0x1B, 7, 0),
    (0x25, 5, 2),
    (0x26, 7, 0),
    (0x37, 5, 3),
    (0x39, 5, 3),
    (0x3A, 4, 0),
    (0x3B, 7, 0),
    (0x3C, 7, 0),
    (0x45, 5, 2),
    (0x46, 4, 2),
    (0x48, 5, 2),
    (0x49, 7, 0),
    (0x4E, 7, 0),
    (0x4F, 7, 0),
    (0x50, 6, 1),
    (0x54, 7, 0),
    (0x68, 5, 2),
    (0x69, 5, 2),
    (0x6A, 5, 2),
    (0x6B, 5, 2),
    (0x78, 7, 0),
    (0x79, 4, 0),
    (0x7E, 7, 0),
    (0x88, 5, 0),
    (0x89, 7, 0),
    (0x8C, 5, 2),
    (0x8D, 5, 2),
    (0x92, 7, 0),
    (0x93, 3, 2),
    (0x94, 5, 2),
    (0x95, 5, 2),
    (0x9A, 7, 0),
    (0x9B, 7, 0),
    (0x9D, 4, 2),
    (0x9E, 11, 0),
    (0x9F, 11, 0),
];
