-- Yawn (the rematch), entity id 0x12.
--
-- The same machine as the first fight: only the init health differs (0x012C,
-- or 0x0190 once the serum has been taken) and the bite does not poison. The
-- shared body lives in `enemy/em0d.lua`.

local yawn = require("./em0d")

function update(e)
    yawn.update(e)
end
