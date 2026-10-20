-- Neptune (shark), entity id 0x0b.
--
-- `neptune_update` (0x0043d8d0) is the per-frame entry. Its gate is an OR, so
-- it reads backwards at a glance: the body runs when the state is 0 **or**
-- behaviour bit 7 is clear, i.e. only a shark past its init tick that also
-- carries 0x80 is skipped entirely. Inside the message gate the state table
-- runs, then the shared collision tail: the entity's SCA hit data is built,
-- the player separation pair runs, `HandleEnemyPlayerCollisions` pushes the
-- shark out of every other active enemy, and the 800-unit nose probe resolves
-- the room at radius 600, ORing the result code into `splatter_flag` and
-- latching `g_tempVar` (the accepted distance) in the room-hit word. The
-- camera switch-zone bit and the fade-sprite queue follow outside the gate.
--
-- The spawn record's behaviour byte is a real bitfield, not a state machine:
--
--   0x01  the aquarium boss variant - starts already circling, never gets the
--         "aligned" status bit, and takes the five-stage beaching death
--   0x02  the small variant - half-size shadow, half-size SCA boxes and the
--         0x800 joint scale, and it never attacks
--   0x80  once state != 0, skip the whole update
--
-- Room 40E1's script spawns three sharks (0x80 and 0x82) that run the init
-- tick, park on the gate, and then have bit 7 cleared by the room's main
-- stream, at which point `neptune_state_run` (0x0043dcb0) takes over: the zone
-- path to the player, the Manhattan distance, the aligned/visual status bits,
-- the wander-turn drift layer with the attack decision, and the six-behaviour
-- jump table.
--
-- `neptune_state_death` (0x0043f970) is entered by a survived weapon hit
-- (state 2 is the damaged state, state 3 the dead one) and splits on behaviour
-- bit 0: the ordinary shark turns, thrashes and sinks back into a respawn
-- (`NE_STATE_BLOCK = 1`), the boss beaches itself in five stages.
--
-- `neptune_state_init` (0x0043da40): state 1, the four-draw health roll
-- (0x28 + three `rand() & 7`, the first draw discarded), the two-box SCA
-- record for the size variant, and the 1000-unit nudge along the shark's own
-- facing. `neptune_sca_info_big` (0x004bc958) and `neptune_sca_info_small`
-- (0x004bc970) are TWO-box arrays laid out as
-- {flags, x, y, z, half_height, radius}.
--
-- All state lives on the Rust entity; the VM may be reset between any two
-- updates. The frame's player distance (`g_playerDisplacement`) is a local
-- threaded through the state-1 call chain, exactly like the original's global
-- that `Joint_move` also clobbers as scratch.

-- The big variant's boxes: front (500, -800, 0) and rear (-500, -800, 0),
-- both radius 600 / half-height 800.
local BIG = {
    radius = 600,
    half_height = 800,
    front_x = 500,
    rear_x = -500,
    rear_radius = 600,
    shadow_half_x = 2000,
    shadow_half_z = 300,
}

-- The small variant's boxes: front (250, -400, 0) radius 300 and rear
-- (-250, -400, 0) radius 600. The trailing 600 is byte-checked against
-- 0x004bc97c and ported verbatim; it is not the 300 every other halved field
-- would predict.
local SMALL = {
    radius = 300,
    half_height = 400,
    front_x = 250,
    rear_x = -250,
    rear_radius = 600,
    shadow_half_x = 1000,
    shadow_half_z = 150,
}

-- The 16-bit signed view of a counter the original reads through a `short`.
local function s16(value)
    value = value & 0xFFFF
    if value >= 0x8000 then
        return value - 0x10000
    end
    return value
end

-- Port-only: whether the player is running (the swim speed-up trigger). A
-- player hidden from the shark is never seen to run.
local function player_running(e)
    return not e:player_hidden() and e.player_action_behavior == 0xD
end

-- Port-only: steer at the player, or hold the heading when they are hidden
-- (`px` is nil).
local function rotate_toward_player(e, px, pz, step)
    if px then
        e:rotate_toward_target(px, pz, step)
    end
end

-- `neptune_seed_from_dead_move`: stage the g_deadMoveValue + 0x14 block (the
-- identity matrix's zero translation) into the shared sprite scratch. Every
-- Neptune effect writes it first and then overwrites the axes it uses.
-- The port's effect helpers take the offset directly, so the seed contributes
-- nothing; the function exists to document the call sites.

-- `neptune_spawn_bubbles`: the bubble trail at the tail and both flanks.
local function bubbles(e)
    e:spawn_joint_effect_at(0x17, 13, 12, 0, -100, 0, 0)
    e:spawn_joint_effect_at(0x17, 8, 6, 0, 0, 0, 0)
    e:spawn_joint_effect_at(0x17, 8, 0, 0, 0, 0, 0)
end

-- `neptune_spawn_blood_cloud`: two randomised puffs anchored to the shark's
-- matrix. FOUR platform draws, in the original's order (x1, z1, x2, z2).
local function blood_cloud(e)
    local x1 = (e:random() & 0x3FF) - 300
    local z1 = (e:random() & 0x3FF) - 750
    e:spawn_effect(0x17, 10, x1, -1500, z1, 0, 0)
    local x2 = (e:random() & 0x3FF) - 1450
    local z2 = (e:random() & 0x3FF) - 750
    e:spawn_effect(0x17, 9, x2, -1500, z2, 0, 0)
end

-- ---------------------------------------------------------------------------
-- State 0 - one-shot spawn init
-- ---------------------------------------------------------------------------

-- State 0: one-shot spawn init.
local function state_init(e)
    -- One dword write: state 1, ignore 0, behaviour 0, action 0.
    e.state = 1
    e.ignore = 0
    e.action_behavior = 0
    e.action_state = 0
    e.action_ticks_counter = 15
    e.hit_state = 0
    e:reset_joints()

    -- The shadow quad: zero local offset, the 0x404040 tint, and the
    -- variant's half extents. The small variant also halves the joint scale
    -- (the +0xCA word `EntityComputeJointWorldMatrices` reads).
    e:set_shadow_offset(0, 0, 0)
    e.shadow_tint = 0x404040

    local box
    if (e.behavior_flags & 2) ~= 0 then
        box = SMALL
        e.joint_scale = 0x800
    else
        box = BIG
    end
    e.shadow_half_x = box.shadow_half_x
    e.shadow_half_z = box.shadow_half_z
    e:set_sca(box.radius, box.half_height, box.front_x, -box.half_height, 0)
    e:set_sca2(box.rear_radius, box.half_height, box.rear_x, -box.half_height, 0)

    -- FOUR draws; the first is discarded. Health is 40..61.
    e:random()
    local r1 = e:random()
    local r2 = e:random()
    local r3 = e:random()
    e.health = 0x28 + (r1 & 7) + (r2 & 7) + (r3 & 7)

    e.animation_id = 0
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0x3F

    if (e.behavior_flags & 1) ~= 0 then
        -- The aquarium boss starts already circling: animation 2 and the
        -- dword state write 0x00010101 (state 1, ignore 1, behaviour 1,
        -- action 0).
        e.animation_id = 2
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0
        e.ignore = 1
        e.action_behavior = 1
        e.action_state = 0
    end

    e:advance_anim(0x40)

    e.move_timer = 7
    e.move_max_steps = 5
    e.splatter_flag = 0
    e.angle_turn_delta = 0

    -- Push 1000 units along the shark's facing, added to the accepted
    -- `position` words so it does not start clipped into the wall behind it.
    e:nudge_spawn(1000)

    -- A whole-byte EQUALITY test, not a bit test (`CMP byte [+2],2`).
    if e.behavior_flags == 2 then
        e.status_flags = e.status_flags | 2
    end
end

-- ---------------------------------------------------------------------------
-- State 1 - the swim/wander AI
-- ---------------------------------------------------------------------------

-- `neptune_swim_actions`: 0 begin, 1 far, 2 near.
local SWIM_ACTIONS = {}

-- 0x0043dfb0 - pick the swim animation and the cruise speed. The poison flag
-- makes the shark noticeably slower, which is what gives the poisoned player
-- a chance to reach the drain valve.
local function swim_begin(e)
    e.animation_id = 0
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0x3F
    e.action_state = e.action_state + 1
    e.move_speed_current = e.second_playthrough and 100 or 0x78
    e.is_moving = 3
end

-- 0x0043e030 - far away. Speed up and move to the near sub-state once the
-- player is beyond 10000 units AND running (player action_behavior == 13).
local function swim_far(e, disp)
    if disp > 10000 and player_running(e) then
        e.action_state = e.action_state + 1
        e.move_speed_current = 0x82
    end
end

-- 0x0043e060 - the mirror of the above; drop back as soon as the player is
-- inside 6000 or stops running.
local function swim_near(e, disp)
    if disp < 6000 or not player_running(e) then
        e.action_state = e.action_state - 1
        e.move_speed_current = 100
    end
end

SWIM_ACTIONS[0] = swim_begin
SWIM_ACTIONS[1] = swim_far
SWIM_ACTIONS[2] = swim_near

-- `neptune_behavior_swim` (0x0043de40) - behaviour 0, the cruise. Dispatches
-- the distance sub-state, advances the animation, fires the swim cue every
-- third completed animation, and trails bubbles.
local function behavior_swim(e, px, pz, disp)
    local action = SWIM_ACTIONS[e.action_state]
    if action then
        action(e, disp)
    end

    -- Joint_move's return is ADDED to the cue counter, not assigned.
    e.is_moving = e.is_moving + (e:advance_anim(0x40) and 1 or 0)
    if e.is_moving == 3 then
        e:play_enemy_sound(0)
        e.is_moving = 0
    end
    e:move(0, e.move_speed_current)

    -- The small variant leaves no wake.
    if (e.behavior_flags & 2) ~= 0 then
        return
    end

    e.move_timer = e.move_timer - 1
    if e.move_timer ~= 0 then
        return
    end

    e:spawn_joint_effect_at(0x17, 8, 6, 500, 0, 0, 0)
    if (e:random() & 1) == 0 then
        e:spawn_joint_effect_at(0x17, 8, 1, 400, 0, 0, 0)
    end
    e:spawn_joint_effect_at(0x17, 8, 12, -100, -400, 0, 0)
    if (e:random() & 3) == 0 then
        e:spawn_joint_effect_at(0x17, 8, 3, -100, 500, 0, 0)
    end
    e.move_timer = 4
end

-- `neptune_behavior_lunge` (0x0043e090) - behaviour 1, the fast pass. Coin
-- flips between two swim animations (the frame-seed LFSR), then swims with a
-- random per-frame yaw offset while steering at the player through a -0x400
-- bias, which makes the shark bank around the player instead of homing in.
local function behavior_lunge(e, px, pz)
    if e.action_state == 0 then
        e.action_state = 1
        e.move_speed_current = 0x32
        e.animation_id = (e:lfsr_step() & 1) + 2
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e:play_enemy_sound(4)
    elseif e.action_state ~= 1 then
        return
    end

    if e.animation_id == 2 and e.animation_frame_id == 0x19 then
        e:play_enemy_sound(5)
    end

    if e:advance_anim(0x40) then
        e.action_state = e.action_state - 1
    end

    e:move(e:random() & 0xFFF, e.move_speed_current)
    -- Port-only: with the player hidden (no `px`) there is no one to bank
    -- around.
    if px then
        e.angle = e.angle - 0x400
        e:rotate_toward_target(px, pz, 4)
        e.angle = e.angle + 0x400
    end
end

-- `neptune_retreat_actions`: TWO entries, not five (the header note).
local RETREAT_ACTIONS = {}

-- 0x0043e1c0 - start the turn-away animation. No Add_speedXZ here; only the
-- run state moves.
local function retreat_begin(e)
    e.action_state = e.action_state + 1
    e.move_speed_current = 0x96
    e.animation_id = 6
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0x3F
end

-- 0x0043e210 - run it, and at frame 23 reset the whole state block back to
-- plain state 1. That dword write is why action_state can never reach 2 here.
local function retreat_run(e)
    e:advance_anim(0x40)
    if e.animation_frame_id == 0x17 then
        e.state = 1
        e.ignore = 0
        e.action_behavior = 0
        e.action_state = 0
    end
    e:move(0, e.move_speed_current)
end

RETREAT_ACTIONS[0] = retreat_begin
RETREAT_ACTIONS[1] = retreat_run

-- `neptune_behavior_retreat` (0x0043e1a0) - behaviour 2.
local function behavior_retreat(e)
    local action = RETREAT_ACTIONS[e.action_state]
    if action then
        action(e)
    end
end

-- `neptune_circle_actions`: 0 begin, 1 left, 2 right.
local CIRCLE_ACTIONS = {}

-- 0x0043e2a0 - seed the first (longer) leg: 90..121 frames.
local function circle_begin(e)
    e.animation_id = 0
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0x3F
    e.action_state = e.action_state + 1
    e.move_speed_current = 100
    e.action_ticks_counter = (e:random() & 0x1F) + 0x5A
    e.bob_speed = 0x20
end

-- 0x0043e310 - circle one way; hand over to the other side after 30..61
-- frames.
local function circle_left(e)
    e:neptune_wander_turn(0x20, 0x3C)
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 then
        e.action_state = e.action_state + 1
        e.action_ticks_counter = (e:random() & 0x1F) + 0x1E
    end
end

-- 0x0043e380 - and back, with the step negated (0xFFE0 is -0x20).
local function circle_right(e)
    e:neptune_wander_turn(0xFFE0, 0x3C)
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 then
        e.action_state = e.action_state - 1
        e.action_ticks_counter = (e:random() & 0x1F) + 0x5A
    end
end

CIRCLE_ACTIONS[0] = circle_begin
CIRCLE_ACTIONS[1] = circle_left
CIRCLE_ACTIONS[2] = circle_right

-- `neptune_behavior_circle` (0x0043e260) - behaviour 3.
local function behavior_circle(e)
    local action = CIRCLE_ACTIONS[e.action_state]
    if action then
        action(e)
    end
    e:advance_anim(0x40)
    e:move(0, e.move_speed_current)
end

-- `neptune_devour_actions`: 0 seize, 1 carry, 2 bite, 3 swallow, 4 finish,
-- 5 spit, 6 recover.
local DEVOUR_ACTIONS = {}

-- 0x0043e4c0 - seize. Snaps the player into animation 7 frame 11 and takes
-- over their facing. `status_flags |= 2` is set twice in the original, once
-- before and once after the sound - kept for fidelity.
local function devour_seize(e, px, pz)
    rotate_toward_player(e, px, pz, 0x100)
    e.animation_id = 6
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0x3F
    e.action_state = e.action_state + 1
    e.status_flags = e.status_flags | 2
    e.move_speed_current = 0x96
    e:play_enemy_sound(2)
    e.status_flags = e.status_flags | 2

    e.player_state = 7
    e.player_anim_frame_id = 0xB
    e.player_action_behavior = 0
    e.player_action_state = 0
    e.player_flags = e.player_flags | 6
    e.player_angle = e.angle
end

-- 0x0043e560 - carry. At animation frame 15 the mouth reach is tested with a
-- generous 1000-unit box: a hit snaps the player onto the shark and advances,
-- a MISS jumps straight to action_state 5 (the spit) and hands the player
-- back.
local function devour_carry(e)
    e:advance_anim(0x40)

    if e.animation_frame_id == 0xF then
        if not e:reach_test(1, 200, 0, 0, 1000) then
            -- The grab missed. Hand the player back and skip to the spit.
            e.player_animation_frame_id = 0
            e.player_unk_bf = 0
            e.player_move_speed = 0
            e.player_attack_anim = 0
            e.player_unk_8c = 0
            -- A DWORD write of 0x01000001: anim 1, frame 0, behaviour 0,
            -- action_state 1.
            e.player_state = 1
            e.player_anim_frame_id = 0
            e.player_action_behavior = 0
            e.player_action_state = 1
            e.player_attack_direction = 100
            e.player_angle = e.player_angle - 0x800
            e.action_state = 5
            e.player_attacked = 0
            e.player_flags = e.player_flags & 0xF9
            return
        end
        -- Hit: the player is now carried at the shark's own position.
        e:set_player_pos(e.pos_x, e.player_pos_y, e.pos_z)
        e.action_state = e.action_state + 1
    end

    if e.animation_frame_id < 3 then
        blood_cloud(e)
    end

    if e.animation_frame_id > 3 and e.animation_frame_id < 5 then
        e:spawn_joint_effect_at(0x17, 13, 12, 0, 500, 0, 0)
        e:spawn_joint_effect_at(0x17, 13, 10, 0, 500, 0, 0)
    end

    e:move(0, e.move_speed_current)
end

-- 0x0043e7b0 - the bite lands. Builds the drag matrix from the mouth joint
-- (joint 1's matrix at joints + 0xC0) and hides NINE of the player's joints
-- in one go: the limbs disappear as they go in.
local function devour_bite(e)
    e:advance_anim(0x40)
    if e.animation_frame_id ~= 0x13 then
        return
    end

    e.action_state = e.action_state + 1
    e:neptune_capture_setup(1)
    e.player_zone_flags = e.player_zone_flags | 0x40
    e.player_action_state = e.player_action_state + 1

    -- Written in exactly this order in the original.
    for _, joint in ipairs({ 1, 0, 9, 12, 10, 13, 11, 14, 2 }) do
        e:set_entity_joint_flag(0, joint, e:entity_joint_flag(0, joint) & 0xFE)
    end
end

-- 0x0043e860 - the swallow proper. Sets the player's health to -1 as a WORD,
-- drags the capture matrix in and down every frame, hides two more joint
-- pairs at frames 41 and 59, and sprays blood on a set of frame-modulo
-- schedules.
local function devour_swallow(e)
    e.action_state = e.action_state + (e:advance_anim(0x40) and 1 or 0)

    e.player_health = -1

    -- DWORD arithmetic on the drag matrix's t[0] and t[1] (0x0043e89c).
    e:neptune_capture_drag()

    if e.animation_frame_id == 0x29 then
        for _, joint in ipairs({ 3, 6 }) do
            e:set_entity_joint_flag(0, joint, e:entity_joint_flag(0, joint) & 0xFE)
        end
    end
    if e.animation_frame_id == 0x3B then
        for _, joint in ipairs({ 4, 7, 5, 8 }) do
            e:set_entity_joint_flag(0, joint, e:entity_joint_flag(0, joint) & 0xFE)
        end
    end

    local frame = e.animation_frame_id

    if frame < 0x38 then
        if frame > 2 and frame < 5 then
            blood_cloud(e)
        end
        if (frame & 7) == 0 and frame > 7 then
            local x = (e:random() & 0x1FF) - 0x100
            local z = (e:random() & 0x1FF) - 0x100
            e:spawn_joint_effect_at(0x17, 10, 5, x, 200, z, 0)
            e:spawn_joint_effect_at(0x17, 9, 1, x, 200, z, 0)
        end
        if (frame & 7) == 0 and frame > 0xA then
            local x = (e:random() & 0x1FF) - 0x100
            local z = (e:random() & 0x1FF) - 0x100
            e:spawn_joint_effect_at(0x17, 10, 12, x, 200, z, 0)
        end
    elseif (frame & 3) == 0 then
        bubbles(e)
    end

    -- An unsigned 8-bit DIV; the remainder is the test (0x0043ebbf).
    if (frame % 5) == 0 and frame > 1 then
        e:spawn_joint_effect_at(0, 0, 1, 0, 0, 0, 0)
    end
end

-- 0x0043ec20 - done eating. Drop the grab bit and go back to plain state 1.
local function devour_finish(e)
    e.status_flags = e.status_flags & 0xFD
    e.state = 1
    e.ignore = 0
    e.action_behavior = 0
    e.action_state = 0
end

-- 0x0043ec40 - the MISSED grab. The shark thrashes, spits, and at frame 23
-- releases the animation lock and moves to the recover state.
local function devour_spit(e)
    e:advance_anim(0x40)

    if e.animation_frame_id == 0x17 then
        e.action_state = e.action_state + 1
        e.action_ticks_counter = 0x1E
        e.status_flags = e.status_flags & 0xFD
        e.animation_id = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x1F
    end

    if e.animation_frame_id > 2 and e.animation_frame_id < 5 then
        blood_cloud(e)
    end

    if e.animation_frame_id > 4 and (e.animation_frame_id & 3) == 0 then
        e:spawn_joint_effect_at(0x17, 8, 0, 0, 200, 0, 0)
        e:spawn_joint_effect_at(0x17, 13, 12, 800 - (e:random() & 0x3FF), 200, 0, 0)
    end

    if (e.animation_frame_id & 7) == 0 then
        bubbles(e)
        e:spawn_player_effect(0x17, 13, 0, -1500, 0, 0)
    end
end

-- 0x0043eee0 - settle after a miss, then resume swimming. The DWORD write
-- here is 0x01000001 - action_state ONE, so the shark re-enters
-- neptune_swim_far rather than re-initialising the animation.
local function devour_recover(e)
    e:advance_anim(0x80)
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 then
        e.state = 1
        e.ignore = 0
        e.action_behavior = 0
        e.action_state = 1
        e.move_speed_current = 0x78
    end
    e:move(0, e.move_speed_current)

    e.move_timer = e.move_timer - 1
    if e.move_timer == 0 then
        bubbles(e)
        e.move_timer = 0xB
    end
end

DEVOUR_ACTIONS[0] = devour_seize
DEVOUR_ACTIONS[1] = devour_carry
DEVOUR_ACTIONS[2] = devour_bite
DEVOUR_ACTIONS[3] = devour_swallow
DEVOUR_ACTIONS[4] = devour_finish
DEVOUR_ACTIONS[5] = devour_spit
DEVOUR_ACTIONS[6] = devour_recover

-- `neptune_behavior_devour` (0x0043e3f0) - behaviour 4. After the action body
-- runs, the shark is pushed out of room geometry and the SAME correction is
-- applied to the pair's animation offsets and `position` words, so the player
-- stays in the shark's mouth while the pair is shoved clear of a wall.
local function behavior_devour(e, px, pz)
    local action = DEVOUR_ACTIONS[e.action_state]
    if action then
        action(e, px, pz)
    end

    local _, dx, dz = e:resolve_collision_radius(500)
    e.unk_c6 = e.unk_c6 + dx
    e.unk_c8 = e.unk_c8 + dz
    e.player_unk_c6 = e.player_unk_c6 + dx
    e.player_unk_c8 = e.player_unk_c8 + dz
    e.saved_x = e.saved_x + dx
    e.saved_z = e.saved_z + dz
    -- The player's own `position` mirror (+0x6C) is not modelled; the port's
    -- player collision reads the live matrix.
end

-- `neptune_bite_actions`: 0 begin, 1 close, 2 seize, 3 shake, 4 release,
-- 5 recover.
local BITE_ACTIONS = {}

-- 0x0043f020 - start closing.
local function bite_begin(e)
    e.action_state = e.action_state + 1
    e.move_speed_current = 100
    e.animation_id = 7
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0x1F
    e.status_flags = e.status_flags | 2
end

-- 0x0043f080 - close in. Tests the mouth reach EVERY frame with a tight
-- 300-unit box (the devour's test is 1000 and fires once), and if the
-- animation runs out first the attack is abandoned to action_state 5.
local function bite_close(e, px, pz)
    if e:advance_anim(0x80) then
        e.action_state = 5
        e.status_flags = e.status_flags & 0xFD
        e.action_ticks_counter = 0x1E
        e.animation_id = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x1F
        return
    end

    local frame = e.animation_frame_id
    if (frame & 1) == 0 then
        if frame < 0x1E then
            e:spawn_joint_effect_at(0x17, 8, 1, 500, 200, 0, 0)
        else
            e:spawn_joint_effect_at(0x17, 13, 12, 0, -100, 0, 0)
            e:spawn_joint_effect_at(0x17, 8, 6, 0, 0, 0, 0)
            e:spawn_joint_effect_at(0x17, 8, 0, 0, 0, 0, 0)
        end
    end

    rotate_toward_player(e, px, pz, 0x20)

    if e:reach_test(1, 200, 0, 0, 300) then
        e.action_state = e.action_state + 1
        e.animation_id = 8
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x1F

        -- Snap the player onto the shark - both the animation base offsets
        -- and the facing - and start the held-in-the-jaws animation.
        e.unk_c6 = e.pos_x
        e.unk_c8 = e.pos_z
        e.player_unk_c6 = e.pos_x
        e.player_unk_c8 = e.pos_z
        e.player_angle = e.angle
        e.player_flags = e.player_flags | 2
        e.player_state = 6
        e.player_anim_frame_id = 0xB
        e.player_action_behavior = 0
        e.player_action_state = 0
    end
    e:move(0, e.move_speed_current)
end

-- 0x0043f2f0 - clamped on. Seeds the 90-frame struggle timer.
local function bite_seize(e)
    if e:advance_anim(0x80) then
        e.action_state = e.action_state + 1
        e:play_enemy_sound(1)
        e.animation_id = 9
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x1F
        e.player_animation_frame_id = 0
        e.player_attack_anim = 1
        e.player_unk_8c = 3
        e.player_unk_bf = 0
        e.writhe_velocity = 0x5A
    end

    if (e.animation_frame_id & 3) == 0 then
        e:spawn_player_effect(0x17, 13, 0, -1500, 0, 0)
    end
end

-- 0x0043f3d0 - the chew. Mashing the controller costs the timer 8 per frame
-- instead of 1. Each completed animation cycle takes 7 health (30 on the
-- second playthrough) and the result is FLOORED AT 1, so this attack can
-- never kill.
local function bite_shake(e)
    e.writhe_velocity = e.writhe_velocity - (e:player_mashing() and 8 or 1)

    if e:advance_anim(0x80) then
        if not e.second_playthrough then
            e.player_health = e.player_health - 7
        else
            e.player_health = e.player_health - 0x1E
        end

        if (e.animation_frame_id & 3) == 0 then
            e:spawn_joint_effect_at(0x17, 10, 12, 0, -100, 0, 0)
            e:spawn_joint_effect_at(0x17, 8, 6, 0, 0, 0, 0)
            -- The original anchors the third spray on the player's joint 11;
            -- the port hangs it off the player body matrix instead.
            e:spawn_player_effect(0x17, 8, 0, 0, 0, 0)
        end

        if e.player_health < 0 then
            e.player_health = 1
        end
    end

    if (e.animation_frame_id & 0xF) == 0 then
        e:spawn_joint_effect_at(0x17, 13, 12, 0, -100, 0, 0)
        -- The player joint-1 spray, deferred to the body matrix.
        e:spawn_player_effect(0x17, 13, 200, 0, 0, 0)
        e:spawn_player_effect(0x17, 13, 0, -1500, 0, 0)
    end

    e.move_timer = e.move_timer - 1
    if e.move_timer == 0 then
        e:spawn_effect(0x17, 13, 600, -200, -1300, 0, 0)
        e:spawn_effect(0, 0, 600, -200, -1300, 0, 0)
        e.move_timer = 0xB
    end

    -- NEGATIVE, not zero - the timer is signed and the mashing path can step
    -- it past 0 in one frame.
    if e.writhe_velocity < 0 then
        e.action_state = e.action_state + 1
        e.move_speed_current = 100
        e.animation_id = 10
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x1F
        e.player_animation_frame_id = 0
        e.player_unk_bf = 0
        e.player_attack_anim = 2
        e.player_unk_8c = 0
        e:play_enemy_sound(1)
    end
end

-- 0x0043f6c0 - let go and give the player back their own animation. 0xFFF0 is
-- -0x10 as the unsigned short the rotate helper takes.
local function bite_release(e, px, pz)
    rotate_toward_player(e, px, pz, 0xFFF0)

    if e:advance_anim(0x80) then
        e.action_state = e.action_state + 1
        e.action_ticks_counter = 0x1E
        e.status_flags = e.status_flags & 0xFD
        e.animation_id = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x1F

        e.player_state = 1
        e.player_anim_frame_id = 0
        e.player_action_behavior = 0
        e.player_action_state = 0
        e.player_flags = e.player_flags & 0xFD
        e.player_attacked = 0
    end

    if (e.animation_frame_id & 7) == 0 then
        bubbles(e)
        e:spawn_player_effect(0x17, 13, 0, -1500, 0, 0)
    end
    e:move(0, e.move_speed_current)
end

-- 0x0043f840 - settle, then resume swimming at action_state 1 (the
-- 0x01000001 dword write again). 0xFFE0 is -0x20.
local function bite_recover(e, px, pz)
    rotate_toward_player(e, px, pz, 0xFFE0)
    e:advance_anim(0x80)

    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 then
        e.state = 1
        e.ignore = 0
        e.action_behavior = 0
        e.action_state = 1
        e.move_speed_current = 0x78
    end
    e:move(0, e.move_speed_current)

    e.move_timer = e.move_timer - 1
    if e.move_timer == 0 then
        bubbles(e)
        e.move_timer = 0xF
    end
end

BITE_ACTIONS[0] = bite_begin
BITE_ACTIONS[1] = bite_close
BITE_ACTIONS[2] = bite_seize
BITE_ACTIONS[3] = bite_shake
BITE_ACTIONS[4] = bite_release
BITE_ACTIONS[5] = bite_recover

-- `neptune_behavior_bite` (0x0043f000) - behaviour 5.
local function behavior_bite(e, px, pz)
    local action = BITE_ACTIONS[e.action_state]
    if action then
        action(e, px, pz)
    end
end

-- `neptune_behavior_jumptable` (0x004bc9c0): swim, lunge, retreat, circle,
-- devour, bite.
local BEHAVIORS = {
    behavior_swim,
    behavior_lunge,
    behavior_retreat,
    behavior_circle,
    behavior_devour,
    behavior_bite,
}

-- `neptune_behavior_dispatch` (0x0043de20): the bare indirect JMP. The port
-- counts an out-of-table slot instead of running nothing.
local function behavior_dispatch(e, px, pz, disp)
    local handler = BEHAVIORS[e.action_behavior + 1]
    if handler == nil then
        e:count_placeholder(e.action_behavior)
        return
    end
    handler(e, px, pz, disp)
end

-- `neptune_choose_attack` (0x0043dd50): the drift/steer layer, plus the
-- attack decision. Runs only while the behaviour has not claimed the shark.
local function choose_attack(e, px, pz, disp)
    -- The shared wander-turn drift (the waypoint is the entity's own
    -- player_pos pair, written by the zone path above).
    e:neptune_wander_turn(0x20, 60)

    -- Never interrupt a grab already in progress, and the small variant never
    -- attacks at all.
    if e.player_attacked ~= 0 then
        return
    end
    if (e.behavior_flags & 2) ~= 0 then
        return
    end

    -- Lined up within 768 (a 0x300 half-window)?
    -- Port-only: no one to line up on when hidden.
    if px == nil or e:turn_toward_target(px, pz, 768) ~= 0 then
        return
    end

    if e.player_health < 26 then
        -- Nearly dead: the swallow, and from further out.
        if disp < 3500 then
            e.ignore = 1
            -- A WORD write: behaviour 4, action_state 0.
            e.action_behavior = 4
            e.action_state = 0
        end
    elseif disp < 3000 then
        e.ignore = 1
        e.action_behavior = 5
        e.action_state = 0
    end
end

-- `neptune_state_run` (0x0043dcb0): the per-frame driver.
local function state_run(e)
    local px, pz, disp
    if e:player_hidden() then
        -- Port-only: no path toward a hidden player, who is as far away as
        -- `check_visual_range` reports; the shark keeps cruising.
        disp = 0x7FFF
    else
        local _
        px, _, pz = e:player_pos()
        e:zone_path_update(px, pz)

        -- Manhattan distance, computed and summed entirely in 32 bits.
        local dx = px - e.pos_x
        local dz = pz - e.pos_z
        disp = (dx < 0 and -dx or dx) + (dz < 0 and -dz or dz)
    end

    e.status_flags = e.status_flags & 0x1F
    -- The aquarium boss never gets the "aligned" bit - it is always hostile.
    if (e.behavior_flags & 1) == 0 and disp < 10000 then
        e.status_flags = e.status_flags | 0x40
    end

    e:check_visual_range(4000)

    if e.ignore == 0 then
        choose_attack(e, px, pz, disp)
    end
    behavior_dispatch(e, px, pz, disp)
end

-- ---------------------------------------------------------------------------
-- State 2 - the death/damage machine
-- ---------------------------------------------------------------------------

-- `neptune_death_actions`: 0 turn, 1 thrash, 2 sink.
local DEATH_ACTIONS = {}

-- 0x0043fbd0 - the thrash. Swims hard (0x800 offset) and, when the animation
-- ends, spins the shark a full half-turn.
local function death_thrash(e)
    if e:advance_anim(0x200) then
        e.action_behavior = e.action_behavior + 1
        e.move_speed_current = 0x78
        e.animation_id = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.angle = (e.angle + 0x800) & 0xFFF
    end
    e:check_special_weapon()
    e:move(0x800, e.move_speed_current)
end

-- 0x0043fa90 - turn to face the player for one last look. If the shark cannot
-- line up it gives up and jumps to stage 2 (the sink) instead of thrashing.
-- The stage is stepped to 1 FIRST and then overwritten with 2 on failure.
local function death_turn(e)
    local px, _, pz = e:player_pos()
    e.action_behavior = e.action_behavior + 1
    e.move_speed_current = 0x50
    e.action_ticks_counter = 0x1E

    if e:turn_toward_target(px, pz, 0x400) ~= 0 then
        e.action_behavior = 2
        e.move_speed_current = 0x78
        return
    end

    e.animation_id = 1
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 7
    e:play_enemy_sound(2)
    e.move_timer = 7

    -- The death burst: a heavier spray, lightFactor 0x1E rather than 0.
    e:spawn_joint_effect_lit(0x17, 13, 12, 0, -100, 0, 0, 0x1E)
    e:spawn_joint_effect_lit(0x17, 8, 2, 0, 0, 0, 0, 0x1E)
    e:spawn_joint_effect_lit(0x17, 8, 9, 0, 0, 0, 0, 0x1E)

    -- Runs the thrash's first frame immediately.
    death_thrash(e)
end

-- 0x0043fc70 - sink away, then RESET to state 1. This is a respawn path, not
-- a removal: the shark comes back as a live entity with a cleared hit latch.
-- The dword write is a plain 1 (action_state 0), so it re-runs
-- neptune_swim_begin.
local function death_sink(e)
    local px, _, pz = e:player_pos()
    e:rotate_toward_target(px, pz, 0xFFE0)
    e:advance_anim(0x40)
    e:move(0, e.move_speed_current)

    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 then
        e.state = 1
        e.ignore = 0
        e.action_behavior = 0
        e.action_state = 0
        e.blend_counter = 0x3F
        e.hit_state = 0
    end
    e:check_special_weapon()
end

DEATH_ACTIONS[0] = death_turn
DEATH_ACTIONS[1] = death_thrash
DEATH_ACTIONS[2] = death_sink

-- 0x0043f9b0 - run the stage, and keep trailing bubbles on a 7-frame cycle.
local function death_normal(e)
    local action = DEATH_ACTIONS[e.action_behavior]
    if action then
        action(e)
    end

    if (e.behavior_flags & 2) ~= 0 then
        return
    end

    e.move_timer = e.move_timer - 1
    if e.move_timer == 0 then
        e.move_timer = 7
        bubbles(e)
    end
end

-- `neptune_boss_death_actions`: 0 begin, 1 charge, 2 charge2, 3 beach,
-- 4 fade.
local BOSS_DEATH_ACTIONS = {}

-- 0x0043fe10 - the first charge. Between frames 13 and 24 the shark surges:
-- 0xC00 when it is already lined up, 0x400 while it is still turning.
-- 0xFFF8 is -8.
local function boss_death_charge(e)
    local px, _, pz = e:player_pos()
    e:rotate_toward_target(px, pz, 0xFFF8)

    if e:advance_anim(0x40) then
        e.action_behavior = e.action_behavior + 1
        e.animation_id = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.move_speed_current = 100
    end

    if e.animation_frame_id > 0xC and e.animation_frame_id < 0x19 then
        e.angle = e.angle + 0x400
        local aligned = e:turn_toward_target(px, pz, 0x400)
        e.angle = e.angle - 0x400
        e:move(aligned ~= 0 and 0x400 or 0xC00, e.move_speed_current)
    end
end

-- 0x0043fef0 - the second charge. Same shape, but the surge is UNCONDITIONAL
-- (no frame window) and the speed is zeroed at the end rather than set to
-- 100.
local function boss_death_charge2(e)
    local px, _, pz = e:player_pos()
    e:rotate_toward_target(px, pz, 0xFFF8)

    if e:advance_anim(0x40) then
        e.action_behavior = e.action_behavior + 1
        e.animation_id = 4
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x3F
        e.move_speed_current = 0
        e.action_ticks_counter = 0x2D
    end

    e.angle = e.angle + 0x400
    local aligned = e:turn_toward_target(px, pz, 0x400)
    e.angle = e.angle - 0x400
    e:move(aligned ~= 0 and 0x400 or 0xC00, e.move_speed_current)
end

-- 0x0043ffd0 - beached. Waits out the timer while the death cry repeats,
-- then recolours the shadow quad to 0x00FFFF50 and SHRINKS it by 100 - the
-- carcass settling.
local function boss_death_beach(e)
    if e.action_state == 0 then
        if e.animation_frame_id == 0 then
            e:play_enemy_sound(4)
        end
        e.action_ticks_counter = e.action_ticks_counter - 1
        -- A SIGNED test: the timer is allowed to go past zero and the stage
        -- only advances on the frame the animation also happens to be at 8.
        if s16(e.action_ticks_counter) < 0 and e.animation_frame_id == 8 then
            e.action_state = e.action_state + 1
            e.animation_id = 5
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 0x3F
            e.action_ticks_counter = 0x46
        end
    elseif e.action_state == 1 then
        e.action_behavior = e.action_behavior + 1
        e.action_state = 0
        e.animation_id = 5
        e.shadow_tint = 0x00FFFF50
        e:adjust_shadow_size(-100, -100)
        e.status_flags = e.status_flags | 2
        e.action_ticks_counter = 0x5A
    end
    e:advance_anim(0x40)
end

-- 0x00440110 - fade out. Health is set to -1 as a WORD, and the quad grows 6
-- per frame until the timer runs out. Nothing ever leaves this stage: the
-- script removes the entity.
local function boss_death_fade(e)
    if e.action_state == 0 then
        e.health = -1
        e:adjust_shadow_size(6, 6)
        e.action_ticks_counter = e.action_ticks_counter - 1
        if e.action_ticks_counter == 0 then
            e.action_state = e.action_state + 1
        end
    end
    e:advance_anim(0x40)
end

-- 0x0043fd30 - raise the room event flag that tells the script the boss is
-- dead, spawn the big one-shot effect, and fall straight into the first
-- charge.
local function boss_death_begin(e)
    e:raise_death_event()
    e.action_behavior = e.action_behavior + 1
    e.move_speed_current = 0x96

    e:spawn_joint_effect_at(0, 0, 7, -100, 0, 0, 3)

    e.action_ticks_counter = 0x2D
    e.animation_id = 2
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0x3F
    e:play_enemy_sound(4)

    boss_death_charge(e)
end

BOSS_DEATH_ACTIONS[0] = boss_death_begin
BOSS_DEATH_ACTIONS[1] = boss_death_charge
BOSS_DEATH_ACTIONS[2] = boss_death_charge2
BOSS_DEATH_ACTIONS[3] = boss_death_beach
BOSS_DEATH_ACTIONS[4] = boss_death_fade

-- 0x0043fd10 - the boss's five-stage beaching, indexed by action_behavior.
local function death_boss(e)
    local action = BOSS_DEATH_ACTIONS[e.action_behavior]
    if action then
        action(e)
    end
end

-- `neptune_state_death` (0x0043f970): entered by weapon_apply_damage.
-- Claims the shark on the first frame, then splits on `behavior_flags & 1`.
local function state_death(e)
    if e.ignore == 0 then
        e.ignore = 1
        e.action_behavior = 0
        e.action_state = 0
    end
    if (e.behavior_flags & 1) ~= 0 then
        death_boss(e)
    else
        death_normal(e)
    end
end

-- `neptune_state_table` (0x004bc9a8): SIX entries. States 3, 4 and 5 are each
-- a single RET in the original - real slots that deliberately do nothing.
local STATES = {
    state_init, -- [0] spawn
    state_run, -- [1] the swim/attack AI
    state_death, -- [2] the death split
    function() end, -- [3] RET
    function() end, -- [4] RET
    function() end, -- [5] RET
}

function update(e)
    -- Only a shark past its init tick that also carries 0x80 is skipped; a
    -- state-0 spawn always gets its init tick.
    if e.state ~= 0 and (e.behavior_flags & 0x80) ~= 0 then
        return
    end

    if not e.monster_paused then
        -- The original dispatches the table unchecked; the port bounds-checks
        -- the slot the original would fault on and runs nothing there.
        local state = STATES[e.state + 1]
        if state then
            state(e)
        end

        -- SetEntityScaHitData + ResolveEntityScaCollision(player) +
        -- HandleEnemyPlayerCollisions, in the original's order.
        e:separate()

        -- Probe 800 units along the shark's own facing at radius 600: the
        -- resolve's code ORs into splatter_flag and its accepted distance is
        -- latched in the +0x16E room-hit word for the next frame's
        -- wander-turn.
        local code, travelled = e:probe_ahead_code(800, 600)
        e.splatter_flag = e.splatter_flag | code
        e.sca_touch = travelled
    end

    -- The camera switch-zone bit and the fade-sprite queue run outside the
    -- message gate, but not after the bit-7 early return.
    e:update_switch_zone()
end
