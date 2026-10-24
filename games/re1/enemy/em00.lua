-- Standard zombie, entity id 0x00.
--
-- The shared driver lives in `lib/zombie.lua`; this file carries only the
-- variant's collision record (the standard zombie's radius-422 box). The
-- per-frame state machine, tables and all data are the driver's.

local zombie = require("./lib/zombie")

local VARIANT = {
    radius = 422,
}

-- The standard zombie's sounds (`enemy/em00_*`), with the room's groan style.
sounds = { styles = zombie.SOUND_STYLES }

function update(e)
    zombie.run(e, VARIANT)
end
