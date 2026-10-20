-- The pause menu's shared cursor grid.
--
-- One cursor byte walks both the top tabs and the inventory slots: `0, 2, 4,
-- 6` are the four tabs (two per row) and `8 + 2 * slot` the item slots, two
-- per row. Left/right flip the column (`cursor ^ 2`); up/down step a row and
-- wrap between the tab row and the last slot row. A six-slot character's
-- slots sit on the panel's bottom three rows, an eight-slot one's on all four.

local Grid = {}

local function math_min(a, b)
    if a < b then
        return a
    end
    return b
end

--- The first item-slot cursor value; anything below it is a top tab.
Grid.FIRST_SLOT_CURSOR = 8

-- The four top-tab positions, indexed by tab (`cursor / 2`).
local TAB_POSITIONS = { [0] = { 208, 16 }, { 256, 16 }, { 208, 32 }, { 256, 32 } }
-- The eight item-slot positions in cursor order.
local SLOT_POSITIONS = {
    [0] = { 220, 56 }, { 260, 56 }, { 220, 86 }, { 260, 86 },
    { 220, 116 }, { 260, 116 }, { 220, 146 }, { 260, 146 },
}

--- Whether `cursor` names a top tab.
function Grid.is_tab(cursor)
    return cursor < Grid.FIRST_SLOT_CURSOR
end

--- The inventory slot of a cursor value, or nil for a tab or odd value.
function Grid.slot_of_cursor(cursor)
    if cursor & 1 ~= 0 or cursor < Grid.FIRST_SLOT_CURSOR then
        return nil
    end
    return (cursor - Grid.FIRST_SLOT_CURSOR) // 2
end

--- The cursor value of inventory slot `slot`.
function Grid.cursor_of_slot(slot)
    return Grid.FIRST_SLOT_CURSOR + slot * 2
end

--- Left/right on the 2-wide grid.
function Grid.move_sideways(cursor)
    return cursor ~ 2
end

--- Down one row, wrapping past the last slot to the tab in the same column.
function Grid.move_down(cursor, slots)
    local next = (cursor + 4) & 0xFF
    if next > slots * 2 + 6 then
        return next & 2
    end
    return next
end

--- Up one row, wrapping from the tab row to the last slot row.
function Grid.move_up(cursor, slots)
    if cursor & 0xFC == 0 then
        return (cursor & 2) + slots * 2 + 4
    end
    return cursor - 4
end

--- The screen position of slot `slot` for a character with `slots` slots.
function Grid.slot_position(slots, slot)
    local offset = 8 - math_min(slots, 8)
    local position = SLOT_POSITIONS[slot + offset]
    if position == nil then
        return nil
    end
    return position[1], position[2]
end

--- The screen position of `cursor`, whether a tab or a slot; nil for an odd
--- value or a slot past the character's last.
function Grid.cursor_position(cursor, slots)
    if cursor & 1 ~= 0 then
        return nil
    end
    if cursor < Grid.FIRST_SLOT_CURSOR then
        local tab = TAB_POSITIONS[math_min(cursor // 2, 3)]
        return tab[1], tab[2]
    end
    return Grid.slot_position(slots, Grid.slot_of_cursor(cursor))
end

return Grid
