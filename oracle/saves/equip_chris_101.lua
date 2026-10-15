-- Equip: Beretta (0x02) in slot 0 with handgun ammo (0x0B) next to it, so
-- the item-action submenu offers equip/unequip on the weapon.
local api = ...
api:set_player_pos(13050, 0, 20100)
api:set_player_angle(0)
api:clear_inventory()
api:set_item_slot(0, 0x02, 1)
api:set_item_slot(1, 0x0B, 15)
api:set_item_slot(2, 0x01, 0)
api:set_item_slot(3, 0x41, 1)
