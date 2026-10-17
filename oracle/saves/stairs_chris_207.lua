-- In-room stairs: room 207's lower landing, facing the flight up to the 2F
-- floor. Stepping onto the flight's action-key event (slot 9) runs a script
-- whose `aot_on` fires `check_door_side`, so the climb starts without a
-- press of its own. The save block carries no height, so both engines start
-- at Y 0 rather than the landing's 0xB45 (drawn above the steps); the climb
-- still lands 0xB45 higher.
local api = ...
api:set_player_pos(9080, 0, 18200)
api:set_player_angle(1152)
