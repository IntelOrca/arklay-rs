# M18 audio-law and background-blend deviations

M18 slices 3 and 4 close the two M13 audio deviations (the pan law and the
per-bank one-shot restart) and the M16 visual TODO at `engine.rs`: the
per-record background blend weight. This document records the capture
re-baselines and the deliberately bounded remainders.

## Closed: DirectSound pan law

`audio::pan_gains` now applies the original's DirectSound split instead of the
constant-power curve: for a normalized pan `p` in `-1..=1` the left gain is
`(1 - p) / 2` and the right `(1 + p) / 2`, so a centered cue plays at half
level per channel and a hard pan leaves the near channel at full scale. Every
path that panned before uses it unchanged: `sfx::sound_gain_pan` for 3D SE,
`sfx::pan_from_raw` for the BGM channel and the voice line, and the unkeyed
one-shot path. The offline test
`audio::tests::mixer_pan_follows_the_3d_curves_at_the_documented_angles` locks
the curve at the `sfx` tests' straight-ahead, 90-degree and far angles.

## Closed: per-bank one-shot restart

A one-shot started on a bank (`Mixer::play_sfx_on_bank` /
`MixState::play_sfx_on_bank`) restarts that bank's voice (buffer and mix
parameters replaced, seek to sample 0) instead of appending, matching the
original's one DirectSound buffer per sound record. The engine keys every
gameplay cue by its original bank: `(bank, id)` for `se_play_3d`, `(2, column)`
for footsteps and entity sounds, `(3, id)` for character cues and `(0, slot)`
for the room-SFX pair. Distinct banks still append through the 16-voice pool,
and the UI/test path (`Mixer::play_sfx`) intentionally keeps the append
behaviour.

## Closed: per-record background blend weight

A mesh record's TMD now carries the original's `+0x68` background weight.
`render::record_blend_weight` transcribes `GetTmdBlendMode`: the first
semi-transparent primitive's ABR field selects `{0x80, 0x00, 0x80, 0xD0}`, and
the blent texel mixes as `source * (256 - weight) + background * weight`.
`ObjectRecord::blend_override` carries `cmd_omodel_set`'s `DAT_004d2be0`
override, armed for the west-wing study (stage 2F and its return) slot 1, the
water tank's surface.

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

## Capture re-baselines

- **ROOM20A0/20A1 (and the revisit ROOM70A0/70A1).** The init script builds
  omodel slot 1, a fully semi-transparent water-tank surface, and
  `cmd_omodel_set` arms `0x30` for it. Its blended texels now mix at 81%
  model / 18.75% background instead of 50/50. The ignored test
  `objects_real::room_20a0_water_tank_blend_weight_rebaselines_its_capture`
  re-renders every cut with and without the override; cut 1 changes 2501
  pixels, and the record shows `blend_override == Some(0x30)`.
- **ROOM20B0/20B1 and ROOM70B0/70B1.** Their omodel 0 is a two-primitive
  record whose first ABR is 1 (`B + F`), so it now draws opaque instead of
  half-blended. No committed capture test draws these rooms; the corpus audit
  still covers them without a placeholder change.
- All other ABE records in the shipped corpus use ABR 0 or 2, whose `0x80`
  weight produces the identical `(source + destination) / 2` pixel the M16
  path already drew, so their captures do not move.

## Verification

Targeted commands used (root and pack from the local install):

```
cargo test --lib
ARKLAY_RE1_ROOT=... ARKLAY_RE1_PACK=... \
  cargo test --test objects_real room_20a0_water_tank_blend_weight_rebaselines_its_capture -- --ignored --nocapture
```

The audio tests are offline (no device): the pan samples and the
restart-vs-append behaviour run under `cargo test --lib audio::`.
