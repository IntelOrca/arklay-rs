-- Plant 42's root mass, entity id 0x0e.
--
-- A stationary root cluster at the base of the giant plant, rendered through
-- the ordinary entity path and doubled by a ground shadow quad. The spawn
-- record's behaviour byte picks the whole life cycle:
--
--   0x80  dormant   - no motion at all: the mass just animates and blocks.
--                     A fresh boot takes this branch.
--   0x01  retracted - one-shot collapse: the mass shrinks to the phase-2
--                     threshold, goes intangible and darkens, then never
--                     moves again. Scenario flag 0 selects it.
--   0x00  writhe    - the full four-phase cycle: start, rise, shrink, sink.
--                     No shipped room selects it, but the machine is kept
--                     whole so an edited spawn kind runs it.
--
-- States 0 init / 1 idle / 2 damage reset / 3-5 inert, and the four cycle
-- phases double as raw state values 6-9. The cycle counter lives in
-- `ignore`; every other timer lives in the shared scratch words, so a
-- scripting-VM reset between any two ticks cannot change behaviour.
--
-- States 2-5 are the damage routes: a survived weapon hit writes state 2 and
-- a killing blow state 3, and states 3-5 never return. Nothing in the port
-- fires weapon damage yet, so those reactions are transcribed but stay
-- unreached until the combat layer lands.

-- Phase 0: one-shot cycle setup. The writhe pair starts at rest, the flash
-- cadence is armed, the mass turns intangible and darkens its health, and a
-- cycle-start cue is requested (the original's id is past the enemy-sound
-- helper's accepted range, so it queues nothing).
local function phase_start(e)
    e.ignore = e.ignore + 1
    e.writhe_velocity = 0
    e.writhe_amplitude = 0
    e.action_ticks_counter = 8
    e.tint_flashes = 6
    e.status_flags = e.status_flags | 0x02
    e.health = -1
    e:play_enemy_sound(0x18)
end

-- Phase 1: the writhe amplitude ramps up by 0x200 a frame to 0x1600.
local function phase_rise(e)
    e.writhe_amplitude = e.writhe_amplitude + 0x200
    if e.writhe_amplitude >= 0x1600 then
        e.writhe_amplitude = 0x1600
        e.ignore = e.ignore + 1
    end
end

-- Phase 2: the joint-scale word shrinks 8 a frame from full size while the
-- shadow follows it down 10 a frame. Every ninth frame of the phase packs
-- the flash cadence word to zero, which fires one of the six model-tint
-- flashes; the phase ends when the scale reaches 0x9C4.
local function phase_shrink(e)
    if e.joint_scale <= 0x9C4 then
        e.ignore = e.ignore + 1
        e.action_ticks_counter = 1
        e.tint_flashes = 1
        return
    end
    e:adjust_shadow_size(-10, -10)
    e.joint_scale = e.joint_scale - 8
    local ticks = e.action_ticks_counter
    e.action_ticks_counter = ticks - 1
    if ticks == 0 and e.tint_flashes ~= 0 then
        e:tint_model(0, -1, -2, 0, 0x100)
        e.action_ticks_counter = 8
        e.tint_flashes = e.tint_flashes - 1
    end
end

-- Phase 3: the amplitude bleeds off 0x70 a frame and the sink wobble ticks
-- up every fourth frame. Bottoming out raises the spawn record's death
-- event flag and ends the cycle.
local function phase_sink(e)
    e.writhe_amplitude = e.writhe_amplitude - 0x70
    e.action_ticks_counter = e.action_ticks_counter + 1
    if e.action_ticks_counter % 4 == 0 then
        e.sink_wobble = e.sink_wobble + 1
    end
    if e.writhe_amplitude <= 0 then
        e.ignore = e.ignore + 1
        e:raise_death_event()
    end
end

local PHASES = { phase_start, phase_rise, phase_shrink, phase_sink }

-- The behaviour-0 writhe sub-behaviour: run the current phase, bob the
-- altitude on the velocity/amplitude pair, then roll the two groan cues.
local function move_a(e)
    local phase = e.ignore
    if phase > 3 then
        return
    end
    PHASES[phase + 1](e)

    -- The velocity is an i16; its arithmetic shift down 8 lands on the
    -- altitude, and the amplitude reloads it with the sign flipped.
    e.pos_y = e.pos_y + e.writhe_velocity // 256
    local velocity = e.writhe_amplitude
    if e.writhe_velocity > 0 then
        velocity = -velocity
    end
    e.writhe_velocity = velocity

    e.groan_timer = e.groan_timer - 1
    if e.groan_timer == 0 then
        e:play_3d_sound(2, 0x19)
        e.groan_timer = (e:random() & 7) + (e:random() & 7) + 0xc
    end
    if (e:random() & 0x1f) == 1 then
        e:play_3d_sound(2, 0x19)
    end
end

-- The retract sub-behaviour, one shot: park the scale at the phase-2 exit
-- threshold, go intangible, darken the model a touch and collapse the
-- shadow. The phase latch then keeps every later frame inert.
local function move_b(e)
    if e.ignore ~= 0 then
        return
    end
    e.ignore = 1
    e.joint_scale = 0x9C4
    e.status_flags = e.status_flags | 0x02
    e.health = -1
    e:retarget_tint(0, -6, -12, 0, 0x100)
    e:adjust_shadow_size(-2000, -2000)
end

-- State 0: full setup. The state word drops to idle, the collision record
-- becomes the fat two-metre trunk, the frozen spawn point is stored for the
-- position pin, and the ground quad is built at 0xC00 squared with the dark
-- grey tint.
local function init(e)
    e.state = 1
    e.ignore = 0
    e.action_behavior = 0
    e.action_state = 0
    e.health = 1
    e:set_sca(0x07D0, 0x1770, 0, 0, 0)
    e.status_flags = (e.status_flags & 0x1F) | 0x04
    e.animation_id = 0
    e.animation_frame_id = 0
    e.timing_control = 0
    e.blend_counter = 0
    e:reset_joints()
    e.joint_scale = 0x1000
    e.hit_state = 1
    e.stored_pos_x = e.pos_x
    e.stored_pos_z = e.pos_z
    -- The paired movement-scratch word at entity +0x172 is cleared.
    e.is_moving = 0
    e.move_max_steps = 0
    e.sink_wobble = 2
    e.groan_timer = 0x28
    e:set_shadow_offset(0, 0, -0x50)
    e.shadow_half_x = 0xC00
    e.shadow_half_z = 0xC00
    e.shadow_tint = 0x404040
end

-- State 1: keep the weapon-targetable and ranged bits raised, select the
-- sub-behaviour from the spawn kind, then advance the clip.
local function idle(e)
    e.status_flags = e.status_flags | 0xE0
    local behavior = e.behavior_flags
    if behavior == 0 then
        move_a(e)
    elseif behavior ~= 0x80 then
        move_b(e)
    end
    e:advance_anim(0x100)
end

-- State 2: a survived weapon hit drops straight back to idle and rewinds the
-- cycle so the next writhe starts from phase 0.
local function hit_reset(e)
    e.state = 1
    e.ignore = 0
    e.hit_state = 0
end

function update(e)
    -- Monsters freeze while a message masks the monster bit: the state
    -- machine and the whole collision pass are inside the gate.
    if not e.monster_paused then
        local state = e.state
        if state == 0 then
            init(e)
        elseif state == 1 then
            idle(e)
        elseif state == 2 then
            hit_reset(e)
        elseif state >= 6 and state <= 9 then
            -- The four phase handlers double as raw state values.
            PHASES[state - 5](e)
        end
        e:separate()
    end

    -- Every shipped spawn kind is non-zero, so the mass stays pinned to the
    -- point init froze, outside the message gate.
    if e.behavior_flags ~= 0 then
        e.pos_x = e.stored_pos_x
        e.pos_z = e.stored_pos_z
    end

    -- The original also clears its matrix scratch word here; the port has no
    -- equivalent field and nothing reads it. Recompute the camera-zone shadow
    -- bit instead; the renderer queues the ground quad while it is set.
    e:update_switch_zone()
end
