-- Classic REbirth-style save/load screen.
--
-- The folder-based typewriter screen: every save file in the save directory
-- is a row (labelled with its base name), the selected row is a highlight
-- band, and a detail line shows the highlighted save's character, save count
-- and location. The last row is `Exit`.
--
-- The screen is selected with `save_screen = "cr"` in arklay.toml (or
-- `--save-screen cr`). It is a normal pack entry, so a mod overrides it by
-- carrying its own `screen/save_cr.lua`. The API it uses is the general
-- screen surface documented in docs/modding.md.

local VISIBLE_ROWS = 10 -- rows drawn at once (CR shows ten)
local ROW_Y = 49 -- the first row's y
local ROW_PITCH = 15 -- row pitch, CR's 15 px
local TITLE_Y = 27 -- the centred title's y
local DETAIL_Y = 216 -- the detail line's y
local PANEL_X = 32 -- the list panel's x
local PANEL_W = 256 -- the list panel's width

local mode = "load"
local saves = {}
local cursor = 0 -- 0..count; the final row is Exit
local scroll = 0 -- the first visible row
local message = nil -- an error line, cleared by any key
local leaving = false -- the load screen's fade-out is running
local fade_alpha = 0 -- its black-overlay opacity

local function clamp(value, low, high)
    if value < low then
        return low
    end
    if value > high then
        return high
    end
    return value
end

-- The row count: the save files plus the exit row.
local function row_count()
    return #saves + 1
end

-- The save entry under the cursor, or nil for the exit row.
local function selected()
    return saves[cursor + 1]
end

-- A language string, or `fallback` when the pack does not carry the key.
local function string_or(api, key, fallback)
    return api:string(key) or fallback
end

-- Draw one centred line.
local function draw_line(api, text, y, colour)
    local x = (320 - api:text_width(text)) // 2
    if colour then
        api:text(x, y, text, { colour = colour })
    else
        api:text(x, y, text)
    end
end

-- The first free `save_NN.dat` name.
local function new_name()
    local used = {}
    for _, entry in ipairs(saves) do
        used[entry.name] = true
    end
    for index = 1, 9999 do
        local name = string.format("save_%02d.dat", index)
        if not used[name] then
            return name
        end
    end
    return "save.dat"
end

function on_open(api)
    mode = api:mode()
    saves = api:saves()
    cursor = 0
    scroll = 0
    message = nil
    leaving = false
    fade_alpha = 0
end

-- Leave the screen. A save screen hands straight back to the room, which
-- fades itself in (the vanilla screen does not fade there either); the title's
-- load screen fades to black first, like the original.
local function leave(api)
    if mode == "save" then
        api:exit()
    else
        leaving = true
    end
end

function update(api, input)
    if leaving then
        fade_alpha = fade_alpha + 16
        api:fade(fade_alpha)
        if fade_alpha >= 255 then
            api:exit()
        end
        return
    end

    if message then
        if input.any then
            message = nil
        end
        return
    end

    if input.up then
        cursor = cursor - 1
        if cursor < 0 then
            cursor = row_count() - 1
        end
        api:sound("save_cursor")
    elseif input.down then
        cursor = cursor + 1
        if cursor > row_count() - 1 then
            cursor = 0
        end
        api:sound("save_cursor")
    elseif input.confirm then
        local entry = selected()
        if not entry then
            api:sound("cancel")
            leave(api)
        elseif mode == "load" then
            if entry.loadable then
                api:sound("save_confirm")
                local ok, err = api:load(entry.name)
                if not ok then
                    api:sound("cancel")
                    message = err
                end
            else
                api:sound("cancel")
            end
        else
            api:sound("save_confirm")
            -- CR overwrites by deleting the old file and writing a new one.
            if entry.loadable then
                api:remove(entry.name)
            end
            local ok, err = api:save(new_name())
            if ok then
                leave(api)
            else
                api:sound("cancel")
                message = err
            end
        end
    elseif input.cancel then
        api:sound("cancel")
        leave(api)
    end

    -- Keep the cursor inside the visible window.
    if cursor < scroll then
        scroll = cursor
    end
    if cursor >= scroll + VISIBLE_ROWS then
        scroll = cursor - VISIBLE_ROWS + 1
    end
    scroll = clamp(scroll, 0, clamp(row_count() - VISIBLE_ROWS, 0, row_count() - 1))
end

function draw(api)
    -- The vanilla typewriter photo; a pack without it gets a black screen.
    if not pcall(api.background, api, "ui/type00.bmp") then
        api:rect(0, 0, 320, 240, 0x000000)
    end

    local title
    if mode == "save" then
        title = string_or(api, "UI_SAVE_CR_SAVE", "Save")
    else
        title = string_or(api, "UI_SAVE_CR_LOAD", "Load")
    end
    draw_line(api, title, TITLE_Y)

    local rows = row_count()
    api:rect(PANEL_X, ROW_Y - 5, PANEL_W, rows * ROW_PITCH + 8, 0x000000, 96)

    -- The selection band follows the cursor.
    local band_y = ROW_Y + (cursor - scroll) * ROW_PITCH - 2
    api:rect(PANEL_X, band_y, PANEL_W, ROW_PITCH - 2, 0xC0C0C0, 72)

    for index = 0, VISIBLE_ROWS - 1 do
        local row = scroll + index
        if row >= rows then
            break
        end
        local y = ROW_Y + index * ROW_PITCH
        local entry = saves[row + 1]
        if entry then
            draw_line(api, entry.base, y, entry.loadable and "white" or "grey")
        else
            draw_line(api, string_or(api, "UI_SAVE_CR_EXIT", "Exit"), y)
        end
    end

    local entry = selected()
    local detail
    if entry then
        if entry.loadable then
            detail = string.format(
                "%s/%02d/%s",
                entry.character_name or "?",
                entry.saves or 0,
                entry.location or ""
            )
        else
            detail = string_or(api, "UI_SAVE_CR_UNLOADABLE", "Not loadable")
        end
    elseif mode == "save" then
        detail = string_or(api, "UI_SAVE_CR_DO_NOT_SAVE", "Do not save")
    else
        detail = string_or(api, "UI_SAVE_CR_DO_NOT_LOAD", "Do not load")
    end
    draw_line(api, detail, DETAIL_Y)

    if message then
        draw_line(api, message, 224, "red")
    end
end
