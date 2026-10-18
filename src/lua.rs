//! Sandboxed Lua 5.4 scripting hooks, behind the default-on `lua` feature.
//!
//! A pack opts into scripting by carrying `lua/**.lua` entries: the mod
//! manifest's `lua` list (ordered) or, when it names none, every `lua/` entry
//! sorted by lowercased path. Each chunk is compiled and run once when the
//! session opens; the global functions `on_room_load(api)`, `on_tick(api, tick)`
//! and `on_message(api, id)` are then looked up and called by the engine:
//!
//! - `on_room_load` runs after a room's state exists and before its first tick;
//! - `on_tick` runs after every fixed room tick, with the completed tick count;
//! - `on_message` runs when a message was raised during the tick and may return
//!   a replacement message id (nothing/nil keeps the original).
//!
//! The `api` argument is the live [`GameState`](crate::game::GameState) exposed as a scoped userdata
//! through [`mlua::Lua::scope`]: the author-facing hook methods
//! (`api:room()`, `api:flag_get`/`api:flag_set`,
//! `api:item_count`/`api:give_item`/`api:item_remove`, `api:kill_event`,
//! `api:message`, `api:log`) plus the coarser state-authoring methods the
//! CLI's `--setup-script`/`--save-out` oracle seam uses (inventory slots, the
//! item box, player health/position/character, room identity/camera, FILE and
//! item-use flags, raw flag banks and state bytes). Methods use Lua's colon
//! syntax (`api:flag_set(0, 1, true)`); `docs/modding.md` is the full table.
//!
//! The sandbox loads only the base, `string` and `table` libraries, so `os`,
//! `io`, `package`, `debug`, `coroutine` and `math` do not exist; the base
//! library's `dofile`, `loadfile`, `load` and `collectgarbage` globals are set
//! to nil before any chunk runs. An instruction-budget hook aborts a script
//! that runs away, a memory limit bounds allocation, and every load or call
//! error is logged once and disables only the failing hook (or skips the
//! failing chunk), never the game session. A pack with no Lua entries, or a
//! build without the `lua` feature, loads no VM: every hook is a no-op and
//! captures stay byte-identical.

#[cfg(not(feature = "lua"))]
mod imp {
    use anyhow::Result;

    use crate::game::GameState;
    use crate::pack::Pack;
    use crate::state::RoomId;

    /// The no-op Lua runtime compiled without the `lua` feature.
    ///
    /// Every hook call is a no-op and a pack with Lua entries is treated
    /// exactly like one without, so a lean build stays deterministic.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct LuaVm;

    impl LuaVm {
        /// Always `None`: the lean build has no Lua runtime.
        pub fn load(_pack: &Pack) -> Result<Option<LuaVm>> {
            Ok(None)
        }

        /// An empty VM. The lean build has none, so `--setup-script` fails.
        pub fn empty() -> Result<LuaVm> {
            Ok(LuaVm)
        }

        /// The lean build cannot run `--setup-script` chunks.
        pub fn run_setup(&self, _game: &mut GameState, _bytes: &[u8], _name: &str) -> Result<()> {
            anyhow::bail!(
                "this build has no Lua runtime; `--setup-script` requires the `lua` feature"
            )
        }

        /// No-op pack-init hook.
        pub fn call_init(&self, _game: &mut GameState) {}

        /// No-op room-entry hook.
        pub fn call_room_load(&self, _game: &mut GameState, _id: RoomId) {}

        /// No-op per-tick hook.
        pub fn call_tick(&self, _game: &mut GameState, _tick: u64) {}

        /// No message rewrite.
        pub fn filter_message(&self, _game: &mut GameState, _id: u16) -> Option<u16> {
            None
        }
    }
}

#[cfg(feature = "lua")]
mod imp {
    use std::cell::{Cell, RefCell};
    use std::collections::HashSet;
    use std::rc::Rc;

    use anyhow::Result;
    use mlua::{
        FromLuaMulti, Function, HookTriggers, Lua, LuaOptions, StdLib, UserData, UserDataMethods,
        Value, VmState,
    };

    use crate::budget;
    use crate::game::GameState;
    use crate::pack::Pack;
    use crate::state::RoomId;

    /// Lua instructions between two budget-hook calls.
    const HOOK_INTERVAL: u32 = 10_000;
    /// Instructions one hook call may execute before the budget aborts it.
    const CALL_BUDGET: u64 = 5_000_000;
    /// Allocation ceiling for the sandboxed state, in bytes.
    const MEMORY_LIMIT: usize = 16 * 1024 * 1024;
    /// Message pause word the Lua `api:message` accessor requests.
    const MESSAGE_PAUSE: u16 = 0;

    /// Base-library globals removed before any chunk runs.
    ///
    /// mlua always opens Lua's base library, and Lua 5.4's base contains the
    /// filesystem loaders (`dofile`, `loadfile`) plus `load` (which can parse
    /// binary chunks) and the GC control; none of them belong in a sandboxed
    /// hook. The library bindings the API uses (`string`, `table`, `print`,
    /// `assert`, ...) stay.
    const REMOVED_GLOBALS: [&str; 4] = ["dofile", "loadfile", "load", "collectgarbage"];

    /// The per-call instruction budget, shared with the VM's hook callback.
    #[derive(Debug, Default)]
    pub(crate) struct Budget {
        /// Instructions left in the current call.
        remaining: Cell<u64>,
    }

    impl Budget {
        /// Start a fresh call budget.
        pub(crate) fn begin(&self) {
            self.remaining.set(CALL_BUDGET);
        }

        /// Charge one hook interval; `true` when the budget is exhausted.
        fn charge(&self) -> bool {
            let remaining = self.remaining.get();
            if remaining == 0 {
                return true;
            }
            self.remaining
                .set(remaining.saturating_sub(u64::from(HOOK_INTERVAL)));
            false
        }
    }

    /// Create the sandboxed state every scripting VM shares: base, `string`
    /// and `table` only, the filesystem/loader globals removed, the 16 MiB
    /// allocation ceiling and the instruction-budget hook installed.
    ///
    /// Returns the state and the per-call budget the caller resets before each
    /// script call. Shared by the mod-hook VM ([`LuaVm`]) and the enemy-script
    /// host so both enforce exactly the same sandbox.
    pub(crate) fn new_sandboxed_state() -> Result<(Lua, Rc<Budget>)> {
        let lua = Lua::new_with(StdLib::STRING | StdLib::TABLE, LuaOptions::default())
            .map_err(|err| anyhow::anyhow!("failed to create the Lua state: {err}"))?;
        {
            let globals = lua.globals();
            for name in REMOVED_GLOBALS {
                globals.set(name, Value::Nil).map_err(|err| {
                    anyhow::anyhow!("failed to remove the Lua global {name}: {err}")
                })?;
            }
        }
        lua.set_memory_limit(MEMORY_LIMIT)
            .map_err(|err| anyhow::anyhow!("failed to set the Lua memory limit: {err}"))?;
        let budget = Rc::new(Budget::default());
        let hook_budget = Rc::clone(&budget);
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(HOOK_INTERVAL),
            move |_lua, _debug| {
                if hook_budget.charge() {
                    Err(mlua::Error::RuntimeError(
                        "instruction budget exceeded".to_string(),
                    ))
                } else {
                    Ok(VmState::Continue)
                }
            },
        );
        Ok((lua, budget))
    }

    /// The hook functions looked up once after every chunk has run.
    #[derive(Default)]
    struct Hooks {
        init: Option<Function>,
        room_load: Option<Function>,
        tick: Option<Function>,
        message: Option<Function>,
    }

    /// A sandboxed Lua VM bound to one open pack.
    pub struct LuaVm {
        lua: Lua,
        budget: Rc<Budget>,
        hooks: RefCell<Hooks>,
    }

    impl LuaVm {
        /// Load the pack's Lua hooks, or `None` when it has no `lua/` entries.
        ///
        /// The manifest's `lua` list selects and orders the chunks; without
        /// one, every `lua/**.lua` entry runs in lowercased-path order. A chunk
        /// that fails to load or run is logged and skipped; only a failure to
        /// create the Lua state fails the call.
        pub fn load(pack: &Pack) -> Result<Option<LuaVm>> {
            let paths = hook_paths(pack);
            if paths.is_empty() {
                return Ok(None);
            }
            budget::check_len(paths.len(), budget::MAX_LUA_CHUNKS, "lua chunk count")?;
            let (lua, budget) = new_sandboxed_state()?;

            for path in &paths {
                let bytes = match pack.read(path) {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        eprintln!("[lua] cannot read hook {path}: {err:#}");
                        continue;
                    }
                };
                budget::check_len(
                    bytes.len(),
                    budget::MAX_LUA_CHUNK,
                    &format!("lua chunk {path} size"),
                )?;
                budget.begin();
                if let Err(err) = lua.load(&*bytes).set_name(path.clone()).exec() {
                    eprintln!("[lua] failed to run {path}: {err}");
                }
            }

            let globals = lua.globals();
            let lookup = |name: &str| match globals.get::<Value>(name) {
                Ok(Value::Function(function)) => Some(function),
                _ => None,
            };
            let hooks = Hooks {
                init: lookup("on_init"),
                room_load: lookup("on_room_load"),
                tick: lookup("on_tick"),
                message: lookup("on_message"),
            };
            Ok(Some(Self {
                lua,
                budget,
                hooks: RefCell::new(hooks),
            }))
        }

        /// An empty VM with no pack hooks, for the CLI `--setup-script` seam
        /// on a pack that carries no `lua/` entries: the chunk still runs in
        /// the same sandbox and the same authoring API.
        pub fn empty() -> Result<LuaVm> {
            let (lua, budget) = new_sandboxed_state()?;
            Ok(Self {
                lua,
                budget,
                hooks: RefCell::new(Hooks::default()),
            })
        }

        /// Run a CLI `--setup-script` chunk against `game`.
        ///
        /// The chunk is called with the `api` userdata as its first argument
        /// (`local api = ...`). Unlike the hook calls, a failure is returned
        /// to the caller: the authoring seam must not silently produce a save.
        pub fn run_setup(&self, game: &mut GameState, bytes: &[u8], name: &str) -> Result<()> {
            budget::check_len(
                bytes.len(),
                budget::MAX_LUA_CHUNK,
                &format!("setup script {name} size"),
            )?;
            let function = self
                .lua
                .load(bytes)
                .set_name(name.to_string())
                .into_function()
                .map_err(|err| anyhow::anyhow!("{name}: {err}"))?;
            self.budget.begin();
            self.lua
                .scope(|scope| {
                    let api = scope.create_userdata_ref_mut(game)?;
                    function.call::<()>(api)
                })
                .map_err(|err| anyhow::anyhow!("{name}: {err}"))
        }

        /// Run `on_init(api)` once the pack's chunks have run, before any
        /// room. A pack uses it to register things the engine reads before a
        /// session exists, such as a save/load screen.
        pub fn call_init(&self, game: &mut GameState) {
            let Some(function) = self.hooks.borrow().init.clone() else {
                return;
            };
            if let Err(err) = self.call::<Value>(&function, game, Value::Nil) {
                eprintln!("[lua] on_init disabled: {err}");
                self.hooks.borrow_mut().init = None;
            }
        }

        /// Run `on_room_load(api)` after the room state exists.
        pub fn call_room_load(&self, game: &mut GameState, id: RoomId) {
            debug_assert_eq!(game.id, id, "the hook runs for the state's own room");
            let Some(function) = self.hooks.borrow().room_load.clone() else {
                return;
            };
            if let Err(err) = self.call::<Value>(&function, game, Value::Nil) {
                eprintln!("[lua] on_room_load disabled: {err}");
                self.hooks.borrow_mut().room_load = None;
            }
        }

        /// Run `on_tick(api, tick)` after one fixed room tick.
        pub fn call_tick(&self, game: &mut GameState, tick: u64) {
            let Some(function) = self.hooks.borrow().tick.clone() else {
                return;
            };
            if let Err(err) = self.call::<Value>(&function, game, tick) {
                eprintln!("[lua] on_tick disabled: {err}");
                self.hooks.borrow_mut().tick = None;
            }
        }

        /// Run `on_message(api, id)` and return the replacement id it asks for.
        ///
        /// A hook that was never defined, returns nothing (or nil), or fails
        /// with an error returns `None`, leaving the raised message untouched.
        pub fn filter_message(&self, game: &mut GameState, id: u16) -> Option<u16> {
            let function = self.hooks.borrow().message.clone()?;
            match self.call::<Option<i64>>(&function, game, id) {
                Ok(Some(value)) => Some(value.clamp(0, i64::from(u16::MAX)) as u16),
                Ok(None) => None,
                Err(err) => {
                    eprintln!("[lua] on_message disabled: {err}");
                    self.hooks.borrow_mut().message = None;
                    None
                }
            }
        }

        /// Call one hook with the scoped `api` userdata bound to `game`.
        fn call<R: FromLuaMulti>(
            &self,
            function: &Function,
            game: &mut GameState,
            args: impl mlua::IntoLua,
        ) -> mlua::Result<R> {
            self.budget.begin();
            self.lua.scope(|scope| {
                let api = scope.create_userdata_ref_mut(game)?;
                function.call((api, args))
            })
        }
    }

    /// The hook paths to load: the manifest's ordered list, or every
    /// `lua/**.lua` entry in lowercased-path order.
    fn hook_paths(pack: &Pack) -> Vec<String> {
        // Every layer may name its hooks: the base first, then each mod in
        // applied order, so a mod's manifest list is honoured like the base's.
        // A layer that names none contributes its own `lua/**.lua` entries in
        // lowercased-path order, which is exactly the single-pack rule.
        let mut paths: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for layer in std::iter::once(pack).chain(pack.overrides()) {
            let candidates: Vec<String> =
                match layer.manifest().filter(|manifest| !manifest.lua.is_empty()) {
                    Some(manifest) => manifest.lua.clone(),
                    None => discovered_hooks(layer.own_paths()),
                };
            for path in candidates {
                if seen.insert(path.to_ascii_lowercase()) {
                    paths.push(path);
                }
            }
        }
        paths
    }

    /// Every `lua/**.lua` entry of `paths`, in lowercased-path order.
    fn discovered_hooks<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<String> {
        let mut paths: Vec<String> = paths
            .filter(|path| path.len() > 4)
            .filter(|path| path[..4].eq_ignore_ascii_case("lua/"))
            .filter(|path| path.to_ascii_lowercase().ends_with(".lua"))
            .map(str::to_owned)
            .collect();
        paths.sort_by_key(|path| path.to_ascii_lowercase());
        paths
    }

    /// The author-facing API: the live game state with a fixed method set.
    impl UserData for GameState {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            // `room()` is read-only: the three hex digits of the room id.
            methods.add_method("room", |_lua, game, ()| Ok(game.id.room3()));

            methods.add_method("flag_get", |_lua, game, (bank, bit): (u8, u8)| {
                Ok(game
                    .flags
                    .get(usize::from(bank))
                    .is_some_and(|flags| flags.bit(bit)))
            });

            // `flag_set(bank, bit, value)`: `value` defaults to true, so
            // `api:flag_set(0, 1)` sets the bit.
            methods.add_method_mut(
                "flag_set",
                |_lua, game, (bank, bit, value): (u8, u8, Option<bool>)| {
                    if let Some(flags) = game.flags.get_mut(usize::from(bank)) {
                        flags.apply(bit, if value.unwrap_or(true) { 0 } else { 1 });
                    }
                    Ok(())
                },
            );

            methods.add_method("item_count", |_lua, game, item: u8| {
                Ok(game.item_count(item))
            });

            // `give_item(item[, quantity])`: the quantity defaults to one, so
            // the original one-argument form is unchanged.
            methods.add_method_mut(
                "give_item",
                |_lua, game, (item, quantity): (u8, Option<u8>)| {
                    game.add_item(item, quantity.unwrap_or(1));
                    Ok(())
                },
            );

            methods.add_method_mut("item_remove", |_lua, game, item: u8| {
                Ok(game.item_remove(item))
            });

            // `set_item_slot(slot, id[, quantity])`: replace the stack in a
            // player slot (or drop it with id 0). An index past the held run
            // appends; the inventory compacts so no empty slot survives.
            methods.add_method_mut(
                "set_item_slot",
                |_lua, game, (slot, id, quantity): (usize, u8, Option<u8>)| {
                    let capacity = game.inventory_capacity();
                    if slot >= capacity {
                        return Err(mlua::Error::RuntimeError(format!(
                            "inventory slot {slot} is out of range (0..{capacity})"
                        )));
                    }
                    let stack = crate::game::InventoryItem {
                        id,
                        quantity: quantity.unwrap_or(1),
                    };
                    if slot < game.inventory.len() {
                        game.inventory[slot] = stack;
                    } else {
                        game.inventory.push(stack);
                    }
                    game.rebuild_slots();
                    Ok(())
                },
            );

            methods.add_method_mut("clear_inventory", |_lua, game, ()| {
                game.inventory.clear();
                game.set_equipped(None);
                game.select_item(None);
                game.rebuild_slots();
                Ok(())
            });

            methods.add_method_mut("set_equipped", |_lua, game, item: Option<u8>| {
                game.set_equipped(item.filter(|item| *item != 0));
                Ok(())
            });

            methods.add_method_mut("set_equipped_weapon", |_lua, game, id: u8| {
                game.equipped_weapon_id = id;
                Ok(())
            });

            methods.add_method_mut(
                "item_box_set",
                |_lua, game, (slot, id, quantity): (usize, u8, Option<u8>)| {
                    if slot >= crate::game::ITEM_BOX_SLOTS {
                        return Err(mlua::Error::RuntimeError(format!(
                            "item box slot {slot} is out of range (0..{})",
                            crate::game::ITEM_BOX_SLOTS
                        )));
                    }
                    game.item_box[slot] = crate::game::InventoryItem {
                        id,
                        quantity: quantity.unwrap_or(1),
                    };
                    Ok(())
                },
            );

            methods.add_method_mut("item_box_clear", |_lua, game, ()| {
                game.item_box =
                    [crate::game::InventoryItem::default(); crate::game::ITEM_BOX_SLOTS];
                Ok(())
            });

            methods.add_method_mut("set_health", |_lua, game, health: i16| {
                game.entities[0].health = health;
                game.set_health_copy(health);
                Ok(())
            });

            methods.add_method_mut("set_max_health", |_lua, game, health: i16| {
                game.max_health = health;
                Ok(())
            });

            methods.add_method_mut("set_health_status", |_lua, game, status: u8| {
                game.set_health_status(status);
                Ok(())
            });

            methods.add_method_mut(
                "set_player_pos",
                |_lua, game, (x, y, z): (i32, i32, i32)| {
                    game.entities[0].pos = [x, y, z];
                    Ok(())
                },
            );

            methods.add_method_mut("set_player_angle", |_lua, game, degrees: i32| {
                game.entities[0].angle = degrees as u16 & 0x0FFF;
                Ok(())
            });

            // `set_character(id)`: 0 Chris, 1 Jill. The variant selects the
            // room RDT, so a script that changes it usually also picks a room
            // booted for that character.
            methods.add_method_mut("set_character", |_lua, game, id: u8| {
                let id = id & 1;
                game.id.player_flag = id;
                game.state_bytes[usize::from(crate::game::STATE_BYTE_CHARACTER)] = id;
                game.state_bytes[usize::from(crate::game::STATE_BYTE_CHARACTER_MODEL)] = id;
                game.entities[0].id = id;
                Ok(())
            });

            // `set_room(stage, room)`: authoring-only. It retargets the room
            // identity the save block records; the session keeps running the
            // room it booted, like a save written while the block is edited.
            methods.add_method_mut("set_room", |_lua, game, (stage, room): (u8, u8)| {
                game.id.stage = stage;
                game.id.room = room;
                game.state_bytes[0] = stage;
                game.state_bytes[1] = room;
                Ok(())
            });

            methods.add_method_mut("set_room_camera", |_lua, game, cut: usize| {
                game.set_camera_cut(cut);
                Ok(())
            });

            // `set_file_collected(item[, value])`: the FILE-tab document item
            // ids (0x5F..=0x6E) map to the saved file bits.
            methods.add_method_mut(
                "set_file_collected",
                |_lua, game, (item, value): (u8, Option<bool>)| {
                    let Some(index) = crate::game::file_index(item) else {
                        return Err(mlua::Error::RuntimeError(format!(
                            "item 0x{item:02X} is not a file document (0x5F..=0x6E)"
                        )));
                    };
                    game.set_file_collected(index, value.unwrap_or(true));
                    Ok(())
                },
            );

            methods.add_method_mut(
                "set_item_use_flag",
                |_lua, game, (item, value): (u8, Option<bool>)| {
                    game.set_item_use_flag(item, value.unwrap_or(true));
                    Ok(())
                },
            );

            // `set_flag_bank(bank, bytes)`: the raw bytes of one bank, from a
            // Lua string or a sequence table of byte values, for state the
            // named methods do not reach.
            methods.add_method_mut("set_flag_bank", |_lua, game, (bank, value): (u8, Value)| {
                let Some(flags) = game.flags.get_mut(usize::from(bank)) else {
                    return Err(mlua::Error::RuntimeError(format!(
                        "flag bank {bank} is out of range (0..{})",
                        crate::game::FLAG_BANK_COUNT
                    )));
                };
                let raw: Vec<u8> = match value {
                    Value::String(text) => text.as_bytes().to_vec(),
                    Value::Table(table) => table
                        .sequence_values::<u8>()
                        .collect::<mlua::Result<Vec<u8>>>()?,
                    other => {
                        return Err(mlua::Error::RuntimeError(format!(
                            "set_flag_bank expects a string or table of bytes, got {}",
                            other.type_name()
                        )));
                    }
                };
                let count = raw.len().min(crate::game::FLAG_BANK_BYTES);
                flags.bytes_mut()[..count].copy_from_slice(&raw[..count]);
                Ok(())
            });

            methods.add_method_mut(
                "set_state_byte",
                |_lua, game, (index, value): (usize, u8)| {
                    let Some(byte) = game.state_bytes.get_mut(index) else {
                        return Err(mlua::Error::RuntimeError(format!(
                            "state byte {index} is out of range (0..{})",
                            crate::game::STATE_BYTES
                        )));
                    };
                    *byte = value;
                    Ok(())
                },
            );

            methods.add_method_mut("kill_event", |_lua, game, slot: u8| {
                game.pending_event_requests
                    .push(crate::scd::host::EventRequest::Kill(slot));
                Ok(())
            });

            methods.add_method_mut("message", |_lua, game, id: u8| {
                game.show_message(id, MESSAGE_PAUSE);
                Ok(())
            });

            // `register_save_screen(entry)`: the pack entry the engine runs
            // as the save/load screen instead of the ported original; `nil`
            // clears a registration. The entry is read when a screen opens, so
            // a missing one falls back with a warning rather than failing here.
            methods.add_method_mut(
                "register_save_screen",
                |_lua, game, entry: Option<String>| {
                    match entry {
                        Some(entry) if entry.is_empty() => {
                            return Err(mlua::Error::RuntimeError(
                                "register_save_screen needs a pack entry path".to_string(),
                            ));
                        }
                        Some(entry) if entry.contains('\0') => {
                            return Err(mlua::Error::RuntimeError(
                                "register_save_screen entry contains a NUL byte".to_string(),
                            ));
                        }
                        Some(entry) => game.save_screen = Some(entry),
                        None => game.save_screen = None,
                    }
                    Ok(())
                },
            );

            methods.add_method("log", |_lua, _game, text: mlua::String| {
                eprintln!("[lua] {}", text.to_string_lossy());
                Ok(())
            });
        }
    }
}

pub use imp::LuaVm;

#[cfg(feature = "lua")]
pub(crate) use imp::{Budget, new_sandboxed_state};

/// The save/load screen entry a pack's scripts register, if any.
///
/// The app needs the registration before any session exists (the title's LOAD
/// GAME and the load screen both run first), so this loads the pack's hooks in
/// a throwaway VM and runs `on_init` over a scratch state. Only a
/// registration is read from it: the state is dropped, and every session keeps
/// its own VM and its own state.
pub fn registered_save_screen(pack: &crate::pack::Pack) -> Option<String> {
    let vm = match LuaVm::load(pack) {
        Ok(Some(vm)) => vm,
        Ok(None) => return None,
        Err(err) => {
            eprintln!("warning: Lua hooks unavailable: {err:#}");
            return None;
        }
    };
    let mut probe = crate::game::GameState::default();
    vm.call_init(&mut probe);
    probe.save_screen
}

/// Compile a Lua source entry without running it, for the pack verifier.
///
/// The check the engine's reader performs is "the sandboxed state can compile
/// this chunk"; the source-size cap is applied by the caller. Without the
/// `lua` feature there is no Lua runtime, so every source verifies.
pub fn verify_source(bytes: &[u8]) -> anyhow::Result<()> {
    crate::budget::check_len(bytes.len(), crate::budget::MAX_LUA_CHUNK, "lua source")?;
    #[cfg(feature = "lua")]
    {
        let (lua, _budget) = new_sandboxed_state()?;
        lua.load(bytes)
            .into_function()
            .map_err(|err| anyhow::anyhow!("lua compile: {err}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "lua")]
    use crate::budget;
    use crate::game::GameState;
    use crate::pack::Pack;

    /// Build an in-memory pack from the given `(path, bytes)` entries.
    fn pack(entries: &[(&str, &[u8])]) -> Pack {
        use crate::pack::PackWriter;

        let mut writer = PackWriter::new();
        for (path, data) in entries {
            writer.add(path, data.to_vec()).unwrap();
        }
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    #[cfg(feature = "lua")]
    fn lua_pack(manifest_lua: &[&str], scripts: &[(&str, &str)]) -> Pack {
        use crate::manifest;

        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        let manifest = manifest::Manifest {
            kind: manifest::PackKind::Mod,
            base: Some("re1".to_string()),
            lua: manifest_lua.iter().map(|path| path.to_string()).collect(),
            ..manifest::Manifest::base("demo")
        };
        entries.push((manifest::ENTRY.to_string(), manifest.render().into_bytes()));
        for (path, source) in scripts {
            entries.push(((*path).to_string(), source.as_bytes().to_vec()));
        }
        let borrowed: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(path, data)| (path.as_str(), data.as_slice()))
            .collect();
        pack(&borrowed)
    }

    #[cfg(feature = "lua")]
    #[test]
    fn on_init_registers_a_save_screen_for_the_app_and_the_session() {
        let source = r#"
function on_init(api)
    api:register_save_screen("script/mine.lua")
end
"#;
        let pack = lua_pack(&["lua/init.lua"], &[("lua/init.lua", source)]);
        // The app reads the registration from a throwaway VM and state.
        assert_eq!(
            registered_save_screen(&pack).as_deref(),
            Some("script/mine.lua")
        );

        // A session's own VM runs the same hook over its live state.
        let vm = LuaVm::load(&pack).unwrap().expect("a Lua VM");
        let mut game = GameState::default();
        assert_eq!(game.save_screen, None);
        vm.call_init(&mut game);
        assert_eq!(game.save_screen.as_deref(), Some("script/mine.lua"));
        // `nil` clears a registration.
        let clear = lua_pack(
            &["lua/init.lua"],
            &[(
                "lua/init.lua",
                "function on_init(api) api:register_save_screen(nil) end",
            )],
        );
        let vm = LuaVm::load(&clear).unwrap().expect("a Lua VM");
        let mut game = GameState::default();
        game.save_screen = Some("script/mine.lua".to_string());
        vm.call_init(&mut game);
        assert_eq!(game.save_screen, None);
        assert!(registered_save_screen(&clear).is_none());
    }

    #[cfg(feature = "lua")]
    #[test]
    fn register_save_screen_rejects_a_bad_entry() {
        let pack = lua_pack(
            &["lua/init.lua"],
            &[(
                "lua/init.lua",
                "function on_init(api) api:register_save_screen(\"\") end",
            )],
        );
        let vm = LuaVm::load(&pack).unwrap().expect("a Lua VM");
        let mut game = GameState::default();
        // The failure disables the hook rather than failing the session.
        vm.call_init(&mut game);
        assert_eq!(game.save_screen, None);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn every_layer_contributes_its_hooks() {
        // A mod's manifest `lua` list is honoured like the base's, and a layer
        // that names none contributes its discovered `lua/` entries: the mod
        // layer in the repository keeps its `mod.lua` at the pack root and
        // names it, so this is the rule it relies on.
        use crate::manifest::{Manifest, PackKind};
        use crate::pack::PackWriter;

        let dir = std::env::temp_dir();
        let tag = std::process::id();
        // Only the last chunk may define `on_room_load`: the hooks are single
        // globals, so each chunk's definition replaces the previous one. The
        // order the chunks ran in is what the test reads.
        let chunk_a = "order = (order or \"\") .. \"a\"\n";
        let chunk_b = "order = (order or \"\") .. \"b\"\n\
                       function on_room_load(api)\n\
                           api:flag_set(0, 1, order == \"ab\")\n\
                       end\n";
        let mut base_writer = PackWriter::new();
        base_writer
            .add(
                crate::manifest::ENTRY,
                Manifest {
                    id: "re1".to_string(),
                    kind: PackKind::Base,
                    ..Manifest::base("re1")
                }
                .render()
                .into_bytes(),
            )
            .unwrap();
        let base_path = dir.join(format!("arklay-hooks-base-{tag}.akpak"));
        base_writer.write(&base_path).unwrap();

        let mut layer_paths = Vec::new();
        for (index, (id, lua_list, source)) in [
            ("a", vec!["lua/a.lua".to_string()], chunk_a),
            ("b", Vec::new(), chunk_b),
        ]
        .into_iter()
        .enumerate()
        {
            let mut writer = PackWriter::new();
            writer
                .add(
                    crate::manifest::ENTRY,
                    Manifest {
                        id: id.to_string(),
                        kind: PackKind::Mod,
                        base: Some("re1".to_string()),
                        load_order: index as i32 * 10,
                        lua: lua_list,
                        ..Manifest::base(id)
                    }
                    .render()
                    .into_bytes(),
                )
                .unwrap();
            writer
                .add(&format!("lua/{id}.lua"), source.as_bytes().to_vec())
                .unwrap();
            let path = dir.join(format!("arklay-hooks-{id}-{tag}.akpak"));
            writer.write(&path).unwrap();
            layer_paths.push(path);
        }
        let layered = Pack::open_layered(&base_path, &layer_paths).unwrap();
        let vm = LuaVm::load(&layered).unwrap().expect("a Lua VM");
        let mut game = GameState::default();
        let id = game.id;
        vm.call_room_load(&mut game, id);
        assert!(
            game.flags[0].bit(1),
            "the named hook and the discovered one both ran, in layer order"
        );
        for path in layer_paths {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_file(base_path);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn a_pack_without_lua_entries_has_no_vm() {
        let pack = pack(&[(
            "manifest.toml",
            b"format = 1\nid = \"x\"\nkind = \"base\"\n",
        )]);
        assert!(LuaVm::load(&pack).unwrap().is_none());
    }

    #[cfg(feature = "lua")]
    #[test]
    fn chunks_run_in_manifest_order_then_discovery_order() {
        // Both chunks append their tag to a shared global and the second one
        // reports the final tag through room flags; the manifest chooses which
        // chunk runs first.
        let hook = r#"
order = (order or "") .. "a"

function on_room_load(api)
    api:flag_set(0, 1, order == "ab")
    api:flag_set(0, 2, order == "ba")
end
"#;
        let append_b = r#"order = (order or "") .. "b""#;
        let manifest_pack = lua_pack(
            &["lua/b.lua", "lua/a.lua"],
            &[("lua/a.lua", hook), ("lua/b.lua", append_b)],
        );
        let vm = LuaVm::load(&manifest_pack).unwrap().expect("a Lua VM");
        let mut game = GameState::default();
        let id = game.id;
        vm.call_room_load(&mut game, id);
        assert!(game.flags[0].bit(2), "the manifest list reversed the order");
        assert!(!game.flags[0].bit(1));

        // Without a list the lowercased path order wins.
        let discovered_pack = lua_pack(&[], &[("lua/b.lua", append_b), ("lua/a.lua", hook)]);
        let vm = LuaVm::load(&discovered_pack).unwrap().expect("a Lua VM");
        let mut game = GameState::default();
        let id = game.id;
        vm.call_room_load(&mut game, id);
        assert!(game.flags[0].bit(1), "discovery sorts a before b");
    }

    #[cfg(feature = "lua")]
    #[test]
    fn hooks_expose_room_flags_items_kills_messages_and_logs() {
        let source = r#"
function on_room_load(api)
    api:log("room " .. api:room())
    assert(api:flag_get(0, 3) == false)
    api:flag_set(0, 3, true)
    assert(api:flag_get(0, 3) == true)
    api:flag_set(0, 3, false)
    assert(api:flag_get(0, 3) == false)
    api:flag_set(0, 3, true)
    api:give_item(0x0B)
    api:give_item(0x0B)
end

function on_tick(api, tick)
    if tick >= 3 then
        api:kill_event(2)
        api:message(9)
        api:item_remove(0x0B)
    end
end

function on_message(api, id)
    return id + 1
end
"#;
        let pack = lua_pack(&["lua/hooks.lua"], &[("lua/hooks.lua", source)]);
        let vm = LuaVm::load(&pack).unwrap().expect("a Lua VM");

        let mut game = GameState::default();
        game.id = crate::state::RoomId::parse("1000").unwrap();
        let id = game.id;
        vm.call_room_load(&mut game, id);
        assert!(game.flags[0].bit(3));
        assert_eq!(game.item_count(0x0B), 2);

        vm.call_tick(&mut game, 1);
        assert!(game.message.id.is_none(), "the tick hook waited for tick 3");
        assert!(game.pending_event_requests.is_empty());

        vm.call_tick(&mut game, 3);
        assert_eq!(
            game.pending_event_requests,
            [crate::scd::host::EventRequest::Kill(2)]
        );
        assert_eq!(game.message.id, Some(9));
        assert_eq!(game.item_count(0x0B), 0, "item_remove clears the slot");

        assert_eq!(vm.filter_message(&mut game, 9), Some(10));
        assert_eq!(vm.filter_message(&mut game, 9), Some(10));
    }

    #[cfg(feature = "lua")]
    #[test]
    fn the_sandbox_removes_the_nondeterministic_libraries() {
        let source = r#"
function on_room_load(api)
    assert(os == nil, "os")
    assert(io == nil, "io")
    assert(package == nil, "package")
    assert(debug == nil, "debug")
    assert(coroutine == nil, "coroutine")
    assert(math == nil, "math")
    assert(dofile == nil, "dofile")
    assert(loadfile == nil, "loadfile")
    assert(load == nil, "load")
    assert(collectgarbage == nil, "collectgarbage")
    assert(type(string.upper) == "function", "string")
    assert(type(table.insert) == "function", "table")
    assert(type(print) == "function", "base")
    api:flag_set(0, 4, true)
end
"#;
        let pack = lua_pack(&["lua/sandbox.lua"], &[("lua/sandbox.lua", source)]);
        let vm = LuaVm::load(&pack).unwrap().expect("a Lua VM");
        let mut game = GameState::default();
        let id = game.id;
        vm.call_room_load(&mut game, id);
        assert!(
            game.flags[0].bit(4),
            "the sandbox hook ran with only base/string/table"
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn the_memory_limit_stops_a_runaway_allocation() {
        let source = r#"
function on_tick(api, tick)
    local huge = string.rep("x", 64 * 1024 * 1024)
    api:flag_set(0, 6, true)
    _ = huge
end
"#;
        let pack = lua_pack(&["lua/memory.lua"], &[("lua/memory.lua", source)]);
        let vm = LuaVm::load(&pack).unwrap().expect("a Lua VM");
        let mut game = GameState::default();
        vm.call_tick(&mut game, 1);
        assert!(
            !game.flags[0].bit(6),
            "the allocation must fail before the flag write"
        );
        // The failed call disables the hook, so it cannot retry the allocation.
        vm.call_tick(&mut game, 2);
        assert!(!game.flags[0].bit(6));
    }

    #[cfg(feature = "lua")]
    #[test]
    fn a_failing_call_disables_only_that_hook() {
        let source = r#"
count = 0

function on_tick(api, tick)
    count = count + 1
    api:flag_set(0, count, true)
    error("always fails")
end

function on_room_load(api)
    api:flag_set(0, 9, true)
end
"#;
        let pack = lua_pack(&["lua/fail.lua"], &[("lua/fail.lua", source)]);
        let vm = LuaVm::load(&pack).unwrap().expect("a Lua VM");
        let mut game = GameState::default();

        vm.call_tick(&mut game, 1);
        assert!(game.flags[0].bit(1), "the first call ran before failing");
        vm.call_tick(&mut game, 2);
        assert!(!game.flags[0].bit(2), "the tick hook was disabled");

        // The other hook still runs.
        let id = game.id;
        vm.call_room_load(&mut game, id);
        assert!(game.flags[0].bit(9));
    }

    #[cfg(feature = "lua")]
    #[test]
    fn a_runaway_script_hits_the_instruction_budget() {
        let source = r#"
function on_tick(api, tick)
    while true do end
end
"#;
        let pack = lua_pack(&["lua/spin.lua"], &[("lua/spin.lua", source)]);
        let vm = LuaVm::load(&pack).unwrap().expect("a Lua VM");
        let mut game = GameState::default();
        // The budget aborts the loop, so this returns instead of hanging.
        vm.call_tick(&mut game, 1);
        // The hook is disabled after the abort.
        vm.call_tick(&mut game, 2);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn rejects_chunks_over_the_source_cap() {
        let source = "x".repeat(budget::MAX_LUA_CHUNK + 1);
        let pack = lua_pack(&["lua/big.lua"], &[("lua/big.lua", &source)]);
        let Err(error) = LuaVm::load(&pack) else {
            panic!("an over-cap chunk must be rejected");
        };
        let message = error.to_string();
        assert!(message.contains("lua chunk"), "{message}");
        assert!(message.contains("limit"), "{message}");
    }

    #[cfg(feature = "lua")]
    #[test]
    fn a_broken_chunk_is_skipped_and_the_rest_still_run() {
        let pack = lua_pack(
            &[],
            &[
                ("lua/a_broken.lua", "function on_room_load(api) return"),
                (
                    "lua/b_ok.lua",
                    "function on_room_load(api) api:flag_set(0, 7, true) end",
                ),
            ],
        );
        let vm = LuaVm::load(&pack).unwrap().expect("a Lua VM");
        let mut game = GameState::default();
        let id = game.id;
        vm.call_room_load(&mut game, id);
        assert!(game.flags[0].bit(7));
    }

    #[cfg(feature = "lua")]
    #[test]
    fn an_undefined_hook_is_a_noop() {
        let pack = lua_pack(&["lua/none.lua"], &[("lua/none.lua", "local x = 1")]);
        let vm = LuaVm::load(&pack).unwrap().expect("a Lua VM");
        let mut game = GameState::default();
        let id = game.id;
        vm.call_room_load(&mut game, id);
        vm.call_tick(&mut game, 1);
        assert_eq!(vm.filter_message(&mut game, 3), None);
        assert_eq!(game, GameState::default());
    }

    #[cfg(feature = "lua")]
    #[test]
    fn identical_packs_produce_identical_hook_effects() {
        let source = r#"
function on_tick(api, tick)
    api:flag_set(1, tick % 8, true)
end
"#;
        let pack = lua_pack(&["lua/d.lua"], &[("lua/d.lua", source)]);
        let mut first = GameState::default();
        let mut second = GameState::default();
        let first_vm = LuaVm::load(&pack).unwrap().expect("a Lua VM");
        let second_vm = LuaVm::load(&pack).unwrap().expect("a Lua VM");
        let first_id = first.id;
        let second_id = second.id;
        first_vm.call_room_load(&mut first, first_id);
        second_vm.call_room_load(&mut second, second_id);
        for tick in 1..=20 {
            first_vm.call_tick(&mut first, tick);
            second_vm.call_tick(&mut second, tick);
        }
        assert_eq!(first, second);
    }

    #[cfg(not(feature = "lua"))]
    #[test]
    fn the_lean_build_compiles_every_hook_to_a_noop() {
        let pack = pack(&[]);
        let vm = LuaVm::load(&pack).unwrap();
        assert!(vm.is_none());
        let mut game = GameState::default();
        let before = game.clone();
        let id = game.id;
        if let Some(vm) = vm {
            vm.call_room_load(&mut game, id);
            vm.call_tick(&mut game, 1);
            assert_eq!(vm.filter_message(&mut game, 5), None);
        }
        assert_eq!(game, before);
    }
}
