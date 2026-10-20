-- The plate slots the RESULT and epilogue screens share, and the credit
-- table they are laid out from.
--
-- A slot is one plate (a 256-pixel-wide row band of the en05/en07 page) or
-- one text string, with its own fade ramp: the reference's signed 0x800
-- ramps, whose high byte is the 0..0x80 shade. Text slots draw through the
-- reference's own font grid addressing (`api:grid_text`), not the encoded
-- text path.

local E = {}

-- The packed plate width: every en05/en07 plate is 256 pixels wide.
E.PLATE_WIDTH = 256
-- The per-frame slot fade step.
E.FADE_STEP = 0x800
-- The saturated fade level.
E.FADE_FULL = 0x8000
-- The Jill layout shift on the background rows, in pixels (0x9C0000 >> 16).
E.JILL_LAYOUT_SHIFT = 0x9C

-- The RESULT/epilogue credit table (s_creditEntries, EndingScreen.cpp:127).
-- Entries 1-5 are the background layout (endings 4-7), 8-12 the plain layout
-- (endings 1-3); entries 6-7 are the epilogue's two captions. `kind` is
-- "sprite", "clear_time" or "save_count".
E.CREDIT_ENTRIES = {
    { kind = "sprite", x = 0x00, y = 0x30, tex_v = 0x00, height = 0x18 },
    { kind = "clear_time", x = 0x30, y = 0x50, tex_v = 0x00, height = 0x00 },
    { kind = "sprite", x = 0x00, y = 0x74, tex_v = 0x18, height = 0x18 },
    { kind = "save_count", x = 0x48, y = 0x94, tex_v = 0x00, height = 0x00 },
    { kind = "sprite", x = 0x25, y = 0x66, tex_v = 0x30, height = 0x28 },
    { kind = "sprite", x = 0x20, y = 0x20, tex_v = 0x58, height = 0x30 },
    { kind = "sprite", x = 0x20, y = 0xC0, tex_v = 0x88, height = 0x20 },
    { kind = "sprite", x = 0x20, y = 0x16, tex_v = 0x00, height = 0x36 },
    { kind = "clear_time", x = 0x80, y = 0x58, tex_v = 0x00, height = 0x00 },
    { kind = "sprite", x = 0x20, y = 0x7A, tex_v = 0x36, height = 0x36 },
    { kind = "save_count", x = 0x98, y = 0xB8, tex_v = 0x00, height = 0x00 },
    { kind = "sprite", x = 0x25, y = 0x66, tex_v = 0x30, height = 0x28 },
}

--- The RESULT layout's first entry: 1-5 for the background rows, 8-12 for
--- the plain rows (EndingScreen.cpp:443).
function E.credit_base(background)
    return background and 1 or 8
end

--- The 0..0x80 shade a fade level contributes: the high byte of the signed
--- 16-bit ramp (EndingScreen.cpp:352).
function E.fade_shade(level)
    if level < 0 then
        level = 0
    elseif level > E.FADE_FULL then
        level = E.FADE_FULL
    end
    return (level >> 8) & 0xFF
end

--- The 0..255 colour multiplier a PSX 0x80 (full) shade maps to.
function E.shade_scale(shade)
    local scale = shade * 255 // 0x80
    return scale < 255 and scale or 255
end

--- The offset that maps the reference's 320x240 layout onto the screen
--- centre: its slot x/y carry the -0xA0/-0x78 centring and the sprite path
--- adds the screen origin back.
function E.origin(api)
    return api:width() // 2 - 0xA0, api:height() // 2 - 0x78
end

local Slot = {}
Slot.__index = Slot

--- A hidden slot from a credit-table entry, moved by `dx` pixels.
function E.slot(entry, dx)
    return setmetatable({
        active = false,
        kind = entry.kind,
        x = entry.x + (dx or 0),
        y = entry.y,
        tex_v = entry.tex_v,
        height = entry.height,
        fade_level = 0,
        fade_vel = 0,
        -- The ending-6 fifth plate's additive glow (drawn opaque).
        additive = false,
    }, Slot)
end

--- Advance the fade ramp one frame, saturating like the reference's signed
--- 16-bit ramp (EndingScreen.cpp:342-350).
function Slot:step_fade()
    if not self.active or self.fade_vel == 0 then
        return
    end
    self.fade_level = self.fade_level + self.fade_vel
    if self.fade_level <= 0 then
        self.fade_level = 0
        self.fade_vel = 0
    elseif self.fade_level >= E.FADE_FULL then
        self.fade_level = E.FADE_FULL
        self.fade_vel = 0
    end
end

function Slot:shade()
    return E.fade_shade(self.fade_level)
end

--- Draw a sprite slot from the `plate` page, or a text slot's `text`.
function Slot:draw(api, plate, text)
    if not self.active or self.fade_level == 0 then
        return
    end
    local ox, oy = E.origin(api)
    local x, y = self.x + ox, self.y + oy
    local scale = E.shade_scale(self:shade())
    if self.kind == "sprite" then
        if plate then
            api:page_sprite_scaled(
                plate,
                0, self.tex_v, E.PLATE_WIDTH, self.height,
                x, y, E.PLATE_WIDTH, self.height,
                0, scale
            )
        end
    elseif text then
        api:grid_text(x, y, scale, text)
    end
end

--- Clear the frame to black.
function E.black(api)
    api:clear()
    api:fill_rect(0, 0, api:width(), api:height(), 0, 0, 0, 255)
end

return E
