-- The pause menu's FILE tab: the two-book document selector and the page
-- reader, which the room's document pick-up also runs on its own.
--
-- The tab shows `file/file000`/`file001` as book covers, each with eight
-- document slots whose icons and cursor are baked into the same cover page.
-- A document is listed once its FILE-collected bit is raised by the pickup
-- (room-flags bank 8, `0x82 + (item - 0x5F)`). Confirming a listed document
-- opens the reader: the JPN build draws a scanned `filei*` backdrop behind
-- each page and stores every page as its own `textm_*` TIM that packs two
-- 256x128 half-pages. The book/page tables below are the Japanese build's
-- per-document start/count/first-page tables; the reader advances one
-- half-page per press and closes on confirm at the last page.
--
-- The Western build's reader (`pickup_screen_render` 0x004823a0) runs the
-- same state machine without the backdrop: each document is a container of
-- full 256x192 pages, converted to `file/<language>/<filem stem>_<page>`, and
-- the page arrows come from `file/arror`. Each document reads the pages of
-- the session language first, then English, French, German and finally the
-- Japanese build's; a Japanese session prefers the Japanese pages.
--
-- Open arguments: `pickup` (a document item id, `0x5F..=0x6E`) opens the
-- reader directly on that document for the room pick-up flow; the screen
-- exits once the reader's exit slide has finished. Without it the screen is
-- the pause menu's tab and exits on a cancel from the selector.
--
-- The engine hands the screen one menu input per frame (the pause menu's
-- priority) and plays its cue.

local M = {}

-- First document item id (`FILE_ITEM_MIN`).
local FILE_ITEM_BASE = 0x5F
-- The room-flags bank and the FILE-collected bit base.
local BANK_ROOM_FLAGS = 8
local ROOM_FLAG_FILE_BASE = 0x82

local function list(...)
    local values = { ... }
    local out = {}
    for index = 1, #values do
        out[index - 1] = values[index]
    end
    return out
end

-- The characters' 16-entry key-item lists: two books of eight. `0xFE` is the
-- scenario-variant marker, `0xFF` an empty slot.
local KEY_ITEM_LISTS = {
    [0] = list(0x0F, 0x02, 0xFE, 0x03, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0C, 0x0D, 0x0E, 0xFF, 0xFF, 0xFF),
    [1] = list(0x0F, 0x02, 0xFE, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0xFF),
}

-- Per-document FILEI backdrop index into `FILEI_NAMES`.
local FILEI_INDEX = list(1, 0, 2, 3, 8, 12, 3, 5, 7, 0, 4, 11, 6, 9, 10, 13)
-- Per-document reading length in half-pages (both 2n and 2n-1 display n
-- pages; parity only decides which half of the last TIM is last).
local PAGE_COUNT = list(4, 6, 11, 3, 2, 6, 7, 7, 8, 5, 5, 4, 2, 2, 2, 5)
-- Per-document title x offset (JPN build).
local TITLE_X = list(42, 42, 42, 21, 42, 21, 49, 28, 0, 42, 14, 42, 35, 35, 35, 63)
-- Per-document start index into `TEXTM_NAMES`.
local PAGE_START = list(0, 2, 5, 11, 13, 14, 17, 21, 25, 29, 32, 35, 37, 38, 39, 40)

-- The 43 per-page TIM names in table order.
local TEXTM_NAMES = list(
    "textm_l0", "textm_l1", "textm_m0", "textm_m1", "textm_m2", "textm_n0", "textm_n1", "textm_n2",
    "textm_n3", "textm_n4", "textm_n5", "textm_o0", "textm_o1", "textm_p0", "textm_q0", "textm_q1",
    "textm_q2", "textm_r0", "textm_r1", "textm_r2", "textm_r3", "textm_s0", "textm_s1", "textm_s2",
    "textm_s3", "textm_t0", "textm_t1", "textm_t2", "textm_t3", "textm_u0", "textm_u1", "textm_u2",
    "textm_v0", "textm_v1", "textm_v2", "textm_w0", "textm_w1", "textm_x0", "textm_y0", "textm_z0",
    "textm_00", "textm_01", "textm_02"
)
-- The Western build's per-document page counts (`g_itemMaxCounts`), title x
-- offsets (the same table from 0x10) and container stems (`0x004d2350`).
local WEST_PAGE_COUNT = list(4, 6, 11, 4, 2, 6, 7, 7, 10, 5, 5, 4, 3, 3, 3, 4)
local WEST_TITLE_X = list(8, 8, 24, 52, 32, 20, 64, 40, 16, 0, 16, 16, 32, 32, 32, 32)
local FILEM_NAMES = list(
    "filem_l0", "filem_m0", "filem_n0", "filem_o0", "filem_p0", "filem_q0", "filem_r0", "filem_s0",
    "filem_t0", "filem_u0", "filem_v0", "filem_w0", "filem_x0", "filem_y0", "filem_z0", "filem_00"
)

-- The 14 used per-document backdrop TIM names in `FILEI_INDEX` order; the
-- tree has no `filei01/02/05`, so those indices are unused.
local FILEI_NAMES = list(
    "filei03", "filei04", "filei06", "filei07", "filei08", "filei09", "filei10", "filei11",
    "filei12", "filei13", "filei14", "filei15", "filei16", "filei17"
)

local BOOKS = 2
local SLOTS_PER_BOOK = 8

-- The book cover draw position and source size (the cover page is the whole
-- selector art).
local COVER_X, COVER_Y, COVER_W, COVER_H = 0x20, 0x18, 0xA0, 0x68
-- The slot-icon (`g_fileSlotDescs`) and cursor-arrow (`DAT_004d27d0`)
-- source rects inside the cover.
local SLOT_ICON_UV = { 0xA8, 0, 4, 8 }
local CURSOR_UV = { 0xA0, 0, 4, 8 }
-- The slot-icon list's base and row step.
local SLOT_X, SLOT_Y, SLOT_STEP = 0x8C, 0x29, 9

-- The reader page position and size (one half of a TEXTM TIM).
local PAGE_X, PAGE_Y, PAGE_W, PAGE_H = 0x18, 0x34, 0x100, 0x80
-- The Western page position and size (`DAT_004d2888`).
local WEST_PAGE_X, WEST_PAGE_Y, WEST_PAGE_W, WEST_PAGE_H = 0x18, 0x18, 0x100, 0xC0
-- The selector title's y and the scroll arrows' x, per build.
local JPN_TITLE_Y, WEST_TITLE_Y = 0xC0, 0xC1
local JPN_SCROLL_X, WEST_SCROLL_X = 0x6F, 0x67
-- The page arrows and the EXIT label.
local PREV_ARROW_X, PREV_ARROW_Y = 0x18, 0x74
local NEXT_ARROW_X, NEXT_ARROW_Y = 0x110, 0x74
local EXIT_X, EXIT_Y = 0x118, 0x6F

-- The reader's entry/exit slide start offset and speed (`0x128`, `300`).
local READER_SLIDE_START = 0x128
local READER_SLIDE_SPEED = 300
-- The backdrop/frame ramp step and its full value (`DAT_004af48c`).
local READER_BACKDROP_STEP = 8
local READER_BACKDROP_FULL = 0x80
-- The six FILEI pieces the reader draws behind the page (`g_jpnReaderFrame`,
-- 0x004af478): the backdrop itself plus the five scraps stacked down the
-- texture's right edge, each a source rect and a destination.
local READER_FRAME_PIECES = {
    { 0x00, 0x00, 0xD0, 0x78, 0x30, 0x30 },
    { 0xD0, 0x00, 0x30, 0x18, 0x30, 0xA8 },
    { 0xD0, 0x18, 0x30, 0x18, 0x60, 0xA8 },
    { 0xD0, 0x30, 0x30, 0x18, 0x90, 0xA8 },
    { 0xD0, 0x48, 0x30, 0x18, 0xC0, 0xA8 },
    { 0xD0, 0x60, 0x10, 0x18, 0xF0, 0xA8 },
}
-- The page arrows' source y in the FILEI texture (texV `0x60`).
local READER_ARROW_V = 0x60
-- The selector's entry slide (`FUN_00481ae0`): the cover starts at `-0x7f`
-- and eases down at speed `0x80`.
local LIST_SLIDE_START = -0x7f
local LIST_SLIDE_SPEED = 0x80
-- The pause menu's screen area. The selector's own depth sits behind the
-- panel's frame parts, so a sliding cover never spills over the panel's
-- chrome; clipping the selector to this area reproduces that.
local SCREEN_AREA = { 0x28, 0x18, 0xF0, 0xC0 }

local s
-- Defined with the input lock below.
local publish

-- ---- document tables -------------------------------------------------------

local function file_collected(api, index)
    return api:flag(BANK_ROOM_FLAGS, (ROOM_FLAG_FILE_BASE + index) & 0xFF)
end

--- The raw key-item list entry of `book` slot `slot`.
local function raw_entry(book, slot)
    book = book < BOOKS - 1 and book or BOOKS - 1
    slot = slot < SLOTS_PER_BOOK - 1 and slot or SLOTS_PER_BOOK - 1
    return KEY_ITEM_LISTS[s.character][book * SLOTS_PER_BOOK + slot]
end

--- Resolve a raw list entry against the collected flags. `0xFF` is empty and
--- `0xFE` picks document 0 or 1 once one of them has been collected;
--- otherwise the entry is a document index that must itself be collected.
local function resolve_entry(api, entry)
    if entry == 0xFF then
        return nil
    elseif entry == 0xFE then
        if file_collected(api, 0) then
            return 0
        elseif file_collected(api, 1) then
            return 1
        end
        return nil
    elseif PAGE_COUNT[entry] ~= nil and file_collected(api, entry) then
        return entry
    end
    return nil
end

--- The document index of `book`/`slot`, resolved against the collected flags.
local function listed_entry(api, book, slot)
    return resolve_entry(api, raw_entry(book, slot))
end

--- The first listed slot of `book`, or `0xFF` when nothing is collected.
local function first_listed_slot(api, book)
    for slot = 0, SLOTS_PER_BOOK - 1 do
        if listed_entry(api, book, slot) then
            return slot
        end
    end
    return 0xFF
end

--- The nearest listed slot strictly above `slot` in `book`.
local function previous_listed_slot(api, book, slot)
    for candidate = slot - 1, 0, -1 do
        if listed_entry(api, book, candidate) then
            return candidate
        end
    end
    return nil
end

--- The nearest listed slot strictly below `slot` in `book`.
local function next_listed_slot(api, book, slot)
    for candidate = slot + 1, SLOTS_PER_BOOK - 1 do
        if listed_entry(api, book, candidate) then
            return candidate
        end
    end
    return nil
end

--- The reading length of document `entry`: half-pages for the Japanese
--- reader, whole pages for the Western one.
local function page_count(entry)
    if s and s.west then
        return WEST_PAGE_COUNT[entry] or 0
    end
    return PAGE_COUNT[entry] or 0
end

--- The TEXTM table index of half-page `page` of document `entry`, or nil for
--- an out-of-range document/page.
local function page_index(entry, page)
    local start = PAGE_START[entry]
    if start == nil or page >= (PAGE_COUNT[entry] or 0) then
        return nil
    end
    return start + page // 2
end

M.page_index = page_index
-- The document tables, for the tests.
M.tables = {
    key_items = KEY_ITEM_LISTS,
    filei_index = FILEI_INDEX,
    page_count = PAGE_COUNT,
    title_x = TITLE_X,
    page_start = PAGE_START,
    textm = TEXTM_NAMES,
    filei = FILEI_NAMES,
    west_page_count = WEST_PAGE_COUNT,
    west_title_x = WEST_TITLE_X,
    filem = FILEM_NAMES,
    title_y = { jpn = JPN_TITLE_Y, west = WEST_TITLE_Y },
    scroll_x = { jpn = JPN_SCROLL_X, west = WEST_SCROLL_X },
}

--- The pack stem of Western page `page` of document `entry` under `prefix`.
local function west_page_stem(prefix, entry, page)
    return ("%s/%s_%02d"):format(prefix, FILEM_NAMES[entry], page)
end

M.west_page_stem = west_page_stem

--- The primary subtag of a session language tag (`ja-JP` -> `ja`), or nil.
local function primary_language(language)
    local primary = language and language:match("^%a+")
    return primary and primary:lower()
end

--- The readers a session in `language` tries, most preferred first: a
--- Western pages directory, or `jpn` for the Japanese build's pages.
local function reader_order(language)
    local primary = primary_language(language)
    local order, seen = {}, {}
    local function add(reader)
        if not seen[reader] then
            seen[reader] = true
            order[#order + 1] = reader
        end
    end
    if primary == "ja" then
        add("jpn")
    elseif primary then
        add("file/" .. primary)
    end
    for _, west in ipairs({ "en", "fr", "de" }) do
        add("file/" .. west)
    end
    add("jpn")
    return order
end

--- The Western pages directory document `entry` reads from, or nil for the
--- Japanese reader: the first of the session's readers that has the
--- document's first page.
local function document_pages(api, entry)
    if entry == nil then
        return nil
    end
    for _, reader in ipairs(s.readers) do
        if reader == "jpn" then
            local start = PAGE_START[entry]
            if start and api:has_image("file/" .. TEXTM_NAMES[start]) then
                return nil
            end
        elseif api:has_image(west_page_stem(reader, entry, 0)) then
            return reader
        end
    end
    return nil
end

-- ---- state -----------------------------------------------------------------

--- Reset the reader's state block (`jpn_file_list_init`).
local function reader_reset()
    s.reader_state = "backdrop"
    s.reader_exit_request = false
    s.reader_slide = 0
    s.reader_speed = 1
    s.reader_page_loaded = false
    s.reader_backdrop = 0
    s.reader_busy = true
    s.reader_busy_frame = true
    s.reader_last_page = false
    s.reader_exit_done = false
end

function M.open(api, args)
    -- Drawn on black: a wider aspect gives the panel a wider canvas.
    api:set("canvas", 1)
    local _, _, _, player = api:room()
    s = {
        -- "list" or "reader".
        mode = "list",
        -- Character half of the key-item list (0 Chris, 1 Jill).
        character = (player or 0) & 1,
        book = 0,
        -- Selected slot, or 0xFF when the book has no document.
        slot = 0xFF,
        -- Reader half-page index.
        page = 0,
        -- Blink counter for the arrows and cursor.
        blink = 0,
        -- The current view's slide (book changes / the entry slide).
        slide_x = 0,
        slide_y = 0,
        -- The selector's entry slide speed (`state[0xa]`).
        list_speed = 1,
        -- Whether the selector's state machine has yet to run its first
        -- frame: the original switches the menu's task a frame after the tab
        -- row's confirm, so that frame still draws the panel alone.
        list_pending = false,
        -- Whether the selector has settled and draws its title, book
        -- indicators and scroll arrows (`FUN_00481d00` display state 1).
        list_ready = true,
        -- Whether the reader was opened for a document pick-up: its exit
        -- ends the whole flow instead of returning to the selector.
        pickup = false,
        -- The selector-to-reader white fade ramp (`pickup_fade_update`,
        -- +/-0x20 per frame) and its direction.
        fade = 0,
        fade_in = false,
        fade_out = false,
        -- Loaded pages by stem (false once a load failed).
        pages = {},
        -- The readers to try per document (`reader_order`).
        readers = {},
        -- The open document's Western pages directory (`file/en`, ...), or
        -- nil for the Japanese reader.
        west = nil,
        -- Whether the selector lays its title out for the Western build.
        west_title = false,
        -- The Western page stem currently held, released on a page change.
        west_held = nil,
    }
    M.state = s
    local language = api:language()
    s.readers = reader_order(language)
    -- The title's offsets fit the session's text.
    s.west_title = language ~= nil and primary_language(language) ~= "ja"
    reader_reset()
    local item = args and args.pickup
    if item then
        -- The original finds the key-item list slot holding the picked index
        -- and enters the reader on it (`menu_update_status_screen`'s
        -- `g_pickupKeyItemList` scan).
        s.mode = "reader"
        s.pickup = true
        local index = (item - FILE_ITEM_BASE) & 0xFF
        s.book, s.slot = 0, 0
        for position = 0, BOOKS * SLOTS_PER_BOOK - 1 do
            if resolve_entry(api, KEY_ITEM_LISTS[s.character][position]) == index then
                s.book, s.slot = position // SLOTS_PER_BOOK, position % SLOTS_PER_BOOK
                break
            end
        end
        s.west = document_pages(api, listed_entry(api, s.book, s.slot))
    else
        s.slot = first_listed_slot(api, 0)
        -- The original's file task starts on the frame after the tab row's
        -- confirm; its `FUN_00481ae0` init starts the cover's entry slide.
        s.list_pending = true
        s.list_ready = false
    end
    publish(api)
end

--- Whether the original's busy lock (`state[9]`, FILE_BUSY) owns this
--- frame's input: the fade hand-off, the selector's entry slide, or the
--- reader's own lock as the previous frame left it.
local function input_locked()
    if s.fade_in or s.fade_out or s.reader_exit_request then
        return true
    end
    if s.mode == "list" then
        return s.list_pending or not s.list_ready
    end
    return s.reader_busy_frame
end

--- Publish the state the engine and its tests read back: `reader` while the
--- reader owns the screen, `idle` once its page has settled, `locked` while
--- the busy lock swallows input and the selector's `slot`.
function publish(api)
    api:set("slot", s.slot)
    api:set("reader", s.mode == "reader" and 1 or 0)
    api:set("idle", s.reader_state == "idle" and 1 or 0)
    api:set("locked", input_locked() and 1 or 0)
end

--- One step of the selector's entry slide (`FUN_00481d00` case 0).
local function step_list_entry()
    s.list_speed = s.list_speed // 2
    s.slide_y = s.slide_y + s.list_speed
    if s.slide_y >= 0 then
        s.slide_y = 0
        s.list_speed = 1
        s.list_ready = true
    end
end

--- State 3: the page eases onto the page box from the right.
local function step_reader_slide_in()
    s.reader_speed = s.reader_speed // 2
    s.reader_slide = s.reader_slide - s.reader_speed
    if s.reader_slide < 1 then
        s.reader_slide = 0
        s.reader_speed = 1
        s.reader_state = "idle"
    end
end

--- State 6: the page runs off to the left; the half-page advances once the
--- texture is fully clear.
local function step_reader_out_left()
    s.reader_slide = s.reader_slide - s.reader_speed
    s.reader_speed = s.reader_speed * 2
    if s.reader_slide < -0x127 then
        s.page = s.page + 1
        s.reader_state = "next_load"
    end
end

--- State 9: the page runs off to the right; the previous half-page is
--- entered once it is clear.
local function step_reader_out_right()
    s.reader_slide = s.reader_slide + s.reader_speed
    s.reader_speed = s.reader_speed * 2
    if s.reader_slide > 0x127 then
        if s.page > 0 then
            s.page = s.page - 1
        end
        s.reader_state = "unpack_left"
    end
end

--- State 0xc: the page eases back in from the left.
local function step_reader_slide_in_left()
    s.reader_speed = s.reader_speed // 2
    s.reader_slide = s.reader_slide + s.reader_speed
    if s.reader_slide > -1 then
        s.reader_slide = 0
        s.reader_speed = 1
        s.reader_state = "idle"
    end
end

--- Advance the reader one frame (`jpn_file_screen_render` 0x0042c080, or
--- the Western `pickup_screen_render` 0x004823a0 without the backdrop
--- ramps). Each state's step runs before the frame draws; the
--- falling-through states (2->3, 5->6, 8->9, 0xb->0xc, 0xd->0xe) are
--- single-frame steps here.
local function tick_reader()
    if s.reader_exit_request then
        -- State 0xd clears the slide and speed and falls into 0xe.
        s.reader_exit_request = false
        s.reader_state = "exit_slide"
        s.reader_slide = 0
        s.reader_speed = 1
    end
    local state = s.reader_state
    if state == "backdrop" and s.west then
        -- `pickup_screen_render` states 0/1: the arrow sheet and the page
        -- container load in one frame; nothing draws yet.
        s.reader_state = "page_load"
    elseif state == "backdrop" then
        -- States 0/1: the frame pieces ramp to 0x80; nothing draws yet.
        s.reader_backdrop = s.reader_backdrop + READER_BACKDROP_STEP
        if s.reader_backdrop >= READER_BACKDROP_FULL then
            s.reader_backdrop = READER_BACKDROP_FULL
            s.reader_state = "page_load"
        end
    elseif state == "page_load" then
        -- State 2: the half-page's TIM pages in and falls into state 3.
        s.reader_page_loaded = true
        s.reader_slide = READER_SLIDE_START
        s.reader_speed = READER_SLIDE_SPEED
        s.reader_state = "slide_in"
        step_reader_slide_in()
    elseif state == "slide_in" then
        step_reader_slide_in()
    elseif state == "idle" then
        -- State 4: the slide settled; the busy lock releases.
        s.reader_busy = false
    elseif state == "slide_out_left" then
        step_reader_out_left()
    elseif state == "next_load" then
        -- State 7: the next half-page's load frame.
        s.reader_state = "page_load"
    elseif state == "slide_out_right" then
        step_reader_out_right()
    elseif state == "unpack_left" then
        -- State 0xa: the previous half-page's unpack frame.
        s.reader_state = "prev_page"
    elseif state == "prev_page" then
        -- State 0xb: the previous half-page pages in and falls into 0xc.
        s.reader_slide = -READER_SLIDE_START
        s.reader_speed = READER_SLIDE_SPEED
        s.reader_state = "slide_in_left"
        step_reader_slide_in_left()
    elseif state == "slide_in_left" then
        step_reader_slide_in_left()
    elseif state == "exit_slide" then
        -- States 0xd/0xe: the reader runs off to the left.
        s.reader_slide = s.reader_slide - s.reader_speed
        s.reader_speed = s.reader_speed * 2
        if s.reader_slide < -0x127 then
            if s.pickup then
                -- A pick-up reader's flow ends with the slide: the engine
                -- raises the filed line and drops the layer.
                s.reader_slide = 0
                s.reader_speed = 1
                s.reader_state = "idle"
                s.reader_exit_done = true
            else
                s.reader_state = "backdrop_out"
            end
        end
    elseif state == "backdrop_out" and s.west then
        -- The Western state 0xf has no ramp: a single black frame.
        s.reader_state = "hold"
    elseif state == "backdrop_out" then
        -- State 0xf: the frame pieces ramp back down to black.
        s.reader_backdrop = s.reader_backdrop - READER_BACKDROP_STEP
        if s.reader_backdrop <= 0 then
            s.reader_backdrop = 0
            s.reader_state = "hold"
        end
    elseif state == "hold" then
        -- State 0x10: the black hold frame.
        s.reader_state = "cover_load"
    elseif state == "cover_load" then
        -- State 0x11: the selector's cover pages back in, dropping the
        -- reader's half-page with it.
        s.reader_page_loaded = false
        s.reader_state = "hand_off"
    elseif state == "hand_off" then
        -- State 0x12: the selector returns in its display state with the
        -- mode-2 fade armed, inheriting the exit slide's offset (the state
        -- block's slide pair is shared) until the fade completes.
        s.mode = "list"
        s.slide_x = s.reader_slide
        s.slide_y = 0
        s.list_speed = 1
        s.list_ready = true
        s.fade = 0xFF
        s.fade_out = true
    end
end

--- Advance one frame: the selector's slides or the reader's state machine,
--- plus the blink counter and the hand-off fades.
local function tick(api)
    -- The original checks the busy lock before the render step that
    -- maintains it, so this frame's input sees the previous frame's lock.
    s.reader_busy_frame = s.reader_busy
    if s.fade_in then
        -- The hand-off fades the screen over eight frames; the frame the ramp
        -- completes on still draws black under the full overlay.
        s.fade = s.fade + 0x20
        if s.fade >= 0xFF then
            s.fade = 0
            s.fade_in = false
            -- The reading mode's first frame is the renderer's state 0.
            s.mode = "reader"
            s.page = 0
            s.west = document_pages(api, listed_entry(api, s.book, s.slot))
            reader_reset()
        end
        return
    end
    if s.fade_out then
        s.fade = s.fade - 0x20
        if s.fade <= 0 then
            s.fade = 0
            s.fade_out = false
            -- `pickup_fade_update(1)` clears the slide pair as it completes.
            s.slide_x = 0
            s.slide_y = 0
        end
    end
    if s.mode == "list" then
        if s.list_pending then
            -- The task's first frame arms the entry slide and steps it once.
            s.list_pending = false
            s.slide_y = LIST_SLIDE_START
            s.list_speed = LIST_SLIDE_SPEED
            s.list_ready = false
            step_list_entry()
        elseif not s.list_ready then
            step_list_entry()
        end
        if s.slide_x > 0 then
            s.slide_x = s.slide_x > 9 and s.slide_x - 9 or 0
        elseif s.slide_x < 0 then
            s.slide_x = s.slide_x < -9 and s.slide_x + 9 or 0
        end
    else
        tick_reader()
    end
    -- The blink counter only runs in the selector's display state and in the
    -- reader's settled state 4.
    local blinking
    if s.mode == "list" then
        blinking = s.list_ready
    else
        blinking = s.reader_state == "idle"
    end
    if blinking then
        s.blink = (s.blink + 1) & 0xFF
    end
end

local function list_input(api, input)
    -- The selector plays the cursor blip (character slot 4) only on a move
    -- that lands, and the decide chime (slot 6) on the reader hand-off; the
    -- cancel cue (slot 5) belongs to the tab's exit (`FUN_00481b90` and
    -- `menu_file_state_machine` 0x00481980).
    if input.cancel then
        api:sound("cancel")
        api:exit()
    elseif input.up then
        if s.slot ~= 0xFF then
            local slot = previous_listed_slot(api, s.book, s.slot)
            if slot then
                s.slot = slot
                api:sound("cursor")
            end
        end
    elseif input.down then
        if s.slot ~= 0xFF then
            local slot = next_listed_slot(api, s.book, s.slot)
            if slot then
                s.slot = slot
                api:sound("cursor")
            end
        end
    elseif input.left then
        if s.book ~= 0 then
            s.book = 0
            s.slot = first_listed_slot(api, 0)
            s.slide_x = -0x78
            api:sound("cursor")
        end
    elseif input.right then
        if s.book + 1 < BOOKS then
            s.book = 1
            s.slot = first_listed_slot(api, 1)
            s.slide_x = 0x78
            api:sound("cursor")
        end
    elseif input.confirm then
        if s.slot ~= 0xFF then
            -- MODE byte 1: the fade ramps over the selector from the confirm
            -- frame and the reading mode starts when it reaches full.
            s.fade_in = true
            s.fade = 0
            api:sound("decide")
        end
    end
end

local function reader_input(api, input)
    local entry = listed_entry(api, s.book, s.slot)
    if entry == nil then
        s.mode = "list"
        return
    end
    -- The reader turns half-pages with the D-pad alone: right pages down,
    -- left pages up. The last half-page has to be acknowledged by a further
    -- page-down, which sets FILE_SAVEFLAG and lights the EXIT label.
    --
    -- The reference's `jpn_file_list_input` plays the page-turn sample
    -- (character slot 8) on a turn and the cursor blip (slot 4) when the
    -- last-page acknowledgement is raised or withdrawn; a silent edge plays
    -- nothing. The cancel and the last-page confirm play the cancel cue
    -- (slot 5) from the tab's state machine.
    if input.right then
        if s.page + 1 < page_count(entry) then
            s.reader_busy = true
            s.reader_slide = 0
            s.reader_speed = 1
            s.reader_state = "slide_out_left"
            -- The input state falls through into the slide.
            step_reader_out_left()
            api:sound("page_turn")
        elseif not s.reader_last_page then
            s.reader_last_page = true
            api:sound("cursor")
        end
    elseif input.left then
        if s.reader_last_page then
            s.reader_last_page = false
            api:sound("cursor")
        elseif s.page > 0 then
            s.reader_busy = true
            s.reader_slide = 0
            s.reader_speed = 1
            s.reader_state = "slide_out_right"
            step_reader_out_right()
            api:sound("page_turn")
        end
    elseif input.cancel or (input.confirm and s.reader_last_page) then
        -- A cancel always slides the reader out; a confirm only once the
        -- last half-page has been acknowledged. The request is applied on
        -- the next frame, so this frame still draws the settled page.
        s.reader_busy = true
        s.reader_exit_request = true
        api:sound("cancel")
    end
end

--- Whether the frame carries a press: the reader only re-checks its document
--- on one.
local function pressed(input)
    return input.up or input.down or input.left or input.right or input.confirm
        or input.cancel or input.page_left or input.page_right
end

function M.update(api, input)
    tick(api)
    if pressed(input) and not input_locked() then
        if s.mode == "list" then
            list_input(api, input)
        else
            reader_input(api, input)
        end
    end
    publish(api)
    if s.reader_exit_done then
        s.reader_exit_done = false
        api:exit()
    end
end

-- ---- drawing ---------------------------------------------------------------

--- Load one page once, warning on the first failure only.
local function page(api, stem)
    local entry = s.pages[stem]
    if entry == nil then
        local handle, err = api:page(stem)
        if handle then
            entry = handle
        else
            api:log(("warning: FILE tab art unavailable: %s"):format(err))
            entry = false
        end
        s.pages[stem] = entry
    end
    return entry or nil
end

--- An unscaled page rect at full brightness.
local function blit(api, art, u, v, w, h, x, y)
    api:page_sprite_mirrored(art, u, v, w, h, x, y, w, h, 0, 0, false, false)
end

local function draw_uv(api, art, uv, x, y)
    blit(api, art, uv[1], uv[2], uv[3], uv[4], x, y)
end

local function slot_position(slot)
    return SLOT_X + (slot >> 1), SLOT_Y + slot * SLOT_STEP
end

--- The arrows' and the other book indicator's blink phase.
local function blink_phase()
    return (s.blink & 0x30) == 0 and 1 or 0
end

local function draw_list(api, ox, oy)
    if s.list_pending then
        -- The tab's first frame: only the panel underneath is drawn.
        return
    end
    local cover = page(api, s.book == 1 and "file/file001" or "file/file000")
    if not cover then
        return
    end
    blit(api, cover, 0, 0, COVER_W, COVER_H, ox + COVER_X + s.slide_x, oy + COVER_Y + s.slide_y)
    -- Only the display state draws the book indicators, the title and the
    -- scroll arrows; the entry slide and the book changes draw just the
    -- cover, slots and cursor.
    local blink = blink_phase()
    if s.list_ready then
        -- The current book's indicator is hidden; the other blinks.
        if s.book ~= 0 then
            blit(api, cover, 0xB0 + blink * 8, 0, 8, 8, ox + 0x3C, oy + 0x48)
        end
        if s.book ~= 1 then
            blit(api, cover, 0xC0 + blink * 8, 0, 8, 8, ox + 0x9C, oy + 0x48)
        end
    end
    -- Seen document icons and the cursor arrow follow the cover's slide.
    for slot = 0, SLOTS_PER_BOOK - 1 do
        if listed_entry(api, s.book, slot) then
            local x, y = slot_position(slot)
            draw_uv(api, cover, SLOT_ICON_UV, ox + x + s.slide_x, oy + y + s.slide_y)
        end
    end
    if s.slot == 0xFF then
        return
    end
    local x, y = slot_position(s.slot)
    draw_uv(api, cover, CURSOR_UV, ox + x + s.slide_x, oy + y + s.slide_y)
    local entry = listed_entry(api, s.book, s.slot)
    if entry == nil or not s.list_ready then
        return
    end
    local name = api:item_name_bytes((entry + FILE_ITEM_BASE) & 0xFF)
    if name then
        local x = (s.west_title and WEST_TITLE_X or TITLE_X)[entry]
        local y = s.west_title and WEST_TITLE_Y or JPN_TITLE_Y
        api:glyphs(ox + x + 0x1E, oy + y, name, { brightness = 0 })
    end
    -- Scroll arrows follow the cursor through the seen slots.
    local scroll_x = ox + (s.west_title and WEST_SCROLL_X or JPN_SCROLL_X)
    if previous_listed_slot(api, s.book, s.slot) then
        blit(api, cover, 0xD0 + blink * 8, 0, 8, 8, scroll_x, oy + 0xB7)
    end
    if next_listed_slot(api, s.book, s.slot) then
        blit(api, cover, 0xE0 + blink * 8, 0, 8, 8, scroll_x, oy + 0xCF)
    end
end

--- Hold one Western page: release the previously held page's art first,
--- since the documents' 84 pages would otherwise fill the screen's art
--- cache.
local function hold_west_page(api, stem)
    if s.west_held ~= stem and s.west_held ~= nil then
        local handle = s.pages[s.west_held]
        if handle then
            api:release(handle)
        end
        s.pages[s.west_held] = nil
    end
    s.west_held = stem
    if stem then
        return page(api, stem)
    end
    return nil
end

--- The EXIT label, lit once the last page has been acknowledged. The
--- reference's colour multiplier is 0x50 / 0x20 over a 0x80 unity, so the
--- drawn scale is twice the byte.
local function draw_exit(api, entry, ox, oy)
    if s.page + 1 >= page_count(entry) then
        local status = page(api, "ui/status")
        if status then
            local scale = (s.reader_last_page and 0x50 or 0x20) * 2
            api:page_sprite_scaled(status, 0x80, 0x88, 0x18, 0x10,
                ox + EXIT_X, oy + EXIT_Y, 0x18, 0x10, 0, scale)
        end
    end
end

--- The Western reader (`pickup_screen_render`): one whole page, the arrows
--- from `file/arror` and the EXIT label.
local function draw_west_reader(api, entry, ox, oy)
    local sheet = hold_west_page(api, s.reader_page_loaded and west_page_stem(s.west, entry, s.page) or nil)
    if sheet then
        blit(api, sheet, 0, 0, WEST_PAGE_W, WEST_PAGE_H, ox + WEST_PAGE_X + s.reader_slide, oy + WEST_PAGE_Y)
    end
    if s.reader_state ~= "idle" then
        return
    end
    local arrows = page(api, "file/arror")
    if arrows then
        local blink = s.reader_last_page and 1 or blink_phase()
        if s.page ~= 0 then
            blit(api, arrows, blink * 8, 0, 8, 8, ox + PREV_ARROW_X, oy + PREV_ARROW_Y)
        end
        blit(api, arrows, 0x10 + blink * 8, 0, 8, 8, ox + NEXT_ARROW_X, oy + NEXT_ARROW_Y)
    end
    draw_exit(api, entry, ox, oy)
end

local function draw_reader(api, ox, oy)
    local entry = listed_entry(api, s.book, s.slot)
    if entry == nil then
        return
    end
    if s.west then
        draw_west_reader(api, entry, ox, oy)
        return
    end
    local backdrop = page(api, "file/" .. FILEI_NAMES[FILEI_INDEX[entry]])
    if s.reader_page_loaded then
        if backdrop then
            -- The six FILEI pieces draw semi-transparent with the ramp byte
            -- as their colour: the fully ramped pieces sit at 50%.
            for _, piece in ipairs(READER_FRAME_PIECES) do
                api:page_ramp(backdrop, piece[1], piece[2], piece[3], piece[4],
                    ox + piece[5], oy + piece[6], s.reader_backdrop)
            end
        end
        local index = page_index(entry, s.page)
        local sheet = index and page(api, "file/" .. TEXTM_NAMES[index])
        if sheet then
            local half = s.page % 2 == 0 and 0 or 0x80
            blit(api, sheet, 0, half, PAGE_W, PAGE_H, ox + PAGE_X + s.reader_slide, oy + PAGE_Y)
        end
    end
    -- The arrows, the blink and the EXIT label belong to the settled state 4.
    if s.reader_state ~= "idle" or not backdrop then
        return
    end
    -- The arrows freeze lit once the last half-page is acknowledged.
    local blink = s.reader_last_page and 1 or blink_phase()
    -- Both arrows are cut from the FILEI art's bottom-right strip; each blink
    -- frame is its own eight-pixel column.
    if s.page ~= 0 then
        blit(api, backdrop, 0xE0 + blink * 8, READER_ARROW_V, 8, 8, ox + PREV_ARROW_X, oy + PREV_ARROW_Y)
    end
    blit(api, backdrop, 0xF0 + blink * 8, READER_ARROW_V, 8, 8, ox + NEXT_ARROW_X, oy + NEXT_ARROW_Y)
    draw_exit(api, entry, ox, oy)
end

--- Draw the tab over the framebuffer's current contents.
function M.draw(api)
    local width, height = api:width(), api:height()
    local ox, oy = width // 2 - 160, height // 2 - 120
    if s.mode == "list" then
        -- The selector draws over the inventory panel but under the panel's
        -- masking rects and frame, so it is clipped to the panel's area.
        -- The reader's last page goes with it.
        hold_west_page(api, nil)
        api:clip(ox + SCREEN_AREA[1], oy + SCREEN_AREA[2], SCREEN_AREA[3], SCREEN_AREA[4])
        draw_list(api, ox, oy)
        api:clip()
    else
        -- The reader's own layer draws over the chrome on an opaque black.
        api:fill_rect(0, 0, width, height, 0, 0, 0)
        draw_reader(api, ox, oy)
    end
    if s.fade > 0 then
        -- The fade rect is an opaque black whose alpha is the ramp byte.
        api:darken(s.fade)
    end
end

return M
