-- Monster plant, entity id 0x0f (the 15-segment vine).
--
-- A stationary vine boss built from one pointer block read through five
-- bases: the state table (init / state check / damaged / die / no-op), the
-- behaviour table indexed by the `ignore` byte, the variant table indexed by
-- the spawn record's low nibble, the per-variant approach tick, and the
-- shared twelve-case step chain entered at three offsets. Behaviour 2
-- (the grab) enters the chain at step 0, behaviour 3 (the bite/hold) at step
-- 6 and behaviour 4 (the strike) at step 8; step 9 aliases step 3.
--
-- The vine "strikes" by revealing segments from the current cursor (joint 11
-- is the head) down to a stop index and retracts by hiding them from the
-- base up. The head's world translation is the hit box: every gated frame
-- the SCA hit point is retargeted to the head-minus-body vector, so an
-- extended vine can be shot and the ground shadow follows the head.
--
-- The state block's four bytes are written together as one word in several
-- places (`MP_STEP16`, the state dword). The scratch views at `0x16C`.. are
-- 16/32-bit words over the bytes the other enemies read at byte width; they
-- are the `mp_*` properties. The shared side-picking register lives on the
-- game state (`monster_plant_sides`), so two consecutive lunges never pick
-- the same side three times running.
--
-- All state lives on the Rust entity; this file keeps none, so the scripting
-- VM may be reset between any two updates.

local SEGMENTS = 15
local HEAD_JOINT = 11

-- Vine EXTEND stops, indexed by the tick counter (guarded below four).
local EXTEND_STOPS = { 8, 4, 2, 0 }

-- Vine RETRACT stops, indexed by the tick counter. The caller's segment
-- guard bounds the index: it trips at entry 10, so the trailing zero is
-- never read. The two leading zeroes are real: they buy a one-frame pause
-- before segment 1 starts to disappear.
local RETRACT_STOPS = { 0, 0, 1, 2, 3, 4, 5, 7, 9, 12, 14, 0 }

-- The compiler's branchless two-constant pick: `(odd ? 0 : mask) + base`,
-- truncated to a byte. 0xFA + 10 truncates to 4, not 0x104.
local function mp_pick(odd, mask, base)
    return ((odd and 0 or mask) + base) & 0xFF
end

-- Branchless-style absolute value (the sandbox carries no math library).
local function abs(value)
    if value < 0 then
        return -value
    end
    return value
end

-- Sign-extend a stored 16-bit counter for the signed compares the original
-- compiles at those sites.
local function as_s16(value)
    value = value & 0xFFFF
    if value >= 0x8000 then
        return value - 0x10000
    end
    return value
end

-- The shared grab closing cue: the enemy-bank grab cue, then the player's
-- per-character 3D cue (the id-nibble special case is kept for fidelity even
-- though the player id never reaches it).
local function grab_sound(e)
    e:play_enemy_sound(3)
    if (e.player_character & 3) ~= 3 then
        local x, y, z = e:player_pos()
        e:play_3d_sound_at(2, (e.player_character & 1) + 0x17, x, y, z)
    else
        local x, y, z = e:player_pos()
        e:play_3d_sound_at(3, 0, x, y, z)
    end
end

-- Idle sway: on a countdown expiry re-roll the sway animation, and only the
-- "3" pose rewinds the frame cursor, and only past frame 0x13.
local function idle_fidget(e)
    if e.mp_fidget ~= 0 then
        e.mp_fidget = e.mp_fidget - 1
        return
    end
    e.animation_id = mp_pick((e:random() & 1) ~= 0, 0xFB, 8)
    e.timing_control = 0
    e.blend_counter = 0x1F
    e.mp_fidget = as_s16(e:random() & 0x1F)
    if as_s16(e.animation_id) == 3 and e.animation_frame_id > 0x13 then
        e.animation_frame_id = e.animation_frame_id - 10
    end
end

-- Hide every segment (the bit-0 clear the original writes over all 15 joints).
local function hide_all_segments(e)
    for segment = 0, SEGMENTS - 1 do
        e:set_joint_visible(segment, false)
    end
end

-- Forward declarations: variant 5 enters the grab step, the behaviours
-- dispatch the shared step chain, and the first death case calls the second.
local step_01
local dispatch_step
local die_case_1

-- ---------------------------------------------------------------------------
-- Variant handlers: one per spawn-record low nibble, run once when the
-- behaviour index is still zero. Each installs the behaviour it wants next.
-- ---------------------------------------------------------------------------

-- Variants 0/1 and the 0x80 scripted variant: plain idle.
local function variant_0(e)
    e.ignore = 1
    e.status_flags = e.status_flags & 0xF7
    e.mp_alerted = 1
end

-- Variant 2: retract out of sight and go dormant.
local function variant_2(e)
    e.status_flags = e.status_flags | 8
    e.ignore = 1
    -- Per-character hiding pose: 0x10 for Jill, 0x0F for Chris.
    e.animation_id = 0x10 - ((e.player_character == 0) and 1 or 0)
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0
    e.mp_shadow = 0
    e.mp_swerve = 0
    hide_all_segments(e)
end

-- Variant 3: wind up for the grab.
local function variant_3(e)
    e.ignore = 2
    e.animation_id = 1
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0x1F
end

-- Variant 4: the lunge. Two entry poses depending on which way the plant is
-- already facing; pose 2 is only for a yaw inside the second half turn.
local function variant_4(e)
    e.ignore = 3
    if as_s16(e.angle) <= 0x7FF or as_s16(e.angle) >= 0x1000 then
        e.animation_id = 9
    else
        e.animation_id = 2
    end
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 7
    e.status_flags = e.status_flags | 2
    e:play_enemy_sound(0)
end

-- Variant 5: strike from hiding. Hides the whole vine, parks the cursor at
-- the tip and snaps to face the player before the reveal starts.
local function variant_5(e)
    e.ignore = 4
    e.animation_id = 0x10 - ((e.player_character == 0) and 1 or 0)
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0
    hide_all_segments(e)
    e.mp_seg = 14
    e.action_ticks_counter = 0
    e.mp_timer_a = 2
    e.mp_alerted = 1
    e.mp_angle_bk = as_s16(e.angle)
    local px, _, pz = e:player_pos()
    e.angle = e:angle_to(px, pz)
    step_01(e) -- grab the player immediately
    e:play_enemy_sound(2)
end

-- Variants 6/7: idle thrash. Variant 7 also arms the poison colour pulse.
local function variant_6(e)
    e.ignore = 5
    e.health = -1
    e.animation_id = mp_pick((e:random() & 1) ~= 0, 7, 0)
    e.animation_frame_id = e:random() & 0xF
    e.timing_control = 0
    e.blend_counter = 0
    e.action_ticks_counter = 1
    if e.behavior_flags == 7 then
        e:retarget_tint(2, -3, 0, 0, 0x100)
    end
end

-- Variants 8/9 (spawn kind 0x29 lands here through the nibble): the poison
-- plant. Alternates lunge sides through the shared shift register.
local function variant_8(e)
    e.ignore = 6
    local side = e:random() & 1
    -- If the last two recorded sides already match this one, flip it, so a
    -- third identical lunge is impossible.
    if side * 3 == (e.monster_plant_sides & 3) then
        side = side ~ 1
    end
    e.monster_plant_sides = side | ((e.monster_plant_sides * 2) & 0xFFFFFFFF)
    e.animation_id = (side == 1) and 0x11 or 0x13
    e.animation_frame_id = ((e:random() & 7) * 3) & 0xFF
    if as_s16(e.animation_id) == 0x11 then
        e.animation_frame_id = (e.animation_frame_id + (e:random() & 7) * 3) & 0xFF
    end
    e.timing_control = 0
    e.blend_counter = 0x1F
    e.mp_fidget = 0
    e.action_ticks_counter = 1
    if e.behavior_flags == 0x29 then
        e.mp_timer_a = 0x28
        e.mp_timer_b = 0x28
        e.mp_angle_bk = 2 -- red tint steps owed
        e.mp_swerve = 3 -- green tint steps owed
        e:play_enemy_sound(4)
    end
end

local VARIANTS = {
    [0] = variant_0,
    [1] = variant_0,
    [2] = variant_2,
    [3] = variant_3,
    [4] = variant_4,
    [5] = variant_5,
    [6] = variant_6,
    [7] = variant_6,
    [8] = variant_8,
    [9] = variant_8,
}

-- The variant selector: a scripted record (bit 0x80) idles and marks itself
-- visible instead, and the step/behaviour word is cleared either way.
local function select_variant(e)
    if (e.behavior_flags & 0x80) == 0 then
        local handler = VARIANTS[e.behavior_flags & 0xF]
        if handler then
            handler(e)
        end
    else
        variant_0(e)
        e.status_flags = e.status_flags | 8
    end
    e.mp_step_word = 0
end

-- ---------------------------------------------------------------------------
-- Behaviour 1's per-variant approach tick.
-- ---------------------------------------------------------------------------

-- Variant 0: wake into the wind-up once the player is close.
local function sub_0(e)
    if e.mp_dist < 0xA8D then
        e.ignore = 0
        e.behavior_flags = 3
    end
end

-- Variant 1: needs a facing check too, and uses a wider trigger radius the
-- first time (while the alerted latch is still 0).
local function sub_1(e)
    if e.mp_holdoff ~= 0 then
        e.mp_holdoff = e.mp_holdoff - 1
        return
    end
    local trigger = 3000
    if e.mp_alerted ~= 0 then
        trigger = trigger - 0x1F4
    end
    if e.mp_dist <= trigger and e.player_attacked == 0 then
        local px, _, pz = e:player_pos()
        if e:turn_toward_target(px, pz, 0x400) == 0 then
            e.ignore = 0
            e.behavior_flags = 4
        end
    end
end

-- Variant 2: dormant. Picks a lunge side, then swings a virtual yaw to test
-- whether the player is reachable from that side before committing.
local function sub_2(e)
    if e.behavior_flags ~= 2 then
        e.ignore = 0
        return
    end
    if e.mp_dist > 4000 then
        e.mp_swerve = 0
        e.mp_holdoff = 0
        return
    end
    if e.mp_dist >= 0xC45 then
        return
    end
    local px, _, pz = e:player_pos()
    if e.mp_swerve == 0 then
        -- 2 = dead ahead, 1 = needs a swerve.
        if e:turn_toward_target(px, pz, 0x200) == 0 then
            e.mp_swerve = 2
        end
        if e:turn_toward_target(px, pz, 0x600) ~= 0 then
            e.mp_swerve = 1
        end
        return
    end
    if e.player_attacked ~= 0 then
        return
    end
    e.mp_angle_bk = as_s16(e.angle)
    e.angle = (e.angle + as_s16((e.mp_swerve - 1) * 0x800)) & 0xFFFF
    if e:turn_toward_target(px, pz, 0x200) == 0 then
        e.ignore = 0
        e.behavior_flags = 5
    end
    e.angle = e.mp_angle_bk
end

-- ---------------------------------------------------------------------------
-- Behaviours (indexed by the `ignore` byte). Behaviour 0 is a bare return in
-- the original and is unreachable: every variant handler leaves 1..6.
-- ---------------------------------------------------------------------------

-- Behaviour 1: idle. Sways on a timer, and on the frames it is not re-rolling
-- the sway it runs the per-variant approach test.
local SUBS = nil -- filled after the sub handlers below

local function behavior_1(e)
    if (e.behavior_flags & 0x20) ~= 0 then
        e.ignore = 0
        return
    end
    local fidget = e.mp_fidget
    if fidget == 0 then
        e.animation_id = mp_pick((e:random() & 1) ~= 0, 7, 0)
        e.timing_control = 0
        e.blend_counter = 0x1F
        e.mp_fidget = as_s16(e:random() & 0x1F)
        return
    end
    e.mp_fidget = fidget - 1
    local flags = e.behavior_flags
    if flags ~= 0x80 then
        local index = flags & 0xF
        if index < 4 then
            local handler = SUBS[index + 1]
            if handler then
                handler(e)
            end
        end
    end
    if e.behavior_flags ~= 2 then
        if e:advance_anim(0x80) then
            e.ignore = 0
        end
    end
    -- While the player is in the "being held" pose the plant runs its
    -- animation a second time so the two stay in step.
    if e.player_state == 1
        and e.player_anim_frame_id == 3
        and e.player_action_behavior == 0x14
        and e.player_action_state == 0
    then
        if e:advance_anim(0x80) then
            e.ignore = 0
        end
    end
end

SUBS = { sub_0, sub_1, sub_2 }

-- Behaviour 2: the grab sequence, entering the step chain at base 0.
local function behavior_2(e)
    if (e.behavior_flags & 0x20) ~= 0 then
        e.ignore = 0
        return
    end
    e.mp_anim_end = e:advance_anim(0x80) and 1 or 0
    dispatch_step(e, e.action_behavior)
end

-- Behaviour 3: the bite/hold sequence, entering at base 6. There is no
-- animation prologue: step 6 runs its own and the later steps inherit the
-- latch it left.
local function behavior_3(e)
    if (e.behavior_flags & 0x20) ~= 0 then
        e.ignore = 0
        return
    end
    dispatch_step(e, e.action_behavior + 6)
end

-- Behaviour 4: the vine strike, entering at base 8.
local function behavior_4(e)
    e.mp_anim_end = e:advance_anim(0x80) and 1 or 0
    dispatch_step(e, e.action_behavior + 8)
end

-- Behaviour 5: winding down after a release. Immortal while the counter
-- drains.
local function behavior_5(e)
    e.health = -1
    if e.action_ticks_counter ~= 0 then
        e:advance_anim(0x100)
        e.action_ticks_counter = e.action_ticks_counter - 1
    end
end

-- Behaviour 6: the poison plant's idle. Re-rolls a thrash pose on half the
-- animation wraps and bleeds the red/green tint out on two 40-frame timers.
local function behavior_6(e)
    if e:advance_anim(0x80) then
        if (e:random() & 0x80) ~= 0 then
            e.animation_id = (((e:random() & 1) ~= 0) and 0 or 2) + 0x11
            e.blend_counter = 0x1F
        end
    end
    if e.behavior_flags == 0x29 then
        e.mp_timer_a = e.mp_timer_a - 1
        if e.mp_timer_a == 0 then
            if e.mp_angle_bk ~= 0 then
                e:tint_model(1, 0, 0, 0, 0x100)
                e.mp_angle_bk = e.mp_angle_bk - 1
            end
            e.mp_timer_a = 0x28
        end
        e.mp_timer_b = e.mp_timer_b - 1
        if e.mp_timer_b == 0 then
            if e.mp_swerve ~= 0 then
                e:tint_model(0, -1, 0, 0, 0x100)
                e.mp_swerve = e.mp_swerve - 1
            end
            e.mp_timer_b = 0x28
        end
    end
end

-- Behaviour 0 is a bare return in the original and is unreachable: every
-- variant handler leaves 1..6 before the dispatch.
local function behavior_0(_e)
end

local BEHAVIORS = {
    behavior_0, behavior_1, behavior_2, behavior_3,
    behavior_4, behavior_5, behavior_6,
}

-- ---------------------------------------------------------------------------
-- The twelve shared step handlers. Behaviour 2 starts at 0, behaviour 3 at
-- 6, behaviour 4 at 8; step 9 aliases step 3.
-- ---------------------------------------------------------------------------

-- Step 0: idle in reach, waiting for the player to come close enough to grab;
-- also the step the plant returns to after a failed grab.
local function step_00(e)
    if e.mp_anim_end ~= 0 and e.animation_id == 1 then
        e.animation_id = 8
        e.timing_control = 0
        e.blend_counter = 0x0F
    end
    if e.animation_id ~= 1 then
        idle_fidget(e)
        if e.player_action_behavior == 0x14 then
            e:advance_anim(0x80)
        end
    end
    if e.mp_dist > 0x2134 then
        e.ignore = 0
        e.behavior_flags = 0
    end
    if e.mp_holdoff ~= 0 then
        e.mp_holdoff = e.mp_holdoff - 1
        return
    end
    if e.mp_dist < 0x708 and e.player_attacked == 0 then
        e.action_behavior = e.action_behavior + 1
        e.animation_id = mp_pick((e.player_character & 1) ~= 0, 0xFA, 10)
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0
        local px, _, pz = e:player_pos()
        -- Not facing the player yet: divert to the recovery step instead.
        if e:turn_toward_target(px, pz, 0x200) ~= 0 then
            e.action_behavior = 4
            e.mp_timer_a = 2
        end
    end
end

-- Step 1: the grab connects. The player is handed to animation 6 / frame 15,
-- whose window entry is the three-state hold table the grabber drives.
function step_01(e)
    e.action_behavior = e.action_behavior + 1
    e.status_flags = e.status_flags | 2
    e.player_attack_anim = (e:player_facing_entity() and 1 or 0) + 2
    e.player_angle = e.angle
    e:snap_grab()
    e.player_action_behavior = 0
    e.player_action_state = 0
    e.player_attacked = 1
    e.player_state = 6
    e.player_anim_frame_id = 0x0F
    e.player_flags = e.player_flags | 2
    e:play_enemy_sound(2)
end

-- Step 2: reel the player in, then start the drain.
local function step_02(e)
    e.mp_angle_bk = as_s16(e.angle)
    local px, _, pz = e:player_pos()
    e:rotate_toward_target(px, pz, 0x80)
    if e.mp_anim_end ~= 0 then
        e.action_behavior = e.action_behavior + 1
        e.animation_id = e.animation_id + 1
        e.timing_control = 0
        e.blend_counter = 0
        e.action_ticks_counter = 0x3C
        e.mp_timer_a = 0x0C
        grab_sound(e)
    end
end

-- Steps 3 and 9: the drain. Two health every 12 frames, and mashing the pad
-- burns the 60-frame hold down four frames faster per drain tick.
local function step_03(e)
    e.mp_timer_a = e.mp_timer_a - 1
    if e.mp_timer_a == 0 then
        e.player_health = e.player_health - 2
        -- Only variant 5 refuses to kill outright.
        if e.behavior_flags == 5 and e.player_health < 0 then
            e.player_health = 0
        end
        e.mp_timer_a = 0x0C
    end
    local hold = as_s16(e.action_ticks_counter)
    if hold > 0 and e.player_health >= 0 then
        local reduce = e:player_mashing() and 3 or 0
        e.action_ticks_counter = hold - 1 - reduce
        return
    end
    if e.mp_anim_end ~= 0 then
        e.action_behavior = e.action_behavior + 1
        e.animation_id = e.animation_id + 1
        e.timing_control = 0
        -- action_state 2 selects the player's release handler.
        e.player_action_state = 2
    end
end

-- Step 4: recover from a grab that never landed.
local function step_04(e)
    if e.mp_anim_end ~= 0 then
        e.ignore = 0
        e.behavior_flags = 0
        e.status_flags = e.status_flags & 0xFD
        e.mp_holdoff = 0x1E
        e.angle = e.mp_angle_bk
    end
end

-- Step 5: the post-release flail. Either re-arms for another grab if the
-- player is still in reach, or gives up and goes back to idle.
local function step_05(e)
    if e.mp_anim_end == 0 then
        return
    end
    e.mp_timer_a = e.mp_timer_a - 1
    if e.mp_timer_a == 0 then
        if e.mp_dist < 0xA8D then
            e.action_behavior = 0
            e.animation_id = 8
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 0x0F
        else
            e.ignore = 0
            e.behavior_flags = 3
        end
        e.mp_holdoff = 0x1E
        return
    end
    e.animation_id = e.animation_id + 2
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0x0F
    e:play_enemy_sound(0)
end

-- Step 6 (behaviour 3's step 0): the bite. Runs its own animation step at
-- blend 0x200 and lands the hit on animation frame 6.
local function step_06(e)
    e.mp_anim_end = e:advance_anim(0x200) and 1 or 0
    if e.mp_anim_end ~= 0 then
        e.action_behavior = e.action_behavior + 1
        e.mp_fidget = 0
        e.mp_alerted = 0
    end
    if e.animation_frame_id == 6 and e.player_attacked == 0 then
        local px, _, pz = e:player_pos()
        if e:turn_toward_target(px, pz, 0x400) == 0 then
            local front = 0
            e.player_action_state = 0
            -- Hit from the front unless the player faces across the plant.
            if as_s16(e.player_angle) <= 0x400 or as_s16(e.player_angle) >= 0xC00 then
                front = 1
            end
            e.player_attacked = front + 1
            e.player_action_behavior = front + 0x66
            e.player_health = e.player_health - 2
            if e.player_health < 0 then
                e.player_health = 0
            end
            e:play_enemy_sound(1)
            e.mp_holdoff = 1
        end
    end
end

-- Step 7 (behaviour 3's step 1): back to sway, still watching.
local function step_07(e)
    idle_fidget(e)
    sub_1(e)
    if e.mp_dist > 0x2133 then
        e.ignore = 0
        e.behavior_flags = 1
    end
    e:advance_anim(0x80)
    if e.player_action_behavior == 0x14 then
        e:advance_anim(0x80)
    end
end

-- Step 8, shared by behaviour 2 (step 8), behaviour 3 (step 2) and behaviour
-- 4 (step 0): the vine EXTENDS. Each pass reveals segments from the current
-- cursor down to the stop; when the animation wraps, the grab closes.
local function step_08(e)
    local revealed = false
    if e.mp_timer_a == 0 and e.action_ticks_counter < 4 then
        local cursor = e.mp_seg
        local stop = EXTEND_STOPS[e.action_ticks_counter + 1]
        while stop <= cursor do
            cursor = cursor - 1
            e:set_joint_visible(cursor + 1, true)
        end
        e.mp_seg = cursor
        e.action_ticks_counter = e.action_ticks_counter + 1
        revealed = true
    end
    -- The decrement is skipped only on a frame that revealed segments; a
    -- spent cursor wraps the timer to 0xFFFF exactly like the original.
    if not revealed then
        e.mp_timer_a = e.mp_timer_a - 1
    end
    if e.mp_anim_end ~= 0 then
        e.action_behavior = e.action_behavior + 1
        e.animation_id = mp_pick((e.player_character & 1) ~= 0, 0xFA, 0x0B)
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0
        e.player_flags = e.player_flags | 8
        e.action_ticks_counter = 0x3C
        e.mp_timer_a = 0x0C
        grab_sound(e)
    end
end

-- Step 10: arm the retraction.
local function step_10(e)
    e.action_behavior = e.action_behavior + 1
    e.animation_id = 0x12
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0
    e.mp_seg = 0
    e.action_ticks_counter = 0
end

-- Step 11: the vine RETRACTS. Hides segments from the base upward, gated on
-- the animation having passed frame 2.
local function step_11(e)
    if e.animation_frame_id > 2 and e.mp_seg < 0x0F then
        local cursor = e.mp_seg
        local stop = RETRACT_STOPS[e.action_ticks_counter + 1]
        while cursor <= stop do
            cursor = cursor + 1
            e:set_joint_visible(cursor - 1, false)
        end
        e.mp_seg = cursor
        e.action_ticks_counter = e.action_ticks_counter + 1
    end
    if e.mp_anim_end ~= 0 then
        e.ignore = 0
        -- 0x80 is the scripted variant; it keeps its flags.
        if e.behavior_flags ~= 0x80 then
            e.behavior_flags = 2
        end
        e.angle = e.mp_angle_bk
        e.mp_swerve = 0
        e.mp_holdoff = 0
    end
end

local STEPS = {
    step_00, step_01, step_02, step_03, step_04, step_05,
    step_06, step_07, step_08, step_03, step_10, step_11,
}

-- The step dispatch is exact in the original (the index always lands); the
-- guard turns a corrupt step into a dropped frame instead of a wild jump.
function dispatch_step(e, index)
    local handler = STEPS[index + 1]
    if handler then
        handler(e)
    end
end

-- ---------------------------------------------------------------------------
-- States.
-- ---------------------------------------------------------------------------

-- State 0: one-shot setup. Health 1 (the plant is immortal until its die
-- state pins -1), the full-reach SCA record, the small ground quad, and the
-- sprung spawn kind hides the whole vine and casts no shadow.
local function init(e)
    e.state_word = 1
    e.ignore = 0
    e.health = 1
    e:set_sca(0x0190, 0, 0, 0, 0)
    e.status_flags = (e.status_flags & 0x1F) | 10
    e.animation_id = 0
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0
    e:reset_joints()
    e.shadow_half_x = 500
    e.shadow_half_z = 100
    e:set_shadow_offset(0, 0, 0)
    e.shadow_tint = 0x404040
    e.mp_shadow = 1
    e.mp_holdoff = 0
    e.mp_fidget = 0
    e.mp_alerted = 1
    e.mp_hits = 0
    e:advance_anim(0x80)
    e.monster_plant_sides = 0
    if (e.behavior_flags & 2) ~= 0 then
        hide_all_segments(e)
        e.health = -1
        e.mp_shadow = 0
    end
end

-- State 1: the decision layer. The spawn record's death request jumps
-- straight to the die state; otherwise the variant is selected once (while
-- the behaviour index is still 0) and the behaviour runs, then the state
-- dword is snapshotted for the damaged state to restore.
local function state_check(e)
    local flags = e.behavior_flags
    if (flags & 0x40) ~= 0 then
        e.state = 3
        e.ignore = 0
        return
    end
    e.status_flags = e.status_flags & 0x1F
    if flags == 0 or flags == 1 then
        e:check_visual_range(5000)
    end
    if flags == 3 or flags == 4 then
        e.status_flags = e.status_flags | 0xC0
        e:check_visual_range(5000)
    end
    local px, _, pz = e:player_pos()
    e.mp_dist = abs(px - e.pos_x) + abs(pz - e.pos_z)
    if e.ignore == 0 then
        e.mp_step_word = 0
        select_variant(e)
    end
    local behavior = e.ignore
    if behavior < 7 then
        local handler = BEHAVIORS[behavior + 1]
        if handler then
            handler(e)
        end
    end
    e.mp_state_bk = e.state_word
end

-- State 2: the plant has no flinch of its own. It counts the hit, raises the
-- combat-progression flag after the third, restores the interrupted
-- behaviour and re-enters the state check.
local function damaged(e)
    e.mp_hits = e.mp_hits + 1
    if (e.mp_hits & 0xFFFF) > 3 then
        e:raise_flag(0, 0x5B)
    end
    e.state_word = e.mp_state_bk
    e.hit_state = 0
    state_check(e)
end

-- State 3: the death thrash, a three-case counter.
local function die_case_0(e)
    e.blend_counter = 0x1F
    e.ignore = 1
    e.mp_fidget = 0
    -- Two rolls summed: 1..63 frames of thrashing.
    local r1 = e:random()
    local r2 = e:random()
    e.action_ticks_counter = ((r1 & 0x1F) + (r2 & 0x1F) + 1) & 0xFFFF
    e.health = -1
    die_case_1(e)
end

function die_case_1(e)
    if e.action_ticks_counter ~= 0 then
        e.action_ticks_counter = e.action_ticks_counter - 1
        if e.action_ticks_counter ~= 0 then
            behavior_6(e) -- keep thrashing (and keep the poison tint)
            return
        end
        e.animation_id = 0x0E
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x1F
    end
    if e:advance_anim(0x80) then
        e.ignore = e.ignore + 1
        -- Four draws, and the first is discarded before it is used.
        e:random()
        e.animation_id = mp_pick((e:random() & 1) ~= 0, 7, 0)
        e.animation_frame_id = e:random() & 0xF
        e.timing_control = 0
        e.blend_counter = 0x1F
        e.action_ticks_counter = ((e:random() & 0xF) + 0x32) & 0xFFFF
        e.mp_timer_a = 4
        e.mp_timer_b = 8
    end
end

local function die_case_2(e)
    -- Signed compare: the twitch rate tapers off as the counter runs down.
    if (e:random() & 0x3F) < as_s16(e.action_ticks_counter) then
        e:advance_anim(0x80)
        e:advance_anim(0x80)
    end
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 then
        e.ignore = e.ignore + 1
        e:raise_death_event()
    end
    -- Spawn-kind bit 0 is the withering colour ramp.
    if (e.behavior_flags & 1) ~= 0 then
        e.mp_timer_a = e.mp_timer_a - 1
        if e.mp_timer_a == 0 then
            e:tint_model(1, 0, 0, 0, 0x100)
            e.mp_timer_a = 4
        end
        e.mp_timer_b = e.mp_timer_b - 1
        if e.mp_timer_b == 0 then
            e:tint_model(0, 1, 0, 0, 0x100)
            e.mp_timer_b = 8
        end
    end
end

local function die(e)
    local sub = e.ignore
    if sub == 0 then
        die_case_0(e)
    elseif sub == 1 then
        die_case_1(e)
    elseif sub == 2 then
        die_case_2(e)
    end
end

local STATES = { init, state_check, damaged, die }

-- ---------------------------------------------------------------------------
-- The per-frame entry.
-- ---------------------------------------------------------------------------

function update(e)
    -- The state dispatch and the head hit retarget are inside the monster
    -- message gate: a paused plant keeps its last hit point.
    if not e.monster_paused then
        local state = e.state
        if state < 5 then
            STATES[state + 1](e)
        end
        local hx = e:joint_world_x(HEAD_JOINT)
        local hy = e:joint_world_y(HEAD_JOINT)
        local hz = e:joint_world_z(HEAD_JOINT)
        e:set_sca_hit_point(hx - e.pos_x, hy - e.pos_y, hz - e.pos_z)
    end

    -- The original also clears its matrix scratch word here; the port has no
    -- equivalent field and nothing reads it.
    local in_zone = e:update_switch_zone()
    e.shadow_suppressed = (e.mp_shadow & 1) == 0
    if in_zone ~= 0 and (e.mp_shadow & 1) ~= 0 then
        -- The ground shadow is anchored on the vine head, so the quad offset
        -- tracks the head's world translation from the last computed pose.
        e:set_shadow_offset(
            e:joint_world_x(HEAD_JOINT) - e.pos_x,
            e:joint_world_y(HEAD_JOINT) - e.pos_y,
            e:joint_world_z(HEAD_JOINT) - e.pos_z
        )
    end
end
