-- Green zombie, entity id 0x11.
--
-- The third zombie dispatch entry: the same machine and the same collision
-- record as the standard zombie, only the model differs.

local zombie = require("./lib/zombie")

local VARIANT = {
    radius = 422,
}

-- The standard zombie's sounds (`enemy/em00_*`), with the room's groan style.
sounds = { base = "em00", styles = zombie.SOUND_STYLES }

function update(e)
    zombie.run(e, VARIANT)
end
