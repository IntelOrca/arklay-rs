-- Boot-camera continue: room 104 (Chris), the player authored inside a
-- collision volume at the room's far wall. The block's camera byte is the
-- cut the room boot had live (2), not the cut the saved position selects, so
-- a continue proves `room_set`'s camera block discards the saved id and
-- re-homes from camera 0 (#42): the reference boots this block on cam 0.
local api = ...
api:set_player_pos(3700, 0, 6600)
api:set_player_angle(0)
