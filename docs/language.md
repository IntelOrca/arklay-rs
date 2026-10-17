# Languages

Every string the engine shows lives in a `language/<tag>.txt` file: global and
room messages, item names and descriptions, the save screen, the port-only
F1 overlay, the F9 prompt and the lab terminal. The repository carries a seed
per language under `games/re1/language/`; `convert-game` packs them and a mod
may add a language or replace single strings.

## Selecting a language

The session's starting language is, in order:

1. `--language <tag>`, e.g. `--language en-GB`;
2. the OS locale (`LC_ALL`, `LC_MESSAGES`, `LANGUAGE`, `LANG`; `fr_FR.UTF-8`
   becomes `fr-FR`, a bare `ja` stays `ja`);
3. `en-US`.

Each key then resolves through the fallback chain, independently:

1. the selected tag's file;
2. the same language in any region (`en-GB` finds `en-US`, a `ja` request finds
   `ja-JP`);
3. `en-US`;
4. a room message no language carries falls back to the RDT's own text;
5. a global or room message no file carries at all reads as its own registry
   id (`MESSAGE_3F`, `ROOM_101_TEXT_03`), so a missing message is visible
   instead of blank.

Item names and UI labels a chain does not carry read as empty, exactly like a
missing entry in the tables this system replaces: an item without a name shows
no name, and an optional save-screen suffix simply disappears.

## File format

One `KEY = "value"` line per string; blank lines and `#` comments (whole-line
or after the value) are ignored. Keys are upper-case ASCII with one of the
`MESSAGE_`/`ROOM_`/`ITEM_`/`UI_` prefixes, and each key appears once.

| syntax | bytes | meaning |
|---|---|---|
| `\n` | `0x02` | line break |
| `\"`, `\\` | - | a literal quote or backslash |
| `\xNN` | `NN` | one raw glyph/operand byte |
| `{clear N}` | `0x03 N` | page break with settle N |
| `{scroll N}` | `0x04 N` | skip/tag block N |
| `{color N}` | `0x05 N` | CLUT tint N |
| `{string N}` | `0x06 N` | item-name substitution N |
| `{branch A B C}` | `0x08 A B C` | yes/no post-action |
| `{s}` | `0xFB` | glyph spacing |
| `{timed N}` | action byte | auto-dismiss after N frames (messages only) |

Any other character is encoded through the game font. Literal `0x01`/`0x07`
are rejected as `\xNN`: a value carries its stream's terminator itself.

Encoding is a direct cell lookup with no reserved glyph bytes: the sheet's
`0x28`/`0x29` cells are the letters L and M, and the round parentheses are the
`0xF8 0x16`/`0xF8 0x17` cells, exactly as the original's message, item-name and
formatted-text renderers index the sheet. Only the original's ASCII-buffer
`PrintText8x14` — the `_Nothing_` row and the passcode panel — remaps its own
40/41 to the parenthesis cells, and that grid is a separate byte space whose
`L` is 76, not 40.

## Keys

| key | meaning |
|---|---|
| `MESSAGE_<nn>` | global message `nn` (`00`..`3F`), including the boot narration |
| `ROOM_<room3>_TEXT_<nn>` | room message shared by the RDT variants |
| `ROOM_<rdt4>_TEXT_<nn>` | per-variant override, e.g. `ROOM_1001_TEXT_0A` |
| `ROOM_<room3>_NAME` | room display name (the F1 overlay) |
| `ROOM_<room3>_NAME_SAVE` | shorthand room name for save data, falling back to `NAME` |
| `ITEM_<nn>_NAME` | item name by item id |
| `ITEM_<nn>_NAME_UNCHECKED` | the generic name shown before the item is examined, only where it differs from `NAME` |
| `ITEM_<nn>_DESCRIPTION` | item description by item id |
| `UI_SAVE_*` | save-screen headers, exits, character names, slots, prompts, errors |
| `UI_LAB_*` | lab terminal labels and the expected `UI_LAB_PASSWORD_*` credentials |
| `UI_PROMPT_RETURN_TITLE_*`, `UI_DEBUG_*` | the F9 prompt and the F1 overlay |

## Mods

A mod pack may ship `language/<tag>.txt` with any subset of keys. Every layer's
copy of a tag merges per key, later layers winning, so a mod can replace three
French strings without restating the file. Whole new tags work too: a mod may
add `language/pl-PL.txt`, and `--language pl-PL` (or a Polish OS locale) picks
it up; the readme of such a mod will usually mention that the sheet's glyphs
limit what it can display.

## Missing glyphs and fonts

Two game-text sheets ship with the releases, and each has its own byte space
(`charset::Script`):

| sheet | size | cells | spells |
|---|---|---|---|
| Japanese `DATA/FONT.TIM` | 768x256 | 14x14, kana/kanji through the `0xF8`/`0xF9`/`0xFA` pages | Japanese and the plain Latin letters |
| Western `Data/fontus.tim` | 256x256 | 8x14, a single page | the accented Latin letters (`ÄÖÜäöüßÀÂÈÉÊÏÎÔÙÛÇç`) and the European punctuation (`; , " ( ) \ - · … – + =`) |

A session draws one script, chosen by the language (`ja` keeps the Japanese
sheet, every other tag prefers the Western one) and resolved against the
sheets the pack carries: `font/font.tim` is the install's own sheet,
`font/latin.tim` and `font/jpn.tim` are the other script's. `convert-game`
walks the root for the sheets, so one pack converted from the Steam release's
install root — whose language trees each hold a complete game — renders every
language with its own sheet. A sheet the root does not carry is a conversion
warning: a Japanese install alone draws Latin languages with the Japanese
sheet's tight metrics, and a lone Steam tree draws Japanese with the Western
sheet. The catalog encodes every language file in the selected script, and
`text::script_for` is the one rule both the encoding and the font loading
follow.

A character the sheet has no cell for is drawn as a small synthetic hollow box
instead of vanishing or switching language. `charset::MISSING` is the reserved
byte pair (`0xFA 0xFF`, an operand no page's table entries reach) the box is
drawn for; the decoded sheet is never modified, so the fonts' own glyphs —
including the controller symbols and the `0x0B` page-wait ▼ — stay intact.

The Japanese sheet has no accented Latin, Cyrillic or the symbols `>`, `=`,
`+`; the Western one has no Cyrillic, Nordic or Iberian letters and no `>`. A
language that needs glyphs its sheets lack must ship one as a mod asset
(`font/font.tim`, or `font/latin.tim`/`font/jpn.tim`), which the pack layering
already supports. Per-font character tables beyond the two shipped sheets are
not implemented yet.

## Latin spacing (per-glyph metrics)

The Japanese sheet's Latin glyphs were drawn full-width: lowercase letters
carry only 7-10px of ink inside their 14px cell and every space is a full cell,
so drawing English through the cells leaves a hole between letters and a wide
gap between words.

The engine measures the sheet's own ink at load time
(`font::GlyphTable::measure`). For every cell it records `indent = min_x` and
`advance = min(14, ink_width + 1)` from the ink's bounding box, and the tight
drawing path blits the cell at `x - indent`. Two authored rules join the
measurement:

- the space byte `0x00` advances 8px;
- the digit bytes `0x0C..=0x15` keep the full 14px, so counters and save
  timers stay monospaced.

The metrics apply only to non-Japanese languages *and* only to the Japanese
sheet: `Catalog::uses_tight_metrics` is true for every tag except `ja`/`ja-*`,
and `Font` ignores the flag for a Western sheet, whose cells the original
advances whole at 8px. `ja-JP` therefore renders exactly as the original, and
so does `en-US`/`fr-FR`/... once the pack carries `fontus.tim`. Raw draws with
no language context keep the full cells too: the fixed-width save-screen slot
templates and reveal string.

`font::Font::draw_text` takes a tight flag, `font::advance`
returns a stream's advance, and `font::wrap_pixels`/`font::fit_pixels` wrap and
clip by advances instead of cell counts. Encoding is unchanged, and the metrics
come from the sheet, so a mod that ships a redrawn narrow `font/font.tim` is
measured automatically.

## Licensing

The seeds combine the user's own install (the `ja-JP` file decodes the
executable's text byte-exactly) with community translation packs. They are
committed so every build ships its strings; their provenance and the credits
question are tracked in `PLAN.md` section 6.
