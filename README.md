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
# A lean build with no Lua runtime (every hook is a no-op) and no PNG/WebP/OGG
# replacement decoders
cargo build --no-default-features
```

Lua 5.4 is vendored and compiled from source by the default-on `lua` feature.
The `hd` feature (also default) pulls the pure-Rust `png`, `image-webp` and
`lewton` crates that decode a mod's replacement art and audio; without it the
engine still resolves replacements by stem but refuses to decode those formats.

## Usage

```sh
# Convert an RE1 installation into a game pack (parallel across CPUs)
cargo run -- convert-game /path/to/re1 --out re1.akpak

# The same, with one line per packed file: its plain and stored size
cargo run -- convert-game /path/to/re1 --out re1.akpak --verbose

# Limit the conversion to N workers (default: one per available CPU)
cargo run -- convert-game /path/to/re1 --out re1.akpak --jobs 4

# Convert the Steam release's install root: the walk finds the language trees
# (english/USA, french/FRA, german/GER, japanese/JPN), converts the first one
# (English) and packs every game-text sheet it carries, so the one pack renders
# English/French/German with the original 8px Latin glyphs and `ja-JP` with the
# Japanese sheet
cargo run -- convert-game /path/to/steam/4249100_Biohazard --out re1.akpak

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

# Boot the title screen (or --ui title|select|game|menu|box|file|map|view|load|death|font|debug)
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

# Draw the 3D scene on the GPU (SDL_GPU over Vulkan) at 4x the 320x240
# internal resolution; falls back to software without a device. Captures and
# --stats always use the software renderer
cargo run -- re1.akpak --renderer gpu --render-scale 4

# Port-only debug tools: press F1 while playing to freeze the room and open
# the tool list (ROOM SELECT, INVENTORY, PLAYER, FLAGS, CHEATS, SAVE, ITEM BOX,
# CLEAR GAME, CONTROLS). Confirm opens a tool, Esc backs out one level, F1 closes from
# anywhere. `--ui debug` captures the tool list
cargo run -- re1.akpak --room 100
cargo run -- re1.akpak --ui debug --capture debug.bmp
```

Captures, assembled scripts and converted packs are written to a sibling
temporary file and renamed into place, so a failed run never leaves a partial
artifact behind.

## Mods and authoring

Packs layer. Any entry of a mod pack shadows the base pack's entry, including a
whole-room `scd/{id}.scd` script override and `lua/**` hooks. Art and audio may
be shipped in the format they are authored in — WebP or PNG images and OGG
Vorbis audio — and the engine resolves them by entry stem with a fixed
extension priority (`webp > png > bmp > tim > pix`, `ogg > wav`):

```sh
# Build the checked-in demo sources into a pack (no game assets included)
cargo run -- mod build mods/demo --out demo.akpak

# Layer it explicitly; --mod is repeatable, later load_order wins
cargo run -- re1.akpak --mod demo.akpak --room 100 --ticks 40 --capture demo.bmp

# The replacement-format example: a PNG background, a PNG/WebP UI page and an
# OGG BGM track over the shipped art
cargo run -- mod build mods/hd --out hd.akpak
cargo run -- re1.akpak --mod hd.akpak --room 100 --ticks 120 --capture hd.bmp

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

`docs/modding.md` documents the manifest grammar, the replacement formats and
their override table, the `.s` grammar, the mod source convention, the
language-file overrides and the sandboxed Lua hook API; `docs/language.md`
documents the `language/<tag>.txt` format, the key registry, the fallback chain
and the missing-glyph box; `docs/m15-deviations.md` records the deliberate
limits (whole-room script replacement, sandbox, error/budget policy). A run
without any layer stays byte-identical to a pre-M15 run, Lua or not.

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

| Key | Gamepad | Action |
| --- | --- | --- |
| Arrow keys | D-pad, left stick | Move / menu selection |
| `Space`, `Return` | South (A / Cross) | Confirm / action / dismiss a message |
| `C` | Left trigger | Aim the equipped weapon (hold) |
| `Z` | Right trigger | Fire / reload with the weapon aimed |
| `Tab` | Start | START: open/close the inventory (pause menu), cycle the top tabs |
| `[`, `]` | Left / right shoulder | L1/R1: page the item box |
| `X`, `Backspace`, `Esc` | East (B / Circle) | Cancel (menus, character select, load screen) |
| `Shift` | West (X / Square) | Run (hold) |
| `Shift` + `,` | Previous camera cut |
| `Shift` + `.` | Next camera cut |
| `F1` (gamepad Back / Select) | Port-only debug tools: freeze the room and open the tool list (room select, inventory, player, flags, cheats, save/load, item box, clear game, controls); Esc backs out one level, F1 closes |
| `F9` | Return-to-title prompt: a second F9 returns to the title screen, any other key cancels |

Every action except the fixed debug inputs (`F1` and the gamepad's Back
button, `F9`) can be rebound, with any number of keyboard, mouse (buttons and
wheel) and gamepad bindings each. The CONTROLS tool in the `F1` menu lists the
actions: up/down pick one, left/right pick a binding slot (the last slot adds
a new one), confirm captures the next key, button, wheel notch or
stick/trigger push (it gives up after about five seconds), START (inventory)
clears the slot and RESET restores the defaults. A binding shared with another
action is drawn red and the other actions are listed below. Up, down, left,
right, confirm, cancel and START must each keep a keyboard key bound to none
of the others, so the menus can always be driven from the keyboard; a change
that breaks this is refused. Changes are saved to the `[bindings]` table of
`arklay.toml`, which can also be edited by hand; only the actions that differ
from the defaults are written, and actions it leaves out keep their defaults:

```toml
[bindings]
confirm = ["key:Space", "key:Return", "pad:a", "mouse:left"]
fire = ["key:Z", "axis:righttrigger+", "wheel:down"]
```

Sources are `key:<SDL key name>`, `mouse:left|middle|right|x1|x2`,
`wheel:up|down|left|right`, `pad:<SDL gamepad button>` and
`axis:<SDL gamepad axis>+|-`; an input SDL has no name for is written by
number (`key:#N`, `mouse:N`, `pad:#N`, `axis:#N+`). The actions are `up`, `down`, `left`, `right`,
`confirm`, `cancel`, `run`, `aim`, `fire`, `start`, `page_left` and
`page_right`. Saving rewrites the file in canonical form (comments are not
kept) and never touches a file that fails to parse. Gamepads are picked up
when plugged in; extra controller mappings can be dropped into
`gamecontrollerdb.txt` in the config directory (or `SDL_GAMECONTROLLERCONFIG`).
Any bound or unbound key, mouse button, wheel notch or gamepad input counts as
"any key" for prompts that take one. `Shift` + `,`/`.` stay the fixed
camera-cut keys, so binding `,` or `.` to an action also steps the camera when
`Shift` is held. Scripted `--input` runs are unaffected by bindings.

The character select slides the two cards between their poses while the pick
changes; the item viewer spins the model while a direction is held and confirm
runs the item's examine check (the rotation-gated descriptions and the red
book's zoom). The map tab shows the floor-plan browser with its "Mansion 1F"
caption; up/down step the rooms of the current floor plan, left/right step
between the plans the owned maps unlock, and confirm shows the full plan of
the current area. A pack without `map/` leaves the tab blank.

Saves live as `savedat*.dat` under `--save-dir` (default `<config
dir>/save`, `~/.config/arklay/save`). The title starts on LOAD GAME when a
save exists; with none, LOAD is refused. `arklay.toml` in the same config
directory sets the save directory, the language and the control bindings. The save/load screen is a
mod: the checked-in `mods/crloadsave/` replaces the ported original with a
Classic REbirth-style folder screen through `api:register_save_screen`, built
and layered like any other mod:

```sh
cargo run -- mod build mods/crloadsave --out crloadsave.akpak
cargo run -- re1.akpak --mod crloadsave.akpak
```

The ignored `tests/real/m16_ui_real.rs` golden harness compares sampled
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
  check, the debug tests, the `test-release` tests (release code without thin
  LTO) and the doc build; `./scripts/check.sh --full` adds every ignored test
  (the real-asset suite and the asset-gated unit tests) when
  `ARKLAY_RE1_ROOT` and `ARKLAY_RE1_PACK` are set.
- The real-asset tests are one binary, `tests/real/main.rs`; a module name
  stays a filter, so `cargo test --profile test-release --test real
  stairs_real:: -- --ignored` runs one module's tests and `... --test real --
  --ignored` runs them all.
- CI gates those commands on Linux and Windows plus a 1.88 MSRV check, and
  uploads `arklay-linux-x86_64.tar.gz` and `arklay-windows-x86_64.zip`
  artifacts (binary, README, LICENSE). A scheduled, non-gating fuzz workflow
  runs every `fuzz/` target for 300 seconds:
  `cd fuzz && cargo +nightly fuzz run <target> -- -max_total_time=300`; the
  stable compile gate `cargo check --manifest-path fuzz/Cargo.toml` runs in
  `scripts/check.sh` and CI.
- A full-game soak (`tests/soak_real.rs`, ignored) walks every RDT with
  transitions and save round-trips under memory and wall-clock ceilings. It
  takes half an hour or more, so it is release-only and opt-in and no routine
  pass runs it:
  `ARKLAY_SOAK=1 cargo test --profile test-release --test soak_real --
  --ignored --nocapture`, or `./scripts/check.sh --soak` for the whole gate.

## License

MIT. No game assets or converted game data are distributed with this project.
