-- WebSpinner, entity id 0x03.
--
-- The big spider. A four-level machine sharing the wasp's shape:
--
--   state            init / run / web-build / web-shoot / two dead slots
--   ignore           0 = the behaviour picker runs this frame, 1 = the chosen
--                    behaviour owns the spider, anything else freezes
--   behavior_flags   the SPAWN KIND, clamped to 2 at init:
--                      0/1 sit on the floor, 2 hangs from the ceiling at
--                      -6136 (-7200 in the guardhouse) and drops on a thread
--                    bit 7 skips the whole update
--   action_behavior  the twelve-entry behaviour word (idle, three walks, the
--                    strafe, two approaches, lunge, spit, webdrop, ret, circle)
--   action_state     the per-behaviour animation sub-state
--
-- The behaviour picker (one of three bodies by spawn kind) only decides WHICH
-- behaviour to enter; the action runner executes it. A completed behaviour
-- clears the ignore flag so the picker runs again. The picker probes the
-- player with `turn_toward_target` steps and the pathfinder's line-of-sight
-- bit, and the runner walks, strafes, closes, lunges (the one attack that
-- damages the player directly), spits a web glob, drops from the ceiling or
-- circles.
--
-- The web shooters clone the spider into the shared web-thread arena: state 3
-- maps the damage hit_state to a behaviour, primes the fang joints, swaps the
-- SCA to the small profile and spawns the threads (`e:web_spawn`), then keeps
-- them flying (`e:web_update`). The count comes from hit_state's middle bits.
-- The per-type web-joint registry lives in the Rust game state (shared by
-- every spider of this id, reset with the room); `e:web_joint_use` performs
-- the "next free leg" scan and store.
--
-- Deaths and damage reactions are the shared layer's job: the damage system
-- parks the spider on state 3 and writes hit_state; this script maps it to a
-- shooter. All state lives on the Rust entity, so the scripting VM may be
-- reset between any two updates. The original's write-only `field_00` and
-- `0xC1` bytes are not modelled, the two raw joint flag ORs the shooters run
-- over the fang/leg joints stay with the renderer's documented joint-object
-- deferral (the joint hide and the leg-reach gate are modelled), and the DC
-- dead-spawn branch has no arklay equivalent.

-- The random health roll, indexed by `rand() & 0xF`.
local HEALTH = {
    0x63, 0x63, 0x63, 0x63, 0x77, 0x63, 0x63, 0x77,
    0x63, 0x63, 0x63, 0x77, 0x63, 0x59, 0x63, 0x63,
}

-- The fast-idle behaviour re-roll, indexed by `(rand() & 7) + LOS * 8`.
local BEHAVIOR_PICK = {
    2, 1, 2, 2, 2, 1, 2, 5,
    0x0B, 0x0B, 5, 0x0B, 3, 5, 4, 4,
}

-- The per-frame scuttle cue, indexed by animation frame.
local FRAME_SFX = {
    0, 2, 0, 0, 0, 0, 0, 1,
    0, 0, 1, 0, 0, 0, 1, 0,
}

-- The eight leg/body joints a web thread can hang from.
local WEB_JOINTS = { 3, 5, 7, 9, 0x0B, 0x0D, 0x0F, 0x11 }

-- The 16-bit signed view of a wrapped word.
local function signed_word(value)
    value = value & 0xFFFF
    if value >= 0x8000 then
        return value - 0x10000
    end
    return value
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

-- The shared player-distance scratch (`ws_dist`): branchless Manhattan with
-- the original's extra unit when the player is on the -X side.
local function web_dist(e)
    return e:web_distance()
end

-- The animation starter every behaviour repeats: reset the clip words and the
-- blend, then move to the given sub-state.
local function anim_start(e, substate, blend)
    e.action_state = substate
    e.animation_frame_id = 0
    e.timing_control = 0
    if blend ~= nil then
        e.blend_counter = blend
    end
end

-- Behaviour 0: idle. Spawn kinds 1/2 idle slowly and never re-roll here; kind
-- 0 looks around for a randomised dwell and then picks the next behaviour
-- from the pathfinder's line-of-sight column.
local function behavior_idle(e)
    if e.behavior_flags ~= 0 then
        if e.action_state == 0 then
            anim_start(e, 1, 3)
            e.animation_id = 0
        elseif e.action_state ~= 1 then
            return
        end
        e:advance_anim(0x400)
        return
    end

    if e.action_state == 0 then
        anim_start(e, 1, 3)
        e.animation_id = 0
        local los = e.ws_path_word & 1
        e.action_ticks_counter = ((1 - los) * 0x50 + (e:random() & 0x3F)) & 0xFFFF
    elseif e.action_state ~= 1 then
        return
    end

    e:advance_anim(0x400)
    local d = signed_word(e.action_ticks_counter)
    e.action_ticks_counter = d - 1
    if d == 0 then
        e.ignore = 0
        local los = e.ws_path_word & 1
        e.action_behavior = BEHAVIOR_PICK[(e:random() & 7) + los * 8 + 1]
        e.action_state = 0
    end
end

-- Behaviour 1/4/11: walk toward. Plays a scuttle cue on the clip's footfall
-- frames, measures the leg stride into the move speed and steps off when the
-- dwell runs out.
local function behavior_walk_turn(e)
    if e.action_state == 0 then
        anim_start(e, 1, 3)
        e.action_ticks_counter =
            ((e:random() & 0x1F) * e.action_behavior) & 0xFFFF
        if (e.action_behavior & 0xFE) == 0 then
            e.action_ticks_counter = (e.action_ticks_counter + 0x50) & 0xFFFF
        end
    elseif e.action_state ~= 1 then
        return
    end

    local frame = e.animation_frame_id
    local sfx = frame < 16 and FRAME_SFX[frame + 1] or 0
    if sfx ~= 0 then
        e:play_enemy_sound(sfx)
    end

    e:advance_anim(0x400)

    -- The leg probe reads the frame the clip just moved to.
    local moved = e.animation_frame_id
    local part = 1
    if moved == 0 or moved > 8 then
        part = 0
    end
    e:leg_reach(part, e.joint_scale)

    local d = signed_word(e.action_ticks_counter)
    e.action_ticks_counter = d - 1
    if d == 0 then
        e.ignore = 0
        e.action_behavior = 0
        e.action_state = 0
    end
end

-- Behaviour 2: strafe. Side-step at a fixed turn whose sign comes from the
-- dwell's low bit, glance at the player, and collapse to the wide-turn walk
-- when the line of sight opens.
local function behavior_strafe(e)
    if e.action_state == 0 then
        anim_start(e, 1, 3)
        e.animation_id = 3
        e.action_ticks_counter = (e:random() & 0x3F) & 0xFFFF
        e.ws_turn = 0x18
        if (e.action_ticks_counter & 1) ~= 0 then
            e.ws_turn = -0x18
        end
    elseif e.action_state ~= 1 then
        return
    end

    e.angle = e.angle + e.ws_turn
    e:advance_anim(0x400)

    local px, _, pz = e:player_pos()
    if (e.ws_path_word & 1) ~= 0 then
        if e:turn_toward_target(px, pz, e.ws_turn) == 0 then
            e.action_behavior = 4
            e.action_state = 0
            return
        end
    end

    local d = signed_word(e.action_ticks_counter)
    e.action_ticks_counter = d - 1
    if d == 0 then
        e.ignore = 0
        e.action_behavior = (e.ws_path_word & 1) << 2
        e.action_state = 0
    end
end

-- Behaviour 3/5: approach. Hold the caller's turn step and close until the
-- dwell or the alignment runs out; then collapse to the chase (behaviour 11
-- when the line of sight is clear) and latch the player's position as the
-- waypoint.
local function behavior_approach(e)
    local sub = e.action_state
    if sub == 0 then
        anim_start(e, 1, 3)
        e.hit_state = 0
        e.action_ticks_counter = ((e:random() & 0x1F) + 0x50) & 0xFFFF
    elseif sub ~= 1 then
        if sub ~= 2 then
            return
        end
        e.ignore = 0
        e.action_behavior = (e.ws_path_word & 1) * 0x0B
        e.action_state = 0
        local px, _, pz = e:player_pos()
        e.player_pos_x = px
        e.player_pos_z = pz
        return
    end

    local px, _, pz = e:player_pos()
    local turn = e:turn_toward_target(px, pz, e.ws_turn)
    local d = signed_word(e.action_ticks_counter)
    e.action_ticks_counter = d - 1
    if d == 0 or turn == 0 then
        e.action_state = 2
    end
    e:advance_anim(0x400)
    e.angle = e.angle + turn
end

-- Behaviour 6: lunge. Wait, rush, turn in, bite, shake and recover. The bite
-- is the spider's only direct damage: inside reach with the player already
-- moving, it subtracts 10 health (18 on a second playthrough), clamps to 1 and
-- writes the facing-based reaction the shared layer reads.
local function behavior_lunge(e)
    local sub = e.action_state
    if sub == 0 then
        anim_start(e, 1, 3)
        e.animation_id = 8
        sub = 1
    end

    if sub == 1 then
        e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
        e.move_speed_current = 100
        e:move(0, 100)
        return
    end

    if sub == 2 then
        e.action_state = 3
        anim_start(e, 3, 3)
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
            e.action_state = 6
        end
        return
    end

    if sub == 4 then
        e.action_state = 5
        anim_start(e, 5, 3)
        e.animation_id = 0xB
        e.action_ticks_counter = ((e:random() & 0x1F) + 10) & 0xFFFF
        sub = 5
    end

    if sub == 5 then
        e:advance_anim(0x400)
        local px, _, pz = e:player_pos()
        e.angle = e.angle + e:turn_toward_target(px, pz, 0x10)
        e.move_speed_current = 300
        e:move(0, 300)

        local d = signed_word(e.action_ticks_counter)
        e.action_ticks_counter = d - 1
        if d == 0 or e:turn_toward_target(px, pz, 0x200) ~= 0 then
            e.action_state = 6
        end

        if e.ws_touch ~= 0 and e.player_attacked == 0 then
            local facing = e:player_facing_entity() and 1 or 0
            e.action_state = 6
            e:play_enemy_sound(4)
            e.player_health = e.player_health
                - (e.second_playthrough and 0x12 or 10)
            if e.player_health < 0 then
                e.player_health = 1
            end
            e.player_attacked = facing + 1
            e.player_action_behavior = facing + 0x66
            return
        end
        return
    end

    if sub == 6 then
        e.action_state = 7
        anim_start(e, 7, 3)
        e.animation_id = 9
        e.action_ticks_counter = 7
        sub = 7
    end

    if sub == 7 then
        e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
        e.move_speed_current = 100
        e:move(0, 100)
        return
    end

    if sub == 8 then
        e.ignore = 0
        e.action_behavior = 0
        e.action_state = 0
        e.ws_delay = 0xF
    end
end

-- Behaviour 7: spit. Rears back with a web glob at the entity matrix and drops
-- back into the tiny-turn chase when the clip trips.
local function behavior_spit(e)
    local sub = e.action_state
    if sub == 0 then
        anim_start(e, 1)
        e.animation_id = 0xC
        e:spawn_effect(0x1E, 0, 1000, -600, 0, 0, 0)
        e:play_enemy_sound(7)
        sub = 1
    elseif sub ~= 1 then
        if sub ~= 2 then
            return
        end
        e.state = 1
        e.ignore = 0
        e.action_behavior = 0xB
        e.action_state = 0
        e.ws_delay = 0xF
        return
    end

    e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
end

-- Behaviour 8: web drop. The ceiling spider swings down on its thread (a
-- squared dwell ramp against the pitch), lands, clears the ceiling spawn bit
-- and plays the landing clip.
local function behavior_webdrop(e)
    local sub = e.action_state
    if sub == 0 then
        anim_start(e, 1, 3)
        e.animation_id = 0
        e.action_ticks_counter = 0
        sub = 1
    end

    if sub == 1 then
        local dwell = signed_word(e.action_ticks_counter)
        e.pos_y = e.pos_y + dwell * dwell * 8
        e.action_ticks_counter = e.action_ticks_counter + 1
        e.pitch = e.pitch - 0x100
        if (e.pitch & 0x8000) ~= 0 then
            e.pitch = 0
        end
        if e.pos_y >= 0 then
            e.pos_y = 0
            e.pitch = 0
            e.action_state = 2
        end
        e:advance_anim(0x400)
        return
    end

    if sub == 2 then
        e.action_state = 3
        anim_start(e, 3, 3)
        e.animation_id = 7
        e:play_enemy_sound(3)
        e.behavior_flags = e.behavior_flags & 0xFD
        sub = 3
    end

    if sub == 3 then
        e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
        return
    end

    if sub == 4 then
        e.ignore = 0
        e.action_behavior = 0
        e.action_state = 0
    end
end

-- Behaviour 9: a deliberate no-op slot.
local function behavior_ret(_e)
end

-- Behaviour 10: circle. Orbits the player on a fixed launch direction whose
-- velocity is stored once and re-added every frame while the heading keeps
-- correcting.
local function behavior_circle(e)
    local sub = e.action_state
    if sub == 0 then
        anim_start(e, 1, 3)
        e.animation_id = 6
        local base = (e:random() & 1) * -0x658 + 0x32C
        local los = e.ws_path_word & 1
        e.ws_turn = base + (los == 0 and 1 or 0) * (e:random() & 0xFFF)
        e.move_speed_current = (e:random() & 0x3F) + 200
        e:move(e.ws_turn, signed_word(e.move_speed_current))
    elseif sub ~= 1 then
        if sub ~= 2 then
            return
        end
        e.hit_state = 0
        e.ignore = 0
        e.action_behavior = (e.ws_path_word & 1) << 2
        e.action_state = 0
        e.ws_count = 0
        return
    end

    e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
    local px, _, pz = e:player_pos()
    e.angle = e.angle + e:turn_toward_target(px, pz, 0x40)
    e:advance_speed()
end

-- The three spawn-kind behaviour pickers. They only choose the next behaviour
-- and raise the ignore latch; the action runner executes it the same frame.
-- A picker that loses the line of sight or gets close enough commits to a
-- chase; a knocked-down player draws the circle.

local function picker_a(e)
    local los = e.ws_path_word & 1
    if web_dist(e) < 6000 and los == 0 then
        e.ignore = 1
        e.action_behavior = 3
        e.action_state = 0
    end
    if e.ws_delay == 0 then
        if (e:random() & 0x1FF) == 0 and los ~= 0 then
            e.ignore = 1
            e.action_behavior = 6
            e.action_state = 0
        end
        if web_dist(e) < 7000 then
            local px, _, pz = e:player_pos()
            if e:turn_toward_target(px, pz, 0x80) == 0 then
                e.ignore = 1
                e.action_behavior = 6 + (e:random() & 1)
                e.action_state = 0
            end
        end
    end
    if player_down(e) then
        if (e:random() & 1) == 0 then
            e.ignore = 1
            e.action_behavior = 10
            e.action_state = 0
        end
    end
    if e.ws_count > 0x1E then
        e.ignore = 1
        e.action_behavior = 10
        e.action_state = 0
    end
    if (e.player_attacked & 0x80) ~= 0 then
        e.ignore = 1
        e.action_behavior = 0xB
        e.action_state = 0
    end
end

local function picker_b(e)
    local px, _, pz = e:player_pos()
    local turn_slow = e:turn_toward_target(px, pz, 0x80)
    local turn_fast = e:turn_toward_target(px, pz, 0x100)

    if player_down(e) and turn_slow ~= 0 then
        e.ignore = 1
        e.action_behavior = 3
        e.action_state = 0
    end
    if web_dist(e) < 8000 and turn_slow ~= 0 then
        e.ignore = 1
        e.action_behavior = 3
        e.action_state = 0
    end
    if e.ws_delay == 0 then
        if web_dist(e) < 0x1D4C and (e.ws_path_word & 1) ~= 0 then
            e.ignore = 1
            e.action_behavior = 6 + (e:random() & 1)
            e.action_state = 0
        end
        if web_dist(e) < 6000 and turn_fast == 0 then
            e.ignore = 1
            e.action_behavior = 7
            e.action_state = 0
        end
    end
    if player_down(e) then
        if (e:random() & 0xF) == 0 then
            e.ignore = 1
            e.action_behavior = 10
            e.action_state = 0
        end
    end
    if e.ws_count > 0x1E then
        e.ignore = 1
        e.action_behavior = 10
        e.action_state = 0
    end
end

local function picker_c(e)
    if web_dist(e) < 3000 then
        e.ignore = 1
        e.action_behavior = 8
        e.action_state = 0
        return
    end
    if player_down(e) then
        e.ignore = 1
        e.action_behavior = 8
        e.action_state = 0
        return
    end
    if web_dist(e) > 5000 then
        e.ignore = 1
        e.action_behavior = 2
        e.action_state = 0
        if web_dist(e) > 5000 then
            local px, _, pz = e:player_pos()
            if e:turn_toward_target(px, pz, 0x80) == 0 then
                e.ignore = 1
                e.action_behavior = 0xB
                e.action_state = 0
            end
        end
    end
end

local PICKERS = { picker_a, picker_b, picker_c, picker_c }

-- The action runner: refresh the pathfinder, keep its line-of-sight bit in
-- the +0x16E word, then execute the selected behaviour.
local function action_runner(e)
    local px, _, pz = e:player_pos()
    local path = e:pathfind_update(px, pz)
    e:pathfind_keep(path)

    local b = e.action_behavior
    if b == 0 then
        behavior_idle(e)
    elseif b == 1 then
        e.animation_id = 3
        e.angle = e.angle + e:turn_toward_target(px, pz, 0x10) * (e.ws_path_word & 1)
        behavior_walk_turn(e)
        e.move_speed_current = e.move_speed_current + 0x3C
        e:move(0, signed_word(e.move_speed_current))
    elseif b == 2 then
        behavior_strafe(e)
    elseif b == 3 then
        e.animation_id = 2
        e.ws_turn = 0x80
        behavior_approach(e)
    elseif b == 4 then
        e.animation_id = 2
        e.angle = e.angle + e:turn_toward_target(px, pz, 0x30)
        behavior_walk_turn(e)
        e.move_speed_current = e.move_speed_current + 100
        e:move(0, signed_word(e.move_speed_current))
    elseif b == 5 then
        e.animation_id = 3
        e.ws_turn = 0x20
        behavior_approach(e)
    elseif b == 6 then
        behavior_lunge(e)
    elseif b == 7 then
        behavior_spit(e)
    elseif b == 8 then
        behavior_webdrop(e)
    elseif b == 9 then
        behavior_ret(e)
    elseif b == 10 then
        behavior_circle(e)
    elseif b == 11 then
        e.animation_id = 3
        e.angle = e.angle + e:turn_toward_target(px, pz, 0x10)
        behavior_walk_turn(e)
        e.move_speed_current = e.move_speed_current + 0x14
        e:move(0, signed_word(e.move_speed_current))
    end
end

-- The web build (state 2 body): spin a thread off the next free leg joint.
-- The chosen joint and its neighbour are armed (their active flag clears,
-- which the leg reach reads) and two web billboards are queued at their world
-- matrices; the used-joint registry is the shared Rust state.
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
            if e:web_joint_use(chosen, e.ws_web_index) then
                e:arm_joint_effect(chosen, 0x1E, 0x14, 3)
                e:arm_joint_effect(chosen + 1, 0x1E, 0x14, 3)
                e:spawn_joint_effect(0, 8, chosen + 1, 0)
                e:spawn_joint_effect(0, 8, chosen, 0)
                e:play_enemy_sound(5)
                e.ws_web_index = e.ws_web_index + 1
            end
        end
    elseif sub ~= 1 then
        if sub ~= 2 then
            return
        end
        e.state = 1
        e.ignore = 0
        e.action_behavior = (e.behavior_flags & 1) + 3
        e.action_state = 0
        local px, _, pz = e:player_pos()
        e.player_pos_x = px
        e.player_pos_z = pz
        e.hit_state = 0
        if (e.ws_path_word & 1) == 0 then
            return
        end
        e.ignore = 1
        e.action_behavior = 10
        e.action_state = 0
        return
    end

    e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
end

-- The normal web shooter (state 3 behaviours other than 1/2/6/8/9). Case 0
-- primes the fangs and queues the fang billboards, case 2 stages the spit and
-- the small SCA profile, case 3 dangles the body and spawns the threads, and
-- case 6 keeps them flying.
local function web_shoot_a(e)
    local sub = e.action_state
    if sub == 0 then
        e.move_speed_current = 0
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.action_ticks_counter = 0x1E
        e.blend_counter = 3
        e:arm_joint_effect(0, 0x1E, 0x1E, 3)
        e:arm_joint_effect(19, 0x1E, 0x1E, 3)
        e:spawn_joint_effect(0, 8, 0, 0)
        e:spawn_joint_effect(0, 8, 19, 0)
        e:raise_death_event()
        e:play_enemy_sound(5)
        -- The raw 0x0C/0x10 flag ORs over joints 3..18 are not modelled.
        sub = 1
    end

    if sub == 1 then
        e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
        return
    end

    if sub == 2 then
        e.shadow_tint = 0x00DF809F
        e:adjust_shadow_size(-100, -100)
        e.action_state = 3
        e.status_flags = e.status_flags | 2
        e.status_flags = e.status_flags | 8
        e.move_speed_current = 0
        e.action_ticks_counter = 0x5A
        e:set_sca(500, 0, 0, 0, 0)
        sub = 3
    end

    if sub == 3 then
        e:adjust_shadow_size(3, 3)
        local d = signed_word(e.action_ticks_counter) - 1
        e.action_ticks_counter = d
        if d == 0 then
            e.action_state = 6
            e:web_spawn(((e.hit_state >> 2) & 0xFE) + 8)
        end
        return
    end

    if sub == 6 then
        e:raise_death_event()
        e:web_update(((e.hit_state >> 2) & 0xFE) + 8)
        return
    end
end

-- The alternate web shooter (state 3 behaviours 1/2/6/8/9): the same shape
-- with a lower thread count. A fresh hit while it is priming re-arms the fangs
-- and restarts the case-2 spit; the bit-1 hit skips straight to the dangle.
local function web_shoot_b(e)
    local sub = e.action_state
    if sub == 1 or sub == 3 then
        if (e.hit_state & 2) ~= 0 then
            e:arm_joint_effect(0, 0x1E, 0x1E, 3)
            e:arm_joint_effect(19, 0x1E, 0x1E, 3)
            e:spawn_joint_effect(0, 8, 0, 0)
            e:spawn_joint_effect(0, 8, 19, 0)
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
        e:raise_death_event()
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
        e.status_flags = e.status_flags | 2
        e.status_flags = e.status_flags | 8
        e.animation_id = 5
        e.action_state = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.hit_state = 0
        e.shadow_tint = 0x00DF809F
        sub = 3
    end

    if sub == 3 then
        e.status_flags = (e.status_flags & 0x1F) | 0x20
        e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
        return
    end

    if sub == 4 then
        e.animation_id = 5
        e.action_state = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 1
        e:play_enemy_sound(5)
        e.shadow_tint = 0x00DF809F
        e:adjust_shadow_size(-100, -100)
        e.action_state = 5
        e.action_ticks_counter = 0x78
        e.move_speed_current = 0
        e.status_flags = e.status_flags | 2
        e.status_flags = e.status_flags | 8
        e:set_sca(500, 0, 0, 0, 0)
        sub = 5
    end

    if sub == 5 then
        e:adjust_shadow_size(3, 3)
        e:advance_anim(0x400)
        local d = signed_word(e.action_ticks_counter) - 1
        e.action_ticks_counter = d
        if d ~= 0 then
            return
        end
        e.action_state = 6
        e:web_spawn(((e.hit_state >> 2) & 0xFE) + 4)
        return
    end

    if sub == 6 then
        e:raise_death_event()
        e:web_update(((e.hit_state >> 2) & 0xFE) + 4)
        return
    end
end

-- State 0: spawn. Kinds 0/1 rest on the floor, kind 2 hangs from the ceiling
-- (lower in the guardhouse) with the nose down and no ground shadow. The
-- health is one discarded draw plus a 16-entry roll; the big SCA profile and
-- the 0x4B0 shadow install here.
local function init(e)
    e.ignore = 0
    e.action_behavior = 0
    e.action_state = 0
    e.action_ticks_counter = 0
    e.death_timer = 0
    e.hit_state = 0
    e.animation_id = 0

    if e.behavior_flags < 2 then
        e.pos_y = 0
    end
    if e.behavior_flags > 1 then
        e.pitch = 0x0800
        e.roll = 0
        e.pos_y = -6136
        if e.stage == 4 then
            e.pos_y = -7200
        end
        if e.behavior_flags > 3 then
            e.behavior_flags = 2
        end
    end

    e:reset_joints()
    e.state = 1
    e:advance_anim(0x400)

    e:set_shadow_offset(0, 0, 0)
    e.shadow_tint = 0x00808080
    e.shadow_half_x = 0x4B0
    e.shadow_half_z = 0x4B0

    e:random()
    e.health = HEALTH[(e:random() & 0xF) + 1]

    e:set_sca(1000, 180, 0, -180, 0)
    e.status_flags = e.status_flags & 0x1F
    e.joint_scale = (e.behavior_flags & 4) * 0xAAA

    e.ws_delay = 0
    e.ws_splat = 0
    e.ws_count = 0
    e.ws_web_index = 0
end

-- State 1: the driver. The picker runs while the ignore latch is clear (and
-- the runner always follows it), the runner alone while it is 1. The trailing
-- block keeps the status bits, checks the visual range, and for the ceiling
-- variant drops to behaviour 8 on a close or knocked-down player. The
-- pathfinder trail gates the chase counter.
local function run(e)
    if (e.behavior_flags & 0x80) ~= 0 then
        return
    end

    if e.ignore == 0 then
        PICKERS[(e.behavior_flags & 3) + 1](e)
        action_runner(e)
    elseif e.ignore == 1 then
        action_runner(e)
    end

    e.status_flags = (e.status_flags & 0x1F) | 0x40
    e:check_visual_range(4000)

    if (e.behavior_flags & 2) ~= 0 then
        e.status_flags = e.status_flags & 0x1F
        if web_dist(e) < 3000 and e.action_behavior ~= 8 then
            e.ignore = 1
            e.action_behavior = 8
            e.action_state = 0
        end
        if player_down(e) then
            e.ignore = 1
            e.action_behavior = 8
            e.action_state = 0
        end
    end

    if e.ws_delay ~= 0 then
        e.ws_delay = e.ws_delay - 1
    end
    if e.ws_trail == 0 then
        e.ws_count = 0
        return
    end
    e.ws_count = e.ws_count + 1
end

-- State 2: the web-build wait; behaviour 0 spins a thread each frame.
local function build(e)
    if e.action_behavior == 0 then
        e.animation_id = 1
        web_build(e)
    end
end

-- State 3: the web-shoot state. The first frame maps the damage hit_state to a
-- behaviour and latches the shooter; the splat word re-enters the alternate
-- spit. Behaviours 1/2/6/8/9 take the alternate shooter.
local function shoot_state(e)
    if e.ignore == 0 then
        e.action_behavior = e.hit_state >> 3
        e.ignore = 1
        if e.ws_splat ~= 0 then
            e.action_state = 3
            e.action_behavior = 2
        end
        e.ws_splat = 1
    end

    local b = e.action_behavior
    if b == 1 or b == 2 or b == 6 or b == 8 or b == 9 then
        web_shoot_b(e)
    else
        e.animation_id = 1
        web_shoot_a(e)
    end
end

local function empty(_e)
end

local STATES = {
    init, run, build, shoot_state, empty, empty,
    picker_a, picker_b, picker_c, picker_c,
}

function update(e)
    -- The state machine and the whole SCA pass run only while monsters are
    -- unpaused; state 5 skips the SCA tail. The literal second gate cannot be
    -- false inside the running branch, so the state-3 re-entry is reached only
    -- on a paused frame (the shooter still runs once per frame either way).
    if not e.monster_paused then
        local state = STATES[e.state + 1]
        if state then
            state(e)
        end

        if e.state ~= 5 then
            e.ws_touch = e:separate() and 1 or 0
            e.ws_trail = e:resolve_collision()
        end

        if not e.monster_paused then
            -- The original's second test always passes here.
        elseif e.state == 3 and e.action_state == 6 then
            shoot_state(e)
        end
    elseif e.state == 3 and e.action_state == 6 then
        shoot_state(e)
    end

    e:update_switch_zone()
    -- The ceiling spider queues no ground shadow until the drop clears spawn
    -- bit 1; the renderer reads the suppression flag.
    e.shadow_suppressed = (e.behavior_flags & 2) ~= 0
end
