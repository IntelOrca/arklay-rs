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
-- The screen state, kept in one table so the engine's tests can inspect it.
local s

local function reset()
    s = {
        -- "press", "menu", "fade_out" (confirm flash chain) or "attract_fade".
        stage = "press",
        -- 1 NEW GAME, 2 LOAD GAME.
        selection = 1,
        any_saves = false,
        press_brightness = 0,
        -- The overlay accumulator: the confirm flash chain or the attract
        -- fade. The title has no entry fade; the PRESS ramp is the whole
        -- entry animation.
        fade = Fade.inactive(),
        -- The confirm flash chain leg (the original's g_titleOptionsFading
        -- 6..9, 4).
        fade_phase = 0,
        -- The idle countdown to the attract demo; negative during the PRESS
        -- ramp.
        attract_countdown = -1,
        ticks = 0,
        exit_action = nil,
    }
    M.state = s
end

--- Publish what the engine reads back: the pacing and the overlay.
local function publish(api)
    -- Confirming NEW GAME or LOAD GAME sets the original's game-active flag,
    -- so the flash chain runs on the 33 ms gameplay limiter.
    api:set("game_active", s.stage == "fade_out")
    s.fade:present(api)
end

--- Move the cursor; LOAD GAME is unreachable while no save exists.
local function move_selection(delta)
    if not s.any_saves then
        s.selection = 1
        return
    end
    s.selection = (s.selection + delta <= 1) and 1 or 2
end

--- The PRESS text's colour scale: ramp up from black, then pulse.
local function press_scale()
    if s.press_brightness < PRESS_FULL then
        return min(s.press_brightness * 2, 255)
    elseif (s.ticks // BLINK_TICKS) % 2 == 0 then
        return 255
    else
        return 140
    end
end

M.press_scale = press_scale

--- Latch the next leg of the confirm flash chain (7, 8, 9 then 4; leg 6 is
--- the white flash set on confirm). Returns true once the black-out is done.
local function advance_fade_phase()
    s.fade_phase = s.fade_phase + 1
    if s.fade_phase == 1 then
        -- 7: white decay at -0x4000.
        s.fade = Fade.fade_in(0x4000 >> 7, 0x7FFF, 1)
    elseif s.fade_phase == 2 then
        -- 8: white decay at -0x8000.
        s.fade = Fade.fade_in(256, 0x7FFF, 1)
    elseif s.fade_phase == 3 then
        -- 9: white decay at -0x800.
        s.fade = Fade.fade_in(0x800 >> 7, 0x7FFF, 1)
    elseif s.fade_phase == 4 then
        -- 4: black-out at +0x270.
        s.fade = Fade.inactive()
        s.fade:set(2, 0x270, 0)
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
    reset()
    if args.folder_saves then
        for _, entry in ipairs(api:saves()) do
            if entry.loadable then
                s.any_saves = true
                break
            end
        end
    else
        for _, present in ipairs(api:save_slots()) do
            if present then
                s.any_saves = true
                break
            end
        end
    end
    s.selection = s.any_saves and 2 or 1
    publish(api)
end

local function update(api, input)
    s.ticks = s.ticks + 1
    -- TODO(parity): (UI) the original's title state 2 also cycles the
    -- selection id NEW/LOAD (and the DC STANDARD/TRAINING/ADVANCED submenu)
    -- while idle before handing off to the attract demo. The port keeps the
    -- two entries and hands the idle timeout straight to the reel. The PRESS
    -- phase and the option-menu moves are silent in the original; confirming
    -- NEW GAME or LOAD GAME plays EVIL01 and runs the white flash chain.
    if s.stage == "press" then
        s.fade:tick()
        s.press_brightness = min(s.press_brightness + PRESS_RAMP_STEP, PRESS_FULL)
        -- The idle countdown starts once the PRESS text has ramped to full;
        -- the original arms it when state 1 hands over to 2.
        if s.press_brightness >= PRESS_FULL then
            if s.attract_countdown < 0 then
                s.attract_countdown = ATTRACT_TIMEOUT
            end
            s.attract_countdown = s.attract_countdown - 1
            if s.attract_countdown <= 0 then
                -- The original's case 2: fade type 2 with counter 0x400.
                s.fade = Fade.fade_out(ATTRACT_FADE_COUNTER, 0, 2)
                s.stage = "attract_fade"
                return
            end
        end
        if input.any or input.confirm or input.cancel then
            s.stage = "menu"
            s.attract_countdown = -1
            s.ticks = 0
        end
    elseif s.stage == "menu" then
        s.fade:tick()
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
            if s.selection == 2 then
                action = s.any_saves and "save_load" or nil
            else
                action = "char_select"
            end
            if action then
                api:sound("title")
                s.exit_action = action
                -- The original's state 6: a white flash at +0x7F00 that wraps
                -- the accumulator in two ticks.
                s.fade = Fade.fade_out(0x7F00 >> 7, 0, 1)
                s.fade_phase = 0
                s.stage = "fade_out"
            end
        end
    elseif s.stage == "fade_out" then
        s.fade:tick()
        if not s.fade:active() and advance_fade_phase() then
            api:exit(s.exit_action or "title")
            return
        end
    elseif s.stage == "attract_fade" then
        s.fade:tick()
        if not s.fade:active() then
            -- The fade reached black: hand off to the attract reel.
            api:exit("attract")
            return
        end
        -- Any button cancels back to the PRESS wait with the overlay cleared
        -- (the original's case 3 cancellation).
        if input.any or input.confirm or input.cancel then
            s.fade = Fade.inactive()
            s.attract_countdown = ATTRACT_TIMEOUT
            s.stage = "press"
        end
    end
end

function M.update(api, input)
    update(api, input)
    publish(api)
end

function M.draw(api)
    -- A shorter (wider-aspect) frame shows the picture's bottom rows: the
    -- logo, the menu and the copyright lines, losing the top of the eye.
    local lift = (api:height() - 240) // 2
    if background then
        -- A full-frame backdrop is stretched, so a replacement may be
        -- authored at any size (an HD title screen, for example).
        api:draw_background(background, 0, lift)
    else
        api:clear()
        api:fill_rect(0, 0, api:width(), api:height(), 0, 0, 0, 255)
    end
    if not prompt then
        return
    end
    local row, scale
    if s.stage == "press" or s.stage == "attract_fade" then
        row, scale = ROW_PRESS, press_scale()
    else
        row = s.selection == 2 and ROW_LOAD_GAME or ROW_NEW_GAME
        scale = 255
    end
    local origin_x = api:width() // 2
    local origin_y = api:height() // 2 + lift
    api:page_sprite_scaled(
        prompt,
        0, row.v, 256, row.height,
        PROMPT_X + origin_x, row.y + PROMPT_Y + origin_y, 256, row.height,
        0, scale, "white"
    )
end

return M
