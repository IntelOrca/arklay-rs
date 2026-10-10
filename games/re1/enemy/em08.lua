-- Plant 42, entity id 0x08 (the guardhouse boss).
--
-- One entity runs the whole machine. The state byte dispatches init / state
-- check / damaged / die / the SCD table, the action-behavior byte selects one
-- of sixteen action handlers, and the state check's selector picks the next
-- action from the range gates. The plant's own skeleton clock is
-- `e:plant42_advance` (the sub-frame blend animator), not the shared one.
--
-- Two companion clones (the flower body and the root ball) live outside the
-- enemy list in the Rust companion arena; `e:plant42_spawn` builds them,
-- `e:plant42_tick` runs their machines, and the `plant42_body_*` /
-- `plant42_body_wither` / `plant42_body_kill` methods are the writes the
-- plant's handlers make on them. The body's `+0x70` word is the shared vine
-- pool: the boss fight counts down one vine per hit and awards the kill on
-- the last one.
--
-- `behavior_flags` selects the set-up: bit 0 the split-vine block (no
-- companions; it shares the boss's body), `0x0F == 4` the room-40C0 fallen
-- core, `== 5` the poison arena, `== 6` Chris held in the vine, `0x0F == 8`
-- the poison body, `0x40` the boss fight (state 8 until the SCD hands over).
--
-- All state lives on the Rust entity and its companions; this file keeps
-- none, so the scripting VM may be reset between any two updates.

local SCA_SPLIT_RADIUS = 200
local SCA_SPLIT_HALF_HEIGHT = 250
local SCA_BOSS_RADIUS = 2000
local SCA_BOSS_HALF_HEIGHT = 7000

-- The idle sway pool, indexed `rand() % 10` (the eleventh entry is
-- unreachable, exactly like the original's table).
local IDLE_ANIMS = { 2, 3, 4, 5, 6, 9, 10, 11, 12, 13, 15 }

-- The recoil poses, indexed `rand() & 7`.
local HIT_ANIMS = { 2, 3, 4, 5, 10, 0xB, 0xC, 0xD }

-- The action selector's tables, indexed `rand() & 7`.
local ACTIONS_DEFAULT = { 1, 2, 3, 1, 3, 1, 1, 2 }
local ACTIONS_FLAGS5 = { 1, 2, 1, 1, 2, 1, 1, 2 }

-- The ambient room spit anchors.
local EFFECT_X = { 0x0D54, 0x45EC }
local EFFECT_Z = { 0x0D54, 0x46FA }

-- The fixed orientation room 40C0 seats Chris in the vine with.
local CHRIS_HOLD_MATRIX = {
    0x0090, 0xF06D, 0xFC95,
    0xF044, 0xFF0D, 0x01E7,
    0xFDE8, 0x034B, 0xF0A4,
}

-- The hard-coded release destination (room 40C0's hand-off).
local RELEASE_X = 0x1757
local RELEASE_Z = 0x3BBD
local RELEASE_ANGLE = 0x499

local function abs(value)
    if value < 0 then
        return -value
    end
    return value
end

-- Sign-extend a stored 8-bit counter for the signed compares the original
-- compiles at those sites.
local function as_s8(value)
    value = value & 0xFF
    if value >= 0x80 then
        return value - 0x100
    end
    return value
end

-- Sign-extend a stored 16-bit counter.
local function as_s16(value)
    value = value & 0xFFFF
    if value >= 0x8000 then
        return value - 0x10000
    end
    return value
end

-- Truncating division (the C integer divide rounds toward zero; Lua's `//`
-- floors).
local function idiv(a, b)
    local q = a // b
    if a % b ~= 0 and ((a < 0) ~= (b < 0)) then
        q = q + 1
    end
    return q
end

-- Queue a `Play3DSnd` cue at the plant's posed joint.
local function joint_sound(e, bank, id, joint)
    e:play_3d_sound_at(
        bank,
        id,
        e:joint_world_x(joint),
        e:joint_world_y(joint),
        e:joint_world_z(joint)
    )
end

-- Queue a `Play3DSnd` cue at the player.
local function player_sound(e, bank, id)
    local x, y, z = e:player_pos()
    e:play_3d_sound_at(bank, id, x, y, z)
end

-- The shared re-arm tail of the three attack behaviours: clear our hit flag,
-- and re-raise it while the main body still has more than one hit left.
local function rearm_hit_flag(e)
    e.hit_state = 0
    if (e.behavior_flags & 1) == 0 and e:plant42_body() ~= nil then
        if (e:plant42_body_vines() or 0) > 1 then
            e.hit_state = 1
        end
    end
end

local idle
local sweep
local spit
local move
local hold
local grab_alt
local suspend
local release
local kill
local wither
local action_select
local recoil
local scd_flinch
local death
local run_action
local ambient_effects

-- Behavior 0: idle sway, also the SCD table's entry 0.
idle = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.p42_ticks = (e:random() & 0xF) + 0xF
        e.blend_counter = 0x3F
        e.p42_yaw_jitter = as_s8(e:random() & 0xF)
        e.p42_roll_jitter = as_s8(e:random() & 7)
        e.p42_yaw_jitter = (1 - (e:random() & 2)) * e.p42_yaw_jitter
        e.p42_roll_jitter = (1 - (e:random() & 2)) * e.p42_roll_jitter
        if e.animation_id == 8 then
            e.animation_id = 9
        end
        e.angle = (e.angle - idiv(e.p42_yaw_jitter, 2)) & 0xFFF
        e.roll = e.roll - idiv(e.p42_roll_jitter, 2)
    end

    if st == 0 or st == 1 then
        if e:plant42_advance(false, 0x40) then
            e.action_state = e.action_state + 1
        end
    elseif st == 2 then
        if e:plant42_advance(true, 0x40) then
            e.action_state = e.action_state + 1
        end
    elseif st == 3 then
        e.action_state = 0
    end

    if (e.behavior_flags & 0x40) == 0 then
        e.angle = (e.p42_yaw_jitter + e.angle) & 0xFFF
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 8)
    end

    local saved_roll = e.roll
    e.roll = e.roll + e.p42_roll_jitter
    if ((e.roll - 0x3E) & 0xFFF) < 0xE32 then
        e.roll = e.roll - e.p42_roll_jitter
    end

    if e:joint_world_y(14) > -600 then
        e.roll = saved_roll - 0x10
        e.animation_id = IDLE_ANIMS[(e:random() % 10) + 1]
        e.blend_counter = 0x3F
    end

    local t = e.p42_ticks
    e.p42_ticks = t - 1
    if t == 0 then
        e.animation_id = IDLE_ANIMS[(e:random() % 10) + 1]
        e.blend_counter = 0x3F
    end
end

local function sweep_anim_a(e)
    return (e.p42_sweep_dir == 0) and 1 or 0
end
local function sweep_anim_b(e)
    return (e.p42_sweep_dir == 0) and 0 or 1
end
local function sweep_sign(e)
    return (e.p42_sweep_dir == 0) and 1 or -1
end

-- The sweep contact test, called from states 5 and 6.
local function sweep_hit(e)
    local reach = (as_s16(e.p42_dist) - 2000) & 0xFFFF
    if e.behavior_flags == 5 then
        reach = (as_s16(e.p42_dist) - 0xDAC) & 0xFFFF
    end
    if reach > 0x157B then
        return
    end
    local px, _, pz = e:player_pos()
    if as_s16(e:turn_toward_target(px, pz, e.p42_step)) ~= 0 then
        return
    end
    if e.player_attacked ~= 0 then
        return
    end
    if as_s16(e.p42_step) < 0x51 then
        return
    end

    e.roll = e.roll - 200
    e.action_state = 6
    e.angle = e.angle + 0x400
    local facing = e:player_facing_entity() and 1 or 0
    e.angle = e.angle - 0x400
    e.player_attacked = 1

    local dir_set = e.p42_sweep_dir ~= 0
    if (facing ~= 0) ~= dir_set then
        e.player_state = 6
        e.player_anim_frame_id = 8
        e.player_action_behavior = 0
        e.player_action_state = 0
        e.player_angle = e.angle + ((facing ~= 0) and 0x400 or -0x400)
        joint_sound(e, 2, 4, 14)
    else
        e:set_plant42_capture_t0(e.angle + ((facing ~= 0) and 0x400 or -0x400))
        e.player_state = 6
        e.player_anim_frame_id = 8
        e.player_action_behavior = 2
        e.player_action_state = 5
        joint_sound(e, 2, 2, 14)
    end

    if e.second_playthrough then
        e.player_health = e.player_health - 0x19
    else
        e.player_health = e.player_health - 0x10
    end
    if e.player_health < 0 then
        e.player_health = 1
    end
end

-- Behavior 1: horizontal sweep.
sweep = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.animation_id = 0x0C
        e.p42_ticks = 0x32
        e.p42_sweep_dir = 1
        local px, _, pz = e:player_pos()
        if as_s16(e:turn_toward_target(px, pz, 1)) < 0 then
            e.p42_sweep_dir = 0
        end
    end

    if st == 0 or st == 1 then
        e:plant42_advance(sweep_anim_a(e) ~= 0, 0x40)
        e.angle = e.angle + as_s16(sweep_sign(e) * 0x10)
        local hy = e:joint_world_y(14)
        if hy < -0x9C4 then
            e.roll = e.roll + 8
        end
        if hy > -1000 then
            e.roll = e.roll - 8
        end
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 2
        end
    elseif st == 2 then
        e.p42_ticks = 0xF
        e.action_state = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e:plant42_advance(sweep_anim_b(e) ~= 0, 0x40)
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 4
        end
    elseif st == 3 then
        e:plant42_advance(sweep_anim_b(e) ~= 0, 0x40)
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 4
        end
    elseif st == 4 then
        e.action_state = 5
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.animation_id = 0x0C
        e.p42_step = e.p42_step + 4
        if e.p42_step > 0x28 then
            e.p42_step = e.p42_step + 0x1C
        end
        e:plant42_advance(sweep_anim_a(e) ~= 0, 0x40)
        e.angle = e.angle - as_s16(sweep_sign(e) * e.p42_step)
        if e.p42_step > 0xC0 then
            e.action_state = 6
            joint_sound(e, 2, 3, 14)
        end
        sweep_hit(e)
    elseif st == 5 then
        e.p42_step = e.p42_step + 4
        if e.p42_step > 0x28 then
            e.p42_step = e.p42_step + 0x1C
        end
        e:plant42_advance(sweep_anim_a(e) ~= 0, 0x40)
        e.angle = e.angle - as_s16(sweep_sign(e) * e.p42_step)
        if e.p42_step > 0xC0 then
            e.action_state = 6
            joint_sound(e, 2, 3, 14)
        end
        sweep_hit(e)
    elseif st == 6 then
        e.roll = e.roll - 8
        e:plant42_advance(sweep_anim_b(e) ~= 0, 0x40)
        sweep_hit(e)
        if e.p42_step < 1 then
            e.action_state = 7
            e.angle = e.angle + as_s16(sweep_sign(e) * -8)
        else
            e.angle = e.angle - as_s16(sweep_sign(e) * e.p42_step)
            e.p42_step = e.p42_step - 0x10
            rearm_hit_flag(e)
        end
    elseif st == 7 then
        e.angle = e.angle + as_s16(sweep_sign(e) * 8)
        e.state_word = 1
        e.p42_step = 0
    end
end

-- Behavior 2: acid spit.
spit = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.animation_id = 7
        e.hit_state = 1
        e.p42_step = 4
    end

    if st == 0 or st == 1 then
        if e:plant42_advance(false, 0x40) then
            e.action_state = 2
            e.roll = e.roll - e.p42_step
            e.p42_step = e.p42_step - 0x30
            joint_sound(e, 2, 1, 14)
            return
        end
        local body_hit = e:plant42_body_hit() or 0
        local step = as_s16(body_hit * 0x20 + 0x10)
        local px, _, pz = e:player_pos()
        e:turn_toward_target(px, pz, step)
        if e.animation_frame_id < 8 then
            e.roll = e.roll + 0x40
            return
        end
        if e.animation_frame_id > 0xC then
            e.roll = e.roll - e.p42_step
            e.p42_step = e.p42_step + 0x18
        end
        if e.animation_frame_id > 10
            and e.player_attacked == 0
            and e:reach_test(15, 0, 0, 0, 0x4B0)
            and e:joint_world_y(15) > -0x9C4
        then
            e.player_attacked = 1
            e.player_state = 2
            e.player_anim_frame_id = 0
            e.player_action_behavior = 0x64
            e.player_action_state = 0
            e.p42_step = e.p42_step - 8
            if e.second_playthrough then
                e.player_health = e.player_health - 15
            else
                e.player_health = e.player_health - 8
            end
            if e.player_health < 0 then
                e.player_health = 1
            end
            joint_sound(e, 2, 2, 14)
            return
        end
    elseif st == 2 then
        e.roll = e.roll - e.p42_step
        e.p42_step = e.p42_step - 8
        if e.p42_step < 0 then
            e.action_state = 3
        end
    elseif st == 3 then
        e.animation_id = 2
        e.action_state = 4
        e.p42_step = (e:random() & 1) * -0x20 + 0x10
        e.blend_counter = 0x3F
        e.roll = e.roll + 0x10
        if e.roll > -400 then
            e.action_state = 5
        end
        e.angle = e.angle + e.p42_step
        e:plant42_advance(false, 0x40)
    elseif st == 4 then
        e.roll = e.roll + 0x10
        if e.roll > -400 then
            e.action_state = 5
        end
        e.angle = e.angle + e.p42_step
        e:plant42_advance(false, 0x40)
    elseif st == 5 then
        e.angle = e.angle + as_s16(idiv(e.p42_step, 2))
        e.state_word = 1
        e.animation_id = 4
        rearm_hit_flag(e)
    end
end

-- The grab/lift speed vector: the joint-to-player delta with the lift offset,
-- divided by six (the approach) or two (the lift easing).
local function grab_speed_from(e, joint, divisor, with_offset, y_bias)
    local px, py, pz = e:player_pos()
    local ox, oy, oz = 0, 0, 0
    if with_offset then
        ox, oy, oz = e:apply_matrix_lv(joint, -0x60F, 0, 700)
    end
    return idiv(e:joint_world_x(joint) - px + ox, divisor),
        idiv(e:joint_world_y(joint) - py + oy + y_bias, divisor),
        idiv(e:joint_world_z(joint) - pz + oz, divisor)
end

-- The lift tick shared by the approach's commit fall-through and the lift
-- state itself.
local function move_lift(e)
    local grab_joint = e.p42_grab_joint
    if e.animation_frame_id > 3 then
        local sx, sy, sz = grab_speed_from(e, grab_joint, 2, true, 0)
        e:set_player_speed(sx, sy, sz)
    end
    if e:joint_world_y(14) > -2000 then
        e.roll = e.roll - 0x10
    end
    e:add_player_speed()
    if e:plant42_advance(false, 0x400) then
        e.action_state = e.action_state + 1
    end
end

-- Behavior 3: approach, grab, lift.
move = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.animation_id = 8
        e.p42_ticks = 0x3C
        e.p42_step = 8
    end

    if st == 0 or st == 1 then
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 5
        end
        e.angle = e.angle - 0x28
        local px, _, pz = e:player_pos()
        e.angle = e.angle + as_s16(e:turn_toward_target(px, pz, 0x10))
        e.angle = e.angle + 0x28
        e:plant42_advance(true, 0x40)
        local head_y = e:joint_world_y(14)
        if head_y < -0x9C4 then
            e.roll = e.roll + 0x10
        elseif head_y < -999 then
            if e.p42_ticks < 0x1E then
                e.action_state = 2
                e.p42_ticks = 0x3C
            end
        else
            e.roll = e.roll - 0x10
        end
    elseif st == 2 then
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 or e.player_attacked ~= 0 then
            e.action_state = 5
        end
        e.angle = e.angle - 0x30
        local px, _, pz = e:player_pos()
        e.angle = e.angle + as_s16(e:turn_toward_target(px, pz, 0x10))
        e.angle = e.angle + 0x30

        local pxx, _, pzz = e:player_pos()
        e.player_displacement = abs(pxx - e:joint_world_x(14)) + abs(pzz - e:joint_world_z(14))
        e.player_distance_z = abs(pzz - e:joint_world_z(12)) + abs(pxx - e:joint_world_x(12))

        if (e.player_displacement > 799 and e.player_distance_z > 499)
            or (e.player_health_status & 8) ~= 0
            or e.player_attacked ~= 0
            or e.player_pos_y ~= 0
            or as_s16(e:turn_toward_target(px, pz, 0x20)) ~= -0x20
        then
            e:plant42_advance(true, 0x40)
            e.roll = e.roll + e.p42_step
            if e:joint_world_y(14) > -0x5DC and e.p42_step >= 0 then
                e.p42_step = -e.p42_step
            end
            if e:joint_world_y(14) < -0x9C4 and e.p42_step < 0 then
                e.p42_step = -e.p42_step
            end
        else
            e.action_state = 3
            e.animation_id = 0x0E
            e.hit_state = 1
            e.p42_grab_joint = 15
            e.player_state = 6
            e.player_anim_frame_id = 8
            e.player_action_behavior = 1
            e.player_action_state = 0
            e.player_attacked = 1
            local sx, sy, sz = grab_speed_from(e, 11, 6, false, 0x7EF)
            e:set_player_speed(sx, sy, sz)
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 3
            move_lift(e)
        end
    elseif st == 3 then
        move_lift(e)
    elseif st == 4 then
        joint_sound(e, 2, 5, 14)
        e.state_word = 0x00040101
    elseif st == 5 then
        e.p42_ticks = 0x14
        e.action_state = 6
        e.p42_step = 0
        if e:joint_world_y(14) > -2000 then
            e.p42_step = 0x10
        end
        e.animation_id = 5
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 7
        end
        e.roll = e.roll - e.p42_step
        e.angle = e.angle + 8
        e:plant42_advance(false, 0x40)
    elseif st == 6 then
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 7
        end
        e.roll = e.roll - e.p42_step
        e.angle = e.angle + 8
        e:plant42_advance(false, 0x40)
    elseif st == 7 then
        e.state_word = 1
        e.animation_id = 9
        rearm_hit_flag(e)
    end
end

-- The drop hand-over shared by the hold's case 7 fall-through and its case 8:
-- place the player, clear the grabbed bits and deal the drop damage.
local function hold_drop_setup(e)
    local px, _, pz = e:player_pos()
    e:set_player_pos(px, 0, pz)
    e.player_zone_flags = e.player_zone_flags & 0x7F
    e.player_flags = e.player_flags & 0xFD
    e.animation_id = 3
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0x3F
    e.player_state = 6
    e.player_anim_frame_id = 8
    e.player_action_behavior = 2
    e.player_action_state = 0
    if e.second_playthrough then
        e.player_health = e.player_health - 0x28
    else
        e.player_health = e.player_health - 0x14
    end
end

-- Behavior 4: held aloft, squeezed, dropped.
hold = function(e)
    local grab_joint = e.p42_grab_joint
    local st = e.action_state
    if st == 0 then
        e.p42_ticks = 10
        e.action_state = 1
        e.p42_step = 0x18
        e.player_flags = e.player_flags | 2
        e:plant42_capture_setup(grab_joint)
        e:set_plant42_capture_t(-0x60F, 0, 700)
        joint_sound(e, 2, 6, 14)
        player_sound(e, 2, (e.player_character & 1) + 0x17)
    end

    if st == 0 or st == 1 then
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 2
        end
        e.roll = e.roll - e.p42_step
        e:plant42_hold_player(grab_joint)
    elseif st == 2 then
        e.action_state = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.animation_id = 0x0F
        if e.player_health < 0x28 then
            e.state_word = 0x00080101
        else
            e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
            e.roll = e.roll - 4
            e:plant42_hold_player(grab_joint)
        end
    elseif st == 3 then
        e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
        e.roll = e.roll - 4
        e:plant42_hold_player(grab_joint)
    elseif st == 4 then
        e.p42_ticks = 0xF
        e.action_state = 5
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 6
        end
        e.roll = e.roll - 8
        e:plant42_hold_player(grab_joint)
    elseif st == 5 then
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 6
        end
        e.roll = e.roll - 8
        e:plant42_hold_player(grab_joint)
    elseif st == 6 then
        -- case 6 falls into case 7 in the original.
        e.action_state = 7
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x1F
        e.animation_id = 0
        e.p42_step = 8
        e.roll = e.roll + e.p42_step
        e.p42_step = e.p42_step + 1
        if e.player_pos_y < -0x5DB then
            e:plant42_advance(false, 0x80)
            e:plant42_hold_player(grab_joint)
        else
            player_sound(e, 2, 0x1A)
            e.action_state = 9
            hold_drop_setup(e)
            e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
            e.roll = e.roll - 2
        end
    elseif st == 7 then
        e.roll = e.roll + e.p42_step
        e.p42_step = e.p42_step + 1
        if e.player_pos_y < -0x5DB then
            e:plant42_advance(false, 0x80)
            e:plant42_hold_player(grab_joint)
        else
            player_sound(e, 2, 0x1A)
            e.action_state = 9
            hold_drop_setup(e)
            e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
            e.roll = e.roll - 2
        end
    elseif st == 8 then
        -- The case-8 hand-over, reachable directly as well as through case 7.
        e.action_state = 9
        hold_drop_setup(e)
        e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
        e.roll = e.roll - 2
    elseif st == 9 then
        e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
        e.roll = e.roll - 2
    elseif st == 10 then
        rearm_hit_flag(e)
        e.state_word = 1
    end
end

-- Behavior 5: the alternate grab that uses joint 15 directly.
grab_alt = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.animation_id = 8
        e.p42_ticks = 0x1E
    end

    if st == 0 or st == 1 then
        e.angle = e.angle - 0x30
        local px, _, pz = e:player_pos()
        e.angle = e.angle + as_s16(e:turn_toward_target(px, pz, 0x10))
        e.angle = e.angle + 0x30
        e:plant42_advance(true, 0x40)
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 2
            e.animation_id = 0x0E
            local sx, sy, sz = grab_speed_from(e, 11, 6, false, 0x7EF)
            e:set_player_speed(sx, sy, sz)
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 3
        end
        if e:joint_world_y(14) < -0x9C4 then
            e.roll = e.roll + 0x10
        end
        if e:joint_world_y(14) > -1000 then
            e.roll = e.roll - 0x10
        end
    elseif st == 2 then
        if e.animation_frame_id > 3 then
            local sx, sy, sz = grab_speed_from(e, 15, 2, true, 0)
            e:set_player_speed(sx, sy, sz)
        end
        e:add_player_speed()
        if e:plant42_advance(false, 0x400) then
            e.action_state = e.action_state + 1
        end
    elseif st == 3 then
        e.p42_ticks = 10
        e.action_state = 4
        e.p42_step = 0x18
        e.player_flags = e.player_flags | 2
        e:plant42_capture_setup(15)
        e:set_plant42_capture_t(-0x60F, 0, 700)
        joint_sound(e, 2, 5, 14)
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 5
            joint_sound(e, 2, 6, 14)
            player_sound(e, 2, (e.player_character & 1) + 0x17)
        end
        e.roll = e.roll - e.p42_step
        e:plant42_hold_player(15)
    elseif st == 4 then
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 5
            joint_sound(e, 2, 6, 14)
            player_sound(e, 2, (e.player_character & 1) + 0x17)
        end
        e.roll = e.roll - e.p42_step
        e:plant42_hold_player(15)
    elseif st == 5 then
        e:plant42_hold_player(15)
    end
end

-- Behavior 6: the player is held motionless in the vine (room 40C0's Chris
-- sequence).
suspend = function(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_id = 0x0E
        e.animation_frame_id = 4
        e.timing_control = 0
        e.blend_counter = 0
        e:plant42_advance(false, 0x400)
        if (e.player_character & 1) == 0 and (e.behavior_flags & 0xF) == 6 then
            e:plant42_capture_orient(
                CHRIS_HOLD_MATRIX[1], CHRIS_HOLD_MATRIX[2], CHRIS_HOLD_MATRIX[3],
                CHRIS_HOLD_MATRIX[4], CHRIS_HOLD_MATRIX[5], CHRIS_HOLD_MATRIX[6],
                CHRIS_HOLD_MATRIX[7], CHRIS_HOLD_MATRIX[8], CHRIS_HOLD_MATRIX[9]
            )
        end
        e.p42_yaw_jitter = 1
        e.p42_roll_jitter = 1
        e.p42_osc_a = 3
        e.p42_osc_b = 6
        e.p42_step = 1
    elseif e.action_state ~= 1 then
        return
    end

    local t = e.p42_step
    e.p42_step = t - 1
    if t == 0 then
        e.angle = e.angle - e.p42_osc_a
        e.roll = e.roll - e.p42_osc_b
        e.p42_osc_a = e.p42_osc_a - e.p42_yaw_jitter
        e.p42_osc_b = e.p42_osc_b - e.p42_roll_jitter
        if (e.p42_osc_a + 4) & 0xFFFF > 8 then
            e.p42_yaw_jitter = -e.p42_yaw_jitter
        end
        if (e.p42_osc_b + 7) & 0xFFFF > 0xE then
            e.p42_roll_jitter = -e.p42_roll_jitter
        end
        e.p42_step = 1
    end
    e:plant42_hold_player(15)
end

-- Behavior 7: release the held player (room 40C0's hand-off; the destination
-- is hard-coded).
release = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.animation_id = 0x0F
        e.player_state = 6
        e.player_anim_frame_id = 8
        e.player_action_behavior = 1
        e.player_action_state = 0
        joint_sound(e, 2, 6, 14)
        player_sound(e, 2, (e.player_character & 1) + 0x17)
    end

    if st == 0 or st == 1 then
        e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
        e.roll = e.roll - 4
        e:plant42_hold_player(15)
    elseif st == 2 then
        e.p42_ticks = 0xF
        e.action_state = 3
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 4
        end
        e.roll = e.roll - 8
        e:plant42_hold_player(15)
    elseif st == 3 then
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 4
        end
        e.roll = e.roll - 8
        e:plant42_hold_player(15)
    elseif st == 4 then
        e.action_state = 5
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.animation_id = 0
        e.p42_step = 8
        e.roll = e.roll + e.p42_step
        e.p42_step = e.p42_step + 1
        if e.player_pos_y < -0x5DB then
            e:plant42_advance(false, 0x40)
            e:plant42_hold_player(15)
        else
            player_sound(e, 2, 0x1A)
            e.action_state = 7
            e:set_player_pos(RELEASE_X, 0, RELEASE_Z)
            e.player_zone_flags = e.player_zone_flags & 0x7F
            e.player_flags = e.player_flags & 0xFD
            e.animation_id = 3
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 0x3F
            e.player_attacked = 1
            e.player_state = 6
            e.player_anim_frame_id = 8
            e.player_action_behavior = 2
            e.player_action_state = 0
            e.player_angle = RELEASE_ANGLE
            e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
            e.roll = e.roll - 2
        end
    elseif st == 5 then
        e.roll = e.roll + e.p42_step
        e.p42_step = e.p42_step + 1
        if e.player_pos_y < -0x5DB then
            e:plant42_advance(false, 0x40)
            e:plant42_hold_player(15)
        else
            player_sound(e, 2, 0x1A)
            e.action_state = 7
            e:set_player_pos(RELEASE_X, 0, RELEASE_Z)
            e.player_zone_flags = e.player_zone_flags & 0x7F
            e.player_flags = e.player_flags & 0xFD
            e.animation_id = 3
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 0x3F
            e.player_attacked = 1
            e.player_state = 6
            e.player_anim_frame_id = 8
            e.player_action_behavior = 2
            e.player_action_state = 0
            e.player_angle = RELEASE_ANGLE
            e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
            e.roll = e.roll - 2
        end
    elseif st == 6 then
        -- case 6 falls into case 7 in the original.
        e.action_state = 7
        e:set_player_pos(RELEASE_X, 0, RELEASE_Z)
        e.player_zone_flags = e.player_zone_flags & 0x7F
        e.player_flags = e.player_flags & 0xFD
        e.animation_id = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.player_attacked = 1
        e.player_state = 6
        e.player_anim_frame_id = 8
        e.player_action_behavior = 2
        e.player_action_state = 0
        e.player_angle = RELEASE_ANGLE
        e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
        e.roll = e.roll - 2
    elseif st == 7 then
        e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
        e.roll = e.roll - 2
    elseif st == 8 then
        e.state_word = 8
    end
end

-- Behavior 8: the kill animation (player health too low).
kill = function(e)
    local grab_joint = e.p42_grab_joint
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.p42_ticks = 0x1E
        joint_sound(e, 2, 6, 14)
    end

    if st == 0 or st == 1 then
        e:plant42_hold_player(grab_joint)
        -- The original twists joint 12 in place; the port derives the pose
        -- from the clip and has no per-joint rotation write.
        e.roll = e.roll - 1
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 2
            e.p42_ticks = 0x5A
            e.player_state = 6
            e.player_anim_frame_id = 8
            e.player_action_behavior = 1
            e.player_action_state = 4
            e.p42_step = 0x18
        end
    elseif st == 2 then
        local divisor = (e:random() & 3) + 1
        e.angle = e.angle + as_s16(idiv(e.p42_step, divisor))
        e.roll = e.roll - 1
        e:plant42_hold_player(grab_joint)
        e.p42_step = -e.p42_step
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 3
            e.p42_ticks = 0x50
            e:tint_joint(14, 0x30, 0x80820, 0x606060)
            e:tint_joint(14, 0x30, 0x80820, 0x606060)
            e:tint_joint(14, 0x30, 0x80820, 0x606060)
            e:set_entity_joint_flag(0, 2, e:entity_joint_flag(0, 2) | 8)
            e:set_entity_joint_flag(0, 0, e:entity_joint_flag(0, 0) | 0x10)
            e.player_state = 7
            e.player_anim_frame_id = 8
            e.player_action_behavior = 0
            e.player_action_state = 0
        end
    elseif st == 3 then
        e:plant42_hold_player(grab_joint)
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 4
        end
    elseif st == 4 then
        e.state_word = 1
        e:set_joint_flag(12, e:joint_flag(12) | 2)
        e.player_state = 7
        e.player_anim_frame_id = 8
        e.player_action_behavior = 0
        e.player_action_state = 2
    end
end

-- Behavior 9: the death/withering sequence.
wither = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.animation_id = 0x0D
        e.joint_scale = 0x1000
        if (e.behavior_flags & 1) == 0 then
            e:plant42_body_wither(0)
            e:play_enemy_sound(7)
        end
        e.joint_scale = e.joint_scale - 0x20
        e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
        return
    elseif st == 1 then
        e.joint_scale = e.joint_scale - 0x20
        e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
        return
    elseif st == 2 then
        e.p42_ticks = 0xD2
        e.action_state = 3
        if (e.behavior_flags & 1) == 0 then
            e:plant42_body_wither(2)
        end
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 4
        end
        return
    elseif st == 3 then
        local t = e.p42_ticks
        e.p42_ticks = t - 1
        if t == 0 then
            e.action_state = 4
        end
        return
    elseif st == 4 then
        e.action_state = 5
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.animation_id = 2
        if (e.behavior_flags & 1) == 0 then
            e:plant42_body_wither(4)
        end
    elseif st == 5 then
        e.joint_scale = e.joint_scale + 0x20
        e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
    elseif st == 6 then
        e.state_word = 1
        e.joint_scale = 0
        e.hit_state = 0
        if (e.behavior_flags & 1) == 0 then
            e:plant42_body_wither(6)
        end
        if (e.behavior_flags & 0x40) ~= 0 then
            e.state = 8
        end
    end
end

-- The idle action selector, also the handler for table slots 10-13 and 15.
-- It re-reads the shared distance scratch the state check's selector left.
action_select = function(e)
    if e.player_attacked ~= 0 then
        return
    end
    if e.player_displacement < 11000 then
        local px, _, pz = e:player_pos()
        e.angle = e.angle + as_s16(e:turn_toward_target(px, pz, 4))
    end
    if e.player_displacement < 10000 then
        local px, _, pz = e:player_pos()
        if e:turn_toward_target(px, pz, 0x80) == 0 and e.player_attacked == 0 then
            e.state_word = 0x00010101
            e.action_behavior = e.action_behavior + (e:random() & 1)
        end
    end
    if e.player_displacement < 9000 then
        local px, _, pz = e:player_pos()
        if e:turn_toward_target(px, pz, 0x100) == 0 and e.player_attacked == 0 then
            e.state_word = 0x00030101
            e.action_behavior = ACTIONS_DEFAULT[(e:random() & 7) + 1]
        end
    end
end

-- The idle selector the state check runs (same tree, the two flag-dependent
-- tables).
local function action_dispatch(e)
    e.player_displacement = e.p42_dist
    local kind = e.behavior_flags & 7
    if kind == 4 then
        return
    end
    local table = (kind == 5) and ACTIONS_FLAGS5 or ACTIONS_DEFAULT
    if e.player_attacked ~= 0 then
        return
    end
    if e.player_displacement < 11000 then
        local px, _, pz = e:player_pos()
        e.angle = e.angle + as_s16(e:turn_toward_target(px, pz, 4))
    end
    if e.player_displacement < 10000 then
        local px, _, pz = e:player_pos()
        if e:turn_toward_target(px, pz, 0x80) == 0 and e.player_attacked == 0 then
            e.state_word = 0x00010101
            e.action_behavior = e.action_behavior + (e:random() & 1)
        end
    end
    if e.player_displacement < 9000 then
        local px, _, pz = e:player_pos()
        if e:turn_toward_target(px, pz, 0x100) == 0 and e.player_attacked == 0 then
            e.state_word = 0x00030101
            e.action_behavior = table[(e:random() & 7) + 1]
        end
    end
end

-- The hit-reaction behaviour (SCD table entry 2's payload).
recoil = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.animation_id = HIT_ANIMS[(e:random() & 7) + 1]
        e.p42_step = as_s16((e:random() & 1) * -0xC0 + 0x60)
        e.p42_grab_joint = 8
        e.p42_ticks = as_s16((e:random() & 7) + 1)
        if (e.behavior_flags & 1) == 0 then
            e.p42_ticks = 6
        end
        if (e:random() & 1) ~= 0 then
            e.blend_counter = 7
            e.action_state = 3
            e.animation_id = 0
            joint_sound(e, 2, 1, 14)
            return
        end
    end

    if st == 0 or st == 1 then
        if ((e.p42_step + 0x60) & 0xFFFF) > 0xC0 then
            e.p42_grab_joint = -e.p42_grab_joint
        end
        e.p42_step = e.p42_step - e.p42_grab_joint
        if e.p42_step == 0 then
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 0x3F
            e.animation_id = HIT_ANIMS[(e:random() & 7) + 1]
            e.p42_ticks = e.p42_ticks - 1
            if e.p42_ticks == 0 then
                e.action_state = 2
            end
        end
        if e.player_weapon > 0x6E and (e.animation_frame_id % 5) == 0 then
            e.hit_state = 0
        end
        if e.roll < -0x17C then
            e.roll = e.roll + 0x10
        end
        e.angle = e.angle - e.p42_step
        e:plant42_advance(false, 0x40)
    elseif st == 2 then
        e.state_word = 1
        e.hit_state = 0
        e.animation_id = 2
        if (e.behavior_flags & 1) == 0 and e:plant42_body() ~= nil then
            e:plant42_body_react_clear()
            if (e:plant42_body_vines() or 0) > 1 then
                e.hit_state = 1
            end
        end
        if (e.behavior_flags & 0x40) ~= 0 then
            e.state_word = 8
            return
        end
    elseif st == 3 then
        e:plant42_advance(false, 0x200)
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x40)
        if e.roll > -0x254 then
            e.roll = e.roll - 0x40
        end
        if e.blend_counter == 0 then
            e.action_state = 1
            return
        end
    end
end

-- The SCD-driven flinch (SCD table entry 16).
scd_flinch = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.animation_id = 0x0D
        e.p42_ticks = 3
    end

    if st == 0 or st == 1 then
        if (e.hit_state & 7) ~= 0 then
            e.roll = e.roll - 8
        end
        if (e.behavior_flags & 0x40) ~= 0 and (e.animation_frame_id & 7) == 0 then
            local joint = e:random() % 5
            e:spawn_joint_effect(0x0E, 3, joint, 0)
            e:spawn_joint_effect(9, 0, joint, 0)
            e.p42_fx_timer_b = 10
        end
        e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
    elseif st == 2 then
        if (e.hit_state & 7) ~= 0 then
            e.roll = e.roll + 8
        end
        if (e.behavior_flags & 0x40) ~= 0 and (e.animation_frame_id & 7) == 0 then
            local joint = e:random() % 5
            e:spawn_joint_effect(0x0E, 3, joint, 0)
            e:spawn_joint_effect(9, 0, joint, 0)
        end
        e.action_state = e.action_state + (e:plant42_advance(true, 0x40) and 1 or 0)
        return
    elseif st == 3 then
        e.state_word = 1
        e.hit_state = 0
        if (e.behavior_flags & 1) == 0 and e:plant42_body() ~= nil then
            e:plant42_body_react_clear()
            if (e:plant42_body_vines() or 0) > 1 then
                e.hit_state = 1
            end
        end
        if (e.behavior_flags & 0x40) ~= 0 then
            e.state = 8
            return
        end
    end
end

-- The death fall (SCD table entry 14's payload).
death = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.animation_id = 0x0D
        e.joint_scale = 0x1000
        e.p42_ticks = 0
        e.p42_pod_counter = 5
        e.p42_death_counter = as_s8((e:random() % 0xE) + 0x0F)
        e.p42_step = as_s16((e:random() & 7) + 8)
        e.p42_step = as_s16(((e:random() & 1) * -2 + 1) * e.p42_step)
        e.p42_osc_a = (e:random() & 1) << 11
        e.p42_osc_b = (e:random() & 1) * 0x800 + 0x400
        e.p42_yaw_jitter = as_s8((e:random() & 1) * -0x40 + 0x20)
        e.p42_roll_jitter = as_s8(as_s8(e:random()) * -0x80 + 0x40)
        if (e.behavior_flags & 1) == 0 then
            e:raise_death_event()
            e:play_enemy_sound(7)
            e:spawn_effect(0, 0x1B, 0, 0, 0, 0, 0)
        end
        joint_sound(e, 2, 9, 9)
    end

    if st == 0 or st == 1 then
        e:plant42_advance(false, 0x40)
        e:plant42_advance(false, 0x40)
        e.roll = e.roll + 0x10
        e.joint_scale = e.joint_scale - 0x10
        local c = e.p42_death_counter
        e.p42_death_counter = as_s8(c - 1)
        if c == 0 then
            e.action_state = 2
            return
        end
    elseif st == 2 then
        e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
        e.action_state = e.action_state + (e:plant42_advance(false, 0x40) and 1 or 0)
    end

    if st == 2 or st == 3 then
        e.animation_id = 2
        if e.joint_scale > 1000 then
            e.joint_scale = e.joint_scale - 8
            local entity_y = as_s16(e.pos_y)
            local j7 = as_s16(e:joint_world_y(7))
            local j14 = as_s16(e:joint_world_y(14))
            if 100 < ((j7 - entity_y + 0x32) & 0xFFFF)
                and 100 < ((j14 - entity_y + 0x32) & 0xFFFF)
            then
                local pitch = as_s16(e.pitch)
                if 100 < ((pitch - e.p42_osc_b + 0x32) & 0xFFFF) then
                    e.pitch = pitch + e.p42_roll_jitter
                end
                if 100 < ((as_s16(e.angle) - e.p42_osc_a + 0x32) & 0xFFFF) then
                    e.angle = e.angle + e.p42_yaw_jitter
                end
            end
            e.pos_y = e.pos_y + e.p42_ticks
            e.p42_ticks = e.p42_ticks + 5
            if e.pos_y > -200 then
                e.p42_ticks = as_s16(idiv(e.p42_ticks, -3))
                e.pos_y = -200
                local b = e.p42_pod_counter
                e.p42_pod_counter = as_s8(b - 1)
                if b == 0 then
                    e.action_state = 4
                    e.hit_state = 0x80
                end
                if e.p42_pod_counter == 4 then
                    joint_sound(e, 2, 8, 10)
                    e:spawn_effect(0, 0x1B, 0, 0, 0, 0, 0)
                    if (e.behavior_flags & 0x40) == 0 then
                        e:spawn_joint_effect_at(9, 0x11, 2, 0, -400, 0, 0)
                        e:spawn_joint_effect_at(9, 0x11, 7, 0, -400, 0, 0)
                        e:spawn_joint_effect_at(9, 0x11, 13, 0, -400, 0, 0)
                    end
                    e:spawn_joint_effect_at(9, 0x11, 4, 0, -400, 0, 0)
                    e:spawn_joint_effect_at(9, 0x11, 9, 0, -400, 0, 0)
                    e:spawn_joint_effect_at(9, 0x11, 14, 0, -400, 0, 0)
                end
            end
            e:plant42_advance(false, 0x40)
            e:plant42_advance(false, 0x40)
        end
    elseif st == 4 then
        if e.joint_scale < 0x12D then
            if (e.behavior_flags & 1) ~= 0 then
                e.has_enter_switch_zone = 0
            end
            e.action_state = 5
            return
        end
        e.joint_scale = e.joint_scale - 0x20
        e:adjust_shadow_size(-21, -1)
        local entity_y = as_s16(e.pos_y)
        local j7 = as_s16(e:joint_world_y(7))
        local j14 = as_s16(e:joint_world_y(14))
        if 100 < ((j7 - entity_y + 0x32) & 0xFFFF)
            and 100 < ((j14 - entity_y + 0x32) & 0xFFFF)
        then
            local pitch = as_s16(e.pitch)
            if 100 < ((pitch - e.p42_osc_b + 0x32) & 0xFFFF) then
                e.pitch = pitch + e.p42_roll_jitter
            end
            if 100 < ((as_s16(e.angle) - e.p42_osc_a + 0x32) & 0xFFFF) then
                e.angle = e.angle + e.p42_yaw_jitter
            end
        end
        e:plant42_advance(false, 0x40)
        e:plant42_advance(false, 0x40)
    end
end

-- The ambient room effects shared by state 1 and state 2.
ambient_effects = function(e)
    e.p42_life = e.p42_life - 1
    if e.p42_life == 0 then
        e.p42_life = (e:random() & 0x3F) + 0x32
    end

    if (e.behavior_flags & 1) == 0 then
        if e.p42_life == 1 and e.p42_dist > 0x1964 then
            local px, _, pz = e:player_pos()
            local x = (e:random() & 0x1FF) + px - 0x100
            local z = (e:random() & 0x1FF) + pz - 0x100
            if (e:random() & 1) == 0 then
                e:spawn_world_effect(0x1E, 1, x, -10000, z, 0)
            end
        end
        if e.p42_life < 0x1E and e.p42_dist > 0x1964 and (e.p42_life % 4) == 0 then
            local px, _, pz = e:player_pos()
            local x = px - (e:random() & 0x1FF) + 0x100
            local z = pz - (e:random() & 0x1FF) + 0x100
            e:spawn_world_effect(0, 0x1C, x, -10000, z, 0)
        end
    end

    if (e.p42_life % 0x32) == 0 then
        local x = EFFECT_X[(e:random() & 1) + 1] + (e:random() & 0x7FF) - 0x400
        local z = EFFECT_Z[(e:random() & 1) + 1] + (e:random() & 0x7FF) - 0x400
        e:spawn_world_effect(0, 0x1C, x, -10000, z, 0)
    end
end

-- The last-vine payoff.
local function award_kill(e)
    e:plant42_award_kill()
end

-- ---------------------------------------------------------------------------
-- States.
-- ---------------------------------------------------------------------------

-- State 0: one-time setup.
local function init(e)
    e.state_word = 1
    -- The original clears a death-screen latch here; the port has no death
    -- screen consumer.
    e.has_enter_switch_zone = 1
    e.hit_state = 0
    e:reset_joints()
    e:set_shadow_offset(0, 0, 0)
    e.shadow_tint = 0x404040
    e.shadow_half_x = 2000
    e.shadow_half_z = 300
    e:random()
    e.health = 0x28
    if (e.behavior_flags & 1) == 0 then
        e.health = e.health + 100
    end
    e.animation_frame_id = 0
    e.timing_control = 0
    e.animation_id = 2
    e:advance_anim(0x40)
    if (e.behavior_flags & 1) == 0 then
        e:set_sca(SCA_BOSS_RADIUS, SCA_BOSS_HALF_HEIGHT, 0, 0, 0)
    else
        e:set_sca(SCA_SPLIT_RADIUS, SCA_SPLIT_HALF_HEIGHT, 0, 0, 0)
    end
    e.p42_step = 0
    e.p42_fx_timer_a = 0
    e.p42_fx_timer_b = 0
    e.p42_life = as_s16(e:random() & 0xFF) + 0x32

    if (e.behavior_flags & 1) == 0 then
        e.hit_state = 1
        e:set_sca(SCA_BOSS_RADIUS, SCA_BOSS_HALF_HEIGHT, 0, 0, 0)
        e:set_sca_hit_point(0, 0, 0)
        e:plant42_spawn()
        if (e.behavior_flags & 0xF) == 8 then
            e:tint_model(-1, -2, 0, 0, 0x200)
        end
    end

    if (e.behavior_flags & 0x40) ~= 0 then
        e.state = 8
    end
    if e.behavior_flags == 4 then
        e.state = 4
        e:tint_model(-4, -5, 0, 0, 0x200)
    end
    if (e.behavior_flags & 0xF) == 6 then
        e.state_word = 0x000B0108
        suspend(e)
    end
    if e.behavior_flags == 5 then
        e.joint_scale = 0x12C0
    end
end

-- State 1: the decision layer.
local function state_check(e)
    e.status_flags = (e.status_flags & 0x1F) | 0x40
    local px, _, pz = e:player_pos()
    e.p42_dist = e:xz_distance_to(px, pz)
    if e.p42_dist < 10000 then
        e.status_flags = e.status_flags | 0x80
    end

    if e.ignore == 0 then
        local body_hit = e:plant42_body_hit()
        if body_hit ~= nil and body_hit ~= 0 then
            e.state_word = 0x00000102
            if (e:random() & 3) == 0 then
                e.action_behavior = 1
            end
            if (e.behavior_flags & 1) == 0 then
                return
            end
            if (e:random() & 3) == 0 then
                return
            end
            e.state_word = 0x00010101
            if (e:random() & 3) == 0 then
                e.action_behavior = e.action_behavior + 1
            end
            return
        end
        action_dispatch(e)
    end

    run_action(e)
    ambient_effects(e)
end

-- The action-table dispatcher.
run_action = function(e)
    local body_health = e:plant42_body_health()
    if body_health ~= nil and body_health < 0 then
        e.state_word = 0x00000103
        e.hit_state = 1
        e.health = -1
        return
    end

    local b = e.action_behavior
    if b == 0 then
        idle(e)
    elseif b == 1 then
        sweep(e)
    elseif b == 2 then
        spit(e)
    elseif b == 3 then
        move(e)
    elseif b == 4 then
        hold(e)
    elseif b == 5 then
        grab_alt(e)
    elseif b == 6 then
        suspend(e)
    elseif b == 7 then
        release(e)
    elseif b == 8 then
        kill(e)
    elseif b == 9 then
        wither(e)
    elseif b == 14 then
        -- no-op
    else
        action_select(e)
    end
end

-- State 2: the damage reaction driver.
local function damage_react(e)
    local body_health = e:plant42_body_health()
    if body_health ~= nil and body_health < 0 then
        e.state_word = 0x00000103
        e.hit_state = 1
        e.health = -1
        return
    end

    if e.ignore ~= 0 or (e.hit_state & 7) == 0 then
        if e.action_behavior == 0 then
            recoil(e)
        elseif e.action_behavior == 1 then
            scd_flinch(e)
        end
        return
    end

    e.hit_state = (e.hit_state & 0xF8) | 1
    e.state_word = 0x00000102
    if (e:random() & 1) ~= 0 then
        e.action_behavior = 1
    end

    local body = e:plant42_body()
    if (e.hit_state & 0x78) == 8 then
        -- The heavy-weapon billboard anchors on the player's joint 14; the
        -- port keeps the reaction state and defers the per-joint sprite.
        e:spawn_player_effect(0, 0x18, 0, 200, 0, 0)
    elseif body == nil or ((e:plant42_body_hit() or 0) & 7) == 0 then
        local joint = 7 + (e:random() & 7)
        e:spawn_joint_effect(0, 0x1B, joint, 0)
        local px, _, pz = e:player_pos()
        local nx, _, nz = e:vector_normal(px - e.pos_x, e.pos_y, pz - e.pos_z)
        e:spawn_world_effect(
            0,
            0x18,
            e.pos_x + (nx // 2),
            e.pos_y + 0x1194,
            e.pos_z + (nz // 2),
            0
        )
    end

    if (e:random() & 1) ~= 0 and (e.behavior_flags & 1) ~= 0 and e.health < 10 then
        e.state_word = 0x00000103
        e.health = -1
        if body ~= nil then
            local remaining = (e:plant42_body_vines() or 0) - 1
            e:set_plant42_body_vines(remaining)
            if remaining ~= 1 then
                return
            end
            award_kill(e)
        end
        return
    end

    if body ~= nil then
        if (e:plant42_body_vines() or 0) < 2 then
            e.hit_state = 0
        end
        e:plant42_body_react()
    end
end

local function damaged(e)
    damage_react(e)
    ambient_effects(e)
end

-- State 3.
local function die(e)
    if e.ignore == 0 then
        e.state_word = 0x00000103
        local body = e:plant42_body()
        if body ~= nil then
            local remaining = (e:plant42_body_vines() or 0) - 1
            e:set_plant42_body_vines(remaining)
            if remaining == 1 then
                award_kill(e)
            end
        end
        if (e.behavior_flags & 1) == 0 then
            e:plant42_body_kill()
        end
    end
    if e.action_behavior == 0 then
        death(e)
    end
end

-- State 8: the SCD action table (its behavior values do not index the normal
-- table).
local function scd_state(e)
    e.status_flags = e.status_flags & 0x1F

    local b = e.action_behavior
    if b == 0 then
        idle(e)
    elseif b == 2 then
        local px, _, pz = e:player_pos()
        local step = as_s16(e:turn_toward_target(px, pz, 0x20))
        e.player_displacement = step
        e.angle = e.angle + step
        if e.player_displacement == 0 then
            e:raise_scd_flag()
        end
        idle(e)
    elseif b == 10 then
        grab_alt(e)
    elseif b == 11 then
        suspend(e)
    elseif b == 12 then
        release(e)
    elseif b == 13 then
        if e.action_state == 0 then
            e.p42_fx_timer_b = 10
        end
        recoil(e)
    elseif b == 14 then
        if e.ignore == 0 then
            e.state_word = 0x000E0108
            local body = e:plant42_body()
            if body ~= nil then
                local remaining = (e:plant42_body_vines() or 0) - 1
                e:set_plant42_body_vines(remaining)
                if remaining == 1 then
                    e:set_plant42_body_vines(remaining - 1)
                end
            end
            if (e.behavior_flags & 1) == 0 then
                e:plant42_body_kill()
            end
        end
        death(e)
    elseif b == 15 then
        wither(e)
    elseif b == 16 then
        scd_flinch(e)
    end

    local fx = e.p42_fx_timer_b
    if fx ~= 0 then
        e.p42_fx_timer_b = fx - 1
        if fx == 1 then
            local body = e:plant42_body()
            e:random()
            local y = (e:random() & 0xFFF) + 0x200
            if body ~= nil and (e:plant42_body_y() or 0) > -2000 then
                y = 0
            end
            local x = 0x200 - (e:random() & 0x3FF)
            local z = 0x200 - (e:random() & 0x3FF)
            if body ~= nil then
                e:spawn_companion_effect(body, 0x0E, 3, x, y, z, 0x28)
                e:spawn_companion_effect(body, 9, 0, x, y, z, 0x28)
            end
            e.p42_fx_timer_b = 0x0D
            if body ~= nil then
                local bx, by, bz = e:companion_pos(body)
                e:play_3d_sound_at(2, 0x1E, bx, by, bz)
                if by > -300 then
                    e.p42_fx_timer_b = 0
                end
            end
        end
    end

    if (e.behavior_flags & 0x40) == 0 then
        e.state_word = 1
    end
end

-- ---------------------------------------------------------------------------
-- The per-frame entry.
-- ---------------------------------------------------------------------------

function update(e)
    if not e.monster_paused then
        -- A withering state-8 boss re-forced by the room's SCD handover.
        if (e.behavior_flags & 0x40) ~= 0 and (e.state == 1 or e.state == 2) then
            local withering = e.mp_step_word == 0x0309
            e.state_word = 0x030F0008
            if not withering then
                e.state_word = 8
            end
        end

        local state = e.state
        if state == 0 then
            init(e)
        elseif state == 1 then
            state_check(e)
        elseif state == 2 then
            damaged(e)
        elseif state == 3 then
            die(e)
        elseif state == 8 then
            scd_state(e)
        end

        -- Keep the SCA hit box glued to the head joint.
        e:set_sca_hit_point(
            e:joint_world_x(14) - e.pos_x,
            e:joint_world_y(14) - e.pos_y,
            e:joint_world_z(14) - e.pos_z
        )
    end

    if (e.behavior_flags & 1) == 0 then
        e:plant42_tick()
    end

    if e.behavior_flags == 4 then
        e.has_enter_switch_zone = 0
        return
    end

    if (e.hit_state & 0x80) == 0 then
        local y = e:joint_world_y(11)
        e.shadow_half_x = idiv(y, 7) + 2000
        e.shadow_half_z = idiv(y, 0x46) + 300
    end
    e:set_shadow_offset(
        e:joint_world_x(11) - e.pos_x,
        e:joint_world_y(11) - e.pos_y,
        e:joint_world_z(11) - e.pos_z
    )
end
