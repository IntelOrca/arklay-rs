# M18 audio-law and background-blend deviations

M18 slices 3 and 4 close the two M13 audio deviations (the pan law and the
per-bank one-shot restart) and the M16 visual TODO at `engine.rs`: the
per-record background blend weight. This document records the capture
re-baselines and the deliberately bounded remainders.

## Closed: DirectSound pan law

`audio::pan_gains` now applies DirectSound's `SetPan` law instead of the
constant-power curve: for a normalized pan `p` in `-1..=1` the near channel
stays at full scale and the far channel is attenuated by `|p| * 10000`
hundredths of a decibel, i.e. amplitude `10^(-5|p|)`. A centered cue therefore
plays at full level in both channels, and a hard pan drops the far channel to
-100 dB. Every path that panned before uses it unchanged: `sfx::sound_gain_pan`
for 3D SE, `sfx::pan_from_raw` for the BGM channel and the voice line, and the
unkeyed one-shot path. The offline tests
`audio::tests::mixer_pan_is_the_directsound_decibel_law`,
`audio::tests::mixer_pan_follows_the_3d_curves_at_the_documented_angles` and the
mixer's per-test channel expectations lock the curve.

## Closed: per-bank one-shot restart

A one-shot started on a bank (`Mixer::play_sfx_on_bank` /
`MixState::play_sfx_on_bank`) restarts that bank's voice (buffer and mix
parameters replaced, seek to sample 0) instead of appending, matching the
original's one DirectSound buffer per sound record. The engine keys every
gameplay cue by its original bank: `(bank, id)` for `se_play_3d`, `(2, column)`
for footsteps and entity sounds, `(3, id)` for character cues and `(0, slot)`
for the room-SFX pair. Distinct banks still append through the 16-voice pool,
and the UI/test path (`Mixer::play_sfx`) intentionally keeps the append
behaviour. The engine-level test
`engine::tests::same_bank_entity_footsteps_restart_the_engine_voice` locks the
bank keying through the real `play_entity_sounds` path.

## Closed: per-record background blend weight

A mesh record's TMD now carries the original's remapped `+0x68` background
weight. `render::record_blend_weight` transcribes `GetTmdBlendMode` and the
`CreateTmdObjectInternal` remap that follows it:

- no semi-transparent primitive leaves the record on the M16 per-packet half
  blend (`None`);
- `GetTmdBlendMode`'s result is the record's `blend_override` (`DAT_004d2be0`)
  when one is armed, otherwise the first semi-transparent primitive's ABR
  field's raw table value `{0x80, 0x00, 0x80, 0xD0}`;
- the creator keeps only that value's zero/non-zero distinction: `0x00` draws
  opaque and every other value collapses to the `0x80` half blend, except the
  `cmd_omodel_set` `0x30` override, which becomes the `0x33` (0.2) background
  weight.

`ObjectRecord::blend_override` carries the override, armed for the west-wing
study (stage 2F and its return) slot 1, the water tank's surface.

Residual scope:

- **Object and item records only.** Player and NPC meshes keep the M16
  per-packet half blend (`EntityMesh::blend_weight = None`); the actor
  captures are budgeted in the parallel actor slice. No shipped RDT room
  object or item whose first ABR is a non-half value is drawn by an existing
  capture test except the tank below.
- **Per-texel selection stays.** The M16 rule (a blend texel is one whose
  palette entry carries STP) is unchanged; the record weight scales the mix
  instead of switching the whole record to one alpha. This keeps the
  documented M16 approximation for palettes that mix STP and opaque entries.
- **No cut-level background colour exists to port.** The PC oracle has no
  per-camera colour field: camera record `+0x24` is zero in all 318 shipped
  RDTs, and `setSomeColor` (the background quad's global colour) is only
  called by the logos and ending screens. `draw_background(image, origin,
  colour)` therefore continues to receive white in gameplay, and the
  "background blend" this milestone closes is the record weight above.

## Documented remainder: the LOS flag write-back

`room_check_sight_blocked` masks each tested collision record's flags down to
the two blocking bits and writes the result back, discarding the low byte's
floor/step fine value for the rest of the room's life. The port reads the
shared `RoomState` immutably from the pathfind driver and drops that
destructive side effect (the note on `enemy::walk::room_check_sight_blocked`
carries the same caveat). This is a real divergence for a room whose collision
resolve reads a floor/step record after a sight check; no port test currently
orders those two operations over a step record, and the state-driven captures
are unaffected.

## Capture re-baselines

- **ROOM20A0/20A1 (and the revisit ROOM70A0/70A1).** The init script builds
  omodel slot 1, a fully semi-transparent water-tank surface, and
  `cmd_omodel_set` arms `0x30` for it. Its blended texels now mix at 80% model
  / 20% background (the `0x33` weight, 51/256) instead of 81% model / 18.75%
  background. The ignored test
  `objects_real::room_20a0_water_tank_blend_weight_rebaselines_its_capture`
  re-renders every cut with and without the override and locks the re-baseline:
  cut 1 changes 837 pixels, and the record shows `blend_override ==
  Some(0x30)`.
- **ROOM20B0/20B1 and ROOM70B0/70B1.** Their omodel 0 is a two-primitive
  record whose first ABR is 1 (`B + F`), so it now draws opaque instead of
  half-blended. No committed capture test draws these rooms; the corpus audit
  still covers them without a placeholder change.
- All other ABE records in the shipped corpus use ABR 0 or 2, whose `0x80`
  weight produces the identical `(source + destination) / 2` pixel the M16
  path already drew, so their captures do not move.

## Documented remainder: the death-screen variants

E11b lands the full death flow; these variants stay deferred with their
reasons:

- **The Plant 42 DIED-screen pose.** The original's screen special-cases a
  player killed by the monster plant (enemy id `0x08` with its death-animation
  flag): joints 0 and 2 are hidden, only the torso draws, and a fixed
  rotation/offset is applied. The port does not model the plant's five-state
  kill animation (`player_anim_dispatch_4c2ac8`) or its flag yet, so the
  standard corpse draws in that case. The head one-shot still applies.
- **The attract-demo and countdown death branches.** The machine wires the
  `MSF2_ATTRACT_DEMO`/`MSF2_DEATH_VARIANT` skips and the room dispatch
  exactly, but the port has no attract-demo replay and no self-destruct
  countdown timer, so those branches are never taken.
- **The sound fade.** `BuildSndFadeTbl`'s volume ramp is not modelled: the
  screen stops the room BGM through the mixer when it opens instead of fading
  it out over the delay. Gameplay audio ends abruptly rather than ramping.
- **The blood-pool extents.** The pool follows the port's existing ground-quad
  half extents (500x700) with the original's -100 shrink and +16/frame growth;
  the original's quad starts at 512x640, so the absolute pool size differs by
  the same pre-existing shadow-baseline difference.
- **The dead full-strip quad.** The original's screen carries a second
  255x64 strip template whose draw is gated on a byte that is only ever set
  negative, so it never paints; the port reproduces the per-column strip and
  omits the unreachable draw.
- **The head one-shot's lifetime.** The original's grab/death flag is a
  process-global whose non-zero image value hides the head on the process's
  first death only. The port keeps it per session (each new game or load
  starts set), so every session's first death hides the head; the in-session
  behaviour is identical.

## Verification

Targeted commands used (root and pack from the local install):

```
cargo test --lib
ARKLAY_RE1_ROOT=... ARKLAY_RE1_PACK=... \
  cargo test --test objects_real room_20a0_water_tank_blend_weight_rebaselines_its_capture -- --ignored --nocapture
ARKLAY_RE1_ROOT=... ARKLAY_RE1_PACK=... \
  cargo test --test walk_real real_corpus_zone_paths_never_panic -- --ignored
```

The audio tests are offline (no device): the pan samples and the
restart-vs-append behaviour run under `cargo test --lib audio::`.
