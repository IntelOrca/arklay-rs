-- The new-game start inventory (`SetInitialItems`).
--
-- The engine runs this chunk once when a new game starts, after the
-- `bio_card.dat` copy, with the scratch state's `api` and the selected
-- character (`0` Chris, `1` Jill). The player's inventory is already empty
-- and, for Jill, the two partner slots her eight-slot run overlaps are
-- cleared. A mod replaces the start items by shipping its own
-- `game/new_game.lua`.
--
-- `api:set_item_slot(slot, id, quantity)` fills the player's slots in order
-- and `api:set_partner_slot(slot, id, quantity)` fills Rebecca's six slots.

local api, character = ...

local KNIFE = 0x01
local BERETTA = 0x02
local FIRST_AID_SPRAY = 0x41

if character == 0 then
    api:set_item_slot(0, KNIFE, 0)
    api:set_item_slot(1, FIRST_AID_SPRAY, 1)
    -- Chris's run is six slots, so Rebecca's card beretta survives.
    api:set_partner_slot(0, BERETTA, 15)
else
    api:set_item_slot(0, KNIFE, 0)
    api:set_item_slot(1, BERETTA, 15)
    api:set_item_slot(2, FIRST_AID_SPRAY, 1)
end
