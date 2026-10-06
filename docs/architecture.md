# Arklay architecture

Arklay is a from-scratch engine for the classic Resident Evil games. A game
installation is migrated into an `.akpak` pack by `convert-game`; the runtime
opens the pack, loads one room and drives it at a fixed 30 Hz. Everything
game-specific lives in the pack, so the engine has no hard-coded game data.

This document is the map: the modules and their responsibilities, the frame
data flow, the trust boundaries, the format inventory with each reader's
budget, the engine invariants and the test taxonomy. `docs/status.md` records
what is implemented and what the project knows it still approximates;
`docs/m17-deviations.md` records what the hardening milestone does not prove.

## Module map

Every module of `src/lib.rs`, with the subsystem that owns it and its
responsibility.

### Container and tooling

| Module | Owner | Responsibility |
|---|---|---|
| `pack` | Container | `.akpak` v1 reader/writer, layered mod packs, entry lookup, atomic `PackWriter::write` |
| `manifest` | Container | `manifest.toml` grammar (`format`, `kind`, `base`, `load_order`, `rdt_version`, `scd_version`, Lua hooks) |
| `modding` | Tooling | Sibling `mods/` discovery and the `mod build` source-directory builder |
| `convert` | Tooling | `convert-game`: discover an install and migrate every asset category into packs (main, voice, movie) |
| `progress` | Tooling | Terminal-aware progress lines and phase totals for the long-running commands |
| `atomic` | Tooling | Sibling-temp-then-rename single-file output used by every artifact writer |
| `verify` | Tooling | `arklay verify`: path-classified parse of every pack entry with a per-format report |
| `stats` | Tooling | Fixed-bucket frame-time histogram and the `--stats` report |

### Runtime

| Module | Owner | Responsibility |
|---|---|---|
| `engine` | Runtime | Entry points, room loading, the fixed 30 Hz game loop, transitions, headless simulate/capture seams, SDL window/renderer |
| `game` | Runtime | `GameState`, script hosts, entities, items, flags, room actions, effects tick, save integration |
| `state` | Runtime | `RoomId`, `RoomState` (cuts, zones, collisions, lights), `Image` |
| `player` | Runtime | Player locomotion, input mapping, collision, reach probes, camera cuts |
| `rdt` | Format | RDT room parser (header pointers, cameras, zones, collisions, embedded model/TIM pairs) |
| `scd` | Runtime/Format | SCD opcode tables, reader, IR, disassembler/decompiler, assembler, command and event VMs |
| `door` | Format/Runtime | `.dor` container parser and the door-animation VM |
| `transition` | Runtime | The door-transition timeline (screen space, masks, camera, `.dor` stepper hand-off) |
| `stairs` | Runtime | `stairs_height_update` and `set_stairs_zone` room actions |
| `objects` | Runtime | RDT-embedded object TMD/TIM pairs, runtime object records and transforms |
| `npc` | Runtime | Scripted characters: model driver, clips, walking, idle, script commands |
| `effects` | Runtime | The 64-slot billboard effect pool, per-room sprite tables, packed texture pages, behaviours |
| `items` | Runtime | Item tables: names, classes, pickup rules, stacking |
| `message` | Runtime | The seven-state message window and its byte grammar |
| `save` | Runtime | The 0x800-byte BioCard block, slot I/O, `Apply_to`/`from_state` round-trip |
| `audio` | Runtime | WAV parsing and the single SDL3 software mixer used for SE, voice, BGM and film audio |
| `bgm` | Runtime | The three-channel music state machine, ramps and channel hand-off |
| `sfx` | Runtime | Per-room sound-name tables and footstep resolution |
| `voice` | Runtime | Per-stage voice-line tables and pack path naming |
| `music` | Runtime | Room BGM tables and track path naming |
| `movie` | Runtime | The film table, `movie_on` hand-off and the playback session |
| `ending` | Runtime | Ending-row selection and its film chain |
| `lua` | Runtime | Sandboxed Lua 5.4 hooks, behind the default-on `lua` feature |

### Presentation and UI

| Module | Owner | Responsibility |
|---|---|---|
| `render` | Presentation | Deterministic software rasterizer: framebuffer, models, masks, shadows, effect billboards, lighting |
| `model` | Presentation | Shared decoded model types (`Texture8`, mesh/skeleton vocabulary) |
| `anim` | Presentation | Fixed-point skeletal animation and clip playback |
| `tmd` | Format | PSX TMD mesh parser |
| `tim` | Format | PSX TIM decoder: 16bpp direct, 8bpp/4bpp indexed with CLUT |
| `emd` | Format | EMD character/room model containers and EMW weapon animations |
| `ivm` | Format | Item-view model (texture page + TMD) parser |
| `avi` | Format | RIFF/AVI demuxer for the shipped Cinepak films |
| `cinepak` | Format | Cinepak (`cvid`) frame decoder |
| `bmp` | Format | BMP encode/decode, including the black-key room-mask decode |
| `lzw` | Format | RE1 `.pak` LZW decoder |
| `shadow` | Presentation | The player's baked ground shadow: decode and placement |
| `font` | Presentation | Glyph decoding, metrics and tinted drawing |
| `text` | Presentation | Executable text tables: messages, names, descriptions, save strings |
| `mask` | Presentation | Room mask (foreground overlay) sprites and depth ordering |
| `ui` | UI | `Screen` contract, title, character select, pause menu, item box, FILE, map, item viewer, save/load |

## The fixed 30 Hz tick and the frame data flow

The original runs at 30 Hz and so does the port: `TICK_MS = 1000/30` and the
interactive loop accumulates real time and runs whole ticks, latching one
input edge per tick. Rendering happens once per accumulated frame; a tick that
opens a door hands control to the transition player until it finishes.

One gameplay tick (`engine::tick_room`) runs this fixed order:

1. Publish the remapped D-pad held/pressed words so `ck_bits` sees this
   frame's pad.
2. Advance the special-room-light state and clear the per-frame item-use
   flags.
3. Desk/item-box flow and deferred key consumption.
4. Command VM `run_main` over the current block, then the event VM step.
5. Apply scripted room edits (collision boxes, `obj_xfm` lights).
6. NPC/entity update (`game.tick_entities`).
7. Mirror scripts' player-entity writes onto the visible player, then
   `player::update_with_room` (movement, collision, animation).
8. Effect pool update (`game.tick_effects`).
9. Room action probe and object update (probes read the frame's final state).
10. Camera cut switch and mask/shadow bookkeeping.

After the tick comes `render_frame`: the 320x240 `Framebuffer` is cleared with
the room's fade/ambient state, the cut background and masks are blitted, the
object/item/effect billboards are depth-sorted and drawn, the player and NPC
models are transformed and rasterised, and the shadow is composited. The
interactive path presents the framebuffer through SDL; the `--capture` path
runs the same renderer under the offscreen/software driver and writes a BMP
through an atomic sibling-temp rename; `--stats` performs the render and
prints timings without opening a display.

Headless seams (`engine::simulate_room*`, `simulate_door`,
`simulate_typewriter`, `render_game_frame`) boot the same state, run the same
tick and render the same frames as the interactive loop, with audio stubbed
and film requests drained. The real-asset tests and the full-game soak are
built entirely on those seams.

## Trust boundaries

Arklay's input class determines what the code may assume:

- **Pack and mod input is untrusted.** A pack can come from a third party; the
  reader validates the TOC with checked arithmetic, rejects unsafe paths
  (absolute, `..`, backslashes, NUL, empty, duplicates) and every parser is
  expected to reject arbitrary bytes with `Err`, never a panic, abort,
  unbounded allocation or unbounded loop. `arklay verify` classifies by path
  and runs every entry through its format's reader; the torture matrix and the
  cargo-fuzz targets hammer the same entry points. Budgets live in
  `src/budget.rs`: `MAX_PACK_ENTRIES`, `MAX_ENTRY_BYTES`,
  `MAX_DECODE_ALLOC`, `MAX_PIXELS`, `MAX_LZW_OUTPUT`, `MAX_RECORDS`, plus
  per-format caps; an over-cap input reports the format, the limit and the
  observed value.
- **The game installation is semi-trusted.** `convert-game` may assume the
  shipped file layout, but a missing or surprising file is a reported warning
  or a category error, never a panic; conversion skips what it does not know.
- **Save blocks are semi-trusted.** The block is fixed 0x800 bytes; a short or
  malformed slot is refused, and a save never writes outside its save
  directory.
- **Lua is sandboxed.** `lua/**.lua` runs in one 16 MiB-limited VM with the
  documented hook API and no filesystem, package or `os` access; a failing
  hook logs and disables itself rather than failing the session. With
  `--no-default-features` every hook compiles to a no-op.

## Format inventory

Each row names the reader, the policy cap applied at the trust boundary and
where the format is exercised. Caps are policy numbers chosen from the shipped
corpus with headroom and recorded in `docs/performance.md`; they are not
claims about the original.

| Format | Entry point | Cap | Tests |
|---|---|---|---|
| `.akpak` pack | `pack::Pack::from_bytes` | `MAX_PACK_ENTRIES` (1M), `MAX_ENTRY_BYTES` (1 GiB) | `pack` unit tests, `tests/torture`, `fuzz/pack` |
| `manifest.toml` | `manifest::Manifest::parse` | line/key/value length caps | `manifest` unit tests, `tests/torture`, `fuzz/manifest` |
| RDT room | `rdt::parse` | section counts bounded by remaining bytes; `MAX_RECORDS` | `rdt` unit tests, `tests/m16_game_real.rs`, `tests/soak_real.rs`, `tests/torture`, `fuzz/rdt` |
| SCD scripts | `scd::reader::parse` | instructions/events per block bounded by block size | `scd` unit tests, `fuzz/scd` |
| SCD assembler | `scd::asm::assemble` | input size, line count, `MAX_LZW_OUTPUT`-class output cap | `scd` unit tests, `tests/cli.rs`, `fuzz/scd_asm` |
| `.dor` door | `door::parse` | bytecode bounded by input; `MAX_RECORDS` | `door` unit tests, `tests/torture`, `tests/doors_scan.rs` |
| TIM | `tim::decode`, `decode_8bpp`, `decode_4bpp` | `MAX_PIXELS` (4096x4096), CLUT entry cap, `budget::alloc` | `tim` unit tests, `tests/torture`, `fuzz/tim` |
| TMD | `tmd::parse` | objects/vertices/normals/primitives, `try_reserve_exact` | `tmd` unit tests, `fuzz/tmd` |
| EMD/EMW | `emd::parse`, `parse_emw`, `parse_room_anim` | skeletons/keyframes/clips | `emd` unit tests, `fuzz/emd` |
| IVM | `ivm::parse` | objects/vertices/texture caps | `ivm` unit tests, `fuzz/ivm` |
| AVI | `avi::Avi::parse` | streams/chunks/frames bounded by the file size | `avi` unit tests, `fuzz/avi`, `tests/m14_real.rs` |
| Cinepak | `cinepak::Decoder::decode` | canvas `MAX_PIXELS`, 32 strips | `cinepak` unit tests, `fuzz/cinepak`, `tests/m14_real.rs` |
| LZW | `lzw::decode` | `MAX_LZW_OUTPUT` (64 MiB), reset-before-full guard | `lzw` unit tests, `fuzz/lzw` |
| BMP | `bmp::decode`, `decode_mask` | dimensions bounded by input | `bmp` unit tests, `tests/torture`, `fuzz/bmp` |
| Save block | `save::SaveFile::from_bytes` | fixed 0x800 layout | `save` unit tests, `tests/save_real.rs`, `tests/soak_real.rs`, `fuzz/save` |
| Mask table | `mask::parse` | sprite/group counts | `mask` unit tests, `tests/mask_render_real.rs`, `fuzz/mask` |
| WAV | `audio::parse_wav` | sample count bounded by the `data` chunk | `audio` unit tests, `fuzz/wav`, `tests/m13_real.rs` |
| Lua source | `lua::LuaVm::load` | 16 MiB VM, source-size cap | `lua` unit tests, `fuzz/lua` |
| Map tables | `ui::map::MapTables::parse` | fixed blob size/magic | `ui::map` unit tests, `tests/m16_ui_real.rs` |
| BioCard prefix | `data/bio_card.dat` | 0x200 required | `save` unit tests, `arklay verify` |
| `roomcut`/`roommask` BMP | `bmp::decode`, `decode_mask` | BMP caps | `arklay verify`, `tests/soak_real.rs` |

`arklay verify` maps entry paths to these readers, requires every RDT camera's
`roomcut/{room}_{camera:03}.bmp` to be present, and counts unknown entries as
*opaque*; `--strict` upgrades an unknown extension to a failure. The
status matrix in `docs/status.md` records the counts a fresh verify reports for
the shipped packs.

## Engine invariants

These hold across every milestone and are the regression gate:

- **Deterministic frames.** For a fixed pack, room, tick count and input the
  rendered frame is byte-identical across runs and platforms; the RNG streams,
  animation phases and camera state are all driven by the fixed tick, never by
  wall time.
- **Index-stable entity slots.** Entity slot 0 is the player; scripted
  characters occupy their scripted slots and are never compacted. No entity id
  `0x00`-`0x1F` allocates (the no-enemy constraint).
- **A fixed save layout.** `SaveFile` is the original 0x800-byte block field
  for field; capture, serialization, parsing and apply are round-trip stable
  (`tests/save_real.rs`, the soak).
- **No behaviour change without a golden update.** A change that alters a
  deterministic capture, state hash, save byte or pack byte for a valid input
  is a bug until a milestone explicitly re-baselines its golden set.
- **Untrusted input fails cleanly.** Every parser returns `Err` for arbitrary
  bytes; the torture matrix, the memory caps and the fuzz targets are the
  proof.

## Test taxonomy and commands

- Unit tests inside `src`: fast, synthetic. `cargo test --lib`.
- Always-run integration tests (`tests/*.rs`, unignored subset): synthetic
  packs, CLI spawning, torture matrix, stats plumbing, the synthetic
  transition + save round-trip. `cargo test`.
- Release-profile green run: `cargo test --release`.
- No-Lua build: `cargo check --no-default-features`.
- Documents: `cargo doc --no-deps`.
- Real-asset suite (ignored; needs both env vars):
  `ARKLAY_RE1_ROOT=/path/to/re1 ARKLAY_RE1_PACK=/path/re1.akpak cargo test -- --ignored`.
  The corpus audit, the soak, the memory passes, the performance budgets and
  the capture/movie/UI goldens live here.
- Full-game soak (release):
  `cargo test --release --test soak_real -- --ignored --nocapture`.
- Performance budgets (release):
  `cargo test --release --test perf_real -- --ignored --nocapture`;
  `ARKLAY_PERF_BUDGET_MS=<ms>` overrides the per-tick p95 budget.
- Fuzzing (nightly, separate package):
  `cd fuzz && cargo +nightly fuzz run <target> -- -max_total_time=60`.
  The stable compile gate is `cargo check --manifest-path fuzz/Cargo.toml`.
- The mirror of the CI gate: `./scripts/check.sh` (add `--full` to include the
  ignored suite when the asset variables are set).

CI runs fmt, clippy `--all-targets --all-features -D warnings`, debug and
release tests, the `--no-default-features` check, the doc build and the Linux
artifact smoke on Ubuntu; tests and the release artifact on Windows; and a
`cargo check --all-features` MSRV job on 1.88.0. A scheduled, non-gating fuzz
workflow runs each target for 60 seconds.
