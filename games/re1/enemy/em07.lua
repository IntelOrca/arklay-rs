-- Wasp, entity id 0x07.
--
-- The mansion's flying swarm. A four-level machine:
--
--   state            init / run / knocked down / death
--   ignore           0 = the hover drift layer runs this frame, 1 = the
--                    behaviour owns the wasp outright, anything else freezes
--   behavior_flags   the NEST state machine, not a spawn kind:
--                      0x00-0x0F loose, wakes on proximity or a timer
--                      0x10      dormant in the nest, running down a dwell
--                      0x11      cleared to emerge
--                      0x12      the tenth respawn (bit 1 is the big flag)
--                      >= 0x20   roused, goes straight to hover
--                    bit 1 of the spawn byte is the BIG-WASP flag: init
--                    consumes it (subtracting 2) and re-reads it all over
--   action_behavior  the seven-entry behaviour table
--   action_state     the per-behaviour animation sub-state
--
-- The behaviours are nest, takeoff, hover, sting, victory (the player is
-- dead), the pin-and-sting grab and emerge. The hover layer drifts the speed
-- up, corrects the heading past frame 0x1C, re-rolls on a stall and commits to
-- an attack only when touching, close, aimed and inside the 600-unit altitude
-- band just under -2300. A normal wasp that catches the player from behind and
-- dead-on pins them (behaviour 5); a big wasp can never grab.
--
-- Both attacks drive the player's own state: the sting writes the attacked
-- flag and action_behavior 100, and a big wasp is allowed to finish the kill,
-- writing animation 3, behavior 200 and health -1 itself. The grab latches
-- both entities' animation offsets to the player's position and plays the
-- per-character clip ((player id & 1) + 4), poisons on a coin flip and then
-- kills the wasp with its own health -44.
--
-- Death forks on the weapon in hit_state's top five bits: a big wasp or a
-- weapon heavier than id 6 is torn apart and removed at once; anything else
-- leaves a corpse that crawls for 20 frames and then respawns at the fixed
-- nest coordinates with fresh health. Every tenth respawn comes back as kind
-- 0x12, which the next init consumes into the big variant.
--
-- All state lives on the Rust entity; this file keeps none, so the scripting
-- VM may be reset between any two updates. The two write-only turn probes the
-- dispatch computes are not modelled (the wasp never reads them), the
-- original's `field_00`/matrix scratch writes are unmodelled (nothing reads
-- them), and the per-joint gore chunks stay with the renderer's documented
-- joint-object deferral (the state contract around them is kept).

-- One word write over `action_behavior` + `action_state`.
local function set_behavior(e, behavior)
    e.action_behavior = behavior
    e.action_state = 0
end

-- The common "start cruising" block.
local function enter_hover(e)
    e.ignore = 0
    e.action_behavior = 2
    e.action_state = 0
    e.wasp_bob = 0x23
    e.wasp_climb = 1
end

-- The hover entry speed: an unsigned 16-bit distance/100 + 200, so a far-away
-- wasp starts its cruise faster.
local function hover_speed(e)
    return (e.wasp_distance // 100 + 200) & 0xFFFF
end

-- Refresh the manhattan distance word: each full 32-bit absolute difference
-- truncates to 16 bits, the two terms add as words and the sum truncates
-- again.
local function refresh_distance(e, px, pz)
    local dx = px - e.pos_x
    local dz = pz - e.pos_z
    if dx < 0 then
        dx = -dx
    end
    if dz < 0 then
        dz = -dz
    end
    e.wasp_distance = ((dx & 0xFFFF) + (dz & 0xFFFF)) & 0xFFFF
end

-- The 16-bit signed view of a wrapped sum.
local function signed_word(value)
    value = value & 0xFFFF
    if value >= 0x8000 then
        return value - 0x10000
    end
    return value
end

-- The four wing joints: 0 blanked, 3 for the one-frame-in-seven blur.
local function set_wings(e, visible)
    for joint = 1, 4 do
        e:set_joint_visible(joint, visible)
    end
end

-- The shared animation starter: arm the clip from frame 0, then loop it.
-- action_state never passes 1, so each behaviour moves itself on. The
-- completion flag is the only thing the sting waits for.
local function wasp_animate(e)
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
    e.wasp_anim_done = e:advance_anim(0x400) and 1 or 0
end

-- Behaviour 0: the nest. The high nibble of behavior_flags decides which half
-- runs: a dormant wasp counts its dwell down (or arms early when the player
-- comes within 4000 units), a cleared one emerges, an already-roused one goes
-- straight to hover; a loose wasp wakes on proximity (kind 0) or its own
-- countdown and always latches bit 5 so it never nests again.
local function nest(e)
    e.hit_state = 1
    e.animation_id = 0
    wasp_animate(e)

    local kind = e.behavior_flags
    if kind & 0xF0 ~= 0 then
        if kind > 0x1F then
            e.hit_state = 0
            enter_hover(e)
            e.wasp_speed = hover_speed(e)
            return
        end
        if kind & 1 ~= 0 then
            set_behavior(e, 6)
            e.wasp_speed = 0x1E
            e.wasp_dwell = 0
            e.status_flags = e.status_flags | 2
            return
        end
        local dwell = e.wasp_dwell
        e.wasp_dwell = dwell - 1
        if dwell == 0 or e.wasp_distance < 4000 then
            e.behavior_flags = 0x11
        end
        return
    end

    if kind == 0 then
        if e.wasp_distance >= 5000 then
            return
        end
    else
        local timer = e.wasp_timer
        e.wasp_timer = timer - 1
        if timer == 0 then
            return
        end
    end

    e.hit_state = 0
    set_behavior(e, 1)
    e.wasp_timer = 0x5A
    e.behavior_flags = e.behavior_flags | 0x20
    e.wasp_speed = 0x32
end

-- Behaviour 1: takeoff. A 24-frame spiral: 85 units of yaw a frame while the
-- forward speed argument decays from 0x800 by 85 a frame, so the wasp
-- corkscrews off the surface and levels into the cruise.
local function takeoff(e)
    e.animation_id = 1
    e.wasp_speed = e.wasp_speed + 1
    wasp_animate(e)
    e.angle = e.angle + 0x55
    e:move((0x800 - e.animation_frame_id * 0x55) & 0xFFFF, e.wasp_speed)

    if e.animation_frame_id == 0x18 then
        enter_hover(e)
    end
end

-- Behaviour 2: hover. The wings blink on every seventh animation frame (the
-- wingbeat cue fires with them, inside 1000 units), the bob step ramps
-- 0x23..0x96 and rides the altitude with the climb flag, and the two altitude
-- flips draw one random bit each. The attack gate needs a touch, under 1000
-- units, an aimed heading and an altitude inside the band just under -2300.
-- A normal wasp that catches the player from behind and dead-on pins them.
local function hover(e, px, py, pz)
    set_wings(e, false)
    e.animation_id = 2
    wasp_animate(e)

    if e.animation_frame_id % 7 == 0 then
        set_wings(e, true)
        if e.wasp_distance < 1000 then
            e:play_enemy_sound(0)
        end
    end

    e:move(0, e.wasp_speed)

    e.wasp_bob = e.wasp_bob + 1
    if e.wasp_bob > 0x96 then
        e.wasp_bob = 0x23
    end

    if (-7 - (e:random() & 1)) * 500 > e.pos_y then
        e.wasp_climb = 0
    end
    if (-3 - (e:random() & 1)) * 500 < e.pos_y then
        e.wasp_climb = 1
    end
    if e.wasp_climb ~= 0 then
        e.pos_y = e.pos_y - e.wasp_bob
    else
        e.pos_y = e.pos_y + e.wasp_bob
    end

    -- The band test is the unsigned `-2300 - y < 600`, i.e.
    -- -2900 < y <= -2300.
    if e.wasp_touch == 0 or e.wasp_distance >= 1000
        or e:turn_toward_target(px, pz, 0x100) ~= 0 then
        return
    end
    local band = -2300 - e.pos_y
    if band < 0 or band >= 600 then
        return
    end

    e.wasp_drift = 0
    set_wings(e, true)
    enter_hover(e)
    e.wasp_speed = hover_speed(e)

    -- No sting when the player is already in an attack pose or the
    -- pathfinder has no route; both still leave the wasp cruising.
    if e.player_attacked ~= 0 or e.wasp_path_bit == 0 then
        return
    end
    e.ignore = 1
    set_behavior(e, 3)

    if not e:player_facing_entity()
        and e:turn_toward_target(px, pz, 0xC0) == 0
        and e.wasp_big == 0 then
        set_behavior(e, 5)
        e.angle = e:angle_to(px, pz)
        e.player_attacked = 1
        e.hit_state = 1
    end
end

-- Behaviour 3: the contact sting. Tracks the player slowly and lands nothing
-- until the animation completes; the damage is 4 (10 on the second
-- playthrough), or double for a big wasp, which is also allowed to finish the
-- kill and drive the player's death pose itself.
local function sting(e, px, py, pz)
    e.animation_id = 3
    e.angle = e.angle + e:turn_toward_target(px, pz, 0x30)
    wasp_animate(e)

    if e.wasp_anim_done == 0 then
        return
    end

    if e.player_attacked == 0 and e.wasp_distance < 0x5DC
        and e:turn_toward_target(px, pz, 0x100) == 0 and e.wasp_path_bit ~= 0 then
        e.player_attacked = 1
        e.player_action_behavior = 100

        local dmg = (1 + e.wasp_big) * (e.second_playthrough and 10 or 4)
        local before = e.player_health
        e:hurt_player(dmg, 1)
        if signed_word(before - dmg) < 0 and e.wasp_big ~= 0 then
            e:set_player_animation(3, 0)
            e:set_player_action(200, 0)
            e.player_health = -1
            return
        end
    end

    e.wasp_speed = 100
    e.ignore = 1
    set_behavior(e, 1)
end

-- Behaviour 4: victory. The player is dead: the hover minus the attack, with
-- the steering scaled by the path bit (a wasp with no route holds its
-- heading). It closes in only while further than 2000 units away.
local function victory(e, px, py, pz)
    set_wings(e, false)
    e.animation_id = 2
    e.angle = e.angle + e:turn_toward_target(px, pz, 0x10) * e.wasp_path_bit * 2
    wasp_animate(e)

    if e.animation_frame_id % 7 == 0 then
        set_wings(e, true)
    end

    e.wasp_bob = e.wasp_bob + 1
    if e.wasp_bob > 0x96 then
        e.wasp_bob = 0x23
    end

    if (-7 - (e:random() & 1)) * 500 > e.pos_y then
        e.wasp_climb = 0
        e.wasp_bob = 0x23
    end
    if (-3 - (e:random() & 1)) * 500 < e.pos_y then
        e.wasp_climb = 1
        e.wasp_bob = 0x23
    end
    if e.wasp_climb ~= 0 then
        e.pos_y = e.pos_y - e.wasp_bob
    else
        e.pos_y = e.pos_y + e.wasp_bob
    end

    if e.wasp_distance >= 2000 then
        e:move(0, e.wasp_speed)
        return
    end
    e:move(0x400, e.wasp_speed)
end

-- Behaviour 5: the pin-and-sting. Sub-state 0 snaps to the player's facing,
-- drops to the floor and latches both animation offsets and the player's
-- grabbed pose; sub-state 1 plays the per-character paired animation, with
-- the sting cue on frame 4 and the hand-off to the payout on the character's
-- own hit frame; sub-state 2 poisons on a coin flip and kills the wasp with
-- its own sting.
local function grab(e)
    local st = e.action_state
    if st == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.action_state = 1
        e.blend_counter = 3
        e.angle = e.player_angle
        e.pos_y = 0
        e.status_flags = e.status_flags | 2
        e.animation_id = e.player_character + 4
        e:grab_player()
    elseif st ~= 1 then
        if st ~= 2 then
            return
        end
        e.health = -44
        if (e:random() & 1) ~= 0 then
            e:poison_player()
        end
        local dmg = e.second_playthrough and 15 or 8
        e:hurt_player(dmg, 1)
        e.state = 3
        e.ignore = 0
        set_behavior(e, 0)
        return
    end

    e:apply_anim_vertex()
    local adv = e:advance_anim(0x400) and 1 or 0
    e.action_state = e.action_state + adv
    e.death_timer = e.death_timer + 1

    if e.animation_frame_id == 4 then
        local px, py, pz = e:player_pos()
        e:play_3d_sound_at(3, 0, px, py, pz)
    end
    if e.player_character * 2 + 0x22 == e.animation_frame_id then
        e.action_state = 2
    end
end

-- Behaviour 6: emerge. The altitude gains 20 a frame and a second 20 once it
-- clears -2800, so the climb accelerates out of the nest; past 20 frames it
-- loses 60 and settles. At 26 frames it clears its own no-collide bit and
-- joins the cruise.
local function emerge(e)
    e.animation_id = 2
    e.wasp_speed = e.wasp_speed + 1
    wasp_animate(e)

    e.pos_y = e.pos_y + 0x14
    if e.pos_y > -0xAF0 then
        e.pos_y = e.pos_y + 0x14
        e:move(0, e.wasp_speed)
        e.wasp_dwell = e.wasp_dwell + 1
        if e.wasp_dwell > 0x14 then
            e.pos_y = e.pos_y - 0x3C
        end
    end

    if e.wasp_dwell > 0x19 then
        e.status_flags = e.status_flags & 0xFD
        e.hit_state = 0
        enter_hover(e)
    end
end

local BEHAVIORS = { nest, takeoff, hover, sting, victory, grab, emerge }

-- Refresh the obstacle pathfinder, keep its bit 0 in the +0x16E scratch byte
-- and jump into the behaviour. The two turn probes the original parks in
-- scratch are write-only for the wasp and are not computed.
local function dispatch(e, px, py, pz)
    local path = e:pathfind_update(px, pz)
    e:wasp_path_keep(path)

    local behavior = BEHAVIORS[e.action_behavior + 1]
    if behavior then
        behavior(e, px, py, pz)
    end
end

-- State 0: full setup. A wasp placed without altitude starts at 4000 units up
-- (Y is negative). Spawn-kind bit 1 is consumed into the big flag, and a kind
-- 0x10 nest rolls its randomised dwell. The health roll is four draws, the
-- first thrown away, plus twenty per size step: 20..62 normal, 40..82 big.
local function init(e)
    e.state = 1
    e.ignore = 1
    e.action_behavior = 0
    e.action_state = 0
    e.wasp_dwell = 0
    e.animation_frame_id = 0
    e.death_timer = 0
    e.hit_state = 0
    e.status_flags = 1
    e.wasp_touch = 0
    e.wasp_drift = 0
    e.wasp_timer = 0x5A
    e.wasp_sound_latch = 0

    e:reset_joints()
    e:set_sca(0, 0, 0, 0, 0)
    e.joint_scale = 0

    if e.pos_y == 0 then
        e.pos_y = -4000
    end

    e.wasp_big = 0
    if (e.behavior_flags & 2) ~= 0 then
        e.joint_scale = 0x2000
        e.behavior_flags = e.behavior_flags - 2
        e.wasp_big = 1
    end

    if e.behavior_flags == 0x10 then
        e.wasp_dwell = (e:random() & 9) * -30 + 800
    end

    e:set_shadow_offset(0, 0, 0)
    e.shadow_tint = 0x606060
    local half = (e.wasp_big * 4 + 4) * 25
    e.shadow_half_x = half
    e.shadow_half_z = half

    e:random()
    local ra = e:random() & 7
    local rb = e:random() & 7
    local rc = e:random() & 7
    e.health = rb * 2 + ra * 2 + rc * 2 + (e.wasp_big + 1) * 20
end

-- State 1: the per-frame driver. A dead player forces the victory circle
-- first (using the previous frame's distance for its entry speed), a bounce
-- swings the heading back, the drift layer owns the cruise while ignore is 0,
-- and the shadow tracks the root joint's world height with a per-size floor.
local function run(e)
    local px, py, pz = e:player_pos()

    if e.player_health < 0 and e.action_behavior ~= 4 then
        e.wasp_bob = 0x23
        e.wasp_climb = 1
        e.wasp_speed = hover_speed(e)
        e.ignore = 0
        set_behavior(e, 4)
    end

    if e.wasp_collision ~= 0 then
        e.angle = e.angle + e:turn_toward_target(px, pz, 0x80)
    end

    refresh_distance(e, px, pz)

    if e.ignore == 0 then
        e.wasp_drift = e.wasp_drift + 1
        e.wasp_speed = e.wasp_speed + 2

        -- Past frame 0x1C, an off-axis wasp pays 8 units of speed to swing
        -- at the player.
        if e.wasp_drift > 0x1C and e:turn_toward_target(px, pz, 0x100) ~= 0 then
            e.angle = e.angle + e:turn_toward_target(px, pz, 0x80)
            e.wasp_speed = e.wasp_speed - 8
        end

        -- Stalled: re-roll the cruise speed and steer away.
        if e.wasp_speed < 0x1E then
            e.wasp_speed = hover_speed(e)
            e.angle = e.angle - e:turn_toward_target(px, pz, 0x200)
        end

        if e.wasp_speed > 400 then
            e.wasp_speed = 400
        end
        dispatch(e, px, py, pz)
    elseif e.ignore == 1 then
        dispatch(e, px, py, pz)
    end

    e.status_flags = (e.status_flags & 0x1F) | 0x40

    -- Above 4000 units the wasp registers as alerted; below 1000 as seen.
    if e.pos_y < -4000 then
        e.status_flags = e.status_flags & 0x1F
        e:check_alert_range(4000)
    end
    if e.pos_y > -1000 then
        e.status_flags = e.status_flags & 0x1F
        e:check_visual_range(4000)
    end

    local size = (e.wasp_big * 4 + 0xC) * 25 + (e:joint_world_y(0) // 16)
    if size < 0 then
        size = (e.wasp_big * 4 + 4) * 25
    end
    e.shadow_half_x = size
    e.shadow_half_z = size
end

-- State 2: shot down but not killed. Sub-state 0 falls straight into 1: the
-- shadow goes, the wings bleed once (big wasps), and the wasp drops at 200 a
-- frame; the landing tints the shadow red and starts the growth timer, which
-- ends in the limp state. A limp wasp is crushed when the moving player walks
-- within 400 units, which drops it into the death state.
local function damaged(e)
    local st = e.action_state
    if st == 0 then
        e.wasp_speed = 0
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.wasp_dwell = 0x1E
        e.blend_counter = 3
        e.hit_state = 1
        e.status_flags = e.status_flags | 2
        e.status_flags = e.status_flags | 8
        e.shadow_half_x = 0
        e.shadow_half_z = 0
        e:spawn_joint_effect(0, 0x11, 0, 0)

        if e.wasp_big ~= 0 then
            -- The wing gore belongs to the joint-object pipeline the
            -- renderer milestone still carries as a documented deferral.
        end
    end

    if st == 0 or st == 1 then
        e.pos_y = e.pos_y + 200
        if e.pos_y > -100 then
            e.pos_y = 0
            e.action_state = 2
            e.wasp_timer = 0x1E
            e.shadow_tint = 0x00FF3030
        end
        return
    end

    if st == 2 then
        e:adjust_shadow_size(8, 8)
        local timer = e.wasp_timer
        e.wasp_timer = timer - 1
        if timer == 0 then
            e.action_state = 3
            e.status_flags = e.status_flags & 0x1F
            e.hit_state = 0
            if e.wasp_big ~= 0 then
                e.wasp_timer = 0x14
                return
            end
        end
        return
    end

    if st == 3 then
        e.status_flags = e.status_flags & 0x1F
        e:check_visual_range(4000)

        local px, _, pz = e:player_pos()
        refresh_distance(e, px, pz)

        if e.wasp_distance < 400 and e.player_move_speed ~= 0 then
            e.hit_state = 1
            e.health = -4
            if e.wasp_sound_latch == 0 then
                e:play_enemy_sound(2)
                e.wasp_sound_latch = 1
            end
            e.state = 3
            e.ignore = 0
            set_behavior(e, 0)
        end
    end
end

-- State 3: the death fork. The kill frame tears a big or heavy-weapon kill
-- apart and removes it at once; everything else leaves a corpse that crawls
-- for 20 frames and then respawns at the fixed nest coordinates, with every
-- tenth respawn coming back as nest kind 0x12. A big wasp crushed while
-- knocked down skips the corpse and just fades its shadow out.
local function death(e)
    local st = e.action_state

    if st == 1 then
        e:adjust_shadow_size(8, 8)
        local timer = e.wasp_timer
        e.wasp_timer = timer - 1
        if timer == 0 then
            e.action_state = 4
            e:raise_death_event()
        end
        return
    end

    if st == 2 then
        e.joint_scale = 1
        local timer = e.wasp_timer
        e.wasp_timer = timer - 1
        if timer ~= 0 then
            return
        end

        e.behavior_flags = 0x10
        if e.wasp_respawns > 10 then
            e.wasp_respawns = 0
        end
        if e.wasp_respawns == 10 then
            e.behavior_flags = 0x12
        end
        e.wasp_respawns = e.wasp_respawns + 1

        e.angle = 0x400
        -- The nest, hard-coded: X 18000/17900, Y -4000/-4100, Z 22900/22800.
        e.pos_x = (0xB4 - (e:random() & 1)) * 100
        e.pos_y = (-0x28 - (e:random() & 1)) * 100
        e.pos_z = (0xE5 - (e:random() & 1)) * 100
        e.state = 0
        e.ignore = 0
        e.action_behavior = 0
        e.action_state = 0
        return
    end

    if st ~= 0 then
        return
    end

    -- The kill frame.
    e.wasp_speed = 0
    e.action_state = 4
    e.animation_frame_id = 0
    e.timing_control = 0
    e.wasp_dwell = 0x1E
    e.blend_counter = 3
    e.status_flags = e.status_flags | 2
    e.status_flags = e.status_flags | 8

    if e.wasp_sound_latch == 0 then
        e:play_enemy_sound(1)
    end

    e:spawn_joint_effect(0, 0x11, 0, 0)
    e:spawn_joint_effect(0, 0x11, 9, 0)

    -- Big wasp, or killed by a heavy weapon (hit_state >> 3 > 6).
    if e.wasp_big ~= 0 or (e.hit_state & 0xF8) > 0x30 then
        -- The joint 0..9 gore is the renderer's joint-object deferral.
        if e.wasp_sound_latch == 0 then
            e:play_enemy_sound(2)
        end
    end

    -- Already knocked down (the big knockdown parked 0x14 in the timer):
    -- skip the corpse and just fade the shadow out.
    if e.wasp_timer == 0x14 then
        e.action_state = 1
        return
    end

    e.shadow_half_x = 0
    e.shadow_half_z = 0

    if e.wasp_big == 0 and (e.hit_state & 0xF8) <= 0x30 then
        e:spawn_joint_effect(0, 0x11, 7, 0)
        e:spawn_joint_effect(0, 0x11, 8, 0)
        e.action_state = 2
        e.wasp_timer = 0x14
        return
    end

    e.action_state = 4
    e:raise_death_event()
end

local STATES = { init, run, damaged, death }

function update(e)
    -- Monsters freeze behind a message: the state machine and the whole
    -- collision pass are inside the gate.
    if not e.monster_paused then
        local state = STATES[e.state + 1]
        if state then
            state(e)
        end

        -- A wasp in the nest (behaviour 0) or still emerging (6), or one
        -- parked on the floor, cannot be touched or collided with.
        if e.pos_y ~= 0 and e.action_behavior ~= 0 and e.action_behavior ~= 6 then
            e.wasp_touch = e:separate() and 1 or 0
            e.wasp_collision = e:resolve_collision()
        end
    end

    e:update_switch_zone()
end
