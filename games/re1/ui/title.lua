-- The title screen.
--
-- The background is `ui/title`. The prompt rows come from the shipped 4bpp
-- `ui/t_press` sheet (or `ui/t_start` when that is the only one present):
-- row 0 is PRESS ANY BUTTON, row 83 the NEW GAME/LOAD GAME band with NEW GAME
-- highlighted and row 175 the band with LOAD GAME highlighted. A band is a
-- full 256-pixel-wide sprite drawn at the original descriptor position, x
-- -130 and y 38 from the screen centre, exactly as the original's sprite path
-- does.
--
-- The PRESS text ramps up from black, then pulses bright/dim. Any key opens
-- the option menu, where left/right picks NEW GAME or LOAD GAME. LOAD is
-- refused when no save exists: the cursor never leaves NEW GAME. An idle
-- title that reaches the `g_titleDemoTime` timeout (0x708 ticks after the
-- PRESS ramp) fades to black and exits to the attract demo; any button during
-- that fade cancels back to the PRESS wait, like the original's state 3.
--
-- args.folder_saves: a registered save screen lists the whole save folder, so
-- every loadable file counts as a save rather than only the eight slots.

local Fade = require("lib/fade")

local M = {}

-- The sandbox has no math library.
local function min(a, b)
    return a < b and a or b
end

-- The descriptor x of every prompt row, from the screen centre.
local PROMPT_X = -130
-- The descriptor y the row's own screen-y is offset from.
local PROMPT_Y = 38
-- Brightness the PRESS ramp adds each tick, and where it ends (0x80).
local PRESS_RAMP_STEP = 4
local PRESS_FULL = 0x80
-- Ticks per PRESS blink phase.
local BLINK_TICKS = 32
-- Idle ticks before the attract demo (the original's g_titleDemoTime).
local ATTRACT_TIMEOUT = 0x708
-- The attract fade-out counter (type 2 black, the original's 0x400).
local ATTRACT_FADE_COUNTER = 0x400 >> 7

-- Prompt rows: sheet row, sprite height and descriptor screen-y.
local ROW_PRESS = { v = 0, height = 54, y = 24 }
local ROW_NEW_GAME = { v = 83, height = 70, y = 8 }
local ROW_LOAD_GAME = { v = 175, height = 70, y = 8 }

local background
local prompt
-- "press", "menu", "fade_out" (confirm flash chain) or "attract_fade".
local stage = "press"
-- 1 NEW GAME, 2 LOAD GAME.
local selection = 1
local any_saves = false
local press_brightness = 0
-- The overlay accumulator: the confirm flash chain or the attract fade. The
-- title has no entry fade; the PRESS ramp is the whole entry animation.
local fade = Fade.inactive()
-- The confirm flash chain leg (the original's g_titleOptionsFading 6..9, 4).
local fade_phase = 0
-- The idle countdown to the attract demo; negative during the PRESS ramp.
local attract_countdown = -1
local ticks = 0
local exit_action

--- Publish what the engine reads back: the pacing and the overlay.
local function publish(api)
    -- Confirming NEW GAME or LOAD GAME sets the original's game-active flag,
    -- so the flash chain runs on the 33 ms gameplay limiter.
    api:set("game_active", stage == "fade_out")
    -- For the engine's tests.
    api:set("selection", selection)
    api:set("load_enabled", any_saves)
    fade:present(api)
end

--- Move the cursor; LOAD GAME is unreachable while no save exists.
local function move_selection(delta)
    if not any_saves then
        selection = 1
        return
    end
    selection = (selection + delta <= 1) and 1 or 2
end

--- The PRESS text's colour scale: ramp up from black, then pulse.
local function press_scale()
    if press_brightness < PRESS_FULL then
        return min(press_brightness * 2, 255)
    elseif (ticks // BLINK_TICKS) % 2 == 0 then
        return 255
    else
        return 140
    end
end

--- Latch the next leg of the confirm flash chain (7, 8, 9 then 4; leg 6 is
--- the white flash set on confirm). Returns true once the black-out is done.
local function advance_fade_phase()
    fade_phase = fade_phase + 1
    if fade_phase == 1 then
        -- 7: white decay at -0x4000.
        fade = Fade.fade_in(0x4000 >> 7, 0x7FFF, 1)
    elseif fade_phase == 2 then
        -- 8: white decay at -0x8000.
        fade = Fade.fade_in(256, 0x7FFF, 1)
    elseif fade_phase == 3 then
        -- 9: white decay at -0x800.
        fade = Fade.fade_in(0x800 >> 7, 0x7FFF, 1)
    elseif fade_phase == 4 then
        -- 4: black-out at +0x270.
        fade = Fade.inactive()
        fade:set(2, 0x270, 0)
    else
        return true
    end
    return false
end

function M.open(api, args)
    local err
    background, err = api:picture("ui/title")
    if not background then
        api:log("warning: no title background: " .. err)
    end
    local stem
    for _, candidate in ipairs({ "ui/t_press", "ui/t_start" }) do
        if api:has_image(candidate) then
            stem = candidate
            break
        end
    end
    if stem then
        prompt, err = api:page(stem)
        if not prompt then
            api:log("warning: invalid title prompt sheet: " .. err)
        end
    else
        api:log("warning: missing title prompt sheet (t_press/t_start)")
    end
    any_saves = false
    if args.folder_saves then
        for _, entry in ipairs(api:saves()) do
            if entry.loadable then
                any_saves = true
                break
            end
        end
    else
        for _, present in ipairs(api:save_slots()) do
            if present then
                any_saves = true
                break
            end
        end
    end
    selection = any_saves and 2 or 1
    publish(api)
end

local function update(api, input)
    ticks = ticks + 1
    -- TODO(parity): (UI) the original's title state 2 also cycles the
    -- selection id NEW/LOAD (and the DC STANDARD/TRAINING/ADVANCED submenu)
    -- while idle before handing off to the attract demo. The port keeps the
    -- two entries and hands the idle timeout straight to the reel. The PRESS
    -- phase and the option-menu moves are silent in the original; confirming
    -- NEW GAME or LOAD GAME plays EVIL01 and runs the white flash chain.
    if stage == "press" then
        fade:tick()
        press_brightness = min(press_brightness + PRESS_RAMP_STEP, PRESS_FULL)
        -- The idle countdown starts once the PRESS text has ramped to full;
        -- the original arms it when state 1 hands over to 2.
        if press_brightness >= PRESS_FULL then
            if attract_countdown < 0 then
                attract_countdown = ATTRACT_TIMEOUT
            end
            attract_countdown = attract_countdown - 1
            if attract_countdown <= 0 then
                -- The original's case 2: fade type 2 with counter 0x400.
                fade = Fade.fade_out(ATTRACT_FADE_COUNTER, 0, 2)
                stage = "attract_fade"
                return
            end
        end
        if input.any or input.confirm or input.cancel then
            stage = "menu"
            attract_countdown = -1
            ticks = 0
        end
    elseif stage == "menu" then
        fade:tick()
        if input.left then
            move_selection(-1)
        end
        if input.right then
            move_selection(1)
        end
        if input.up then
            move_selection(-1)
        end
        if input.down then
            move_selection(1)
        end
        if input.confirm then
            local action
            if selection == 2 then
                action = any_saves and "save_load" or nil
            else
                action = "char_select"
            end
            if action then
                api:sound("title")
                exit_action = action
                -- The original's state 6: a white flash at +0x7F00 that wraps
                -- the accumulator in two ticks.
                fade = Fade.fade_out(0x7F00 >> 7, 0, 1)
                fade_phase = 0
                stage = "fade_out"
            end
        end
    elseif stage == "fade_out" then
        fade:tick()
        if not fade:active() and advance_fade_phase() then
            api:exit(exit_action or "title")
            return
        end
    elseif stage == "attract_fade" then
        fade:tick()
        if not fade:active() then
            -- The fade reached black: hand off to the attract reel.
            api:exit("attract")
            return
        end
        -- Any button cancels back to the PRESS wait with the overlay cleared
        -- (the original's case 3 cancellation).
        if input.any or input.confirm or input.cancel then
            fade = Fade.inactive()
            attract_countdown = ATTRACT_TIMEOUT
            stage = "press"
        end
    end
end

function M.update(api, input)
    update(api, input)
    publish(api)
end

function M.draw(api)
    if background then
        -- A full-frame backdrop is stretched, so a replacement may be
        -- authored at any size (an HD title screen, for example).
        api:draw_background(background, 0, 0)
    else
        api:clear()
        api:fill_rect(0, 0, api:width(), api:height(), 0, 0, 0, 255)
    end
    if not prompt then
        return
    end
    local row, scale
    if stage == "press" or stage == "attract_fade" then
        row, scale = ROW_PRESS, press_scale()
    else
        row = selection == 2 and ROW_LOAD_GAME or ROW_NEW_GAME
        scale = 255
    end
    local origin_x = api:width() // 2
    local origin_y = api:height() // 2
    api:page_sprite_scaled(
        prompt,
        0, row.v, 256, row.height,
        PROMPT_X + origin_x, row.y + PROMPT_Y + origin_y, 256, row.height,
        0, scale, "white"
    )
end

return M
