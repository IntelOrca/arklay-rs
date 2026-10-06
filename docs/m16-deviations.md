# M16 integration deviations

M16 is the fidelity sweep: exact fixed-point presentation, the remaining
scripting/search/room-effect parity and the UI long tail (message order,
character select, item view, map tab). This document records the deliberate
approximations the milestone leaves, the sites it intentionally does not close,
and the provenance of the golden-frame harness. The no-enemy, no-combat scope
of the engine is unchanged and is covered by `docs/m14-deviations.md`; the
authoring limits are in `docs/m15-deviations.md`.

## Approximations the milestone accepts

1. **Random consumer call order.** The platform LCG and the per-frame BioCard
   word are exact, and the screen shake draws the original's three values, but
   the *order* in which consumers draw from the shared stream within a frame is
   the port's own: the frame word is drawn once before entity thinking and the
   shake's three draws happen when the frame is drawn, not at the original's
   exact instruction boundaries. Scripts that roll dice read the correct word;
   scripts that compare two consumers' draws in one frame can differ.

2. **Flat half blend where per-texel STP is not modelled.** A textured packet
   whose CLUT carries ABE blends as `(source + destination) / 2`. The original
   selects the blend per texel from the CLUT entry's STP bit; the port applies
   the documented flat half blend when the palette has STP entries, and keeps
   the per-texel bitmap only as far as the TIM decoder exposes it.

3. **Camera roll is synthetic-only.** `Camera` composes a roll and the integer
   projection is unit-tested, but every shipped RDT camera stores roll 0, so no
   corpus capture exercises the roll path. A synthetic basis difference in the
   fixed-point view build cannot be proven against reference frames without the
   golden set (item 8).

4. **Map tab scope.** The tab browser (floor plan, visited-room tinting, the
   current-room highlight and the dpad page/room steps) is implemented from the
   extracted tables. The full-display zoom is a single swap rather than the
   original's frame-by-frame zoom/light animation, the objective highlight is
   transcribed from the scenario flags rather than extracted as data, and the
   arrow/dot blink sprites are not drawn. A pack without `map/` degrades to a
   blank tab with a warning.

5. **Item-view examine fallback.** When the model's pose is outside the item's
   examine windows the port refuses the confirm (the rotation puzzle stays
   closed). The original instead falls back to the item's plain description
   message; the port has a single description path, so the fallback is the
   refused confirm rather than a second message. The entry light ramp is
   approximated by a black screen fade over the zoom spin, and the port's
   examine records are read in the model's pitch/yaw/roll order (the original's
   extracted x/y/z angles).

6. **Message-window timing within a frame.** The window advances after the
   script/entity pass, exactly like the original's `main_loop`. The consequence
   is visible on the dismissal frame: the room probe has already run before the
   window consumes the press, so the dismissing press can reach an action-gated
   zone in the same frame; the swallow latch keeps a *held* key from firing
   again. Captures of rooms that dismiss a message over an action zone can
   differ by one frame from the pre-M16 order.

7. **World simulation stays deferred.** Zone-walker CW/CCW weighting, SCA
   volume lists, `entity_pathfind_update` and `Add_speedXZ` are unchanged from
   M15; see the remaining-site list below.

8. **Golden harness provenance.** `ARKLAY_RE1_GOLDEN` compares sampled
   room/cut/message/NPC/UI frames against a locally generated set. The goldens
   come from the original game through the project author's own tooling and are
   never committed; the harness is a regression and cross-implementation check,
   **not** an independently produced oracle. Only an unset variable skips.

## `TODO(parity)` sites that intentionally remain

Line numbers are from the M16 tree. The exact-camera and platform-random
slices close the camera/rand sites (`src/render.rs` `Camera::from_points`,
`src/game.rs` `next_random`/`rand_seed`); they are listed here only if a tree
predating those slices is read.

| Site | Group | Why it stays |
|---|---|---|
| `src/game.rs:1931` | gameplay | monster ids below `0x20` allocate no entity; no enemies |
| `src/game.rs:1941` | scripting | the force-init re-init block needs the enemy snapshot store |
| `src/game.rs:1945` | gameplay | occupied-slot re-init is part of the enemy system |
| `src/game.rs:1982` | gameplay | per-character SCA hit records; no hit volumes |
| `src/game.rs:2041` | gameplay | per-joint flag XOR needs the enemy joint renderer |
| `src/game.rs:2075` | gameplay | monster spawn slots stay empty (no enemies) |
| `src/game.rs:2089` | gameplay | the frozen-message NPC update path is enemy/actor work |
| `src/npc/walk.rs:101` | gameplay | `Add_speedXZ` is an un-collided step; world milestone |
| `src/npc/walk.rs:216` | gameplay | zone-graph walker weighting; world milestone |
| `src/npc/walk.rs:910` | gameplay | SCA volume resolution; no hit volumes |
| `src/npc/walk.rs:1009` | gameplay | `entity_pathfind_update`; world milestone |
| `src/npc/scd.rs:140` | gameplay | `EntityUpdateWeaponJoint`; no held-weapon system |
| `src/npc/scd.rs:799` | audio | the flamethrower cues live in the enemy bank |
| `src/npc/idle.rs:113` | gameplay | per-character joint tints/shadow quads/SCA records |
| `src/player.rs:55` | gameplay | the backward-walk clip 2 fallback needs an enemy-visibility test |
| `src/audio.rs:223` | audio | the DirectSound pan curve stays the M13 deviation |
| `src/audio.rs:451` | audio | one-shot-vs-restart semantics stay the M13 deviation |
| `src/ui/title.rs:204` | UI | the title idle timer waits on the attract/demo milestone |
| `src/convert.rs:464` | conversion | held-weapon TMDs (`players/ws*.tmd`) wait on weapons |
| `src/engine.rs:5823` | visual | per-record background blend weight/STP is drawn opaque |

Every other `TODO(parity)` from the M16 inventory is closed by its slice:
the camera/projection/roll, the platform random and BioCard word, the packet
ABE/STP/cull/gouraud/near-plane rules, the latched lighting and background
modulation, the sprite blend/mirror and text/NPC shadows, the animation blends
and joint gates, the item searches and condition semantics, the scripted room
effects and player ops, the message order, the character-select slide, the
item-view examine combos, the map tab, the herb combine refusal, the raw-slot
swap and held-pad blanking, the locked-door cue/key timing, the slow-motion
cadence and the headless transition follow.

## Capture re-baselines

- `--ui select` and `--ui view` captures are unchanged at their settled poses.
- `--ui menu` is unchanged; `--ui map` is new (the pack gains `map/`).
- Room captures change where a message is dismissed over an action zone (item 6)
  or where a scripted door is now followed inside the tick window. The corpus
  audit prints the rooms whose captures transition; none did under the audit's
  idle/action drive, and no room's placeholder set changed.
- The map conversion additions (`map/map00.tim`..`map0e.tim`, `map/blue.tim`,
  `map/tables.bin`) are the only new pack entries; every pre-existing entry is
  byte-identical.
