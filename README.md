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

# The same, with one line per packed file: its plain and stored size
cargo run -- convert-game /path/to/re1 --out re1.akpak --verbose

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

# Pick the displayed language; the default is the OS locale, then en-US
cargo run -- re1.akpak --room 100 --language ja-JP

# Boot the title screen (or --ui title|select|game|menu|box|file|map|view|load|death|font)
cargo run -- re1.akpak

# Play one attract reel directly (the reference's --test-demo oracle anchor):
# --demo N is the Nth entry of the pack's sorted demo/<hex>.dat list; with
# --ticks/--trace it emits the per-tick differential trace
cargo run -- re1.akpak --ui demo --demo 0 --ticks 600 --trace demo0.trace

# `--ui game` with `--room` boots the new game straight into that room through
# the session flow (boot narration, entry fade, camera and idle mirroring), the
# anchor the reference oracle's test-only room boot mirrors
cargo run -- re1.akpak --ui game --room 101 --player 0 --ticks 600 \
  --trace room101.trace

# Load a save block (savedatN.dat) and boot its saved game immediately: the
# file's stage, room, character, position, health, inventory and flags drive
# the session. `--ui game --load` is the oracle anchor; without `--ui` it is
# the interactive continue
cargo run -- re1.akpak --ui game --load savedat1.dat --ticks 600 \
  --trace loaded.trace
cargo run -- re1.akpak --load savedat1.dat

# Authoring/oracle seams: run a Lua chunk once on the freshly booted session
# (before its first tick; it receives the game state as `local api = ...`) and
# serialise the resulting state to a save block, or both
cargo run -- re1.akpak --ui game --room 101 \
  --setup-script oracle/saves/pause_chris_101.lua \
  --save-out out/savedat1.dat

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
# Kill the player and capture the DIED screen (--character 0|1)
cargo run -- re1.akpak --ui death --character 1 --capture died.bmp
# Capture the post-film ending screens
cargo run -- re1.akpak --ui result --capture result.bmp
cargo run -- re1.akpak --ui epilogue --capture epilogue.bmp
# Play an ending end to end: the films, RESULT, epilogue and next-cycle save
cargo run -- re1.akpak --ending 1

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
source convention, the language-file overrides and the sandboxed Lua hook API;
`docs/language.md` documents the `language/<tag>.txt` format, the key
registry, the fallback chain and the missing-glyph box; `docs/m15-deviations.md`
records the deliberate limits (whole-room script replacement, sandbox,
error/budget policy). A run without any layer stays byte-identical to a
pre-M15 run, Lua or not.

`convert-game` and `extract` report progress on stderr as a single bar for the
whole command. `convert-game` runs its asset categories (RDTs, camera cuts,
BGM, player files, compression, bytes written) as internal phases under that
one bar, which names the phase in progress, so a conversion never scrolls a bar
per category. On a terminal it hides the cursor and redraws the bar in place
with a percentage, then erases it when the run finishes (restoring the cursor);
when redirected it emits periodic plain lines without control characters
instead. The warnings are buffered and
printed to stderr after the bar is gone, so they never corrupt it, and a
conversion ends with a single
`Created <pack> (<n> entries, <size> MiB) in <time>` line. With `--verbose`
the conversion additionally prints the stage and category totals, the
compression summary and one line per packed file with its plain
(pre-compression) and stored size on stdout, in processing order; the report
is identical whatever the worker count. Every entry of a converted pack is
zstd-compressed at a fixed level except formats that already carry their own
compression (`avi`, `ogg`, `png`, `webp`), which are stored raw; an entry that
would not shrink also stays raw.
`extract` rejects packs whose entry paths are absolute, contain
`..` or backslashes, or are empty, so it never writes outside `--out`.

### Controls

| Key | Action |
| --- | --- |
| Arrow keys | Move / menu selection |
| `Space`, `Return` | Confirm / action / dismiss a message |
| `C` | Aim the equipped weapon (hold) |
| `Z` | Fire / reload with the weapon aimed |
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
book's zoom). The map tab shows the floor-plan browser with its "Mansion 1F"
caption; up/down step the rooms of the current floor plan, left/right step
between the plans the owned maps unlock, and confirm shows the full plan of
the current area. A pack without `map/` leaves the tab blank.

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
