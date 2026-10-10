-- Kenneth, devoured. A corpse prop.
local npc = require("lib/npc")
local data = {
    radius = 422,
    corpse = true,
    idle0_plays = true,
    rebecca = false,
    wesker = false,
    sca = { 422, 0x5FA, 0, -0x5FA, 0 },
}

function update(e)
    npc.update(e, data)
end
