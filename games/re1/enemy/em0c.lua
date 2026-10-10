-- Tyrant, entity ids 0x0c (the lab encounter) and 0x10 (the heliport).
--
-- Both ids run one machine. The state byte dispatches init / think+act / hit /
-- forced / SCD; the action-behavior byte selects one of sixteen behaviours
-- through two bases: state 1 dispatches `table[action_behavior + 2]`, state 3
-- dispatches `table[action_behavior]` with no pathfinding. Everything the AI
-- writes is therefore in state-1 space (0 pause, 1 walk, 3 swipe, 4 slash,
-- 5 thrust, 6 backhand, 7 impale, 8 charge, 9 eruption, 10 rush, 11 rise).
--
-- id 0x0c starts in state 8 with SCD behaviour 11 (float in the stasis pod,
-- blocking on SysFlags 0x1F) and the room script walks it through the pod ->
-- glass break -> Wesker impale -> idle sequence. id 0x10 starts in state 1
-- with behaviour 9 (the eruption entrance), then fights on the heliport; its
-- rocket death runs SCD behaviour 13 and stops at `health = -1` / SysFlags
-- 0x1F (the ending sequence is a separate milestone).
--
-- The slash ribbon, claw ghosts and exposed heart live in Rust scratch
-- (`e:tyrant_trail_*`, `e:tyrant_ghosts_tick`, `e:tyrant_heart_*`); the
-- root-motion extractor (`e:tyrant_root_motion`) slides the walk, thrust and
-- impale clips; the player stagger/knock-down windows run in the player state
-- machine. All state lives on the Rust entity, so the VM may be reset between
-- any two updates.

-- The SCD victim is the enemy-list head (entity slot 1 in the port), the lab's
-- Wesker record.
local VICTIM = 1

-- SCA {0,-2000,0,2000,800}: radius 800, half height 2000, local centre
-- (0, -2000, 0).
local SCA_RADIUS = 800
local SCA_HALF_HEIGHT = 2000
local SCA_OFFSET_Y = -2000

-- The 53 glass shards, in the original's exact order (cluster A..F). Each is
-- {rotation base, x base, z base, xz mask, effect variant, light factor}; the
-- draw order is rot, y, x, z per shard.
local GLASS_SHARDS = {
    { -0x254, 0x2E18, 0x14B4, 0x1FF, 1, 0x0A },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 2, 0x14 },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 3, 0x0A },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 4, 0x1E },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 5, 0x0A },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 6, 0x14 },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 7, 0x14 },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 1, 0x0A },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 5, 0x0A },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 6, 0x1E },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 7, 0x14 },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 1, 0x0A },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 5, 0x0A },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 6, 0x1E },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 7, 0x14 },
    { -0x254, 0x2E18, 0x12C0, 0x1FF, 1, 0x0A },
    { 0x0B00, 0x28A0, 5000, 0x1FF, 1, 0x0A },
    { 0x0B00, 0x28A0, 5000, 0x1FF, 2, 0x14 },
    { 0x0B00, 0x28A0, 5000, 0x1FF, 3, 0x0A },
    { 0x0B00, 0x28A0, 5000, 0x1FF, 4, 0x14 },
    { 0x0B00, 0x28A0, 5000, 0x1FF, 5, 0x0A },
    { 0x0B00, 0x28A0, 5000, 0x1FF, 6, 0x19 },
    { 0x0B00, 0x28A0, 5000, 0x1FF, 7, 0x14 },
    { 0x0B00, 0x28A0, 5000, 0x1FF, 1, 0x0A },
    { 0x0B00, 0x28A0, 5000, 0x1FF, 5, 0x0A },
    { 0x0B00, 0x28A0, 5000, 0x1FF, 6, 0x14 },
    { 0x0B00, 0x28A0, 5000, 0x1FF, 7, 0x0F },
    { -0x31C, 11000, 0x1324, 0x7FF, 7, 0x0F },
    { -0x31C, 11000, 0x1324, 0x7FF, 1, 0x19 },
    { -0x31C, 11000, 0x1324, 0x7FF, 5, 0x0F },
    { -0x31C, 11000, 0x1324, 0x7FF, 6, 0x0A },
    { -0x31C, 11000, 0x1324, 0x7FF, 7, 0x0A },
    { -0x31C, 11000, 0x1324, 0x7FF, 1, 0x0A },
    { -0x31C, 11000, 0x1324, 0x7FF, 6, 0x14 },
    { -0x31C, 11000, 0x1324, 0x7FF, 7, 0x0A },
    { -0x31C, 11000, 0x1324, 0x7FF, 1, 0x0A },
    { 0x0CF4, 11000, 0x1324, 0x7FF, 1, 0x0F },
    { 0x0CF4, 11000, 0x1324, 0x7FF, 2, 0x19 },
    { 0x0CF4, 11000, 0x1324, 0x7FF, 3, 0x0F },
    { 0x0CF4, 11000, 0x1324, 0x7FF, 4, 0x0A },
    { 0x0CF4, 11000, 0x1324, 0x7FF, 5, 0x05 },
    { 0x0CF4, 11000, 0x1324, 0x7FF, 3, 0x0F },
    { 0x0CF4, 11000, 0x1324, 0x7FF, 4, 0x0F },
    { 0x0CF4, 11000, 0x1324, 0x7FF, 5, 0x0A },
    { -0x31C, 0x2A30, 0x1324, 0x7FF, 7, 0x19 },
    { -0x31C, 0x2A30, 0x1324, 0x7FF, 1, 0x05 },
    { -0x31C, 0x2A30, 0x1324, 0x7FF, 6, 0x0F },
    { -0x31C, 0x2A30, 0x1324, 0x7FF, 7, 0x19 },
    { -0x31C, 0x2A30, 0x1324, 0x7FF, 1, 0x05 },
    { 0x0CF4, 0x2A30, 0x1324, 0x7FF, 1, 0x05 },
    { 0x0CF4, 0x2A30, 0x1324, 0x7FF, 2, 0x05 },
    { 0x0CF4, 0x2A30, 0x1324, 0x7FF, 3, 0x0F },
    { 0x0CF4, 0x2A30, 0x1324, 0x7FF, 4, 0x0F },
}

-- The eruption's eleven debris sprites: {type, depth, light}.
local ERUPT_DEBRIS = {
    { 0x18, 3, 0x0F }, { 0x18, 5, 0x14 }, { 0x18, 5, 0x0F }, { 0x19, 6, 0x19 },
    { 0x19, 6, 0x0A }, { 0x19, 6, 0x0F }, { 0x19, 6, 0x0A }, { 0x18, 3, 0x14 },
    { 0x19, 6, 0x0A }, { 0x19, 6, 0x05 }, { 0x18, 3, 0x05 },
}

-- The rocket death's five severed limbs: {joint, vx, vy, vz, tx, ty, tz}.
local ROCKET_LIMBS = {
    { 2,  0,  -500,    0, 0x40,    0, 0x60 },
    { 4,  10, -400, 0x19, 0x40, 0x60,    0 },
    { 7, -0x1E, -0x1C2, 0x0F, 0,    0, 0x40 },
    { 10, -0x14, -0x15E, -10, 0, 0x60,    0 },
    { 12, -0x14, -300, 0x0F, 0, 0x40, 0x40 },
}

-- The frame-8 flash joints of the rocket death.
local ROCKET_FLASH_JOINTS = { 2, 4, 5, 7, 8, 10, 11, 12, 13, 14 }

local function abs(value)
    if value < 0 then
        return -value
    end
    return value
end

local function as_s8(value)
    value = value & 0xFF
    if value >= 0x80 then
        return value - 0x100
    end
    return value
end

local function as_s16(value)
    value = value & 0xFFFF
    if value >= 0x8000 then
        return value - 0x10000
    end
    return value
end

-- C's integer division rounds toward zero; Lua's `//` floors.
local function idiv(a, b)
    local q = a // b
    if a % b ~= 0 and ((a < 0) ~= (b < 0)) then
        q = q + 1
    end
    return q
end

-- `tyrant_player_distance` in the shared scratch: the quirky Manhattan sum the
-- original's thresholds run on (the negative-X path is one short).
local function distance(e)
    local d = e:web_distance()
    e.player_displacement = d
    return d
end

-- `tyrant_damage_player`: the second-playthrough fork.
local function damage_player(e, base, with_flag)
    if not e.second_playthrough then
        e.player_health = e.player_health - base
    else
        e.player_health = e.player_health - with_flag
    end
end

-- `tyrant_flash_claw_and_stagger`: tint the claw joint and pin the player's
-- generic hit pose. The player's animation window is selected by the frame
-- byte (`player_anim_frame_id`, the port's model of the player's animFrameId)
-- and its animation id by `player_attack_anim`.
local function flash_claw(e, behavior)
    e:tint_joint(8, 0xFF, 0x80880, 0x808080)
    e.player_attack_anim = 6
    e.player_anim_frame_id = 0x0C
    e.player_state = 6
    e.player_action_behavior = behavior
    e.player_action_state = 0
end

-- `tyrant_set_player_knockback`: the player's knock-back facing from the
-- Tyrant-to-player delta, with the two sites' base bias and zero-delta bias.
local function set_knockback(e, base, zero_bias)
    local dx = e.player_pos_x - e.pos_x
    local dz = e.player_pos_z - e.pos_z
    e.player_attack_anim = 6
    e.player_anim_frame_id = 0x0C
    e.player_state = 6
    e.player_action_behavior = 2
    e.player_action_state = 0
    if dx == 0 then
        local v = ((dz > 0) and 1 or 0) + zero_bias
        e.player_angle = (v << 11) & 0xFFFF
        return
    end
    local q = e:angle_quadrant(idiv(dz * 0x1000, dx))
    local cx = ((dx < 0) and 1 or 0) << 11
    e.player_angle = (base - cx - q) & 0xFFF
end

-- `tyrant_latch_player_attacker`.
local function latch_attacker(e, bias)
    e:latch_player_attacker(bias)
end

-- `tyrant_decay_speed`.
local function decay_speed(e, per_frame)
    local value = as_s16((e.move_speed_current & 0xFFFF) + (e.animation_frame_id * per_frame & 0xFFFF))
    if value < 0 then
        value = 0
    end
    e.move_speed_current = value
end

-- A hit test at the claw joint with the anchor y the site stages. The room
-- anchor `g_deadMoveValue+0x14` is the identity in the port, so the local
-- offset is (0, y, 0).
local function claw_reach(e, y, radius)
    return e:reach_test(8, 0, y, 0, radius)
end

-- The SCA pair and the room collision every gated frame runs (skipped while
-- the impale holds the grabbed player). The travelled distance lands in the
-- movement word the walk steering reads.
local function collision_tail(e)
    e:separate()
    local code, _, travelled = e:resolve_collision()
    e.attacking_direction = e.attacking_direction | code
    e.ty_walk_dist = travelled
end

local function root_motion(e, set, apply)
    e:tyrant_root_motion(set, apply)
end

-- ---------------------------------------------------------------------------
-- The behaviour table. State 3 dispatches `BEHAVIORS[action_behavior]`; state
-- 1 dispatches `BEHAVIORS[action_behavior + 2]`.
-- ---------------------------------------------------------------------------

local BEHAVIORS = {}

-- Table slot 0: id 0x0c strapped to the slab.
BEHAVIORS[0] = function(e)
    local sub = e.action_state
    if sub == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 0x0F
        e.animation_id = 8
        e.health = -1
        e:raise_death_event()
    elseif sub ~= 1 then
        if sub == 2 then
            e.status_flags = e.status_flags | 10
            e:raise_death_event()
        end
        root_motion(e, 0, false)
        e:move(0, as_s16(e.move_speed_current))
        return
    end
    e.action_state = e.action_state + (e:advance_anim(0x100) and 1 or 0)
    if e.animation_frame_id == 0x19 and (e.unk_bf & 1) ~= 0 then
        e:play_enemy_sound(6)
    end
    if e.animation_frame_id == 0x32 and (e.unk_bf & 1) ~= 0 then
        e:play_enemy_sound(6)
    end
    root_motion(e, 0, false)
    e:move(0, as_s16(e.move_speed_current))
end

-- Table slot 1: hand control to the SCD.
BEHAVIORS[1] = function(e)
    e:raise_death_event()
    e.behavior_flags = e.behavior_flags | 0x40
    e.animation_frame_id = 0
    e.unk_bf = 0
    e.animation_id = 0
    e.blend_counter = 0
    e.state_word = 0x01000008
end

-- Table slot 2: pause for 60 frames.
BEHAVIORS[2] = function(e)
    local sub = e.action_state
    if sub == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.animation_id = 0
        e.action_ticks_counter = 0x3C
        e.blend_counter = 0x1F
    elseif sub ~= 1 then
        return
    end
    e:advance_anim(0x80)
    local t = e.action_ticks_counter
    e.action_ticks_counter = t - 1
    if t == 0 then
        e.ignore = 0
        e.action_behavior = 1
        e.action_state = 0
    end
end

-- Table slot 3: walk the pathfinder waypoint.
BEHAVIORS[3] = function(e)
    local step = true
    local sub = e.action_state
    if sub == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 7
        e.animation_id = 1
    elseif sub ~= 1 then
        step = false
    end
    if step then
        e:tyrant_wander_turn(e.ty_walk_dist, 0x28, 0x28)
        if e.ty_close ~= 0 then
            e.ty_close = e.ty_close - 1
            e:tyrant_wander_turn(e.ty_walk_dist, 0x28, 0x28)
        end
        e:advance_anim(0x200)
    end
    local frame = e.animation_frame_id
    if frame > 5 and frame < 0x1F then
        if frame == 6 then
            e:play_enemy_sound(0)
        end
        root_motion(e, 0, false)
    else
        if frame == 0x1F then
            e:play_enemy_sound(0)
        end
        root_motion(e, 1, false)
    end
    e.move_speed_current = as_s16(e.move_speed_current + 0x19)
    e:move(0, as_s16(e.move_speed_current))
end

-- Table slot 4: a bare RET.
BEHAVIORS[4] = function(_e) end

-- Table slot 5: the wide claw swipe.
BEHAVIORS[5] = function(e)
    local sub = e.action_state
    if sub == 0 then
        e.ty_flags = e.ty_flags & 0xFB
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 7
        e.animation_id = 5
        e.move_speed_current = 200
        e:play_enemy_sound(1)
        e.ty_repause = 10
        e.ty_crowd = as_s8(e.ty_crowd + 0x14)
        e.ty_trail_timer = 0x801F
    elseif sub ~= 1 then
        if sub == 2 then
            e.state_word = 0x00010001
            e.ty_hit_mask = e.ty_hit_mask & 0x74
        end
        decay_speed(e, -5)
        e:move(0, as_s16(e.move_speed_current))
        return
    end
    e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
    if e.move_speed_current ~= 0 then
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x10)
        if e.ty_close ~= 0 then
            e:rotate_toward_target(px, pz, 0x10)
        end
    end
    local frame = e.animation_frame_id
    if ((frame - 5) & 0xFFFF) < 4 then
        if claw_reach(e, 500, 800) and e.player_health >= 0 then
            flash_claw(e, 1)
            e:spawn_joint_effect_at(0, 3, 8, 0, 800, 0, 0)
            e.player_attack_anim = 6
            e.player_anim_frame_id = 0x0C
            e.player_state = 6
            e.player_action_behavior = 1
            e.player_action_state = 0
            latch_attacker(e, 0)
            e.ty_hit_mask = e.ty_hit_mask | 1
            if (e.ty_flags & 4) == 0 then
                e:play_enemy_sound(2)
                e.ty_flags = e.ty_flags | 4
            end
        end
    end
    if ((frame - 5) & 0xFFFF) == 3 and (e.ty_hit_mask & 1) ~= 0 then
        damage_player(e, 10, 0x12)
    end
    if e.animation_frame_id == 0x32 then
        e.action_state = 2
    end
    decay_speed(e, -5)
    e:move(0, as_s16(e.move_speed_current))
end

-- Table slot 6: the big overhead slash.
BEHAVIORS[6] = function(e)
    local sub = e.action_state
    if sub == 0 then
        e.ty_flags = e.ty_flags & 0xFB
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 7
        e.animation_id = 4
        e.ty_repause = 0x0F
    elseif sub ~= 1 then
        if sub ~= 2 then
            return
        end
        e.state_word = 0x00010001
        local d = distance(e)
        if e.id == 0x10 and d > 5000 and (e.ty_hit_mask & 2) ~= 0 and (e:random() & 1) ~= 0 then
            e.state_word = 0x000A0101
        end
        e.ty_hit_mask = e.ty_hit_mask & 0x75
        return
    end
    local px, _, pz = e:player_pos()
    e:rotate_toward_target(px, pz, 8)
    if e.ty_close ~= 0 then
        e:rotate_toward_target(px, pz, 0x28)
    end
    if e.animation_frame_id == 7 then
        e:play_enemy_sound(1)
        e.ty_trail_timer = 0x8008
    end
    local frame = e.animation_frame_id
    if ((frame - 7) & 0xFFFF) <= 4 then
        if claw_reach(e, 0, 0x5DC) and e.player_health >= 0 then
            flash_claw(e, 1)
            if not e.second_playthrough then
                if e.id == 0x10 and e.player_health > 0x0C then
                    set_knockback(e, 0xFC00, 0)
                    e.ty_cooldown = 0xD2
                end
            elseif e.id == 0x10 and e.player_health > 0x12 then
                set_knockback(e, 0xFC00, 0)
                e.ty_cooldown = 0xD2
            end
            latch_attacker(e, 0x400)
            e.ty_hit_mask = e.ty_hit_mask | 2
            e.ty_flags = e.ty_flags | 4
        end
    end
    if ((frame - 7) & 0xFFFF) == 5 and (e.ty_hit_mask & 2) ~= 0 then
        damage_player(e, 0x0C, 0x12)
        local snd = 2
        if e.player_health >= 0 then
            snd = 3
        else
            e.player_health = 1
            e.ty_hit_mask = e.ty_hit_mask | 0x80
        end
        e:play_enemy_sound(snd)
    end
    e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
    if (e.ty_hit_mask & 0x82) == 0x82 and e.animation_frame_id == 0x11 then
        e.action_behavior = 6
        e.action_state = 0
        e:rotate_toward_target(px, pz, 0x20)
    end
end

-- Table slot 7: the claw thrust.
BEHAVIORS[7] = function(e)
    local sub = e.action_state
    if sub == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 7
        e.animation_id = 7
        e.ty_repause = 0x0F
    elseif sub ~= 1 then
        if sub == 2 then
            e.state_word = 0x00010001
            local d = distance(e)
            if e.id == 0x10 and d > 5000 and (e.ty_hit_mask & 4) ~= 0 and (e:random() & 1) ~= 0 then
                e.state_word = 0x000A0101
            end
            e.ty_hit_mask = e.ty_hit_mask & 0xFB
        end
        if e.animation_frame_id < 0x0E then
            root_motion(e, 0, true)
        else
            root_motion(e, 1, true)
        end
        return
    end
    if e.animation_frame_id == 10 then
        e.ty_trail_timer = 0x8008
    end
    if e.animation_frame_id == 0x0C then
        e:play_enemy_sound(1)
    end
    local frame = e.animation_frame_id
    if ((frame - 8) & 0xFFFF) < 8 then
        if claw_reach(e, 0, 0x5DC) and e.player_health >= 0 then
            flash_claw(e, 1)
            local hp = e.player_health
            if not e.second_playthrough then
                if hp > 0x10 then
                    set_knockback(e, 0x400, 1)
                end
            elseif hp > 0x14 then
                set_knockback(e, 0x400, 1)
            end
            e.ty_cooldown = 0xD2
            latch_attacker(e, 0x400)
            e.ty_hit_mask = e.ty_hit_mask | 4
        end
    end
    if ((frame - 8) & 0xFFFF) == 7 and (e.ty_hit_mask & 4) ~= 0 then
        damage_player(e, 0x10, 0x14)
        if e.player_health < 0 then
            e:play_enemy_sound(2)
        else
            e:play_enemy_sound(3)
        end
    end
    if e:advance_anim(0x200) then
        e.action_state = 2
        e.angle = as_s16(e.angle + 0x800)
    end
    if e.animation_frame_id < 0x0E then
        root_motion(e, 0, true)
    else
        root_motion(e, 1, true)
    end
end

-- Table slot 8: the backhand.
BEHAVIORS[8] = function(e)
    local sub = e.action_state
    if sub == 0 then
        e.ty_flags = e.ty_flags & 0xFB
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 7
        e.animation_id = 3
        e.move_speed_current = 0x12C
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x30)
        e.ty_trail_segments = 8
        e.ty_trail_timer = 0x0F
    elseif sub ~= 1 then
        if sub == 2 then
            e.state_word = 0x00010001
            e.ty_hit_mask = e.ty_hit_mask & 0x75
        end
        e:move(0, as_s16(e.move_speed_current))
        return
    end
    if e.animation_frame_id == 4 then
        e:play_enemy_sound(1)
    end
    local frame = e.animation_frame_id
    if ((frame - 4) & 0xFFFF) < 4 then
        if claw_reach(e, 500, 800) and e.player_health >= 0 then
            flash_claw(e, 0)
            latch_attacker(e, as_s16(-0x400))
            e.ty_hit_mask = e.ty_hit_mask | 8
            if (e.ty_flags & 4) == 0 then
                e:play_enemy_sound(2)
                e.ty_flags = e.ty_flags | 4
            end
        end
    end
    if ((frame - 4) & 0xFFFF) == 3 and (e.ty_hit_mask & 8) ~= 0 then
        damage_player(e, 0x0C, 0x12)
        if e.player_health < 0 and (e.ty_hit_mask & 0x82) == 0x82 then
            e.player_health = 1
        end
    end
    e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
    decay_speed(e, -3)
    if (e.ty_hit_mask & 0x8A) == 0x8A and e.animation_frame_id == 0x0C and e.player_attacked ~= 0 then
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x20)
        e.action_behavior = 3
        e.action_state = 0
        if e.player_health < 0x10 then
            if not e:player_facing_entity() then
                e.action_behavior = e.action_behavior + 4
            end
        end
    end
    e:move(0, as_s16(e.move_speed_current))
end

-- Table slot 9: the grab and impale kill move.
BEHAVIORS[9] = function(e)
    local sub = e.action_state
    if sub == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 7
        e.animation_id = 5
        e.move_speed_current = 200
        e.hit_state = 1
        e:snap_grab()
        e.status_flags = e.status_flags | 2
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x400)
        e.player_angle = e.angle
        e.player_attack_anim = 7
        e.player_anim_frame_id = 0x0C
        e.player_state = 7
        e.player_action_behavior = 0
        e.player_action_state = 0
        e:set_player_attacker()
        e:play_enemy_sound(1)
        e.ty_trail_timer = 0x802F
    elseif sub ~= 1 then
        if sub == 2 then
            e.ignore = 0
            e.action_behavior = 1
            e.action_state = 0
            e.ty_hit_mask = e.ty_hit_mask & 0xF8
            e.status_flags = e.status_flags & 0xFD
        end
        decay_speed(e, -5)
        e:move(0, as_s16(e.move_speed_current))
        e.unk_c6 = e.unk_c6 + (e.speed_x & 0xFFFF)
        e.unk_c8 = e.unk_c8 + (e.speed_z & 0xFFFF)
        e.player_unk_c6 = e.player_unk_c6 + (e.speed_x & 0xFFFF)
        e.player_unk_c8 = e.player_unk_c8 + (e.speed_z & 0xFFFF)
        return
    end
    if e.animation_frame_id == 5 or e.animation_frame_id == 0x5C then
        e:spawn_joint_effect_at(0, 3, 8, 0, 800, 0, 0)
    end
    if e.animation_frame_id == 5 then
        e:play_enemy_sound(4)
    end
    if e.animation_frame_id < 0x61 and e.animation_frame_id % 7 == 0 then
        e:spawn_joint_effect_at(0, 0, 8, 0, 800, 0, 0)
    end
    if e.animation_frame_id == 0x5F then
        e:play_enemy_sound(7)
    end
    e:apply_anim_vertex()
    e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
    if e.move_speed_current ~= 0 then
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x10)
    end
    decay_speed(e, -5)
    e:move(0, as_s16(e.move_speed_current))
    e.unk_c6 = e.unk_c6 + (e.speed_x & 0xFFFF)
    e.unk_c8 = e.unk_c8 + (e.speed_z & 0xFFFF)
    e.player_unk_c6 = e.player_unk_c6 + (e.speed_x & 0xFFFF)
    e.player_unk_c8 = e.player_unk_c8 + (e.speed_z & 0xFFFF)
    -- The grabbed player rides the root motion: the port moves the player by
    -- the same per-frame velocity the offsets carry.
    local px, py, pz = e:player_pos()
    e:set_player_pos(px + as_s16(e.speed_x), py, pz + as_s16(e.speed_z))
end

-- Table slot 10: charge, then the heavy swing.
BEHAVIORS[10] = function(e)
    local sub = e.action_state
    if sub == 0 then
        e.action_state = 1
        e.animation_frame_id = e.animation_frame_id >> 1
        e.unk_bf = 0
        e.blend_counter = 7
        e.animation_id = 2
        e.move_speed_current = 0x190
        if e:flag_bit(4, 0x1E) then
            e.move_speed_current = 0x12C
        end
        e.ty_repause = 0x0F
        sub = 1
    end
    if sub == 1 then
        local px, _, pz = e:player_pos()
        if e:flag_bit(4, 0x1E) then
            e:rotate_toward_target(px, pz, 0x30)
        else
            e:rotate_toward_target(px, pz, 0x20)
        end
        e:advance_anim(0x200)
        local d = distance(e)
        if (d < 5000 and as_s16(e:turn_toward_target(px, pz, 0x80)) == 0)
            or as_s16(e:turn_toward_target(px, pz, 900)) ~= 0
        then
            e.action_state = 2
        end
        local sx, sy, sz = e.pos_x, e.pos_y, e.pos_z
        local code = e:resolve_collision()
        e.pos_x, e.pos_y, e.pos_z = sx, sy, sz
        if code ~= 0 then
            e.action_state = 2
        end
        if e.animation_frame_id == 3 or e.animation_frame_id == 0x0F then
            e:play_enemy_sound(0)
        end
    elseif sub == 2 then
        e.action_state = 3
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 7
        e.animation_id = 9
        e.move_speed_current = 0x12C
        sub = 3
    end
    if sub == 3 then
        if e.animation_frame_id == 8 then
            e.ty_trail_timer = 0x800F
        end
        local phase = (e.animation_frame_id - 0x0F) & 0xFFFF
        if phase < 5 then
            if claw_reach(e, 500, 0x708) and e.player_health >= 0 then
                flash_claw(e, 0)
                latch_attacker(e, as_s16(-0x400))
                e.ty_hit_mask = e.ty_hit_mask | 8
                e.ty_cooldown = 0xD2
            end
        end
        if phase == 4 and (e.ty_hit_mask & 8) ~= 0 then
            damage_player(e, 0x14, 0x1E)
            e:play_enemy_sound(2)
        end
        if e.animation_frame_id > 7 and e.animation_frame_id < 0x0F then
            e:spawn_joint_effect_at(0x11, 4, 8, 0, 1000, 0, 0)
            e:spawn_joint_effect_at(0x11, 4, 8, 0, 900, 0, 0)
        end
        if e.animation_frame_id == 7 then
            e:play_enemy_sound(5)
        end
        if e.animation_frame_id == 0x0D then
            e:play_enemy_sound(1)
        end
        e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
        decay_speed(e, -1)
    elseif sub == 4 then
        e.state_word = 0x00010001
        e.ty_hit_mask = e.ty_hit_mask & 0xF7
    end
    e:move(0, as_s16(e.move_speed_current))
end

-- Table slot 11: the eruption entrance (id 0x10).
BEHAVIORS[11] = function(e)
    local sub = e.action_state
    if sub == 0 then
        e.pos_y = 0x12C
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 7
        e.animation_id = 6
        e:advance_anim(0x200)
        e.action_ticks_counter = 0x3C
        e:play_voice(0x2D)
        e.voice_playing = true
        e:play_3d_sound(2, 0x1C)
        e:srand(0xB23)
        sub = 1
    end
    if sub == 1 then
        e.action_ticks_counter = e.action_ticks_counter - 1
        local t = as_s16(e.action_ticks_counter)
        if t == 0 then
            e.action_state = e.action_state + 1
            return
        end
        if t > 0x19 and t < 0x21 then
            for i = 1, 11 do
                local burst = ERUPT_DEBRIS[i]
                local x = (e:random() & 0x7FF) - 0x1194
                local y = -(e:random() & 0x1FF)
                local z = -(e:random() & 0x7FF)
                local yaw = as_s16(e:random() & 0xFFF)
                e:spawn_effect(burst[1], burst[2], x, y, z, yaw, burst[3])
            end
        end
        local u = as_s16(e.action_ticks_counter)
        if u > 0x25 and u < 0x37 then
            for i = 0, 2 do
                local x = (e:random() & 0xFFF) - 3000
                local y = -(e:random() & 0x1FF)
                local z
                if i == 0 then
                    z = e:random() & 0x7FF
                else
                    z = -(e:random() & 0x7FF)
                end
                e:spawn_effect(9, 5, x, y, z, 0, 0x1E)
            end
        end
    elseif sub == 2 then
        e:advance_anim(0x200)
        if e.animation_frame_id == 0x21 then
            e.action_state = e.action_state + 1
            e.action_ticks_counter = 1
        end
        e.pos_z = e.pos_z - 8
        e.pos_x = e.pos_x - 8
        return
    elseif sub == 3 then
        e.action_ticks_counter = e.action_ticks_counter - 1
        if as_s16(e.action_ticks_counter) < 1 then
            e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
            if e.animation_frame_id < 0x23 then
                e.pos_z = e.pos_z - 0x14
                e.pos_x = e.pos_x - 0x14
            end
            if e.animation_frame_id == 0x34 then
                e:play_3d_sound(2, 0x1D)
            end
            if e.animation_frame_id == 0x40 then
                e:play_3d_sound(2, 0x1E)
            end
            e.pos_y = e.pos_y - 10
            if e.pos_y < 0 then
                e.pos_y = 0
            end
            local frame = e.animation_frame_id
            if frame > 0x23 and frame < 0x5A and (frame & 3) == 0 then
                -- Two puffs riding a random joint. The jitter order differs
                -- between them (joint then y, then y then joint).
                local r = e:random()
                local s = (r >= 0x80000000) and -1 or 0
                local v = ((r ~ s) - s)
                local joint = as_s16(((v & 0xF) ~ s) - s)
                e:spawn_joint_effect_at(9, 1, joint, 0, -(e:random() & 0x1FF), 0, 0)
                local y2 = -(e:random() & 0x1FF)
                local r2 = e:random()
                local s2 = (r2 >= 0x80000000) and -1 or 0
                local v2 = ((r2 ~ s2) - s2)
                local joint2 = as_s16(((v2 & 0xF) ~ s2) - s2)
                e:spawn_joint_effect_at(9, 1, joint2, 0, y2, 0, 0)
            end
            if e.animation_frame_id == 0x2B then
                e.ty_flags = e.ty_flags | 1
                return
            end
        end
    elseif sub == 4 then
        e.ignore = 0
        e.action_behavior = 1
        e.action_state = 0
        e.ty_flags = e.ty_flags | 2
        return
    end
end

-- Table slot 12: the enraged rush.
BEHAVIORS[12] = function(e)
    local sub = e.action_state
    if sub == 0 then
        e.action_state = 1
        e.animation_frame_id = e.animation_frame_id >> 1
        e.unk_bf = 0
        e.blend_counter = 7
        e.animation_id = 2
        e.move_speed_current = 0x190
        if e:flag_bit(4, 0x1E) then
            e.move_speed_current = 0x12C
        end
        e.action_ticks_counter = 0x3C
        e.ty_repause = 0x0D
        sub = 1
    end
    if sub == 1 then
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 0x80)
        e:advance_anim(0x200)
        local d = distance(e)
        if d < 6000 and as_s16(e:turn_toward_target(px, pz, 0x200)) == 0 then
            e.state_word = 0x00010001
            if e.ty_cooldown == 0 then
                e.state_word = 0x020A0101
            end
            e.ty_cooldown = 0xD2
        end
        local sx, sy, sz = e.pos_x, e.pos_y, e.pos_z
        local code = e:resolve_collision()
        e.pos_x, e.pos_y, e.pos_z = sx, sy, sz
        if e.action_ticks_counter ~= 0 then
            e.action_ticks_counter = e.action_ticks_counter - 1
        end
        if code ~= 0 or e.action_ticks_counter == 0 then
            e.state_word = 0x00010001
            if e.ty_cooldown == 0 then
                e.state_word = 0x020A0101
            end
            e.ty_cooldown = 0xD2
        end
        if e.animation_frame_id == 3 or e.animation_frame_id == 0x0F then
            e:play_enemy_sound(0)
        end
    elseif sub == 2 then
        e.ty_flags = e.ty_flags & 0xFB
        e.action_state = 3
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 3
        e.animation_id = 0x0B
        e.ty_repause = 0x0F
        sub = 3
    end
    if sub == 3 then
        local px, _, pz = e:player_pos()
        e:rotate_toward_target(px, pz, 8)
        if e.animation_frame_id == 6 then
            e.ty_trail_timer = 0x800A
        end
        if e.animation_frame_id == 10 then
            e:play_enemy_sound(1)
        end
        if e.animation_frame_id < 3 then
            e.move_speed_current = as_s16(e.move_speed_current + 0x1E)
        end
        local phase = (e.animation_frame_id - 9) & 0xFFFF
        if phase < 8 then
            if claw_reach(e, 0, 0x4B0) and e.player_health >= 0 then
                flash_claw(e, 1)
                local hp = e.player_health
                if not e.second_playthrough then
                    if hp > 0x0C then
                        set_knockback(e, 0x400, 1)
                    end
                elseif hp > 0x12 then
                    set_knockback(e, 0x400, 1)
                end
                latch_attacker(e, 0x400)
                e.ty_hit_mask = e.ty_hit_mask | 2
                e.ty_flags = e.ty_flags | 4
            end
        end
        if e.ty_claw_scale_b > 5000 and phase > 9 then
            e.ty_claw_scale_b = as_s16(e.ty_claw_scale_b - 800)
        end
        if phase == 8 and (e.ty_hit_mask & 2) ~= 0 then
            damage_player(e, 0x14, 0x1E)
            local snd = 2
            if e.player_health >= 0 then
                snd = 3
            else
                e.player_health = 1
                e.ty_hit_mask = e.ty_hit_mask | 0x80
            end
            e:play_enemy_sound(snd)
        end
        e.action_state = e.action_state + (e:advance_anim(0x400) and 1 or 0)
        if (e.ty_hit_mask & 0x82) == 0x82 and e.animation_frame_id == 0x11 then
            e.action_behavior = 6
            e.action_state = 0
            e:rotate_toward_target(px, pz, 0x20)
        end
        if (e.ty_hit_mask & 2) == 0 and e.animation_frame_id < 0x11 then
            e:rotate_toward_target(px, pz, 0x40)
        end
        if e.move_speed_current ~= 0 and e.animation_frame_id > 8 then
            decay_speed(e, -1)
        end
    elseif sub == 4 then
        e.ty_claw_scale_b = e.ty_claw_scale_a
        e.state_word = 0x00010001
        if e.id == 0x10 and (e.ty_hit_mask & 2) ~= 0 and (e:random() & 1) ~= 0 then
            e.state_word = 0x000A0101
            e.state_word = 0x00050101
        end
        e.ty_hit_mask = e.ty_hit_mask & 0x75
    end
    e:move(0, as_s16(e.move_speed_current))
end

-- Table slot 13: rise / power up.
BEHAVIORS[13] = function(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 0x1F
        e.animation_id = 0x0C
    end
    local frame = e.animation_frame_id
    if frame > 0x28 then
        if e.ty_claw_scale_b < 9000 then
            e.ty_claw_scale_b = as_s16(e.ty_claw_scale_b + 1000)
        end
        frame = e.animation_frame_id
    end
    if frame == 0x28 then
        e:play_enemy_sound(1)
    end
    if e:advance_anim(0x80) then
        e.state_word = 0x000A0101
    end
    root_motion(e, 1, true)
end

-- Table slot 14: id 0x0c's decision layer.
BEHAVIORS[14] = function(e)
    if e.ty_repause ~= 0 then
        e.ty_repause = e.ty_repause - 1
    end
    e.ty_cooldown = 0
    if e.ty_repause ~= 0 then
        return
    end
    local d = e.player_displacement
    local px, _, pz = e:player_pos()
    if d < 0xA8C and as_s16(e:turn_toward_target(px, pz, 0x340)) == 0 then
        e.state_word = 0x00040101
    end
    if e.ty_close ~= 0 and d < 0xA8C and as_s16(e:turn_toward_target(px, pz, 0x340)) == 0 then
        e.state_word = 0x00040101
        e.ty_hit_mask = e.ty_hit_mask | 0x80
        return
    end
    if d < 2000 and as_s16(e:turn_toward_target(px, pz, 0x200)) == 0 then
        e.state_word = 0x00030101
        if e.player_health < 0x10 then
            if not e:player_facing_entity() then
                e.action_behavior = e.action_behavior + 4
            end
        end
    end
end

-- Table slot 15: id 0x10's decision layer.
BEHAVIORS[15] = function(e)
    if e.ty_repause ~= 0 then
        e.ty_repause = e.ty_repause - 1
    end
    if e.player_weapon == 10 then
        e.ty_rocket_flag = e.ty_rocket_flag | 0x80
    end
    if e.ty_cooldown ~= 0 then
        e.ty_cooldown = e.ty_cooldown - 1
    end
    local d = e.player_displacement
    local px, _, pz = e:player_pos()
    if e.ty_repause == 0 then
        if d > 12000 and as_s16(e:turn_toward_target(px, pz, 0x40)) == 0 then
            e.state_word = 0x00080101
        end
        local skip_short = false
        if e.ty_close ~= 0 and d < 0xA8C then
            if as_s16(e:turn_toward_target(px, pz, 0x340)) == 0 then
                e.state_word = 0x00040101
                e.ty_hit_mask = e.ty_hit_mask | 0x80
                return
            end
        else
            skip_short = (e.ty_close ~= 0)
        end
        if not skip_short then
            if d < 0xA8C and as_s16(e:turn_toward_target(px, pz, 0x340)) == 0 then
                e.state_word = 0x00040101
            end
        end
        if d < 2000 and as_s16(e:turn_toward_target(px, pz, 0x200)) == 0 then
            e.state_word = 0x00030101
            if e.player_health < 0x10 then
                if not e:player_facing_entity() then
                    e.action_behavior = e.action_behavior + 4
                end
            end
        end
        if d < 7000 and as_s16(e:turn_toward_target(px, pz, 0x4C8)) ~= 0 then
            e.state_word = 0x00050101
        end
        if e.ty_cooldown == 0 then
            e.state_word = 0x000B0101
        end
    end
    if e:flag_bit(4, 0x1E) and d < 0xA8C and as_s16(e:turn_toward_target(px, pz, 0x200)) == 0 then
        e.state_word = 0x00070101
    end
end

-- The behaviour table by index (state 3's base).
local TYRANT_BEHAVIORS = {
    [0] = BEHAVIORS[0],
    [1] = BEHAVIORS[1],
    [2] = BEHAVIORS[2],
    [3] = BEHAVIORS[3],
    [4] = BEHAVIORS[4],
    [5] = BEHAVIORS[5],
    [6] = BEHAVIORS[6],
    [7] = BEHAVIORS[7],
    [8] = BEHAVIORS[8],
    [9] = BEHAVIORS[9],
    [10] = BEHAVIORS[10],
    [11] = BEHAVIORS[11],
    [12] = BEHAVIORS[12],
    [13] = BEHAVIORS[13],
    [14] = BEHAVIORS[14],
    [15] = BEHAVIORS[15],
}

local function run_behavior(e, index)
    local behavior = TYRANT_BEHAVIORS[index]
    if behavior then
        behavior(e)
    end
end

-- ---------------------------------------------------------------------------
-- The SCD table (state 8, `behavior_flags & 0x40`). Indexed by the raw
-- action_behavior byte.
-- ---------------------------------------------------------------------------

local SCD = {}

-- Behaviour 0: hold the idle pose.
SCD[0] = function(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.animation_id = 0
        e.blend_counter = 0x1F
        e.ty_trail_timer = 0
    end
    e:advance_anim(0x80)
end

-- Behaviour 2: walk to the SCD target and raise the completion flag.
SCD[2] = function(e)
    local step = true
    local sub = as_s8(e.action_state)
    if sub == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 0x1F
        e.animation_id = 1
        e.action_ticks_counter = 2
        if (e.behavior_flags & 1) ~= 0 then
            e.action_ticks_counter = 1
        end
    elseif sub ~= 1 then
        if sub == 2 then
            e:raise_scd_flag()
            if (e.collision_flags & 0x80) == 0 then
                e.state_word = e.state_word & 0x0000FFFF
            else
                e.action_state = 1
            end
        end
        step = false
    end
    if step then
        local tx = e.unk_c6
        local tz = e.unk_c8
        e:rotate_toward_target(tx, tz, 0x28)
        e.action_ticks_counter = e.action_ticks_counter - 1
        if e.action_ticks_counter == 0 then
            e:advance_anim(0x80)
            e.action_ticks_counter = 2
            if (e.behavior_flags & 1) ~= 0 then
                e.action_ticks_counter = 1
            end
        end
        if e:xz_distance_to(tx, tz) < 0x5DC then
            e.action_state = e.action_state + 1
        end
    end
    if (e.animation_frame_id == 3 or e.animation_frame_id == 0x24) and (e.action_ticks_counter & 1) ~= 0 then
        e:play_enemy_sound(0)
    end
    if e.animation_frame_id > 3 and e.animation_frame_id < 0x23 then
        root_motion(e, 0, false)
    else
        root_motion(e, 1, false)
    end
    e.move_speed_current = as_s16(e.move_speed_current + 5)
    e:move(0, as_s16(e.move_speed_current))
end

-- Behaviour 10: break out of the capsule.
SCD[10] = function(e)
    local sub = as_s8(e.action_state)
    if sub == 0 then
        e.pos_y = 0
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 0x1F
        e.animation_id = 6
    elseif sub ~= 1 then
        if sub ~= 2 then
            return
        end
        e:raise_scd_flag()
        e.state_word = e.state_word & 0x0000FFFF
        return
    end
    e.action_state = e.action_state + (e:advance_anim(0x80) and 1 or 0)
    if e.animation_frame_id == 0x1E then
        e:spawn_world_effect(1, 0, 0x2A30, -0x1004, 5000, 0)
        e:play_3d_sound_at(2, 0x19, 0x2A30, -0x1004, 5000)
    end
    if e.animation_frame_id == 0x55 then
        e:play_voice(0xB3)
        e.voice_playing = true
        e:play_3d_sound_at(2, 0x1A, 0x2A30, -0x1004, 5000)
        e:play_3d_sound_at(2, 0x1B, 0x2A30, -0x1004, 5000)
    end
    if e.animation_frame_id == 0x59 then
        for i = 1, 53 do
            local s = GLASS_SHARDS[i]
            local rot = as_s16((e:random() & 0x1FF) + s[1])
            local y = -0xA28 - (e:random() & 0xFFF)
            local x = (e:random() & s[4]) + s[2]
            local z = (e:random() & s[4]) + s[3]
            e:spawn_world_effect(0x15, s[5], x, y, z, rot)
        end
    end
    if e.animation_frame_id == 0x91 then
        e.ty_flags = e.ty_flags | 1
    end
    if e.animation_frame_id == 0x97 or e.animation_frame_id == 0x8F then
        e:play_enemy_sound(0)
    end
end

-- Behaviour 11: float in the stasis pod, blocking on SysFlags 0x1F.
SCD[11] = function(e)
    local sub = e.action_state
    if sub == 0 then
        -- The original also drops a stage texture set here on the lab stage
        -- (`StMask`); the port has no page-gardening consumer, so that side
        -- effect is inert.
        e.pos_y = -200
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 0
        e.animation_id = 6
        e.action_ticks_counter = 5
        e:advance_anim(0x80)
        sub = 1
    end
    if sub == 1 then
        e.action_ticks_counter = e.action_ticks_counter - 1
        if e.action_ticks_counter == 0 then
            e.action_ticks_counter = 5
            e.pos_y = e.pos_y - 5
            if e.pos_y == -0x15E then
                e.action_state = e.action_state + 1
            end
        end
    elseif sub == 2 then
        e.action_ticks_counter = e.action_ticks_counter - 1
        if e.action_ticks_counter == 0 then
            e.action_ticks_counter = 5
            e.pos_y = e.pos_y + 5
            if e.pos_y == -200 then
                e.action_state = e.action_state - 1
            end
        end
    elseif sub == 3 then
        e.pos_y = e.pos_y + 4
        if e.pos_y > 0 then
            e:raise_scd_flag()
            e.action_state = e.action_state + 1
            e.pos_y = 0
        end
    end
    if e:flag_bit(4, 0x1F) then
        e.action_state = 3
    end
end

-- Behaviour 12: the scripted impale of the victim entity.
SCD[12] = function(e)
    local sub = e.action_state
    if sub == 0 then
        -- The original drops texture set 2 on the main lab here
        -- (`TexturePage_DeleteSet`); inert in the port like the other
        -- page-gardening calls.
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 0x1F
        e.animation_id = 5
        e.move_speed_current = 100
        e:snap_grab_slot(VICTIM)
        e.status_flags = e.status_flags | 2
        local vx, _, vz = e:entity_pos(VICTIM)
        e:rotate_toward_target(vx, vz, 0x400)
        e:set_entity_status(VICTIM, e:entity_status(VICTIM) | 6)
        e:set_entity_state_word(VICTIM, 0x00030001)
        e:play_enemy_sound(1)
        return
    end
    if sub ~= 1 then
        if sub ~= 2 then
            return
        end
        e:raise_flag(4, 0x1E)
        e.action_behavior = 0
        e.action_state = 0
        e.ty_hit_mask = e.ty_hit_mask & 0xF8
        e.status_flags = e.status_flags & 0xFD
        return
    end
    if e.animation_frame_id == 6 then
        e:play_enemy_sound(4)
    end
    if e.animation_frame_id == 6 or e.animation_frame_id == 0x5C then
        e:spawn_joint_effect_at(0, 3, 8, 0, 800, 0, 0)
        e:tint_joint(8, 0xFF, 0x80880, 0x808080)
    end
    if e.animation_frame_id < 0x61 and e.animation_frame_id % 7 == 0 then
        e:spawn_joint_effect_at(0, 0, 8, 0, 800, 0, 0)
    end
    if e.animation_frame_id == 0x5F then
        e:play_enemy_sound(7)
    end
    e:apply_anim_vertex()
    e.action_state = e.action_state + (e:advance_anim(0x80) and 1 or 0)
    if e.move_speed_current ~= 0 then
        local vx, _, vz = e:entity_pos(VICTIM)
        e:rotate_toward_target(vx, vz, 0x10)
    end
    decay_speed(e, -5)
    e:move(0, as_s16(e.move_speed_current))
    e.unk_c6 = e.unk_c6 + (e.speed_x & 0xFFFF)
    e.unk_c8 = e.unk_c8 + (e.speed_z & 0xFFFF)
    e:add_entity_anim_offset(VICTIM, e.speed_x, e.speed_z)
end

-- Behaviour 13: the rocket-launcher death.
SCD[13] = function(e)
    local sub = as_s8(e.action_state)
    if sub == 0 then
        e.ty_flags = e.ty_flags | 8
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 7
        e.animation_id = 0
        e.ty_flags = e.ty_flags & 0xFD
        e.look_at_flags = 0
        e.action_ticks_counter = 0
        e:srand(1534)
        e:tyrant_heart_set_speed(-0x12C)
        e:play_3d_sound(2, 0x19)
        e:play_3d_sound(2, 0x1A)
        e:play_3d_sound(2, 0x1B)
        e:set_joint_flag(0, e:joint_flag(0) | 0x28)
        e:arm_joint_effect(0, 0x13, 0x0C, 3)
        e:arm_joint_effect(1, 0x13, 0, 3)
        e:arm_joint_effect(3, 0x13, 0, 3)
        e:arm_joint_effect(6, 0x13, 0, 3)
        e:arm_joint_effect(9, 0x13, 0, 3)
        for i = 1, 5 do
            local limb = ROCKET_LIMBS[i]
            e:tyrant_limb_launch(i - 1, limb[2], limb[3], limb[4], limb[5], limb[6], limb[7])
            e:set_joint_visible(limb[1], false)
        end
        e.action_ticks_counter = 0
        e:store_camera_speed()
        return
    end
    if sub ~= 1 then
        if sub ~= 2 then
            return
        end
        if as_s16(e.action_ticks_counter) > 200 then
            e.action_state = 3
            e.health = -1
        end
        e.action_ticks_counter = e.action_ticks_counter + 1
        if (e.action_ticks_counter & 3) == 0 then
            local x = (e:random() & 0x7FF) - 1000
            local z = (e:random() & 0x7FF) - 1000
            e:spawn_effect(9, 0x0D, x, 0, z, 0, 0x1E)
        end
        if (e.action_ticks_counter & 7) ~= 0 then
            return
        end
        for _, limb in ipairs({ 1, 2, 3, 4 }) do
            local lx, ly, lz = e:tyrant_limb_pos(limb)
            e:spawn_world_effect(9, 0x0D, lx, ly + 1000, lz, 0)
        end
        return
    end
    if e.action_ticks_counter == 2 then
        e:play_3d_sound(2, 0x19)
        e:play_3d_sound(2, 0x1A)
        e:play_3d_sound(2, 0x1B)
    end
    if e.action_ticks_counter == 6 then
        e:play_3d_sound(2, 0x19)
        e:play_3d_sound(2, 0x1A)
        e:play_3d_sound(2, 0x1B)
    end
    if e.action_ticks_counter == 9 then
        e:play_3d_sound(2, 0x19)
    end
    if e.animation_frame_id < 0x0E then
        local t = as_s16(e.action_ticks_counter)
        local hx = e.pos_x + idiv(t * 0x0E7A, 0x0C)
        local hy = e.pos_y + idiv(t * -0x0FB0, 0x0C) - 0x9C4
        local hz = e.pos_z + idiv(t * -0x09AE, 0x0C)
        e:spawn_world_effect(0x0E, 3, hx, hy, hz, 0)
        e:spawn_world_effect(0x0E, 3, hx - (e:random() & 0x7F) + 0x40, hy - (e:random() & 0x7F) + 0x40, hz - (e:random() & 0x7F) + 0x40, 0)
        e.action_ticks_counter = e.action_ticks_counter + 1
        if as_s16(e.action_ticks_counter) > 10 then
            e.action_ticks_counter = 10
            e:spawn_world_effect(0x0E, 3, hx - (e:random() & 0x1FF) + 0x100, hy - (e:random() & 0x7F) + 0x40, hz - (e:random() & 0x1FF) + 0x100, 0)
            e:spawn_world_effect(0x0E, 0x0B, hx - (e:random() & 0x1FF) + 0x100, hy - (e:random() & 0x7F) + 0x40, hz - (e:random() & 0x1FF) + 0x100, 0)
            e.ty_flags = e.ty_flags & 0xFE
        end
    end
    if e.animation_frame_id == 8 then
        for _, joint in ipairs(ROCKET_FLASH_JOINTS) do
            e:tint_joint(joint, 0, 0x102810, 0x202030)
        end
    end
    if e.animation_frame_id < 10 and (e.animation_frame_id & 7) == 0 then
        local x = (e:random() & 0x3FF) - 500
        local y = e:random() & 0x3FF
        local z = (e:random() & 0x3FF) - 500
        for i = 0, 4 do
            local lx, ly, lz = e:tyrant_limb_pos(i)
            e:spawn_world_effect(0x0E, 3, lx + x, ly + y, lz + z, 0)
        end
    end
    if e.animation_frame_id > 0x10 and e.animation_frame_id < 0x13 then
        local puffs = {
            { 9, 0x05 }, { 9, 0x15 }, { 9, 0x15 },
            { 0x0E, 0x01 }, { 0x0E, 0x11 }, { 0x0E, 0x01 },
        }
        for _, puff in ipairs(puffs) do
            local x = (e:random() & 0x7FF) - 1000
            local y = -1000 - (e:random() & 0x7FF)
            local z = (e:random() & 0x7FF) - 1000
            e:spawn_effect(puff[1], puff[2], x, y, z, 0, 0x1E)
        end
    end
    if e.animation_frame_id > 0x12 and ((e.animation_frame_id + 1) & 3) == 0 then
        local x = (e:random() & 0x3FF) - 500
        local y = e:random() & 0x3FF
        local z = (e:random() & 0x3FF) - 500
        for i = 0, 4 do
            local lx, ly, lz = e:tyrant_limb_pos(i)
            e:spawn_world_effect(9, 0x0D, lx + x, ly + y, lz + z, 0)
        end
    end
    if e:advance_anim(0x200) then
        e.action_state = 2
        e.action_ticks_counter = 0
    end
    e:tyrant_heart_drop()
    for i = 0, 4 do
        e:tyrant_limb_update(i)
    end
end

-- Behaviour 14: play animation 10, then report.
SCD[14] = function(e)
    if e.action_state == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 0x1F
        e.animation_id = 10
        e.move_speed_current = 0
    elseif e.action_state ~= 1 then
        return
    end
    if e:advance_anim(0x80) then
        e:raise_scd_flag()
        e.state_word = e.state_word & 0x0000FFFF
    end
end

-- Behaviour 15: turn to face the player, then report.
SCD[15] = function(e)
    if e.action_state == 0 then
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.blend_counter = 0x1F
        e.hit_state = 0
        local px, _, pz = e:player_pos()
        if as_s16(e:turn_toward_target(px, pz, 0x400)) ~= 0 then
            e.animation_id = 1
        end
        e.action_state = 1
        e.action_ticks_counter = (e:random() & 0x3F) + 0x28
    end
    local step_size = 0x28
    if (e.behavior_flags & 1) ~= 0 then
        step_size = 0x14
    end
    local px, _, pz = e:player_pos()
    local step = as_s16(e:turn_toward_target(px, pz, step_size))
    local prev = as_s16(e.action_ticks_counter)
    e.action_ticks_counter = prev - 1
    if prev == 0 or step == 0 then
        e:raise_scd_flag()
        e.state_word = e.state_word & 0x0000FFFF
        local ppx, _, ppz = e:player_pos()
        e.player_pos_x = as_s16(ppx)
        e.player_pos_z = as_s16(ppz)
    else
        e:advance_anim(0x80)
        e.angle = as_s16(e.angle + step)
    end
    if e.animation_frame_id == 3 or e.animation_frame_id == 0x24 then
        e:play_enemy_sound(0)
    end
end

-- Behaviour 16: the scripted swing at the victim.
SCD[16] = function(e)
    local sub = as_s8(e.action_state)
    if sub == 0 then
        e.action_state = 1
        e.animation_frame_id = 0
        e.unk_bf = 0
        e.animation_id = 4
        e.blend_counter = 7
        e:play_enemy_sound(1)
    elseif sub ~= 1 then
        if sub ~= 2 then
            return
        end
        e:raise_flag(4, 0x1E)
        e.state_word = e.state_word & 0x0000FFFF
        return
    end
    e.action_state = e.action_state + (e:advance_anim(0x200) and 1 or 0)
    if e.animation_frame_id == 0x0B then
        e:set_entity_state_word(VICTIM, 0x00010001)
        e:spawn_joint_effect_at(0, 3, 8, 0, 800, 0, 0)
        e:tint_joint(8, 0xFF, 0x80880, 0x808080)
        e:play_enemy_sound(2)
    end
end

-- ---------------------------------------------------------------------------
-- The states.
-- ---------------------------------------------------------------------------

-- State 0: one-time setup.
local function init(e)
    e.pos_y = -210
    e.hit_state = 0
    e:reset_joints()

    e:set_shadow_offset(0, 0, 0)
    e.shadow_tint = 0x808080
    e.shadow_half_x = 1000
    e.shadow_half_z = 1000

    e.health = 0xDC
    if e.id == 0x10 then
        e.health = 600
    end
    e:set_sca(SCA_RADIUS, SCA_HALF_HEIGHT, 0, SCA_OFFSET_Y, 0)

    -- The exposed heart rides joint 1 for the whole encounter.
    e:tyrant_heart_spawn()

    e.ty_pad_counter = 0
    e.ty_hit_mask = 0
    e.ty_look_mode = 3
    e.ty_flags = 0
    e.ty_repause = 0
    e.ty_rocket_flag = 0
    e.ty_cooldown = 0xD2
    e.attacking_direction = 0
    e.behavior_step = 0
    e.ty_crowd = 0
    e.ty_close = 0

    e.state_word = 0x000B0008
    e.animation_frame_id = 0
    e.unk_bf = 0
    e.blend_counter = 0
    e.animation_id = 6
    if e.id == 0x10 then
        e.state_word = 0x00090101
    end
    e:advance_anim(0x40)

    e.look_at_joint = 2
    e.look_at_flags = 2
    e.look_at_yaw_step = 0xA0
    e.look_at_pitch_step = 0x60

    if e.id == 0x0C then
        return
    end

    -- id 0x10 only: the claw ghosts and the ribbon pool.
    e:tyrant_trail_alloc()
    e.ty_repause = 0x5A
end

-- State 1: think, then act.
local function think(e)
    if e.player_health < 0 then
        e.state_word = 0x00000101
        return
    end
    if e.health < 0x3C then
        e.ty_hit_mask = e.ty_hit_mask | 0x80
    end
    local d = distance(e)
    if (d & 0xFFFFFFFF) < 4000 then
        e.ty_crowd = as_s8(e.ty_crowd + 1)
        if (e.ty_crowd & 0xFF) > 0x78 then
            e.ty_close = 0x3C
            e.ty_hit_mask = e.ty_hit_mask | 0x80
        end
    else
        e.ty_crowd = 0
    end
    if e.id == 0x10 then
        run_behavior(e, 15)
    else
        run_behavior(e, 14)
    end
end

local function act(e)
    local px, _, pz = e:player_pos()
    e:pathfind_update(px, pz)
    e:zone_path_update(px, pz)
    run_behavior(e, e.action_behavior + 2)
end

local function state1(e)
    e.status_flags = (e.status_flags & 0x1F) | 0x40
    e:check_visual_range(4000)
    e:check_alert_range(4000)
    if e.ignore == 0 then
        think(e)
    elseif e.ignore ~= 1 then
        return
    end
    act(e)
end

-- State 2: the hit reaction restores the backed-up state word.
local function state_hit(e)
    if e.ignore ~= 0 then
        return
    end
    e.state_word = e.ty_state_bk
    if e.ty_look_mode == 0 then
        e.ty_look_mode = 8
    end
    if e.ty_cooldown ~= 0 then
        e.ty_cooldown = 0xD2
    end
    local px, _, pz = e:player_pos()
    local yaw = e:angle_to(px, pz)
    e:spawn_effect(0, 0, 0, -0x834, 0, as_s16(yaw), 0)
end

-- State 3: act with no pathfinding and no behaviour bias.
local function state_forced(e)
    if e.ignore == 0 then
        e.state_word = 0x00000103
        if e.id == 0x10 then
            e.action_behavior = 1
        end
    end
    run_behavior(e, e.action_behavior)
end

-- State 8: the SCD-driven sequence.
local function state_scd(e)
    if (e.behavior_flags & 0x40) ~= 0 then
        local handler = SCD[e.action_behavior]
        if handler then
            handler(e)
        else
            e:count_placeholder(e.action_behavior)
        end
        return
    end
    e.state_word = 0x00010001
    e.ty_flags = e.ty_flags | 2
end

-- ---------------------------------------------------------------------------
-- The per-frame entry.
-- ---------------------------------------------------------------------------

function update(e)
    -- Any live Tyrant whose spawn record carries 0x40 hands over to the SCD.
    if e.state ~= 0 and (e.behavior_flags & 0x40) ~= 0 then
        e.state = 8
    end

    -- behavior_flags 0x80 suspends everything except the init pass.
    if e.state ~= 0 and (e.behavior_flags & 0x80) ~= 0 then
        return
    end

    if not e.monster_paused then
        local state = e.state
        if state == 0 then
            init(e)
        elseif state == 1 then
            state1(e)
        elseif state == 2 then
            state_hit(e)
        elseif state == 3 then
            state_forced(e)
        elseif state == 8 then
            state_scd(e)
        end

        -- The impale holds the grabbed player: no separation and no room
        -- collision while it runs.
        if e.action_behavior ~= 9 then
            collision_tail(e)
        end

        -- The pad-rumble phase counter still advances (the rumble itself is
        -- compiled out of the original).
        local pad = (e.ty_pad_counter + 1) & 0xFF
        if pad > 0x1D then
            pad = 0
        end
        e.ty_pad_counter = pad

        if (e.ty_flags & 2) ~= 0 then
            local mode = e.ty_look_mode
            if mode == 0 then
                e.look_at_flags = 0x13
                -- The target is the player's joint 1 world; the port tracks
                -- the player position (documented approximation).
                local px, py, pz = e:player_pos()
                e.target_x = px
                e.target_y = py
                e.target_z = pz
            elseif mode < 4 then
                e.ty_look_mode = mode - 1
                e.look_at_flags = 0
                e.hit_state = 0
            else
                e.ty_look_mode = mode - 1
                e.look_at_flags = 0x33
                e.target_x = 400
                e.target_y = 0xE0C
            end
        end

        if e.health >= 0 then
            e:update_look_at()
        end

        e.ty_state_bk = e.state_word
    end

    -- Outside the gate: the heart, the switch zone, the ribbon and the claw
    -- ghosts, the shadow, the id-16 kill pin and the room-action probe.
    if (e.ty_flags & 8) == 0 then
        e:tyrant_heart_tick()
    end

    e:update_switch_zone()

    if e.id ~= 0x0C and (e.ty_flags & 8) == 0 then
        e:tyrant_ghosts_tick()
        if (e.ty_trail_timer & 0x8000) ~= 0 then
            e:tyrant_trail_arm()
        end
        if e.ty_trail_timer ~= 0 then
            e:tyrant_trail_update()
        end
    end

    if (e.ty_flags & 1) ~= 0 and e.has_enter_switch_zone ~= 0 then
        e.shadow_suppressed = false
    else
        e.shadow_suppressed = true
    end

    if (e.ty_flags & 8) == 0 and e.id == 0x10 and e.health < 0xC9 then
        e:raise_flag(4, 0x1F)
        e.health = 200
    end

    e:probe_room_actions(2)
end

return { update = update }
