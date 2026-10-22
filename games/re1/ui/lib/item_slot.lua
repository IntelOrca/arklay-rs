-- The pause menu's 40x30 item slot: an item's icon and its quantity digits.
--
-- Regular items (ids up to `0x6E`) sample `item/item_all`'s 40x30 rows; the
-- Ingram and Minimi specials sample `ui/staitem`. The quantity digits and the
-- infinity mark come from `ui/status`. Positions are screen coordinates; the
-- callers add their own origin.

local Slot = {}

--- The highest regular item id; higher ids are the Ingram/Minimi specials.
Slot.MAX_ITEM = 0x6E
--- The slot (and icon) size.
Slot.W, Slot.H = 40, 30

local STATUS_STEM = "ui/status"
local BLUE_STEM = "ui/blue"
local STAITEM_STEM = "ui/staitem"
local ITEM_ALL_STEM = "item/item_all"

--- Whether every sheet the slots draw from is in the pack.
function Slot.art_present(api)
    return api:page(STATUS_STEM) ~= nil and api:page(BLUE_STEM) ~= nil
        and api:page(STAITEM_STEM) ~= nil
        and api:picture(ITEM_ALL_STEM, 0x000000, "shipped") ~= nil
end

--- An empty slot's plate (`ui/blue`), `height` rows of it.
function Slot.draw_empty(api, x, y, height)
    height = height or Slot.H
    api:page_sprite_mirrored(api:page(BLUE_STEM), 0, 0, Slot.W, height, x, y, Slot.W, height,
        0, 0, false, false)
end

--- One 40-wide slice of an item's icon: `height` rows from `source_v` rows
--- into its 40x30 cell (the whole icon by default).
function Slot.draw_icon(api, item, x, y, source_v, height)
    source_v = source_v or 0
    height = height or Slot.H
    if height <= 0 then
        return
    end
    if item <= Slot.MAX_ITEM then
        local row = api:item_icon_row(item)
        if row == nil then
            return
        end
        local atlas = api:picture(ITEM_ALL_STEM, 0x000000, "shipped")
        -- The PC game uploads each icon as a texture of its own, so its HD
        -- replacement is found by the icon's cell.
        api:picture_sprite_scaled(atlas, 0, row * Slot.H + source_v, Slot.W, height,
            x, y, Slot.W, height, 255, { 0, row * Slot.H, Slot.W, Slot.H })
    else
        api:page_sprite_mirrored(api:page(STAITEM_STEM), 0, (item - 0x6F) * Slot.H + source_v,
            Slot.W, height, x, y, Slot.W, height, 0, 0, false, false)
    end
end

--- A stack's quantity digits under its icon at `(x, y)`, clipped above
--- `clip_bottom` (exclusive, unclipped by default): ammunition-like items
--- only, the infinite mark for the specials and the unlimited-ammo rocket
--- launcher.
function Slot.draw_quantity(api, item, quantity, x, y, clip_bottom)
    local quantifiable = (item < 0x13 and item ~= 0x01) or item == 0x2F or item > Slot.MAX_ITEM
    if not quantifiable then
        return
    end
    local infinite = item > Slot.MAX_ITEM or (item == 0x0A and api:flag(0, 0x7E))
    y = y + 0x14
    local height = 8
    if clip_bottom ~= nil and clip_bottom - y < height then
        height = clip_bottom - y
    end
    if height <= 0 then
        return
    end
    local status = api:page(STATUS_STEM)
    if infinite then
        api:page_sprite_mirrored(status, 152, 112, 10, height, x + 6, y, 10, height,
            0, 0, false, false)
        return
    end
    if item ~= 0x06 and item < 0x0B then
        quantity = quantity & 0x7F
    end
    local digits = { quantity // 100, (quantity // 10) % 10, quantity % 10 }
    local column = 0x80
    if item == 4 or item == 8 or item == 0x0D or item == 0x11 then
        column = 0x88
    elseif item == 7 or item == 0x12 then
        column = 0xC0
    end
    local cursor = x + ((item < 0x0B) and 4 or 0x0E)
    local drawn = false
    for index = 1, 3 do
        local digit = digits[index]
        if digit ~= 0 or drawn or index == 3 then
            api:page_sprite_mirrored(status, column, digit * 8, 8, height, cursor, y,
                8, height, 0, 0, false, false)
            drawn = true
        end
        if drawn or item > 0x0A then
            cursor = cursor + 8
        end
    end
end

return Slot
