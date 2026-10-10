-- Forest's sprawled corpse. A corpse prop.
local npc = require("lib/npc")
local data = {
    radius = 500,
    corpse = true,
    idle0_plays = true,
    rebecca = false,
    wesker = false,
    sca = { 500, 0xB4, 0x258, -0xB4, -0xC8 },
}

function update(e)
    npc.update(e, data)
end
