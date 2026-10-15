-- Typewriter: the player inside the quiet B1 save room's typewriter action rect
-- (aot_set slot 4, x 1300..3200, z 8000..9700) with an ink ribbon, so
-- the save prompt and the save screen open (and a slot choice can be
-- cancelled before anything is written).
local api = ...
api:set_player_pos(2578, 0, 7615)
api:set_player_angle(3072)
api:clear_inventory()
api:set_item_slot(0, 0x01, 0)
api:set_item_slot(1, 0x41, 1)
api:set_item_slot(2, 0x2F, 1)
