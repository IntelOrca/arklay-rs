-- Rebecca (S.T.A.R.S.); the wounded scenario flag selects her opening pose.
local npc = require("lib/npc")
local data = {
    radius = 372,
    corpse = false,
    idle0_plays = false,
    rebecca = true,
    wesker = false,
    sca = { 372, 0x5FA, 0, -0x5FA, 0 },
}

function update(e)
    npc.update(e, data)
end
