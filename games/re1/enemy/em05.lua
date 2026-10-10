-- Crow, entity id 0x05.
--
-- The mansion's crows. A four-level machine, like the wasp:
--
--   state            init / driver / recoil / death / five SCD-flight slots
--   ignore           0 = perched, watching for the take-off cue, 1 = airborne,
--                    anything else freezes the dispatcher
--   action_behavior  the fourteen-entry flight behaviour (behaviour 13 launches
--                    a perched crow; behaviour 0 waits on a branch)
--   action_state     the per-behaviour animation sub-state, driven by the four
--                    anim helpers
--
-- Altitude is just pos_y and Y is negative up: the flight envelope is -100
-- (floor) to -30000 (ceiling), "climbing" subtracts, and the ground shadow
-- shrinks with `Y // 16 + 400` floored at 50. The driver steers toward the
-- player only while the pathfinder's bit 0 is set, bounces off geometry with
-- the shared swerve helper (a 90-degree sidestep held four frames), rams the
-- player when fast enough and facing away, and commits to a strike run or a
-- full dive on the way down. The peck bites through the shared player
-- helpers, and the grab rides the player with a signed struggle counter the
-- player shakes off by mashing about nine times faster. A killed crow drops:
-- real floor fades the shadow yellow and plays the landing, an off-map death
-- (the floor step below -2000) bursts every joint and removes the entity.
--
-- The spawn record's behavior_flags selects the entrance: bit 4 starts the
-- crow airborne in the high hover, bit 1 parks it under SCD control (state 8),
-- bit 0 is the scripted set-dressing variant (no health, a -400 altitude bias)
-- and bit 7 skips the update entirely. Every RNG draw comes from the platform
-- stream in the original's order; all state lives on the Rust entity, so the
-- scripting VM may be reset between any two updates.
--
-- The original's write-only `field_00`, `CR_UNK_C1` and matrix-scratch writes
-- are not modelled (nothing reads them), the two per-joint `0x0C`/`0x10` flag
-- ORs stay with the renderer's documented joint-object deferral, and the death
-- burst hides every joint through the joint mask instead of arming the async
-- gore objects.

local JOINTS = 13

-- The 16-bit signed view of a word the original reads through a `short`.
local function signed_word(value)
    if value >= 0x8000 then
        return value - 0x10000
    end
    return value
end

-- C integer division truncates toward zero; Lua's `//` floors.
local function idiv(a, b)
    if (a < 0) ~= (b < 0) then
        return -((-a) // b)
    end
    return a // b
end

-- One word write over `action_behavior` + `action_state`: every behaviour
-- assignment zeroes the sub-state as it writes the behaviour.
local function set_behavior(e, behavior)
    e.action_behavior = behavior
    e.action_state = 0
end

-- The original's whole-dword state write: state, ignore, behaviour, sub-state.
local function set_crow_state(e, state, airborne, behavior, substate)
    e.state = state
    e.ignore = airborne
    e.action_behavior = behavior
    e.action_state = substate
end

-- Two feather puffs, half a turn apart, at the crow's own matrix.
local function feathers(e)
    e:spawn_effect(0x1C, 0, 0, 0, 0, e.angle, 0)
    e:spawn_effect(0x1C, 0, 0, 0, 0, e.angle + 0x400, 0)
end

-- `crow_anim_hold`: start the current clip and loop it forever; the behaviour
-- has to move itself on.
local function anim_hold(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.death_timer = 0
        e.blend_counter = 3
    elseif st ~= 1 then
        return
    end
    e:advance_anim(0x400)
end

-- `crow_anim_step`: as above, but the clip clock's completion flag is added to
-- the sub-state, so it reaches 2 on the frame the clip ends; at 2 the crow
-- drops back to the perched behaviour word.
local function anim_step(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.death_timer = 0
        e.blend_counter = 3
    elseif st ~= 1 then
        if st ~= 2 then
            return
        end
        e.ignore = 0
        e.action_behavior = 0
        e.action_state = 0
        return
    end
    local adv = e:advance_anim(0x400) and 1 or 0
    e.action_state = e.action_state + adv
end

-- `crow_anim_turn`: loop the flap while turning toward the player at the
-- rolled turn rate, for 80..142 frames or until the turn completes. On
-- completion it hands back to the driver and latches the player's position as
-- the waypoint.
local function anim_turn(e, px, pz)
    local st = e.action_state
    if st == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.death_timer = 0
        e.blend_counter = 3
        e.action_state = 1
        e.action_ticks_counter = ((e:random() & 0x1F) * 2 + 0x50) & 0xFFFF
    elseif st ~= 1 then
        if st ~= 2 then
            return
        end
        set_crow_state(e, 1, 0, 0, 0)
        e.player_pos_x = px
        e.player_pos_z = pz
        return
    end

    local turn = e:turn_toward_target(px, pz, e.crow_turn_rate)
    local ticks = signed_word(e.action_ticks_counter)
    e.action_ticks_counter = ticks - 1
    if ticks == 0 or turn == 0 then
        e.action_state = 2
    end
    e:advance_anim(0x400)
    e.angle = e.angle + turn
end

-- `crow_anim_grab`: the crow rides the player pecking. CR_STRUGGLE starts at
-- 100 and loses one per frame, or nine while the player is mashing; it is a
-- signed byte and the release test is `< 0`. The grab also ends if the player
-- walks beyond 1500 units and is no longer aligned.
local function anim_grab(e, px, pz)
    local st = e.action_state
    if st == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.death_timer = 0
        e.blend_counter = 3
        e.action_state = 1
        e.crow_struggle = 100
    elseif st ~= 1 then
        if st ~= 2 then
            return
        end
        -- Release: peel off at speed, snap 180 degrees half the time, climb.
        e.move_speed_current = (4 - (e:random() & 1)) * 0x32 + e.crow_dist // 100
        e.roll = 0xFF00
        e.angle = e.angle + (e:random() & 1) * 0x800
        e.pos_y = e.pos_y - 100
        set_crow_state(e, 1, 1, 6, 0)
        e.hit_state = 0
        return
    end

    e:advance_anim(0x400)

    if e.player_health < 0 then
        set_crow_state(e, 1, 1, 12, 0)
        e:set_player_animation(3, 0)
        e:set_player_action(200, 0)
        return
    end

    local mashed = e:player_mashing()
    e.crow_struggle = e.crow_struggle - ((mashed and 8 or 0) + 1)

    if e.crow_struggle < 0 and e.player_action_state == 3 then
        e.action_state = 2
        e.player_action_state = 4
        return
    end
    if e.crow_dist > 0x5DC then
        if e:turn_toward_target(px, pz, 0x100) ~= 0 then
            e.action_state = 2
            e.player_action_state = 4
            return
        end
    end

    -- A peck lands every 20 player animation frames.
    if e.player_animation_frame_id % 0x14 == 0 then
        e:spawn_player_effect(0, 1, 100, -0x974, 0, 0x200)
        e:spawn_effect(0x1C, 0, 0, 0, 0, e.angle, 0)
        e.player_health = e.player_health - (e.second_playthrough and 4 or 3)
        e:play_enemy_sound(3)
    end
end

-- Behaviour 0: perched. Sit still for 40..102 frames, then play one of three
-- idle clips (11, 12, or a quarter of the time 2 with a caw) and return to the
-- driver when it finishes.
local function perch(e)
    local st = e.action_state
    if st == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.death_timer = 0
        e.blend_counter = 3
        e.action_state = 1
        e.action_ticks_counter = ((e:random() & 0x1F) * 2 + 0x28) & 0xFFFF
    end

    if st == 0 or st == 1 then
        local ticks = signed_word(e.action_ticks_counter)
        e.action_ticks_counter = ticks - 1
        if ticks == 0 or e.player_health < 0 then
            e.action_state = 2
        end
        return
    end

    if st == 2 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.death_timer = 0
        e.blend_counter = 3
        e.action_state = 3
        e.animation_id = 0xB
        if (e:random() & 1) ~= 0 then
            e.animation_id = 0xC
            if (e:random() & 1) ~= 0 then
                e.animation_id = 2
                e:play_enemy_sound(1)
            end
        end
    elseif st == 4 then
        set_crow_state(e, 1, 0, 0, 0)
        return
    elseif st ~= 3 then
        return
    end

    local adv = e:advance_anim(0x400) and 1 or 0
    e.action_state = e.action_state + adv
end

-- Behaviour 1: wingbeat. A flat 30/frame climb plus, past frame 8, a sink of
-- (frame + 52), so the crow rises for eight frames and then falls faster -
-- the flap's arc. Ends in the descending glide, or the strike run if already
-- low.
local function wingbeat(e)
    e.animation_id = 1
    e.move_speed_current = ((e:random() & 1) * 0x10) + 0x32
    anim_step(e)
    e:move(0, signed_word(e.move_speed_current))
    e.pos_y = e.pos_y - 30
    if e.animation_frame_id > 8 then
        e.crow_vy = e.animation_frame_id + 0x34
        e.pos_y = e.pos_y + e.crow_vy
    end
    e.roll = e.roll + 0x15

    if e.ignore == 0 or e.pos_y > -1500 then
        e.roll = 0x100
        set_crow_state(e, 1, 1, 5, 0)
        if e.pos_y > -800 then
            set_crow_state(e, 1, 1, 7, 0)
        end
    end
end

-- Behaviour 2: dive. Nose down and drop; speed bleeds off six per frame and
-- the crow descends by the ceiling bound behaviour 5 rolled for this dive.
local function dive(e, px, py, pz, bkp)
    e.animation_id = 4
    anim_step(e)
    e.move_speed_current = 200 - e.animation_frame_id * 6
    if signed_word(e.move_speed_current) > 0 then
        e:move(0, signed_word(e.move_speed_current))
    end
    e.pos_y = e.pos_y - idiv(e.crow_ceil_limit, 0x28)
    e.roll = e.roll - 6

    if e.ignore == 0 or e.pos_y > -450 then
        e.move_speed_current = 0
        e.roll = 0
        e.pos_y = -450
        set_crow_state(e, 1, 1, 3, 0)
        if bkp == 0 then
            set_behavior(e, 4)
        end
        if e.player_health < 0 then
            set_behavior(e, 0)
        end
    end
end

-- Behaviour 3: banking turn. Circle back for another pass at a random turn
-- rate of 0x40, 0x60, 0x80 or 0xA0 per frame.
local function bank_turn(e, px, py, pz, bkp)
    e.animation_id = 3
    local r1 = e:random()
    local r2 = e:random()
    e.crow_turn_rate = (r1 & 1) * 0x20 - (r2 & 1) * 0x40 + 0x80
    anim_turn(e, px, pz)

    if e.ignore == 0 then
        set_crow_state(e, 1, 1, 3, 0)
        if bkp == 0 then
            set_behavior(e, 4)
        end
        if e.player_health < 0 then
            set_behavior(e, 0)
        end
    end
end

-- Behaviour 4: strike run. The committed attack run; past frame 8 it computes
-- the climb velocity it will need on the way out and hands over to the low
-- hover with a caw.
local function strike_run(e)
    e.animation_id = 7
    anim_step(e)
    e.roll = e.roll - 0x19

    if e.animation_frame_id > 8 then
        local dist = e.crow_dist
        if dist ~= 0 then
            local scaled = (dist // 100 + 200) * 2600
            e.crow_vy = idiv(scaled, dist)
            if e.crow_vy > 500 then
                e.crow_vy = 200
            end
            e.move_speed_current = dist // 100 + 200
            e.roll = 0xFF00
            set_crow_state(e, 1, 1, 6, 0)
            e:play_enemy_sound(4)
        end
    end
end

-- Behaviour 5: descending glide. Sink on a velocity that ramps one per frame
-- to 500 and snaps back to 200; steering scales by the pathfinder bit, so the
-- crow only corrects course on frames where the path is clear. Passing head
-- height while aligned turns into the peck.
local function descend(e, px, py, pz, bkp)
    e.animation_id = 0
    local turn = e:turn_toward_target(px, pz, 0x10)
    e.angle = e.angle + turn * (e.crow_path_word & 1) * 2

    if e.crow_dist < 2000 and e.animation_frame_id % 9 == 0 then
        e:play_enemy_sound(5)
    end
    anim_hold(e)
    e:move(0, signed_word(e.move_speed_current))

    e.crow_vy = e.crow_vy + 1
    if e.crow_vy > 500 then
        e.crow_vy = 200
    end
    e.pos_y = e.pos_y + e.crow_vy

    e.crow_ceil_limit = ((e:random() & 1) * -500 + e.crow_alt_bias) * 2 - 1500

    if e.crow_ceil_limit < e.pos_y or e.pos_y > -1500 then
        set_crow_state(e, 1, 1, 7, 0)
        if bkp ~= 0 and (e.behavior_flags & 1) == 0 then
            set_crow_state(e, 1, 1, 2, 0)
        end
    end
    if e.crow_dist < 0x4B0 then
        if e:turn_toward_target(px, pz, 0x100) == 0
            and ((-2500 - e.pos_y) & 0xFFFFFFFF) < 300 then
            set_crow_state(e, 1, 1, 9, 0)
        end
    end
end

-- Behaviour 6: climb out. The mirror of the glide: the same steering and caw
-- but the velocity is subtracted, so the crow gains height. Rolls a ceiling
-- bound of roughly -4500 or -5500 a frame and levels out past it.
local function ascend(e, px, py, pz, bkp)
    e.animation_id = 5
    local turn = e:turn_toward_target(px, pz, 0x10)
    e.angle = e.angle + turn * (e.crow_path_word & 1) * 2

    if e.crow_dist < 2000 and e.animation_frame_id % 9 == 0 then
        e:play_enemy_sound(5)
    end
    anim_hold(e)
    e:move(0, signed_word(e.move_speed_current))

    e.crow_vy = e.crow_vy + 1
    if e.crow_vy > 500 then
        e.crow_vy = 200
    end
    e.pos_y = e.pos_y - e.crow_vy

    e.crow_floor_limit = (e:random() & 1) * -1000 + e.crow_alt_bias - 0x1194

    if e.pos_y < e.crow_floor_limit then
        set_crow_state(e, 1, 1, 10, 0)
    end
    if e.crow_dist < 0x4B0 then
        if e:turn_toward_target(px, pz, 0x100) == 0
            and ((-2500 - e.pos_y) & 0xFFFFFFFF) < 300 then
            set_crow_state(e, 1, 1, 9, 0)
        end
    end
end

-- Behaviour 7: dive attack. Speed is set from the distance so the crow arrives
-- fast from far away; the run ends after 18 animation frames or the instant
-- the player is hit. Landing the hit while the player is in reaction 4 also
-- knocks the crow back and spins it half the time.
local function dive_attack(e, px, py, pz)
    e.animation_id = 7
    anim_step(e)
    e.move_speed_current = (4 - (e:random() & 1)) * 0x32 + e.crow_dist // 100
    e:move(0, signed_word(e.move_speed_current))
    e.roll = e.roll - 0x19

    if e.animation_frame_id > 0x12 or (e.player_attacked & 1) ~= 0 then
        if (e.player_attacked & 1) ~= 0 and e.player_action_state == 4 then
            e.pos_y = e.pos_y - 50
            e.angle = e.angle + (e:random() & 1) * 0x800
        end
        e.roll = 0xFF00
        set_crow_state(e, 1, 1, 6, 0)

        if (e.behavior_flags & 1) ~= 0 then
            local dist = e.crow_dist
            if dist ~= 0 then
                local scaled = ((dist // 100) * 0x145 + 65000) * 8
                e.crow_vy = idiv(scaled, dist)
                if e.crow_vy > 500 then
                    e.crow_vy = 200
                end
            end
        end
    end
end

-- Behaviour 8: approach. A short turn-and-close before the climb-out takes
-- over.
local function approach(e, px, py, pz)
    e.animation_id = 5
    e:rotate_toward_target(px, pz, 0x40)
    anim_hold(e)
    e:move(0, signed_word(e.move_speed_current))
    if e.animation_frame_id > 8 then
        set_crow_state(e, 1, 1, 6, 0)
    end
end

-- Behaviour 9: peck. Hover at head height and jab. Once the flap clip ends
-- and the crow is lined up with a clear path it bites and latches on
-- (behaviour 11); a player already in a reaction still parks the crow on 9.
local function peck(e, px, py, pz)
    e.animation_id = 8
    e:rotate_toward_target(px, pz, 0x30)
    anim_step(e)

    if (e.player_attacked & 1) ~= 0 and e.player_action_state > 3 then
        set_crow_state(e, 1, 1, 6, 0)
        e.move_speed_current = (4 - (e:random() & 1)) * 0x32 + e.crow_dist // 100
        e.roll = 0xFF00
        e.angle = e.angle + (e:random() & 1) * 0x800
        e.pos_y = e.pos_y - 100
    end

    if e.action_state == 0 then
        e.roll = 0x100
        set_crow_state(e, 1, 1, 7, 0)
        e:spawn_effect(0x1C, 0, 0, 0, 0, e.angle, 0)

        if e.crow_dist < 0x5DC then
            if e:turn_toward_target(px, pz, 0x100) == 0
                and (e.crow_path_word & 1) ~= 0 then
                -- The behaviour word is set before the hit test, so a crow
                -- that reaches here while the player is already attacked
                -- still parks on 9.
                set_behavior(e, 9)
                if e.player_attacked == 0 then
                    e.player_health = e.player_health
                        - (e.second_playthrough and 16 or 6)
                    e.player_attacked = 1
                    e:set_player_animation(6, 5)
                    e:set_player_action(0, 0)
                    local qx, qy, qz = e:player_pos()
                    e:play_3d_sound_at(3, 0, qx, qy, qz)
                    e:spawn_player_effect(0, 1, 0, -0x9D8, 0, e.player_angle)
                    e.hit_state = 1
                    e.roll = 0
                    set_behavior(e, 0xB)
                end
            end
        end
    end
end

-- Behaviour 10: level out. Enters when the crow tops out against the ceiling
-- clamp; climbs 25 a frame but sheds 50 while the nose is up, so after the
-- first frame or two the net is a 25/frame descent. Hands back to the glide
-- when the clip completes.
local function level_out(e)
    e.animation_id = 5
    anim_step(e)
    e.pos_y = e.pos_y - 25
    if signed_word(e.roll) > 0 then
        e.pos_y = e.pos_y + 50
    end
    e:move(0, signed_word(e.move_speed_current))
    e.roll = e.roll + 0x19

    if e.ignore == 0 then
        e.roll = 0x100
        set_crow_state(e, 1, 1, 5, 0)
    end
end

-- Behaviour 11: latched on. Ride the player while the grab animation runs the
-- struggle timer; the movement offset flutters the crow around the player.
local function grab(e, px, py, pz)
    e.animation_id = 8
    e:rotate_toward_target(px, pz, 0x80)
    e.move_speed_current = 0x32
    anim_grab(e, px, pz)
    e:move((e:random() & 1) * 0x800 + 0x400, signed_word(e.move_speed_current))
end

-- Behaviour 12: victory descend. Entered from the grab the moment the player
-- dies. Drops back down toward the body until it is under -3000, then resumes
-- diving.
local function victory_descend(e, px, py, pz)
    e.animation_id = 0
    e:rotate_toward_target(px, pz, 0x80)
    anim_hold(e)
    e.move_speed_current = 200
    e:move(0, 200)
    e.crow_vy = e.crow_vy + 1
    if e.crow_vy > 500 then
        e.crow_vy = 200
    end
    e.pos_y = e.pos_y + e.crow_vy
    if e.pos_y > -3000 then
        set_crow_state(e, 1, 1, 2, 0)
    end
end

-- Behaviour 13: take off. Waits a random 0, 3, 8 or 11 frames - staggering a
-- flock so they do not all leave on the same frame - raises the airborne bit,
-- then caws, throws feathers and enters the climb.
local function takeoff(e)
    local st = e.action_state
    if st == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.death_timer = 0
        e.blend_counter = 3
        e.action_state = 1
        local r1 = e:random()
        local r2 = e:random()
        e.action_ticks_counter = ((r2 & 1) * 3) + ((r1 & 1) * 8)
        e.ignore = 1
        e.behavior_flags = e.behavior_flags | 0x10
    elseif st ~= 1 then
        if st ~= 2 then
            return
        end
        set_crow_state(e, 1, 1, 1, 0)
        e:play_enemy_sound(4)
        feathers(e)
        return
    end

    local ticks = signed_word(e.action_ticks_counter)
    e.action_ticks_counter = ticks - 1
    if ticks == 0 then
        e.action_state = 2
    end
end

local BEHAVIORS = {
    perch, wingbeat, dive, bank_turn, strike_run, descend, ascend,
    dive_attack, approach, peck, level_out, grab, victory_descend, takeoff,
}

-- Refresh the obstacle pathfinder, keep its bit 0 in the +0x16E scratch byte,
-- cache the wide turn probe the dive, banking turn and glide branch on, then
-- jump into the current behaviour. The `0x100` probe the original parks in
-- scratch too is never read by any behaviour and is not computed.
local function dispatch_behavior(e, px, py, pz)
    local path = e:pathfind_update(px, pz)
    e:wasp_path_keep(path)

    local bkp = e:turn_toward_target(px, pz, 0x200)
    local behavior = BEHAVIORS[e.action_behavior + 1]
    if behavior then
        behavior(e, px, py, pz, bkp)
    end
end

-- State 0: spawn. behavior_flags bit 4 starts airborne in the high hover with
-- a random cruise speed and the nose up; bit 1 parks under SCD control; the
-- rest perch. The health roll is four draws - the first thrown away - for
-- 10..52; the scripted bit-0 variant gets no health and flies lower on a -400
-- bias.
local function init(e)
    set_crow_state(e, 1, 0, 0, 0)
    e.action_ticks_counter = 0
    e.animation_id = 2
    e.death_timer = 0
    e.hit_state = 0
    e.collision_flags = e.collision_flags | 4

    if (e.behavior_flags & 0x10) ~= 0 then
        e.crow_vy = 200
        e.move_speed_current = ((e:random() & 1) * 0x10) + 0x32
        e.roll = 0x100
        set_crow_state(e, 1, 1, 5, 0)
        e.animation_id = 0
    end
    if (e.behavior_flags & 2) ~= 0 then
        set_crow_state(e, 8, 0, 0, 0)
        e.animation_id = 0
    end

    e.crow_stuck = 0
    e.crow_phase = 100
    e.crow_swerve_latch = 0

    e:reset_joints()
    e:advance_anim(0x400)

    e:set_shadow_offset(0, 0, 0)
    e.shadow_tint = 0x00808080
    e.shadow_half_x = 200
    e.shadow_half_z = 200

    e:random()
    local ra = e:random() & 7
    local rb = e:random() & 7
    local rc = e:random() & 7
    e.health = rb * 2 + ra * 2 + rc * 2 + 10

    e.crow_alt_bias = 0
    if (e.behavior_flags & 1) ~= 0 then
        e.health = 0
        e.crow_alt_bias = -400
    end

    e:set_sca(200, 180, 0, 0, 0)
    e.joint_scale = 0x14CC
end

-- `crow_scd_fly_to_target`: cruise toward the SCD waypoint, accelerating from
-- 200 to 400 and resetting, climbing while below -1500, until within 150 units.
local function fly_to_target(e)
    local st = e.action_state
    if st == 0 then
        e.animation_id = 5
        e.move_speed_current = 200
        e.action_state = st + 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e:play_enemy_sound(4)
    elseif st ~= 1 then
        if st ~= 2 then
            return
        end
        set_behavior(e, 0)
        return
    end

    if e.animation_frame_id % 9 == 0 then
        if (e:random() & 1) ~= 0 and (e:random() & 1) ~= 0 then
            e:play_enemy_sound(5)
        end
    end

    e:rotate_toward_target(e.unk_c6, e.unk_c8, 0x40)
    e:advance_anim(0x200)

    e.move_speed_current = e.move_speed_current + 1
    if signed_word(e.move_speed_current) > 400 then
        e.move_speed_current = 200
    end
    e:move(0, signed_word(e.move_speed_current))
    if e.pos_y > -1500 then
        e.pos_y = e.pos_y - 500
    end

    if e:xz_distance_to(e.unk_c6, e.unk_c8) < 0x96 then
        e.action_state = e.action_state + 1
    end
end

-- States 4-8: the SCD flight slots. Behaviour 1 is the stand-down command,
-- which resets to state 0 and rewrites behavior_flags to 0x11 so the next
-- init puts the crow back in the air under normal AI.
local function scd(e)
    if (e.behavior_flags & 0x80) ~= 0 then
        return
    end
    local beh = e.action_behavior
    if beh == 0 or beh == 2 then
        fly_to_target(e)
        e.status_flags = e.status_flags & 0x1F
        return
    end
    if beh == 1 then
        set_crow_state(e, 0, 0, 0, 0)
        e.behavior_flags = 0x11
    end
    e.status_flags = e.status_flags & 0x1F
end

-- State 1: the driver. Contact resolution first, then obstacle steering: a
-- blocked crow ramps the stuck counter; past 30 frames it either rams the
-- player or asks the swerve helper for a way around. CR_DIST is the manhattan
-- distance, recomputed every frame. A perched crow watches the player's state
-- dword for the scatter cue (animation id 1, frame 3, behaviour 0x14, state
-- 0/3) and takes off with a random half-turn. The trailing block re-derives
-- the alert/visual bits from the altitude.
local function run(e)
    local bflags = e.behavior_flags
    if (bflags & 0x80) ~= 0 then
        return
    end

    local px, py, pz = e:player_pos()

    if (bflags & 0x10) ~= 0 then
        if e.crow_touch ~= 0 and e.player_attacked == 0
            and e.action_behavior ~= 9 then
            if (bflags & 1) ~= 0 then
                -- Unsigned compare: only pull down when more than 300 units
                -- above head height.
                if ((-2500 - e.pos_y) & 0xFFFFFFFF) > 300 then
                    e.pos_y = -2500
                end
                set_crow_state(e, 1, 1, 9, 0)
                return
            end
            if signed_word(e.move_speed_current) > 300
                and not e:player_facing_entity() then
                set_crow_state(e, 2, 0, 0, 0)
                e.player_attacked = 1
                e.player_action_behavior = 100
                return
            end
        end

        local reacting = false
        if e.crow_coll == 0 then
            reacting = e.player_attacked ~= 0
        else
            e.crow_stuck = e.crow_stuck + 1
            if e.crow_stuck > 5 and (e.behavior_flags & 1) ~= 0 then
                e.angle = e.angle + e:turn_toward_target(px, pz, 0x40)
            end
            if e.crow_stuck <= 0x1E then
                reacting = e.player_attacked ~= 0
            elseif e.player_attacked ~= 0 then
                reacting = true
            else
                e.crow_stuck = 0
                if signed_word(e.move_speed_current) > 300
                    and e.player_attacked == 0
                    and (e.behavior_flags & 1) == 0 then
                    set_crow_state(e, 2, 0, 0, 0)
                    return
                end
                -- Four frames of held turn.
                e.angle = e.angle + e:swerve(0x40, true, 4)
                reacting = e.player_attacked ~= 0
            end
        end

        if reacting then
            e.angle = e.angle + e:turn_toward_target(px, pz, 0x40)
            if e.player_action_state > 3 then
                -- A one-unit nudge when already perfectly lined up, so the
                -- crow never sits exactly on the boundary.
                if e:turn_toward_target(px, pz, 0x80) == 0 then
                    e.angle = e.angle + 1
                end
            end
            if e.player_health < 0 and e.player_action_behavior == 200
                and e.action_behavior == 9 then
                set_crow_state(e, 1, 1, 6, 0)
            end
        end
    end

    local dz = pz - e.pos_z
    local dx = px - e.pos_x
    if dz < 0 then
        dz = -dz
    end
    if dx < 0 then
        dx = -dx
    end
    e.crow_dist = ((dx & 0xFFFF) + (dz & 0xFFFF)) & 0xFFFF

    local dispatch = true
    if e.ignore == 0 then
        -- The player's whole state dword: animation id 1, frame 3, behaviour
        -- 0x14, state 0 or 3.
        local player_state = e.player_animation_id
            | (e.player_animation_frame_id << 8)
            | (e.player_action_behavior << 16)
            | (e.player_action_state << 24)
        if (player_state == 0x0140301 or player_state == 0x03140301)
            and (e.behavior_flags & 0x10) == 0 then
            e.angle = e.angle + (e:random() & 1) * 0x40
            set_crow_state(e, 1, 1, 13, 0)
        end
    elseif e.ignore ~= 1 then
        dispatch = false
    end
    if dispatch then
        dispatch_behavior(e, px, py, pz)
    end

    -- Altitude gates the crow's presence: aligned below -4500, visible at 2000
    -- below -2000 and at 5000 below -1500.
    e.status_flags = e.status_flags & 0x1F
    e:check_alert_range(5000)
    if e.pos_y > -4500 then
        e.status_flags = e.status_flags & 0x1F
        e.status_flags = e.status_flags | 0x40
        if e.pos_y > -2000 then
            e:check_visual_range(2000)
        end
    end
    if e.pos_y > -1500 then
        e.status_flags = e.status_flags & 0x1F
        e:check_visual_range(5000)
    end
end

-- State 2: recoil. The damage system parks a surviving crow here, and the
-- driver does the same after a ram. It beats hard upward, throws feathers on
-- the first animation frame, and once past -450 plays the recovery flap and
-- rejoins in the banking turn.
local function recoil(e)
    if e.action_behavior == 0 then
        e.animation_id = 6
        anim_hold(e)
        e.move_speed_current = 200
        e.pos_y = e.pos_y + 200
        if e.crow_touch ~= 0 then
            e:move(0x800, 200)
        end
        if e.animation_frame_id == 1 then
            e:play_enemy_sound(2)
            feathers(e)
        end
        if e.pos_y > -450 then
            e.action_behavior = 1
            e.action_state = 0
            e.pos_y = -450
        end
    elseif e.action_behavior == 1 then
        e.animation_id = 9
        anim_step(e)
        if e.action_state == 0 then
            set_crow_state(e, 1, 1, 3, 0)
            e.hit_state = 0
            e.status_flags = e.status_flags & 0x1F
            e:check_visual_range(5000)
        end
    end
end

-- State 3: death. The crow drops; the exit depends on the floor step the room
-- resolve reported. A real floor (step != 100) fades the shadow yellow,
-- shrinks it and plays the landing; an off-map death (the guard rewrites
-- anything below -2000 to 100) bursts every joint and removes the entity.
local function death(e)
    if e.floor_step < -2000 then
        e.floor_step = 100
    end

    local behavior = e.action_behavior
    if behavior == 0 then
        e:raise_death_event()
        e.animation_id = 6
        -- hit_state bit 0 means the killing blow knocked it sideways: it
        -- tumbles instead of flapping.
        if (e.hit_state & 1) == 0 then
            e.move_speed_current = 200
            local px, _, pz = e:player_pos()
            local turn = e:turn_toward_target(px, pz, 0x400)
            e:move(turn == 0 and 0x800 or 0, 200)
        end
        anim_hold(e)
        e.pos_y = e.pos_y + 200
        e.roll = e.roll + 0x80

        if e.animation_frame_id == 1 then
            e:play_enemy_sound(0)
            feathers(e)
        end
        if e.pos_y > -450 then
            if e.floor_step ~= 100 then
                e.action_behavior = 1
                e.action_state = 0
                e.pos_y = -450
                e.roll = 0
                e.shadow_tint = 0x00FFFF50
                e:adjust_shadow_size(-100, -100)
            else
                e.action_behavior = 2
                e.action_state = 0
            end
        end
    elseif behavior == 1 then
        e.animation_id = 0xA
        anim_step(e)
        e:adjust_shadow_size(6, 6)
        if e.action_state == 0 then
            e.action_behavior = 4
            e.action_state = 0
        end
    elseif behavior == 2 then
        e.shadow_half_x = 0
        e.shadow_half_z = 0
        for joint = JOINTS - 1, 0, -1 do
            e:set_joint_visible(joint, false)
        end
        e:play_enemy_sound(0)
        e.action_behavior = 4
        e.action_state = 0
        -- The original falls through into case 4.
        e.status_flags = e.status_flags | 2
        e.status_flags = e.status_flags | 8
    elseif behavior == 4 then
        e.status_flags = e.status_flags | 2
        e.status_flags = e.status_flags | 8
    end
end

local STATES = { init, run, recoil, death, scd, scd, scd, scd, scd }

function update(e)
    -- Monsters freeze behind a message: the state machine and the whole SCA
    -- pass are inside the gate, exactly like the original's `g_message_flags`.
    if not e.monster_paused then
        local state = STATES[e.state + 1]
        if state then
            state(e)
        end

        e.crow_touch = e:separate() and 1 or 0
        local code, step = e:resolve_collision()
        e.crow_coll = code
        e.floor_step = step
    end

    e:update_switch_zone()

    -- The flight envelope: the floor and ceiling clamps rewrite the state
    -- block directly rather than letting the behaviour notice.
    if e.pos_y > 0 then
        e.crow_vy = 200
        e.pos_y = -100
        set_crow_state(e, 1, 1, 7, 0)
    end
    if e.pos_y < -30000 then
        e.crow_vy = 200
        e.pos_y = -29000
        set_crow_state(e, 1, 1, 10, 0)
    end

    -- The ground shadow shrinks with altitude; Y is negative up, so the
    -- arithmetic shift is what makes it shrink.
    local size = (e.pos_y // 16) + 400
    if size < 0 then
        size = 50
    end
    e.shadow_half_x = size
    e.shadow_half_z = size

    -- The renderer queues the ground quad while the camera-zone bit is set;
    -- the original's extra gate is the floor-step and state-word test, so the
    -- quad is suppressed when it fails. A perched crow casts no shadow.
    if e.has_enter_switch_zone == 0 or e.floor_step <= -100
        or (e.state == 1 and e.ignore == 0) then
        e.shadow_half_x = 0
        e.shadow_half_z = 0
    end
end
