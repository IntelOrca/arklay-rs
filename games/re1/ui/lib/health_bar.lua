-- The status panel's animated health monitor: the condition face and the EKG.
--
-- The monitor derives a state from the player's health and maximum health,
-- `(health - 1) / (max_health >> 2)` clamped to `0..=3`, which selects the
-- EKG colour red/orange/yellow/green. The poison flags (`status & 0x22`)
-- swap the face and the EKG wave group without changing the colour. The bar
-- runs the original's four-state animation (init, sweep, flush-in, flush)
-- over the transcribed wave tables, and `start_flush` is the heal flush: blue
-- when only the status was cured, green when only health was restored, white
-- for both.
--
-- The sweep queues one line per wave record (`FUN_00470c60`), the trailing
-- line steps its colour down per record (`FUN_00438800`), and the flush draws
-- its fading rows - all transcribed from `menu_draw_health_bar`.
--
-- The wave variant (and, when poisoned, the poison-pick bit) comes from the
-- game's shared `rand()` stream (`api:rand()`), so the scripts' random draws
-- stay in step with the reference.

local Bar = {}

-- The face tile.
local FACE_X, FACE_Y, FACE_W, FACE_H = 100, 0xA8, 32, 8
-- The EKG window: its left and right edges and the baseline.
local EKG_MIN_X, EKG_MAX_X, EKG_BASE_Y = 0x54, 0x83, 0xA2
-- The heal flush stops drawing at this y.
local FLUSH_BOTTOM_Y = 0xB0

-- EKG colour per health state (red, orange, yellow, green).
local HEALTH_COLORS = {
    [0] = { 0xC0, 0x00, 0x00 },
    { 0xFF, 0x7F, 0x00 },
    { 0xD8, 0xD8, 0x00 },
    { 0x00, 0xFF, 0x00 },
}

-- The face span and flash tables (indexed by `sweep_index`, from 1).
local FACE_WARN_SPAN = { 0x0C, 0x11, 0x14, 0x3C, 0x3F, 0x44, 0x50, 0x00 }
local FACE_DANGER_SPAN = {
    0x03, 0x07, 0x21, 0x25, 0x2B, 0x2F, 0x49, 0x4D, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
}
local FACE_CAUTION_FLASH = { 0x00, 0x03, 0x02, 0x01, 0x02, 0x03, 0x00, 0x00 }
local FACE_DANGER_FLASH = {
    0x00, 0x02, 0x01, 0x02, 0x00, 0x02, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
}

-- The 20 EKG wave tables, four per health state (`0..=3`) then the four
-- poisoned variants (`16..=19`), exactly as the original stores them: 4-byte
-- `{x, prev_x, y, slope}` records, each segment ending at `x` with height `y`
-- and rising by `slope` for every pixel to the left of `x`.
local EKG_WAVES = {
    [0] = { 28, 47, 0, 0, 27, 28, -1, 1, 26, 27, -3, 2, 25, 26, 0, -3, 24, 25, 2, -2, 23, 24, 1, 1, 22, 23, -1, 2, 21, 22, -4, 3, 20, 21, 0, -4, 19, 20, 2, -2, 18, 19, 0, 2, 0, 18, 0, 0 },
    { 28, 47, 0, 0, 27, 28, -1, 1, 26, 27, -3, 2, 24, 26, 1, -2, 23, 24, 0, 1, 21, 23, -4, 2, 20, 21, -1, -3, 19, 20, 1, 2, 18, 19, 0, 1, 0, 18, 0, 0 },
    { 28, 47, 0, 0, 27, 28, -2, 2, 25, 27, 2, -2, 24, 25, 1, 1, 23, 24, -1, 2, 22, 23, -4, 3, 20, 22, 2, -3, 19, 20, 0, 2, 0, 19, 0, 0 },
    { 28, 47, 0, 0, 27, 28, -1, 1, 26, 27, -3, 2, 24, 26, 1, -2, 23, 24, -1, 2, 22, 23, -4, 3, 21, 22, -1, -3, 20, 21, 1, -2, 19, 20, 0, 1, 0, 19, 0, 0 },
    { 29, 47, 0, 0, 28, 29, -1, 1, 27, 28, -3, 2, 26, 27, 0, -3, 25, 26, 2, -2, 24, 25, 1, 1, 23, 24, -1, 2, 21, 23, -7, 3, 20, 21, -3, -4, 19, 20, 0, -3, 18, 19, 2, -2, 17, 18, 0, 2, 0, 17, 0, 0 },
    { 28, 47, 0, 0, 27, 28, -1, 1, 26, 27, -3, 2, 24, 26, 1, -2, 23, 24, -1, 2, 21, 23, -7, 3, 19, 21, 1, -4, 18, 19, 0, 1, 0, 18, 0, 0 },
    { 29, 47, 0, 0, 27, 29, -4, 2, 25, 27, 2, -3, 24, 25, 1, 1, 23, 24, -1, 2, 21, 23, -7, 3, 18, 21, 2, -3, 17, 18, 0, 2, 0, 17, 0, 0 },
    { 29, 47, 0, 0, 28, 29, -1, 1, 27, 28, -3, 2, 25, 27, 1, -2, 24, 25, -1, 2, 22, 24, -7, 3, 20, 22, -1, -3, 19, 20, 1, -2, 18, 19, 0, 1, 0, 18, 0, 0 },
    { 31, 47, 0, 0, 29, 31, -4, 2, 28, 29, -1, -3, 26, 28, 3, -2, 25, 26, 2, 1, 21, 25, -10, 3, 20, 21, -5, -5, 18, 20, 3, -3, 17, 18, 1, 2, 16, 17, 0, 1, 0, 16, 0, 0 },
    { 31, 47, 0, 0, 30, 31, -2, 2, 29, 30, -5, 3, 28, 29, -2, -3, 26, 28, 2, -2, 23, 26, -4, 2, 21, 23, -10, 3, 17, 21, 2, -3, 16, 17, 0, 2, 0, 16, 0, 0 },
    { 31, 47, 0, 0, 30, 31, -1, 1, 29, 30, -2, 2, 26, 29, 3, -2, 25, 26, 2, 1, 24, 25, 0, 2, 21, 24, -9, 3, 18, 21, 3, -4, 17, 18, 1, 2, 16, 17, 0, 1, 0, 16, 0, 0 },
    { 31, 47, 0, 0, 29, 31, -2, 2, 26, 29, 2, -2, 25, 26, 1, 1, 24, 25, -1, 2, 21, 24, -10, 3, 17, 21, 2, -3, 16, 17, 0, 2, 0, 16, 0, 0 },
    { 32, 47, 0, 0, 29, 32, -6, 2, 26, 29, 3, -3, 25, 26, 2, 1, 21, 25, -14, 4, 19, 21, -4, -5, 18, 19, 0, -4, 17, 18, 3, -3, 16, 17, 1, 2, 15, 16, 0, 1, 0, 15, 0, 0 },
    { 32, 47, 0, 0, 30, 32, -6, 3, 29, 30, -4, -2, 26, 29, 2, -3, 24, 26, -2, 2, 21, 24, -14, 4, 17, 21, 2, -4, 16, 17, 0, 2, 0, 16, 0, 0 },
    { 31, 47, 0, 0, 29, 31, -4, 2, 27, 29, 0, -2, 26, 27, 3, -3, 25, 26, 2, 1, 24, 25, -1, 3, 21, 24, -13, 4, 19, 21, -3, -5, 17, 19, 3, -3, 16, 17, 1, 2, 15, 16, 0, 1, 0, 15, 0, 0 },
    { 32, 47, 0, 0, 30, 32, -4, 2, 27, 30, 2, -2, 26, 27, 1, 1, 21, 26, -14, 3, 17, 21, 2, -4, 16, 17, 0, 2, 0, 16, 0, 0 },
    { 38, 47, 0, 0, 36, 38, -6, 3, 34, 36, 4, -5, 33, 34, 0, 4, 30, 33, -15, 5, 27, 30, 3, -6, 26, 27, 1, 2, 25, 26, 0, 1, 22, 25, 0, 0, 20, 22, -4, 2, 18, 20, 4, -4, 14, 18, -12, 4, 11, 14, 3, -5, 10, 11, 1, 2, 9, 10, 0, 1, 0, 9, 0, 0 },
    { 38, 47, 0, 0, 36, 38, -4, 2, 34, 36, 4, -4, 30, 34, -12, 4, 27, 30, 3, -5, 26, 27, 1, 2, 25, 26, 0, 1, 22, 25, 0, 0, 20, 22, -6, 3, 18, 20, 4, -5, 17, 18, 0, 4, 14, 17, -15, 5, 11, 14, 3, -6, 10, 11, 1, 2, 9, 10, 0, 1, 0, 9, 0, 0 },
    { 37, 47, 0, 0, 36, 37, -1, 1, 34, 36, -7, 3, 31, 34, 5, -4, 27, 31, -15, 5, 24, 27, 3, -6, 23, 24, 0, 3, 21, 23, 0, 0, 20, 21, -2, 2, 19, 20, -5, 3, 17, 19, 3, -4, 14, 17, -12, 5, 11, 14, 3, -5, 10, 11, 0, 3, 0, 10, 0, 0 },
    { 37, 47, 0, 0, 36, 37, -2, 2, 35, 36, -5, 3, 33, 35, 3, -4, 30, 33, -12, 5, 27, 30, 3, -5, 26, 27, 0, 3, 24, 26, 0, 0, 23, 24, -1, 1, 21, 23, -7, 3, 18, 21, 5, -4, 14, 18, -15, 5, 11, 14, 3, -6, 10, 11, 0, 3, 0, 10, 0, 0 },
}
local WAVE_COUNT = 20

local function clamp(value, low, high)
    if value < low then
        return low
    elseif value > high then
        return high
    end
    return value
end

--- The health state `(health - 1) / (max_health >> 2)` clamped to `0..=3`.
--- A non-positive health is state 0; a maximum below four degenerates to the
--- full-health state instead of dividing by zero.
function Bar.health_state(health, max_health)
    if health <= 0 or max_health <= 0 then
        return 0
    end
    local step = max_health >> 2
    if step <= 0 then
        return 3
    end
    -- Both operands are positive, so floor division truncates like C.
    return clamp((health - 1) // step, 0, 3)
end

--- A monitor in the init state.
function Bar.new()
    return {
        -- Animation state: 0 init, 1 sweep, 2 flush-in, 3 flush.
        state = 0,
        health_state = 3,
        poisoned = false,
        color = HEALTH_COLORS[3],
        wave = 0,
        secondary_wave = 0,
        -- The heal flush colour while one runs, else nil.
        flush = nil,
        head = 0x84,
        secondary_head = 0x84,
        secondary_color = HEALTH_COLORS[3],
        -- `DAT_00ae9f48`: the low two bits are the active wave variant and
        -- bits 2-3 the wave group (health state, or 4 poisoned). The menu
        -- open resets it to `0xFF`, so the first sweep's variant draw folds
        -- away a 3 rather than a 0.
        wave_variant = 0xFF,
    }
end

-- One step of the primary/secondary sweep (the original's case 1).
local function sweep_step(bar, api, health, max_health)
    bar.head = bar.head + 1
    if bar.head > EKG_MAX_X then
        bar.head = 0x35
        -- The health state and colour are recomputed only at the wrap.
        bar.health_state = Bar.health_state(health, max_health)
        bar.color = HEALTH_COLORS[bar.health_state]
        -- Poisoned health picks the poisoned wave group (4) on a random bit;
        -- the variant draw follows with the original's no-repeat fold.
        local group = bar.health_state
        if bar.poisoned and api:rand() & 1 ~= 0 then
            group = 4
        end
        local variant = api:rand() & 3
        if bar.wave_variant & 3 == variant then
            variant = (variant + 1) & 3
        end
        bar.wave_variant = variant | (group << 2)
        bar.wave = group * 4 + variant
        if bar.wave >= WAVE_COUNT then
            bar.wave = 0
        end
    end
    bar.secondary_head = bar.secondary_head + 1
    if bar.secondary_head > EKG_MAX_X then
        bar.secondary_head = bar.head - 0x1F
        bar.secondary_wave = bar.wave
        bar.secondary_color = bar.color
    end
end

--- Advance the monitor one frame for the player's current health.
function Bar.update(bar, api)
    local health, max_health, status = api:player_health()
    bar.poisoned = status & 0x22 ~= 0
    if bar.state == 0 then
        bar.state = 1
        bar.head = 0x84
        bar.secondary_head = 0x84
        -- The original's case 0 falls through into case 1, so the first
        -- frame already steps the head to 0x85 and wraps it to 0x35.
        sweep_step(bar, api, health, max_health)
    elseif bar.state == 1 then
        sweep_step(bar, api, health, max_health)
    elseif bar.state == 2 then
        bar.state = 3
        bar.head = 0xAF
    elseif bar.state == 3 then
        bar.head = bar.head - 2
        if bar.head <= EKG_MAX_X then
            bar.state = 0
            bar.flush = nil
        end
    else
        bar.state = 0
    end
end

--- Request the heal flush: the state jumps to the vertical sweep that fades
--- the line to blue (cure), green (heal) or white (both).
function Bar.start_flush(bar, healed, cured)
    if healed and cured then
        bar.flush = { 0xFF, 0xFF, 0xFF }
    elseif healed then
        bar.flush = { 0, 0xFF, 0 }
    elseif cured then
        bar.flush = { 0, 0, 0xFF }
    else
        return
    end
    bar.state = 2
end

--- The primary sweep's left edge (clamped) and right edge.
function Bar.primary_span(bar)
    return clamp(bar.head + 0x0F, EKG_MIN_X, EKG_MAX_X), clamp(bar.head, EKG_MIN_X, EKG_MAX_X)
end

--- The trailing sweep's span.
function Bar.secondary_span(bar)
    return clamp(bar.secondary_head + 0x1F, EKG_MIN_X, EKG_MAX_X),
        clamp(bar.secondary_head, EKG_MIN_X, EKG_MAX_X)
end

-- The original's `FUN_004387e0`: count the span-table entries at or below
-- the sweep head's low byte minus `0x35`. The subtraction lands in a byte, so
-- the last four heads of a sweep (`0x80..=0x83`) wrap to `0x4B..0x4E` and keep
-- matching the table's tail instead of dropping to zero entries.
local function sweep_index(span, head)
    local threshold = (head - 0x35) & 0xFF
    local count = 0
    for _, byte in ipairs(span) do
        if byte > threshold then
            break
        end
        count = count + 1
    end
    return count
end

local function draw_face_tile(api, ox, oy, v)
    api:page_sprite_mirrored(api:page("ui/status"), 0x60, v, FACE_W, FACE_H, ox + FACE_X,
        oy + FACE_Y, FACE_W, FACE_H, 0, 0, false, false)
end

-- The face tile the health state and the sweep position select.
local function draw_face(api, bar, ox, oy)
    local row = bar.poisoned and 4 or bar.health_state
    local span, base, flash
    if row == 0 then
        span, base, flash = FACE_DANGER_SPAN, 0x30, FACE_DANGER_FLASH
    elseif row == 1 then
        span, base, flash = FACE_WARN_SPAN, 0x18, FACE_CAUTION_FLASH
    elseif row == 2 or row == 3 then
        draw_face_tile(api, ox, oy, 0)
        return
    else
        span, base, flash = FACE_DANGER_SPAN, 0x40, FACE_DANGER_FLASH
    end
    local select = flash[sweep_index(span, bar.head) + 1]
    if select == nil or select == 0 then
        return
    end
    draw_face_tile(api, ox, oy, select * 8 + base)
end

-- The original hands its EKG lines to the line primitive as colour bytes,
-- which the reference converts through `byte / 256 * 255`, so every channel
-- draws one step down: the 0xD8 yellow lands on 0xD7 and 0xFF on 0xFE.
local function line_channel(value)
    return (value * 255) // 256
end

-- The heal flush: the original's state-3 sweep draws up to 15 one-pixel
-- lines downward from the flush base, their tint starting at `0xFF` (or the
-- pre-faded value once the base has left the window) and dropping by `0x10`
-- each line, stopping at the window's bottom edge.
local function draw_flush(api, bar, ox, oy)
    local base = bar.flush
    if base == nil then
        return
    end
    local y, count, tint
    if bar.head < 0x92 then
        local offset = (0x92 - bar.head) & 0xFF
        y = 0x92
        count = 0x0F - offset
        if count < 0 then
            count = 0
        end
        tint = (0xFF - offset * 0x10) & 0xFF
    else
        y, count, tint = bar.head, 0x0F, 0xFF
    end
    for _ = 1, count do
        if y >= FLUSH_BOTTOM_Y then
            break
        end
        local channel = line_channel(tint)
        local r = base[1] ~= 0 and channel or 0
        local g = base[2] ~= 0 and channel or 0
        local b = base[3] ~= 0 and channel or 0
        api:line(ox + EKG_MIN_X, oy + y, ox + EKG_MAX_X, oy + y, r, g, b)
        if tint ~= 0 then
            tint = (tint - 0x10) & 0xFF
        end
        y = y + 1
    end
end

-- One queued EKG line. A zero-length segment draws nothing: the original's
-- GPU primitive builds a quad from the segment's perpendicular offsets, and a
-- point has no area.
local function queue_line(api, ox, oy, x0, y0, x1, y1, r, g, b)
    if x0 == x1 and y0 == y1 then
        return
    end
    api:line(ox + x0, oy + y0, ox + x1, oy + y1, r, g, b)
end

-- The primary sweep. The original walks the wave's segment records from the
-- head (its right edge, `DAT_00ae9f36`) back to the tail (`DAT_00ae9f38`,
-- clamped into the window) and queues one line per record: a partial up to the
-- first record's right edge, one line per whole record, then a partial from
-- the last record's left edge to the tail. Only the partials evaluate the
-- record's slope; a whole-record line joins the two records' own heights.
local function draw_primary(api, ox, oy, bar)
    local wave = EKG_WAVES[bar.wave]
    if wave == nil then
        return
    end
    local r = line_channel(bar.color[1])
    local g = line_channel(bar.color[2])
    local b = line_channel(bar.color[3])
    local x0 = clamp(bar.head + 0x0F, EKG_MIN_X, EKG_MAX_X)
    local tail = clamp(bar.head, EKG_MIN_X, EKG_MAX_X)
    -- The record whose right edge is at or left of the head side, else the
    -- last (bottom) record.
    local at = 1
    while x0 < wave[at] + EKG_MIN_X do
        at = at + 4
    end
    local x1 = wave[at] + EKG_MIN_X
    local y1 = wave[at + 2] + EKG_BASE_Y
    local y0 = (x0 - x1) * wave[at + 3] + y1
    if tail < x1 then
        queue_line(api, ox, oy, x0, y0, x1, y1, r, g, b)
        if wave[at] == 0 then
            -- The bottom record: the original skips its tail draw.
            return
        end
        at = at + 4
        while tail < wave[at] + EKG_MIN_X do
            queue_line(api, ox, oy, wave[at + 1] + EKG_MIN_X, wave[at - 2] + EKG_BASE_Y,
                wave[at] + EKG_MIN_X, wave[at + 2] + EKG_BASE_Y, r, g, b)
            at = at + 4
        end
        x0, y0 = wave[at + 1] + EKG_MIN_X, wave[at - 2] + EKG_BASE_Y
        y1 = (tail - (wave[at] + EKG_MIN_X)) * wave[at + 3] + wave[at + 2] + EKG_BASE_Y
    end
    queue_line(api, ox, oy, x0, y0, tail, y1, r, g, b)
end

-- The trailing sweep. Each segment is queued in the chain's near colour and
-- the chain then steps down by `colour >> 5` per pixel of the segment's width,
-- so the trail brightens toward its right edge in flat per-record steps. The
-- chain starts at the colour latched when the trail wrapped
-- (`DAT_00ae9f33..35`), pre-faded by any overshoot past the window's right
-- edge.
local function draw_secondary(api, ox, oy, bar)
    local wave = EKG_WAVES[bar.secondary_wave]
    if wave == nil then
        return
    end
    local base = bar.secondary_color
    if base[1] == 0 and base[2] == 0 and base[3] == 0 then
        base = bar.color
    end
    local step_r, step_g, step_b = base[1] >> 5, base[2] >> 5, base[3] >> 5
    local near_r, near_g, near_b = base[1], base[2], base[3]
    local right = bar.secondary_head + 0x1F
    local x0
    if right < 0x84 then
        x0 = right
    else
        local fade = EKG_MAX_X - right
        near_r = (near_r + step_r * fade) & 0xFF
        near_g = (near_g + step_g * fade) & 0xFF
        near_b = (near_b + step_b * fade) & 0xFF
        x0 = EKG_MAX_X
    end
    local tail = clamp(bar.secondary_head, EKG_MIN_X, EKG_MAX_X)
    -- Queue one segment in the near colour, then slide the chain to the
    -- segment's far end (the original's `FUN_00438800`).
    local function queue(xa, ya, xb, yb)
        queue_line(api, ox, oy, xa, ya, xb, yb, line_channel(near_r), line_channel(near_g),
            line_channel(near_b))
        local delta = xa - xb
        if delta ~= 0 then
            near_r = (near_r - delta * step_r) & 0xFF
            near_g = (near_g - delta * step_g) & 0xFF
            near_b = (near_b - delta * step_b) & 0xFF
        end
    end
    local at = 1
    while x0 < wave[at] + EKG_MIN_X do
        at = at + 4
    end
    local x1 = wave[at] + EKG_MIN_X
    local y1 = wave[at + 2] + EKG_BASE_Y
    local y0 = (x0 - x1) * wave[at + 3] + y1
    if x1 <= tail then
        queue(x0, y0, tail, y1)
        return
    end
    queue(x0, y0, x1, y1)
    if wave[at] == 0 then
        return
    end
    at = at + 4
    while tail < wave[at] + EKG_MIN_X do
        queue(wave[at + 1] + EKG_MIN_X, wave[at - 2] + EKG_BASE_Y, wave[at] + EKG_MIN_X,
            wave[at + 2] + EKG_BASE_Y)
        at = at + 4
    end
    queue(wave[at + 1] + EKG_MIN_X, wave[at - 2] + EKG_BASE_Y, tail,
        (tail - (wave[at] + EKG_MIN_X)) * wave[at + 3] + wave[at + 2] + EKG_BASE_Y)
end

--- Draw the health face and the EKG.
function Bar.draw(api, bar, ox, oy)
    -- The original's face draw sits in the sweep states only: a flush frame
    -- replaces the whole monitor (face included) with the fading lines, and a
    -- monitor that has not ticked yet (state 0) has nothing to show.
    if bar.state == 2 or bar.state == 3 then
        draw_flush(api, bar, ox, oy)
        return
    end
    if bar.state ~= 1 then
        return
    end
    draw_face(api, bar, ox, oy)
    if bar.head + 0x0F > 0x53 and bar.head < 0x84 then
        draw_primary(api, ox, oy, bar)
    end
    if bar.secondary_head + 0x1F > 0x53 and bar.secondary_head < 0x84 then
        draw_secondary(api, ox, oy, bar)
    end
end

return Bar
