-- Barry 2 (S.T.A.R.S.). Shared NPC driver data.
local npc = require("lib/npc")
local data = {
    radius = 422,
    corpse = false,
    idle0_plays = false,
    rebecca = false,
    wesker = false,
    sca = { 422, 0x5FA, 0, -0x5FA, 0 },
}

function update(e)
    npc.update(e, data)
end
