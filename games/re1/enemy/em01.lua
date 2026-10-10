-- Naked zombie, entity id 0x01.
--
-- Identical to the standard zombie except for the collision record: the
-- naked variant's narrower radius-322 box.

local zombie = require("./lib/zombie")

local VARIANT = {
    radius = 322,
}

function update(e)
    zombie.run(e, VARIANT)
end
