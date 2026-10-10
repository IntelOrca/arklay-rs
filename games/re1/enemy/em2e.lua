-- Wesker 2 (S.T.A.R.S.); the variant flag selects his opening pose and the
local npc = require("lib/npc")
-- laboratory power room deactivates him.
local data = {
    radius = 422,
    corpse = false,
    idle0_plays = false,
    rebecca = false,
    wesker = true,
    sca = { 422, 0x5FA, 0, -0x5FA, 0 },
}

function update(e)
    npc.update(e, data)
end
