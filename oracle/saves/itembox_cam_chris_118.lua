-- Item box seen from the camera that frames it: the player just north of the
-- quiet B1 save room's camera 0 -> 1 switch strip (z 4472..5433 at x 5200),
-- facing the item-box action rect (aot_set slot 3, x 4750..7550, z 1900..3950).
-- A save boots on camera 0 and re-homes by position, so the first tick of the
-- scenario switches onto camera 1, the cut that shows the box and its lid in
-- the world (camera 0 looks the other way, so `itembox_chris_118` never sees
-- the lid). Same mixed inventory and box slots as `itembox_chris_118`.
local api = ...
api:set_player_pos(5200, 0, 4600)
api:set_player_angle(1024)
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
