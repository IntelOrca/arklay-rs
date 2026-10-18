# M19 config and Lua-screen deviations

M19 adds the `arklay.toml` user configuration, moves the default save
directory to `<config dir>/save`, and ships the Classic REbirth-style
save/load screen as the Lua pack entry `screen/save_cr.lua` driven through the
general screen surface in `src/ui/lua_screen.rs`. This document records the
deliberate approximations and the parts of CR's screen that are not
reproduced.

## Closed: the 1-based save location

`SaveStrings::location` passed the block's 0-based stage byte
(`SaveFile::stage`, the original's `g_stageId`) into `Text::room_label`, whose
keys are the 1-based `ROOM_<room3>_NAME_SAVE` names, so every vanilla row for
stage 1 read blank. `Text::save_location` now adds one, and both screens use
it. The Lua screen's `location` field resolves through the same helper.

## Bounded: the screen's layout is measured, not pixel-exact

The CR screen's geometry follows the disassembly notes (title at y 27, ten
15-px rows from y 49, the detail line at y 216, the list centred), but the
list panel's extent and the highlight band's grey and alpha are read from the
notes' description rather than a captured frame of the running DLL, so the
built-in script's panel and band are a close approximation. A mod can shadow
`screen/save_cr.lua` and adjust them; the API is the same.

## Bounded: the save-mode flow

CR's save mode was never captured (the notes record the load mode only), so
the shipped script's save flow is the port's own reading of the disassembly:
confirming a row deletes the old file, writes a new one and leaves the screen.
There is no confirmation dialog, matching CR's overwrite-by-delete. The
typewriter's ink-ribbon consumption and the save counter stay in the engine
and are unchanged by the screen choice.

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

## Not ported: the PNG background override

CR looks for `type.png` before the original TIM. Format-level asset overrides
are a general feature tracked separately (issue #53), so the screen draws
whatever the pack carries under `ui/type00.bmp` (or a mod's replacement) and
falls back to a black frame when the entry is missing.

## Not ported: ModSDK text overrides

CR's ModSDK can replace the screen's text and assets. The port's equivalent is
the language catalog (`UI_SAVE_CR_*` keys, en-US and ja-JP; other languages
fall back per key) plus pack-entry shadowing, so a mod replaces strings
through `language/<tag>.txt` and the script through `screen/save_cr.lua`.
