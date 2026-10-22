-- EKG health states: Chris at 20/140 (Danger, red) carrying both poison
-- flags, a serum (cures the 0x20 poison only, for a blue cure flush) and a
-- green+red mix (full heal plus the 0x02 cure, for a white heal-and-cure
-- flush).
local api = ...
api:set_player_pos(13050, 0, 20100)
api:set_player_angle(0)
api:set_health(20)
api:set_health_status(0x22)
api:clear_inventory()
api:set_item_slot(0, 0x42, 1)
api:set_item_slot(1, 0x49, 1)
