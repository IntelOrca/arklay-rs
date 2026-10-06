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
//! The `api` argument is the live [`GameState`] exposed as a scoped userdata
//! through [`mlua::Lua::scope`], with only the author-facing accessors:
//! `api:room()` (read-only), `api:flag_get`/`api:flag_set`,
//! `api:item_count`/`api:give_item`/`api:item_remove`, `api:kill_event`,
//! `api:message` and `api:log` (stderr with a `[lua]` prefix). Methods use
//! Lua's colon syntax (`api:flag_set(0, 1, true)`).
//!
//! The sandbox loads only the base, `string` and `table` libraries, so `os`,
//! `io`, `package`, `debug`, `coroutine` and `math` do not exist. An
//! instruction-budget hook aborts a script that runs away, and every load or
//! call error is logged once and disables only the failing hook (or skips the
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
    use std::rc::Rc;

    use anyhow::Result;
    use mlua::{
        FromLuaMulti, Function, HookTriggers, Lua, LuaOptions, StdLib, UserData, UserDataMethods,
        Value, VmState,
    };

    use crate::game::GameState;
    use crate::pack::Pack;
    use crate::state::RoomId;

    /// Lua instructions between two budget-hook calls.
    const HOOK_INTERVAL: u32 = 10_000;
    /// Instructions one hook call may execute before the budget aborts it.
    const CALL_BUDGET: u64 = 5_000_000;
    /// Message pause word the Lua `api:message` accessor requests.
    const MESSAGE_PAUSE: u16 = 0;

    /// The per-call instruction budget, shared with the VM's hook callback.
    #[derive(Debug, Default)]
    struct Budget {
        /// Instructions left in the current call.
        remaining: Cell<u64>,
    }

    impl Budget {
        /// Start a fresh call budget.
        fn begin(&self) {
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

    /// The hook functions looked up once after every chunk has run.
    #[derive(Default)]
    struct Hooks {
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
            let lua = Lua::new_with(StdLib::STRING | StdLib::TABLE, LuaOptions::default())
                .map_err(|err| anyhow::anyhow!("failed to create the Lua state: {err}"))?;
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

            for path in &paths {
                let bytes = match pack.read(path) {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        eprintln!("[lua] cannot read hook {path}: {err:#}");
                        continue;
                    }
                };
                budget.begin();
                if let Err(err) = lua.load(bytes).set_name(path.clone()).exec() {
                    eprintln!("[lua] failed to run {path}: {err}");
                }
            }

            let globals = lua.globals();
            let lookup = |name: &str| match globals.get::<Value>(name) {
                Ok(Value::Function(function)) => Some(function),
                _ => None,
            };
            let hooks = Hooks {
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
        if let Some(manifest) = pack.manifest()
            && !manifest.lua.is_empty()
        {
            return manifest.lua.clone();
        }
        let mut paths: Vec<String> = pack
            .paths()
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

            methods.add_method_mut("give_item", |_lua, game, item: u8| {
                game.add_item(item, 1);
                Ok(())
            });

            methods.add_method_mut("item_remove", |_lua, game, item: u8| {
                Ok(game.item_remove(item))
            });

            methods.add_method_mut("kill_event", |_lua, game, slot: u8| {
                game.pending_event_kills.push(slot);
                Ok(())
            });

            methods.add_method_mut("message", |_lua, game, id: u8| {
                game.show_message(id, MESSAGE_PAUSE);
                Ok(())
            });

            methods.add_method("log", |_lua, _game, text: mlua::String| {
                eprintln!("[lua] {}", text.to_string_lossy());
                Ok(())
            });
        }
    }
}

pub use imp::LuaVm;

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(game.pending_event_kills.is_empty());

        vm.call_tick(&mut game, 3);
        assert_eq!(game.pending_event_kills, [2]);
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
