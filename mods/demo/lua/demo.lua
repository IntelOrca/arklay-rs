-- Demo hooks: log and flag on room entry, then grant one item on the 30th
-- tick. The API uses Lua's method syntax; see docs/modding.md.
local granted = false

function on_room_load(api)
    api:log("demo: room " .. api:room())
    api:flag_set(0x00, 0x02, true)
end

function on_tick(api, tick)
    if tick >= 30 and not granted then
        granted = true
        api:give_item(0x0F)
    end
end
