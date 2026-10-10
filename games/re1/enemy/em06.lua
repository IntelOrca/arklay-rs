-- Hunter, entity id 0x06 (model em1006).
--
-- The last of the common roster monsters. Three stacked dispatch layers share
-- the entity's four state bytes:
--   state  0 spawn / 1 the AI driver / 2 the action layer / 3 death /
--          4 a bare return / 8 script-controlled. Whenever the spawn's
--          behavior_flags bit 0x40 is set, an already-running hunter is forced
--          back into state 8 before anything else happens.
--   ignore 0 lets the AI driver decide; 1 means a behaviour (or the action or
--          death layer) owns the hunter.
--   action_behavior is shared by the AI layer (the twelve-entry behaviour
--          table) and the action layer (the nine-entry action table):
--          behaviour 0 idle, 1 chase, 2 approach, 3 nop, 4 swipe, 5 pounce,
--          6 dodge, 7 grab-hold, 8 formation jump-down, 9 scream, 10 sidestep,
--          11 scripted walk-in; action 0/1/3 swipe, 2/8 pounce chain, 4 leap,
--          5 claw flurry, 6 hold the player.
--   action_state is the per-behaviour sub-state.
--
-- behavior_flags doubles as the variant selector: the low nibble picks one of
-- the eight variant AI handlers, bit 0x02 marks hard mode, bit 0x08 the
-- scripted intro, bit 0x10 the coward, bit 0x20 the late-game collision record,
-- bit 0x40 script control and bit 0x80 a full skip.
--
-- Pack tactics: variants 0/1 pair two hunters. The init stores a partner slot
-- (the enemy-list head, or the next slot when this hunter is the head), and the
-- variant-0 decision uses the partner's health, hit state, speed and position
-- to pick the coordinated hold-and-pounce.
--
-- The action layer's end-of-frame collision pass is gated by the literal state
-- test the original uses; the port keeps the same test.
--
-- One file-scope latch is shared by every hunter in the room: "the howl has
-- already played". The port keeps it on the game state; the death dispatcher
-- clears it and the scream/death handlers set it once.
--
-- All mutable state lives on the Rust entity; this script keeps none, so the
-- scripting VM may be reset between any two updates.

-- ---------------------------------------------------------------------------
-- Constants
-- ---------------------------------------------------------------------------

-- Health roll, index rand() & 0xF: 0x5F = 95, 0x4F = 79, 0x6F = 111.
local HEALTH = {
    0x5F, 0x5F, 0x5F, 0x5F, 0x4F, 0x5F, 0x4F, 0x6F,
    0x5F, 0x5F, 0x4F, 0x5F, 0x5F, 0x4F, 0x5F, 0x5F,
}

-- Low-health pounce chance (health < 0x28) and mid (health < 0x4F), index
-- rand() & 0xF. The low table is checked first.
local POUNCE_LOW = {
    0, 1, 0, 1, 0, 1, 0, 1, 1, 0, 1, 0, 1, 0, 0, 1,
}
local POUNCE_MID = {
    1, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1,
}

-- The wounded-player gate for the pouncer, indexed by the player's id & 1:
-- 0 (Jill) at 105, 1 (Chris) at 72.
local PLAYER_GATE = { 105, 72 }

-- Initial behaviour roll, index hit_state & 7.
local BEHAVIOR_SEED = { 0, 0, 2, 0, 2, 0, 0, 0 }

-- The scripted walk-in animation and yaw tables, indexed by the whole
-- behavior_flags byte. The stored rows are the values the original's
-- stride-of-two reads produce (the table overlaps the dodge sub-table's
-- pointer bytes in the image); only the 8..15 rows are reachable.
local INTRO_ANIM = {
    0xA0, 0x41, 0xF0, 0x41, 0x40, 0x41, 0x00, 0x00,
    0x18, 0x19, 0x00, 0x04, 0x00, 0x1A, 0x74, 0xFE,
}
local INTRO_ANGLE = {
    0x7DF0, 0x0041, 0x7E40, 0x0041, 0, 0, 0x0018, 0x0019,
    0xFC00, 0x0400, 0x0000, 0xFE1A, 0xFE74, 0x0000, 0x0113, 0x0000,
}

-- Scripted walk-in movement per frame (doubled by the caller), indexed by the
-- stage/room-selected jump kind.
local INTRO_MOVE = {
    { 24, 25 },      -- [0] default
    { -1024, 1024 }, -- [1]
    { 0, -486 },     -- [2] the courtyard room (stage 3 room 0A)
    { -396, 0 },     -- [3] the return-mansion west passage (stage 6 room 3)
    { 275, 0 },      -- [4] the return-mansion winding corridor (stage 6 room 9)
    { -384, 0 },     -- [5] the same room, attract cameras 10/21
}

-- The stage|room<<8 words the intro selects on (0-based stage digits).
local STAGE_ROOM_F_PASSAGE = 0x0305
local STAGE_ROOM_TRAP_PASSAGE = 0x0905
local STAGE_ROOM_ENRICO = 0x0A02

-- The counter-swipe reach records, walked backwards (rows 2 then 1):
-- start_frame, frame_window, joint_idx, billboard_x, reach_radius.
local SWIPE_REACH = {
    { start = 7, window = 4, joint = 12, x = 1500, radius = 600 },
    { start = 18, window = 2, joint = 15, x = 1000, radius = 600 },
    { start = 20, window = 5, joint = 9, x = 0, radius = 1500 },
}

-- ---------------------------------------------------------------------------
-- Helpers
-- ---------------------------------------------------------------------------

-- Forward declarations for the dispatch tables and the mutually recursive
-- handlers.
local STATES
local SCD
local BEHAVIOR
local ACT
local ATTACK_SUB
local POUNCE_SUB
local DODGE_SUB
local variant_dispatch
local behavior_update
local behavior_dispatch
local death_dispatch
local scd_state_dispatch
local roll_leap
local roll_leap_fixed
local act_swipe
local act_leapattack
local act_pounce
local flurry

-- The 8-bit signed view of a value the original reads through a `char`.
local function s8(value)
    value = value & 0xFF
    if value >= 0x80 then
        return value - 0x100
    end
    return value
end

-- The 16-bit signed view of a counter the original reads through a `short`.
local function s16(value)
    value = value & 0xFFFF
    if value >= 0x8000 then
        return value - 0x10000
    end
    return value
end

-- The +0x86 word store: behavior low byte, action state high byte.
local function set_beh_word(e, value)
    e.action_behavior = value & 0xFF
    e.action_state = (value >> 8) & 0xFF
end

-- The +0x84 word store: state low byte, ignore high byte.
local function set_state_word(e, value)
    e.state = value & 0xFF
    e.ignore = (value >> 8) & 0xFF
end

-- |dz| + |dx|, the Manhattan distance the decision layer accumulates.
local function manhattan(dz, dx)
    return (dz < 0 and -dz or dz) + (dx < 0 and -dx or dx)
end

-- The player's Manhattan distance into the shared displacement scratch.
local function player_distance(e)
    local px, _, pz = e:player_pos()
    return manhattan(pz - e.pos_z, px - e.pos_x)
end

-- The partner's Manhattan distance into the shared line scratch.
local function partner_distance(e)
    local px, pz = e:partner_pos()
    return manhattan(pz - e.pos_z, px - e.pos_x)
end

-- The turn-toward return reduced to the one decision bit the seed roll uses.
local function turn_bit(turn)
    return (turn >> 10) & 1
end

-- ---------------------------------------------------------------------------
-- The pre-AI roll
-- ---------------------------------------------------------------------------

-- The coward variants never fight: they clear their own coward bit once the
-- player gets close enough.
local function coward(e)
    if e.hunter_path_latch ~= 0 and e.player_displacement < 9000 then
        e.behavior_flags = e.behavior_flags & 0xEF
    end
    if e.player_displacement < 4000 then
        e.behavior_flags = e.behavior_flags & 0xEF
    end
end

-- The shared leap-roll tail of variants 0 and 2: aim a pounce target point
-- past the player and hand over to behaviour 5.
local function roll_leap_common(e, rand_bit)
    local px, _, pz = e:player_pos()
    local dx = s16(px - s16(e.pos_x))
    local dz = s16(pz - s16(e.pos_z))
    local yaw = rand_bit * 300 - 0x96
    local rx, rz = e:rotate_xz(yaw, dx, dz)
    rx = s16(rx)
    rz = s16(rz)
    e.hunter_joint_sel = ((yaw + 0x96) // 100) + 6
    e.hunter_target_x = s16(e.pos_x + rx)
    e.hunter_target_z = s16(e.pos_z + rz)
    e:move(0, e.hunter_speed)
    e.angle = e.angle + e.hunter_joint_sel * -0x14 + 0x98
    e.ignore = 1
    e.action_behavior = 5
    e.action_state = 0
end

-- The variant-0 flavour reads the frame seed.
roll_leap = function(e)
    roll_leap_common(e, e.rand_seed & 1)
end

-- The variant-2 flavour draws the platform stream instead.
roll_leap_fixed = function(e)
    roll_leap_common(e, e:random() & 1)
end

-- The plain hunter's decision layer.
local function variant_0(e)
    if e.behavior_flags == 0 then
        e.behavior_flags = 2
        return
    end

    if e:partner_health() < 0 then
        e.behavior_flags = 2
    end
    e.player_distance_z = partner_distance(e)
    e.scaled_down_dist = e.action_behavior

    if e:partner_hit_state() ~= 0 then
        if e.scaled_down_dist ~= 2 then
            e.action_state = 0
            e.blend_counter = 7
        end
        e.action_behavior = 2
        e.ignore = 0
    end
    if e.hunter_path_latch ~= 0 and e.player_displacement < 4000 then
        if e.scaled_down_dist ~= 2 then
            e.action_state = 0
            e.blend_counter = 7
        end
        e.action_behavior = 2
        e.ignore = 0
    end
    if (e.player_attacked & 0x80) ~= 0 and e.player_displacement < 3000 then
        e.ignore = 1
        e.action_behavior = 9
        e.action_state = 0
    end

    if e.player_attacked == 0 then
        local px, _, pz = e:player_pos()
        if e:line_of_sight() == 0
            and e.player_displacement < 3000
            and s16(e:turn_toward_target(px, pz, 0x80)) == 0 then
            e.ignore = 1
            e.action_behavior = 4
            e.action_state = 0
        end

        if e.hunter_path_latch ~= 0 and (e.behavior_flags & 0xF) ~= 7 then
            local wx, wz = e:partner_pos()
            if e.player_distance_z < e.player_displacement
                and s16(e:turn_toward_target(px, pz, 0x20)) == 0
                and e.player_distance_z < 4000
                and s16(e:turn_toward_target(wx, wz, 0x200)) == 0
                and e:partner_move_speed() < 0x32 then
                e.ignore = 1
                e.action_behavior = 6
                e.action_state = 0
                e.status_flags = e.status_flags & 0x1F
            end
            if s16(e:turn_toward_target(wx, wz, 0x200)) == 0
                and e.hunter_room_hit ~= 0
                and e.player_distance_z < 3000 then
                e.ignore = 1
                e.action_behavior = 6
                e.action_state = 0
                e.status_flags = e.status_flags & 0x1F
            end
        end

        e.player_displacement = player_distance(e)
        if e.player_displacement < 2000
            and s16(e:turn_toward_target(px, pz, 0x100)) == 0
            and e.hunter_pounce_latch ~= 0 then
            roll_leap(e)
        end
    end
end

-- The pack follower: it never decides on its own, it just mirrors the
-- scripted formation move while the lead hunter is alive.
local function variant_1(e)
    e.player_distance_z = partner_distance(e)
    if e.action_behavior ~= 8 then
        e.action_state = 0
        e.blend_counter = 7
    end
    e.action_behavior = 8
    e.ignore = 0
end

-- Variants 2 and 7: the health-gated pouncer.
local function variant_2(e)
    local player_health = e.player_health

    e.player_displacement = player_distance(e)
    e.scaled_down_dist = e.action_behavior
    e.hunter_pounce_latch = 0

    if (player_health & 0xFFFF) < PLAYER_GATE[(e.player_character & 1) + 1]
        and player_health > 0 then
        e.hunter_poise = 0
        if e.health < 0x28 then
            e.hunter_pounce_latch = POUNCE_LOW[(e:random() & 0xF) + 1]
        end
        if e.health < 0x4F then
            e.hunter_pounce_latch = POUNCE_MID[(e:random() & 0xF) + 1]
        end
        if player_health > 0xF
            and (e.player_displacement < 0x157C or e.player_attacked ~= 0) then
            e.hunter_pounce_latch = 0
        end
    end
    if e.hunter_repause ~= 0 then
        e.hunter_pounce_latch = 0
    end

    local px, _, pz = e:player_pos()
    if s16(e:turn_toward_target(px, pz, 0x2C8)) == 0
        and e.player_displacement < 0x1900
        and e.hunter_pounce_latch ~= 0 then
        roll_leap_fixed(e)
        return
    end

    if e.hunter_path_latch ~= 0 and e.player_displacement < 6000 then
        if e.scaled_down_dist ~= 2 then
            e.action_state = 0
            e.blend_counter = 7
        end
        e.action_behavior = 2
        e.ignore = 0
    end
    if (e.player_attacked & 0x80) ~= 0 and e.player_displacement < 3000 then
        e.ignore = 1
        e.action_behavior = 9
        e.action_state = 0
    end

    if e.player_attacked == 0 then
        if e:line_of_sight() == 0
            and e.player_displacement < 3000
            and s16(e:turn_toward_target(px, pz, 0x80)) == 0 then
            e.ignore = 1
            e.action_behavior = 4
            e.action_state = 0
            return
        end
        if e.hunter_path_latch ~= 0
            and (e.behavior_flags & 0xF) ~= 7
            and e.player_displacement > 0x1900
            and s16(e:turn_toward_target(px, pz, 0x20)) == 0
            and (e.player_action_behavior == 0x12
                or (e.player_action_behavior == 0x13 and (e.rand_seed & 1) ~= 0))
            and not e:player_facing_entity() then
            e.ignore = 1
            e.action_behavior = 6
            e.action_state = 0
            e.status_flags = e.status_flags & 0x1F
        end
    end
end

-- Variants 4-6: walk to the player until close, then hand over to the
-- scripted formation behaviour and drop the walk-in bit.
local function variant_4(e)
    e.status_flags = e.status_flags & 0x1F
    if e.player_displacement < 4000 then
        e.ignore = 1
        e.action_behavior = 8
        e.action_state = 0
        e.behavior_flags = (e.behavior_flags - 4) & 0xFF
    end
end

local VARIANT = {
    variant_0, -- 0 plain
    variant_1, -- 1 pack follower
    variant_2, -- 2 health-gated pouncer
    nil,       -- 3 no such variant
    variant_4, -- 4
    variant_4, -- 5
    variant_4, -- 6
    variant_2, -- 7
}

variant_dispatch = function(e)
    e.player_displacement = player_distance(e)
    if (e.behavior_flags & 0x10) ~= 0 then
        coward(e)
        return
    end
    local handler = VARIANT[(e.behavior_flags & 0xF) + 1]
    if handler == nil then
        e:count_placeholder(e.behavior_flags & 0xF)
    else
        handler(e)
    end
end

-- ---------------------------------------------------------------------------
-- The AI-layer behaviours
-- ---------------------------------------------------------------------------

-- Behaviour 0, stand half: animation 0x15 for 30 frames, then either re-arm
-- for 200 more (coward bit) or release the action layer to the chase.
local function idle_stand(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0x15
        e.hunter_ticks = 0x1E
        e.blend_counter = 7
        e.hunter_joint_sel = 0
        e.hunter_speed = 0
    end
    e:advance_anim(0x200)
    local prev = e.hunter_ticks
    e.hunter_ticks = prev - 1
    if prev == 0 then
        if (e.behavior_flags & 0x10) ~= 0 then
            e.hunter_ticks = 200
            return
        end
        e.ignore = 0
        set_beh_word(e, 1)
    end
end

-- Behaviour 0, walk half: animation 1, no timer, the AI layer keeps steering.
local function idle_walk(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 1
        e.blend_counter = 7
        e.hunter_speed = 0
        e.hunter_joint_sel = 0
    end
    e:advance_anim(0x200)
end

local IDLE_VARIANT = {
    idle_stand, -- 0
    idle_walk,  -- 1
    idle_stand, -- 2
    nil,        -- 3
    idle_walk,  -- 4
    idle_walk,  -- 5
    idle_walk,  -- 6
    idle_stand, -- 7
}

local function idle_dispatch(e)
    local handler = IDLE_VARIANT[(e.behavior_flags & 0xF) + 1]
    if handler == nil then
        e:count_placeholder(e.behavior_flags & 0xF)
    else
        handler(e)
    end
end

-- Behaviour 1: the run. Wander steering toward the player, footstep cues at
-- animation frames 1 and 32, hard homing inside 2000.
local function chase(e)
    e.animation_id = 0x10
    e.hunter_speed = 0x28
    e:hunter_wander_turn(e.hunter_step_word, 0x18, 0x3C)

    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.hunter_joint_sel = 0
    end
    if e.animation_frame_id == 1 then
        e:play_enemy_sound(0)
    end
    if e.animation_frame_id == 32 then
        e:play_enemy_sound(0)
    end

    local px, _, pz = e:player_pos()
    e.player_displacement = manhattan(pz - e.pos_z, px - e.pos_x)
    if e.player_displacement < 2000 then
        e:rotate_toward_target(px, pz, 0x18)
    end

    e:advance_anim(0x400)
    e:move(0, e.hunter_speed)
end

local function nop(_e) end

-- Behaviour 2: the slow walk-in, capped at 90 frames.
local function approach(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = 2
        e.hunter_speed = 0xF0
        e.hunter_ticks = 10
        e.hunter_joint_sel = 0
        e.hunter_approach_cnt = 0
    end

    e.hunter_approach_cnt = e.hunter_approach_cnt + 1
    if e.hunter_approach_cnt == 90 then
        set_beh_word(e, 1)
    end

    e:advance_anim(0x200)
    if e.animation_frame_id == 1 then
        e:play_enemy_sound(1)
    end
    if e.animation_frame_id == 15 then
        e:play_enemy_sound(1)
    end

    e:hunter_wander_turn(e.hunter_step_word, 0x60, 0x3C)
    e.hunter_ticks = e.hunter_ticks - 1

    local px, _, pz = e:player_pos()
    e.player_displacement = manhattan(pz - e.pos_z, px - e.pos_x)
    if e.player_displacement < 4000 then
        e:rotate_toward_target(px, pz, 0x60)
    end
    e:move(0, e.hunter_speed)
end

-- Behaviour 8: the scripted formation jump-down. Hop, ballistic fall, land,
-- then resume the chase or park for scripted hunters.
local function ledgejump(e)
    e.status_flags = e.status_flags & 0x1F

    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = 0
        e.hunter_speed = 0x5A
        e.hunter_joint_sel = 0
        e.hunter_grab_word = 1
        st = 1
    end

    if st == 1 then
        e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
        if e.animation_frame_id > 9 then
            if e:ballistic(0, 0, -0x3C, 0) ~= 0 then
                e.action_state = 3
                e.animation_frame_id = 0
                e.timing_control = 0
                e.blend_counter = 7
                e.animation_id = e.animation_id + 1
                e.hunter_speed = 0
                e:play_enemy_sound(4)
            end
        end
    elseif st == 2 then
        if e:ballistic(0, 0, -0x3C, 0) ~= 0 then
            e.action_state = 3
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 7
            e.animation_id = e.animation_id + 1
            e.hunter_speed = 0
            e:play_enemy_sound(4)
        end
    elseif st == 3 then
        if e:advance_anim(0x200) then
            e.ignore = 0
            set_beh_word(e, 1)
            e.hunter_grab_word = 0
            e.status_flags = e.status_flags & 0xFB
            if (e.behavior_flags & 0x40) ~= 0 then
                e:raise_scd_flag()
                set_beh_word(e, 0)
            end
        end
    end
    e:move(0, e.hunter_speed)
end

-- Behaviour 9: the howl while the player is held (or dying).
local function scream(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 1
        e.blend_counter = 7
        e.animation_id = 0x17
        e.hunter_scream_latch = 0
    elseif e.action_state ~= 1 then
        return
    end

    if e.animation_frame_id == 8 and e.hunter_scream_latch == 0 then
        e:play_enemy_sound(7)
        e.hunter_scream_latch = 1
    end

    if e:advance_anim(0x200) then
        if (e.behavior_flags & 0x40) ~= 0 then
            e:raise_scd_flag()
        end
        set_beh_word(e, 0)
    end
end

-- Behaviour 10: the post-swipe sidestep. The turn roll picks the side, stored
-- as the signed turn plus two.
local function backstep(e)
    e.player_displacement = player_distance(e)
    e.status_flags = e.status_flags & 0x1F

    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 1
        e.blend_counter = 7
        e.animation_id = 0x14
        e.hunter_speed = (e.player_displacement < 3000) and 0x96 or 0xFA
        local px, _, pz = e:player_pos()
        e.hunter_strafe_dir = (e:turn_toward_target(px, pz, 1) + 2) & 0xFF
    elseif st ~= 1 then
        e:move((s8(e.hunter_strafe_dir) << 10) & 0xFFFF, e.hunter_speed)
        return
    end

    if e:advance_anim(0x200, true) then
        e.state = 1
        e.ignore = 0
        e.action_behavior = 2
        e.action_state = 0
    end
    e.angle = e.angle + (s8(e.hunter_strafe_dir) - 2) * 0x40
    e:move((s8(e.hunter_strafe_dir) << 10) & 0xFFFF, e.hunter_speed)
end

-- Behaviour 11: the scripted walk-in. The stage/room word picks the movement
-- row; the spawn's behavior_flags byte picks the animation and the entry yaw.
local function intro(e)
    if e.action_state == 0 then
        local stage_room = (e.room << 8) | e.stage_id
        if stage_room == STAGE_ROOM_F_PASSAGE then
            e.hunter_intro_jump_kind = 3
        elseif stage_room == STAGE_ROOM_TRAP_PASSAGE then
            e.hunter_intro_jump_kind = 4
            if e.attract_room_camera_id == 10 or e.attract_room_camera_id == 21 then
                e.hunter_intro_jump_kind = 5
            end
        elseif stage_room == STAGE_ROOM_ENRICO then
            e.hunter_intro_jump_kind = 2
        end
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = INTRO_ANIM[e.behavior_flags + 1] or 0
    elseif e.action_state ~= 1 then
        return
    end

    if not e:advance_anim(0x200) then
        if e.animation_frame_id < 6 then
            local row = INTRO_MOVE[e.hunter_intro_jump_kind + 1]
            e.pos_x = e.pos_x + row[1] * 2
            e.pos_z = e.pos_z + row[2] * 2
        end
        if e.animation_frame_id == 20 then
            e:play_enemy_sound(7)
        end
        return
    end

    e.state_word = 0x20001
    e.angle = ((INTRO_ANGLE[e.behavior_flags + 1] or 0) + e.angle) & 0xFFF
    e.behavior_flags = 2
end

-- The twelve-entry AI behaviour table, indexed by action_behavior while the
-- state is 1.
BEHAVIOR = {
    idle_dispatch, -- [0]
    chase,         -- [1]
    approach,      -- [2]
    nop,           -- [3]
    nil,           -- [4] attack, filled below
    nil,           -- [5] pounce, filled below
    nil,           -- [6] dodge, filled below
    nil,           -- [7] grabhold, filled below
    ledgejump,     -- [8]
    scream,        -- [9]
    backstep,      -- [10]
    intro,         -- [11]
}

-- ---------------------------------------------------------------------------
-- The state-2 action layer
-- ---------------------------------------------------------------------------

local function atk_start(e)
    e.action_state = 1
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 7
    e.hunter_speed = 0
    e.animation_id = 5
end

-- Swipe [1]: the claw swing. Frames 4-11 reach-test joint 9 with an 800-unit
-- box; a hit snaps the player into the clawed pose and takes 10 health (13 on
-- the second playthrough). On animation end the hunter drops back to the
-- chase unless script-controlled.
local function atk_swing(e)
    if not e:advance_anim(0x200) then
        if (e.behavior_flags & 0x40) == 0
            and ((e.animation_frame_id - 4) & 0xFF) < 8 then
            local hit = e:reach_test(9, 0, 0, 0, 800)
            e.player_distance_z = hit and 1 or 0
            if hit and e.player_attacked == 0 then
                e:play_enemy_sound(5)
                e.player_attacked = 1
                e.player_state = 2
                e.player_anim_frame_id = 0
                e.player_action_behavior = 100
                e.player_action_state = 0
                e.action_state = 2
                if e.second_playthrough then
                    e.player_health = e.player_health - 13
                    return
                end
                e.player_health = e.player_health - 10
            end
        end
    else
        e.action_state = e.action_state + 1
        if (e.behavior_flags & 0x40) == 0 then
            e.state_word = 0x10001
        end
    end
end

-- Swipe [2]: the follow-up lunge, a burst at speed 200 with a blood billboard
-- on the arm and a red tint flash.
local function atk_lunge(e)
    e.hunter_speed = 200
    e:move(0x52C, e.hunter_speed)
    e.action_state = 3
    e.hunter_ticks = 4

    e:spawn_joint_effect_at(0, 0, 9, 200, 0, 0, 0)
    e:tint_joint(9, 0x30, 0x80820, 0x00606060)
end

-- Swipe [3]: recovery. While the tick counter runs, the player is dragged
-- along by the stored velocity.
local function atk_recover(e)
    e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
    if e.hunter_ticks ~= 0 then
        e.hunter_ticks = e.hunter_ticks - 1
        e:drag_player()
    end
end

-- Swipe [4]: re-roll the next action. A 1-in-4 chains straight into the pounce
-- chain; variant 7 never chains, variant 0 flips a fair coin while its partner
-- is alive.
local function atk_finish(e)
    set_state_word(e, 1)

    local chain = (e:random() & 3) == 0
    if (e.behavior_flags & 0xF) == 0 and e:partner_health() >= 0 then
        chain = (e:random() & 1) ~= 0
    end
    if (e.behavior_flags & 0xF) == 7 then
        chain = false
    end

    e.ignore = chain and 1 or 0
    e.action_behavior = (chain and 5 or 0) + 1
    e.action_state = 0
end

-- Swipe slots 0/1/3. The standing swipe: sub-states 0/1 play it out with the
-- heavy-weapon flinch cancel; sub-state 2 re-rolls the next action and breaks
-- off into the sidestep after two hits inside the latch window.
act_swipe = function(e)
    e.animation_id = (4 - (e.action_behavior == 0 and 1 or 0)) & 0xFF

    local sub = e.action_state
    if sub == 0 then
        e.action_state = 1
        e.hunter_speed = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e:play_enemy_sound(6)
        if (e.behavior_flags & 0x40) ~= 0 then
            e:spawn_joint_effect(3, 8, 1, 0)
            e:spawn_joint_effect(9, 6, 1, 0)
        end
    elseif sub ~= 1 then
        if sub ~= 2 then
            return
        end

        local px, _, pz = e:player_pos()
        local turn = e:turn_toward_target(px, pz, 0x20)
        e.player_displacement = s16(s16(turn) >> 5)

        if (e:random() & 1) == 0 then
            turn = e:turn_toward_target(px, pz, 0x80)
            e.player_displacement = s16(s16(turn) >> 7)
            if e.player_displacement == 0 then
                e.action_behavior = e.action_behavior + 5
                e.ignore = 1
            else
                e.ignore = 0
            end
        end
        if (e.behavior_flags & 2) ~= 0 then
            local roll = e:random() & 1
            e.ignore = roll
            e.action_behavior = roll * 4 + 2
            e.status_flags = e.status_flags & 0x1F
        end

        e.player_pos_x = s16(px)
        e.player_pos_z = s16(pz)
        e.hit_state = 0

        if (e.behavior_flags & 0x40) == 0 then
            e.state = 1
            e.ignore = 0
            e.action_behavior = 1
            e.action_state = 0
        else
            e:raise_scd_flag()
            e.action_behavior = 0
            e.action_state = 0
        end

        if e.hunter_death_cnt_a == 0 then
            return
        end
        if e.hunter_death_cnt_b ~= 2 then
            return
        end

        e.hunter_death_cnt_a = 0
        e.hunter_death_cnt_b = 0
        e.status_flags = e.status_flags & 0x1F
        e.state = 1
        e.ignore = 1
        e.action_behavior = 10
        e.action_state = 0
        return
    end

    e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
    e:check_special_weapon()
end

ATTACK_SUB = { atk_start, atk_swing, atk_lunge, atk_recover, atk_finish, nil }

local function attack(e)
    local handler = ATTACK_SUB[e.action_state + 1]
    if handler == nil then
        e:count_placeholder(e.action_state)
    else
        handler(e)
    end
    e:recenter_on_joint(1)
end

-- Pounce [0]: the crouch. Two crouch clips from the joint selector, or the
-- scripted-spawn crouch for script-controlled hunters.
local function pounce_start(e)
    e.action_state = 1
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 7
    e.hunter_speed = 300
    e.animation_id = (0x12 - s8(e.hunter_joint_sel)) & 0xFF
    if (e.behavior_flags & 0x40) ~= 0 then
        e.animation_id = 0xC
    end
    e:play_enemy_sound(2)
end

-- Pounce [1]: the airborne bite. Frames 7-14 reach-test the selected mouth
-- joint with a 700-unit box while the player's head joint is visible; a hit
-- snaps the bitten pose and hands over to the drag sub-state.
local function pounce_bite(e)
    if (e.behavior_flags & 0x40) == 0
        and e.player_health > 0
        and ((e.animation_frame_id - 7) & 0xFF) < 8
        and (e:entity_joint_flag(0, 1) & 0x40) == 0 then
        local sel = e.hunter_joint_sel
        local hit = e:reach_test(sel, 0, 0, 0, 700)
        e.player_distance_z = hit and 1 or 0
        if hit then
            e.player_attacked = 1
            e.player_state = 7
            e.player_anim_frame_id = 6
            e.player_action_behavior = 0
            e.player_action_state = 0
            e.action_state = 3
            e.hunter_ticks = 4

            e:spawn_joint_effect_at(0, 3, sel, 200, 0, 0, 0)
            e:tint_joint(sel, 0x30, 0x80820, 0x00606060)
            e:play_enemy_sound(5)
            return
        end
    end
    e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
end

-- Pounce [2]: land, release back to the AI layer.
local function pounce_end(e)
    e.state_word = 0x10001
    e.hunter_joint_sel = 0
end

-- Pounce [3]: drag the grabbed player with the mouth-joint tracker and raise
-- the shared grab one-shot.
local function pounce_track(e)
    e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
    e:raise_grab_one_shot()
    e:track_player_joint(e.hunter_joint_sel)
end

-- Pounce [4]: hand over to the hold behaviour.
local function pounce_to_hold(e)
    set_beh_word(e, 7)
end

POUNCE_SUB = { pounce_start, pounce_bite, pounce_end, pounce_track, pounce_to_hold, nil }

-- Behaviour 5: the aimed leap. While the wind-up plays it homes at the stored
-- target point; past frame 0x17 it charges forward.
local function pounce(e)
    e.hunter_pounce_latch = 0
    local handler = POUNCE_SUB[e.action_state + 1]
    if handler == nil then
        e:count_placeholder(e.action_state)
    else
        handler(e)
    end

    if e.animation_frame_id < 0x17 then
        local turn = e:turn_toward_target(e.hunter_target_x, e.hunter_target_z, 0x18)
        e.angle = e.angle + turn
        e:move(0, e.hunter_speed)
        return
    end
    if e.animation_frame_id < 0x1F then
        e.hunter_speed = 0x32
        e:move(0x800, e.hunter_speed)
    end
end

-- Dodge [0]: the retreat hop, animation 0x13, backward drift, grab word armed.
local function dodge_start(e)
    e.action_state = 1
    e.ignore = 1
    e.animation_frame_id = 0
    e.timing_control = 0
    e.death_timer = 0
    e.blend_counter = 7
    e.hunter_speed = 0x5A
    e.animation_id = 0x13
    e.hunter_grab_word = 1
    e.hunter_joint_sel = 0
    if (e.behavior_flags & 0x40) ~= 0 then
        e.hunter_speed = 0x3C
    end
end

-- Dodge [1]: the hop. Every frame whose number has bit 2 set re-arms the
-- counter-swipe.
local function dodge_hop(e)
    e.status_flags = e.status_flags & 0x1F
    e:advance_anim(0x200)
    if (e.animation_frame_id & 4) ~= 0 then
        e.action_state = 2
        e.hunter_speed = 0x82
        e:play_enemy_sound(2)
    end
end

-- Dodge [2]: the counter-swipe, launched ballistically. The two reach records
-- (rows 2 then 1) test their joint during their frame window; a hit snaps the
-- player into one of two clawed poses by facing, tints the joint and takes
-- 15 health (20 on the second playthrough). Landing releases speed 0x5A.
local function dodge_swipe(e)
    e.status_flags = e.status_flags & 0x1F
    if e.animation_frame_id > 8 then
        e.status_flags = e.status_flags | 0x80
    end

    if (e.behavior_flags & 0x40) == 0 then
        for row = 2, 1, -1 do
            local rec = SWIPE_REACH[row + 1]
            local in_window = (e.animation_frame_id - rec.start) & 0xFF
            if in_window < rec.window and e.player_attacked == 0 then
                local hit = e:reach_test(rec.joint, rec.x, 0, 0, rec.radius)
                e.player_distance_z = hit and 1 or 0
                if hit and e.player_attacked == 0 then
                    local facing = e:player_facing_entity() and 1 or 0
                    e.scaled_down_dist = facing
                    e.player_attacked = facing + 2
                    e.player_action_behavior = facing + 0x66

                    e:spawn_joint_effect_at(0, 0, rec.joint, 100, 0, 0, 0)

                    e.action_state = 4
                    e:tint_joint(rec.joint, 0x30, 0x80820, 0x00606060)
                    e:play_enemy_sound(3)
                    if e.second_playthrough then
                        e.player_health = e.player_health - 0x14
                        return
                    end
                    e.player_health = e.player_health - 0xF
                    return
                end
            end
        end
    end

    e:advance_anim(0x400)
    if e:ballistic(e.hunter_speed, 0x244, -0x32, 0) ~= 0 then
        e.action_state = 3
        e.hunter_speed = 0x5A
        e.hunter_grab_word = 0
        e.status_flags = e.status_flags & 0x1F
    end
end

-- Dodge [3]: release back to the chase.
local function dodge_end(e)
    if e:advance_anim(0x400) then
        e.ignore = 0
        set_beh_word(e, 2)
    end
end

-- Dodge [4]: the swiping landing, same arc without the reach tests.
local function dodge_land(e)
    e:advance_anim(0x400)
    e.status_flags = e.status_flags & 0x1F
    if e.animation_frame_id > 8 then
        e.status_flags = e.status_flags | 0xC0
    end
    if e:ballistic(e.hunter_speed, 0x244, -0x32, 0) ~= 0 then
        e.ignore = 0
        e.action_behavior = 2
        e.action_state = 0
        e.hunter_grab_word = 0
        e.status_flags = e.status_flags & 0x1F
    end
end

DODGE_SUB = { dodge_start, dodge_hop, dodge_swipe, dodge_end, dodge_land, nil }

local function dodge(e)
    local handler = DODGE_SUB[e.action_state + 1]
    if handler == nil then
        e:count_placeholder(e.action_state)
    else
        handler(e)
    end
    e:move(0, e.hunter_speed)
end

-- Behaviour 7: holding the grabbed player. Four sub-states: bite down, hold
-- with the head-joint tracker, shake, release back to the AI layer.
local function grabhold(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = e.animation_id + 1
        e.hunter_speed = 0
        st = 1
    end
    if st == 1 then
        e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
        e:track_player_joint(e.hunter_joint_sel)
        return
    end
    if st == 2 then
        e.action_state = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = e.animation_id + 1
        e.hunter_ticks = 5
        st = 3
    end
    if st == 3 then
        local moved = e:advance_anim(0x400) and 1 or 0
        e.hunter_ticks = s16(e.hunter_ticks) - moved
        if e.hunter_ticks == 0 then
            e.action_state = 4
        end
        e:track_player_joint(e.hunter_joint_sel)
        return
    end
    if st == 4 then
        e.state_word = 0x10001
        return
    end
end

BEHAVIOR[5] = attack
BEHAVIOR[6] = pounce
BEHAVIOR[7] = dodge
BEHAVIOR[8] = grabhold

-- Action slot 4: the flying leap. Crouch, ballistic arc, mid-air pose, a
-- randomised hover that can re-roll into the flurry, the landing dive and the
-- release (into the pounce chain when the leap flag was rolled).
act_leapattack = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = 7
        e:play_enemy_sound(6)
        if (e.behavior_flags & 0x40) ~= 0 then
            e:spawn_joint_effect(9, 6, 1, 0)
            e:spawn_joint_effect(3, 8, 1, 0)
        end
        st = 1
    end
    if st == 1 then
        e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
        st = 2
    end
    if st == 2 then
        if e:ballistic(e.move_speed_current // 2, 0, -0x32, 0) ~= 0 then
            e.action_state = 3
            return
        end
    elseif st == 3 then
        e.action_state = 4
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = 0x12
        st = 4
    end
    if st == 4 then
        if e:advance_anim(0x200) then
            e.action_state = 5
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 7
            e.animation_id = 8
            e.hunter_ticks = (e:random() & 0x20) + 0x2D
            e.hunter_speed = 0
            e.hit_state = 0
            return
        end
    elseif st == 5 then
        e.status_flags = (e.status_flags & 0x1F) | 0x20
        e:advance_anim(0x200)

        e.player_displacement = player_distance(e)

        if e.hunter_ticks == 3 and (e:random() & 1) ~= 0 then
            e.action_state = 0
            e.action_behavior = 6
        end
        local prev = e.hunter_ticks
        e.hunter_ticks = prev - 1
        if prev == 0 then
            e.action_state = 6
            return
        end
    elseif st == 6 then
        e.action_state = 7
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = 0xF
        e.hunter_speed = 0x50
        e:move(0xC00, e.hunter_speed)
        e.hit_state = 1
        if (e.rand_seed & 1) ~= 0 and (e.behavior_flags & 0xF) ~= 7 then
            e.hunter_leap_flag = e.hunter_leap_flag | 1
        end
        st = 7
    end
    if st == 7 then
        local adv = e:advance_anim(0x200) and 1 or 0
        e.player_displacement = adv
        e.action_state = e.action_state + adv
        if e.animation_frame_id < 0x1F then
            e.angle = e.angle + 0x40
        end
        if e.animation_frame_id < 0x14 then
            e.pos_x = e.pos_x + e.speed_x
            e.pos_z = e.pos_z + e.speed_z
        end
        if e.player_displacement ~= 0 then
            e.angle = e.angle + 0x800
        end
    elseif st == 8 then
        if (e.behavior_flags & 0x40) == 0 then
            e.state = 1
            e.ignore = 0
            e.action_behavior = 2
            e.action_state = 0
            if (e.hunter_leap_flag & 1) ~= 0 then
                e.state = 1
                e.ignore = 1
                e.action_behavior = 6
                e.action_state = 0
                e.hunter_leap_flag = e.hunter_leap_flag & 0xFE
            end
        else
            e:raise_scd_flag()
            e.action_behavior = 0
            e.action_state = 0
        end
        e.hunter_speed = 0
        e.hunter_grab_word = 0
        local px, _, pz = e:player_pos()
        e.player_pos_x = s16(px)
        e.player_pos_z = s16(pz)
        e.hit_state = 0
        e.status_flags = e.status_flags | 0x40
        return
    end
end

-- Action slots 2 and 8: the pounce chain. The run phase charges at one of
-- three rates per the variant byte; on animation end it re-arms the swipe or
-- raises the script flag.
act_pounce = function(e)
    if e.action_state == 0 then
        if (e.behavior_flags & 8) ~= 0 then
            if e.animation_frame_id > 5 then
                e.angle = e.angle + (INTRO_ANGLE[e.behavior_flags + 1] or 0)
            end
            e.behavior_flags = 2
        end
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        if (e.behavior_flags & 8) ~= 0 then
            e.blend_counter = 0
        end
        e.animation_id = 7
        e:play_enemy_sound(6)
    end

    if not e:advance_anim(0x200) then
        e.hunter_speed = (e.animation_frame_id < 3) and 0xFA or 0x46
        if e.animation_frame_id == 0x10 then
            e:play_enemy_sound(4)
        end
        if e.behavior_flags == 8 then
            if e.animation_frame_id > 5 then
                e.angle = e.angle - 0x400
            end
            e:move(0x400, e.hunter_speed)
            return
        end
        if e.behavior_flags == 9 then
            if e.animation_frame_id > 5 then
                e.angle = e.angle + 0x400
            end
            e:move(0xC00, e.hunter_speed)
            return
        end
        e:move(0x800, e.hunter_speed)
        return
    end

    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 7
    e.animation_id = e.animation_id + 1
    e.hunter_ticks = (e:random() & 0x20) + 0x1E
    e.hunter_speed = 0
    set_beh_word(e, 0x604)
    if (e.behavior_flags & 8) ~= 0 then
        e.behavior_flags = 2
    end
    if (e.behavior_flags & 0x40) ~= 0 then
        e:raise_scd_flag()
        set_beh_word(e, 0)
    end
end

-- Action slot 5: the claw flurry, a blood billboard on the arm every frame.
flurry = function(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.hunter_speed = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = 6
        e:play_enemy_sound(6)
    end

    e:spawn_joint_effect(0, 0, 3, 0)

    if e:advance_anim(0x200) then
        set_beh_word(e, 0x604)
        if (e.behavior_flags & 0x40) ~= 0 then
            e:raise_scd_flag()
            set_beh_word(e, 0)
        end
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = 6
        e.hunter_ticks = 9
        e.hunter_speed = 0
        e.hit_state = 0
    end
end

-- Action slot 6: hold the grabbed player with animation 0x16 until released.
local function holdplayer(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.hunter_speed = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = 0x16
        e.hit_state = 1
    end
    if e:advance_anim(0x200) then
        e.state_word = 0x20001
        e.hunter_speed = 0
        e.hit_state = 0
    end
end

-- The nine-entry action table, indexed by action_behavior while the state is
-- 2. Slot 7 is a null entry no code path assigns.
ACT = {
    act_swipe,      -- [0]
    act_swipe,      -- [1]
    act_pounce,     -- [2]
    act_swipe,      -- [3]
    act_leapattack, -- [4]
    flurry,         -- [5]
    holdplayer,     -- [6]
    nil,            -- [7]
    act_pounce,     -- [8]
}

-- ---------------------------------------------------------------------------
-- The state-3 death layer
-- ---------------------------------------------------------------------------

-- The shared death-animation driver. Sub-state 0/1 play the fall with a
-- forward drift; 2 tints the corpse quad and raises the death event; 3 shrinks
-- the fade quad over the tick counter; 4 raises the script flag.
local function death_fall_driver(e)
    local st = e.action_state
    if st == 0 then
        e.health = -1
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.hunter_ticks = 0x46
        e:play_enemy_sound(7)
        if (e.behavior_flags & 0x40) ~= 0 then
            e:spawn_joint_effect(9, 6, 1, 0)
            e:spawn_joint_effect(3, 8, 1, 0)
            e:spawn_joint_effect(0, 0, 1, 0)
        end
        st = 1
    end

    if st == 1 then
        e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
        e:move(0x800, e.hunter_speed)
        return
    end
    if st == 2 then
        e.shadow_tint = 0x00FFFF50
        e:adjust_shadow_size(-100, -100)
        e:raise_death_event()
        e.action_state = 3
        e.status_flags = e.status_flags | 0x0A
        e.hunter_speed = 0
        return
    end
    if st == 3 then
        e:adjust_shadow_size(12, 12)
        e.hunter_ticks = e.hunter_ticks - 1
        if e.hunter_ticks == 0 then
            e.action_state = 4
        end
        return
    end
    if st == 4 then
        if (e.behavior_flags & 0x40) ~= 0 then
            e:raise_scd_flag()
        end
    end
end

-- Death slots 0/1/3: the standard fall with the death howl once and a second
-- cry at frame 0x4B, then the recenter pass that pivots the body.
local function death_fall(e)
    e.animation_id = 0x11
    e.hunter_speed = 0
    death_fall_driver(e)

    if (e.animation_frame_id == 0x32 or e.animation_frame_id == 0x24)
        and e.hunter_scream_latch == 0 then
        e:play_enemy_sound(7)
        e.hunter_scream_latch = 1
    end
    if e.animation_frame_id == 0x4B then
        e:play_enemy_sound(4)
    end
    if e.blend_counter == 0 then
        if e.animation_frame_id > 0x17 then
            e:recenter_on_joint(1)
            return
        end
        e:recenter_on_joint(0)
    end
end

-- Death slot 2: the thrash, a burst speed that settles after frame 3.
local function death_thrash(e)
    e.animation_id = 7
    e.hunter_speed = (e.animation_frame_id < 3) and 0xFA or 0x46
    death_fall_driver(e)
    if e.animation_frame_id == 0x0D or e.animation_frame_id == 0x17 then
        e:play_enemy_sound(4)
    end
end

-- Death slot 5: animation 0x0F, no drive.
local function death_settle(e)
    e.animation_id = 0x0F
    e.hunter_speed = 0
    death_fall_driver(e)
    e:recenter_on_joint(0)
end

-- Death slot 6: animation 6, no drive.
local function death_collapse(e)
    e.animation_id = 6
    e.hunter_speed = 0
    death_fall_driver(e)
end

-- Death slot 4: killed mid-pounce. The leap continues ballistically, the body
-- crashes down and the loop hands back to the settle animation.
local function death_pounce(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.hunter_ticks = 0x46
        e.animation_id = 0x11
        e:play_enemy_sound(7)
        st = 1
    end
    if st == 1 then
        e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
        st = 2
    end
    if st == 2 then
        if e:ballistic(e.move_speed_current // 2, 0, -0x32, 0) ~= 0 then
            e.action_state = 3
            e.hunter_speed = 0
            e.hunter_grab_word = 0
            return
        end
    elseif st == 3 then
        e.action_state = 4
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.animation_id = 0x12
        e:play_enemy_sound(4)
        st = 4
    end
    if st == 4 then
        if e:advance_anim(0x200) then
            e.action_behavior = 0
            e.action_state = 2
        end
    end
end

-- The eight-entry death table, indexed by action_behavior while the state is
-- 3. Slots 0/1/3 share the fall; 7 is a null entry.
local DEATH = {
    death_fall,     -- [0]
    death_fall,     -- [1]
    death_thrash,   -- [2]
    death_fall,     -- [3]
    death_pounce,   -- [4]
    death_settle,   -- [5]
    death_collapse, -- [6]
    nil,            -- [7]
}

-- ---------------------------------------------------------------------------
-- The SCD-derived action layer (state 8)
-- ---------------------------------------------------------------------------

-- Slot 0: park on animation 0x15 until the script moves the hunter.
local function scd_idle(e)
    if e.action_state == 0 then
        e.animation_id = 0x15
        e.hunter_speed = 0
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x1F
    end
    e:advance_anim(0x80, (e.flags & 1) ~= 0)
end

-- Slot 2: walk toward the scripted waypoint pair (+0xC6/+0xC8).
local function scd_walk(e)
    local st = e.action_state
    if st == 0 then
        e.animation_id = 0x10
        e.hunter_speed = 0x28
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x1F
        e.hunter_joint_sel = 0
        st = 1
    end
    if st == 1 then
        e:rotate_toward_target(e.unk_c6, e.unk_c8, 0x40)
        e:advance_anim(0x80, (e.flags & 1) ~= 0)
        e:move(0, e.hunter_speed)
        if e.animation_frame_id == 1 then
            e:play_enemy_sound(0)
        end
        if e.animation_frame_id == 32 then
            e:play_enemy_sound(0)
        end
        if e:xz_distance_to(e.unk_c6, e.unk_c8) < 0x96 then
            e.action_state = e.action_state + 1
        end
        return
    end
    if st == 2 then
        e:raise_scd_flag()
        if (e.collision_flags & 0x80) ~= 0 then
            e.action_state = 1
            return
        end
        set_beh_word(e, 0)
    end
end

-- Slot 3: the same skeleton with animation 2, speed 200 and a faster turn.
local function scd_run(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x1F
        e.animation_id = 2
        e.hunter_speed = 200
        st = 1
    end
    if st == 1 then
        e:advance_anim(0x80, (e.flags & 1) ~= 0)
        if e.animation_frame_id == 1 then
            e:play_enemy_sound(1)
        end
        if e.animation_frame_id == 15 then
            e:play_enemy_sound(1)
        end
        e:rotate_toward_target(e.unk_c6, e.unk_c8, 0x60)
        e:move(0, e.hunter_speed)
        if e:xz_distance_to(e.unk_c6, e.unk_c8) < 0x96 then
            e.action_state = e.action_state + 1
        end
        return
    end
    if st == 2 then
        e:raise_scd_flag()
        if (e.collision_flags & 0x80) ~= 0 then
            e.action_state = 1
            return
        end
        set_beh_word(e, 0)
    end
end

-- Slot 7: the scripted death, two animation steps then the target tracking.
local function scd_death(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x1F
        e.animation_id = e.animation_id + 1
        e.hunter_speed = 0
        st = 1
    end
    if st == 1 then
        if e:advance_anim(0x80) then
            e.action_state = 2
            e.animation_frame_id = 0
            e.timing_control = 0
            e.animation_id = e.animation_id + 1
        end
        e:track_target_joint(e.hunter_joint_sel)
        return
    end
    if st == 2 then
        e:advance_anim(0x80)
        e:track_target_joint(e.hunter_joint_sel)
    end
end

-- Slot 10: re-enter the table at the dodge sub-range base, then move.
local function scd_dodge_run(e)
    local handler = SCD[24 + e.action_state + 1]
    if handler == nil then
        e:count_placeholder(24 + e.action_state)
    else
        handler(e)
    end
    e:move(0, e.hunter_speed)
end

-- Slot 27: hold the animation blend to completion, then raise the flag.
local function scd_anim_release(e)
    if e:advance_anim(0x400) then
        e:raise_scd_flag()
        set_beh_word(e, 0)
    end
end

-- Slot 11: re-enter at the pounce sub-range base, then home or charge.
local function scd_pounce_run(e)
    local handler = SCD[28 + e.action_state + 1]
    if handler == nil then
        e:count_placeholder(28 + e.action_state)
    else
        handler(e)
    end
    if e.animation_frame_id < 0x17 then
        e.angle = e.angle + e:turn_toward_target(e.hunter_target_x, e.hunter_target_z, 0x18)
        e:move(0, e.hunter_speed)
        return
    end
    if e.animation_frame_id < 0x1F then
        e.hunter_speed = 0x32
        e:move(0x800, e.hunter_speed)
    end
end

-- Slot 30: pure completion.
local function scd_flag_release(e)
    e:raise_scd_flag()
    set_beh_word(e, 0)
end

-- Slot 13: re-enter at the swipe sub-range base, then recenter on the claw.
local function scd_swipe_run(e)
    local handler = SCD[32 + e.action_state + 1]
    if handler == nil then
        e:count_placeholder(32 + e.action_state)
    else
        handler(e)
    end
    e:recenter_on_joint(1)
end

-- Slot 34: tint the claw joint before the standard completion.
local function scd_tint_release(e)
    e:tint_joint(9, 0x30, 0x80820, 0x00606060)
    e:raise_scd_flag()
    set_beh_word(e, 0)
end

-- Slot 12: the same driver over the bite sub-range base.
local function scd_attack_run(e)
    local handler = SCD[36 + e.action_state + 1]
    if handler == nil then
        e:count_placeholder(36 + e.action_state)
    else
        handler(e)
    end
    if e.animation_frame_id < 0x17 then
        e.angle = e.angle + e:turn_toward_target(e.hunter_target_x, e.hunter_target_z, 0x18)
        e:move(0, e.hunter_speed)
        return
    end
    if e.animation_frame_id < 0x1F then
        e.hunter_speed = 0x32
        e:move(0x800, e.hunter_speed)
    end
end

-- Slot 36: the one-shot lunge setup. The original computes 0x12 - the joint
-- selector into the animation id and immediately overwrites it with 0xC.
local function scd_lunge_start(e)
    e.action_state = 1
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 7
    e.hunter_speed = 300
    e.animation_id = (0x12 - (e.hunter_joint_sel & 0xFF)) & 0xFF
    e.animation_id = 0xC
    e:play_enemy_sound(2)
end

-- Slot 37: the lunge-in and the scripted bite. During frames 7-14, once the
-- target's bite-joint latch clears, the grab lands on the target entity.
local function scd_bite_driver(e)
    if ((e.animation_frame_id - 7) & 0xFF) < 8
        and (e:entity_joint_flag(1, 1) & 0x40) == 0 then
        e:set_entity_hit_state(1, 1)
        e:set_entity_state_word(1, 0x20001)
        e.action_state = 2
        e.hunter_ticks = 4
        e:spawn_joint_effect_at(0, 0, 6, 200, 0, 0, 0)
        e:tint_joint(6, 0x30, 0x80820, 0x00606060)
        e:play_enemy_sound(5)
        return
    end
    e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
end

-- Slot 38: finish the bite, raise the flag, clear the target's latch and run
-- the head tracking.
local function scd_bite_end(e)
    if e:advance_anim(0x200) then
        e:raise_scd_flag()
        set_beh_word(e, 7)
    end
    e:set_entity_joint_flag(1, 1, e:entity_joint_flag(1, 1) & 0xFE)
    e:track_target_joint(e.hunter_joint_sel)
end

-- Slot 22: face and follow the player for a randomised stalk window.
local function scd_stalk_player(e)
    if e.action_state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x1F
        e.hit_state = 0
        local px, _, pz = e:player_pos()
        if s16(e:turn_toward_target(px, pz, 0x400)) ~= 0 then
            e.animation_id = 0x10
        end
        e.action_state = 1
        e.hunter_ticks = (e:random() & 0x3F) + 0x1E
    end

    local px, _, pz = e:player_pos()
    local delta = s16(e:turn_toward_target(px, pz, 100))
    local ticks = e.hunter_ticks
    e.hunter_ticks = s16(ticks - 1)
    if ticks ~= 0 and delta ~= 0 then
        e:advance_anim(0x80)
        e.angle = e.angle + delta
        return
    end
    e:raise_scd_flag()
    set_beh_word(e, 0)
    e.player_pos_x = s16(px)
    e.player_pos_z = s16(pz)
end

-- The 39-slot table. Slots 10-13 are composite wrappers that re-enter this
-- same array at the dodge/pounce/attack/swipe sub-range bases, indexed by
-- action_state; the four sub-ranges share the array with everything else.
SCD = {
    scd_idle,           -- [0]
    nil,                -- [1]
    scd_walk,           -- [2]
    scd_run,            -- [3]
    nil,                -- [4]
    nil,                -- [5]
    nil,                -- [6]
    scd_death,          -- [7]
    nil,                -- [8]
    nil,                -- [9]
    scd_dodge_run,      -- [10] -> [24 + action_state]
    scd_pounce_run,     -- [11] -> [28 + action_state]
    scd_attack_run,     -- [12] -> [36 + action_state]
    scd_swipe_run,      -- [13] -> [32 + action_state]
    act_swipe,          -- [14]
    act_leapattack,     -- [15]
    act_pounce,         -- [16]
    flurry,             -- [17]
    scream,             -- [18]
    death_fall,         -- [19]
    death_collapse,     -- [20]
    death_thrash,       -- [21]
    scd_stalk_player,   -- [22]
    ledgejump,          -- [23]
    dodge_start,        -- [24] dodge sub-range base
    dodge_hop,          -- [25]
    dodge_swipe,        -- [26]
    scd_anim_release,   -- [27]
    pounce_start,       -- [28] pounce sub-range base
    pounce_bite,        -- [29]
    scd_flag_release,   -- [30]
    nil,                -- [31]
    atk_start,          -- [32] swipe sub-range base
    atk_swing,          -- [33]
    scd_tint_release,   -- [34]
    nil,                -- [35]
    scd_lunge_start,    -- [36] bite sub-range base
    scd_bite_driver,    -- [37]
    scd_bite_end,       -- [38]
}

-- The state-8 dispatcher: without the script-control bit the state falls back
-- to 1; with it the script's behaviour byte indexes the SCD table.
scd_state_dispatch = function(e)
    if (e.behavior_flags & 0x40) == 0 then
        e.state = 1
        return
    end
    local handler = SCD[e.action_behavior + 1]
    if handler == nil then
        e:count_placeholder(e.action_behavior)
    else
        handler(e)
    end
end

-- ---------------------------------------------------------------------------
-- The state tables and the per-frame entry
-- ---------------------------------------------------------------------------

-- The AI-layer behaviour driver: latch the pathfinder result (a non-zero
-- result also arms the 60-frame re-pause), refresh the waypoint pair, then
-- dispatch on action_behavior.
behavior_update = function(e)
    local px, _, pz = e:player_pos()
    local path = e:pathfind_update(px, pz)

    if e.hunter_path_latch == 0 then
        if (path & 0xFE) == 0 then
            e.hunter_path_latch = e.hunter_path_latch | (path & 1)
        end
        if e.hunter_path_latch ~= 0 then
            e.hunter_repause = 0x3C
        end
    end

    e:zone_path_update(px, pz)

    local handler = BEHAVIOR[e.action_behavior + 1]
    if handler == nil then
        e:count_placeholder(e.action_behavior)
    else
        handler(e)
    end
end

-- The state-2 action layer. On entry it seeds the action behaviour from the
-- hit-state roll, cancels into the pounce when a grab is pending, and forces
-- the leap or the pounce chain from the status/flag bits.
behavior_dispatch = function(e)
    if e.ignore == 0 then
        e.hunter_death_cnt_b = e.hunter_death_cnt_b + 1

        if e.hunter_death_cnt_a == 0 then
            e.hunter_death_cnt_a = 0x32
        end
        e.state = 2
        e.ignore = 1
        e.action_behavior = 0
        e.action_state = 0

        local px, _, pz = e:player_pos()
        e.action_behavior = BEHAVIOR_SEED[(e.hit_state & 7) + 1]
        e.action_behavior =
            (e.action_behavior + turn_bit(e:turn_toward_target(px, pz, 0x400))) & 0xFF

        if e.hunter_grab_word ~= 0 then
            local saved = e.action_behavior
            e.action_behavior = 4
            if e.pos_y > -500 then
                e.pos_y = 0
                e.hunter_grab_word = 0
                e.action_behavior = saved
            end
        end
        if (e.status_flags & 0xE0) == 0x20 then
            e.action_behavior = 5
        end
        if (e.behavior_flags & 8) ~= 0 then
            e.action_behavior = 8
        end
    end

    local handler = ACT[e.action_behavior + 1]
    if handler == nil then
        e:count_placeholder(e.action_behavior)
    else
        handler(e)
    end
end

-- State 3: the same seeding minus the turn bit for hit-state 2, plus the death
-- event raise and the scream-latch reset.
death_dispatch = function(e)
    if e.ignore == 0 then
        e.state = 3
        e.ignore = 1
        e.action_behavior = 0
        e.action_state = 0

        e.action_behavior = BEHAVIOR_SEED[(e.hit_state & 7) + 1]
        if (e.hit_state & 3) ~= 2 then
            local px, _, pz = e:player_pos()
            e.action_behavior =
                (e.action_behavior + turn_bit(e:turn_toward_target(px, pz, 0x400))) & 0xFF
        end

        if e.hunter_grab_word ~= 0 then
            local saved = e.action_behavior
            e.action_behavior = 4
            if e.pos_y > -500 then
                e.pos_y = 0
                e.hunter_grab_word = 0
                e.action_behavior = saved
            end
        end
        if (e.status_flags & 0xE0) == 0x20 then
            e.action_behavior = 6
        end

        e:raise_death_event()
        e.hunter_scream_latch = 0
    end

    local handler = DEATH[e.action_behavior + 1]
    if handler == nil then
        e:count_placeholder(e.action_behavior)
    else
        handler(e)
    end
end

-- State 0: the one-shot spawn.
local function init(e)
    e.state_word = 1
    e.hunter_ticks = 0
    e.animation_id = 0
    e.hit_state = 0
    e:reset_joints()

    e:set_shadow_offset(0, 0, 0)
    e.shadow_tint = 0x808080
    e.shadow_half_x = 1000
    e.shadow_half_z = 1000

    e.health = HEALTH[(e:random() & 0xF) + 1]

    e:set_sca(900, 180, 0, -180, 0)
    if (e.behavior_flags & 0x20) ~= 0 or e.stage_id > 4 then
        e:set_sca(500, 180, 0, -180, 0)
    end

    if (e.behavior_flags & 0x0F) < 2 then
        e.hunter_partner = e:pick_partner()
        if (e:partner_status() & 1) == 0 then
            e.behavior_flags = 2
        end
    end
    if (e.behavior_flags & 0x0F) == 0 then
        e.behavior_flags = e.behavior_flags | 2
    end

    e.hunter_path_latch = 0
    e.hunter_grab_word = 0
    e.hunter_joint_sel = 0
    e.hunter_pounce_latch = 0
    e.hunter_poise = 0
    e.hunter_repause = 0
    e.hunter_death_cnt_a = 0
    e.hunter_death_cnt_b = 0

    if (e.behavior_flags & 8) ~= 0 then
        e.ignore = 1
        e.action_behavior = 0xB
    end
end

-- Function-local copies of the entries the dispatch tables and the entry point
-- need before their definitions above.
local function state_ret(_e) end

-- State 1: the per-frame AI driver.
local function state_run(e)
    if (e.behavior_flags & 0x80) ~= 0 then
        return
    end

    e.status_flags = (e.status_flags & 0x1F) | 0x40
    e:check_visual_range(4000)

    local _, py, _ = e:player_pos()
    local dy = py - e.pos_y
    if dy ~= 0 then
        e.status_flags = e.status_flags & 0x1F
        if dy < 0 then
            e.status_flags = e.status_flags | 0x20
        else
            e.status_flags = e.status_flags | 0x80
        end
    end

    if e.hunter_repause ~= 0 then
        e.hunter_repause = e.hunter_repause - 1
    end

    if e.ignore == 0 then
        variant_dispatch(e)
    end
    behavior_update(e)
end

-- The nine-slot state table, indexed by the entity state. Slots 5-7 are null
-- entries no live path reaches.
STATES = {
    init,                  -- [0]
    state_run,             -- [1]
    behavior_dispatch,     -- [2]
    death_dispatch,        -- [3]
    state_ret,             -- [4]
    nil,                   -- [5]
    nil,                   -- [6]
    nil,                   -- [7]
    scd_state_dispatch,    -- [8]
}

-- `hunter_update`: force script-controlled hunters into state 8, tick the
-- damage latch, run the state table under the message gate and close with the
-- shared collision pass and the switch-zone shadow bit.
function update(e)
    if e.state ~= 0 and (e.behavior_flags & 0x40) ~= 0 then
        e.state = 8
    end

    local a = (e.hunter_death_cnt_a - 1) & 0xFF
    e.hunter_death_cnt_a = a
    if s8(a) < 0 then
        e.hunter_death_cnt_a = 0
        e.hunter_death_cnt_b = 0
    end

    if not e.monster_paused then
        local handler = STATES[e.state + 1]
        if handler == nil then
            e:count_placeholder(e.state)
        else
            handler(e)
        end

        -- The literal gate the original uses; no live state reaches 5.
        if e.state ~= 5 then
            e:separate()
            local old_x, old_z = e.prev_pos_x, e.prev_pos_z
            local hit = e:resolve_collision()
            e.hunter_room_hit = e.hunter_room_hit | hit
            e.hunter_step_word = e:xz_distance_to(old_x, old_z) & 0xFFFF
        end
    end

    e.has_enter_switch_zone = e:update_switch_zone()
end
