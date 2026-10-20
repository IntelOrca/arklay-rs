-- The pause menu's map tab.
--
-- The tab is always available: the original's `menu_tab_map` runs the map
-- state machine without a scenario check (the radio's flag gates only the
-- radio tab). The browser composites the highest available layout's art
-- (`map/map0e`, `map00`, `map0c` or `map0d` - `map_tab_load_map` loads the
-- file of the mask's highest set bit, one page for every browser sprite):
-- the 160x72 base window at (33,32), the four layout markers (the current
-- layout's at full brightness, the others at half), the current area's name
-- row ("Mansion 1F" and friends, 80x16 at (57,104)) and the current floor's
-- number glyph (24x16 at (137,104)), plus the room-step arrows while a
-- neighbouring room is reachable. Every sprite's index-0 texels are
-- transparent. Up/down step the room index inside the current layout (only
-- when the neighbouring area is known), left/right step between the layouts
-- the owned maps unlock. Confirm switches to the full floor-plan view of the
-- current area; cancel returns to the browser and then closes the tab.
--
-- The area/layout/room tables are the built-in values below; a pack that
-- carries `map/tables.bin` (an older conversion or a mod) overrides them.
-- The full-display zoom animation and the objective highlight are deliberate
-- approximations: the zoom is a single swap and the objective rule is
-- transcribed from the scenario flags rather than extracted. A pack without
-- the map art leaves the tab blank with a warning.
--
-- The screen publishes `zoomed` (0/1) for the engine's tests. The room-flags/SCD
-- bit that records a map pick-up is raised by the engine's pick-up rule.

local Panel = require("lib/status_panel")

local M = {}

-- The screen rects the browser's composite covers (the base window's 160x72
-- plus the name and glyph rows below it) and the full display's plan: the
-- status panel's decorations that overlap them are re-drawn on top.
local BROWSER_RECT = { 33, 32, 160, 96 }
local DISPLAY_RECT = { 48, 28, 128, 100 }

local TABLES_ENTRY = "map/tables.bin"
local BLUE_STEM = "map/blue"
-- The four browser floor-plan stems, indexed by layout.
local TAB_STEMS = { [0] = "map/map0d", "map/map0c", "map/map00", "map/map0e" }
-- The eleven full-display floor-plan stems, indexed by area.
local AREA_STEMS = {
    [0] = "map/map01", "map/map02", "map/map03", "map/map04", "map/map05", "map/map06",
    "map/map07", "map/map08", "map/map09", "map/map0a", "map/map0b",
}

-- The flag bank the visited rooms and owned maps live in, and the owned-map
-- bit base (`ROOM_FLAG_MAP_BASE`).
local BANK_ROOM_FLAGS = 8
local ROOM_FLAG_MAP_BASE = 0x7C

-- The browser's base map window (`g_MapBaseDesc`): the 160x72 top-left window
-- of the loaded layout art at (33,32).
local BASE_RECT = { x = 33, y = 32, w = 160, h = 72, u = 0x00, v = 0x00 }
-- The browser's sprite table (`g_MapSprites`), in draw order: the four layout
-- markers, the area-name row, the floor number glyph and the two room-step
-- arrows.
local SPRITES = {
    [0] = { x = 121, y = 32, w = 56, h = 64, u = 0xA0, v = 0x00 },
    { x = 73, y = 32, w = 80, h = 64, u = 0x00, v = 0x48 },
    { x = 49, y = 32, w = 40, h = 64, u = 0x50, v = 0x48 },
    { x = 121, y = 32, w = 28, h = 64, u = 0xC8, v = 0x48 },
    { x = 57, y = 104, w = 80, h = 16, u = 0x78, v = 0x48 },
    { x = 137, y = 104, w = 24, h = 16, u = 0xE8, v = 0x60 },
    { x = 145, y = 96, w = 8, h = 8, u = 0xA0, v = 0x40 },
    { x = 145, y = 120, w = 8, h = 8, u = 0xB0, v = 0x40 },
}

-- The `colorMul` of the browser's sprites, and of the markers of the layouts
-- the player is not standing in.
local MUL_FULL = 0x80
local MUL_HALF = 0x40

-- The full display's plan origin and size.
local DISPLAY_X, DISPLAY_Y, DISPLAY_W, DISPLAY_H = 48, 28, 128, 100
-- The current room's red highlight (`0x001f` BGR555).
local HIGHLIGHT = 0xF80000

local BACKDROP_WARNING = "warning: no map backdrop %s: %s; the map tab stays blank"
local PAGE_WARNING = "warning: no map page %s: %s; the map tab stays blank"
local PLAN_WARNING = "warning: no map plan %s: %s; the map display stays blank"

local s

-- ---- tables ----------------------------------------------------------------

--- The built-in tables, 0-based like the original's arrays.
local function default_tables()
    local function list(...)
        local values = { ... }
        local out = {}
        for index = 1, #values do
            out[index - 1] = values[index]
        end
        return out
    end
    return {
        -- Visited-room bit base per map group (`(stage - 1) % 5`).
        stage_room_offsets = list(0, 32, 63, 82, 100, 0),
        -- Room count per map group.
        room_counts = list(32, 31, 19, 18, 24, 0),
        -- Area per `(layout, room index)`; 0xFF is no room.
        area = list(2, 0, 1, 0xFF, 4, 3, 0xFF, 0xFF, 6, 5, 0xFF, 0xFF, 10, 9, 8, 7),
        -- Layout per area.
        layouts = list(0, 0, 0, 1, 1, 2, 2, 3, 3, 3, 3, 0, 0, 0, 0, 0),
        -- Map group per area.
        groups = list(0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 4, 4, 0, 3, 2, 2),
        -- Room count per layout (the index of the trailing exit room).
        layout_room_count = list(0, 3, 2, 2),
        -- Floor-number offset per layout.
        layout_offset = list(4, 4, 4, 4),
    }
end

-- The `map/tables.bin` layout: the magic, then the seven tables.
local TABLE_FIELDS = {
    { "stage_room_offsets", 6 }, { "room_counts", 6 }, { "area", 16 }, { "layouts", 16 },
    { "groups", 16 }, { "layout_room_count", 4 }, { "layout_offset", 4 },
}
local TABLES_BYTES = 4 + 6 + 6 + 16 + 16 + 16 + 4 + 4

--- Parse `map/tables.bin`; nil and the reason when it is invalid.
function M.parse_tables(data)
    if #data < TABLES_BYTES then
        return nil, ("map table blob is %d bytes, need %d"):format(#data, TABLES_BYTES)
    end
    if data:sub(1, 4) ~= "MAP1" then
        return nil, "map table blob has no MAP1 magic"
    end
    local tables, cursor = {}, 5
    for _, entry in ipairs(TABLE_FIELDS) do
        local name, count = entry[1], entry[2]
        local values = {}
        for index = 0, count - 1 do
            values[index] = data:byte(cursor + index)
        end
        tables[name] = values
        cursor = cursor + count
    end
    return tables
end

local function load_tables(api)
    local data = api:pack_bytes(TABLES_ENTRY)
    if not data then
        -- A converted pack carries no tables entry; the built-ins are the
        -- documented values.
        return default_tables()
    end
    local tables, err = M.parse_tables(data)
    if not tables then
        api:log(("warning: invalid %s: %s; the map tab uses the built-in tables"):format(TABLES_ENTRY, err))
        return default_tables()
    end
    return tables
end

--- The area at a layout's room index (0xFF when the slot is empty).
local function area_at(layout, room)
    return s.tables.area[layout * 4 + room] or 0xFF
end

-- The map index an area's owned bit uses, or nil for the areas that are
-- never "known".
local MAP_INDEX = { [0] = 0, [1] = 1, [3] = 2, [4] = 3, [5] = 4, [6] = 4, [8] = 5, [9] = 5 }

--- Whether the area's map is owned (the `0x7C + index` room-flags bit).
local function area_known(api, area)
    local index = MAP_INDEX[area]
    return index ~= nil and api:flag(BANK_ROOM_FLAGS, ROOM_FLAG_MAP_BASE + index)
end

--- Whether a group's room has been visited.
local function room_visited(api, group, room)
    local base = s.tables.stage_room_offsets[group]
    if base == nil or room >= (s.tables.room_counts[group] or 0) then
        return false
    end
    return api:flag(BANK_ROOM_FLAGS, (base + room) & 0xFF)
end

--- The area a group's visited room belongs to.
local function room_area(group, room)
    if group == 1 then
        return (room < 0x1A or room == 0x1D) and 1 or 2
    elseif group == 2 then
        return (room < 0x06 or room == 0x10) and 3 or 4
    elseif group == 3 then
        return room < 0x0D and 5 or 6
    elseif group == 4 then
        if room < 0x02 or room == 0x16 then
            return 7
        elseif room < 0x05 then
            return 8
        elseif room < 0x13 then
            return 9
        end
        return 10
    end
    return 0
end

--- The area mask from the visited rooms and owned maps.
local function area_mask(api)
    local mask = 0
    for group = 0, 4 do
        for room = 0, s.tables.room_counts[group] - 1 do
            if room_visited(api, group, room) then
                mask = mask | (1 << room_area(group, room))
            end
        end
    end
    for area = 0, 10 do
        if area_known(api, area) then
            mask = mask | (1 << area)
        end
    end
    return mask
end

--- The layout-availability mask from the area mask.
local function layout_mask(api)
    local mask = area_mask(api)
    local layouts = 0
    for area = 0, 10 do
        if mask & (1 << area) ~= 0 then
            layouts = layouts | (1 << s.tables.layouts[area])
        end
    end
    return layouts
end

--- The initial area for a room, transcribed from the original's
--- special-camera cases.
local function initial_area(group, room, cut)
    if group == 0 then
        if room == 0x10 and cut == 0 then
            return 2
        end
        return 0
    elseif group == 1 then
        if room == 0x0F and (cut == 2 or cut == 5) then
            return 1
        elseif room == 0x0C and cut >= 3 and cut < 8 then
            return 0
        end
        return room < 0x1A and 1 or 2
    elseif group == 2 then
        if (room == 0x0B and cut >= 4 and cut < 7) or (room == 0x0F and cut == 6) then
            return 4
        end
        return room < 0x06 and 3 or 4
    elseif group == 3 then
        return room < 0x0D and 5 or 6
    end
    if room == 0x07 and cut < 4 then
        return 9
    elseif room < 0x02 then
        return 7
    elseif room < 0x05 then
        return 8
    elseif room > 0x12 then
        return 10
    end
    return 9
end

--- The area of the room *after* `room`, exactly as the original's
--- `g_MapAreaTable[layout * 4 + room + 1]` read resolves it: past a layout's
--- last slot the read continues into the next layout's first room, and
--- layout 3's lands on the layout table's own first byte (Mansion 1F).
local function next_area(layout, room)
    local index = room + 1
    if index < 4 then
        return area_at(layout, index)
    elseif index == 4 then
        return layout == 3 and 0 or area_at(layout + 1, 0)
    end
    return 0xFF
end

--- The right/left move flags of a layout position: bit 0 = a room to the
--- right exists, bit 1 = a room to the left, both gated by the neighbour's
--- area being reachable.
local function move_flags(api, layout, room)
    local mask = area_mask(api)
    local flags = 0
    local last = s.tables.layout_room_count[layout] or 0
    if (last - room) & 0xFF ~= 1 then
        local next = next_area(layout, room)
        if next ~= 0xFF and (mask & (1 << (next & 0x0F)) ~= 0 or area_known(api, next)) then
            flags = flags | 1
        end
    end
    if room ~= 0 then
        local prev = area_at(layout, room - 1)
        if prev ~= 0xFF and (mask & (1 << (prev & 0x0F)) ~= 0 or area_known(api, prev)) then
            flags = flags | 2
        end
    end
    return flags
end

M.initial_area = initial_area
M.area_at = function(layout, room)
    return area_at(layout, room)
end

-- ---- state -----------------------------------------------------------------

local function room_index()
    return s.room[s.layout]
end

--- Select the area under the current layout position.
local function refresh_area()
    local area = area_at(s.layout, room_index())
    if area ~= 0xFF then
        s.area = area
    end
end

--- Load one page once, warning on the first failure only. `warning` formats
--- the stem and the error.
local function page(api, stem, warning)
    local entry = s.pages[stem]
    if entry == nil then
        local handle, err = api:indexed_page(stem)
        if handle then
            entry = handle
        else
            api:log(warning:format(stem, err))
            entry = false
        end
        s.pages[stem] = entry
    end
    return entry or nil
end

function M.open(api)
    s = {
        tables = load_tables(api),
        -- The current layout (0..3) and the selected room index per layout.
        layout = 0,
        room = { [0] = 1, 1, 1, 3 },
        area = 0,
        -- Whether the full floor plan is shown (the original's display zoom).
        zoomed = false,
        -- Blink phase for the cursor arrows (`MAP_HL_COUNT`).
        ticks = 0,
        -- The original's tab action runs one frame after its confirm, so the
        -- browser stays blank until its first update.
        started = false,
        -- Loaded pages by stem (false once a load failed).
        pages = {},
    }
    M.state = s
    page(api, BLUE_STEM, BACKDROP_WARNING)
    local stage, room, cut = api:room()
    local group = (((stage or 1) - 1) & 0xFF) % 5
    s.area = initial_area(group, room or 0, cut or 0)
    s.layout = s.tables.layouts[s.area] or 0
    -- The original overwrites each layout's index with the last room slot
    -- whose area matches the current one.
    for layout = 0, 3 do
        for slot = 0, 3 do
            if area_at(layout, slot) == s.area then
                s.room[layout] = slot
            end
        end
    end
    api:set("zoomed", 0)
end

--- The next layout a left (`step == 1`) or right (`step == -1`) press picks.
--- The D-pad branches read the raw pad bits: left steps to the next layout
--- and right to the previous one; with all four layouts the original wraps
--- in pairs.
local function step_layout(mask, current, left)
    if left then
        if mask & 8 ~= 0 then
            return ({ [0] = 3, 2, 2, 1 })[current]
        end
        local next = current + 1
        if current == 2 or mask & (1 << next) == 0 then
            return current
        end
        return next
    elseif mask & 8 ~= 0 then
        return ({ [0] = 0, 3, 1, 0 })[current]
    elseif current ~= 0 and mask & (1 << (current - 1)) ~= 0 then
        return current - 1
    end
    return current
end

local function handle_input(api, input)
    if input.cancel then
        if s.zoomed then
            s.zoomed = false
        else
            api:exit()
        end
    elseif input.confirm then
        s.zoomed = not s.zoomed
    elseif input.up or input.down then
        if s.zoomed then
            return
        end
        local room = room_index()
        local flags = move_flags(api, s.layout, room)
        if input.up and flags & 1 ~= 0 then
            room = room + 1
        elseif input.down and flags & 2 ~= 0 then
            room = room - 1
        else
            return
        end
        s.room[s.layout] = room
        refresh_area()
    elseif input.left or input.right then
        if s.zoomed then
            return
        end
        local next = step_layout(layout_mask(api), s.layout, input.left)
        if next ~= s.layout then
            s.layout = next
            refresh_area()
        end
    end
end

--- One frame: advance the blink phase, then take the frame's one menu input.
--- The first update is the browser's first drawn frame, whose counter step
--- is its first frame's `1`.
function M.update(api, input)
    if not s.started then
        s.started = true
    else
        s.ticks = s.ticks + 1
    end
    handle_input(api, input)
    api:set("zoomed", s.zoomed and 1 or 0)
end

-- ---- drawing ---------------------------------------------------------------

--- One `colorMul`-scaled channel: the texel times `colorMul * 2 / 255`,
--- clamped at 1.0 and rounded to nearest, so 0x80 is unity.
local function tint(channel, mul)
    local factor = mul * 2
    if factor > 255 then
        factor = 255
    end
    return (channel * factor + 127) // 255
end

local function rgb(colour)
    return colour >> 16, (colour >> 8) & 0xFF, colour & 0xFF
end

--- The browser lut of `mul`: index 0 transparent, the rest tinted.
local function browser_lut(api, art, mul)
    s.luts = s.luts or {}
    local key = art .. ":" .. mul
    if not s.luts[key] then
        local palette = api:page_palette(art)
        local lut = {}
        for index = 1, 255 do
            local r, g, b = rgb(palette[index])
            lut[index] = (tint(r, mul) << 16) | (tint(g, mul) << 8) | tint(b, mul)
        end
        s.luts[key] = lut
    end
    return s.luts[key]
end

--- Blit one browser sprite. The original's page search rejects a sprite
--- whose rect leaves the loaded page, which is what makes the wrapped
--- floor-glyph `texV` disappear.
local function blit(api, art, rect, mul, ox, oy)
    local width, height = api:art_size(art)
    if rect.u + rect.w > width or rect.v + rect.h > height then
        return
    end
    api:page_lut(art, rect.u, rect.v, rect.w, rect.h, ox + rect.x, oy + rect.y, browser_lut(api, art, mul))
end

local function with(rect, field, value)
    local copy = {}
    for key, item in pairs(rect) do
        copy[key] = item
    end
    copy[field] = value
    return copy
end

--- The map browser: the loaded layout art's base window, the marker of every
--- available layout (the current one at full brightness, the rest at half),
--- the current area's name row, the floor's number glyph and the arrows.
local function draw_browser(api, ox, oy)
    -- The original loads one page for the whole browser: the file of the
    -- availability mask's highest set layout bit.
    local mask = layout_mask(api)
    local top = 0
    for layout = 3, 0, -1 do
        if mask & (1 << layout) ~= 0 then
            top = layout
            break
        end
    end
    local art = page(api, TAB_STEMS[top], PAGE_WARNING)
    if not art then
        return
    end
    blit(api, art, BASE_RECT, MUL_FULL, ox, oy)
    for layout = 0, 3 do
        if mask & (1 << layout) ~= 0 then
            blit(api, art, SPRITES[layout], layout == s.layout and MUL_FULL or MUL_HALF, ox, oy)
        end
    end
    blit(api, art, with(SPRITES[4], "v", (s.layout * 0x10 + 0x48) & 0xFF), MUL_FULL, ox, oy)
    local offset = s.tables.layout_offset[s.layout] or 0
    local glyph_v = (((6 - room_index() - offset) & 0xFF) * 0x10) & 0xFF
    blit(api, art, with(SPRITES[5], "v", glyph_v), MUL_FULL, ox, oy)
    -- `MAP_HL_COUNT`: bit 4 set for the dim half of its 64-frame cycle.
    local blink = ((s.ticks + 1) & 0x30) ~= 0
    local flags = move_flags(api, s.layout, room_index())
    for index, bit in ipairs({ 1, 2 }) do
        local sprite = SPRITES[5 + index]
        if flags & bit ~= 0 then
            blit(api, art, with(sprite, "u", (sprite.u + (blink and 8 or 0)) & 0xFF), MUL_FULL, ox, oy)
        end
    end
end

--- Half brightness (`colorMul` 0x80 over the full-display layers).
local function dim(channel)
    return channel * 0x80 // 0xFF
end

local function dimmed(colour)
    local r, g, b = rgb(colour)
    return (dim(r) << 16) | (dim(g) << 8) | dim(b)
end

--- The full display: the blue grid backdrop (a solid layer; its index 0 is
--- the dark blue square colour) with the area's floor plan over it. The
--- plan's unvisited room markers stay transparent, the current room's marker
--- is the red highlight and everything else keeps its palette colour.
local function draw_display(api, ox, oy)
    local area = s.area < 10 and s.area or 10
    local plan = page(api, AREA_STEMS[area], PLAN_WARNING)
    if not plan then
        return
    end
    local width, height = api:art_size(plan)
    width = width < DISPLAY_W and width or DISPLAY_W
    height = height < DISPLAY_H and height or DISPLAY_H
    local backdrop = page(api, BLUE_STEM, BACKDROP_WARNING)
    if backdrop then
        local palette = api:page_palette(backdrop)
        local lut = {}
        for index = 0, 255 do
            lut[index] = dimmed(palette[index])
        end
        api:page_lut(backdrop, 0, 0, width, height, ox + DISPLAY_X, oy + DISPLAY_Y, lut)
    end
    local group = s.tables.groups[s.area] or 0
    local room_count = s.tables.room_counts[group]
    local highlight = area_at(s.layout, room_index()) == s.area and room_index() or nil
    local palette = api:page_palette(plan)
    local lut = {}
    for index = 1, 255 do
        local room = index >= 13 and (index - 13) // 6 or nil
        if room ~= nil and room < room_count then
            if highlight == room then
                lut[index] = dimmed(HIGHLIGHT)
            elseif room_visited(api, group, room) then
                lut[index] = dimmed(palette[index])
            end
        else
            lut[index] = dimmed(palette[index])
        end
    end
    api:page_lut(plan, 0, 0, width, height, ox + DISPLAY_X, oy + DISPLAY_Y, lut)
end

--- Draw the tab over the frozen menu frame the engine already drew.
function M.draw(api)
    local ox, oy = api:width() // 2 - 160, api:height() // 2 - 120
    if s.zoomed then
        draw_display(api, ox, oy)
    elseif s.started then
        draw_browser(api, ox, oy)
    end
    -- The original's depth puts the status screen's frame decorations in
    -- front of the map's sprites, so the panel's dithered right-hand edge
    -- shows over the composite.
    local rect = s.zoomed and DISPLAY_RECT or BROWSER_RECT
    Panel.draw_frames_over(api, ox, oy, api:inventory_capacity(), rect)
end

return M
