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

# Launch room 100 as player 0 (RDT 1000)
cargo run -- re1.akpak --room 100 --player 0

# Headless capture (no display)
cargo run -- re1.akpak --room 100 --player 0 --capture cut0.bmp
```

### Controls

| Key | Action |
| --- | --- |
| `Shift` + `,` | Previous camera cut |
| `Shift` + `.` | Next camera cut |
| `Esc` | Quit |

## License

MIT. No game assets or converted game data are distributed with this project.
