-- Enrico, sprawled. Shared NPC driver data.
local npc = require("lib/npc")
local data = {
    radius = 500,
    corpse = false,
    idle0_plays = false,
    rebecca = false,
    wesker = false,
    sca = { 500, 0xB4, 0x258, -0xB4, 0 },
}

function update(e)
    npc.update(e, data)
end
