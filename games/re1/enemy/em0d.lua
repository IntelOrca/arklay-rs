-- Yawn, entity ids 0x0d (the first fight) and 0x12 (the rematch).
--
-- One model with fifteen joints and thirteen enemy slots. This script runs
-- the head: the state byte dispatches init / state check / damaged / die /
-- wait, the action-behavior byte selects one of nine action handlers, and the
-- state check's selector picks the next action from the range gates. The
-- head's skeleton is posed by its own fixed-point chain animator
-- (`e:yawn_pose_init`, `e:yawn_anim`, `e:yawn_post_move`), not the shared
-- clock: joints 0-2 take the animation directly, joints 3-14 stack yaw and
-- roll onto the previous joint and anchor the body on the init ground height.
--
-- The twelve body segments live in ordinary entity slots created by
-- `e:yawn_spawn_segments`; each carries `behavior_flags == 1` and mirrors the
-- joint its link names. Their whole per-frame branch is Rust
-- (`e:yawn_segment_tick`): mirror the joint, room collision, chain drag and
-- the head's damage routing. `e:yawn_set_status_range` is the head's batched
-- status writer over the thirteen slots (0 is the head, 1..12 the segments).
--
-- `behavior_flags & 7` selects the decision tree: 0/2 the free-roaming
-- selector, 4/6 the scripted one; `& 0xF` 0/4 is the scripted entrance (the
-- head starts hidden in the ceiling and behaviour 5 crawls it in), 2/6 an
-- already-in-room form, and `& 0x80` parks the state machine in state 4.
--
-- All state lives on the Rust entity and its chain pose; this file keeps
-- none, so the scripting VM may be reset between any two updates.

local SCA_RADIUS = 800
local SCA_HALF_HEIGHT = 2000
local SCA_OFFSET_Y = -0x7D0

-- The bite damage window, two bytes per form: {first frame, length}. The
-- grown form bites later and for one frame less.
local BITE_WINDOW = {
    { 13, 6 },
    { 18, 5 },
}

-- The patrol path walked by behaviour 5 (the scripted entry crawl).
local ENTRY_PATH = {
    { 4500, 24500 },
    { 11000, 24500 },
    { 10500, 14000 },
    { 8500, 11500 },
    { 6800, 14200 },
    { 3800, 8400 },
}

-- The flee path walked by behaviour 7 (low health, the snake leaves). Step 9
-- raises behavior_flags 0x80, which parks the state machine in state 4.
local FLEE_PATH = {
    { 7300, 10000 },
    { 12000, 12000 },
    { 11000, 15500 },
    { 11000, 25000 },
    { 11000, 25000 },
    { 6500, 25000 },
    { 2500, 26000 },
    { 2500, 31000 },
    { 15000, 31000 },
    { 0, 0 },
}

-- The reposition path walked by behaviour 8 (wall-stuck recovery).
local REPOSITION_PATH = {
    { 6500, 7500 },
    { 11500, 19500 },
    { 7500, 24500 },
    { 11500, 24500 },
}

-- Dust-puff offsets in the head joint's local frame; the Z component is
-- mirrored at random so both sides of the body kick up dust.
local DUST_OFFSET = {
    { 300, 400 },
    { 600, 100 },
    { 800, 50 },
}

-- The ceiling burst's ten sprite pairs (emerge case 3).
local EMERGE_TYPE = { 9, 9, 0x0E, 0x0E, 9, 0x0E, 0x0E, 0x0E, 0x0E, 0x11 }
local EMERGE_DEPTH = { 0x13, 0x15, 0x13, 0x11, 0x15, 0x13, 0x11, 0x13, 0x11, 0x11 }

local function abs(value)
    if value < 0 then
        return -value
    end
    return value
end

-- Sign-extend a stored 8-bit value for the signed compares the original
-- compiles at those sites.
local function as_s8(value)
    value = value & 0xFF
    if value >= 0x80 then
        return value - 0x100
    end
    return value
end

-- Sign-extend a stored 16-bit value.
local function as_s16(value)
    value = value & 0xFFFF
    if value >= 0x8000 then
        return value - 0x10000
    end
    return value
end

-- Truncating division (C's integer divide rounds toward zero; Lua's `//`
-- floors).
local function idiv(a, b)
    local q = a // b
    if a % b ~= 0 and ((a < 0) ~= (b < 0)) then
        q = q + 1
    end
    return q
end

-- The Manhattan distance between the player and this entity, the original's
-- shared `g_playerDisplacement` scratch.
local function manhattan(e)
    local px, _, pz = e:player_pos()
    return abs(px - e.pos_x) + abs(pz - e.pos_z)
end

-- A ground dust puff, positioned in the head joint's frame.
local function spawn_dust(e, side)
    local offset = DUST_OFFSET[side + 1]
    local r = e:random()
    local z = (1 - (r & 2)) * offset[2]
    e:spawn_joint_effect_lit(0, 4, 1, offset[1], 0x32, z, 0, 0x14)
end

-- `yawn_turn_accel`: the shared "swim" acceleration. The accelerator byte
-- doubles until it reaches 0x40; when the speed leaves the +/-900 band it is
-- divided by 0x40, negated and the slither cue plays.
local function turn_accel(e)
    local speed = as_s16(e.yawn_speed)
    if ((speed + 900) & 0xFFFF) > 0x708 then
        e.yawn_speed = as_s16(speed - as_s8(e.yawn_turn_accel))
        local accel = idiv(as_s8(e.yawn_turn_accel), 0x40)
        e.yawn_turn_accel = as_s8(-accel)
        e:play_enemy_sound(0)
    end
    local accel = as_s8(e.yawn_turn_accel)
    if (accel % 0x40) ~= 0 then
        e.yawn_turn_accel = as_s8(accel * 2)
    end
    e.yawn_speed = as_s16(e.yawn_speed + as_s8(e.yawn_turn_accel))
end

local idle
local move
local bite
local rear
local swallow
local entry
local emerge
local flee
local reposition
local run_action
local damaged_run
local die_run

-- The free-roaming decision tree. Every clause writes the whole state word,
-- so the last matching one wins; the order is the priority order.
local function pick_normal(e)
    local px, _, pz = e:player_pos()
    e:rotate_toward_target(px, pz, 0x30)

    if e.player_attacked == 0 then
        local los = e:line_of_sight()

        -- Close enough to bite outright.
        if e.player_displacement < 2000 and los == 0 and e.player_health > 9 then
            e.state_word = 0x00020101
        end
        -- Or inside the form's lunge range and already lined up.
        if e.player_displacement < (e.yawn_form * 5 + 0x19) * 200
            and los == 0 and e.player_health > 9
        then
            if as_s16(e:turn_toward_target(px, pz, 0x200)) == 0 then
                e.state_word = 0x00020101
            end
        end
        -- Too many bites in a row: rear up.
        if e.player_displacement < 7000 and as_s8(e.yawn_bites) > 4 then
            e.state_word = 0x00030101
        end
        -- Grown form, player nearly dead, lined up and off cooldown: swallow.
        if e.player_displacement < 4000 and e.yawn_form ~= 0 and los == 0 then
            if as_s16(e:turn_toward_target(px, pz, 0x40)) == 0
                and e.player_health - 10 < 0
                and as_s8(e.yawn_bite_cool) == 0
            then
                e.state_word = 0x00040101
            end
        end
        -- Juvenile form with a nearly-dead player: grow first.
        if e.player_health - 10 < 0 and e.yawn_form == 0 then
            e.yawn_nearly_dead = 1
            e.state_word = 0x00030101
        end
        -- Grown form pushed too far back in the room while biting: shrink.
        if e.pos_z > 17000 and e.yawn_form ~= 0 and e.action_behavior == 2 then
            e.state_word = 0x00030101
            e.yawn_nearly_dead = 0
        end
        -- Wedged against geometry: reposition.
        if as_s8(e.yawn_stuck) > 0x96 then
            e.state_word = 0x00080101
        end
        -- Player poisoned but healthy: shrink back to the juvenile form.
        if (e.player_health_status & 8) ~= 0
            and e.yawn_form ~= 0 and e.player_health - 10 >= 0
        then
            e.state_word = 0x00030101
            e.yawn_nearly_dead = 0
        end
        -- Low health: flee.
        if e.health < 0xAF0 then
            e.state_word = 0x00070101
        end
    end

    turn_accel(e)
end

-- The cutscene-spawned Yawn: the same tree minus the four clauses that would
-- end the fight on their own.
local function pick_scripted(e)
    local px, _, pz = e:player_pos()
    e:rotate_toward_target(px, pz, 0x30)

    if e.player_attacked == 0 then
        local los = e:line_of_sight()
        if e.player_displacement < 2000 and los == 0 and e.player_health > 9 then
            e.state_word = 0x00020101
        end
        if e.player_displacement < (e.yawn_form * 5 + 0x19) * 200
            and los == 0 and e.player_health > 9
        then
            if as_s16(e:turn_toward_target(px, pz, 0x200)) == 0 then
                e.state_word = 0x00020101
            end
        end
        if e.player_displacement < 7000 and as_s8(e.yawn_bites) > 4 then
            e.state_word = 0x00030101
        end
        if e.player_displacement < 4000 and e.yawn_form ~= 0 and los == 0 then
            if as_s16(e:turn_toward_target(px, pz, 0x40)) == 0
                and e.player_health - 10 < 0
                and as_s8(e.yawn_bite_cool) == 0
            then
                e.state_word = 0x00040101
            end
        end
        if e.player_health - 10 < 0 and e.yawn_form == 0 then
            e.yawn_nearly_dead = 1
            e.state_word = 0x00030101
        end
    end

    turn_accel(e)
end

-- The selector: Manhattan distance to the player, then dispatch on
-- `behavior_flags & 7`.
local function select_behavior(e)
    e.player_displacement = manhattan(e)
    local kind = e.behavior_flags & 7
    if kind == 0 or kind == 2 then
        pick_normal(e)
    elseif kind == 4 or kind == 6 then
        pick_scripted(e)
    end
end

-- Behaviour 0: idle.
idle = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 1
        e.blend_counter = 7
        e.yawn_speed = 0
        e.action_ticks_counter = (e:random() & 0xF) + 0xF
    end

    e:yawn_anim(false, 0x200)

    local ticks = as_s16(e.action_ticks_counter)
    e.action_ticks_counter = as_s16(ticks - 1)
    if ticks == 0 then
        e.action_behavior = 1
        e.action_state = 0
    end

    e:yawn_post_move(0x30)
end

-- Behaviour 1: move.
move = function(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = e.yawn_form + 1
        e.blend_counter = 7
        e.yawn_speed = 0
        e.move_speed_current = 0xA0

        -- The grown form switches to the fast slither once the player is
        -- nearly dead, has been chased off, or is recovering from a bite.
        if (e.player_health - 10 < 0 or e.yawn_nearly_dead ~= 0
                or (e.yawn_recovery & 0x80) ~= 0)
            and e.yawn_form ~= 0
        then
            e.animation_id = 5
        end
    end

    if (e.yawn_recovery & 0x7F) ~= 0 then
        e.yawn_recovery = e.yawn_recovery - 1
        if as_s8(e.yawn_recovery) == 0 then
            e.yawn_recovery = 0
            e.action_state = 0
        end
    end

    if e.animation_id == 5
        and (e.animation_frame_id == 0x0F or e.animation_frame_id == 0x28)
    then
        e:play_enemy_sound(1)
    end

    e:yawn_anim(false, 0x200)

    if as_s8(e.yawn_hiss) ~= 0 and (e.animation_frame_id & 7) == 0 then
        local r = e:random()
        spawn_dust(e, r < 0 and -(-r & 1) or (r & 1))
    end

    e:move(0, as_s16(e.yawn_speed))
    e:yawn_post_move(0x30)
end

-- Behaviour 2: bite. States 0/1 are the lunge, 2/3 the recoil-and-turn, 4/5
-- the miss recovery. The damage window is BITE_WINDOW[form].
bite = function(e)
    local st = e.action_state

    local function lunge()
        local win = BITE_WINDOW[e.yawn_form + 1]
        if ((e.animation_frame_id - win[1]) & 0xFF) < win[2] then
            local reach = e:reach_test(0, 1000, 0, 0, 800)

            -- Already-wounded players get sprayed with blood on alternate
            -- frames.
            if e.player_action_behavior > 0x65 and (e.animation_frame_id & 1) ~= 0 then
                e:spawn_joint_effect_at(0, 0, 1, 1000, 0, 0, 0)
                e:spawn_player_effect(0, 0, 0, e.yawn_form * -0x5DC - 300, 0, 0)
            end

            if reach and e.player_attacked == 0 then
                local facing = e:player_facing_entity() and 1 or 0

                if e.second_playthrough then
                    e.player_health = e.player_health - 0x1C
                else
                    e.player_health = e.player_health - 10
                end
                if e.player_health < 0 then
                    e.player_health = 1
                end

                e.player_attacked = facing + 1
                e.player_action_behavior = facing + 0x66

                e:spawn_joint_effect_at(0, 0, 1, 1000, 0, 0, 0)
                e:tint_joint(1, 0x30, 0x80820, 0x606060)
                e.angle = e.angle + 0x100

                -- Only the first Yawn poisons, and only while the serum has
                -- not been taken.
                if e.id == 0x0D and not e:flag_bit(0, 0x47) then
                    e:raise_flag(1, 0x43)
                    e.player_health_status = e.player_health_status | 0x20
                end

                e.yawn_bite_dir = 0xFF
                e.yawn_recovery = 0
                e.yawn_stuck = 0
                e.yawn_hiss = 0xF0
                e:play_enemy_sound(4)
            end
        end

        e.action_state = e.action_state + (e:yawn_anim(false, 0x200) and 1 or 0)

        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x38)

        if e.yawn_form * 5 - e.animation_frame_id == -8 then
            if e.yawn_form == 0 or as_s8(e.yawn_stuck_cool) < 0xB then
                e.move_speed_current = 200
                e:play_enemy_sound(3)
            else
                -- Grown form biting into a wall: abandon the lunge, back off.
                e.ignore = 0
                e.action_behavior = 1
                e.action_state = 0
                e.yawn_bite_dir = 0
                e.yawn_bites = 5
                e.hit_state = 0
                e:rotate_toward_target(px, pz, 0xFFF0)
            end
        end
    end

    local function recoil()
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, (as_s8(e.yawn_bite_dir) << 4) & 0xFFFF)
        local ticks = as_s16(e.action_ticks_counter)
        e.action_ticks_counter = as_s16(ticks - 1)
        if ticks == 0 then
            e.ignore = 0
            e.action_behavior = 1
            e.action_state = 0
            e.yawn_bite_dir = 0
            e.hit_state = 0
        end
    end

    local function miss()
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, (as_s8(e.yawn_bite_dir) << 4) & 0xFFFF)
        if e:yawn_anim(false, 0x200) then
            e.ignore = 0
            e.action_behavior = 1
            e.action_state = 0
            e.yawn_bite_dir = 0
            e.hit_state = 0
        end
    end

    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = e.yawn_form * 10 + 3
        e.move_speed_current = 100
        e.yawn_bites = e.yawn_bites + 1
        e.yawn_bite_dir = 1
        e:play_enemy_sound(2)
        lunge()
    elseif st == 1 then
        lunge()
    elseif st == 2 then
        e.action_ticks_counter = 0x1E
        e.move_speed_current = 0x78
        e.action_state = e.yawn_form + 3
        recoil()
    elseif st == 3 then
        recoil()
    elseif st == 4 then
        e.action_state = 5
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 4
        e.blend_counter = 7
        e.move_speed_current = 0x78
        miss()
    elseif st == 5 then
        miss()
    end

    e:move(0, 0)
    e:yawn_post_move(0x30)
end

-- Behaviour 3: rear up / form change. Rearing while form 0 raises it to 1
-- and goes straight to the hiss; rearing while form 1 drops back to 0 and
-- returns to the move behaviour.
rear = function(e)
    local st = e.action_state

    local function hiss()
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x30)
        e:yawn_anim(e.yawn_form ~= 0, 0x200)
        if e.animation_frame_id == 0x0F or e.animation_frame_id == 0x28 then
            e:play_enemy_sound(1)
        end
        local ticks = as_s16(e.action_ticks_counter)
        e.action_ticks_counter = as_s16(ticks - 1)
        if ticks == 0 then
            e.ignore = 0
            e.action_behavior = 1
            e.action_state = 0
        end
    end

    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 4
        e.blend_counter = 7
        e.move_speed_current = 0x78
        e.yawn_bites = 0
        if as_s8(e.yawn_nearly_dead) ~= 0 and e.yawn_form ~= 0 then
            e.action_state = 3
        else
            local px, _, pz = e:player_pos()
            e:rotate_toward_target(px, pz, 0x30)
            e.action_state = e.action_state
                + (e:yawn_anim(e.yawn_form ~= 0, 0x200) and 1 or 0)
        end
    elseif st == 1 then
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x30)
        e.action_state = e.action_state
            + (e:yawn_anim(e.yawn_form ~= 0, 0x200) and 1 or 0)
    elseif st == 2 then
        if e.yawn_form == 0 then
            e.yawn_form = 1
            e.action_state = 3
        else
            e.yawn_form = 0
            e.ignore = 0
            e.action_behavior = 1
            e.action_state = 0
        end
    elseif st == 3 then
        e.action_state = 4
        e.timing_control = 0
        e.animation_id = 5
        e.blend_counter = 7
        e.move_speed_current = 0x46
        e.action_ticks_counter = 0x3C
        hiss()
    elseif st == 4 then
        hiss()
    end

    turn_accel(e)
    e:move(0, as_s16(e.yawn_speed))
    e:yawn_post_move(0x30)
end

-- Behaviour 4: swallow, the instant-death grab. action_state runs 0 -> 1
-- (the mouth-open lunge), then either releases or goes to 2/3 (the player is
-- held in the mouth and shaken), 4/5 (the swallow), 6/7 (the coil-and-settle).
swallow = function(e)
    local st = e.action_state

    -- The head's scale ramp tail: while the ramp is up, joint 3's world and
    -- joint 2's transform are scaled every frame.
    local function scale_tail()
        local ramp = as_s16(e.yawn_scale_ramp)
        if ramp ~= 0 then
            e:yawn_scale_worlds(3, 3, ramp - 300, ramp - 300, ramp - 300)
            e:yawn_scale_transform(2, ramp, ramp, ramp)
        end
    end

    local function tail()
        e:yawn_post_move(0x30)

        if e.action_state > 1 then
            if (e:random() & 1) == 0 then
                spawn_dust(e, 0)
            end
            if (e:random() & 1) == 0 then
                spawn_dust(e, 1)
            end
            if (e:random() & 1) == 0 then
                spawn_dust(e, 2)
            end
        end

        scale_tail()
    end

    local function grab_hold()
        e:yawn_anim(false, 0x200)

        local distance = manhattan(e)
        e.move_speed_current = 0xA0
        if distance < 0x9C4 then
            e.move_speed_current = 0
        end

        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x30)

        if e.animation_frame_id == 10 then
            e:play_enemy_sound(3)
        end

        if e.animation_frame_id == 0x14 then
            e.hit_state = 0
            if distance > 2000 then
                -- Player broke free before the mouth closed.
                e.player_state = 1
                e.player_anim_frame_id = 0
                e.player_action_behavior = 0
                e.player_action_state = 0
                e.ignore = 0
                e.action_behavior = 1
                e.action_state = 0
                e.player_flags = e.player_flags & 0xF9
                e.player_attacked = 0
                tail()
                return
            end

            e.action_state = 2
            e:yawn_set_status_range(0, 12, 0xFF, 4)
            e:set_entity_joint_flag(0, 0, e:entity_joint_flag(0, 0) & 0xFE)
            e:set_entity_joint_flag(0, 1, e:entity_joint_flag(0, 1) & 0xFE)
            e:set_entity_joint_flag(0, 9, e:entity_joint_flag(0, 9) & 0xFE)
            e:set_entity_joint_flag(0, 12, e:entity_joint_flag(0, 12) & 0xFE)
            e.move_speed_current = 0

            e:spawn_player_effect(0, 3, 0, 1000, 0, 0)
            e:spawn_player_effect(0, 0, 0, 1000, 0, 0)
            e:spawn_player_effect(0, 3, 0, 1000, 0, 0)
            e:spawn_player_effect(0, 0, 0, 1000, 0, 0)
            e:spawn_player_effect(0, 0, 0, 1000, 0, 0)
            e:spawn_player_effect(0, 0, 0, 1000, 0, 0)
            for k = 3, 8 do
                e:tint_joint(k, 0x70, 0x484860, 0xA0A0A0)
            end
            e:play_enemy_sound(5)
            e:play_3d_sound_at(2, (e.player_character & 1) + 0x17,
                e.pos_x, e.pos_y, e.pos_z)
        end

        e:move(0, 0)
    end

    if st == 0 then
        -- grab_start
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = 0x0E
        e.yawn_bites = e.yawn_bites + 1
        e.hit_state = 1
        e.status_flags = e.status_flags | 2
        e.player_flags = e.player_flags | 6
        e.player_state = 7
        e.player_anim_frame_id = 0x0D
        e.player_action_behavior = 0
        e.player_action_state = 0
        e.yawn_bite_cool = 0x1E
        grab_hold()
    elseif st == 1 then
        grab_hold()
    elseif st == 2 then
        e.action_state = 3
        e:yawn_capture_setup(0)
        e:yawn_capture_rotate_z(-200)
        e:set_yawn_capture_t(0x937, 700, 0)
        e.yawn_scale_ramp = 0x1388
        -- fall through to case 3
        e.player_zone_flags = e.player_zone_flags | 0x80
        e:yawn_hold_player()
        e.action_state = e.action_state + (e:yawn_anim(false, 0x200) and 1 or 0)

        if e.animation_frame_id > 0x42 and e.animation_frame_id < 0x4B then
            if e.animation_frame_id == 0x43 then
                e:play_enemy_sound(6)
            end
            if e.animation_frame_id == 0x4A then
                e:play_enemy_sound(7)
                e:play_enemy_sound(4)
            end

            e:yawn_capture_t_step(-0x78, -0x28, 0)
            e:set_entity_joint_flag(0, 2, e:entity_joint_flag(0, 2) & 0xFE)
            e:set_entity_joint_flag(0, 10, e:entity_joint_flag(0, 10) & 0xFE)
            e:set_entity_joint_flag(0, 13, e:entity_joint_flag(0, 13) & 0xFE)
            e:set_entity_joint_flag(0, 11, e:entity_joint_flag(0, 11) & 0xFE)
            e:set_entity_joint_flag(0, 14, e:entity_joint_flag(0, 14) & 0xFE)

            -- The original anchors these sprays on the player's own joints;
            -- the port has no player joint worlds, so they hang off the body
            -- matrix at the same offsets.
            e:spawn_player_effect(0, 3, 0, 500, 0, 0)
            e:spawn_player_effect(0, 0, 0, 500, 0, 0)
            e:spawn_player_effect(0, 3, 0, 500, 0, 0)
            e:spawn_player_effect(0, 0, 0, 500, 0, 0)
            e:spawn_player_effect(0, 0, 0, 500, 0, 0)
            e:spawn_player_effect(0, 0, 0, 500, 0, 0)

            local diff = (e.angle - e.player_angle) & 0xFFFFFFFF
            local yaw = (((diff - 0x800) & 0xFFFFFFFF) >> 3) & 0x1FF
            e:yawn_capture_rotate_y(yaw)
            e.yawn_scale_ramp = as_s16(e.yawn_scale_ramp + 100)
        end
        tail()
    elseif st == 3 then
        e.player_zone_flags = e.player_zone_flags | 0x80
        e:yawn_hold_player()
        e.action_state = e.action_state + (e:yawn_anim(false, 0x200) and 1 or 0)

        if e.animation_frame_id > 0x42 and e.animation_frame_id < 0x4B then
            if e.animation_frame_id == 0x43 then
                e:play_enemy_sound(6)
            end
            if e.animation_frame_id == 0x4A then
                e:play_enemy_sound(7)
                e:play_enemy_sound(4)
            end

            e:yawn_capture_t_step(-0x78, -0x28, 0)
            e:set_entity_joint_flag(0, 2, e:entity_joint_flag(0, 2) & 0xFE)
            e:set_entity_joint_flag(0, 10, e:entity_joint_flag(0, 10) & 0xFE)
            e:set_entity_joint_flag(0, 13, e:entity_joint_flag(0, 13) & 0xFE)
            e:set_entity_joint_flag(0, 11, e:entity_joint_flag(0, 11) & 0xFE)
            e:set_entity_joint_flag(0, 14, e:entity_joint_flag(0, 14) & 0xFE)

            -- The original anchors these sprays on the player's own joints;
            -- the port has no player joint worlds, so they hang off the body
            -- matrix at the same offsets.
            e:spawn_player_effect(0, 3, 0, 500, 0, 0)
            e:spawn_player_effect(0, 0, 0, 500, 0, 0)
            e:spawn_player_effect(0, 3, 0, 500, 0, 0)
            e:spawn_player_effect(0, 0, 0, 500, 0, 0)
            e:spawn_player_effect(0, 0, 0, 500, 0, 0)
            e:spawn_player_effect(0, 0, 0, 500, 0, 0)

            local diff = (e.angle - e.player_angle) & 0xFFFFFFFF
            local yaw = (((diff - 0x800) & 0xFFFFFFFF) >> 3) & 0x1FF
            e:yawn_capture_rotate_y(yaw)
            e.yawn_scale_ramp = as_s16(e.yawn_scale_ramp + 100)
        end
        tail()
    elseif st == 4 then
        e.action_state = 5
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = 4
        e.move_speed_current = 0
        e.player_health = -1
        -- fall through to case 5
        e.action_state = e.action_state + (e:yawn_anim(true, 0x200) and 1 or 0)
        e:move(0, 0)
        if e.animation_frame_id == 0x0F then
            e:yawn_set_status_range(0, 12, 0xFF, 4)
        end
        tail()
    elseif st == 5 then
        e.action_state = e.action_state + (e:yawn_anim(true, 0x200) and 1 or 0)
        e:move(0, 0)
        if e.animation_frame_id == 0x0F then
            e:yawn_set_status_range(0, 12, 0xFF, 4)
        end
        tail()
    elseif st == 6 then
        e.action_state = 7
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = 1
        e.move_speed_current = 0
        e.action_ticks_counter = 0x96
        e.status_flags = e.status_flags | 4
        -- fall through to case 7
        e:yawn_capture_t_step(-10, -6, 0)
        e:yawn_anim(false, 0x200)
        local ticks = as_s16(e.action_ticks_counter)
        e.action_ticks_counter = as_s16(ticks - 1)
        if ticks == 0 then
            e.action_state = 8
        end
        if as_s16(e.action_ticks_counter) == 0x6E then
            e:set_entity_joint_flag(0, 3, e:entity_joint_flag(0, 3) & 0xFE)
            e:set_entity_joint_flag(0, 4, e:entity_joint_flag(0, 4) & 0xFE)
            e:set_entity_joint_flag(0, 5, e:entity_joint_flag(0, 5) & 0xFE)
            e:set_entity_joint_flag(0, 6, e:entity_joint_flag(0, 6) & 0xFE)
            e:set_entity_joint_flag(0, 7, e:entity_joint_flag(0, 7) & 0xFE)
            e:set_entity_joint_flag(0, 8, e:entity_joint_flag(0, 8) & 0xFE)
        end
        tail()
    elseif st == 7 then
        e:yawn_capture_t_step(-10, -6, 0)
        e:yawn_anim(false, 0x200)
        local ticks = as_s16(e.action_ticks_counter)
        e.action_ticks_counter = as_s16(ticks - 1)
        if ticks == 0 then
            e.action_state = 8
        end
        if as_s16(e.action_ticks_counter) == 0x6E then
            e:set_entity_joint_flag(0, 3, e:entity_joint_flag(0, 3) & 0xFE)
            e:set_entity_joint_flag(0, 4, e:entity_joint_flag(0, 4) & 0xFE)
            e:set_entity_joint_flag(0, 5, e:entity_joint_flag(0, 5) & 0xFE)
            e:set_entity_joint_flag(0, 6, e:entity_joint_flag(0, 6) & 0xFE)
            e:set_entity_joint_flag(0, 7, e:entity_joint_flag(0, 7) & 0xFE)
            e:set_entity_joint_flag(0, 8, e:entity_joint_flag(0, 8) & 0xFE)
        end
        tail()
    elseif st == 8 then
        tail()
    end
end

-- Behaviour 5: scripted entry crawl. Walks ENTRY_PATH, hands over to
-- behaviour 6 at the end, and rains ambient dust in one of two room-sized
-- boxes depending on `behavior_flags & 0x3F`.
entry = function(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 1
        e.blend_counter = 7
        e.yawn_speed = 0
        e.move_speed_current = 0xBE
        if e.behavior_flags == 4 then
            e.move_speed_current = 0x82
            e.yawn_path_index = 3
        end
        e.action_ticks_counter = 2
        e.hit_state = 1
        if e.id == 0x0D then
            e:raise_flag(0, 0x10)
        end
    end

    local waypoint = ENTRY_PATH[e.yawn_path_index + 1]
    local tx, tz = waypoint[1], waypoint[2]
    e:rotate_toward_target(tx, tz, 0x30)

    if e:xz_distance_to(tx, tz) < 500 then
        local ticks = as_s16(e.action_ticks_counter)
        e.action_ticks_counter = as_s16(ticks - 1)
        if ticks == 0 then
            e.action_behavior = 6
            e.action_state = 0
        end
        e.yawn_path_index = e.yawn_path_index + 1
        e:play_enemy_sound(0)
    end

    if (e.behavior_flags & 0x3F) == 0 then
        local x = (e:random() & 0x1FF) + 0x10CC
        local y = -(e:random() & 0x3FF)
        local z = (e:random() & 0x3FF) + 0x5C94
        e:spawn_world_effect(9, 0x11, x, y, z, 0)
    else
        local x = (e:random() & 0x1FF) + 0x2904
        local y = -(e:random() & 0x3FF)
        local z = (e:random() & 0x3FF) + 0x2904
        e:spawn_world_effect(9, 0x11, x, y, z, 0)
    end

    e:yawn_anim(false, 0x200)
    e:move(0, as_s16(e.yawn_speed))
    e:yawn_post_move(0x30)
end

-- Behaviour 6: emerge / hiss. Raises the snake to form 1 and blows the
-- ceiling dust. A `behavior_flags == 0` Yawn skips straight to the hiss.
emerge = function(e)
    local st = e.action_state

    local function rear_body()
        e.action_state = e.action_state + (e:yawn_anim(false, 0x200) and 1 or 0)
        if e.yawn_form * 5 - e.animation_frame_id == -8 then
            e:play_enemy_sound(3)
            e.move_speed_current = 0xBE
        end
        if e.animation_frame_id > 0x17 then
            e.move_speed_current = 0x32
            for i = 1, 10 do
                local x = (e:random() & 0x7FF) + 2000
                local z = (e:random() & 0x7FF) + 0xE74
                e:spawn_world_effect(EMERGE_TYPE[i], EMERGE_DEPTH[i], x, 0, z, 0)
            end
        end
        e:move(0, 0)
        e:yawn_post_move(0x30)
    end

    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 4
        e.blend_counter = 7
        e.move_speed_current = 0
        e.yawn_form = 1
        e.yawn_path_index = 0
        if e:yawn_anim(false, 0x200) then
            e.action_state = 2
            if e.behavior_flags == 0 then
                e.action_state = 6
                return
            end
        end
    elseif st == 1 then
        if e:yawn_anim(false, 0x200) then
            e.action_state = 2
            if e.behavior_flags == 0 then
                e.action_state = 6
                return
            end
        end
    elseif st == 2 then
        e.action_state = 3
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = e.yawn_form * 10 + 3
        rear_body()
    elseif st == 3 then
        rear_body()
    elseif st == 4 then
        e.action_state = 5
        e.timing_control = 0
        e.animation_id = 4
        e.blend_counter = 7
        e.action_state = e.action_state + (e:yawn_anim(false, 0x200) and 1 or 0)
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x30)
    elseif st == 5 then
        e.action_state = e.action_state + (e:yawn_anim(false, 0x200) and 1 or 0)
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x30)
    elseif st == 6 then
        e.action_state = 7
        e.timing_control = 0
        e.animation_id = 5
        e.blend_counter = 7
        e.move_speed_current = 0x46
        e.action_ticks_counter = 0x5A
        -- fall through to case 7
        e:yawn_anim(e.yawn_form ~= 0, 0x200)
        if e.animation_frame_id == 0x0F or e.animation_frame_id == 0x28 then
            e:play_enemy_sound(1)
        end
        local ticks = as_s16(e.action_ticks_counter)
        e.action_ticks_counter = as_s16(ticks - 1)
        if ticks == 0 then
            e.ignore = 0
            e.action_behavior = 1
            e.action_state = 0
            e.hit_state = 0
            e:yawn_set_status_range(0, 12, 0xFB, 0)
        end
    elseif st == 7 then
        e:yawn_anim(e.yawn_form ~= 0, 0x200)
        if e.animation_frame_id == 0x0F or e.animation_frame_id == 0x28 then
            e:play_enemy_sound(1)
        end
        local ticks = as_s16(e.action_ticks_counter)
        e.action_ticks_counter = as_s16(ticks - 1)
        if ticks == 0 then
            e.ignore = 0
            e.action_behavior = 1
            e.action_state = 0
            e.hit_state = 0
            e:yawn_set_status_range(0, 12, 0xFB, 0)
        end
    end
end

-- Behaviour 7: flee. Picks the nearest entry point on FLEE_PATH, walks it and
-- switches the segments off in three batches; step 9 raises behavior_flags
-- 0x80, which parks the state machine for good.
flee = function(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 1
        e.blend_counter = 0x0F
        e.move_speed_current = 0xB4
        e.yawn_stuck = 0

        e.yawn_path_index = 4
        if e.pos_x < 9000 and e.pos_z < 23000 then
            e.yawn_path_index = 3
        end
        if e.pos_z < 0x3C8C then
            e.yawn_path_index = 2
        end
        if e.pos_z < 9000 then
            e.yawn_path_index = 0
        end
        if e.pos_x > 0x2580 then
            e.yawn_path_index = 1
        end

        e.yawn_form = 0
        e.yawn_bite_dir = 1
        e.yawn_turn_accel = 1
        e.yawn_speed = 0
        e:raise_death_event()
    end

    if e.yawn_path_index > 6 then
        local x = (e:random() & 0x1FF) + 0x10CC
        local y = -(e:random() & 0x3FF)
        local z = (e:random() & 0x7FF) + 0x58AC
        e:spawn_world_effect(9, 0x11, x, y, z, 0)
    end

    local waypoint = FLEE_PATH[e.yawn_path_index + 1]
    local tx, tz = waypoint[1], waypoint[2]
    e:rotate_toward_target(tx, tz, 0x30)

    if e:xz_distance_to(tx, tz) < 800 then
        e.yawn_path_index = e.yawn_path_index + 1
        if e.yawn_path_index == 1 then
            e.yawn_path_index = 2
        end
        e.yawn_speed = 0

        if e.yawn_path_index == 6 then
            e:yawn_set_status_range(0, 3, 0xFF, 4)
        end
        if e.yawn_path_index == 7 then
            e:yawn_set_status_range(4, 7, 0xFF, 4)
        end
        if e.yawn_path_index == 8 then
            e:yawn_set_status_range(8, 12, 0xFF, 4)
        end
        if e.yawn_path_index == 9 then
            e.behavior_flags = e.behavior_flags | 0x80
        end
    end

    if as_s8(e.yawn_stuck) > 0x96 then
        e.state_word = 0x00080101
    end

    e:yawn_anim(false, 0x100)
    if e.yawn_path_index < 4 then
        turn_accel(e)
    end
    e:move(0, as_s16(e.yawn_speed))
    e:yawn_post_move(0x38)
end

-- Behaviour 8: reposition. Picks the nearest of four open floor points,
-- crawls to it, then drops back to state 1 / behaviour 1.
reposition = function(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 1
        e.blend_counter = 0x0F
        e.move_speed_current = 0xB4
        e.yawn_stuck = 0

        e.yawn_path_index = 3
        if e.pos_x > 9000 then
            e.yawn_path_index = 2
        end
        if e.pos_z < 19000 then
            e.yawn_path_index = 1
        end
        if e.pos_z < 15000 then
            e.yawn_path_index = 0
        end

        e.yawn_form = 0
        e.yawn_bite_dir = 1
        e.yawn_turn_accel = 1
        e.yawn_speed = 0
    end

    if as_s8(e.yawn_stuck) > 0x96 then
        e.action_state = 0
    end

    local waypoint = REPOSITION_PATH[e.yawn_path_index + 1]
    local tx, tz = waypoint[1], waypoint[2]
    e:rotate_toward_target(tx, tz, 0x30)

    if e:xz_distance_to(tx, tz) < 800 then
        e.yawn_speed = 0
        e.state_word = 0x00010001
    end

    e:yawn_anim(false, 0x100)
    turn_accel(e)
    e:move(0, as_s16(e.yawn_speed))
    e:yawn_post_move(0x38)
end

-- The action-table dispatcher.
run_action = function(e)
    local behavior = e.action_behavior
    if behavior == 0 then
        idle(e)
    elseif behavior == 1 then
        move(e)
    elseif behavior == 2 then
        bite(e)
    elseif behavior == 3 then
        rear(e)
    elseif behavior == 4 then
        swallow(e)
    elseif behavior == 5 then
        entry(e)
    elseif behavior == 6 then
        emerge(e)
    elseif behavior == 7 then
        flee(e)
    elseif behavior == 8 then
        reposition(e)
    end
end

-- State 2's flinch (`yawn_damaged_run`).
damaged_run = function(e)
    local st = e.action_state

    local function recovery_turn()
        e:yawn_anim(false, 0x200)
        if e.animation_frame_id == 0x0F or e.animation_frame_id == 0x28 then
            e:play_enemy_sound(1)
        end
        local direction = 1
        if as_s16(e.action_ticks_counter) > 0x1E then
            direction = -1
        end
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, (direction * 0x30) & 0xFFFF)
        local ticks = as_s16(e.action_ticks_counter)
        e.action_ticks_counter = as_s16(ticks - 1)
        if ticks == 0 then
            e.state = 1
            e.ignore = 0
            e.action_behavior = 1
            e.action_state = 0
            e.yawn_form = 1
            e.hit_state = 0
            e.yawn_recovery = 0xDA
            e:play_enemy_sound(1)
        end
        turn_accel(e)
    end

    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = e.yawn_form + 9
        e.blend_counter = 7
        e.yawn_speed = 0
        e.move_speed_current = 0x46

        local y = -300
        if e:joint_world_y(0) < -0x5DC then
            y = 300
        end
        e:spawn_joint_effect_at(0, 8, 0, 0, y, 0, 0)
        e:play_enemy_sound(2)
        e.action_state = e.action_state + (e:yawn_anim(false, 0x200) and 1 or 0)
    elseif st == 1 then
        e.action_state = e.action_state + (e:yawn_anim(false, 0x200) and 1 or 0)
    elseif st == 2 then
        e.action_state = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 5
        e.blend_counter = 7
        e.move_speed_current = 0x78
        e.action_ticks_counter = 0x3C
        recovery_turn()
    elseif st == 3 then
        recovery_turn()
    end

    e:move(0, as_s16(e.yawn_speed))
    e:yawn_post_move(0x30)
end

-- State 3's death (`yawn_die_run`): thrash (0/1), coil (2/3), collapse
-- (4/5), then the two-phase dissolve.
die_run = function(e)
    local st = e.action_state

    local function first_phase()
        e.action_state = e.action_state + (e:yawn_anim(false, 0x200) and 1 or 0)
        e:move(0, as_s16(e.yawn_speed))
        e:yawn_post_move(0x30)
    end

    local function dissolve(phase)
        e:move(0, as_s16(e.yawn_speed))
        e:yawn_post_move(0x30)
        e.has_enter_switch_zone = e.has_enter_switch_zone | 0x80

        if phase == 7 then
            local shrink = as_s16(e.yawn_shrink)
            local side = idiv(0x1000 - shrink, 2) + 0x1000
            e:yawn_scale_worlds(3, 14, side, shrink, side)
            e.pos_y = e.pos_y + 2

            if as_s16(e.action_ticks_counter) < 0x50 then
                e.action_ticks_counter = as_s16(e.action_ticks_counter + 1)
                e.action_ticks_counter = as_s16(e.action_ticks_counter + 1)
                if (e.action_ticks_counter & 7) == 0 then
                    e:tint_model(-1, 0, 0, 0, 0x200)
                end
                e.action_ticks_counter = as_s16(e.action_ticks_counter + 1)
                if (e.action_ticks_counter & 7) == 0 then
                    e:tint_model(0, -1, 0, 0, 0x200)
                end
                if (e.action_ticks_counter & 0x1F) == 0 then
                    e:tint_model(0, 0, -1, 0, 0x200)
                end
            end

            e.yawn_shrink = as_s16(e.yawn_shrink - 0x14)
            if as_s16(e.yawn_shrink) < 500 then
                e.action_state = 8
                e.action_ticks_counter = 0
                e:yawn_set_shadow_tint(0xAFDF9F)
            end
        else
            local ticks = as_s16(e.action_ticks_counter)
            local side = ticks * -0x10 + 0x1706
            e:yawn_scale_worlds(3, 14, side, 500 - ticks, side)
            e.action_ticks_counter = as_s16(ticks + 1)
            if as_s16(e.action_ticks_counter) > 0x15E then
                e.action_state = 9
            end
        end
    end

    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 6
        e.blend_counter = 7
        e.yawn_speed = 0
        e.move_speed_current = 0
        e.hit_state = 1

        local y = -300
        if e:joint_world_y(0) < -0x5DC then
            y = 500
        end
        e:spawn_joint_effect_at(0, 0x0B, 0, 0, y, 0, 0)
        e:spawn_joint_effect_at(0, 8, 0, 0, y, 0, 0)
        e:play_enemy_sound(2)
        e:raise_death_event()
        first_phase()
    elseif st == 1 then
        first_phase()
    elseif st == 2 then
        e.action_state = 3
        e.timing_control = 0
        e.animation_id = 7
        e.blend_counter = 7
        e.move_speed_current = 0x46
        e:play_enemy_sound(1)
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x30)
        e.action_state = e.action_state + (e:yawn_anim(false, 0x200) and 1 or 0)
        e:move(0, as_s16(e.yawn_speed))
        e:yawn_post_move(0x30)
    elseif st == 3 then
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x30)
        e.action_state = e.action_state + (e:yawn_anim(false, 0x200) and 1 or 0)
        e:move(0, as_s16(e.yawn_speed))
        e:yawn_post_move(0x30)
    elseif st == 4 then
        e.action_state = 5
        e.timing_control = 0
        e.animation_id = 0x0F
        e.blend_counter = 7
        e.move_speed_current = 100
        e:play_enemy_sound(2)
        e.action_state = e.action_state + (e:yawn_anim(false, 0x200) and 1 or 0)
        if e.animation_frame_id == 0x32 then
            e:spawn_joint_effect_at(0, 0x0B, 0, 0, 500, 0, 0)
            e:spawn_joint_effect_at(0, 8, 0, 0, 500, 0, 0)
        end
        e:move(0, as_s16(e.yawn_speed))
        e:yawn_post_move(0x30)
    elseif st == 5 then
        e.action_state = e.action_state + (e:yawn_anim(false, 0x200) and 1 or 0)
        if e.animation_frame_id == 0x32 then
            e:spawn_joint_effect_at(0, 0x0B, 0, 0, 500, 0, 0)
            e:spawn_joint_effect_at(0, 8, 0, 0, 500, 0, 0)
        end
        e:move(0, as_s16(e.yawn_speed))
        e:yawn_post_move(0x30)
    elseif st == 6 then
        e.move_speed_current = 0
        e.status_flags = e.status_flags | 6
        e:yawn_set_status_range(0, 12, 0xFF, 4)
        e.action_state = 7
        e.yawn_shrink = 0x1000
        e.action_ticks_counter = 0
        dissolve(7)
    elseif st == 7 then
        dissolve(7)
    elseif st == 8 then
        dissolve(8)
    end

    -- The ambient dissolve sparkle runs for 800 frames from the moment the
    -- death state starts, independent of the phase.
    if e.yawn_sparkle < 800 then
        e.yawn_sparkle = e.yawn_sparkle + 1
        if (e.yawn_sparkle & 0xF) == 0 then
            for _ = 1, 3 do
                local joint = e:random() % 0xF
                local x = (e:random() & 0x3FF) - 0x200
                local z = (e:random() & 0x3FF) - 0x200
                e:spawn_joint_effect_lit(9, 0x11, joint, x, 500, z, 0, 0x14)
            end
        end
        if ((e.yawn_sparkle + 3) & 7) == 0 and e.yawn_sparkle < 600 then
            for _ = 1, 3 do
                local joint = e:random() % 0xF
                local x = (e:random() & 0x3FF) - 0x200
                local z = (e:random() & 0x3FF) - 0x200
                e:spawn_joint_effect_lit(9, 0x11, joint, x, 500, z, 0, 0x14)
            end
        end
    end
end

-- State 0: one-time setup.
local function init(e)
    e.state_word = 0x00000101
    e.hit_state = 0
    e:reset_joints()

    e:set_shadow_offset(0, 0, 0)
    e.shadow_tint = 0x808080
    e.shadow_half_x = 1000
    e.shadow_half_z = 1000

    e.health = 0x0BEA
    if e.id == 0x12 then
        if e.second_playthrough then
            e.health = 0x0190
        else
            e.health = 0x012C
        end
    end

    e.animation_frame_id = 0
    e.timing_control = 0
    e.animation_id = (e.behavior_flags & 2) * 5 + 1
    if (e.behavior_flags & 0xF) == 2 then
        e.animation_id = 2
    end
    e.blend_counter = 0

    e:yawn_back_off()
    e:advance_anim(0x40)
    e:yawn_standard_worlds(false)

    e.pos_x = e:joint_world_x(0)
    e.pos_y = 0
    e.pos_z = e:joint_world_z(0)
    e:save_pos()
    e.yawn_ground_y = as_s16(e:joint_world_y(7))

    e:set_sca(SCA_RADIUS, SCA_HALF_HEIGHT, 0, SCA_OFFSET_Y, 0)
    e:set_sca_hit_point(0, 0, 0)

    for k = 3, 14 do
        e:set_joint_flag(k, e:joint_flag(k) | 8)
    end
    e:yawn_standard_worlds(true)

    e.yawn_speed = 0
    e.yawn_turn_accel = 0x40
    e.yawn_form = 0
    e.yawn_bites = 0
    e.yawn_scale_ramp = 0
    e.yawn_bite_dir = 0
    e.yawn_recovery = 0
    e.yawn_path_index = 0
    e.yawn_shrink = 0
    e.yawn_sparkle = 0
    e.yawn_nearly_dead = 0
    e.yawn_stuck = 0
    e.yawn_hiss = 0
    e.yawn_bite_cool = 0

    e:yawn_spawn_segments()
    e:yawn_pose_init()

    if (e.behavior_flags & 0xF) == 0 or (e.behavior_flags & 0xF) == 4 then
        -- The scripted entrance: hidden until behaviour 5 crawls it in.
        e.status_flags = e.status_flags | 4
        e.ignore = 1
        e.action_behavior = 5
        e.action_state = 0
        if (e.behavior_flags & 0x80) ~= 0 then
            e.state = 4
        end
    else
        -- Already in the room, grown, and the fight flag is up.
        e.yawn_form = 1
        if e.id == 0x0D then
            e:raise_flag(0, 0x10)
        end
        if (e.behavior_flags & 0x80) ~= 0 then
            e.state = 4
        end
    end
end

-- State 1: the decision layer.
local function state_check(e)
    if (e.behavior_flags & 0x80) ~= 0 then
        return
    end

    e.status_flags = (e.status_flags & 0x1F) | 0x40
    if e:check_alert_range(4000) < 4000 then
        e.status_flags = e.status_flags & 0xBF
    end
    if e.yawn_form == 0 then
        e.status_flags = e.status_flags & 0x7F
        e:check_visual_range(4000)
    end

    if e.ignore == 0 then
        select_behavior(e)
    end
    run_action(e)

    -- Snapshot the state word so a hit reaction can restore it.
    e.yawn_state_bk = e.state_word

    local hiss = e.yawn_hiss
    if hiss ~= 0 then
        e.yawn_hiss = hiss - 1
        if (hiss & 7) == 0 then
            local r = e:random()
            spawn_dust(e, r < 0 and -(-r & 1) or (r & 1))
        end
    end
end

-- State 2: a hit only interrupts when the snake is not mid-bite and not in
-- the post-bite lockout.
local function damaged(e)
    if as_s8(e.yawn_bite_dir) ~= 0 then
        e.state_word = e.yawn_state_bk
        return
    end
    if e.yawn_recovery ~= 0 then
        e.state_word = e.yawn_state_bk
        return
    end
    if e.ignore == 0 then
        e.state_word = 0x00000102
    end
    damaged_run(e)
end

-- State 3.
local function die(e)
    if e.ignore == 0 then
        e.state_word = 0x00000103
    end
    die_run(e)
end

-- State 4: parking state for a Yawn whose script has taken it over.
local function wait_state(e)
    if (e.behavior_flags & 0x80) == 0 then
        e.state = 1
    end
end

-- ---------------------------------------------------------------------------
-- The per-frame entry.
-- ---------------------------------------------------------------------------

function update(e)
    if (e.behavior_flags & 1) ~= 0 then
        if e.behavior_flags == 1 then
            e:yawn_segment_tick()
        end
        return
    end

    if not e.monster_paused then
        local state = e.state
        if state == 0 then
            init(e)
        elseif state == 1 then
            state_check(e)
        elseif state == 2 then
            damaged(e)
        elseif state == 3 then
            die(e)
        elseif state == 4 then
            wait_state(e)
        end

        if e.yawn_bite_cool ~= 0 then
            e.yawn_bite_cool = e.yawn_bite_cool - 1
        end
    end

    e:update_switch_zone_keep_high()
end

return { update = update }
