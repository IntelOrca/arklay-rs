-- Computer arm, entity id 0x15 (the left forearm at the lab terminal).
--
-- Slot identity is semantic: this is the arm the terminal addresses as slot
-- zero, and the only one with command 8 - the release-lever pull after the
-- second door is confirmed. Commands 1..4 enter its own nine-step chain (the
-- three reaches and their move steps, a log-in gesture with a frame gate
-- instead of a voice handshake, and the typing play-out). Command 5 is the
-- success gesture, the only one Jill gets. Command 6 reaches down and pulls,
-- command 7 drops the hand off the keyboard and lets it hang, and command 8
-- runs its own three-step lever chain.
--
-- All arithmetic is the original's 32-bit wrapping/truncating integer maths
-- (the `i32`/`trunc_div`/`sar` helpers). All state lives on the Rust entity;
-- this file keeps none.

local i32, trunc_div, sar, as_s8

-- 32-bit wrap, the C int arithmetic the original compiles.
i32 = function(value)
    value = value & 0xFFFFFFFF
    if value >= 0x80000000 then
        return value - 0x100000000
    end
    return value
end

-- C integer division truncates toward zero; Lua's `//` floors.
trunc_div = function(a, b)
    local quotient = a // b
    if (a % b ~= 0) and ((a < 0) ~= (b < 0)) then
        quotient = quotient + 1
    end
    return quotient
end

-- Arithmetic right shift (Lua's `>>` is logical).
sar = function(value, bits)
    return value // (1 << bits)
end

-- Sign-extend one byte (`(char)` casts).
as_s8 = function(value)
    if value >= 0x80 then
        return value - 0x100
    end
    return value
end

local function is_jill(e)
    return e.player_character == 1
end

local function set_anim(e, id, frame, blend)
    e.animation_id = id
    e.animation_frame_id = frame
    e.timing_control = 0
    e.blend_counter = blend
end

-- Aim the fixed-point velocity so the hand covers `from` to `to` in `frames`
-- frames; a zero frame count stops it.
local function set_velocity(e, from_x, from_z, to_x, to_z, frames)
    local n = frames & 0xFFFF
    if n == 0 then
        e.arm_vel_x = 0
        e.arm_vel_z = 0
        return
    end
    e.arm_vel_x = trunc_div(i32((to_x - from_x) * 0x10000), n)
    e.arm_vel_z = trunc_div(i32((to_z - from_z) * 0x10000), n)
end

-- Advance the 16.16 position by the current velocity and publish the integer
-- part to the entity position.
local function integrate(e)
    e.arm_pos_x = i32(e.arm_pos_x + e.arm_vel_x)
    e.arm_pos_z = i32(e.arm_pos_z + e.arm_vel_z)
    e.pos_x = sar(e.arm_pos_x, 16)
    e.pos_z = sar(e.arm_pos_z, 16)
end

local function command_done(e)
    e.behavior_flags = e.behavior_flags | 0x20
    e.ignore = 0
end

-- Start a 15-frame reach to (home + dx, home + dz) and play the matching
-- animation. Animations 2 and 3 are the two "reach across" variants; if the
-- arm is already in one of them the other is chosen so the hand does not
-- snap.
local function begin_reach(e, dx, dz, anim)
    local current = as_s8(e.animation_id)
    if anim == 2 and current == 1 then
        anim = 3
    elseif anim == 3 and current == 3 then
        anim = 2
    end
    e.animation_id = anim
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 7
    set_velocity(e, e.pos_x, e.pos_z, e.target_x + dx, e.target_z + dz, 15)
end

-- This arm's reach offsets keep the hand on its own half of the keyboard.
local REACH = { { -0x32, -0x32 }, { 0x32, 0 }, { -0x14, 0x32 } }

-- Reach A: anim 1 for both characters; B: Chris 1, Jill 2; C: Chris 2, Jill 3.
local function reach_anim(e, index)
    if index == 0 then
        return 1
    end
    if index == 1 then
        return is_jill(e) and 2 or 1
    end
    return is_jill(e) and 3 or 2
end

local function step_reach(e, index)
    e.ignore = e.ignore + 1
    local offset = REACH[index + 1]
    begin_reach(e, offset[1], offset[2], reach_anim(e, index))
end

-- chain[0]: reach left-and-forward.
local function step_reach_a(e)
    step_reach(e, 0)
end

-- chain[2]: reach right.
local function step_reach_b(e)
    step_reach(e, 1)
end

-- chain[4]: reach left-and-back.
local function step_reach_c(e)
    step_reach(e, 2)
end

-- The shared "interpolate and wait for the reach animation" step. Completing
-- the animation ends the command.
local function step_move(e)
    e.arm_pos_x = i32(e.arm_pos_x + e.arm_vel_x)
    e.arm_pos_z = i32(e.arm_pos_z + e.arm_vel_z)
    e.pos_x = sar(e.arm_pos_x, 16)
    e.pos_z = sar(e.arm_pos_z, 16)
    if e:advance_anim(0x200) then
        command_done(e)
    end
    if not is_jill(e) then
        if e.animation_frame_id == 8 then
            e:play_sfx(2, 0x18)
        end
    elseif e.animation_frame_id == 3 or e.animation_frame_id == 13 then
        e:play_sfx(2, 0x18)
    end
end

-- chain[6]: the log-in gesture. No voice line and no voice handshake - those
-- belong to the right arm. This one slides the hand down over `gate` frames
-- instead, and the same gate is the frame at which the typing step stops
-- integrating.
local function step_login15(e)
    e.ignore = e.ignore + 1
    e.action_behavior = 0
    set_anim(e, 4, 0, 7)
    local gate = is_jill(e) and 0x0A or 0x12
    e.arm_gate = gate
    set_velocity(e, e.pos_x, e.pos_z, e.pos_x, e.target_z - (is_jill(e) and 200 or 300), gate)
end

-- chain[7]: play animation 4 out. Integrates only while the frame is below
-- the gate, so the hand stops travelling part way through the animation.
-- Jill gets a click on frame 0x0D.
local function step_typing15(e)
    if e.animation_frame_id < e.arm_gate then
        integrate(e)
    end
    if e:advance_anim(0x200) then
        command_done(e)
    end
    if is_jill(e) and e.animation_frame_id == 0x0D then
        e:play_sfx(2, 0x1B)
    end
end

-- chain[8]: command 4 never reaches it (the typing step ends the command
-- itself), but it is in the table.
local function step_settle15(e)
    integrate(e)
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 then
        command_done(e)
    end
end

-- ---------------------------------------------------------------------------
-- Command 5: the success gesture. Both characters get one, and they differ.
-- ---------------------------------------------------------------------------

local function cmd5_begin(e)
    e.ignore = e.ignore + 1
    e.action_behavior = 0
end

-- Jill's first step: straight into animation 5.
local function cmd5_jill_start(e)
    e.action_behavior = e.action_behavior + 1
    set_anim(e, 5, 0, 7)
end

-- Chris's first step: slide the hand down 0x50 over four frames.
local function cmd5_chris_slide(e)
    e.action_behavior = e.action_behavior + 1
    e.action_ticks_counter = 4
    set_velocity(e, e.pos_x, e.pos_z, e.target_x, e.target_z - 0x50, 4)
end

-- Then start animation 5.
local function cmd5_chris_wait(e)
    integrate(e)
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 then
        e.action_behavior = e.action_behavior + 1
        set_anim(e, 5, 0, 7)
        e.action_ticks_counter = 4
    end
end

-- The shared final step: drift out any remaining timer, play the animation
-- out.
local function cmd5_play(e)
    if e.action_ticks_counter ~= 0 then
        integrate(e)
        e.action_ticks_counter = e.action_ticks_counter - 1
    end
    if e:advance_anim(0x200) then
        command_done(e)
    end
end

-- Sub-state 1 re-dispatches on the step byte into the two per-character
-- tables.
local function cmd5_dispatch(e)
    local step = e.action_behavior
    if is_jill(e) then
        if step == 0 then
            cmd5_jill_start(e)
        else
            cmd5_play(e)
        end
        return
    end
    if step == 0 then
        cmd5_chris_slide(e)
    elseif step == 1 then
        cmd5_chris_wait(e)
    else
        cmd5_play(e)
    end
end

local CMD5 = { cmd5_begin, cmd5_dispatch, cmd5_chris_slide, cmd5_chris_wait, cmd5_play }

-- ---------------------------------------------------------------------------
-- The lever pull, shared by command 6's tail and command 8.
-- ---------------------------------------------------------------------------

-- Glide down, then restart animation 6 from frame 0 as the pull proper.
local function step_lever_move(e)
    integrate(e)
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 then
        e.ignore = e.ignore + 1
        set_anim(e, 6, 0, 7)
    end
    e:advance_anim(0x200)
end

-- Play it out. This step raises the done bit WITHOUT clearing the sub-state;
-- the driver resets it when the next command arrives.
local function step_lever_pull(e)
    if e:advance_anim(0x200) then
        e.behavior_flags = e.behavior_flags | 0x20
    end
    if e.animation_frame_id == 9 then
        e:play_sfx(2, 0x18)
    end
end

-- Command 6: like the lever reach but stopping at home Z - 0x28.
local function cmd6_reach(e)
    e.ignore = e.ignore + 1
    begin_reach(e, -0x28, 0, 6)
    set_velocity(e, e.pos_x, e.pos_z, e.target_x - 0x28, e.target_z - 0x28, 4)
    e.action_ticks_counter = 4
end

local CMD6 = { cmd6_reach, step_lever_move, step_lever_pull }

-- Command 8's chain, indexed by the sub-state; steps 1 and 2 are shared with
-- command 6.
local function lever_reach(e)
    e.ignore = e.ignore + 1
    begin_reach(e, -0x28, 0, 6)
    set_velocity(e, e.pos_x, e.pos_z, e.target_x - 0x28, e.target_z - 0x8C, 4)
    e.action_ticks_counter = 4
end

local LEVER = { lever_reach, step_lever_move, step_lever_pull }

-- ---------------------------------------------------------------------------
-- Command 7: hold, then drop the hand off the keyboard and let it hang,
-- looping a per-character animation. The sub-state stops at 3: the loop step
-- never advances it.
-- ---------------------------------------------------------------------------

local function cmd7_hold(e)
    e.ignore = e.ignore + 1
    e.action_ticks_counter = 0xA0
    e.blend_counter = (e.animation_id == 0) and 0 or 7
    set_anim(e, 0, 0, e.blend_counter)
end

-- After the hold, seed the Y drop and start the 12-frame slide to
-- (home X - 100, home Z - 400).
local function cmd7_drop(e)
    e:advance_anim(0x200)
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter ~= 0 then
        return
    end
    e.ignore = e.ignore + 1
    e.action_ticks_counter = 0x0C
    e.arm_y = i32(e.pos_y * 0x10000)
    e.arm_vel_y = -0xC0000
    set_velocity(e, e.pos_x, e.pos_z, e.target_x - 0x64, e.target_z - 0x190, 0x0C)
end

-- Gravity on Y while the slide runs, clamped to the spawn height (the
-- floor), then animation 2 from frame 0x0E.
local function cmd7_fall(e)
    e:advance_anim(0x80)
    if e.action_ticks_counter == 0 then
        e.ignore = e.ignore + 1
        e.pos_y = e.target_y
        set_anim(e, 2, 0x0E, 7)
        e.action_ticks_counter = 0x5A
        return
    end
    integrate(e)
    e.arm_y = i32(e.arm_y + e.arm_vel_y)
    e.arm_vel_y = i32(e.arm_vel_y + 0x28000)
    local y = sar(e.arm_y, 16)
    if y > e.target_y then
        y = e.target_y
    end
    e.pos_y = y
    e.action_ticks_counter = e.action_ticks_counter - 1
end

-- The hanging loop. The animation only runs while the frame is under a
-- per-character clamp, and the timer restarts it from frame 0.
local function cmd7_loop(e)
    local clamp = is_jill(e) and 0x0A or 0x0E
    if e.animation_frame_id < clamp then
        e:advance_anim(0x200)
    end
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter ~= 0 then
        return
    end
    e.action_ticks_counter = is_jill(e) and 0x0A or ((e:random() & 0x1F) + 0x0A)
    set_anim(e, e.animation_id, 0, 7)
end

local CMD7 = { cmd7_hold, cmd7_drop, cmd7_fall, cmd7_loop }

-- Command 0 for this arm: the same glide home, without the right arm's
-- "already within 200 units" shortcut.
local function cmd15_rest(e)
    if e.ignore == 0 then
        e.ignore = 1
        e.blend_counter = (e.animation_id == 0) and 0 or 7
        set_anim(e, 0, 0, e.blend_counter)
        e.action_ticks_counter = 8
        set_velocity(e, e.pos_x, e.pos_z, e.target_x, e.target_z, 8)
    end
    if e.action_ticks_counter ~= 0 then
        integrate(e)
        e.action_ticks_counter = e.action_ticks_counter - 1
    end
    e:advance_anim(0x200)
end

-- The nine-step shared chain: the three reaches and their move steps, the
-- log-in gesture, the typing play-out and the settle step command 4 never
-- reaches.
local CHAIN = {
    step_reach_a, step_move, step_reach_b, step_move, step_reach_c, step_move,
    step_login15, step_typing15, step_settle15,
}

-- Entry point into the chain per command 0..4.
local CMD_ENTRY = { -1, 0, 2, 4, 6 }

-- State 0: one-shot init. This arm also records its spawn height, which its
-- command-7 drop uses as the floor.
local function state_init(e)
    e.state = e.state + 1
    e.ignore = 0
    e.health = 1
    e.status_flags = (e.status_flags & 0x1F) | 4
    e.animation_id = 0
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0
    e:reset_joints()
    e:advance_anim(0x200)
    e.target_x = e.pos_x
    e.target_z = e.pos_z
    e.arm_pos_x = i32(e.pos_x * 0x10000)
    e.arm_pos_z = i32(e.pos_z * 0x10000)
    e.target_y = e.pos_y
end

-- State 1: the command driver.
local function state_run(e)
    if (e.behavior_flags & 0xA0) ~= 0 then
        e.ignore = 0
        return
    end
    if (e.behavior_flags & 0x40) ~= 0 then
        e.ignore = 0
        e.behavior_flags = e.behavior_flags & 0xBF
    end
    local cmd = e.behavior_flags & 0x0F
    local sub = e.ignore
    if cmd == 0 then
        cmd15_rest(e)
        return
    end
    if cmd == 5 then
        local handler = CMD5[sub + 1]
        if handler then
            handler(e)
        end
        return
    end
    if cmd == 6 then
        local handler = CMD6[sub + 1]
        if handler then
            handler(e)
        end
        return
    end
    if cmd == 7 then
        local handler = CMD7[sub + 1]
        if handler then
            handler(e)
        end
        return
    end
    if cmd == 8 then
        local handler = LEVER[sub + 1]
        if handler then
            handler(e)
        end
        return
    end
    if cmd > 4 then
        return
    end
    local slot = CMD_ENTRY[cmd + 1] + sub
    local handler = CHAIN[slot + 1]
    if handler then
        handler(e)
    end
end

function update(e)
    if e.state == 0 then
        state_init(e)
    else
        state_run(e)
    end
    if (e.behavior_flags & 0x80) ~= 0 then
        e.has_enter_switch_zone = 0
        return
    end
    e.has_enter_switch_zone = e:update_switch_zone()
end
