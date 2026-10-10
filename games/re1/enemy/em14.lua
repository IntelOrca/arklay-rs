-- Computer arm, entity id 0x14 (the right forearm at the lab terminal).
--
-- Not a monster: this is a fixed-point prop the room-5060 terminal drives by
-- writing a command into `behavior_flags` and polling bit 0x20 for "done".
-- The protocol is:
--   bit 0x80  parked - the arm does nothing and leaves the switch zone
--   bit 0x40  a new command: consume the latch and restart the sub-state
--   bit 0x20  set by the arm when the command completes
--   bits 0-3  the command index
--
-- Commands 1..7 are entry points into one shared twenty-step chain: the three
-- typing reaches and their move steps, the login gesture, the keypress and
-- its voice wait, the lift/withdraw/settle sequence, the far reach and the
-- held key. Command 0 glides home and idles. Command 8 does not exist in this
-- arm's command table: the index falls off the table into the first entry of
-- the chain, so it re-runs reach A every frame without ever completing - the
-- exact loop the terminal's second-door command would hang on. Command 5 is a
-- no-op for Jill on this arm.
--
-- The arm integrates a 16.16 fixed-point position and writes the integer part
-- back to the entity position. All arithmetic is the original's 32-bit
-- wrapping/truncating integer maths (the `i32`/`trunc_div`/`sar` helpers).
--
-- All state lives on the Rust entity; this file keeps none.

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

-- The three reach offsets are per-arm; this arm's keep the hand on its own
-- half of the keyboard.
local REACH = { { -0x28, 0x28 }, { 0x28, 0 }, { -0x14, -0x28 } }

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

-- chain[6]: the log-in gesture. The arm settles onto the keyboard and speaks;
-- the shared main-state flag is the handshake with the terminal's voice
-- sequence. This arm raises it and waits for it to clear.
local function step_login(e)
    e.ignore = e.ignore + 1
    e.action_behavior = 0
    set_anim(e, 4, 0, 7)
    if not is_jill(e) then
        set_velocity(e, e.pos_x, e.pos_z, e.pos_x, e.target_z + 200, 0x12)
        e:play_voice(0xB8)
    else
        e:play_voice(0xB9)
    end
    e.voice_playing = true
end

local step_press, step_wait_voice

-- chain[7]: Jill just plays the animation out; Chris re-dispatches on the
-- step byte into the press/voice-wait pair.
local function step_typing(e)
    if is_jill(e) then
        if e.action_behavior == 0 then
            if e:advance_anim(0x200) then
                e.action_behavior = e.action_behavior + 1
            end
        elseif not e.voice_playing then
            command_done(e)
        end
        return
    end
    if e.action_behavior == 0 then
        step_press(e)
    else
        step_wait_voice(e)
    end
end

-- chain[8]: the keypress itself. Frames 0x15 and 0x22 are the two contact
-- points in the animation.
function step_press(e)
    local frame = e.animation_frame_id
    if frame < 0x12 then
        integrate(e)
    end
    if frame == 0x15 or frame == 0x22 then
        e:play_sfx(2, 0x1A)
    end
    if e:advance_anim(0x200) then
        e.action_behavior = e.action_behavior + 1
        e.action_ticks_counter = 4
    end
end

-- chain[9].
function step_wait_voice(e)
    if not e.voice_playing then
        command_done(e)
    end
end

-- chain[10].
local function step_lift(e)
    e.ignore = e.ignore + 1
    e.action_ticks_counter = 4
    set_velocity(e, e.pos_x, e.pos_z, e.target_x, e.target_z + 0x50, 4)
end

-- chain[11].
local function step_lift_wait(e)
    integrate(e)
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 then
        e.ignore = e.ignore + 1
        set_anim(e, 5, 0, 7)
    end
end

-- chain[12].
local function step_withdraw(e)
    if not is_jill(e) and e.animation_frame_id == 0x0F then
        e:play_sfx(2, 0x1C)
    end
    if e:advance_anim(0x200) then
        e.ignore = e.ignore + 1
        e.action_ticks_counter = 4
        set_velocity(e, e.pos_x, e.pos_z, e.target_x, e.target_z, 4)
        set_anim(e, 0, 0, 7)
    end
end

-- chain[13].
local function step_settle(e)
    integrate(e)
    e:advance_anim(0x200)
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 then
        command_done(e)
    end
end

-- chain[14]: the right arm's far reach, down and forward. Chris 2, Jill 3.
local function step_reach_far(e)
    e.ignore = e.ignore + 1
    begin_reach(e, -0x14, 0xA0, is_jill(e) and 3 or 2)
end

-- chain[16].
local function step_hold(e)
    e.ignore = e.ignore + 1
    e.action_ticks_counter = 0x96
    e.blend_counter = (e.animation_id == 0) and 0 or 7
    set_anim(e, 0, 0, e.blend_counter)
end

-- chain[17].
local function step_hold_wait(e)
    e:advance_anim(0x200)
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter ~= 0 then
        return
    end
    e.ignore = e.ignore + 1
    e.animation_id = 4
    e.animation_frame_id = is_jill(e) and 0 or 0x28
    e.timing_control = 0
    e.blend_counter = 0x1F
    e.action_ticks_counter = 0x18
    set_velocity(
        e,
        e.pos_x,
        e.pos_z,
        e.target_x - 100,
        e.target_z + 0x8C + (is_jill(e) and 0 or -0x46),
        0x18
    )
end

-- chain[18].
local function step_hold_move(e)
    e:advance_anim(0x80)
    if not is_jill(e) then
        e.animation_frame_id = 0x28
    elseif e.animation_frame_id > 5 then
        e.animation_frame_id = 5
    end
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter ~= 0 then
        integrate(e)
        return
    end
    e.ignore = e.ignore + 1
end

-- chain[19]: terminal state, nothing to do.
local function step_idle_end(_e)
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
    -- The keystroke click, on whichever animation frame makes contact.
    if not is_jill(e) then
        if e.animation_frame_id == 8 then
            e:play_sfx(2, 0x18)
        end
    elseif e.animation_frame_id == 3 or e.animation_frame_id == 13 then
        e:play_sfx(2, 0x18)
    end
end

local CHAIN = {
    step_reach_a, step_move, step_reach_b, step_move, step_reach_c, step_move,
    step_login, step_typing, step_press, step_wait_voice,
    step_lift, step_lift_wait, step_withdraw, step_settle,
    step_reach_far, step_move, step_hold, step_hold_wait, step_hold_move,
    step_idle_end,
}

-- Entry point into the chain per command; -1 is "handled separately".
local CMD_ENTRY = { -1, 0, 2, 4, 6, 10, 14, 16 }

-- Command 0: return to rest and idle there. Already home (within 200 units,
-- Manhattan) skips the glide.
local function cmd_rest(e)
    if e.ignore == 0 then
        e.ignore = 1
        e.blend_counter = (e.animation_id == 0) and 0 or 7
        e.animation_id = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.action_ticks_counter = 8
        set_velocity(e, e.pos_x, e.pos_z, e.target_x, e.target_z, 8)
        local dz = e.pos_z - e.target_z
        local dx = e.pos_x - e.target_x
        local dist = ((dz < 0) and -dz or dz) + ((dx < 0) and -dx or dx)
        if dist < 200 then
            e.action_ticks_counter = 0
        end
    end
    if e.action_ticks_counter ~= 0 then
        integrate(e)
        e.action_ticks_counter = e.action_ticks_counter - 1
    end
    e:advance_anim(0x200)
end

-- State 0: one-shot init. Home is the spawn point; the fixed-point position
-- starts there at 16.16.
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
end

-- State 1: the command driver.
local function state_run(e)
    -- 0x80 parked, or 0x20 already done: idle.
    if (e.behavior_flags & 0xA0) ~= 0 then
        e.ignore = 0
        return
    end
    -- 0x40: a command the terminal has just issued; consume the latch.
    if (e.behavior_flags & 0x40) ~= 0 then
        e.ignore = 0
        e.behavior_flags = e.behavior_flags & 0xBF
    end
    local cmd = e.behavior_flags & 0x0F
    local sub = e.ignore
    if cmd == 0 then
        cmd_rest(e)
        return
    end
    -- This arm has no separate animation for command 5 when the player is
    -- Jill.
    if cmd == 5 and is_jill(e) then
        return
    end
    if cmd > 8 then
        return
    end
    -- Command 8 falls off the command table into the first entry of the step
    -- chain: reach A re-runs every frame and never raises the done bit.
    if cmd == 8 then
        step_reach_a(e)
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
    -- The original also clears its matrix scratch dword; the port has no
    -- equivalent field and nothing reads it.
    if (e.behavior_flags & 0x80) ~= 0 then
        e.has_enter_switch_zone = 0
        return
    end
    e.has_enter_switch_zone = e:update_switch_zone()
end
