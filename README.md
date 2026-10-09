# Arklay

A from-scratch engine for the classic Resident Evil games (RE1, RE2, RE3),
written in Rust with SDL3. Each game is a game pack; everything game-specific
lives in the pack. Mods layer on top with any asset overridable.

The engine reads the player's own game installation: `convert-game` migrates a
supported install into an `.akpak` pack, and the engine runs the pack.

## Build

Requires Rust stable and SDL3. If SDL3 is not installed system-wide, set
`SDL3_DIR` to a prefix containing `include/` and `lib/`, or place one in
`../vendor/sdl3` (the workspace default). On Windows, SDL3 is built from source
and linked statically, so no separate DLL is needed.

```sh
cargo build
# A lean build with no Lua runtime (every hook is a no-op)
cargo build --no-default-features
```

Lua 5.4 is vendored and compiled from source by the default-on `lua` feature.

## Usage

```sh
# Convert an RE1 installation into a game pack (parallel across CPUs)
cargo run -- convert-game /path/to/re1 --out re1.akpak

# Limit the conversion to N workers (default: one per available CPU)
cargo run -- convert-game /path/to/re1 --out re1.akpak --jobs 4

# List every pack entry with its size, then the entry count and total bytes
cargo run -- list re1.akpak

# Validate a pack: parse every known-format entry and print a per-format
# report; exits 1 when a known-format entry fails (--strict also fails
# unknown extensions)
cargo run -- verify re1.akpak

# Extract every pack entry below a directory, preserving relative paths
cargo run -- extract re1.akpak --out extracted/

# Launch room 100 as player 0 (RDT 1000)
cargo run -- re1.akpak --room 100 --player 0

# Boot the title screen (or --ui title|select|game|menu|box|file|map|view|load|font)
cargo run -- re1.akpak

# Headless capture (no display)
cargo run -- re1.akpak --room 100 --player 0 --capture cut0.bmp
# Run N fixed 30 Hz ticks before a room capture (deterministic, audio-free),
# so scripted NPC scenes are captured in motion
cargo run -- re1.akpak --room 20D --player 0 --ticks 30 --capture npc.bmp
cargo run -- re1.akpak --ui title --capture title.bmp
cargo run -- re1.akpak --ui select --capture select.bmp
cargo run -- re1.akpak --ui menu --capture menu.bmp
cargo run -- re1.akpak --ui box --capture box.bmp
cargo run -- re1.akpak --ui file --capture file.bmp
cargo run -- re1.akpak --ui map --capture map.bmp
cargo run -- re1.akpak --ui view --capture view.bmp

# Frame-time report for a 600-tick room run: per-tick min/avg/p95/max, the
# load/update/effect/render phase totals and the entity/effect high-water
# marks. Needs no --capture; the budgets live in docs/performance.md
cargo run -- re1.akpak --room 100 --ticks 600 --stats

# Port-only debug room-select overlay: press F1 while playing to freeze the
# room and browse the pack's rooms; up/down move, confirm jumps, F1/Esc closes
cargo run -- re1.akpak --room 100
```

Captures, assembled scripts and converted packs are written to a sibling
temporary file and renamed into place, so a failed run never leaves a partial
artifact behind.

## Mods and authoring

Packs layer. Any entry of a mod pack shadows the base pack's entry, including a
whole-room `scd/{id}.scd` script override and `lua/**` hooks:

```sh
# Build the checked-in demo sources into a pack (no game assets included)
cargo run -- mod build mods/demo --out demo.akpak

# Layer it explicitly; --mod is repeatable, later load_order wins
cargo run -- re1.akpak --mod demo.akpak --room 100 --ticks 40 --capture demo.bmp

# A clean vanilla run: no --mod and no sibling mods/ discovery
cargo run -- re1.akpak --no-mods --room 100

# Every sibling mods/*.akpak applies automatically
cargo run -- re1.akpak --room 100

# Inspect a pack's manifest, layers and merged entries
cargo run -- pack info re1.akpak --mod demo.akpak

# Disassemble/reassemble a room's scripts
cargo run -- scd export ROOM1000.RDT --out 1000.s
cargo run -- scd build 1000.s --out scd/1000.scd
```

`docs/modding.md` documents the manifest grammar, the `.s` grammar, the mod
source convention and the sandboxed Lua hook API; `docs/m15-deviations.md`
records the deliberate limits (whole-room script replacement, sandbox,
error/budget policy). A run without any layer stays byte-identical to a
pre-M15 run, Lua or not.

`convert-game` and `extract` report progress on stderr per phase (RDTs,
camera cuts, BGM, player files, bytes written): one in-place line with
`done/total`, percentage, elapsed time and ETA when stderr is a terminal, or
periodic plain lines without control characters when it is redirected.
Conversions end with per-category totals, the output size and the total
elapsed time. `extract` rejects packs whose entry paths are absolute, contain
`..` or backslashes, or are empty, so it never writes outside `--out`.

### Controls

| Key | Action |
| --- | --- |
| Arrow keys | Move / menu selection |
| `Space`, `Return` | Confirm / action / dismiss a message |
| `Tab` | START: open/close the inventory (pause menu), cycle the top tabs |
| `[`, `]` | L1/R1: page the item box |
| `X`, `Backspace`, `Esc` | Cancel (menus, character select, load screen) |
| `Shift` + `,` | Previous camera cut |
| `Shift` + `.` | Next camera cut |
| `F1` | Port-only debug room-select overlay: freeze the room and browse the pack's rooms; up/down move, confirm jumps, F1/Esc closes |
| `F9` | Return-to-title prompt: a second F9 returns to the title screen, any other key cancels |

The character select slides the two cards between their poses while the pick
changes; the item viewer spins the model while a direction is held and confirm
runs the item's examine check (the rotation-gated descriptions and the red
book's zoom). The map tab needs the radio's scenario flag; up/down step the
rooms of the current floor plan, left/right step between the plans the owned
maps unlock, and confirm shows the full plan of the current area. A pack
without `map/` leaves the tab blank.

Saves live as `savedat*.dat` under `--save-dir` (default `saves/` beside the
pack). The title starts on LOAD GAME when a save exists; with none, LOAD is
refused.

The ignored `tests/m16_ui_real.rs` golden harness compares sampled
room/message/NPC/UI frames against a locally generated set when
`ARKLAY_RE1_GOLDEN` points at it; the set is never committed and is a
regression/cross-check, not an independent oracle. `docs/m16-deviations.md`
lists every approximation M16 leaves.

## Hardening and release

- `docs/architecture.md` maps every module, the fixed 30 Hz frame flow, the
  trust boundaries, the per-format budget caps and the engine invariants.
- `docs/status.md` is the milestone/format/verification matrix;
  `docs/m17-deviations.md` records what the hardening tooling does not prove.
- `docs/performance.md` records the frame-time budgets and the reference
  machine; `ARKLAY_PERF_BUDGET_MS` overrides the per-tick p95 budget in the
  ignored performance test.
- The local CI mirror: `./scripts/check.sh` runs fmt, clippy
  `--all-targets --all-features -D warnings`, the `--no-default-features`
  check, debug and release tests and the doc build; `./scripts/check.sh --full`
  adds the ignored real-asset suite when `ARKLAY_RE1_ROOT` and
  `ARKLAY_RE1_PACK` are set.
- CI gates those commands on Linux and Windows plus a 1.88 MSRV check, and
  uploads `arklay-linux-x86_64.tar.gz` and `arklay-windows-x86_64.zip`
  artifacts (binary, README, LICENSE). A scheduled, non-gating fuzz workflow
  runs every `fuzz/` target for 300 seconds:
  `cd fuzz && cargo +nightly fuzz run <target> -- -max_total_time=300`; the
  stable compile gate `cargo check --manifest-path fuzz/Cargo.toml` runs in
  `scripts/check.sh` and CI.
- A full-game soak (`tests/soak_real.rs`, ignored) walks every RDT with
  transitions and save round-trips under memory and wall-clock ceilings:
  `ARKLAY_RE1_ROOT=... ARKLAY_RE1_PACK=... cargo test --release --test
  soak_real -- --ignored --nocapture`.

## License

MIT. No game assets or converted game data are distributed with this project.
