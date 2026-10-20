-- The original's full-screen fade accumulator.
--
-- A fade carries a type (1 white, 2 black), a per-tick counter and a 16-bit
-- state. Each tick adds the counter to a non-negative state with 16-bit
-- wrapping; the overlay alpha is `state >> 7`, and a negative state means no
-- overlay. `fade_out(value)` steps up by `value` alpha a tick, `fade_in` down.

local Fade = {}
Fade.__index = Fade

--- Wrap an integer to a signed 16-bit value.
local function i16(value)
    return ((value + 0x8000) & 0xFFFF) - 0x8000
end

--- No fade: the accumulator starts negative.
function Fade.inactive()
    return setmetatable({ type = 0, counter = 0, state = -1 }, Fade)
end

--- A fade to black/white: `counter = value << 7`.
function Fade.fade_out(value, state, fade_type)
    return setmetatable({ type = fade_type, counter = value << 7, state = state }, Fade)
end

--- A fade from black/white: `counter = -(value << 7)`.
function Fade.fade_in(value, state, fade_type)
    return setmetatable({ type = fade_type, counter = -(value << 7), state = state }, Fade)
end

--- Latch fade values verbatim; the counter is truncated to 16 bits on add.
function Fade:set(fade_type, counter, state)
    self.type = fade_type
    self.counter = counter
    self.state = state
end

--- Advance one tick. A negative accumulator stays put.
function Fade:tick()
    if self.state >= 0 then
        self.state = i16(self.state + i16(self.counter))
    end
end

--- Whether the accumulator is non-negative (the overlay draws).
function Fade:active()
    return self.state >= 0
end

--- The overlay colour name and alpha for `state`, or nil.
local function overlay_of(fade_type, state)
    if state < 0 then
        return nil
    end
    local alpha = state >> 7
    if alpha > 255 then
        alpha = 255
    end
    return fade_type == 2 and "black" or "white", alpha
end

--- This frame's overlay: colour name and alpha, or nil.
function Fade:overlay()
    return overlay_of(self.type, self.state)
end

--- The overlay the original draws this frame: the value before this tick's
--- step (the main loop draws the fade, then adds the counter).
function Fade:drawn_overlay()
    return overlay_of(self.type, i16(self.state - i16(self.counter)))
end

--- Publish this fade as the screen's overlay.
function Fade:present(api)
    api:overlay(self:overlay())
end

return Fade
