-- The save/load screen.
--
-- A port of the original's per-tick state machine (SaveLoadScreen.cpp):
-- scan the eight savedat*.dat slots, navigate the eight rows plus the exit
-- row with a blinking cursor, confirm an overwrite in save mode, write the
-- block and play the typed reveal (name/count/location, two bytes per
-- step), or load the chosen slot. It serves both modes: the title's load
-- path opens it in load mode, and a room typewriter opens it in save mode
-- over the frozen gameplay frame.
--
-- Column positions are the Japanese glyph grid's: the save-count digits at
-- countX 0x6F, the location at locX 0x99, the header at 0x61 and the slot
-- cursor one glyph cell left of the text at 0x29. The reveal string is
-- assembled exactly like the original's: three name cells, a separator, the
-- two count digits, a separator and the eight-cell location name. All text
-- is the text table's font-encoded bytes, drawn with api:glyphs.

local M = {}

local HEADER_X, HEADER_Y = 0x61, 13
local ROW_X, ROW_Y, ROW_HEIGHT = 55, 45, 16
local COUNT_X, LOC_X = 0x6F, 0x99
local CURSOR_X = 0x29
-- The overwrite-confirm cursor; YES and NO are four Japanese cells apart.
local CONFIRM_X, CONFIRM_Y, CONFIRM_STRIDE = 118, 209, 56
local PROMPT_X, PROMPT_Y = 49, 193
local ERROR_Y, ERROR_Y2 = 209, 225
local SLOT_COUNT = 8
-- The exit row's index (one past the slot rows, 0-based).
local EXIT_ROW = SLOT_COUNT
local EXIT_Y = ROW_Y + SLOT_COUNT * ROW_HEIGHT
-- Bytes of the three-cell name and the eight-cell location (terminator
-- included), and of the assembled reveal.
local NAME_LEN, LOC_LEN = 6, 17
M.REVEAL_LEN = NAME_LEN + 8 + LOC_LEN
local BLINK_TICKS = 5
local REVEAL_TICKS = 5
local REVEAL_STEP = 2
-- The black-overlay steps of the fade-in and the (faster) fade-out.
local FADE_STEP, FADE_OUT_STEP = 8, 16
-- The slash separator glyph (cell 2 of row 2) and the blinking cursor: the
-- reference's DrawSaveCursor blits the arrowhead at the glyph grid's plain
-- byte 0x02.
local SEPARATOR = 0x38
local CURSOR_GLYPH = string.char(0x02)
local PAD = 0xFB

local background
local s

--- The save-count digits as the original's %02d line: tens, units, each in
--- its own cell with a 0xFB pad.
function M.count_bytes(saves)
    saves = saves % 100
    return string.char(0x0C + (saves // 10) % 10, PAD, 0x0C + saves % 10, PAD)
end

--- `src` cut or zero-padded to `len` bytes.
local function padded(src, len)
    src = src:sub(1, len)
    return src .. string.rep("\0", len - #src)
end

--- Assemble the reveal string: name, separator, count digits, separator,
--- location.
function M.build_reveal(name, saves, location)
    return padded(name, NAME_LEN)
        .. string.char(SEPARATOR, PAD)
        .. M.count_bytes(saves)
        .. string.char(SEPARATOR, PAD)
        .. padded(location, LOC_LEN)
end

--- Whether the reveal has shown everything the original shows. The last
--- printed slice is REVEAL_LEN - 1 bytes: the final pass's check steps past
--- the string before it draws the terminator-only slice.
local function reveal_finished(reveal)
    return reveal.length + REVEAL_STEP + REVEAL_STEP > M.REVEAL_LEN + 1
end

--- Whether the slice drawn at the current length asks for the reference's
--- typewriter tick (play_sfx(sfxBank, 31)): the byte two back from the copy
--- end is not a space (spaces are 0x00).
local function reveal_cues(reveal)
    local index = reveal.length - REVEAL_STEP
    if index < 0 then
        return false
    end
    local byte = reveal.bytes:byte(index + 1)
    return byte ~= nil and byte ~= 0
end

local function reset_blink(visible)
    s.blink_state = visible or false
    s.blink_timer = BLINK_TICKS
end

--- Move the cursor, wrapping through the exit row.
local function move_cursor(delta)
    s.cursor = (s.cursor + delta) % (SLOT_COUNT + 1)
    reset_blink()
end

local function tick_blink()
    if s.blink_timer == 0 then
        s.blink_state = not s.blink_state
        s.blink_timer = BLINK_TICKS
    else
        s.blink_timer = s.blink_timer - 1
    end
end

--- Leave through cancel: back to gameplay in save mode, back to the title
--- in load mode.
local function leave()
    s.exit = s.save and "resume" or "title"
    s.stage = "fade_out"
end

--- Write the pending block to the cursor's slot and start the reveal.
---
--- `slot_cued` says the tick that arrived here already played the slot's
--- confirm cue - the navigation confirm plays it, while the overwrite
--- dialog's YES is silent. The reference plays play_sfx(sfxBank, 31) on the
--- confirm frame and, when the byte is not a space, again on its first
--- STATE_SAVE_ANIM_STEP1 frame; the port draws the first slice on the
--- write's own tick, so only the silent overwrite path adds that first
--- slice's tick here.
local function begin_save(api, slot_cued)
    local ok, info = api:save_slot(s.cursor)
    if not ok then
        api:log("warning: failed to save slot " .. (s.cursor + 1) .. ": " .. tostring(info))
        s.stage = "error"
        return
    end
    s.slots[s.cursor + 1] = info
    local name = api:ui_bytes(
        info.character & 1 == 0 and "UI_SAVE_CHARACTER_0_NAME" or "UI_SAVE_CHARACTER_1_NAME"
    ) or ""
    local location = api:location_bytes(info.stage, info.room) or ""
    local reveal = {
        bytes = M.build_reveal(name, info.saves, location),
        length = REVEAL_STEP,
        timer = REVEAL_TICKS,
    }
    if not slot_cued and reveal_cues(reveal) then
        api:sound("save_confirm")
    end
    s.reveal = reveal
    s.stage = "save_anim"
end

--- Confirm the current row.
local function confirm(api)
    if s.cursor == EXIT_ROW then
        leave()
        return
    end
    local occupied = s.slots[s.cursor + 1] ~= false
    if not s.save then
        -- An empty slot refuses.
        if occupied then
            s.exit = "load_game"
            s.stage = "fade_out"
        end
    elseif occupied then
        s.confirm_choice = 0
        reset_blink(true)
        s.stage = "confirm_overwrite"
    else
        -- The caller played the slot's confirm cue (the same bank slot as
        -- the first slice's typewriter tick).
        begin_save(api, true)
    end
end

function M.open(api)
    local err
    background, err = api:picture("ui/type00")
    if not background then
        api:log("warning: no save/load background: " .. err)
    end
    s = {
        save = api:mode() == "save",
        slots = api:slot_info(),
        -- 0-7 are the slot rows, EXIT_ROW the exit row.
        cursor = 0,
        stage = "idle",
        blink_timer = BLINK_TICKS,
        blink_state = false,
        -- 0 YES, 1 NO in the overwrite confirmation.
        confirm_choice = 0,
        reveal = nil,
        exit = nil,
        fade = 0,
    }
    M.state = s
end

local function finish(api)
    if s.exit == "load_game" then
        api:exit("load_game", s.cursor)
    else
        api:exit(s.exit or "resume")
    end
end

local function step(api, input)
    if s.stage == "idle" then
        if input.up then
            api:sound("save_cursor")
            move_cursor(-1)
        end
        if input.down then
            api:sound("save_cursor")
            move_cursor(1)
        end
        if input.confirm then
            -- The exit row uses the cancel slot; the slots use the confirm
            -- slot (the original's play_sfx 29/31).
            api:sound(s.cursor == EXIT_ROW and "cancel" or "save_confirm")
            confirm(api)
        elseif input.cancel then
            api:sound("cancel")
            leave()
        end
    elseif s.stage == "confirm_overwrite" then
        -- The original's overwrite dialog is silent.
        if input.left then
            s.confirm_choice = 0
            reset_blink()
        end
        if input.right then
            s.confirm_choice = 1
            reset_blink()
        end
        if input.confirm then
            if s.confirm_choice == 1 then
                s.stage = "idle"
                reset_blink()
            else
                -- The dialog's YES is silent in the reference, so the
                -- write's own tick plays the first slice's tick.
                begin_save(api, false)
            end
        elseif input.cancel then
            s.stage = "idle"
            reset_blink()
        end
    elseif s.stage == "save_anim" then
        local reveal = s.reveal
        if reveal.timer == 0 then
            if reveal_finished(reveal) then
                s.exit = "resume"
                s.stage = "fade_out"
            else
                reveal.length = reveal.length + REVEAL_STEP
                reveal.timer = REVEAL_TICKS
                if reveal_cues(reveal) then
                    api:sound("save_confirm")
                end
            end
        else
            reveal.timer = reveal.timer - 1
        end
    elseif s.stage == "error" then
        if input.confirm or input.cancel or input.any then
            s.stage = "idle"
            reset_blink()
        end
    end
end

function M.update(api, input)
    if s.stage == "fade_out" then
        s.fade = s.fade + FADE_OUT_STEP
        if s.fade >= 255 then
            s.fade = 255
            api:fade(s.fade)
            finish(api)
            return
        end
        api:fade(s.fade)
        return
    end
    s.fade = s.fade - FADE_STEP
    if s.fade < 0 then
        s.fade = 0
    end
    tick_blink()
    step(api, input)
    api:fade(s.fade)
    -- The in-game (typewriter) screen does not fade itself: the reference's
    -- LoadSaveGameState returns straight to check_typewriter_state, which
    -- fades the room back in from the black the save hand-off left. Only
    -- the title's load screen fades its own overlay.
    if s.stage == "fade_out" and s.save then
        finish(api)
    end
end

--- The layout origin: the original's 320x240 columns, centred.
local function origin(api)
    return api:width() // 2 - 160, api:height() // 2 - 120
end

-- The rows a frame shorter than 240 (a wider aspect's band) stacks the
-- screen on: the header, the slot rows at a tighter pitch, the exit row and
-- the prompt/confirm lines, measured from the band's top.
local COMPACT = {
    header = 4, row = 20, pitch = 14,
    prompt = 148, confirm = 164, error = 148, error2 = 164,
}

--- The vertical layout in the origin's coordinates: the original's rows
--- on a 240-row frame, the compact rows on a shorter one.
local function layout(api, oy)
    if api:height() >= 240 then
        return {
            header = HEADER_Y, row = ROW_Y, pitch = ROW_HEIGHT, exit = EXIT_Y,
            prompt = PROMPT_Y, confirm = CONFIRM_Y, error = ERROR_Y, error2 = ERROR_Y2,
        }
    end
    local rows = {}
    for key, y in pairs(COMPACT) do
        rows[key] = key == "pitch" and y or y - oy
    end
    rows.exit = rows.row + SLOT_COUNT * rows.pitch
    return rows
end

local function cursor(api, x, y)
    if api:has_font() then
        api:glyphs(x, y, CURSOR_GLYPH, { tight = false })
    else
        api:fill_rect(x, y, 8, 12, 255, 255, 255, 255)
    end
end

function M.draw(api)
    local ox, oy = origin(api)
    local rows = layout(api, oy)
    if background then
        -- A full-frame backdrop is stretched, so a replacement may be
        -- authored at any size.
        api:draw_background(background, 0, 0, 255, 255, 255)
    else
        api:clear()
        api:fill_rect(0, 0, api:width(), api:height(), 0, 0, 0, 255)
    end
    local row_y = oy + rows.row + s.cursor * rows.pitch
    if not (api:has_font() and api:has_text()) then
        cursor(api, ox + CURSOR_X, row_y)
        return
    end
    local function text(x, y, key, fixed)
        api:glyphs(ox + x, oy + y, api:ui_bytes(key), fixed and { tight = false } or nil)
    end
    local mode = s.save and 0 or 1

    -- The header is the original's formatted path, so the suffix's M is
    -- the sheet's letter M.
    text(HEADER_X, rows.header, "UI_SAVE_HEADER_" .. mode)
    text(HEADER_X, rows.header, "UI_SAVE_HEADER_SUFFIX")

    for index = 0, SLOT_COUNT - 1 do
        local y = rows.row + index * rows.pitch
        local info = s.slots[index + 1]
        if s.stage == "save_anim" and s.cursor == index then
            -- The reveal draws this row.
        elseif info then
            -- The slot template is fixed-width art authored on the
            -- Japanese 14px grid: keep its separators on the count and
            -- location columns.
            text(ROW_X, y, "UI_SAVE_SLOT_FILLED", true)
            text(ROW_X, y, "UI_SAVE_CHARACTER_" .. (info.character & 1) .. "_NAME")
            api:glyphs(ox + COUNT_X, oy + y, M.count_bytes(info.saves))
            api:glyphs(ox + LOC_X, oy + y, api:location_bytes(info.stage, info.room))
        else
            -- The empty-slot row is fixed-width art too.
            text(ROW_X, y, "UI_SAVE_SLOT_EMPTY", true)
        end
    end

    text(ROW_X, rows.exit, "UI_SAVE_EXIT_" .. mode)
    text(ROW_X, rows.exit, "UI_SAVE_EXIT_SUFFIX")

    if s.reveal then
        -- The reveal is assembled on the fixed 14px grid to line up with
        -- the row's separators.
        api:glyphs(ox + ROW_X, row_y, s.reveal.bytes:sub(1, s.reveal.length), { tight = false })
    end

    if s.stage == "confirm_overwrite" then
        text(PROMPT_X, rows.prompt, "UI_SAVE_OVERWRITE_PROMPT")
        text(CONFIRM_X, rows.confirm, "UI_SAVE_YES_NO")
    elseif s.stage == "error" then
        text(PROMPT_X, rows.error, "UI_SAVE_ERROR_0")
        text(PROMPT_X, rows.error2, "UI_SAVE_ERROR_1")
    end

    if s.stage == "confirm_overwrite" then
        -- The slot cursor stays visible while the confirm cursor blinks.
        cursor(api, ox + CURSOR_X, row_y)
        if not s.blink_state then
            cursor(api, ox + CONFIRM_X + s.confirm_choice * CONFIRM_STRIDE, oy + rows.confirm)
        end
    elseif not s.blink_state then
        cursor(api, ox + CURSOR_X, row_y)
    end
end

return M
