-- The pause menu's status/inventory screen and its USE/CHECK/MOVE submenu.
--
-- This is menu mode 0: the status panel (portrait, health face, EKG) plus
-- the inventory grid, its cursor and the item action submenu. The engine
-- owns the pad mapping, the message window, the fades and the sub-screens
-- (map, file, item box, item viewer); this script owns the cursor, the
-- submenu and every draw.
--
-- The engine drives it through:
--
-- * `open(api)`: the menu opens on the current inventory;
-- * `tick(api)`: one frame of the animations and of a deferred use, also
--   run under a message and the sub-screens (the original keeps drawing the
--   health monitor, and drawing from the shared `rand()`, there);
-- * `update(api, input)`: one frame's input, with at most one of the pad
--   edges set plus `start`;
-- * `view_closed(api)`, `reselect(api)`, `move_cursor(api, cursor)`: the
--   item viewer closed, re-read the item under the cursor, follow the item
--   box's cursor;
-- * `present(api, show_cursor, show_name)`: what the next draw shows.
--
-- After `tick` and `update` the engine reads `event` (and `event_id`):
-- `1` close the menu (the engine plays the exit's cancel cue), `2` close it
-- silently, `3` start the item viewer on item `event_id`, `4` run
-- top tab `event_id`. Messages go straight to the game's window.

local Grid = require("lib/menu_grid")
local Panel = require("lib/status_panel")
local Slot = require("lib/item_slot")
local Bar = require("lib/health_bar")

local M = {}

local EVENT_NONE, EVENT_CLOSE, EVENT_CLOSE_SILENT, EVENT_VIEW, EVENT_TAB = 0, 1, 2, 3, 4

-- Which part of the screen owns the cursor.
local NAVIGATION, ACTIONS, MOVE = 0, 1, 2

-- The action submenu: its box grows over this many frames.
local SUBMENU_OPEN_FRAMES = 8
local SUBMENU_X, SUBMENU_Y = 0x90, 0x39
local OPTION_W, OPTION_H = 0x30, 0x18

-- Global messages the actions show. 0xF5 is the original's yes/no prompt
-- ("Do you want to mix the herbs?") and 0xF6 the no-recipe refusal.
local MESSAGE_NEEDS_DRUG_STORE = 0xF2
local MESSAGE_MIX_PROMPT = 0xF5
local MESSAGE_NO_RECIPE = 0xF6
local MESSAGE_USE_REFUSED = 0xF7

local EQUIPPED_X, EQUIPPED_Y = 0xA0, 0x92
local PORTRAIT_X, PORTRAIT_Y, PORTRAIT_SIZE = 0x16, 0x92, 30
local ITEM_NAME_Y = 0xBA

local s = {}
M.state = s

-- Whether an item takes the equip action instead of use.
local function is_equippable(item)
    return not (item >= 0x0B and item <= 0x6E)
end

local function emit(api, kind, id)
    api:set("event", kind)
    api:set("event_id", id or 0)
end

-- Publish what the engine (and its tests) read back.
local function publish(api)
    api:set("cursor", s.cursor)
    api:set("slot", Grid.slot_of_cursor(s.cursor) or -1)
    api:set("mode", s.mode)
    api:set("selected_item", s.selected_item)
    api:set("ready", s.frames >= SUBMENU_OPEN_FRAMES)
    api:set("closing", s.closing)
    api:set("viewing", s.view ~= nil)
    api:set("monitor", s.bar.state)
    local left, right = Bar.primary_span(s.bar)
    api:set("monitor_left", left)
    api:set("monitor_right", right)
end

-- Re-read the item under the cursor and mirror it into the state byte the
-- scripts observe.
local function refresh_selection(api)
    local item = 0
    local slot = Grid.slot_of_cursor(s.cursor)
    if slot ~= nil then
        item = api:inventory(slot)
    end
    s.selected_item = item
    s.equippable = item ~= 0 and is_equippable(item)
    api:select_item(item)
end

local function blink()
    s.blink = (s.blink - 1) & 0xFF
end

function M.open(api, _args)
    -- Drawn on black: a wider aspect gives the panel a wider canvas.
    api:set("canvas", 1)
    s.slots = api:inventory_capacity()
    s.cursor = Grid.FIRST_SLOT_CURSOR
    s.mode = NAVIGATION
    s.action = 0
    s.move_cursor = s.cursor
    s.selected_item = 0
    s.equippable = false
    s.bar = Bar.new()
    -- The submenu's open animation frame: 0 on the confirm frame, then up
    -- to SUBMENU_OPEN_FRAMES as the box grows; it accepts input once open.
    s.frames = SUBMENU_OPEN_FRAMES
    -- A USE confirmed on the previous frame, run at the top of the next
    -- tick (the original's submenu state 4).
    s.pending_use = false
    -- The submenu's close animation is running.
    s.closing = false
    -- The whole menu exits when the close animation ends.
    s.close_after = false
    -- The close shrinks toward the bottom-right (an action closed it)
    -- rather than the top-left (a cancel).
    s.from_action = false
    -- The CHECK slide: nil, "out", "viewing" or "back".
    s.view = nil
    s.view_item = 0
    s.view_slide = 0
    -- A herb mix waiting on the reference's 0xF5 yes/no prompt: the slots to
    -- combine once the player answers Yes.
    s.pending_combine = nil
    -- A refusal message that closes the submenu when it dismisses.
    s.pending_close = false
    -- The message task runs after the menu task in the original, so the
    -- answer reaches the menu one tick after the window hides it.
    s.pending_answer = false
    -- The original retracts the MOVE cursor for four ticks before the
    -- submenu's own eight-frame close starts.
    s.move_close_delay = 0
    -- The cursor blink counter: bit 0x20 draws the bright cursor.
    s.blink = 0
    s.show_cursor = true
    s.show_name = true
    refresh_selection(api)
    emit(api, EVENT_NONE)
    publish(api)
end

local function use_selected(api)
    if s.selected_item == 0 then
        return
    end
    local used, healed, cured, closes = api:use_item(Grid.slot_of_cursor(s.cursor) or 0)
    if used == nil then
        return
    end
    if used then
        Bar.start_flush(s.bar, healed, cured)
        refresh_selection(api)
        -- A successful use always runs the submenu's close animation; a
        -- key/special/book item also exits the whole menu after it.
        s.closing = true
        s.from_action = true
        s.close_after = closes
    else
        -- A refusal returns the use category in place of `healed`.
        local category = healed
        api:show_message(MESSAGE_USE_REFUSED + category)
    end
end

local function step_view_back()
    s.view_slide = s.view_slide - 1
    if s.view_slide < 0 then
        s.view_slide = 0
    end
    if s.view_slide == 0 then
        s.view = nil
    end
end

--- Resolve a message the MOVE confirm opened.
---
--- The engine runs the message window before the menu's tick, so the choice
--- byte has already changed on the frame the player answers. 0xF5 is the
--- reference's yes/no prompt: Yes applies the mix and closes the submenu,
--- No returns to the move cursor. 0xF2's refusal message closes the submenu,
--- and 0xF6's no-recipe message leaves the move cursor up; both dismiss
--- through the game's message window.
local function resolve_pending(api)
    local choice = api:message_choice()
    if choice & 0x80 ~= 0 then
        return
    end
    -- The original's menu task runs before the message task, so the answer
    -- the message task resolved this tick reaches the menu on the next one.
    if not s.pending_answer then
        s.pending_answer = true
        return
    end
    if s.pending_close then
        s.pending_close = false
        s.pending_answer = false
        s.closing = true
        s.from_action = true
        return
    end
    if s.pending_combine == nil then
        return
    end
    local pending = s.pending_combine
    s.pending_combine = nil
    s.pending_answer = false
    if choice & 1 == 0 then
        api:sound("decide")
        api:combine(pending.slot, pending.target)
        refresh_selection(api)
        s.move_close_delay = 4
    else
        api:sound("cancel")
    end
end

--- One frame: a deferred use first (the original's input runs before the
--- health monitor), the CHECK slide, the submenu animation, the monitor.
function M.tick(api)
    emit(api, EVENT_NONE)
    if s.pending_use then
        s.pending_use = false
        use_selected(api)
    end
    if s.view == "out" then
        -- The options step right each frame; the eighth step hides them
        -- and the viewer starts on the same frame.
        s.view_slide = s.view_slide + 1
        if s.view_slide & 8 ~= 0 then
            s.view = "viewing"
            emit(api, EVENT_VIEW, s.view_item)
        end
    elseif s.view == "back" then
        step_view_back()
    end
    local started_close = false
    if s.move_close_delay > 0 then
        s.move_close_delay = s.move_close_delay - 1
        if s.move_close_delay == 0 then
            s.closing = true
            s.from_action = true
            started_close = true
        end
    end
    if s.closing and not started_close then
        -- The close walks the open sequence backwards; the cursor returns
        -- to the inventory when the box is gone.
        if s.frames > 0 then
            s.frames = s.frames - 1
        end
        if s.frames == 0 then
            s.closing = false
            s.mode = NAVIGATION
            refresh_selection(api)
            if s.close_after then
                s.close_after = false
                emit(api, EVENT_CLOSE)
            end
        end
    elseif s.mode == NAVIGATION then
        refresh_selection(api)
    elseif s.frames < SUBMENU_OPEN_FRAMES then
        s.frames = s.frames + 1
    end
    Bar.update(s.bar, api)
    resolve_pending(api)
    publish(api)
end

--- The viewer finished closing: the options start sliding back.
function M.view_closed(api)
    if s.view == "viewing" then
        s.view = "back"
        step_view_back()
    end
    publish(api)
end

--- Re-read the item under the cursor (the viewer may rewrite its slot).
function M.reselect(api)
    refresh_selection(api)
    publish(api)
end

--- Follow the item box's copy of the inventory cursor.
function M.move_cursor(api, cursor)
    s.cursor = cursor
    if s.mode == NAVIGATION then
        refresh_selection(api)
    end
    publish(api)
end

--- What the next draw shows: the cursors, and the item-name line.
function M.present(_api, show_cursor, show_name)
    s.show_cursor = show_cursor
    s.show_name = show_name
end

-- START: the next top tab (map, file, radio, exit), skipping the radio
-- without the radio.
local function cycle_tab(api)
    local next = 0
    if Grid.is_tab(s.cursor) then
        next = (s.cursor + 2) & 7
    end
    if next == 4 and not api:has_radio() then
        next = 6
    end
    s.cursor = next
    s.mode = NAVIGATION
    refresh_selection(api)
    return next
end

-- The frame's one pad input, by the engine's priority.
local function single(input)
    for _, name in ipairs({ "up", "down", "left", "right", "page_left", "page_right", "confirm", "cancel" }) do
        if input[name] then
            return name
        end
    end
    return nil
end

local function navigation_input(api, key)
    if key == "left" or key == "right" then
        s.cursor = Grid.move_sideways(s.cursor)
    elseif key == "up" then
        s.cursor = Grid.move_up(s.cursor, s.slots)
    elseif key == "down" then
        s.cursor = Grid.move_down(s.cursor, s.slots)
    elseif key == "cancel" then
        -- The close plays the state-1 exit's cancel cue; the navigation
        -- cancel has none of its own.
        emit(api, EVENT_CLOSE)
        return false
    elseif key == "page_left" or key == "page_right" then
        blink()
        return true
    elseif key == "confirm" then
        if Grid.is_tab(s.cursor) then
            s.blink = 0
            if s.cursor == 6 then
                -- `menu_tab_exit` closes with no cue after the decide.
                emit(api, EVENT_CLOSE_SILENT)
            else
                emit(api, EVENT_TAB, s.cursor)
            end
            return true
        end
        if s.selected_item == 0 then
            -- The refused confirm counts the blink down and stays silent.
            blink()
            return false
        end
        s.blink = 0
        s.mode = ACTIONS
        s.action = 0
        s.move_cursor = s.cursor
        s.equippable = is_equippable(s.selected_item)
        -- The submenu box opens from nothing.
        s.frames = 0
        return true
    end
    refresh_selection(api)
    s.blink = 0
    return true
end

local function apply_action(api)
    local item = s.selected_item
    if s.action == 0 and s.equippable then
        if api:equipped() == item then
            api:equip(0)
        else
            api:equip(item)
        end
        -- The equip toggle closes the submenu through its animation.
        s.closing = true
        s.from_action = true
    elseif s.action == 0 then
        s.pending_use = true
    elseif s.action == 1 then
        s.view = "out"
        s.view_item = item
        s.view_slide = 0
    elseif s.action == 2 then
        s.mode = MOVE
        s.move_cursor = s.cursor
    end
end

local function action_input(api, key)
    if key == "cancel" then
        -- The cancel's close animation shrinks toward the top-left, and its
        -- first step lands on this frame.
        s.closing = true
        s.from_action = false
        if s.frames > 0 then
            s.frames = s.frames - 1
        end
    elseif key == "up" then
        s.action = (s.action + 2) % 3
    elseif key == "down" then
        s.action = (s.action + 1) % 3
    elseif key == "confirm" then
        apply_action(api)
    end
end

local function move_input(api, key)
    if key == "cancel" then
        -- Back to the already-open submenu, without its open animation.
        s.mode = ACTIONS
        s.frames = SUBMENU_OPEN_FRAMES
    elseif key == "left" or key == "right" then
        s.move_cursor = Grid.move_sideways(s.move_cursor)
    elseif key == "up" then
        s.move_cursor = Grid.move_up(s.move_cursor, s.slots)
    elseif key == "down" then
        s.move_cursor = Grid.move_down(s.move_cursor, s.slots)
    elseif key == "confirm" then
        if s.move_cursor == s.cursor then
            return
        end
        local slot = Grid.slot_of_cursor(s.cursor)
        local target = Grid.slot_of_cursor(s.move_cursor)
        if slot == nil or target == nil then
            return
        end
        -- The original's `menu_item_check_combine`: ordinary recipes apply
        -- on the confirm and close the submenu; a herb mix only opens the
        -- 0xF5 yes/no prompt, and the MOVE confirm applies it after Yes.
        local result = api:combine_check(slot, target)
        if result == "applied" then
            refresh_selection(api)
            s.move_close_delay = 4
        elseif result == "herb" then
            s.pending_combine = { slot = slot, target = target }
            s.pending_answer = false
            api:show_message(MESSAGE_MIX_PROMPT)
        elseif result == "drug_store" then
            -- The refusal message closes the submenu when it dismisses.
            s.pending_close = true
            s.pending_answer = false
            api:show_message(MESSAGE_NEEDS_DRUG_STORE)
        elseif result == "herb_no_recipe" then
            -- Two herbs with no recipe; every other pair is refused
            -- silently.
            api:show_message(MESSAGE_NO_RECIPE)
        end
    end
end

-- The cue a pad input plays.
local CUES = {
    up = "cursor", down = "cursor", left = "cursor", right = "cursor",
    confirm = "decide", cancel = "cancel",
}

function M.update(api, input)
    emit(api, EVENT_NONE)
    -- The action submenu ignores all input while its open or close
    -- animation runs.
    if s.mode == ACTIONS and (s.frames < SUBMENU_OPEN_FRAMES or s.closing) then
        publish(api)
        return
    end
    -- START cycles the top tabs.
    if input.start then
        emit(api, EVENT_TAB, cycle_tab(api))
        publish(api)
        return
    end
    local key = single(input)
    if key == nil then
        -- An idle navigation frame counts the cursor blink down.
        if s.mode == NAVIGATION then
            blink()
        end
        publish(api)
        return
    end
    local cue = CUES[key]
    if s.mode == NAVIGATION then
        if not navigation_input(api, key) then
            -- A confirm that did nothing (an empty slot) stays silent, as
            -- does the cancel, whose cue the close plays.
            cue = nil
        end
    elseif s.closing then
        -- The close animation swallows the input.
    elseif s.mode == ACTIONS then
        action_input(api, key)
    else
        move_input(api, key)
    end
    if cue ~= nil then
        api:sound(cue)
    end
    publish(api)
end

-- ---- drawing -----------------------------------------------------------

-- The USE/EQUIP, CHECK and MOVE options, growing from nothing at their
-- corner: each open frame samples a 6px-wider, 3px-taller region of the
-- option art. A cancel shrinks the box back the same way; a close after an
-- action shrinks it toward the bottom-right, sampling the art's corner.
local function draw_submenu(api, page, ox, oy)
    local labels = s.equippable and { 0x00, 0x30, 0x48 } or { 0x18, 0x30, 0x48 }
    local frame = s.frames
    if frame > SUBMENU_OPEN_FRAMES then
        frame = SUBMENU_OPEN_FRAMES
    end
    local w = frame * 6
    if w > OPTION_W then
        w = OPTION_W
    end
    local h = frame * 3
    if h > OPTION_H then
        h = OPTION_H
    end
    if w <= 0 or h <= 0 then
        return
    end
    local shrink = 8 - frame
    local x, y, u, dv = SUBMENU_X, SUBMENU_Y, 0x30, 0
    if s.closing and s.from_action then
        x, y, u, dv = SUBMENU_X + shrink * 6, (0x1B - frame) * 3, 0x30 + shrink * 6, shrink * 3
    end
    if s.frames >= SUBMENU_OPEN_FRAMES and not s.closing and s.view_slide == 0 then
        -- The selected action's highlight behind the option list.
        api:page_sprite_mirrored(page, 0x30, 0x60, OPTION_W, OPTION_H, ox + x,
            oy + y + s.action * OPTION_H, OPTION_W, OPTION_H, 0, 0, false, false)
    end
    for option = 0, 2 do
        -- The CHECK slide staggers the options: each trails the one above.
        local slide = s.view_slide - option
        if slide < 0 then
            slide = 0
        end
        api:page_sprite_mirrored(page, u, labels[option + 1] + dv, w, h,
            ox + x + slide * 8, oy + y + option * OPTION_H, w, h, 0, 0, false, false)
    end
end

-- The top-tab highlight, drawn behind the tab art so the tab's transparent
-- background reveals it.
local function draw_tab_cursor(api, page, ox, oy)
    if s.mode ~= NAVIGATION or not Grid.is_tab(s.cursor) then
        return
    end
    local x, y = Grid.cursor_position(s.cursor, s.slots)
    if x == nil then
        return
    end
    api:page_sprite_mirrored(page, 0, 0x50, 48, 16, ox + x, oy + y, 48, 16, 0, 0, false, false)
end

local function draw_stack(api, item, quantity, x, y)
    if item == 0 then
        Slot.draw_empty(api, x, y)
    else
        Slot.draw_icon(api, item, x, y)
        Slot.draw_quantity(api, item, quantity, x, y)
    end
end

local function draw_inventory(api, ox, oy)
    for slot = 0, s.slots - 1 do
        local x, y = Grid.slot_position(s.slots, slot)
        if x ~= nil then
            local item, quantity = api:inventory(slot)
            draw_stack(api, item, quantity, ox + x, oy + y)
        end
    end
    local equipped = api:equipped()
    local count = api:item_count(equipped)
    if count > 0xFF then
        count = 0xFF
    end
    draw_stack(api, equipped, count, ox + EQUIPPED_X, oy + EQUIPPED_Y)
end

local function draw_portrait(api, ox, oy)
    local character = api:character()
    api:page_sprite_mirrored(api:page("ui/statface"), (character & 1) * 32, (character & 2) * 16,
        32, 32, ox + PORTRAIT_X, oy + PORTRAIT_Y, PORTRAIT_SIZE, PORTRAIT_SIZE, 0, 0, false, false)
end

-- The item-slot cursor, the topmost layer. It blinks between the bright and
-- the dim cell; the MOVE cursor is always bright.
local function draw_slot_cursor(api, page, ox, oy)
    local cursor, u = s.cursor, 0xA8
    if s.mode == MOVE then
        cursor, u = s.move_cursor, 0x80
    elseif s.blink & 0x20 ~= 0 then
        u = 0x80
    end
    if Grid.is_tab(cursor) then
        return
    end
    local x, y = Grid.cursor_position(cursor, s.slots)
    if x == nil then
        return
    end
    api:page_sprite_mirrored(page, u, 0xE0, Slot.W, Slot.H, ox + x, oy + y, Slot.W, Slot.H,
        0, 0, false, false)
end

local function draw_item_name(api, ox, oy)
    if s.selected_item == 0 then
        return
    end
    local name = api:item_name_bytes(s.selected_item, true)
    if name == nil then
        return
    end
    api:glyphs(ox + api:font_margin(), oy + ITEM_NAME_Y, name, { brightness = 0 })
end

--- The whole screen. The layers follow the original's depth sort: the
--- submenu behind the border masks (the CHECK slide pushes the options
--- under the right mask), the tab highlight behind the tab art, and the
--- item-slot cursor on top.
function M.draw(api)
    local ox, oy = api:width() // 2 - 160, api:height() // 2 - 120
    local status = api:page(Panel.STATUS_STEM)
    if status == nil or not Slot.art_present(api) or api:page("ui/statface") == nil then
        return
    end
    api:fill_rect(0, 0, api:width(), api:height(), 0, 0, 0)
    if (s.mode ~= NAVIGATION or s.closing) and s.view_slide & 8 == 0 then
        draw_submenu(api, status, ox, oy)
    end
    Panel.draw_masks(api, ox, oy)
    if s.show_cursor then
        draw_tab_cursor(api, status, ox, oy)
    end
    Panel.draw_frames(api, ox, oy, s.slots)
    draw_inventory(api, ox, oy)
    draw_portrait(api, ox, oy)
    Bar.draw(api, s.bar, ox, oy)
    Panel.draw_tabs(api, ox, oy, api:has_radio())
    if s.show_cursor then
        draw_slot_cursor(api, status, ox, oy)
    end
    if s.show_name then
        draw_item_name(api, ox, oy)
    end
end

return M
