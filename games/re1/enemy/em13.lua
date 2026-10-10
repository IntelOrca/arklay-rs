-- Spider web, entity id 0x13.
--
-- A stationary six-joint web that blocks a doorway. Only the knife (weapon 1)
-- and the fire/launcher weapons (weapon 6 and up) can destroy it: every hit
-- re-derives how many strands have burned away from the remaining health and
-- clears those joints' visibility, and the killing hit raises the spawn
-- record's death flag, which the room script polls to open the door.
--
-- State 0 init / 1 idle / 2 damaged / 3 destroy / 4 gone. Unlike the monster
-- drivers this entity's update has no message gate: it keeps running behind a
-- message window.
--
-- All state lives on the Rust entity; this file keeps none, so the scripting
-- VM may be reset between any two updates.

local JOINTS = 6

-- Clear the visibility of the top `burnt` strands, joint 5 downwards. Joint 0
-- is only ever cleared by the destroy state.
local function burn_joints(e, burnt)
    for v = burnt - 1, 0, -1 do
        e:set_joint_visible(5 - v, false)
    end
end

function update(e)
    local state = e.state
    if state == 0 then
        -- Init: back to state 1 with the other three state bytes zeroed.
        e.state = 1
        e.ignore = 0
        e.action_behavior = 0
        e.action_state = 0
        -- The rotation X/Z components are zeroed; the scripted placement yaw
        -- between them stays.
        e.pitch = 0
        e.roll = 0
        e:reset_joints()
        -- The long thin ground quad across the doorway, untinted.
        e.shadow_half_x = 10
        e.shadow_half_z = 1000
        e.shadow_tint = 0xFFFFFF
        e.health = 0x37
        e.joint_scale = 0x1000
        -- SCA cylinder: the local X offset differs per scenario, the radius
        -- is 500 and the half height 0.
        local side = (e.player_character == 0) and -700 or -500
        e:set_sca(500, 0, side, 0, 0)
        e.animation_id = 0
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0
        e:advance_anim(0x400)
    elseif state == 1 then
        -- Keep bits 0-4 (the intangible bit once destroyed survives) and
        -- force the ALIGNED bit that makes the web a valid weapon target.
        e.status_flags = (e.status_flags & 0x1F) | 0x40
    elseif state == 2 then
        -- Survived a hit: straight back to idle. The top bits of hit_state
        -- are the weapon id; the knife and weapon 6+ cut or burn strands, the
        -- handgun through magnum family leaves the web untouched.
        e.state = 1
        e.ignore = 0
        e.action_behavior = 0
        e.action_state = 0
        local weapon = e.hit_state & 0xF8
        if weapon == 0x08 or weapon > 0x28 then
            local hp = e.health
            local burnt = 0
            if hp < 0x32 then burnt = 1 end
            if hp < 0x28 then burnt = 2 end
            if hp < 0x1E then burnt = 3 end
            if hp < 0x14 then burnt = 4 end
            if hp < 0x0A then burnt = 5 end
            burn_joints(e, burnt)
        end
        -- Cleared even for the ignored weapons, or the hit gate would never
        -- let the web be hit again.
        e.hit_state = 0
    elseif state == 3 then
        -- The killing hit: every strand goes, the intangible bit stops the
        -- web blocking the doorway, and the death flag opens the room script.
        for j = JOINTS - 1, 0, -1 do
            e:set_joint_visible(j, false)
        end
        e.status_flags = e.status_flags | 0x02
        e:raise_death_event()
        e.state = 4
        e.ignore = 0
        e.action_behavior = 0
        e.action_state = 0
    end
    -- Tail: the entity's matrix scratch word is zeroed (not modelled), then
    -- the camera-switch-zone shadow bit is recomputed. The renderer queues
    -- the ground quad while that bit is set, like the fade-sprite queue.
    e:update_switch_zone()
end
