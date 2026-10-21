-- The pause menu's status/inventory panel frame (`ui/status`).
--
-- The original packs every frame part, border, decoration and black mask of
-- the panel into one 712-byte block and walks each region backwards. The
-- tables below list the decoded entries in that draw order (last record
-- first). Positions are 320x240 screen coordinates; every draw takes the
-- caller's origin.

local Panel = {}

Panel.STATUS_STEM = "ui/status"

-- The 22 plain frame parts (main panel, health/portrait and equipped-weapon
-- frames, bottom decorations). `{x, y, w, h, u, v}`.
local FRAME_PARTS = {
    { 120, 216, 200, 16, 0x00, 0xA8 },
    { 0, 216, 120, 16, 0x00, 0x88 },
    { 192, 182, 128, 2, 0x00, 0xA6 },
    { 0, 182, 192, 2, 0x00, 0xA6 },
    { 304, 176, 8, 8, 0x90, 0x50 },
    { 208, 176, 8, 8, 0x88, 0x50 },
    { 300, 176, 4, 8, 0x7C, 0x80 },
    { 216, 176, 4, 8, 0x68, 0x80 },
    { 208, 146, 8, 30, 0xB8, 0x2E },
    { 204, 48, 104, 4, 0x00, 0x84 },
    { 204, 12, 104, 4, 0x00, 0x80 },
    { 148, 142, 60, 38, 0x84, 0xBA },
    { 8, 140, 128, 40, 0x00, 0xB8 },
    { 32, 12, 160, 12, 0x00, 0x98 },
    { 192, 20, 8, 112, 0xB0, 0x00 },
    { 8, 20, 24, 112, 0x98, 0x00 },
    { 16, 132, 16, 8, 0x40, 0x78 },
    { 16, 12, 16, 8, 0x30, 0x78 },
    { 192, 132, 8, 8, 0x60, 0x78 },
    { 192, 12, 8, 8, 0x60, 0x70 },
    { 8, 132, 8, 8, 0x58, 0x78 },
    { 8, 12, 8, 8, 0x50, 0x78 },
}

-- The four top tabs in cursor order (map, file, radio, exit).
local TABS = {
    [0] = { 208, 16, 48, 16, 0x00, 0x00 },
    { 256, 16, 48, 16, 0x00, 0x20 },
    { 208, 32, 48, 16, 0x00, 0x10 },
    { 256, 32, 48, 16, 0x00, 0x30 },
}

-- Tiled records: a first tile `{x, y, w, h, u, v}` plus its sprite flags
-- (`0x80` mirror U, `0x40` mirror V) and a count byte (low seven bits the
-- tile count, `0x80` stepping down by the height instead of across).
local COMMON_TILES = {
    { 220, 176, 16, 8, 0x6C, 0x80, 0x00, 0x05 },
    { 304, 16, 4, 16, 0x7C, 0x88, 0x00, 0x82 },
    { 204, 16, 4, 16, 0x78, 0x88, 0x00, 0x82 },
    { 32, 128, 160, 12, 0x00, 0x98, 0x40, 0x01 },
}

-- The inventory border of a six-slot (Chris) character.
local BORDER_6 = {
    { 304, 86, 8, 15, 0x60, 0x60, 0x00, 0x86 },
    { 300, 86, 4, 30, 0x7C, 0x60, 0x00, 0x83 },
    { 216, 86, 4, 30, 0x68, 0x60, 0x00, 0x83 },
    { 208, 86, 8, 15, 0xB8, 0x28, 0x00, 0x84 },
    { 304, 78, 8, 8, 0x80, 0x50, 0x00, 0x01 },
    { 300, 78, 4, 8, 0x7C, 0x58, 0x00, 0x01 },
    { 220, 78, 16, 8, 0x6C, 0x58, 0x00, 0x05 },
    { 216, 78, 4, 8, 0x68, 0x58, 0x00, 0x01 },
    { 208, 78, 8, 8, 0x60, 0x58, 0x00, 0x01 },
}

-- The inventory border of an eight-slot (Jill) character.
local BORDER_8 = {
    { 304, 56, 8, 15, 0x60, 0x60, 0x00, 0x88 },
    { 300, 56, 4, 30, 0x7C, 0x60, 0x00, 0x84 },
    { 216, 56, 4, 30, 0x68, 0x60, 0x00, 0x84 },
    { 208, 56, 8, 15, 0xB8, 0x28, 0x00, 0x86 },
    { 304, 48, 8, 8, 0x80, 0x50, 0x00, 0x01 },
    { 300, 48, 4, 8, 0x7C, 0x58, 0x00, 0x01 },
    { 220, 48, 16, 8, 0x6C, 0x58, 0x00, 0x05 },
    { 216, 48, 4, 8, 0x68, 0x58, 0x00, 0x01 },
    { 208, 48, 8, 8, 0x60, 0x58, 0x00, 0x01 },
}

-- The four black masks around the panel. `{x, y, w, h}`.
local BLACK_RECTS = {
    { 0, 140, 320, 100 },
    { 199, 12, 121, 128 },
    { 0, 12, 9, 128 },
    { 0, 0, 320, 12 },
}

local function draw_part(api, page, ox, oy, part)
    local x, y, w, h, u, v = part[1], part[2], part[3], part[4], part[5], part[6]
    api:page_sprite_mirrored(page, u, v, w, h, ox + x, oy + y, w, h, 0, 0, false, false)
end

local function draw_tile(api, page, ox, oy, tile)
    local x, y, w, h, u, v = tile[1], tile[2], tile[3], tile[4], tile[5], tile[6]
    local flags, count = tile[7], tile[8]
    local mirror_u, mirror_v = flags & 0x80 ~= 0, flags & 0x40 ~= 0
    for index = 0, (count & 0x7F) - 1 do
        local dx, dy = index * w, 0
        if count & 0x80 ~= 0 then
            dx, dy = 0, index * h
        end
        api:page_sprite_mirrored(page, u, v, w, h, ox + x + dx, oy + y + dy, w, h, 0, 0,
            mirror_u, mirror_v)
    end
end

local function border(slots)
    if slots >= 8 then
        return BORDER_8
    end
    return BORDER_6
end

--- The black masks over the screen edges around the panel.
function Panel.draw_masks(api, ox, oy)
    for _, rect in ipairs(BLACK_RECTS) do
        api:fill_rect(ox + rect[1], oy + rect[2], rect[3], rect[4], 0, 0, 0)
    end
end

-- The bottom strip's repeatable slices, `{y, h, u, v}` at an 8-column
-- stretch of the line (y 182) and the two strip halves (y 216): what a
-- wider canvas tiles across the margins either side of the panel.
local STRIP_LEFT = {
    { 182, 2, 0x08, 0xA6 },
    { 216, 16, 0x08, 0x88 },
}
local STRIP_RIGHT = {
    { 182, 2, 0xB0, 0xA6 },
    { 216, 16, 0xB8, 0xA8 },
}
local STRIP_SLICE = 8

local function draw_strip_margins(api, page, ox, oy)
    if ox <= 0 then
        return
    end
    local right = ox + 320
    for _, slice in ipairs(STRIP_LEFT) do
        for x = ox - STRIP_SLICE, -STRIP_SLICE + 1, -STRIP_SLICE do
            api:page_sprite_mirrored(page, slice[3], slice[4], STRIP_SLICE, slice[2],
                x, oy + slice[1], STRIP_SLICE, slice[2], 0, 0, false, false)
        end
    end
    for _, slice in ipairs(STRIP_RIGHT) do
        for x = right, api:width() - 1, STRIP_SLICE do
            api:page_sprite_mirrored(page, slice[3], slice[4], STRIP_SLICE, slice[2],
                x, oy + slice[1], STRIP_SLICE, slice[2], 0, 0, false, false)
        end
    end
end

--- Every frame part, then the common tiles, then the character's border.
--- On a canvas wider than the panel the bottom strip runs on to its edges.
function Panel.draw_frames(api, ox, oy, slots)
    local page = api:page(Panel.STATUS_STEM)
    draw_strip_margins(api, page, ox, oy)
    for _, part in ipairs(FRAME_PARTS) do
        draw_part(api, page, ox, oy, part)
    end
    for _, tile in ipairs(COMMON_TILES) do
        draw_tile(api, page, ox, oy, tile)
    end
    for _, tile in ipairs(border(slots)) do
        draw_tile(api, page, ox, oy, tile)
    end
end

--- Re-draw the frame decorations whose first tile overlaps the screen rect
--- `{x, y, w, h}` (panel coordinates) over a sub-screen's content.
---
--- The original queues the panel's decorations and the map tab's sprites
--- into one ordering table, and the decorations' depth is the nearer one:
--- the panel's right-hand edge (the 8x112 part at (192,20)) is drawn over
--- the map's composite, and its dithered left column lets the map show
--- through every other row. Everything else the panel draws is under the
--- map's sprites, so only the overlapping decorations are re-drawn.
function Panel.draw_frames_over(api, ox, oy, slots, rect)
    local page = api:page(Panel.STATUS_STEM)
    if page == nil then
        return
    end
    local function overlaps(entry)
        return entry[1] < rect[1] + rect[3] and entry[1] + entry[3] > rect[1]
            and entry[2] < rect[2] + rect[4] and entry[2] + entry[4] > rect[2]
    end
    for _, part in ipairs(FRAME_PARTS) do
        if overlaps(part) then
            draw_part(api, page, ox, oy, part)
        end
    end
    for _, tile in ipairs(COMMON_TILES) do
        if overlaps(tile) then
            draw_tile(api, page, ox, oy, tile)
        end
    end
    for _, tile in ipairs(border(slots)) do
        if overlaps(tile) then
            draw_tile(api, page, ox, oy, tile)
        end
    end
end

--- The four top tabs. Without the radio the original swaps the RADIO tab's
--- art for a blank dash (source V `0x40`): `MainMenu.cpp`'s
--- `if (bVar6 == 4 && DAT_00ae9f1f == 0) g_TextureDesc.texV = 0x40`.
function Panel.draw_tabs(api, ox, oy, has_radio)
    local page = api:page(Panel.STATUS_STEM)
    for tab = 0, 3 do
        local part = TABS[tab]
        if tab == 2 and not has_radio then
            part = { part[1], part[2], part[3], part[4], part[5], 0x40 }
        end
        draw_part(api, page, ox, oy, part)
    end
end

return Panel
