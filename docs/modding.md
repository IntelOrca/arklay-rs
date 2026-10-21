# Modding and authoring

An `.akpak` pack is a flat store of named entries; a mod is a second pack whose
entries layer over a base pack. Nothing about the engine is special-cased for a
particular mod: every loader reads through the layered `Pack`, so any entry
(room RDT, background, `scd/{id}.scd` script override, BGM, UI art, Lua source,
...) can be shadowed by a later layer.

## The pack manifest

Every pack may carry a `manifest.toml` entry. `arklay convert-game` writes one
for the base pack; a mod source must have one. The grammar is a small TOML
subset: comments, `key = value` with strings, integers, booleans and string
arrays, and a single `[pack]` section.

```toml
[pack]
format = 1
id = "demo"
name = "Demo mod"
version = "0.1.0"
kind = "mod"
base = "re1"
load_order = 10
rdt = "re1"
scd = "re1"
language = "ja-JP"
lua = ["lua/demo.lua"]
```

| Key | Meaning |
| --- | --- |
| `format` | Manifest format; `1` today. Required. |
| `id` | Stable pack id. Required. |
| `name`, `version` | Human-facing labels. Optional. |
| `kind` | `"base"` or `"mod"`. Required. |
| `base` | A mod's base pack id. Required for mods. |
| `load_order` | Layer order; lower applies first. Default 0. |
| `rdt`, `scd` | Dialect tags; default `"re1"`. |
| `language` | The language of the pack's own room text (`ja-JP` for the Japanese install). Optional. |
| `engine` | Optional note about the target engine build. |
| `lua` | Ordered hook paths; empty means discover `lua/**.lua`. |

A base pack without a manifest is tolerated as `id = <file stem>`,
`kind = "base"` with a warning; a mod without one is rejected by name.

## Layers at runtime

A game pack — the positional argument, `--mod`, and every pack command — may
be either an `.akpak` image or a directory of unpacked entries. A directory
pack maps every regular file below it to an entry named by its
directory-relative path (the layout `arklay extract` writes) and reads entries
on demand, so it does not load the whole game into memory. A directory mod must
hold `manifest.toml`; `arklay mod build` is optional when running directly.

```sh
# Explicit layers, repeatable; later load_order wins a shared entry. A mod may
# be an .akpak or a source directory.
arklay re1.akpak --mod mods/balance.akpak --mod mods/demo --room 100

# Discovery: every top-level *.akpak or mod directory in the pack's sibling
# mods/ and in the per-user <config dir>/mods directory is applied. On Windows
# that is %APPDATA%\arklay\mods; ARKLAY_CONFIG_DIR relocates both.
arklay re1.akpak --room 100

# A clean vanilla run: no discovery, no --mod
arklay re1.akpak --no-mods --room 100
```

`<config dir>/mods` holds per-user mods next to the config file. Only its
top-level entries are candidates: an `.akpak` file or a directory containing
`manifest.toml`. A nested directory is not scanned, so a mod's own asset tree
is never mistaken for another mod.

### Configuring discovered mods

`arklay.toml`'s `[mods]` table selects and orders discovered mods by their
manifest `id`:

```toml
[mods]
# When present, only these discovered ids apply (an empty list applies none).
enabled = ["hd", "demo"]
# Always excluded from discovery.
disabled = ["balance"]
# Application order: these ids apply first in this order, then the rest by
# (manifest load_order, id).
order = ["demo", "hd"]
```

`disabled` wins over `enabled`. An explicit `--mod` layer always applies, even
when the config excludes its id, and prints a warning; `--no-mods` ignores the
explicit layers and both discovery directories. A missing `[mods]` table keeps
the default: every discovered mod applies in `(load_order, id)` order. An
`enabled` id that was not discovered is reported as a warning.

Mods sort by the configured `order`, then by `(load_order, id)`; within one
layer the existing ASCII case-insensitive lookup wins. Duplicate mod ids and
duplicate layer paths are rejected, and a mod may only name a `base` matching
the base pack's id. With no layers at all the engine takes exactly the
single-pack path, so a run without `--mod` (and with no discovered mods) is
byte-identical to earlier releases.

`arklay list`, `arklay extract` and `arklay pack info` accept `--mod` too;
`pack info` prints the manifest, the applied layers and the merged entry table.
Their `--mod` paths accept directories, but they do not auto-discover the
sibling or per-user directories.

## Replacement formats

Mods are authored in WebP/PNG and OGG Vorbis while the pack stores the art and
audio the original shipped. Every loader therefore looks an asset up by *stem*
— the entry path without its extension — and the pack resolves the formats:

| class | priority, highest first |
| --- | --- |
| image | `webp` > `png` > `bmp` > `tim` > `pix` |
| audio | `ogg` > `wav` |
| movie | `mp4` > `avi` |

**Any entry in a mod whose stem matches a known asset is an override.** The
winning layer is still decided by `(load_order, id)` exactly as above, and only
then does the extension priority pick within that layer: `roomcut/100_000.png`
replaces the background `roomcut/100_000.bmp`, `ui/itemboxn.webp` replaces the
page `ui/itemboxn.tim`, `bgm/013.ogg` replaces `bgm/013.wav`, and a layer that
ships both `x.png` and `x.webp` gets the WebP. A higher layer's PNG beats a
lower layer's WebP, because the override is decided per layer first.

PNG, WebP and OGG Vorbis are decoded by the pure-Rust `png`, `image-webp` and
`lewton` crates behind the default-on `hd` feature (the same switch `cargo
build --no-default-features` turns off, keeping that build's dependency list to
`zstd`). A build without `hd` still resolves replacements by stem but refuses
to decode those formats, naming the feature. BMP, TIM and WAV stay the port's
own decoders with the same size budgets as the shipped formats. `mod build`
packs replacement sources verbatim and stores them raw, and `convert-game`
needs no change: it reads the install's own BMP/TIM/WAV/PIX sources, which
never carry a replacement format.

A film replacement is either an MP4 or an AVI holding H.264 video (any
profile OpenH264 decodes, e.g. High) with optional AAC-LC audio, or a Cinepak
AVI like the shipped films. The decoder is picked from the container and the
codec, so an H.264 AVI can sit at the shipped `movie/<name>.avi` path or an MP4
next to it at `movie/<name>.mp4`; there is no hash lookup. The film plays at
its own frame rate on the 30 Hz clock and its audio is resampled to the
mixer's 22050 Hz. The GPU renderer scales the film's own frames to the window
(letterboxed to their aspect); the software renderer box-filters them onto the
320x240 frame. Colour follows the stream's signalled matrix (BT.709 or
BT.601, limited or full range) and assumes BT.601 limited range when there is
none. The audio track is optional; an MP4 edit list's encoder priming is
skipped. H.264 is decoded by Cisco's OpenH264 (built from source by the
`openh264` crate) and AAC by `symphonia`, both behind the `hd` feature. A film
that does not open logs a warning and the shipped film it shadows plays
instead; a damaged frame later on holds the previous picture.

Jill's prologue (`movie/pj`) removes 177.8 s to 188.5 s of the film, so
playback jumps to 188.5 s. Decoding resumes from the keyframe at or before that
point and catches up from there, so a keyframe exactly at 188.5 s avoids a
brief freeze at the jump (for example ffmpeg's `-force_key_frames 188.5`, or
the closest frame boundary). Keep GOPs closed (x264's default) so the restart
is exact.

### What each class accepts

| class | entry stems (one example) | formats | size |
| --- | --- | --- | --- |
| room background | `roomcut/100_000` | webp, png, bmp, tim, pix | any size: stretched over the 320x240 frame |
| camera mask | `roommask/100_000` | webp, png, bmp, tim, pix | the shipped page's; black is a hole |
| full-frame UI backdrop | `ui/title`, `ui/sel_back`, `ui/type00`, `ui/chris02`, `ui/jill02`, `ui/clis01`, `ui/jill01`, `ui/rc1121` | webp, png, bmp, tim | any size: stretched |
| UI sheets | `ui/status`, `ui/blue`, `ui/statface`, `ui/staitem`, `ui/itemboxn`, `ui/select_b`, `ui/select_k`, `ui/t_press`, `ui/t_start`, `ui/died`, `ui/en05`, `ui/en07` | webp, png, bmp, tim | the shipped page's; an RGBA replacement uses its own alpha |
| item art | `item/item_all`, `item/item_mix`, `item/medal` | webp, png, bmp, tim | the shipped sheet's; black is a hole |
| item-view / enemy / player art | (inside `item/*.ivm`, `enemy/em*.emd`, `player/*.emd`) | — | override the whole model entry |
| doors | (inside `door/*.dor`) | — | override the whole entry |
| font sheets | `font/font`, `font/jpn`, `font/latin` | webp, png, bmp, tim | the shipped sheet's; at most 255 opaque colours |
| map pages | `map/blue`, `map/map00` … `map/map0e` | webp, png, bmp, tim | the shipped page's; at most 255 opaque colours |
| FILE pages | `file/file000`, `file/file001`, `file/filei*`, `file/textm_*` | webp, png, bmp, tim | the shipped page's |
| room effect sheets | `effspr/esp000`, `effspr/esp2xx` | webp, png, bmp, tim | the shipped page's (the room's own sprites composite onto it) |
| player shadow | `shadow/kage` | webp, png, bmp, tim | the shipped page's; an RGBA page's alpha is the coverage |
| BGM | `bgm/013`, `bgm/024_00` | ogg, wav | any |
| sound effect | `se/a_mcn03`, `se/chain1`, `se/v110_00` | ogg, wav | any |
| voice | `voice/01_09` | ogg, wav | any |
| film | `movie/oj`, `movie/capcom`, `movie/ed1` | mp4, avi | any size up to 4096x4096, any frame rate |

The stems are the shipped entries' names without their extensions: `roomcut` and
`roommask` use `{stage}{room:02X}_{camera:03}` (`RoomId::cut_stem`), `bgm` uses
`{id:03x}` plus an optional `_{variant:02}` (`music::pack_path`), `voice` uses
the 1-based stage row and the lowercase hex id (`voice::entry_path`), and
everything else is the literal name `arklay list` prints.

### Mismatch behaviour

A replacement is the winning entry, so it is validated exactly like a shipped
entry, with these extra rules:

- **Format.** A replacement that does not decode fails like a corrupt shipped
  entry: a required asset (a room background, the menu or font art a screen
  opens with) aborts that load with the entry's own path in the message, and an
  optional one logs a warning and leaves its layer empty.
- **Size.** Fixed-geometry art is addressed by UVs or sprite rectangles, so a
  page replacement must keep the dimensions of the entry it replaces; a
  mismatch is refused with a message naming the replacement's size, the shipped
  size and the reason. Full-frame pictures (room backgrounds and the `ui/*`
  backdrops) may be any size: they are stretched over the frame, which is what
  an HD replacement wants. `.pix` is headerless, so it takes the shipped
  entry's dimensions and is refused when it shadows nothing.
- **Colours.** An indexed consumer (the font sheets and the map pages, whose
  layouts the engine addresses by palette index) accepts a true-colour
  replacement with at most 255 distinct opaque colours and re-indexes it colour
  for colour; more colours is an error naming the count. The other sheet
  classes take a true-colour replacement as-is.
- **Layering.** A replacement the engine cannot classify (a fifth format, an
  `.ogg` for an image class) is not a lookup candidate at all, so it stays an
  ordinary new entry and the shipped asset is drawn.

### The checked-in example

`mods/hd/` is the reference replacement mod: a 640x480 PNG `roomcut/100_000`
(a 2x background), a 128x208 `ui/itemboxn` shipped as both PNG and WebP (so the
layer's own priority picks the WebP), and an OGG Vorbis `bgm/013`.

```sh
arklay mod build mods/hd --out hd.akpak
arklay verify hd.akpak                 # the png and ogg rows
arklay re1.akpak --mod hd.akpak --room 100 --ticks 120 --capture hd.bmp
arklay re1.akpak --mod hd.akpak --ui box --capture hd-box.bmp
```

`arklay mod build --base <base.akpak>` counts a format override as an override:
`roomcut/100_000.png` over the base's `.bmp` is not reported as a new file.

### HD texture-pack names

The RE1 HD texture pack (`bio1hd.asi`, run by Classic REbirth) names every
replacement `hires/<class>/<hash>.webp`. `<hash>` is the CRC-32 (IEEE / zlib) of the
texture's texels as the game uploads them, in 32-bit BGRA, written as 8 uppercase hex
digits. `arklay hash FILE` reproduces the conversion and prints the name for every
texture in an original file:

```sh
arklay hash JPN/STAGE1/RC1000.pak        # bgd/DD17F52F.webp
arklay hash JPN/objspr/OSP00001.pak      # mask/C5157461.webp
arklay hash JPN/ENEMY/EM1002.EMD         # skin/474AD7E1.webp, one line per TIM and CLUT row
arklay hash JPN/DATA/SEL_BACK.PIX        # bgd/B87E761B.webp
arklay hash JPN/DATA/BLUE.TIM            # misc/4F8DC05B.webp
```

The game's conversion, which the hash depends on:

- A 15-bit texel widens each channel to 8 bits. Room backgrounds (`RC*.pak`) and
  masks (`OSP*.pak`) bit-replicate (`v << 3 | v >> 2`). Model, menu and `DATA/*.PIX`
  textures shift (`v << 3`).
- Pure black becomes `0x000001` (blue 1) so it stays distinct from transparent `0`. A
  mask's colour 0 is transparent.
- `bgd` hashes the left 256 x 240 texels with alpha forced to `0xFF`. `mask` hashes
  the left 256 x 256 texels, with opaque texels at alpha `0xFF`. `misc`, `skin` and
  `skin2` hash the whole texture, with every texel but `0` and `1` at alpha `0xFF`. A
  window larger than the texture is padded with zero texels.

The class comes from how the game uses the texture, so `arklay hash` infers it from
the file: a `.pak` is `bgd` or `mask`, models (`EMD`, `EMW`, `IVM`, ...) are `skin`,
rooms (`RDT`) are `skin2`, and other 320x240 images are `bgd`; anything else is
`misc`. `misc`, `skin` and `skin2` share one hash rule, so `--class` only changes the
directory for those.

The hash covers the texture the game builds at runtime, so a screen composited from
several sources has no single source file. Classic REbirth's save screen is its own
`type.png` with the menu panel baked in, which hashes to `bgd/D1D144A8.webp`. The raw
`JPN/DATA/TYPE00.TIM` does not. PNG/BMP/WebP input is treated as that kind of
artwork: alpha 0 is transparent, and every other pixel keeps its colour, with black
remapped to `0x000001`.

### HD texture packs

A mod replaces textures by these names under `htexture/`. On first use every image
(`webp`, `png` or `bmp`) anywhere below `htexture/` is indexed by the first 8 hex
digits of its file name, so `htexture/bgd/DD17F52F.webp`,
`htexture/anything/DD17F52F.png` and `htexture/DD17F52F_3B.webp` all replace the
texture hashing to `DD17F52F`; the subdirectories are only for organising files. When
two files share a hash, a bare `<hash>` name wins over a suffixed `<hash>_XX` one.

The lookup is the last step of loading a texture. The usual overrides by path decide
which pixels load, and then the hash of those pixels picks the replacement, so an HD
pack never changes which texture a room shows, and it composes with any mod that
replaces the originals. If the pixels a mod supplies hash to no file, the texture is
drawn as loaded.

Replacements are drawn by the GPU renderer (`--renderer gpu`) at their own
resolution. The software renderer is 320x240 and keeps the originals.

- A background or mask image is sampled from the replacement in place of the
  original. The replacement covers the texels its hash window covers: the whole
  background, or 256 x 256 mask texels.
- A model page keeps its original indices, which decide transparency and the STP
  blend. Each CLUT row's colours come from that row's replacement. A 128-texel-wide
  page is stretched to 256 before hashing, as the game uploads it, so its
  replacement covers 128 x 256 texels.
- The 2D screens (files, item pictures, the status screen and item box, maps,
  `DATA/*.PIX` screens such as the select and epilogue backgrounds) are replaced per
  draw. Each draw is still clipped and positioned as in software, so the HD art covers
  exactly the original's texels, and text or highlights drawn over it stay on top.
  A menu page is hashed unstretched with black kept as `0x000001`, in a 256 x 256
  window, or at its own size when smaller. A `.PIX` screen is hashed like a `bgd`
  but with shift widening.

Some 2D art stays software: map room highlighting (drawn through a palette lookup),
the fonts, the title screen and the raw save screen (`TYPE00.TIM`), whose hashes
match no file in the RE1 HD pack.

A texture with no replacement is reported once on stderr, with every hash that was
tried:

```text
htexture: no replacement for 256x256 model page, CLUT row 1: 3D4A4499 ...
```

A complete pack should report nothing while playing. Missing reports mean either the
pack lacks the texture or its name was hashed from different pixels.

## Language files

A mod may ship `language/<tag>.txt` with any subset of strings. Every layer's
copy of a tag merges per key, later layers winning, so a translation mod can
replace a handful of strings without restating the file; a whole new tag works
too. The runtime picks the file through `--language`, the OS locale and the
`en-US` fallback; the format, the tag vocabulary and the key registry live in
[`language.md`](language.md).

```sh
# A French-language mod overriding one item name and one message
mkdir -p mods/fr/language
cat > mods/fr/manifest.toml <<'EOF'
[pack]
format = 1
id = "fr"
kind = "mod"
base = "re1"
EOF
cat > mods/fr/language/fr-FR.txt <<'EOF'
ITEM_33_NAME = "Clé du manoir"
MESSAGE_1B = "{scroll 2}Ils se sont échappés…{timed 48}"
EOF
arklay mod build mods/fr --out fr.akpak
arklay re1.akpak --mod fr.akpak --language fr-FR --room 100
```

## Mod source directories

`arklay mod build <dir> --out <pack.akpak>` builds a mod from a directory:
`manifest.toml` is required and packed, every other file maps to a pack entry
equal to its directory-relative path, and a `scd/<stem>.s` source is assembled
into `scd/<stem>.scd` (the `.s` itself is not packed). `--base <base.akpak>`
checks the declared base id and reports override paths the base does not hold
as a warning, never an error; a replacement whose stem matches a base entry of
its own class (the previous section) is an override, not a new file. PNG, WebP
and OGG sources are packed verbatim and stored raw, exactly like the formats
that carry their own compression.

The checked-in `mods/demo/` is the reference mod:

```sh
arklay mod build mods/demo --out demo.akpak
arklay re1.akpak --mod demo.akpak --room 100 --capture demo.bmp
```

It carries a manifest, a whole-room script override for RDT 1000, a Lua hook
and a generated 320x240 background, but no game assets.

The checked-in `mods/hd/` is the replacement-format reference: a 2x PNG room
background, a UI page shipped as both PNG and WebP, and an OGG Vorbis BGM
track, with no game assets either.

## The `.s` script assembler

`arklay scd export ROOM1000.RDT --out 1000.s` writes the lossless
disassembly; `arklay scd build 1000.s --out scd/1000.scd` assembles it back
into a standalone container that `scd/{id}.scd` overrides and the engine both
read through the RDT pointer-table ABI. `--list` also writes a `.lst`
listing. The assembler accepts exactly the disassembler's grammar, including
`.block` section markers, `off_XXXX` labels, `db` trailing chunks, the named
operand constants and the variable-width commands; a script override replaces
the whole room's init/main/event scripts, so a demo room's vanilla doors and
events are inert.

## Lua hooks

A pack opts into scripting by carrying `lua/**.lua` entries. The manifest's
`lua` list selects and orders them; without one, every `lua/` entry runs in
lowercased-path order. In a layered run each layer contributes its own list
(the base first, then each mod in applied order), so a mod names its hooks in
its own manifest. Each chunk is compiled and run once when the session opens;
the globals `on_init`, `on_room_load`, `on_tick` and `on_message` are then
called:

```lua
local granted = false

function on_init(api)
    api:register_save_screen("script/mine.lua")  -- before any room
end

function on_room_load(api)
    api:log("entered " .. api:room())
    api:flag_set(0, 2, true)          -- bank 0, bit 2, set
end

function on_tick(api, tick)           -- tick is the completed fixed-tick count
    if tick >= 30 and not granted then
        granted = true
        api:give_item(0x0F)
    end
end

function on_message(api, id)          -- raised by the preceding tick
    return id + 1                     -- nil/nothing keeps the original id
end
```

The hook order is:

1. `on_init(api)` once the pack's scripts have loaded and before any room. It
   is where a pack registers things the engine reads outside a room, such as
   the save/load screen (`api:register_save_screen`). The app runs it once in
   a throwaway VM and scratch state before the title appears; every session's
   own VM runs it again over its live state.
2. `on_room_load(api)` once a room's state exists (RDT, init script and room
   edits applied) and before its first tick. It runs again on every room
   entry, including a door transition; the Lua globals themselves persist for
   the session (`granted` above is not reset). The room-load camera scan runs
   after it, so `api:set_room_camera` only sticks when the room's init script
   holds the camera lock; otherwise the cut is re-homed from camera 0 at the
   player's position, exactly like the original's `room_set`.
3. `on_tick(api, tick)` after every fixed 30 Hz room tick, before that tick's
   audio dispatch. `tick` is `game.frame`, the number of completed room ticks.
4. `on_message(api, id)` when the tick raised a message; the returned id
   replaces it (must fit a byte, else the original stays). The window's bytes
   are resolved on the next tick, so the new id is the one displayed.

The `api` is the live game state behind a scoped userdata. Only these methods
exist (Lua colon syntax):

| Method | Effect |
| --- | --- |
| `api:room()` | The three-digit room id, read-only. |
| `api:flag_get(bank, bit)` | Whether a flag bit is set. |
| `api:flag_set(bank, bit[, value])` | Set (default) or clear a flag bit. |
| `api:item_count(item)` | Total held quantity. |
| `api:give_item(item[, quantity])` | Add items through the normal merge rules (default one). |
| `api:item_remove(item)` | Clear the whole inventory slot holding the item. |
| `api:set_item_slot(slot, id[, quantity])` | Replace a player slot's stack; past the held run it appends, id `0` drops it. |
| `api:clear_inventory()` | Empty the inventory and clear the equipped/selected item. |
| `api:set_equipped(id)` | Equip an item (or `nil`/`0` to unequip); derives the weapon id. |
| `api:set_equipped_weapon(id)` | Set the weapon id the firing model uses, without touching the equipped item. |
| `api:item_box_set(slot, id[, quantity])` | Replace one of the 48 item-box slots. |
| `api:item_box_clear()` | Empty the whole item box. |
| `api:set_health(hp)` | Set the player's health and its BioCard copy. |
| `api:set_max_health(hp)` | Set the player's maximum health. |
| `api:set_health_status(byte)` | Set the health-status flags (poison bits included). |
| `api:set_player_pos(x, y, z)` | Set the player entity's position. |
| `api:set_player_angle(degrees)` | Set the player's 12-bit yaw. |
| `api:set_character(id)` | Select Chris (`0`) or Jill (`1`): identity, model and entity id. |
| `api:set_room(stage, room)` | Rewrite the room identity the save block records (authoring only). |
| `api:set_room_camera(cut)` | Select the room camera cut. |
| `api:set_file_collected(item[, value])` | Set/clear a FILE-tab document's collected bit by item id (`0x5F..=0x6E`). |
| `api:set_item_use_flag(item[, value])` | Set/clear a key/special/chemical item-use flag. |
| `api:set_flag_bank(bank, bytes)` | Overwrite a bank's raw bytes, from a string or a sequence table. |
| `api:set_state_byte(index, value)` | Write one BioCard state-image byte. |
| `api:kill_event(slot)` | Deactivate a running event slot. |
| `api:message(id)` | Request a room/global message. |
| `api:register_save_screen(entry)` | Run `entry` (a pack path) as the save/load screen instead of the ported original; `nil` restores the original. See [Screen scripts](#screen-scripts). |
| `api:log(text)` | Write one `[lua]` line to stderr. |

The state-authoring methods (`set_*`, `clear_inventory`, `item_box_*`) exist
for the CLI's `--setup-script`/`--save-out` oracle seam: a mod may use them
too, but they are deliberately coarse. `set_room` only rewrites the identity
the save block serialises; it does not load another room.

### Sandbox and errors

The VM loads the base, `string` and `table` libraries only: `os`, `io`,
`package`, `debug`, `coroutine` and `math` do not exist, so there is no
filesystem, network, clock or scheduling access. The base library's
`dofile`, `loadfile`, `load` and `collectgarbage` globals are set to nil
before any chunk runs, so a hook cannot load a file (or a binary chunk) or
drive the collector, and the state carries a 16 MiB allocation ceiling. An
instruction-budget hook aborts a script that runs away (the call errors out
within a few million instructions). Every load or call error is logged once
with a `[lua]` prefix and disables only the failing hook or chunk; the game
session always continues. A pack with no `lua/` entries loads no VM, and runs stay
byte-identical.

Hooks cannot spawn enemies: the hook API has no such operation by
construction. Entity behaviour is the separate enemy-script surface below.

## Screen scripts

A pack may carry Lua scripts that implement whole screens, and register one as
the save/load screen. Nothing about a screen is built into the engine: the
ported original is the default, and a registered script replaces it. The
checked-in `mods/crloadsave/` is the reference — a Classic REbirth-style
folder screen:

```
mods/crloadsave/manifest.toml        lua = ["mod.lua"]
mods/crloadsave/mod.lua              function on_init(api) api:register_save_screen("script/loadsave.lua") end
mods/crloadsave/script/loadsave.lua  the screen itself
mods/crloadsave/language/*.txt       its UI_SAVE_CR_* strings
```

`on_init` is the hook that runs once the pack's scripts have loaded, before
any room; `api:register_save_screen(entry)` names the screen script (or `nil`
to go back to the ported original). The registration is pack-level: the app
runs the pack's scripts once in a throwaway VM before the title appears, so
the title's LOAD GAME and the load screen honour it, and every session's own
VM runs `on_init` again over its live state for the typewriter. A registered
screen also changes the title's existence probe: it counts every loadable file
in the save directory rather than the eight `savedatN.dat` slots, since a
script lists the directory itself.

Screen scripts are not hooks: `lua/**.lua` entries run once when a session
opens, while a screen script is loaded and run only when its screen opens.

A screen script defines three globals (`update` is required; `on_open` and
`draw` are optional):

```lua
function on_open(api) end        -- once, before the first tick
function update(api, input) end  -- once per fixed tick
function draw(api) end           -- once per frame
```

The screen runs in the hooks' sandbox — base/`string`/`table` only, 16 MiB, the
instruction budget — in its own VM and draws in the 320x240 layout space (the band or canvas
described below at a wider aspect). A
screen that fails to load, or a missing `update`, leaves the engine on the
ported original screen; a script that errors while running is logged once and
the screen leaves (back to gameplay from a save screen, back to the title from
the load screen).

### The `api` userdata

| Method | Effect |
| --- | --- |
| `api:mode()` | `"save"` or `"load"`. |
| `api:ticks()` | Fixed ticks since the screen opened (0 during the first `update`). |
| `api:saves()` | The save directory's files, sorted by name. Each entry is a table: `path`, `name` (`savedat1.dat`), `base` (`savedat1`), `timestamp` (Unix seconds), `loadable`; a loadable file also carries `character` (`0` Chris, `1` Jill), `character_name`, `saves`, `stage` (the block's 0-based stage byte), `room` and `location` (the resolved room label). The list is rescanned after a save or remove. |
| `api:string(key)` | A `language/<tag>.txt` string, decoded to characters; `nil` when no language carries the key. |
| `api:character_name(index)` | The save screen's name for character `0`/`1`, decoded. |
| `api:text_width(text[, tight])` | The pixel width `text` draws at (the session's tight metrics unless `tight` is given). |
| `api:sound(name)` | Queue a screen sound: `cursor`, `decide`, `cancel`, `error` (the cancel cue), `title`, `select_cursor`, `select_confirm`, `save_cursor` (the save screen's cursor move), `save_confirm` or `page_turn` (the FILE reader's page turn). Cues queue from `on_open`, `update` and the engine's hooks and play after the call (a cue raised while drawing is dropped); the engine drops them in a capture run, which stays silent. |
| `api:save(name)` | Write the pending block to `<save_dir>/<name>` and return `ok, err`. `name` is one plain file name (no directory part); the extension is the script's choice. Save mode only. |
| `api:remove(name)` | Delete `<save_dir>/<name>`, ignoring a missing file. Returns `ok, err`. |
| `api:load(name)` | Load `<save_dir>/<name>`: the engine continues from that save on the next tick. Returns `ok, err`. Load mode only. |
| `api:exit()` | Leave the screen (back to gameplay from a save screen, back to the title from the load screen). |
| `api:log(text)` | Write one `[lua]` line to stderr. |

### Drawing

The drawing methods (`background`, `rect`, `text`, `sprite`, `sprite_region`)
are only available inside `draw`; calling one from `on_open`/`update` raises an
error, and every other method works from any of the three. Every colour is
`0xRRGGBB` or `"#RRGGBB"`; text also takes a tint name (`white`, `green`,
`red`, `grey`, `yellow`) or a CLUT row number, the rules the vanilla screens
use. A text `"#RRGGBB"` is a per-channel multiplier over the sheet's grey ramp,
so `"#3F7DFA"` draws the blue the built-in screen's save rows use.

| Method | Effect |
| --- | --- |
| `api:background(entry[, opts])` | Draw the pack image `entry` over the whole screen, scaled. A 320x240 RGBA image with default options is blitted exactly; `colour` tints any background, and `brightness`/`blend` apply to an indexed one. |
| `api:rect(x, y, w, h, colour[, alpha])` | A filled rectangle; `alpha` (default 255) blends it over what is already there. |
| `api:text(x, y, text[, opts])` | Draw `text` through the game font and the session's text encoding, with the vanilla shadow. |
| `api:sprite(entry, x, y[, opts])` | Draw the whole pack image at `(x, y)`. |
| `api:sprite_region(entry, sx, sy, sw, sh, dx, dy, dw, dh[, opts])` | Draw the `src` region of the pack image, scaled into the `dst` rectangle. |
| `api:image_size(entry)` | The pack image's width and height. |
| `api:fade(alpha)` | Set the screen's black-overlay opacity (`0` clear, `255` black). |

An image `entry` resolves like every other asset: by its stem, highest
priority first `webp > png > bmp > tim > pix`, so a mod's replacement wins and
a `.tim` keeps its palette (4bpp/8bpp indexed, 16bpp RGBA). Decoded images are
cached per screen, up to 64 images and 64 MiB of pixels. The `opts` table
is `{colour = "white"|3|"#RRGGBB", brightness = 2, tight = true, clut = 0,
blend = "opaque"|"half"|"half_variant"}` — `brightness` is the font's `0..=30`
scale, `clut` the indexed palette row and `blend` the sprite blend mode.

### The input table

`update(api, input)` receives the tick's edges: `up`, `down`, `left`, `right`,
`confirm`, `cancel`, `page_left`, `page_right`, `start`, `return_title` (F9),
`any`, plus the
original pad words `pad` (the edge word) and `held` (the held directions).
The bit layout is the original's: confirm `0x0001`, cancel `0x0002`, start
`0x0008`, up `0x0010`, down `0x0020`, left `0x0040`, right `0x0080`, L1
`0x0400`, R1 `0x0800`.

### Engine screens

The game's own screens are pack scripts too: `ui/<id>.lua` (`title`,
`char_select`, `save_load`, `ending_result`, `ending_epilogue`,
`return_title`, `death`, `computer_lab`, `main_menu`, `map`, `file`, `item_view`, `item_box`, …), with shared code under `ui/lib/`. A pack or mod entry
replaces the engine's built-in copy. An engine screen is a module rather than a
set of globals:

```lua
local M = {}
function M.open(api, args) end    -- once; `args` is the opener's table
function M.update(api, input) end -- once per fixed tick
function M.draw(api) end          -- once per frame
return M
```

It gets every method above plus the ones below. The layout space is
`api:width()` x `api:height()`; a screen should centre its 320x240 art on
`(width // 2 - 160, height // 2 - 120)` rather than assuming the original size.
At the default 4:3 aspect the space is 320x240. A wider `aspect` (see
`arklay.toml`) changes it in one of two ways:

- **The band.** By default a screen draws in the cropped band the window
  shows: 320 wide and `320 * h / w` rows (180 at 16:9, 200 at 16:10). A screen
  whose content is taller than that has to lay it out again: the shipped title
  shows the bottom of its art, the save screen stacks its rows tighter, and the
  ending captions are pulled inside the band. `api:background` keeps
  stretching over the logical 320x240 quad centred in the space, so the band
  crops a full-screen picture rather than squashing it.
- **The canvas.** A screen drawn on black can call `api:set("canvas", 1)`. The
  frame then becomes a full-height canvas of the aspect's width (427x240 at
  16:9, 384x240 at 16:10) with no room under it, so the screen keeps every row
  and gains columns at its sides. The pause menu, the item box, the file and
  map screens and the examine viewer use it; the status panel runs its bottom
  strip out to the canvas edges.

Model draws (`api:draw_ivm`, actors) project about the centre of the layout
space, so a screen camera's `screen` offset stays relative to the 320x240
layout's centre.
A screen that fails to open or errors leaves through the opener's fallback.
The engine may also call a named module function, `M.<name>(api)`, to tell a
running screen something happened (the pick-up viewer's `begin_exit`), or
`M.<name>(api, value)` to hand it one value (the item box's `cursor_shown`
before each draw); a screen without that function ignores it.

The pause menu (`main_menu`) changes the game: the engine lends it the
session's state for `open`, `update` and its hooks, so the methods marked
*lent* below work there and raise an error from any other screen (or from
`draw`).

| Method | Effect |
| --- | --- |
| `api:width()`, `api:height()` | The layout size. |
| `api:exit([action[, arg]])` | Leave the screen, reporting `action`: `title`, `char_select`, `save_load`, `attract`, `resume`, `quit`, `ending_result`, `ending_epilogue`, `ending_save`, `new_game` (`arg` the character) or `load_game` (`arg` the slot `0..=7`). |
| `api:set(key, value)` | Publish an integer/boolean the engine reads back (`nil` clears it); up to 64 keys. |
| `api:overlay(colour, alpha)` | A `"black"`/`"white"` full-screen overlay at `alpha`, or `nil` for none. |
| `api:has_font()`, `api:has_text()` | Whether the pack has a game font / text table. |
| `api:pack_bytes(entry)` | A pack entry's raw bytes as a string; `nil` when absent. |
| `api:ui_bytes(key)` | The text table's font-encoded bytes for `key`; `nil` without a text table. |
| `api:location_bytes(stage, room)` | The font-encoded location label of a save's room. |
| `api:wrap(bytes, max_width[, opts])` | Font-encoded `bytes` broken at spaces into a table of rows no wider than `max_width` pixels; one row without a font. |
| `api:glyphs(x, y, bytes[, opts])` | Draw font-encoded bytes (white, brightness 2, the session's metrics; `{tight = false}` draws on the fixed grid). `{fit = width}` condenses a row wider than `width` pixels. |
| `api:text_8x14(x, y, bytes[, opts])` | Draw raw bytes through the original's unshadowed 8x14 grid in `opts.colour`. |
| `api:grid_text(x, y, scale, bytes)` | Draw raw font-sheet cells. |
| `api:save_slots()` | Eight booleans: which numbered slots hold a save. |
| `api:slot_info()` | Eight entries: `false` for an empty slot, else `{character, saves, stage, room}`. |
| `api:save_slot(slot)` | Save mode: write the pending block to numbered slot `0..=7` (consuming an ink ribbon when the opener asked) and return `true, info` or `false, err`. |
| `api:has_image(stem)` | Whether the pack has the image. |
| `api:picture(stem[, key[, geometry]])` | Load a full-colour image and return a handle; `key` is a transparent colour, `geometry` `"free"` (default) or `"shipped"` (pinned to the original sheet's size). |
| `api:page(stem)` | Load an indexed UI sheet and return a handle. |
| `api:indexed_page(stem)` | Like `api:page`, but always indexed: a true-colour replacement is re-indexed colour for colour, for screens that composite by texel index (`api:page_lut`). |
| `api:page_palette(page[, row])` | An indexed page's CLUT row (default 0) as a table of `0xRRGGBB` colours keyed by texel index `0..=255`; `nil` for a full-colour page. |
| `api:ivm(entry)` | Load a model and return a handle. |
| `api:art_size(handle)` | A handle's width and height. |
| `api:clear()` | Clear the frame to black. |
| `api:fill_rect(x, y, w, h, r, g, b[, a])` | A filled rectangle. |
| `api:line(x0, y0, x1, y1, r, g, b)` | A one-pixel opaque line, both ends included. |
| `api:blend_black_rect(x, y, w, h, alpha)` | Darken a rectangle. |
| `api:draw_background(picture, x, y[, r, g, b])` | Draw a picture tinted by `r, g, b`. |
| `api:picture_sprite_scaled(picture, sx, sy, sw, sh, dx, dy, dw, dh, scale)` | A picture region, scaled. |
| `api:page_sprite_scaled(page, sx, sy, sw, sh, dx, dy, dw, dh, clut, scale[, tint])` | A sheet region through CLUT row `clut`, scaled and tinted. |
| `api:page_sprite_mirrored(page, sx, sy, sw, sh, dx, dy, dw, dh, clut, brightness, mirror_u, mirror_v)` | A sheet region flipped per axis. |
| `api:page_lut(page, sx, sy, w, h, dx, dy, lut)` | An unscaled rect of an indexed page drawn through `lut`, a table of opaque `0xRRGGBB` colours keyed by texel index; an index without an entry (index 0 included) is transparent. |
| `api:draw_ivm(model, {joints, objects, camera = {from, to, fov, screen}, light})` | Draw a model: object `i` takes joint chain `objects[i]`, the rest the one `joints` chain; `screen = {x, y}` offsets the projection centre. |
| `api:item_model(item)` | Load the item viewer's model for `item` and return a handle, or `nil, err`. |
| `api:model_objects(model)` | The number of objects in a model. |
| `api:clip([x, y, w, h])` | Clip this draw's later primitives to a rectangle; no arguments clears it. |
| `api:darken(alpha)` | Fade the whole frame toward black by `alpha`. |
| `api:page_ramp(page, sx, sy, w, h, dx, dy, ramp)` | An unscaled sheet region modulated by `2 * ramp`, half-blended. |
| `api:flag(bank, bit)` | One bit of the paused session's flag banks; `false` outside a session. |
| `api:room()` | The paused session's stage, room, camera cut and player (0 Chris, 1 Jill). |
| `api:item_name_bytes(id[, examined])` | The font-encoded item name; with `examined`, the menu's name, which stays the unchecked one until the item's class is examined. |
| `api:font_margin()` | The session font's left text margin. |
| `api:message_active()` | Whether the game's message window is up. |
| `api:examine_spin(pitch, yaw, roll, sx, sy, sz)` | The item viewer's pose after pre-multiplying a spin, re-extracted the reference way. |
| `api:examine_check(item, pitch, yaw, roll, zoom_spin, quantity)` | The examine check: `"description"`, `"zoom"` or `"message", id, mark, select`. |
| `api:class_message(item)` | The global message the item's examine class shows, or `nil`. |
| `api:inventory(slot)` | The id and quantity of inventory slot `slot` (`0, 0` past the held run). |
| `api:inventory_capacity()` | The current character's inventory slots. |
| `api:item_box(slot)` | The id and quantity of item-box slot `slot`. |
| `api:player_health()` | The player's health, maximum health and status byte (poison bits `0x22`). |
| `api:character()` | The portrait's character index (`0` Chris, `1` Jill, `2`/`3` the alternates). |
| `api:has_radio()` | Whether the radio tab is live. |
| `api:equipped()` | The equipped item, `0` for none. |
| `api:item_count(item)` | How many of `item` the inventory holds. |
| `api:rand()` | *Lent.* The next draw of the game's shared `rand()` stream, `0..=0x7FFF`. |
| `api:select_item(item)` | *Lent.* Mirror the item under the cursor into the state byte the scripts test (`0` for none). |
| `api:equip(item)` | *Lent.* Equip `item`, or unequip with `0`. |
| `api:use_item(slot)` | *Lent.* USE the item in `slot`: `true, healed, cured, closes` (the item is consumed; `closes` exits the whole menu), `false, category` for a refusal, nothing for an empty slot. |
| `api:combine(slot, target)` | *Lent.* MOVE `slot` onto `target`: `"applied", herb`, `"drug_store"`, `"herb_no_recipe"` or `"none"`. |
| `api:show_message(id)` | *Lent.* Open the game's message window on global message `id`. |
| `api:item_icon_row(item)` | The item's 40x30 row in `item/item_all`, or `nil`. |
| `api:draw_actor(name, {from, to, fov})` | Draw an engine-owned actor through the camera and return whether it drew; the DIED screen offers `"player"`, the posed corpse. |

## Enemy scripts

Entity behaviour is pack data. The engine looks up `enemy/em{id:02x}.lua` for
every active monster slot (`0x00..=0x16`) and runs its
`function update(e)` once per fixed tick. Shared library files under
`enemy/lib/**.lua` are ordinary pack modules: a script imports one with the
sandboxed, pack-scoped `require(name)`.

### require

`require(name)` loads any `.lua` entry of the **game pack** and caches its
return value once per session:

| Name form | Resolves to |
| --- | --- |
| `/enemy/lib/npc` | Absolute from the pack root. |
| `./npc`, `../lib/npc` | Relative to the directory of the requiring module. |
| `enemy/lib/npc` | The exact pack entry if it exists, otherwise relative to the requiring module. |

The `.lua` suffix is optional, paths are case-insensitive, and `..` may not
escape the pack root. A module that returns nothing caches `true` (Lua
semantics); requiring a missing module raises a script error, which parks the
requiring id like any other error. There is no filesystem access: only pack
entries can be reached.

```lua
-- enemy/em13.lua
function update(e)
    if e.state == 0 then
        e.state = 1
        e.health = 0x37
        e:advance_anim(0x400)
    end
end
```

The `e` argument is the live entity behind a scoped userdata. Its scalar
fields are Lua properties: `e.state`, `e.ignore`, `e.action_behavior`,
`e.action_state`, `e.health`, `e.hit_state`, `e.death_timer`, `e.status_flags`,
`e.angle`, `e.pitch`, `e.roll`, `e.animation_id`, `e.animation_frame_id`,
`e.timing_control`, `e.blend_counter`, `e.joint_scale`, `e.sca_radius`,
`e.sca_half_height`, `e.shadow_half_x`, `e.shadow_half_z`, `e.shadow_tint`,
`e.flags`, `e.scd_timer`, `e.unk_c6`, `e.unk_c8`, `e.tex_bank`,
`e.dir_control_flags`, `e.seq_counter`, `e.angle_turn_delta`, `e.move_timer`,
`e.is_moving`, `e.move_max_steps`, `e.splatter_flag`, `e.bob_speed`,
`e.player_pos_x`, `e.player_pos_z`, `e.reaction_timer`, `e.pathfind_state`,
`e.look_at_flags`, `e.look_at_yaw_step`, `e.look_at_pitch_step`,
`e.target_x`/`y`/`z`, `e.pos_x`/`y`/`z`, `e.move_speed_current`,
`e.action_ticks_counter`, `e.collision_flags`, `e.attacking_direction`,
`e.has_enter_switch_zone` and `e.active`. Each getter returns the field's own
width and each assignment truncates to it exactly like the original's C store,
so a script may compute in 64-bit integers and assign without narrowing by
hand. `e.behavior_flags` is the one driver byte a script may also write (the
adder rewrites its spawn kind when it drops, emerges or respawns). The shared
scratch block also has named word views that a couple of bytes or fields
compose: `e.writhe_velocity` (`i16`, the `attacking_direction`/
`dir_control_flags` pair), `e.writhe_amplitude` (`i16` over
`tex_bank`/`seq_counter`), `e.tint_flashes` (`i16` over
`angle_turn_delta`/`move_timer`), `e.groan_timer` (`i16` over
`action_speed`/`hit_threshold`), `e.sink_wobble` (`i16` over
`behavior_step`/`action_counter`) and `e.stored_pos_x` (`i32` over
`splatter_flag`/`bob_speed`/`reaction_timer`); `e.stored_pos_z` is the plain
`i32` field behind it. The adder's views over the same scratch bytes are
`e.sca_active` (`u16`, `splatter_flag`/`bob_speed`), `e.sca_touch` (`i16`,
`tex_bank`/`seq_counter`), `e.room_collision` (`u16`,
`is_moving`/`move_max_steps`), `e.stuck_frames` (`u16`, the `reaction_timer`
word) and `e.player_distance` (`u16`, `angle_turn_delta`/`move_timer`); the
read-only `e.path_bit` is bit 0 of the pathfinder scratch byte. The wasp's
views over the same block are `e.wasp_drift` (`i16`, the
`attacking_direction`/`dir_control_flags` pair), `e.wasp_touch` (`i16`,
`angle_turn_delta`/`move_timer`), `e.wasp_climb` (`i16`,
`action_speed`/`hit_threshold`), `e.wasp_timer` (`i16`,
`behavior_step`/`action_counter`), `e.wasp_bob` (`i16`, the `reaction_timer`
word), `e.wasp_distance`/`e.wasp_collision` (`u16`, the two halves of the
`+0x178` dword, each store preserving the other half), `e.wasp_speed` (`i16`
over `move_speed_current`), `e.wasp_dwell` (`i16` over
`action_ticks_counter`), `e.wasp_anim_done` (`u16`, the `Joint_move`
completion flag), `e.wasp_big`/`e.wasp_respawns`/`e.wasp_sound_latch` (`u8`,
the big-wasp flag, the nest respawn counter and the one-shot sound latch) and
the read-only `e.wasp_path_bit` (bit 0 of the `+0x16E` path-result byte). The
crow's views over the same block are `e.crow_turn_rate` (`i16`, the
`attacking_direction`/`dir_control_flags` pair), `e.crow_floor_limit`/
`e.crow_ceil_limit` (`i16`, the signed views of the `+0x172`/`+0x174` words),
`e.crow_vy` (`i16`, the `reaction_timer` word), `e.crow_dist`/`e.crow_coll`
(`u16`, the two halves of the `+0x178` dword), `e.crow_stuck` (`u16`, the
`+0x17C` word), `e.crow_phase` (`i16`, the `+0x17E` wobble word),
`e.crow_path_word` (`u16`, the `+0x16E` path-result word), the read-only
`e.crow_path_bit`, and its own scratch fields `e.crow_touch` (`i16`, the
`+0x170` volume-touch word), `e.crow_swerve`/`e.crow_swerve_latch`
(`i16`/`u8`), `e.crow_struggle` (`i8`), `e.crow_alt_bias` (`i16`) and
`e.floor_step` (`i16`, the room resolve's crossed step). The two spiders
re-view the same block as `e.ws_turn`/`e.bt_turn` (`i16`, `+0x16C`),
`e.ws_path_word`/`e.bt_path_word` (`u16`, `+0x16E`),
`e.ws_touch`/`e.bt_touch` (`i16`, `+0x170`), `e.ws_delay`/`e.bt_delay`
(`i16`, `+0x172`), `e.ws_splat`/`e.bt_splat` (`i16`, `+0x174`),
`e.ws_trail`/`e.bt_trail` (`i16`, `+0x176`), `e.ws_count`/`e.bt_count`
(`i16`, `+0x178`), `e.ws_web_index` (`i16`, `+0x17A`) and, on the Black
Tiger, `e.bt_cflag` (`i16`, `+0x17A`), `e.bt_web_index` (`i16`, `+0x17C`)
and `e.bt_cooldown` (`i16`, `+0x17E`). `e.shadow_suppressed` is a `bool`
queue gate for the scripted ground quad (the ceiling spider's drop clears
it). The zombies read the same block through their own typed views:
`e.state_word`/`e.state_mirror` (`u32`, the four state bytes at `+0x84` and
their mirror at `+0x184`, read and written as one dword),
`e.move_speed_byte`/`e.turn_speed`/`e.internal_timer`/`e.stagger_timer`
(`u8`, the `+0x180`..`+0x188` behaviour bytes), `e.action_speed`/
`e.hit_threshold` (`u8`, the `+0x17C` pair), `e.behavior_step`/
`e.action_counter` (`u8`, the `+0x17E` pair), `e.next_turn_timer` (`u16`,
`+0xE2`), `e.attack_timer` (`u8`, the high half of the `+0x176` word),
`e.zombie_turn_delta` (`i16`, `+0x170`), `e.zombie_move_word` (`u16`,
`+0x172`), `e.zombie_part_word` (`u16`, `+0x174`), `e.zombie_wander_word`
(`u16`, `+0x17A`) and the two bytes below it, `e.zombie_unk_178`/
`e.zombie_unk_179` (`u8`). The composition lives in Rust, so scripts never
shift bytes together. The hound re-views the block as `e.cb_dist` (`i32`, the
four bytes at `+0x16C`), `e.cb_turn_step`/`e.cb_launch_vy` (`i16`, the signed
views of the `+0x172`/`+0x174` words), `e.cb_probe` (`i16`, `+0x178`),
`e.cb_path` (`u8`, `+0x17A`), `e.cb_swerve` (`i16`, `+0x180`), `e.cb_blood`
(`i16`, `+0x182`), `e.cb_alert` (`u8`, `+0x184`), `e.cb_behflags` (`i16`,
`+0x186`) with its low byte `e.cb_behflags_byte`, `e.cb_aiflags` (`i16`,
`+0x188`) and `e.cb_repause` (`i16`, `+0x18A`). The chimera's views are
`e.c_repause` (`i16`, `+0x172`), `e.c_fade_freeze` (`i16`, `+0x174`),
`e.c_wall_frames` (`i16`, `+0x178`) and `e.c_far_latch` (`i16`, `+0x17A`); it
reads its turn step, path latch, touch latch, wall hit, grab meter and hard
word through the generic accessors above. `e.saved_x`/`y`/`z` (`i16`) expose
the original's 16-bit `position` words the hound's spawn nudge and probes
save and restore, `e.scratch_x`/`z` (`i32`) the shared probe scratch vector
the leap recovery steers at, and `e.cerberus_barked` (`bool`) the
room-shared "a hound has already barked" latch. The hunter's views are
`e.hunter_speed`/`e.hunter_ticks` (`i16`, the signed `+0xC2` speed and `+0xC4`
tick words), `e.hunter_path_latch` (`i16`, `+0x170`), `e.hunter_grab_word`
(`i16`, `+0x172`), `e.hunter_target_x`/`e.hunter_target_z` (`i16`, the two
halves of the `+0x178` dword, each store preserving the other half),
`e.hunter_joint_sel` (`u16`, `+0x17C`), `e.hunter_step_word` (`u16`,
`+0x17E`), `e.hunter_room_hit` (`u8`, `+0x180`), `e.hunter_pounce_latch`
(`u8`, `+0x182`), `e.hunter_death_cnt_a`/`e.hunter_death_cnt_b` (`u8`, the
`+0x183`/`+0x184` damage latch), `e.hunter_strafe_dir` (`u8`, `+0x185`),
`e.hunter_approach_cnt` (`u8`, `+0x187`), `e.hunter_poise` (`u8`, `+0x188`),
`e.hunter_repause` (`u8`, `+0x189`), `e.hunter_leap_flag` (`u8`, `+0x18A`,
the low byte of the crow's altitude word) and `e.hunter_partner` (`u8`, the
partner slot the init picks). Its shared globals are the writable
`e.player_displacement`, `e.player_distance_z` and `e.scaled_down_dist`
(`i32`; the line-of-sight helper overwrites the middle one with its result
byte, exactly like the original), `e.hunter_scream_latch` (`u8`, the
one-per-room howl latch), `e.hunter_intro_jump_kind` (`u8`),
`e.hunter_grab_one_shot` (`bool`, the pounce grab's one-shot flag the death
screen will consume) and the read-only `e.attract_room_camera_id` (`u8`, the
outgoing room id the scripted intro's camera selector reads) and `e.stage_id`
(`u8`, the 0-based `get_stage_id` digit the SCA/intro selection uses).
Read-only views: `e.id`, `e.player_character`, `e.monster_paused`,
`e.scd_anim_param`, `e.prev_pos_x`/`y`/`z` (the stored collision-accepted
position) and the model tint record (`e.model_tint_r`/`g`/`b`,
`e.tint_queue_r`/`g`/`b`, `e.tint_queue_word_a`/`b`, `e.tint_queue_armed`).
The composite helpers stay methods:
animation and pose (`e:advance_anim(step, reverse)`, `e:blend_step()`,
`e:reset_joints()`, `e:set_joint_visible(joint, on)` (32 joints wide),
`e:arm_joint_effect(joint, kind, timer, frame)` (arms one joint's attack
effect: hides it and clears its active flag so `e:leg_reach` reads it),
`e:set_sca(...)`, `e:set_sca2(...)` (a second profile volume; the two-box
walk record), `e:set_sca_hit_point(dx, dy, dz)` (the per-frame SCA hit
retarget: a world-space bias added to the primary volume's rotated centre,
which the bosses use to move a segmented hitbox onto a posed joint; zero
restores the record), `e:leg_reach(part, scale)` (the leg pair's stride
length into `move_speed_current`),
`e:set_shadow_offset(...)`, `e:adjust_shadow_size(dx, dz)`,
`e:update_switch_zone()`, `e:update_look_at()`,
`e:lookat_reset()`, `e:lookat_target(x, z)`, `e:lookat_wander(param)`,
`e:apply_anim_vertex()` to snap the entity's X/Z to the current clip frame's
root vertex plus the `unk_c6`/`unk_c8` offsets the grab latched),
steering and collision (`e:move(offset, distance)` stores the rotated vector,
`e:advance_speed()` re-adds that stored vector without recomputing the yaw,
`e:walk_xz(distance)` is the characters' `MovePlayerXZ` step, which rounds
through the full `RotMatrix` and leaves the stored vector alone,
`e:try_move(...)`, `e:apply_walk_speed(speed)`,
`e:turn_toward_target(x, z, step)`, `e:rotate_toward_target(x, z, step)`,
`e:turn_toward_heading(heading, step)`,
`e:heading_within(heading, half)`, `e:xz_distance_to(x, z)`, `e:angle_to(x, z)`,
`e:check_visual_range(range)`, `e:check_alert_range(range)`,
`e:pathfind_keep(result)` (into the adder's `+0x16C` byte),
`e:wasp_path_keep(result)`, `e:spider_path_keep(result)` (the same keep into
the `+0x16E` word the wasp, WebSpinner and Black Tiger gate on),
`e:zone_path_update(x, z)` (the zombie distance
driver's out-parameter form: write the steered waypoint into
`player_pos_x`/`player_pos_z`), `e:player_facing_entity()` (is the player's yaw within
half a turn of the entity?), `e:player_mashing()` (any d-pad direction or face
button held, the grab struggle gate), `e:mash_reduce()` (how many frames the
held controls shorten the zombie bite: 3 for the face button, +2 for a
direction), `e:footstep(sound_type)`,
`e:separate()`, `e:resolve_collision()`, `e:save_pos()`,
`e:swerve(step, blocked, seed)` (the obstacle-avoidance steering: returns the
yaw delta and updates the swerve/latch scratch, steering toward the player)),
zone navigation
(`e:update_walk_zone()`, `e:pathfind_update(x, z)`, `e:zone_path_find(x, z)`,
`e:zone_shared_edge(a, b)`, `e:corridor_open(...)`,
`e:crossing_heading(from, next)`, `e:position_blocked(x, y, z)`), the posed
skeleton (`e:joint_world_x`/`y`/`z(joint)` return the joint world translation
the previous update's render pass computed; `e:reach_test(joint, x, y, z,
radius)` is the square joint-reach box the monsters test against the player),
the zombie's perception and body helpers
(`e:angular_view_and_distance(fov_half, max_distance)` is the wedge + folded
Manhattan range gate, `e:line_of_sight()` the boundary sight-ray test that
also stores its result byte in `e.player_distance_z` like the original's
shared scratch, `e:wander_turn(movement_dist, angle_step, turn_limit)` the stuck-detection
steering with its flags/counter scratch, `e:two_point_probe(ax, az, bx, bz)`
the prone body's two-endpoint floor probe returning end B's flag bits or
`0x80` after a blocked rollback, `e:blood_splatter(joint, gravity)` the
bleeding-joint physics, `e:set_joint_blood(joint, vel_x, vel_y, counter,
flags)` to arm its scratch, `e:body_part_speed(param)` the leg-chain stride
into `move_speed_current`, `e:rotate_xz(angle, x, z)` the shared 4.12 yaw
rotation, `e:snap_grab()` the bite anchor that pins the entity's and the
player's `unk_c6`/`unk_c8` to the root vertex, `e:joint_flag(joint)`/
`e:set_joint_flag(joint, value)` the joint flag byte: bit 0 active, the gore
bits the death paths read),
the hound's probes and head tracking (`e:nudge_spawn(distance)` steps the
spawn point along the full rotation vector and adds it to the `position`
words, `e:probe_ahead(distance, radius)` steps the entity, runs the room
resolve at that radius and steps back, returning whether it reported a signed
positive code, `e:probe_turn(delta, mul)` scales the live speed, steers
`delta` off the yaw, probes and restores the position words and speed,
`e:ballistic(fwd, vy0, gravity, ground)` is one frame of the shared
projectile arc and `e:head_track()` integrates the accumulated head yaw
toward the player, clamps it to `+/-0x100` and stores it in the swerve word),
the hunter's pair, tracking and drag helpers (`e:hunter_wander_turn(
movement_dist, angle_step, turn_limit)` is the same wander steering with the
hunter's own control byte and path-latch counter, `e:pick_partner()` stores
and returns the partner slot (the enemy-list head, or the next slot when the
hunter is the head), `e:partner_health()`/`e:partner_hit_state()`/
`e:partner_move_speed()`/`e:partner_status()` read that slot's fields,
`e:partner_pos()` its X/Z, `e:track_player_joint(sel)` recomposes the grab
chain around the mouth joint and stores the resulting joint-1 world
translation into the player's tracked-joint record,
`e:track_target_joint(sel)` does the same into the enemy-list base slot
(`e:entity_joint_track(slot)` reads a record back), `e:recenter_on_joint(
which)` pivots the body X/Z around the death chain's tip joint,
`e:raise_grab_one_shot()` sets the pounce grab's one-shot flag and
`e:drag_player()` adds the stored velocity to the player's X/Z; the stored
vector itself is read through `e.speed_x`/`e.speed_y`/`e.speed_z`), and the
cross-slot joint/state accessors the scripted bite uses
(`e:entity_joint_flag(slot, joint)`/`e:set_entity_joint_flag(slot, joint,
value)`, `e:entity_state_word(slot)`/`e:set_entity_state_word(slot, value)`
and `e:entity_hit_state(slot)`/`e:set_entity_hit_state(slot, value)`),
sound and model tint (`e:play_enemy_sound(id)` for `Snd_em`'s group-offset
enemy-bank cue, `e:play_3d_sound(bank, id)` and
`e:play_3d_sound_at(bank, id, x, y, z)` for `Play3DSnd`'s room/character banks,
`e:tint_model(r, g, b, word_a, word_b)`, `e:retarget_tint(r, g, b, word_a,
word_b)` and `e:tint_joint(joint, a, b, rgb)`, whose per-joint colour pipeline
is the documented renderer deferral and records nothing), randomness
(`e:random()`, the
next platform-stream draw the original's direct `rand()` calls consume,
`e:srand(seed)` to reseed that stream before a scripted burst of draws, and
`e:lfsr_step()` for the frame seed's Fibonacci-LFSR step, taps at bits 1 and 9,
which mutates `e.rand_seed` in place and returns the shifted low byte; the
`e.rand_seed` property is also writable for a direct frame-snapshot write) and
flags/effects (`e:flag_bit(bank, sel)`, `e:raise_flag(bank, sel)` for a raw
`Flg_on`, `e:check_special_weapon()`,
`e:raise_scd_flag()`, `e:raise_death_event()`, `e:count_placeholder(behavior)`,
`e:spawn_effect(...)`, `e:spawn_joint_effect(type, depth, joint, yaw)` to
anchor a billboard at a posed joint,
`e:spawn_joint_effect_at(type, depth, joint, x, y, z, yaw)` for one with a
local offset, `e:spawn_world_effect(type, depth, x, y, z, yaw)` for a
billboard at a world point on the identity transform (the shared dead-move
matrix the gore spawns use), `e:spawn_player_effect(type, depth, x, y,
z, yaw)` to anchor one at the player, `e:tag_effect(slot, field, value)`,
`e:player_pos()`, `e:player_radius()`, `e:entity_angle(slot)`, the shared
web-clone subsystem (`e:web_distance()` is the spiders' flat Manhattan
player distance, `e:web_joint_use(joint, index)` is the per-type web-joint
registry scan-and-mark, `e:web_spawn(count)` allocates the spider's
web-thread chain and returns its head, `e:web_update(count)` ticks it)).
`e:separate()`
returns whether the entity touched the player's SCA volume (the value the
attacking monsters park as their touch word) and `e:resolve_collision()`
returns the wall-resolve code (`0` clear, `1` pushed, `2` rolled back, `3` a
floor/step zone crossed) together with the crossed floor step the flying crow
keeps as its ground reference (`e.floor_step`). `e:play_sfx(bank, id)` queues
the raw one-shot cue the computer arms use (identical to `e:play_3d_sound` for
the banks it names), and `e:play_voice(id)` resolves the lab terminal's voice
id to the stage row's positional `voice/<row>_<id>.wav` pack entry (1-based
row, lowercase hex id; a mod override must use this name, not the shipped
`V004_00`-style basename) and queues it for the engine, while
`e:voice_playing` reads and writes the `MSF_VOICE_PLAYING` handshake bit the
arms poll. All
mutable state lives on the Rust entity: **scripts must keep no state of their
own**, so the engine may drop and recreate the whole script VM between any two
updates and behaviour stays identical. Scripts that fail to load, or that
error, are logged once and park that entity id; the other ids keep running.
Models pair with the scripts: `enemy/em{id:02x}{player}.emd` (player 0 Chris,
1 Jill) with the fallback `enemy/em{id:02x}.emd`.

### Boss scratch views

The set-piece monsters re-read the same scratch bytes at their own widths and
under their own names, so a script never aliases a zombie word by accident:

- The monster plant: `e.mp_dist` (i32 Manhattan distance), `e.mp_holdoff`,
  `e.mp_anim_end`, `e.mp_seg`, `e.mp_timer_a`, `e.mp_timer_b`, `e.mp_fidget`,
  `e.mp_alerted`, `e.mp_angle_bk`, `e.mp_swerve`, `e.mp_shadow` and `e.mp_hits`
  (all i16), `e.mp_step_word` (the behaviour/step word at `+0x86`) and
  `e.mp_state_bk` (the state-dword snapshot the damaged state restores). The
  room-shared side-picking register two poison plants alternate through is
  `e.monster_plant_sides`.
- The computer arms: `e.arm_vel_x`/`e.arm_vel_z`, `e.arm_pos_x`/`e.arm_pos_z`,
  `e.arm_y`/`e.arm_vel_y` (i32 16.16 fixed point) and `e.arm_gate` (the typing
  frame gate, i16), plus the `e.target_x`/`e.target_y`/`e.target_z` home
  triple every arm records at init (the left arm's `target_y` is its
  command-7 floor).
- Plant 42: `e.p42_yaw_jitter`/`e.p42_roll_jitter` (i8 idle sway bytes),
  `e.p42_osc_a`/`e.p42_osc_b` (i16 suspend oscillators),
  `e.p42_step` (i16 sweep speed), `e.p42_sweep_dir` (u8),
  `e.p42_grab_joint` (i8), `e.p42_fx_timer_a`/`e.p42_fx_timer_b` (u8),
  `e.p42_dist` (u32 player distance), `e.p42_life` (i16 ambient timer),
  `e.p42_death_counter`/`e.p42_pod_counter` (i8) and `e.p42_ticks` (i16 frame
  timer). `e.plant42_capture_t0` is the shared capture matrix's `t[0]`, the
  sweep's knock-back facing (the setter is `e:set_plant42_capture_t0`). The
  player-side additions are `e.player_zone_flags` (the grabbed bit `0x80`),
  `e.player_weapon` (the equipped item id the recoil's heavy-weapon cancel
  reads) and `e.player_pos_y`.
- Yawn: `e.yawn_turn_accel` (i8), `e.yawn_form`, `e.yawn_bites`,
  `e.yawn_bite_dir` (i8), `e.yawn_recovery`, `e.yawn_path_index`,
  `e.yawn_nearly_dead`, `e.yawn_stuck`/`e.yawn_stuck_cool` (i8),
  `e.yawn_hiss`/`e.yawn_bite_cool` (i8), `e.yawn_scale_ramp`/`e.yawn_speed`/
  `e.yawn_ground_y`/`e.yawn_shrink` (i16), `e.yawn_sparkle` (u16) and
  `e.yawn_state_bk` (the saved state dword a hit reaction restores).

### Player combat surface

The attacking monsters read and write the player through the same `e` handle.
The shared damage rules stay in Rust so every attacker applies them
identically:

- `e:hurt_player(damage, flags)` subtracts `damage` from the player's health
  with the original's 16-bit wrap. `flags` bit `0x01` clamps a killing blow to
  `1`, and with it bit `0x02` resolves a flagged lethal blow to the scripted
  `-1` death (the big wasp's sting). Returns the written health.
- `e:poison_player()` raises the poison status bit (`0x02`) and arms the
  150-frame poison timer.
- `e:grab_player()` is the wasp grab pose: both the player's and the calling
  entity's animation offsets latch to the player's X/Z, and the player enters
  the grabbed animation (attacked flag, animation id 5, animFrameId 7, the
  state-5 window's crawl pin).
- `e:set_player_animation(id, frame)` writes the player's `animationId`/
  `animFrameId` bytes at `+0x84` (the state and ignore bytes, selecting the
  reaction window) and `e:set_player_action(behavior, state)` the behavior and
  action-state bytes (e.g. the sting kill's `200` behavior). The adder's bite
  writes only the behavior byte, so `e.player_action_behavior` and
  `e.player_action_state` expose the two bytes individually as well.
- `e:player_mashing()` is `GetPlayerInputMasked`: true while any d-pad
  direction or face button is held (the crow grab's struggle-shake gate).
- Properties: `e.player_health` (i16, read/write), `e.player_attacked`,
  `e.player_health_status`, `e.player_poison_timer` (read-only),
  `e.player_angle` (the facing a grab latches; writable),
  `e.player_move_speed` (read-only, the knockdown crush's "moving" gate),
  `e.player_animation_id` (read-only, the original's `animationId`, i.e. the
  state byte the perched crow's scatter cue reads) and
  `e.player_animation_frame_id` (read-only, the clip's current frame, the grab
  peck cadence),
  `e.second_playthrough` (read-only, `g_ScenarioFlags` bit `0x7B`, selecting
  the sting's and grab's harder damage rows) and the animation words above.
  The zombie bite writes the player's own state and attack words:
  `e.player_state`/`e.player_anim_frame_id` (read/write, the `animationId`/
  `animFrameId` bytes at `+0x84`), `e.player_attack_anim` (`attackAnim`; its
  setter mirrors the clip id the reaction windows advance), `e.player_attack_direction`
  (the `+0xC4` word) and `e.player_attack_timer`
  (the `+0xE2` word the bite reads back), and `e:snap_grab()` pins the
  player's grab offsets to the zombie's root-motion vertex. The hound's bite
  and maul additionally write the player's `PlayerEntity` flags byte
  (`e.player_flags`, the no-control bits `2`/`6`), the grabbed animation
  offsets on both sides (`e.player_unk_c6`/`e.player_unk_c8`) and the player's
  attack-direction word the chimera's grab meter mirrors
  (`e.player_attack_direction`).

The monster plant's grab drives the shared player-held window: writing
`e.player_state = 6` with `e.player_anim_frame_id = 0x0F` selects the
three-state hold table (entry, struggle, release) that runs in Rust, advances
the grab clip the plant chose (`e.player_attack_anim`) and hands control back
when the drain step flags `e.player_action_state = 2`; the release spins a
back grab (`attackAnim` 2) and clears the grabbed bits of `e.player_flags`.
Plant 42's sweep, release and hold write `e.player_state = 6` with
`e.player_anim_frame_id = 8`, whose entry is the attacked-flag table
(knockdown / grabbed / thrown) dispatched on `e.player_action_behavior`: the
knockdown's fall/slide/get-up chain returns control from its states 4/0x0B,
the thrown slide reads the capture matrix's `t[0]` as its knock-back facing,
and the spit's `e.player_state = 2` / `e.player_action_behavior = 0x64` runs
the generic hit reaction that plays the body clip out and clears
`e.player_attacked`. The same state-6 window also carries the Tyrant's own
table at `e.player_anim_frame_id = 0x0C` (the staggers and the knock-back
above), so a monster that grabs or strikes the player never needs to run the
hold itself.

Yawn's swallow uses the state-7 window instead: writing `e.player_state = 7`
with `e.player_anim_frame_id = 0x0D` selects the swallowed player's own
machine (carried animation, then the head-joint capture hold). The shared
Yawn capture matrix is built with `e:yawn_capture_setup(joint)`, left-multiplied
by a Z rotation with `e:yawn_capture_rotate_z(angle)` and right-multiplied by
the shake yaw with `e:yawn_capture_rotate_y(angle)`, stepped with
`e:yawn_capture_t_step(dx, dy, dz)` and re-applied through the head joint with
`e:yawn_hold_player()` (writes the player position and raises the grabbed zone
bit).

Every other reaction entry the original's animation-function table populates
is wired too, and the clips play from the loaded enemy model's second EDD
chunk (the damage bank, `emdScratchPtr1/2` in the original):

- State 5 (`e.player_state = 5`): the zombie's bite writes `animFrameId 0`
  and `e.player_attack_anim` (0/3/6/9 by attacking direction); the recoil
  machine snaps the player to the grab offsets, plays the hold/loop clips and
  plays the break-free clip when the zombie flags `e.player_action_state = 3`.
  The wasp's grab writes `animFrameId 7` (the crawl pin), which plays damage
  clip 0 out.
- State 6 (`e.player_state = 6`): besides the plant/Plant 42/Tyrant entries,
  the crow's peck-grab writes `animFrameId 5` (three clips, released by the
  crow's own `e.player_action_state = 4`), the chimera's grabhold writes
  `animFrameId 9` (the mash meter runs the maul, the reverse weapon-EMW
  recovery and the low-health death pool) and Neptune's jaw writes
  `animFrameId 0x0B`.
- State 7 (`e.player_state = 7`): the hound's maul writes `animFrameId 2`
  (the recovery clip and spin), the hunter's pounce bite writes `animFrameId
  6` (the kill, the ground pool and the scripted death fade), Plant 42's eat
  writes `animFrameId 8` (body/damage clip phases with the drop and bounce),
  Neptune's devour writes `animFrameId 0x0B` and the Tyrant's impale writes
  `animFrameId 0x0C` (the kill).

### Plant 42's companion arena and custom animator

Plant 42's flower body and root ball are full entity clones outside the enemy
list. `e:plant42_spawn()` builds both from the calling entity (its
`behavior_flags` selects the poison/failed-core set-ups), stores their handles
on `e.plant42_body`/`e.plant42_roots` and in the room-shared
`game.plant42_shared_body` the split-vine records read, and `e:plant42_tick()`
runs their machines (body idle/shrivel/fall, roots idle/pulse/rise) with the
original's draw order; the render pass draws each companion's own mesh object
with its scale. The body's `+0x70` vine pool is the plant's real health pool:
`e:plant42_body_vines()` reads it, `e:set_plant42_body_vines(v)` writes it,
`e:plant42_body_hit()`/`e:plant42_body_health()` read the body's latch and
health, `e:plant42_body_react()`/`e:plant42_body_react_clear()` drive the
damage reaction and `e:plant42_body_wither(sub)`/`e:plant42_body_kill()`
drive the withering/death writes. `e:companion_pos(handle)` and
`e:spawn_companion_effect(handle, type, depth, x, y, z, yaw)` address a
companion's transform. `e:plant42_award_kill()` is the last-vine payoff
(`SCENARIO_FLAG_PLANT42_DEAD`, Jill's message bit, the enemy-list wither).

The plant does not use the shared `Joint_move` clock: `e:plant42_advance(
reverse, blend)` is its own animator, which always poses the skeleton,
sub-frame blends once the blend counter has run out, consumes the counter
before use and only advances the frame when the hold expires. The capture
matrix is built with `e:plant42_capture_setup(joint)` (the joint-relative
player transform), re-applied through a joint with `e:plant42_hold_player(
joint)` (writes the player position and raises the grabbed zone bit),
overwritten with `e:set_plant42_capture_t(x, y, z)` and oriented from a fixed
3x3 block with `e:plant42_capture_orient(m00..m22)`. `e:apply_matrix_lv(
joint, x, y, z)` rotates a vector by a joint's world matrix, and
`e:vector_normal(x, y, z)` is the shared scale-to-4096 normaliser.

### Yawn's chain and the segment slots

Yawn (ids `0x0d`/`0x12`) is one fifteen-joint model spread over thirteen enemy
slots. `e:yawn_spawn_segments()` builds the twelve body slots from the head's
record (each `behavior_flags = 1`, linked to the joint it mirrors and counted
into `game.enemy_count`), and every slot's `update` starts with
`if e.behavior_flags == 1 then e:yawn_segment_tick() return end`: the whole
segment branch (mirror the joint, room collision, chain drag, damage routing to
the head, switch-zone) lives in Rust, because Lua cannot hold joint pointers or
allocate slots. `e:yawn_set_status_range(first, last, and, or)` is the head's
batched status writer over the enemy-list range (`0` the head, `1..12` the
segments), the three swallow/flee/death loops; `e:yawn_set_shadow_tint(tint)`
recolours the head's and every segment's ground quad.

The head does not use the shared `Joint_move` clock either: `e:yawn_back_off()`
is the init back-off, `e:yawn_standard_worlds(zero_root)` runs the two
init-time hierarchy passes (`zero_root` models the second, where the head
joint's local X/Z are cleared), `e:yawn_pose_init()` lays down the fixed-point
chain and `e:yawn_anim(reverse, blend)` is the chain animator (returns the wrap
flag every caller adds into `action_state`); `e:yawn_post_move(step)` is the
collision and follow-the-leader tail. The scale ramps use
`e:yawn_scale_worlds(first, last, sx, sy, sz)` and
`e:yawn_scale_transform(joint, sx, sy, sz)`. The render pass recomposes the
head's first three joints from their transforms and draws the chain joints from
the animator's worlds, exactly like the original's `EntityComputeJointWorldMatrices`
walk; the twelve segments draw no mesh of their own.

The cross-slot accessors every multi-slot machine shares are
`e:entity_state_word(slot)`/`e:set_entity_state_word(slot, word)`,
`e:entity_status(slot)`/`e:set_entity_status(slot, v)`,
`e:entity_health(slot)`/`e:set_entity_health(slot, v)`,
`e:entity_hit_state(slot)`/`e:set_entity_hit_state(slot, v)`,
`e:entity_pos(slot)`, `e:entity_angle(slot)`/`e:set_entity_angle(slot, v)` and
the per-joint `e:entity_joint_flag(slot, joint)`/
`e:set_entity_joint_flag(slot, joint, v)`. The player's limbs are hidden by
clearing the player entity's joint flag bit 0 the same way. The Tyrant's
scripted impale also needs the victim's animation grab offsets:
`e:add_entity_anim_offset(slot, dx, dz)` adds to the target's `+0xC6`/`+0xC8`
words, and `e:snap_grab_slot(slot)` is `snap_grab` aimed at a non-player
victim.

### The Tyrant's ribbon, ghosts, heart and root motion

The Tyrant (ids `0x0c`/`0x10`, `enemy/em0c.lua`) is the only monster whose
pose feeds back into its movement. `e:tyrant_root_motion(set, apply)` composes
the entity yaw with the walk (`set = 0`) or run (`set = 1`) claw chain from
the pose the clock just applied, subtracts the chain end's previous-frame
world translation, stores the planar result in `e.move_speed_current` and,
with `apply`, slides the entity by the leftover XZ delta. It returns the
stored speed. The script's typo-prone raw scratch views are exposed with
their own names: `e.ty_pad_counter` (the rumble phase counter at `+0x16D`),
`e.ty_hit_mask` (`+0x16E`: which swing connected plus the `0x80`
would-have-killed latch), `e.ty_look_mode` (`+0x16F`), `e.ty_state_bk` (the
`+0x174` state dword the hit reaction restores), `e.ty_flags` (`+0x179`:
shadow, live look-at, connected-sound and hidden bits), `e.ty_repause`
(`+0x17F`), `e.ty_rocket_flag` (`+0x180`, the rocket-launcher latch),
`e.ty_cooldown` (`+0x181`), `e.ty_crowd` (`+0x182`), `e.ty_close` (`+0x183`)
and `e.ty_walk_dist` (the `+0x17C` travelled-distance word the walk steering
reads). `e:tyrant_wander_turn(dist, step, limit)` is the shared wander-turn
core with the Tyrant's control/counter bytes, and `e:resolve_collision()` now
returns `(code, floor_step, travelled_distance)` so the tail can store the
same word the original's `g_tempVar` carried.

The render-only machines live in `enemy/tyrant.rs` and surface as named
calls. The slash ribbon: `e:tyrant_trail_alloc()` reserves the pool at init
and stores the `0x70` tint, `e:tyrant_trail_arm()` runs the `0x8000`-bit
sweep that seeds the nine history slots from the current claw pose with the
`+100`-step far blade and the degenerate slot-8 terminator, and
`e:tyrant_trail_update()` scrolls the history one segment per frame and
counts the tail down inside the message gate. The two claw ghosts:
`e:tyrant_ghosts_tick()` refreshes the unscaled and scaled claw copies and
steps the `e.ty_claw_scale_a`/`e.ty_claw_scale_b` pair by
`e.ty_claw_scale_step` (a signed byte, `-56`), flipping at the 3000/6000
bounds. The five severed limbs: `e:tyrant_limb_launch(i, vx, vy, vz, tx, ty,
tz)` copies limb `i`'s joint world matrix and arms its physics,
`e:tyrant_limb_update(i)` integrates it (X/Z velocity, gravity into Y, the
`-200`-floor bounce with the halved inverted velocity, the tumble rotation)
and `e:tyrant_limb_pos(i)` reads its live world translation for the rocket
death's smoke anchors. The exposed heart is a companion-arena clone:
`e:tyrant_heart_spawn()` clones it (consuming the one clone draw),
`e:tyrant_heart_tick()` runs the beat/scale update and composes it under
joint 1 (before the switch-zone refresh, exactly like the original),
`e:tyrant_heart_drop()` integrates the rocket death's free fall and
`e:tyrant_heart_set_speed(v)` re-arms its velocity. The render pass draws the
ghost, limb and heart mesh objects from the scratch matrices; the ribbon's
quads are computed for determinism but not rasterised (documented
approximation).

The player side of a Tyrant hit runs in the shared state-6 window:
`e:latch_player_attacker(bias)` stores this entity as the attacker (with the
yaw bias the site uses) and writes the player's facing latch,
`e:set_player_attacker()` is the bare store the impale uses, and
`e:snap_grab_slot(slot)` mirrors the grab offsets onto a victim. Writing
`e.player_state = 6` with `e.player_anim_frame_id = 0x0C` selects the Tyrant's
own three-entry table dispatched on `e.player_action_behavior`: the backhand
and swipe staggers (with their claw/player blood sheets) and the knock-back
whose room probe aborts into the wall slam. `e:angle_quadrant(slope)` is the
game's fixed-point slope-to-angle helper the knock-back facing needs, and
`e:store_camera_speed()` records the rocket death's camera-aim vector in the
entity speed words.

The shared weapon-damage pipeline itself is Rust-side, driven by the weapon
runtime in `src/weapons.rs`: the aim/fire/reload machine takes the player's
tick while a weapon is raised (the aim input, with fire mapped to the new
port-only keys), selects the packed `player/w{id:02x}.emw` motion and fires
through `apply_weapon_damage_with`. That entry writes the target monster's
`health`, `hit_state` and `state` (`3` for a killing blow, `2` when it
survives, with `ignore`/`action_behavior`/`action_state` zeroed). A monster
script never applies its own damage; it reacts to state 2/3 on its next
update, exactly like the web and the roots already do.

The base game's scripts live in the engine repository under
`games/re1/enemy/` and are packed by `convert-game`. A mod overrides any one of
them simply by carrying an `enemy/em{id:02x}.lua` (or model) entry: pack
layering is the override mechanism, exactly like any other entry. The scripts
run in the same sandbox as the hooks (base/`string`/`table` only, 16 MiB,
instruction budget) in their own per-session VM.

The scripted characters show the intended per-id pattern: every id has a thin
`em{id:02x}.lua` carrying that character's data (collision radius, SCA record,
corpse flag, story-pose membership) and calling the shared driver in
`enemy/lib/npc.lua`, where the state machine lives once — the state-0 spawn
init, state-1 idle behaviours, state-8 scripted-action handlers and the
state-9 follow/pathfind driver:

```lua
-- enemy/em23.lua
local npc = require("lib/npc")
local data = {
    radius = 372,
    corpse = false,
    idle0_plays = false,
    rebecca = true,
    wesker = false,
    sca = { 372, 0x5FA, 0, -0x5FA, 0 },
}

function update(e)
    npc.update(e, data)
end
```

A pack can override one character's data without touching the shared driver,
or shadow `enemy/lib/npc.lua` wholesale to replace the behaviour for all of
them.

## Determinism

A capture or headless run is deterministic. Lua hooks run in a fixed order on
fixed ticks, and `api:log` writes to stderr only, so a `--capture` BMP with
hooks still matches between runs; a run with no `--mod` and no discovered mods
matches the pre-M15 captures exactly.
