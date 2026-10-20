-- The rocket-launcher epilogue after the RESULT screen (endings 6/7).
--
-- A port of ending_epilogue_* (EndingScreen.cpp:687-836): the rc1121
-- backdrop, the two caption plates from the en07 page and the spinning
-- i73v.ivm rocket launcher, lit by the sequence's own light record (full
-- ambient plus one white light), held for 0x10D frames and faded out.

local E = require("lib/ending")

local M = {}

-- Frames the built scene is held, counted from the first update after the
-- build, fade-in included (EndingScreen.cpp:812-818).
local HOLD = 0x10D
-- The backdrop's fixed shade level (s_bgSlot->fadeLevel, 0x6000,
-- EndingScreen.cpp:792): the still draws at 0x60/0x80 brightness.
local BACKDROP_FADE = 0x6000
-- The per-frame fade step: the reference's 0x800 counter step of 0x8000.
local FADE_STEP = 16
-- The model spin step per frame.
local SPIN_STEP = 0x20

-- The model's light record (ending_epilogue_build, EndingScreen.cpp:742-750):
-- setBackColor(0xFFF, 0xFFF, 0xFFF) plus one white light at (100, 100, 100),
-- a direction rather than a world point. The reference leaves light slots 1
-- and 2 untouched (dark here).
local LIGHT = {
    ambient = 0xFFF,
    sources = { { pos = { 100, 100, 100 }, color = { 0xFF, 0xFF, 0xFF } } },
}
-- The fixed camera.
local CAMERA = { from = { 12000, 0, 0 }, to = { 0, 0, 0 }, fov = 0x1A4 }

local backdrop
local plate
local model
local s

--- Wrap to a signed 16-bit angle.
local function i16(value)
    return ((value + 0x8000) & 0xFFFF) - 0x8000
end

function M.open(api)
    local err
    backdrop, err = api:picture("ui/rc1121")
    if not backdrop then
        api:log("warning: no epilogue backdrop: " .. err)
    end
    plate, err = api:page("ui/en07")
    if not plate then
        api:log("warning: no epilogue plate: " .. err)
    end
    model, err = api:ivm("item/i73v.ivm")
    if not model then
        api:log("warning: no epilogue model: " .. err)
    end
    s = { captions = {}, spin = 0, step = 0, timer = 0, fade = 0, done = false }
    -- The captions are credit entries 6 and 7, fully faded in from the start.
    for index, entry in ipairs({ 6, 7 }) do
        local slot = E.slot(E.CREDIT_ENTRIES[entry])
        slot.active = true
        slot.fade_level = E.FADE_FULL
        s.captions[index] = slot
    end
    M.state = s
end

function M.update(api)
    if s.step == 0 then
        s.fade = 255
        s.step = 1
        s.timer = 0
    elseif s.step == 1 then
        -- Fade in from black while the hold counts.
        if s.fade > 0 then
            s.fade = s.fade - FADE_STEP
            if s.fade < 0 then
                s.fade = 0
            end
        end
        s.timer = s.timer + 1
        if s.timer > HOLD then
            s.step = 2
        end
    elseif s.step == 2 then
        s.fade = s.fade + FADE_STEP
        if s.fade >= 255 then
            s.fade = 255
            s.done = true
        end
    end
    -- Spin the model about Z one step per frame.
    s.spin = i16(s.spin + SPIN_STEP)
    api:fade(s.fade)
    if s.done then
        api:exit("ending_save")
    end
end

function M.draw(api)
    E.black(api)
    if backdrop then
        local shade = E.shade_scale(E.fade_shade(BACKDROP_FADE))
        api:draw_background(backdrop, 0, 0, shade, shade, shade)
    end
    for _, slot in ipairs(s.captions) do
        slot:draw(api, plate)
    end
    if model then
        -- Root X rotation 0xF95C at (0, 500, 0), the Z spin, then the leaf
        -- X rotation 0xAA (EndingScreen.cpp:756-776, 824-827).
        api:draw_ivm(model, {
            joints = {
                { rx = 0x0F95C & 0x0FFF, ty = 500 },
                { rz = s.spin },
                { rx = 0x0AA },
            },
            camera = CAMERA,
            light = LIGHT,
        })
    end
end

return M
