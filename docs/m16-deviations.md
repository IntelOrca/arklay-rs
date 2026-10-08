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

2. **ABE blends only STP-flagged texels.** A textured packet whose CLUT carries
   ABE blends as `(source + destination) / 2`, but only at texels whose palette
   entry carries the STP bit. A palette row with no STP entries has no blend
   texels at all, so the packet draws fully opaque; the TIM decoder exposes the
   full per-entry STP bitmap for this.

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

9. **Look-at target modes.** The yaw slew transcribes the original's routine
   (the `yawStep * 2` fold, the zero-target decay, the out-of-cone snap reject
   and the two cone clamps). The look-at control byte's absolute-angle mode
   (`0x20`, target fields become the literal yaw/pitch), the one-shot
   aim-then-freeze mode (`0x40`) and the live-target reload (`0x80`, which
   re-reads the target in world space with the `-0xA28` head-height bias) are
   not modelled: the port's `target` is the interface's own refreshed position
   and there is no per-entity `scd_pos` state. The pitch target is derived from
   the entity's position rather than the tracking joint's world translation and
   uses the port's under-two-steps snap rule instead of the original's
   revert-past-the-cone pitch branches, and a zero horizontal distance yields
   pitch 0 rather than the original's ceiling/floor value.

## `TODO(parity)` sites that intentionally remain

Line numbers are from the M16 tree. The exact-camera and platform-random
slices close the camera/rand sites (`src/render.rs` `Camera::from_points`,
`src/game.rs` `next_random`/`rand_seed`); they are listed here only if a tree
predating those slices is read.

| Site | Group | Why it stays |
|---|---|---|
| `src/game.rs` `on_flags` `0x05` | scripting | an out-of-range flag bank/selector/mode records a placeholder; the original's indexed write is undefined there |
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
ABE/STP/cull/gouraud/near-plane rules, the per-vertex latched lighting and
background modulation, the `room_sprite_hide` (0x49) sprite-mask pass, the
sprite blend/mirror and text/NPC shadows, the within-clip animation blends
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
  audit prints the rooms whose captures transition; none do under its
  idle/action drive, so a companion real-door test asserts the shipped
  transition-follow path, and no room's placeholder set changed.
- The root pack was reconverted after the review fixes. The map conversion
  additions (`map/map00.tim`..`map0e.tim`, `map/blue.tim`, `map/tables.bin`) and
  the pack `manifest.toml` are the only new entries; an entry-by-entry
  extraction diff of the old and new packs is byte-identical everywhere else.
  The `--ui map`/`--ui view` ignored tests pass against the new pack.
- The M16 review fixes re-baseline the lit captures: entity lighting now
  evaluates each packet normal against the entity's latched three lights
  (ambient `value * 255 / 4096`), the room `0x49` sprite-hide op applies, the
  special-room-light overlay blends its saturated mask at `state >> 7`, the STP
  fallback is opaque and the random stream starts at the CRT default 1 and never
  reseeds. The synthetic unit expectations were re-baselined; the golden-frame
  harness would need its lit frames regenerated before comparing.
