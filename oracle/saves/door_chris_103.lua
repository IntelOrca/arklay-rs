-- Same-stage door: room 103 (dining room), standing in front of its door to
-- room 101 and facing it. A press of action takes the door; unlike the
-- 10B->207 stairwell this load stays in stage 1, so `room_set` runs on the
-- same frame as the placement.
local api = ...
api:set_player_pos(651, 0, 24750)
api:set_player_angle(0)
