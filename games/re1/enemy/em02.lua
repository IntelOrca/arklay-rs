-- Cerberus, the zombie dog, entity id 0x02 (model em1002).
--
-- The dog is a two-level machine. The entity state byte picks init / the
-- behaviour driver / the hit reaction / death, and while the driver runs the
-- `ignore` byte picks the behaviour: patrol, chase, leap, strafe, alert,
-- maul, bite, scripted idle or stalk. Each behaviour then runs its own little
-- sub-machine off `action_behavior`.
--
-- What makes the dog special is that nearly all of its steering is done by
-- PROBING: a forward probe steps the dog 500 units along its facing, asks the
-- room collision whether the stepped point is inside geometry, and steps
-- straight back; a turn probe wraps that with a temporary yaw and speed scale
-- so the AI can ask "would I hit a wall if I turned left at half speed?"
-- before committing. The answers accumulate in the probe-history word, and
-- the probe helpers live in Rust so the room-collision conventions stay in
-- one place.
--
-- Dispatch aliasing, exactly like the original: the state table and the
-- behaviour table are ONE pointer block read through two bases - the
-- behaviour view is the state table biased by five, indexed by `ignore`. The
-- sixteen-slot block is spelled out once and sliced, so the aliasing is the
-- original's, not a copy with matching values.
--
-- The `behavior_flags` spawn kind is the selector table's index: 0..3 walk or
-- run patrol (3 tails into stalk), 4 chases, 5/6/7 are the scripted entrances
-- (a running leap, a window crash and a ceiling drop), 8 bites, 9 strafes,
-- 10/11 alert, 12 stalks, and 0x80 is the script-driven idle that bypasses
-- the table. The five sub-machines are action_behavior-indexed.
--
-- One file-scope latch is shared by every dog in the room: "a dog has already
-- barked its approach". The port keeps it on the game state, the init clears
-- it and the stalk commit tests and sets it. A room load never touches it.
--
-- All mutable state lives on the Rust entity; this script keeps none, so the
-- scripting VM may be reset between any two updates.

-- ---------------------------------------------------------------------------
-- Constants
-- ---------------------------------------------------------------------------

-- AI flags at +0x188.
local AI_REPROBE = 0x0001
local AI_LOCKED_ON = 0x0002
local AI_SKID = 0x0004
local AI_SKID2 = 0x0008
local AI_WALL = 0x0010
local AI_HURT = 0x0020
local AI_ACTIVE = 0x0080

-- The two scripted player poses the dog cannot reach (the climb): the whole
-- player animation dword compared against these drops the dog to the chase.
local UNREACHABLE_A = 0x00140301
local UNREACHABLE_B = 0x03140301

-- Health base, index rand() & 0xF; init adds rand() & 3.
local HEALTH = {
    119, 99, 119, 99, 119, 99, 119, 99,
    99, 99, 59, 99, 59, 99, 59, 99,
}

-- Extra yaw on the hit-recoil direction, index rand() & 7.
local RECOIL = { 0x80, -0xC0, -0x80, 0xC0, -0x100, 0x100, 0x100, -0x100 }

-- Walk/run parameter rows, indexed by behavior_flags >> 1.
local WALK_ROWS = {
    { anim = 1, speed = 40 },
    { anim = 14, speed = 130 },
}

-- Turn animations by the sign of the turn: lean left / straight / lean right.
local TURN_ANIM = { [0] = 15, [1] = 2, [2] = 16 }

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

-- An arithmetic right shift of a 16-bit signed value (Lua's `>>` is logical).
local function sar16(value, bits)
    return s16(value) // (1 << bits)
end

-- Ramp the live speed toward a cap, one step per frame (no-op at the cap).
local function speed_up(e, step, max)
    if s16(e.move_speed_current) < max then
        e.move_speed_current = s16(e.move_speed_current) + step
        if s16(e.move_speed_current) > max then
            e.move_speed_current = max
        end
    end
end

local function slow_down(e, step, min)
    if min < s16(e.move_speed_current) then
        e.move_speed_current = s16(e.move_speed_current) - step
        if s16(e.move_speed_current) < min then
            e.move_speed_current = min
        end
    end
end

-- Swap the body animation to match the sign of the turn, but only when it
-- actually changes; the blend restarts on the swap.
local function set_turn_anim(e, turn)
    local sign = 0
    if turn < 0 then
        sign = -1
    elseif turn > 0 then
        sign = 1
    end
    local anim = TURN_ANIM[sign + 1]
    if e.animation_id ~= anim then
        e.animation_id = anim
        e.timing_control = 0
        if e.blend_counter < 4 then
            e.blend_counter = 7
        end
    end
end

-- The player's animationId/frame/behavior/action bytes as one dword.
local function player_anim_word(e)
    return (e.player_state & 0xFF)
        | ((e.player_anim_frame_id & 0xFF) << 8)
        | ((e.player_action_behavior & 0xFF) << 16)
        | ((e.player_action_state & 0xFF) << 24)
end

local function player_unreachable(e)
    local word = player_anim_word(e)
    return word == UNREACHABLE_A or word == UNREACHABLE_B
end

-- Probe a hard left and a hard right turn. Returns +1 "only left blocked",
-- -1 "only right blocked", 0 "both clear", or `both` when both are blocked.
local function probe_both_turns(e, mul, both)
    local left = e:probe_turn(0x400, mul) and 1 or 0
    local right = e:probe_turn(-0x400, mul) and 1 or 0
    right = -right
    if left ~= 0 and right ~= 0 then
        return both
    end
    return right + left
end

-- The same probe aimed at the player.
local function probe_toward_player(e, mul)
    local px, _, pz = e:player_pos()
    local to_player = e:angle_to(px, pz)
    return e:probe_turn(to_player - e.angle, mul)
end

-- One owed blood billboard on the dog's own joint, depth group `depth`.
local function spawn_entity_blood(e, joint, depth)
    if e.cb_blood <= 0 then
        return
    end
    e:spawn_joint_effect(0, depth, joint, 0)
    e.cb_blood = e.cb_blood - 1
end

-- The maul's blood on the player. The original anchors it to the player's
-- joint 1 world position; the port has no player-joint attach, so the
-- billboard is attached to the player matrix origin instead (documented
-- approximation). The owed-count contract is exact.
local function spawn_player_blood(e)
    if e.cb_blood <= 0 then
        return
    end
    e:spawn_player_effect(0, 3, 0, 0, 0, 0)
    e.cb_blood = e.cb_blood - 1
end

-- Footfall cue on the two contact frames.
local function footstep(e)
    if e.animation_frame_id == 0 or e.animation_frame_id == 0x1B then
        e:play_enemy_sound(9)
    end
end

-- Forward declarations for the mutually recursive handlers.
local select_stalk
local consider_attack
local bite_player
local STATES

-- ---------------------------------------------------------------------------
-- The behaviour selectors (indexed by behavior_flags)
-- ---------------------------------------------------------------------------

-- Patrol: 0/1 walk, 2 runs, 3 tail-jumps into the stalk setup.
local function select_patrol(e)
    e.cb_behflags = 4
    if e.behavior_flags == 3 then
        select_stalk(e)
        return
    end
    e.ignore = 1
    local row = WALK_ROWS[(e.behavior_flags >> 1) + 1]
    e.animation_id = row.anim
    e.animation_frame_id = e:random() & 0x1F
    e.move_speed_current = row.speed
    e.tint_flashes = ((e.behavior_flags >> 1) + 1) * 0x10
    e.cb_behflags = e.behavior_flags & 1
    e.cb_swerve = (e:random() - 2) & 0xF
    e.blend_counter = 0xF
end

-- Chase: latch the player's current position as the waypoint and clear the
-- skid bits.
local function select_chase(e)
    if e.blend_counter > 7 then
        e.blend_counter = 7
    end
    e.tint_flashes = (e:random() & 7) + 0x3D
    e.groan_timer = 0
    e.sink_wobble = 0
    e.cb_swerve = 0
    e.reaction_timer = 0
    if e.cb_aiflags & AI_REPROBE ~= 0 then
        e.cb_swerve = (e:random() & 0xF) + 0x14
    end
    e.ignore = 2
    local px, _, pz = e:player_pos()
    e.player_pos_x = px
    e.player_pos_z = pz
    e.cb_aiflags = (e.cb_aiflags & ~(AI_SKID | AI_SKID2)) | AI_ACTIVE
end

-- The scripted entrances: 5 running leap, 6 window crash, 7 ceiling drop.
local function select_entrance(e)
    e.death_timer = 0
    local beh = e.behavior_flags
    if beh == 5 then
        if e.animation_id == 0x10 then
            e.angle = e.angle - 0x20
        elseif e.animation_id == 0x0F then
            e.angle = e.angle + 0x20
        end
        e.animation_id = 6
        e.move_speed_current = 0x41
        e.cb_launch_vy = 0x012C
        e.reaction_timer = 0xFFD8
    elseif beh == 6 then
        e.status_flags = e.status_flags | 6
        e.move_speed_current = 0x41
        e.cb_launch_vy = 0x01E0
        e.reaction_timer = 0xFFD8
        e.animation_id = 6
        e.cb_behflags = 5
        e.hit_state = 1
    elseif beh == 7 then
        e.status_flags = e.status_flags | 4
        e.animation_id = 7
        e.death_timer = 10
        e.move_speed_current = 0x118
        e.cb_launch_vy = 0x0190
        e.reaction_timer = 0xFFD8
        e.cb_behflags = 5
        e.hit_state = 1
    end
    e.ignore = 3
    e.blend_counter = 3
    e.action_ticks_counter = 8
    e.groan_timer = 0
    e.cb_blood = 0
end

-- Bite: straight into the close-range snap.
local function select_bite(e)
    e.ignore = 7
    e.animation_id = 4
    e.blend_counter = 0
    e.hit_state = 1
end

-- Strafe: pick a side if no turn is in progress.
local function select_strafe(e)
    e.ignore = 4
    if e.cb_turn_step == 0 then
        e.cb_turn_step = (e:random() & 1) == 0 and -0x80 or 0x80
    end
    e.action_ticks_counter = 8
    e.groan_timer = 0
    e.cb_aiflags = e.cb_aiflags | AI_ACTIVE
end

-- Alert / bark pause.
local function select_alert(e)
    e.ignore = 5
    e.animation_id = 0
    e.blend_counter = 0xF
    e.animation_frame_id = 0
    e.action_ticks_counter = 0x14
    e.cb_turn_step = 0
    e.groan_timer = 0
    e.hit_state = 1
end

-- Stalk: raise the hunting bit unless the dog has already given up.
select_stalk = function(e)
    e.ignore = 9
    e.animation_id = 1
    e.animation_frame_id = e:random() & 0x1F
    e.blend_counter = 0
    e.groan_timer = 0
    e.move_speed_current = 0
    if (e.cb_behflags & 4) == 0 then
        e.cb_behflags = e.cb_behflags | 2
    end
    e.hit_state = 0
end

-- The selector table, indexed by the whole behavior_flags byte.
local SELECT = {
    select_patrol, -- 0
    select_patrol, -- 1
    select_patrol, -- 2
    select_patrol, -- 3 (tails into stalk)
    select_chase, -- 4
    select_entrance, -- 5
    select_entrance, -- 6
    select_entrance, -- 7
    select_bite, -- 8
    select_strafe, -- 9
    select_alert, -- 10
    select_alert, -- 11
    select_stalk, -- 12
}

-- Turn the spawn's behavior_flags byte into a behaviour index. The scripted
-- idle (0x80) bypasses the table.
local function behavior_select(e)
    e.cb_alert = 0
    e.timing_control = 0
    if e.cb_aiflags & 0x80 ~= 0 then
        e.animation_frame_id = 0
    end
    if e.behavior_flags == 0x80 then
        e.ignore = 8
        e.animation_id = 0
        e.blend_counter = 0xF
        return
    end
    local handler = SELECT[e.behavior_flags + 1]
    if handler == nil then
        e:count_placeholder(e.behavior_flags)
        return
    end
    handler(e)
end

-- ---------------------------------------------------------------------------
-- State 0 - one-time init
-- ---------------------------------------------------------------------------

local function init(e)
    e.state = 1
    e.ignore = 0
    e.action_behavior = 0
    e.action_state = 0

    -- Three draws; the first is thrown away. Health is base[rand & 0xF] plus
    -- rand() & 3, so a dog spawns with 59..122 HP.
    e:random()
    e.health = HEALTH[(e:random() & 0xF) + 1] + (e:random() & 3)
    e.hit_state = 0

    e.cb_probe = 0
    e.cb_behflags = 1
    e.cb_aiflags = AI_ACTIVE

    e:set_sca(400, 800, 500, -800, 0)
    e.status_flags = e.status_flags & 0x1F

    e.animation_id = 0
    e.animation_frame_id = 0
    e.timing_control = 0
    e:reset_joints()

    e.action_ticks_counter = 0
    e.groan_timer = 0
    e.sink_wobble = 0
    e.cb_swerve = 0
    e.cb_repause = 0
    e.move_speed_current = 0
    e.cb_path = 0
    e.cb_alert = 0

    -- Nudge the spawn point 500 units along the dog's own facing; the step is
    -- added to the accepted `position` words.
    e:nudge_spawn(500)

    -- Ground shadow: the small forward offset, the mid grey tint and the
    -- 0x550 x 0x200 quad.
    e:set_shadow_offset(0, 0, -0x50)
    e.shadow_tint = 0x808080
    e.shadow_half_x = 0x550
    e.shadow_half_z = 0x200

    -- Every dog in the room shares one "already barked" latch.
    e.cerberus_barked = false

    -- Behaviours 6, 7 and 0x80 start the dog in the air; everything else is
    -- planted on the floor.
    local beh = e.behavior_flags
    if beh >= 6 and (beh <= 7 or beh == 0x80) then
        e.status_flags = e.status_flags | 0x04
        return
    end
    e.pos_y = 0
end

-- ---------------------------------------------------------------------------
-- State 1 - the behaviour driver
-- ---------------------------------------------------------------------------

local function behavior_run(e)
    if e.behavior_flags == 0x80 then
        e.ignore = 0
    end

    local px, _, pz = e:player_pos()
    local dx = px - e.pos_x
    local dz = pz - e.pos_z
    e.cb_dist = (dx < 0 and -dx or dx) + (dz < 0 and -dz or dz)

    e.cb_path = e:pathfind_update(px, pz)
    e.status_flags = (e.status_flags & 0x1F) | 0x40

    if e.cb_alert == 0 then
        e:check_visual_range(5000)
    end
    if e.ignore == 0 then
        behavior_select(e)
        e.action_behavior = 0
        e.action_state = 0
    end
    local handler = STATES[5 + e.ignore + 1]
    if handler == nil then
        e:count_placeholder(e.ignore)
        return
    end
    handler(e)
end

-- ---------------------------------------------------------------------------
-- Behaviour 1 - patrol and its three sub-states
-- ---------------------------------------------------------------------------

local function patrol_begin(e)
    e.action_behavior = e.action_behavior + 1
    e.pos_y = 0
    e.timing_control = 0
    e.blend_counter = 0xF
    e.groan_timer = 0
    e.sink_wobble = 0
    e.cb_swerve = 0
end

local function patrol_pick_turn(e)
    if (e.cb_probe & 1) == 0
        and (e.cb_behflags ~= 0 or e.action_ticks_counter ~= 0) then
        e.action_ticks_counter = e.action_ticks_counter - 1
        return
    end
    e.action_behavior = e.action_behavior + 1
    e.action_ticks_counter = 0x400

    local dir = probe_both_turns(e, 3, 0)
    if dir == 0 then
        if e.cb_behflags == 0 then
            e.cb_turn_step = (e:random() & 1) == 0 and -e.tint_flashes or e.tint_flashes
        else
            e.cb_turn_step = e.tint_flashes
        end
    else
        e.cb_turn_step = e.tint_flashes * dir
        e.sink_wobble = 0
    end

    -- A walking dog that would be turning away from the player cuts the turn
    -- short so it drifts back toward them.
    if e.cb_behflags == 0 then
        local px, _, pz = e:player_pos()
        local to_player = e:turn_toward_target(px, pz, 0x200)
        if to_player * dir < 0 then
            e.action_ticks_counter = sar16(e.action_ticks_counter, 2)
        end
    end
end

local function patrol_turn(e)
    e.angle = e.angle + e.cb_turn_step
    local mag = e.cb_turn_step < 0 and -e.cb_turn_step or e.cb_turn_step
    e.action_ticks_counter = s16(e.action_ticks_counter) - mag
    if s16(e.action_ticks_counter) < 0 then
        e.action_behavior = 0
        if e.cb_behflags == 0 then
            e.action_ticks_counter = (e:random() & 0x1F) + 0x4B
        end
    end
end

local PATROL = { patrol_begin, patrol_pick_turn, patrol_turn }

local function beh_patrol(e)
    if e.cb_dist < 0x1194 or player_unreachable(e) then
        e.ignore = 0
        e.behavior_flags = 4
        e.blend_counter = 7
    end
    e:move(8, e.move_speed_current)
    local handler = PATROL[e.action_behavior + 1]
    if handler == nil then
        e:count_placeholder(e.action_behavior)
    else
        handler(e)
    end
    e:advance_anim(0x100)
    footstep(e)
end

-- ---------------------------------------------------------------------------
-- Behaviour 2 - chase and its three sub-states
-- ---------------------------------------------------------------------------

-- The attack decision: only inside 5000 units and while the player is not
-- already grabbed.
consider_attack = function(e)
    local health = e.player_health
    if e.cb_dist >= 5000 or e.player_attacked ~= 0 then
        return
    end

    local px, _, pz = e:player_pos()
    if e:turn_toward_target(px, pz, 0x100) == 0 then
        local poisoned = e.player_health_status & 8
        local chance = (poisoned == 0 and 0x1A or 0) + 100
        local threshold = e.second_playthrough and 0x11 or 0x0D
        if health < threshold
            and e:player_facing_entity()
            and (e:random() & 0x7F) < chance then
            e.ignore = 0
            e.behavior_flags = 9
            e:advance_anim(0x100)
            return
        end
        e.ignore = 0
        e.behavior_flags = 5
        e.cb_aiflags = e.cb_aiflags | AI_ACTIVE
        e.cb_swerve = 0
        return
    end

    if e.cb_swerve < 10 then
        e.cb_swerve = e.cb_swerve + 1
        if e.cb_swerve > 9 then
            e.cb_swerve = (e:random() & 7) + e.cb_swerve + 0x1E
            e.reaction_timer = 0
        end
    end

    if e:turn_toward_target(px, pz, 0x400) == 0 then
        e.cb_aiflags = e.cb_aiflags | AI_LOCKED_ON
        return
    end
    if (e.cb_aiflags & 3) == AI_LOCKED_ON then
        if (e:random() & 0x40) > 0x38 then
            e.cb_aiflags = (e.cb_aiflags & ~AI_LOCKED_ON) | (AI_REPROBE | AI_SKID)
        end
    end
end

local function chase_run(e)
    speed_up(e, 0x14, 0xF0)
    local wx, wz = e.player_pos_x, e.player_pos_z

    local turn
    if e.cb_repause == 0 and e.cb_swerve < 10 and s16(e.move_speed_current) > 0xEF then
        turn = e:turn_toward_target(wx, wz, e.tint_flashes)
        e:rotate_toward_target(wx, wz, e.tint_flashes)
    else
        if e.cb_probe & 0x11 ~= 0 then
            local step = e.tint_flashes + ((e.cb_aiflags & AI_REPROBE) ~= 0 and 0x20 or 0)
            e.reaction_timer = e:turn_toward_target(wx, wz, step)
            e.cb_aiflags = e.cb_aiflags & ~AI_REPROBE
        end
        turn = e.reaction_timer
        e.angle = e.angle + e.reaction_timer
        e.cb_swerve = e.cb_swerve - 1
        if e.cb_swerve < 0xB then
            e.cb_swerve = 5
            e.cb_aiflags = e.cb_aiflags & ~AI_REPROBE
        end
    end

    -- The pathfinder's verdict: bit 1 = still thinking; bit 0 = refreshed.
    -- Six consecutive dead frames give up and switch to the stalk with the
    -- "gave up" bit set.
    if (e.cb_path & 2) == 0 then
        if (e.cb_path & 1) == 0 then
            e.sink_wobble = e.sink_wobble + 1
            if e.sink_wobble == 6 then
                e.ignore = 0
                e.behavior_flags = 12
                e.cb_behflags_byte = e.cb_behflags_byte | 4
                return
            end
        else
            e.sink_wobble = 0
            e.move_speed_current = 0xF0
            local px, _, pz = e:player_pos()
            e.player_pos_x = px
            e.player_pos_z = pz
        end
    end

    set_turn_anim(e, turn)

    if e.cb_behflags_byte & 2 == 0 then
        if e.cb_probe & 0x11 ~= 0 then
            e.groan_timer = e.groan_timer + 2
            if e.groan_timer > 0xF then
                e.ignore = 0
                e.behavior_flags = 9
                e.cb_turn_step = turn * 2
                e.cb_aiflags = e.cb_aiflags & ~AI_ACTIVE
            end
        elseif e.groan_timer > 0 then
            e.groan_timer = e.groan_timer - 1
        end
    else
        -- Hunting: probe straight at the player and, when clear, wall-follow.
        if probe_toward_player(e, 4) and e.cb_repause == 0 then
            e.action_behavior = 1
            e.action_state = 0
        end
        if e.cb_repause > 0 then
            e.cb_repause = e.cb_repause - 1
        end
    end

    if e.cb_aiflags & (AI_SKID | AI_SKID2) ~= 0 then
        e.action_behavior = 2
        e.action_state = 0
    end
end

-- The wall-follow sequence, dispatched on action_state. Each step's return
-- value is reduced to a sign and shifted into 0/1/2.
local function wall_step_wait(e)
    local result = 0
    if e.cb_probe & 1 ~= 0 then
        e.action_state = e.action_state + 1
        result = e.tint_flashes
    end
    if e.sink_wobble > 8 then
        e.sink_wobble = 8
    end
    return result
end

local function wall_step_pick(e, step)
    e.action_state = e.action_state + 1
    e.sink_wobble = 0
    local px, _, pz = e:player_pos()
    e.tint_flashes = e:turn_toward_target(px, pz, step)
    if e.tint_flashes == 0 then
        e.tint_flashes = step
    end
    e.cb_aiflags = e.cb_aiflags & ~AI_WALL
    return wall_step_wait(e)
end

local function wall_step_slide(e)
    if (e.cb_probe & 0xF) == 0 then
        e.action_state = e.action_state + 1
        e.action_ticks_counter = 0
        local dir = probe_both_turns(e, 2, 1)
        e.cb_turn_step = dir
        if e.cb_turn_step == 0 then
            e.cb_turn_step = -1
        end
    end
    e.angle = e.angle + e.tint_flashes
    if e.sink_wobble > 8 then
        e.sink_wobble = 8
    end
    return e.tint_flashes
end

local function wall_step_commit(e)
    local blocked = e:probe_turn(e.cb_turn_step << 10, 1) and 1 or 0
    if (blocked ~= 0 or e.action_ticks_counter ~= 0) and (e.cb_probe & 1) == 0 then
        if s16(e.action_ticks_counter) > 0 then
            e.action_ticks_counter = s16(e.action_ticks_counter) - 1
        end
        return 0
    end
    e.action_state = 4
    e.tint_flashes = e.cb_turn_step
    e.action_ticks_counter = 0x400
    return 0
end

local function wall_step_run(e, step)
    e.angle = e.angle + e.tint_flashes * step
    e.action_ticks_counter = s16(e.action_ticks_counter) - step
    if s16(e.action_ticks_counter) < 1 then
        e.action_state = 3
        e.action_ticks_counter = 4
    end
    return e.tint_flashes
end

local function wall_follow_step(e, phase_limit, probe_speed)
    local state = e.action_state
    local sub
    if state == 0 then
        sub = wall_step_pick(e, probe_speed)
    elseif state == 1 then
        sub = wall_step_wait(e)
    elseif state == 2 then
        sub = wall_step_slide(e)
    elseif state == 3 then
        sub = wall_step_commit(e)
    else
        sub = wall_step_run(e, probe_speed)
    end

    if not probe_toward_player(e, 4) and e.sink_wobble > 0xC then
        e.cb_turn_step = 0
        e.cb_aiflags = e.cb_aiflags | AI_WALL
    end
    e.sink_wobble = e.sink_wobble + 1
    if phase_limit < e.sink_wobble then
        e.action_state = 0
    end

    if sub == 0 then
        return 1
    end
    if sub < 1 then
        return 0
    end
    return 2
end

local function chase_turn(e)
    speed_up(e, 0x14, 0xF0)
    local dir = wall_follow_step(e, 0x5A, 0x40)
    if dir ~= 3 then
        set_turn_anim(e, dir - 1)
    end
    if e.cb_aiflags & AI_WALL ~= 0 then
        e.ignore = 0
        e.action_behavior = 4
        e.tint_flashes = (e:random() & 7) + 0x3D
        e.cb_aiflags = e.cb_aiflags & ~AI_ACTIVE
        e.cb_repause = (e:random() & 3) * 2 + 8
    end
end

local function chase_skid(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.action_ticks_counter = 0
    end
    local wx, wz = e.player_pos_x, e.player_pos_z
    e.cb_turn_step = e:turn_toward_target(wx, wz, 0x60)
    e:move(-s16(e.action_ticks_counter), e.move_speed_current)

    if e.action_state == 1 then
        slow_down(e, 0x0E, 10)
        if s16(e.move_speed_current) < 0xB then
            e.action_state = e.action_state + 1
        end
    else
        speed_up(e, 0x0E, 0xF0)
    end

    e.angle = e.angle + e.cb_turn_step
    e.action_ticks_counter = s16(e.action_ticks_counter) + e.cb_turn_step
    set_turn_anim(e, e.cb_turn_step)

    local swept = s16(e.action_ticks_counter)
    local abs = swept < 0 and -swept or swept
    if abs > 0x7FF or e.cb_turn_step == 0 then
        e.action_behavior = 0
        e.cb_aiflags = e.cb_aiflags & ~(AI_SKID | AI_SKID2)
    end
end

local CHASE = { chase_run, chase_turn, chase_skid }

local function beh_chase(e)
    if e.action_behavior < 2 then
        e:move(0, e.move_speed_current)
    end
    local handler = CHASE[e.action_behavior + 1]
    if handler == nil then
        e:count_placeholder(e.action_behavior)
    else
        handler(e)
    end
    consider_attack(e)
    if e:advance_anim(0x200) then
        e:play_enemy_sound(0)
    end
end

-- ---------------------------------------------------------------------------
-- Behaviour 3 - the leap and its five sub-states
-- ---------------------------------------------------------------------------

-- The hit itself. A low-health, facing-away player turns the bite into the
-- kill: the player is forced into the maul animation and the return value 2
-- sends the leap to the maul behaviour. Otherwise it is an ordinary -12 bite.
bite_player = function(e)
    e.hit_state = 0
    local threshold = e.second_playthrough and 0x11 or 0x0D

    if e.player_health < threshold and not e:player_facing_entity() then
        e.player_flags = e.player_flags | 2
        e.status_flags = e.status_flags | 2
        e.move_speed_current = 0
        e.groan_timer = 2
        e.player_angle = (e.angle + 0x800) & 0xFFF
        e.hit_state = 1
        e.player_state = 7
        e.player_anim_frame_id = 2
        e.player_action_behavior = 0
        e.player_action_state = 2
        e.player_attacked = 1
        e.status_flags = e.status_flags & 0xFB
        return 2
    end

    e.player_action_state = e:player_facing_entity() and 1 or 0
    local px, _, pz = e:player_pos()
    e.unk_c6 = px
    e.unk_c8 = pz

    if e.behavior_flags ~= 6 then
        e.action_behavior = 4
        e.animation_id = 0x11
        e.animation_frame_id = 1
        e.timing_control = 0
        e.blend_counter = 0xF
        e.status_flags = e.status_flags & 0xFB
        e.move_speed_current = sar16(e.move_speed_current, 2)
    end

    e.player_unk_c6 = px
    e.player_unk_c8 = pz

    local facing = e:player_facing_entity() and 1 or 0
    e.player_attacked = facing + 1
    e.player_action_behavior = facing + 0x66
    e.player_health = e.player_health - 0xC

    e.tint_flashes = 0x200
    e.cb_swerve = 0
    e.cb_blood = 3
    e.sink_wobble = 0
    e:play_enemy_sound(3)
    return e.groan_timer
end

local function leap_crouch(e)
    e:move(0, e.move_speed_current)
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 or e.death_timer ~= 0 then
        e.action_behavior = e.action_behavior + 1
        e.move_speed_current = 0x118
        e.cb_alert = 1
        if e.behavior_flags ~= 7 then
            e:play_enemy_sound(6)
        end
    end
    e:advance_anim(0x400)
end

local function leap_airborne(e)
    e.status_flags = e.status_flags | 0x40
    e:ballistic(e.move_speed_current, e.cb_launch_vy, e.reaction_timer, 0)

    local air_ticks = e.death_timer
    local gravity = e.reaction_timer
    local launch_vy = e.cb_launch_vy

    local reach
    if e.second_playthrough then
        reach = e.player_health > 0x10 and 1000 or 800
    else
        reach = e.player_health > 0x0C and 1000 or 800
    end
    if e.player_health_status & 8 ~= 0 then
        reach = 0x5DC
    end

    if e.groan_timer == 0 then
        e.groan_timer = e:reach_test(4, 0, 0, 0, reach) and 1 or 0
        if e.groan_timer ~= 0
            and e.player_attacked == 0
            and e.pos_y > -0x708
            and bite_player(e) == 1 then
            if e.animation_frame_id == 0 then
                return
            end
            e:advance_anim(0x100)
            return
        end
    end

    -- Past the top of the arc? The test is next frame's vertical velocity.
    if (air_ticks + 1) * gravity + launch_vy < 1 then
        if e.groan_timer == 2 then
            e.ignore = 6
            e.action_behavior = 0
            e.status_flags = e.status_flags | 2
            e.player_flags = e.player_flags | 2
        else
            e.action_behavior = e.action_behavior + 1
            if e.animation_id == 6 then
                e.animation_id = 7
                e.timing_control = 0
                e.blend_counter = 0xF
            end
        end
    end

    if e.animation_frame_id ~= 0 then
        e:advance_anim(0x100)
    end
end

local function leap_land(e)
    e.status_flags = e.status_flags | 0x40
    e:ballistic(e.move_speed_current, e.cb_launch_vy, e.reaction_timer, 0)
    if e.pos_y >= 0 then
        e.action_behavior = e.action_behavior + 1
        e.pos_y = 0
        e.status_flags = (e.status_flags & 0xF9) | 0x20
        e:play_enemy_sound(5)
    end
    if e.animation_frame_id ~= 9 then
        e:advance_anim(0x100)
    end
end

local function leap_recover(e)
    e:move(0, e.move_speed_current)
    slow_down(e, 0x40, 0xF0)
    if not e:advance_anim(0x100) then
        return
    end

    e.status_flags = e.status_flags & 0xFB
    e.ignore = 0
    e.hit_state = 0
    e.cb_aiflags = e.cb_aiflags & ~AI_REPROBE

    -- The original aims at the shared scratch vector, which still holds the
    -- previous frame's probe result.
    if e.cb_probe & 0x11 ~= 0
        and e.behavior_flags ~= 6
        and e.behavior_flags ~= 7
        and (e.cb_behflags_byte & 2) == 0 then
        e.behavior_flags = 9
        e.cb_turn_step = e:turn_toward_target(e.scratch_x, e.scratch_z, e.tint_flashes) * 2
        if e.cb_turn_step == 0 then
            local dir = probe_both_turns(e, 1, -1)
            e.cb_turn_step = dir * e.tint_flashes * 2
        end
        return
    end
    e.behavior_flags = 4
end

local function leap_tumble(e)
    if e.animation_frame_id < 0x10 then
        e:move(0x800, e.move_speed_current)
    end
    if e.cb_swerve < 0x800 then
        e.angle = e.angle + 0x80
        e.cb_swerve = e.cb_swerve + 0x80
    end
    e:ballistic(0, e.cb_launch_vy, e.reaction_timer, 0)
    if e.pos_y >= 0 and e.sink_wobble == 0 then
        e:play_enemy_sound(5)
        e.sink_wobble = 1
    end
    if e:advance_anim(0x100) then
        e.behavior_flags = 4
        e.ignore = 0
        e.move_speed_current = 0
        e.cb_aiflags = e.cb_aiflags | AI_REPROBE
    end
end

local LEAP = { leap_crouch, leap_airborne, leap_land, leap_recover, leap_tumble }

local function beh_leap(e)
    local handler = LEAP[e.action_behavior + 1]
    if handler == nil then
        e:count_placeholder(e.action_behavior)
    else
        handler(e)
    end
    spawn_entity_blood(e, 4, 0)
end

-- ---------------------------------------------------------------------------
-- Behaviour 4 - the strafe and its three sub-states
-- ---------------------------------------------------------------------------

local function strafe_hold(e)
    if s16(e.action_ticks_counter) < 1 then
        if (e.cb_probe & 0x11) == 0 then
            e.ignore = 0
            e.behavior_flags = 4
            e.cb_aiflags = e.cb_aiflags & ~AI_ACTIVE
            if (e.cb_behflags_byte & 1) == 0 then
                e.cb_swerve = 0x28
            end
        else
            local n = e.groan_timer
            e.groan_timer = n + 1
            if n > 7 and e.sink_wobble == 0 then
                e.action_behavior = e.action_behavior + 1
                e.cb_turn_step = -e.cb_turn_step
                e.groan_timer = 0
                return
            end
        end
        consider_attack(e)
    else
        e.action_ticks_counter = e.action_ticks_counter - 1
    end
    set_turn_anim(e, e.cb_turn_step)
end

local function strafe_accel(e)
    if e.sink_wobble ~= 0 then
        speed_up(e, 0x32, 0x82)
    end
    if (e.cb_probe & 0x11) == 0 then
        e.ignore = 0
        e.behavior_flags = 4
        e.cb_aiflags = e.cb_aiflags & ~AI_ACTIVE
        if (e.cb_behflags_byte & 1) == 0 then
            e.cb_swerve = 0x28
        end
    else
        local n = e.groan_timer
        e.groan_timer = n + 1
        if n > 7 then
            e.action_behavior = e.action_behavior + 1
            e.move_speed_current = 0xFF7E
            e.animation_id = 10
            e.animation_frame_id = 0
            e.timing_control = 0
            e.groan_timer = 0
            return
        end
    end
    set_turn_anim(e, e.cb_turn_step)
end

local function strafe_spring(e)
    if e.animation_frame_id < 0xF then
        if e.animation_frame_id > 7 then
            slow_down(e, 0x20, 0x28)
        end
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x40)
    else
        e.move_speed_current = 0
        e.cb_turn_step = 0
    end
    if e.animation_frame_id > 0x12 then
        e.ignore = 0
        e.behavior_flags = 4
    end
end

local STRAFE = { strafe_hold, strafe_accel, strafe_spring }

local function beh_strafe(e)
    e.angle = e.angle + e.cb_turn_step
    e:move(0, e.move_speed_current)
    local handler = STRAFE[e.action_behavior + 1]
    if handler == nil then
        e:count_placeholder(e.action_behavior)
    else
        handler(e)
    end
    if e:advance_anim(0x200) then
        e:play_enemy_sound(0)
    end
end

-- ---------------------------------------------------------------------------
-- Behaviour 5 - alert / bark
-- ---------------------------------------------------------------------------

local function alert_wait(e)
    if e.cb_dist < 0x1964 or player_unreachable(e) or e.behavior_flags == 11 then
        e.action_behavior = e.action_behavior + 1
        e.cb_swerve = 0
        e.animation_id = 1
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0x1F
        e.action_ticks_counter = (e:random() & 0xF) + 0x20
    end
end

local function alert_track(e)
    e:head_track()
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 then
        e.ignore = 0
        e.behavior_flags = 12
    end
end

local function beh_alert(e)
    -- The frame id is saved across the animation advance so the listening
    -- pose holds its frame while the blend keeps moving.
    local saved_frame = e.animation_frame_id
    e:advance_anim(0x80)
    e.animation_frame_id = saved_frame
    if e.action_behavior == 0 then
        alert_wait(e)
    else
        alert_track(e)
    end
end

-- ---------------------------------------------------------------------------
-- Behaviour 6 - the maul
-- ---------------------------------------------------------------------------

local function beh_maul(e)
    if e.action_behavior > 1 then
        return
    end
    if e.action_behavior == 0 then
        e.action_behavior = e.action_behavior + 1
        e.animation_id = 8
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 0
        e.cb_blood = 0
        e.player_flags = e.player_flags | 6
        e.player_angle = e.angle
        e:snap_grab()
        e.cb_swerve = 0
        e.player_attacked = 1
        e:set_player_animation(7, 2)
        e.player_action_behavior = 0
        e.player_action_state = 0
        e.move_speed_current = 0
        return
    end

    e.pos_y = 0
    if e.animation_frame_id < 0x14 then
        e:snap_grab()
    end
    e:apply_anim_vertex()
    if e:advance_anim(0x100) then
        e.action_behavior = e.action_behavior + 1
    end

    local frame = e.animation_frame_id
    if frame == 0x2A then
        e.player_health = -1
    end
    if frame == 0x28 then
        e:play_enemy_sound(3)
    end
    if frame == 0x5E then
        e:play_enemy_sound(7)
    end
    if frame == 0x66 then
        e.cb_blood = 6
        -- The original greys four player joints and the dog's joint 4 with an
        -- immediate colour. The port has no per-joint render tint; the calls
        -- are transcribed and recorded as the joint-state contract only.
        e:tint_joint(0, 0x30, 0x80820, 0x00606060)
        e:tint_joint(1, 0x30, 0x80820, 0x00606060)
        e:tint_joint(9, 0x30, 0x80820, 0x00606060)
        e:tint_joint(0xC, 0x30, 0x80820, 0x00606060)
        e:tint_joint(4, 0x30, 0x80820, 0x00606060)
        local px, py, pz = e:player_pos()
        e:play_3d_sound_at(3, 3, px, py, pz)
    end

    spawn_player_blood(e)
    spawn_entity_blood(e, 3, 0)
end

-- ---------------------------------------------------------------------------
-- Behaviour 7 - the close-range bite
-- ---------------------------------------------------------------------------

local function beh_bite(e)
    e:advance_anim(0x100)
    if e.action_behavior == 0 then
        if e.cb_dist < 0x1195 or player_unreachable(e) then
            e.action_behavior = e.action_behavior + 1
            e.animation_id = e.animation_id + 1
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 0xF
            e.cb_swerve = 0
        end
        return
    end
    e:head_track()
    if e.animation_frame_id == 0x21 then
        e.ignore = 0
        e.behavior_flags = 4
        e.hit_state = 0
    end
end

-- ---------------------------------------------------------------------------
-- Behaviour 8 - the script-driven idle
-- ---------------------------------------------------------------------------

local function beh_scd(e)
    e.ignore = 0
    e:advance_anim(0x100)
end

-- ---------------------------------------------------------------------------
-- Behaviour 9 - the stalk and its four sub-states
-- ---------------------------------------------------------------------------

local function stalk_begin(e)
    e.action_behavior = e.action_behavior + 1
    e.tint_flashes = (e:random() & 7) + 0x10
    e.sink_wobble = 0
    if e.cb_swerve < 0 then
        e.tint_flashes = -e.tint_flashes
    end
    e.action_ticks_counter = (e:random() & 0x1F) + 0x2D
end

local function stalk_arc(e)
    e.angle = e.angle + e.tint_flashes
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter == 0 then
        e.action_behavior = e.action_behavior + 1
        local a = e:random() & 7
        local b = e:random() & 0xF
        e.action_ticks_counter = a - b + 0x1E
    end
end

local function stalk_reface(e)
    e.action_ticks_counter = e.action_ticks_counter - 1
    if e.action_ticks_counter ~= 0 and (e.cb_probe & 1) == 0 then
        return
    end
    local px, _, pz = e:player_pos()
    e.tint_flashes = e:turn_toward_target(px, pz, (e:random() & 7) + 0x10)
    e.action_ticks_counter = 2
    e.action_behavior = 1
    e.action_ticks_counter = (e:random() & 0x1F) + 0x2D
    if (e:random() & 0x3F) > 0x30 then
        e:play_enemy_sound(2)
    end
end

local function stalk_wallfollow(e)
    wall_follow_step(e, 600, 0x10)
    if e.cb_aiflags & AI_WALL ~= 0 then
        e.action_behavior = 0
    end
end

local STALK = { stalk_begin, stalk_arc, stalk_reface, stalk_wallfollow }

local function beh_stalk(e)
    speed_up(e, 2, 0x28)
    e:move(0, e.move_speed_current)
    local handler = STALK[e.action_behavior + 1]
    if handler == nil then
        e:count_placeholder(e.action_behavior)
    else
        handler(e)
    end
    e:advance_anim(0x100)
    footstep(e)
    e:head_track()

    if probe_toward_player(e, 8) and e.action_behavior ~= 3 then
        e.action_behavior = 3
        e.action_state = 0
    end

    local px, _, pz = e:player_pos()
    if e.cb_dist < 0x9C4 then
        e.groan_timer = e.groan_timer + 1
        if e.cb_behflags_byte & 4 ~= 0 then
            e.groan_timer = 0x41
        end
        if e.player_move_speed > 200 then
            e.groan_timer = e.groan_timer + 0x14
        end
        if e:turn_toward_target(px, pz, 128) == 0 and e.cb_dist > 800 then
            e.ignore = 0
            e.behavior_flags = 5
            e.cerberus_barked = true
        end
    else
        if e.groan_timer > 0 then
            e.groan_timer = e.groan_timer - 1
        end
        if e:turn_toward_target(px, pz, 0x80) == 0 and e.player_move_speed > 200 then
            e.groan_timer = e.groan_timer + 6
        end
    end

    if e.groan_timer > 0x40
        or player_unreachable(e)
        or (e.cerberus_barked and (e.cb_behflags_byte & 4) == 0) then
        e.ignore = 0
        e.behavior_flags = 4
        e.cb_swerve = 0
        if not e.cerberus_barked then
            e:play_enemy_sound(6)
            e.cerberus_barked = true
        end
    end
end

-- ---------------------------------------------------------------------------
-- State 2 - the hit reaction, switched on `ignore`
-- ---------------------------------------------------------------------------

local function damaged(e)
    e.status_flags = e.status_flags & 0x1F
    if e.cb_aiflags & AI_HURT == 0 then
        e.status_flags = e.status_flags | 0x40
    end
    e:check_visual_range(5000)

    local ignore = e.ignore
    if ignore == 0 then
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 3
        e.cb_aiflags = e.cb_aiflags | AI_HURT

        local beh = e.behavior_flags
        if not ((beh >= 4 and beh <= 7) or beh == 9) and (e.hit_state & 1) ~= 0 then
            -- A hit near the ground/strafe does not knock the dog down.
            e.ignore = 1
            e.animation_id = 9
        else
            e.tint_flashes = (e.player_angle & 0xFFF) - (e.angle & 0xFFF)
            e.tint_flashes = e.tint_flashes + RECOIL[(e:random() & 7) + 1]

            if beh == 4 or (e.cb_alert & 0x7F) == 0 then
                e.ignore = 3
                e.animation_id = 0x0C
                e.blend_counter = 0
                e.move_speed_current = 0xF0
                e:play_enemy_sound(1)
            else
                e.ignore = 2
                e.animation_id = 0x0B
                e.move_speed_current = sar16(e.move_speed_current, 1)
                e.cb_alert = e.cb_alert | 0x80
            end
        end
        e.action_behavior = 0
        e:advance_anim(0x400)
        e:play_enemy_sound(8)
        return
    end

    if ignore == 1 then
        e.cb_swerve = e:advance_anim(0x400) and 1 or 0
        if e.action_behavior == 0 then
            if e.cb_swerve ~= 0 then
                e.action_behavior = e.action_behavior + 1
                e.animation_id = 10
                e.timing_control = 0
                e.move_speed_current = 0x82
            end
        else
            if e.animation_frame_id < 0xF then
                if e.animation_frame_id > 7 then
                    slow_down(e, 0x20, 0x28)
                end
                e:move(0x800, e.move_speed_current)
                local px, _, pz = e:player_pos()
                e:rotate_toward_target(px, pz, 0x40)
            end
            if e.cb_swerve ~= 0 then
                e.state = 1
                e.ignore = 0
                e.behavior_flags = 4
                e.hit_state = 0
            end
        end
    elseif ignore == 2 then
        e:move(e.tint_flashes, e.move_speed_current)
        e:ballistic(0, e.cb_launch_vy, e.reaction_timer, 0)
        if e.pos_y >= 0 then
            e.ignore = e.ignore + 1
            e.animation_id = e.animation_id + 1
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 0
            e.pos_y = 0
            e:play_enemy_sound(1)
        end
        if e.animation_frame_id ~= 0 then
            e:advance_anim(0x200)
        end
        return
    elseif ignore == 3 then
        if e.animation_id == 0x0C then
            e:move(e.tint_flashes, e.move_speed_current)
            slow_down(e, 10, 0)
        end
        if e:advance_anim(0x400) then
            e.animation_id = e.animation_id + 1
            e.hit_state = 0
            if e.animation_id == 0x0E then
                e.cb_aiflags = e.cb_aiflags & ~AI_HURT
                local px, _, pz = e:player_pos()
                if e:turn_toward_target(px, pz, 0x200) == 0 then
                    e.ignore = e.ignore + 1
                    e.animation_id = 3
                else
                    e.ignore = 1
                    e.action_behavior = 1
                    e.animation_id = 10
                    e.move_speed_current = 0x82
                end
            end
            e.timing_control = 0
            e.blend_counter = 3
        end
    elseif ignore == 4 then
        if e.animation_frame_id == 10 and e.timing_control == 1 then
            e:play_enemy_sound(2)
        end
        if e:advance_anim(0x400) then
            e.state = 1
            e.ignore = 0
            e.behavior_flags = 4
            e.hit_state = 0
            e.cb_alert = e.cb_alert & 0x7F
            e:play_enemy_sound(6)
        end
    else
        e:check_special_weapon()
        return
    end

    e:check_special_weapon()
end

-- ---------------------------------------------------------------------------
-- State 3 - death, switched on `ignore`
-- ---------------------------------------------------------------------------

local function die(e)
    local ignore = e.ignore
    if ignore == 0 then
        e:raise_death_event()
        e.animation_frame_id = 0
        e.timing_control = 0
        e.blend_counter = 7
        e.action_behavior = 0
        e.tint_flashes = (e.player_angle & 0xFFF) - (e.angle & 0xFFF)
        if e.behavior_flags == 4 or (e.cb_alert & 0x7F) == 0 then
            e.ignore = 2
            e.animation_id = 0x0C
            e.blend_counter = 0
            e.move_speed_current = 0xF0
        else
            e.ignore = 1
            e.animation_id = 0x0B
            e.move_speed_current = sar16(e.move_speed_current, 1)
        end
        e:advance_anim(0x200)
        e:play_enemy_sound(4)
        return
    end

    if ignore == 1 then
        e:move(e.tint_flashes, e.move_speed_current)
        e:ballistic(0, e.cb_launch_vy, e.reaction_timer, 0)
        if e.pos_y >= 0 then
            e.ignore = e.ignore + 1
            e.animation_id = e.animation_id + 1
            e.animation_frame_id = 0
            e.timing_control = 0
            e.blend_counter = 0
            e.pos_y = 0
            e:play_enemy_sound(1)
        end
        if e.animation_frame_id ~= 0 then
            e:advance_anim(0x200)
        end
        return
    end

    if ignore == 2 then
        if e.animation_id == 0x0C then
            e:move(e.tint_flashes, e.move_speed_current)
            slow_down(e, 10, 0)
        end
        if e:advance_anim(0x200) then
            e.animation_id = e.animation_id + 1
            e.timing_control = 0
        end
        if e.animation_id == 0x0D then
            e.ignore = e.ignore + 1
            e.status_flags = e.status_flags | 10
            e.move_speed_current = 0
            e.action_ticks_counter = 0x46
            e.shadow_tint = 0x00FFFF50
            e:adjust_shadow_size(-90, -90)
        end
        return
    end

    if ignore == 3 then
        if s16(e.action_ticks_counter) == 0 then
            e.ignore = e.ignore + 1
            return
        end
        e:adjust_shadow_size(6, 6)
        e.action_ticks_counter = s16(e.action_ticks_counter) - 1
    end
end

-- ---------------------------------------------------------------------------
-- The dispatch block, read through both bases
-- ---------------------------------------------------------------------------

local function no_action(_e) end
local function beh_none(_e) end

-- One sixteen-slot block; `STATES[state + 1]` is the state view and
-- `STATES[5 + ignore + 1]` is the behaviour view (behaviour[n] = state[n + 5],
-- so behaviour 0 aliases state 5).
STATES = {
    init, -- 0
    behavior_run, -- 1
    damaged, -- 2
    die, -- 3
    no_action, -- 4
    beh_none, -- 5 == behaviour 0
    beh_patrol, -- 6 == behaviour 1
    beh_chase, -- 7 == behaviour 2
    beh_leap, -- 8 == behaviour 3
    beh_strafe, -- 9 == behaviour 4
    beh_alert, -- 10 == behaviour 5
    beh_maul, -- 11 == behaviour 6
    beh_bite, -- 12 == behaviour 7
    beh_scd, -- 13 == behaviour 8
    beh_stalk, -- 14 == behaviour 9
    -- 15 would be behaviour 10, NULL in the original.
}

-- ---------------------------------------------------------------------------
-- The per-frame entry
-- ---------------------------------------------------------------------------

function update(e)
    -- The position latch the "did not move" bit compares against.
    local start_x = e.pos_x
    local start_z = e.pos_z

    if not e.monster_paused then
        local state = e.state
        local handler = STATES[state + 1]
        if handler == nil then
            e:count_placeholder(state)
        else
            handler(e)
        end

        -- The SCA and player-collision tail; the dog has no room resolve of
        -- its own here, its only room resolve is the probe below.
        e:separate()

        -- Age the probe history: the low three bits shift up, everything
        -- above bit 3 is dropped.
        e.cb_probe = (e.cb_probe & 7) * 2
        if e.status_flags & 4 == 0 then
            if e:probe_ahead(500, 400) then
                e.cb_probe = e.cb_probe | 1
            end
        else
            -- Airborne: no probe, just mirror the world position into the
            -- 16-bit position words the animator reads.
            e.saved_x = e.pos_x
            e.saved_y = e.pos_y
            e.saved_z = e.pos_z
        end
    end

    -- Bit 4 = "I tried to move and went nowhere".
    if (e.cb_probe & 1) == 0 and e.pos_x == start_x and e.pos_z == start_z then
        e.cb_probe = e.cb_probe | 0x10
    end

    e:update_switch_zone()
end
