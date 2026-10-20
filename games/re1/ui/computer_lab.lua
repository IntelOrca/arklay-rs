-- The laboratory computer terminal (room 5060), the interactive screen the
-- small lab's console opens.
--
-- The original's `display_computer_lab` (ComputerLab.cpp) is a three-level
-- state machine: the login (user JOHN, password ADA), the door menu and the
-- command prompt (MOLE), whose success releases one of the two cell doors.
-- The engine owns the rules around it: the monitor camera and lights the
-- terminal installs, and - from the door this screen publishes as
-- `released_door` (1 or 2) - the lock bit and the lab map flag it raises on
-- close.
--
-- This port transcribes the interaction: the same virtual keyboard (a 4x8
-- cell grid, char code `cell` for A..V, `cell - 1` for W..Z, ENTER at cell
-- 0x17, BACKSPACE at 0x1d, EXIT at cell 0), the same expected inputs, the
-- same rejected/accepted holds. The original draws its windows and keyboard
-- from the `umb00`-`umb02` texture pages; those are not packed, so the
-- presentation is the game font over simple panels (a documented deviation).

local M = {}

-- Keyboard grid columns and rows.
local COLS = 8
local ROWS = 4
-- The ENTER, BACKSPACE and EXIT cells.
local CELL_ENTER = 0x17
local CELL_BACKSPACE = 0x1d
local CELL_EXIT = 0x00
-- The two covered cells right of BACKSPACE.
local CELL_COVERED = { [0x1e] = true, [0x1f] = true }
-- Longest accepted field, the original's 8-byte text buffers.
local FIELD_LEN = 8
-- The accepted/rejected hold and the unlock hold.
local HOLD_TICKS = 90
-- The game font's `A` glyph byte, and the terminator a lone glyph draws with.
local GLYPH_A = 0x1D
local GLYPH_END = 0x01
-- The font-encoded letter ranges the credential keys decode from.
local UPPER_A, UPPER_Z = 0x1D, 0x36
local LOWER_A, LOWER_Z = 0x3D, 0x56

local BACKGROUND = { 8, 10, 20 }
local FIELD_BLOCK = { 200, 255, 200 }
local MENU_MARK = { 80, 255, 80 }
local KEY_CURSOR = { 60, 120, 60 }

local s

function M.open()
    s = {
        -- "login", "menu", "command" or "unlocked".
        stage = "login",
        -- "user", "secret", "rejected" or "accepted".
        sub = "user",
        timer = 0,
        -- `cl_login_init`: ST_COL = 1, ST_ROW = 0.
        cell = 1,
        field = {},
        user = {},
        -- 0/1 the two doors, 2 exit.
        menu_row = 0,
    }
    M.state = s
end

--- The expected credential char codes (1 = A) of a language-file key: its
--- font-encoded letters mapped back to the keyboard's code space, so the
--- check is case-insensitive and independent of the sheet's layout.
function M.expected_codes(api, key)
    local codes = {}
    local bytes = api:ui_bytes(key) or ""
    for index = 1, #bytes do
        local byte = bytes:byte(index)
        if byte >= UPPER_A and byte <= UPPER_Z then
            codes[#codes + 1] = byte - (UPPER_A - 1)
        elseif byte >= LOWER_A and byte <= LOWER_Z then
            codes[#codes + 1] = byte - (LOWER_A - 1)
        end
    end
    return codes
end

local function same(a, b)
    if #a ~= #b then
        return false
    end
    for index = 1, #a do
        if a[index] ~= b[index] then
            return false
        end
    end
    return true
end

--- The keyboard char code of the current cell: -1 ENTER, -2 EXIT, -3
--- BACKSPACE (`cl_keyboard_input`).
local function cell_code()
    local cell = s.cell
    if cell == CELL_EXIT then
        return -2
    elseif cell == CELL_ENTER then
        return -1
    elseif cell == CELL_BACKSPACE then
        return -3
    elseif cell >= 0x18 and cell <= 0x1c then
        return cell - 1
    elseif CELL_COVERED[cell] then
        return 0
    end
    return cell
end

--- Move the keyboard cursor; the covered cells step back like the original.
local function move_cursor(input)
    local col = s.cell % COLS
    local row = s.cell // COLS
    if input.up and row > 0 then
        row = row - 1
    end
    if input.down and row < ROWS - 1 then
        row = row + 1
    end
    if input.left then
        col = col == 0 and COLS - 1 or col - 1
    end
    if input.right then
        col = col + 1
        if col >= COLS then
            col = 0
        end
    end
    local cell = row * COLS + col
    if cell == 0x1f then
        row = row - 1
        cell = row * COLS + col
    end
    if cell == 0x1e then
        col = col - 1
        cell = row * COLS + col
    end
    s.cell = cell
end

--- One typing pass; "submit" on ENTER, "exit" on EXIT, else nil.
local function type_field(input)
    move_cursor(input)
    if not input.confirm then
        return nil
    end
    local code = cell_code()
    if code == -1 then
        return "submit"
    elseif code == -2 then
        return "exit"
    elseif code == -3 then
        s.field[#s.field] = nil
    elseif code > 0 and #s.field < FIELD_LEN then
        s.field[#s.field + 1] = code
    end
    return nil
end

local function tick_login(api, input)
    if s.sub == "user" or s.sub == "secret" then
        local typed = type_field(input)
        if typed == "exit" then
            return true
        elseif typed ~= "submit" then
            return false
        end
        if s.sub == "user" then
            s.user = s.field
            s.field = {}
            s.sub = "secret"
            s.cell = 1
        else
            local accepted = same(s.user, M.expected_codes(api, "UI_LAB_PASSWORD_USER"))
                and same(s.field, M.expected_codes(api, "UI_LAB_PASSWORD_ENTRY"))
            s.sub = accepted and "accepted" or "rejected"
            s.timer = HOLD_TICKS
        end
        return false
    end
    s.timer = s.timer - 1
    if s.timer <= 0 then
        if s.sub == "rejected" then
            s.cell = 1
        else
            s.stage = "menu"
            s.menu_row = 0
        end
        s.sub = "user"
        s.user = {}
        s.field = {}
    end
    return false
end

local function tick_menu(input)
    if input.up and s.menu_row > 0 then
        s.menu_row = s.menu_row - 1
    end
    if input.down and s.menu_row < 2 then
        s.menu_row = s.menu_row + 1
    end
    if input.cancel then
        return true
    end
    if input.confirm then
        if s.menu_row == 2 then
            return true
        end
        s.stage = "command"
        s.sub = "user"
        s.field = {}
        s.cell = 1
    end
    return false
end

local function tick_command(api, input)
    if s.sub == "user" then
        local typed = type_field(input)
        if typed == "exit" then
            return true
        elseif typed == "submit" then
            local accepted = same(s.field, M.expected_codes(api, "UI_LAB_PASSWORD_COMMAND"))
            s.sub = accepted and "accepted" or "rejected"
            s.timer = HOLD_TICKS
        end
        return false
    end
    s.timer = s.timer - 1
    if s.timer <= 0 then
        if s.sub == "accepted" then
            -- `cl_cmd_unlock`: release the selected door.
            api:set("released_door", s.menu_row + 1)
            s.stage = "unlocked"
            s.timer = HOLD_TICKS
        else
            s.stage = "menu"
        end
        s.sub = "user"
        s.field = {}
    end
    return false
end

function M.update(api, input)
    local done
    if s.stage == "login" then
        done = tick_login(api, input)
    elseif s.stage == "menu" then
        done = tick_menu(input)
    elseif s.stage == "command" then
        done = tick_command(api, input)
    else
        s.timer = s.timer - 1
        done = s.timer <= 0 or input.confirm or input.cancel
    end
    if done then
        api:exit()
    end
end

-- ---- drawing ---------------------------------------------------------------

--- The glyph byte of the keyboard letter `code` (1 = A), as a lone glyph.
local function letter(code)
    return string.char(GLYPH_A + code - 1, GLYPH_END)
end

local function fill(api, x, y, w, h, rgb)
    api:fill_rect(x, y, w, h, rgb[1], rgb[2], rgb[3])
end

--- Render a field's chars on the fixed 8px grid.
local function draw_field(api, x, y, field)
    for index, code in ipairs(field) do
        if code ~= 0 then
            api:glyphs(x + (index - 1) * 8, y, letter(code), { tight = false })
        end
    end
end

--- The virtual keyboard: 4 rows x 8 cells, A..Z around ENTER, with the
--- cursor cell highlighted.
local function draw_keyboard(api, ox, oy)
    for row = 0, ROWS - 1 do
        for col = 0, COLS - 1 do
            local cell = row * COLS + col
            local x = ox + 40 + col * 24
            local y = oy + 96 + row * 20
            if s.cell == cell then
                fill(api, x - 2, y - 2, 20, 16, KEY_CURSOR)
            end
            local key
            if cell == CELL_EXIT then
                key = "UI_LAB_EXIT"
            elseif cell == CELL_ENTER then
                key = "UI_LAB_KEY_ENT"
            elseif cell == CELL_BACKSPACE then
                key = "UI_LAB_KEY_DEL"
            end
            if cell >= 1 and cell <= 0x16 then
                api:glyphs(x, y, letter(cell), { tight = false })
            elseif cell >= 0x18 and cell <= 0x1b then
                api:glyphs(x, y, letter(cell - 1), { tight = false })
            elseif key then
                api:glyphs(x, y, api:ui_bytes(key), { colour = "yellow" })
            end
        end
    end
end

--- Draw the screen; without a font only the panels draw.
function M.draw(api)
    local ox, oy = api:width() // 2 - 160, api:height() // 2 - 120
    fill(api, 0, 0, api:width(), api:height(), BACKGROUND)
    if not (api:has_font() and api:has_text()) then
        return
    end
    local function line(y, key)
        local bytes = api:ui_bytes(key)
        if #bytes > 0 then
            api:glyphs(ox + 16, oy + y, bytes, { colour = "green" })
        end
    end
    if s.stage == "login" then
        line(8, "UI_LAB_TITLE")
        line(32, "UI_LAB_USER")
        draw_field(api, ox + 72, oy + 32, s.field)
        line(48, "UI_LAB_PASSWORD")
        if s.sub ~= "user" then
            for index = 0, #s.field - 1 do
                fill(api, ox + 104 + index * 8, oy + 48, 6, 10, FIELD_BLOCK)
            end
        end
        draw_keyboard(api, ox, oy)
        if s.sub == "rejected" then
            line(208, "UI_LAB_ACCESS_DENIED")
        elseif s.sub == "accepted" then
            line(208, "UI_LAB_ACCESS_GRANTED")
        else
            line(208, "UI_LAB_ENTER_CREDENTIALS")
        end
    elseif s.stage == "menu" then
        line(8, "UI_LAB_SELECT_DOOR")
        for row, key in ipairs({ "UI_LAB_RELEASE_DOOR_1", "UI_LAB_RELEASE_DOOR_2", "UI_LAB_EXIT" }) do
            local y = 48 + (row - 1) * 20
            line(y, key)
            if s.menu_row == row - 1 then
                fill(api, ox + 10, oy + y + 2, 4, 10, MENU_MARK)
            end
        end
    elseif s.stage == "command" then
        line(8, "UI_LAB_COMMAND")
        for index = 0, #s.field - 1 do
            fill(api, ox + 80 + index * 8, oy + 32, 6, 10, FIELD_BLOCK)
        end
        draw_keyboard(api, ox, oy)
        if s.sub == "rejected" then
            line(208, "UI_LAB_INVALID_COMMAND")
        elseif s.sub == "accepted" then
            line(208, "UI_LAB_COMMAND_ACCEPTED")
        else
            line(208, "UI_LAB_TYPE_COMMAND")
        end
    else
        line(96, "UI_LAB_DOOR_RELEASED")
    end
end

return M
