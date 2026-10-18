-- Classic REbirth-style save/load screen mod.
--
-- Registers the screen with the engine; `script/loadsave.lua` is the screen
-- itself and is loaded only when the screen opens. Nothing in the engine
-- names this screen: `register_save_screen` is the whole link.

function on_init(api)
    api:register_save_screen("script/loadsave.lua")
end
