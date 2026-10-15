-- Combine: green herb (0x44) in slot 0, red herb (0x43) in slot 1 and a
-- second green herb in slot 2. Slot 0 + slot 1 is the green+red recipe
-- (mixed herb 0x46 + the herbal-mix message); slot 0 + slot 2 is the two
-- herbs with no recipe, the original's 0xF6 refusal message.
local api = ...
api:set_player_pos(13050, 0, 20100)
api:set_player_angle(0)
api:clear_inventory()
api:set_item_slot(0, 0x44, 1)
api:set_item_slot(1, 0x43, 1)
api:set_item_slot(2, 0x44, 1)
api:set_item_slot(3, 0x41, 1)
