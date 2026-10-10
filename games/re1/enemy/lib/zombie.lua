-- Zombie, shared driver for entity ids 0x00 (standard), 0x01 (naked) and
-- 0x11 (green). The per-id scripts pass their collision record in and this
-- module runs the whole machine.
--
-- The machine is the original's five overlapping dispatch tables:
--
--   STATES           one 22-slot pointer block read through two bases:
--                    state[k] is STATES[k], behaviour[k] is STATES[10 + k].
--                    States 6, 7 and 9 are NULL in the original; behaviours
--                    9 and 11 are too. The port bounds-checks the slots the
--                    original would fault on and runs nothing there.
--   DAMAGE_ACTIONS   one 28-slot block read through three bases:
--                    damage[k]  = DAMAGE_ACTIONS[k]      (k 0..11)
--                    action[k]  = DAMAGE_ACTIONS[8 + k]  (k 0..7, and 8..12
--                                 alias move[0..4] through the same block)
--                    move[k]    = DAMAGE_ACTIONS[16 + k] (k 0..11)
--                    This module keeps the one block, so the aliasing is the
--                    original's, not a copy with matching values.
--   SCD_ACTIONS      the separate 14-slot scripted-action table.
--
-- Behaviour byte semantics:
--   bit 0x02  laying down (a prone body: the attack rows and the two-point
--             floor probe switch on it)
--   bit 0x04  deactivated on the floor
--   bit 0x40  vomiting (the SCD path and the vomit stagger)
--   bit 0x80  SCD-controlled (state 1 does nothing)
--   nibble 0x0F selects the behaviour row; the full byte is compared in the
--   init's dead/floor/vomit branches.
--
-- Death forks on hit_state's direction and the head joint's gore byte:
--   action_behavior 0  the plain fall, which can get back up (1-in-4 roll,
--                      clear floor, intact head, not the 2F dining room)
--   action_behavior 1  the headshot fall (the bite leaves the head intact,
--                      so the player-kill path can still raise it)
--   action_behavior 2  a prone body already on the floor
--   action_behavior 3  the magnum pushback, then the fall
--
-- The head-explosion and limb gore *state machine* is complete: the joint
-- flag bytes (bit 0 active, 0x28 armed/exploded, 0x0C severed arm, 0x88
-- vomiting head), the 0xCC/0x40 head tests, the blood scratch the vomit arms
-- and the blood billboards/cues all run. The per-joint colour tints and the
-- async joint-object chunks stay with the renderer's documented deferral:
-- `tint_joint` is called at the tint sites but records nothing, and the
-- joint-based second ground quad (the severed limb's pool at the joint world
-- position) keeps its flag/position contract only.
--
-- Director's Cut is not modelled: the port has no DC mode, so the 0xC/0xD/0xE
-- spawn nibbles, the third SCA record, the double-step fast zombie and
-- behaviour 11 (`dc_standup_lunge`) stay dormant exactly as the USA build
-- ships them.
--
-- All state lives on the Rust entity; this module keeps none, so the scripting
-- VM may be reset between any two updates.

local M = {}

-- ---------------------------------------------------------------------------
-- Constants
-- ---------------------------------------------------------------------------

local STATE_INIT = 0
local STATE_IDLE = 1
local STATE_DAMAGED = 2
local STATE_DIE = 3
local STATE_ATTACK = 5
local STATE_ACTION_UPDATE = 8

local FLAG_LAYING_DOWN = 0x02
local FLAG_VOMITING = 0x40
local FLAG_SCD_CONTROLLED = 0x80

local STATUS_ACTIVE = 0x01
local STATUS_DEAD = 0x08
local STATUS_ALIGNED = 0x40

-- ---------------------------------------------------------------------------
-- Helpers
-- ---------------------------------------------------------------------------

-- The 16-bit signed view of a counter the original reads through a `short`.
local function s16(value)
    value = value & 0xFFFF
    if value >= 0x8000 then
        return value - 0x10000
    end
    return value
end

-- The 8-bit signed view of a byte the original reads through a `char`.
local function s8(value)
    value = value & 0xFF
    if value >= 0x80 then
        return value - 0x100
    end
    return value
end

-- C-style truncating division (Lua's `//` floors).
local function idiv(a, b)
    local q = a // b
    if a % b ~= 0 and (a < 0) ~= (b < 0) then
        q = q + 1
    end
    return q
end

-- C-style remainder (Lua's `%` floors).
local function cmod(a, b)
    return a - idiv(a, b) * b
end

-- The original's Manhattan distance to the player with its sign quirk:
-- absDz - (dx >> 31) + absDx, so a player to the left adds one extra unit.
local function player_distance(e)
    local px, _, pz = e:player_pos()
    local dz = pz - e.pos_z
    local dx = px - e.pos_x
    local abs_dz = (dz < 0) and -dz or dz
    local abs_dx = (dx < 0) and -dx or dx
    return abs_dz - (dx >> 31) + abs_dx
end

-- `sVar = *ticks; *ticks = sVar - 1; return sVar == 0` - the
-- value-before-decrement timer test that fires a frame later than
-- `--ticks == 0` and lets the counter wrap through 0xFFFF.
local function tick_expired(e)
    local ticks = s16(e.action_ticks_counter)
    e.action_ticks_counter = ticks - 1
    return ticks == 0
end

-- The one-word idle hand-back: state 1, ignore 1, behaviour 3.
local function hand_to_behavior_3(e)
    e.state = STATE_IDLE
    e.ignore = 1
    e.action_behavior = 3
    e.action_state = 0
end

local function second_playthrough(e)
    return e.second_playthrough and true or false
end

local function threshold_from_table(e, normal, hard)
    if second_playthrough(e) then
        return hard[(e.rand_seed & 0x1F) + 1]
    end
    return normal[(e.rand_seed & 0x1F) + 1]
end

-- ---------------------------------------------------------------------------
-- Data tables
-- ---------------------------------------------------------------------------

-- 0x004bb288 - health base, index = rand() & 0xF; init subtracts rand() % 22.
local HEALTH = {
    59, 59, 79, 59, 59, 39, 59, 79,
    79, 99, 59, 79, 59, 59, 79, 59,
}

-- 0x004bb298 - initial animationId, index = behavior_flags & 0xF.
local ANIM_ID = {
    0, 0, 9, 9, 0, 12, 9, 29,
    0, 0, 9, 0, 0, 0, 0, 0,
}

-- 0x004bb2a8 - stagger_timer (poise) budget, index = g_RandSeed & 0x1F.
local STAGGER = {
    4, 3, 5, 3, 4, 4, 3, 4,
    3, 5, 4, 4, 5, 3, 4, 5,
    4, 3, 3, 4, 4, 3, 4, 4,
    5, 3, 5, 3, 4, 3, 3, 4,
}

-- The hit-threshold roll: the second half of the init's local table is the
-- first-playthrough row, the first half the second-playthrough row.
local THRESHOLD_NORMAL = {
    1, 2, 3, 2, 3, 3, 2, 3,
    3, 2, 2, 3, 2, 3, 2, 3,
    2, 3, 2, 4, 2, 2, 2, 2,
    3, 2, 2, 3, 2, 2, 2, 3,
}
local THRESHOLD_HARD = {
    3, 2, 3, 3, 2, 3, 4, 3,
    2, 3, 3, 4, 3, 2, 3, 3,
    3, 2, 3, 3, 3, 3, 2, 3,
    3, 2, 3, 4, 3, 4, 3, 2,
}

-- 0x004bb31f - the unaligned byte table indexed by
-- (hit_state & 7) + (behavior_flags & 2) * 3; the prone stride is six.
local DAMAGE_ACTION = {
    0, 0, 5, 0, 2, 0, 0, 4,
    4, 0, 0, 0, 0, 0,
}

-- 0x004bb3a0 - one 12-byte block, three overlapping views per
-- attacking_direction: attack anim +0, player anim +1, keyframe +2.
local ATTACK_ANIM = {
    0x11, 0x00, 0x00,
    0x14, 0x03, 0x00,
    0x17, 0x06, 0x06,
    0x1A, 0x09, 0x0E,
}

-- 0x004bb3b0 - player attackDirection per direction; 0x7FFF keeps the
-- player's facing.
local ATTACK_DIR_OFFSET = { 0x7FFF, 0x0000, 0x7FFF, 0x0000 }

-- 0x004bb3c8 / 0x004bb3d8 - bite damage per behaviour nibble.
local DAMAGE_NORMAL = { 10, 10, 6, 6, 10, 10, 10, 10, 10, 0, 0, 0, 0, 0, 0, 0 }
local DAMAGE_HARD = { 12, 12, 9, 9, 12, 12, 12, 12, 12, 0, 0, 0, 0, 0, 0, 0 }

-- 0x004bb3e8 - the falling attack per attacking_direction:
-- {speed_offset, timer, angle_offset}.
local ATTACK_DATA = {
    0x0004, 0x0020, 0x0020,
    0x0004, 0x0055, 0x000C,
    0x0233, 0x0041, 0xFFE0,
    0x0000, 0x0A0A, 0x090C,
}

-- 0x004bb400 - recovery timer multipliers (x30 frames), index g_RandSeed & 0xF.
local RECOVERY = { 2, 4, 4, 4, 4, 4, 4, 6, 6, 6, 6, 6, 6, 9, 9, 9 }

-- The forward declarations the tables and each other need.
local STATES
local DAMAGE_ACTIONS
local SCD_ACTIONS
local init
local state_check
local damaged
local die
local attack
local action_update
local chase_player
local pushed_back
local random_chase
local eating
local update_player_distance
local update_zombie_action
local zombie_idle
local zombie_slow_walk
local zombie_chase_walk
local fast_player_facing
local benddown_and_eat
local turn_towards_player
local zombie_vomiting
local zombie_falldown
local zombie_walk1
local zombie_check_player_distance
local zombie_slow_walk_alt
local zombie_idling
local zombie_walk2
local pushback_action
local short_push_back
local push_and_stagger
local push_and_drop
local explode_leg_and_drop
local long_push_back
local benddown_and_standup
local zombie_pushback_idle
local zombie_pushback_stagger
local magnum_shot_pushback
local zombie_dead_animation
local zombie_headshot
local zombie_scd_dying
local zombie_scd_vomiting
local zombie_scd_vomiting2
local zombie_aggresive_roar

-- ---------------------------------------------------------------------------
-- State 0: init
-- ---------------------------------------------------------------------------

init = function(e, config)
    e.state = STATE_IDLE
    e.ignore = 0
    e.action_behavior = 0
    e.action_state = 0
    e.action_ticks_counter = 0
    e.death_timer = 0
    e.hit_state = 0

    e:set_sca(config.radius, 1530, 0, -1530, 0)

    -- Two ground quads are built here in the original: the 700x900 dark body
    -- shadow (the per-frame fade sprite) and a 400x400 blood-tinted quad the
    -- severed-limb tail draws at the joint position. The port models the body
    -- shadow; the limb quad keeps its flag/position contract and stays with
    -- the joint-object renderer deferral.
    e:set_shadow_offset(0, 0, 0)
    e.shadow_half_x = 700
    e.shadow_half_z = 900
    e.shadow_tint = 0x00808080

    e:reset_joints()

    -- Health: two draws, the first indexes the table, the second rolls the
    -- 0..21 subtraction (17..99 HP). The attract demo's seed-based variant is
    -- not modelled (the port has no attract demo).
    local health_base = HEALTH[(e:random() & 0xF) + 1]
    local health_variance = e:random() % 22
    e.health = health_base - health_variance

    if (e.behavior_flags & 0x0F) == 5 then
        e.action_state = 2
    end

    e.zombie_move_word = 0
    e.bob_speed = 0
    e.reaction_timer = e.reaction_timer & ~0xFF
    e.splatter_flag = 0
    e.dir_control_flags = 0
    e.zombie_unk_179 = 0
    e.action_speed = 0

    e.hit_threshold = threshold_from_table(e, THRESHOLD_NORMAL, THRESHOLD_HARD)
    e.stagger_timer = STAGGER[(e.rand_seed & 0x1F) + 1]
    e.zombie_unk_178 = 0
    e.behavior_step = 0
    e.move_speed_byte = 45
    e.turn_speed = 24
    e.action_counter = 0
    e.internal_timer = 0
    e.blend_counter = 0

    e.animation_id = ANIM_ID[(e.behavior_flags & 0x0F) + 1]
    e.animation_frame_id = 0
    e.timing_control = 0
    e:advance_anim(0x400)

    if (e.behavior_flags & FLAG_LAYING_DOWN) ~= 0 then
        e.pos_y = 1
    end

    if e.behavior_flags == 6 then
        e.status_flags = e.status_flags | 0x0A
        e.health = -1
        e.state = STATE_DIE
        e.ignore = 1
        e.action_behavior = 4
        e.action_state = 0
    end

    if e.behavior_flags == 10 then
        e.status_flags = e.status_flags | 0x04
        for joint = 9, 14 do
            e:set_joint_flag(joint, e:joint_flag(joint) & 0xFE)
        end
    end

    if (e.behavior_flags & FLAG_VOMITING) ~= 0 then
        e.state = STATE_ACTION_UPDATE
    end
end

-- ---------------------------------------------------------------------------
-- State 1: the idle/behaviour dispatcher
-- ---------------------------------------------------------------------------

state_check = function(e)
    if (e.behavior_flags & FLAG_SCD_CONTROLLED) ~= 0 then
        return
    end

    -- The behaviour view of the states block: behaviour[k] = STATES[10 + k].
    local behavior = STATES[(e.behavior_flags & 0x0F) + 11]
    if behavior then
        behavior(e)
    end

    e.status_flags = e.status_flags & 0x1F

    if (e.behavior_flags & FLAG_LAYING_DOWN) == 0
        and (e.action_speed & 0x80) == 0 then
        e.status_flags = e.status_flags | STATUS_ALIGNED
        e:check_alert_range(3000)
    end

    e:check_visual_range(4500)

    local _, py, _ = e:player_pos()
    local vertical = py - e.pos_y
    if vertical < -100 or vertical > 100 then
        e.status_flags = e.status_flags & 0x1F
        if vertical < 0 then
            e.status_flags = e.status_flags | 0x20
        else
            e.status_flags = e.status_flags | 0x80
        end
    end

    if e.behavior_flags == (FLAG_LAYING_DOWN | 0x05) or e.behavior_flags == 5 then
        e.status_flags = e.status_flags & 0x1F
        e:check_visual_range(4000)
    end

    if (e.behavior_step & 0x04) ~= 0 then
        e.status_flags = e.status_flags | STATUS_ALIGNED
    end
end

-- ---------------------------------------------------------------------------
-- State 2: the damage reaction
-- ---------------------------------------------------------------------------

damaged = function(e)
    if e.ignore == 0 then
        local beh_type = e.behavior_flags & 0x0F

        -- A hit while falling restores the mirrored state and drops the latch.
        if (e.behavior_step & 0x04) ~= 0 then
            e.state_word = e.state_mirror
            e.hit_state = 0
            return
        end

        -- Poise spent: the benddown reaction owns the zombie outright.
        if (e.action_speed & 0x80) ~= 0 then
            e.state = STATE_IDLE
            e.ignore = 1
            e.action_behavior = 7
            e.action_state = 3
            e.blend_counter = 0
            e.hit_state = 1
            if (e.behavior_step & 1) == 0 then
                e:play_enemy_sound(9)
            end
            return
        end

        -- The bullet counter spends against the hit threshold.
        if (e.hit_state & 0x78) == 0x08 then
            e.action_speed = e.action_speed + 1
            if s8(e.hit_threshold) <= s8(e.action_speed) then
                if (e.behavior_flags & FLAG_LAYING_DOWN) == 0 then
                    e.action_speed = 0x80
                    e.hit_threshold =
                        threshold_from_table(e, THRESHOLD_NORMAL, THRESHOLD_HARD)
                end
            end
        end

        -- Sustained damage spends the stagger budget (pre-decrement test).
        if (e.hit_state & 0x78) == 0x10 and e.action_state == 0 then
            e.stagger_timer = e.stagger_timer - 1
            if e.stagger_timer == 0 and (e.behavior_flags & FLAG_LAYING_DOWN) == 0 then
                e.action_speed = 0x80
            end
        end

        e.action_behavior = DAMAGE_ACTION[(e.hit_state & 7)
            + (e.behavior_flags & FLAG_LAYING_DOWN) * 3 + 1]

        -- The hit direction adds the turn's bit 10: a hit from the side picks
        -- the side-push row.
        if ((e.hit_state - 1) & 0x02) ~= 0 then
            local px, _, pz = e:player_pos()
            local angle = e:turn_toward_target(px, pz, 1024)
            e.action_behavior = e.action_behavior + ((angle >> 10) & 1)
        end

        -- An eating zombie always bends down (behaviour 7/5), whatever the
        -- table picked; only the sub-state is gated on the clip.
        if beh_type == 7 or beh_type == 5 then
            e.action_behavior = 7
            if e.animation_id == 13 then
                e.action_state = 1
            end
        end

        e.ignore = 1
    end

    local behavior = DAMAGE_ACTIONS[e.action_behavior + 1]
    if behavior then
        behavior(e)
    end
end

-- ---------------------------------------------------------------------------
-- State 3: death
-- ---------------------------------------------------------------------------

die = function(e)
    if e.ignore == 0 then
        e.ignore = 1
        e.action_behavior = 0

        if (e.behavior_flags & FLAG_LAYING_DOWN) ~= 0
            or (e.action_speed & 0x80) ~= 0
            or e.behavior_flags == 5 then
            e.action_behavior = 2
        end

        if (e.hit_state & 7) == 4 then
            local px, _, pz = e:player_pos()
            local angle = e:turn_toward_target(px, pz, 1024)
            if s16(angle) == 0 then
                e.action_behavior = 1
            end
        end

        if e.action_behavior == 1
            and (e:joint_flag(2) & 0x40) ~= 0
            and (e.rand_seed & 1) ~= 0 then
            e.action_behavior = 3
        end

        e.behavior_step = e.behavior_step & ~0x04
        e:raise_death_event()
    end

    if e.action_behavior == 0 then
        e.animation_id = 8
        e.move_speed_current = 20
        if e.timing_control == 1 then
            if e.animation_frame_id == 8 then
                e:play_enemy_sound(1)
            end
            if e.animation_frame_id == 0x1A then
                e:play_enemy_sound(0)
            end
        end
        zombie_dead_animation(e)
        e:move(0, e.move_speed_current)
    elseif e.action_behavior == 1 then
        e.animation_id = 10
        e.move_speed_current = 20
        if e.animation_frame_id == 18 and e.timing_control == 1 then
            e:play_enemy_sound(0)
        end
        if e.animation_frame_id < 5 then
            e.move_speed_current = e.move_speed_current + 90
        end
        zombie_dead_animation(e)
        e:move(2048, e.move_speed_current)
    elseif e.action_behavior == 2 then
        e.animation_id = 16
        zombie_dead_animation(e)
    elseif e.action_behavior == 3 then
        magnum_shot_pushback(e)
    end
end

-- `zombie_dead_animation`: the corpse tail of every death. Runs the clip out,
-- holds the body for 70 frames while its shadow shrinks into the pool, raises
-- the room event, and - the one place a dead zombie gets back up - rolls the
-- 1-in-4 revival.
zombie_dead_animation = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.death_timer = 70
        e.blend_counter = 3
        e.hit_state = 1
        -- The moan is skipped for a body whose head is already blown apart.
        if (e.behavior_step & 1) == 0 and (e:joint_flag(2) & 0xCC) == 0 then
            e:play_enemy_sound(5)
        end
        st = 1
    end

    if st == 1 then
        local adv = e:advance_anim(0x400) and 1 or 0
        e.action_state = e.action_state + adv
        return
    end

    if st == 2 then
        local saved_angle = e.angle
        local saved_x, saved_y, saved_z = e.pos_x, e.pos_y, e.pos_z

        local blocked = e:two_point_probe(-800, 0, 800, 0)

        e.pos_x, e.pos_y, e.pos_z = saved_x, saved_y, saved_z
        e.angle = saved_angle

        -- The revival: not scripted, behaviour byte not 4, head intact, not
        -- the 2F dining room, the plain fall clip, clear floor, a 1-in-4 roll
        -- and no screen-intensity ramp.
        if (e.behavior_flags & 0x40) == 0
            and e.behavior_flags ~= 0x04
            and (e:joint_flag(2) & 0xCC) == 0
            and not (e.stage == 1 and e.room == 2)
            and e.animation_id == 8
            and blocked == 0
            and (e.rand_seed & 3) == 0
            and not e:flag_bit(5, 16) then
            e.behavior_flags = 3
            e.health = 1
            e.state = STATE_IDLE
            e.ignore = 0
            e.action_behavior = 0
            e.action_state = 0
            e.hit_state = 0
            e.pos_y = 1
            e.behavior_step = e.behavior_step & ~0x04
            e.status_flags = e.status_flags & 0x1F
            return
        end

        e.health = -1
        e.shadow_tint = 0x00FFFF50
        e:adjust_shadow_size(-100, -100)
        e:raise_death_event()
        e.action_state = 3
        e.status_flags = e.status_flags | 0x0E
        st = 3
    end

    if st == 3 then
        e.move_speed_current = 0
        e:adjust_shadow_size(6, 6)
        e.death_timer = e.death_timer - 1
        if e.death_timer == 0 then
            e.action_state = 4
            return
        end
        return
    end

    if st == 4 then
        e.move_speed_current = 0
        if (e.behavior_flags & 0x40) ~= 0 then
            e:raise_scd_flag()
            e.action_behavior = 0
            e.action_state = 0
        end
        return
    end
end

-- ---------------------------------------------------------------------------
-- State 5: the attack FSM
-- ---------------------------------------------------------------------------

local function attack_withdraw(e)
    e:apply_anim_vertex()
    if e:advance_anim(0x400) then
        e.state = STATE_DAMAGED
        e.ignore = 1
        e.action_behavior = 6
        e.action_state = 0
        e.status_flags = e.status_flags & 0xF5
        e.hit_state = 1
    end
end

local function attack_head_bite(e)
    e:apply_anim_vertex()
    local adv = e:advance_anim(0x400) and 1 or 0
    e.action_state = e.action_state + adv

    local keyframe = ATTACK_ANIM[e.attacking_direction * 3 + 3]
    if keyframe == e.animation_frame_id then
        e.action_state = 10
        e:arm_joint_effect(2, 0x1E, 0, 3)
        e:spawn_joint_effect_at(3, 0, 2, 0, 0, 0, 0)
        e:spawn_effect(4, 0, 500, 0, 0, 0x800, 0)
        e:spawn_effect(4, 1, 500, 0, 0, 0x5E8, 0)
        e:spawn_effect(4, 2, 500, 0, 0, 0x9F4, 0)
        e:spawn_effect(4, 4, 500, 0, 0, 0xB84, 0)
        e:spawn_effect(4, 2, 500, 0, 0, 0xDB8, 0)
        e:tint_joint(3, 0x30, 0x80820, 0x606060)
        e:tint_joint(6, 0x30, 0x80820, 0x606060)
        e:play_enemy_sound(6)
    end
end

local function attack_vomit(e)
    e:apply_anim_vertex()
    local adv = e:advance_anim(0x400) and 1 or 0
    e.action_state = e.action_state + adv

    local keyframe = ATTACK_ANIM[e.attacking_direction * 3 + 3]
    if keyframe == e.animation_frame_id then
        e:set_joint_flag(2, e:joint_flag(2) | 0x88)
        e:tint_joint(2, 0x30, 0x80820, 0x606060)
        -- The head joint's rotation words become the blood scratch: velocity
        -- X/Y, counter and flags; the direction's Z word stays as posed.
        e:set_joint_blood(2, 0xFED4, 0xFA, 0, 0)
        e:spawn_joint_effect_at(0, 0, 2, 0, 0, 0, 0)
    end

    if (e:joint_flag(2) & 0x40) ~= 0 then
        e.splatter_flag = 1
        e.bob_speed = 0
    end

    if e.player_animation_frame_id == 6 then
        e:play_enemy_sound(7)
    end
end

local function attack_headless_death(e)
    e.shadow_tint = 0x00FFFF50
    e:raise_death_event()
    e.death_timer = 0x46
    e.state = STATE_DIE
    e.ignore = 1
    e.action_behavior = 2
    e.action_state = 3
    e.status_flags = e.status_flags | 0x0E
    e.health = -1
    e.hit_state = 1
end

attack = function(e)
    local st = e.action_state

    if st == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.action_state = 1
        e.blend_counter = 3
        e.status_flags = e.status_flags | (STATUS_ACTIVE | STATUS_DEAD)
        e:play_enemy_sound(4)

        -- standing/not-facing, standing/facing, laying/not-facing, laying/facing.
        e.attacking_direction = (e.behavior_flags & FLAG_LAYING_DOWN)
            + (e:player_facing_entity() and 1 or 0)

        e.animation_id = ATTACK_ANIM[e.attacking_direction * 3 + 1]
        e.player_attack_anim = ATTACK_ANIM[e.attacking_direction * 3 + 2]
        e.player_attack_direction = ATTACK_DIR_OFFSET[e.attacking_direction + 1]

        e.hit_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0

        e:snap_grab()
        e.player_attacked = 1
        e.player_state = 5
        e.player_anim_frame_id = 0
        e.player_action_behavior = 0
        e.player_action_state = 0
        e.player_angle = e.angle
        return
    end

    if st == 1 then
        e:apply_anim_vertex()
        local adv = e:advance_anim(0x400) and 1 or 0
        e.action_state = e.action_state + adv
        return
    end

    if st == 2 then
        e.action_state = 3
        e.animation_id = e.animation_id + 1
        e.timing_control = 0
        e.action_ticks_counter = 0
        e.attack_timer = 105
        -- A player already mashing the attack cuts the bite to 30 frames.
        if e.player_attack_timer ~= 0 then
            e.attack_timer = 30
        end
        return
    end

    if st == 3 then
        -- Value-before-increment: the first bite lands on the entry frame.
        local tick = s16(e.action_ticks_counter)
        e.action_ticks_counter = tick + 1
        if cmod(tick, 19) == 0 then
            local damage
            if second_playthrough(e) then
                damage = DAMAGE_HARD[(e.behavior_flags & 0x0F) + 1]
            else
                damage = DAMAGE_NORMAL[(e.behavior_flags & 0x0F) + 1]
            end
            e.player_health = e.player_health - damage
            e:play_enemy_sound(3)

            e:spawn_world_effect(0, 0,
                e:joint_world_x(2), e:joint_world_y(2), e:joint_world_z(2),
                e.player_angle + 2048)

            if e.player_health < 0 and (e.attacking_direction & 2) ~= 0 then
                e.player_health = 1
            end
        end

        e:apply_anim_vertex()
        e:advance_anim(0x400)

        local reduce = e:mash_reduce()
        e.attack_timer = e.attack_timer - (reduce + 1)
        if s8(e.attack_timer) < 0 then
            e.action_state = 4
            e.player_action_state = 3
        end

        if e.player_health < 0 then
            e.state = STATE_IDLE
            e.ignore = 1
            e.action_behavior = 5
            e.action_state = 0
            e.action_behavior = e.action_behavior
                + ((e.behavior_flags & FLAG_LAYING_DOWN) ~= 0 and -5 or 0)

            e.player_state = 1
            e.player_action_behavior = 200

            e:spawn_joint_effect_at(0, 0, 2, 0, 0, 0, 0x200)
            -- The player's death-pose joint tints swap the current entity to
            -- the player in the original; the joint colour pipeline is the
            -- documented renderer deferral.
            e:tint_joint(0, 0x30, 0x80820, 0x606060)
        end
        return
    end

    if st == 4 then
        e.action_state = 5
        e.animation_id = e.animation_id + 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3

        if (e.behavior_flags & FLAG_LAYING_DOWN) == 0 then
            attack_withdraw(e)
        elseif e.player_character == 0 or e.attacking_direction ~= 2 then
            e.action_state = 6
            attack_head_bite(e)
        else
            e.action_state = 8
            attack_vomit(e)
        end
        return
    end

    if st == 5 then
        attack_withdraw(e)
    elseif st == 6 then
        attack_head_bite(e)
    elseif st == 8 then
        attack_vomit(e)
    elseif st == 7 or st == 9 then
        attack_headless_death(e)
    elseif st == 10 then
        e:apply_anim_vertex()
        if e:advance_anim(0x400) then
            e.action_state = 7
        end
    end
end

-- ---------------------------------------------------------------------------
-- State 8: the SCD action table
-- ---------------------------------------------------------------------------

zombie_aggresive_roar = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.move_speed_current = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 2
        e.blend_counter = 3
        e:play_enemy_sound(4)
        st = 1
    end

    if st == 1 then
        if e:advance_anim(1024) then
            e.action_state = 2
            e.move_speed_current = 0x23
            e.animation_frame_id = 0
            e.timing_control = 0
            e.animation_id = 3
            e.blend_counter = 3
        end
        e:move(0, e.move_speed_current)
        return
    end

    if st == 2 then
        e:rotate_toward_target(e.unk_c6, e.unk_c8, 64)

        if e.animation_frame_id == 8 then
            e:play_enemy_sound(1)
        end
        if e.animation_frame_id == 0x1D then
            e:play_enemy_sound(1)
        end

        e:advance_anim(0x400, (e.flags & 1) ~= 0)
        if e:xz_distance_to(e.unk_c6, e.unk_c8) < 150 then
            e.action_state = 3
        end
        e:move(0, e.move_speed_current)
        return
    end

    if st == 3 then
        e:raise_scd_flag()
        if (e.collision_flags & 0x80) == 0 then
            e.action_behavior = 0
            e.action_state = 0
        else
            e.action_state = 2
        end
        e:move(0, e.move_speed_current)
    end
end

zombie_headshot = function(e)
    e.animation_id = 8
    e.move_speed_current = 0x14

    if e.action_state == 0 then
        local away = s16(e.player_angle - e.angle)
        e:arm_joint_effect(2, 30, 2, 3)
        e:spawn_effect(3, 0, 100, -0xA3C, 0, away + 0x800, 0)
        e:spawn_joint_effect_at(0, 3, 2, 0, -600, 0, 0)
        e:spawn_effect(4, 0, 0, -600, 0, away + 1536, 0)
        e:spawn_effect(4, 2, 0, -600, 0, away + 1792, 0)
        e:spawn_effect(4, 3, 0, -600, 0, away + 2304, 0)
        e:play_enemy_sound(6)
    end

    if e.animation_frame_id == 8 then
        e:play_enemy_sound(1)
    end
    if e.animation_frame_id == 26 then
        e:play_enemy_sound(0)
    end

    zombie_dead_animation(e)
    e:move(0, e.move_speed_current)
end

zombie_scd_dying = function(e)
    e.animation_id = 10
    e.move_speed_current = 0x14

    if e.animation_frame_id == 0x12 then
        e:play_enemy_sound(2)
    end
    if e.animation_frame_id < 5 then
        e.move_speed_current = e.move_speed_current + 90
    end

    zombie_dead_animation(e)
    e:move(2048, e.move_speed_current)
end

local function scd_vomiting_common(e, anim_id)
    e.animation_id = anim_id
    short_push_back(e)
    if (e.action_state & 2) ~= 0 then
        e:raise_scd_flag()
        e.action_behavior = 0
        e.action_state = 0
    end
end

zombie_scd_vomiting = function(e)
    scd_vomiting_common(e, 4)
end

zombie_scd_vomiting2 = function(e)
    scd_vomiting_common(e, 5)
end

action_update = function(e)
    local behavior = e.action_behavior
    if behavior < 14 then
        local handler = SCD_ACTIONS[behavior + 1]
        if handler then
            handler(e)
        end
    end
end

-- ---------------------------------------------------------------------------
-- The chase behaviours
-- ---------------------------------------------------------------------------

chase_player = function(e)
    if (e.behavior_step & 0x04) == 0 and (e.action_speed & 0x80) == 0 then
        if e:angular_view_and_distance(700, 1500) ~= 0 then
            if e:line_of_sight() == 0 and e.player_attacked == 0 then
                local px, _, pz = e:player_pos()
                e.angle = e:angle_to(px, pz)
                e.state = STATE_ATTACK
                e.ignore = 0
                e.action_state = 0
                attack(e)
                return
            end
        end
    end

    if e.ignore == 0 then
        update_player_distance(e)
    end
    update_zombie_action(e)
end

pushed_back = function(e)
    if e:angular_view_and_distance(512, 2200) ~= 0 then
        if e:line_of_sight() == 0 and e.player_attacked == 0 then
            local px, _, pz = e:player_pos()
            e.angle = e:angle_to(px, pz)
            e.state = STATE_ATTACK
            e.ignore = 0
            e.action_state = 0
            attack(e)
            return
        end
    end

    if e.ignore == 0 then
        update_player_distance(e)
    end
    pushback_action(e)
end

random_chase = function(e)
    if (e.behavior_step & 0x04) == 0
        and (e.action_speed & 0x80) == 0
        and (e.collision_flags & 0x08) == 0 then
        if e:angular_view_and_distance(700, 1500) ~= 0 then
            if e:line_of_sight() == 0 and e.player_attacked == 0 then
                local px, _, pz = e:player_pos()
                e.angle = e:angle_to(px, pz)
                e.state = STATE_ATTACK
                e.ignore = 0
                e.action_state = 0
                attack(e)
                return
            end
        end
    end

    if e.ignore == 0 then
        update_player_distance(e)
    end
    update_zombie_action(e)
end

eating = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = e.rand_seed & 0x1F
        e.timing_control = 0
        e.blend_counter = 3
        e.animation_id = 0x1E
        e.action_ticks_counter = (e.rand_seed & 0xF) + 0x2D
        st = 1
    end

    if st == 1 then
        if e:advance_anim(0x400) then
            e.animation_id = (e.rand_seed & 1) ~= 0 and 0x1D or 0x1E
        end
        if e.animation_frame_id == 0x19 then
            e:spawn_effect(0, 0, 800, -300, 0, 0, 0)
            e:play_enemy_sound(3)
        end
        if player_distance(e) < 3000 then
            e.action_state = 2
            return
        end
        return
    end

    if st == 2 then
        e.action_state = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0x0D
        st = 3
    end

    if st == 3 then
        if e:advance_anim(0x400) then
            e.behavior_flags = 0
            if e.stage == 4 and e.room == 5 then
                e.behavior_flags = 4
            end
            hand_to_behavior_3(e)
            e.pos_y = 0
        end
    end
end

-- ---------------------------------------------------------------------------
-- The move behaviours
-- ---------------------------------------------------------------------------

update_player_distance = function(e)
    if e.zombie_move_word ~= 0 then
        local px, _, pz = e:player_pos()
        e:zone_path_update(px, pz)
    end

    e.player_distance = player_distance(e)

    local behavior = e.behavior_flags & 0x0F
    if behavior < 12 then
        -- move[k] is the block's [16 + k] view.
        local handler = DAMAGE_ACTIONS[behavior + 17]
        if handler then
            handler(e)
        end
    end
end

update_zombie_action = function(e)
    local behavior = e.action_behavior
    if behavior <= 12 then
        -- action[k] is the block's [8 + k] view, so 8..12 alias move[0..4].
        local handler = DAMAGE_ACTIONS[behavior + 9]
        if handler then
            handler(e)
        end
    end
end

-- `zombie_walk1`: waypoint follow with the wander-turn jitter.
zombie_walk1 = function(e)
    local px, _, pz = e:player_pos()
    local entry_behavior = e.action_behavior
    -- The raw pathfinder result, or the kept bit when no call ran this frame.
    local path_result = e.path_bit

    if e.zombie_move_word == 0 then
        path_result = e:pathfind_update(px, pz)
        if (path_result & 0xFE) == 0 then
            e.is_moving = e.is_moving & 0xFE
            e.zombie_move_word = e.zombie_move_word | (path_result & 1)
        end
    end

    if (e.action_speed & 0x80) ~= 0 then
        e.state = STATE_IDLE
        e.ignore = 1
        e.action_behavior = 7
        e.action_state = 0
        return
    end

    if e.action_behavior ~= 0 and e.action_behavior ~= 1 then
        e:wander_turn(e.zombie_wander_word, e.turn_speed, 60)
    end

    if e.player_distance < 3000 then
        local step = e:turn_toward_target(px, pz, 1024)
        if s16(step) ~= 0 then
            e.ignore = 1
            e.action_behavior = 3
            e.action_state = 0
            return
        end
    end

    if e.zombie_move_word ~= 0 and e.player_attacked == 0 then
        if entry_behavior ~= 2 then
            e.action_state = 0
            e.blend_counter = 3
        end
        e.action_behavior = 2
        e.ignore = 0
    end

    if (e.player_attacked & 0x80) ~= 0 then
        if (path_result & 1) ~= 0 then
            if entry_behavior ~= 2 then
                e.action_state = 0
                e.blend_counter = 3
            end
            e.action_behavior = 2
            e.ignore = 0
        end
        if e.player_distance < 1200 then
            local step = e:turn_toward_target(px, pz, 712)
            if s16(step) == 0 then
                e.ignore = 1
                e.action_behavior = 4
                e.action_state = 0
            end
        end
    end
end

zombie_check_player_distance = function(e)
    e.action_ticks_counter = e.action_ticks_counter + 1

    local angle = e:turn_toward_target(e.player_pos_x, e.player_pos_z, 4)
    e.angle = e.angle + angle

    local px, _, pz = e:player_pos()
    local result = e:pathfind_update(px, pz)
    if (result & 1) == 0 or e.player_distance > 8999 then
        if e.action_behavior ~= 0 or e.player_distance < 3000 then
            e.behavior_flags = e.behavior_flags - 1
            if e.behavior_flags == 7 then
                e.behavior_flags = 4
            end
        end
    else
        e.behavior_flags = e.behavior_flags - 1
        if e.behavior_flags == 7 then
            e.behavior_flags = 4
        end
    end
end

zombie_slow_walk_alt = function(e)
    local dx = e.player_pos_x - e.pos_x
    local dz = e.player_pos_z - e.pos_z
    local abs_dx = (dx < 0) and -dx or dx
    local abs_dz = (dz < 0) and -dz or dz
    -- The original publishes the waypoint's Manhattan distance into the
    -- shared collision scratch; nothing reads it back here.
    local waypoint_distance = abs_dz - (dx >> 31) + abs_dx
    local entry_behavior = e.action_behavior

    if e.zombie_move_word == 0 then
        local px, _, pz = e:player_pos()
        local result = e:pathfind_update(px, pz)
        if (result & 0xFE) == 0 then
            e.is_moving = e.is_moving & 0xFE
            e.zombie_move_word = e.zombie_move_word | (result & 1)
        end
        return
    end

    if entry_behavior ~= 1 then
        e.action_state = 0
        e.blend_counter = 3
    end
    e.action_behavior = 1
    e.ignore = 0
end

zombie_idling = function(e)
    if e.action_behavior ~= 0 then
        e.action_behavior = 0
        e.action_state = 0
    end
    local px, _, pz = e:player_pos()
    e:pathfind_update(px, pz)
end

-- `zombie_walk2`: waypoint follow with the player-awareness ladder.
zombie_walk2 = function(e)
    local px, _, pz = e:player_pos()
    local entry_behavior = e.action_behavior
    -- The raw pathfinder result, or the kept bit when no call ran this frame.
    local path_result = e.path_bit

    if e.zombie_move_word == 0 then
        path_result = e:pathfind_update(px, pz)
        if (path_result & 0xFE) == 0 then
            e.is_moving = e.is_moving & 0xFE
            e.zombie_move_word = e.zombie_move_word | (path_result & 1)
        end
    end

    if e.action_behavior ~= 0 and e.action_behavior ~= 1 then
        e:wander_turn(e.zombie_wander_word, e.turn_speed, 60)
    end

    if (e.action_speed & 0x80) ~= 0 then
        e.state = STATE_IDLE
        e.ignore = 1
        e.action_behavior = 7
        e.action_state = 0
        return
    end

    if e.player_distance < 3000 then
        local angle = e:turn_toward_target(px, pz, 0x400)
        if s16(angle) ~= 0 then
            e.ignore = 1
            e.action_behavior = 3
            e.action_state = 0
            return
        end
    end

    if (e.collision_flags & 8) ~= 0 and e.player_distance < 2000 then
        local angle = e:turn_toward_target(px, pz, 0x200)
        if s16(angle) == 0 then
            e.ignore = 1
            e.action_behavior = 6
            e.action_state = 0
            if e.player_attacked ~= 0 then
                e.action_state = 2
            end
            return
        end
    end

    if e.zombie_move_word ~= 0 and e.player_attacked == 0 then
        if entry_behavior ~= 2 then
            e.action_state = 0
            e.blend_counter = 3
        end
        e.action_behavior = 2
        e.ignore = 0
    end

    if (e.player_attacked & 0x80) ~= 0 then
        if (path_result & 1) ~= 0 then
            if entry_behavior ~= 2 then
                e.action_state = 0
                e.blend_counter = 3
            end
            e.action_behavior = 2
            e.ignore = 0
        end
        if (e.collision_flags & 8) == 0 and e.player_distance < 1200 then
            local angle = e:turn_toward_target(px, pz, 0x2C8)
            if s16(angle) == 0 then
                e.ignore = 1
                e.action_behavior = 4
                e.action_state = 0
            end
        end
    end
end

-- ---------------------------------------------------------------------------
-- The action behaviours (the block's [8 + k] view)
-- ---------------------------------------------------------------------------

zombie_idle = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0
        e.action_ticks_counter = (e.rand_seed & 0x7F) + 50
        e.blend_counter = 3
        st = 1
    end

    if st == 1 then
        e:advance_anim(0x400)
        -- `DEC word` then compare: test-after-decrement.
        e.action_ticks_counter = e.action_ticks_counter - 1
        if e.action_ticks_counter == 0 then
            e.action_behavior = 1
            e.action_state = 0
        end
        return
    end

    if st == 2 then
        e.action_state = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0
        e.blend_counter = 3
        e.action_ticks_counter = (e.rand_seed & 0xF) + 8
        st = 3
    end

    if st == 3 then
        e:advance_anim(0x400)
        if tick_expired(e) then
            e.action_state = 4
            e.action_ticks_counter = (e.rand_seed & 0xF) + 8
        end
        e.angle = e.angle + 12
        return
    end

    if st == 4 then
        e:advance_anim(0x400)
        if tick_expired(e) then
            e.action_state = 5
            e.action_ticks_counter = (e.rand_seed & 0xF) + 4
        end
        e.angle = e.angle - 24
        return
    end

    if st == 5 then
        e:advance_anim(0x400)
        if tick_expired(e) then
            e.action_state = 6
            e.action_ticks_counter = (e.rand_seed & 0xF) + 8
        end
        e.angle = e.angle - 32
        return
    end

    if st == 6 then
        e:advance_anim(0x400)
        if tick_expired(e) then
            e.action_state = 7
            e.action_ticks_counter = (e.rand_seed & 0xF) + 4
        end
        e.angle = e.angle + 24
        return
    end

    if st == 7 then
        e.action_behavior = 0
        e.action_state = 0
    end
end

zombie_slow_walk = function(e)
    if e.action_state == 0 then
        e.move_speed_current = 20
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 1
        e.blend_counter = 3
        e.action_ticks_counter = (e.rand_seed & 0x7F) + 300

        -- The wander waypoint: a 5000-unit forward vector rotated by the yaw
        -- plus eight, added to the truncated position.
        local rx, rz = e:rotate_xz(e.angle + 8, 5000, 0)
        e.player_pos_x = (e.pos_x & 0xFFFF) + rx
        e.player_pos_z = (e.pos_z & 0xFFFF) + rz
    end

    if e.timing_control == 1 then
        if e.animation_frame_id == 13 then
            e:play_enemy_sound(1)
        end
        if e.animation_frame_id == 0x15 then
            e:play_enemy_sound(2)
        end
    end

    e:advance_anim(0x400)

    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 then
        e.action_behavior = 0
        e.action_state = 2
    end

    e:wander_turn(e.zombie_wander_word, e.turn_speed, 60)
    if e.animation_frame_id > 15 then
        e:move(0, e.move_speed_current)
    end
end

zombie_chase_walk = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.move_speed_current = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 2
        e.blend_counter = 3
        st = 1
    end

    if st == 1 then
        local adv = e:advance_anim(0x400) and 1 or 0
        e.action_state = e.action_state + adv
    elseif st == 2 then
        e.move_speed_current = e.move_speed_byte
        e.action_state = 3
        e.timing_control = 0
        e.animation_id = 3
        e.blend_counter = 3
        e.action_ticks_counter = (e.rand_seed & 0x7F) + 10
        e.next_turn_timer = 0
        st = 3
    end

    if st == 3 then
        if e.timing_control == 1 then
            if e.animation_frame_id == 8 then
                e:play_enemy_sound(1)
            end
            if e.animation_frame_id == 29 then
                e:play_enemy_sound(1)
            end
        end

        -- Value-before-decrement: when it hits zero, arm a weave.
        local ticks = s16(e.action_ticks_counter)
        e.action_ticks_counter = ticks - 1
        if ticks == 0 then
            e.next_turn_timer = (e.rand_seed & 0x0F) + 0x14
            e.action_ticks_counter =
                s16(e.next_turn_timer) + (e.rand_seed & 0x7F)
        end

        e:advance_anim(0x400)
        e.move_speed_current = 45

        if s16(e.next_turn_timer) ~= 0 then
            e:wander_turn(e.zombie_wander_word,
                (e.turn_speed * -2) & 0xFFFF, 60)
            e.move_speed_current = 10
            e.next_turn_timer = s16(e.next_turn_timer) - 1
        end

        e.player_distance = player_distance(e)

        -- Someone else already has the player: stand still for 60 frames.
        if e.player_attacked ~= 0 and e.player_distance < 800 then
            e.ignore = 1
            e.action_state = e.action_state + 1
            e.action_ticks_counter = 60
            e.animation_frame_id = 0
            e.timing_control = 0
            e.animation_id = 0
            e.blend_counter = 3
            e.move_speed_current = 0
        end
    elseif st == 4 then
        e:advance_anim(0x400)
        e.action_ticks_counter = e.action_ticks_counter - 1
        if e.action_ticks_counter == 0 then
            e.ignore = 0
            e.action_state = 0
        end
    end

    e:move(0, e.move_speed_current)
end

fast_player_facing = function(e)
    if e.action_state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.hit_state = 0
        local px, _, pz = e:player_pos()
        e:turn_toward_target(px, pz, 0x400)
        e.animation_id = 3
        e.action_state = 1
        e.action_ticks_counter = (e.rand_seed & 0x3F) + 0x1E
    end

    local px, _, pz = e:player_pos()
    local step = e:turn_toward_target(px, pz, 0x54)

    local ticks = s16(e.action_ticks_counter)
    e.action_ticks_counter = ticks - 1

    if ticks ~= 0 and step ~= 0 then
        e:advance_anim(0x400)
        e.angle = e.angle + step
        return
    end

    e.state = STATE_IDLE
    e.ignore = 0
    e.action_behavior = 2
    e.action_state = 2
    e.player_pos_x = px & 0xFFFF
    e.player_pos_z = pz & 0xFFFF
end

benddown_and_eat = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.animation_id = 0x0B
    end
    if st == 1 then
        e:advance_anim(0x400)
        e.action_ticks_counter = e.action_ticks_counter - 1
        if e.action_ticks_counter == 0 then
            e.action_state = e.action_state + 1
        end
    end
    if st == 2 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.action_state = e.action_state + 1
        e.blend_counter = 3
        e.animation_id = 0x0B
        st = 3
    end
    if st == 3 then
        if e:advance_anim(0x400) then
            e.action_state = e.action_state + 1
        end
        local px, _, pz = e:player_pos()
        local turn = e:turn_toward_target(px, pz, 0x20)
        e.angle = e.angle + turn
        return
    end
    if st == 4 then
        e.action_state = e.action_state + 1
        e.animation_id = e.animation_id + 1
        e.timing_control = 0
        e.action_ticks_counter = 120
        st = 5
    end
    if st == 5 then
        e:advance_anim(0x400)
        if (e.animation_frame_id & 7) == 0 then
            e:spawn_effect(0, 0, 800, -300, 0, 0, 0)
            e:play_enemy_sound(3)
        end
        if e.animation_frame_id == 0x0E then
            -- The player joint tints and joint-anchored sprays are the
            -- deferred joint-object half; the state contract is the timer
            -- below.
            e:tint_joint(0, 0x30, 0x80820, 0x606060)
        end
        local ticks = s16(e.action_ticks_counter)
        e.action_ticks_counter = ticks - 1
        if ticks == 0 then
            e.action_state = e.action_state + 1
            e.animation_id = e.animation_id + 1
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 3
        end
    end
    if st == 6 then
        if e:advance_anim(0x400) then
            e.state = STATE_IDLE
            e.ignore = 0
            e.action_behavior = 0
            e.action_state = 0
        end
    end
end

turn_towards_player = function(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.animation_id = 3
        e.move_speed_current = 45
        local dir = e.attacking_direction
        e:move(ATTACK_DATA[dir * 3 + 1], e.move_speed_current)
        e.action_ticks_counter = ATTACK_DATA[dir * 3 + 2]
    end

    if e.timing_control == 1 then
        if e.animation_frame_id == 8 then
            e:play_enemy_sound(1)
        end
        if e.animation_frame_id == 0x1D then
            e:play_enemy_sound(1)
        end
    end

    local dir = e.attacking_direction
    e.angle = e.angle - s16(ATTACK_DATA[dir * 3 + 3])
    e:advance_anim(0x400)

    local ticks = s16(e.action_ticks_counter)
    e.action_ticks_counter = ticks - 1
    if ticks == 0 then
        e.ignore = 1
        e.action_behavior = 4
        e.action_state = 0
    end

    e:advance_speed()
end

zombie_vomiting = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 5
        e:spawn_effect(0x20, 0, 500, -2500, 0, 0, 0)
        e:play_enemy_sound(7)
        st = 1
    end

    if st == 1 then
        local adv = e:advance_anim(0x400) and 1 or 0
        e.action_state = e.action_state + adv
        return
    end

    if st == 2 then
        e.action_state = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0
        e.action_ticks_counter = (e.rand_seed & 0x1F) + 20
        e.blend_counter = 3
        st = 3
    end

    if st == 3 then
        e:advance_anim(0x400)
        e.action_ticks_counter = e.action_ticks_counter - 1
        if e.action_ticks_counter == 0 then
            e.ignore = 0
            e.action_behavior = 0
            e.action_state = 0
        end
    end
end

zombie_falldown = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 8
        e.blend_counter = 7
        e.move_speed_current = 20
        e:play_enemy_sound(5)
        e.pos_y = 1
    elseif st == 1 then
        e.behavior_step = e.behavior_step & ~0x04
        e.hit_state = 1
        e.action_speed = e.action_speed | 0x80

        if e.animation_frame_id < 4 then
            e.action_speed = e.action_speed & ~0x80
            e.behavior_step = e.behavior_step | 0x04
            e.hit_state = 0
        end

        if e.timing_control == 1 then
            if e.animation_frame_id == 8 then
                e:play_enemy_sound(1)
            end
            if e.animation_frame_id == 0x1A then
                e:play_enemy_sound(0)
            end
        end

        if e:advance_anim(0x200) then
            e.hit_state = 0
            e.action_ticks_counter = RECOVERY[(e.rand_seed & 0x0F) + 1] * 30
            e.action_state = 2
        end
        e:move(0, e.move_speed_current)
        return
    elseif st == 2 then
        e.action_ticks_counter = e.action_ticks_counter - 1
        if e.action_ticks_counter == 0 then
            e.action_state = 3
            e.blend_counter = 0
            e.hit_state = 1
            e:play_enemy_sound(5)
            return
        end
        return
    elseif st == 3 then
        e.behavior_step = e.behavior_step & ~0x04
        e.hit_state = 1
        e.action_speed = e.action_speed | 0x80

        if e.animation_frame_id > 0x1A then
            e.action_speed = e.action_speed & ~0x7F
            e.behavior_step = e.behavior_step | 0x04
            e.hit_state = 0
        end

        if e:advance_anim(0x200, true) then
            if e.stagger_timer == 0 then
                e.stagger_timer = STAGGER[(e.rand_seed & 0x1F) + 1]
                e.action_speed = e.action_speed & ~0x80
            else
                e.action_speed = 0
            end
            e.hit_state = 0
            hand_to_behavior_3(e)
            e.behavior_step = e.behavior_step & ~0x04
            e.pos_y = 0
        end
        e:move(0x800, e.move_speed_current)
        return
    end
end

-- ---------------------------------------------------------------------------
-- The damage behaviours (the block's [0 + k] view)
-- ---------------------------------------------------------------------------

short_push_back = function(e)
    if (e.behavior_flags & FLAG_VOMITING) == 0 then
        e.animation_id = 5 - (e.action_behavior == 0 and 1 or 0)
    end
    e.move_speed_current = 15

    if e.action_state == 0 then
        e.action_state = 1
        e.move_speed_current = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3

        if (e.behavior_flags & FLAG_VOMITING) ~= 0 then
            e:spawn_effect(0, 0, 100, -2620, 0, 0, 0)
        end

        if (e.hit_state & 7) == 3 and (e:joint_flag(4) & 4) == 0 then
            local rand_bit = (0x40 >> (e.rand_seed & 7)) & 1
            if rand_bit ~= 0 then
                e:set_joint_flag(4, e:joint_flag(4) | 12)
                e:spawn_joint_effect(0, 0, 4, 0)
                local jx, jy, jz =
                    e:joint_world_x(4), e:joint_world_y(4), e:joint_world_z(4)
                e:spawn_world_effect(0, 0, jx, jy, jz, 0)
                e:set_joint_flag(5, e:joint_flag(5) | 0x10)
                e:tint_joint(4, 0x30, 0x80820, 0x606060)
                e:tint_joint(3, 0x30, 0x80820, 0x606060)
            end
        end

        if e.internal_timer == 0 then
            e:play_enemy_sound(9)
            e.internal_timer = 150
        end
    end

    local done = e:advance_anim(0x400)
    if done then
        e.action_state = e.action_state + 1

        if (e.behavior_flags & FLAG_VOMITING) == 0 then
            if (e.behavior_flags & 0x0F) == 5 then
                e.behavior_flags = 0
            end
            hand_to_behavior_3(e)
        end

        local px, _, pz = e:player_pos()
        e.player_pos_x = px & 0xFFFF
        e.player_pos_z = pz & 0xFFFF
        e.hit_state = 0
    end

    e:check_special_weapon()

    if (e.hit_state & 1) ~= 0 then
        e.action_counter = e.action_counter + 1
        if e.action_counter == 2 then
            e.move_speed_byte = 20
            e.turn_speed = 14
        end
    end

    e:move(0, e.move_speed_current)
end

push_and_stagger = function(e)
    local speed_arg
    if e.action_behavior == 2 then
        e.animation_id = 7
        speed_arg = 0x800
    else
        e.animation_id = 6
        speed_arg = 0
    end
    e.move_speed_current = 15

    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.animation_id = 6

        if e.internal_timer == 0 then
            e:play_enemy_sound(9)
            e.internal_timer = 150
        end
    end

    local done = e:advance_anim(1024)
    if done then
        if (e.behavior_flags & 0x0F) == 5 then
            e.behavior_flags = 0
        end
        hand_to_behavior_3(e)
        local px, _, pz = e:player_pos()
        e.player_pos_x = px & 0xFFFF
        e.player_pos_z = pz & 0xFFFF
        e.hit_state = 0
    else
        if e.animation_frame_id < 5 then
            e.move_speed_current = e.move_speed_current + 90
        end
        if e.animation_frame_id == 0x14 then
            e.hit_state = 0
        end
    end

    e:check_special_weapon()
    e:move(speed_arg, e.move_speed_current)
end

push_and_drop = function(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 15
        if e.internal_timer == 0 then
            e:play_enemy_sound(9)
            e.internal_timer = 150
        end
    end

    if e:advance_anim(1024) then
        e.state = STATE_IDLE
        e.ignore = 0
        e.action_behavior = 1
        e.action_state = 0
        local px, _, pz = e:player_pos()
        e.player_pos_x = px & 0xFFFF
        e.player_pos_z = pz & 0xFFFF
        e.hit_state = 0

        if (e.collision_flags & 8) == 0 then
            if (e.behavior_flags & 0x20) == 0
                and (e.behavior_flags & 0x0F) == 3 then
                e.behavior_flags = 2
            end
        end
    end

    e:check_special_weapon()
end

explode_leg_and_drop = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.death_timer = 70
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.animation_id = 8
        e.move_speed_current = 0x14

        e:arm_joint_effect(10, 0x14, 5, 3)
        e:arm_joint_effect(11, 0x14, 5, 3)
        e:spawn_joint_effect(0, 0, 9, 0)
        e:spawn_joint_effect(0, 0, 10, 0)
        e:spawn_joint_effect(0, 0, 13, 0)
        e:play_enemy_sound(6)

        if e.internal_timer == 0 then
            e:play_enemy_sound(9)
            e.internal_timer = 150
        end

        if e.health < 0 then
            e.health = 1
        end
        e.pos_y = 1
        st = 1
    end

    if st == 1 then
        local adv = e:advance_anim(1024) and 1 or 0
        e.action_state = e.action_state + adv
        if e.timing_control == 1 then
            if e.animation_frame_id == 8 then
                e:play_enemy_sound(1)
            end
            if e.animation_frame_id == 0x1A then
                e:play_enemy_sound(0)
            end
        end
        e:move(0, e.move_speed_current)
        return
    end

    if st == 2 then
        local saved_x, saved_y, saved_z = e.pos_x, e.pos_y, e.pos_z

        e.status_flags = e.status_flags | 0x0E
        local blocked = e:two_point_probe(-800, 0, 800, 0)

        e.pos_x, e.pos_y, e.pos_z = saved_x, saved_y, saved_z

        if e.behavior_flags ~= 4
            and not (e.stage == 1 and e.room == 2)
            and blocked == 0
            and e.health > 0 then
            e.behavior_flags = ((e.rand_seed & 3) == 0 and 1 or 0) + 2
            e.state = STATE_IDLE
            e.ignore = 0
            e.action_behavior = 0
            e.action_state = 0
            e.hit_state = 0
            local px, _, pz = e:player_pos()
            e.player_pos_x = px & 0xFFFF
            e.player_pos_z = pz & 0xFFFF
            e.status_flags = e.status_flags & 0xF1
            return
        end

        e.health = -1
        e.shadow_tint = 0x00FFFF50
        e:adjust_shadow_size(-100, -100)
        e:raise_death_event()
        e.action_state = 3
        e.status_flags = e.status_flags | 0x0E
        e.move_speed_current = 0
        st = 3
    end

    if st == 3 then
        e:adjust_shadow_size(6, 6)
        e.death_timer = e.death_timer - 1
        if e.death_timer == 0 then
            e.action_state = 4
        end
        e:move(0, e.move_speed_current)
    end
end

long_push_back = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0x0C
        e.timing_control = 1
        e.hit_state = 1
        e.animation_id = 7
        e.move_speed_current = 0x1E
    elseif st ~= 1 then
        if st == 2 then
            local ticks = s16(e.action_ticks_counter)
            e.action_ticks_counter = ticks - 1
            if ticks == 0 then
                e.state = STATE_IDLE
                e.ignore = 0
                e.action_behavior = 2
                e.action_state = 2
                local px, _, pz = e:player_pos()
                e.player_pos_x = px & 0xFFFF
                e.player_pos_z = pz & 0xFFFF
                e.hit_state = 0
            end
        end
        e:move(2048, e.move_speed_current)
        return
    end

    if not e:advance_anim(0x400) then
        if e.animation_frame_id == 0x14 then
            e.hit_state = 0
        end
    else
        e.action_state = 2
        e.action_ticks_counter = e.rand_seed & 7
        e.move_speed_current = 15
    end

    e:check_special_weapon()
    e:move(2048, e.move_speed_current)
end

benddown_and_standup = function(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0x0D
    elseif e.action_state ~= 1 then
        return
    end

    if e:advance_anim(0x400) then
        e.state = STATE_IDLE
        e.ignore = 1
        e.action_behavior = 3
        e.action_state = 0
        e.behavior_flags = 0
        if e.stage == 4 and e.room == 5 then
            e.behavior_flags = 4
        end
        e.hit_state = 0
    end

    e:check_special_weapon()
end

-- ---------------------------------------------------------------------------
-- The pushback behaviours
-- ---------------------------------------------------------------------------

pushback_action = function(e)
    if e.action_behavior == 0 then
        zombie_pushback_idle(e)
    else
        zombie_pushback_stagger(e)
    end
end

zombie_pushback_idle = function(e)
    e.animation_id = 9

    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
    end

    e:advance_anim(0x400)
end

zombie_pushback_stagger = function(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.animation_id = 0x0E
    elseif st == 1 then
        e:advance_anim(0x400)
        if e.animation_frame_id == 5 then
            e.action_state = 2
            e.blend_counter = 3
            e.move_speed_current = 20
            e.action_ticks_counter = 1
        end
    elseif st == 2 then
        if (e.animation_frame_id == 7 or e.animation_frame_id == 0x18)
            and e.timing_control == 1 then
            e:play_enemy_sound(2)
        end

        e:advance_anim(0x400)
        e:body_part_speed(1)

        if (e:joint_flag(4) & 4) ~= 0 and e.animation_frame_id > 0x11 then
            return
        end

        local step = e:turn_toward_target(e.player_pos_x, e.player_pos_z, 8)
        e.zombie_turn_delta = step

        if (e.dir_control_flags & 0x80) ~= 0 then
            e.zombie_turn_delta = -e.zombie_turn_delta
            e.action_state = 3
            e.action_ticks_counter = (e.rand_seed & 0x1F) + 30
        end

        e.angle = e.angle + e.zombie_turn_delta
        e:move(0, e.move_speed_current)
    elseif st == 3 then
        e:advance_anim(0x400)
        e:body_part_speed(1)
        e.angle = e.angle + e.zombie_turn_delta
        e:move(0, e.move_speed_current)

        e.action_ticks_counter = e.action_ticks_counter - 1
        if e.action_ticks_counter == 0 then
            e.action_state = 3
        end
    end

    if (e.collision_flags & 8) ~= 0 then
        e.behavior_flags = e.behavior_flags + 1
    end
end

magnum_shot_pushback = function(e)
    if e.action_state == 0 then
        e.behavior_step = e.behavior_step | 0x01
        e.move_speed_current = 45
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 3
        e.blend_counter = 3
        return
    end

    if e.action_state == 1 then
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 32)

        if e.timing_control == 1 then
            if e.animation_frame_id == 8 then
                e:play_enemy_sound(1)
            end
            if e.animation_frame_id == 29 then
                e:play_enemy_sound(1)
            end
        end

        if e:advance_anim(0x400) then
            e.state_word = 0x103
        end
    end

    e:move(0, e.move_speed_current)
end

-- ---------------------------------------------------------------------------
-- The tables
-- ---------------------------------------------------------------------------

-- One 22-slot block; behaviour[k] is STATES[10 + k].
STATES = {
    init,             -- [0]  init
    state_check,      -- [1]  idle / behaviour dispatch
    damaged,          -- [2]  hit reaction
    die,              -- [3]  death sequence
    function() end,   -- [4]  a bare RET
    attack,           -- [5]  attacking the player
    nil,              -- [6]  unused (the original faults)
    nil,              -- [7]  unused (the original faults)
    action_update,    -- [8]  SCD action dispatch
    nil,              -- [9]  unused (the original faults)
    chase_player,     -- [10] = behaviour 0
    chase_player,     -- [11] = behaviour 1
    pushed_back,      -- [12] = behaviour 2
    pushed_back,      -- [13] = behaviour 3
    random_chase,     -- [14] = behaviour 4
    eating,           -- [15] = behaviour 5
    chase_player,     -- [16] = behaviour 6
    eating,           -- [17] = behaviour 7
    chase_player,     -- [18] = behaviour 8
    nil,              -- [19] = behaviour 9
    pushed_back,      -- [20] = behaviour 10
    nil,              -- [21] = behaviour 11 (DC-only in the original)
}

-- One 28-slot block read through three bases: damage [0..11], action [8..15]
-- (8..12 alias move) and move [16..27].
DAMAGE_ACTIONS = {
    short_push_back,             -- [0]
    short_push_back,             -- [1]
    push_and_stagger,            -- [2]
    push_and_stagger,            -- [3]
    push_and_drop,               -- [4]
    explode_leg_and_drop,        -- [5]
    long_push_back,              -- [6]
    benddown_and_standup,        -- [7]
    zombie_idle,                 -- [8]  = action 0
    zombie_slow_walk,            -- [9]  = action 1
    zombie_chase_walk,           -- [10] = action 2
    fast_player_facing,          -- [11] = action 3
    benddown_and_eat,            -- [12] = action 4
    turn_towards_player,         -- [13] = action 5
    zombie_vomiting,             -- [14] = action 6
    zombie_falldown,             -- [15] = action 7
    zombie_walk1,                -- [16] = move 0
    zombie_check_player_distance,-- [17] = move 1
    zombie_slow_walk_alt,        -- [18] = move 2
    zombie_idling,               -- [19] = move 3
    zombie_walk2,                -- [20] = move 4
    zombie_check_player_distance,-- [21] = move 5
    zombie_check_player_distance,-- [22] = move 6
    zombie_check_player_distance,-- [23] = move 7
    zombie_check_player_distance,-- [24] = move 8
    nil,                         -- [25] = move 9
    zombie_idling,               -- [26] = move 10
    nil,                         -- [27] = move 11
}

-- The separate 14-slot SCD table.
SCD_ACTIONS = {
    function() end,   -- [0]  a bare RET
    nil,              -- [1]
    zombie_aggresive_roar, -- [2]
    nil,              -- [3]
    nil,              -- [4]
    nil,              -- [5]
    nil,              -- [6]
    nil,              -- [7]
    nil,              -- [8]
    nil,              -- [9]
    zombie_headshot,  -- [10]
    zombie_scd_dying, -- [11]
    zombie_scd_vomiting,  -- [12]
    zombie_scd_vomiting2, -- [13]
}

-- ---------------------------------------------------------------------------
-- The per-frame driver
-- ---------------------------------------------------------------------------

-- `zombie_update`: the state dispatch, the state mirror, the collision tail,
-- the blood physics and the switch-zone/shadow gate.
M.run = function(e, config)
    if not e.monster_paused then
        local state = STATES[e.state + 1]
        if state then
            state(e, config)
        end

        -- Mirror the state block for the damage reaction's restore.
        e.state_mirror = e.state_word

        if e.internal_timer ~= 0 then
            e.internal_timer = e.internal_timer - 1
        end

        if e.state ~= STATE_ATTACK then
            -- The three entity-collision calls.
            e:separate()

            e.collision_flags = e.collision_flags & ~0x08

            local old_x, old_z = e.prev_pos_x, e.prev_pos_z
            local code = e:resolve_collision()
            e.dir_control_flags = e.dir_control_flags | code
            e.zombie_wander_word = e:xz_distance_to(old_x, old_z) & 0xFFFF

            if (e.behavior_flags & FLAG_LAYING_DOWN) ~= 0 then
                local floor = e:two_point_probe(-600, 0, 600, 0)
                e.dir_control_flags = e.dir_control_flags | floor
            end
        end

        if e.zombie_part_word ~= 0 then
            e:blood_splatter(2, 6)
        end
    end

    e:update_switch_zone()
    -- The body shadow is drawn by the renderer from the shadow fields while
    -- the switch-zone bit is set; the limb quad's flag/position contract:
    if (e:joint_flag(4) & 4) ~= 0
        and e:joint_world_y(4) == -100
        and e.has_enter_switch_zone ~= 0 then
        -- The 400x400 blood quad at the joint position is the deferred
        -- joint-object half; the state above is what it will read.
    end
end

return M
