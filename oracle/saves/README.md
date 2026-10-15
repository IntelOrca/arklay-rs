# Oracle save blocks (`savedatN.dat`)

Checked-in test data for the save-mode differential oracle. Each `.dat` is an
original 0x800-byte BioCard block (`src/save.rs` writes exactly the original
layout, no port-only tail), so the same file boots **both** engines into the
same game state:

- arklay: `arklay <pack> --ui game --load <file> [--input ...] [--trace ...]`
- reference: `residentevil --test-load <file> [--test-input ...] [--test-trace ...]`

The files are committed so they stay loadable forever and can be re-checked
against future engine changes. They are generated deterministically from the
authored setup chunk next to each file:

```
scripts/oracle-saves.sh [NAME...]
```

For every save the script boots `--ui game --room <room> --player <p>
--character <c>` on arklay, runs `<name>.lua` as the `--setup-script` chunk and
writes the state with `--save-out`. The table of boot parameters is in
`scripts/oracle-saves.sh`; regenerate after any engine change and commit the
diff so the data stays honest.

## Provenance

All saves were generated from the Japanese MediaKite assets at
`/usr/share/games/re1` (the install the reference's `config.ini` points at,
`[Assets] Version=JPN`) packed by `arklay convert-game`. They are:

| Save | Boot | Authored state |
| --- | --- | --- |
| `pause_chris_101.dat` | room 101, Chris | new-game inventory (Combat Knife + First Aid Spray), player at the room's walk-zone spawn. The pause-menu scenario suite. |
| `file_chris_101.dat` | room 101, Chris | pause anchor + three FILE documents collected (`0x5F..0x61`). |
| `map_chris_101.dat` | room 101, Chris | pause anchor + the first eight map areas owned (room-flags bits `0x7C..`). |
| `radio_chris_101.dat` | room 101, Chris | pause anchor + `HAS_RADIO` (scenario bit `0x7F`) + the communication radio (`0x4D`). |
| `combine_chris_101.dat` | room 101, Chris | green herb (`0x44`), red herb (`0x43`) and a second green herb, for the mix and the refusal. |
| `use_chris_101.dat` | room 101, Chris | health 30/140 + green herb + first-aid spray. |
| `equip_chris_101.dat` | room 101, Chris | Beretta (`0x02`) + handgun ammo (`0x0B`). |
| `itembox_chris_118.dat` | room 118, Chris | player inside the quiet B1 save room's item-box action rect + mixed inventory/box. |
| `typewriter_chris_118.dat` | room 118, Chris | player inside the quiet B1 save room's typewriter action rect + an ink ribbon. |

## Format notes

- The block's stage byte (0x200) is the original's **0-based** `g_stageId`
  (`0` = STAGE1); the room byte (0x201) is the RDT room number.
- The block carries room, position, facing, health, inventory, item box, the
  saved flag banks and the room-BGM table. It cannot carry enemies or live
  effects; a scenario that needs those must author them at boot.
- The reference refuses a Director's Cut (TRAINING/ADVANCED) save in OG mode
  (`DebugQuick_LoadSlot`'s `fileBuffer[0x233] & 3` gate); these saves are all
  standard mode.
- A save is state, not a script: moving the *authored* state means regenerating
  the file with `scripts/oracle-saves.sh`.
