# Arklay

A from-scratch engine for the classic Resident Evil games (RE1, RE2, RE3),
written in Rust with SDL3. Each game is a game pack; everything game-specific
lives in the pack. Mods layer on top with any asset overridable.

The engine reads the player's own game installation: `convert-game` migrates a
supported install into an `.akpak` pack, and the engine runs the pack. A pack
may also be a directory of unpacked entries (the layout `arklay extract`
writes); directory entries are read on demand, not loaded wholesale.

## Build

Requires Rust stable and SDL3. If SDL3 is not installed system-wide, set
`SDL3_DIR` to a prefix containing `include/` and `lib/`, or place one in
`../vendor/sdl3` (the workspace default). On Windows, SDL3 is built from source
and linked statically, so no separate DLL is needed.

```sh
cargo build
# A lean build without the PNG/WebP/OGG replacement decoders
cargo build --no-default-features
```

Lua 5.4 is vendored and compiled from source on every build.
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
# (english/USA, french/FRA, german/GER, japanese/JPN), takes the shared assets
# from the first tree (English), fills in the Japanese-only document pages from
# the other trees, resolves each release's film names to the canonical entries,
# and packs every game-text sheet it carries, so the one pack renders
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

# Launch room 100 as player 0 (RDT 1000); the pack may also be a directory
cargo run -- re1.akpak --room 100 --player 0
cargo run -- extracted/ --room 100 --player 0

# Pick the displayed language; the default is the OS locale, then en-US
cargo run -- re1.akpak --room 100 --language ja-JP

# Boot a screen directly (--ui title|select|game|demo|menu|box|file|map|view|save|load|
# death|result|epilogue|font|debug)
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
# marks. Needs no --capture; the budgets live in docs/performance.md. Adding
# --frame-rate 120 also times the frames a 120 fps display would draw between
# the ticks against the per-frame budget
cargo run -- re1.akpak --room 100 --ticks 600 --stats

# Present 120 frames per second (the simulation stays at 30 Hz; the scene is
# drawn between ticks). With --stats and no --ticks the room is played in the
# window and the actual presented rate is printed when it closes
cargo run -- re1.akpak --room 100 --frame-rate 120 --stats

# Draw the 3D scene on the GPU (SDL_GPU over Vulkan) at the window's
# resolution, here at 150% of it with 4x MSAA; falls back to software without
# a device. Captures and --stats always use the software renderer
cargo run -- re1.akpak --renderer gpu --render-scale 150 --msaa 4

# Port-only debug tools: press F1 while playing to freeze the room and open
# the tool list (ROOM SELECT, INVENTORY, PLAYER, FLAGS, CHEATS, SAVE, ITEM BOX,
# CLEAR GAME, CONTROLS, DISPLAY). Confirm opens a tool, Esc backs out one
# level, F1 closes from anywhere and reopening returns to the same tool.
# `--ui debug` captures the tool list
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
# Print the HD texture-pack file names for an original file's textures
cargo run -- hash /path/to/JPN/STAGE1/RC1000.pak   # bgd/DD17F52F.webp

# Build the checked-in demo sources into a pack (no game assets included)
cargo run -- mod build mods/demo --out demo.akpak

# Layer it explicitly; --mod is repeatable, later load_order wins. A mod
# source directory can be passed directly, without `mod build` first.
cargo run -- re1.akpak --mod demo.akpak --room 100 --ticks 40 --capture demo.bmp
cargo run -- re1.akpak --mod mods/demo --room 100 --ticks 40 --capture demo.bmp

# The replacement-format example: a PNG background, a PNG/WebP UI page and an
# OGG BGM track over the shipped art
cargo run -- mod build mods/hd --out hd.akpak
cargo run -- re1.akpak --mod hd.akpak --room 100 --ticks 120 --capture hd.bmp

# A clean vanilla run: no --mod and no discovery
cargo run -- re1.akpak --no-mods --room 100

# Every mod in the pack's sibling mods/ and in <config dir>/mods applies
# automatically. Only top-level *.akpak files and mod directories are
# candidates; the per-user directory is %APPDATA%\arklay\mods on Windows and
# <XDG config>/arklay/mods elsewhere. `[mods]` in arklay.toml enables,
# disables and orders them by manifest id (see docs/modding.md).
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
| `Space`, `Return` | South (A / Cross) | Confirm / action / dismiss a message; skips the loading narration and a letterboxed cutscene |
| `C` | Left trigger | Aim the equipped weapon (hold) |
| `Z` | Right trigger | Fire / reload with the weapon aimed |
| `Tab` | Start | START: open/close the inventory (pause menu), cycle the top tabs |
| `[`, `]` | Left / right shoulder | L1/R1: page the item box |
| `X`, `Backspace`, `Esc` | East (B / Circle) | Cancel (menus, character select, load screen) |
| `Shift` | West (X / Square) | Run (hold) |
| `Shift` + `,` | Previous camera cut |
| `Shift` + `.` | Next camera cut |
| `F1` (gamepad Back / Select) | Port-only debug tools: freeze the room and open the tool list (room select, inventory, player, flags, cheats, save/load, item box, clear game, controls, display); Esc backs out one level, F1 closes |
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
kept). A bad line or value in `arklay.toml` (an unknown key, `msaa = 3`, a
binding to a key that does not exist) never stops the game: it is reported
as a warning and that setting keeps its default. The file is not rewritten
while it has such a mistake in it, so a change made in game (a rebinding, F8)
then applies for the run only. Gamepads are picked up when plugged in; extra
controller mappings can be dropped into `gamecontrollerdb.txt` in the config
directory (or `SDL_GAMECONTROLLERCONFIG`).

`[game]` holds gameplay options that stay at the original's behaviour unless
turned on. `quick_turn` enables Classic REbirth's quick turn: while walking
backward (including the back diagonals), pressing RUN turns the player 180
degrees in place over eight ticks, plays the no-weapon walk animation and its
footsteps, and hands control back to normal movement. The player does not
move or collide during the turn, and the new facing is live for rendering,
collision and scripts. `--quick-turn` and `--no-quick-turn` override the file
for one run; scripted `--trace`/`--input` runs force it off unless
`--quick-turn` is given explicitly, so existing traces are unchanged:

```toml
[game]
quick_turn = true # BACKWARD + RUN turns the player 180 degrees
```

`F8` steps the window up to the next whole multiple of 320x240, switches to
borderless fullscreen once the next size would not fit the display, and goes
back to 320x240 after that; `Alt+Enter` toggles borderless fullscreen. The
window cannot be resized by hand, but `window_width` and `window_height` can
be set to any size. The frame (4:3 by default, or the wider aspect set below)
always fits the window or screen, centred with black bars where the shape
differs: a whole zoom stays exactly pixel-sharp, and a fractional one keeps
every pixel the same size, blending only along their edges. The window size,
fullscreen, aspect and the display it is on are saved to the `[graphics]`
table of `arklay.toml`, so the next run opens the same way on the same screen:

```toml
[graphics]
renderer = "gpu"   # "software" (default) or "gpu"; --renderer overrides it
fullscreen = false # borderless fullscreen (Alt+Enter)
window_width = 960 # the window's client size in pixels (F8 sets multiples
window_height = 720 # of 320x240); a window too big for its display shrinks
display = 1        # the display to open on, numbered from 1
aspect = "4:3"     # "16:9" or "16:10": crop the frame to the wider shape
render_scale = 100 # GPU only: the 3D scene's resolution, % of the screen area it fills (25..400)
msaa = 4           # GPU only: anti-aliasing samples per pixel (1, 2, 4 or 8)
frame_rate = 120   # presented frames per second, 30..1000 (default 30)
```

`frame_rate` only changes presentation: the game always simulates at the
original's 30 Hz, so input timing, scripts and traces are identical at every
rate. Above 30 each frame draws the characters, objects, shadows, effect
sprites and room fades between the last two ticks (one tick behind, like any
interpolation); camera cuts, room loads, teleports, door transitions, films
and the 2D screens snap. Any rate from 30 to 1000 is accepted, though vsync
caps it at the display's refresh rate; the title, menus and door transitions,
which tick every 16 ms, present once per tick (about 62 fps) at any rate below
that. The F1 menu's DISPLAY tool steps it through 30,
60, 120, 144, 165 and 240 and saves the choice.

`aspect` (also `--aspect` and the F1 DISPLAY tool) presents a wider frame by
cropping the 320x240 view rather than pillarboxing it. In a room the visible
band follows the player's chest row with a small dead zone; films show the
centred band, while the F1 debug menu keeps the whole 4:3 frame, pillarboxed.
The 2D screens lay out in the band, and the pause menu and its sub-screens
take a full-height 16:9 or 16:10 canvas. Captures, traces, the oracle and the
goldens always run 4:3, so a wider aspect changes presentation only.

The GPU renderer draws its 3D scene at the size the frame fills on screen, so
`render_scale` above 100 supersamples and below 100 trades sharpness for
speed, while `msaa` smooths the polygon edges. The software renderer always
draws the original 320x240 frame and only scales it up.

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
directory sets the save directory, the language, the control bindings and
the graphics options. The save/load screen is a
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
