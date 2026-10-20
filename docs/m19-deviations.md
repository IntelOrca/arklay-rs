# M19 config and Lua-screen deviations

M19 adds the `arklay.toml` user configuration, moves the default save
directory to `<config dir>/save`, and ships the Classic REbirth-style
save/load screen as an ordinary mod (`mods/crloadsave`) driven through the
general screen surface in `src/ui/lua_screen.rs`. The engine keeps only the
registration hook (`on_init` + `api:register_save_screen`) and the ported
original screen; nothing in `src/` or `games/re1` names the CR screen. This
document records the deliberate approximations and the parts of CR's screen
that are not reproduced.

## Closed: the 1-based save location

`SaveStrings::location` passed the block's 0-based stage byte
(`SaveFile::stage`, the original's `g_stageId`) into `Text::room_label`, whose
keys are the 1-based `ROOM_<room3>_NAME_SAVE` names, so every vanilla row for
stage 1 read blank. `Text::save_location` now adds one, and both screens use
it. The Lua screen's `location` field resolves through the same helper.

## Measured: the screen's geometry and colours

The layout was measured against the running CR 1.1.3 captures in
`docs/images/cr-load-screen-*.png` (the reference's game space is the 320x240
frame at 2x), not only the disassembly notes:

| Element | Measurement |
| --- | --- |
| Title | Centred, ink rows 21..33, i.e. drawn at y 19 (the notes' "y ≈ 27" is the ink's centre) |
| Rows | 14-px selection band from y 49 at a 15-px pitch, text one pixel above the band |
| Detail line | Centred at y 215 (ink rows 217..229) |
| List panel | `x 10, y 45, 300x156` — a fixed ten-row panel, not one that grows with the entry count |
| Panel colour | Grey `0x797979` at alpha 76 (fits `out = 0.703*bg + 36` across the capture's pixels) |
| Band colour | Black at alpha 191 (`out = 0.25*panel`) |
| Text | Save rows and the detail line's character name blue (`#3F7DFA` over the sheet's grey ramp); the exit row and the rest of the detail line white |

The port's advances match CR's when the pack carries the Western sheet, and
widen when it does not:

| Pack | Title | Row label | Detail line |
| --- | --- | --- | --- |
| CR 1.1.3 | 30 px | 56 px | 102 px |
| USA/Steam install (`fontus.tim` present) | 30 px | 61 px | 117 px |
| MediaKite JPN install (no Western sheet) | 40 px | 74 px | 125 px |

CR draws its English screen text with the Western font even on the MediaKite
install, which has no `fontus.tim`; a pack converted from that install has
only the 14-px Japanese sheet, so a Latin session falls back to the Japanese
sheet's wider advances (the documented `font::session_entry` fallback, shared
with every other screen). The list contents, row order and labels match
either way.

The Western sheet has no `/` cell, so `charset::latin` now substitutes one
with the sheet's slash-shaped backslash cell (0x38): the USA release's own
save screen spells its field separators that way (`SaveLoadScreen.cpp`'s
`"     \  \"`), and the port's `UI_SAVE_SLOT_FILLED` template — authored on
the Japanese grid — had been drawing missing-glyph boxes on a USA pack.

## Bounded: the save-mode flow

CR's save mode was never captured (the notes record the load mode only), so
the shipped script's save flow is the port's own reading of the disassembly:
save mode always lists a white `Create New` row above the files, and
confirming it writes the first free `save_NN.dat` and leaves the screen.
Confirming an existing row keeps CR's overwrite-by-delete, writing a new file
and leaving. There is no confirmation dialog, matching CR's overwrite-by-delete.
The typewriter's ink-ribbon consumption and the save counter stay in the engine
and are unchanged by the screen choice.

## Bounded: the app's registration pass

The title's LOAD GAME and the load screen run before any session, so the app
loads the pack's scripts once in a throwaway VM and runs `on_init` over a
scratch state to read a registration. A pack's chunk-level code therefore runs
one extra time per process (the app's pass plus each session's own VM); only
`on_init` should do work there, and the scratch state is discarded, so a
registration is the only thing the pass can carry. A registration that names a
missing entry logs a warning and the ported original opens.

## Deviation: save file naming

CR names new saves `<name>_<NN>_<timestamp>.dat`. The shipped script uses the
first free `save_NN.dat` instead: the sandbox has no clock, and the engine's
`api:save(name)` takes the name the script chooses, so a mod that wants
timestamped names can build them itself if a clock API is added later. The
list's `timestamp` field is the file's own modification time either way, so
rows still sort and display by real time.

## Deviation: no legacy-save migration

CR moves `savedat*.dat` from the game root into its `savedata/` folder at
patch time. The port does not move anything: the default save directory
changed to `<config dir>/save`, and an existing `saves/` directory beside a
pack is left alone. Point `save_dir` (or `--save-dir`) at it to keep using
those files, or copy them into the new directory. The block layout is
unchanged, so both directions work.

## Deviation: the PNG background override

CR looks for `type.png` beside the executable before the original TIM. The
port has no such automatic lookup, but format-level asset overrides landed as
a general feature (#53): a layer resolves a replacement by entry stem with the
fixed extension priority `webp > png > bmp > tim > pix`, so a mod can ship
`ui/type00.png` (or `.webp`) and it wins over the pack's `ui/type00.bmp`.
Without a replacement the screen draws the pack's entry and falls back to a
black frame when it is missing.

## Not ported: ModSDK text overrides

CR's ModSDK can replace the screen's text and assets. The port's equivalent is
the language catalog (`UI_SAVE_CR_*` keys, en-US and ja-JP; other languages
fall back per key) plus pack-entry shadowing, so a mod replaces strings
through `language/<tag>.txt` and the script through the mod's own entry
(`script/loadsave.lua`).
