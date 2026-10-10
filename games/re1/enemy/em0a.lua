-- Adder, entity id 0x0a.
--
-- The venomous snakes. A four-level machine:
--
--   state            init / run / damaged / death
--   ignore           0 = the decision layer runs this frame, 1 = the
--                    behaviour owns the snake outright, anything else freezes
--   behavior_flags   the SPAWN KIND and the decision-table index:
--                      0 = already on the floor
--                      1 = hanging from the ceiling, drops when the player
--                          walks under it
--                      2 = ceiling ambush, teleports above the player and
--                          drops when the door-opening pose is held (or its
--                          fuse burns out)
--                      3 = hidden in the scenery, emerges when the player is
--                          close
--   action_behavior  the seven-entry behaviour table (chase, drop, turn away,
--                    bite, victory, idle, emerge) with action_state as the
--                    per-behaviour sub-state.
--
-- The run state refreshes two turn probes every frame: the slow one picks the
-- "lined up with the player" test in the ground decision, the fast one is the
-- bite's aim gate. Both live in locals because the whole decide-dispatch-bite
-- chain happens inside one update.
--
-- A shot snake retreats instead of dying: it sprays blood, hides every joint,
-- sinks out of sight and then resets to state 0, which re-runs init with fresh
-- health and a ceiling spawn kind. In the courtyard's water-gate room it comes
-- back as spawn kind 1, everywhere else as kind 2. The all-zero state dword
-- write at the end clears state, ignore, action_behavior and action_state.
--
-- All state lives on the Rust entity; this file keeps none, so the scripting
-- VM may be reset between any two updates. The original's `AD_UNK_C1` and
-- matrix scratch word writes are not modelled (nothing reads them).
--
-- The explosive-round death gores every joint through the engine's per-joint
-- object-effect pipeline; the port has no joint-object pass, so that branch
-- transcribes the state contract (shadow gone, behaviour 4, death flag) and
-- leaves the chunks to the renderer's documented deferral.

-- One word write over `action_behavior` + `action_state`: every decision-table
-- assignment zeroes the sub-state as it writes the behaviour.
local function set_behavior(e, behavior)
    e.action_behavior = behavior
    e.action_state = 0
end

-- The 16-bit signed view of a counter the original reads through a `short`.
local function signed_word(value)
    if value >= 0x8000 then
        return value - 0x10000
    end
    return value
end

-- Behaviour 0: chase. The turn rate and the approach cadence are rolled once
-- at setup, the yaw wobbles by two low-nibble draws every frame, and inside
-- 8000 units the heading follows the pathfinder bit - a snake with no route
-- holds its heading.
local function chase(e, px, _, pz)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0
        e.blend_counter = 3
        e.move_speed_current = 0x32 - (e:random() & 0xF)
        e.action_ticks_counter = (e:random() & 0xF) + 0x10
    elseif st ~= 1 then
        return
    end

    if e.player_distance < 8000 then
        local turn = e:turn_toward_target(px, pz, signed_word(e.action_ticks_counter))
        e.angle = e.angle + turn * e.path_bit
    else
        e:rotate_toward_target(px, pz, 0x40)
    end

    local wobble = (e:random() & 0xF) + (e:random() & 0xF)
    e.angle = e.angle + wobble

    e:advance_anim(0x400)
    e:move(0, e.move_speed_current)
end

-- Behaviour 1: the drop. The altitude accelerates as ticks^2 * 8 - a
-- quadratic, not a velocity - and the 180-degree Z spin bleeds off 0x55 a
-- frame. The landing frame sets Y and the spin to zero; the next frame hisses
-- and hands over to the floor decision layer.
local function drop(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.animation_id = 0
        e.action_ticks_counter = 0
        if e.pos_y < -0x1FB8 then
            e.pos_y = -0x1FB8
        end
        e.behavior_flags = 0
    elseif st == 2 then
        e:play_enemy_sound(2)
        e.ignore = 0
        set_behavior(e, 0)
        return
    elseif st ~= 1 then
        return
    end

    local ticks = signed_word(e.action_ticks_counter)
    e.pos_y = e.pos_y + ticks * ticks * 8
    e.action_ticks_counter = e.action_ticks_counter + 1

    e.roll = e.roll - 0x55
    if e.roll >= 0x8000 then
        e.roll = 0
    end

    if e.pos_y >= 0 then
        e.pos_y = 0
        e.roll = 0
        e.action_state = 2
    end

    e:advance_anim(0x400)
end

-- Behaviour 2: turn away. One of the two coil clips (rolled), then a flat
-- 180-degree snap and back to the decision layer.
local function turn_away(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = (e:random() & 1) + 1
        e.action_ticks_counter = e:random() & 0x3F
        e.blend_counter = 3
    elseif e.action_state ~= 1 then
        return
    end

    if e:advance_anim(0x400) then
        e.angle = e.angle + 0x800
        e.ignore = 0
        set_behavior(e, 0)
    end
end

-- Behaviour 3: the poison bite. The reach test runs every frame against
-- joint 1's world matrix with a 900-unit square box; the hit lands only on
-- animation frame 0xB and only when the fast turn probe reads 0 (the snake is
-- aimed), the box hit, the player is not already attacked and the pathfinder
-- bit is set. Poison is a double coin flip, so one bite in four envenomates.
local function bite(e, px, py, pz, fast)
    local st = e.action_state
    if st == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.death_timer = 0
        e.animation_id = 3
        e.blend_counter = 3
        e.action_state = 1
        e.move_speed_current = 0x32
        e:play_enemy_sound(0)
    elseif st == 2 then
        e.ignore = 1
        set_behavior(e, 2)
        return
    elseif st ~= 1 then
        return
    end

    -- The shared effect-seed block is the identity matrix's translation, so
    -- the reach probe composes a zero local offset.
    local reach = e:reach_test(1, 0, 0, 0, 900)
    local adv = e:advance_anim(0x400) and 1 or 0
    e.action_state = e.action_state + adv

    if e.animation_frame_id == 0xB and fast == 0 and reach
        and e.player_attacked == 0 and e.path_bit ~= 0 then
        e:play_enemy_sound(1)
        local c0 = e:random() & 1
        local c1 = e:random() & 1
        if c0 * c1 ~= 0 then
            e:poison_player()
        end
        e.player_attacked = 1
        e.player_action_behavior = 100
        e:hurt_player(6)
        e:play_3d_sound_at(3, 0, px, py, pz)
        e:spawn_player_effect(0, 1, 0, -520, 0, e.player_angle)
    end

    e:move(0, e.move_speed_current)
end

-- Behaviour 4: victory. The player is dead: the strike animation loops
-- forever with no reach test and no exit.
local function victory(e)
    if e.action_state == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.death_timer = 0
        e.animation_id = 3
        e.blend_counter = 3
        e.action_state = 1
        e.move_speed_current = 0x32
        e:play_enemy_sound(0)
    elseif e.action_state ~= 1 then
        return
    end

    e:advance_anim(0x400)
    e:move(0, e.move_speed_current)
end

-- Behaviour 5: idle. The hold state for a snake that has not been triggered;
-- nothing runs at all.
local function idle()
end

-- Behaviour 6: emerge. Sixty frames of the walk clip, after which the snake
-- becomes tangible (collision re-enabled, hit latch cleared) and hands over to
-- the floor decision layer.
local function emerge(e)
    local st = e.action_state
    if st == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.animation_id = 0
        e.blend_counter = 3
        e.move_speed_current = 0x1E
        e.action_ticks_counter = 0x3C
        e.behavior_flags = 0
    elseif st ~= 1 then
        return
    end

    e:advance_anim(0x400)
    e:move(0, e.move_speed_current)

    local ticks = signed_word(e.action_ticks_counter)
    e.action_ticks_counter = ticks - 1
    if ticks == 0 then
        e.hit_state = 0
        e.sca_active = 1
        e.ignore = 0
        set_behavior(e, 0)
    end
end

local BEHAVIORS = { chase, drop, turn_away, bite, victory, idle, emerge }

-- Spawn kind 0: a snake already on the floor. The order matters - the
-- "close but no route" test can pick behaviour 2 and then be overridden in
-- the same frame by the bite test, which is why a cornered snake still
-- strikes.
local function decide_ground(e, turn_slow)
    if e.player_distance < 10000 and e.path_bit == 0 then
        e.ignore = 1
        set_behavior(e, 2)
    end

    if (e.player_distance < 2000 and turn_slow == 0) or e.sca_touch ~= 0 then
        e.ignore = 1
        set_behavior(e, 3)
        e.sca_touch = 0
    else
        -- Pinned against geometry for a full second: turn and try again.
        if e.stuck_frames > 0x3C then
            e.ignore = 1
            set_behavior(e, 2)
            e.stuck_frames = 0
        end
        if e.player_health < 0 then
            e.ignore = 1
            set_behavior(e, 4)
        end
    end
end

-- Spawn kind 1: hangs still until the player walks under it.
local function decide_ceiling_near(e)
    set_behavior(e, 5)
    if e.player_distance < 3000 then
        e.ignore = 1
        set_behavior(e, 1)
    end
end

-- Spawn kind 2: the scripted ambush. It waits for the player to hold the
-- door-opening pose within 5000 units, or for the fuse to burn out, and then
-- teleports itself within 100 units of the player before falling. The fuse
-- only ticks on the frames the pose test fails.
local function decide_ceiling_ambush(e, _, px, pz)
    set_behavior(e, 5)

    local drop
    if e.player_action_behavior == 0x14 and e.player_action_state == 0
        and e.player_distance < 5000 then
        drop = true
    else
        local ticks = signed_word(e.action_ticks_counter)
        e.action_ticks_counter = ticks - 1
        drop = ticks == 0
    end

    if drop then
        e.pos_x = (e:random() & 1) * 100 + px
        e.pos_z = (e:random() & 1) * 100 + pz
        e.ignore = 1
        set_behavior(e, 1)
    end
end

-- Spawn kind 3: lies intangible in the scenery until the player is within
-- 4500 units, then emerges.
local function decide_hidden(e)
    set_behavior(e, 5)
    if e.player_distance < 0x1194 then
        e.ignore = 1
        set_behavior(e, 6)
    end
end

local DECIDES = { decide_ground, decide_ceiling_near, decide_ceiling_ambush, decide_hidden }

-- Refresh the distance to the player and run this spawn kind's decision.
-- Every absolute value is taken on the full 32-bit difference and only the
-- sum truncates to 16 bits; a player on another floor reports a flat 4999,
-- which sits between the bite range and the notice ranges.
local function decide_action(e, turn_slow, px, py, pz)
    if py == 0 then
        local dz = pz - e.pos_z
        if dz < 0 then
            dz = -dz
        end
        local dx = px - e.pos_x
        if dx < 0 then
            dx = -dx
        end
        e.player_distance = (dx + dz) & 0xFFFF
    else
        e.player_distance = 4999
    end

    local decide = DECIDES[e.behavior_flags + 1]
    if decide then
        decide(e, turn_slow, px, pz)
    end
end

-- Refresh the obstacle pathfinder, keep its bit 0 in the scratch byte and
-- jump into the behaviour. The whole decide-dispatch chain is one call, so
-- the fast turn probe travels as the behaviour's `fast` argument.
local function dispatch(e, px, py, pz, fast)
    local path = e:pathfind_update(px, pz)
    e:pathfind_keep(path)

    local behavior = BEHAVIORS[e.action_behavior + 1]
    if behavior then
        behavior(e, px, py, pz, fast)
    end
end

-- State 0: full setup. Runs on spawn and again after a retreat; the health
-- roll is four draws, the first thrown away, giving 10..52. Any non-zero
-- spawn kind starts the snake at the ceiling, spun 180 degrees about Z, with
-- a 180..240 frame ambush fuse; kind 3 instead lies on the floor, intangible
-- and marked as not yet emerged.
local function init(e)
    e.state = 1
    e.ignore = 0
    e.action_behavior = 0
    e.action_state = 0
    e.action_ticks_counter = 0
    e.hit_state = 0
    e.status_flags = 1
    e:reset_joints()

    -- The ground quad: no extents until the run state sizes it from the head
    -- joint's height, near-black tint.
    e:set_shadow_offset(0, 0, 0)
    e.shadow_half_x = 0
    e.shadow_half_z = 0
    e.shadow_tint = 0x202020

    e:random()
    local h0 = e:random() & 7
    local h1 = e:random() & 7
    local h2 = e:random() & 7
    e.health = h0 * 2 + h1 * 2 + h2 * 2 + 10

    e.animation_frame_id = 0
    e.timing_control = 0
    e.animation_id = 0
    e:advance_anim(0x40)

    e:set_sca(200, 0, 0, 0, 0)
    e.joint_scale = 0x3000
    e.stuck_frames = 0
    e.sca_active = 1

    if e.behavior_flags ~= 0 then
        e.action_ticks_counter = (0x18 - (e:random() & 6)) * 10
        e.pitch = 0
        e.roll = 0x800
        if e.pos_y == 0 then
            e.pos_y = -0x1FB8
        end
    end

    if e.behavior_flags == 3 then
        e.roll = 0
        e.pos_y = 0
        e.sca_active = 0
        e.hit_state = 1
    end
end

-- State 1: the main driver.
local function run(e)
    local px, py, pz = e:player_pos()
    local turn_slow = e:turn_toward_target(px, pz, 0x100)
    local fast = e:turn_toward_target(px, pz, 0x200)

    if e.ignore == 0 then
        decide_action(e, turn_slow, px, py, pz)
        dispatch(e, px, py, pz, fast)
    elseif e.ignore == 1 then
        dispatch(e, px, py, pz, fast)
    end

    e.status_flags = e.status_flags & 0x1F

    -- Only a snake on the player's floor notices it.
    if e.pos_y == 0 and py == 0 then
        e:check_visual_range(5000)
    end

    -- The shadow tracks the head joint's world height; it is skipped
    -- entirely above 4000 units, and the arithmetic shift is what shrinks it
    -- as the snake rises (Y is negative up).
    if e.pos_y > -4000 then
        local hi = (e:joint_world_y(0) // 16) + 500
        if hi < 0 then
            hi = 300
        end
        e.shadow_half_x = hi + 500
        e.shadow_half_z = hi
    end

    if e.room_collision ~= 0 then
        e.stuck_frames = e.stuck_frames + 1
    else
        e.stuck_frames = 0
    end
end

-- State 2's only behaviour: play the strike animation as a recoil and hide
-- the four tail joints for the duration, which is what makes a hit snake look
-- shortened.
local function flinch(e)
    local st = e.action_state
    if st == 0 then
        e.animation_id = 3
        e.animation_frame_id = 0
        e.timing_control = 0
        e.death_timer = 0
        e.blend_counter = 3
        e.action_state = 1
        e.action_ticks_counter = (e:random() & 0x1F) + 0x10
        for joint = 8, 11 do
            e:set_joint_visible(joint, false)
        end
    elseif st == 2 then
        local ticks = signed_word(e.action_ticks_counter)
        e.action_ticks_counter = ticks - 1
        if ticks ~= 0 then
            return
        end
        e.state = 1
        e.ignore = 0
        set_behavior(e, 0)
        e.hit_state = 0
        return
    elseif st ~= 1 then
        return
    end

    local adv = e:advance_anim(0x400) and 1 or 0
    e.action_state = e.action_state + adv
    e:check_special_weapon()
end

-- State 2: the hit reaction, then the shadow resize that runs for every
-- behaviour while the state lasts.
local function damaged(e)
    if e.action_behavior == 0 then
        flinch(e)
    end

    local hi = (e:joint_world_y(0) // 16) + 400
    if hi < 0 then
        hi = 200
    end
    e.shadow_half_x = hi + 400
    e.shadow_half_z = hi
end

-- Death, behaviour 0: writhe. The weapon that killed the snake is in the top
-- five bits of hit_state: the explosive round gores every joint and ends the
-- branch immediately; any other firearm switches to the retreat; the knife
-- and an unarmed kill fall into the writhe animation, which ends in the blood
-- pool and behaviour 4.
local function writhe(e)
    local st = e.action_state
    if st == 0 then
        e.animation_id = 4
        e.animation_frame_id = 0x1E
        e.timing_control = 0
        e.death_timer = 0
        e.blend_counter = 3
        e.action_state = 1
        e.status_flags = e.status_flags | 0x02
        e.status_flags = e.status_flags | 0x08
        e.move_speed_current = 0x28
        e.action_ticks_counter = (e:random() & 0xF) + 0x10

        local weapon = e.hit_state & 0xF8
        if weapon == 0x38 then
            -- The per-joint gore chunks are engine-bound; the state contract
            -- is kept and the chunks stay with the renderer deferral.
            e.shadow_half_x = 0
            e.shadow_half_z = 0
            e.action_behavior = 4
            e:raise_death_event()
            return
        end
        if weapon > 8 then
            e.shadow_half_x = 0
            e.shadow_half_z = 0
            e.action_behavior = 3
            return
        end
    elseif st == 2 then
        -- The blood pool spreading under the corpse.
        e:adjust_shadow_size(0x16, 0x0B)
        local ticks = signed_word(e.action_ticks_counter)
        e.action_ticks_counter = ticks - 1
        if ticks ~= 0 then
            return
        end
        e.action_behavior = 4
        e:raise_death_event()
        return
    elseif st ~= 1 then
        return
    end

    if e:advance_anim(0x400) then
        -- The shadow becomes the blood pool: recoloured, reseeded small, then
        -- grown by the adjust above.
        e.shadow_tint = 0x00FFFF50
        e.shadow_half_x = 0x14
        e.shadow_half_z = 10
        e.action_state = 2
    end

    -- The writhe stops sliding once the animation timer passes 0x23.
    if e.timing_control < 0x23 then
        e:move(0, e.move_speed_current)
    end
end

-- Death, behaviour 3: the retreat. Not a corpse handler - the snake sprays
-- blood at the head and joint 5, thrashes, hides all eleven joints, sinks to
-- -20000, waits out a 210..241 frame cooldown and resets itself to state 0
-- with a ceiling spawn kind, so init rolls fresh health and the snake comes
-- back. The water-gate room gets spawn kind 1, everywhere else kind 2.
local function retreat(e)
    local st = e.action_state
    if st == 0 then
        e.animation_id = 4
        e.animation_frame_id = 0
        e.timing_control = 0
        e.death_timer = 0
        e.blend_counter = 3
        e.action_state = 1
        e.action_ticks_counter = (e:random() & 0x1F) + 0x10

        local yaw = signed_word((e.player_angle - e.angle + 0x800) & 0xFFFF)
        e:spawn_joint_effect(0, 7, 0, yaw)
        e:spawn_joint_effect(0, 7, 5, yaw)
    end

    if st == 0 or st == 1 then
        e.joint_scale = e.joint_scale - 0xF5
        local adv = e:advance_anim(0x400) and 1 or 0
        e.action_state = e.action_state + adv
        return
    end

    if st == 2 then
        for joint = 0, 10 do
            e:set_joint_visible(joint, false)
        end
        e.action_state = 3
        e.action_ticks_counter = (e:random() & 0x1F) + 0xD2
        return
    end

    if st == 3 then
        e.pos_y = -20000
        e.action_state = 4
    end

    if st == 3 or st == 4 then
        local ticks = signed_word(e.action_ticks_counter)
        e.action_ticks_counter = ticks - 1
        if ticks == 0 then
            for joint = 0, 10 do
                e:set_joint_visible(joint, true)
            end
            e.behavior_flags = (e.room == 1) and 1 or 2
            e.state = 0
            e.ignore = 0
            e.action_behavior = 0
            e.action_state = 0
        end
        return
    end
end

-- State 3: the death fork.
local function death(e)
    if e.action_behavior == 0 then
        writhe(e)
    elseif e.action_behavior == 3 then
        retreat(e)
    end
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

        if e.sca_active ~= 0 then
            e.sca_touch = e:separate() and 1 or 0
            e.room_collision = e:resolve_collision()
        end
    end

    -- Any snake that has not become a floor snake yet is excluded from the
    -- camera switch zones, which also suppresses its ground shadow.
    e:update_switch_zone()
    if e.behavior_flags ~= 0 then
        e.has_enter_switch_zone = 0
    end
end
