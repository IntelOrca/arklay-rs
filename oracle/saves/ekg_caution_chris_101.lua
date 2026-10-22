-- EKG health states: Chris at 50/140 (Caution, orange) holding one green
-- herb, for the orange sweep and the yellow one the heal latches at the next
-- wrap.
local api = ...
api:set_player_pos(13050, 0, 20100)
api:set_player_angle(0)
api:set_health(50)
api:clear_inventory()
api:set_item_slot(0, 0x44, 1)
