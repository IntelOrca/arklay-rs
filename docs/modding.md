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
| `engine` | Optional note about the target engine build. |
| `lua` | Ordered hook paths; empty means discover `lua/**.lua`. |

A base pack without a manifest is tolerated as `id = <file stem>`,
`kind = "base"` with a warning; a mod without one is rejected by name.

## Layers at runtime

```sh
# Explicit layers, repeatable; later load_order wins a shared entry
arklay re1.akpak --mod mods/balance.akpak --mod mods/demo.akpak --room 100

# Sibling mods/ discovery: every sibling mods/*.akpak is applied
arklay re1.akpak --room 100

# A clean vanilla run: no discovery, no --mod
arklay re1.akpak --no-mods --room 100
```

Mods sort by `(load_order, id)`; within one layer the existing ASCII
case-insensitive lookup wins. Duplicate mod ids and duplicate layer paths are
rejected, and a mod may only name a `base` matching the base pack's id. With no
layers at all the engine takes exactly the single-pack path, so a run without
`--mod` (and with no sibling `mods/`) is byte-identical to earlier releases.

`arklay list`, `arklay extract` and `arklay pack info` accept `--mod`
too; `pack info` prints the manifest, the applied layers and the merged entry
table.

## Mod source directories

`arklay mod build <dir> --out <pack.akpak>` builds a mod from a directory:
`manifest.toml` is required and packed, every other file maps to a pack entry
equal to its directory-relative path, and a `scd/<stem>.s` source is assembled
into `scd/<stem>.scd` (the `.s` itself is not packed). `--base <base.akpak>`
checks the declared base id and reports override paths the base does not hold
as a warning, never an error.

The checked-in `mods/demo/` is the reference mod:

```sh
arklay mod build mods/demo --out demo.akpak
arklay re1.akpak --mod demo.akpak --room 100 --capture demo.bmp
```

It carries a manifest, a whole-room script override for RDT 1000, a Lua hook
and a generated 320x240 background, but no game assets.

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
lowercased-path order. Each chunk is compiled and run once when the session
opens; the globals `on_room_load`, `on_tick` and `on_message` are then called:

```lua
local granted = false

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

1. `on_room_load(api)` once a room's state exists (RDT, init script and room
   edits applied) and before its first tick. It runs again on every room
   entry, including a door transition; the Lua globals themselves persist for
   the session (`granted` above is not reset).
2. `on_tick(api, tick)` after every fixed 30 Hz room tick, before that tick's
   audio dispatch. `tick` is `game.frame`, the number of completed room ticks.
3. `on_message(api, id)` when the tick raised a message; the returned id
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
| `api:give_item(item)` | Add one item through the normal merge rules. |
| `api:item_remove(item)` | Clear the whole inventory slot holding the item. |
| `api:kill_event(slot)` | Deactivate a running event slot. |
| `api:message(id)` | Request a room/global message. |
| `api:log(text)` | Write one `[lua]` line to stderr. |

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
session always continues. A pack with no `lua/` entries loads no VM, and a
`--no-default-features` build has no Lua runtime at all: every hook compiles
to a no-op and runs stay byte-identical.

Hooks cannot spawn enemies or otherwise leave the no-enemy scope of the
engine: the API has no such operation by construction.

## Determinism

A capture or headless run is deterministic. Lua hooks run in a fixed order on
fixed ticks, and `api:log` writes to stderr only, so a `--capture` BMP with
hooks still matches between runs; a run with no `--mod` and no sibling `mods/`
matches the pre-M15 captures exactly.
