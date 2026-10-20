-- The character-selection screen.
--
-- `ui/sel_back` is the desk background. The two police cards come from
-- `ui/select_b`: the sheet holds one shared blue S.T.A.R.S. portrait half at
-- (0, 0), Chris's information half at (0x50, 0) and Jill's at (0, 0x80), so
-- both cards draw the shared portrait half next to their own information
-- half. The selected card sits at its front position at full scale and
-- brightness, the other at its back position scaled to 0.8125 and dimmed.
-- While the pick changes the cards slide, overshoot and settle on the
-- opposite poses over 34 ticks, exactly like the original's acceleration
-- state machine: the card on the way out accelerates toward the other slot,
-- its velocity reverses, the scale and brightness ramp by +-2 and +-3 per
-- frame, and the tpage (draw order) flips at the velocity zero crossing. The
-- blinking pick arrows are the small sprites at (0xC0, 0x30)/(0xC0, 0x50).
-- `ui/select_k` is loaded the way the shipped screen does, but the end poses
-- do not sample it.
--
-- Left/right switches the pick, confirm starts a new game with that
-- character and cancel fades back to the title.

local Fade = require("lib/fade")

local M = {}

local function min(a, b)
    return a < b and a or b
end

local function max(a, b)
    return a > b and a or b
end

-- Width of a card's left (portrait) half.
local CARD_LEFT_W = 0x50
-- Width of a card's right (information) half.
local CARD_RIGHT_W = 0x70
-- Height of both card halves.
local CARD_H = 0x80
-- The selected card's position (the original's g_char0PosX/Y).
local FRONT_POS = { 0x88, 0x48 }
-- The unselected card's position (the original's g_char1PosX/Y).
local BACK_POS = { 200, 0x68 }
-- The panel scale fields ComputeScale maps to 1.0 and 0.8125.
local FRONT_PANEL_SCALE = 0xF0
local BACK_PANEL_SCALE = 0x110
-- The panel brightness fields.
local FRONT_PANEL_BRIGHT = 0x80
local BACK_PANEL_BRIGHT = 0x50
-- The card shadow's black blend, 100 / 255.
local SHADOW_ALPHA = 100
-- Pick-arrow source region and the rows for the bright and dim blink.
local ARROW_W = 0x0C
local ARROW_H = 0x0B
local ARROW_V_BRIGHT = 0x30
local ARROW_V_DIM = 0x50
-- Ticks per arrow blink phase.
local BLINK_MASK = 0x30

local background
local cards
-- The screen state, kept in one table so the engine's tests can inspect it.
local s

--- The boot poses: card 1 (Chris) at the front, card 2 (Jill) at the back.
local function initial_poses()
    return {
        {
            x = FRONT_POS[1], y = FRONT_POS[2],
            scale = FRONT_PANEL_SCALE, bright = FRONT_PANEL_BRIGHT, tpage = 2,
            vx = 0, vy = 0, ax = 0, ay = 0,
        },
        {
            x = BACK_POS[1], y = BACK_POS[2],
            scale = BACK_PANEL_SCALE, bright = BACK_PANEL_BRIGHT, tpage = 3,
            vx = 0, vy = 0, ax = 0, ay = 0,
        },
    }
end

local function reset()
    s = {
        -- 0 Chris, 1 Jill: which card owns the front pose once a swap settles.
        selected = 0,
        poses = initial_poses(),
        -- The swap state machine (the original's g_selSubState/g_selTimer).
        swap_sub = 0,
        swap_timer = 0,
        swap_dir = 0,
        -- Black while the screen fades in, then the original's white flash
        -- chain once the pick is confirmed.
        fade = Fade.fade_in(8, 0x7FFF, 2),
        -- Ticks left of the post-flash hold. The original stores -16 in a
        -- signed byte and decrements to zero, so the hold is 240 ticks.
        hold_timer = 0,
        ticks = 0,
        -- "idle", "swapping", "confirm_fade_in" (state 3),
        -- "confirm_fade_out" (state 4), "confirm_hold" (state 5) or
        -- "cancel_fade_out" (state 7).
        stage = "idle",
        exit = nil,
    }
    M.state = s
end

--- Start the swap toward `dir` (0 right, 1 left, the original's
--- g_selSwapDir). The pick itself flips when the slide lands.
local function start_swap(dir)
    dir = dir & 1
    local ax, ay
    if dir == s.selected then
        ax, ay = 2, 1
    else
        ax, ay = -2, -1
    end
    s.poses[1].ax, s.poses[1].ay = ax, ay
    s.poses[2].ax, s.poses[2].ay = -ax, -ay
    for _, pose in ipairs(s.poses) do
        pose.vx, pose.vy = 0, 0
    end
    s.swap_dir = dir
    s.swap_sub = 0
    s.swap_timer = 8
    s.stage = "swapping"
end

--- The original's ComputeScale: the panel scale field to 4.12.
local function compute_scale(panel)
    return panel * -24 + 0x2680
end

--- The sprite brightness the panel field maps to (0x80 full, 0x50 160).
local function panel_brightness(panel)
    return min(panel * 2, 255)
end

local function add_velocity()
    for _, pose in ipairs(s.poses) do
        pose.vx = pose.vx + pose.ax
        pose.vy = pose.vy + pose.ay
    end
end

local function reverse_accel()
    for _, pose in ipairs(s.poses) do
        pose.ax, pose.ay = -pose.ax, -pose.ay
    end
end

local function apply_position()
    for _, pose in ipairs(s.poses) do
        pose.x = pose.x + pose.vx
        pose.y = pose.y + pose.vy
    end
end

--- Phase 3's ramp: the selected card grows and brightens, the other shrinks
--- and dims, by +-2/+-3 per frame (the brightness is a byte that saturates).
local function ramp_scale()
    local front, back = s.poses[1], s.poses[2]
    if s.selected ~= 0 then
        front, back = back, front
    end
    front.scale = front.scale + 2
    back.scale = back.scale - 2
    front.bright = max(front.bright - 3, 0)
    back.bright = min(back.bright + 3, 255)
end

--- Enter the second movement phase and, when the direction needs no coast
--- window, fall straight through into the final phase in this same tick (the
--- original's case 3 -> case 4 fallthrough).
local function swap_sub_4_entry()
    s.swap_sub = 4
    add_velocity()
    s.swap_timer = s.swap_dir << 2
    if s.swap_timer == 0 then
        s.swap_sub = 5
        s.swap_timer = 7
        reverse_accel()
        add_velocity()
    end
end

--- The shared sub-2/3 entry: flip the draw order at the velocity zero
--- crossing, ramp the scale, then either coast or enter the next phase.
local function swap_sub_23()
    if s.poses[1].vx == 0 then
        s.swap_sub = 3
        s.poses[1].tpage = s.poses[1].tpage ~ 1
        s.poses[2].tpage = s.poses[2].tpage ~ 1
    end
    ramp_scale()
    if s.swap_timer == 0 then
        swap_sub_4_entry()
    else
        add_velocity()
    end
end

--- One tick of the original's swap state machine.
local function step_swap()
    s.swap_timer = s.swap_timer - 1
    local sub = s.swap_sub
    if sub == 0 then
        if s.swap_timer == 0 then
            s.swap_sub = 1
            add_velocity()
            s.swap_timer = (s.swap_dir ~ 1) << 2
            if s.swap_timer == 0 then
                s.swap_sub = 2
                s.swap_timer = 0x0F
                reverse_accel()
                swap_sub_23()
            end
        else
            add_velocity()
        end
    elseif sub == 1 then
        if s.swap_timer == 0 then
            s.swap_sub = 2
            s.swap_timer = 0x0F
            reverse_accel()
            swap_sub_23()
        end
    elseif sub == 2 then
        swap_sub_23()
    elseif sub == 3 then
        ramp_scale()
        if s.swap_timer == 0 then
            swap_sub_4_entry()
        else
            add_velocity()
        end
    elseif sub == 4 then
        if s.swap_timer == 0 then
            s.swap_sub = 5
            s.swap_timer = 7
            reverse_accel()
            add_velocity()
        end
    elseif sub == 5 then
        if s.swap_timer == 0 then
            s.selected = s.selected ~ 1
            add_velocity()
            apply_position()
            s.stage = "idle"
            return
        end
        add_velocity()
    end
    apply_position()
end

--- The screen position of a card's top-left corner.
local function card_origin(api, pose)
    return pose.x - 0x100 + api:width() // 2, pose.y - 0x98 + api:height() // 2
end

--- Draw one card: the shared portrait half then this character's
--- information half (Chris at (0x50, 0), Jill at (0, 0x80); the sheet's
--- other columns are padding and must not be sampled).
local function draw_card(api, card)
    if not cards then
        return
    end
    local pose = s.poses[card]
    local x, y = card_origin(api, pose)
    local scale = compute_scale(pose.scale)
    local brightness = panel_brightness(pose.bright)
    local info_x, info_y = CARD_LEFT_W, 0
    if card == 2 then
        info_x, info_y = 0, CARD_H
    end
    local left_w = (CARD_LEFT_W * scale) >> 12
    local right_w = (CARD_RIGHT_W * scale) >> 12
    local height = (CARD_H * scale) >> 12
    api:picture_sprite_scaled(cards, 0, 0, CARD_LEFT_W, CARD_H, x, y, left_w, height, brightness)
    api:picture_sprite_scaled(
        cards, info_x, info_y, CARD_RIGHT_W, CARD_H, x + left_w, y, right_w, height, brightness
    )
end

--- The black shadow the original lays over the back card.
local function draw_shadow(api, card)
    local pose = s.poses[card]
    local x, y = card_origin(api, pose)
    local scale = compute_scale(pose.scale)
    api:blend_black_rect(
        x, y, ((CARD_LEFT_W + CARD_RIGHT_W) * scale) >> 12, (CARD_H * scale) >> 12, SHADOW_ALPHA
    )
end

--- The two blinking pick arrows.
local function draw_cursors(api)
    if not cards then
        return
    end
    local v = (s.ticks & BLINK_MASK) == 0 and ARROW_V_DIM or ARROW_V_BRIGHT
    local cx, cy = api:width() // 2, api:height() // 2
    local y = -0x16 + cy
    api:picture_sprite_scaled(cards, 0xC0, v, ARROW_W, ARROW_H, -0x88 + cx, y, ARROW_W, ARROW_H, 255)
    api:picture_sprite_scaled(
        cards, 0xC0, v + 0x10, ARROW_W, ARROW_H, 0x4C + cx, y, ARROW_W, ARROW_H, 255
    )
end

local function black(api)
    api:clear()
    api:fill_rect(0, 0, api:width(), api:height(), 0, 0, 0, 255)
end

function M.open(api)
    local err
    background, err = api:picture("ui/sel_back")
    if not background then
        api:log("warning: no character-select background: " .. err)
    end
    -- The card sheet's transparency is stored as exact black (the original's
    -- BGR555 0x8000 cut-outs around the rounded corners and the arrows); the
    -- converted 24-bit BMP cannot carry alpha, so key black out here.
    cards, err = api:picture("ui/select_b", 0x000000, "shipped")
    if not cards then
        api:log("warning: no character-select cards: " .. err)
    end
    -- The shipped screen's second selection sheet; loaded for parity.
    local extras
    extras, err = api:page("ui/select_k")
    if not extras then
        api:log("warning: no character-select sheet: " .. err)
    end
    reset()
    s.fade:present(api)
end

local function update(api, input)
    s.ticks = s.ticks + 1
    local stage = s.stage
    if stage == "idle" then
        s.fade:tick()
        if input.left or input.right then
            api:sound("select_cursor")
            -- Left is g_selSwapDir 1, right 0. The original enters the
            -- animation in the same tick (goto case_2).
            start_swap(input.left and 1 or 0)
            step_swap()
        elseif input.confirm then
            api:sound("select_confirm")
            s.exit = s.selected
            -- State 3: the white flash rises two alpha steps a tick for 128
            -- ticks (set_fading(1, 0x100)).
            s.fade = Fade.fade_out(2, 0, 1)
            s.stage = "confirm_fade_in"
        elseif input.cancel then
            -- The original's cancel is silent and black-fades
            -- (set_fading(2, 0xC00)), ~11 ticks.
            s.fade = Fade.fade_out(24, 0, 2)
            s.stage = "cancel_fade_out"
        end
    elseif stage == "swapping" then
        step_swap()
    elseif stage == "confirm_fade_in" then
        s.fade:tick()
        if not s.fade:active() then
            -- State 4: the full white decays from 0x7FFF at two alpha steps
            -- a tick.
            s.fade = Fade.fade_in(2, 0x7FFF, 1)
            s.stage = "confirm_fade_out"
        end
    elseif stage == "confirm_fade_out" then
        s.fade:tick()
        if not s.fade:active() then
            s.stage = "confirm_hold"
            s.hold_timer = 240
        end
    elseif stage == "confirm_hold" then
        s.hold_timer = max(s.hold_timer - 1, 0)
        if s.hold_timer == 0 then
            api:exit("new_game", s.exit)
        end
    elseif stage == "cancel_fade_out" then
        s.fade:tick()
        if not s.fade:active() then
            api:exit("title")
        end
    end
end

function M.update(api, input)
    update(api, input)
    s.fade:present(api)
end

function M.draw(api)
    -- The original blanks the background and both cards the instant the
    -- confirm flash fills the screen: state 4 decays the white over flat
    -- black, and the standalone cancel fade runs over black too.
    local stage = s.stage
    if stage == "confirm_fade_out" or stage == "confirm_hold" or stage == "cancel_fade_out" then
        black(api)
        return
    end
    if background then
        -- A full-frame backdrop is stretched, so a replacement may be
        -- authored at any size.
        api:draw_background(background, 0, 0)
    else
        black(api)
    end
    -- The back card draws first, then its shadow, then the front card. The
    -- tpage owns the order, so the flip mid-slide changes it. The arrows only
    -- draw while idle.
    local first, second = 1, 2
    if s.poses[1].tpage < s.poses[2].tpage then
        first, second = 2, 1
    end
    draw_card(api, first)
    draw_shadow(api, first)
    draw_card(api, second)
    if stage == "idle" then
        draw_cursors(api)
    end
end

return M
