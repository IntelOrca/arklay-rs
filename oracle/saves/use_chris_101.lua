-- Use health: Chris at 30/140 with a green herb in slot 0 (one-third heal,
-- EKG flush) and a first-aid spray in slot 1 for a second use.
local api = ...
api:set_player_pos(13050, 0, 20100)
api:set_player_angle(0)
api:set_health(30)
api:clear_inventory()
api:set_item_slot(0, 0x44, 1)
api:set_item_slot(1, 0x41, 1)
api:set_item_slot(2, 0x01, 0)
