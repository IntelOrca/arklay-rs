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

Run a scenario against both engines with `scripts/oracle-ui.sh`.

## Scenarios

The pause menu takes ~12 frames to open after `start` (the fade). Scripts
below press `start` at tick 60 and begin navigating once the panel has settled
(~tick 80), leaving room for the fade.

- `inventory_open.txt` — open the pause menu and settle.
- `inventory_nav.txt` — open, then walk the item grid cursor.
- `inventory_examine.txt` — open, then confirm the selected item (examine/use
  submenu).
