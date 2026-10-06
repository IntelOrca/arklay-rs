# Arklay status

A milestone-by-milestone feature table, the parser/format coverage a fresh
`arklay verify` reports, the verification-spine checklist with what is still
missing, the known `TODO(parity)` sites and their dispositions, and the road to
the enemy milestone. Numbers below were measured on 2026-10-06 with the
workspace install (`/home/ted/openre/assets/re1`) and converted packs.

## Milestones

| Milestone | Scope | State |
|---|---|---|
| M0 First Light | Convert room 1000, decode cuts, display | done |
| M1 Whole-game conversion + music | All rooms/categories into a pack; BGM playback | done |
| M2 Player | Player model, locomotion, collision, camera cuts | done |
| M3 SCD reading | SCD reader, VM skeletons, `scd export` | done |
| M4 SCD semantics | Full command/event opcode semantics | done |
| M5 Doors, masks, footsteps | `.dor` transitions, room masks, footstep zones | done |
| M6 Game flow and UI | Fonts, messages, inventory, menus, save/load | done |
| M9 Scripted characters | NPC models, animation, escort/walk behaviours (no enemies) | done |
| M10 Effects | Billboards, effect pool, sprite sheets, fire handler | done |
| M11 World interactions | Pushables, mirrors, item-box lid, ladders | done |
| M12 World items | Floor items, sparkles, desks, item-view models | done |
| M13 Voice and audio | Dialogue, per-room BGM state machine, 3D SE dispatch | done |
| M14 FMV | AVI/Cinepak, `movie_on` hand-off, opening/ending flow | done |
| M15 Mods and authoring | Manifest, layered packs, text-SCD assembler, Lua hooks, demo pack | done |
| M16 Fidelity sweep | Exact fixed-point presentation, scripting/UI parity, harness leftovers | done |
| M17 Hardening | Budgets, torture, fuzz, `verify`, atomic CLI, `--stats`, soak, CI, docs | this milestone |

Enemies, combat and the attract demo are explicitly out of scope so far; every
deferred item is listed below or in the milestone deviations files.

## Format coverage

`arklay verify <pack>` classifies every entry by path/extension, runs it
through the reader the engine uses, and reports a per-format table. The
converted base pack on the reference install reports:

```
pack: re1.akpak
2541 entries, 408551500 bytes
format       entries        ok    failed          bytes
manifest           1         1         0            101
rdt              348       348         0      107997096
roomcut          842       842         0      175695340
roommask         601       601         0       29133338
bmp               12        12         0        1784964
tim              121       121         0        6335154
ivm               77        77         0        6525012
emd               19        19         0        2733776
emw                2         2         0          28400
dor               34        34         0        2279820
wav              475       475         0       75972806
text               5         5         0           6581
maptables          1         1         0             72
biocard            1         1         0           1052
opaque             2         2         0          57988
verify: 2541 entries, 2541 ok, 0 failed, 2 opaque
```

The companion packs verify clean too: the voice pack (`re1.voice.akpak`, 517
`wav` entries, 101,697,368 bytes) and the movie pack (`re1.movie.akpak`, 27
`avi` entries, 251,624,740 bytes). The two opaque entries are
`data/core00.esp` and `data/core00.etm`, the effect tables whose pair parser is
not in the path classifier; `--strict` turns them into failures by design.

## Verification spine

| Commitment | Where | State |
|---|---|---|
| Golden capture harness | `tests/m16_ui_real.rs` (ignored, local goldens) | present; goldens are regression cross-checks, not an oracle |
| Movie decode goldens | `tests/m14_real.rs` (ignored) | present |
| Deterministic captures/states/saves | M16 tests, `tests/save_real.rs`, soak round-trip | present |
| Save round-trip | `tests/save_real.rs`, `tests/soak_real.rs`, synthetic M16 slice-10 test | present |
| Stable torture matrix (>=10k mutated inputs) | `tests/torture.rs` (parallel budget slice) | scheduled for this milestone |
| cargo-fuzz targets and seeds | `fuzz/` package (parallel fuzz slice) | scheduled for this milestone; `fuzz.yml` is gated on `fuzz/Cargo.toml` |
| Scheduled fuzz workflow | `.github/workflows/fuzz.yml` | present, nightly, non-gating |
| Decode budgets and memory ceilings | `src/budget.rs` (parallel budget slice), `tests/memory.rs` | scheduled for this milestone |
| `arklay verify` | `src/verify.rs`, `tests/cli.rs` | present |
| CLI error paths and atomic outputs | `src/atomic.rs`, `tests/cli.rs` | present |
| `--stats` frame-time report | `src/stats.rs`, `tests/cli.rs`, `tests/perf_real.rs` | present |
| Full-game soak | `tests/soak_real.rs` (ignored) | present |
| CI matrix (Linux/Windows/release/MSRV/artifact) | `.github/workflows/ci.yml` | present |
| Local gate | `scripts/check.sh` | present |
| `docs/architecture.md`, `docs/status.md`, `docs/m17-deviations.md` | this docs pass | present |

Still missing by design:

- The fuzz package's seeds and 17 targets are their own slice; until it lands,
  the workflow job skips and the always-run torture matrix (parallel slice) is
  the fuzz gate.
- The real-asset tests, goldens and budgets need a local install and are never
  run in CI with game data; they are ignored tests behind
  `ARKLAY_RE1_ROOT`/`ARKLAY_RE1_PACK`.
- Performance budgets are machine-specific policy; the numbers and the
  reference machine are recorded in `docs/performance.md`.

## `TODO(parity)` dispositions

20 sites (`rg -n "TODO\(parity\)" src`); M17 changes none of them:

| Group | Count | Sites | Disposition |
|---|---|---|---|
| Gameplay | 14 | enemy spawn/re-init `game.rs:1992/2002/2006/2043/2102/2136/2149`; walkers/world `npc/walk.rs:101/216/910/1009`; weapon joint `npc/scd.rs:140`; idle init `npc/idle.rs:113`; backward clip `player.rs:55` | enemy/weapon work or the actor/world milestone |
| Audio | 3 | pan law `audio.rs:223`; one-shot restart `audio.rs:451`; flamethrower cues `npc/scd.rs:799` | the first two are the documented M13 deviations; the third needs the enemy bank |
| UI | 1 | title idle timer `ui/title.rs:204` | blocked on the attract demo, which replays inputs over monster rooms |
| Visual | 1 | per-record background blend weight/STP `engine.rs:5899` | the record's blend source is not decoded |
| Conversion | 1 | held-weapon TMDs `convert.rs:464` | needs the weapon system |

## Road to enemies

The next milestone pays these prerequisites in order (from the M17 plan):

1. **Actor/world layer first.** Fix the walker/idle/backward-clip parity sites
   behind an actor differential harness (position/pose/state hashes against
   the original's replay tooling) before combat data rides on them.
2. **Enemy content and data extraction.** Monster EMD/EMW models and clips,
   enemy and weapon sound banks (including the flamethrower cues), held-weapon
   TMDs and the damage/health/state tables, using the M16 table-extraction
   pattern.
3. **Systems.** Spawn/re-init, enemy AI states, SCA hit volumes and reactions,
   weapons/ammo, the damage path, the death screen and film, then the attract
   demo and the title idle timer.
4. **Verification.** Per-enemy golden captures and the actor differential on
   top of M17's budgeted, fuzzed, verify-covered readers; the new enemy data
   enters the same `verify` table, torture seeds and soak budget.

No RE2/RE3 runtime, format, pack or migration work is planned until the RE1
enemy milestone is complete: there is no RE2 corpus in the workspace, and the
canonical re-base cannot be validated without the full enemy roster. The M17
harnesses already key off the manifest's `rdt_version`/`scd_version` and
per-entry path prefixes, so a second dialect adds parser rows rather than
rewriting them.
