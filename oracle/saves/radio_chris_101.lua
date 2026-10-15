-- RADIO tab: the pause-menu anchor carrying the communication radio (item
-- 0x4D) with the scenario HAS_RADIO bit (bank 0 bit 0x7F) up, so the tab is
-- drawn and the confirm uses the radio (item-use byte 0x4D) and closes the
-- menu.
local api = ...
api:set_player_pos(13050, 0, 20100)
api:set_player_angle(0)
api:flag_set(0, 0x7F, true)
api:clear_inventory()
api:set_item_slot(0, 0x01, 0)
api:set_item_slot(1, 0x41, 1)
api:set_item_slot(2, 0x4D, 1)
