-- The game-over ("YOU DIED") screen.
--
-- The engine's death sequence (the 90-frame fall and the white fade) opens
-- this screen over the corpse. It is the original's display state machine,
-- stepped once per 30 Hz tick:
--
-- - 1 waits for the entry white flash (counter 0xFE80 from 0x7FFF) to clear;
-- - 2 ramps the black overlay 3 per tick and, once it passes 0x3F, draws and
--   steps the wave amplitude down from 59;
-- - 3 holds the settled strip for 49 ticks;
-- - 4 arms the black fade-out (counter 0x180);
-- - 5 re-waves the strip while the fade runs, then finishes.
--
-- Confirm skips the rest at any state. The corpse spins 8 angle units a tick
-- in front of a fixed camera built from the death position; the screen
-- publishes the yaw as `spin` and the engine applies it to the player.
--
-- `died.tim` is an 8bpp page: rows 0..0x40 hold the red "YOU DIED" art and
-- rows 0x40..0xB8 the grey static backdrop, drawn as four mirrored 160x120
-- quadrants so the static tiles seamlessly. The strip is 256 one-pixel
-- columns, each displaced by `sin(phase) * param^2 >> 13` with the phase
-- stepping 0x100 a column. At the entry amplitude the wave is ~1700px, so the
-- two halves fly in from above and below and settle into 16 ripples.
--
-- args: x, y, z (the corpse's position), angle (its yaw), fov (the room
-- camera's focal length).

local Fade = require("lib/fade")

local M = {}

-- The entry white flash's counter (i16 -384, decays from 0x7FFF).
local FLASH_COUNTER = 0xFE80
-- The black fade-out's counter.
local FADE_OUT_COUNTER = 0x180
-- The confirm skip's black fade.
local SKIP_COUNTER = 0x800
-- Ticks the settled strip is held (the original's `0x30` comparison).
local HOLD_FRAMES = 0x30
-- The black overlay ramp step, the level past which the wave steps down and
-- the ceiling under which the overlay keeps updating.
local RECT_RAMP = 3
local RECT_STEP_THRESHOLD = 0x3F
local RECT_MAX = 0xFF
-- The wave amplitude at entry and the value the settled strip holds.
local WAVE_START = 0x3B
local WAVE_SETTLED = 4
-- The corpse's yaw step per tick.
local SPIN_STEP = 8
-- The fixed camera: 5000 units along X and 7000 above the corpse, looking
-- at a point 1000 above it.
local CAMERA_OFFSET_X = 5000
local CAMERA_HEIGHT = 7000
local TARGET_HEIGHT = 1000
-- The strip's columns and rows, and its left edge in the 320-wide frame.
local STRIP_COLUMNS = 256
local STRIP_ROWS = 0x40
local STRIP_X = 32
-- The backdrop's region of the page and its four mirrored quadrants.
local BACKDROP_V = 0x40
local QUADRANT_W = 160
local QUADRANT_H = 120
local QUADRANTS = {
    { 0, 0, false, false },
    { 0, 120, false, true },
    { 160, 0, true, false },
    { 160, 120, true, true },
}
-- The original's fix12 sine `(int)(sin(phase * 2pi / 4096) * 4096)` at the
-- column phases `column * 0x100`, which repeat every 16 columns.
local SIN = {
    0, 1567, 2896, 3784, 4095, 3784, 2896, 1567,
    0, -1567, -2896, -3784, -4095, -3784, -2896, -1567,
}

local s
local page

--- Wrap an integer to a signed 16-bit value.
local function i16(value)
    return ((value + 0x8000) & 0xFFFF) - 0x8000
end

--- The strip's baseline for amplitude `param`: the negative centre-origin Y
--- of the column tops before the wave.
function M.strip_base(param)
    local squared = param * 8 * param * 8
    local u = ((squared >> 4) + 0x1600) & 0xFFFF
    return i16(-((u * 0x40) >> 13))
end

--- One column's wave displacement for amplitude `param`. The original
--- shifts the product unsigned, so a negative one wraps below the baseline.
function M.strip_wave(param, column)
    local product = (SIN[column % 16 + 1] * param * param) & 0xFFFFFFFF
    return i16((product >> 13) & 0xFFFF)
end

function M.open(api, args)
    s = {
        state = 1,
        image_idx = WAVE_START,
        hold = 0,
        rect = 0,
        rect_color = 0,
        -- The amplitude the strip draws with this frame, if any.
        strip = nil,
        fade = Fade.inactive(),
        spin = args.angle,
        from = { args.x + CAMERA_OFFSET_X, args.y - CAMERA_HEIGHT, args.z },
        to = { args.x, args.y - TARGET_HEIGHT, args.z },
        fov = args.fov,
    }
    s.fade:set(1, FLASH_COUNTER, 0x7FFF)
    M.state = s
    local err
    page, err = api:page("ui/died")
    if not page then
        api:log("warning: " .. err)
    end
end

function M.update(api, input)
    -- Only the states that draw the strip set it.
    s.strip = nil
    local armed = false
    local finished = false
    if s.state == 1 then
        if s.fade.state < 0 then
            s.state = 2
        end
    elseif s.state == 2 then
        if s.rect_color < RECT_MAX then
            s.rect = s.rect_color
        end
        s.rect_color = i16(s.rect_color + RECT_RAMP)
        if s.rect_color > RECT_STEP_THRESHOLD then
            -- The strip draws the pre-step amplitude, then it steps down.
            s.strip = s.image_idx
            s.image_idx = s.image_idx - 1
        end
        if s.image_idx < WAVE_SETTLED then
            s.state = 3
        end
    elseif s.state == 3 then
        s.strip = s.image_idx
        s.hold = s.hold + 1
        if s.hold > HOLD_FRAMES then
            s.state = 4
            s.hold = 0
        end
    elseif s.state == 4 then
        s.fade:set(2, FADE_OUT_COUNTER, 0)
        armed = true
        s.image_idx = WAVE_SETTLED
        s.state = 5
    elseif s.state == 5 then
        s.strip = s.image_idx
        s.image_idx = s.image_idx + 1
        finished = s.fade.state < 0
    end
    if not armed then
        s.fade:tick()
    end
    if input.confirm then
        s.fade:set(2, SKIP_COUNTER, 0)
        finished = true
    end
    if finished then
        api:exit("title")
        return
    end
    s.spin = (s.spin + SPIN_STEP) & 0xFFFF
    api:set("spin", s.spin)
end

--- The static backdrop, the corpse, the black overlay, the strip and the
--- screen's own fade. Without the page only the fade draws.
function M.draw(api)
    api:clear()
    s.fade:present(api)
    if not page then
        return
    end
    local width, height = api:width(), api:height()
    local ox, oy = width // 2 - 160, height // 2 - 120
    for _, quadrant in ipairs(QUADRANTS) do
        local x, y, mirror_u, mirror_v = quadrant[1], quadrant[2], quadrant[3], quadrant[4]
        api:page_sprite_mirrored(page, 0, BACKDROP_V, QUADRANT_W, QUADRANT_H,
            ox + x, oy + y, QUADRANT_W, QUADRANT_H, 0, 2, mirror_u, mirror_v)
    end
    api:draw_actor("player", { from = s.from, to = s.to, fov = s.fov })
    if s.rect > 0 then
        api:rect(0, 0, width, height, 0, s.rect)
    end
    if s.strip then
        local base = M.strip_base(s.strip)
        for column = 0, STRIP_COLUMNS - 1 do
            local top = i16(base - M.strip_wave(s.strip, column)) + 120
            api:page_sprite_scaled(page, column, 0, 1, STRIP_ROWS,
                ox + STRIP_X + column, oy + top, 1, STRIP_ROWS, 0, 255)
        end
    end
end

return M
