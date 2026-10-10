-- Chimera, entity id 0x09 (model em1009).
--
-- The humanoid-ape mutant that hangs from ceilings and drops on the player.
--
-- Three stacked layers:
--   state  0 init / 1 the AI driver / 2 the hit reaction / 3 death. There is
--          no state 4: the death dissolve raises the room's death event and
--          the room script despawns the corpse before a next frame could
--          index past the table, exactly like the shipped build.
--   ignore 0 = the variant brain plus the behaviour dispatch run; 1 = a
--          behaviour or hit layer owns the chimera; anything else is frozen.
--   action_behavior is shared by the AI driver and the hit/death layers
--          (0 idle, 1 walk, 2 turn, 3 swipe, 4 grab, 5 toggle, 6 drop,
--          7 swoop, 8 spit, 9 flee, 10 turn-back, 11 quick toggle, 12 claw,
--          13 hit-fall, 14/15/16 the three brains, which the behaviour table
--          and the variant brain table share).
--
-- behavior_flags is the variant selector: 0/1 floor chimeras, 2 the
-- ceiling-hanging one, 3 the scripted hard mode (init rerolls it to 0/1 and
-- latches the hard word). Bit 0x80 freezes the whole update, bit 2 forces the
-- drop death.
--
-- The ceiling pose is `pos_y = -6008` with `roll = 0x800` (upside down), the
-- ceiling brain runs its whole decision body with the yaw flipped 180
-- degrees, and the drop/swoop/knockdown/death drivers read the dive arc table
-- as negated Y heights.
--
-- All mutable state lives on the Rust entity; this script keeps none, so the
-- scripting VM may be reset between any two updates.

-- ---------------------------------------------------------------------------
-- Constants
-- ---------------------------------------------------------------------------

-- The hit/death behaviour roll, indexed by hit_state & 7.
local SEED_TBL = { 0, 2, 0, 2, 0, 4, 4, 0 }

-- The hit-fall animation per variant (the third byte is the unused variant 2).
local DEATH_ANIM = { 6, 6, 0 }

-- The ceiling dive arc as negated Y heights: the swoop reads forward, the
-- drop and the knockdown/death drivers read backward (index 32 - frame).
local ARC = {
    1540, 1225, 1040, 1290, 1625, 2100, 2500, 2900,
    3200, 3500, 3800, 4100, 4300, 4500, 4600, 4700,
    4700, 4700, 4700, 4700, 4700, 4700, 4700, 4700,
    4700,
    4800, 4900, 5000, 5068, 5068, 5068, 5068, 5068,
}

-- Horizontal drift read alongside the arc.
local SWOOP_SPEED = {
    0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0,
    0, 0,
    150, 150, 150, 150, 150, 150, 150, 150, 150, 150,
    0, 0, 0, 0, 0,
}
local DROP_SPEED = {
    0, 0, 0, 0, 0, 0,
    150, 150, 150, 150, 150, 150, 150, 150, 150, 150,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
}

-- ---------------------------------------------------------------------------
-- Helpers
-- ---------------------------------------------------------------------------

-- Forward declarations for the dispatch tables and the handlers they name
-- before their definitions.
local BEHAVIORS
local BRAINS
local STATES
local brain_a
local brain_b
local brain_c
local hit_stagger
local hit_knockdown
local death_dissolve
local death_drop_dissolve

-- The 16-bit signed view of a counter the original reads through a `short`.
local function s16(value)
    value = value & 0xFFFF
    if value >= 0x8000 then
        return value - 0x10000
    end
    return value
end

-- Arithmetic shifts, floor division (Lua's `>>` is logical).
local function sar16(value, bits)
    return s16(value) // (1 << bits)
end

local function sar32(value, bits)
    return value // (1 << bits)
end

-- The +0x86 word store: behavior low byte, action state high byte.
local function set_beh(e, value)
    e.action_behavior = value & 0xFF
    e.action_state = (value >> 8) & 0xFF
end

-- The +0x84 word store: state low byte, ignore high byte.
local function set_state(e, value)
    e.state = value & 0xFF
    e.ignore = (value >> 8) & 0xFF
end

-- The pathfinder latch word's low bit.
local function path_latch(e)
    return e.writhe_amplitude & 1
end

-- The scripted hard-mode latch.
local function hard(e)
    return e.sink_wobble ~= 0
end

local function player_facing(e)
    return e:player_facing_entity() and 1 or 0
end

local function second_playthrough(e)
    return e.second_playthrough and true or false
end

-- One torso-joint impact billboard (joint 3's posed matrix, zero offset).
local function torso_impact(e)
    e:spawn_joint_effect(0, 0, 3, 0)
end

-- The acid/blood burst the wounded stagger and every death plays: two
-- billboards on the torso joint, then a fan of five type-0x1d billboards
-- around the entity matrix, yawed away from the player's facing.
local function acid_burst(e)
    e:spawn_joint_effect(0, 0, 1, 0)
    e:spawn_joint_effect(3, 0, 1, 0)
    local yaw = 0
    for depth = 4, 1, -1 do
        yaw = (depth + 5) * 0x100 - e.angle + e.player_angle
        e:spawn_effect(0x1D, depth, 100, -0x5DC, 0, yaw, 0)
    end
end

-- ---------------------------------------------------------------------------
-- State 0 - spawn
-- ---------------------------------------------------------------------------

local function state_init(e)
    set_state(e, 1)
    set_beh(e, 0)
    e.action_ticks_counter = 0
    e.death_timer = 0
    e.hit_state = 0
    e.sink_wobble = 0

    if e.behavior_flags == 3 then
        e.sink_wobble = 1
        e.behavior_flags = e:random() & 1
    end
    if e.behavior_flags < 2 then
        e.pitch = 0
        e.roll = 0
        e.pos_y = 0
    end
    if e.behavior_flags > 1 then
        e.pitch = 0
        e.roll = 0x800
        e.pos_y = -6008
        e.behavior_flags = 2
    end

    e:reset_joints()

    e:set_shadow_offset(0, 0, 0)
    e.shadow_tint = 0x808080
    e.shadow_half_x = 0
    e.shadow_half_z = 0

    -- One discarded draw, then three d3s on top of 0x50: 80..122 health.
    e:random()
    local r1 = e:random()
    local r2 = e:random()
    local r3 = e:random()
    e.health = 2 * ((r2 & 7) + (r1 & 7) + (r3 & 7)) + 0x50

    e.animation_frame_id = 0
    e.timing_control = 0
    e.animation_id = e.behavior_flags
    e.joint_scale = 0
    e.status_flags = e.status_flags & 0x1F
    e:set_sca(500, 180, 0, -180, 0)

    e.c_repause = 0
    e.c_fade_freeze = 0
    e.c_wall_frames = 0
    e.c_far_latch = 0

    e:advance_anim(0x40)
end

-- ---------------------------------------------------------------------------
-- State 1 - the AI driver
-- ---------------------------------------------------------------------------

-- The per-variant pre-AI roll: the Manhattan distance to the player, then the
-- brain through the table the behaviour slots 14-16 share.
local function variant_ai(e)
    local px, _, pz = e:player_pos()
    local dx = px - e.pos_x
    local dz = pz - e.pos_z
    local dist = (dz < 0 and -dz or dz) + (dx < 0 and -dx or dx)
    local brain = BRAINS[e.behavior_flags + 1]
    if brain == nil then
        e:count_placeholder(e.behavior_flags)
        return
    end
    brain(e, dist)
end

-- The behaviour prologue: latch the pathfinder result's bit 0, then dispatch.
local function behavior_dispatch(e)
    local px, _, pz = e:player_pos()
    local path = e:pathfind_update(px, pz)
    e:wasp_path_keep(path)
    local handler = BEHAVIORS[e.action_behavior + 1]
    if handler == nil then
        e:count_placeholder(e.action_behavior)
        return
    end
    handler(e)
end

local function state_run(e)
    if e.behavior_flags & 0x80 ~= 0 then
        return
    end

    if e.ignore == 0 then
        variant_ai(e)
        behavior_dispatch(e)
    elseif e.ignore == 1 then
        behavior_dispatch(e)
    end

    e.status_flags = (e.status_flags & 0x1F) | 0x40
    e:check_visual_range(4000)

    if e.behavior_flags ~= 0 and e.action_behavior == 0 then
        e.status_flags = e.status_flags & 0x1F
        e:check_visual_range(4000)
    end
    if e.behavior_flags & 2 ~= 0 then
        e.status_flags = e.status_flags & 0x1F
        e:check_alert_range(5000)
    end

    if e.c_repause ~= 0 then
        e.c_repause = e.c_repause - 1
    end

    if e.reaction_timer == 0 then
        e.c_wall_frames = 0
        return
    end
    e.c_wall_frames = e.c_wall_frames + 1
end

-- ---------------------------------------------------------------------------
-- The three brains (also behaviour slots 14/15/16)
-- ---------------------------------------------------------------------------

-- Variant 0: a confirmed path hands to the walk; losing the player or getting
-- wedged drops to the quick toggle; a player aiming from range rolls the swoop
-- or the claw; hard mode with a pending repause toggles variants.
brain_a = function(e, dist)
    local saved_state = e.action_state
    local prev = e.action_behavior

    if path_latch(e) ~= 0 then
        e.ignore = 0
        e.action_behavior = 1
        if prev ~= 1 then
            e.animation_id = 9
            e.move_speed_current = 0xB4
            e.action_state = 0
        end
    end

    local px, _, pz = e:player_pos()
    local in_range = e:angular_view_and_distance(0x200, 0x5DC)
    if in_range == 0 or e:line_of_sight() ~= 0 or e.player_attacked ~= 0 then
        if (path_latch(e) == 0 and dist < 6000) or e.c_wall_frames > 0x14 then
            e.ignore = 1
            set_beh(e, 0xB)
            e.c_wall_frames = 0
        end
        if e.player_action_behavior == 0x14 and dist > 2000
            and not e:player_facing_entity() and e.player_action_state == 0 then
            e.ignore = 1
            set_beh(e, (e:random() & 1) * 4 + 7)
            if hard(e) then
                set_beh(e, 0xB)
            end
        end
        if e.player_attacked & 0x80 ~= 0 then
            e.ignore = 1
            set_beh(e, 0xD)
        end
        return
    end

    e.ignore = 1
    set_beh(e, 7)
    if not hard(e) then
        if not e:player_facing_entity() then
            return
        end
        set_beh(e, 0xC)
        return
    end
    set_beh(e, 0xC)
    if e.c_repause == 0 then
        return
    end
    e.action_behavior = 5
    e.action_state = saved_state
    if prev == 5 then
        return
    end
    e.action_state = 0
end

-- Variant 1: the walk hand-over keeps the run/step pair, the attack roll is a
-- coin flip gated on range + line of sight, and losing the player hands over
-- to the turn behaviour with a distance-dependent step.
brain_b = function(e, dist)
    local prev = e.action_behavior
    local saved_state = e.action_state

    if path_latch(e) ~= 0 then
        e.ignore = 0
        e.action_behavior = 1
        if prev ~= 1 then
            e.action_state = 0
            if dist > 5000 then
                e.c_far_latch = 1
            end
            e.animation_id = 3
            e.move_speed_current = 100
            if e.c_far_latch ~= 0 then
                e.animation_id = 2
                e.move_speed_current = 200
            end
        end
    end

    local in_range = e:angular_view_and_distance(0x200, 0x5DC)
    if in_range ~= 0 and e:line_of_sight() == 0
        and e.player_attacked == 0 and (e:random() & 1) ~= 0 then
        e.ignore = 1
        set_beh(e, 8)
        if e:player_facing_entity() then
            set_beh(e, 0xC)
        end
        if hard(e) then
            if e.c_repause == 0 then
                if e.tint_flashes ~= 0 then
                    set_beh(e, 8)
                end
            else
                e.action_behavior = 5
                e.action_state = saved_state
                if prev ~= 5 then
                    e.action_state = 0
                end
            end
        end
        e.c_far_latch = 0
        return
    end

    if (path_latch(e) == 0 and dist < 8000) or e.c_wall_frames > 0x14 then
        e.ignore = 1
        set_beh(e, 2)
        e.animation_id = 3
        e.writhe_velocity = 0x20
        if dist < 4000 or e.c_far_latch ~= 0 then
            e.animation_id = 2
            e.writhe_velocity = 0x40
        end
        e.c_wall_frames = 0
        e.c_far_latch = 0
    end

    if path_latch(e) ~= 0 then
        if not e:player_facing_entity() and dist > 6000 then
            local px, _, pz = e:player_pos()
            local turn = e:turn_toward_target(px, pz, 0x40)
            if turn == 0 and e.player_action_behavior == 0x13 then
                e.ignore = 1
                set_beh(e, 0xB)
                e.c_far_latch = 0
            end
        end
    end

    if e.player_attacked & 0x80 ~= 0 then
        e.ignore = 1
        set_beh(e, 0xD)
    end
end

-- Variant 2, the ceiling brain: the whole body runs with the yaw flipped by
-- 180 degrees, so every decision sees the mirrored angle.
brain_c = function(e, dist)
    local prev = e.action_behavior

    e.angle = e.angle + 0x800

    if path_latch(e) == 0 then
        e.ignore = 0
        e.action_behavior = 9
        if prev ~= 9 then
            e.action_state = 0
            if dist > 3000 then
                e.c_far_latch = 1
            end
        end
    end

    if path_latch(e) ~= 0 or e.c_wall_frames > 0x1E then
        e.ignore = 1
        set_beh(e, 10)
        e.animation_id = 3
        e.writhe_velocity = 0x20
        if dist < 4000 or e.c_far_latch ~= 0 then
            e.animation_id = 2
            e.writhe_velocity = 0x40
        end
        e.c_wall_frames = 0
        e.c_far_latch = 0
    end

    if e.c_repause == 0 and dist < 2000 and e.player_attacked == 0 then
        e.ignore = 1
        set_beh(e, 3)
        if e:player_facing_entity() then
            set_beh(e, 6)
        end
        e.c_far_latch = 0
    end

    local px, _, pz = e:player_pos()
    if dist > 15000 and e:turn_toward_target(px, pz, 0x200) == 0 then
        e.ignore = 1
        set_beh(e, 6)
        e.c_far_latch = 0
    end

    if e.player_attacked & 0x80 ~= 0 then
        e.ignore = 1
        set_beh(e, 0xD)
    end

    e.angle = e.angle - 0x800
end

-- ---------------------------------------------------------------------------
-- The behaviours
-- ---------------------------------------------------------------------------

-- 0 idle: floor variants play their own clip; the ceiling variant waits out a
-- random timer.
local function idle_floor(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = e.behavior_flags
        e.blend_counter = 3
    elseif e.action_state ~= 1 then
        return
    end
    e:advance_anim(0x400)
end

local function idle_ceiling(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 1
        e.action_ticks_counter = (e:random() & 0x3F) + (1 - path_latch(e)) * 0x50
        e.blend_counter = 3
    elseif e.action_state ~= 1 then
        return
    end
    e:advance_anim(0x400)
    local prev = s16(e.action_ticks_counter)
    e.action_ticks_counter = prev - 1
    if prev == 0 then
        e.ignore = 0
        set_beh(e, 0)
    end
end

local function behavior_idle(e)
    local variant
    if e.behavior_flags == 0 or e.behavior_flags == 1 then
        variant = idle_floor
    elseif e.behavior_flags == 2 then
        variant = idle_ceiling
    else
        e:count_placeholder(e.behavior_flags)
        return
    end
    variant(e)
end

-- The shared idle-timeout driver the walk (1) and flee (9) run.
local function walk_timeout(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.action_ticks_counter = (e:random() & 0x1F) * e.action_behavior
        if (e.action_behavior & 0xFE) == 0 then
            e.action_ticks_counter = s16(e.action_ticks_counter) + 0x50
        end
    elseif e.action_state ~= 1 then
        return
    end
    e:advance_anim(0x400)
    local prev = s16(e.action_ticks_counter)
    e.action_ticks_counter = prev - 1
    if prev == 0 then
        e.ignore = 0
        set_beh(e, 0)
    end
end

-- 1 walk: with a path, refresh the zone waypoint and steer at it; without one,
-- turn at the player scaled by the (zero) path latch.
local function behavior_walk(e)
    local px, _, pz = e:player_pos()
    if path_latch(e) == 0 then
        local turn = e:turn_toward_target(px, pz, 0x10)
        e.angle = e.angle + turn * path_latch(e)
    else
        e:zone_path_update(px, pz)
        e:rotate_toward_target(e.player_pos_x, e.player_pos_z, 0x40)
    end

    walk_timeout(e)
    e:move(0, e.move_speed_current)

    if e.animation_frame_id % 6 == 0 and e.animation_id == 2 then
        e:play_enemy_sound(0)
    end
    if e.animation_frame_id % 10 == 0 and e.animation_id == 3 then
        e:play_enemy_sound(0)
    end
    if e.animation_frame_id % 15 == 0 and e.animation_id == 9 then
        e:play_enemy_sound(0)
    end
end

-- 2 turn: turn toward the player at the stored step until the timer burns out
-- or the chimera is aligned, then release and refresh the waypoint pair.
local function behavior_turn(e)
    local state = e.action_state
    if state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.action_state = 1
        e.action_ticks_counter = (e:random() & 0x1F) + 0x50
    elseif state ~= 1 then
        if state ~= 2 then
            return
        end
        e.ignore = 0
        set_beh(e, 0)
        if path_latch(e) ~= 0 then
            e.c_far_latch = 1
        end
        local px, _, pz = e:player_pos()
        e.player_pos_x = px
        e.player_pos_z = pz
        return
    end

    local px, _, pz = e:player_pos()
    local turn = e:turn_toward_target(px, pz, e.writhe_velocity)
    local saved_turn = turn
    local prev = s16(e.action_ticks_counter)
    e.action_ticks_counter = prev - 1
    if prev == 0 or turn == 0 then
        e.action_state = 2
    end
    e:advance_anim(0x400)
    e.angle = e.angle + saved_turn
end

-- 3 swipe: the double-claw reach test during frames 7-12.
local function behavior_swipe(e)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 8
        e.blend_counter = 3
        e:play_enemy_sound(3)
    elseif state ~= 1 then
        if state ~= 2 then
            return
        end
        e.ignore = 0
        set_beh(e, 0)
        e.c_repause = 0x3C
        return
    end

    local player_distance_z = e:reach_test(6, 100, 0, 0, 800) and 1 or 0
    local scaled = e:reach_test(10, 100, 0, 0, 800) and 1 or 0

    if e:line_of_sight() == 0 and e.player_attacked == 0
        and (player_distance_z ~= 0 or scaled ~= 0)
        and ((e.animation_frame_id - 7) & 0xFF) < 6 then
        e:spawn_player_effect(0, 0, 0, -0x9D8, 0, 0x200)
        e.player_health = e.player_health - 0x14

        local facing = player_facing(e)
        scaled = facing
        e.player_attacked = facing + 1
        e.player_action_behavior = facing + 0x66
        local px, py, pz = e:player_pos()
        e:play_3d_sound_at(3, e.player_attacked, px, py, pz)
        e:play_enemy_sound(5)

        if player_distance_z ~= 0 then
            e:spawn_joint_effect_at(0, 0, 6, 100, 0, 0, 0)
            e:spawn_joint_effect_at(0, 0, 6, 300, 0, 0, 0)
            e:tint_joint(6, 0, 0x40, 0x90)
        end
        if scaled ~= 0 then
            e:spawn_joint_effect_at(0, 0, 10, 100, 0, 0, 0)
            e:spawn_joint_effect_at(0, 0, 10, 300, 0, 0, 0)
            e:tint_joint(10, 0x90, 0x50, 0x10)
        end
    end

    if e:advance_anim(0x400) then
        e.action_state = 2
    end
end

-- 4 grabhold: snap onto the player, maul, then release into the swoop.
local function behavior_grabhold(e)
    local state = e.action_state
    if state == 0 then
        e.animation_frame_id = 10
        e.timing_control = 0
        e.action_state = 1
        e.blend_counter = 0
        e.angle = e.player_angle
        e.pos_y = 0
        e.status_flags = e.status_flags | 2
        e.groan_timer = 0x5A
        e.animation_id = 5
        local px, _, pz = e:player_pos()
        e.unk_c6 = px
        e.unk_c8 = pz
        e.player_unk_c6 = px
        e.player_unk_c8 = pz
        e.hit_state = 1
        e.player_action_behavior = 0
        e.player_action_state = 0
        e.player_attacked = 1
        e:set_player_animation(6, 9)
    elseif state ~= 1 then
        if state ~= 2 then
            return
        end
        e.hit_state = 0
        e.behavior_flags = 1
        e.ignore = 1
        e.action_behavior = 7
        e.action_state = 0
        if not hard(e) then
            return
        end
        e.action_behavior = 5
        e.action_state = 0
        e.status_flags = e.status_flags & 0xFD
        e.c_repause = 0x5A
        return
    end

    -- States 0 and 1 run the maul body.
    e:apply_anim_vertex()
    if e:advance_anim(0x400) then
        e.action_state = 2
    end
    e.death_timer = e.death_timer + 1

    if e.animation_frame_id == 0x22 then
        e:spawn_player_effect(0, 0, 800, -0x71C, 0, 0x200)
        local px, py, pz = e:player_pos()
        e:play_3d_sound_at(3, 1, px, py, pz)
        e:play_enemy_sound(6)
        e:spawn_joint_effect_at(0, 0, 6, 0, 0, 0, 0)
        e:tint_joint(6, 0xA0, 0x50, 0x10)
        if second_playthrough(e) then
            e.player_health = e.player_health - 0x1E
        else
            e.player_health = e.player_health - 10
        end
    end

    local input = e:player_mashing()
    e.groan_timer = e.groan_timer + (input and -3 or 0)
    e.player_attack_direction = e.groan_timer
    if e.groan_timer < 0 then
        e.groan_timer = 0
        e.player_attack_direction = 0
    end
end

-- 5 toggle: the floor<->ceiling variant swap with the full landing.
local function behavior_toggle(e)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        if e.behavior_flags ~= 0 then
            e.action_state = 3
            return
        end
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.animation_id = 4
        e:play_enemy_sound(0)
        state = 1
    end
    if state == 1 then
        if not e:advance_anim(0x400, e.behavior_flags ~= 0) then
            return
        end
        e.action_state = 2
        return
    end
    if state == 2 then
        e:play_enemy_sound(0)
        e.behavior_flags = (e.behavior_flags + 1) & 1
        e.action_state = 3
        state = 3
    end
    if state == 3 then
        e.action_state = 4
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.animation_id = 7
        e:spawn_effect(0, 0x1F, 200, -0x654, 0, 0, 0)
        e.c_far_latch = 0
        state = 4
    end
    if state == 4 then
        local done = e:advance_anim(0x400)
        e.action_state = e.action_state + (done and 1 or 0)
        return
    end
    if state == 5 then
        e.action_state = 0
        if e.c_repause == 0 then
            set_state(e, 1)
            set_beh(e, 0)
        end
    end
end

-- 6 drop: the ceiling drop attack.
local function behavior_drop(e)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0
        e.animation_id = 0x10
        e.action_ticks_counter = 0
        e.hit_state = 1
        e.status_flags = e.status_flags | 2
        state = 1
    end
    if state == 1 then
        e.pos_y = -ARC[32 - e.animation_frame_id + 1]
        e:advance_anim(0x400, true)
        e.action_state = 2
        return
    end
    if state == 4 then
        e.hit_state = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 3
        e:advance_anim(0x40)
        e.status_flags = e.status_flags & 0xFD
        e.pos_y = 0
        e.ignore = 0
        set_beh(e, 0)
        return
    end
    if state == 2 then
        e.roll = 0
        e.action_state = 3
        e.blend_counter = 0
        e.behavior_flags = 1
        e:play_enemy_sound(3)
    elseif state ~= 3 then
        return
    end

    -- The descent.
    local px, _, pz = e:player_pos()
    e.angle = e.angle + e:turn_toward_target(px, pz, 0x20)
    if e.animation_frame_id == 0x19
        and e:angular_view_and_distance(0x400, 2000) ~= 0
        and e:line_of_sight() == 0
        and e.player_attacked == 0
        and e:player_facing_entity() then
        e.action_behavior = 4
        e.action_state = 0
        e.angle = e:angle_to(px, pz)
        e.player_attacked = 1
        e.hit_state = 0
        return
    end
    e.pos_y = -ARC[32 - e.animation_frame_id + 1]
    e.move_speed_current = DROP_SPEED[e.animation_frame_id + 1]
    e:move(0x800, e.move_speed_current)
    if e:advance_anim(0x400, true) then
        e.action_state = e.action_state + 1
    end
    if e.animation_frame_id == 0x1F then
        e:play_enemy_sound(4)
    end
end

-- 7 swoop: the ceiling swoop, forward along the arc.
local function behavior_swoop(e)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0
        e.animation_id = 0x10
        e.action_ticks_counter = 0
        e.hit_state = 1
        e:play_enemy_sound(1)
        state = 1
    end
    if state == 1 then
        e.pos_y = -ARC[e.animation_frame_id + 1]
        e.move_speed_current = SWOOP_SPEED[e.animation_frame_id + 1]
        e:move(0, e.move_speed_current)
        e:advance_anim(0x400)
        if e.animation_frame_id == 0x20 then
            e.action_state = 2
            return
        end
        return
    end
    if state == 2 then
        e.pos_y = -ARC[e.animation_frame_id + 1]
        e.roll = 0x800
        e.action_state = 3
        e.blend_counter = 0
        e.behavior_flags = 2
        e:play_enemy_sound(2)
    elseif state == 4 then
        e.hit_state = 0
        e.c_repause = 0x5A
        e.status_flags = e.status_flags & 0xFD
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 1
        e:advance_anim(0x40)
        e.pos_y = -6008
        e.ignore = 0
        set_beh(e, 0)
        return
    elseif state ~= 3 then
        return
    end

    if e:advance_anim(0x400) then
        e.action_state = e.action_state + 1
    end
end

-- 8 spit: the acid spit during frames 10-12.
local function behavior_spit(e)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0x0E
        e.blend_counter = 3
        e:play_enemy_sound(3)
    elseif state ~= 1 then
        if state ~= 2 then
            return
        end
        if hard(e) then
            e.c_repause = 0x28
        end
        e.behavior_flags = 0
        set_state(e, 1)
        set_beh(e, 0)
        return
    end

    if e:angular_view_and_distance(0x200, 2000) ~= 0 then
        if e:line_of_sight() == 0 and e.player_attacked == 0
            and ((e.animation_frame_id - 10) & 0xFF) < 3 then
            e:spawn_player_effect(0, 0, 0, -0x9D8, 0, 0x200)
            if second_playthrough(e) then
                e.player_health = e.player_health - 20
            else
                e.player_health = e.player_health - 15
            end
            local facing = player_facing(e)
            e.player_attacked = facing + 1
            e.player_action_behavior = facing + 0x66
            local px, py, pz = e:player_pos()
            e:play_3d_sound_at(3, 1, px, py, pz)
            e:play_enemy_sound(6)
            e:spawn_joint_effect(0, 0, 6, 0)
            e:tint_joint(6, 0x10, 0x20, 0x60)
        end
    end

    if e:advance_anim(0x400) then
        e.action_state = 2
    end
end

-- 9 flee: the yaw is flipped so the walk steering drives it away.
local function behavior_flee(e)
    e.animation_id = 3
    e.move_speed_current = 100
    if e.c_far_latch ~= 0 then
        e.animation_id = 2
        e.move_speed_current = 200
    end
    local px, _, pz = e:player_pos()
    e.angle = e.angle + 0x800
    local turn = e:turn_toward_target(px, pz, 0x10)
    e.angle = e.angle + turn * path_latch(e)
    e.angle = e.angle - 0x800

    walk_timeout(e)
    e:move(0x800, e.move_speed_current)
    if e.animation_frame_id == 0 and e.animation_id == 2 then
        e:play_enemy_sound(2)
    end
end

-- 10 turn-back: a timed turn executed with the yaw flipped, so the chimera
-- turns its back to the player.
local function behavior_turn_back(e)
    local state = e.action_state
    if state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.action_state = 1
        e.action_ticks_counter = 0x50
    elseif state ~= 1 then
        if state ~= 2 then
            return
        end
        e.ignore = 0
        set_beh(e, 0)
        local px, _, pz = e:player_pos()
        e.player_pos_x = px
        e.player_pos_z = pz
        return
    end

    e.angle = e.angle + 0x800
    local px, _, pz = e:player_pos()
    local turn = e:turn_toward_target(px, pz, e.writhe_velocity)
    local saved_turn = turn
    local prev = s16(e.action_ticks_counter)
    e.action_ticks_counter = prev - 1
    if prev == 0 or turn == 0 then
        e.action_state = 2
    end
    e:advance_anim(0x400)
    e.angle = e.angle + saved_turn
    e.angle = e.angle - 0x800
end

-- 11 toggle_short: crouch, swap the variant bit, done.
local function behavior_toggle_short(e)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 4
        e.blend_counter = 3
        e:play_enemy_sound(0)
        return
    end
    if state ~= 1 then
        if state ~= 2 then
            return
        end
        e:play_enemy_sound(0)
        e.behavior_flags = (e.behavior_flags + 1) & 1
        set_state(e, 1)
        set_beh(e, 0)
        return
    end
    if e:advance_anim(0x400, e.behavior_flags ~= 0) then
        e.action_state = 2
    end
end

-- 12 claw: the standing swipe with the grab cancel.
local function behavior_claw(e)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 10
        e.blend_counter = 3
        e:play_enemy_sound(1)
    elseif state ~= 1 then
        if state ~= 2 then
            return
        end
        e.behavior_flags = 1
        set_state(e, 1)
        set_beh(e, 0)
        return
    end

    local px, py, pz = e:player_pos()
    e.angle = e.angle + e:turn_toward_target(px, pz, 0x20) * path_latch(e)

    if e.animation_frame_id == 10 then
        if e:angular_view_and_distance(0x200, 2000) ~= 0
            and e:line_of_sight() == 0
            and e.player_attacked == 0 then
            if e:player_facing_entity() then
                e.collision_flags = e.collision_flags & 8
                if e.collision_flags == 0 then
                    set_beh(e, 4)
                    e.angle = e:angle_to(px, pz)
                    e.player_attacked = 1
                    e.hit_state = 1
                    return
                end
            end
        end
    end

    if hard(e) and e.tint_flashes ~= 0 and e.animation_frame_id == 10
        and e.player_attacked == 0 then
        if second_playthrough(e) then
            e.player_health = e.player_health - 10
        else
            e.player_health = e.player_health - 5
        end
        local facing = player_facing(e)
        e.player_attacked = facing + 1
        e.player_action_behavior = facing + 0x66
        e:play_3d_sound_at(3, 0, px, py, pz)
    end

    if e:advance_anim(0x400) then
        e.action_state = 2
    end
    e:move(0, e.move_speed_current)
    if e.animation_frame_id == 0x17 then
        e:play_enemy_sound(4)
    end
end

-- 13 hit_fall: the knockdown hit reaction plays the variant's death clip.
local function behavior_hit_fall(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        local anim = DEATH_ANIM[e.behavior_flags + 1]
        if anim == nil then
            anim = 0
        end
        e.animation_id = anim
        e:play_enemy_sound(8)
    elseif e.action_state ~= 1 then
        return
    end
    e:advance_anim(0x400)
end

-- ---------------------------------------------------------------------------
-- State 2 - the hit reaction
-- ---------------------------------------------------------------------------

local function hit_dispatch(e)
    if e.ignore == 0 then
        local behavior = SEED_TBL[(e.hit_state & 7) + 1]
        local px, _, pz = e:player_pos()
        local turn = e:turn_toward_target(px, pz, 0x400)
        behavior = behavior + (((turn & 0xFFFFFFFF) >> 10) & 1)
        e.action_behavior = behavior
        e.ignore = 1
        e.action_state = 0
    end

    local behavior = e.action_behavior
    if behavior == 0 then
        e.animation_id = 0x0D
        hit_stagger(e)
    elseif behavior == 1 then
        e.animation_id = 0x0C
        hit_stagger(e)
    elseif behavior == 2 or behavior == 3 then
        e.animation_id = 0x0F
        if e.behavior_flags ~= 0 then
            e.animation_id = 6
        end
        hit_stagger(e)
    elseif behavior == 4 or behavior == 5 then
        e.animation_id = 0x0B
        hit_knockdown(e)
    end
end

-- The stagger driver (hit behaviours 0-3).
hit_stagger = function(e)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.move_speed_current = 0xA0
        e:play_enemy_sound(7)
        torso_impact(e)

        local wounded
        if not second_playthrough(e) then
            wounded = e.health <= 0x1D
        else
            wounded = e.health <= 0x13
        end
        if wounded and (e.hit_state & 1) == 0 then
            acid_burst(e)
        end
    elseif state ~= 1 then
        if state ~= 2 then
            return
        end
        e.hit_state = 0
        set_state(e, 1)
        set_beh(e, 0)
        if (e:random() & 1) == 0 then
            return
        end
        e.ignore = 1
        set_beh(e, 7)
        if not hard(e) then
            return
        end
        set_beh(e, 0xC)
        return
    end

    if e:advance_anim(0x400) then
        e.action_state = e.action_state + 1
    end
    if e.action_behavior < 2 then
        if e.animation_frame_id > 9 then
            e.move_speed_current = 0x3C
        end
        e:move((1 - e.action_behavior) * 0x800, e.move_speed_current)
    end
    if e.animation_frame_id == 10 then
        torso_impact(e)
    end
    e:check_special_weapon()
end

-- The knockdown driver (hit behaviours 4/5).
hit_knockdown = function(e)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0
        e.animation_id = 0x10
        e.action_ticks_counter = 0
        state = 1
    end
    if state == 1 then
        e.pos_y = -ARC[32 - e.animation_frame_id + 1]
        e:advance_anim(0x400, true)
        e.action_state = 2
        return
    end
    if state == 2 then
        e.pos_y = e.pos_y + 1000
        e.roll = 0
        e.action_state = 3
        e.blend_counter = 0
        e.behavior_flags = 1
        e.action_ticks_counter = sar32(0x82 - e.health, 2) + 0x14
        torso_impact(e)
        local wounded
        if not second_playthrough(e) then
            wounded = e.health < 0x1E
        else
            wounded = e.health < 0x14
        end
        if wounded and (e.hit_state & 1) == 0 then
            acid_burst(e)
        end
        state = 3
    end
    if state == 3 then
        e.pos_y = e.pos_y + (e.animation_frame_id * 5 + 10) * 10
        if e.pos_y > 0 then
            e.pos_y = 0
            if e.hit_state > 1 then
                e:play_enemy_sound(4)
                e.hit_state = 1
            end
        end
        if e:advance_anim(0x400) then
            e.action_state = 4
        end
        return
    end
    if state == 4 then
        local prev = s16(e.action_ticks_counter)
        e.action_ticks_counter = prev - 1
        if prev == 0 then
            e.action_state = 5
        end
        return
    end
    if state == 5 then
        e.action_state = 6
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0
        e.action_ticks_counter = 0
        state = 6
    end
    if state == 6 then
        e.animation_id = 0x11
        if e.animation_frame_id == 0x16 then
            e.angle = e.angle + 0x800
        end
        if e:advance_anim(0x400) then
            e.action_state = e.action_state + 1
        end
        return
    end
    if state == 7 then
        e.hit_state = 0
        set_state(e, 1)
        set_beh(e, 0)
        if (e:random() & 1) ~= 0 then
            e.ignore = 1
            set_beh(e, 7)
        end
    end
end

-- ---------------------------------------------------------------------------
-- State 3 - death
-- ---------------------------------------------------------------------------

local function death_dispatch(e)
    if e.ignore == 0 then
        local px, _, pz = e:player_pos()
        local turn = e:turn_toward_target(px, pz, 0x400)
        e.action_behavior = ((turn & 0xFFFFFFFF) >> 10) & 1
        e.ignore = 1
        e.action_state = 0
        if e.behavior_flags & 2 ~= 0 then
            e.action_behavior = 4
        end
    end

    local behavior = e.action_behavior
    if behavior == 0 then
        e.animation_id = 0x0B
        death_dissolve(e)
    elseif behavior == 1 then
        e.animation_id = 0x0C
        death_dissolve(e)
    elseif behavior == 4 then
        e.animation_id = 0x0B
        death_drop_dissolve(e)
    end
end

-- The ground death: play the death clip while sliding, then dissolve.
death_dissolve = function(e)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.move_speed_current = 0xA0
        e.action_ticks_counter = 0x3C
        torso_impact(e)
        e:play_enemy_sound(8)
        acid_burst(e)
        state = 1
    end
    if state == 1 then
        if e:advance_anim(0x400) then
            e.action_state = e.action_state + 1
        end
        if e.animation_frame_id > 9 then
            e.move_speed_current = 0x3C
        end
        e:move((1 - e.action_behavior) * 0x800, e.move_speed_current)
        if e.animation_frame_id == 0xF and e.action_behavior ~= 0 then
            e.action_state = 2
            return
        end
        return
    end
    if state == 2 then
        e.action_state = 3
        e.status_flags = e.status_flags | 2
        e.status_flags = e.status_flags | 8
        e.c_fade_freeze = 1
        e.shadow_tint = 0x00FFFF50
        e.shadow_half_x = 0
        e.shadow_half_z = 0
        state = 3
    end
    if state == 3 then
        e:adjust_shadow_size(0x16, 0x16)
        local prev = s16(e.action_ticks_counter)
        e.action_ticks_counter = prev - 1
        if prev == 0 then
            e.action_state = 4
            e:raise_death_event()
        end
    end
end

-- The air death: fall along the arc, land, then dissolve.
death_drop_dissolve = function(e)
    local state = e.action_state
    if state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0
        e.animation_id = 0x10
        e.action_ticks_counter = 0x3C
        e:play_enemy_sound(8)
        state = 1
    end
    if state == 1 then
        e.pos_y = -ARC[32 - e.animation_frame_id + 1]
        e:advance_anim(0x400, true)
        e.action_state = 2
        return
    end
    if state == 2 then
        e.pos_y = e.pos_y + 1000
        e.roll = 0
        e.action_state = 3
        e.blend_counter = 0
        e.behavior_flags = 1
        torso_impact(e)
        acid_burst(e)
        state = 3
    end
    if state == 3 then
        e.pos_y = e.pos_y + (e.animation_frame_id * 5 + 10) * 10
        if e.pos_y > 0 then
            e.pos_y = 0
            if e.hit_state > 1 then
                e:play_enemy_sound(4)
                e.hit_state = 1
            end
        end
        if e:advance_anim(0x400) then
            e.action_state = 4
            e.status_flags = e.status_flags | 2
            e.status_flags = e.status_flags | 8
            e.c_fade_freeze = 1
            e.shadow_tint = 0x00FFFF50
            e.shadow_half_x = 0
            e.shadow_half_z = 0
        end
        return
    end
    if state == 4 then
        e:adjust_shadow_size(0x18, 0x18)
        local prev = s16(e.action_ticks_counter)
        e.action_ticks_counter = prev - 1
        if prev == 0 then
            e.action_state = 5
            e:raise_death_event()
        end
    end
end

-- ---------------------------------------------------------------------------
-- The dispatch tables
-- ---------------------------------------------------------------------------

BEHAVIORS = {
    behavior_idle, -- 0
    behavior_walk, -- 1
    behavior_turn, -- 2
    behavior_swipe, -- 3
    behavior_grabhold, -- 4
    behavior_toggle, -- 5
    behavior_drop, -- 6
    behavior_swoop, -- 7
    behavior_spit, -- 8
    behavior_flee, -- 9
    behavior_turn_back, -- 10
    behavior_toggle_short, -- 11
    behavior_claw, -- 12
    behavior_hit_fall, -- 13
    brain_a, -- 14
    brain_b, -- 15
    brain_c, -- 16
}

BRAINS = { brain_a, brain_b, brain_c }

STATES = { state_init, state_run, hit_dispatch, death_dispatch }

-- ---------------------------------------------------------------------------
-- The per-frame entry
-- ---------------------------------------------------------------------------

function update(e)
    if not e.monster_paused then
        local state = e.state
        local handler = STATES[state + 1]
        if handler == nil then
            e:count_placeholder(state)
        else
            handler(e)
        end

        -- The SCA and room tails: the touch latch, the collision-flags bit-3
        -- clear and the entity's own room resolve.
        e.tint_flashes = e:separate() and 1 or 0
        e.collision_flags = e.collision_flags & 0xF7
        e.reaction_timer = e:resolve_collision()
    end

    -- The switch-zone bit and the per-frame shadow resize (frozen while the
    -- death dissolve owns the quad).
    e:update_switch_zone()
    if e.has_enter_switch_zone ~= 0 and e.c_fade_freeze == 0 then
        local joint_y = e:joint_world_y(3)
        local size = sar32(joint_y, 4) + 800
        if ((-joint_y - 0x898) & 0xFFFFFFFF) < 2000 then
            size = 600
        end
        if e.pos_y < -0x17D4 then
            size = size - 200
        end
        e.shadow_half_x = size + 0x32
        e.shadow_half_z = size + 200
    end
end
