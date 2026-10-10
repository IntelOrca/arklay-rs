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

Hooks cannot spawn enemies: the hook API has no such operation by
construction. Entity behaviour is the separate enemy-script surface below.

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
`e:try_move(...)`, `e:apply_walk_speed(speed)`,
`e:turn_toward_target(x, z, step)`, `e:rotate_toward_target(x, z, step)`,
`e:turn_toward_heading(heading, step)`,
`e:heading_within(heading, half)`, `e:xz_distance_to(x, z)`, `e:angle_to(x, z)`,
`e:check_visual_range(range)`, `e:check_alert_range(range)`,
`e:pathfind_keep(result)`, `e:wasp_path_keep(result)` (the same keep into the
wasp's `+0x16E` byte), `e:zone_path_update(x, z)` (the zombie distance
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
id in the stage's voice table and queues it for the engine, while
`e.voice_playing` reads and writes the `MSF_VOICE_PLAYING` handshake bit the
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
hooks still matches between runs; a run with no `--mod` and no sibling `mods/`
matches the pre-M15 captures exactly.
