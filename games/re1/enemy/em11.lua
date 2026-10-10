-- Green zombie, entity id 0x11.
--
-- The third zombie dispatch entry: the same machine and the same collision
-- record as the standard zombie, only the model differs.

local zombie = require("./lib/zombie")

local VARIANT = {
    radius = 422,
}

function update(e)
    zombie.run(e, VARIANT)
end
