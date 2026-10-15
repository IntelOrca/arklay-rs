-- MAP tab: the pause-menu anchor with the first eight map areas owned, so
-- the floor-plan browser draws them (the tab itself needs no flag).
local api = ...
api:set_player_pos(13050, 0, 20100)
api:set_player_angle(0)
for area = 0, 7 do
    api:flag_set(8, 0x7C + area, true)
end
