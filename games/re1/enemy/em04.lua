-- Black Tiger, entity id 0x04.
--
-- The giant spider boss. The same two-layer shape as the WebSpinner with one
-- picker instead of three spawn kinds:
--
--   state            init / run / web-build / web-shoot
--   ignore           0 = the picker runs this frame, 1 = the chosen behaviour
--                    owns the tiger, anything else freezes it
--   action_behavior  0 idle, 1 walk, 2 strafe, 3 approach, 10 circle,
--                    12 bite, 13 spit (6/8/9 and 1/2 are also routed back into
--                    the web shooters by state 3)
--   action_state     the per-behaviour animation sub-state
--
-- Every behaviour assignment is a word write over behaviour + sub-state, so a
-- pick or completion also zeroes the sub. The picker chases on the
-- pathfinder's line-of-sight bit, closes for the bite when lined up, spits
-- when the player is far, and stands down on a cooldown after each attack.
-- The bite is the only direct damage: at animation frame 0xC it tests the two
-- fang joints and costs 20 health. The walk and approach profiles install the
-- TWO-volume SCA record (front and rear boxes 2000 apart), the idle and shoot
-- profiles a single box.
--
-- The web shooters are the same clone-arena subsystem the WebSpinner uses:
-- three variants select the count from hit_state, prime the fangs and fly the
-- threads. `e:web_spawn`/`e:web_update` drive the shared Rust arena, and the
-- per-type web-joint registry is shared game state reset with the room.
--
-- All state lives on the Rust entity, so the scripting VM may be reset between
-- any two updates. The original's write-only `field_00` and the raw joint
-- flag ORs stay unmodelled (the joint hide and the leg-reach gate are), and
-- the two fang reach probes use a zero local offset (the original stages the
-- shared scratch block; only the joint translation and the 0x514 radius
-- participate for the shipped data).

-- The per-frame scuttle cue, indexed by animation frame (the same table the
-- WebSpinner uses).
local FRAME_SFX = {
    0, 2, 0, 0, 0, 0, 0, 1,
    0, 0, 1, 0, 0, 0, 1, 0,
}

-- The eight leg/body joints a web thread can hang from (5 and 7 repeat).
local WEB_JOINTS = { 5, 7, 7, 9, 0x0B, 0x0B, 0x0F, 0x11 }

-- The 16-bit signed view of a wrapped word.
local function signed_word(value)
    value = value & 0xFFFF
    if value >= 0x8000 then
        return value - 0x10000
    end
    return value
end

-- The 16-bit state write: state + ignore only.
local function set_state_word(e, state, ignore)
    e.state = state
    e.ignore = ignore
end

-- One word write over `action_behavior` + `action_state`.
local function set_behavior(e, behavior)
    e.action_behavior = behavior
    e.action_state = 0
end

-- The player is knocked down or grabbed.
local function player_down(e)
    return e.player_action_behavior == 0x14 and e.player_action_state == 0
end

-- The three SCA profiles: the idle single box at the rear, the two-box walk
-- profile (front then rear, 2000 apart), and the shoot box.
local function sca_idle(e)
    e:set_sca(2500, 180, -500, -180, 0)
end

local function sca_walk(e)
    e:set_sca(2000, 180, 0, -180, 1000)
    e:set_sca2(2000, 180, 0, -180, -1000)
end

local function sca_shoot(e)
    e:set_sca(100, 0, 0, 0, 0)
end

-- Behaviour 0: idle. Stands on the standard clip; the picker owns the
-- transitions.
local function behavior_idle(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0
        e.blend_counter = 3
    elseif e.action_state ~= 1 then
        return
    end
    e:advance_anim(0x400)
end

-- Behaviour 1 (and 4, the chase resume): walk toward. The scuttle cue, the
-- leg stride and the 0x50 speed cap all land here; the walk profile is
-- installed at setup and the idle profile restored on completion.
local function behavior_walk(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.action_ticks_counter =
            ((e:random() & 0x1F) * e.action_behavior) & 0xFFFF
        if (e.action_behavior & 0xFE) == 0 then
            e.action_ticks_counter = (e.action_ticks_counter + 0x50) & 0xFFFF
        end
        sca_walk(e)
    elseif e.action_state ~= 1 then
        return
    end

    local frame = e.animation_frame_id
    local sfx = frame < 16 and FRAME_SFX[frame + 1] or 0
    if sfx ~= 0 then
        e:play_enemy_sound(sfx)
    end

    e:advance_anim(0x400)

    local moved = e.animation_frame_id
    local part = 1
    if moved == 0 or moved > 8 then
        part = 0
    end
    e:leg_reach(part, e.joint_scale)

    if signed_word(e.move_speed_current) > 0x50 then
        e.move_speed_current = 0x50
    end
    e:move(0, signed_word(e.move_speed_current))

    local d = signed_word(e.action_ticks_counter)
    e.action_ticks_counter = d - 1
    if d == 0 then
        e.ignore = 0
        set_behavior(e, 0)
        sca_idle(e)
    end
end

-- Behaviour 2: strafe. Side-step with the walk profile, then flip to the spit
-- the moment the player lines up and the post-attack delay has run out.
local function behavior_strafe(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 3
        e.action_ticks_counter = (e:random() & 0x3F) & 0xFFFF
        e.blend_counter = 3
        e.bt_turn = 0x18
        if (e.action_ticks_counter & 1) ~= 0 then
            e.bt_turn = -0x18
        end
        sca_walk(e)
    elseif e.action_state ~= 1 then
        return
    end

    e.angle = e.angle + e.bt_turn
    e:advance_anim(0x400)

    if (e.bt_path_word & 1) ~= 0 then
        local px, _, pz = e:player_pos()
        local turn = e:turn_toward_target(px, pz, e.bt_turn)
        if turn == 0 and e.bt_delay == 0 then
            e.ignore = 1
            set_behavior(e, 0xD)
            return
        end
    end

    local d = signed_word(e.action_ticks_counter)
    e.action_ticks_counter = d - 1
    if d == 0 then
        e.ignore = 0
        set_behavior(e, 0)
        sca_idle(e)
    end
end

-- Behaviour 3: approach. Close on the caller's turn step with the walk
-- profile, then collapse to the idle chase and latch the player's position as
-- the waypoint.
local function behavior_approach(e)
    local sub = e.action_state
    if sub == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.hit_state = 0
        e.action_state = 1
        e.action_ticks_counter = ((e:random() & 0x1F) + 0x50) & 0xFFFF
        sca_walk(e)
    elseif sub ~= 1 then
        if sub ~= 2 then
            return
        end
        e.hit_state = 0
        e.ignore = 0
        set_behavior(e, 0)
        sca_idle(e)
        local px, _, pz = e:player_pos()
        e.player_pos_x = px
        e.player_pos_z = pz
        return
    end

    local px, _, pz = e:player_pos()
    local turn = e:turn_toward_target(px, pz, e.bt_turn)
    local d = signed_word(e.action_ticks_counter)
    e.action_ticks_counter = d - 1
    if d == 0 or turn == 0 then
        e.action_state = 2
    end
    e:advance_anim(0x400)
    e.angle = e.angle + turn
end

-- Behaviour 10: circle. Orbits on a stored launch velocity while the heading
-- corrects, then resumes the chase from the line-of-sight bit.
local function behavior_circle(e)
    local sub = e.action_state
    if sub == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.action_state = 1
        e.animation_id = 6
        e.bt_turn = (e:random() & 1) * -0x658 + 0x32C
        local los = e.bt_path_word & 1
        e.bt_turn = e.bt_turn + (los == 0 and 1 or 0) * (e:random() & 0xFFF)
        e.move_speed_current = (e:random() & 0x3F) + 0xFA
        e:move(e.bt_turn, signed_word(e.move_speed_current))
    elseif sub ~= 1 then
        if sub ~= 2 then
            return
        end
        e.hit_state = 0
        e.ignore = 0
        set_behavior(e, (e.bt_path_word & 1) << 2)
        e.bt_count = 0
        return
    end

    e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
    local px, _, pz = e:player_pos()
    e.angle = e.angle + e:turn_toward_target(px, pz, 0x40)
    e:advance_speed()
end

-- Behaviour 12: bite. Wait, rush, turn in, bite and recover. The bite lands at
-- animation frame 0xC when the player is not already reacting and either fang
-- joint reaches: 20 health, the facing-based reaction, then the recovery cue.
local function behavior_bite(e)
    local sub = e.action_state
    if sub == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.animation_id = 8
        sub = 1
    end

    if sub == 1 then
        e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
        return
    end

    if sub == 2 then
        e.action_state = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.animation_id = 0xB
        e.action_ticks_counter = ((e:random() & 0x1F) + 0x14) & 0xFFFF
        sub = 3
    end

    if sub == 3 then
        e:advance_anim(0x400)
        local px, _, pz = e:player_pos()
        local turn = e:turn_toward_target(px, pz, 0x40)
        e.angle = e.angle + turn
        if turn == 0 then
            e.action_state = 4
        end
        local d = signed_word(e.action_ticks_counter)
        e.action_ticks_counter = d - 1
        if d == 0 then
            e.action_state = 4
        end
        return
    end

    if sub == 4 then
        e.action_state = 5
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.animation_id = 9
        sub = 5
    end

    if sub == 5 then
        if e.animation_frame_id == 0xC and e.player_attacked == 0 then
            local hit = e:reach_test(1, 0, 0, 0, 0x514)
                or e:reach_test(2, 0, 0, 0, 0x514)
            if hit then
                local facing = e:player_facing_entity() and 1 or 0
                e:play_enemy_sound(4)
                e.player_health = e.player_health - 0x14
                if e.player_health < 0 then
                    e.player_health = 1
                end
                if e.player_health >= 0 then
                    e.player_attacked = facing + 1
                    e.player_action_behavior = facing + 0x66
                end
            end
        end
        e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
        return
    end

    if sub == 6 then
        e:play_enemy_sound(3)
        e.ignore = 0
        set_behavior(e, 0)
        if e.player_health < 0 then
            set_state_word(e, 1, 1)
            set_behavior(e, 0xC)
            return
        end
    end
end

-- Behaviour 13: spit. Three acid globs fanned by a random count, then back to
-- the bite with the post-attack delay armed.
local function behavior_spit(e)
    local sub = e.action_state
    if sub == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0xC
        e.action_ticks_counter = 0
        local r1 = e:random()
        local r2 = e:random()
        local n = 2 - (r1 & 1) * (r2 & 1)
        e:spawn_effect(0x1E, 0, 1000, -600, 0, n * -0x80, 0)
        e:spawn_effect(0x1E, 0, 1000, -600, 0, 0, 0)
        e:spawn_effect(0x1E, 0, 1000, -600, 0, n * 0x80, 0)
        e:play_enemy_sound(7)
        sub = 1
    elseif sub ~= 1 then
        if sub ~= 2 then
            return
        end
        set_state_word(e, 1, 1)
        set_behavior(e, 0xC)
        e.bt_delay = 0x14
        return
    end

    e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
end

-- The picker: the post-attack cooldown first (walk off when the player is far
-- enough), then the line-of-sight chase. Inside 5000 it commits to the bite;
-- lined up with no delay it spits; past 9000 it gives up; without the sight
-- bit it re-rolls the strafe/approach and resets the chase count.
local function picker(e)
    local dist = e:web_distance()
    local saved_sub = e.action_state

    if e.bt_cooldown ~= 0 then
        if dist > 2000 then
            e.ignore = 0
            e.action_behavior = 1
            e.action_state = saved_sub
        end
        e.bt_cooldown = e.bt_cooldown - 1
        return
    end

    if e.bt_count < 0x15 and (e.bt_path_word & 1) ~= 0 then
        if dist < 5000 then
            e.ignore = 1
            set_behavior(e, 0xC)
        end
        if e.bt_delay == 0 then
            local px, _, pz = e:player_pos()
            if e:turn_toward_target(px, pz, 0x200) == 0 then
                e.ignore = 1
                set_behavior(e, 0xD)
            end
        end
        if dist > 9000 then
            e.ignore = 0
            e.action_behavior = 1
            e.action_state = saved_sub
            return
        end
    else
        e.ignore = 1
        set_behavior(e, (e:random() & 1) + 2)
        e.bt_count = 0
    end
end

-- The action runner: refresh the pathfinder and execute the selected
-- behaviour. Behaviours 1 and 4 share the walk (4 is the original's no-op
-- slot the picker can strand the tiger on with a clear line of sight).
local function action_runner(e)
    local px, _, pz = e:player_pos()
    local path = e:pathfind_update(px, pz)
    e:pathfind_keep(path)

    local b = e.action_behavior
    if b == 0 then
        behavior_idle(e)
    elseif b == 1 or b == 4 then
        e.animation_id = 3
        e.angle = e.angle + e:turn_toward_target(px, pz, 0x10) * (e.bt_path_word & 1)
        e.bt_cflag = 6
        behavior_walk(e)
    elseif b == 2 then
        behavior_strafe(e)
    elseif b == 3 then
        e.animation_id = 2
        e.bt_turn = 0x80
        behavior_approach(e)
    elseif b == 10 then
        behavior_circle(e)
    elseif b == 0xC then
        behavior_bite(e)
    elseif b == 0xD then
        behavior_spit(e)
    end
end

-- The web build (state 2 body): spin a thread off the next free leg joint.
-- The chosen joint and its neighbour are armed and two depth-8 billboards are
-- queued at their world matrices (chosen first); the used-joint registry is
-- the shared Rust state.
local function web_build(e)
    local sub = e.action_state
    if sub == 0 then
        e.action_state = 1
        e.move_speed_current = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e:play_enemy_sound(4)

        if (e.hit_state & 1) == 0 then
            local pick = e:random() & 7
            local chosen = WEB_JOINTS[pick + 1]
            if e:web_joint_use(chosen, e.bt_web_index) then
                e:arm_joint_effect(chosen, 0x1E, 0x14, 3)
                e:arm_joint_effect(chosen + 1, 0x1E, 0x14, 3)
                e:spawn_joint_effect(0, 8, chosen, 0)
                e:spawn_joint_effect(0, 8, chosen + 1, 0)
                e:play_enemy_sound(5)
                e.bt_web_index = e.bt_web_index + 1
            end
        end
    elseif sub ~= 1 then
        if sub ~= 2 then
            return
        end
        set_state_word(e, 1, 1)
        set_behavior(e, 3)
        local px, _, pz = e:player_pos()
        e.player_pos_x = px
        e.player_pos_z = pz
        e:check_special_weapon()
        if (e.bt_path_word & 1) ~= 0 then
            set_state_word(e, 1, 1)
            set_behavior(e, 10)
        end
        e.bt_delay = 0
        return
    end

    e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
end

-- The default web shooter (state 3 behaviours other than 1/2/6/8/9): prime
-- the fangs, spit, swap to the shoot SCA profile and spawn 51 threads, then
-- keep them flying. The death flag rises with each stage.
local function web_shoot_a(e)
    local sub = e.action_state
    if sub == 0 then
        e.move_speed_current = 0
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0
        e.shadow_half_x = 0
        e.shadow_half_z = 0
        e:arm_joint_effect(0, 0x1E, 0x14, 3)
        e:arm_joint_effect(19, 0x1E, 0x14, 3)
        e:spawn_joint_effect(0, 8, 0, 0)
        e:spawn_joint_effect(0, 8, 19, 0)
        e:play_enemy_sound(5)
        -- The raw joint flag ORs over joints 3..18 are not modelled.
        sub = 1
    end

    if sub == 1 then
        e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
        return
    end

    if sub == 2 then
        e.shadow_tint = 0x00DF809F
        e.shadow_half_x = 500
        e.shadow_half_z = 500
        e.action_state = 3
        e.status_flags = e.status_flags | 0xA
        e.move_speed_current = 0
        e.action_ticks_counter = 0x5A
        sca_shoot(e)
        e:raise_death_event()
        sub = 3
    end

    if sub == 3 then
        e:adjust_shadow_size(9, 9)
        local d = signed_word(e.action_ticks_counter) - 1
        e.action_ticks_counter = d
        if d == 0 then
            e:raise_death_event()
            e.action_state = 4
            e:web_spawn(0x33)
        end
        return
    end

    if sub == 4 then
        e:raise_death_event()
        e:web_update(0x33)
        return
    end
end

-- The web shooter for state 3 behaviours 6/8/9: a variable thread count from
-- the high hit_state bits. A fresh hit while priming re-arms the fangs.
local function web_shoot_b(e)
    local sub = e.action_state
    if sub == 1 or sub == 3 then
        if (e.hit_state & 2) ~= 0 then
            e.shadow_half_x = 0
            e.shadow_half_z = 0
            e:arm_joint_effect(0, 0x1E, 0x1E, 3)
            e:arm_joint_effect(19, 0x1E, 0x1E, 3)
            e:spawn_joint_effect(0, 8, 0, 0)
            e:spawn_joint_effect(0, 8, 19, 0)
            e:play_enemy_sound(5)
            -- The raw joint flag ORs are not modelled.
            e.action_behavior = 3
            e.action_state = 2
            return
        end
        if (e.hit_state & 1) ~= 0 then
            e.action_state = 4
            sub = 4
        end
    end

    if sub == 0 then
        e.move_speed_current = 0
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.hit_state = 0
        e.animation_id = 4
        e.shadow_half_x = 0x708
        e.shadow_half_z = 0x708
        sub = 1
    end

    if sub == 1 then
        e.status_flags = e.status_flags & 0x1F
        if e.animation_frame_id > 0xD then
            e.status_flags = e.status_flags | 0x20
        end
        e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
        return
    end

    if sub == 2 then
        e.status_flags = e.status_flags | 0xA
        e.animation_id = 5
        e.action_state = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.hit_state = 0
        e.action_ticks_counter = 0xB4
        e:raise_death_event()
        sub = 3
    end

    if sub == 3 then
        e.status_flags = (e.status_flags & 0x1F) | 0x20
        local d = signed_word(e.action_ticks_counter)
        e.action_ticks_counter = d - 1
        if d == 0 then
            e.action_state = 4
            e.hit_state = 0x11
        end
        e:advance_anim(0x400)
        return
    end

    if sub == 4 then
        e.animation_id = 5
        e.action_state = 5
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 1
        e:play_enemy_sound(5)
        e.shadow_tint = 0x00DF809F
        e.shadow_half_x = 2000
        e.shadow_half_z = 2000
        e.action_ticks_counter = 100
        e.move_speed_current = 0
        e.status_flags = e.status_flags | 0xA
        sca_shoot(e)
        sub = 5
    end

    if sub == 5 then
        e:adjust_shadow_size(4, 4)
        e:advance_anim(0x400)
        local d = signed_word(e.action_ticks_counter) - 1
        e.action_ticks_counter = d
        if d ~= 0 then
            return
        end
        e.action_state = 6
        e:web_spawn((e.hit_state >> 3) * 5 + 1)
        return
    end

    if sub == 6 then
        e:raise_death_event()
        e:web_update((e.hit_state >> 3) * 5 + 1)
        return
    end
end

-- The web shooter for state 3 behaviours 1/2: prime the fangs (joint 19 only
-- while the registry is young), grow the shadow and fly the threads while
-- steering with the stored speed.
local function web_shoot_c(e)
    local sub = e.action_state
    if sub == 1 or sub == 3 then
        if (e.hit_state & 2) ~= 0 then
            e.shadow_half_x = 0
            e.shadow_half_z = 0
            e:arm_joint_effect(0, 0x1E, 0x1E, 3)
            e:arm_joint_effect(19, 0x1E, 0x1E, 3)
            e:spawn_joint_effect(0, 8, 0, 0)
            e:spawn_joint_effect(0, 8, 19, 0)
            e:play_enemy_sound(5)
            -- The raw joint flag ORs are not modelled.
            e.action_behavior = 3
            e.action_state = 2
            return
        end
        if (e.hit_state & 1) ~= 0 then
            e.action_state = 4
            sub = 4
        end
    end

    if sub == 0 then
        e.move_speed_current = 0
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.hit_state = 0
        e.animation_id = 1
        e.shadow_half_x = 0x5DC
        e.shadow_half_z = 0x5DC
        if e.bt_web_index < 6 then
            e:arm_joint_effect(19, 0x1E, 0x1E, 3)
        end
        e:spawn_joint_effect(0, 8, 0, 0)
        e:spawn_joint_effect(0, 8, 0, 0)
        e:spawn_joint_effect(0, 0xB, 1, 0)
        e:spawn_joint_effect(0, 0xB, 2, 0)
        e:play_enemy_sound(5)
        sub = 1
    end

    if sub == 1 then
        e.status_flags = e.status_flags & 0x1F
        e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
        return
    end

    if sub == 2 then
        e.animation_id = 7
        e.animation_frame_id = 0
        e.timing_control = 0
        e.action_ticks_counter = 0xD2
        e.status_flags = e.status_flags | 0xA
        e.hit_state = 0
        e:raise_death_event()
        e.action_state = 3
        sub = 3
    end

    if sub == 3 then
        local u = e.action_ticks_counter
        e.action_ticks_counter = u - 1
        if (u & 1) ~= 0 then
            e:advance_anim(0x400)
        end
        -- The frame test is outside the odd-frame step: on an even frame the
        -- frame stays put, so a frame-0 pose re-queues the fang billboards
        -- every other tick.
        if e.animation_frame_id == 0 then
            e.status_flags = (e.status_flags & 0x1F) | 0x20
            e:spawn_joint_effect(0, 0xB, 1, 0)
            e:spawn_joint_effect(0, 0xB, 2, 0)
        end
        if u - 1 == 0 then
            e.action_state = 4
            e.hit_state = 0x11
        end
        return
    end

    if sub == 4 then
        e.animation_id = 1
        e.action_state = 5
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 1
        e:play_enemy_sound(5)
        e.action_ticks_counter = 100
        e.move_speed_current = 0x78
        e.status_flags = e.status_flags | 0xA
        sca_shoot(e)
        e:web_spawn((e.hit_state >> 3) * 5 + 1)
        sub = 5
    end

    if sub == 5 then
        e:advance_anim(0x400)
        e:web_update((e.hit_state >> 3) * 5 + 1)
        e.move_speed_current = e.move_speed_current - 1
        local i = e:random() & 7
        local j = e:random()
        e:move(((j & 1) + 1) * WEB_JOINTS[i + 1] * 100, signed_word(e.move_speed_current))
        local d = signed_word(e.action_ticks_counter)
        e.action_ticks_counter = d - 1
        if d == 0 then
            e.action_state = 6
            return
        end
        return
    end

    if sub == 6 then
        e.move_speed_current = 0
        e.action_state = 7
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.animation_id = 4
        sub = 7
    end

    if sub == 7 then
        e:web_update((e.hit_state >> 3) * 5 + 1)
        if e:advance_anim(0x400) then
            e.action_state = 8
            e.action_ticks_counter = 0x3C
        end
        return
    end

    if sub == 8 then
        e:raise_death_event()
        e:web_update((e.hit_state >> 3) * 5 + 1)
        return
    end
end

-- State 0: spawn. Clears the state block, builds the 0x9C4 shadow, rolls the
-- fixed 204 health and installs the idle SCA profile, the 0x1B33 joint scale
-- and the cooldown.
local function init(e)
    e.state = 1
    e.ignore = 0
    e.action_behavior = 0
    e.action_state = 0
    e.action_ticks_counter = 0
    e.hit_state = 0
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0
    e.animation_id = 0

    e:reset_joints()
    e:advance_anim(0x400)

    e:set_shadow_offset(0, 0, 0)
    e.shadow_tint = 0x00404040
    e.shadow_half_x = 0x9C4
    e.shadow_half_z = 0x9C4

    e:random()
    e.health = 0xCC

    sca_idle(e)
    e.joint_scale = 0x1B33

    e.bt_delay = 0
    e.bt_splat = 0
    e.bt_count = 0
    e.bt_cflag = 0
    e.bt_web_index = 0
    e.bt_cooldown = 0x2D
end

-- State 1: the driver. The picker runs while the ignore latch is clear; the
-- runner runs for 0 and 1 and is skipped otherwise. The tail keeps the status
-- bits, checks the visual range, and counts the wall trail into the chase
-- counter.
local function run(e)
    if e.ignore == 0 then
        picker(e)
    end
    if e.ignore == 0 or e.ignore == 1 then
        action_runner(e)
    end

    e.status_flags = (e.status_flags & 0x1F) | 0x40
    e:check_visual_range(4000)

    if e.bt_delay ~= 0 then
        e.bt_delay = e.bt_delay - 1
    end
    if e.bt_trail == 0 then
        e.bt_count = 0
        return
    end
    e.bt_count = e.bt_count + 1
end

-- State 2: the web-build wait; behaviour 0 spins a thread each frame.
local function build(e)
    if e.action_behavior == 0 then
        e.animation_id = 1
        web_build(e)
    end
end

-- State 3: the web-shoot state. The first frame maps the damage hit_state to a
-- behaviour and latches the shooter; the splat word re-enters a sub-state.
-- Behaviours 1/2 take shoot_c, 6/8/9 shoot_b, everything else shoot_a.
local function shoot_state(e)
    if e.ignore == 0 then
        e.action_behavior = e.hit_state >> 3
        e.ignore = 1
        if e.bt_splat ~= 0 then
            e.action_state = 3
            e.action_behavior = e.bt_splat
        end
    end

    local b = e.action_behavior
    if b == 1 or b == 2 then
        e.bt_splat = 1
        web_shoot_c(e)
    elseif b == 6 or b == 8 or b == 9 then
        e.bt_splat = 6
        web_shoot_b(e)
    else
        web_shoot_a(e)
    end
end

local STATES = { init, run, build, shoot_state }

function update(e)
    -- The state machine and the SCA pass run only while monsters are
    -- unpaused; the tiger's SCA tail is unconditional inside the gate.
    if not e.monster_paused then
        local state = STATES[e.state + 1]
        if state then
            state(e)
        end
        e.bt_touch = e:separate() and 1 or 0
        e.bt_trail = e:resolve_collision()
    end

    e:update_switch_zone()
end
