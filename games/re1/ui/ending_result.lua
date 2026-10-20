-- The RESULT screen after the ending films.
--
-- A port of the reference's ending_result_* (EndingScreen.cpp:327-681): it
-- reveals five baked plates from the en05/en07 page in pairs, fades each
-- slot, prints the clear time and the save count, waits at the step-4 input
-- gate, and - for ending 6 - shows the additive fifth plate before the
-- confetti bonus. The row's `background` flag selects the plate page and the
-- character backdrop, and the already-cleared-once bit (0x7B) swaps in the
-- alternate stills. The original draws no letter rank or item summary.
--
-- args: ending_id, character, time_ticks, clear_text, save_text, no_save,
-- background, second_playthrough.

local E = require("lib/ending")

local M = {}

-- The step-4 wait before the timeout advances.
local INPUT_TIMEOUT = 0x708
-- Frames the confetti burst runs before it is retired (0x1E + 0x32).
local CONFETTI_FRAMES = 0x1E + 0x32
-- One clear-time hour in ticks, for the over-two-digit re-centring.
local TICKS_PER_HOUR_CENTRE = 0x34BC0

local CONFETTI_COLORS = {
    { 255, 64, 64 },
    { 255, 220, 64 },
    { 64, 220, 96 },
    { 64, 160, 255 },
    { 255, 128, 220 },
}

local plate
local backdrop
local s

--- The RESULT backdrop: the "escaped" still, swapped for the alternate one
--- once the game was cleared before (EndingScreen.cpp:875-898).
local function backdrop_stem(character, second_playthrough)
    local chris = character & 1 == 0
    if chris then
        return second_playthrough and "ui/clis01" or "ui/chris02"
    end
    return second_playthrough and "ui/jill01" or "ui/jill02"
end

--- One step of the deterministic LCG the port uses for the confetti.
local function lcg(seed)
    return (seed * 1664525 + 1013904223) & 0xFFFFFFFF
end

--- Spawn the no-save run's confetti burst.
local function spawn_confetti(count)
    local particles = {}
    local seed = 0x12345678
    for index = 0, count - 1 do
        seed = lcg(seed)
        local x = (seed >> 8) % 320
        seed = lcg(seed)
        local vx = (seed >> 12) % 5 - 2
        seed = lcg(seed)
        local vy = -((seed >> 10) % 6) - 4
        particles[#particles + 1] = {
            x = x, y = 96, vx = vx, vy = vy,
            color = CONFETTI_COLORS[index % #CONFETTI_COLORS + 1],
        }
    end
    s.confetti = particles
end

local function step_confetti()
    for _, particle in ipairs(s.confetti) do
        particle.x = particle.x + particle.vx
        particle.y = particle.y + particle.vy
        particle.vy = particle.vy + 1
    end
end

--- Install the five RESULT slots from the credit table
--- (ending_result_build, EndingScreen.cpp:440-518).
local function build()
    local base = E.credit_base(s.background_row)
    local jill = s.character & 1 == 1
    for index = 0, 4 do
        local entry = E.CREDIT_ENTRIES[base + index]
        local dx = 0
        if jill and s.background_row then
            dx = E.JILL_LAYOUT_SHIFT
        end
        if entry.kind == "clear_time" then
            -- Re-centre the clear time once it needs more than two digits of
            -- hours (EndingScreen.cpp:472-480); unreachable in practice.
            local hours = s.time_ticks // TICKS_PER_HOUR_CENTRE
            while hours // 100 ~= 0 do
                dx = dx - 0x70
                hours = hours // 10
            end
        end
        s.slots[index + 1] = E.slot(entry, dx)
    end
end

--- Activate slot `index` (1-based) with a rising or falling fade velocity.
local function set_fade(index, velocity)
    s.slots[index].active = true
    s.slots[index].fade_vel = velocity
end

function M.open(api, args)
    s = {
        ending_id = args.ending_id or 1,
        character = args.character or 0,
        time_ticks = args.time_ticks or 0,
        clear_text = args.clear_text or "",
        save_text = args.save_text or "",
        no_save = args.no_save or false,
        background_row = args.background or false,
        -- The type-1 screen-colour slot: its fade drives the backdrop.
        background = E.slot({ kind = "sprite", x = 0, y = 0, tex_v = 0, height = 0 }),
        slots = {},
        step = 0,
        timer = 0,
        effects = false,
        done = false,
        confetti = {},
        confetti_timer = 0,
    }
    s.background.active = true
    local empty = { kind = "sprite", x = 0, y = 0, tex_v = 0, height = 0 }
    for index = 1, 5 do
        s.slots[index] = E.slot(empty)
    end
    M.state = s

    local err
    plate, err = api:page(s.background_row and "ui/en07" or "ui/en05")
    if not plate then
        api:log("warning: no result plate: " .. err)
    end
    if s.background_row then
        backdrop, err = api:picture(backdrop_stem(s.character, args.second_playthrough))
        if not backdrop then
            api:log("warning: no result backdrop: " .. err)
        end
    end
end

local function step(api, input)
    local st = s.step
    if st == 0 then
        s.timer = 0
        s.background.fade_level = 0
        s.background.fade_vel = E.FADE_STEP
        build()
        s.step = 1
    elseif st == 1 then
        s.timer = s.timer + 1
        if s.timer > 0x3C then
            s.timer = 0
            set_fade(1, E.FADE_STEP)
            set_fade(2, E.FADE_STEP)
            s.step = 2
        end
    elseif st == 2 then
        s.timer = s.timer + 1
        if s.timer > 0x3C then
            s.timer = 0
            set_fade(3, E.FADE_STEP)
            set_fade(4, E.FADE_STEP)
            s.step = 3
        end
    elseif st == 3 then
        s.timer = s.timer + 1
        if s.timer > 0x5A then
            s.timer = 0
            s.step = 4
        end
    elseif st == 4 then
        -- The single input gate: any button, or the timeout.
        s.timer = s.timer + 1
        if s.timer > INPUT_TIMEOUT or input.any or input.confirm or input.cancel then
            s.timer = 0
            s.step = s.ending_id == 6 and 6 or 5
        end
    elseif st == 5 then
        s.step = 10
        s.timer = 0
        s.background.fade_vel = -E.FADE_STEP
        for index = 1, 4 do
            s.slots[index].fade_vel = -E.FADE_STEP
        end
    elseif st == 6 then
        -- Ending 6 keeps the backdrop up for the fifth plate.
        s.step = 7
        s.timer = 0
        for index = 1, 4 do
            s.slots[index].fade_vel = -E.FADE_STEP
        end
    elseif st == 7 then
        s.timer = s.timer + 1
        if s.timer > 0x5A then
            local fifth = s.slots[5]
            fifth.active = true
            fifth.fade_vel = E.FADE_STEP
            fifth.additive = true
            s.timer = 0
            s.step = 8
        end
    elseif st == 8 then
        s.timer = s.timer + 1
        if s.slots[5].fade_level >= E.FADE_FULL then
            s.slots[5].additive = false
        end
        if s.timer > 0x96 then
            s.timer = 0
            s.step = 9
            s.background.fade_vel = -E.FADE_STEP
        end
    elseif st == 9 then
        s.timer = s.timer + 1
        if s.timer > 0x96 then
            s.timer = 0
            s.step = 10
            s.slots[5].fade_vel = -E.FADE_STEP
            s.slots[5].additive = true
        end
    elseif st == 10 then
        s.timer = s.timer + 1
        if s.timer > 0x1E then
            -- The no-save bonus is a run whose counter still reads its
            -- initial one (EndingScreen.cpp:426-434).
            s.effects = s.no_save
            s.background.active = false
            for _, slot in ipairs(s.slots) do
                slot.active = false
            end
            if s.effects then
                -- The reference's Effect_CreateBillboard(0x0B, 0x0A); the
                -- port approximates it with a deterministic burst.
                spawn_confetti(48)
                api:sound("decide")
            end
            s.confetti_timer = 0
            s.step = 11
            s.timer = 0
        end
    elseif st == 11 then
        s.timer = s.timer + 1
        if s.timer > 0x32 then
            s.step = 12
        end
    elseif st == 12 then
        s.done = true
        s.timer = 0
    end
end

function M.update(api, input)
    -- Step every slot's fade first, exactly once per fixed tick.
    s.background:step_fade()
    for _, slot in ipairs(s.slots) do
        slot:step_fade()
    end
    if s.effects and s.confetti_timer < CONFETTI_FRAMES then
        step_confetti()
        s.confetti_timer = s.confetti_timer + 1
    end
    step(api, input)
    if s.done then
        api:exit("ending_epilogue")
    end
end

function M.draw(api)
    E.black(api)
    if backdrop then
        -- A full-frame still is stretched, so a replacement may be any size.
        local shade = E.shade_scale(s.background:shade())
        api:draw_background(backdrop, 0, 0, shade, shade, shade)
    end
    for _, slot in ipairs(s.slots) do
        local text
        if slot.kind == "clear_time" then
            text = s.clear_text
        elseif slot.kind == "save_count" then
            text = s.save_text
        end
        slot:draw(api, plate, text)
    end
    if s.effects and s.confetti_timer < CONFETTI_FRAMES then
        local ox, oy = E.origin(api)
        for _, particle in ipairs(s.confetti) do
            local c = particle.color
            api:fill_rect(particle.x + ox, particle.y + oy, 2, 2, c[1], c[2], c[3], 255)
        end
    end
end

return M
