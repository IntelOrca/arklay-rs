-- The in-game F9 return-to-title prompt.
--
-- Pressing F9 during play freezes the room and shows three lines over a
-- dimmed frame; a second F9 confirms the return to the title screen and any
-- other key cancels. The engine owns the freeze and the paused sounds. The
-- text comes from the language files' UI_PROMPT_RETURN_TITLE_1..3; on a
-- sheet whose glyphs are wider than the USA 8px cells the lines wrap to the
-- frame instead of running off the right edge.

local M = {}

-- The full-screen dim drawn over the frozen frame before the text.
local DIM_ALPHA = 100
-- Left edge of the text, and the top of each of the three lines in the
-- original's 240-line frame.
local TEXT_X = 16
local LINE_Y = { 100, 116, 150 }
-- Vertical distance between the rows a line wraps into.
local LINE_SPACING = 16
local KEYS = {
    "UI_PROMPT_RETURN_TITLE_1",
    "UI_PROMPT_RETURN_TITLE_2",
    "UI_PROMPT_RETURN_TITLE_3",
}

function M.open() end

--- The F9 edge wins over the same tick's `any`, so the confirm press
--- cannot cancel itself.
function M.update(api, input)
    if input.return_title then
        api:exit("title")
    elseif input.any then
        api:exit("resume")
    end
end

--- Dim the frozen frame and draw the three lines. Without a font the dim
--- still darkens the frame. The third line keeps the original's gap under
--- the shorter first lines.
function M.draw(api)
    api:blend_black_rect(0, 0, api:width(), api:height(), DIM_ALPHA)
    if not (api:has_font() and api:has_text()) then
        return
    end
    local oy = api:height() // 2 - 120
    local max_width = api:width() - 2 * TEXT_X
    if max_width < 1 then
        max_width = 1
    end
    local y = LINE_Y[1]
    for index, key in ipairs(KEYS) do
        if index == #LINE_Y and y < LINE_Y[index] then
            y = LINE_Y[index]
        end
        for _, row in ipairs(api:wrap(api:ui_bytes(key), max_width)) do
            api:glyphs(TEXT_X, oy + y, row)
            y = y + LINE_SPACING
        end
    end
end

return M
