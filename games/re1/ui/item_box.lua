-- The item-box storage screen (pause-menu mode 2).
--
-- The box is opened by a room's `item_box` action: the room freezes, the
-- status/inventory panel is drawn behind it and this overlay draws the
-- `ui/itemboxn` frame, the selected box slot's 40x30 icon with its quantity,
-- the three-row item name list and the page-position ticks. The player cursor
-- stays on the inventory grid (it only leaves it for the box's tab row):
-- confirming an inventory stack arms the swap, up/down (or L1/R1) page the box
-- cursor with a slide, and confirming again performs it. Cancel (or confirming
-- on a top-tab cursor) closes the screen.
-- The box only ever reaches the EXIT tab of the pause menu's top row: the
-- greyed-out Map, File and Radio tabs are unselectable (`itembox_draw_cursor`
-- draws every tab-row cursor on EXIT, and left/right do nothing there).
--
-- Args: `cursor`, the inventory panel's shared cursor the box adopts.
--
-- The screen publishes `cursor` (the inventory cursor, which the engine
-- mirrors onto the menu so the slot highlight and item name follow it; on the
-- tab row it is already the drawn EXIT cursor, the original's `cursor | 2`)
-- and, on the frame a confirmed swap happens, `swap = true` with `swap_box`
-- (the box slot) and `swap_slot` (the inventory slot); the engine performs the
-- swap. `api:exit()` closes the box and the pause menu. The engine tells the
-- screen whether the menu has revealed its cursor with `M.cursor_shown(api,
-- shown)` before each draw.
--
-- The frame tables preserve the original's memory layout order: entries are
-- listed in the order the original's backwards walk draws them.

local Grid = require("lib/menu_grid")
local Slot = require("lib/item_slot")

local M = {}

local ITEMBOX_STEM = "ui/itemboxn"
local STATUS_STEM = "ui/status"

-- Number of box slots (8bpp sheet and save block agree on 48).
local SLOTS_TOTAL = 48
-- The page-slide counter's wrap value (`SUBMENU_STATE_ID`, `0x0F`): the
-- counter steps by the slide direction and wraps to 0 there, which ends the
-- slide. The drawn states are 1..14, moving the list one row (15px).
local SLIDE_WRAP = 0x0F
-- The cursor blink counter's dim phase: while bit `0x20` of `DAT_00ae9f18` is
-- clear the original draws the slot cursor's dim cell (`texU 0xA8`).
local CURSOR_DIM_BIT = 0x20

-- Frame textures, in the original's draw order: the preview border, the two
-- box arrows, the item-list bottom and top edge lines (both sampling the
-- sheet's green edge rows). `{x, y, w, h, u, v}`.
local FRAME_PARTS_A = {
    { 0x5B, 0x57, 0x2A, 0x20, 0x10, 0x30 },
    { 0xAF, 0x53, 8, 7, 0x48, 0x40 },
    { 0xAF, 0x18, 8, 7, 0x48, 0x38 },
    { 41, 80, 128, 1, 0, 0x2F },
    { 41, 33, 128, 1, 0, 0 },
}
-- Mask rectangles: the nearest layer, clipping the sliding list content and
-- covering the inventory panel around the box. `{x, y, w, h}`.
local FRAME_MASKS = {
    { 0x5C, 0x77, 40, 9 },
    { 0x85, 0x51, 0x23, 0x0F },
    { 0x5B, 0x51, 0x2A, 6 },
    { 0x2A, 0x51, 0x31, 0x0F },
    { 0x2A, 0x18, 0x7E, 9 },
}
-- The leading frame textures, in draw order: the list box interior (whose
-- blue fill and green side columns close the rectangle), the bottom
-- half-arrow and the top arrow.
local FRAME_PARTS_C = {
    { 41, 34, 128, 0x2E, 0, 1 },
    { 0xAF, 0x20, 8, 0x20, 0, 0x30 },
    { 0xAF, 0x40, 8, 0x12, 8, 0x30 },
}

-- Half-strength shadow over the list's first and third name rows.
local SHADOW_TOP = { 42, 34, 126, 15 }
-- Half-strength shadow over the list's fourth name row.
local SHADOW_BOTTOM = { 42, 0x40, 126, 0x10 }
-- The browse-mode shadows over the whole list box and the preview slot.
local SHADOW_LIST = { 42, 34, 126, 0x2E }
local SHADOW_SLOT = { 0x5C, 0x58, 40, 0x1E }
-- The shadow dim, the original's `0x70` grey blend over the list panel.
local SHADOW_ALPHA = 0x70
-- The dark scrollbar tracks above and beside the list.
local TRACK_TOP = { 0xD2, 0x10, 0x5E, 0x10 }
local TRACK_SIDE = { 0xD2, 0x20, 0x2F, 0x10 }

-- The label the original prints on a slot row that holds nothing.
local NOTHING_LABEL = "_Nothing_"
-- The "box is full" warning line: its span `[0x2A, 0xA8)`, base y (the last
-- cursor's row) and the original's `ff ef 00` colour.
local FULL_LINE_X = 0x2A
local FULL_LINE_END_X = 0xA8
local FULL_LINE_Y = 0x31
local FULL_LINE_COLOR = { 0xFF, 0xEF, 0x00 }

-- The item-name list: left edge, first row y and row step. The list frame is
-- 128px wide: the widest English name ("Hand Gun Bullets") just fills it, and
-- a wider name (Latin drawn through the Japanese sheet) is condensed to fit.
local NAME_X = 0x2A
local NAME_Y = 0x23
local NAME_STEP = 0x0F
local NAME_MAX_WIDTH = 128

-- The box preview slot and its icon size.
local SLOT_X, SLOT_Y = 0x5C, 0x58
local SLOT_H = Slot.H
-- Page-position ticks.
local TICK_X, TICK_Y = 0xB0, 0x21
-- The preview column's bottom drawable row (exclusive). The right half of a
-- sliding slot would otherwise spill past the preview window, so the port
-- clips it at the panel's lower edge: the reference does paint those rows,
-- but the panel's own bottom strip and the mask strip `0x77..0x7F` cover
-- them, so the clip is pixel-equivalent.
local SLOT_CLIP_BOTTOM = 0x80

local s = {
    -- `browse` (`DAT_00ae9f20` 0/1) or `armed` (2/3).
    mode = "browse",
    -- The box cursor (`0..48`).
    box_cursor = 0,
    -- The inventory cursor value (`8 + 2 * slot`), or `4`/`6` on the tab row.
    player_cursor = Grid.FIRST_SLOT_CURSOR,
    -- Inventory slots of the current character.
    slots = 6,
    -- The page-slide counter: `0` idle, `1..=14` while the slide is drawn.
    slide = 0,
    -- Page-slide direction (`DAT_00ae9f1c`): `0` idle, `+1` next, `-1` previous.
    slide_dir = 0,
    -- The cursor blink counter (`DAT_00ae9f18`), a byte.
    cursor_blink = 0,
    -- Whether the menu has revealed its cursor (set by the engine).
    cursor_shown = false,
}
M.state = s

local function next_slot(slot)
    if slot >= SLOTS_TOTAL - 1 then
        return 0
    end
    return slot + 1
end

local function previous_slot(slot)
    if slot == 0 then
        return SLOTS_TOTAL - 1
    end
    return slot - 1
end

local function cursor_on_tab()
    return s.player_cursor < Grid.FIRST_SLOT_CURSOR
end

-- The cursor value both the box and the menu's tab highlight draw. The
-- item-box cursor only ever reaches the original's tab-row values 4 and 6,
-- and its `itembox_draw_cursor` (and the tab-art highlight) selects the tab
-- with `cursor | 2`: both values draw on EXIT. The other three tabs are
-- greyed out and unreachable.
local function drawn_cursor()
    if cursor_on_tab() then
        return s.player_cursor | 2
    end
    return s.player_cursor
end

-- Down one inventory row (`menu_itembox_interaction` state 1). Unlike the
-- pause menu's grid, the item box never wraps onto the Map/File row: past the
-- last slot row it lands on the tab-row cursor in the same column, 4 or 6.
local function move_down(cursor)
    local moved = cursor + 4
    if moved > s.slots * 2 + 6 then
        return (cursor & 2) | 4
    end
    return moved
end

-- Up one row: from the first slot row onto the tab row (4 or 6), and from
-- there back to the last slot row in the same column.
local function move_up(cursor)
    local moved = cursor - 4
    if moved < 0 or moved & 0xFC == 0 then
        return (cursor & 2) | (s.slots * 2 + 4)
    end
    return moved
end

-- The inventory slot under the player cursor.
local function player_slot()
    local slot = Grid.slot_of_cursor(s.player_cursor)
    if slot == nil or slot >= s.slots then
        return nil
    end
    return slot
end

-- The item id under the player cursor, or `0`.
local function selected_item(api)
    local slot = player_slot()
    if slot == nil then
        return 0
    end
    local id = api:inventory(slot)
    return id
end

function M.open(api, args)
    -- Drawn on black: a wider aspect gives the panel a wider canvas.
    api:set("canvas", 1)
    s.mode = "browse"
    s.box_cursor = 0
    s.player_cursor = args.cursor or Grid.FIRST_SLOT_CURSOR
    -- The box's tab row is the EXIT cursor pair only. A caller that hands the
    -- box a Map/File cursor (0/2) still opens on the row's own values, so no
    -- greyed-out tab can be highlighted.
    if s.player_cursor < Grid.FIRST_SLOT_CURSOR then
        s.player_cursor = (s.player_cursor & 2) | 4
    end
    s.slots = api:inventory_capacity()
    s.slide = 0
    s.slide_dir = 0
    s.cursor_blink = 0
    s.cursor_shown = false
    api:set("cursor", drawn_cursor())
    -- On a slot the box draws the shared cursor itself; on the tab row the
    -- menu draws its tab highlight under the EXIT tab art.
    api:set("on_slot", not Grid.is_tab(s.player_cursor))
end

function M.cursor_shown(_api, shown)
    s.cursor_shown = shown == true
end

-- One state-3 slide step: step the counter, wrap it at `SLIDE_WRAP`, and on
-- reaching 0 advance the box cursor for a forward slide and hand the grid
-- back to the armed state.
local function advance_slide()
    s.slide = (s.slide + s.slide_dir) & 0xFF
    if s.slide == SLIDE_WRAP then
        s.slide = 0
    end
    if s.slide == 0 then
        if s.slide_dir > 0 then
            s.box_cursor = next_slot(s.box_cursor)
        end
        s.slide_dir = 0
    end
end

-- Start a page slide: the original's state-2 page branch plus the first step
-- of the state-3 slide it falls into. The backward branch moves the box
-- cursor immediately; the forward one only advances it when the slide ends,
-- which keeps the drawn list and the swapped slot in step.
local function start_slide(direction)
    if direction > 0 then
        s.slide = 0
        s.slide_dir = 1
    else
        s.slide = SLIDE_WRAP
        s.slide_dir = -1
        s.box_cursor = previous_slot(s.box_cursor)
    end
    advance_slide()
end

local function browse_input(api, input)
    if input.cancel then
        api:exit()
    elseif input.confirm then
        if cursor_on_tab() then
            api:exit()
            return
        end
        s.mode = "armed"
        -- The arm resets the blink, so the armed cursor draws its dim frame
        -- from the confirm frame on.
        s.cursor_blink = 0
    elseif input.left or input.right then
        -- The original ignores left/right on the tab row (its
        -- `(cursor & 0xF8) == 0` guard); the blink still resets and the cue
        -- still plays.
        if not cursor_on_tab() then
            s.player_cursor = Grid.move_sideways(s.player_cursor)
        end
        s.cursor_blink = 0
    elseif input.up then
        s.player_cursor = move_up(s.player_cursor)
        s.cursor_blink = 0
    elseif input.down then
        s.player_cursor = move_down(s.player_cursor)
        s.cursor_blink = 0
    end
end

local function armed_input(api, input)
    if input.cancel then
        s.mode = "browse"
    elseif input.up or input.page_left then
        -- The original's state 2 pages the box grid with the raw D-pad
        -- up/down; L1/R1 alias them here. Down/R1 is next, up/L1 previous.
        start_slide(-1)
    elseif input.down or input.page_right then
        start_slide(1)
    elseif input.confirm then
        local box_id = api:item_box(s.box_cursor)
        if box_id == 0 and selected_item(api) == 0 then
            return
        end
        local slot = player_slot()
        if slot == nil then
            return
        end
        api:set("swap", true)
        api:set("swap_box", s.box_cursor)
        api:set("swap_slot", slot)
        s.mode = "browse"
    end
end

function M.update(api, input)
    api:set("swap", false)
    -- The tick: `DAT_00ae9f18` counts down only in the browse state (state
    -- 1); the armed and sliding states (2/3) leave it alone, which is why an
    -- armed box keeps drawing the cursor's dim frame. The tick sits before
    -- this frame's input, so the frame the page key was pressed on already
    -- draws the slide's first step.
    if s.mode == "browse" then
        s.cursor_blink = (s.cursor_blink - 1) & 0xFF
    end
    if s.slide_dir ~= 0 then
        advance_slide()
    end

    if s.mode == "browse" then
        browse_input(api, input)
    elseif s.slide_dir == 0 then
        -- The original's state 3 (a slide is running) reads no buttons at
        -- all: the confirm that lands mid-slide is dropped, which is why a
        -- page-then-confirm sequence swaps the slot the slide landed on.
        armed_input(api, input)
    end
    api:set("cursor", drawn_cursor())
    api:set("on_slot", not Grid.is_tab(s.player_cursor))
end

-- Drawing ---------------------------------------------------------------

local function origin(api)
    return api:width() // 2 - 160, api:height() // 2 - 120
end

local function draw_part(api, page, ox, oy, part)
    local x, y, w, h, u, v = part[1], part[2], part[3], part[4], part[5], part[6]
    api:page_sprite_mirrored(page, u, v, w, h, ox + x, oy + y, w, h, 0, 0, false, false)
end

local function shadow(api, ox, oy, rect)
    api:blend_black_rect(ox + rect[1], oy + rect[2], rect[3], rect[4], SHADOW_ALPHA)
end

-- The item name list ending at the box cursor: three rows normally, four
-- while a page slide runs (the extra row fills the gap the list leaves).
-- Rows whose slot holds nothing print `_Nothing_` through the 8x14 renderer
-- in its grey tint.
local function draw_names(api, ox, oy)
    local slot = previous_slot(s.box_cursor)
    local rows = (s.slide_dir ~= 0) and 4 or 3
    for row = 0, rows - 1 do
        local id = api:item_box(slot)
        -- The list slides up one pixel per slide step.
        local y = NAME_Y + row * NAME_STEP - s.slide
        if id == 0 then
            api:text_8x14(ox + NAME_X, oy + y, NOTHING_LABEL, { colour = "grey" })
        else
            local name = api:item_name_bytes(id, true)
            if name ~= nil then
                api:glyphs(ox + NAME_X, oy + y, name, { brightness = 0, fit = NAME_MAX_WIDTH })
            end
        end
        slot = next_slot(slot)
    end
end

-- The preview slot: the box cursor's 40x30 icon with its quantity, plus the
-- incoming slot during a page slide. The current slot's icon samples two
-- pixels further down the sheet per slide step inside a window that shrinks
-- from the bottom, so it scrolls up and out; the next slot slides up from
-- 30px below into place. Both quantities ride up with their slots.
local function draw_slot(api, ox, oy)
    local source_v = s.slide * 2
    local qty_y = (0x2C - s.slide) * 2
    local slot = s.box_cursor
    local y = SLOT_Y
    local height = SLOT_H - source_v
    local count = (s.slide_dir ~= 0) and 2 or 1
    for _ = 1, count do
        -- Rows below the panel's lower mask are never painted.
        local visible = height
        if SLOT_CLIP_BOTTOM - y < visible then
            visible = SLOT_CLIP_BOTTOM - y
        end
        local id, quantity = api:item_box(slot)
        if visible > 0 then
            if id == 0 then
                Slot.draw_empty(api, ox + SLOT_X, oy + y, visible)
            else
                Slot.draw_icon(api, id, ox + SLOT_X, oy + y, source_v, visible)
            end
        end
        if id ~= 0 then
            -- The box's quantity is drawn from `screenX - 2`, two pixels left
            -- of the inventory panel's own quantity.
            Slot.draw_quantity(api, id, quantity, ox + SLOT_X - 2, oy + qty_y,
                oy + SLOT_CLIP_BOTTOM)
        end
        source_v = 0
        y = qty_y + SLOT_H
        qty_y = qty_y + SLOT_H
        height = SLOT_H
        slot = next_slot(slot)
    end
end

-- The "box is full" warning line under the name list. The original's test
-- is `0x2C < (unsigned char)(cursor - 1)`, so the byte underflow draws it on
-- the first slot as well as the last two, one row per slide step higher.
local function draw_full_line(api, ox, oy)
    if ((s.box_cursor - 1) & 0xFF) < 0x2D then
        return
    end
    local cursor_term = 0
    if s.box_cursor ~= 0 then
        cursor_term = (0x30 - s.box_cursor) * 0x0F
    end
    local y = FULL_LINE_Y - s.slide + cursor_term
    api:fill_rect(ox + FULL_LINE_X, oy + y, FULL_LINE_END_X - FULL_LINE_X, 1,
        FULL_LINE_COLOR[1], FULL_LINE_COLOR[2], FULL_LINE_COLOR[3])
end

-- The three page-position ticks following the box cursor. A backward slide
-- starts one slot further on.
local function draw_ticks(api, page, ox, oy)
    local tick = previous_slot(s.box_cursor)
    if s.slide_dir < 0 then
        tick = (tick + 1) & 0xFF
    end
    for _ = 1, 3 do
        if tick >= SLOTS_TOTAL then
            tick = 0
        end
        for x = 0, 5 do
            api:page_sprite_mirrored(page, 0x40 + x, 0x38, 1, 1, ox + TICK_X + x, oy + TICK_Y + tick,
                1, 1, 0, 0, false, false)
        end
        tick = tick + 1
    end
end

-- The box's own cursor frame (`itembox_draw_cursor`): a 40x30 frame with a
-- bright cell (`texU 0x80`) and the dim blink phase (`0xA8`). A tab cursor is
-- left to the menu's tab highlight.
local function draw_cursor(api, ox, oy)
    if cursor_on_tab() then
        return
    end
    local x, y = Grid.cursor_position(s.player_cursor, s.slots)
    if x == nil then
        return
    end
    local tex_u = (s.cursor_blink & CURSOR_DIM_BIT == 0) and 0xA8 or 0x80
    api:page_sprite_mirrored(api:page(STATUS_STEM), tex_u, 0xE0, 40, 30, ox + x, oy + y, 40, 30,
        0, 0, false, false)
end

-- The layer order is the original's pending-sprite depth, not its submission
-- order: the leading frame pieces, the name rows, the full-box line and the
-- preview icon/quantity all sort behind the two 50% shadow rects, which sort
-- behind the bright frame lines and the near masks and scrollbar tracks.
-- Drawing the names and the icon before the shadows is what dims the rows
-- above and below the selected one while the selected row stays bright.
function M.draw(api)
    local ox, oy = origin(api)
    -- A pack missing the box or menu art leaves the overlay undrawn.
    local page = api:page(ITEMBOX_STEM)
    if page == nil or not Slot.art_present(api) then
        return
    end
    for _, part in ipairs(FRAME_PARTS_C) do
        draw_part(api, page, ox, oy, part)
    end

    draw_names(api, ox, oy)
    draw_slot(api, ox, oy)
    draw_full_line(api, ox, oy)

    -- Every shadow composites over the box-full line, which is what the
    -- reference's frames show when the line is drawn on the first slot.
    if s.mode == "browse" then
        shadow(api, ox, oy, SHADOW_LIST)
        shadow(api, ox, oy, SHADOW_SLOT)
    end
    shadow(api, ox, oy, SHADOW_TOP)
    shadow(api, ox, oy, SHADOW_BOTTOM)
    -- The bright frame pieces draw over the shadows so they stay green.
    for _, part in ipairs(FRAME_PARTS_A) do
        draw_part(api, page, ox, oy, part)
    end
    draw_ticks(api, page, ox, oy)
    -- The scrollbar tracks keep the original's grey `0x70` "fade to black"
    -- level, so they darken the panel behind them rather than erasing it.
    shadow(api, ox, oy, TRACK_TOP)
    shadow(api, ox, oy, TRACK_SIDE)
    for _, mask in ipairs(FRAME_MASKS) do
        api:fill_rect(ox + mask[1], oy + mask[2], mask[3], mask[4], 0, 0, 0)
    end

    -- The interaction draws the cursor after its own overlay (pending depth
    -- 5: nearer than the frame pieces but behind the masks), withheld until
    -- the menu's first input frame like the menu's own.
    if s.cursor_shown then
        draw_cursor(api, ox, oy)
    end
end

return M
