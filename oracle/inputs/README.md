# Input playback scripts (`--input` / `--test-input`)

One entry per line: `<tick> <button>...`. `#` starts a comment; blank lines are
ignored. An entry replaces the held button set from its tick until the next
entry, so a line with no buttons releases everything and a one-tick tap is a
press line followed by a release line:

```
40 start      # START pressed from traced tick 40
41            # released on tick 41
```

`tick` is the oracle trace tick: `0` is the first gameplay tick after the room
entry, exactly the reference's `t`. Scripts should not place input on tick 0
(the reference cannot inject on its arm frame); the first usable tick is 1.

## Buttons

| name | reference raw pad bit | arklay key |
| --- | --- | --- |
| `up` `down` `left` `right` | d-pad `0x1000`/`0x4000`/`0x8000`/`0x2000` | arrow keys |
| `confirm` (`action`, `cross`) | Cross `0x0080` | Space/Return |
| `cancel` (`circle`) | Circle `0x0040` | X/Backspace/Esc |
| `run` | Circle `0x0040` | Shift |
| `aim` (`square`) | Square `0x0008` | C |
| `fire` | R1 | Z |
| `start` (`inventory`) | START `0x0800` | Tab |
| `page_left` (`l1`) | L1 | `[` |
| `page_right` (`r1`) | R1 | `]` |

Run a scenario against both engines with `scripts/oracle-ui.sh`:

```
scripts/oracle-ui.sh ROOM PLAYER INPUT [TICKS] [OUTDIR] [CAPTURE_EVERY]
scripts/oracle-ui.sh --save SAVEDAT INPUT [TICKS] [OUTDIR] [CAPTURE_EVERY]
```

Room mode boots both engines into a new game in `ROOM`; save mode boots both
engines from the same checked-in block in `oracle/saves/` (see its README),
which is how scenarios that need documents, keys, the item box or a typewriter
reach their screens.

## Scenarios

The pause menu takes ~12 frames to open after `start` (the fade). The
scenarios below press `start` at tick 60 and begin navigating once the panel
has settled (~tick 90), leaving room for the fade.

Room mode / `pause_chris_101.dat`:

- `inventory_open.txt` — open the pause menu and settle.
- `inventory_nav.txt` — open, then walk the item grid cursor.
- `inventory_examine.txt` — open, then confirm the selected item (examine/use
  submenu).
- `inventory_open_close.txt` — open the pause menu, then cancel it.

Room mode (from the new-game spawn):

- `walk_forward.txt` — ROOM 101: hold `up` out of the idle, hand over to the
  run with `run` held and release; the pad-driven locomotion's rendered pose
  must step with the same clip frames as the reference.

Save mode (each header names its save):

- `file_open.txt`, `file_pages.txt` — `file_chris_101.dat`: FILE tab selector
  and the document reader, including the page turns and the cancel path.
- `map_open.txt` — `map_chris_101.dat`: MAP tab, owned areas and room stepping.
- `radio_use.txt` — `radio_chris_101.dat`: RADIO tab use (item-use byte 0x4D).
- `combine_ok.txt`, `combine_refuse.txt` — `combine_chris_101.dat`: the MOVE
  combine flow for green+red and for the two-herbs refusal.
- `use_herb.txt` — `use_chris_101.dat`: the USE action, the heal and the EKG
  flush.
- `equip_weapon.txt` — `equip_chris_101.dat`: equip and unequip the Beretta
  through the action submenu.
- `itembox_open.txt`, `itembox_page.txt` — `itembox_chris_101.dat`: the item
  box world action, its overlay, the L1/R1 pages and a swap.
- `save_screen.txt` — `typewriter_chris_101.dat`: the typewriter prompt and
  the save screen (cancelled before any write).
