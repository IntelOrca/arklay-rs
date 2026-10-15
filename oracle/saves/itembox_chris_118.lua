-- Item box: the player inside the quiet B1 save room's item-box action rect
-- (aot_set slot 3, x 4750..7550, z 1900..3950) with a mixed inventory,
-- and a few box slots occupied so the box pages have entries.
local api = ...
api:set_player_pos(5200, 0, 2500)
api:set_player_angle(0)
api:clear_inventory()
api:set_item_slot(0, 0x01, 0)
api:set_item_slot(1, 0x41, 1)
api:set_item_slot(2, 0x44, 1)
api:set_item_slot(3, 0x0B, 15)
api:item_box_clear()
api:item_box_set(0, 0x43, 1)
api:item_box_set(1, 0x2F, 1)
api:item_box_set(2, 0x02, 1)
api:item_box_set(24, 0x45, 1)
