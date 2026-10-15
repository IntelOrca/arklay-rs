-- Typewriter: the player inside the dining room's typewriter action zone
-- (aot_set slot 7, centre 2300,8750 size 1600x2000) with an ink ribbon, so
-- the save prompt and the save screen open (and a slot choice can be
-- cancelled before anything is written).
local api = ...
api:set_player_pos(2300, 0, 8750)
api:set_player_angle(0)
api:clear_inventory()
api:set_item_slot(0, 0x01, 0)
api:set_item_slot(1, 0x41, 1)
api:set_item_slot(2, 0x2F, 1)
