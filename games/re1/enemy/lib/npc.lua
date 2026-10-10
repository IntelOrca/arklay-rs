-- Shared scripted-character (NPC) driver: states 0 and 1, the state-8
-- scripted-action handlers and the state-9 follow/pathfind driver.
--
-- Every character id 0x20..0x2E has its own enemy/em{id:02x}.lua which does
-- `local npc = require("lib/npc")` and calls npc.update(e, data) with its
-- per-id data table. This module holds the shared state machine: the state-0
-- spawn init (including the story-flag pose variants), the state-1 idle
-- behaviours and their blood-spray effect requests, the state-8 scripted
-- walk/turn/weapon behaviours, the state-9 follow/pathfind behaviours, and the
-- common character tail.
--
-- All state lives on the Rust entity; this file keeps none, so the scripting
-- VM may be reset between any two updates. States 0, 1, 8 and 9 are scripted;
-- only the native fallback/oracle remains.

local npc = {}

npc.INIT_BLEND_STEP = 0x400
npc.IDLE_BLEND_STEP = 0x400

-- Wounded Rebecca / variant Wesker pose selection.
npc.REBECCA_WOUNDED_FLAG = 0xC0
npc.WESKER_VARIANT_FLAG = 0x37
npc.REBECCA_WOUNDED_ANIM = 0x33
npc.REBECCA_WOUNDED_FRAME = 0x3D
npc.WESKER_VARIANT_ANIM = 0x30
npc.WESKER_VARIANT_FRAME = 0x6D

-- The lab power room Wesker is deactivated in.
npc.STAGE_LABORATORY = 5
npc.ROOM_POWER = 0x11

-- A signed 16-bit view of a value, for the speed trim and move distance.
local function i16(value)
    value = value & 0xFFFF
    if value >= 0x8000 then
        value = value - 0x10000
    end
    return value
end

-- Apply the collected billboard spawns in order, attached to this entity's
-- own matrix (the original spawns the death blood in the character's space).
local function apply_spawns(e, spawns)
    for i = 1, #spawns do
        local s = spawns[i]
        e:spawn_effect(s[1], s[2], s[3], s[4], s[5], s[6], s[7])
    end
end

-- The flag-gated per-character opening pose.
local function apply_pose_variant(e, data)
    if data.rebecca and e:flag_bit(1, npc.REBECCA_WOUNDED_FLAG) then
        e.animation_id = npc.REBECCA_WOUNDED_ANIM
        e.animation_frame_id = npc.REBECCA_WOUNDED_FRAME
        e.timing_control = 0
    elseif data.wesker and e:flag_bit(0, npc.WESKER_VARIANT_FLAG) then
        e.animation_id = npc.WESKER_VARIANT_ANIM
        e.animation_frame_id = npc.WESKER_VARIANT_FRAME
    end
    if data.wesker and e.stage == npc.STAGE_LABORATORY and e.room == npc.ROOM_POWER then
        e.status_flags = e.status_flags | 2
    end
end

-- State 0: drop into state 1 with the behaviour scratch cleared, zero the
-- rotation X/Z (never the spawned yaw), force health to -1, install the
-- character's collision record and pose the skeleton once.
local function init(e, data)
    e.state = 1
    e.ignore = 0
    e.action_behavior = 0
    e.action_state = 0
    e.pitch = 0
    e.roll = 0
    e.health = -1
    e.sca_radius = data.radius
    e:set_sca(data.sca[1], data.sca[2], data.sca[3], data.sca[4], data.sca[5])
    if data.corpse then
        -- The corpse props restart on animation 0 frame 0.
        e.animation_id = 0
        e.animation_frame_id = 0
        e.timing_control = 0
    end
    apply_pose_variant(e, data)
    e.blend_counter = 0
    e:advance_anim(npc.INIT_BLEND_STEP)
end

-- Behaviour 9/10/13: rewind to animation 0 frame 0 on entry, then keep
-- playing; the driver advances while this returns true.
local function play_anim(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_id = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0
    elseif e.action_state ~= 1 then
        return false
    end
    return true
end

-- Behaviour 0: re-dispatch on the entity id. The corpse props land on
-- play_anim; every living character is a no-op.
local function behavior_by_id(e, data)
    if data.idle0_plays then
        return play_anim(e)
    end
    return false
end

-- Behaviour 1, walking: bleed 15 off the speed for every frame spent, then
-- step backwards along the facing with the pre-checked move.
local function walk_01_step(e)
    local trim = e.animation_frame_id * 0xF
    e.move_speed_current = i16(e.move_speed_current) - i16(trim)
    e:advance_anim(npc.IDLE_BLEND_STEP)
    if e:try_move(0x800, i16(e.move_speed_current)) then
        e:save_pos()
    else
        e.action_state = 2
    end
end

-- Behaviour 1, knocking on the obstacle until the knock clip completes.
local function walk_01_knock(e)
    if e:advance_anim(npc.IDLE_BLEND_STEP) then
        e.action_state = e.action_state + 1
    end
end

-- Behaviour 1: walk forward until the collision probe fires.
local function walk_01(e)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0x35
        e.blend_counter = 0
        e.move_speed_current = 1000
        walk_01_step(e)
    elseif state == 1 then
        walk_01_step(e)
    elseif state == 2 then
        e.action_state = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0x36
        e.blend_counter = 3
        walk_01_knock(e)
    elseif state == 3 then
        walk_01_knock(e)
    end
end

-- Behaviour 2, frame body: spray the first ten frames, advance, and count up
-- to the inert park state on completion.
local function walk_02_step(e, spawns)
    if e.animation_frame_id < 10 then
        spawns[#spawns + 1] = { 0, 0, 0, -0x898, 0, 0, 0 }
    end
    if e:advance_anim(npc.IDLE_BLEND_STEP) then
        e.action_state = e.action_state + 1
    end
end

-- Behaviour 2: the scripted death, two depth-3 blood sheets on entry.
local function walk_02(e, spawns)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0x33
        e.blend_counter = 3
        spawns[#spawns + 1] = { 0, 3, 0, 0, 0, 0, 0 }
        spawns[#spawns + 1] = { 0, 3, 0, 0, 0, 0, 0 }
        walk_02_step(e, spawns)
    elseif state == 1 then
        walk_02_step(e, spawns)
    end
end

-- Behaviour 3, frame body: spray below the body before frame 9 and at the
-- origin past frame 0x5F, advance, and count to the park state.
local function walk_03_step(e, spawns)
    local frame = e.animation_frame_id
    if frame < 9 then
        spawns[#spawns + 1] = { 0, 0, 0, -0x5DC, 0, 0, 0 }
    end
    if frame > 0x5F then
        spawns[#spawns + 1] = { 0, 0, 0, 0, 0, 0, 0 }
    end
    if e:advance_anim(npc.IDLE_BLEND_STEP) then
        e.action_state = e.action_state + 1
    end
end

-- Behaviour 3: the bleeding-out death. Faces the second enemy-list slot on
-- entry; after the clip, clears status bit 1 and runs the 250-tick pool
-- countdown.
local function walk_03(e, spawns)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0x30
        e.blend_counter = 0
        e.action_ticks_counter = 0xFA
        e.hit_state = 0x80
        e.status_flags = e.status_flags | 6
        e.angle = e:entity_angle(2) or 0
        walk_03_step(e, spawns)
    elseif state == 1 then
        walk_03_step(e, spawns)
    elseif state == 2 then
        e.status_flags = e.status_flags & ~2
        e.health = -1
        e.action_ticks_counter = e.action_ticks_counter - 1
        if e.action_ticks_counter == 0 then
            e.action_state = 3
        end
    end
end

-- State 1 dispatch on the action_behavior byte.
local function idle(e, data)
    local spawns = {}
    local advance = false
    local behavior = e.action_behavior
    if behavior == 0 then
        advance = behavior_by_id(e, data)
    elseif behavior == 1 then
        walk_01(e)
    elseif behavior == 2 then
        walk_02(e, spawns)
    elseif behavior == 3 then
        walk_03(e, spawns)
    elseif behavior == 9 or behavior == 10 or behavior == 13 then
        advance = play_anim(e)
    end
    apply_spawns(e, spawns)
    if advance then
        e:advance_anim(npc.IDLE_BLEND_STEP)
    end
end

-- State 8: the scripted-action handlers (the native `scd` driver's port).
--
-- `action_behavior` indexes the handlers 0..10; 8 is the weapon-fire handler,
-- whose three 14-row effect tables are keyed by `behavior_flags - 2`. An
-- out-of-range behaviour (>= 11) is the original's NULL table slot: it is
-- counted in the placeholder map and dispatches nothing. `npc.update` runs
-- the handler, then runs it a second time when entity flag bit 1 is set,
-- re-reading the behaviour byte. A handler that completes raises
-- `scd_anim_param` in the system bank; the `act_anim_seq` collision flag bit 7
-- keeps the walk behaviours running on arrival.

-- The three fire tables, one 14-row record per weapon (`behavior_flags - 2`).
-- A row is `{ frame, effect_type, depth, x, y, z }`; frame 0x63 never fires.
-- Muzzle-flash table, spawned in the weapon hand's space.
local FIRE_FX_MUZZLE = {
    { 0x01, 0x11, 0x00,  110,  540,   0 },
    { 0x01, 0x11, 0x01,  640, 1110,   0 },
    { 0x01, 0x11, 0x02,  160,  610,   0 },
    { 0x01, 0x11, 0x0A,  160,  610,   0 },
    { 0x00, 0x00, 0x00,    0,    0,   0 },
    { 0x02, 0x08, 0x07,  400,  660,   0 },
    { 0x02, 0x08, 0x07,  400,  660,   0 },
    { 0x02, 0x08, 0x07,  400,  660,   0 },
    { 0x01, 0x0B, 0x09, -190, 1020,  90 },
    { 0x01, 0x0B, 0x09, -190, 1020, -60 },
    { 0x01, 0x0B, 0x09,  -60, 1040,  90 },
    { 0x01, 0x0B, 0x09,  -60, 1040, -60 },
    { 0x01, 0x11, 0x00,  110,  540,   0 },
    { 0x01, 0x11, 0x00,  600, 1370,   0 },
}

-- Ejected-shell/smoke table, spawned in the character's own matrix.
local FIRE_FX_SHELL = {
    { 0x03, 0x05, 0x00,  370, -2870, -220 },
    { 0x19, 0x05, 0x09,  360, -2050, -440 },
    { 0x63, 0x00, 0x00,    0,     0,    0 },
    { 0x63, 0x00, 0x00,    0,     0,    0 },
    { 0x00, 0x00, 0x00,    0,     0,    0 },
    { 0x63, 0x00, 0x00,    0,     0,    0 },
    { 0x63, 0x00, 0x00,    0,     0,    0 },
    { 0x63, 0x00, 0x00,    0,     0,    0 },
    { 0x02, 0x09, 0x0B, 1400, -2800, -300 },
    { 0x00, 0x00, 0x00,    0,     0,    0 },
    { 0x00, 0x00, 0x00,    0,     0,    0 },
    { 0x00, 0x00, 0x00,    0,     0,    0 },
    { 0x03, 0x05, 0x00,  250, -1900, -250 },
    { 0x03, 0x05, 0x00,  250, -1900, -250 },
}

-- Secondary-flash table, spawned in the weapon hand's space.
local FIRE_FX_FLASH2 = {
    { 0x02, 0x09, 0x0B,  110,   500,   0 },
    { 0x02, 0x09, 0x0B,  640,  1060,   0 },
    { 0x02, 0x09, 0x0B,  160,   610,   0 },
    { 0x02, 0x09, 0x0B,  160,   610,   0 },
    { 0x00, 0x00, 0x00,    0,     0,   0 },
    { 0x02, 0x09, 0x0B,  640,  1060,   0 },
    { 0x02, 0x09, 0x0B,  640,  1060,   0 },
    { 0x02, 0x09, 0x0B,  640,  1060,   0 },
    { 0x02, 0x08, 0x02,  430,  -830,  90 },
    { 0x02, 0x08, 0x02,  430,  -830, -60 },
    { 0x02, 0x08, 0x02,  570,  -810,  90 },
    { 0x02, 0x08, 0x02,  570,  -810, -60 },
    { 0x02, 0x09, 0x0B,  110,   500,   0 },
    { 0x02, 0x09, 0x0B,  640,  1500,   0 },
}

-- The flamethrower's spray offset (weapon-hand space) and cue interval.
local FLAME_OFFSET = { 0x21C, 0x4EC, 0 }
local FLAME_CUE_TICKS = 0x0F

-- Raise the completion flag and clear the behaviour unless collision bit 7
-- keeps it running (`finish_walk`).
local function scd_finish_walk(e)
    e:raise_scd_flag()
    if e.collision_flags & 0x80 == 0 then
        e.action_behavior = 0
        e.action_state = 0
    end
end

-- The turn phase of behaviours 2/3: step toward the scripted target, advance
-- the clip and drop into the walk phase once aligned within 0x16A.
local function scd_turn(e)
    local turn = e:turn_toward_target(e.unk_c6, e.unk_c8, e.scd_timer)
    e.angle = (e.angle + turn) & 0xFFFF
    e:advance_anim(0x200, e.flags & 1 ~= 0)
    if e:turn_toward_target(e.unk_c6, e.unk_c8, 0x16A) == 0 then
        e.action_state = 2
    end
end

-- Behaviour 0: play the scripted clip and nothing else; the script ends it
-- with an explicit opcode.
local function scd_handler_00(e)
    if e.action_state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0
        e.action_state = 1
        e.blend_counter = 7
    elseif e.action_state ~= 1 then
        return
    end
    e:advance_anim(0x200, e.flags & 1 ~= 0)
end

-- Behaviour 1: plain playback. Flag 0x80 holds every frame for an extra tick;
-- flag 0x10 loops back to state 0 instead of parking at state 2.
local function scd_handler_01(e)
    local state = e.action_state
    if state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.action_state = 1
        e.blend_counter = 7
        e.move_speed_current = 0
        if e.flags & 0x20 ~= 0 then
            e.blend_counter = 0
        end
        e.action_ticks_counter = 1
    elseif state ~= 1 then
        if state ~= 2 then
            return
        end
        e:raise_scd_flag()
        e.scd_timer = 0
        if e.flags & 0x10 == 0 then
            return
        end
        e.action_state = 0
        return
    end

    if e.flags & 0x80 ~= 0 then
        local previous = e.action_ticks_counter
        e.action_ticks_counter = previous - 1
        if previous == 0 then
            e.action_ticks_counter = 1
            return
        end
    end

    if e:advance_anim(0x200, e.flags & 1 ~= 0) then
        e.action_state = 2
        e.angle = (e.angle + e.scd_timer) & 0xFFFF
    end
end

-- Behaviour 2 walk phase: step at the 0x5D pace with a footstep on frames 8
-- and 0x16, finishing inside 150 units.
local function scd_02_walk(e)
    local frame = e.animation_frame_id
    if frame == 8 or frame == 0x16 then
        e:footstep(0)
    end
    e:apply_walk_speed(0x5D)
    e:rotate_toward_target(e.unk_c6, e.unk_c8, e.scd_timer)
    e:advance_anim(0x200, e.flags & 1 ~= 0)
    e:try_move(0, i16(e.move_speed_current))
    if e:xz_distance_to(e.unk_c6, e.unk_c8) < 0x96 then
        scd_finish_walk(e)
    end
end

-- Behaviour 2: turn in place, then walk to the scripted target.
local function scd_handler_02(e)
    local state = e.action_state
    if state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 7
        e.action_state = 1
        e.blend_counter = 7
        scd_turn(e)
    elseif state == 1 then
        scd_turn(e)
    elseif state == 2 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 7
        e.action_state = 3
        e.blend_counter = 7
        scd_02_walk(e)
    elseif state == 3 then
        scd_02_walk(e)
    end
end

-- Behaviour 3 run phase: footsteps on frames 0 and 10, finishing inside 250
-- units (collision bit 7 finishes immediately instead of decelerating).
local function scd_03_walk(e)
    local frame = e.animation_frame_id
    if frame == 0 or frame == 10 then
        e:footstep(1)
    end
    e:rotate_toward_target(e.unk_c6, e.unk_c8, e.scd_timer)
    e:advance_anim(0x200, e.flags & 1 ~= 0)
    e:try_move(0, i16(e.move_speed_current))
    if e:xz_distance_to(e.unk_c6, e.unk_c8) < 0xFA then
        e.action_state = 4
        if e.collision_flags & 0x80 ~= 0 then
            e.action_state = 3
            e:raise_scd_flag()
        end
    end
end

-- Behaviour 3 deceleration: trim 0x1E off the speed per tick and count four
-- ticks before clearing the behaviour.
local function scd_03_stop(e)
    e:advance_anim(0x200, e.flags & 1 ~= 0)
    e.action_ticks_counter = e.action_ticks_counter + 1
    if i16(e.action_ticks_counter) > 3 then
        e.action_state = 6
    end
    e.move_speed_current = e.move_speed_current - 0x1E
    e:try_move(0, i16(e.move_speed_current))
end

-- Behaviour 3: turn, run to the target, decelerate, then signal.
local function scd_handler_03(e)
    local state = e.action_state
    if state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 8
        e.action_state = 1
        e.blend_counter = 7
        scd_turn(e)
    elseif state == 1 then
        scd_turn(e)
    elseif state == 2 then
        e.move_speed_current = 0xD2
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 8
        e.action_state = 3
        e.blend_counter = 7
        scd_03_walk(e)
    elseif state == 3 then
        scd_03_walk(e)
    elseif state == 4 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0
        e.action_state = 5
        e.blend_counter = 7
        e.action_ticks_counter = 0
        scd_03_stop(e)
    elseif state == 5 then
        scd_03_stop(e)
    elseif state == 6 then
        e.action_behavior = 0
        e.action_state = 0
        e:raise_scd_flag()
    end
end

-- The shared state 0/1 body of the backward walks: flip the facing 180
-- degrees, steer the back onto the target, flip back and move backwards
-- along the heading. Arrival is 100 units.
local function scd_backward_step(e)
    e.angle = (e.angle + 0x800) & 0x0FFF
    e:rotate_toward_target(e.unk_c6, e.unk_c8, e.scd_timer)
    e.angle = e.angle - 0x800
    e:advance_anim(0x200, e.flags & 1 ~= 0)
    e:try_move(0x800, i16(e.move_speed_current))
    if e:xz_distance_to(e.unk_c6, e.unk_c8) < 100 then
        scd_finish_walk(e)
    end
end

-- Behaviour 4: walk backwards at the fast pace (animation 3), footsteps on
-- frames 8 and 0x16.
local function scd_handler_04(e)
    if e.action_state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.action_state = 1
        e.blend_counter = 7
        e.animation_id = 3
    elseif e.action_state ~= 1 then
        return
    end

    local frame = e.animation_frame_id
    if frame == 8 or frame == 0x16 then
        e:footstep(0)
    end
    e.move_speed_current = 0x3C
    -- The original's `frame > 4 || frame < 8` is true for every byte, so the
    -- +4 always applies; kept as written.
    if frame > 4 or frame < 8 then
        e.move_speed_current = e.move_speed_current + 4
    end
    scd_backward_step(e)
end

-- Behaviour 5: walk backwards at the slow pace (animation 2); footsteps fire
-- only while `timing_control` is exactly 2, so a held frame steps once.
local function scd_handler_05(e)
    if e.action_state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.action_state = 1
        e.blend_counter = 7
        e.animation_id = 2
        e.move_speed_current = 0x1D
    elseif e.action_state ~= 1 then
        return
    end

    local frame = e.animation_frame_id
    if (frame == 7 or frame == 0x1B) and e.timing_control == 2 then
        e:footstep(0)
    end
    scd_backward_step(e)
end

-- Behaviour 6: turn in place toward the scripted target, finishing once
-- aligned within 0x28.
local function scd_handler_06(e)
    local state = e.action_state
    if state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 7
        e.action_state = 1
        e.blend_counter = 7
    elseif state ~= 1 then
        if state ~= 2 then
            return
        end
        e.action_behavior = 0
        e.action_state = 0
        e:raise_scd_flag()
        return
    end

    e:rotate_toward_target(e.unk_c6, e.unk_c8, e.scd_timer)
    e:advance_anim(0x200, e.flags & 1 ~= 0)
    if e:turn_toward_target(e.unk_c6, e.unk_c8, 0x28) == 0 then
        e.action_state = 2
    end
end

-- Behaviour 7: play the scripted clip to its end, signal, and add
-- `scd_timer` to the yaw on every tick including after completion.
local function scd_handler_07(e)
    local state = e.action_state
    if state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.action_state = 1
        e.blend_counter = 7
    elseif state ~= 1 then
        if state == 2 then
            e:raise_scd_flag()
        end
        e.angle = (e.angle + e.scd_timer) & 0xFFFF
        return
    end

    if e:advance_anim(0x200, e.flags & 1 ~= 0) then
        e.action_state = e.action_state + 1
    end
    e.angle = (e.angle + e.scd_timer) & 0xFFFF
end

-- Behaviour 8 support: copy the weapon id into one header byte of a spawned
-- slot (the original's `fire_fx_tag`). A refused spawn yields nil, no write.
local function scd_fire_fx_tag(e, child, field, weapon)
    if child then
        e:tag_effect(child, field, weapon)
    end
end

-- Behaviour 8 support: spawn the weapon's muzzle flash, ejected shell and
-- secondary flash for the current frame, in the original's order.
local function scd_fire_fx_spawns(e, weapon)
    if weapon >= 14 then
        -- The original indexes the tables with a raw byte and never bounds
        -- it; a weapon id this high means `behavior_flags` was never set up.
        return
    end
    local frame = e.animation_frame_id
    local row = FIRE_FX_MUZZLE[weapon + 1]

    if frame == row[1] then
        e:spawn_effect(row[2], row[3], row[4], row[5], row[6], 0, 0)
        if weapon == 2 then
            e:spawn_effect(0x11, 0x03, 0x96, 0x17C, 0, 0, 0)
        end
    end

    row = FIRE_FX_SHELL[weapon + 1]
    if frame == row[1] then
        -- Odd-id characters lift the shell by 300, scaled by (1 - weapon) -
        -- negative for weapon >= 2, exactly the original's signed arithmetic.
        local lift = (e.id & 1) * (1 - weapon) * 300
        local yaw = 0x555
        if weapon == 8 then
            yaw = 0
        end
        local child = e:spawn_effect(row[2], row[3], row[4], lift + row[5], row[6], yaw, 0)
        scd_fire_fx_tag(e, child, 3, weapon)
    end

    row = FIRE_FX_FLASH2[weapon + 1]
    if frame == row[1] then
        local child = e:spawn_effect(row[2], row[3], row[4], row[5], row[6], 0, 0)
        scd_fire_fx_tag(e, child, 0, weapon)
    end
end

-- Behaviour 8 support: the shared animation step of states 0/1/3.
local function scd_fire_play_anim(e)
    if e:advance_anim(0x400) then
        e.action_state = e.action_state + 1
    end
end

-- Behaviour 8 state 5: spray a type-0x0C billboard every sixth frame, count
-- the looping sound cue down, advance the clip and sweep the yaw.
local function scd_fire_flame_step(e)
    if e.animation_frame_id % 6 == 0 then
        e:spawn_effect(0x0C, 0, FLAME_OFFSET[1], FLAME_OFFSET[2], FLAME_OFFSET[3], 0, 0)
    end
    local ticks = e.action_ticks_counter
    e.action_ticks_counter = ticks - 1
    if ticks == 0 then
        e.action_ticks_counter = FLAME_CUE_TICKS
        -- The original queues the two flamethrower cues on the room/enemy
        -- bank (`Play3DSnd(2, 0x1E/0x1F, ...)`).
        e:play_3d_sound(2, 0x1E)
        e:play_3d_sound(2, 0x1F)
    end
    e:advance_anim(0x400)
    e.angle = (e.angle + e.scd_timer) & 0xFFFF
end

-- Behaviour 8: the weapon-fire handler. Weapon 3 (flamethrower) skips the
-- shot effects and plays animation 0x17 straight into the flame loop; state 2
-- raises the completion flag a waiting script tests.
local function scd_handler_08(e)
    local state = e.action_state
    local weapon = (e.behavior_flags - 2) & 0xFF
    if state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.action_state = 1
        e.blend_counter = 3
        if weapon == 3 then
            e.action_state = 3
            e.animation_id = 0x17
        end
        if weapon ~= 3 then
            scd_fire_fx_spawns(e, weapon)
        end
        scd_fire_play_anim(e)
    elseif state == 1 then
        scd_fire_fx_spawns(e, weapon)
        scd_fire_play_anim(e)
    elseif state == 2 then
        e:raise_scd_flag()
    elseif state == 3 then
        scd_fire_play_anim(e)
    elseif state == 4 then
        e.action_state = 5
        e.timing_control = 0
        e.animation_id = 0x14
        e.blend_counter = 3
        e.action_ticks_counter = FLAME_CUE_TICKS
        scd_fire_flame_step(e)
    elseif state == 5 then
        scd_fire_flame_step(e)
    end
end

-- Behaviour 9: play the scripted clip in reverse (hard-coded), then clear the
-- behaviour and signal.
local function scd_handler_09(e)
    local state = e.action_state
    if state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.action_state = 1
        e.blend_counter = 3
    elseif state ~= 1 then
        if state ~= 2 then
            return
        end
        e.action_behavior = 0
        e.action_state = 0
        e:raise_scd_flag()
        return
    end

    if e:advance_anim(0x400, true) then
        e.action_state = e.action_state + 1
    end
end

-- Behaviour 10: play the clip, signal, and loop back to the start while
-- `scd_entity_flags` bit 4 is set.
local function scd_handler_10(e)
    local state = e.action_state
    if state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.action_state = 1
        e.blend_counter = 7
        e.move_speed_current = 0
    elseif state ~= 1 then
        if state ~= 2 then
            return
        end
        e:raise_scd_flag()
        if e.flags & 0x10 == 0 then
            return
        end
        e.action_state = 0
        return
    end

    if e:advance_anim(0x200, e.flags & 1 ~= 0) then
        e.action_state = e.action_state + 1
    end
end

-- Dispatch one behaviour. An out-of-range behaviour is the original's NULL
-- table slot and is counted instead of dispatched.
local function scd_run(e, behavior)
    if behavior == 0 then
        scd_handler_00(e)
    elseif behavior == 1 then
        scd_handler_01(e)
    elseif behavior == 2 then
        scd_handler_02(e)
    elseif behavior == 3 then
        scd_handler_03(e)
    elseif behavior == 4 then
        scd_handler_04(e)
    elseif behavior == 5 then
        scd_handler_05(e)
    elseif behavior == 6 then
        scd_handler_06(e)
    elseif behavior == 7 then
        scd_handler_07(e)
    elseif behavior == 8 then
        scd_handler_08(e)
    elseif behavior == 9 then
        scd_handler_09(e)
    elseif behavior == 10 then
        scd_handler_10(e)
    else
        e:count_placeholder(behavior)
    end
end

-- State 8: run the handler, then run it again when entity flag bit 1 is set,
-- re-reading the behaviour byte for the repeat. An out-of-range behaviour is
-- reported before any dispatch.
local function scd(e)
    local behavior = e.action_behavior
    if behavior >= 11 then
        e:count_placeholder(behavior)
        return
    end
    scd_run(e, behavior)
    if e.flags & 2 ~= 0 then
        scd_run(e, e.action_behavior)
    end
end

-- State 9: the follow/pathfind driver (the native `walk::update` port).
--
-- Entry snapshots the player position and the frame seed, marks the character
-- as ignoring the player on the first tick and resets the look-at. Every tick
-- runs the obstacle pathfinder, swaps the behaviour on the distance ring,
-- records the character's zone, runs the selected behaviour and advances the
-- animation with `blend_step` and the reverse bit of `dir_control_flags`. The
-- tail queues the footfall sound, runs the SCA separation pairs (player first,
-- then every other active character) and resolves the result against the room
-- collision, storing the accepted position.
--
-- The original's `position` word is the rollback point, not the live position.
-- The entry snapshot keeps it when it differs from the live position and
-- otherwise pins the entry position as `saved_pos`, so `separate` and
-- `resolve_collision` read exactly the `saved_pos.unwrap_or(pos)` the native
-- driver captured before the tick moved the character.

-- Rust integer division truncates toward zero; Lua's `//` floors, so a
-- non-exact quotient with differing signs is stepped back up. The sandbox
-- carries no `math` library.
local function trunc_div(a, b)
    local quotient = a // b
    if a % b ~= 0 and (a < 0) ~= (b < 0) then
        quotient = quotient + 1
    end
    return quotient
end

-- The original's `SquareRoot0` as a truncating positive integer square root
-- (Newton's method; exact for integer inputs).
local function sqrt0(value)
    if value <= 0 then
        return 0
    end
    local x = value
    local y = (x + 1) // 2
    while y < x do
        x = y
        y = (x + value // x) // 2
    end
    return x
end

-- The follow behaviour distance ring: `(near, far)` thresholds per behaviour.
-- The near test swaps when the player is closer than the first value, the far
-- test when the player is at least the second; a zero threshold is disabled.
local NEAR_THRESHOLDS = { 0x0708, 0x0AF0, 0x1194, 0 }
local FAR_THRESHOLDS = { 0x1194, 0x1964, 0, 0x09C4 }
local NEAR_SWAP = { 3, 0, 1, 3 }
local FAR_SWAP = { 1, 2, 2, 0 }

-- `FUN_00471e90`: swap the follow behaviour when the player crosses a distance
-- threshold. The 16-bit store the original performs clears `action_state`
-- along with `action_behavior`.
local function swap_behavior(e, player_x, player_z)
    local dx = e.pos_x - player_x
    local dz = e.pos_z - player_z
    local dist = sqrt0(dx * dx + dz * dz)
    local behavior = e.action_behavior
    if behavior >= 4 then
        return
    end
    local near = NEAR_THRESHOLDS[behavior + 1]
    if near ~= 0 and dist < near then
        e.action_behavior = NEAR_SWAP[behavior + 1]
        e.action_state = 0
        return
    end
    local far = FAR_THRESHOLDS[behavior + 1]
    if far ~= 0 and dist >= far then
        e.action_behavior = FAR_SWAP[behavior + 1]
        e.action_state = 0
    end
end

-- `npc_walk_choose_heading`: run the zone path from the character to the
-- player and pick the walk heading and waypoint.
--
-- A direct path heads straight at the player. A crossing is extrapolated from
-- the shared edge onto the character-player line and accepted when the
-- corridor test passes and the point is clear of walls; otherwise
-- `crossing_heading` clamps the character's own position into the corridor.
-- The waypoint lands in `player_pos_x`/`player_pos_z`, the path result in
-- `bob_speed` and the heading in `reaction_timer`; the heading is returned.
local function choose_heading(e, player_x, player_z)
    local entity_x = e.pos_x
    local entity_z = e.pos_z
    local kind, from, next, cross_x, cross_z = e:zone_path_find(player_x, player_z)
    local heading
    if kind == 0 then
        e.bob_speed = from | 0x10
        e.player_pos_x = player_x
        e.player_pos_z = player_z
        heading = e:angle_to(player_x, player_z)
    elseif kind == 1 then
        e.bob_speed = next
        local edge_flag = e:zone_shared_edge(from, next)
        local pos_x = 0
        local pos_z = 0
        local degenerate = false
        if edge_flag == 0 then
            local denominator = player_x - entity_x
            if denominator == 0 then
                degenerate = true
            else
                pos_x = cross_x
                pos_z = entity_z
                    + trunc_div((cross_x - entity_x) * (player_z - entity_z), denominator)
            end
        else
            local denominator = player_z - entity_z
            if denominator == 0 then
                degenerate = true
            else
                pos_z = cross_z
                pos_x = entity_x
                    + trunc_div((cross_z - entity_z) * (player_x - entity_x), denominator)
            end
        end

        if not degenerate
            and e:corridor_open(edge_flag, pos_x, pos_z, from, next)
            and not e:position_blocked(pos_x, e.pos_y, pos_z)
        then
            e.player_pos_x = pos_x
            e.player_pos_z = pos_z
            heading = e:angle_to(pos_x, pos_z)
        else
            local waypoint_x, _, waypoint_z, waypoint_heading = e:crossing_heading(from, next)
            e.player_pos_x = waypoint_x
            e.player_pos_z = waypoint_z
            heading = waypoint_heading
        end
    else
        e.bob_speed = 0xFF
        e.player_pos_x = player_x
        e.player_pos_z = player_z
        heading = e:angle_to(player_x, player_z)
    end
    e.reaction_timer = heading
    return heading
end

-- Behaviour 0's walk state: switch to animation 6 and the parity wander once
-- the short walk's animation completes.
local function behavior_00_walk(e)
    if e.attacking_direction & 1 ~= 0 then
        e.action_state = 3
        e.animation_id = 6
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        -- `wander_lookat(entity, !(id & 1), seed)`: the u8 bitwise not.
        e:lookat_wander(255 - (e.id & 1))
    end
end

-- Behaviour 0: pace in place. A random wait countdown (`tex_bank`), then a
-- short walk on animation 5 until the animation reports done, then animation
-- 6 while the wander look-at runs.
local function behavior_00(e, seed)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        e.animation_id = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.tex_bank = (seed & 0x38) + 0x40
        e:lookat_reset()
    elseif state == 1 then
        e.tex_bank = e.tex_bank - 1
        if e.tex_bank ~= 0 then
            e:lookat_wander(0)
            return
        end
        e.action_state = 2
        e.animation_id = 5
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e:lookat_reset()
        behavior_00_walk(e)
    elseif state == 2 then
        behavior_00_walk(e)
    elseif state == 3 then
        e:lookat_wander(255 - (e.id & 1))
    end
end

-- Behaviour 1: walk to the player on animation 7 at speed 0x5D. While the
-- heading is within +/-0x180 of the yaw the character walks; otherwise it only
-- turns, at half the step.
local function behavior_01(e, player_x, player_z)
    local heading = choose_heading(e, player_x, player_z)
    if e.action_state == 0 then
        e.action_state = 1
        if e.animation_id ~= 7 then
            e.animation_id = 7
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 7
        end
    end

    if e:heading_within(heading, 0x180) then
        e:turn_toward_heading(heading, 0x30)
        e:apply_walk_speed(0x5D)
        e:move(0, i16(e.move_speed_current))
        e:lookat_wander(0)
        return
    end
    e:turn_toward_heading(heading, 0x28)
    e:lookat_wander(0)
end

-- Behaviour 2: fast walk to the player. On animation 8 at speed 0xD2 while
-- the heading is within +/-0x180; when it is far off the character falls back
-- to animation 7, and a second +/-0x200 test skips the movement entirely.
local function behavior_02(e, player_x, player_z)
    local heading = choose_heading(e, player_x, player_z)
    local move_speed
    if e:heading_within(heading, 0x180) then
        if e.animation_id ~= 8 then
            e.animation_id = 8
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 7
        end
        e:turn_toward_heading(heading, 0x60)
        move_speed = 0xD2
    else
        if e.animation_id ~= 7 then
            e.animation_id = 7
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 7
        end
        e:turn_toward_heading(heading, 0x30)
        if not e:heading_within(heading, 0x200) then
            e:lookat_target(e.player_pos_x, e.player_pos_z)
            return
        end
        e:apply_walk_speed(0x5D)
        move_speed = i16(e.move_speed_current)
    end
    e:move(0, move_speed)
    e:lookat_target(e.player_pos_x, e.player_pos_z)
end

-- Behaviour 3: face the player and keep the distance. While the player is
-- within +/-0x200 of straight ahead the character walks backward on animation
-- 3 at -0x3C with the look-at locked on the player; otherwise it turns 180
-- degrees and walks forward on animation 7 with the look-at reset.
local function behavior_03(e, player_x, player_z)
    local heading = e:angle_to(player_x, player_z)
    e.reaction_timer = heading
    if e:heading_within(heading, 0x200) then
        if e.animation_id ~= 3 then
            e.animation_id = 3
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 7
        end
        e:turn_toward_heading(heading, 0x30)
        e:move(0, -0x3C)
        e:lookat_target(player_x, player_z)
        return
    end

    if e.animation_id ~= 7 then
        e.animation_id = 7
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
    end
    heading = (heading + 0x800) & 0x0FFF
    e.reaction_timer = heading
    e:turn_toward_heading(heading, 0x30)
    e:apply_walk_speed(0x5D)
    e:move(0, i16(e.move_speed_current))
    e:lookat_reset()
end

-- `npc_walk_footstep_sound`: the state-9 footfall frames. Animations 3 and 7
-- step on frames 8 and 0x16 with sound 0; animation 8 on frames 0 and 0xA
-- with sound 1.
local function walk_footstep_sound(e)
    local anim = e.animation_id
    local frame = e.animation_frame_id
    local sound_type
    if (anim == 3 or anim == 7) and (frame == 8 or frame == 0x16) then
        sound_type = 0
    elseif anim == 8 and (frame == 0 or frame == 0x0A) then
        sound_type = 1
    else
        return
    end
    e:footstep(sound_type)
end

-- One state-9 tick: the follow/pathfind driver.
local function follow(e)
    local player_x, _, player_z = e:player_pos()
    local seed = e.rand_seed

    if e.ignore == 0 then
        e.ignore = 1
        e:lookat_reset()
    end

    -- Pin the original's `position` word to the entry snapshot when it is
    -- unset (or already names the live position); the tail methods read
    -- `saved_pos.unwrap_or(pos)` like the native driver's captured `prev_pos`.
    if e.prev_pos_x == e.pos_x and e.prev_pos_y == e.pos_y and e.prev_pos_z == e.pos_z then
        e:save_pos()
    end

    e:pathfind_update(player_x, player_z)
    swap_behavior(e, player_x, player_z)
    e:update_walk_zone()

    local behavior = e.action_behavior
    if behavior == 0 then
        behavior_00(e, seed)
    elseif behavior == 1 then
        behavior_01(e, player_x, player_z)
    elseif behavior == 2 then
        behavior_02(e, player_x, player_z)
    elseif behavior == 3 then
        behavior_03(e, player_x, player_z)
    end

    local reverse = e.dir_control_flags & 1
    local step = e:blend_step()
    local done = e:advance_anim(step, reverse ~= 0)
    e.attacking_direction = done and 1 or 0
    walk_footstep_sound(e)

    e:separate()
    e:resolve_collision()
end

-- The per-id entry point. This handles the four ported states and then runs
-- the shared character tail: refresh and slew the look-at, recompute the
-- camera-switch-zone bit and keep the entity active.
function npc.update(e, data)
    if e.state == 0 then
        init(e, data)
    elseif e.state == 1 then
        idle(e, data)
    elseif e.state == 8 then
        scd(e)
    elseif e.state == 9 then
        follow(e)
    end
    e:update_look_at()
    e:update_switch_zone()
    e.active = true
end

return npc
