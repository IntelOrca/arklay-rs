# Oracle trace v1

The shared, engine-neutral per-tick trace both the C++ reference oracle and
arklay emit. One gameplay tick = one line. All values are integers; everything
is decimal (angles included). Lines end with `\n`.

## Header

The first line names the engine:

```
oracle-trace v1 engine=ref
```

or `engine=ark`.

## Tick lines

```
t=<u32> room=<stage>:<room> rng=<u32> rseed=<u16> fade=<i16>:<i8> p=<...> cam=<u8> e=<...> e=<...> f=<u32>
```

Fields, in this exact order, separated by single spaces:

- `t` — tick index. Starts at 0 on the first traced tick; increments by one
  per gameplay tick.
- `room` — `stage:room`. `stage` is the 1-based RDT stage digit (arklay's
  `RoomId.stage`; the reference's `g_stageId + 1`), `room` is the decimal room
  byte (e.g. `1:6` for ROOM106; the same room's RDT file name is
  `ROOM1060.RDT`).
- `rng` — u32 state of the platform `rand()` stream (`g_randState` in the
  reference, `GameState::rand_state` in arklay).
- `rseed` — u16 value of `g_RandSeed` / `GameState::rand_seed` (the per-frame
  `(short)rand()` draw).
- `fade` — `<counter>:<state>`: the room fade accumulator (`g_fading_counter`
  / the port's fade accumulator, i16) and fade state (`g_fading_state`
  / the port's fade state, i16; the reference's is the short `BioCard.fadingState`,
  values `0x7FFF`/`0` while active, negative when inactive). While the state
  is negative the counter is only a stale leftover, so the comparator ignores
  it in that case and compares the state alone. While the state is negative the fade is
  inactive and the comparator ignores the counter (the reference keeps the
  last counter value in the global, the port resets it to zero).
- `p` — the player:
  `x,y,z,angle,beh,as,hp` where x/y/z are i32 room coordinates, angle is the
  12-bit yaw (`value & 0x0FFF`; the reference stores a sign-extended word, so
  `0xFC00` and `0x0C00` are the same yaw), `beh` is `action_behavior` (u8),
  `as` is `action_state` (u8), `hp` is i16 health.
- `cam` — u8 current camera cut index (`g_roomCameraId` / the port's cut).
- `e` — zero or more entity tokens, one per **active** entity, in ascending
  slot order, each:
  `slot,id,x,y,z,angle,state,beh,as,hp,hit,status,ticks`
  where `slot` (u8), `id` (u8), x/y/z i32, angle is the 12-bit yaw
  (`& 0x0FFF`), `state` u8 (the reference's `state` byte / the port's state low
  byte), `beh` `action_behavior`, `as` `action_state`, `hp` i16, `hit`
  `hit_state` u8 — except slot 0, which emits the player's
  `is_being_attacked` flag: the reference reads one byte at 0x8A named
  `isBeingAttackedFlag` on the player struct and `hit_state` on every other
  entity, and the port keeps the player's flag in its own field — `status`
  `status_flags` u8, `ticks` `action_ticks_counter` u16.
- `f` — u32 FNV-1a hash of the rendered frame (see below).

### Entity position and tick sources

- Every slot emits the entity's live matrix translation
  (`scaMatrixData.localMatrix.t`) in the reference, the field comparable to
  arklay's `Entity::pos`. The `position` field is not a per-tick mirror: it is
  only synced by the player init and some walk handlers, so a grabbed/bitten
  player and a scripted character NPC (the intro's Wesker and Jill walk in the
  matrix) keep a stale value there. Both `p=` and `e=0` emit the matrix for
  slot 0.
- `ticks` is the reference's `g_playerEntity` / `Entity` +0xC4
  (`attackDirection` / `action_ticks_counter`). arklay emits its
  `entities[0].action_ticks_counter`, which the engine keeps in step with the
  reference's uses of that byte: the idle behavior's 100-tick settle countdown
  (`player_behavior_00_idle`), the scripted behavior-1 frame hold and the
  behavior-3 stop counter. The player's other internal counters (`slow_counter`,
  the run-stop field, the climb side) do not appear in this column.

### Slot mapping

- Slot 0 is always the player. The reference emits it from `g_playerEntity`;
  arklay emits `game.entities[0]`.
- Slot `i >= 1` is `game.entities[i]` in arklay, and `g_EnemiesList[i-1]` in
  the reference.
- A slot is emitted only when the entity is active (`status_flags & 0x01`).
  The player is always emitted.

### Frame hash

FNV-1a, 32-bit, offset basis `0x811c9dc5`, prime `0x01000193`:

```
h = 0x811c9dc5
for b in (w as u32 LE bytes, h as u32 LE bytes, then every RGBA byte,
          row-major, top row first, left to right):
    h ^= b
    h = h.wrapping_mul(0x01000193)
```

`w`/`h` are the rendered frame dimensions (320x240 for both engines). The
reference hashes the captured back buffer after normalising it to top-down
RGBA; arklay hashes its `Framebuffer.rgba` directly. The hash is a fast
signal; the pixel comparator is authoritative.

### Capture files

Every `capture_every` traced ticks both engines also write a full frame:

- reference: `<capture_dir>/f%05d.rgba` — `RE1CAP <w> <h>\n` followed by
  top-down RGBA bytes (the Linux hook's format).
- arklay: `<capture_dir>/f%05d.bmp` — the existing BMP writer.

Numbering is by `t` (the traced tick index).

## Trace origin

Tick 0 is the first gameplay tick after the new-game room (106) is live and
the room transition has completed. Both engines normalise the platform PRNG
to `re1_srand(1)` before the room runs. Any constant tick offset between the
two traces is a hook bug, not a comparator feature.

### Room anchor

A room-specific scenario boots the same new-game flow directly into another
room instead of 106:

- arklay: `--ui game --room <3hex> --player <n> --ticks N --trace ark.trace`
  (the session flow: boot narration, room-entry fade, camera and idle
  mirroring).
- reference: `--test-newgame <0|1> --test-room <3hex> --test-trace
  ref.trace --test-exit-after N --test-nolimit` (test-only hooks that apply
  the room override before `init_room` and spawn the player at the room's
  first walk zone centre).

Both place the player at that centre with the new game's health and run the
room-entry fade; tick 0 is the first gameplay tick of the requested room.
The reference window must be 320x240 (`config.ini [Display] Width/Height`) so
the capture frames and hashes match arklay's logical size.
