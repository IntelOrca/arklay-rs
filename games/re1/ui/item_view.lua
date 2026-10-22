-- The item model viewer (the original's `FUN_0044e1b0`): the pause menu's
-- CHECK examine and the pick-up flows' spinning model.
--
-- The screen owns the viewer's stage machine, the rotation arrows and the
-- item name. The engine keeps the game rules: `api:examine_spin` re-extracts
-- the turntable pose, `api:examine_check` runs the examine check and the
-- engine applies what the screen publishes (the messages, the examined
-- bits and the doom books' medal swap) through the game's message window.
--
-- Open arguments:
--   item      the item id to show.
--   quantity  the examined slot's quantity (the loaded-weapon message).
--   pickup    true for a ground/got-item pick-up: the model spins in, the
--             screen publishes `settled` and waits; the engine calls
--             `M.begin_exit` once its prompt resolves and the screen exits
--             when the closing animation has run. A pick-up reads no input.
--
-- Without `pickup` the screen is the examine viewer. Each update publishes
-- `event` (0 none, 1 show the item's description, 2 show global message
-- `event_id`, marking item `event_mark` examined and selecting
-- `event_select` first when non-zero, 3 set examined bit `event_id`, 4 the
-- book turned into medal `event_id`), plus `item` and `ready` (the turntable
-- is live). It exits once its closing animation has finished.
--
-- The model renders with the viewer's three fixed lights over a dim
-- ambient. The intro ramps the lights up from black and the exit ramps them
-- back down; the screen itself never fades. While the model rests,
-- left/right turn it about the screen's vertical axis and up/down tip it,
-- `0x20` 12-bit units per 30 Hz frame while the key is held.

local M = {}

-- Viewer camera: on the +X axis looking at the origin, projected about the
-- menu's (112, 76) centre (`SetSubpixelOffset(112, 76)`).
local CAMERA = {
    from = { 15000, 0, 0 },
    to = { 0, 0, 0 },
    fov = 0xC0,
    screen = { 112 - 160, 76 - 120 },
}
-- The model's resting translation toward the camera.
local MODEL_REST_X = 0x1980
-- Rotation step per frame, in 12-bit angle units.
local ROTATE_STEP = 0x20
-- The intro/exit duration and its per-frame spin, roll and zoom steps.
local INTRO_FRAMES = 0x40
local INTRO_YAW_STEP = 0xC0
local INTRO_ROLL_STEP = 0x80
local INTRO_ZOOM_STEP = 0x322
local INTRO_START_X = MODEL_REST_X - INTRO_FRAMES * INTRO_ZOOM_STEP
-- `setBackColor(0x199, 0x199, 0x199)` and the three light positions
-- (`main_menu`'s `DAT_00d22700`): upper left, upper right and below.
local AMBIENT = 0x199
local LIGHTS = { { 100, 0x50, 0x50 }, { -100, 0x50, 0x50 }, { 0, -100, 0x50 } }
-- The book zoom opens its message once object 1's yaw passes this.
local ZOOM_TARGET = 0x300
-- The status-sheet arrows (`FUN_0044eca0`) in draw order: left, down,
-- right, up, as screen position, sheet `v` and the held key that lights it.
local ARROWS = {
    { 0x22, 0x49, 0x7C, "left" },
    { 0x6D, 0x76, 0x70, "down" },
    { 0xB6, 0x49, 0x64, "right" },
    { 0x6D, 0x1A, 0x58, "up" },
}
local ARROW_LIT_U = 0x80
local ARROW_IDLE_U = 0x8C
local ARROW_SIZE = 9
-- The item-name line.
local ITEM_NAME_Y = 0xBA
-- The status panel's viewport inside its frame, `{x, y, w, h}`: the examine
-- model is drawn behind the frame, so nothing of it shows past the frame's
-- inner edge.
local VIEWPORT = { 32, 24, 160, 104 }
-- The standalone viewer's navy backdrop behind the examine model, `{r, g, b}`
-- (Classic REbirth shows srgb(0, 0, 32) there while the viewer is up). The
-- viewer opened over the pause menu is an overlay on the panel's black
-- instead: the reference's radio/CHECK view draws the same model, but its
-- viewport is black there.
local BACKDROP = { 0, 0, 0x20 }
-- The red book, the second doom book and the medal the first one becomes.
local ITEM_RED_BOOK = 0x3E
local ITEM_DOOM_BOOK_2 = 0x3F
local ITEM_FIRST_AID_SPRAY = 0x41
local ITEM_WOLF_MEDAL = 0x24
-- Examined bit the red book's read zoom sets.
local RED_BOOK_READ_BIT = 0x0D

local EVENT_DESCRIPTION = 1
local EVENT_MESSAGE = 2
local EVENT_MARK_BIT = 3
local EVENT_MEDAL = 4

local s = {}
M.state = s

local function is_doom_book(item)
    return ITEM_RED_BOOK < item and item < ITEM_FIRST_AID_SPRAY
end

local function load_model(api)
    local handle, err = api:item_model(s.item)
    if handle then
        s.model = handle
        s.objects = api:model_objects(handle)
    else
        api:log(("warning: item view model unavailable: %s"):format(err))
        s.model = nil
        s.objects = 1
    end
end

-- The setup state (state 0): park the model far away on the identity pose
-- and arm the intro.
local function restart_intro()
    s.pitch, s.yaw, s.roll = 0, 0, 0
    s.obj_rot = { { 0, 0, 0 }, { 0, 0, 0 } }
    s.obj0_sca = nil
    s.model_x = INTRO_START_X
    s.stage = "intro"
    s.timer = INTRO_FRAMES
end

-- The render's `& 0xFFF` on the three base angles.
local function mask_angles()
    s.pitch = s.pitch & 0x0FFF
    s.yaw = s.yaw & 0x0FFF
    s.roll = s.roll & 0x0FFF
end

-- The render's per-object matrix build (`FUN_0044ea50`): object 0 draws with
-- the angles it already holds, then takes the current ones, so it always
-- shows the previous frame's.
local function latch_render()
    s.obj0_draw = s.obj0_sca
    local o = s.obj_rot[1]
    s.obj0_sca = { o[1], o[2], o[3] }
end

-- State 1: `0x40` frames of zoom, spin and roll while the lights ramp up.
-- Returns true once the model rests.
local function step_intro()
    if s.stage == "setup" then
        restart_intro()
    end
    if s.stage ~= "intro" then
        return true
    end
    s.model_x = s.model_x + INTRO_ZOOM_STEP
    s.yaw = s.yaw + INTRO_YAW_STEP
    s.roll = s.roll + INTRO_ROLL_STEP
    s.timer = s.timer - 1
    s.light = (0xFF - (s.timer << 2)) & 0xFF
    mask_angles()
    latch_render()
    if s.timer > 0 then
        return false
    end
    s.stage = "display"
    s.action = "turn"
    return true
end

-- One frame of the zoom-out the exit and the medal swap share.
local function step_out()
    s.model_x = s.model_x - INTRO_ZOOM_STEP
    s.yaw = s.yaw - INTRO_YAW_STEP
    s.roll = s.roll - INTRO_ROLL_STEP
    s.light = ((s.timer << 2) - 1) & 0xFF
    mask_angles()
end

-- State 5: the intro in reverse, dimming the lights. The frame the timer
-- runs out draws nothing; returns true then.
local function step_exit()
    if s.stage ~= "exit" then
        return true
    end
    s.timer = s.timer - 1
    if s.timer <= 0 then
        s.stage = "closed"
        return true
    end
    step_out()
    latch_render()
    return false
end

local function event(api, kind, id, mark, select)
    api:set("event", kind)
    api:set("event_id", id or 0)
    api:set("event_mark", mark or 0)
    api:set("event_select", select or 0)
end

local function held(input, key, bit)
    return input[key] or (input.held & bit) ~= 0
end

-- The display state's action table (`PTR_FUN_004bf310`). Returns true when
-- the turntable confirm should run the examine check.
local function step_action(api, input)
    local up, down = held(input, "up", 0x10), held(input, "down", 0x20)
    local left, right = held(input, "left", 0x40), held(input, "right", 0x80)
    if s.action == "turn" then
        if input.confirm then
            return true
        end
        s.arrows = true
        s.held = { up = up, right = right, down = down, left = left }
        if input.cancel then
            s.stage = "exit"
            s.timer = INTRO_FRAMES
            return false
        end
        -- Left/right drive the spin's y (yaw) axis, up/down its z axis; the
        -- x axis belongs to the shift-held roll the port leaves out.
        local yaw = 0
        if left then
            yaw = ROTATE_STEP
        elseif right then
            yaw = -ROTATE_STEP
        end
        local tip = 0
        if up then
            tip = -ROTATE_STEP
        elseif down then
            tip = ROTATE_STEP
        end
        if yaw ~= 0 or tip ~= 0 then
            s.pitch, s.yaw, s.roll = api:examine_spin(s.pitch, s.yaw, s.roll, 0, yaw, tip)
        end
    elseif s.action == "zoom" then
        s.timer = s.timer + 1
        local step = (s.timer * 4) & 0xFF
        if step > 0x20 then
            step = 0x20
        end
        local spin, counter = s.obj_rot[2], s.obj_rot[1]
        spin[2] = (spin[2] + step) & 0x0FFF
        counter[2] = (counter[2] - step * 2) & 0x0FFF
        if spin[2] > ZOOM_TARGET then
            s.action = "message"
            local id = api:class_message(s.item)
            if id then
                event(api, EVENT_MESSAGE, id)
            end
            return false
        end
        s.show_name = true
    elseif s.action == "message" then
        if api:message_active() then
            return false
        end
        local spun = s.obj_rot[2][2] ~= 0
        if s.item == ITEM_RED_BOOK then
            s.action = "turn"
            if spun then
                event(api, EVENT_MARK_BIT, RED_BOOK_READ_BIT)
            end
        elseif is_doom_book(s.item) and spun then
            s.action = "medal"
            s.timer = INTRO_FRAMES
        else
            s.action = "turn"
        end
    elseif s.action == "medal" then
        s.timer = s.timer - 1
        step_out()
        if s.timer ~= 0 then
            return false
        end
        -- The book becomes its medal and the viewer restarts on it; the
        -- action stays on the swap so the intro shows no name.
        s.item = s.item - ITEM_DOOM_BOOK_2 + ITEM_WOLF_MEDAL
        s.quantity = 1
        s.stage = "setup"
        load_model(api)
        event(api, EVENT_MEDAL, s.item)
    end
    return false
end

-- The examine check (`FUN_0044ed40`) for the resting pose: a zoom starts
-- here, the other outcomes wait for their message to close.
local function confirm(api)
    local kind, id, mark, select =
        api:examine_check(s.item, s.pitch, s.yaw, s.roll, s.obj_rot[2][2], s.quantity)
    if kind == "zoom" then
        s.action = "zoom"
        s.timer = 0
        -- The confirm frame names the item once more.
        s.show_name = true
        return
    end
    s.action = "message"
    s.show_name = false
    if kind == "description" then
        event(api, EVENT_DESCRIPTION)
    else
        event(api, EVENT_MESSAGE, id, mark and s.item or 0, select)
    end
end

-- One examine frame (`FUN_0044e1b0` in the status-screen mode).
local function step_examine(api, input)
    s.arrows = false
    s.show_name = false
    local check = false
    if s.stage == "setup" or s.stage == "intro" then
        step_intro()
    elseif s.stage == "display" then
        check = step_action(api, input)
        -- The medal swap parks the viewer on its setup state, which draws
        -- nothing until the next frame's intro.
        if s.stage ~= "setup" then
            latch_render()
        end
    elseif s.stage == "exit" then
        if step_exit() then
            api:exit()
            return
        end
    else
        api:exit()
        return
    end
    if s.action == "turn" then
        s.show_name = true
    end
    if check then
        confirm(api)
    end
end

local function publish(api)
    api:set("item", s.item)
    api:set("ready", s.stage == "display" and s.action == "turn")
    api:set("settled", s.settled)
end

function M.open(api, args)
    s.item = args.item or 0
    s.quantity = args.quantity or 0
    s.pickup = args.pickup or false
    s.menu = args.menu or false
    -- The examine viewer draws on black, so a wider aspect gives it a wider
    -- canvas; the pickup viewer draws in the inventory panel, which asks for
    -- the canvas itself.
    api:set("canvas", s.pickup and 0 or 1)
    s.pitch, s.yaw, s.roll = 0, 0, 0
    s.model_x = MODEL_REST_X
    s.stage = "setup"
    s.action = "turn"
    s.timer = 0
    s.obj_rot = { { 0, 0, 0 }, { 0, 0, 0 } }
    s.obj0_sca = nil
    s.obj0_draw = nil
    s.light = 0
    s.held = {}
    s.arrows = false
    s.show_name = false
    s.settled = false
    s.status = nil
    if s.pickup then
        restart_intro()
    end
    load_model(api)
    publish(api)
end

-- The pick-up flow's prompt resolved: start the closing animation.
function M.begin_exit(api)
    s.stage = "exit"
    s.timer = INTRO_FRAMES
end

function M.update(api, input)
    event(api, 0)
    if s.pickup then
        if s.stage == "setup" or s.stage == "intro" then
            s.settled = step_intro()
        elseif step_exit() and s.stage == "closed" then
            api:exit()
        end
    else
        step_examine(api, input)
    end
    publish(api)
end

local function draw_model(api)
    if s.stage == "setup" or s.stage == "closed" or not s.model then
        return
    end
    local o0, o1 = s.obj0_draw, s.obj_rot[2]
    local root = { rx = s.pitch, ry = s.yaw, rz = s.roll, tx = s.model_x }
    local spin = { rx = o1[1], ry = o1[2], rz = o1[3] }
    -- Object 0 is `root * R(obj1) * R(obj0, last frame)` and object 1
    -- `root * R(obj1)`; a one-object model skips the lagged rotation, and any
    -- further object (none ship) rides the root.
    local objects
    if s.objects >= 2 then
        local lagged = { root, spin }
        if o0 then
            lagged[3] = { rx = o0[1], ry = o0[2], rz = o0[3] }
        end
        objects = { lagged, { root, spin } }
    else
        objects = { { root, spin } }
    end
    local sources = {}
    for index, pos in ipairs(LIGHTS) do
        sources[index] = { pos = pos, color = { s.light, s.light, s.light } }
    end
    if not s.pickup then
        local ox, oy = api:width() // 2 - 160, api:height() // 2 - 120
        api:clip(ox + VIEWPORT[1], oy + VIEWPORT[2], VIEWPORT[3], VIEWPORT[4])
    end
    api:draw_ivm(s.model, {
        joints = { root },
        objects = objects,
        camera = CAMERA,
        light = { ambient = AMBIENT, sources = sources },
    })
    api:clip()
end

function M.draw(api)
    local ox, oy = api:width() // 2 - 160, api:height() // 2 - 120
    if not s.pickup and s.stage ~= "setup" and s.stage ~= "closed" then
        local backdrop = s.menu and { 0, 0, 0 } or BACKDROP
        api:fill_rect(ox + VIEWPORT[1], oy + VIEWPORT[2], VIEWPORT[3], VIEWPORT[4],
            backdrop[1], backdrop[2], backdrop[3])
    end
    draw_model(api)
    if s.pickup then
        return
    end
    if s.arrows then
        if s.status == nil then
            s.status = api:page("ui/status") or false
        end
        if s.status then
            for _, arrow in ipairs(ARROWS) do
                local u = s.held[arrow[4]] and ARROW_LIT_U or ARROW_IDLE_U
                api:page_sprite_mirrored(s.status, u, arrow[3], ARROW_SIZE, ARROW_SIZE,
                    ox + arrow[1], oy + arrow[2], ARROW_SIZE, ARROW_SIZE, 0, 0, false, false)
            end
        end
    end
    if s.show_name then
        local name = api:item_name_bytes(s.item, true)
        if name then
            api:glyphs(ox + api:font_margin(), oy + ITEM_NAME_Y, name, { brightness = 0 })
        end
    end
end

return M
