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
```

## Usage (M0)

```sh
# Convert an RE1 installation into a game pack
cargo run -- convert-game /path/to/re1 --out re1.akpak

# List every pack entry with its size, then the entry count and total bytes
cargo run -- list re1.akpak

# Extract every pack entry below a directory, preserving relative paths
cargo run -- extract re1.akpak --out extracted/

# Launch room 100 as player 0 (RDT 1000)
cargo run -- re1.akpak --room 100 --player 0

# Boot the title screen (or --ui title|select|game|menu|box|file|load|font)
cargo run -- re1.akpak

# Headless capture (no display)
cargo run -- re1.akpak --room 100 --player 0 --capture cut0.bmp
cargo run -- re1.akpak --ui title --capture title.bmp
cargo run -- re1.akpak --ui menu --capture menu.bmp
cargo run -- re1.akpak --ui box --capture box.bmp
cargo run -- re1.akpak --ui file --capture file.bmp
```

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
| `X`, `Backspace` | Cancel (menus, character select, load screen) |
| `Shift` + `,` | Previous camera cut |
| `Shift` + `.` | Next camera cut |
| `Esc` | Quit |

Saves live as `savedat*.dat` under `--save-dir` (default `saves/` beside the
pack). The title starts on LOAD GAME when a save exists; with none, LOAD is
refused.

## License

MIT. No game assets or converted game data are distributed with this project.
