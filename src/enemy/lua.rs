//! The enemy-script runtime: one sandboxed Lua VM per session hosting the
//! per-id `enemy/em{id:02x}.lua` update functions behind the default-on `lua`
//! feature.
//!
//! A pack opts into enemy scripting by carrying `enemy/em{id:02x}.lua` entries;
//! each defines `function update(e)`, called once per fixed tick for every
//! active entity slot whose id it names. Scripts import other pack entries
//! with a sandboxed, pack-scoped `require(name)`: `/path` is absolute from the
//! pack root, `./path` and `../path` are relative to the requiring module, and
//! a bare `path` is tried as an exact entry and then relative to the requiring
//! module, with the `.lua` suffix optional. Modules execute once per VM and
//! cache their returned value, so shared behaviour lives in one pack entry.
//!
//! # Statelessness
//!
//! Enemy scripts must not keep mutable Lua state: the contract is that
//! dropping and recreating the whole VM between two updates must not change
//! behaviour. Every field a script reads or writes lives on the Rust entity;
//! the host keeps only the compiled functions and the module cache, all of
//! which are recreated byte-for-byte on reset. [`LuaEnemyHost::reset`]
//! drops the VM and its caches; the unit tests run whole scripted scenes with
//! and without a reset on every tick and compare the resulting state.
//!
//! The `e` argument is a scoped userdata over the live game state, valid only
//! for the duration of one call. A script that fails to load, or a call that
//! errors, is logged once and parks that entity id for the session; the other
//! ids keep running. A pack with no enemy scripts loads no VM at all, and a
//! `--no-default-features` build compiles every entry point to a no-op.

#[cfg(not(feature = "lua"))]
mod imp {
    use crate::game::GameState;
    use crate::model::Clip;
    use crate::pack::Pack;
    use crate::state::RoomState;

    /// The no-op enemy-script host compiled without the `lua` feature.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct LuaEnemyHost;

    impl LuaEnemyHost {
        /// An empty host.
        pub fn new() -> Self {
            LuaEnemyHost
        }

        /// Drop nothing; the lean build has no VM.
        pub fn reset(&mut self) {}

        /// Never runs a script.
        pub fn update(
            &mut self,
            _game: &mut GameState,
            _room: &RoomState,
            _pack: &Pack,
            _slot: usize,
            _clips: &[Clip],
        ) -> bool {
            false
        }

        /// No scripts exist in the lean build.
        pub fn failed_scripts(&self) -> usize {
            0
        }
    }
}

#[cfg(feature = "lua")]
mod imp {
    use std::cell::{Cell, RefCell};
    use std::collections::{HashMap, HashSet};
    use std::rc::Rc;

    use mlua::{Function, Lua, Table, UserData, UserDataFields, UserDataMethods, Value};

    use crate::budget;
    use crate::effects::Attach;
    use crate::enemy::{EntityAnim, in_camera_zone, live_look_at_target};
    use crate::game::{BANK_ENEMIES, BANK_SCENARIO, BANK_SYSTEM, EntitySound, GameState};
    use crate::lua::new_sandboxed_state;
    use crate::model::Clip;
    use crate::pack::Pack;
    use crate::state::RoomState;

    /// The enemy-script host: one sandboxed VM, the compiled `update`
    /// functions keyed by entity id and the per-VM module cache behind the
    /// pack-scoped `require`.
    pub struct LuaEnemyHost {
        lua: Option<Lua>,
        /// The VM's per-call instruction budget.
        budget: Rc<crate::lua::Budget>,
        /// Compiled `update` functions, one per scripted id.
        scripts: HashMap<u8, Function>,
        /// Ids the pack carries no script for; the pack is asked once.
        missing: HashSet<u8>,
        /// Ids whose script failed to load or whose call errored.
        failed: HashSet<u8>,
        /// The pack `require` resolves against, set for the duration of every
        /// load and every update call and never read outside one.
        ///
        /// # Safety
        ///
        /// The pointer is only dereferenced from inside `require`, which can
        /// only run while the host is inside a call that received a live
        /// `&Pack`; no reference derived from it outlives the call or is
        /// stored anywhere.
        pack: Rc<Cell<*const Pack>>,
        /// Resolved modules, keyed by lowercased pack entry; cleared with the
        /// VM. Values are constant module tables/functions, not live state.
        modules: Rc<RefCell<HashMap<String, Value>>>,
        /// Modules currently being loaded, for `require` cycle detection.
        loading: Rc<RefCell<HashSet<String>>>,
    }

    impl Default for LuaEnemyHost {
        fn default() -> Self {
            Self::new()
        }
    }

    impl LuaEnemyHost {
        /// An empty host; the VM is created when the first script loads.
        pub fn new() -> Self {
            LuaEnemyHost {
                lua: None,
                budget: Rc::new(crate::lua::Budget::default()),
                scripts: HashMap::new(),
                missing: HashSet::new(),
                failed: HashSet::new(),
                pack: Rc::new(Cell::new(std::ptr::null())),
                modules: Rc::new(RefCell::new(HashMap::new())),
                loading: Rc::new(RefCell::new(HashSet::new())),
            }
        }

        /// Drop the whole VM, every compiled script and every cached module.
        ///
        /// The statelessness contract makes this transparent to behaviour: the
        /// next update recreates the sandbox, re-runs every `require` and
        /// recompiles the failing script. The test suite uses it to prove no
        /// script depends on VM-resident state.
        pub fn reset(&mut self) {
            self.lua = None;
            self.budget = Rc::new(crate::lua::Budget::default());
            self.scripts.clear();
            self.missing.clear();
            self.failed.clear();
            self.modules = Rc::new(RefCell::new(HashMap::new()));
            self.loading = Rc::new(RefCell::new(HashSet::new()));
        }

        /// The number of compiled scripts held.
        pub fn loaded_scripts(&self) -> usize {
            self.scripts.len()
        }

        /// The number of ids whose script failed to load or errored at runtime.
        pub fn failed_scripts(&self) -> usize {
            self.failed.len()
        }

        /// Run the `update(e)` function of the entity in `slot`.
        ///
        /// Returns whether a script ran; an id with no script, a parked id or
        /// a lean build returns `false` without touching the state. `clips`
        /// are the entity model's animation clips, exposed to the script's
        /// animation helper.
        pub fn update(
            &mut self,
            game: &mut GameState,
            room: &RoomState,
            pack: &Pack,
            slot: usize,
            clips: &[Clip],
        ) -> bool {
            self.pack.set(pack as *const Pack);
            let id = game.entities[slot].id;
            let Some(function) = self.function(pack, id) else {
                return false;
            };
            self.budget.begin();
            // The scoped userdata borrows the call's data for exactly this
            // call; `Lua::scope` seals it so the script cannot retain it.
            let mut api = EnemyApi {
                game: game as *mut GameState,
                room: room as *const RoomState,
                clips: clips as *const [Clip],
                slot,
            };
            let result = {
                let lua = self.lua.as_ref().expect("a compiled function implies a VM");
                lua.scope(|scope| {
                    let e = scope.create_userdata_ref_mut(&mut api)?;
                    function.call::<()>(e)
                })
            };
            // The monster joint world matrices, computed from the pose this
            // update leaves behind: the original's render pass does the same
            // between two entity updates, so the next tick's script and this
            // tick's later effect pass read exactly these matrices.
            if id < crate::enemy::CHARACTER_ID_MIN {
                let worlds = game.entity_anims[slot].joint_worlds(&game.entities[slot], clips);
                game.joint_worlds[slot] = worlds;
            }
            match result {
                Ok(()) => true,
                Err(err) => {
                    eprintln!("[enemy] em{id:02x} update disabled: {err}");
                    self.failed.insert(id);
                    self.scripts.remove(&id);
                    false
                }
            }
        }

        /// The compiled `update` function of `id`, loading the script from the
        /// pack on first use.
        fn function(&mut self, pack: &Pack, id: u8) -> Option<Function> {
            if let Some(function) = self.scripts.get(&id) {
                return Some(function.clone());
            }
            if self.missing.contains(&id) || self.failed.contains(&id) {
                return None;
            }
            let path = script_path(id);
            let bytes = match pack.read(&path) {
                Ok(bytes) => bytes,
                Err(_) => {
                    self.missing.insert(id);
                    return None;
                }
            };
            if let Err(err) = budget::check_len(
                bytes.len(),
                budget::MAX_LUA_CHUNK,
                &format!("enemy script {path}"),
            ) {
                eprintln!("[enemy] {err:#}");
                self.failed.insert(id);
                return None;
            }
            if !self.ensure_vm() {
                self.failed.insert(id);
                return None;
            }
            let lua = self.lua.as_ref().expect("ensure_vm created the state");
            let env = match create_env(
                lua,
                Rc::clone(&self.pack),
                Rc::clone(&self.modules),
                Rc::clone(&self.loading),
                &path,
            ) {
                Ok(env) => env,
                Err(err) => {
                    eprintln!("[enemy] failed to create the environment for {path}: {err:#}");
                    self.failed.insert(id);
                    return None;
                }
            };
            self.budget.begin();
            self.pack.set(pack as *const Pack);
            if let Err(err) = lua
                .load(bytes)
                .set_name(path.clone())
                .set_environment(env.clone())
                .exec()
            {
                eprintln!("[enemy] failed to load {path}: {err:#}");
                self.failed.insert(id);
                return None;
            }
            let update = match env.get::<Value>("update") {
                Ok(Value::Function(function)) => function,
                _ => {
                    eprintln!("[enemy] {path} defines no update function");
                    self.failed.insert(id);
                    return None;
                }
            };
            self.scripts.insert(id, update.clone());
            Some(update)
        }

        /// Create the VM when the first script loads.
        fn ensure_vm(&mut self) -> bool {
            if self.lua.is_some() {
                return true;
            }
            let (lua, budget) = match new_sandboxed_state() {
                Ok(state) => state,
                Err(err) => {
                    eprintln!("[enemy] failed to create the Lua state: {err:#}");
                    return false;
                }
            };
            self.lua = Some(lua);
            self.budget = budget;
            true
        }
    }

    /// The pack entry of id `id`'s script.
    fn script_path(id: u8) -> String {
        format!("enemy/em{id:02x}.lua")
    }

    /// Create a module environment: a fresh table falling back to the VM's
    /// globals, carrying a `require` bound to the module's own directory.
    fn create_env(
        lua: &Lua,
        pack: Rc<Cell<*const Pack>>,
        modules: Rc<RefCell<HashMap<String, Value>>>,
        loading: Rc<RefCell<HashSet<String>>>,
        path: &str,
    ) -> mlua::Result<Table> {
        let env = lua.create_table()?;
        let meta = lua.create_table()?;
        meta.set("__index", lua.globals())?;
        env.set_metatable(Some(meta));
        let require = make_require(lua, pack, modules, loading, dir_of(path))?;
        env.set("require", require)?;
        Ok(env)
    }

    /// Build a `require` bound to `base_dir`, the directory of the module
    /// that owns it.
    fn make_require(
        lua: &Lua,
        pack: Rc<Cell<*const Pack>>,
        modules: Rc<RefCell<HashMap<String, Value>>>,
        loading: Rc<RefCell<HashSet<String>>>,
        base_dir: String,
    ) -> mlua::Result<Function> {
        lua.create_function(move |lua, name: String| {
            require_module(lua, &pack, &modules, &loading, &base_dir, &name)
        })
    }

    /// The `require(name)` implementation.
    ///
    /// `name` is a pack path, with the `.lua` suffix optional:
    ///
    /// - `/enemy/lib/npc` is absolute, from the pack root;
    /// - `./npc` and `../lib/npc` are relative to the requiring module;
    /// - `enemy/lib/npc` is tried as an exact entry first and relative to the
    ///   requiring module second, so shared modules read naturally.
    ///
    /// The first module found is executed once per VM and its return value
    /// cached under the resolved entry, so every later `require` of the same
    /// module returns the same value (a module that returns nothing caches
    /// `true`, matching Lua).
    fn require_module(
        lua: &Lua,
        pack_cell: &Rc<Cell<*const Pack>>,
        modules: &Rc<RefCell<HashMap<String, Value>>>,
        loading: &Rc<RefCell<HashSet<String>>>,
        base_dir: &str,
        name: &str,
    ) -> mlua::Result<Value> {
        // SAFETY: `require` only runs while the host is inside a load or
        // update call that set the pack pointer; see the field comment.
        let pack = unsafe {
            pack_cell.get().as_ref().ok_or_else(|| {
                mlua::Error::RuntimeError(
                    "require is only available while a script is running".to_string(),
                )
            })?
        };
        let entry = resolve_module(pack, base_dir, name)
            .ok_or_else(|| mlua::Error::RuntimeError(format!("module not found: {name}")))?;
        if let Some(value) = modules.borrow().get(&entry) {
            return Ok(value.clone());
        }
        if !loading.borrow_mut().insert(entry.clone()) {
            return Err(mlua::Error::RuntimeError(format!("loop requiring {entry}")));
        }
        let result = load_module(lua, pack, pack_cell, modules, loading, &entry);
        loading.borrow_mut().remove(&entry);
        let value = result?;
        modules.borrow_mut().insert(entry, value.clone());
        Ok(value)
    }

    /// Load and execute one module, returning its value (or `true` when the
    /// chunk returns nothing).
    fn load_module(
        lua: &Lua,
        pack: &Pack,
        pack_cell: &Rc<Cell<*const Pack>>,
        modules: &Rc<RefCell<HashMap<String, Value>>>,
        loading: &Rc<RefCell<HashSet<String>>>,
        entry: &str,
    ) -> mlua::Result<Value> {
        let bytes = pack.read(entry).map_err(|err| {
            mlua::Error::RuntimeError(format!("cannot read module {entry}: {err:#}"))
        })?;
        budget::check_len(
            bytes.len(),
            budget::MAX_LUA_CHUNK,
            &format!("enemy module {entry}"),
        )
        .map_err(|err| mlua::Error::RuntimeError(format!("{err:#}")))?;
        if modules.borrow().len() >= budget::MAX_LUA_CHUNKS {
            return Err(mlua::Error::RuntimeError(format!(
                "too many modules (limit {})",
                budget::MAX_LUA_CHUNKS
            )));
        }
        let env = create_env(
            lua,
            Rc::clone(pack_cell),
            Rc::clone(modules),
            Rc::clone(loading),
            entry,
        )?;
        let value: Value = lua
            .load(bytes)
            .set_name(entry.to_string())
            .set_environment(env)
            .eval()?;
        Ok(if value.is_nil() {
            Value::Boolean(true)
        } else {
            value
        })
    }

    /// Resolve `name` to a lowercased pack entry, or `None` when no candidate
    /// exists or the path escapes the pack root.
    fn resolve_module(pack: &Pack, base_dir: &str, name: &str) -> Option<String> {
        if name.is_empty() || name.contains('\0') {
            return None;
        }
        let rooted = name.starts_with('/');
        let relative = name.starts_with("./") || name.starts_with("../");
        let raw = name.trim_start_matches('/');
        let mut candidates: Vec<String> = Vec::new();
        if rooted {
            candidates.push(raw.to_string());
        } else if relative {
            candidates.push(join_path(base_dir, raw));
        } else {
            candidates.push(raw.to_string());
            candidates.push(join_path(base_dir, raw));
        }
        for candidate in candidates {
            let candidate = if candidate.to_ascii_lowercase().ends_with(".lua") {
                candidate
            } else {
                format!("{candidate}.lua")
            };
            let Some(normalized) = normalize_path(&candidate) else {
                continue;
            };
            if pack.contains(&normalized) {
                return Some(normalized);
            }
        }
        None
    }

    /// `base/name`, or `name` from the pack root when `base` is empty.
    fn join_path(base: &str, name: &str) -> String {
        if base.is_empty() {
            name.to_string()
        } else {
            format!("{base}/{name}")
        }
    }

    /// The directory part of a pack entry (`enemy/em20.lua` -> `enemy`).
    fn dir_of(path: &str) -> String {
        path.rsplit_once('/')
            .map_or(String::new(), |(dir, _)| dir.to_string())
    }

    /// Normalize `/`-separated path segments and lowercase the result.
    /// Returns `None` for an empty path or one escaping the pack root.
    fn normalize_path(path: &str) -> Option<String> {
        let mut parts: Vec<&str> = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop()?;
                }
                other => parts.push(other),
            }
        }
        if parts.is_empty() {
            return None;
        }
        Some(parts.join("/").to_ascii_lowercase())
    }

    /// The per-call API object Lua sees as the `e` argument.
    ///
    /// # Safety
    ///
    /// The scoped userdata holds this struct by mutable reference for exactly
    /// one script call; `Lua::scope` seals it so the script cannot retain it.
    /// The three pointers borrow caller-owned data that outlives the call, and
    /// no method stores them anywhere. Every dereference happens inside the
    /// call.
    struct EnemyApi {
        game: *mut GameState,
        room: *const RoomState,
        clips: *const [Clip],
        slot: usize,
    }

    impl EnemyApi {
        fn entity(&self) -> &crate::game::Entity {
            // SAFETY: see the type-level comment.
            unsafe { &(*self.game).entities[self.slot] }
        }

        fn entity_mut(&mut self) -> &mut crate::game::Entity {
            // SAFETY: see the type-level comment.
            unsafe { &mut (*self.game).entities[self.slot] }
        }

        fn game(&mut self) -> &mut GameState {
            // SAFETY: see the type-level comment.
            unsafe { &mut *self.game }
        }

        fn game_ref(&self) -> &GameState {
            // SAFETY: see the type-level comment.
            unsafe { &*self.game }
        }
    }

    impl UserData for EnemyApi {
        /// The entity's scalar fields as Lua properties (`e.state = 1`,
        /// `e.health`, ...). Every numeric property takes and returns the
        /// field's own width; an assignment truncates exactly like the C
        /// store, so a script may compute in 64-bit integers and assign
        /// without emulating the narrowing itself. Read-only properties have
        /// no setter and error on assignment, like any missing userdata field.
        fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
            // Read-only views.
            fields.add_field_method_get("id", |_lua, api| Ok(api.entity().id));
            fields.add_field_method_get("player_character", |_lua, api| {
                Ok(api.game_ref().id.player_flag & 1)
            });
            fields.add_field_method_get("monster_paused", |_lua, api| {
                Ok(api.game_ref().message_freezes_monsters())
            });
            fields.add_field_method_get("has_enter_switch_zone", |_lua, api| {
                Ok(api.entity().has_enter_switch_zone)
            });
            fields.add_field_method_set("has_enter_switch_zone", |_lua, api, value: i64| {
                api.entity_mut().has_enter_switch_zone = value as u8;
                Ok(())
            });
            fields.add_field_method_get("active", |_lua, api| Ok(api.entity().active()));
            fields.add_field_method_set("active", |_lua, api, active: bool| {
                api.entity_mut().set_active(active);
                Ok(())
            });

            // The state word's two bytes are methods on the entity because
            // they share one field word.
            fields.add_field_method_get("state", |_lua, api| Ok(api.entity().state()));
            fields.add_field_method_set("state", |_lua, api, value: i64| {
                api.entity_mut().set_state(value as u8);
                Ok(())
            });
            fields.add_field_method_get("ignore", |_lua, api| Ok(api.entity().ignore()));
            fields.add_field_method_set("ignore", |_lua, api, value: i64| {
                api.entity_mut().set_ignore(value as u8);
                Ok(())
            });

            // Every other scalar is a plain field; the macro pairs the getter
            // and the truncating setter.
            macro_rules! field {
                ($name:literal, $member:ident, $type:ty) => {
                    fields.add_field_method_get($name, |_lua, api| Ok(api.entity().$member));
                    fields.add_field_method_set($name, |_lua, api, value: i64| {
                        api.entity_mut().$member = value as $type;
                        Ok(())
                    });
                };
            }
            field!("action_behavior", action_behavior, u8);
            field!("action_state", action_state, u8);
            field!("health", health, i16);
            field!("hit_state", hit_state, u8);
            field!("death_timer", death_timer, u8);
            field!("status_flags", status_flags, u8);
            field!("angle", angle, u16);
            field!("pitch", pitch, u16);
            field!("roll", roll, u16);
            field!("animation_id", animation_id, u8);
            field!("animation_frame_id", animation_frame_id, u8);
            field!("unk_bf", unk_bf, u8);
            field!("timing_control", timing_control, u8);
            field!("blend_counter", blend_counter, u8);
            field!("joint_scale", joint_scale, u16);
            field!("sca_radius", sca_radius, i16);
            field!("sca_half_height", sca_half_height, i16);
            field!("shadow_half_x", shadow_half_x, i16);
            field!("shadow_half_z", shadow_half_z, i16);
            field!("shadow_tint", shadow_tint, u32);
            field!("move_speed_current", move_speed_current, u16);
            field!("action_ticks_counter", action_ticks_counter, u16);
            field!("collision_flags", collision_flags, u8);
            field!("flags", flags, u16);
            field!("scd_timer", scd_timer, u16);
            field!("unk_c6", unk_c6, u16);
            field!("unk_c8", unk_c8, u16);

            // The spawn record's behaviour selector and the SCD completion flag
            // id: written by the script opcodes and the monster scripts (the
            // adder's drop, emerge and respawn rewrite the spawn kind), only
            // read by the driver.
            fields.add_field_method_get("behavior_flags", |_lua, api| {
                Ok(api.entity().behavior_flags)
            });
            fields.add_field_method_set("behavior_flags", |_lua, api, value: i64| {
                api.entity_mut().behavior_flags = value as u8;
                Ok(())
            });
            fields.add_field_method_get("scd_anim_param", |_lua, api| {
                Ok(api.entity().scd_anim_param)
            });

            // The follow driver's (state 9) behaviour scratch.
            field!("tex_bank", tex_bank, u8);
            field!("attacking_direction", attacking_direction, u8);
            field!("dir_control_flags", dir_control_flags, u8);
            field!("seq_counter", seq_counter, u8);
            field!("angle_turn_delta", angle_turn_delta, u8);
            field!("move_timer", move_timer, u8);
            field!("is_moving", is_moving, u8);
            field!("move_max_steps", move_max_steps, u8);
            field!("splatter_flag", splatter_flag, u8);
            field!("bob_speed", bob_speed, u8);
            field!("player_pos_x", player_pos_x, i16);
            field!("player_pos_z", player_pos_z, i16);
            field!("reaction_timer", reaction_timer, i16);
            field!("pathfind_state", pathfind_state, u8);
            field!("look_at_flags", look_at_flags, u8);
            field!("look_at_joint", look_at_joint, u8);
            field!("look_at_yaw_step", look_at_yaw_step, u8);
            field!("look_at_pitch_step", look_at_pitch_step, u8);

            // The shared scratch words the roots machine drives. Each signed
            // 16-bit view composes two adjacent bytes and the stored position
            // X composes three fields into one little-endian dword; the Rust
            // accessors own that composition so scripts never do byte math.
            fields.add_field_method_get("writhe_velocity", |_lua, api| {
                Ok(api.entity().writhe_velocity())
            });
            fields.add_field_method_set("writhe_velocity", |_lua, api, value: i64| {
                api.entity_mut().set_writhe_velocity(value as i16);
                Ok(())
            });
            fields.add_field_method_get("writhe_amplitude", |_lua, api| {
                Ok(api.entity().writhe_amplitude())
            });
            fields.add_field_method_set("writhe_amplitude", |_lua, api, value: i64| {
                api.entity_mut().set_writhe_amplitude(value as i16);
                Ok(())
            });
            fields
                .add_field_method_get("tint_flashes", |_lua, api| Ok(api.entity().tint_flashes()));
            fields.add_field_method_set("tint_flashes", |_lua, api, value: i64| {
                api.entity_mut().set_tint_flashes(value as i16);
                Ok(())
            });
            fields.add_field_method_get("groan_timer", |_lua, api| Ok(api.entity().groan_timer()));
            fields.add_field_method_set("groan_timer", |_lua, api, value: i64| {
                api.entity_mut().set_groan_timer(value as i16);
                Ok(())
            });
            fields.add_field_method_get("sink_wobble", |_lua, api| Ok(api.entity().sink_wobble()));
            fields.add_field_method_set("sink_wobble", |_lua, api, value: i64| {
                api.entity_mut().set_sink_wobble(value as i16);
                Ok(())
            });
            fields
                .add_field_method_get("stored_pos_x", |_lua, api| Ok(api.entity().stored_pos_x()));
            fields.add_field_method_set("stored_pos_x", |_lua, api, value: i64| {
                api.entity_mut().set_stored_pos_x(value as i32);
                Ok(())
            });
            fields
                .add_field_method_get("stored_pos_z", |_lua, api| Ok(api.entity().stored_pos_z()));
            fields.add_field_method_set("stored_pos_z", |_lua, api, value: i64| {
                api.entity_mut().set_stored_pos_z(value as i32);
                Ok(())
            });

            // The adder's shared scratch words. Each composes two adjacent
            // bytes that the port also exposes separately; the typed accessor
            // owns the little-endian composition.
            fields.add_field_method_get("sca_active", |_lua, api| Ok(api.entity().sca_active()));
            fields.add_field_method_set("sca_active", |_lua, api, value: i64| {
                api.entity_mut().set_sca_active(value as u16);
                Ok(())
            });
            fields.add_field_method_get("sca_touch", |_lua, api| Ok(api.entity().sca_touch()));
            fields.add_field_method_set("sca_touch", |_lua, api, value: i64| {
                api.entity_mut().set_sca_touch(value as i16);
                Ok(())
            });
            fields.add_field_method_get("room_collision", |_lua, api| {
                Ok(api.entity().room_collision())
            });
            fields.add_field_method_set("room_collision", |_lua, api, value: i64| {
                api.entity_mut().set_room_collision(value as u16);
                Ok(())
            });
            fields
                .add_field_method_get("stuck_frames", |_lua, api| Ok(api.entity().stuck_frames()));
            fields.add_field_method_set("stuck_frames", |_lua, api, value: i64| {
                api.entity_mut().set_stuck_frames(value as u16);
                Ok(())
            });
            fields.add_field_method_get("player_distance", |_lua, api| {
                Ok(api.entity().player_distance())
            });
            fields.add_field_method_set("player_distance", |_lua, api, value: i64| {
                api.entity_mut().set_player_distance(value as u16);
                Ok(())
            });
            // The pathfinder result bit (bit 0 of the scratch byte at +0x16C)
            // the monsters steer on; written only through `pathfind_keep`.
            fields.add_field_method_get("path_bit", |_lua, api| {
                Ok(api.entity().attacking_direction & 1)
            });

            // The wasp's views over the shared scratch block. The names are
            // the original's aliases; each typed accessor owns the
            // little-endian composition or the signed width, so the script
            // never does byte math. `wasp_distance` and `wasp_collision` are
            // the two 16-bit halves of the +0x178 dword and each store
            // preserves the other half.
            fields
                .add_field_method_get("wasp_drift", |_lua, api| Ok(api.entity().writhe_velocity()));
            fields.add_field_method_set("wasp_drift", |_lua, api, value: i64| {
                api.entity_mut().set_writhe_velocity(value as i16);
                Ok(())
            });
            fields.add_field_method_get("wasp_touch", |_lua, api| Ok(api.entity().tint_flashes()));
            fields.add_field_method_set("wasp_touch", |_lua, api, value: i64| {
                api.entity_mut().set_tint_flashes(value as i16);
                Ok(())
            });
            fields.add_field_method_get("wasp_climb", |_lua, api| Ok(api.entity().groan_timer()));
            fields.add_field_method_set("wasp_climb", |_lua, api, value: i64| {
                api.entity_mut().set_groan_timer(value as i16);
                Ok(())
            });
            fields.add_field_method_get("wasp_timer", |_lua, api| Ok(api.entity().sink_wobble()));
            fields.add_field_method_set("wasp_timer", |_lua, api, value: i64| {
                api.entity_mut().set_sink_wobble(value as i16);
                Ok(())
            });
            fields.add_field_method_get("wasp_bob", |_lua, api| Ok(api.entity().reaction_timer));
            fields.add_field_method_set("wasp_bob", |_lua, api, value: i64| {
                api.entity_mut().reaction_timer = value as i16;
                Ok(())
            });
            fields.add_field_method_get("wasp_distance", |_lua, api| {
                Ok(api.entity().wasp_distance())
            });
            fields.add_field_method_set("wasp_distance", |_lua, api, value: i64| {
                api.entity_mut().set_wasp_distance(value as u16);
                Ok(())
            });
            fields.add_field_method_get("wasp_collision", |_lua, api| {
                Ok(api.entity().wasp_collision())
            });
            fields.add_field_method_set("wasp_collision", |_lua, api, value: i64| {
                api.entity_mut().set_wasp_collision(value as u16);
                Ok(())
            });
            fields.add_field_method_get("wasp_anim_done", |_lua, api| {
                Ok(api.entity().wasp_anim_done)
            });
            fields.add_field_method_set("wasp_anim_done", |_lua, api, value: i64| {
                api.entity_mut().wasp_anim_done = value as u16;
                Ok(())
            });
            fields.add_field_method_get("wasp_big", |_lua, api| Ok(api.entity().wasp_big));
            fields.add_field_method_set("wasp_big", |_lua, api, value: i64| {
                api.entity_mut().wasp_big = value as u8;
                Ok(())
            });
            fields
                .add_field_method_get("wasp_respawns", |_lua, api| Ok(api.entity().wasp_respawns));
            fields.add_field_method_set("wasp_respawns", |_lua, api, value: i64| {
                api.entity_mut().wasp_respawns = value as u8;
                Ok(())
            });
            fields.add_field_method_get("wasp_sound_latch", |_lua, api| {
                Ok(api.entity().wasp_sound_latch)
            });
            fields.add_field_method_set("wasp_sound_latch", |_lua, api, value: i64| {
                api.entity_mut().wasp_sound_latch = value as u8;
                Ok(())
            });
            // The wasp's signed views of the two unsigned scratch counters.
            fields.add_field_method_get("wasp_speed", |_lua, api| {
                Ok(api.entity().move_speed_current as i16)
            });
            fields.add_field_method_set("wasp_speed", |_lua, api, value: i64| {
                api.entity_mut().move_speed_current = value as u16;
                Ok(())
            });
            fields.add_field_method_get("wasp_dwell", |_lua, api| {
                Ok(api.entity().action_ticks_counter as i16)
            });
            fields.add_field_method_set("wasp_dwell", |_lua, api, value: i64| {
                api.entity_mut().action_ticks_counter = value as u16;
                Ok(())
            });
            // Bit 0 of the wasp's path-result word at +0x16E; written only
            // through `wasp_path_keep`.
            fields.add_field_method_get("wasp_path_bit", |_lua, api| Ok(api.entity().tex_bank & 1));

            // The crow's views over the same scratch block, the sibling of the
            // wasp's aliases: the signed turn rate over +0x16C, the signed
            // altitude bounds over the +0x172/+0x174 words, the vertical
            // velocity in the +0x176 word, the +0x178/+0x17A dword halves, the
            // stuck counter and the phase word. Each typed accessor owns the
            // composition or the signed width, so the script never does byte
            // math.
            fields.add_field_method_get("crow_turn_rate", |_lua, api| {
                Ok(api.entity().writhe_velocity())
            });
            fields.add_field_method_set("crow_turn_rate", |_lua, api, value: i64| {
                api.entity_mut().set_writhe_velocity(value as i16);
                Ok(())
            });
            fields.add_field_method_get("crow_floor_limit", |_lua, api| {
                Ok(api.entity().crow_floor_limit())
            });
            fields.add_field_method_set("crow_floor_limit", |_lua, api, value: i64| {
                api.entity_mut().set_crow_floor_limit(value as i16);
                Ok(())
            });
            fields.add_field_method_get("crow_ceil_limit", |_lua, api| {
                Ok(api.entity().crow_ceil_limit())
            });
            fields.add_field_method_set("crow_ceil_limit", |_lua, api, value: i64| {
                api.entity_mut().set_crow_ceil_limit(value as i16);
                Ok(())
            });
            fields.add_field_method_get("crow_vy", |_lua, api| Ok(api.entity().reaction_timer));
            fields.add_field_method_set("crow_vy", |_lua, api, value: i64| {
                api.entity_mut().reaction_timer = value as i16;
                Ok(())
            });
            fields.add_field_method_get("crow_touch", |_lua, api| Ok(api.entity().tint_flashes()));
            fields.add_field_method_set("crow_touch", |_lua, api, value: i64| {
                api.entity_mut().set_tint_flashes(value as i16);
                Ok(())
            });
            fields.add_field_method_get("crow_dist", |_lua, api| Ok(api.entity().wasp_distance()));
            fields.add_field_method_set("crow_dist", |_lua, api, value: i64| {
                api.entity_mut().set_wasp_distance(value as u16);
                Ok(())
            });
            fields.add_field_method_get("crow_coll", |_lua, api| Ok(api.entity().wasp_collision()));
            fields.add_field_method_set("crow_coll", |_lua, api, value: i64| {
                api.entity_mut().set_wasp_collision(value as u16);
                Ok(())
            });
            fields.add_field_method_get("crow_stuck", |_lua, api| {
                Ok(api.entity().groan_timer() as u16)
            });
            fields.add_field_method_set("crow_stuck", |_lua, api, value: i64| {
                api.entity_mut().set_groan_timer(value as i16);
                Ok(())
            });
            fields.add_field_method_get("crow_phase", |_lua, api| Ok(api.entity().sink_wobble()));
            fields.add_field_method_set("crow_phase", |_lua, api, value: i64| {
                api.entity_mut().set_sink_wobble(value as i16);
                Ok(())
            });
            fields.add_field_method_get("crow_path_word", |_lua, api| {
                Ok(api.entity().writhe_amplitude() as u16)
            });
            fields.add_field_method_set("crow_path_word", |_lua, api, value: i64| {
                api.entity_mut().set_writhe_amplitude(value as i16);
                Ok(())
            });
            fields.add_field_method_get("crow_path_bit", |_lua, api| Ok(api.entity().tex_bank & 1));

            // The crow's own scratch fields (+0x182/+0x184/+0x186/+0x18A), the
            // floor step the room resolve left behind (+0x8E), and the stored
            // velocity the spiders' circle re-adds (+0x78).
            field!("floor_step", floor_step, i16);
            field!("crow_swerve", swerve, i16);
            field!("crow_swerve_latch", swerve_latch, u8);
            field!("crow_struggle", struggle, i8);
            field!("crow_alt_bias", alt_bias, i16);

            // The WebSpinner's typed views over the shared scratch block: the
            // signed turn step (+0x16C), the path-result word (+0x16E), the
            // touch latch (+0x170), the post-attack delay (+0x172), the splat
            // flag (+0x174), the room-collision trail (+0x176), the chase
            // count (+0x178) and the web-joint registry index (+0x17A). Each
            // typed accessor owns the composition or the signed width.
            fields.add_field_method_get("ws_turn", |_lua, api| Ok(api.entity().writhe_velocity()));
            fields.add_field_method_set("ws_turn", |_lua, api, value: i64| {
                api.entity_mut().set_writhe_velocity(value as i16);
                Ok(())
            });
            fields.add_field_method_get("ws_path_word", |_lua, api| {
                Ok(api.entity().writhe_amplitude() as u16)
            });
            fields.add_field_method_set("ws_path_word", |_lua, api, value: i64| {
                api.entity_mut().set_writhe_amplitude(value as i16);
                Ok(())
            });
            fields.add_field_method_get("ws_touch", |_lua, api| Ok(api.entity().tint_flashes()));
            fields.add_field_method_set("ws_touch", |_lua, api, value: i64| {
                api.entity_mut().set_tint_flashes(value as i16);
                Ok(())
            });
            fields.add_field_method_get("ws_delay", |_lua, api| {
                Ok(api.entity().room_collision() as i16)
            });
            fields.add_field_method_set("ws_delay", |_lua, api, value: i64| {
                api.entity_mut().set_room_collision(value as u16);
                Ok(())
            });
            fields
                .add_field_method_get("ws_splat", |_lua, api| Ok(api.entity().sca_active() as i16));
            fields.add_field_method_set("ws_splat", |_lua, api, value: i64| {
                api.entity_mut().set_sca_active(value as u16);
                Ok(())
            });
            fields.add_field_method_get("ws_trail", |_lua, api| Ok(api.entity().reaction_timer));
            fields.add_field_method_set("ws_trail", |_lua, api, value: i64| {
                api.entity_mut().reaction_timer = value as i16;
                Ok(())
            });
            fields.add_field_method_get("ws_count", |_lua, api| {
                Ok(api.entity().wasp_distance() as i16)
            });
            fields.add_field_method_set("ws_count", |_lua, api, value: i64| {
                api.entity_mut().set_wasp_distance(value as u16);
                Ok(())
            });
            fields.add_field_method_get("ws_web_index", |_lua, api| {
                Ok(api.entity().wasp_collision() as i16)
            });
            fields.add_field_method_set("ws_web_index", |_lua, api, value: i64| {
                api.entity_mut().set_wasp_collision(value as u16);
                Ok(())
            });

            // The Black Tiger's typed views over the same block: the shared
            // turn/path/touch/delay/splat/trail/count words, its per-run
            // counter at +0x17A (`bt_cflag`), the web-joint index at +0x17C
            // and the post-attack cooldown at +0x17E.
            fields.add_field_method_get("bt_turn", |_lua, api| Ok(api.entity().writhe_velocity()));
            fields.add_field_method_set("bt_turn", |_lua, api, value: i64| {
                api.entity_mut().set_writhe_velocity(value as i16);
                Ok(())
            });
            fields.add_field_method_get("bt_path_word", |_lua, api| {
                Ok(api.entity().writhe_amplitude() as u16)
            });
            fields.add_field_method_set("bt_path_word", |_lua, api, value: i64| {
                api.entity_mut().set_writhe_amplitude(value as i16);
                Ok(())
            });
            fields.add_field_method_get("bt_touch", |_lua, api| Ok(api.entity().tint_flashes()));
            fields.add_field_method_set("bt_touch", |_lua, api, value: i64| {
                api.entity_mut().set_tint_flashes(value as i16);
                Ok(())
            });
            fields.add_field_method_get("bt_delay", |_lua, api| {
                Ok(api.entity().room_collision() as i16)
            });
            fields.add_field_method_set("bt_delay", |_lua, api, value: i64| {
                api.entity_mut().set_room_collision(value as u16);
                Ok(())
            });
            fields
                .add_field_method_get("bt_splat", |_lua, api| Ok(api.entity().sca_active() as i16));
            fields.add_field_method_set("bt_splat", |_lua, api, value: i64| {
                api.entity_mut().set_sca_active(value as u16);
                Ok(())
            });
            fields.add_field_method_get("bt_trail", |_lua, api| Ok(api.entity().reaction_timer));
            fields.add_field_method_set("bt_trail", |_lua, api, value: i64| {
                api.entity_mut().reaction_timer = value as i16;
                Ok(())
            });
            fields.add_field_method_get("bt_count", |_lua, api| {
                Ok(api.entity().wasp_distance() as i16)
            });
            fields.add_field_method_set("bt_count", |_lua, api, value: i64| {
                api.entity_mut().set_wasp_distance(value as u16);
                Ok(())
            });
            fields.add_field_method_get("bt_cflag", |_lua, api| {
                Ok(api.entity().wasp_collision() as i16)
            });
            fields.add_field_method_set("bt_cflag", |_lua, api, value: i64| {
                api.entity_mut().set_wasp_collision(value as u16);
                Ok(())
            });
            fields.add_field_method_get("bt_web_index", |_lua, api| Ok(api.entity().groan_timer()));
            fields.add_field_method_set("bt_web_index", |_lua, api, value: i64| {
                api.entity_mut().set_groan_timer(value as i16);
                Ok(())
            });
            fields.add_field_method_get("bt_cooldown", |_lua, api| Ok(api.entity().sink_wobble()));
            fields.add_field_method_set("bt_cooldown", |_lua, api, value: i64| {
                api.entity_mut().set_sink_wobble(value as i16);
                Ok(())
            });

            // The hound's typed views over the shared scratch block: the
            // 32-bit distance dword (+0x16C), the signed turn step and launch
            // velocity (+0x172/+0x174), the probe history (+0x178), the path
            // byte (+0x17A), the head-track swerve (+0x180), the owed blood
            // count (+0x182), the alert latch byte (+0x184), the behaviour
            // flags word and its low byte (+0x186) and the AI flags (+0x188).
            fields.add_field_method_get("cb_dist", |_lua, api| Ok(api.entity().cb_dist()));
            fields.add_field_method_set("cb_dist", |_lua, api, value: i64| {
                api.entity_mut().set_cb_dist(value as i32);
                Ok(())
            });
            fields
                .add_field_method_get("cb_turn_step", |_lua, api| Ok(api.entity().cb_turn_step()));
            fields.add_field_method_set("cb_turn_step", |_lua, api, value: i64| {
                api.entity_mut().set_cb_turn_step(value as i16);
                Ok(())
            });
            fields
                .add_field_method_get("cb_launch_vy", |_lua, api| Ok(api.entity().cb_launch_vy()));
            fields.add_field_method_set("cb_launch_vy", |_lua, api, value: i64| {
                api.entity_mut().set_cb_launch_vy(value as i16);
                Ok(())
            });
            fields.add_field_method_get("cb_probe", |_lua, api| Ok(api.entity().cb_probe()));
            fields.add_field_method_set("cb_probe", |_lua, api, value: i64| {
                api.entity_mut().set_cb_probe(value as i16);
                Ok(())
            });
            fields.add_field_method_get("cb_path", |_lua, api| Ok(api.entity().cb_path()));
            fields.add_field_method_set("cb_path", |_lua, api, value: i64| {
                api.entity_mut().set_cb_path(value as u8);
                Ok(())
            });
            fields.add_field_method_get("cb_swerve", |_lua, api| Ok(api.entity().cb_swerve()));
            fields.add_field_method_set("cb_swerve", |_lua, api, value: i64| {
                api.entity_mut().set_cb_swerve(value as i16);
                Ok(())
            });
            fields.add_field_method_get("cb_blood", |_lua, api| Ok(api.entity().cb_blood()));
            fields.add_field_method_set("cb_blood", |_lua, api, value: i64| {
                api.entity_mut().set_cb_blood(value as i16);
                Ok(())
            });
            fields.add_field_method_get("cb_alert", |_lua, api| Ok(api.entity().cb_alert()));
            fields.add_field_method_set("cb_alert", |_lua, api, value: i64| {
                api.entity_mut().set_cb_alert(value as u8);
                Ok(())
            });
            fields.add_field_method_get("cb_behflags", |_lua, api| Ok(api.entity().cb_behflags()));
            fields.add_field_method_set("cb_behflags", |_lua, api, value: i64| {
                api.entity_mut().set_cb_behflags(value as i16);
                Ok(())
            });
            fields.add_field_method_get("cb_behflags_byte", |_lua, api| {
                Ok(api.entity().cb_behflags_byte())
            });
            fields.add_field_method_set("cb_behflags_byte", |_lua, api, value: i64| {
                api.entity_mut().set_cb_behflags_byte(value as u8);
                Ok(())
            });
            fields.add_field_method_get("cb_aiflags", |_lua, api| Ok(api.entity().cb_aiflags()));
            fields.add_field_method_set("cb_aiflags", |_lua, api, value: i64| {
                api.entity_mut().set_cb_aiflags(value as i16);
                Ok(())
            });
            fields.add_field_method_get("cb_repause", |_lua, api| Ok(api.entity().cb_repause()));
            fields.add_field_method_set("cb_repause", |_lua, api, value: i64| {
                api.entity_mut().set_cb_repause(value as i16);
                Ok(())
            });

            // The chimera's typed views over the same scratch block: the
            // signed re-target pause (+0x172), fade-freeze (+0x174), wall-stuck
            // count (+0x178) and far-target latch (+0x17A). Its turn step,
            // path latch, touch latch, wall hit and grab meter reuse the
            // generic accessors above.
            fields.add_field_method_get("c_repause", |_lua, api| Ok(api.entity().c_repause()));
            fields.add_field_method_set("c_repause", |_lua, api, value: i64| {
                api.entity_mut().set_c_repause(value as i16);
                Ok(())
            });
            fields.add_field_method_get("c_fade_freeze", |_lua, api| {
                Ok(api.entity().c_fade_freeze())
            });
            fields.add_field_method_set("c_fade_freeze", |_lua, api, value: i64| {
                api.entity_mut().set_c_fade_freeze(value as i16);
                Ok(())
            });
            fields.add_field_method_get("c_wall_frames", |_lua, api| {
                Ok(api.entity().c_wall_frames())
            });
            fields.add_field_method_set("c_wall_frames", |_lua, api, value: i64| {
                api.entity_mut().set_c_wall_frames(value as i16);
                Ok(())
            });
            fields.add_field_method_get("c_far_latch", |_lua, api| Ok(api.entity().c_far_latch()));
            fields.add_field_method_set("c_far_latch", |_lua, api, value: i64| {
                api.entity_mut().set_c_far_latch(value as i16);
                Ok(())
            });

            // The hunter's typed views over its scratch block. Every accessor
            // owns the composition or the signed width, so the script never
            // does byte math.
            fields
                .add_field_method_get("hunter_speed", |_lua, api| Ok(api.entity().hunter_speed()));
            fields.add_field_method_set("hunter_speed", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_speed(value as i16);
                Ok(())
            });
            fields
                .add_field_method_get("hunter_ticks", |_lua, api| Ok(api.entity().hunter_ticks()));
            fields.add_field_method_set("hunter_ticks", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_ticks(value as i16);
                Ok(())
            });
            fields.add_field_method_get("hunter_path_latch", |_lua, api| {
                Ok(api.entity().hunter_path_latch())
            });
            fields.add_field_method_set("hunter_path_latch", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_path_latch(value as i16);
                Ok(())
            });
            fields.add_field_method_get("hunter_grab_word", |_lua, api| {
                Ok(api.entity().hunter_grab_word())
            });
            fields.add_field_method_set("hunter_grab_word", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_grab_word(value as i16);
                Ok(())
            });
            fields.add_field_method_get("hunter_target_x", |_lua, api| {
                Ok(api.entity().hunter_target_x())
            });
            fields.add_field_method_set("hunter_target_x", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_target_x(value as i16);
                Ok(())
            });
            fields.add_field_method_get("hunter_target_z", |_lua, api| {
                Ok(api.entity().hunter_target_z())
            });
            fields.add_field_method_set("hunter_target_z", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_target_z(value as i16);
                Ok(())
            });
            fields.add_field_method_get("hunter_joint_sel", |_lua, api| {
                Ok(api.entity().hunter_joint_sel())
            });
            fields.add_field_method_set("hunter_joint_sel", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_joint_sel(value as u16);
                Ok(())
            });
            fields.add_field_method_get("hunter_step_word", |_lua, api| {
                Ok(api.entity().hunter_step_word())
            });
            fields.add_field_method_set("hunter_step_word", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_step_word(value as u16);
                Ok(())
            });
            fields.add_field_method_get("hunter_room_hit", |_lua, api| {
                Ok(api.entity().hunter_room_hit())
            });
            fields.add_field_method_set("hunter_room_hit", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_room_hit(value as u8);
                Ok(())
            });
            fields.add_field_method_get("hunter_pounce_latch", |_lua, api| {
                Ok(api.entity().hunter_pounce_latch())
            });
            fields.add_field_method_set("hunter_pounce_latch", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_pounce_latch(value as u8);
                Ok(())
            });
            fields.add_field_method_get("hunter_death_cnt_a", |_lua, api| {
                Ok(api.entity().hunter_death_cnt_a())
            });
            fields.add_field_method_set("hunter_death_cnt_a", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_death_cnt_a(value as u8);
                Ok(())
            });
            fields.add_field_method_get("hunter_death_cnt_b", |_lua, api| {
                Ok(api.entity().hunter_death_cnt_b())
            });
            fields.add_field_method_set("hunter_death_cnt_b", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_death_cnt_b(value as u8);
                Ok(())
            });
            fields.add_field_method_get("hunter_strafe_dir", |_lua, api| {
                Ok(api.entity().hunter_strafe_dir())
            });
            fields.add_field_method_set("hunter_strafe_dir", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_strafe_dir(value as u8);
                Ok(())
            });
            fields.add_field_method_get("hunter_approach_cnt", |_lua, api| {
                Ok(api.entity().hunter_approach_cnt())
            });
            fields.add_field_method_set("hunter_approach_cnt", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_approach_cnt(value as u8);
                Ok(())
            });
            fields
                .add_field_method_get("hunter_poise", |_lua, api| Ok(api.entity().hunter_poise()));
            fields.add_field_method_set("hunter_poise", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_poise(value as u8);
                Ok(())
            });
            fields.add_field_method_get("hunter_repause", |_lua, api| {
                Ok(api.entity().hunter_repause())
            });
            fields.add_field_method_set("hunter_repause", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_repause(value as u8);
                Ok(())
            });
            fields.add_field_method_get("hunter_leap_flag", |_lua, api| {
                Ok(api.entity().hunter_leap_flag())
            });
            fields.add_field_method_set("hunter_leap_flag", |_lua, api, value: i64| {
                api.entity_mut().set_hunter_leap_flag(value as u8);
                Ok(())
            });
            fields.add_field_method_get("hunter_partner", |_lua, api| {
                Ok(api.entity().hunter_partner)
            });
            fields.add_field_method_set("hunter_partner", |_lua, api, value: i64| {
                api.entity_mut().hunter_partner = value as u8;
                Ok(())
            });

            // The stored `Add_speedXZ` vector, split so the drags can read one
            // axis (the hunter's recover and leap handlers re-add the words).
            fields.add_field_method_get("speed_x", |_lua, api| Ok(api.entity().speed[0]));
            fields.add_field_method_get("speed_y", |_lua, api| Ok(api.entity().speed[1]));
            fields.add_field_method_get("speed_z", |_lua, api| Ok(api.entity().speed[2]));

            // The shared globals the hunter's decisions run through: its
            // scratch distances, the one-per-room howl latch, the intro row,
            // the grab one-shot and the outgoing room id the intro's camera
            // selector reads.
            fields.add_field_method_get("player_displacement", |_lua, api| {
                Ok(api.game_ref().player_displacement)
            });
            fields.add_field_method_set("player_displacement", |_lua, api, value: i64| {
                api.game().player_displacement = value as i32;
                Ok(())
            });
            fields.add_field_method_get("player_distance_z", |_lua, api| {
                Ok(api.game_ref().player_distance_z)
            });
            fields.add_field_method_set("player_distance_z", |_lua, api, value: i64| {
                api.game().player_distance_z = value as i32;
                Ok(())
            });
            fields.add_field_method_get("scaled_down_dist", |_lua, api| {
                Ok(api.game_ref().scaled_down_dist)
            });
            fields.add_field_method_set("scaled_down_dist", |_lua, api, value: i64| {
                api.game().scaled_down_dist = value as i32;
                Ok(())
            });
            fields.add_field_method_get("hunter_scream_latch", |_lua, api| {
                Ok(api.game_ref().hunter_scream_latch)
            });
            fields.add_field_method_set("hunter_scream_latch", |_lua, api, value: i64| {
                api.game().hunter_scream_latch = value as u8;
                Ok(())
            });
            fields.add_field_method_get("hunter_intro_jump_kind", |_lua, api| {
                Ok(api.game_ref().hunter_intro_jump_kind)
            });
            fields.add_field_method_set("hunter_intro_jump_kind", |_lua, api, value: i64| {
                api.game().hunter_intro_jump_kind = value as u8;
                Ok(())
            });
            fields.add_field_method_get("hunter_grab_one_shot", |_lua, api| {
                Ok(api.game_ref().hunter_grab_one_shot)
            });
            fields.add_field_method_set("hunter_grab_one_shot", |_lua, api, value: bool| {
                api.game().hunter_grab_one_shot = value;
                Ok(())
            });
            fields.add_field_method_get("attract_room_camera_id", |_lua, api| {
                Ok(api.game_ref().attract_room_camera_id)
            });
            // `get_stage_id()`: the original's 0-based stage digit (the room
            // id's stage is 1-based).
            fields.add_field_method_get("stage_id", |_lua, api| {
                Ok(api.game_ref().id.stage.saturating_sub(1))
            });

            // The zombie's state-word aliases and scratch views. The four
            // bytes at +0x84 and their mirror at +0x184 are read and written
            // as one dword, exactly like the original's block copies; the
            // remaining accessors own the little-endian compositions and
            // signed widths.
            fields.add_field_method_get("state_word", |_lua, api| Ok(api.entity().state_word()));
            fields.add_field_method_set("state_word", |_lua, api, value: i64| {
                api.entity_mut().set_state_word(value as u32);
                Ok(())
            });
            fields.add_field_method_get("state_mirror", |_lua, api| Ok(api.entity().state_mirror));
            fields.add_field_method_set("state_mirror", |_lua, api, value: i64| {
                api.entity_mut().state_mirror = value as u32;
                Ok(())
            });
            field!("move_speed_byte", move_speed_byte, u8);
            field!("turn_speed", turn_speed, u8);
            field!("internal_timer", internal_timer, u8);
            field!("stagger_timer", stagger_timer, u8);
            field!("action_speed", action_speed, u8);
            field!("hit_threshold", hit_threshold, u8);
            field!("behavior_step", behavior_step, u8);
            field!("action_counter", action_counter, u8);
            field!("next_turn_timer", next_turn_timer, u16);
            // The high byte of the +0x176 word, the zombie bite countdown
            // (`ATTACK_TIMER`); the low half is `reaction_timer`.
            fields.add_field_method_get("attack_timer", |_lua, api| {
                Ok((api.entity().reaction_timer as u16 >> 8) as u8)
            });
            fields.add_field_method_set("attack_timer", |_lua, api, value: i64| {
                let low = (api.entity().reaction_timer as u16) & 0xFF;
                api.entity_mut().reaction_timer = (low | ((value as u8 as u16) << 8)) as i16;
                Ok(())
            });
            fields.add_field_method_get("zombie_turn_delta", |_lua, api| {
                Ok(api.entity().zombie_turn_delta())
            });
            fields.add_field_method_set("zombie_turn_delta", |_lua, api, value: i64| {
                api.entity_mut().set_zombie_turn_delta(value as i16);
                Ok(())
            });
            fields.add_field_method_get("zombie_move_word", |_lua, api| {
                Ok(api.entity().zombie_move_word())
            });
            fields.add_field_method_set("zombie_move_word", |_lua, api, value: i64| {
                api.entity_mut().set_zombie_move_word(value as u16);
                Ok(())
            });
            fields.add_field_method_get("zombie_part_word", |_lua, api| {
                Ok(api.entity().zombie_part_word())
            });
            fields.add_field_method_set("zombie_part_word", |_lua, api, value: i64| {
                api.entity_mut().set_zombie_part_word(value as u16);
                Ok(())
            });
            fields.add_field_method_get("zombie_wander_word", |_lua, api| {
                Ok(api.entity().zombie_wander_word())
            });
            fields.add_field_method_set("zombie_wander_word", |_lua, api, value: i64| {
                api.entity_mut().set_zombie_wander_word(value as u16);
                Ok(())
            });
            fields.add_field_method_get("zombie_unk_178", |_lua, api| {
                Ok(api.entity().zombie_unk_178())
            });
            fields.add_field_method_set("zombie_unk_178", |_lua, api, value: i64| {
                api.entity_mut().set_zombie_unk_178(value as u8);
                Ok(())
            });
            fields.add_field_method_get("zombie_unk_179", |_lua, api| {
                Ok(api.entity().zombie_unk_179())
            });
            fields.add_field_method_set("zombie_unk_179", |_lua, api, value: i64| {
                api.entity_mut().set_zombie_unk_179(value as u8);
                Ok(())
            });

            // The scripted ground-shadow queue gate (the spiders' ceiling
            // variant suppresses the quad until the drop clears spawn bit 1).
            fields.add_field_method_get("shadow_suppressed", |_lua, api| {
                Ok(api.entity().shadow_suppressed)
            });
            fields.add_field_method_set("shadow_suppressed", |_lua, api, value: bool| {
                api.entity_mut().shadow_suppressed = value;
                Ok(())
            });

            // The model tint record and its live multiplier, read-only:
            // scripts set them through `tint_model`/`retarget_tint`.
            fields.add_field_method_get("model_tint_r", |_lua, api| {
                Ok(api.entity().model_tint_rgb()[0])
            });
            fields.add_field_method_get("model_tint_g", |_lua, api| {
                Ok(api.entity().model_tint_rgb()[1])
            });
            fields.add_field_method_get("model_tint_b", |_lua, api| {
                Ok(api.entity().model_tint_rgb()[2])
            });
            fields.add_field_method_get("tint_queue_r", |_lua, api| Ok(api.entity().tint_queue[0]));
            fields.add_field_method_get("tint_queue_g", |_lua, api| Ok(api.entity().tint_queue[1]));
            fields.add_field_method_get("tint_queue_b", |_lua, api| Ok(api.entity().tint_queue[2]));
            fields.add_field_method_get("tint_queue_word_a", |_lua, api| {
                Ok(api.entity().tint_queue_word_a)
            });
            fields.add_field_method_get("tint_queue_word_b", |_lua, api| {
                Ok(api.entity().tint_queue_word_b)
            });
            fields.add_field_method_get("tint_queue_armed", |_lua, api| {
                Ok(api.entity().tint_queue_armed)
            });

            // The look-at target word, split into scalars like `pos`.
            fields.add_field_method_get("target_x", |_lua, api| Ok(api.entity().target[0]));
            fields.add_field_method_set("target_x", |_lua, api, value: i64| {
                api.entity_mut().target[0] = value as i32;
                Ok(())
            });
            fields.add_field_method_get("target_y", |_lua, api| Ok(api.entity().target[1]));
            fields.add_field_method_set("target_y", |_lua, api, value: i64| {
                api.entity_mut().target[1] = value as i32;
                Ok(())
            });
            fields.add_field_method_get("target_z", |_lua, api| Ok(api.entity().target[2]));
            fields.add_field_method_set("target_z", |_lua, api, value: i64| {
                api.entity_mut().target[2] = value as i32;
                Ok(())
            });

            // The stored collision-accepted position (the original's
            // `position` word); the current position until a driver stores one.
            fields.add_field_method_get("prev_pos_x", |_lua, api| {
                let entity = api.entity();
                Ok(entity.saved_pos.unwrap_or(entity.pos)[0])
            });
            fields.add_field_method_get("prev_pos_y", |_lua, api| {
                let entity = api.entity();
                Ok(entity.saved_pos.unwrap_or(entity.pos)[1])
            });
            fields.add_field_method_get("prev_pos_z", |_lua, api| {
                let entity = api.entity();
                Ok(entity.saved_pos.unwrap_or(entity.pos)[2])
            });

            // The original's 16-bit `position` words at entity +0x6C
            // (`saved_pos`): the hound's spawn nudge rewrites them and its
            // probe saves and restores them.
            fields.add_field_method_get("saved_x", |_lua, api| {
                let entity = api.entity();
                Ok(entity.saved_pos.unwrap_or(entity.pos)[0] as u16 as i16)
            });
            fields.add_field_method_set("saved_x", |_lua, api, value: i64| {
                let entity = api.entity_mut();
                let mut pos = entity.saved_pos.unwrap_or(entity.pos);
                pos[0] = i32::from(value as u16 as i16);
                entity.saved_pos = Some(pos);
                Ok(())
            });
            fields.add_field_method_get("saved_y", |_lua, api| {
                let entity = api.entity();
                Ok(entity.saved_pos.unwrap_or(entity.pos)[1] as u16 as i16)
            });
            fields.add_field_method_set("saved_y", |_lua, api, value: i64| {
                let entity = api.entity_mut();
                let mut pos = entity.saved_pos.unwrap_or(entity.pos);
                pos[1] = i32::from(value as u16 as i16);
                entity.saved_pos = Some(pos);
                Ok(())
            });
            fields.add_field_method_get("saved_z", |_lua, api| {
                let entity = api.entity();
                Ok(entity.saved_pos.unwrap_or(entity.pos)[2] as u16 as i16)
            });
            fields.add_field_method_set("saved_z", |_lua, api, value: i64| {
                let entity = api.entity_mut();
                let mut pos = entity.saved_pos.unwrap_or(entity.pos);
                pos[2] = i32::from(value as u16 as i16);
                entity.saved_pos = Some(pos);
                Ok(())
            });

            // The shared probe/position scratch vector the hound's leap
            // recovery steers at (the original's `g_playerPosScratch`, written
            // by the forward probe each frame).
            fields.add_field_method_get("scratch_x", |_lua, api| Ok(api.game_ref().scratch_vec[0]));
            fields.add_field_method_set("scratch_x", |_lua, api, value: i64| {
                api.game().scratch_vec[0] = value as i32;
                Ok(())
            });
            fields.add_field_method_get("scratch_z", |_lua, api| Ok(api.game_ref().scratch_vec[2]));
            fields.add_field_method_set("scratch_z", |_lua, api, value: i64| {
                api.game().scratch_vec[2] = value as i32;
                Ok(())
            });

            // The hounds' shared "already barked" latch, one per room.
            fields.add_field_method_get("cerberus_barked", |_lua, api| {
                Ok(api.game_ref().cerberus_barked)
            });
            fields.add_field_method_set("cerberus_barked", |_lua, api, value: bool| {
                api.game().cerberus_barked = value;
                Ok(())
            });

            // Position components; the array is split into three scalars so a
            // script can read or steer one axis at a time.
            fields.add_field_method_get("pos_x", |_lua, api| Ok(api.entity().pos[0]));
            fields.add_field_method_set("pos_x", |_lua, api, value: i64| {
                api.entity_mut().pos[0] = value as i32;
                Ok(())
            });
            fields.add_field_method_get("pos_y", |_lua, api| Ok(api.entity().pos[1]));
            fields.add_field_method_set("pos_y", |_lua, api, value: i64| {
                api.entity_mut().pos[1] = value as i32;
                Ok(())
            });
            fields.add_field_method_get("pos_z", |_lua, api| Ok(api.entity().pos[2]));
            fields.add_field_method_set("pos_z", |_lua, api, value: i64| {
                api.entity_mut().pos[2] = value as i32;
                Ok(())
            });

            // The room the entity belongs to, for the story-flag pose variants.
            fields.add_field_method_get("stage", |_lua, api| Ok(api.game_ref().id.stage));
            fields.add_field_method_get("room", |_lua, api| Ok(api.game_ref().id.room));
            // The per-frame platform random snapshot (`g_RandSeed`).
            fields.add_field_method_get("rand_seed", |_lua, api| Ok(api.game_ref().rand_seed));
            // A write reseeds the frame snapshot (`g_RandSeed`) so the same
            // frame's later readers see it; the stream itself is `e:srand`.
            fields.add_field_method_set("rand_seed", |_lua, api, value: i64| {
                api.game().rand_seed = value as u16;
                Ok(())
            });

            // The player-side surface the attacking monsters read: health and
            // its status byte, the attacked latch, the poison countdown and
            // the facing the grab pose latches.
            fields.add_field_method_get("player_health", |_lua, api| {
                Ok(api.game_ref().entities[0].health)
            });
            fields.add_field_method_set("player_health", |_lua, api, value: i64| {
                api.game().entities[0].health = value as i16;
                Ok(())
            });
            fields.add_field_method_get("player_attacked", |_lua, api| {
                Ok(api.game_ref().entities[0].is_being_attacked)
            });
            fields.add_field_method_set("player_attacked", |_lua, api, value: i64| {
                api.game().entities[0].is_being_attacked = value as u8;
                Ok(())
            });
            // The player's flags byte (offset 0x00): the hound's kill and maul
            // force the no-control bits onto it.
            fields
                .add_field_method_get("player_flags", |_lua, api| Ok(api.game_ref().player_flags));
            fields.add_field_method_set("player_flags", |_lua, api, value: i64| {
                api.game().player_flags = value as u8;
                Ok(())
            });
            // The player's animation position offsets (`unk_c6`/`unk_c8`), the
            // grab point both sides of a bite store.
            fields.add_field_method_get("player_unk_c6", |_lua, api| {
                Ok(api.game_ref().entities[0].unk_c6)
            });
            fields.add_field_method_set("player_unk_c6", |_lua, api, value: i64| {
                api.game().entities[0].unk_c6 = value as u16;
                Ok(())
            });
            fields.add_field_method_get("player_unk_c8", |_lua, api| {
                Ok(api.game_ref().entities[0].unk_c8)
            });
            fields.add_field_method_set("player_unk_c8", |_lua, api, value: i64| {
                api.game().entities[0].unk_c8 = value as u16;
                Ok(())
            });
            fields.add_field_method_get("player_health_status", |_lua, api| {
                Ok(api.game_ref().health_status)
            });
            fields.add_field_method_set("player_health_status", |_lua, api, value: i64| {
                api.game().health_status = value as u8;
                Ok(())
            });
            fields.add_field_method_get("player_poison_timer", |_lua, api| {
                Ok(api.game_ref().poison_timer)
            });
            fields.add_field_method_get("player_angle", |_lua, api| {
                Ok(api.game_ref().entities[0].angle)
            });
            fields.add_field_method_set("player_angle", |_lua, api, value: i64| {
                api.game().entities[0].angle = value as u16;
                Ok(())
            });
            // The player's attack-animation fields the zombie bite writes:
            // `attackAnim` and `animFrameId` are the entity animation words,
            // `attackDirection` reinterprets the +0xC4 word and `attackTimer`
            // the +0xE2 word. The player's `animationId`/`animFrameId` bytes
            // are the entity state block's two bytes at +0x84.
            fields.add_field_method_get("player_state", |_lua, api| {
                Ok(api.game_ref().entities[0].state())
            });
            fields.add_field_method_set("player_state", |_lua, api, value: i64| {
                api.game().entities[0].set_state(value as u8);
                Ok(())
            });
            fields.add_field_method_get("player_anim_frame_id", |_lua, api| {
                Ok(api.game_ref().entities[0].ignore())
            });
            fields.add_field_method_set("player_anim_frame_id", |_lua, api, value: i64| {
                api.game().entities[0].set_ignore(value as u8);
                Ok(())
            });
            fields.add_field_method_get("player_attack_anim", |_lua, api| {
                Ok(api.game_ref().entities[0].attack_anim)
            });
            fields.add_field_method_set("player_attack_anim", |_lua, api, value: i64| {
                // The original's dword store leaves both the generic clip id
                // and the player's `attackAnim` name on the same value.
                let player = &mut api.game().entities[0];
                player.attack_anim = value as u8;
                player.animation_id = value as u8;
                Ok(())
            });
            fields.add_field_method_get("player_attack_direction", |_lua, api| {
                Ok(api.game_ref().entities[0].action_ticks_counter)
            });
            fields.add_field_method_set("player_attack_direction", |_lua, api, value: i64| {
                api.game().entities[0].action_ticks_counter = value as u16;
                Ok(())
            });
            fields.add_field_method_get("player_attack_timer", |_lua, api| {
                Ok(api.game_ref().entities[0].next_turn_timer)
            });
            fields.add_field_method_set("player_attack_timer", |_lua, api, value: i64| {
                api.game().entities[0].next_turn_timer = value as u16;
                Ok(())
            });
            fields.add_field_method_set("player_animation_frame_id", |_lua, api, value: i64| {
                api.game().entities[0].animation_frame_id = value as u8;
                Ok(())
            });
            // The player's live translation speed, the knockdown crush test's
            // "moving" gate.
            fields.add_field_method_get("player_move_speed", |_lua, api| {
                Ok(api.game_ref().entities[0].move_speed_current)
            });
            // The player's height, the plant's drop tests read it directly.
            fields.add_field_method_get("player_pos_y", |_lua, api| {
                Ok(api.game_ref().entities[0].pos[1])
            });
            // The player's animation id and frame, the two bytes the crow's
            // perched-scatter cue reads as part of the state dword and the grab
            // peck cadence tests against `% 0x14`: `player_animation_id` is the
            // original's animationId (+0x84, the entity state byte) and
            // `player_animation_frame_id` the clip frame (+0xBE).
            fields.add_field_method_get("player_animation_id", |_lua, api| {
                Ok(api.game_ref().entities[0].state())
            });
            fields.add_field_method_get("player_animation_frame_id", |_lua, api| {
                Ok(api.game_ref().entities[0].animation_frame_id)
            });
            // `Flg_ck(g_ScenarioFlags, SCENARIO_FLAG_SECOND_PLAYTHROUGH)`.
            fields.add_field_method_get("second_playthrough", |_lua, api| {
                Ok(
                    api.game_ref().flags[usize::from(crate::game::BANK_SCENARIO)]
                        .bit(crate::game::SCENARIO_FLAG_SECOND_PLAYTHROUGH),
                )
            });
            // The player animation words an attack writes one at a time (the
            // adder's bite sets only the behavior byte, unlike the grab pose).
            fields.add_field_method_get("player_action_behavior", |_lua, api| {
                Ok(api.game_ref().entities[0].action_behavior)
            });
            fields.add_field_method_set("player_action_behavior", |_lua, api, value: i64| {
                api.game().entities[0].action_behavior = value as u8;
                Ok(())
            });
            fields.add_field_method_get("player_action_state", |_lua, api| {
                Ok(api.game_ref().entities[0].action_state)
            });
            fields.add_field_method_set("player_action_state", |_lua, api, value: i64| {
                api.game().entities[0].action_state = value as u8;
                Ok(())
            });

            // The monster plant's signed 16/32-bit scratch views; each maps
            // onto the byte fields the original's raw offsets actually
            // overlap. `mp_state_bk` is the full state-dword snapshot the
            // damaged state restores.
            fields.add_field_method_get("mp_dist", |_lua, api| Ok(api.entity().mp_dist()));
            fields.add_field_method_set("mp_dist", |_lua, api, value: i64| {
                api.entity_mut().set_mp_dist(value as i32);
                Ok(())
            });
            fields.add_field_method_get("mp_holdoff", |_lua, api| Ok(api.entity().mp_holdoff()));
            fields.add_field_method_set("mp_holdoff", |_lua, api, value: i64| {
                api.entity_mut().set_mp_holdoff(value as i16);
                Ok(())
            });
            fields.add_field_method_get("mp_anim_end", |_lua, api| Ok(api.entity().mp_anim_end()));
            fields.add_field_method_set("mp_anim_end", |_lua, api, value: i64| {
                api.entity_mut().set_mp_anim_end(value as i16);
                Ok(())
            });
            fields.add_field_method_get("mp_seg", |_lua, api| Ok(api.entity().mp_seg()));
            fields.add_field_method_set("mp_seg", |_lua, api, value: i64| {
                api.entity_mut().set_mp_seg(value as i16);
                Ok(())
            });
            fields.add_field_method_get("mp_timer_a", |_lua, api| Ok(api.entity().mp_timer_a()));
            fields.add_field_method_set("mp_timer_a", |_lua, api, value: i64| {
                api.entity_mut().set_mp_timer_a(value as i16);
                Ok(())
            });
            fields.add_field_method_get("mp_timer_b", |_lua, api| Ok(api.entity().mp_timer_b()));
            fields.add_field_method_set("mp_timer_b", |_lua, api, value: i64| {
                api.entity_mut().set_mp_timer_b(value as i16);
                Ok(())
            });
            fields.add_field_method_get("mp_fidget", |_lua, api| Ok(api.entity().mp_fidget()));
            fields.add_field_method_set("mp_fidget", |_lua, api, value: i64| {
                api.entity_mut().set_mp_fidget(value as i16);
                Ok(())
            });
            fields.add_field_method_get("mp_alerted", |_lua, api| Ok(api.entity().mp_alerted()));
            fields.add_field_method_set("mp_alerted", |_lua, api, value: i64| {
                api.entity_mut().set_mp_alerted(value as i16);
                Ok(())
            });
            fields.add_field_method_get("mp_angle_bk", |_lua, api| Ok(api.entity().mp_angle_bk()));
            fields.add_field_method_set("mp_angle_bk", |_lua, api, value: i64| {
                api.entity_mut().set_mp_angle_bk(value as i16);
                Ok(())
            });
            fields.add_field_method_get("mp_swerve", |_lua, api| Ok(api.entity().mp_swerve()));
            fields.add_field_method_set("mp_swerve", |_lua, api, value: i64| {
                api.entity_mut().set_mp_swerve(value as i16);
                Ok(())
            });
            fields.add_field_method_get("mp_shadow", |_lua, api| Ok(api.entity().mp_shadow()));
            fields.add_field_method_set("mp_shadow", |_lua, api, value: i64| {
                api.entity_mut().set_mp_shadow(value as i16);
                Ok(())
            });
            fields.add_field_method_get("mp_hits", |_lua, api| Ok(api.entity().mp_hits()));
            fields.add_field_method_set("mp_hits", |_lua, api, value: i64| {
                api.entity_mut().set_mp_hits(value as i16);
                Ok(())
            });
            fields.add_field_method_get("mp_state_bk", |_lua, api| Ok(api.entity().state_mirror));
            fields.add_field_method_set("mp_state_bk", |_lua, api, value: i64| {
                api.entity_mut().state_mirror = value as u32;
                Ok(())
            });
            fields
                .add_field_method_get("mp_step_word", |_lua, api| Ok(api.entity().mp_step_word()));
            fields.add_field_method_set("mp_step_word", |_lua, api, value: i64| {
                api.entity_mut().set_mp_step_word(value as u16);
                Ok(())
            });
            fields.add_field_method_get("monster_plant_sides", |_lua, api| {
                Ok(api.game_ref().monster_plant_sides)
            });
            fields.add_field_method_set("monster_plant_sides", |_lua, api, value: i64| {
                api.game().monster_plant_sides = value as u32;
                Ok(())
            });

            // The computer arms' 16.16 fixed-point scratch: two XZ velocity
            // words, the XZ position pair and the left arm's Y pair, plus the
            // typing gate word.
            fields.add_field_method_get("arm_vel_x", |_lua, api| Ok(api.entity().arm_vel_x()));
            fields.add_field_method_set("arm_vel_x", |_lua, api, value: i64| {
                api.entity_mut().set_arm_vel_x(value as i32);
                Ok(())
            });
            fields.add_field_method_get("arm_vel_z", |_lua, api| Ok(api.entity().arm_vel_z()));
            fields.add_field_method_set("arm_vel_z", |_lua, api, value: i64| {
                api.entity_mut().set_arm_vel_z(value as i32);
                Ok(())
            });
            fields.add_field_method_get("arm_pos_x", |_lua, api| Ok(api.entity().arm_pos_x()));
            fields.add_field_method_set("arm_pos_x", |_lua, api, value: i64| {
                api.entity_mut().set_arm_pos_x(value as i32);
                Ok(())
            });
            fields.add_field_method_get("arm_pos_z", |_lua, api| Ok(api.entity().arm_pos_z()));
            fields.add_field_method_set("arm_pos_z", |_lua, api, value: i64| {
                api.entity_mut().set_arm_pos_z(value as i32);
                Ok(())
            });
            fields.add_field_method_get("arm_y", |_lua, api| Ok(api.entity().arm_y()));
            fields.add_field_method_set("arm_y", |_lua, api, value: i64| {
                api.entity_mut().set_arm_y(value as i32);
                Ok(())
            });
            fields.add_field_method_get("arm_vel_y", |_lua, api| Ok(api.entity().arm_vel_y()));
            fields.add_field_method_set("arm_vel_y", |_lua, api, value: i64| {
                api.entity_mut().set_arm_vel_y(value as i32);
                Ok(())
            });
            fields.add_field_method_get("arm_gate", |_lua, api| Ok(api.entity().arm_gate()));
            fields.add_field_method_set("arm_gate", |_lua, api, value: i64| {
                api.entity_mut().set_arm_gate(value as i16);
                Ok(())
            });

            // Plant 42's raw scratch views: the two signed idle-jitter bytes,
            // the two suspend oscillators, the sweep step/direction, the
            // active grab joint, the two effect timers, the 32-bit distance,
            // the ambient life timer, the death/bounce counters and the
            // signed frame timer. Each re-reads bytes the generic fields above
            // also expose, at the widths the plant's own handlers use.
            fields.add_field_method_get("p42_yaw_jitter", |_lua, api| {
                Ok(api.entity().p42_yaw_jitter())
            });
            fields.add_field_method_set("p42_yaw_jitter", |_lua, api, value: i64| {
                api.entity_mut().set_p42_yaw_jitter(value as i8);
                Ok(())
            });
            fields.add_field_method_get("p42_roll_jitter", |_lua, api| {
                Ok(api.entity().p42_roll_jitter())
            });
            fields.add_field_method_set("p42_roll_jitter", |_lua, api, value: i64| {
                api.entity_mut().set_p42_roll_jitter(value as i8);
                Ok(())
            });
            fields.add_field_method_get("p42_osc_a", |_lua, api| Ok(api.entity().p42_osc_a()));
            fields.add_field_method_set("p42_osc_a", |_lua, api, value: i64| {
                api.entity_mut().set_p42_osc_a(value as i16);
                Ok(())
            });
            fields.add_field_method_get("p42_osc_b", |_lua, api| Ok(api.entity().p42_osc_b()));
            fields.add_field_method_set("p42_osc_b", |_lua, api, value: i64| {
                api.entity_mut().set_p42_osc_b(value as i16);
                Ok(())
            });
            fields.add_field_method_get("p42_step", |_lua, api| Ok(api.entity().p42_step()));
            fields.add_field_method_set("p42_step", |_lua, api, value: i64| {
                api.entity_mut().set_p42_step(value as i16);
                Ok(())
            });
            fields.add_field_method_get("p42_sweep_dir", |_lua, api| {
                Ok(api.entity().p42_sweep_dir())
            });
            fields.add_field_method_set("p42_sweep_dir", |_lua, api, value: i64| {
                api.entity_mut().set_p42_sweep_dir(value as u8);
                Ok(())
            });
            fields.add_field_method_get("p42_grab_joint", |_lua, api| {
                Ok(api.entity().p42_grab_joint())
            });
            fields.add_field_method_set("p42_grab_joint", |_lua, api, value: i64| {
                api.entity_mut().set_p42_grab_joint(value as i8);
                Ok(())
            });
            fields.add_field_method_get("p42_fx_timer_a", |_lua, api| {
                Ok(api.entity().p42_fx_timer_a())
            });
            fields.add_field_method_set("p42_fx_timer_a", |_lua, api, value: i64| {
                api.entity_mut().set_p42_fx_timer_a(value as u8);
                Ok(())
            });
            fields.add_field_method_get("p42_fx_timer_b", |_lua, api| {
                Ok(api.entity().p42_fx_timer_b())
            });
            fields.add_field_method_set("p42_fx_timer_b", |_lua, api, value: i64| {
                api.entity_mut().set_p42_fx_timer_b(value as u8);
                Ok(())
            });
            fields.add_field_method_get("p42_dist", |_lua, api| Ok(api.entity().p42_dist()));
            fields.add_field_method_set("p42_dist", |_lua, api, value: i64| {
                api.entity_mut().set_p42_dist(value as u32);
                Ok(())
            });
            fields.add_field_method_get("p42_life", |_lua, api| Ok(api.entity().p42_life()));
            fields.add_field_method_set("p42_life", |_lua, api, value: i64| {
                api.entity_mut().set_p42_life(value as i16);
                Ok(())
            });
            fields.add_field_method_get("p42_death_counter", |_lua, api| {
                Ok(api.entity().p42_death_counter())
            });
            fields.add_field_method_set("p42_death_counter", |_lua, api, value: i64| {
                api.entity_mut().set_p42_death_counter(value as i8);
                Ok(())
            });
            fields.add_field_method_get("p42_pod_counter", |_lua, api| {
                Ok(api.entity().p42_pod_counter())
            });
            fields.add_field_method_set("p42_pod_counter", |_lua, api, value: i64| {
                api.entity_mut().set_p42_pod_counter(value as i8);
                Ok(())
            });
            fields.add_field_method_get("p42_ticks", |_lua, api| Ok(api.entity().p42_ticks()));
            fields.add_field_method_set("p42_ticks", |_lua, api, value: i64| {
                api.entity_mut().set_p42_ticks(value as i16);
                Ok(())
            });

            // Yawn's raw scratch views: the turn accelerator, form, bite
            // counter, scale ramp, XZ speed, ground height, death shrink,
            // saved state dword, bite direction/recovery, path index, the
            // nearly-dead flag, the sparkle timer, the stuck pair and the
            // hiss/bite cooldowns. Each re-reads bytes the generic fields
            // above also expose, at the widths Yawn's handlers use.
            fields.add_field_method_get("yawn_turn_accel", |_lua, api| {
                Ok(api.entity().yawn_turn_accel())
            });
            fields.add_field_method_set("yawn_turn_accel", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_turn_accel(value as i8);
                Ok(())
            });
            fields.add_field_method_get("yawn_form", |_lua, api| Ok(api.entity().yawn_form()));
            fields.add_field_method_set("yawn_form", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_form(value as u8);
                Ok(())
            });
            fields.add_field_method_get("yawn_bites", |_lua, api| Ok(api.entity().yawn_bites()));
            fields.add_field_method_set("yawn_bites", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_bites(value as u8);
                Ok(())
            });
            fields.add_field_method_get("yawn_scale_ramp", |_lua, api| {
                Ok(api.entity().yawn_scale_ramp())
            });
            fields.add_field_method_set("yawn_scale_ramp", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_scale_ramp(value as i16);
                Ok(())
            });
            fields.add_field_method_get("yawn_speed", |_lua, api| Ok(api.entity().yawn_speed()));
            fields.add_field_method_set("yawn_speed", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_speed(value as i16);
                Ok(())
            });
            fields.add_field_method_get("yawn_ground_y", |_lua, api| {
                Ok(api.entity().yawn_ground_y())
            });
            fields.add_field_method_set("yawn_ground_y", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_ground_y(value as i16);
                Ok(())
            });
            fields.add_field_method_get("yawn_shrink", |_lua, api| Ok(api.entity().yawn_shrink()));
            fields.add_field_method_set("yawn_shrink", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_shrink(value as i16);
                Ok(())
            });
            fields.add_field_method_get("yawn_state_bk", |_lua, api| {
                Ok(api.entity().yawn_state_bk())
            });
            fields.add_field_method_set("yawn_state_bk", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_state_bk(value as u32);
                Ok(())
            });
            fields.add_field_method_get("yawn_bite_dir", |_lua, api| {
                Ok(api.entity().yawn_bite_dir())
            });
            fields.add_field_method_set("yawn_bite_dir", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_bite_dir(value as i8);
                Ok(())
            });
            fields.add_field_method_get("yawn_recovery", |_lua, api| {
                Ok(api.entity().yawn_recovery())
            });
            fields.add_field_method_set("yawn_recovery", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_recovery(value as u8);
                Ok(())
            });
            fields.add_field_method_get("yawn_path_index", |_lua, api| {
                Ok(api.entity().yawn_path_index())
            });
            fields.add_field_method_set("yawn_path_index", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_path_index(value as u8);
                Ok(())
            });
            fields.add_field_method_get("yawn_nearly_dead", |_lua, api| {
                Ok(api.entity().yawn_nearly_dead())
            });
            fields.add_field_method_set("yawn_nearly_dead", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_nearly_dead(value as u8);
                Ok(())
            });
            fields
                .add_field_method_get("yawn_sparkle", |_lua, api| Ok(api.entity().yawn_sparkle()));
            fields.add_field_method_set("yawn_sparkle", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_sparkle(value as u16);
                Ok(())
            });
            fields.add_field_method_get("yawn_stuck", |_lua, api| Ok(api.entity().yawn_stuck()));
            fields.add_field_method_set("yawn_stuck", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_stuck(value as i8);
                Ok(())
            });
            fields.add_field_method_get("yawn_stuck_cool", |_lua, api| {
                Ok(api.entity().yawn_stuck_cool())
            });
            fields.add_field_method_set("yawn_stuck_cool", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_stuck_cool(value as i8);
                Ok(())
            });
            fields.add_field_method_get("yawn_hiss", |_lua, api| Ok(api.entity().yawn_hiss()));
            fields.add_field_method_set("yawn_hiss", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_hiss(value as i8);
                Ok(())
            });
            fields.add_field_method_get("yawn_bite_cool", |_lua, api| {
                Ok(api.entity().yawn_bite_cool())
            });
            fields.add_field_method_set("yawn_bite_cool", |_lua, api, value: i64| {
                api.entity_mut().set_yawn_bite_cool(value as i8);
                Ok(())
            });

            // The Tyrant's raw scratch views: the pad counter (+0x16D), the
            // hit mask (+0x16E), the look-at mode/repause (+0x16F), the state
            // word backup (+0x174), the ty_flags byte (+0x179), the attack
            // repause (+0x17F), the rocket latch (+0x180), the combo cooldown
            // (+0x181), the crowd counter (+0x182), the close counter
            // (+0x183) and the movement-distance word (+0x17C). Each re-reads
            // bytes the generic fields above also expose, at the widths the
            // Tyrant's handlers use.
            fields.add_field_method_get("ty_pad_counter", |_lua, api| {
                Ok(api.entity().ty_pad_counter())
            });
            fields.add_field_method_set("ty_pad_counter", |_lua, api, value: i64| {
                api.entity_mut().set_ty_pad_counter(value as u8);
                Ok(())
            });
            fields.add_field_method_get("ty_hit_mask", |_lua, api| Ok(api.entity().ty_hit_mask()));
            fields.add_field_method_set("ty_hit_mask", |_lua, api, value: i64| {
                api.entity_mut().set_ty_hit_mask(value as u8);
                Ok(())
            });
            fields
                .add_field_method_get("ty_look_mode", |_lua, api| Ok(api.entity().ty_look_mode()));
            fields.add_field_method_set("ty_look_mode", |_lua, api, value: i64| {
                api.entity_mut().set_ty_look_mode(value as u8);
                Ok(())
            });
            fields.add_field_method_get("ty_state_bk", |_lua, api| Ok(api.entity().ty_state_bk()));
            fields.add_field_method_set("ty_state_bk", |_lua, api, value: i64| {
                api.entity_mut().set_ty_state_bk(value as u32);
                Ok(())
            });
            fields.add_field_method_get("ty_flags", |_lua, api| Ok(api.entity().ty_flags()));
            fields.add_field_method_set("ty_flags", |_lua, api, value: i64| {
                api.entity_mut().set_ty_flags(value as u8);
                Ok(())
            });
            fields.add_field_method_get("ty_repause", |_lua, api| Ok(api.entity().ty_repause()));
            fields.add_field_method_set("ty_repause", |_lua, api, value: i64| {
                api.entity_mut().set_ty_repause(value as i8);
                Ok(())
            });
            fields.add_field_method_get("ty_rocket_flag", |_lua, api| {
                Ok(api.entity().ty_rocket_flag())
            });
            fields.add_field_method_set("ty_rocket_flag", |_lua, api, value: i64| {
                api.entity_mut().set_ty_rocket_flag(value as u8);
                Ok(())
            });
            fields.add_field_method_get("ty_cooldown", |_lua, api| Ok(api.entity().ty_cooldown()));
            fields.add_field_method_set("ty_cooldown", |_lua, api, value: i64| {
                api.entity_mut().set_ty_cooldown(value as u8);
                Ok(())
            });
            fields.add_field_method_get("ty_crowd", |_lua, api| Ok(api.entity().ty_crowd()));
            fields.add_field_method_set("ty_crowd", |_lua, api, value: i64| {
                api.entity_mut().set_ty_crowd(value as i8);
                Ok(())
            });
            fields.add_field_method_get("ty_close", |_lua, api| Ok(api.entity().ty_close()));
            fields.add_field_method_set("ty_close", |_lua, api, value: i64| {
                api.entity_mut().set_ty_close(value as u8);
                Ok(())
            });
            fields
                .add_field_method_get("ty_walk_dist", |_lua, api| Ok(api.entity().ty_walk_dist()));
            fields.add_field_method_set("ty_walk_dist", |_lua, api, value: i64| {
                api.entity_mut().set_ty_walk_dist(value as u16);
                Ok(())
            });

            // The Tyrant's render scratch: the heart handle, the ghost scale
            // pair and the ribbon timer/segment/arm words.
            fields.add_field_method_get("ty_heart", |_lua, api| {
                Ok(api.entity().tyrant_heart.map_or(-1, i64::from))
            });
            fields.add_field_method_get("ty_claw_scale_a", |_lua, api| {
                Ok(api.game_ref().tyrant.ghost.scale_a)
            });
            fields.add_field_method_set("ty_claw_scale_a", |_lua, api, value: i64| {
                api.game().tyrant.ghost.scale_a = value as i16;
                Ok(())
            });
            fields.add_field_method_get("ty_claw_scale_b", |_lua, api| {
                Ok(api.game_ref().tyrant.ghost.scale_b)
            });
            fields.add_field_method_set("ty_claw_scale_b", |_lua, api, value: i64| {
                api.game().tyrant.ghost.scale_b = value as i16;
                Ok(())
            });
            fields.add_field_method_get("ty_claw_scale_step", |_lua, api| {
                Ok(api.game_ref().tyrant.ghost.step)
            });
            fields.add_field_method_set("ty_claw_scale_step", |_lua, api, value: i64| {
                api.game().tyrant.ghost.step = value as i8;
                Ok(())
            });
            fields.add_field_method_get("ty_trail_timer", |_lua, api| {
                Ok(api.game_ref().tyrant.trail.timer)
            });
            fields.add_field_method_set("ty_trail_timer", |_lua, api, value: i64| {
                api.game().tyrant.trail.timer = value as u16;
                Ok(())
            });
            fields.add_field_method_get("ty_trail_segments", |_lua, api| {
                Ok(api.game_ref().tyrant.trail.segments)
            });
            fields.add_field_method_set("ty_trail_segments", |_lua, api, value: i64| {
                api.game().tyrant.trail.segments = value as i32;
                Ok(())
            });

            // The entity slot the player's hit reaction latched, -1 for none.
            fields.add_field_method_get("player_attacker", |_lua, api| {
                Ok(api.game_ref().player_attacker.map_or(-1, i64::from))
            });
            fields.add_field_method_set("player_attacker", |_lua, api, value: i64| {
                api.game().player_attacker =
                    if (0..crate::game::ENTITY_COUNT as i64).contains(&value) {
                        Some(value as u8)
                    } else {
                        None
                    };
                Ok(())
            });

            // The shared capture matrix's t[0], doubling as the knock-back
            // facing the sweep stores and the thrown player reads (the setter
            // is `e:set_plant42_capture_t0`).
            fields.add_field_method_get("plant42_capture_t0", |_lua, api| {
                Ok(api.game_ref().plant42_capture.t[0] as i16)
            });

            // The companion's position and body fields the plant's SCD/effect
            // paths read (the flower body's height gates the ambient spray).
            fields.add_field_method_get("plant42_body_y", |_lua, api| {
                Ok(crate::enemy::companion::body_y(api.game_ref(), api.slot))
            });

            // The player's zone flags (`zoneFlags`): the hold raises the
            // grabbed bit `0x80` and the release clears it. The equipped
            // weapon id backs the recoil's heavy-weapon cancel.
            fields.add_field_method_get("player_zone_flags", |_lua, api| {
                Ok(api.game_ref().entities[0].zone_flags)
            });
            fields.add_field_method_set("player_zone_flags", |_lua, api, value: i64| {
                api.game().entities[0].zone_flags = value as u8;
                Ok(())
            });
            fields.add_field_method_get("player_weapon", |_lua, api| {
                Ok(api.game_ref().equipped.unwrap_or(0))
            });

            // `MSF_VOICE_PLAYING`: the arms raise it after they queue their
            // login voice and wait for the engine to clear it.
            fields.add_field_method_get("voice_playing", |_lua, api| {
                Ok(api.game_ref().voice_playing())
            });
            fields.add_field_method_set("voice_playing", |_lua, api, value: bool| {
                if value {
                    api.game().set_voice_playing();
                } else {
                    api.game().clear_voice_playing();
                }
                Ok(())
            });
        }

        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            // Advance the entity's animation clock one tick, exactly like the
            // original's `Joint_move` with the given blend step.
            methods.add_method_mut(
                "advance_anim",
                |_lua, api, (step, reverse): (i64, Option<bool>)| {
                    let clips = api.clips;
                    let slot = api.slot;
                    let game = api.game();
                    // SAFETY: the clips pointer borrows the caller's model for
                    // the whole call (see the type-level comment).
                    let clips = unsafe { &*clips };
                    let completed = game.entity_anims[slot].advance(
                        &mut game.entities[slot],
                        clips,
                        reverse.unwrap_or(false),
                        step as u16,
                    );
                    Ok(completed)
                },
            );

            // The shared `is_facing_toward_entity` test: whether the player's
            // own yaw points at this entity (the hover behaviour's grab pick).
            methods.add_method("player_facing_entity", |_lua, api, ()| {
                let player_angle = api.game_ref().entities[0].angle;
                Ok(crate::enemy::walk::is_facing_toward_entity(
                    api.entity().angle,
                    player_angle,
                ))
            });

            // `GetPlayerInputMasked`: any d-pad direction or face button held,
            // the crow grab's struggle-shake gate.
            methods.add_method("player_mashing", |_lua, api, ()| {
                Ok(api.game_ref().player_mashing())
            });

            // `entity_apply_anim_vertex`: set the entity's X/Z from the
            // current clip frame's root vertex plus the `unk_c6`/`unk_c8`
            // grab offsets, rotated by the entity's yaw.
            methods.add_method_mut("apply_anim_vertex", |_lua, api, ()| {
                let clips = api.clips;
                let slot = api.slot;
                let game = api.game();
                // SAFETY: the clips pointer borrows the caller's model for
                // the whole call (see the type-level comment).
                let clips = unsafe { &*clips };
                game.entity_anims[slot].apply_anim_vertex(&mut game.entities[slot], clips);
                Ok(())
            });

            // The reset-joints equivalent: clear the scripted hidden-joint
            // mask and restart the clip clock, keeping the shared keyframe
            // table and skeleton the driver installed for this frame.
            methods.add_method_mut("reset_joints", |_lua, api, ()| {
                let slot = api.slot;
                let game = api.game();
                let keyframes = game.entity_anims[slot].keyframes.take();
                let skeleton = game.entity_anims[slot].skeleton.take();
                game.entity_anims[slot] = EntityAnim::default();
                game.entity_anims[slot].keyframes = keyframes;
                game.entity_anims[slot].skeleton = skeleton;
                game.entities[slot].joint_flags = 0;
                game.entities[slot].joint_armed = 0;
                game.entities[slot].joint_flags_hi = [0; 32];
                game.entities[slot].joint_blood = [crate::game::JointBlood::default(); 32];
                Ok(())
            });

            methods.add_method_mut(
                "set_joint_visible",
                |_lua, api, (joint, visible): (i64, bool)| {
                    let joint = joint as u8;
                    if joint < 32 {
                        let mask = &mut api.entity_mut().joint_flags;
                        if visible {
                            *mask &= !(1u32 << joint);
                        } else {
                            *mask |= 1u32 << joint;
                        }
                    }
                    Ok(())
                },
            );

            // `joint_setup_attack_effect` (the minimal port): arm one joint's
            // attack effect. The original clears the joint's active bit and
            // sets `0x28` on the flag byte, which hides the joint, makes
            // `leg_reach` treat the leg as inactive, and is the gore contract
            // the zombie death paths test; the async tint/trail stays
            // deferred with the joint-object work. The effect type, timer and
            // frame match are recorded on no field the port reads yet.
            methods.add_method_mut(
                "arm_joint_effect",
                |_lua, api, (joint, _kind, _timer, _frame): (i64, i64, i64, i64)| {
                    let joint = joint as usize;
                    if joint < 32 {
                        let entity = api.entity_mut();
                        if entity.joint_flag(joint) & 1 != 0 {
                            let value = (entity.joint_flag(joint) & !1) | 0x28;
                            entity.set_joint_flag(joint, value);
                        }
                        entity.joint_armed |= 1u32 << joint;
                    }
                    Ok(())
                },
            );

            // The ground-shadow quad's local offset; the extents and tint are
            // the `shadow_*` properties.
            methods.add_method_mut(
                "set_shadow_offset",
                |_lua, api, (x, y, z): (i64, i64, i64)| {
                    api.entity_mut().shadow_offset = [x as i16, y as i16, z as i16];
                    Ok(())
                },
            );

            // The enemy's SCA collision volume record (local offset, half
            // height and radius), read by the shared collision pass. Radius
            // and half height are also the `sca_radius`/`sca_half_height`
            // properties; this sets the whole record at once.
            methods.add_method_mut(
                "set_sca",
                |_lua,
                 api,
                 (radius, half_height, offset_x, offset_y, offset_z): (i64, i64, i64, i64, i64)| {
                    let entity = api.entity_mut();
                    entity.sca_radius = radius as i16;
                    entity.sca_half_height = half_height as i16;
                    entity.sca_offset = [offset_x as i16, offset_y as i16, offset_z as i16];
                    // A single-volume record terminates on the first entry, so
                    // a second profile volume is dropped.
                    entity.sca2 = None;
                    Ok(())
                },
            );

            // The second SCA profile volume (the Black Tiger's two-box walk
            // record). `check_room_collision` still reads the first volume's
            // radius, exactly like the original.
            methods.add_method_mut(
                "set_sca2",
                |_lua,
                 api,
                 (radius, half_height, offset_x, offset_y, offset_z): (i64, i64, i64, i64, i64)| {
                    api.entity_mut().sca2 = Some(crate::game::ScaVolume {
                        radius: radius as i16,
                        half_height: half_height as i16,
                        offset: [offset_x as i16, offset_y as i16, offset_z as i16],
                    });
                    Ok(())
                },
            );

            // The per-frame SCA hit retarget the bosses' extended limbs use:
            // the primary volume's world centre becomes the posed joint's
            // offset from the body. The original writes the head-minus-body
            // vector into the entity's SCA hit data every gated frame; zero
            // restores the record distance.
            methods.add_method_mut(
                "set_sca_hit_point",
                |_lua, api, (dx, dy, dz): (i64, i64, i64)| {
                    api.entity_mut().sca_hit_delta = [dx as i16, dy as i16, dz as i16];
                    Ok(())
                },
            );

            // Recompute the camera-switch-zone bit the shadow gate reads, and
            // return it.
            methods.add_method_mut("update_switch_zone", |_lua, api, ()| {
                let room = api.room;
                let slot = api.slot;
                let game = api.game();
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                let zone = u8::from(in_camera_zone(
                    room,
                    room.current_cut,
                    game.entities[slot].pos,
                ));
                game.entities[slot].has_enter_switch_zone = zone;
                Ok(zone)
            });
            // Raise the spawn record's death-event bit in the enemy flag bank,
            // exactly like the original's `Flg_on(g_EnemiesFlags, id)`.
            methods.add_method_mut("raise_death_event", |_lua, api, ()| {
                let bit = api.entity().death_event_id;
                if bit != 0xFF {
                    api.game().flags[usize::from(BANK_ENEMIES)].apply(bit, 0);
                }
                Ok(())
            });

            // The shared player-hurt helper: subtract `damage` with the
            // original's 16-bit wrap and optional clamp/lethal rules
            // (flags 0x01 clamp to 1, 0x02 resolve a clamped kill to -1).
            methods.add_method_mut(
                "hurt_player",
                |_lua, api, (damage, flags): (i64, Option<i64>)| {
                    Ok(api
                        .game()
                        .apply_player_hurt(damage as i16, flags.unwrap_or(0) as u16))
                },
            );

            // Arm the poison status bit and the 150-frame poison timer.
            methods.add_method_mut("poison_player", |_lua, api, ()| {
                api.game().poison_player();
                Ok(())
            });

            // The wasp grab pose: pin both entities' animation offsets, set
            // the attacked flag and play the grabbed animation.
            methods.add_method_mut("grab_player", |_lua, api, ()| {
                let slot = api.slot;
                api.game().grab_player(slot);
                Ok(())
            });

            // The player animation overrides the attacks write: the original's
            // dword store at +0x84 writes animationId and animFrameId, the
            // entity's state and ignore bytes.
            methods.add_method_mut(
                "set_player_animation",
                |_lua, api, (id, frame): (i64, i64)| {
                    let player = &mut api.game().entities[0];
                    player.set_state(id as u8);
                    player.set_ignore(frame as u8);
                    Ok(())
                },
            );
            methods.add_method_mut(
                "set_player_action",
                |_lua, api, (behavior, state): (i64, i64)| {
                    let player = &mut api.game().entities[0];
                    player.action_behavior = behavior as u8;
                    player.action_state = state as u8;
                    Ok(())
                },
            );

            // Scenario/state flag queries (`Flg_ck`).
            methods.add_method("flag_bit", |_lua, api, (bank, sel): (i64, i64)| {
                Ok(api
                    .game_ref()
                    .flags
                    .get(bank as usize)
                    .is_some_and(|flags| flags.bit(sel as u8)))
            });

            // Mark the current position as the collision-accepted rollback
            // point (the original's accepted `position` word).
            methods.add_method_mut("save_pos", |_lua, api, ()| {
                let pos = api.entity().pos;
                api.entity_mut().saved_pos = Some(pos);
                Ok(())
            });

            // The pre-checked walk: commit the step and roll it back when the
            // room collision refuses the destination.
            methods.add_method_mut("try_move", |_lua, api, (offset, distance): (i64, i64)| {
                let room = api.room;
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                let entity = api.entity_mut();
                Ok(crate::enemy::walk::try_advance_xz(
                    room,
                    entity,
                    offset as u16,
                    distance as i16,
                ))
            });

            // Raise the SCD completion flag `scd_anim_param` in the system
            // bank, the way a finished scripted-action handler signals the
            // event script waiting on it.
            methods.add_method_mut("raise_scd_flag", |_lua, api, ()| {
                let param = api.entity().scd_anim_param;
                api.game().flags[usize::from(BANK_SYSTEM)].apply(param, 0);
                Ok(())
            });

            // `Flg_on` on any bank/selector: the plant's combat-progression
            // flag and the bosses' other handshake bits. Raising a bit is
            // mode 0, clearing mode 1.
            methods.add_method_mut("raise_flag", |_lua, api, (bank, sel): (i64, i64)| {
                api.game().apply_flag(bank as u8, sel as u8, 0);
                Ok(())
            });

            // Count one dispatch of an out-of-table `action_behavior` in the
            // placeholder map (`npc_placeholders`), the record the original's
            // NULL table slot leaves behind.
            methods.add_method_mut("count_placeholder", |_lua, api, behavior: i64| {
                *api.game()
                    .npc_placeholders
                    .entry(behavior as u8)
                    .or_insert(0) += 1;
                Ok(())
            });

            // Write one header byte of a spawned effect slot (the original's
            // `fire_fx_tag`, which tags the shell/secondary flash with the
            // weapon id).
            methods.add_method_mut(
                "tag_effect",
                |_lua, api, (slot, field, value): (i64, i64, i64)| {
                    if let Some(effect) = api.game().effects.slot_mut(slot as usize)
                        && let Some(byte) = effect.header.get_mut(field as usize)
                    {
                        *byte = value as u8;
                    }
                    Ok(())
                },
            );

            // The shared walk helpers the scripted and follow drivers steer
            // with; all take the scripted target's X/Z plane and use the
            // entity's live angle/speed.

            // Write `move_speed_current` and trim it on the clip's footfall
            // frames.
            methods.add_method_mut("apply_walk_speed", |_lua, api, speed: i64| {
                crate::enemy::walk::entity_apply_walk_speed(api.entity_mut(), speed as i16);
                Ok(())
            });

            // The turn delta toward the target (0 when aligned within twice
            // the step), for the caller to add to the yaw.
            methods.add_method(
                "turn_toward_target",
                |_lua, api, (x, z, step): (i64, i64, i64)| {
                    Ok(crate::enemy::walk::turn_toward_target(
                        api.entity(),
                        [x as i32, 0, z as i32],
                        step as i16,
                    ))
                },
            );

            // Rotate one step toward the target, snapping when within twice
            // the step (the original's `entity_rotate_toward_target`).
            methods.add_method_mut(
                "rotate_toward_target",
                |_lua, api, (x, z, step): (i64, i64, i64)| {
                    crate::enemy::walk::rotate_toward_target(
                        api.entity_mut(),
                        [x as i32, 0, z as i32],
                        step as u16,
                    );
                    Ok(())
                },
            );

            // The floored XZ distance from the entity to a point.
            methods.add_method("xz_distance_to", |_lua, api, (x, z): (i64, i64)| {
                Ok(crate::enemy::walk::xz_distance_to(
                    api.entity(),
                    [x as i32, 0, z as i32],
                ))
            });

            // Queue the footstep sound for the entity's current animation
            // frame (the shared walk-footstep resolution).
            methods.add_method_mut("footstep", |_lua, api, sound_type: i64| {
                let slow = api.game_ref().flags[5].bit(crate::game::MSF2_EFFECT_ZONE);
                let room = api.room;
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                let slot = api.slot;
                let game = api.game();
                crate::enemy::walk::footstep(
                    &mut game.entity_sounds,
                    room,
                    &game.entities[slot],
                    sound_type as u8,
                    slow,
                );
                Ok(())
            });

            // The player's live position and collision radius, the state-9
            // driver's steering target.
            methods.add_method("player_pos", |_lua, api, ()| {
                let pos = api.game_ref().entities[0].pos;
                Ok((pos[0], pos[1], pos[2]))
            });
            methods.add_method("player_radius", |_lua, api, ()| {
                Ok(crate::enemy::walk::player_radius(
                    api.game_ref().id.player_flag,
                ))
            });

            // The 12-bit XZ angle from the entity to a point.
            methods.add_method("angle_to", |_lua, api, (x, z): (i64, i64)| {
                let entity = api.entity();
                Ok(crate::sfx::angle_between_xz(
                    entity.pos[0],
                    entity.pos[2],
                    x as i32,
                    z as i32,
                ))
            });

            // The shared 4.12 yaw rotation (`RotMatrixY` + `ApplyMatrixSV`):
            // the zombie's wander waypoint rotates a 5000-unit forward vector
            // by its yaw plus eight.
            methods.add_method("rotate_xz", |_lua, _api, (angle, x, z): (i64, i64, i64)| {
                let (rx, rz) = crate::player::rotate_xz(angle as u16, x as i32, z as i32);
                Ok((rx, rz))
            });

            // The walk layer's blend step for the entity's current
            // `blend_counter` (0x1000 / (counter + 1)).
            methods.add_method("blend_step", |_lua, api, ()| {
                Ok(crate::enemy::anim::blend_step(api.entity()))
            });

            // The un-collided `Add_speedXZ` step (the follow driver resolves
            // room collision in its own tail). The rotated vector is stored on
            // the entity, exactly like the original's `speed` SVECTOR.
            methods.add_method_mut("move", |_lua, api, (offset, distance): (i64, i64)| {
                crate::enemy::walk::advance_xz(api.entity_mut(), offset as u16, distance as i16);
                Ok(())
            });

            // Re-add the stored `Add_speedXZ` vector without recomputing the
            // yaw (the web threads' fixed launch direction).
            methods.add_method_mut("advance_speed", |_lua, api, ()| {
                crate::enemy::walk::advance_speed(api.entity_mut());
                Ok(())
            });

            // The spiders' shared player-distance scratch (`ws_dist`): the
            // flat Manhattan sum of the two axis differences.
            methods.add_method("web_distance", |_lua, api, ()| {
                Ok(crate::enemy::walk::spider_distance(
                    api.entity(),
                    api.game_ref().entities[0].pos,
                ))
            });

            // `leg_reach`: the stride length of leg pair `part`, stored in
            // `move_speed_current`. The WebSpinner and Black Tiger chain the
            // leg joints differently; the entity id picks the shape.
            methods.add_method_mut("leg_reach", |_lua, api, (part, scale): (i64, i64)| {
                let clips = api.clips;
                let slot = api.slot;
                let game = api.game();
                // SAFETY: the clips pointer borrows the caller's model for the
                // whole call (see the type-level comment).
                let clips = unsafe { &*clips };
                let chain = if game.entities[slot].id == 0x04 {
                    crate::enemy::walk::LegChain::Tiger
                } else {
                    crate::enemy::walk::LegChain::Spinner
                };
                crate::enemy::walk::leg_reach(
                    &mut game.entities[slot],
                    &game.entity_anims[slot],
                    clips,
                    &game.joint_worlds[slot],
                    chain,
                    part as u8,
                    scale as u16,
                );
                Ok(())
            });

            // The per-type web-joint registry: report whether `chosen` is
            // already carrying a web thread, and if not store it at `index & 7`
            // (the original's `ws_web_joints_used` scan and write). The two
            // registries are shared by every spider of that id and reset with
            // the room.
            methods.add_method_mut("web_joint_use", |_lua, api, (chosen, index): (i64, i64)| {
                let registry = match api.entity().id {
                    0x03 => 0,
                    0x04 => 1,
                    _ => return Ok(false),
                };
                let chosen = chosen as u8;
                if api.game_ref().web_joint_registry[registry].contains(&chosen) {
                    return Ok(false);
                }
                api.game().web_joint_registry[registry][(index as usize) & 7] = chosen;
                Ok(true)
            });

            // Spawn `count` web-thread clones from this spider (the shooters'
            // `ws_clone_entity`). Returns the chain head slot.
            methods.add_method_mut("web_spawn", |_lua, api, count: i64| {
                let slot = api.slot;
                Ok(crate::enemy::web::spawn(api.game(), slot, count as u8))
            });

            // Tick this spider's web-thread chain (the shooters'
            // `ws_update_webs`).
            methods.add_method_mut("web_update", |_lua, api, count: i64| {
                let room = api.room;
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                let slot = api.slot;
                crate::enemy::web::update(api.game(), room, slot, count as u8);
                Ok(())
            });

            // `entity_swerve_around_obstacle`: steer around room geometry,
            // returning the yaw delta and updating the swerve/latch scratch.
            // The target is the player, the crow's only call site.
            methods.add_method_mut(
                "swerve",
                |_lua, api, (step, blocked, seed): (i64, bool, i64)| {
                    let target = api.game_ref().entities[0].pos;
                    Ok(crate::enemy::walk::swerve_around_obstacle(
                        api.entity_mut(),
                        target,
                        step as i16,
                        blocked,
                        seed as u8,
                    ))
                },
            );

            // Record the walk zone containing the entity (0xFF when none) in
            // `splatter_flag` and return it.
            methods.add_method_mut("update_walk_zone", |_lua, api, ()| {
                let room = api.room;
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                let slot = api.slot;
                let game = api.game();
                let zone = crate::enemy::walk::walk_zone_find(
                    room,
                    game.entities[slot].pos[0],
                    game.entities[slot].pos[2],
                )
                .unwrap_or(0xFF);
                game.entities[slot].splatter_flag = zone;
                Ok(zone)
            });

            // One obstacle-pathfinder tick against a target point; returns 0
            // (blocked), 1 (waypoint refreshed) or 2 (counting).
            methods.add_method_mut("pathfind_update", |_lua, api, (x, z): (i64, i64)| {
                let room = api.room;
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                let slot = api.slot;
                let game = api.game();
                Ok(crate::enemy::walk::entity_pathfind_update(
                    room,
                    &mut game.entities[slot],
                    [x as i32, 0, z as i32],
                ))
            });

            // The walk-zone graph primitives the follow driver steers with.
            methods.add_method("zone_shared_edge", |_lua, api, (a, b): (i64, i64)| {
                let room = api.room;
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                let (flag, point) =
                    crate::enemy::walk::walk_zone_shared_edge(room, a as u8, b as u8);
                Ok((flag, point[0], point[1]))
            });

            // The zone path from the entity's position to a point. Returns
            // (kind, from, next, crossing_x, crossing_z) with kind 0 direct,
            // 1 cross, 2 unreachable.
            methods.add_method("zone_path_find", |_lua, api, (x, z): (i64, i64)| {
                let room = api.room;
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                let entity = api.entity();
                match crate::enemy::walk::zone_path_find(room, entity.pos, [x as i32, 0, z as i32])
                {
                    crate::enemy::walk::ZonePath::Direct { zone } => Ok((0, zone, 0, 0, 0)),
                    crate::enemy::walk::ZonePath::Cross {
                        from,
                        next,
                        crossing,
                    } => Ok((1, from, next, crossing[0], crossing[1])),
                    crate::enemy::walk::ZonePath::Unreachable => Ok((2, 0, 0, 0, 0)),
                }
            });

            // The zombie distance driver's `zone_path_find` out-parameter
            // form: steer the entity's waypoint toward the player and write
            // the crossing (or the target) back into `player_pos_x`/`z`.
            methods.add_method_mut("zone_path_update", |_lua, api, (x, z): (i64, i64)| {
                let room = api.room;
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                let slot = api.slot;
                let game = api.game();
                crate::enemy::walk::zone_path_update(
                    room,
                    &mut game.entities[slot],
                    x as i32,
                    z as i32,
                );
                Ok(())
            });

            // Whether the shared-edge span between two zones admits the point
            // for a mover of the entity's radius.
            methods.add_method(
                "corridor_open",
                |_lua, api, (flag, x, z, from, next): (i64, i64, i64, i64, i64)| {
                    let room = api.room;
                    // SAFETY: the room pointer borrows the caller's room for
                    // the whole call (see the type-level comment).
                    let room = unsafe { &*room };
                    let radius = i32::from(api.entity().sca_radius);
                    Ok(crate::enemy::walk::corridor_open(
                        room, flag as u8, x as i32, z as i32, from as u8, next as u8, radius,
                    ))
                },
            );

            // Whether a point is inside the room's blocking geometry for the
            // entity's radius and collision flags.
            methods.add_method(
                "position_blocked",
                |_lua, api, (x, y, z): (i64, i64, i64)| {
                    let room = api.room;
                    // SAFETY: the room pointer borrows the caller's room for
                    // the whole call (see the type-level comment).
                    let room = unsafe { &*room };
                    let entity = api.entity();
                    Ok(crate::player::position_blocked(
                        room,
                        [x as i32, y as i32, z as i32],
                        i32::from(entity.sca_radius),
                        entity.collision_flags,
                    ))
                },
            );

            // Clamp the entity into the corridor of its current -> next shared
            // edge; returns the waypoint (x, y, z) and the heading.
            methods.add_method("crossing_heading", |_lua, api, (from, next): (i64, i64)| {
                let room = api.room;
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                let entity = api.entity();
                let (waypoint, heading) = crate::enemy::walk::crossing_heading(
                    room,
                    entity.pos,
                    Some(from as u8),
                    next as u8,
                    i32::from(entity.sca_radius),
                );
                Ok((waypoint[0], waypoint[1], waypoint[2], heading))
            });

            // Whether a heading lies within `half` of the entity's yaw.
            methods.add_method(
                "heading_within",
                |_lua, api, (heading, half): (i64, i64)| {
                    Ok(crate::enemy::walk::heading_within(
                        api.entity().angle,
                        heading as u16,
                        half as i32,
                    ))
                },
            );

            // Step the yaw toward a heading (odd ids turn eight units faster).
            methods.add_method_mut(
                "turn_toward_heading",
                |_lua, api, (heading, step): (i64, i64)| {
                    crate::enemy::walk::turn_toward_heading(
                        api.entity_mut(),
                        heading as i16,
                        step as i16,
                    );
                    Ok(())
                },
            );

            // The look-at state machine: look at nothing / at a point / wander
            // from the frame's random seed.
            methods.add_method_mut("lookat_reset", |_lua, api, ()| {
                let seed = api.game_ref().rand_seed;
                crate::enemy::walk::reset_lookat(api.entity_mut(), seed);
                Ok(())
            });
            methods.add_method_mut("lookat_target", |_lua, api, (x, z): (i64, i64)| {
                let seed = api.game_ref().rand_seed;
                crate::enemy::walk::set_lookat_target(api.entity_mut(), x as i32, z as i32, seed);
                Ok(())
            });
            methods.add_method_mut("lookat_wander", |_lua, api, param: i64| {
                let seed = api.game_ref().rand_seed;
                crate::enemy::walk::wander_lookat(api.entity_mut(), param as u8, seed);
                Ok(())
            });

            // The SCA separation pass: the player pair then every other
            // entity with a status bit, in slot order. Returns the player
            // pair's hit flag, the value the attacking monsters park as their
            // touch word.
            methods.add_method_mut("separate", |_lua, api, ()| {
                let slot = api.slot;
                Ok(crate::enemy::walk::separate_all(api.game(), slot))
            });

            // The next draw from the platform stream (`rand()`), for the
            // monsters that consume the CRT stream directly instead of the
            // per-frame seed.
            methods.add_method_mut("random", |_lua, api, ()| {
                Ok(crate::game::platform_rand(&mut api.game().rand_state))
            });

            // `srand(seed)`: reseed the platform stream before a scripted
            // burst of draws.
            methods.add_method_mut("srand", |_lua, api, seed: i64| {
                api.game().srand(seed as u32);
                Ok(())
            });

            // The frame snapshot's Fibonacci-LFSR step (the shark's swim
            // pick): shifts the stored seed and returns its low byte.
            methods.add_method_mut("lfsr_step", |_lua, api, ()| Ok(api.game().lfsr_step()));

            // `Snd_em(id)`: queue the entity's group-offset enemy-bank cue at
            // the entity position. The sound-group nibble comes from the
            // spawn record's variant byte; an id at or above ten, or a record
            // past the 48-entry bank, queues nothing.
            methods.add_method_mut("play_enemy_sound", |_lua, api, id: i64| {
                let room = api.room;
                let slot = api.slot;
                let game = api.game();
                let group = (game.entities[slot].variant >> 4) & 0x7;
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                if let Some((name, column)) = crate::sfx::enemy_sound(room, id as u8, group) {
                    let pos = game.entities[slot].pos;
                    game.entity_sounds.push(EntitySound {
                        name,
                        bank: 2,
                        column,
                        pos,
                    });
                }
                Ok(())
            });

            // `Play3DSnd(bank, id)`: queue the named cue at the entity
            // position. Banks 0/2/3 resolve through the room and character
            // tables; the unloaded weapon bank (1) and the BGM pan channel
            // (4) queue nothing.
            methods.add_method_mut("play_3d_sound", |_lua, api, (bank, id): (i64, i64)| {
                let room = api.room;
                let slot = api.slot;
                let character = api.game_ref().id.player_flag;
                // SAFETY: the room pointer borrows the caller's room for
                // the whole call (see the type-level comment).
                let room = unsafe { &*room };
                if let Some((name, bank, column)) =
                    crate::sfx::play_3d_cue(room, character, bank as u8, id as u8)
                {
                    let game = api.game();
                    let pos = game.entities[slot].pos;
                    game.entity_sounds.push(EntitySound {
                        name,
                        bank,
                        column,
                        pos,
                    });
                }
                Ok(())
            });

            // `play_sfx(bank, id)`: the raw one-shot cue, identical to
            // `Play3DSnd` for the banks it names (the weapon bank ships
            // unloaded, so bank 1 queues nothing).
            methods.add_method_mut("play_sfx", |_lua, api, (bank, id): (i64, i64)| {
                let room = api.room;
                let slot = api.slot;
                let character = api.game_ref().id.player_flag;
                // SAFETY: the room pointer borrows the caller's room for
                // the whole call (see the type-level comment).
                let room = unsafe { &*room };
                if let Some((name, bank, column)) =
                    crate::sfx::play_3d_cue(room, character, bank as u8, id as u8)
                {
                    let game = api.game();
                    let pos = game.entities[slot].pos;
                    game.entity_sounds.push(EntitySound {
                        name,
                        bank,
                        column,
                        pos,
                    });
                }
                Ok(())
            });

            // The lab terminal's voice line: resolve the id in the stage's
            // voice table and queue the request for the engine, counting a
            // miss for an empty record so a package without the line degrades
            // instead of deadlocking. The caller owns the playing bit.
            methods.add_method_mut("play_voice", |_lua, api, id: i64| {
                let stage = api.game_ref().id.stage.saturating_sub(1);
                match crate::voice::name(stage, id as u16) {
                    Some(name) => {
                        let volume = if stage == 0 && id == 0x33 { -300 } else { 0 };
                        let game = api.game();
                        game.voice.request = Some(crate::game::VoiceRequest {
                            name,
                            volume,
                            pan: 0,
                        });
                    }
                    None => api.game().voice.misses += 1,
                }
                Ok(())
            });

            // `BillboardAdjSize` on the entity's ground quad: the half
            // extents move by the signed deltas with 16-bit wrap.
            methods.add_method_mut(
                "adjust_shadow_size",
                |_lua, api, (half_x, half_z): (i64, i64)| {
                    let entity = api.entity_mut();
                    entity.shadow_half_x = entity.shadow_half_x.wrapping_add(half_x as i16);
                    entity.shadow_half_z = entity.shadow_half_z.wrapping_add(half_z as i16);
                    Ok(())
                },
            );

            // The shared additive model tint (`scd_model_tint_apply`'s enemy
            // path): accumulate the queue record on this entity and add the
            // raw deltas to every enemy with a matching id. The absolute
            // tint ids (8/0x0F/0x12) are not modelled until a monster uses
            // them.
            methods.add_method_mut(
                "tint_model",
                |_lua, api, (r, g, b, word_a, word_b): (i64, i64, i64, i64, i64)| {
                    let slot = api.slot;
                    let id = api.entity().id;
                    let game = api.game();
                    game.entities[slot].queue_model_tint(
                        r as i16,
                        g as i16,
                        b as i16,
                        word_a as u16,
                        word_b as u16,
                    );
                    game.tint_enemies_by_id(id, r as i16, g as i16, b as i16);
                    Ok(())
                },
            );

            // `FUN_00473d10`: retarget the tint queue record without
            // touching the live model.
            methods.add_method_mut(
                "retarget_tint",
                |_lua, api, (r, g, b, word_a, word_b): (i64, i64, i64, i64, i64)| {
                    api.entity_mut().retarget_model_tint(
                        r as i16,
                        g as i16,
                        b as i16,
                        word_a as u16,
                        word_b as u16,
                    );
                    Ok(())
                },
            );

            // The room-collision resolve + accept: push the entity out of the
            // walls, or roll X/Z back to the stored position, then store the
            // accepted result. Returns the original's result code (0 clear,
            // 1 pushed, 2 rolled back, 3 a floor/step zone crossed) for the
            // monsters' blocked-frame count together with the crossed floor
            // step (the original's `g_animFrameIdSave` read-back).
            methods.add_method_mut("resolve_collision", |_lua, api, ()| {
                let room = api.room;
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                let slot = api.slot;
                let game = api.game();
                let before = game.entities[slot]
                    .saved_pos
                    .unwrap_or(game.entities[slot].pos);
                let code = crate::enemy::walk::check_room_collision(room, &mut game.entities[slot]);
                // The original's `g_tempVar`: the distance actually travelled,
                // measured between the resolved position and the previous
                // accepted one, both through their 16-bit `position` words.
                let after = game.entities[slot].pos;
                let dx = after[0].wrapping_sub(i32::from(before[0] as i16));
                let dz = after[2].wrapping_sub(i32::from(before[2] as i16));
                let sum = dx.wrapping_mul(dx).wrapping_add(dz.wrapping_mul(dz));
                let distance = if sum <= 0 {
                    0
                } else {
                    (f64::from(sum)).sqrt() as u32
                };
                Ok((code, game.entities[slot].floor_step, distance))
            });

            // The path-result keep: reduce `entity_pathfind_update`'s returned
            // bitfield to bit 0 of the scratch byte at +0x16C, only when no
            // higher bit was set.
            methods.add_method_mut("pathfind_keep", |_lua, api, path: i64| {
                crate::enemy::walk::pathfind_track(api.entity_mut(), path as u8);
                Ok(())
            });

            // The wasp's path-result keep: the same reduction, into the
            // scratch byte at +0x16E the hover and victory behaviours gate on.
            methods.add_method_mut("wasp_path_keep", |_lua, api, path: i64| {
                crate::enemy::walk::wasp_pathfind_track(api.entity_mut(), path as u8);
                Ok(())
            });

            // The flat distance to the player with the visual-range status bit
            // write (`entity_check_visual_range`); the alert variant raises bit
            // 7 instead.
            methods.add_method_mut("check_visual_range", |_lua, api, range: i64| {
                let player = api.game_ref().entities[0].pos;
                Ok(crate::enemy::walk::entity_check_visual_range(
                    api.entity_mut(),
                    player,
                    range as u32,
                ))
            });
            methods.add_method_mut("check_alert_range", |_lua, api, range: i64| {
                let player = api.game_ref().entities[0].pos;
                Ok(crate::enemy::walk::entity_check_alert_range(
                    api.entity_mut(),
                    player,
                    range as u32,
                ))
            });

            // The shared special-weapon check: a high player weapon clears the
            // entity's hit latch on five-frame animation boundaries.
            methods.add_method_mut("check_special_weapon", |_lua, api, ()| {
                let slot = api.slot;
                api.game().zombie_check_special_weapon(slot);
                Ok(())
            });

            // The zombie's chase gates: the angular wedge + range test and
            // the boundary sight-ray test, both anchored on the entity and
            // aimed at the player.
            methods.add_method_mut(
                "angular_view_and_distance",
                |_lua, api, (fov_half, max_distance): (i64, i64)| {
                    let player = api.game_ref().entities[0].pos;
                    Ok(crate::enemy::walk::angular_view_and_distance(
                        api.entity(),
                        fov_half as i16,
                        max_distance as i16,
                        player,
                    ))
                },
            );
            methods.add_method_mut("line_of_sight", |_lua, api, ()| {
                let room = api.room;
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                let player = api.game_ref().entities[0].pos;
                let result = crate::enemy::walk::check_line_of_sight(room, api.entity(), player);
                // The original stores the result byte in the shared
                // `player_distance_z` scratch before returning it; the hunter
                // reads the same word back after the call.
                api.game().player_distance_z = i32::from(result);
                Ok(result)
            });

            // The hound's spawn nudge: step `distance` along the full rotation
            // vector and add the low X/Z words onto the accepted `position`
            // words the spawn record stored.
            methods.add_method_mut("nudge_spawn", |_lua, api, distance: i64| {
                let slot = api.slot;
                crate::enemy::walk::nudge_spawn(api.game(), slot, distance as i16);
                Ok(())
            });

            // The hound's forward collision probe: step `distance` along the
            // entity's own rotation, resolve the room at `radius`, and step
            // back. The resolve is destructive (it may push the position and
            // advances the accepted `position` words), matching the original;
            // `probe_turn` restores everything around it.
            methods.add_method_mut(
                "probe_ahead",
                |_lua, api, (distance, radius): (i64, i64)| {
                    let room = api.room;
                    // SAFETY: the room pointer borrows the caller's room for
                    // the whole call (see the type-level comment).
                    let room = unsafe { &*room };
                    let slot = api.slot;
                    Ok(crate::enemy::walk::probe_ahead(
                        room,
                        api.game(),
                        slot,
                        distance as i16,
                        radius as i16,
                    ))
                },
            );

            // The hound's turn probe: scale the live speed by `mul`, steer
            // `delta` off the current yaw with a real `Add_speedXZ`, probe
            // ahead, and restore the position words and speed.
            methods.add_method_mut("probe_turn", |_lua, api, (delta, mul): (i64, i64)| {
                let room = api.room;
                // SAFETY: the room pointer borrows the caller's room for
                // the whole call (see the type-level comment).
                let room = unsafe { &*room };
                let slot = api.slot;
                Ok(crate::enemy::walk::probe_turn(
                    room,
                    api.game(),
                    slot,
                    delta as i16,
                    mul as i16,
                ))
            });

            // One frame of the shared projectile arc: walk `fwd` along the
            // entity's yaw and subtract `vy0 + air_ticks * gravity` from Y,
            // with the original's 16-bit wrap. Returns the landing velocity
            // (zero while airborne).
            methods.add_method_mut(
                "ballistic",
                |_lua, api, (fwd, vy0, gravity, ground): (i64, i64, i64, i64)| {
                    Ok(crate::enemy::walk::entity_ballistic_step(
                        api.entity_mut(),
                        fwd as i16,
                        vy0 as i16,
                        gravity as i16,
                        ground as i32,
                    ))
                },
            );

            // The hound's head tracking: integrate the accumulated head yaw
            // toward the player, clamp it to `+/-0x100` and store it back in
            // the swerve word. Returns whether the clamp bit.
            methods.add_method_mut("head_track", |_lua, api, ()| {
                let clips = api.clips;
                let slot = api.slot;
                let game = api.game();
                // SAFETY: the clips pointer borrows the caller's model for
                // the whole call (see the type-level comment).
                let clips = unsafe { &*clips };
                Ok(crate::enemy::walk::head_track(game, slot, clips))
            });

            // `entity_update_wander_turn`: steer at the stored waypoint,
            // updating the direction-control flags and the +0x179 counter. The
            // high half of `dir_control_flags`' 0x80 beat keeps the entity
            // turning for the frames the helper seeds from the frame random.
            methods.add_method_mut(
                "wander_turn",
                |_lua, api, (movement_dist, angle_step, turn_limit): (i64, i64, i64)| {
                    let seed = api.game_ref().rand_seed;
                    Ok(crate::enemy::walk::update_wander_turn(
                        api.entity_mut(),
                        movement_dist as u32,
                        angle_step as u16,
                        turn_limit as u8,
                        seed,
                    ))
                },
            );

            // The hunter's call of the same steering: the control byte is
            // `hunter_room_hit` and the turn counter the low byte of
            // `hunter_path_latch`.
            methods.add_method_mut(
                "hunter_wander_turn",
                |_lua, api, (movement_dist, angle_step, turn_limit): (i64, i64, i64)| {
                    let seed = api.game_ref().rand_seed;
                    Ok(crate::enemy::walk::hunter_wander_turn(
                        api.entity_mut(),
                        movement_dist as u32,
                        angle_step as u16,
                        turn_limit as u8,
                        seed,
                    ))
                },
            );

            // The hunter's partner pick: the enemy-list head, or the next
            // slot when this hunter already is the head.
            methods.add_method_mut("pick_partner", |_lua, api, ()| {
                let slot = api.slot;
                let partner = if slot == 1 { 2 } else { 1 };
                api.entity_mut().hunter_partner = partner as u8;
                Ok(partner)
            });

            // The partner's live fields, read through the picked slot.
            methods.add_method("partner_health", |_lua, api, ()| {
                let partner = api.entity().hunter_partner;
                Ok(api
                    .game_ref()
                    .entities
                    .get(usize::from(partner))
                    .map_or(0, |entity| entity.health))
            });
            methods.add_method("partner_hit_state", |_lua, api, ()| {
                let partner = api.entity().hunter_partner;
                Ok(api
                    .game_ref()
                    .entities
                    .get(usize::from(partner))
                    .map_or(0, |entity| entity.hit_state))
            });
            methods.add_method("partner_move_speed", |_lua, api, ()| {
                let partner = api.entity().hunter_partner;
                Ok(api
                    .game_ref()
                    .entities
                    .get(usize::from(partner))
                    .map_or(0, |entity| entity.move_speed_current))
            });
            methods.add_method("partner_status", |_lua, api, ()| {
                let partner = api.entity().hunter_partner;
                Ok(api
                    .game_ref()
                    .entities
                    .get(usize::from(partner))
                    .map_or(0, |entity| entity.status_flags))
            });
            methods.add_method("partner_pos", |_lua, api, ()| {
                let partner = api.entity().hunter_partner;
                Ok(api
                    .game_ref()
                    .entities
                    .get(usize::from(partner))
                    .map_or((0, 0), |entity| (entity.pos[0], entity.pos[2])))
            });

            // Cross-slot joint flags and state words: the scripted bite
            // checks and latches the SCD target's head joint and rewrites its
            // state word.
            methods.add_method(
                "entity_joint_flag",
                |_lua, api, (slot, joint): (i64, i64)| {
                    Ok(api
                        .game_ref()
                        .entities
                        .get(slot as usize)
                        .map_or(0, |entity| entity.joint_flag(joint as usize)))
                },
            );
            methods.add_method_mut(
                "set_entity_joint_flag",
                |_lua, api, (slot, joint, value): (i64, i64, i64)| {
                    if let Some(entity) = api.game().entities.get_mut(slot as usize) {
                        entity.set_joint_flag(joint as usize, value as u8);
                    }
                    Ok(())
                },
            );
            methods.add_method("entity_state_word", |_lua, api, slot: i64| {
                Ok(api
                    .game_ref()
                    .entities
                    .get(slot as usize)
                    .map_or(0, |entity| entity.state_word()))
            });
            methods.add_method_mut(
                "set_entity_state_word",
                |_lua, api, (slot, value): (i64, i64)| {
                    if let Some(entity) = api.game().entities.get_mut(slot as usize) {
                        entity.set_state_word(value as u32);
                    }
                    Ok(())
                },
            );
            methods.add_method("entity_status", |_lua, api, slot: i64| {
                Ok(api
                    .game_ref()
                    .entities
                    .get(slot as usize)
                    .map_or(0, |entity| entity.status_flags))
            });
            methods.add_method_mut(
                "set_entity_status",
                |_lua, api, (slot, value): (i64, i64)| {
                    if let Some(entity) = api.game().entities.get_mut(slot as usize) {
                        entity.status_flags = value as u8;
                    }
                    Ok(())
                },
            );
            methods.add_method("entity_health", |_lua, api, slot: i64| {
                Ok(api
                    .game_ref()
                    .entities
                    .get(slot as usize)
                    .map_or(0, |entity| entity.health))
            });
            methods.add_method_mut(
                "set_entity_health",
                |_lua, api, (slot, value): (i64, i64)| {
                    if let Some(entity) = api.game().entities.get_mut(slot as usize) {
                        entity.health = value as i16;
                    }
                    Ok(())
                },
            );
            methods.add_method("entity_pos", |_lua, api, slot: i64| {
                Ok(api
                    .game_ref()
                    .entities
                    .get(slot as usize)
                    .map_or((0, 0, 0), |entity| {
                        (entity.pos[0], entity.pos[1], entity.pos[2])
                    }))
            });
            methods.add_method_mut(
                "set_entity_angle",
                |_lua, api, (slot, value): (i64, i64)| {
                    if let Some(entity) = api.game().entities.get_mut(slot as usize) {
                        entity.angle = value as u16;
                    }
                    Ok(())
                },
            );
            methods.add_method("entity_hit_state", |_lua, api, slot: i64| {
                Ok(api
                    .game_ref()
                    .entities
                    .get(slot as usize)
                    .map_or(0, |entity| entity.hit_state))
            });
            methods.add_method_mut(
                "set_entity_hit_state",
                |_lua, api, (slot, value): (i64, i64)| {
                    if let Some(entity) = api.game().entities.get_mut(slot as usize) {
                        entity.hit_state = value as u8;
                    }
                    Ok(())
                },
            );
            // The victim's animation grab offsets (`+0xC6`/`+0xC8`), the pair
            // the SCD impale drags with the Tyrant's own root motion.
            methods.add_method_mut(
                "add_entity_anim_offset",
                |_lua, api, (slot, dx, dz): (i64, i64, i64)| {
                    if let Some(entity) = api.game().entities.get_mut(slot as usize) {
                        entity.unk_c6 = entity.unk_c6.wrapping_add(dx as u16);
                        entity.unk_c8 = entity.unk_c8.wrapping_add(dz as u16);
                    }
                    Ok(())
                },
            );
            // The tracked joint-1 world translation a track helper last wrote
            // into the slot.
            methods.add_method("entity_joint_track", |_lua, api, slot: i64| {
                Ok(api
                    .game_ref()
                    .entities
                    .get(slot as usize)
                    .map_or((0, 0, 0), |entity| {
                        (
                            entity.joint_track[0],
                            entity.joint_track[1],
                            entity.joint_track[2],
                        )
                    }))
            });

            // The hunter's track/recenter render helpers.
            methods.add_method_mut("track_player_joint", |_lua, api, sel: i64| {
                let clips = api.clips;
                let slot = api.slot;
                let game = api.game();
                // SAFETY: the clips pointer borrows the caller's model for
                // the whole call (see the type-level comment).
                let clips = unsafe { &*clips };
                Ok(
                    crate::enemy::walk::hunter_track_joint(game, clips, slot, 0, sel as u8, false)
                        .is_some(),
                )
            });
            methods.add_method_mut("track_target_joint", |_lua, api, sel: i64| {
                let clips = api.clips;
                let slot = api.slot;
                let game = api.game();
                // SAFETY: the clips pointer borrows the caller's model for
                // the whole call (see the type-level comment).
                let clips = unsafe { &*clips };
                // The SCD target is always the enemy-list base, the port's
                // entity slot 1.
                Ok(
                    crate::enemy::walk::hunter_track_joint(game, clips, slot, 1, sel as u8, true)
                        .is_some(),
                )
            });
            methods.add_method_mut("recenter_on_joint", |_lua, api, which: i64| {
                let clips = api.clips;
                let slot = api.slot;
                let game = api.game();
                // SAFETY: the clips pointer borrows the caller's model for
                // the whole call (see the type-level comment).
                let clips = unsafe { &*clips };
                crate::enemy::walk::hunter_recenter_on_joint(game, clips, slot, which as u8);
                Ok(())
            });

            // The pounce grab's one-shot flag (`DAT_004bd2b0`).
            methods.add_method_mut("raise_grab_one_shot", |_lua, api, ()| {
                api.game().hunter_grab_one_shot = true;
                Ok(())
            });

            // `PLAYER_T_INT[0] += (short)ENTITY->speed.x` (and Z): the swipe
            // recovery drags the grabbed player by the stored velocity.
            methods.add_method_mut("drag_player", |_lua, api, ()| {
                let speed = api.entity().speed;
                let player = &mut api.game().entities[0];
                player.pos[0] = player.pos[0].wrapping_add(i32::from(speed[0]));
                player.pos[2] = player.pos[2].wrapping_add(i32::from(speed[2]));
                Ok(())
            });

            // `check_room_collision_two_point`: the prone-body floor probe.
            // The two endpoints are body-local (x, z) pairs; returns end B's
            // flag bits, 0x80 after a blocked rollback.
            methods.add_method_mut(
                "two_point_probe",
                |_lua, api, (ax, az, bx, bz): (i64, i64, i64, i64)| {
                    let room = api.room;
                    // SAFETY: the room pointer borrows the caller's room for
                    // the whole call (see the type-level comment).
                    let room = unsafe { &*room };
                    Ok(crate::enemy::walk::two_point_probe(
                        &room.collision,
                        api.entity_mut(),
                        [ax as i16, az as i16],
                        [bx as i16, bz as i16],
                    ))
                },
            );

            // `blood_splatter_physics` on the named joint (the zombie update's
            // head-joint bleed).
            methods.add_method_mut(
                "blood_splatter",
                |_lua, api, (joint, gravity): (i64, i64)| {
                    let room = api.room;
                    let slot = api.slot;
                    let game = api.game();
                    // SAFETY: the room pointer borrows the caller's room for
                    // the whole call (see the type-level comment).
                    let room = unsafe { &*room };
                    crate::enemy::walk::blood_splatter(
                        game,
                        room,
                        slot,
                        joint as usize,
                        gravity as i16,
                    );
                    Ok(())
                },
            );

            // The blood scratch the vomit attack arms: the joint's velocity X
            // and Y words, the gravity counter and the bounce/frame flags (the
            // original's writes at joint +2/+3/+4/+6); the direction's Z word
            // is left as the pose set it.
            methods.add_method_mut(
                "set_joint_blood",
                |_lua, api, (joint, vel_x, vel_y, counter, flags): (i64, i64, i64, i64, i64)| {
                    let joint = joint as usize & 31;
                    let rot_z = api.entity().joint_blood[joint].rot_z;
                    api.entity_mut().joint_blood[joint] = crate::game::JointBlood {
                        counter: counter as u8,
                        flags: flags as u8,
                        vel_x: vel_x as i16,
                        vel_y: vel_y as i16,
                        rot_z,
                    };
                    Ok(())
                },
            );

            // `zombie_body_part_physics`: derive `move_speed_current` from the
            // posed leg joint chain.
            methods.add_method_mut("body_part_speed", |_lua, api, param: i64| {
                let clips = api.clips;
                let slot = api.slot;
                let game = api.game();
                // SAFETY: the clips pointer borrows the caller's model for the
                // whole call (see the type-level comment).
                let clips = unsafe { &*clips };
                crate::enemy::walk::body_part_speed(game, clips, slot, param as u8);
                Ok(())
            });

            // `reduce_attack_time_by_btn_press`: how many frames the held
            // controls shorten the bite this frame.
            methods.add_method("mash_reduce", |_lua, api, ()| {
                Ok(api.game_ref().mash_reduce())
            });

            // One joint's flag byte: bit 0 active/hidden, bits 1-7 the gore
            // state (`joint_setup_attack_effect`, severed limbs, blown head).
            methods.add_method("joint_flag", |_lua, api, joint: i64| {
                Ok(api.entity().joint_flag(joint as usize))
            });
            methods.add_method_mut("set_joint_flag", |_lua, api, (joint, value): (i64, i64)| {
                api.entity_mut().set_joint_flag(joint as usize, value as u8);
                Ok(())
            });

            // `snap_player_to_grab_position`: pin the grabbing entity's
            // `unk_c6`/`unk_c8` offsets and the player's copies to the root
            // motion vertex's world position, the bite anchor.
            methods.add_method_mut("snap_grab", |_lua, api, ()| {
                let clips = api.clips;
                let slot = api.slot;
                let game = api.game();
                // SAFETY: the clips pointer borrows the caller's model for the
                // whole call (see the type-level comment).
                let clips = unsafe { &*clips };
                let Some((x, z)) = game.entity_anims[slot].root_vertex(&game.entities[slot], clips)
                else {
                    return Ok(());
                };
                let pos = game.entities[slot].pos;
                let c6 = pos[0].wrapping_sub(x) as i16 as u16;
                let c8 = pos[2].wrapping_sub(z) as i16 as u16;
                game.entities[slot].unk_c6 = c6;
                game.entities[slot].unk_c8 = c8;
                game.entities[0].unk_c6 = c6;
                game.entities[0].unk_c8 = c8;
                Ok(())
            });

            // A billboard anchored to one of the entity's posed joints with a
            // local offset (the zombie attack sprays stage their sprite
            // offsets through the shared dead-move block).
            methods.add_method_mut(
                "spawn_joint_effect_at",
                |_lua,
                 api,
                 (effect_type, depth, joint, x, y, z, yaw): (i64, i64, i64, i64, i64, i64, i64)| {
                    let slot = api.slot;
                    let game = api.game();
                    let room_effects = Rc::clone(&game.room_effects);
                    Ok(crate::effects::create_attached(
                        game,
                        &room_effects,
                        effect_type as u8,
                        depth as u8,
                        Attach::Joint(slot as u8, joint as u8),
                        [x as i32, y as i32, z as i32],
                        yaw as i16,
                        0,
                    ))
                },
            );

            // A billboard at a world point with the identity transform, the
            // original's `g_deadMoveValue` sprite-info spawns.
            methods.add_method_mut(
                "spawn_world_effect",
                |_lua, api, (effect_type, depth, x, y, z, yaw): (i64, i64, i64, i64, i64, i64)| {
                    let game = api.game();
                    let room_effects = Rc::clone(&game.room_effects);
                    Ok(crate::effects::create_attached(
                        game,
                        &room_effects,
                        effect_type as u8,
                        depth as u8,
                        Attach::Identity,
                        [x as i32, y as i32, z as i32],
                        yaw as i16,
                        0,
                    ))
                },
            );

            // The soft body-tint writer (`JointApplyColorTint`): the port has
            // no per-joint colour pipeline, so the call is recorded as the
            // joint's gore state only. The state contract stays observable
            // through `joint_flag`.
            methods.add_method_mut(
                "tint_joint",
                |_lua, _api, (_joint, _a, _b, _rgb): (i64, i64, i64, i64)| Ok(()),
            );

            // The posed skeleton's joint world translation (the original's
            // `joints[i].world.t`): the matrix the previous update's render
            // pass computed.
            methods.add_method("joint_world_x", |_lua, api, joint: i64| {
                Ok(api.game_ref().joint_worlds[api.slot]
                    .get(joint as usize)
                    .map_or(0, |matrix| matrix.t[0]))
            });
            methods.add_method("joint_world_y", |_lua, api, joint: i64| {
                Ok(api.game_ref().joint_worlds[api.slot]
                    .get(joint as usize)
                    .map_or(0, |matrix| matrix.t[1]))
            });
            methods.add_method("joint_world_z", |_lua, api, joint: i64| {
                Ok(api.game_ref().joint_worlds[api.slot]
                    .get(joint as usize)
                    .map_or(0, |matrix| matrix.t[2]))
            });

            // `FUN_0048ae00`: the square joint-reach test. `offset` is the
            // local probe position composed onto the joint matrix and `radius`
            // the box half-extent; height never participates.
            methods.add_method(
                "reach_test",
                |_lua, api, (joint, x, y, z, radius): (i64, i64, i64, i64, i64)| {
                    let worlds = &api.game_ref().joint_worlds[api.slot];
                    let Some(matrix) = worlds.get(joint as usize) else {
                        return Ok(false);
                    };
                    let player = api.game_ref().entities[0].pos;
                    Ok(crate::enemy::walk::joint_reach_test(
                        matrix,
                        [x as i32, y as i32, z as i32],
                        radius as i16,
                        player,
                    ))
                },
            );

            // A billboard anchored to one of the entity's posed joints (the
            // retreat's blood sprays): the joint's world matrix is the effect's
            // attach frame and its local offset is zero.
            methods.add_method_mut(
                "spawn_joint_effect",
                |_lua, api, (effect_type, depth, joint, yaw): (i64, i64, i64, i64)| {
                    let slot = api.slot;
                    let game = api.game();
                    let room_effects = Rc::clone(&game.room_effects);
                    Ok(crate::effects::create_attached(
                        game,
                        &room_effects,
                        effect_type as u8,
                        depth as u8,
                        Attach::Joint(slot as u8, joint as u8),
                        [0, 0, 0],
                        yaw as i16,
                        0,
                    ))
                },
            );

            // A billboard anchored to the player's matrix with a local offset
            // (the adder's bite spray).
            methods.add_method_mut(
                "spawn_player_effect",
                |_lua, api, (effect_type, depth, x, y, z, yaw): (i64, i64, i64, i64, i64, i64)| {
                    let game = api.game();
                    let room_effects = Rc::clone(&game.room_effects);
                    Ok(crate::effects::create_attached(
                        game,
                        &room_effects,
                        effect_type as u8,
                        depth as u8,
                        Attach::Player,
                        [x as i32, y as i32, z as i32],
                        yaw as i16,
                        0,
                    ))
                },
            );

            // `Play3DSnd(bank, id)` at an explicit position: the attacking
            // monsters play their cue at the player's position, not their own.
            methods.add_method_mut(
                "play_3d_sound_at",
                |_lua, api, (bank, id, x, y, z): (i64, i64, i64, i64, i64)| {
                    let room = api.room;
                    let character = api.game_ref().id.player_flag;
                    // SAFETY: the room pointer borrows the caller's room for
                    // the whole call (see the type-level comment).
                    let room = unsafe { &*room };
                    if let Some((name, bank, column)) =
                        crate::sfx::play_3d_cue(room, character, bank as u8, id as u8)
                    {
                        api.game().entity_sounds.push(EntitySound {
                            name,
                            bank,
                            column,
                            pos: [x as i32, y as i32, z as i32],
                        });
                    }
                    Ok(())
                },
            );

            // The yaw of another entity slot, `nil` when the slot does not
            // exist (the bleeding-out idle copies the second enemy-list slot).
            methods.add_method("entity_angle", |_lua, api, slot: i64| {
                Ok(api
                    .game_ref()
                    .entities
                    .get(slot as usize)
                    .map(|entity| entity.angle))
            });

            // Spawn a billboard attached to this entity's matrix, the idle
            // handlers' blood sprays. Returns the first-frame pool slot.
            methods.add_method_mut(
                "spawn_effect",
                |_lua,
                 api,
                 (effect_type, depth, x, y, z, yaw, light): (i64, i64, i64, i64, i64, i64, i64)| {
                    let slot = api.slot;
                    let game = api.game();
                    let room_effects = Rc::clone(&game.room_effects);
                    Ok(crate::effects::create_attached(
                        game,
                        &room_effects,
                        effect_type as u8,
                        depth as u8,
                        Attach::Entity(slot as u8),
                        [x as i32, y as i32, z as i32],
                        yaw as i16,
                        light as u8,
                    ))
                },
            );

            // Refresh the live look-at target and slew the tracking joint,
            // the character driver's shared tail.
            methods.add_method_mut("update_look_at", |_lua, api, ()| {
                let slot = api.slot;
                let game = api.game();
                if let Some(target) = live_look_at_target(game, slot) {
                    game.entities[slot].target = target;
                }
                let target = game.entities[slot];
                game.entity_anims[slot].slew_look_at(&target);
                Ok(())
            });

            // Plant 42's own skeleton clock: the sub-frame blend animator the
            // plant uses instead of the shared `Joint_move`.
            methods.add_method_mut(
                "plant42_advance",
                |_lua, api, (reverse, blend): (bool, i64)| {
                    let clips = api.clips;
                    let slot = api.slot;
                    let game = api.game();
                    // SAFETY: the clips pointer borrows the caller's model for
                    // the whole call (see the type-level comment).
                    let clips = unsafe { &*clips };
                    Ok(crate::enemy::custom_anim::plant42_advance(
                        &mut game.entity_anims[slot],
                        &mut game.entities[slot],
                        clips,
                        reverse,
                        blend as u16,
                    ))
                },
            );

            // Build the plant's two companions (the flower body and the root
            // ball) and store their handles on the entity.
            methods.add_method_mut("plant42_spawn", |_lua, api, ()| {
                let slot = api.slot;
                let flags = api.entity().behavior_flags;
                let spawned = crate::enemy::companion::plant42_spawn(api.game(), slot, flags);
                Ok(spawned.map_or((None, None), |(body, roots)| (Some(body), Some(roots))))
            });

            // Tick both companion machines (body then roots) and mark them
            // live for the render pass.
            methods.add_method_mut("plant42_tick", |_lua, api, ()| {
                let room = api.room;
                let slot = api.slot;
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                crate::enemy::companion::plant42_tick(api.game(), room, slot);
                Ok(())
            });

            // The resolved companion handles (the original's `scd_target_ptr`
            // body and the roots pointer).
            methods.add_method("plant42_body", |_lua, api, ()| {
                Ok(crate::enemy::companion::resolve_body(
                    api.game_ref(),
                    api.slot,
                ))
            });
            methods.add_method("plant42_roots", |_lua, api, ()| {
                Ok(crate::enemy::companion::resolve_roots(
                    api.game_ref(),
                    api.slot,
                ))
            });
            methods.add_method("plant42_body_hit", |_lua, api, ()| {
                Ok(crate::enemy::companion::body_hit_state(
                    api.game_ref(),
                    api.slot,
                ))
            });
            methods.add_method("plant42_body_vines", |_lua, api, ()| {
                Ok(crate::enemy::companion::body_vines(
                    api.game_ref(),
                    api.slot,
                ))
            });
            methods.add_method("plant42_body_health", |_lua, api, ()| {
                Ok(crate::enemy::companion::body_health(
                    api.game_ref(),
                    api.slot,
                ))
            });
            methods.add_method_mut("plant42_body_react", |_lua, api, ()| {
                let slot = api.slot;
                crate::enemy::companion::body_react(api.game(), slot);
                Ok(())
            });
            methods.add_method_mut("plant42_body_react_clear", |_lua, api, ()| {
                let slot = api.slot;
                crate::enemy::companion::body_react_clear(api.game(), slot);
                Ok(())
            });
            methods.add_method_mut("plant42_body_wither", |_lua, api, sub: i64| {
                let slot = api.slot;
                crate::enemy::companion::body_wither(api.game(), slot, sub as u8);
                Ok(())
            });
            methods.add_method_mut("plant42_body_kill", |_lua, api, ()| {
                let slot = api.slot;
                crate::enemy::companion::body_kill(api.game(), slot);
                Ok(())
            });

            // The last-vine payoff: raise the scenario flag, hand Jill her
            // message bit and wither every enemy-list slot up to the sixth
            // (the port's slots 1..=6).
            methods.add_method_mut("plant42_award_kill", |_lua, api, ()| {
                let game = api.game();
                game.flags[usize::from(BANK_SCENARIO)]
                    .apply(crate::game::SCENARIO_FLAG_PLANT42_DEAD, 0);
                if game.id.player_flag & 1 != 0 {
                    game.message_flags |= 0x100;
                }
                for slot in (1..=6).rev() {
                    let entity = &mut game.entities[slot];
                    if entity.health as u16 & 0x8000 == 0 {
                        entity.set_state(1);
                        entity.set_ignore(1);
                        entity.action_behavior = 9;
                        entity.action_state = 0;
                        entity.hit_state = 1;
                    }
                }
                Ok(())
            });

            // The capture matrix: build it from the grabbing joint and the
            // player's current transform.
            methods.add_method_mut("plant42_capture_setup", |_lua, api, joint: i64| {
                let slot = api.slot;
                let game = api.game();
                let Some(joint_matrix) = game.joint_worlds[slot].get(joint as usize).copied()
                else {
                    return Ok(());
                };
                let player = game.entities[0];
                let player_matrix = crate::anim::entity_matrix_rotated(
                    player.pos,
                    player.pitch,
                    player.angle,
                    player.roll,
                );
                game.plant42_capture =
                    crate::enemy::custom_anim::capture_setup(&joint_matrix, &player_matrix);
                Ok(())
            });

            // Apply the capture transform through a joint to the player: the
            // grabbed pose's position follows the joint.
            methods.add_method_mut("plant42_hold_player", |_lua, api, joint: i64| {
                let slot = api.slot;
                let game = api.game();
                let Some(joint_matrix) = game.joint_worlds[slot].get(joint as usize).copied()
                else {
                    return Ok(());
                };
                let capture = game.plant42_capture;
                let held = crate::enemy::custom_anim::hold_player(&joint_matrix, &capture);
                game.entities[0].pos = held.t;
                game.entities[0].zone_flags |= 0x80;
                Ok(())
            });

            // The Chris hold orientation: derive the capture rotation from a
            // fixed 3x3 block, preserving the stored translation.
            methods.add_method_mut(
                "plant42_capture_orient",
                |_lua, api, m: mlua::Variadic<i64>| {
                    let values: Vec<i16> = m.iter().map(|value| *value as i16).collect();
                    if values.len() < 9 {
                        return Ok(());
                    }
                    let mut block = [0i16; 9];
                    block.copy_from_slice(&values[..9]);
                    let orientation = crate::enemy::custom_anim::orient_from_matrix(block);
                    api.game().plant42_capture.r = orientation.r;
                    Ok(())
                },
            );

            // The shared capture matrix's t[0], doubling as the knock-back
            // facing the sweep stores and the thrown player reads.
            methods.add_method_mut("set_plant42_capture_t0", |_lua, api, value: i64| {
                api.game().plant42_capture.t[0] = i32::from(value as i16);
                Ok(())
            });

            // The player's position and the grab speed vector the plant's
            // hold behaviours drive directly.
            methods.add_method_mut("set_player_pos", |_lua, api, (x, y, z): (i64, i64, i64)| {
                api.game().entities[0].pos = [x as i32, y as i32, z as i32];
                Ok(())
            });
            methods.add_method_mut(
                "set_player_speed",
                |_lua, api, (x, y, z): (i64, i64, i64)| {
                    api.game().player_speed = [x as i16, y as i16, z as i16];
                    Ok(())
                },
            );
            methods.add_method_mut("add_player_speed", |_lua, api, ()| {
                let speed = api.game_ref().player_speed;
                let player = &mut api.game().entities[0];
                player.pos[0] = player.pos[0].wrapping_add(i32::from(speed[0]));
                player.pos[1] = player.pos[1].wrapping_add(i32::from(speed[1]));
                player.pos[2] = player.pos[2].wrapping_add(i32::from(speed[2]));
                Ok(())
            });

            // Store the whole capture translation (the hold writes the lift
            // offset over the setup's relative translation).
            methods.add_method_mut(
                "set_plant42_capture_t",
                |_lua, api, (x, y, z): (i64, i64, i64)| {
                    api.game().plant42_capture.t = [x as i32, y as i32, z as i32];
                    Ok(())
                },
            );

            // The body's vine pool write (the plant's real health pool).
            methods.add_method_mut("set_plant42_body_vines", |_lua, api, value: i64| {
                let slot = api.slot;
                crate::enemy::companion::set_body_vines(api.game(), slot, value as i16);
                Ok(())
            });

            // A companion's world position by handle.
            methods.add_method("companion_pos", |_lua, api, handle: i64| {
                Ok(
                    crate::enemy::companion::companion_pos(api.game_ref(), handle as u16)
                        .map_or((0, 0, 0), |pos| (pos[0], pos[1], pos[2])),
                )
            });

            // A billboard attached to a companion's matrix with a local
            // offset, the SCD fx timer's body sprays.
            methods.add_method_mut(
                "spawn_companion_effect",
                |_lua,
                 api,
                 (handle, effect_type, depth, x, y, z, yaw): (
                    i64,
                    i64,
                    i64,
                    i64,
                    i64,
                    i64,
                    i64,
                )| {
                    let game = api.game();
                    let room_effects = Rc::clone(&game.room_effects);
                    Ok(crate::effects::create_attached(
                        game,
                        &room_effects,
                        effect_type as u8,
                        depth as u8,
                        Attach::Companion(handle as u16),
                        [x as i32, y as i32, z as i32],
                        yaw as i16,
                        0,
                    ))
                },
            );

            // `ApplyMatrixLV`: rotate a vector by a joint's world matrix
            // (translation ignored), the grab lift offset.
            methods.add_method(
                "apply_matrix_lv",
                |_lua, api, (joint, x, y, z): (i64, i64, i64, i64)| {
                    let Some(matrix) = api.game_ref().joint_worlds[api.slot]
                        .get(joint as usize)
                        .copied()
                    else {
                        return Ok((0, 0, 0));
                    };
                    let vector = [x as i32, y as i32, z as i32];
                    let mut out = [0i32; 3];
                    for (row, value) in matrix.r.iter().zip(out.iter_mut()) {
                        let mut sum = 0i64;
                        for (element, input) in row.iter().zip(&vector) {
                            sum += i64::from(*element) * i64::from(*input);
                        }
                        *value = ((sum + ((sum >> 63) & 0xFFF)) >> 12) as i32;
                    }
                    Ok((out[0], out[1], out[2]))
                },
            );

            // -----------------------------------------------------------------
            // Yawn's custom-skeleton surface and the segment-link slots.
            // -----------------------------------------------------------------

            // The init back-off: subtract the head joint's local X, rotated by
            // the entity's rotation triple, from the entity position.
            methods.add_method_mut("yawn_back_off", |_lua, api, ()| {
                let slot = api.slot;
                let game = api.game();
                let Some(relative_x) = game.entity_anims[slot]
                    .skeleton
                    .as_deref()
                    .and_then(|skeleton| skeleton.relative.first())
                    .map(|relative| relative[0])
                else {
                    return Ok(());
                };
                let entity = game.entities[slot];
                let matrix = crate::anim::entity_matrix_rotated(
                    entity.pos,
                    entity.pitch,
                    entity.angle,
                    entity.roll,
                );
                let rotated =
                    crate::enemy::custom_anim::apply_matrix_sv(&matrix, [relative_x, 0, 0]);
                let entity = &mut game.entities[slot];
                entity.pos[0] = entity.pos[0].wrapping_sub(i32::from(rotated[0]));
                entity.pos[2] = entity.pos[2].wrapping_sub(i32::from(rotated[2]));
                Ok(())
            });

            // `EntityComputeJointWorldMatrices` over the shared clock's pose:
            // the init's standard-world passes. `zero_root` models the second
            // pass, where the head joint's local X/Z have been cleared.
            methods.add_method_mut("yawn_standard_worlds", |_lua, api, zero_root: bool| {
                let slot = api.slot;
                let clips = unsafe { &*api.clips };
                let game = api.game();
                let entity = game.entities[slot];
                let clock = &game.entity_anims[slot];
                let (Some(keyframes), Some(skeleton)) =
                    (clock.keyframes.as_deref(), clock.skeleton.as_deref())
                else {
                    return Ok(());
                };
                let Some(pose) = clock.pose_keyframe(&entity, clips, keyframes) else {
                    return Ok(());
                };
                let mut transforms = crate::anim::joint_local_transforms(skeleton, &pose);
                if zero_root && let Some(root) = transforms.first_mut() {
                    root.t[0] = 0;
                    root.t[2] = 0;
                }
                let matrix = crate::anim::entity_matrix_rotated(
                    entity.pos,
                    entity.pitch,
                    entity.angle,
                    entity.roll,
                );
                game.joint_worlds[slot] = crate::enemy::custom_anim::compose_from_transforms(
                    skeleton,
                    &transforms,
                    &matrix,
                );
                Ok(())
            });

            // `yawn_pose_init`: lay down the fixed-point chain from the frame
            // the clock is showing.
            methods.add_method_mut("yawn_pose_init", |_lua, api, ()| {
                let slot = api.slot;
                let clips = unsafe { &*api.clips };
                let game = api.game();
                let entity = game.entities[slot];
                let clock = &game.entity_anims[slot];
                let (Some(keyframes), Some(skeleton)) =
                    (clock.keyframes.clone(), clock.skeleton.clone())
                else {
                    return Ok(());
                };
                let Some(clip) = clips.get(usize::from(entity.animation_id)) else {
                    return Ok(());
                };
                let Some(frame) = clip.frames.get(usize::from(entity.animation_frame_id)) else {
                    return Ok(());
                };
                let Some(keyframe) = keyframes.get(usize::from(frame.keyframe)).cloned() else {
                    return Ok(());
                };
                let pose = game.entity_anims[slot]
                    .yawn
                    .get_or_insert_with(Default::default);
                crate::enemy::custom_anim::yawn_pose_init(pose, &entity, &skeleton, &keyframe);
                if let Some(pose) = game.entity_anims[slot].yawn.as_deref() {
                    game.joint_worlds[slot] = pose.world.to_vec();
                }
                Ok(())
            });

            // `yawn_anim_advance`: one tick of the chain clock, returning the
            // wrap flag.
            methods.add_method_mut("yawn_anim", |_lua, api, (reverse, blend): (bool, i64)| {
                let slot = api.slot;
                let clips = unsafe { &*api.clips };
                let game = api.game();
                let completed = crate::enemy::custom_anim::yawn_advance(
                    &mut game.entity_anims[slot],
                    &mut game.entities[slot],
                    clips,
                    reverse,
                    blend as u16,
                );
                if let Some(pose) = game.entity_anims[slot].yawn.as_deref() {
                    game.joint_worlds[slot] = pose.world.to_vec();
                }
                Ok(completed)
            });

            // `yawn_post_move`: the head's collision and chain-follow tail.
            methods.add_method_mut("yawn_post_move", |_lua, api, step: i64| {
                let slot = api.slot;
                let room = unsafe { &*api.room };
                let game = api.game();
                crate::enemy::custom_anim::yawn_post_move(game, room, slot, step as i16);
                if let Some(pose) = game.entity_anims[slot].yawn.as_deref() {
                    game.joint_worlds[slot] = pose.world.to_vec();
                }
                Ok(())
            });

            // Build the twelve body-segment slots from the head's record.
            methods.add_method_mut("yawn_spawn_segments", |_lua, api, ()| {
                let slot = api.slot;
                Ok(crate::enemy::yawn::spawn_segments(api.game(), slot).len())
            });

            // One body segment's frame (the original's `behavior_flags == 1`
            // branch); refreshes the head's world snapshot after any drag.
            methods.add_method_mut("yawn_segment_tick", |_lua, api, ()| {
                let slot = api.slot;
                let room = unsafe { &*api.room };
                let game = api.game();
                crate::enemy::yawn::segment_tick(game, room, slot);
                if let Some(head) = game.entities[slot].yawn_head {
                    let head = usize::from(head);
                    if let Some(pose) = game.entity_anims[head].yawn.as_deref() {
                        game.joint_worlds[head] = pose.world.to_vec();
                    }
                }
                Ok(())
            });

            // The head's batched status write over the enemy-list range
            // `first..=last` (0 the head, 1..12 the segments).
            methods.add_method_mut(
                "yawn_set_status_range",
                |_lua, api, (first, last, and, or): (i64, i64, i64, i64)| {
                    let slot = api.slot;
                    crate::enemy::yawn::set_status_range(
                        api.game(),
                        slot,
                        first as usize,
                        last as usize,
                        and as u8,
                        or as u8,
                    );
                    Ok(())
                },
            );

            // The swallow capture matrix: built from the head joint 0's world
            // matrix and the player's transform.
            methods.add_method_mut("yawn_capture_setup", |_lua, api, joint: i64| {
                let slot = api.slot;
                let game = api.game();
                let Some(joint_matrix) = game.joint_worlds[slot].get(joint as usize).copied()
                else {
                    return Ok(());
                };
                let player = game.entities[0];
                let player_matrix = crate::anim::entity_matrix_rotated(
                    player.pos,
                    player.pitch,
                    player.angle,
                    player.roll,
                );
                game.yawn_capture =
                    crate::enemy::custom_anim::capture_setup(&joint_matrix, &player_matrix);
                Ok(())
            });

            // The capture matrix's rotation is multiplied by a yaw rotation in
            // place (`MulMatrix`), the swallow's shake.
            methods.add_method_mut("yawn_capture_rotate_y", |_lua, api, angle: i64| {
                let game = api.game();
                let rotation = crate::anim::Mat4x3 {
                    r: crate::anim::rotation_matrix(0, angle as i32, 0),
                    t: [0; 3],
                };
                game.yawn_capture.r = crate::anim::compose(&game.yawn_capture, &rotation).r;
                Ok(())
            });

            // The setup's left-multiply by a Z rotation
            // (`MulMatrixInPlace(g_matrixScratch, capture)`).
            methods.add_method_mut("yawn_capture_rotate_z", |_lua, api, angle: i64| {
                let game = api.game();
                let rotation = crate::anim::Mat4x3 {
                    r: crate::anim::rotation_matrix(0, 0, angle as i32),
                    t: [0; 3],
                };
                game.yawn_capture.r = crate::anim::compose(&rotation, &game.yawn_capture).r;
                Ok(())
            });

            // Step the capture translation (`g_yawnCaptureMatrix.t`), the
            // swallow's per-frame offsets.
            methods.add_method_mut(
                "yawn_capture_t_step",
                |_lua, api, (x, y, z): (i64, i64, i64)| {
                    let capture = &mut api.game().yawn_capture;
                    capture.t[0] = capture.t[0].wrapping_add(x as i32);
                    capture.t[1] = capture.t[1].wrapping_add(y as i32);
                    capture.t[2] = capture.t[2].wrapping_add(z as i32);
                    Ok(())
                },
            );

            // Scale a joint world's rotation columns (`ScaleMatrixCols`), the
            // swallow and death ramps.
            methods.add_method_mut(
                "yawn_scale_worlds",
                |_lua, api, (first, last, sx, sy, sz): (i64, i64, i64, i64, i64)| {
                    let slot = api.slot;
                    let game = api.game();
                    let scale = [sx as i32, sy as i32, sz as i32];
                    if let Some(pose) = game.entity_anims[slot].yawn.as_deref_mut() {
                        for joint in first.min(last)..=first.max(last) {
                            if let Some(world) = pose.world.get_mut(joint as usize) {
                                crate::enemy::custom_anim::scale_columns_xyz(world, scale);
                            }
                        }
                    }
                    if let Some(pose) = game.entity_anims[slot].yawn.as_deref() {
                        game.joint_worlds[slot] = pose.world.to_vec();
                    }
                    Ok(())
                },
            );

            // Scale a joint transform's rotation columns.
            methods.add_method_mut(
                "yawn_scale_transform",
                |_lua, api, (joint, sx, sy, sz): (i64, i64, i64, i64)| {
                    let slot = api.slot;
                    let game = api.game();
                    if let Some(pose) = game.entity_anims[slot].yawn.as_deref_mut()
                        && let Some(transform) = pose.transform.get_mut(joint as usize)
                    {
                        crate::enemy::custom_anim::scale_columns_xyz(
                            transform,
                            [sx as i32, sy as i32, sz as i32],
                        );
                    }
                    Ok(())
                },
            );

            // Recolour the ground-shadow quad of the head and every body
            // segment (the death dissolve's phase-8 pass).
            methods.add_method_mut("yawn_set_shadow_tint", |_lua, api, tint: i64| {
                let slot = api.slot;
                let game = api.game();
                game.entities[slot].shadow_tint = tint as u32;
                if let Some(segments) = crate::enemy::yawn::segment_slots(game, slot) {
                    for segment in segments {
                        game.entities[segment].shadow_tint = tint as u32;
                    }
                }
                Ok(())
            });

            // Store the capture translation (the setup's offsets and the
            // swallow's per-frame stepping).
            methods.add_method_mut(
                "set_yawn_capture_t",
                |_lua, api, (x, y, z): (i64, i64, i64)| {
                    api.game().yawn_capture.t = [x as i32, y as i32, z as i32];
                    Ok(())
                },
            );

            // Apply the capture transform through the head joint 0 to the
            // player, the swallowed hold.
            methods.add_method_mut("yawn_hold_player", |_lua, api, ()| {
                let slot = api.slot;
                let game = api.game();
                let Some(joint_matrix) = game.joint_worlds[slot].first().copied() else {
                    return Ok(());
                };
                let capture = game.yawn_capture;
                let held = crate::enemy::custom_anim::hold_player(&joint_matrix, &capture);
                game.entities[0].pos = held.t;
                game.entities[0].zone_flags |= 0x80;
                Ok(())
            });

            // The Tyrant's root-motion extractor: compose the entity yaw with
            // the selected clip set's claw chain, derive the frame's speed
            // from the leftover delta and (optionally) slide the entity.
            methods.add_method_mut(
                "tyrant_root_motion",
                |_lua, api, (set, apply): (i64, bool)| {
                    let clips = api.clips;
                    let slot = api.slot;
                    let game = api.game();
                    // SAFETY: the clips pointer borrows the caller's model for
                    // the whole call (see the type-level comment).
                    let clips = unsafe { &*clips };
                    Ok(crate::enemy::custom_anim::tyrant_root_motion(
                        game, slot, clips, set as u8, apply,
                    ))
                },
            );

            // Spawn the exposed-heart rider (the init clone). Returns its
            // companion handle, or -1 when the arena is full.
            methods.add_method_mut("tyrant_heart_spawn", |_lua, api, ()| {
                let slot = api.slot;
                Ok(crate::enemy::companion::spawn_heart(api.game(), slot).map_or(-1, i64::from))
            });

            // The heart's per-frame draw half: the gated beat/scale update and
            // the joint-1 anchor composition. Called before the switch-zone
            // refresh, exactly like the original.
            methods.add_method_mut("tyrant_heart_tick", |_lua, api, ()| {
                let slot = api.slot;
                crate::enemy::companion::heart_tick(api.game(), slot);
                Ok(())
            });

            // The rocket death's heart drop integrator.
            methods.add_method_mut("tyrant_heart_drop", |_lua, api, ()| {
                let slot = api.slot;
                crate::enemy::companion::heart_drop(api.game(), slot);
                Ok(())
            });

            // Re-arm the drop velocity (the rocket setup's `-0x12C` store).
            methods.add_method_mut("tyrant_heart_set_speed", |_lua, api, velocity: i64| {
                let slot = api.slot;
                crate::enemy::companion::heart_set_drop_speed(api.game(), slot, velocity as i16);
                Ok(())
            });

            // The heart's stored world height (`+0x6E`), 0 when it has none.
            methods.add_method("tyrant_heart_world_y", |_lua, api, ()| {
                Ok(crate::enemy::companion::heart_world_y(api.game_ref(), api.slot).unwrap_or(0))
            });

            // `tyrant_draw_claw_ghosts`' state half: compose the claw chain and
            // refresh the two ghost matrices and the oscillating scale pair.
            methods.add_method_mut("tyrant_ghosts_tick", |_lua, api, ()| {
                let clips = api.clips;
                let slot = api.slot;
                let game = api.game();
                // SAFETY: the clips pointer borrows the caller's model for the
                // whole call (see the type-level comment).
                let clips = unsafe { &*clips };
                let Some(scratch) = claw_chain(game, slot, clips) else {
                    return Ok(());
                };
                let gated = !game.message_freezes_monsters();
                crate::enemy::tyrant::ghost_tick(&mut game.tyrant.ghost, &scratch, gated);
                Ok(())
            });

            // Reserve the ribbon pool and store its tint (the init call).
            methods.add_method_mut("tyrant_trail_alloc", |_lua, api, ()| {
                crate::enemy::tyrant::trail_alloc(&mut api.game().tyrant, 0x70);
                Ok(())
            });

            // The `0x8000` arm sweep: seed the whole history from the current
            // claw pose.
            methods.add_method_mut("tyrant_trail_arm", |_lua, api, ()| {
                let slot = api.slot;
                let game = api.game();
                let claw = game.joint_worlds[slot]
                    .get(crate::enemy::tyrant::CLAW_OBJECT)
                    .copied()
                    .unwrap_or_default();
                crate::enemy::tyrant::trail_arm(&mut game.tyrant, &claw);
                Ok(())
            });

            // One frame of the ribbon: scroll the history, store the fresh
            // segment and count the tail down inside the message gate.
            methods.add_method_mut("tyrant_trail_update", |_lua, api, ()| {
                let clips = api.clips;
                let slot = api.slot;
                let game = api.game();
                // SAFETY: the clips pointer borrows the caller's model for the
                // whole call (see the type-level comment).
                let clips = unsafe { &*clips };
                let claw = game.joint_worlds[slot]
                    .get(crate::enemy::tyrant::CLAW_OBJECT)
                    .copied()
                    .unwrap_or_default();
                let Some(scratch) = claw_chain(game, slot, clips) else {
                    return Ok(());
                };
                let gated = !game.message_freezes_monsters();
                crate::enemy::tyrant::trail_update(&mut game.tyrant, &claw, &scratch, gated);
                Ok(())
            });

            // The Tyrant's call of the shared wander-turn steering: the
            // control byte is +0x16C and the counter +0x17E.
            methods.add_method_mut(
                "tyrant_wander_turn",
                |_lua, api, (movement_dist, angle_step, turn_limit): (i64, i64, i64)| {
                    let seed = api.game_ref().rand_seed;
                    Ok(crate::enemy::walk::tyrant_wander_turn(
                        api.entity_mut(),
                        movement_dist as u32,
                        angle_step as u16,
                        turn_limit as u8,
                        seed,
                    ))
                },
            );

            // Launch one severed limb: copy the joint's current world matrix
            // into the free-body slot and arm its physics.
            methods.add_method_mut(
                "tyrant_limb_launch",
                |_lua,
                 api,
                 (index, vx, vy, vz, tx, ty, tz): (i64, i64, i64, i64, i64, i64, i64)| {
                    let slot = api.slot;
                    let game = api.game();
                    let index = index as usize;
                    if index >= crate::enemy::tyrant::LIMB_COUNT {
                        return Ok(());
                    }
                    let world = game.joint_worlds[slot]
                        .get(crate::enemy::tyrant::LIMB_JOINTS[index])
                        .copied()
                        .unwrap_or_default();
                    crate::enemy::tyrant::limb_launch(
                        &mut game.tyrant.limbs[index],
                        world,
                        [vx as i16, vy as i16, vz as i16],
                        [tx as i16, ty as i16, tz as i16],
                        0x0F,
                        3,
                    );
                    Ok(())
                },
            );

            // One frame of a severed limb's free-body physics.
            methods.add_method_mut("tyrant_limb_update", |_lua, api, index: i64| {
                let index = index as usize;
                if index < crate::enemy::tyrant::LIMB_COUNT {
                    crate::enemy::tyrant::limb_update(&mut api.game().tyrant.limbs[index]);
                }
                Ok(())
            });

            // A severed limb's live world translation (the rocket death's
            // smoke anchors).
            methods.add_method("tyrant_limb_pos", |_lua, api, index: i64| {
                let index = index as usize;
                let limb = api
                    .game_ref()
                    .tyrant
                    .limbs
                    .get(index)
                    .copied()
                    .unwrap_or_default();
                Ok((limb.world.t[0], limb.world.t[1], limb.world.t[2]))
            });

            // `tyrant_latch_player_attacker`: point the player's attack latch
            // at this entity, store the facing (with the yaw bias the site
            // applies) and spin the entity's yaw only for the test.
            methods.add_method_mut("latch_player_attacker", |_lua, api, yaw_bias: i64| {
                let slot = api.slot;
                let game = api.game();
                let bias = yaw_bias as u16;
                game.player_attacker = Some(slot as u8);
                let saved = game.entities[slot].angle;
                game.entities[slot].angle = saved.wrapping_add(bias);
                let facing = crate::enemy::walk::is_facing_toward_entity(
                    game.entities[slot].angle,
                    game.entities[0].angle,
                );
                game.entities[slot].angle = saved;
                game.entities[0].attack_anim = u8::from(facing);
                Ok(())
            });

            // The bare attacker latch (the impale's `unk_b8` store, with no
            // facing write).
            methods.add_method_mut("set_player_attacker", |_lua, api, ()| {
                let slot = api.slot;
                api.game().player_attacker = Some(slot as u8);
                Ok(())
            });

            // `snap_player_to_grab_position` for the SCD victim: mirror the
            // caller's grab offsets onto the target slot.
            methods.add_method_mut("snap_grab_slot", |_lua, api, target: i64| {
                let clips = api.clips;
                let slot = api.slot;
                let game = api.game();
                // SAFETY: the clips pointer borrows the caller's model for the
                // whole call (see the type-level comment).
                let clips = unsafe { &*clips };
                let Some((x, z)) = game.entity_anims[slot].root_vertex(&game.entities[slot], clips)
                else {
                    return Ok(());
                };
                let pos = game.entities[slot].pos;
                let c6 = pos[0].wrapping_sub(x) as i16 as u16;
                let c8 = pos[2].wrapping_sub(z) as i16 as u16;
                game.entities[slot].unk_c6 = c6;
                game.entities[slot].unk_c8 = c8;
                if let Some(victim) = game.entities.get_mut(target as usize) {
                    victim.unk_c6 = c6;
                    victim.unk_c8 = c8;
                }
                Ok(())
            });

            // The Tyrant's room-action probe (`update_player_position(ENTITY,
            // 2)`): only observable in the heliport/ending sequence, which is
            // out of scope, so the call is inert and documented.
            methods.add_method_mut("probe_room_actions", |_lua, _api, _mask: i64| Ok(()));

            // The shared switch-zone tail that preserves the death-park bit.
            methods.add_method_mut("update_switch_zone_keep_high", |_lua, api, ()| {
                let room = unsafe { &*api.room };
                let slot = api.slot;
                let game = api.game();
                let zone = u8::from(in_camera_zone(
                    room,
                    room.current_cut,
                    game.entities[slot].pos,
                ));
                let high = game.entities[slot].has_enter_switch_zone & 0x80;
                game.entities[slot].has_enter_switch_zone = high | zone;
                Ok(zone)
            });

            // A billboard at a joint with an explicit light factor (the dust
            // puffs' `0x14`).
            methods.add_method_mut(
                "spawn_joint_effect_lit",
                |_lua,
                 api,
                 (effect_type, depth, joint, x, y, z, yaw, light): (
                    i64,
                    i64,
                    i64,
                    i64,
                    i64,
                    i64,
                    i64,
                    i64,
                )| {
                    let slot = api.slot;
                    let game = api.game();
                    let room_effects = Rc::clone(&game.room_effects);
                    Ok(crate::effects::create_attached(
                        game,
                        &room_effects,
                        effect_type as u8,
                        depth as u8,
                        Attach::Joint(slot as u8, joint as u8),
                        [x as i32, y as i32, z as i32],
                        yaw as i16,
                        light as u8,
                    ))
                },
            );

            // `GetAngleQuadrantValue`: a fixed-point slope to the game's 12-bit
            // angle (the Tyrant's knock-back facing).
            methods.add_method("angle_quadrant", |_lua, _api, slope: i64| {
                Ok(crate::enemy::custom_anim::angle_quadrant(slope as i32))
            });

            // The rocket death's camera-aim store: write the vector toward the
            // live room camera into the entity's speed words (the original's
            // `ENTITY->speed` write; the smoke trail itself uses a fixed
            // vector, so this is observable state only).
            methods.add_method_mut("store_camera_speed", |_lua, api, ()| {
                let room = api.room;
                let slot = api.slot;
                let game = api.game();
                // SAFETY: the room pointer borrows the caller's room for the
                // whole call (see the type-level comment).
                let room = unsafe { &*room };
                let cam = room
                    .cuts
                    .get(room.current_cut)
                    .map_or([0; 3], |cut| cut.pos);
                let pos = game.entities[slot].pos;
                game.entities[slot].speed = [
                    ((cam[0] - pos[0]) / 0x32) as i16,
                    (cam[1] as i16).wrapping_add(((1000 - pos[1]) / 0x32) as i16),
                    ((cam[2] - pos[2]) / 0x32) as i16,
                ];
                Ok(())
            });

            // `VectorNormal`: scale a vector to length 4096 with the
            // original's double-precision truncation.
            methods.add_method("vector_normal", |_lua, _api, (x, y, z): (i64, i64, i64)| {
                let (x, y, z) = (x as f64, y as f64, z as f64);
                let length_squared = x * x + y * y + z * z;
                let mut length = length_squared.sqrt();
                if length == 0.0 {
                    length = 1e-9;
                }
                Ok((
                    (x * 4096.0 / length) as i32,
                    (y * 4096.0 / length) as i32,
                    (z * 4096.0 / length) as i32,
                ))
            });
        }
    }

    /// The Tyrant's claw chain matrix: the entity rotation times joint 0's
    /// transform, then the transforms of joints 1, 6, 7 and 8 (the original's
    /// five in-place compositions). `None` when the pose is unavailable.
    fn claw_chain(
        game: &crate::game::GameState,
        slot: usize,
        clips: &[crate::model::Clip],
    ) -> Option<crate::anim::Mat4x3> {
        let entity = game.entities[slot];
        let transforms = game.entity_anims[slot].local_transforms(&entity, clips);
        let mut scratch = crate::anim::compose(
            &crate::anim::entity_matrix_rotated(
                entity.pos,
                entity.pitch,
                entity.angle,
                entity.roll,
            ),
            transforms.first()?,
        );
        for index in [1usize, 6, 7, 8] {
            scratch = crate::anim::compose(&scratch, transforms.get(index)?);
        }
        Some(scratch)
    }
}

pub use imp::LuaEnemyHost;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::GameState;
    use crate::pack::{Pack, PackWriter};
    use crate::state::RoomState;

    /// Build an in-memory pack from `(path, source)` entries.
    fn pack(entries: &[(&str, &str)]) -> Pack {
        let mut writer = PackWriter::new();
        for (path, source) in entries {
            writer.add(path, source.as_bytes().to_vec()).unwrap();
        }
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    /// The checked-in spider web script, so the tests exercise the real port.
    #[cfg(feature = "lua")]
    fn spiderweb_source() -> &'static str {
        crate::enemy::ENEMY_SCRIPTS
            .iter()
            .find(|(path, _)| *path == "enemy/em13.lua")
            .expect("the checked-in spider web script")
            .1
    }

    /// One active spider web in slot 1 with death event 3.
    fn spiderweb_game() -> GameState {
        let mut game = GameState::default();
        game.entities[1].id = 0x13;
        game.entities[1].death_event_id = 3;
        game.entities[1].set_active(true);
        game.enemy_count = 1;
        game
    }

    fn update(host: &mut LuaEnemyHost, game: &mut GameState, pack: &Pack) -> bool {
        host.update(game, &RoomState::default(), pack, 1, &[])
    }

    #[test]
    fn a_pack_without_enemy_scripts_parks_the_slot() {
        let pack = pack(&[]);
        let mut game = spiderweb_game();
        let mut host = LuaEnemyHost::new();
        assert!(!update(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].state(), 0, "the script never ran");
    }

    #[cfg(feature = "lua")]
    #[test]
    fn spiderweb_script_initialises_and_idles() {
        let pack = pack(&[("enemy/em13.lua", spiderweb_source())]);
        let mut game = spiderweb_game();
        let mut host = LuaEnemyHost::new();

        assert!(update(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1, "init moves to idle");
        assert_eq!(entity.health, 0x37);
        assert_eq!(entity.joint_scale, 0x1000);
        assert_eq!(entity.sca_radius, 500);
        assert_eq!(entity.sca_offset, [-700, 0, 0], "Chris' scenario offset");
        assert_eq!(entity.shadow_half_x, 10);
        assert_eq!(entity.shadow_half_z, 1000);
        assert_eq!(entity.shadow_tint, 0x00FF_FFFF);

        // Idle keeps the low bits and forces the aligned target bit.
        assert!(update(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].status_flags, (1 & 0x1F) | 0x40);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn spiderweb_script_burns_and_destroys() {
        let pack = pack(&[("enemy/em13.lua", spiderweb_source())]);
        let mut game = spiderweb_game();
        let mut host = LuaEnemyHost::new();
        let _ = update(&mut host, &mut game, &pack);
        let _ = update(&mut host, &mut game, &pack);

        // A handgun hit (weapon bits 0x10) only clears hit_state.
        game.entities[1].set_state(2);
        game.entities[1].hit_state = 0x10;
        assert!(update(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].state(), 1);
        assert_eq!(game.entities[1].hit_state, 0);
        assert_eq!(game.entities[1].joint_flags, 0, "the web is untouched");

        // A knife hit at 45 health burns the top strand (joint 5).
        game.entities[1].set_state(2);
        game.entities[1].hit_state = 0x08;
        game.entities[1].health = 0x2D;
        assert!(update(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].joint_flags, 1 << 5);

        // A rocket at 5 health burns through joint 1; joint 0 survives.
        game.entities[1].set_state(2);
        game.entities[1].hit_state = 0x38;
        game.entities[1].health = 0x05;
        assert!(update(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].joint_flags, 0b0011_1110);

        // The killing hit clears joint 0 too, raises the death flag and parks.
        game.entities[1].set_state(3);
        assert!(update(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].joint_flags, 0b0011_1111);
        assert_eq!(game.entities[1].status_flags & 0x02, 0x02);
        assert_eq!(game.entities[1].state(), 4);
        assert!(game.flags[crate::game::BANK_ENEMIES as usize].bit(3));
    }

    /// The shared fire entry drives a scripted web through the whole
    /// damage -> state 2/3 -> script reaction -> death-flag contract.
    #[cfg(feature = "lua")]
    #[test]
    fn a_scripted_web_kill_runs_through_the_shared_damage_pipeline() {
        let pack = pack(&[("enemy/em13.lua", spiderweb_source())]);
        let room = RoomState::default();
        let mut game = spiderweb_game();
        let mut host = LuaEnemyHost::new();
        // Spawn init then idle: the web carries its aligned target bit.
        let _ = update(&mut host, &mut game, &pack);
        let _ = update(&mut host, &mut game, &pack);
        game.player_flags = 0x40;
        game.entities[1].pos = [200, 0, 0];

        // Knife: 10 damage, the surviving hit enters state 2.
        assert_eq!(game.apply_weapon_damage(&room, 1), 1);
        assert_eq!(game.entities[1].health, 0x37 - 10);
        assert_eq!(game.entities[1].state(), 2);
        assert_eq!(game.entities[1].hit_state, 0x08 | 1 | 2);
        assert!(update(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].state(), 1, "the script rewound to idle");
        assert_eq!(game.entities[1].hit_state, 0);
        assert_eq!(game.entities[1].joint_flags, 1 << 5, "one strand burned");

        // The rocket one-shots it through the projectile class; the killing
        // blow enters state 3 and raises the record's death flag.
        assert_eq!(game.apply_weapon_damage(&room, 10), 1);
        assert_eq!(game.entities[1].state(), 3);
        assert!(game.flags[crate::game::BANK_ENEMIES as usize].bit(3));
        assert!(update(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].state(), 4, "destroy parks the web");
        assert_eq!(game.entities[1].joint_flags, 0b0011_1111);
    }

    /// The roots are untargetable by design: their init arms `hit_state = 1`,
    /// which is exactly the shared pipeline's `hit_state == 0` candidate
    /// gate, so no weapon ever reaches their state 2/3 entries and no death
    /// flag is raised.
    #[cfg(feature = "lua")]
    #[test]
    fn the_roots_hit_state_gate_rejects_every_weapon() {
        let pack = pack(&[("enemy/em0e.lua", roots_source())]);
        let room = RoomState::default();
        let mut game = roots_game(0x80);
        game.entities[1].death_event_id = 0xFF;
        let mut host = LuaEnemyHost::new();
        let _ = roots_step(&mut host, &mut game, &pack, &room);
        assert_eq!(game.entities[1].hit_state, 1, "the init armed the gate");
        game.player_flags = 0x40;
        game.entities[1].pos = [200, 0, 0];
        game.entities[1].health = 1;

        for weapon in [1u8, 2, 5, 10] {
            assert_eq!(
                game.apply_weapon_damage(&room, weapon),
                0,
                "weapon {weapon} must be rejected by the hit-state gate"
            );
        }
        assert_eq!(game.entities[1].health, 1);
        assert_eq!(game.entities[1].state(), 1);
        assert!(
            !game.flags[crate::game::BANK_ENEMIES as usize]
                .bytes()
                .iter()
                .any(|byte| *byte != 0),
            "the roots raise no death flag"
        );
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        assert_eq!(game.entities[1].state(), 1);
    }

    /// The shared player-side helpers exposed to the attacking monsters:
    /// hurt, poison, the animation overrides and the grab pose.
    #[cfg(feature = "lua")]
    #[test]
    fn the_player_hurt_helpers_write_the_player_state() {
        let pack = pack(&[(
            "enemy/em13.lua",
            r#"
function update(e)
  e:hurt_player(6, 1)
  e:poison_player()
  e:set_player_animation(5, 7)
  e:set_player_action(0, 0)
  e.player_attacked = 1
  e:grab_player()
end
"#,
        )]);
        let mut game = spiderweb_game();
        game.entities[0].health = 5;
        game.entities[0].pos = [123, 0, -456];
        game.enemy_count = 1;
        let mut host = LuaEnemyHost::new();
        assert!(update(&mut host, &mut game, &pack));
        assert_eq!(game.entities[0].health, 1, "clamped to one");
        assert_eq!(game.health_status & 0x02, 0x02);
        assert_eq!(game.poison_timer, crate::combat::POISON_TIMER);
        // The overrides write the state block's animationId/animFrameId bytes
        // (state 5 / ignore 7), not the clip id and clip frame.
        assert_eq!(game.entities[0].state(), 5);
        assert_eq!(game.entities[0].ignore(), 7);
        assert_eq!(game.entities[0].animation_id, 0, "the clip id is untouched");
        assert_eq!(
            game.entities[0].animation_frame_id, 0,
            "the clip frame is untouched"
        );
        assert_eq!(game.entities[0].is_being_attacked, 1);
        assert_eq!(game.entities[0].unk_c6, 123);
        assert_eq!(game.entities[0].unk_c8, (-456i16) as u16);
        assert_eq!(game.entities[1].unk_c6, 123);
        assert_eq!(game.entities[1].unk_c8, (-456i16) as u16);
    }

    /// The ported NPC states 0 and 1 must match the native driver field for
    /// field, tick for tick, for every character id, idle behaviour, pose
    /// variant and the two flag-gated openings.
    #[cfg(feature = "lua")]
    #[test]
    fn npc_states_zero_and_one_match_the_native_driver() {
        use crate::enemy::{FIRST_ID, LAST_ID};
        use crate::state::RoomId;

        let pack = pack(crate::enemy::ENEMY_SCRIPTS);
        let room = RoomState::default();
        let mut host = LuaEnemyHost::new();

        // (flags setup, stage, room) variants: bare, wounded Rebecca and
        // variant Wesker flags, and the laboratory power room.
        let variants: &[(bool, u8, u8)] = &[(false, 1, 0), (true, 1, 0), (true, 5, 0x11)];

        for id in FIRST_ID..=LAST_ID {
            for behavior in [0u8, 1, 2, 3, 9, 10, 13, 7] {
                for &(set_flags, stage, room_id) in variants {
                    let mut native = GameState::default();
                    let mut scripted = GameState::default();
                    for game in [&mut native, &mut scripted] {
                        game.id = RoomId {
                            stage,
                            room: room_id,
                            player_flag: 0,
                        };
                        if set_flags {
                            game.flags[1].apply(0xC0, 0);
                            game.flags[0].apply(0x37, 0);
                        }
                        let entity = &mut game.entities[1];
                        entity.id = id;
                        entity.set_active(true);
                        entity.action_behavior = behavior;
                        entity.animation_id = 5;
                        entity.animation_frame_id = 3;
                        entity.timing_control = 1;
                        entity.status_flags = 1;
                    }

                    for tick in 0..8 {
                        let clips: &[crate::model::Clip] = &[];
                        let target = crate::enemy::live_look_at_target(&native, 1);
                        crate::enemy::update_entity(&mut native, 1, &room, clips, target);
                        assert!(host.update(&mut scripted, &room, &pack, 1, clips));
                        assert_eq!(
                            native.entities[1], scripted.entities[1],
                            "id {id:#04x} behaviour {behavior} tick {tick}"
                        );
                        assert_eq!(
                            native.entity_anims[1], scripted.entity_anims[1],
                            "clock id {id:#04x} behaviour {behavior} tick {tick}"
                        );
                        assert_eq!(
                            native.effects.active_count(),
                            scripted.effects.active_count(),
                            "effects id {id:#04x} behaviour {behavior} tick {tick}"
                        );
                    }
                }
            }
        }
    }

    /// The ported NPC state-8 scripted-action handlers must match the native
    /// driver field for field, tick for tick, across every character id,
    /// behaviour handler, weapon id, entity flag bit and action state.
    #[cfg(feature = "lua")]
    #[test]
    fn npc_state_eight_matches_the_native_driver() {
        use crate::effects::fixtures::{block, sprite};
        use crate::enemy::{FIRST_ID, LAST_ID};
        use crate::model::ClipFrame;
        use crate::state::FootstepZone;

        /// One differential case.
        #[derive(Clone, Copy)]
        struct Case {
            behavior: u8,
            weapon: u8,
            flags: u16,
            state: u8,
            frame: u8,
            timing: u8,
            collision: u8,
            target_x: u16,
            target_z: u16,
            timer: u16,
            counter: u16,
            slow: bool,
            ticks: usize,
        }

        fn case(behavior: u8, state: u8) -> Case {
            Case {
                behavior,
                weapon: 2,
                flags: 0,
                state,
                frame: 0,
                timing: 0,
                collision: 0,
                target_x: 1000,
                target_z: 0,
                timer: 0x40,
                counter: 0,
                slow: false,
                ticks: 7,
            }
        }

        /// The shared opening state of one case, with the weapon effect
        /// sprites the fire tables reference.
        fn seed(template: &GameState, id: u8, case: &Case) -> GameState {
            let mut game = template.clone();
            if case.slow {
                game.flags[5].apply(crate::game::MSF2_EFFECT_ZONE, 0);
            }
            let entity = &mut game.entities[1];
            entity.id = id;
            entity.set_active(true);
            entity.status_flags = 1;
            entity.set_state(8);
            entity.action_behavior = case.behavior;
            entity.behavior_flags = case.weapon;
            entity.flags = case.flags;
            entity.action_state = case.state;
            entity.animation_id = 0x11;
            entity.animation_frame_id = case.frame;
            entity.timing_control = case.timing;
            entity.blend_counter = 7;
            entity.move_speed_current = 0;
            entity.action_ticks_counter = case.counter;
            entity.collision_flags = case.collision;
            entity.scd_timer = case.timer;
            entity.unk_c6 = case.target_x;
            entity.unk_c8 = case.target_z;
            entity.scd_anim_param = 0x21;
            game
        }

        let pack = pack(crate::enemy::ENEMY_SCRIPTS);
        let room = RoomState {
            stage: 1,
            room: 0,
            footstep_zones: vec![FootstepZone {
                base_x: 0,
                base_z: 0,
                width: 0x8000,
                height: 0x8000,
                sound_data: 0x2D,
            }],
            ..RoomState::default()
        };

        // Two one-tick frames per clip id, so every handler completes within
        // a handful of ticks and every advance step is exercised.
        let clips: Vec<crate::model::Clip> = (0..=0x36)
            .map(|_| crate::model::Clip {
                frames: vec![
                    ClipFrame {
                        keyframe: 0,
                        timing: 1,
                    },
                    ClipFrame {
                        keyframe: 1,
                        timing: 1,
                    },
                ],
            })
            .collect();

        let mut template = GameState::default();
        for index in [0u8, 5, 8, 9, 11, 12, 17] {
            template.weapon_effects.sprites.push(sprite(
                index,
                std::array::from_fn(|_| vec![vec![block(1, 0, 0)]]),
            ));
        }

        let mut cases: Vec<Case> = Vec::new();

        // Core sweep: every handler in every action state. The run/weapon
        // handlers that branch on the higher states get the full run; the
        // plain playback handlers only distinguish 0/1/2 from the rest. Close
        // targets finish the walks; far off-axis targets keep the turn phases
        // turning, and the flag bits rotate down the state column.
        const STATE_FLAGS: [u16; 7] = [0x0000, 0x0001, 0x0010, 0x0020, 0x0080, 0x0002, 0x0022];
        for behavior in 0..=10u8 {
            let states: &[u8] = match behavior {
                3 => &[0, 1, 2, 3, 4, 5, 6],
                8 => &[0, 1, 2, 3, 4, 5, 6],
                _ => &[0, 1, 2, 3, 4],
            };
            for &state in states {
                let mut c = case(behavior, state);
                c.flags = STATE_FLAGS[usize::from(state)];
                c.weapon = if behavior == 8 { 5 } else { 2 };
                c.ticks = 5;
                if state <= 1 {
                    c.target_x = 1000;
                    c.target_z = 1000;
                } else {
                    c.target_x = 0x50;
                    c.target_z = 0;
                }
                if state == 1 {
                    match behavior {
                        2..=4 => c.frame = 8,
                        5 => {
                            c.frame = 7;
                            c.timing = 2;
                        }
                        _ => {}
                    }
                }
                if behavior == 3 && state == 5 {
                    c.counter = 3;
                }
                if behavior == 8 && state == 5 {
                    c.counter = 1;
                }
                cases.push(c);
            }
        }

        // Fire sweep: every weapon record on each fire-table trigger frame and
        // at the state-0 setup (behavior_flags 2..=17 also covers the
        // out-of-range weapons 14/15, which spawn nothing); the flame loop
        // (states 4/5) never reads the weapon, so one record covers it.
        for weapon in 2..=17u8 {
            for &frame in &[0u8, 1, 2, 3, 0x19] {
                let mut c = case(8, 1);
                c.weapon = weapon;
                c.frame = frame;
                c.ticks = 2;
                cases.push(c);
            }
            let mut c = case(8, 0);
            c.weapon = weapon;
            c.ticks = 2;
            cases.push(c);
        }
        for &(state, frame) in &[(4u8, 0u8), (4, 6), (5, 0), (5, 6)] {
            let mut c = case(8, state);
            c.weapon = 5;
            c.frame = frame;
            c.ticks = 2;
            cases.push(c);
        }

        // Repeat sweep: entity flag bit 1 re-runs the handler with the
        // re-read behaviour byte, alone and combined with the reverse and
        // loop bits, at the setup and completion states.
        for behavior in 0..=10u8 {
            for &(flags, state) in &[(0x0002u16, 1u8), (0x0082, 1), (0x0002, 2), (0x0012, 2)] {
                let mut c = case(behavior, state);
                c.flags = flags;
                c.weapon = if behavior == 8 { 4 } else { 2 };
                c.frame = 8;
                c.target_x = 0x50;
                c.ticks = 3;
                cases.push(c);
            }
        }

        // Targeted walk/turn seeds: the footstep frames, the collision-flag
        // arrivals, the state-5 deceleration countdown and the slow/fast
        // footstep resolution.
        for &behavior in &[2u8, 3, 4, 5] {
            for &(state, frame, timing, collision, target_x, target_z) in &[
                (0u8, 0u8, 0u8, 0u8, 1000u16, 0u16),
                (1, 8, 0, 0, 1000, 0),
                (1, 0x16, 0, 0, 1000, 0),
                (1, 0, 0, 0x80, 1000, 0),
                (2, 0, 0, 0x80, 0x50, 0),
                (3, 0, 0, 0, 0x50, 0),
                (3, 0, 0, 0x80, 0x50, 0),
                (4, 0, 0, 0, 0x50, 0),
                (5, 0, 0, 0, 0x50, 0),
                (6, 0, 0, 0, 0x50, 0),
            ] {
                let mut c = case(behavior, state);
                c.frame = frame;
                c.timing = timing;
                c.collision = collision;
                c.target_x = target_x;
                c.target_z = target_z;
                c.counter = 3;
                c.ticks = 4;
                cases.push(c);
            }
        }
        for &state in &[0u8, 1, 3] {
            let mut c = case(6, state);
            c.target_x = 0x50;
            c.ticks = 4;
            cases.push(c);
        }
        // The timer's bit 15 flips `rotate_toward_target` into the face-away
        // branch, and a large timer exercises the turn/align arithmetic.
        for &behavior in &[2u8, 3, 6] {
            let mut c = case(behavior, 1);
            c.timer = 0x8040;
            c.target_x = 1000;
            c.target_z = 1000;
            c.ticks = 3;
            cases.push(c);
        }
        for &(behavior, frame) in &[(5u8, 7u8), (5, 0x1B)] {
            let mut c = case(behavior, 1);
            c.frame = frame;
            c.timing = 2;
            c.target_x = 0x50;
            c.ticks = 3;
            cases.push(c);
        }
        for &behavior in &[2u8, 3, 4, 5] {
            for &slow in &[false, true] {
                let mut c = case(behavior, 1);
                c.frame = if behavior == 5 { 7 } else { 8 };
                c.timing = u8::from(behavior == 5) * 2;
                c.slow = slow;
                c.target_x = 1000;
                c.ticks = 3;
                cases.push(c);
            }
        }

        // Out-of-range behaviours: the NULL table slot is counted and inert
        // (behaviour 11, the first slot past the table, and far past it).
        for behavior in [11u8, 12, 200] {
            for &state in &[0u8, 2, 6] {
                let mut c = case(behavior, state);
                c.ticks = 3;
                cases.push(c);
            }
        }

        let mut host = LuaEnemyHost::new();
        // Coverage markers: the matrix must reach every handler, an effect
        // spawn, a footstep and the placeholder count.
        let mut covered = [false; 11];
        let mut saw_effect = false;
        let mut saw_sound = false;
        let mut saw_placeholder = false;
        for id in FIRST_ID..=LAST_ID {
            for (index, case) in cases.iter().enumerate() {
                let mut native = seed(&template, id, case);
                let mut scripted = seed(&template, id, case);
                for tick in 0..case.ticks {
                    let target = crate::enemy::live_look_at_target(&native, 1);
                    crate::enemy::update_entity(&mut native, 1, &room, &clips, target);
                    assert!(
                        host.update(&mut scripted, &room, &pack, 1, &clips),
                        "id {id:#04x} case {index} tick {tick}: script did not run"
                    );
                    if usize::from(case.behavior) < covered.len() {
                        covered[usize::from(case.behavior)] = true;
                    }
                    saw_effect |= native.effects.active_count() > 0;
                    saw_sound |= !native.entity_sounds.is_empty();
                    saw_placeholder |= !native.npc_placeholders.is_empty();
                    assert!(
                        native.entities[1] == scripted.entities[1],
                        "entity id {id:#04x} case {index} tick {tick}"
                    );
                    assert!(
                        native.entity_anims[1] == scripted.entity_anims[1],
                        "clock id {id:#04x} case {index} tick {tick}"
                    );
                    assert!(
                        native.effects == scripted.effects,
                        "effects id {id:#04x} case {index} tick {tick}"
                    );
                    assert!(
                        native.entity_sounds == scripted.entity_sounds,
                        "sounds id {id:#04x} case {index} tick {tick}"
                    );
                    assert!(
                        native.npc_placeholders == scripted.npc_placeholders,
                        "placeholders id {id:#04x} case {index} tick {tick}"
                    );
                    assert!(
                        native.flags == scripted.flags,
                        "flags id {id:#04x} case {index} tick {tick}"
                    );
                }
            }
        }
        assert!(
            covered.iter().all(|&hit| hit),
            "every handler ran: {covered:?}"
        );
        assert!(saw_effect, "the fire handler spawned effects");
        assert!(saw_sound, "the walk handlers queued footsteps");
        assert!(saw_placeholder, "out-of-range behaviours were counted");
    }

    /// The ported NPC state-9 follow/pathfind driver must match the native
    /// driver field for field, tick for tick, across every behaviour, action
    /// state, distance ring, path result, animation reverse, footstep and
    /// collision case.
    #[cfg(feature = "lua")]
    #[test]
    fn npc_state_nine_matches_the_native_driver() {
        use crate::game::Entity;
        use crate::model::{Clip, ClipFrame};
        use crate::state::{CollisionRect, FootstepZone, WalkZone};

        type Check = Box<dyn Fn(&Entity, usize)>;

        fn zone(x1: i16, z1: i16, x2: i16, z2: i16, flags: u16) -> WalkZone {
            WalkZone {
                x1,
                z1,
                x2,
                z2,
                field_08: 0,
                flags,
            }
        }

        fn rect(x_min: u16, z_min: u16, x_max: u16, z_max: u16, flags: u16) -> CollisionRect {
            CollisionRect {
                x_max,
                z_max,
                x_min,
                z_min,
                kind: 1,
                flags,
            }
        }

        /// One differential case: the seeded state of slot 1 (plus any extra
        /// character slots) and how long to run it.
        struct Case {
            id: u8,
            behavior: u8,
            state: u8,
            ignore: u8,
            dir: u8,
            collision: u8,
            pathfind: u8,
            saved: Option<[i32; 3]>,
            tex_bank: u8,
            attacking: u8,
            seq_counter: u8,
            pos: [i32; 3],
            angle: u16,
            player: [i32; 3],
            player_angle: u16,
            slow: bool,
            anim: u8,
            frame: u8,
            ticks: usize,
            /// Reset the wander countdown to one every tick, so each tick
            /// reseeds from that tick's platform draw.
            reseeds: bool,
            others: Vec<(u8, [i32; 3], u16, i16)>,
            check: Option<Check>,
        }

        fn base(id: u8) -> Case {
            Case {
                id,
                behavior: 1,
                state: 1,
                ignore: 1,
                dir: 0,
                collision: 0,
                pathfind: 0,
                saved: None,
                tex_bank: 0,
                attacking: 0,
                seq_counter: 0,
                pos: [500, 0, 500],
                angle: 0,
                player: [1500, 0, 500],
                player_angle: 0,
                slow: false,
                anim: 7,
                frame: 0,
                ticks: 20,
                reseeds: false,
                others: Vec::new(),
                check: None,
            }
        }

        fn seed(case: &Case) -> GameState {
            let mut game = GameState::default();
            if case.slow {
                game.flags[5].apply(crate::game::MSF2_EFFECT_ZONE, 0);
            }
            game.entities[0].pos = case.player;
            game.entities[0].angle = case.player_angle;
            game.entities[0].set_active(true);
            let entity = &mut game.entities[1];
            entity.id = case.id;
            entity.set_active(true);
            entity.set_state(9);
            entity.set_ignore(case.ignore);
            entity.pos = case.pos;
            entity.angle = case.angle;
            entity.sca_radius = 100;
            entity.action_behavior = case.behavior;
            entity.action_state = case.state;
            entity.animation_id = case.anim;
            entity.animation_frame_id = case.frame;
            entity.timing_control = 0;
            entity.blend_counter = 7;
            entity.attacking_direction = case.attacking;
            entity.tex_bank = case.tex_bank;
            entity.seq_counter = case.seq_counter;
            entity.dir_control_flags = case.dir;
            entity.collision_flags = case.collision;
            entity.pathfind_state = case.pathfind;
            entity.saved_pos = case.saved;
            for (index, &(id, pos, angle, radius)) in case.others.iter().enumerate() {
                let other = &mut game.entities[2 + index];
                other.id = id;
                other.pos = pos;
                other.angle = angle;
                other.sca_radius = radius;
                other.set_active(true);
            }
            game
        }

        /// The behaviour × action-state × parity sweep at fixed positions. The
        /// ignore, reverse and collision flags rotate down the state column so
        /// every combination is reached without a full product.
        fn sweep(id: u8, pos: [i32; 3], player: [i32; 3]) -> Vec<Case> {
            let mut cases = Vec::new();
            for behavior in 0..=3u8 {
                for state in 0..=3u8 {
                    let mut case = base(id);
                    case.behavior = behavior;
                    case.state = state;
                    case.ignore = state & 1;
                    case.dir = (state >> 1) & 1;
                    case.collision = [0, 0x10, 0x80][usize::from(state) % 3];
                    case.anim = match behavior {
                        1 | 3 => 7,
                        2 => 8,
                        _ => 0,
                    };
                    case.pos = pos;
                    case.player = player;
                    cases.push(case);
                }
            }
            cases
        }

        // Clips with two dozen frames and varied holds, so the clock advances,
        // wraps and reads different data forwards and in reverse.
        let clips: Vec<Clip> = (0..=0x40)
            .map(|_| Clip {
                frames: (0..24)
                    .map(|index| ClipFrame {
                        keyframe: 0,
                        timing: (index % 3) + 1,
                    })
                    .collect(),
            })
            .collect();

        // 1. Same-zone room: the path is Direct.
        let direct = RoomState {
            walk_zones: vec![zone(0, 0, 8000, 8000, 0)],
            ..RoomState::default()
        };
        // 2. Open two-zone corridors, one sharing an X edge and one sharing a
        // Z edge (the Z edge's extrapolation divides with a negative
        // remainder, pinning the truncating division).
        let x_corridor = RoomState {
            walk_zones: vec![
                zone(0, 0, 100, 100, 1 << 1),
                zone(100, 0, 200, 100, (1 << 0) | (1 << 2)),
                zone(200, 0, 300, 100, 1 << 1),
            ],
            ..RoomState::default()
        };
        let z_corridor = RoomState {
            walk_zones: vec![
                zone(0, 0, 1000, 100, 1 << 1),
                zone(0, 100, 1000, 200, 1 << 0),
            ],
            ..RoomState::default()
        };
        // 3a. The corridor's far end is walled off: `corridor_open` fails and
        // the crossing clamp pulls the waypoint to z = 235.
        let mut blocked_corridor = RoomState {
            walk_zones: vec![
                zone(0, 0, 100, 1000, 1 << 1),
                zone(100, 0, 200, 1000, 1 << 0),
            ],
            ..RoomState::default()
        };
        blocked_corridor.collision.quadrants[0].push(rect(0, 950, 300, 1050, 0));
        // 3b. A wall sits on the extrapolated crossing: `corridor_open`
        // passes but `position_blocked` rejects the point, so the fallback
        // clamps the entity's own X into the corridor.
        let mut pinched_corridor = RoomState {
            walk_zones: vec![
                zone(0, 0, 1000, 100, 1 << 1),
                zone(0, 100, 1000, 200, 1 << 0),
            ],
            ..RoomState::default()
        };
        pinched_corridor.collision.quadrants[0].push(rect(450, 90, 550, 110, 0));
        // 4. A disconnected pair, and the target outside every zone.
        let unreachable = RoomState {
            walk_zones: vec![zone(0, 0, 100, 100, 0), zone(1000, 1000, 1100, 1100, 0)],
            ..RoomState::default()
        };
        // The sight-geometry gate: a fully blocking record in the quadrant the
        // counter-3 frame reads blocks the latch; a record without the 0x300
        // bits does not.
        let mut los_room = direct.clone();
        los_room.collision.quadrants[3].push(rect(200, 0, 300, 100, 0x300));
        let mut los_clear_room = direct.clone();
        los_clear_room.collision.quadrants[3].push(rect(200, 0, 300, 100, 0));
        // 5. The saved position rollback wall and a kind-5 floor volume.
        let mut rollback_room = direct.clone();
        rollback_room.collision.quadrants[0].push(rect(1000, 0, 1400, 1000, 0));
        rollback_room.collision.quadrants[0].push(CollisionRect {
            x_max: 1800,
            z_max: 1000,
            x_min: 1500,
            z_min: 0,
            kind: 5,
            flags: 0,
        });
        // 6. A room sound plus the effect-zone slow bit.
        let footstep_room = RoomState {
            stage: 1,
            room: 6,
            walk_zones: vec![zone(0, 0, 8000, 8000, 0)],
            footstep_zones: vec![FootstepZone {
                base_x: 0,
                base_z: 0,
                width: 0x8000,
                height: 0x8000,
                sound_data: 45,
            }],
            ..RoomState::default()
        };

        let mut fixtures: Vec<(&str, RoomState, Vec<Case>)> = Vec::new();

        // Same-zone room: near and far sweeps cover every behaviour body (the
        // ring swaps some of them into their neighbours), plus targeted
        // heading, swap and pathfind cases.
        let mut cases = sweep(0x20, [500, 0, 500], [1500, 0, 500]);
        cases.extend(sweep(0x23, [500, 0, 500], [5500, 0, 500]));

        let mut c = base(0x23);
        c.behavior = 1;
        c.player = [4000, 0, 500];
        c.check = Some(Box::new(|e, tick| {
            if tick == 0 {
                assert_eq!(e.animation_id, 7);
                assert_eq!((e.player_pos_x, e.player_pos_z), (4000, 500));
                assert_ne!(e.bob_speed & 0x10, 0, "direct carries bit 4");
            }
        }));
        cases.push(c);

        let mut c = base(0x23);
        c.behavior = 1;
        c.player = [-3000, 0, 500];
        c.check = Some(Box::new(|e, tick| {
            if tick == 0 {
                assert_eq!(e.animation_id, 7);
                assert_eq!(e.pos[0], 500, "the turn-only branch does not walk");
            }
        }));
        cases.push(c);

        let mut c = base(0x23);
        c.behavior = 2;
        c.state = 1;
        c.anim = 8;
        c.player = [5500, 0, 500];
        c.check = Some(Box::new(|e, tick| {
            if tick == 0 {
                assert_eq!(e.animation_id, 8);
                assert_ne!(e.bob_speed & 0x10, 0);
            }
        }));
        cases.push(c);

        let mut c = base(0x23);
        c.behavior = 2;
        c.state = 0;
        c.anim = 8;
        c.player = [-4000, 0, 500];
        c.check = Some(Box::new(|e, tick| {
            if tick == 0 {
                assert_eq!(e.animation_id, 7, "the off-axis fallback clip");
            }
        }));
        cases.push(c);

        let mut c = base(0x23);
        c.behavior = 3;
        c.state = 1;
        c.anim = 3;
        c.frame = 7;
        c.player = [1500, 0, 500];
        c.check = Some(Box::new(|e, tick| {
            if tick == 0 {
                assert_eq!(e.animation_id, 3, "the backward walk clip");
                assert!(e.pos[0] < 500, "the character backed away");
            }
        }));
        cases.push(c);

        let mut c = base(0x23);
        c.behavior = 3;
        c.player = [-1000, 0, 500];
        c.check = Some(Box::new(|e, tick| {
            if tick == 0 {
                assert_eq!(e.animation_id, 7, "the turned walk clip");
            }
        }));
        cases.push(c);

        let mut c = base(0x23);
        c.behavior = 0;
        c.state = 1;
        c.anim = 0;
        c.tex_bank = 1;
        c.player = [3000, 0, 500];
        c.check = Some(Box::new(|e, tick| {
            if tick == 0 {
                assert_eq!(e.action_state, 2);
                assert_eq!(e.animation_id, 5, "the short walk clip");
            }
        }));
        cases.push(c);

        let mut c = base(0x23);
        c.behavior = 0;
        c.state = 2;
        c.anim = 5;
        c.attacking = 1;
        c.player = [3000, 0, 500];
        c.check = Some(Box::new(|e, tick| {
            if tick == 0 {
                assert_eq!(e.action_state, 3);
                assert_eq!(e.animation_id, 6, "the wander clip");
            }
        }));
        cases.push(c);

        // The wandering idle with the reseed due: the per-tick random seed
        // repaints the look-at angles. Both id parities select different draw
        // masks.
        let mut c = base(0x23);
        c.behavior = 0;
        c.state = 3;
        c.player = [3000, 0, 500];
        c.seq_counter = 1;
        c.reseeds = true;
        cases.push(c);
        let mut c = base(0x20);
        c.behavior = 0;
        c.state = 3;
        c.player = [3000, 0, 500];
        c.seq_counter = 1;
        c.reseeds = true;
        cases.push(c);

        // Reverse animation playback.
        let mut c = base(0x20);
        c.behavior = 1;
        c.dir = 1;
        c.player = [4000, 0, 500];
        cases.push(c);

        // A stored rollback position distinct from the live position.
        let mut c = base(0x23);
        c.behavior = 2;
        c.anim = 8;
        c.saved = Some([460, 0, 500]);
        c.player = [5500, 0, 500];
        cases.push(c);

        // The distance ring's exact boundaries, both sides of every test.
        for &(behavior, dist, expected) in &[
            (0u8, 1800i32, 0u8),
            (0, 1799, 3),
            (0, 4500, 1),
            (0, 4499, 0),
            (1, 2800, 1),
            (1, 2799, 0),
            (1, 6500, 2),
            (1, 6499, 1),
            (2, 4500, 2),
            (2, 4499, 1),
            (2, 6500, 2),
            (3, 2500, 0),
            (3, 2499, 3),
        ] {
            let mut c = base(0x23);
            c.behavior = behavior;
            c.state = 1;
            c.pos = [1000, 0, 1000];
            c.player = [1000 + dist, 0, 1000];
            c.anim = match behavior {
                1 | 3 => 7,
                2 => 8,
                _ => 0,
            };
            c.check = Some(Box::new(move |e, tick| {
                if tick == 0 {
                    assert_eq!(e.action_behavior, expected, "distance {dist}");
                }
            }));
            cases.push(c);
        }

        // Obstacle pathfinder: the counter-3 latch (clear, angularly blocked
        // and with a stale LOS bit) on a behaviour that never writes the
        // waypoint itself.
        for &(pathfind, player, latched) in &[
            (3u8, [3499i32, 0, 1000], true),
            (3, [-1499, 0, 1000], false),
            (0x23, [3499, 0, 1000], false),
        ] {
            let mut c = base(0x23);
            c.behavior = 3;
            c.state = 1;
            c.anim = 7;
            c.pos = [1000, 0, 1000];
            c.player = player;
            c.pathfind = pathfind;
            c.check = Some(Box::new(move |e, tick| {
                if tick == 0 {
                    assert_eq!(e.pathfind_state, 4);
                    assert_eq!(
                        i32::from(e.player_pos_x),
                        if latched { player[0] } else { 0 },
                        "latch with seed {pathfind:#04x}"
                    );
                }
            }));
            cases.push(c);
        }

        fixtures.push(("direct", direct.clone(), cases));

        // Open X-edge corridor: Cross, extrapolated onto the crossing.
        let mut cases = vec![{
            let mut c = base(0x23);
            c.behavior = 2;
            c.state = 0;
            c.anim = 8;
            c.pos = [50, 0, 50];
            c.player = [250, 0, 50];
            c.check = Some(Box::new(|e, tick| {
                if tick == 0 {
                    assert_eq!(e.bob_speed, 1, "the path's next zone");
                    assert_eq!(e.player_pos_x, 100, "the shared X edge");
                }
            }));
            c
        }];
        cases.push({
            let mut c = base(0x20);
            c.behavior = 1;
            c.state = 3;
            c.anim = 7;
            c.pos = [50, 0, 50];
            c.player = [150, 0, 50];
            c
        });
        fixtures.push(("x_corridor", x_corridor, cases));

        // Open Z-edge corridor: the negative-remainder extrapolation must
        // truncate toward zero (452, not floor's 451).
        let cases = vec![{
            let mut c = base(0x23);
            c.behavior = 2;
            c.state = 1;
            c.anim = 8;
            c.pos = [600, 0, 50];
            c.player = [301, 0, 151];
            c.check = Some(Box::new(|e, tick| {
                if tick == 0 {
                    assert_eq!(e.player_pos_x, 452, "truncating division");
                }
            }));
            c
        }];
        fixtures.push(("z_corridor", z_corridor, cases));

        // The corridor-open failure and the position_blocked failure.
        let cases = vec![{
            let mut c = base(0x23);
            c.behavior = 2;
            c.state = 0;
            c.anim = 8;
            c.pos = [50, 0, 500];
            c.player = [150, 0, 500];
            c.check = Some(Box::new(|e, tick| {
                if tick == 0 {
                    assert_eq!(e.player_pos_x, 100);
                    assert_eq!(e.player_pos_z, 235, "the clamp nudged the end");
                }
            }));
            c
        }];
        fixtures.push(("blocked_corridor", blocked_corridor, cases));
        let cases = vec![{
            let mut c = base(0x23);
            c.behavior = 2;
            c.state = 0;
            c.anim = 8;
            c.pos = [400, 0, 50];
            c.player = [600, 0, 150];
            c.check = Some(Box::new(|e, tick| {
                if tick == 0 {
                    assert_eq!(e.player_pos_x, 400, "position_blocked forced the clamp");
                    assert_eq!(e.player_pos_z, 100);
                }
            }));
            c
        }];
        fixtures.push(("pinched_corridor", pinched_corridor, cases));

        // Unreachable paths: disconnected zones, an outside target and an
        // outside start.
        let cases = vec![
            {
                let mut c = base(0x23);
                c.behavior = 2;
                c.state = 0;
                c.anim = 8;
                c.pos = [50, 0, 50];
                c.player = [1050, 0, 1050];
                c.ticks = 10;
                c.check = Some(Box::new(|e, tick| {
                    if tick == 0 {
                        assert_eq!(e.bob_speed, 0xFF);
                    }
                }));
                c
            },
            {
                let mut c = base(0x23);
                c.behavior = 2;
                c.pos = [50, 0, 50];
                c.player = [5000, 0, 5000];
                c.ticks = 10;
                c.check = Some(Box::new(|e, tick| {
                    if tick == 0 {
                        assert_eq!(e.bob_speed, 0xFF, "the target is outside");
                    }
                }));
                c
            },
            {
                let mut c = base(0x23);
                c.behavior = 1;
                c.pos = [5000, 0, 5000];
                c.player = [1050, 0, 1050];
                c.ticks = 10;
                c.check = Some(Box::new(|e, tick| {
                    if tick == 0 {
                        assert_eq!(e.bob_speed, 0xFF, "the start is outside");
                    }
                }));
                c
            },
        ];
        fixtures.push(("unreachable", unreachable, cases));

        // The sight-geometry gate both ways.
        let cases = vec![{
            let mut c = base(0x23);
            c.behavior = 3;
            c.state = 1;
            c.anim = 7;
            c.pos = [50, 0, 50];
            c.player = [450, 0, 50];
            c.pathfind = 3;
            c.ticks = 20;
            c.check = Some(Box::new(|e, tick| {
                if tick == 0 {
                    assert_eq!(e.pathfind_state, 4);
                }
                assert_eq!(e.player_pos_x, 0, "the wall blocked the latch");
            }));
            c
        }];
        fixtures.push(("los_blocked", los_room, cases));
        let cases = vec![{
            let mut c = base(0x23);
            c.behavior = 3;
            c.state = 1;
            c.anim = 7;
            c.pos = [50, 0, 50];
            c.player = [450, 0, 50];
            c.pathfind = 3;
            c.ticks = 20;
            c.check = Some(Box::new(|e, tick| {
                if tick == 0 {
                    assert_eq!(e.player_pos_x, 450, "a non-blocking record latched");
                }
            }));
            c
        }];
        fixtures.push(("los_clear", los_clear_room, cases));

        // 5. SCA separation: a static pace pushed by another character, and a
        // moving character overlapping both the player and slot 2.
        let mut cases = vec![{
            let mut c = base(0x23);
            c.behavior = 0;
            c.state = 3;
            c.player = [3000, 0, 500];
            c.others = vec![(0x27, [530, 0, 500], 0, 100)];
            c.ticks = 3;
            c.check = Some(Box::new(|e, tick| {
                if tick == 0 {
                    assert_ne!(e.pos, [500, 0, 500], "the character pair pushed");
                }
            }));
            c
        }];
        cases.push({
            let mut c = base(0x23);
            c.behavior = 3;
            c.state = 1;
            c.anim = 3;
            c.pos = [500, 0, 500];
            c.player = [560, 0, 500];
            c.others = vec![(0x27, [530, 0, 500], 0, 100)];
            c.ticks = 5;
            c
        });
        cases.push({
            let mut c = base(0x20);
            c.behavior = 3;
            c.state = 2;
            c.anim = 3;
            c.pos = [500, 0, 500];
            c.player = [560, 0, 500];
            c.others = vec![(0x27, [520, 0, 500], 0x400, 100)];
            c.ticks = 5;
            c
        });
        fixtures.push(("separate", direct.clone(), cases));

        // Stored and missing rollback positions against a wall, plus the
        // kind-5 volume the 0x10 collision flag skips.
        let cases = vec![
            {
                let mut c = base(0x23);
                c.behavior = 1;
                c.pos = [1200, 0, 500];
                c.saved = Some([400, 0, 500]);
                c.player = [6000, 0, 500];
                c.ticks = 5;
                c
            },
            {
                let mut c = base(0x23);
                c.behavior = 2;
                c.anim = 8;
                c.pos = [1200, 0, 500];
                c.player = [6000, 0, 500];
                c.ticks = 5;
                c
            },
            {
                let mut c = base(0x20);
                c.behavior = 1;
                c.pos = [1700, 0, 500];
                c.collision = 0x10;
                c.player = [6000, 0, 500];
                c.ticks = 3;
                c
            },
            {
                let mut c = base(0x20);
                c.behavior = 1;
                c.pos = [1700, 0, 500];
                c.player = [6000, 0, 500];
                c.ticks = 3;
                c
            },
        ];
        fixtures.push(("rollback", rollback_room, cases));

        // 6. Footsteps through a room sound, with the effect-zone slow bit
        // both ways.
        let cases = vec![
            {
                let mut c = base(0x23);
                c.behavior = 1;
                c.state = 1;
                c.anim = 7;
                c.frame = 7;
                c.player = [3500, 0, 500];
                c
            },
            {
                let mut c = base(0x23);
                c.behavior = 1;
                c.state = 1;
                c.anim = 7;
                c.frame = 7;
                c.player = [3500, 0, 500];
                c.slow = true;
                c
            },
            {
                let mut c = base(0x23);
                c.behavior = 2;
                c.state = 1;
                c.anim = 8;
                c.frame = 9;
                c.player = [5500, 0, 500];
                c
            },
            {
                let mut c = base(0x20);
                c.behavior = 3;
                c.state = 1;
                c.anim = 3;
                c.frame = 7;
                c.player = [1500, 0, 500];
                c
            },
            {
                let mut c = base(0x20);
                c.behavior = 1;
                c.state = 0;
                c.anim = 7;
                c.player = [3500, 0, 500];
                c.slow = true;
                c
            },
        ];
        fixtures.push(("footsteps", footstep_room, cases));

        let pack = pack(crate::enemy::ENEMY_SCRIPTS);
        let mut host = LuaEnemyHost::new();
        let mut saw_sound = false;
        let mut saw_slow_sound = false;
        let mut slow_names = std::collections::HashSet::new();
        let mut fast_names = std::collections::HashSet::new();

        for (label, room, cases) in &fixtures {
            for (index, case) in cases.iter().enumerate() {
                let mut native = seed(case);
                let mut scripted = seed(case);
                for tick in 0..case.ticks {
                    // The platform draw varies per tick, so the wander reseeds
                    // repaint from different values.
                    let random = 0xACE1u16
                        .wrapping_add((tick as u16).wrapping_mul(0x1F3B))
                        .wrapping_add(u16::from(case.id));
                    native.rand_seed = random;
                    scripted.rand_seed = random;
                    if case.reseeds {
                        native.entities[1].seq_counter = 1;
                        scripted.entities[1].seq_counter = 1;
                    }
                    let target = crate::enemy::live_look_at_target(&native, 1);
                    crate::enemy::update_entity(&mut native, 1, room, &clips, target);
                    assert!(
                        host.update(&mut scripted, room, &pack, 1, &clips),
                        "{label} case {index} tick {tick}: script did not run"
                    );
                    assert_eq!(
                        native.entities[1], scripted.entities[1],
                        "{label} case {index} tick {tick}"
                    );
                    assert_eq!(
                        native.entity_anims[1], scripted.entity_anims[1],
                        "{label} case {index} tick {tick}"
                    );
                    assert_eq!(
                        native.effects, scripted.effects,
                        "{label} case {index} tick {tick}"
                    );
                    assert_eq!(
                        native.entity_sounds, scripted.entity_sounds,
                        "{label} case {index} tick {tick}"
                    );
                    assert_eq!(
                        native.flags, scripted.flags,
                        "{label} case {index} tick {tick}"
                    );
                    assert_eq!(
                        native.npc_placeholders, scripted.npc_placeholders,
                        "{label} case {index} tick {tick}"
                    );
                    if let Some(check) = &case.check {
                        check(&native.entities[1], tick);
                    }
                    for sound in &native.entity_sounds {
                        saw_sound = true;
                        if case.slow {
                            saw_slow_sound = true;
                            slow_names.insert(sound.name);
                        } else {
                            fast_names.insert(sound.name);
                        }
                    }
                }
            }
        }

        assert!(saw_sound, "the footstep fixtures queued room sounds");
        assert!(saw_slow_sound, "the slow-bit fixture queued a room sound");
        assert!(
            slow_names.iter().all(|name| !fast_names.contains(name)),
            "the effect-zone slow bit shifted the footstep column: {slow_names:?} vs {fast_names:?}"
        );
    }

    /// The checked-in adder script, so the tests exercise the real port.
    #[cfg(feature = "lua")]
    fn adder_source() -> &'static str {
        crate::enemy::ENEMY_SCRIPTS
            .iter()
            .find(|(path, _)| *path == "enemy/em0a.lua")
            .expect("the checked-in adder script")
            .1
    }

    /// The adder's synthetic model: four clips of 32 one-tick frames and a
    /// two-joint skeleton whose second joint sits 100 units along the model's
    /// X axis, so the bite reach has a joint to test.
    #[cfg(feature = "lua")]
    fn adder_model() -> (
        Vec<crate::model::Clip>,
        std::sync::Arc<Vec<crate::model::Keyframe>>,
        std::sync::Arc<crate::model::Skeleton>,
    ) {
        use crate::model::{Clip, ClipFrame, Keyframe, Skeleton};
        let clips = (0..6)
            .map(|_| Clip {
                frames: (0..32)
                    .map(|_| ClipFrame {
                        keyframe: 0,
                        timing: 1,
                    })
                    .collect(),
            })
            .collect();
        let keyframes = vec![Keyframe {
            offset: [0, 0, 0],
            rotations: vec![[0, 0, 0], [0, 0, 0]],
        }];
        let skeleton = Skeleton {
            relative: vec![[0, 0, 0], [100, 0, 0]],
            children: vec![vec![1], vec![]],
        };
        (
            clips,
            std::sync::Arc::new(keyframes),
            std::sync::Arc::new(skeleton),
        )
    }

    /// One active adder in slot 1 with the given spawn kind.
    #[cfg(feature = "lua")]
    fn adder_game(kind: u8) -> GameState {
        let mut game = GameState::default();
        let entity = &mut game.entities[1];
        entity.id = 0x0A;
        entity.set_active(true);
        entity.status_flags = 1;
        entity.behavior_flags = kind;
        entity.death_event_id = 3;
        entity.pos = [1000, 0, 1000];
        game.enemy_count = 1;
        game
    }

    /// The adder rooms' sound-row fixture (stage 1 room 1), whose enemy column
    /// 0/1 names a cue.
    #[cfg(feature = "lua")]
    fn adder_room() -> RoomState {
        RoomState {
            stage: 1,
            room: 1,
            ..RoomState::default()
        }
    }

    #[cfg(feature = "lua")]
    fn adder_step(
        host: &mut LuaEnemyHost,
        game: &mut GameState,
        pack: &Pack,
        clips: &[crate::model::Clip],
    ) -> bool {
        host.update(game, &adder_room(), pack, 1, clips)
    }

    /// The checked-in wasp script, so the tests exercise the real port.
    #[cfg(feature = "lua")]
    fn wasp_source() -> &'static str {
        crate::enemy::ENEMY_SCRIPTS
            .iter()
            .find(|(path, _)| *path == "enemy/em07.lua")
            .expect("the checked-in wasp script")
            .1
    }

    /// The wasp's synthetic model: seven clips of 32 one-tick frames and a
    /// ten-joint skeleton, so the effect anchors, the wing joints and the
    /// counts the death paths use all exist.
    #[cfg(feature = "lua")]
    fn wasp_model() -> (
        Vec<crate::model::Clip>,
        std::sync::Arc<Vec<crate::model::Keyframe>>,
        std::sync::Arc<crate::model::Skeleton>,
    ) {
        use crate::model::{Clip, ClipFrame, Keyframe, Skeleton};
        let clips = (0..8)
            .map(|_| Clip {
                frames: (0..40)
                    .map(|_| ClipFrame {
                        keyframe: 0,
                        timing: 1,
                    })
                    .collect(),
            })
            .collect();
        let keyframes = vec![Keyframe {
            offset: [0, 0, 0],
            rotations: vec![[0, 0, 0]; 10],
        }];
        let skeleton = Skeleton {
            relative: vec![[0, 0, 0]; 10],
            children: (0..10)
                .map(|index| if index == 0 { vec![1] } else { vec![] })
                .collect(),
        };
        (
            clips,
            std::sync::Arc::new(keyframes),
            std::sync::Arc::new(skeleton),
        )
    }

    /// One active wasp in slot 1 with the given nest kind.
    #[cfg(feature = "lua")]
    fn wasp_game(kind: u8) -> GameState {
        use crate::effects::fixtures::{block, sprite};
        let mut game = GameState::default();
        game.weapon_effects.sprites.push(sprite(
            0,
            std::array::from_fn(|_| vec![vec![block(1, 0, 0)]]),
        ));
        let entity = &mut game.entities[1];
        entity.id = 0x07;
        entity.set_active(true);
        entity.status_flags = 1;
        entity.behavior_flags = kind;
        entity.death_event_id = 3;
        entity.pos = [1000, -4000, 1000];
        entity.saved_pos = Some(entity.pos);
        game.enemy_count = 1;
        game
    }

    #[cfg(feature = "lua")]
    fn wasp_step(
        host: &mut LuaEnemyHost,
        game: &mut GameState,
        pack: &Pack,
        clips: &[crate::model::Clip],
    ) -> bool {
        host.update(game, &adder_room(), pack, 1, clips)
    }

    /// The wasp's manhattan distance word for the current player position,
    /// the value the script's `refresh_distance` writes.
    #[cfg(feature = "lua")]
    fn wasp_distance(game: &GameState) -> u16 {
        let dx = (game.entities[0].pos[0] - game.entities[1].pos[0]).unsigned_abs() & 0xFFFF;
        let dz = (game.entities[0].pos[2] - game.entities[1].pos[2]).unsigned_abs() & 0xFFFF;
        dx.wrapping_add(dz) as u16
    }

    #[cfg(feature = "lua")]
    #[test]
    fn adder_initialises_for_every_spawn_kind() {
        let pack = pack(&[("enemy/em0a.lua", adder_source())]);
        let (clips, _, _) = adder_model();
        for kind in [0u8, 1, 2, 3] {
            let mut game = adder_game(kind);
            let mut host = LuaEnemyHost::new();
            game.rand_state = 1;
            let mut expected = 1u32;
            let _discarded = crate::game::platform_rand(&mut expected);
            let h0 = crate::game::platform_rand(&mut expected) & 7;
            let h1 = crate::game::platform_rand(&mut expected) & 7;
            let h2 = crate::game::platform_rand(&mut expected) & 7;
            let fuse = if kind != 0 {
                Some(crate::game::platform_rand(&mut expected) & 6)
            } else {
                None
            };

            assert!(
                adder_step(&mut host, &mut game, &pack, &clips),
                "kind {kind}"
            );
            let entity = &game.entities[1];
            assert_eq!(entity.state(), 1, "init moves to run");
            assert_eq!(
                (entity.ignore(), entity.action_behavior, entity.action_state),
                (0, 0, 0)
            );
            assert_eq!(
                entity.health,
                (h0 + h1 + h2) as i16 * 2 + 10,
                "the health roll is three low-bit draws after a discarded one"
            );
            assert_eq!(entity.status_flags, 1);
            assert_eq!(entity.sca_radius, 200);
            assert_eq!(entity.sca_half_height, 0);
            assert_eq!(entity.sca_offset, [0, 0, 0]);
            assert_eq!(entity.joint_scale, 0x3000);
            assert_eq!(entity.shadow_half_x, 0);
            assert_eq!(entity.shadow_half_z, 0);
            assert_eq!(entity.shadow_tint, 0x0020_2020);
            assert_eq!(entity.shadow_offset, [0, 0, 0]);
            assert_eq!(
                game.entity_anims[1].display_frame(),
                0,
                "the init clip ran once"
            );

            if kind == 0 {
                assert_eq!(entity.hit_state, 0);
                assert_eq!(entity.sca_active(), 1);
                assert_eq!((entity.pitch, entity.roll, entity.pos[1]), (0, 0, 0));
            } else {
                assert_eq!(entity.pitch, 0);
                assert_eq!(
                    entity.action_ticks_counter,
                    ((0x18 - fuse.unwrap()) * 10),
                    "the ambush fuse is a masked draw"
                );
                if kind == 3 {
                    assert_eq!(entity.hit_state, 1, "not yet emerged");
                    assert_eq!(entity.sca_active(), 0, "a hidden snake is intangible");
                    assert_eq!((entity.roll, entity.pos[1]), (0, 0));
                } else {
                    assert_eq!(entity.hit_state, 0);
                    assert_eq!(entity.sca_active(), 1);
                    assert_eq!(entity.roll, 0x800);
                    assert_eq!(entity.pos[1], -0x1FB8, "the ceiling");
                }
            }
            assert_eq!(game.rand_state, expected, "draw order, kind {kind}");
        }
    }

    #[cfg(feature = "lua")]
    #[test]
    fn adder_decide_tables_select_the_behaviour_in_order() {
        let pack = pack(&[("enemy/em0a.lua", adder_source())]);
        let (clips, keyframes, skeleton) = adder_model();
        let mut host = LuaEnemyHost::new();

        // Ground kind: close, aimed and path-clear -> the bite test overrides
        // the turn-away test in the same frame.
        let mut game = adder_game(0);
        game.id = crate::state::RoomId {
            stage: 1,
            room: 1,
            player_flag: 0,
        };
        game.entity_anims[1].keyframes = Some(keyframes.clone());
        game.entity_anims[1].skeleton = Some(skeleton.clone());
        game.rand_state = 7;
        game.entities[0].pos = [1500, 0, 1000];
        game.entities[1].angle = 0;
        game.entities[1].pathfind_state = 4;
        game.entities[1].attacking_direction = 1;
        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.player_distance(), 500);
        assert_eq!(entity.action_behavior, 3, "the bite takes priority");
        assert_eq!(entity.ignore(), 1);
        assert_eq!(entity.action_state, 1);
        assert_eq!(entity.animation_id, 3);
        assert_eq!(entity.move_speed_current, 0x32);

        // Ground kind, mid range: the clear path bit picks the turn-away.
        let mut game = adder_game(0);
        game.id = crate::state::RoomId {
            stage: 1,
            room: 1,
            player_flag: 0,
        };
        game.entities[0].pos = [6000, 0, 1000];
        game.entities[1].angle = 0;
        game.entities[1].pathfind_state = 4;
        let mut host = LuaEnemyHost::new();
        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.player_distance(), 5000);
        assert_eq!(entity.action_behavior, 2, "turn away, not chase");
        assert_eq!(entity.ignore(), 1);
        assert!(
            matches!(entity.animation_id, 1 | 2),
            "one of the coil clips"
        );

        // Ceiling near: under 3000 units selects the drop.
        let mut game = adder_game(1);
        game.entities[0].pos = [1000, 0, 1500];
        let mut host = LuaEnemyHost::new();
        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_behavior, 1, "drop");
        assert_eq!(game.entities[1].action_state, 1);
        assert_eq!(
            game.entities[1].behavior_flags, 0,
            "a dropping snake becomes a floor snake"
        );

        // Ceiling ambush: a burnt-out fuse teleports onto the player and
        // drops; two low-bit draws pick the +/-100 unit offset.
        let mut game = adder_game(2);
        game.entities[0].pos = [8000, 0, 8000];
        let mut host = LuaEnemyHost::new();
        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        game.entities[1].action_ticks_counter = 0;
        game.rand_state = 1;
        let mut expected = 1u32;
        let dx = crate::game::platform_rand(&mut expected) & 1;
        let dz = crate::game::platform_rand(&mut expected) & 1;
        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.action_behavior, 1, "the ambush drops");
        assert_eq!(entity.pos[0], 8000 + i32::from(dx) * 100);
        assert_eq!(entity.pos[2], 8000 + i32::from(dz) * 100);
        assert_eq!(entity.ignore(), 1);
        assert_eq!(game.rand_state, expected, "two draws, X then Z");

        // Hidden: under 4500 units selects the emerge, which clears the spawn
        // kind so the next decision is the ground one.
        let mut game = adder_game(3);
        game.entities[0].pos = [1000, 0, 1500];
        let mut host = LuaEnemyHost::new();
        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.action_behavior, 6, "emerge");
        assert_eq!(entity.behavior_flags, 0);
        assert_eq!(entity.move_speed_current, 0x1E);
        assert_eq!(
            entity.action_ticks_counter, 0x3B,
            "the setup frame immediately runs the countdown"
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn adder_chase_draws_its_setup_and_wobble_in_order() {
        let pack = pack(&[("enemy/em0a.lua", adder_source())]);
        let (clips, _, _) = adder_model();
        let mut game = adder_game(0);
        game.id = crate::state::RoomId {
            stage: 1,
            room: 1,
            player_flag: 0,
        };
        game.entities[0].pos = [3000, 0, 1000];
        let mut host = LuaEnemyHost::new();
        game.rand_state = 1;
        assert!(adder_step(&mut host, &mut game, &pack, &clips));

        // Force the chase: the behaviour owns the snake, the pathfinder keeps
        // counting and the distance word is stale-clear.
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 0;
        game.entities[1].pathfind_state = 4;
        game.entities[1].attacking_direction = 1;
        let angle = game.entities[1].angle;

        let mut expected = game.rand_state;
        let speed = 0x32 - (crate::game::platform_rand(&mut expected) & 0xF);
        let ticks = (crate::game::platform_rand(&mut expected) & 0xF) + 0x10;
        let wobble = (crate::game::platform_rand(&mut expected) & 0xF)
            + (crate::game::platform_rand(&mut expected) & 0xF);
        let turn = crate::enemy::walk::turn_toward_target(
            &game.entities[1],
            [3000, 0, 1000],
            ticks as i16,
        );

        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.move_speed_current, speed);
        assert_eq!(entity.action_ticks_counter, ticks);
        assert_eq!(
            entity.angle,
            angle.wrapping_add((turn + wobble as i16) as u16)
        );
        assert_eq!(
            game.rand_state, expected,
            "four draws: speed, ticks, wobble"
        );
    }

    /// The bite applies its damage, poison and effect through the shared
    /// player helpers when the reach box, the aim gate and the path bit agree.
    #[cfg(feature = "lua")]
    #[test]
    fn adder_bite_hits_the_player_through_the_hit_frame() {
        use crate::effects::fixtures::{block, sprite};
        let pack = pack(&[("enemy/em0a.lua", adder_source())]);
        let (clips, keyframes, skeleton) = adder_model();
        let mut game = adder_game(0);
        game.weapon_effects.sprites.push(sprite(
            0,
            std::array::from_fn(|_| vec![vec![block(9, 0, 0)]]),
        ));
        game.id = crate::state::RoomId {
            stage: 1,
            room: 1,
            player_flag: 0,
        };
        game.entity_anims[1].keyframes = Some(keyframes);
        game.entity_anims[1].skeleton = Some(skeleton);
        game.entities[1].pos = [1000, 0, 1000];
        let mut host = LuaEnemyHost::new();
        game.rand_state = 1;
        assert!(adder_step(&mut host, &mut game, &pack, &clips));

        // Force the bite and keep the path bit latched.
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 3;
        game.entities[1].action_state = 0;
        game.entities[1].pathfind_state = 4;
        game.entities[1].attacking_direction = 1;
        game.entities[0].health = 100;
        assert!(adder_step(&mut host, &mut game, &pack, &clips));

        // Put the player on joint 1's world position and aim the snake at it;
        // the square reach box is then dead centre. Zero the speed so the
        // snake stays aimed while the strike clip advances.
        let joint = game.joint_worlds[1][1].t;
        game.entities[0].pos = [joint[0], 0, joint[2]];
        game.entities[0].action_behavior = 0x14;
        game.entities[1].angle = crate::sfx::angle_between_xz(
            game.entities[1].pos[0],
            game.entities[1].pos[2],
            joint[0],
            joint[2],
        );
        game.entities[1].move_speed_current = 0;
        game.entities[0].is_being_attacked = 0;

        // The poison coin is the first two draws on the hit frame; pick a
        // seed that lands both.
        let lucky = (1u32..)
            .find(|&seed| {
                let mut state = seed;
                crate::game::platform_rand(&mut state) & 1 == 1
                    && crate::game::platform_rand(&mut state) & 1 == 1
            })
            .unwrap();
        game.rand_state = lucky;

        // The bite hits on the frame whose animation word is 0xB. The path
        // result is re-latched each tick so the case is about the bite.
        let mut hit = false;
        for _ in 0..16 {
            game.entities[1].attacking_direction = 1;
            game.entities[1].pathfind_state = 4;
            assert!(adder_step(&mut host, &mut game, &pack, &clips));
            if game.entities[0].health != 100 {
                hit = true;
                break;
            }
        }
        assert!(hit, "the reach box landed within the clip window");
        assert_eq!(game.entities[0].health, 94, "six damage");
        assert_eq!(game.health_status & 0x02, 0x02, "poisoned");
        assert_eq!(game.poison_timer, crate::combat::POISON_TIMER);
        assert_eq!(game.entities[0].is_being_attacked, 1);
        assert_eq!(
            game.entities[0].action_behavior, 100,
            "only the behavior byte is overridden"
        );
        assert_eq!(
            game.entities[1].action_behavior, 3,
            "the bite keeps running"
        );

        // Snd_em(1) plus Play3DSnd(3, 0) at the player's position.
        assert!(
            game.entity_sounds
                .iter()
                .any(|sound| sound.name == "Chris01" && sound.pos == game.entities[0].pos),
            "the bite cue plays at the player"
        );
        // The bite billboard is attached to the player at (0, -520, 0).
        assert!(
            game.effects
                .active()
                .any(|(_, effect)| effect.attach == crate::effects::Attach::Player),
            "the bite billboard anchors to the player"
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn adder_flinch_hides_the_tail_and_special_weapons_clear_the_latch() {
        let pack = pack(&[("enemy/em0a.lua", adder_source())]);
        let (clips, _, _) = adder_model();
        let mut game = adder_game(0);
        let mut host = LuaEnemyHost::new();
        game.rand_state = 1;
        assert!(adder_step(&mut host, &mut game, &pack, &clips));

        // State 2 with behaviour 0: the flinch setup hides joints 8..11.
        game.entities[1].set_state(2);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 0;
        game.entities[1].hit_state = 0x10;
        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.animation_id, 3);
        assert_eq!(entity.joint_flags, 0x0F00, "joints 8..11 hidden");
        assert_eq!(entity.action_state, 1);

        // The special-weapon check clears the latch on a five-frame boundary
        // once a high special weapon is equipped.
        game.equipped = Some(0x6F);
        game.entities[1].hit_state = 0x18;
        let mut cleared = false;
        for _ in 0..128 {
            assert!(adder_step(&mut host, &mut game, &pack, &clips));
            if game.entities[1].hit_state == 0 {
                cleared = true;
                break;
            }
        }
        assert!(cleared, "the high weapon cleared the latch");

        // The flinch's countdown returns to the run state.
        game.equipped = None;
        for _ in 0..64 {
            assert!(adder_step(&mut host, &mut game, &pack, &clips));
            if game.entities[1].state() == 1 {
                break;
            }
        }
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1);
        assert_eq!(
            (entity.ignore(), entity.action_behavior, entity.action_state),
            (0, 0, 0)
        );
        assert_eq!(entity.hit_state, 0);
    }

    /// The knife death writhe: the blood pool spreads and the branch ends in
    /// behaviour 4 with the spawn record's death flag raised.
    #[cfg(feature = "lua")]
    #[test]
    fn adder_knife_death_writhes_and_raises_the_event() {
        let pack = pack(&[("enemy/em0a.lua", adder_source())]);
        let (clips, keyframes, skeleton) = adder_model();
        let mut game = adder_game(0);
        game.entity_anims[1].keyframes = Some(keyframes);
        game.entity_anims[1].skeleton = Some(skeleton);
        let mut host = LuaEnemyHost::new();
        game.rand_state = 1;
        assert!(adder_step(&mut host, &mut game, &pack, &clips));

        game.entities[1].set_state(3);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 0;
        game.entities[1].hit_state = 0x08; // the knife
        game.entities[1].health = -1;
        let ticks = {
            let mut state = game.rand_state;
            (crate::game::platform_rand(&mut state) & 0xF) + 0x10
        };
        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.animation_id, 4);
        assert_eq!(
            entity.animation_frame_id, 0x1F,
            "the setup frame is applied and the word moves to the pending frame"
        );
        assert_eq!(entity.action_state, 1);
        assert_eq!(entity.move_speed_current, 0x28);
        assert_eq!(entity.action_ticks_counter, ticks);
        assert_eq!(entity.status_flags & 0x0A, 0x0A, "intangible and dead");

        // The next advance consumes the last clip frame: the shadow becomes
        // the blood pool and the branch moves to its countdown.
        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.action_state, 2);
        assert_eq!(entity.shadow_tint, 0x00FF_FF50);
        assert_eq!((entity.shadow_half_x, entity.shadow_half_z), (0x14, 10));

        // The countdown then ends the branch and raises the event flag.
        for _ in 0..64 {
            assert!(adder_step(&mut host, &mut game, &pack, &clips));
            if game.entities[1].action_behavior == 4 {
                break;
            }
        }
        assert_eq!(game.entities[1].action_behavior, 4);
        assert!(game.flags[crate::game::BANK_ENEMIES as usize].bit(3));

        // Behaviour 4 parks the corpse: the state holds still from here.
        let latched = game.entities[1];
        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1], latched);
    }

    /// The firearm death retreat: the writhe setup forks into behaviour 3,
    /// which hides the joints, parks the snake out of sight and respawns it
    /// with a ceiling kind - spawn kind 1 in the water-gate room, 2
    /// everywhere else. The retreat's own case-0 setup (the blood sprays) is
    /// only reached when behaviour 3 is entered with a clear sub-state, so it
    /// is exercised separately.
    #[cfg(feature = "lua")]
    #[test]
    fn adder_firearm_death_retreats_and_respawns_by_room() {
        use crate::effects::fixtures::{block, sprite};
        let pack = pack(&[("enemy/em0a.lua", adder_source())]);
        let (clips, keyframes, skeleton) = adder_model();
        for (room_number, expected_kind) in [(0u8, 2u8), (1, 1)] {
            let mut game = adder_game(1);
            game.weapon_effects.sprites.push(sprite(
                0,
                std::array::from_fn(|_| vec![vec![block(9, 0, 0)]]),
            ));
            game.id = crate::state::RoomId {
                stage: 1,
                room: room_number,
                player_flag: 0,
            };
            game.entity_anims[1].keyframes = Some(keyframes.clone());
            game.entity_anims[1].skeleton = Some(skeleton.clone());
            let mut host = LuaEnemyHost::new();
            game.rand_state = 1;
            assert!(adder_step(&mut host, &mut game, &pack, &clips));

            game.entities[1].set_state(3);
            game.entities[1].action_behavior = 0;
            game.entities[1].action_state = 0;
            game.entities[1].hit_state = 0x10; // the handgun
            game.entities[1].health = -1;
            game.entities[0].angle = 0x200;

            // The first call runs the writhe setup, whose weapon fork hands
            // over to the retreat without its own setup.
            assert!(adder_step(&mut host, &mut game, &pack, &clips));
            let entity = &game.entities[1];
            assert_eq!(entity.action_behavior, 3, "the retreat, not the writhe");
            assert_eq!((entity.shadow_half_x, entity.shadow_half_z), (0, 0));
            assert_eq!(entity.animation_frame_id, 0x1E, "the writhe setup pose");
            assert_eq!(entity.action_state, 1);
            assert_eq!(game.effects.active_count(), 0, "the fork spawns no spray");

            // The next call runs the retreat at sub-state 1: the first
            // advance of the writhe pose.
            assert!(adder_step(&mut host, &mut game, &pack, &clips));
            assert_eq!(
                (
                    game.entities[1].animation_id,
                    game.entities[1].animation_frame_id
                ),
                (4, 0x1F)
            );

            // Advance to the hide substate: all eleven joints vanish and the
            // tilt winds down.
            for _ in 0..40 {
                assert!(adder_step(&mut host, &mut game, &pack, &clips));
                if game.entities[1].action_state >= 3 {
                    break;
                }
            }
            assert!(game.entities[1].action_state >= 3);
            assert_eq!(game.entities[1].joint_flags & 0x07FF, 0x07FF);
            assert!(game.entities[1].joint_scale < 0x3000, "the tilt winds down");

            // The next tick parks it out of sight and starts the cooldown;
            // forcing the counter to zero then respawns it.
            for _ in 0..4 {
                assert!(adder_step(&mut host, &mut game, &pack, &clips));
                if game.entities[1].action_state >= 4 {
                    break;
                }
            }
            assert!(game.entities[1].action_state >= 4);
            game.entities[1].action_ticks_counter = 0;
            assert!(adder_step(&mut host, &mut game, &pack, &clips));
            let entity = &game.entities[1];
            assert_eq!(entity.state(), 0, "reset to run init again");
            assert_eq!(
                entity.behavior_flags, expected_kind,
                "room {room_number} respawns as kind {expected_kind}"
            );
            assert_eq!(entity.joint_flags & 0x07FF, 0, "the joints are shown again");
            assert_eq!(entity.pos[1], -20000);

            // The reset runs init on the next tick: fresh health and the
            // ceiling pose for the non-zero kind.
            assert!(adder_step(&mut host, &mut game, &pack, &clips));
            let entity = &game.entities[1];
            assert_eq!(entity.state(), 1);
            assert!(entity.health >= 10 && entity.health <= 52);
            assert_eq!(entity.roll, if expected_kind == 3 { 0 } else { 0x800 });
        }
    }

    /// The retreat's case-0 setup, reached only when behaviour 3 is entered
    /// with a clear sub-state: animation 4 rebased to frame 0, and the two
    /// blood sprays anchored at the head and joint 5.
    #[cfg(feature = "lua")]
    #[test]
    fn adder_retreat_setup_sprays_blood_at_the_two_joints() {
        use crate::effects::fixtures::{block, sprite};
        let pack = pack(&[("enemy/em0a.lua", adder_source())]);
        let (clips, keyframes, skeleton) = adder_model();
        let mut game = adder_game(1);
        game.weapon_effects.sprites.push(sprite(
            0,
            std::array::from_fn(|_| vec![vec![block(9, 0, 0)]]),
        ));
        game.id = crate::state::RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        game.entity_anims[1].keyframes = Some(keyframes);
        game.entity_anims[1].skeleton = Some(skeleton);
        let mut host = LuaEnemyHost::new();
        game.rand_state = 1;
        assert!(adder_step(&mut host, &mut game, &pack, &clips));

        game.entities[1].set_state(3);
        game.entities[1].action_behavior = 3;
        game.entities[1].action_state = 0;
        game.entities[0].angle = 0x200;
        let ticks = {
            let mut state = game.rand_state;
            (crate::game::platform_rand(&mut state) & 0x1F) + 0x10
        };

        assert!(adder_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.action_state, 1);
        assert_eq!(entity.animation_id, 4);
        assert_eq!(entity.animation_frame_id, 1, "the setup falls into case 1");
        assert_eq!(entity.action_ticks_counter, ticks);
        assert_eq!(
            game.effects
                .active()
                .filter(|(_, effect)| effect.attach == crate::effects::Attach::Joint(1, 0))
                .count(),
            1,
            "a blood spray at the head"
        );
        assert_eq!(
            game.effects
                .active()
                .filter(|(_, effect)| effect.attach == crate::effects::Attach::Joint(1, 5))
                .count(),
            1,
            "a blood spray at joint 5"
        );
    }

    /// A multi-tick scene spanning init, the ambush drop, a flinch and the
    /// death retreat must be byte-identical with and without a VM reset
    /// between every tick.
    #[cfg(feature = "lua")]
    #[test]
    fn adder_reset_between_frames_preserves_the_scene() {
        let pack = pack(&[("enemy/em0a.lua", adder_source())]);
        let (clips, keyframes, skeleton) = adder_model();
        let room = adder_room();
        let mut cached_game = adder_game(2);
        let mut reset_game = adder_game(2);
        for game in [&mut cached_game, &mut reset_game] {
            game.id = crate::state::RoomId {
                stage: 1,
                room: 1,
                player_flag: 0,
            };
            game.entity_anims[1].keyframes = Some(keyframes.clone());
            game.entity_anims[1].skeleton = Some(skeleton.clone());
            game.entities[0].pos = [2000, 0, 1000];
            game.entities[0].action_behavior = 0x14;
            game.entities[0].action_state = 0;
            game.rand_state = 0x1234;
        }
        let mut cached = LuaEnemyHost::new();
        let mut reset = LuaEnemyHost::new();

        // The scene: init, the ambush drop and landing, a run frame, a flinch
        // from a knife hit, back to run, then the knife death writhe.
        let scenario: &[(u8, u8, u8)] = &[
            (0, 0, 0),    // init (state 0)
            (1, 0, 0),    // decide: ambush drop
            (1, 0, 0),    // fall
            (1, 0, 0),    // fall, land
            (1, 0, 0),    // hiss, become a floor snake
            (2, 0, 0x10), // flinch setup
            (2, 0, 0x10), // flinch advance
            (1, 0, 0),    // back to the run
            (3, 0, 0x08), // knife death writhe
            (3, 0, 0x08),
            (3, 0, 0x08),
            (3, 0, 0x08),
        ];
        for (tick, &(state, behavior, hit_state)) in scenario.iter().enumerate() {
            for game in [&mut cached_game, &mut reset_game] {
                game.entities[1].set_state(state);
                game.entities[1].action_behavior = behavior;
                if hit_state != 0 {
                    game.entities[1].hit_state = hit_state;
                }
            }
            assert!(cached.update(&mut cached_game, &room, &pack, 1, &clips));
            reset.reset();
            assert!(reset.update(&mut reset_game, &room, &pack, 1, &clips));
            assert_eq!(
                cached_game, reset_game,
                "the scene diverged at tick {tick} (state {state})"
            );
        }

        // The knife writhe needs its whole clip and countdown to raise the
        // flag; keep both games on the branch and compare every tick.
        let mut raised = false;
        for tick in 0..64 {
            for game in [&mut cached_game, &mut reset_game] {
                game.entities[1].set_state(3);
                game.entities[1].action_behavior = 0;
                game.entities[1].hit_state = 0x08;
            }
            assert!(cached.update(&mut cached_game, &room, &pack, 1, &clips));
            reset.reset();
            assert!(reset.update(&mut reset_game, &room, &pack, 1, &clips));
            assert_eq!(
                cached_game, reset_game,
                "the writhe scene diverged at tick {tick}"
            );
            if cached_game.flags[crate::game::BANK_ENEMIES as usize].bit(3) {
                raised = true;
                break;
            }
        }
        assert!(raised, "the writhe death raised the event flag");
    }

    /// The wasp init: every nest kind, the big-flag consumption, the dwell
    /// roll and the four-draw health roll, in exact draw order.
    #[cfg(feature = "lua")]
    #[test]
    fn wasp_initialises_for_every_nest_kind() {
        let pack = pack(&[("enemy/em07.lua", wasp_source())]);
        let (clips, _, _) = wasp_model();
        for kind in [0x00u8, 0x02, 0x10, 0x11, 0x12, 0x20, 0x22] {
            let mut game = wasp_game(kind);
            let mut host = LuaEnemyHost::new();
            game.rand_state = 1;
            game.entities[1].pos[1] = 0;
            let mut expected = 1u32;
            let big = kind & 2 != 0;
            let effective = if big { kind - 2 } else { kind };
            let dwell_draw = if effective == 0x10 {
                Some(crate::game::platform_rand(&mut expected) & 9)
            } else {
                None
            };
            crate::game::platform_rand(&mut expected);
            let ra = crate::game::platform_rand(&mut expected) & 7;
            let rb = crate::game::platform_rand(&mut expected) & 7;
            let rc = crate::game::platform_rand(&mut expected) & 7;
            let health = (ra + rb + rc) as i16 * 2 + (i16::from(big) + 1) * 20;
            let dwell = dwell_draw.map_or(0, |draw| draw as i16 * -30 + 800);

            assert!(
                wasp_step(&mut host, &mut game, &pack, &clips),
                "kind {kind:#04x}"
            );
            let entity = &game.entities[1];
            assert_eq!(entity.state(), 1, "kind {kind:#04x}");
            assert_eq!(entity.ignore(), 1);
            assert_eq!((entity.action_behavior, entity.action_state), (0, 0));
            assert_eq!(entity.health, health, "kind {kind:#04x}");
            assert_eq!(entity.behavior_flags, effective, "the big bit is consumed");
            assert_eq!(entity.wasp_big, u8::from(big));
            assert_eq!(entity.joint_scale, if big { 0x2000 } else { 0 });
            assert_eq!(entity.pos[1], -4000, "the default cruise altitude");
            assert_eq!(entity.animation_frame_id, 0);
            assert_eq!(entity.death_timer, 0);
            assert_eq!(entity.hit_state, 0);
            assert_eq!(entity.status_flags, 1);
            assert_eq!(entity.tint_flashes(), 0, "touch");
            assert_eq!(entity.writhe_velocity(), 0, "drift");
            assert_eq!(entity.sink_wobble(), 0x5A, "timer");
            assert_eq!(entity.action_ticks_counter, dwell as u16, "dwell");
            assert_eq!(entity.wasp_sound_latch, 0);
            assert_eq!(entity.sca_radius, 0);
            assert_eq!(entity.sca_half_height, 0);
            assert_eq!(entity.sca_offset, [0, 0, 0]);
            assert_eq!(entity.shadow_half_x, if big { 200 } else { 100 });
            assert_eq!(entity.shadow_half_z, if big { 200 } else { 100 });
            assert_eq!(entity.shadow_tint, 0x0060_6060);
            assert_eq!(entity.shadow_offset, [0, 0, 0]);
            assert_eq!(game.rand_state, expected, "draw order, kind {kind:#04x}");
        }
    }

    /// The nest state machine: the dormant dwell, the early arm, the emerge,
    /// the roused short-circuit and both loose-wasp wake paths.
    #[cfg(feature = "lua")]
    #[test]
    fn wasp_nest_state_machine_arms_emerges_and_wakes() {
        let pack = pack(&[("enemy/em07.lua", wasp_source())]);
        let (clips, _, _) = wasp_model();

        // A dormant nest counts its dwell down and arms at the pre-decrement
        // zero.
        let mut game = wasp_game(0x10);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        game.entities[1].action_ticks_counter = 1;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].behavior_flags, 0x11);
        assert_eq!(game.entities[1].action_ticks_counter, 0);

        // A dormant nest arms early when the player is within 4000 units.
        let mut game = wasp_game(0x10);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        game.entities[1].action_ticks_counter = 0x30;
        game.entities[0].pos = [1500, 0, 1000];
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].behavior_flags, 0x11);

        // Kind 0x11 emerges: behaviour 6, speed 0x1E, intangible.
        let mut game = wasp_game(0x11);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.action_behavior, 6);
        assert_eq!(entity.move_speed_current, 0x1E);
        assert_eq!(entity.status_flags & 0x02, 0x02);

        // Kind > 0x1F is already roused: straight to a full hover.
        let mut game = wasp_game(0x20);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.action_behavior, 2);
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.reaction_timer, 0x23, "bob");
        assert_eq!(entity.groan_timer(), 1, "climb");
        assert_eq!(entity.hit_state, 0);

        // A loose wasp (kind 0) waits beyond 5000 units, then takes off and
        // latches bit 5 so it never nests again.
        let mut game = wasp_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        game.entities[0].pos = [6500, 0, 1000];
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_behavior, 0, "still nesting");
        assert_eq!(game.entities[1].hit_state, 1, "still intangible");
        game.entities[0].pos = [4500, 0, 1000];
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.action_behavior, 1, "takeoff");
        assert_eq!(entity.hit_state, 0);
        assert_eq!(entity.move_speed_current, 0x32);
        assert_eq!(entity.sink_wobble(), 0x5A);
        assert_eq!(entity.behavior_flags & 0x20, 0x20, "roused latch");

        // A kind-1 loose wasp reads the timer before decrementing: a zero
        // timer parks the wake for exactly one frame.
        let mut game = wasp_game(1);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        game.entities[1].set_sink_wobble(0);
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_behavior, 0, "the zero timer parks");
        assert_eq!(game.entities[1].sink_wobble(), -1, "the store wraps");
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_behavior, 1, "then it wakes");
    }

    /// Hover: the wing blink, the bob ramp, the two altitude draws in order
    /// and the un-touched return before the attack gate.
    #[cfg(feature = "lua")]
    #[test]
    fn wasp_hover_blinks_the_wings_and_flips_altitude_in_draw_order() {
        let pack = pack(&[("enemy/em07.lua", wasp_source())]);
        let (clips, _, _) = wasp_model();
        let mut game = wasp_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));

        // Park a hovering wasp on the wing frame with a clear touch word.
        game.entities[1].action_behavior = 2;
        game.entities[1].action_state = 1;
        game.entities[1].animation_frame_id = 6;
        game.entities[1].timing_control = 0;
        game.entities[1].pos[1] = -3000;
        game.entities[1].reaction_timer = 0x23;
        game.entities[1].set_groan_timer(1);
        game.entities[1].set_tint_flashes(0);
        game.entities[0].pos = [1200, 0, 1000];
        game.rand_state = 1;
        let mut expected = 1u32;
        let _floor = crate::game::platform_rand(&mut expected) & 1;
        let _ceiling = crate::game::platform_rand(&mut expected) & 1;
        let expected_sound = crate::sfx::enemy_sound(&adder_room(), 0, 0).is_some();

        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(game.rand_state, expected, "floor then ceiling");
        assert_eq!(entity.reaction_timer, 0x24, "the bob ramps");
        assert_eq!(entity.groan_timer(), 1, "neither flip fires at -3000");
        assert_eq!(entity.pos[1], -3000 - 0x24, "the bob rides the climb sign");
        assert_eq!(entity.animation_frame_id, 7);
        assert_eq!(
            entity.joint_flags & 0b1_1110,
            0,
            "the wings show on every seventh frame"
        );
        assert_eq!(game.entity_sounds.len(), usize::from(expected_sound));

        // A frame that is not a seventh leaves the wings blanked.
        game.entities[1].animation_frame_id = 7;
        game.entities[1].timing_control = 0;
        game.entities[1].set_tint_flashes(0);
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].joint_flags & 0b1_1110, 0b1_1110);
    }

    /// Victory and takeoff: the path-gated steering, the two altitude draws
    /// with the bob reset, and the takeoff spiral's speed/offset arithmetic.
    #[cfg(feature = "lua")]
    #[test]
    fn wasp_victory_and_takeoff_steer_and_draw_in_order() {
        let pack = pack(&[("enemy/em07.lua", wasp_source())]);
        let (clips, _, _) = wasp_model();
        let mut game = wasp_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));

        // Victory with the player off-axis: the path bit gates the turn.
        game.entities[0].pos = [1000, 0, 5000];
        game.entities[1].set_state(1);
        game.entities[1].action_behavior = 4;
        game.entities[1].action_state = 1;
        game.entities[1].angle = 0;
        game.entities[1].tex_bank = 1;
        game.entities[1].pos[1] = -3000;
        game.entities[1].reaction_timer = 0x23;
        game.entities[1].move_speed_current = 0x40;
        game.rand_state = 7;
        let mut expected = 7u32;
        let _floor = crate::game::platform_rand(&mut expected) & 1;
        let _ceiling = crate::game::platform_rand(&mut expected) & 1;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(game.rand_state, expected, "floor then ceiling");
        assert_eq!(entity.angle, 0xFFE0, "the path-gated doubled turn");
        assert_eq!(entity.reaction_timer, 0x24);
        assert_eq!(entity.groan_timer(), 0, "the default climb sinks");
        let (dx, dz) = crate::player::rotate_speed(0xFFE0, 0, 0x40);
        assert_eq!(
            entity.pos,
            [1000 + dx, -3000 + 0x24, 1000 + dz],
            "the far wasp closes in straight ahead"
        );

        // With no path bit the wasp holds its heading.
        game.entities[1].action_state = 1;
        game.entities[1].angle = 0;
        game.entities[1].tex_bank = 0;
        game.entities[1].set_tint_flashes(0);
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].angle, 0);

        // Takeoff: speed +1, an 85-unit yaw step, the decaying forward offset
        // and the hover hand-off on frame 0x18.
        game.entities[1].set_state(1);
        game.entities[1].action_behavior = 1;
        game.entities[1].action_state = 1;
        game.entities[1].animation_frame_id = 0;
        game.entities[1].timing_control = 0;
        game.entities[1].angle = 0;
        game.entities[1].move_speed_current = 0x32;
        game.entities[1].pos = [0, -4000, 0];
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.move_speed_current, 0x33);
        assert_eq!(entity.angle, 0x55);
        assert_eq!(entity.animation_frame_id, 1);
        let (dx, dz) = crate::player::rotate_speed(0x55, (0x800 - 0x55) as u16, 0x33);
        assert_eq!(entity.pos, [dx, -4000, dz]);

        game.entities[1].animation_frame_id = 0x17;
        game.entities[1].timing_control = 0;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.animation_frame_id, 0x18);
        assert_eq!(entity.action_behavior, 2, "the hover hand-off");
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.reaction_timer, 0x23);
        assert_eq!(entity.groan_timer(), 1);
    }

    /// The hover attack gate: the contact sting commit, and the normal-wasp
    /// pin-and-sting pick when the player is not facing the wasp.
    #[cfg(feature = "lua")]
    #[test]
    fn wasp_hover_commits_to_the_sting_or_the_grab() {
        let pack = pack(&[("enemy/em07.lua", wasp_source())]);
        let (clips, _, _) = wasp_model();

        // Touch, close, aimed, in the band, path clear, not attacked: the
        // normal pick is the contact sting.
        let mut game = wasp_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        game.entities[0].pos = [1500, 0, 1000];
        game.entities[1].set_state(1);
        game.entities[1].action_behavior = 2;
        game.entities[1].action_state = 1;
        game.entities[1].pos[1] = -2600;
        game.entities[1].angle = 0;
        game.entities[1].set_tint_flashes(1);
        game.entities[1].tex_bank = 1;
        game.entities[1].set_wasp_distance(wasp_distance(&game));
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.action_behavior, 3, "the contact sting");
        assert_eq!(entity.ignore(), 1);
        assert_eq!(entity.writhe_velocity(), 0, "the drift resets first");

        // The player facing the wasp blocks the grab; caught from behind it
        // pins instead, latching the face and the attacked flag.
        game.entities[1].action_behavior = 2;
        game.entities[1].action_state = 1;
        game.entities[1].set_ignore(0);
        game.entities[1].set_tint_flashes(1);
        game.entities[0].is_being_attacked = 0;
        game.entities[0].angle = 0x800; // looking away from the wasp
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.action_behavior, 5, "the pin-and-sting");
        assert_eq!(entity.angle, 0, "facing the player dead-on");
        assert_eq!(game.entities[0].is_being_attacked, 1);
        assert_eq!(entity.hit_state, 1);

        // A big wasp can never grab: the same catch becomes a sting.
        game.entities[1].action_behavior = 2;
        game.entities[1].action_state = 1;
        game.entities[1].set_ignore(0);
        game.entities[1].wasp_big = 1;
        game.entities[1].set_tint_flashes(1);
        game.entities[0].is_being_attacked = 0;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_behavior, 3);
    }

    /// The contact sting: first-run and second-playthrough damage, the
    /// attacked latch and behavior 100, and the big wasp's scripted kill.
    #[cfg(feature = "lua")]
    #[test]
    fn wasp_sting_damages_by_playthrough_and_big_kills_scriptedly() {
        let pack = pack(&[("enemy/em07.lua", wasp_source())]);
        let (clips, _, _) = wasp_model();

        let mut game = wasp_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        game.entities[0].pos = [1500, 0, 1000];
        game.entities[1].set_state(1);
        game.entities[1].action_behavior = 3;
        game.entities[1].action_state = 1;
        game.entities[1].set_ignore(1);
        game.entities[1].animation_frame_id = 39;
        game.entities[1].timing_control = 0;
        game.entities[1].angle = 0;
        game.entities[1].tex_bank = 1;
        game.entities[0].health = 100;
        game.entities[0].is_being_attacked = 0;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[0].health, 96, "four damage on the first run");
        assert_eq!(game.entities[0].action_behavior, 100);
        assert_eq!(game.entities[0].is_being_attacked, 1);
        assert_eq!(game.entities[1].action_behavior, 1, "back to takeoff");
        assert_eq!(game.entities[1].move_speed_current, 100);
        assert_eq!(game.entities[1].ignore(), 1);

        // The second playthrough's flag raises the sting to ten.
        game.flags[usize::from(crate::game::BANK_SCENARIO)]
            .apply(crate::game::SCENARIO_FLAG_SECOND_PLAYTHROUGH, 0);
        game.entities[0].health = 100;
        game.entities[0].is_being_attacked = 0;
        game.entities[1].action_behavior = 3;
        game.entities[1].action_state = 1;
        game.entities[1].animation_frame_id = 39;
        game.entities[1].timing_control = 0;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[0].health, 90, "ten on the second run");

        // A big wasp's killing sting clamps the player to 1 and then drives
        // the scripted death itself, skipping the recovery writes.
        game.entities[1].wasp_big = 1;
        game.entities[0].health = 2;
        game.entities[0].is_being_attacked = 0;
        game.entities[1].action_behavior = 3;
        game.entities[1].action_state = 1;
        game.entities[1].animation_frame_id = 39;
        game.entities[1].timing_control = 0;
        game.entities[1].move_speed_current = 0x123;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let player = &game.entities[0];
        assert_eq!(player.health, -1);
        assert_eq!((player.state(), player.ignore()), (3, 0));
        assert_eq!((player.action_behavior, player.action_state), (200, 0));
        assert_eq!(
            game.entities[1].action_behavior, 3,
            "the recovery is skipped"
        );
        assert_eq!(game.entities[1].move_speed_current, 0x123);
    }

    /// The pin-and-sting: the setup latch, the per-character root motion and
    /// hit frame, the sting cue and the payout.
    #[cfg(feature = "lua")]
    #[test]
    fn wasp_grab_latches_the_pose_and_pays_out() {
        let pack = pack(&[("enemy/em07.lua", wasp_source())]);
        let (clips, keyframes, skeleton) = wasp_model();
        let mut game = wasp_game(0);
        game.entity_anims[1].keyframes = Some(keyframes);
        game.entity_anims[1].skeleton = Some(skeleton);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));

        game.entities[0].pos = [1234, 0, 5678];
        game.entities[0].angle = 0x234;
        game.entities[1].set_state(1);
        game.entities[1].action_behavior = 5;
        game.entities[1].action_state = 0;
        game.entities[1].set_ignore(1);
        game.entities[1].pos[1] = -2600;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let wasp = &game.entities[1];
        assert_eq!(wasp.action_state, 1);
        assert_eq!(wasp.angle, 0x234, "the player's facing");
        assert_eq!(wasp.pos[1], 0, "the floor");
        assert_eq!(wasp.status_flags & 0x02, 0x02, "intangible");
        assert_eq!(wasp.animation_id, 4, "Chris' grab clip");
        assert_eq!(wasp.death_timer, 1, "the first frame falls through");
        assert_eq!(wasp.pos[0], 1234, "the root-motion snap to the player");
        assert_eq!(wasp.pos[2], 5678);
        assert_eq!(wasp.unk_c6, 1234);
        assert_eq!(wasp.unk_c8, 5678);
        let player = &game.entities[0];
        assert_eq!(player.unk_c6, 1234);
        assert_eq!(player.unk_c8, 5678);
        assert_eq!(player.is_being_attacked, 1);
        // The grab writes animationId 5 / animFrameId 7 (the state-5 window's
        // crawl pin), not the clip id and frame.
        assert_eq!((player.state(), player.ignore()), (5, 7));
        assert_eq!((player.action_behavior, player.action_state), (0, 0));

        // Frame 4 queues the character-bank sting cue at the player.
        game.entities[1].animation_frame_id = 3;
        game.entities[1].timing_control = 0;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].animation_frame_id, 4);
        assert!(
            game.entity_sounds
                .iter()
                .any(|sound| sound.bank == 3 && sound.pos == game.entities[0].pos),
            "the sting cue plays at the player"
        );

        // Chris' hit frame is 0x22: the sub-state moves to the payout.
        game.entities[1].animation_frame_id = 0x21;
        game.entities[1].timing_control = 0;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_state, 2);

        // The payout: the wasp dies of its own sting, the coin poisons and
        // the first-run damage is eight.
        game.entities[0].health = 100;
        game.rand_state = (1u32..)
            .find(|&seed| {
                let mut state = seed;
                crate::game::platform_rand(&mut state) & 1 == 1
            })
            .unwrap();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let wasp = &game.entities[1];
        assert_eq!(wasp.health, -44);
        assert_eq!(wasp.state(), 3);
        assert_eq!((wasp.action_behavior, wasp.action_state), (0, 0));
        assert_eq!(wasp.ignore(), 0);
        assert_eq!(game.entities[0].health, 92, "eight damage");
        assert_eq!(game.health_status & 0x02, 0x02, "poisoned");
        assert_eq!(game.poison_timer, crate::combat::POISON_TIMER);

        // Jill gets the other clip and hit frame.
        let mut game = wasp_game(0);
        game.id.player_flag = 1;
        game.entity_anims[1].keyframes = Some(std::sync::Arc::new(vec![crate::model::Keyframe {
            offset: [0, 0, 0],
            rotations: vec![[0, 0, 0]; 10],
        }]));
        game.entity_anims[1].skeleton = Some(std::sync::Arc::new(crate::model::Skeleton {
            relative: vec![[0, 0, 0]; 10],
            children: vec![vec![]; 10],
        }));
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        game.entities[0].pos = [500, 0, 0];
        game.entities[1].set_state(1);
        game.entities[1].action_behavior = 5;
        game.entities[1].action_state = 0;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].animation_id, 5, "Jill's grab clip");
        game.entities[1].animation_frame_id = 0x23;
        game.entities[1].timing_control = 0;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_state, 2, "Jill's hit frame");
    }

    /// The knockdown: the drop, the landing tint, the growing shadow and the
    /// crush that drops a limp wasp into the death state.
    #[cfg(feature = "lua")]
    #[test]
    fn wasp_knockdown_falls_lands_and_is_crushed() {
        let pack = pack(&[("enemy/em07.lua", wasp_source())]);
        let (clips, _, _) = wasp_model();
        let mut game = wasp_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));

        // The setup falls straight through into the fall and lands.
        game.entities[1].set_state(2);
        game.entities[1].action_state = 0;
        game.entities[1].action_behavior = 0;
        game.entities[1].pos[1] = -50;
        game.entities[1].move_speed_current = 0x77;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let wasp = &game.entities[1];
        assert_eq!(wasp.action_state, 2, "landed in the same frame");
        assert_eq!(wasp.pos[1], 0);
        assert_eq!(wasp.move_speed_current, 0, "the setup zeroes the speed");
        assert_eq!(wasp.hit_state, 1);
        assert_eq!(wasp.status_flags & 0x0A, 0x0A, "intangible and dead");
        assert_eq!(wasp.action_ticks_counter, 0x1E);
        assert_eq!((wasp.shadow_half_x, wasp.shadow_half_z), (0, 0));
        assert_eq!(wasp.shadow_tint, 0x00FF3030, "the landing tint");
        assert_eq!(wasp.sink_wobble(), 0x1E);
        assert_eq!(
            game.effects
                .active()
                .filter(|(_, effect)| effect.attach == crate::effects::Attach::Joint(1, 0))
                .count(),
            1,
            "the knockdown blood puff"
        );

        // The shadow grows every frame; the timer expiry goes limp.
        game.entities[1].shadow_half_x = 10;
        game.entities[1].shadow_half_z = 10;
        game.entities[1].set_sink_wobble(0);
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let wasp = &game.entities[1];
        assert_eq!(wasp.shadow_half_x, 18, "BillboardAdjSize");
        assert_eq!(wasp.shadow_half_z, 18);
        assert_eq!(wasp.action_state, 3);
        assert_eq!(wasp.status_flags & 0xE0, 0);
        assert_eq!(wasp.hit_state, 0);

        // A limp wasp is crushed by the moving player inside 400 units.
        game.entities[0].pos = [1000, 0, 1300];
        game.entities[0].move_speed_current = 5;
        game.entities[1].move_speed_current = 0;
        game.entities[1].set_wasp_distance(wasp_distance(&game));
        let expected_sound = crate::sfx::enemy_sound(&adder_room(), 2, 0).is_some();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let wasp = &game.entities[1];
        assert_eq!(wasp.health, -4);
        assert_eq!(wasp.hit_state, 1);
        assert_eq!(wasp.state(), 3);
        assert_eq!((wasp.action_behavior, wasp.action_state), (0, 0));
        assert_eq!(wasp.ignore(), 0);
        assert_eq!(wasp.wasp_sound_latch, 1);
        assert_eq!(game.entity_sounds.len(), usize::from(expected_sound));
    }

    /// The death fork: the light corpse that crawls and respawns at the nest,
    /// the tenth respawn's big variant, and the heavy/big removal.
    #[cfg(feature = "lua")]
    #[test]
    fn wasp_death_corpse_respawns_and_big_is_removed() {
        let pack = pack(&[("enemy/em07.lua", wasp_source())]);
        let (clips, _, _) = wasp_model();

        // The light kill frame leaves the corpse and its two blood puffs.
        let mut game = wasp_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        game.entities[1].set_state(3);
        game.entities[1].action_state = 0;
        game.entities[1].action_behavior = 0;
        game.entities[1].hit_state = 0x10;
        game.entities[1].move_speed_current = 0x99;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let wasp = &game.entities[1];
        assert_eq!(wasp.action_state, 2, "the corpse crawls");
        assert_eq!(wasp.sink_wobble(), 0x14);
        assert_eq!(wasp.move_speed_current, 0);
        assert_eq!((wasp.shadow_half_x, wasp.shadow_half_z), (0, 0));
        assert_eq!(wasp.status_flags & 0x0A, 0x0A);
        for joint in [0, 9, 7, 8] {
            assert_eq!(
                game.effects
                    .active()
                    .filter(|(_, effect)| {
                        effect.attach == crate::effects::Attach::Joint(1, joint)
                    })
                    .count(),
                1,
                "a blood puff at joint {joint}"
            );
        }
        assert!(game.entity_sounds.iter().any(|sound| sound.bank == 2));

        // The crawl countdown respawns it at the fixed nest with fresh draws.
        game.entities[1].set_sink_wobble(0);
        game.rand_state = 0x2233;
        let mut expected = 0x2233u32;
        let dx = crate::game::platform_rand(&mut expected) & 1;
        let dy = crate::game::platform_rand(&mut expected) & 1;
        let dz = crate::game::platform_rand(&mut expected) & 1;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let wasp = &game.entities[1];
        assert_eq!(wasp.state(), 0, "the reset dword re-runs init");
        assert_eq!(wasp.behavior_flags, 0x10);
        assert_eq!(wasp.wasp_respawns, 1);
        assert_eq!(wasp.angle, 0x400);
        assert_eq!(wasp.pos[0], (0xB4 - i32::from(dx)) * 100);
        assert_eq!(wasp.pos[1], (-0x28 - i32::from(dy)) * 100);
        assert_eq!(wasp.pos[2], (0xE5 - i32::from(dz)) * 100);
        assert_eq!(wasp.joint_scale, 1, "the death tilt");
        assert_eq!(game.rand_state, expected, "X then Y then Z");

        // The next tick runs init again with the respawn kind.
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].state(), 1);
        assert_eq!(game.entities[1].wasp_big, 0, "an ordinary respawn");

        // The tenth respawn comes back as kind 0x12, which the next init
        // consumes into the big variant.
        game.entities[1].set_state(3);
        game.entities[1].action_state = 2;
        game.entities[1].wasp_respawns = 10;
        game.entities[1].set_sink_wobble(0);
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].behavior_flags, 0x12);
        assert_eq!(game.entities[1].wasp_respawns, 11);
        assert_eq!(game.entities[1].state(), 0);
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        let wasp = &game.entities[1];
        assert_eq!(wasp.wasp_big, 1, "the tenth respawn is the big wasp");
        assert_eq!(wasp.behavior_flags, 0x10, "the bit is consumed again");
        assert_eq!(wasp.joint_scale, 0x2000);

        // A heavy weapon (hit_state > 0x30) is removed at the kill frame and
        // raises the spawn record's flag at once.
        let mut game = wasp_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        game.entities[1].set_state(3);
        game.entities[1].action_state = 0;
        game.entities[1].action_behavior = 0;
        game.entities[1].hit_state = 0x38;
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_state, 4);
        assert!(game.flags[crate::game::BANK_ENEMIES as usize].bit(3));

        // A big wasp is removed even by a light weapon.
        let mut game = wasp_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        game.entities[1].set_state(3);
        game.entities[1].action_state = 0;
        game.entities[1].wasp_big = 1;
        game.entities[1].hit_state = 0x10;
        game.entities[1].set_sink_wobble(0);
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_state, 4);
        assert!(game.flags[crate::game::BANK_ENEMIES as usize].bit(3));

        // The knocked-down big wasp skips the corpse: its parked timer hands
        // straight to the shadow fade and the death flag on expiry.
        let mut game = wasp_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        game.entities[1].set_state(3);
        game.entities[1].action_state = 0;
        game.entities[1].wasp_big = 1;
        game.entities[1].hit_state = 0x10;
        game.entities[1].set_sink_wobble(0x14);
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_state, 1, "the shadow fade");
        assert!(!game.flags[crate::game::BANK_ENEMIES as usize].bit(3));
        game.entities[1].set_sink_wobble(0);
        assert!(wasp_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_state, 4);
        assert!(game.flags[crate::game::BANK_ENEMIES as usize].bit(3));
    }

    /// A multi-tick scene spanning init, the nest, emerge, hover, a sting, a
    /// knockdown and a death respawn must be byte-identical with and without
    /// a VM reset between every tick.
    #[cfg(feature = "lua")]
    #[test]
    fn wasp_reset_between_frames_preserves_the_scene() {
        let pack = pack(&[("enemy/em07.lua", wasp_source())]);
        let (clips, keyframes, skeleton) = wasp_model();
        let mut cached_game = wasp_game(0x10);
        let mut reset_game = wasp_game(0x10);
        for game in [&mut cached_game, &mut reset_game] {
            game.entity_anims[1].keyframes = Some(keyframes.clone());
            game.entity_anims[1].skeleton = Some(skeleton.clone());
            game.entities[0].pos = [1500, 0, 1000];
            game.entities[0].action_behavior = 0x14;
            game.entities[0].action_state = 0;
            game.rand_state = 0x1234;
        }
        let mut cached = LuaEnemyHost::new();
        let mut reset = LuaEnemyHost::new();

        // The scene: init, the dwell arm, emerge, hover, a forced sting, a
        // knockdown and the corpse respawn.
        let scenario: &[(u8, u8)] = &[
            (0, 0),
            (1, 0),
            (1, 0),
            (1, 6),
            (1, 6),
            (1, 6),
            (1, 6),
            (1, 6),
            (1, 2),
            (1, 2),
            (1, 3),
            (1, 3),
            (2, 0),
            (2, 0),
            (3, 0),
            (3, 0),
            (3, 2),
            (3, 2),
        ];
        for (tick, &(state, behavior)) in scenario.iter().enumerate() {
            for game in [&mut cached_game, &mut reset_game] {
                game.entities[1].set_state(state);
                game.entities[1].action_behavior = behavior;
                game.entities[1].action_state = 0;
                if state == 3 {
                    game.entities[1].hit_state = 0x10;
                }
            }
            assert!(cached.update(&mut cached_game, &adder_room(), &pack, 1, &clips));
            reset.reset();
            assert!(reset.update(&mut reset_game, &adder_room(), &pack, 1, &clips));
            assert_eq!(
                cached_game, reset_game,
                "the wasp scene diverged at tick {tick} (state {state})"
            );
        }

        // The corpse branch needs its countdown; keep both on it.
        let mut respawned = false;
        for tick in 0..64 {
            for game in [&mut cached_game, &mut reset_game] {
                if game.entities[1].state() != 0 {
                    game.entities[1].set_state(3);
                    game.entities[1].action_behavior = 0;
                    game.entities[1].action_state = 2;
                }
            }
            assert!(cached.update(&mut cached_game, &adder_room(), &pack, 1, &clips));
            reset.reset();
            assert!(reset.update(&mut reset_game, &adder_room(), &pack, 1, &clips));
            assert_eq!(
                cached_game, reset_game,
                "the respawn scene diverged at tick {tick}"
            );
            if cached_game.entities[1].state() == 1 {
                respawned = true;
                break;
            }
        }
        assert!(respawned, "the corpse respawned through a fresh init");
    }

    /// The wasp's typed property surface: the signed aliases over the shared
    /// scratch bytes, the two halves of the +0x178 dword, the path-keep into
    /// +0x16E and the player-side reads.
    #[cfg(feature = "lua")]
    #[test]
    fn wasp_script_sees_typed_scratch_properties() {
        let pack = pack(&[(
            "enemy/em07.lua",
            r#"
function update(e)
    e.wasp_drift = -0x1234
    e.wasp_touch = -2
    e.wasp_climb = 0x1234
    e.wasp_timer = -3
    e.wasp_bob = -4
    e.wasp_distance = 0xBEEF
    e.wasp_collision = 0xCAFE
    e.wasp_anim_done = 0x12345
    e.wasp_big = 0x102
    e.wasp_respawns = 0x1FF
    e.wasp_sound_latch = 0x101
    e.wasp_speed = -5
    e.wasp_dwell = -6
    assert(e.wasp_path_bit == 0)
    e:wasp_path_keep(1)
    assert(e.wasp_path_bit == 1)
    assert(e.player_move_speed == 0)
    assert(e.second_playthrough == false)
    assert(e:player_facing_entity() == true)
end
"#,
        )]);
        let mut game = wasp_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(wasp_step(&mut host, &mut game, &pack, &[]));
        let entity = &game.entities[1];
        assert_eq!(entity.writhe_velocity(), -0x1234, "drift");
        assert_eq!(entity.tint_flashes(), -2, "touch");
        assert_eq!(entity.groan_timer(), 0x1234, "climb");
        assert_eq!(entity.sink_wobble(), -3, "timer");
        assert_eq!(entity.reaction_timer, -4, "bob");
        assert_eq!(entity.wasp_distance(), 0xBEEF);
        assert_eq!(entity.wasp_collision(), 0xCAFE);
        assert_eq!(entity.subpixel_pos_x, 0xCAFE_BEEFu32 as i32);
        assert_eq!(entity.wasp_anim_done, 0x2345, "u16 truncation");
        assert_eq!(entity.wasp_big, 0x02, "u8 truncation");
        assert_eq!(entity.wasp_respawns, 0xFF);
        assert_eq!(entity.wasp_sound_latch, 0x01);
        assert_eq!(entity.move_speed_current, (-5i16) as u16);
        assert_eq!(entity.action_ticks_counter, (-6i16) as u16);
        assert_eq!(entity.tex_bank & 1, 1);

        // The second playthrough flag reads through the scenario bank.
        game.flags[usize::from(crate::game::BANK_SCENARIO)]
            .apply(crate::game::SCENARIO_FLAG_SECOND_PLAYTHROUGH, 0);
        assert!(
            game.flags[usize::from(crate::game::BANK_SCENARIO)]
                .bit(crate::game::SCENARIO_FLAG_SECOND_PLAYTHROUGH)
        );
    }

    /// The checked-in crow script, so the tests exercise the real port.
    #[cfg(feature = "lua")]
    fn crow_source() -> &'static str {
        crate::enemy::ENEMY_SCRIPTS
            .iter()
            .find(|(path, _)| *path == "enemy/em05.lua")
            .expect("the checked-in crow script")
            .1
    }

    /// The crow's synthetic model: sixteen one-tick clips and a 13-joint
    /// skeleton, matching em1005.
    #[cfg(feature = "lua")]
    fn crow_model() -> (
        Vec<crate::model::Clip>,
        std::sync::Arc<Vec<crate::model::Keyframe>>,
        std::sync::Arc<crate::model::Skeleton>,
    ) {
        use crate::model::{Clip, ClipFrame, Keyframe, Skeleton};
        let clips = (0..16)
            .map(|_| Clip {
                frames: (0..32)
                    .map(|_| ClipFrame {
                        keyframe: 0,
                        timing: 1,
                    })
                    .collect(),
            })
            .collect();
        let keyframes = vec![Keyframe {
            offset: [0, 0, 0],
            rotations: vec![[0, 0, 0]; 13],
        }];
        let skeleton = Skeleton {
            relative: vec![[0, 0, 0]; 13],
            children: (0..13)
                .map(|index| if index == 0 { vec![1] } else { vec![] })
                .collect(),
        };
        (
            clips,
            std::sync::Arc::new(keyframes),
            std::sync::Arc::new(skeleton),
        )
    }

    /// One active crow in slot 1 with the given spawn kind.
    #[cfg(feature = "lua")]
    fn crow_game(kind: u8) -> GameState {
        use crate::effects::fixtures::{block, sprite};
        let mut game = GameState::default();
        for index in [0u8, 0x1C] {
            game.weapon_effects.sprites.push(sprite(
                index,
                std::array::from_fn(|_| vec![vec![block(1, 0, 0)]]),
            ));
        }
        let entity = &mut game.entities[1];
        entity.id = 0x05;
        entity.set_active(true);
        entity.status_flags = 1;
        entity.behavior_flags = kind;
        entity.death_event_id = 3;
        entity.pos = [1000, -2500, 1000];
        entity.saved_pos = Some(entity.pos);
        game.enemy_count = 1;
        game
    }

    /// The crow room's sound row and camera zone covering the spawn point, so
    /// the shadow gate is live.
    #[cfg(feature = "lua")]
    fn crow_room() -> RoomState {
        use crate::state::Zone;
        RoomState {
            stage: 1,
            room: 1,
            zones: vec![Zone {
                cam_from: 0,
                cam_to: 0,
                corners: [[0, 0], [0, 5000], [5000, 5000], [5000, 0]],
            }],
            ..RoomState::default()
        }
    }

    #[cfg(feature = "lua")]
    fn crow_step(
        host: &mut LuaEnemyHost,
        game: &mut GameState,
        pack: &Pack,
        clips: &[crate::model::Clip],
    ) -> bool {
        host.update(game, &crow_room(), pack, 1, clips)
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_initialises_for_every_spawn_kind() {
        let pack = pack(&[("enemy/em05.lua", crow_source())]);
        let (clips, _, _) = crow_model();
        for kind in [0u8, 0x10, 0x02, 0x11] {
            let mut game = crow_game(kind);
            let mut host = LuaEnemyHost::new();
            game.rand_state = 1;
            let mut expected = 1u32;
            let speed = if kind & 0x10 != 0 {
                Some(crate::game::platform_rand(&mut expected) & 1)
            } else {
                None
            };
            let _discarded = crate::game::platform_rand(&mut expected);
            let ra = crate::game::platform_rand(&mut expected) & 7;
            let rb = crate::game::platform_rand(&mut expected) & 7;
            let rc = crate::game::platform_rand(&mut expected) & 7;
            let mut health = (rb * 2 + ra * 2 + rc * 2 + 10) as i16;

            assert!(
                crow_step(&mut host, &mut game, &pack, &clips),
                "kind {kind:#04x}"
            );
            let entity = &game.entities[1];
            assert_eq!(game.rand_state, expected, "kind {kind:#04x} draw order");

            if kind & 2 != 0 {
                assert_eq!(entity.state(), 8, "the SCD spawn lands on state 8");
                assert_eq!(entity.ignore(), 0);
                assert_eq!(entity.action_behavior, 0);
                assert_eq!(entity.animation_id, 0);
            } else if kind & 0x10 != 0 {
                assert_eq!(entity.state(), 1);
                assert_eq!(entity.ignore(), 1, "the airborne spawn is airborne");
                assert_eq!(entity.action_behavior, 5);
                assert_eq!(entity.animation_id, 0);
                assert_eq!(entity.roll, 0x100);
                assert_eq!(entity.reaction_timer, 200, "the spawned VY");
                assert_eq!(
                    entity.move_speed_current,
                    0x32 + speed.expect("the airborne speed draw") * 0x10
                );
            } else {
                assert_eq!(entity.state(), 1);
                assert_eq!(entity.ignore(), 0, "the grounded spawn perches");
                assert_eq!(entity.action_behavior, 0);
                assert_eq!(entity.animation_id, 2);
            }

            assert_eq!(entity.action_state, 0);
            assert_eq!(entity.collision_flags, 4, "floor reporting is armed");
            assert_eq!(entity.sca_radius, 200);
            assert_eq!(entity.sca_half_height, 180);
            assert_eq!(entity.sca_offset, [0, 0, 0]);
            assert_eq!(entity.joint_scale, 0x14CC);
            assert_eq!(entity.shadow_tint, 0x0080_8080);
            assert_eq!(entity.joint_flags, 0);
            if kind & 1 != 0 {
                health = 0;
                assert_eq!(entity.alt_bias, -400);
            } else {
                assert_eq!(entity.alt_bias, 0);
            }
            assert_eq!(entity.health, health, "kind {kind:#04x} health roll");

            // The tail sizes the quad from the altitude; a perched crow queues
            // no shadow, so the extents are suppressed.
            let perched = entity.state() == 1 && entity.ignore() == 0;
            let expected_half = if perched { 0 } else { 243 };
            assert_eq!(
                (entity.shadow_half_x, entity.shadow_half_z),
                (expected_half, expected_half),
                "kind {kind:#04x} shadow"
            );
            assert_eq!(entity.has_enter_switch_zone, 1, "the zone bit");
        }
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_perch_counts_down_and_rolls_the_idle_clip() {
        let pack = pack(&[("enemy/em05.lua", crow_source())]);
        let (clips, _, _) = crow_model();
        let mut game = crow_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));

        // The perch setup rolls 40..102 and counts one down on the same frame.
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(0);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 0;
        game.rand_state = 1;
        let mut expected = 1u32;
        let roll = crate::game::platform_rand(&mut expected) & 0x1F;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.rand_state, expected, "the perch setup draw");
        let entity = &game.entities[1];
        assert_eq!(
            entity.action_ticks_counter,
            (roll * 2 + 0x28).wrapping_sub(1)
        );
        assert_eq!(entity.action_state, 1);

        // While counting, a non-zero counter means no clip work and no draws.
        let before = game.rand_state;
        game.entities[1].action_ticks_counter = 5;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_ticks_counter, 4);
        assert_eq!(game.entities[1].action_state, 1);
        assert_eq!(game.rand_state, before);

        // A dead player ends the count immediately.
        game.entities[1].action_ticks_counter = 30;
        game.entities[0].health = -1;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_state, 2);
        game.entities[0].health = 100;

        // Sub-state 2 rolls the idle clip: 11, or 12, or a quarter of the time
        // clip 2 with a caw. The second roll is only drawn when the first hit.
        let mut seeds = std::collections::HashMap::new();
        for seed in 1u32.. {
            let mut state = seed;
            let first = crate::game::platform_rand(&mut state) & 1;
            if first != 0 {
                let second = crate::game::platform_rand(&mut state) & 1;
                seeds.entry((first, second)).or_insert(seed);
            } else {
                seeds.entry((0, 0)).or_insert(seed);
            }
            if seeds.len() == 3 {
                break;
            }
        }
        for (first, second) in [(0u16, 0u16), (1, 0), (1, 1)] {
            let seed = seeds[&(first, second)];
            let mut game = crow_game(0);
            let mut host = LuaEnemyHost::new();
            assert!(crow_step(&mut host, &mut game, &pack, &clips));
            game.entities[1].set_state(1);
            game.entities[1].set_ignore(0);
            game.entities[1].action_behavior = 0;
            game.entities[1].action_state = 2;
            game.rand_state = seed;
            let mut expected = seed;
            let _ = crate::game::platform_rand(&mut expected);
            if first != 0 {
                let _ = crate::game::platform_rand(&mut expected);
            }
            assert!(crow_step(&mut host, &mut game, &pack, &clips));
            let entity = &game.entities[1];
            let expected_anim = match (first, second) {
                (0, _) => 0xB,
                (_, 0) => 0xC,
                (_, _) => 2,
            };
            assert_eq!(entity.animation_id, expected_anim);
            assert_eq!(entity.action_state, 3, "the clip starts immediately");
            assert_eq!(game.rand_state, expected, "roll order ({first},{second})");
            assert_eq!(
                game.entity_sounds.len(),
                usize::from(first == 1 && second == 1),
                "the caw only on clip 2"
            );
        }
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_scatter_takeoff_reads_the_player_state_word() {
        let pack = pack(&[("enemy/em05.lua", crow_source())]);
        let (clips, _, _) = crow_model();

        for player_state in [0u8, 3] {
            let mut game = crow_game(0);
            let mut host = LuaEnemyHost::new();
            assert!(crow_step(&mut host, &mut game, &pack, &clips));
            {
                let e = &mut game.entities[1];
                e.set_state(1);
                e.set_ignore(0);
                e.action_behavior = 0;
                e.action_state = 1;
                e.angle = 0;
            }
            let player = &mut game.entities[0];
            player.set_state(1);
            player.set_ignore(3);
            player.action_behavior = 0x14;
            player.action_state = player_state;

            game.rand_state = 7;
            let mut expected = 7u32;
            let scatter = crate::game::platform_rand(&mut expected) & 1;
            let r1 = crate::game::platform_rand(&mut expected) & 1;
            let r2 = crate::game::platform_rand(&mut expected) & 1;

            assert!(crow_step(&mut host, &mut game, &pack, &clips));
            let entity = &game.entities[1];
            assert_eq!(entity.angle, scatter * 0x40, "the random half-turn");
            assert_eq!(entity.state(), 1);
            assert_eq!(entity.ignore(), 1);
            assert_eq!(entity.action_behavior, 13, "the scatter is a takeoff");
            assert_eq!(entity.action_state, 1);
            assert_eq!(entity.behavior_flags & 0x10, 0x10, "the airborne bit");
            assert_eq!(
                entity.action_ticks_counter,
                ((r2 & 1) * 3 + (r1 & 1) * 8).wrapping_sub(1),
                "r2 first, then r1"
            );
            assert_eq!(
                game.rand_state, expected,
                "scatter then the two takeoff rolls (player state {player_state})"
            );
        }

        // An already-airborne crow ignores the cue.
        let mut game = crow_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        game.entities[1].behavior_flags |= 0x10;
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(0);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 0;
        game.entities[0].set_state(1);
        game.entities[0].set_ignore(3);
        game.entities[0].action_behavior = 0x14;
        game.entities[0].action_state = 0;
        game.rand_state = 7;
        let mut expected = 7u32;
        let roll = crate::game::platform_rand(&mut expected) & 0x1F;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.rand_state, expected, "the perch setup draw");
        assert_eq!(game.entities[1].action_ticks_counter, roll * 2 + 0x28 - 1);
        assert_eq!(
            game.entities[1].action_behavior, 0,
            "no scatter while airborne"
        );

        // An `ignore` outside 0/1 freezes the dispatcher entirely.
        let mut game = crow_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(2);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 1;
        game.entities[1].action_ticks_counter = 7;
        let before = game.rand_state;
        game.entities[0].set_state(1);
        game.entities[0].set_ignore(3);
        game.entities[0].action_behavior = 0x14;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_ticks_counter, 7, "frozen");
        assert_eq!(game.rand_state, before, "no draws while frozen");
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_wingbeat_arcs_and_hands_over() {
        let pack = pack(&[("enemy/em05.lua", crow_source())]);
        let (clips, _, _) = crow_model();
        let mut game = crow_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));

        // Fresh climb: one speed draw, a flat 30-unit rise and the roll bank.
        {
            let e = &mut game.entities[1];
            e.set_state(1);
            e.set_ignore(1);
            e.action_behavior = 1;
            e.action_state = 0;
            e.pos[1] = -2500;
            e.roll = 0;
            e.reaction_timer = 0;
            e.move_speed_current = 0;
            e.animation_id = 0;
            e.animation_frame_id = 0;
            e.timing_control = 0;
        }
        game.rand_state = 3;
        let mut expected = 3u32;
        let speed = crate::game::platform_rand(&mut expected) & 1;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.rand_state, expected, "one wingbeat draw");
        let entity = &game.entities[1];
        assert_eq!(entity.animation_id, 1);
        assert_eq!(entity.move_speed_current, 0x32 + speed * 0x10);
        assert_eq!(entity.pos[1], -2530, "the flat climb");
        assert_eq!(entity.roll, 0x15);
        assert!(entity.speed[0] > 0, "the stored vector moved on X");
        assert_eq!(entity.speed[2], 0);

        // Past frame 8 the sink (frame + 52) outweighs the climb.
        {
            let e = &mut game.entities[1];
            e.action_state = 1;
            e.animation_frame_id = 8;
            e.timing_control = 0;
            e.pos[1] = -2500;
            e.roll = 0;
            e.reaction_timer = 0;
        }
        game.rand_state = 9;
        let mut expected = 9u32;
        let _speed = crate::game::platform_rand(&mut expected) & 1;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.animation_frame_id, 9, "the clip advanced");
        assert_eq!(entity.reaction_timer, 9 + 0x34, "VY from the frame");
        assert_eq!(entity.pos[1], -2500 - 30 + (9 + 0x34));
        assert_eq!(game.rand_state, expected);

        // Below the hand-over altitude the glide takes over.
        game.entities[1].action_state = 1;
        game.entities[1].animation_frame_id = 0;
        game.entities[1].timing_control = 0;
        game.entities[1].pos[1] = -1400;
        game.entities[1].roll = 0;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.roll, 0x100, "the roll is levelled for the glide");
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.action_behavior, 5);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_dive_and_glide_bounds() {
        let pack = pack(&[("enemy/em05.lua", crow_source())]);
        let (clips, _, _) = crow_model();
        let mut game = crow_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        game.entities[0].pos = [2000, 0, 1000];

        // Dive: speed bleeds six a frame and the descent uses the ceiling
        // bound's signed 40th.
        {
            let e = &mut game.entities[1];
            e.set_state(1);
            e.set_ignore(1);
            e.action_behavior = 2;
            e.action_state = 1;
            e.animation_id = 4;
            e.animation_frame_id = 0;
            e.timing_control = 0;
            e.pos[1] = -2000;
            e.roll = 0;
            e.move_speed_current = 0;
            e.set_crow_ceil_limit(-3500);
        }
        let before = game.rand_state;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.rand_state, before, "the dive draws nothing");
        let entity = &game.entities[1];
        assert_eq!(entity.animation_frame_id, 1);
        assert_eq!(entity.move_speed_current, 194);
        assert_eq!(entity.pos[1], -1913, "-87 per frame");
        assert_eq!(entity.roll, 0xFFFA);

        // Facing the player, the floor exit hands to the strike run instead of
        // a full dive.
        game.entities[1].action_state = 1;
        game.entities[1].animation_frame_id = 0;
        game.entities[1].timing_control = 0;
        game.entities[1].pos[1] = -400;
        game.entities[1].angle = 0;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.pos[1], -450);
        assert_eq!(entity.move_speed_current, 0);
        assert_eq!(entity.roll, 0);
        assert_eq!(entity.action_behavior, 4, "aligned, so the strike run");
        assert_eq!(entity.action_state, 0);

        // Descending glide: the sink ramps to 11, the ceiling bound rolls, and
        // the ninth frame caws when the player is close.
        {
            let e = &mut game.entities[1];
            e.set_state(1);
            e.set_ignore(1);
            e.action_behavior = 5;
            e.action_state = 1;
            e.animation_id = 0;
            e.animation_frame_id = 9;
            e.timing_control = 0;
            e.pos[1] = -2000;
            e.roll = 0;
            e.reaction_timer = 10;
            e.move_speed_current = 200;
            e.alt_bias = 0;
            e.set_writhe_amplitude(0);
            e.angle = 0;
        }
        game.entities[0].pos = [1500, 0, 1000];
        game.rand_state = 21;
        let mut expected = 21u32;
        let ceil_roll = crate::game::platform_rand(&mut expected) & 1;
        game.entity_sounds.clear();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.rand_state, expected, "one ceiling draw");
        let entity = &game.entities[1];
        assert_eq!(entity.animation_frame_id, 10);
        assert_eq!(entity.reaction_timer, 11);
        assert_eq!(entity.pos[1], -1989);
        assert_eq!(
            entity.crow_ceil_limit(),
            (ceil_roll as i16 * -500) * 2 - 1500
        );
        assert_eq!(game.entity_sounds.len(), 1, "the ninth-frame caw");
        assert_eq!(game.entity_sounds[0].column, 5);
        if ceil_roll == 1 {
            assert_eq!(entity.action_behavior, 7, "the bound hands to the run");
        } else {
            assert_eq!(entity.action_behavior, 5);
        }

        // Climb out: the velocity subtracts and the floor bound rolls -4500 or
        // -5500.
        {
            let e = &mut game.entities[1];
            e.set_state(1);
            e.set_ignore(1);
            e.action_behavior = 6;
            e.action_state = 1;
            e.animation_id = 5;
            e.animation_frame_id = 0;
            e.timing_control = 0;
            e.pos[1] = -5000;
            e.roll = 0;
            e.reaction_timer = 10;
            e.move_speed_current = 200;
            e.alt_bias = 0;
            e.angle = 0;
        }
        game.entities[0].pos = [1500, 0, 1000];
        game.rand_state = 22;
        let mut expected = 22u32;
        let floor_roll = crate::game::platform_rand(&mut expected) & 1;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.rand_state, expected);
        let entity = &game.entities[1];
        assert_eq!(entity.reaction_timer, 11);
        assert_eq!(entity.pos[1], -5011, "the climb subtracts");
        assert_eq!(
            entity.crow_floor_limit(),
            floor_roll as i16 * -1000 - 0x1194
        );
        if floor_roll == 0 {
            assert_eq!(entity.action_behavior, 10, "levelling out on the bound");
        } else {
            assert_eq!(entity.action_behavior, 6);
        }
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_strike_run_and_dive_attack_vy_recipes() {
        let pack = pack(&[("enemy/em05.lua", crow_source())]);
        let (clips, _, _) = crow_model();
        let mut game = crow_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        game.entities[0].pos = [2000, 0, 1000];

        // The strike run past frame 8: dist 1000 gives VY 546, clamped to 200.
        {
            let e = &mut game.entities[1];
            e.set_state(1);
            e.set_ignore(1);
            e.action_behavior = 4;
            e.action_state = 1;
            e.animation_id = 7;
            e.animation_frame_id = 8;
            e.timing_control = 0;
            e.pos = [1000, -2500, 1000];
            e.roll = 0;
            e.reaction_timer = 0;
        }
        game.entity_sounds.clear();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.wasp_distance(), 1000);
        assert_eq!(entity.reaction_timer, 200, "the VY clamp");
        assert_eq!(entity.move_speed_current, 210);
        assert_eq!(entity.roll, 0xFF00);
        assert_eq!(entity.action_behavior, 6);
        assert_eq!(game.entity_sounds.len(), 1, "the strike-run caw");
        assert_eq!(game.entity_sounds[0].column, 4);

        // The dive attack: speed from the roll and distance, and the scripted
        // variant recomputes its exit climb.
        for scripted in [false, true] {
            let mut game = crow_game(0);
            let mut host = LuaEnemyHost::new();
            assert!(crow_step(&mut host, &mut game, &pack, &clips));
            game.entities[0].pos = [2000, 0, 1000];
            game.entities[1].behavior_flags |= u8::from(scripted);
            {
                let e = &mut game.entities[1];
                e.set_state(1);
                e.set_ignore(1);
                e.action_behavior = 7;
                e.action_state = 1;
                e.animation_id = 7;
                e.animation_frame_id = 0x12;
                e.timing_control = 0;
                e.pos = [1000, -2500, 1000];
                e.roll = 0;
                e.reaction_timer = 0;
            }
            game.rand_state = 5;
            let mut expected = 5u32;
            let speed_roll = crate::game::platform_rand(&mut expected) & 1;
            assert!(crow_step(&mut host, &mut game, &pack, &clips));
            let entity = &game.entities[1];
            assert_eq!(entity.move_speed_current, (4 - speed_roll) * 0x32 + 10);
            assert_eq!(entity.roll, 0xFF00);
            assert_eq!(entity.action_behavior, 6);
            if scripted {
                assert_eq!(entity.reaction_timer, 200, "the scripted exit VY");
            } else {
                assert_eq!(entity.reaction_timer, 0);
            }
            assert_eq!(game.rand_state, expected, "one draw without a hit");
        }

        // Landing on a reacting player knocks the crow back and spins it on a
        // second draw.
        {
            let mut game = crow_game(0);
            let mut host = LuaEnemyHost::new();
            assert!(crow_step(&mut host, &mut game, &pack, &clips));
            game.entities[0].pos = [2000, 0, 1000];
            game.entities[0].is_being_attacked = 1;
            game.entities[0].action_state = 4;
            game.entities[1].set_state(1);
            game.entities[1].set_ignore(1);
            game.entities[1].action_behavior = 7;
            game.entities[1].action_state = 1;
            game.entities[1].animation_id = 7;
            game.entities[1].animation_frame_id = 0;
            game.entities[1].timing_control = 0;
            game.entities[1].pos = [1000, -2500, 1000];
            game.entities[1].angle = 0;
            game.rand_state = 8;
            let mut expected = 8u32;
            let _speed_roll = crate::game::platform_rand(&mut expected) & 1;
            let spin = crate::game::platform_rand(&mut expected) & 1;
            assert!(crow_step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.entities[1].pos[1], -2550, "the knockback");
            assert_eq!(game.entities[1].angle, spin * 0x800);
            assert_eq!(game.rand_state, expected, "the speed then the spin");
        }
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_peck_bites_and_latches_on() {
        for (second, damage) in [(false, 6u16), (true, 16)] {
            let pack = pack(&[("enemy/em05.lua", crow_source())]);
            let (clips, _, _) = crow_model();
            let mut game = crow_game(0);
            let mut host = LuaEnemyHost::new();
            assert!(crow_step(&mut host, &mut game, &pack, &clips));
            if second {
                game.flags[usize::from(crate::game::BANK_SCENARIO)]
                    .apply(crate::game::SCENARIO_FLAG_SECOND_PLAYTHROUGH, 0);
            }
            game.entities[0].pos = [1000, 0, 1400];
            game.entities[0].health = 100;
            {
                let e = &mut game.entities[1];
                e.set_state(1);
                e.set_ignore(1);
                e.action_behavior = 9;
                e.action_state = 2;
                e.animation_id = 8;
                e.pos = [1000, -2500, 1000];
                e.angle = 0xC00;
                e.roll = 0;
                e.set_writhe_amplitude(1);
            }
            let before = game.rand_state;
            game.entity_sounds.clear();
            assert!(crow_step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.rand_state, before, "the bite itself draws nothing");
            let entity = &game.entities[1];
            assert_eq!(entity.action_behavior, 0xB, "the latch");
            assert_eq!(entity.action_state, 0);
            assert_eq!(entity.hit_state, 1);
            assert_eq!(entity.roll, 0);
            assert_eq!(game.entities[0].health, 100 - damage as i16);
            assert_eq!(game.entities[0].is_being_attacked, 1);
            assert_eq!(game.entities[0].state(), 6, "the grab window");
            assert_eq!(game.entities[0].ignore(), 5, "the grab sub-window");
            assert_eq!(game.entities[0].action_behavior, 0);
            assert_eq!(game.entity_sounds.len(), 1, "the bite cue at the player");
            assert_eq!(game.entity_sounds[0].bank, 3);
            assert_eq!(game.effects.active_count(), 2, "feather and blood");

            // The latched player's own state-6 peck machine now runs: the
            // damage bank's clip 0 wraps into the hold and clip 1 becomes the
            // held peck the crow's release gate reads.
            game.player_damage = Some(std::sync::Arc::new(crate::game::PlayerDamageBank {
                keyframes: std::sync::Arc::new(vec![crate::model::Keyframe::default()]),
                clips: std::sync::Arc::new(
                    (0..4)
                        .map(|_| crate::model::Clip {
                            frames: (0..1)
                                .map(|_| crate::model::ClipFrame {
                                    keyframe: 0,
                                    timing: 1,
                                })
                                .collect(),
                        })
                        .collect(),
                ),
            }));
            let mut player = crate::player::spawn(game.id, &RoomState::default());
            player.pos = game.entities[0].pos;
            crate::player_script::update(
                &mut game,
                &mut player,
                &RoomState::default(),
                &[],
                &[],
                &[],
            );
            assert_eq!(
                game.entities[0].action_state, 2,
                "clip 0 wraps into the hold"
            );
            assert_eq!(player.clip_source, crate::player::ClipSource::Damage);
            assert_eq!(player.anim.clip, 0, "the first peck clip");
            crate::player_script::update(
                &mut game,
                &mut player,
                &RoomState::default(),
                &[],
                &[],
                &[],
            );
            assert_eq!(game.entities[0].action_state, 3, "the held peck");
            assert_eq!(player.anim.clip, 1);

            // The crow's struggle counter releases by writing action_state 4;
            // the player's window runs the last clip out and hands control
            // back.
            game.entities[1].action_state = 1;
            game.entities[1].struggle = 1;
            game.dpad_held = 1;
            assert!(crow_step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.entities[0].action_state, 4, "the crow's release write");
            for _ in 0..8 {
                crate::player_script::update(
                    &mut game,
                    &mut player,
                    &RoomState::default(),
                    &[],
                    &[],
                    &[],
                );
                if game.entities[0].state() == 1 {
                    break;
                }
            }
            assert_eq!(game.entities[0].state(), 1, "the player returns control");
            assert_eq!(game.entities[0].ignore(), 0);
            assert_eq!(game.entities[0].action_behavior, 0);
            assert_eq!(game.entities[0].is_being_attacked, 0);
        }

        // A crow that reaches the landing while the player is already in a
        // reaction parks on 9 and does not double-bite.
        let pack = pack(&[("enemy/em05.lua", crow_source())]);
        let (clips, _, _) = crow_model();
        let mut game = crow_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        game.entities[0].pos = [1000, 0, 1400];
        game.entities[0].health = 100;
        game.entities[0].is_being_attacked = 1;
        {
            let e = &mut game.entities[1];
            e.set_state(1);
            e.set_ignore(1);
            e.action_behavior = 9;
            e.action_state = 2;
            e.animation_id = 8;
            e.pos = [1000, -2500, 1000];
            e.angle = 0xC00;
            e.set_writhe_amplitude(1);
        }
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_behavior, 9);
        assert_eq!(game.entities[0].health, 100, "no second bite");
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_grab_struggle_mashes_and_releases() {
        let pack = pack(&[("enemy/em05.lua", crow_source())]);
        let (clips, _, _) = crow_model();

        let branch = |mash: bool, struggle: i8| {
            let mut game = crow_game(0);
            let mut host = LuaEnemyHost::new();
            assert!(crow_step(&mut host, &mut game, &pack, &clips));
            game.entities[0].pos = [1000, 0, 1100];
            game.entities[0].health = 50;
            game.entities[0].action_state = 3;
            game.entities[0].animation_frame_id = 1;
            {
                let e = &mut game.entities[1];
                e.set_state(1);
                e.set_ignore(1);
                e.action_behavior = 11;
                e.action_state = 1;
                e.animation_id = 8;
                // Above the player's SCA volume's vertical span, so the grab
                // logic is tested without the separation push.
                e.pos = [1000, -4000, 1000];
                e.angle = 0xC00;
                e.struggle = struggle;
            }
            game.dpad_held = if mash { 1 } else { 0 };
            (game, host)
        };

        let (mut game, mut host) = branch(true, 100);
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].struggle, 91, "nine a frame while mashing");
        assert_eq!(game.entities[1].action_state, 1);

        let (mut game, mut host) = branch(false, 100);
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].struggle, 99, "one a frame otherwise");

        // The counter crossing zero while the player is in reaction 3 starts
        // the release; the frame still draws only the flutter offset.
        let (mut game, mut host) = branch(true, 1);
        let mut expected = game.rand_state;
        let _flutter = crate::game::platform_rand(&mut expected) & 1;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].struggle, -8);
        assert_eq!(game.entities[1].action_state, 2);
        assert_eq!(game.entities[0].action_state, 4);
        assert_eq!(game.rand_state, expected, "the flutter offset still draws");

        // The release frame draws the speed and the 180-degree flip, then the
        // behaviour's flutter offset. Park the player dead ahead so the
        // release frame's rotate step is a no-op.
        let crow = game.entities[1].pos;
        game.entities[0].pos = [crow[0], 0, crow[2] + 100];
        game.rand_state = 11;
        let mut expected = 11u32;
        let speed_roll = crate::game::platform_rand(&mut expected) & 1;
        let spin = crate::game::platform_rand(&mut expected) & 1;
        let _flutter = crate::game::platform_rand(&mut expected) & 1;
        let angle = game.entities[1].angle;
        let y = game.entities[1].pos[1];
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.move_speed_current, (4 - speed_roll) * 0x32 + 1);
        assert_eq!(entity.roll, 0xFF00);
        assert_eq!(entity.angle, angle + spin * 0x800);
        assert_eq!(entity.pos[1], y - 100);
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.action_behavior, 6);
        assert_eq!(entity.hit_state, 0);
        assert_eq!(game.rand_state, expected, "release draws then the offset");

        // A mash releases early even without the player reaction state.
        let (mut game, mut host) = branch(true, 1);
        game.entities[0].action_state = 0;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(
            game.entities[1].action_state, 1,
            "reaction 3 gates the exit"
        );
        assert_eq!(game.entities[1].struggle, -8);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_recoil_beats_up_and_recovers() {
        let pack = pack(&[("enemy/em05.lua", crow_source())]);
        let (clips, _, _) = crow_model();
        let mut game = crow_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        game.entities[1].set_tint_flashes(1);
        {
            let e = &mut game.entities[1];
            e.set_state(2);
            e.behavior_flags = 0x10;
            e.action_behavior = 0;
            e.action_state = 0;
            e.animation_id = 6;
            e.animation_frame_id = 0;
            e.timing_control = 0;
            e.pos[1] = -600;
            e.roll = 0;
            e.move_speed_current = 0;
        }
        game.entity_sounds.clear();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.animation_id, 6);
        assert_eq!(entity.animation_frame_id, 1);
        assert_eq!(entity.move_speed_current, 200);
        assert_eq!(entity.pos[1], -450, "beat to the floor and clamped");
        assert_eq!(entity.action_behavior, 1, "into the recovery flap");
        assert_eq!(entity.action_state, 0);
        assert_eq!(game.entity_sounds.len(), 1, "the flap cue");
        assert_eq!(game.entity_sounds[0].column, 2);
        assert_eq!(game.effects.active_count(), 2, "the feather pair");

        // The recovery flap hands the crow back to the banking turn.
        game.entities[1].action_state = 1;
        game.entities[1].animation_id = 9;
        for _ in 0..40 {
            let before = game.rand_state;
            assert!(crow_step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.rand_state, before, "the recovery flap draws nothing");
            if game.entities[1].action_behavior != 1 {
                break;
            }
        }
        let entity = &game.entities[1];
        assert_eq!(entity.action_behavior, 3, "rejoins in the banking turn");
        assert_eq!(entity.hit_state, 0);
        assert_eq!(
            entity.status_flags & 0xE0,
            entity.status_flags & 0x20,
            "the cleared gates then the visual range bit"
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_death_lands_or_bursts_on_the_floor_step() {
        let pack = pack(&[("enemy/em05.lua", crow_source())]);
        let (clips, _, _) = crow_model();

        // A real floor: land, fade the shadow yellow and play the landing.
        let mut game = crow_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        game.entities[0].pos = [2000, 0, 1000];
        {
            let e = &mut game.entities[1];
            e.set_state(3);
            e.action_behavior = 0;
            e.action_state = 0;
            e.hit_state = 0;
            e.pos[1] = -500;
            e.roll = 0;
            e.angle = 0;
            e.floor_step = 1;
        }
        game.entity_sounds.clear();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert!(
            game.flags[usize::from(crate::game::BANK_ENEMIES)].bit(3),
            "the spawn record's death event"
        );
        assert_eq!(entity.action_behavior, 1, "the landing");
        assert_eq!(entity.action_state, 0);
        assert_eq!(entity.pos[1], -450);
        assert_eq!(entity.roll, 0);
        assert_eq!(entity.shadow_tint, 0x00FF_FF50);
        assert_eq!(game.entity_sounds.len(), 1, "the landing cue");
        assert_eq!(game.entity_sounds[0].column, 0);
        assert_eq!(game.effects.active_count(), 2, "the landing feathers");
        // The tail's per-frame quad size runs after the shrink, exactly like
        // the original's BillboardSetSize.
        assert_eq!(entity.shadow_half_x, 371);

        // The off-map step (rewritten to the sentinel) bursts every joint and
        // frees the entity.
        let mut game = crow_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        game.entities[0].pos = [2000, 0, 1000];
        {
            let e = &mut game.entities[1];
            e.set_state(3);
            e.action_behavior = 0;
            e.action_state = 0;
            e.hit_state = 0;
            e.pos[1] = -500;
            e.floor_step = -2501;
        }
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(
            game.entities[1].action_behavior, 2,
            "the guard rewrite sends it straight to the burst"
        );
        game.entity_sounds.clear();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.joint_flags, (1 << 13) - 1, "every joint hidden");
        assert_eq!(entity.action_behavior, 4);
        assert_eq!(entity.status_flags & 0x0A, 0x0A, "intangible and free");
        assert_eq!(game.entity_sounds.len(), 1, "the burst cue");
        // The burst zeroes the quad, then the tail re-sizes it from the
        // altitude, exactly like the original's BillboardSetSize order.
        assert_eq!(entity.shadow_half_x, 381);
        assert_eq!(entity.shadow_half_z, 381);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_tail_clamps_and_shadow_gate() {
        let pack = pack(&[("enemy/em05.lua", crow_source())]);
        let (clips, _, _) = crow_model();
        let mut game = crow_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));

        // The floor clamp rewrites the state block outside the message gate.
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(2);
        game.entities[1].action_behavior = 0;
        game.entities[1].pos[1] = 50;
        game.message_flags &= !crate::game::MESSAGE_FLAG_MONSTERS;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.pos[1], -100);
        assert_eq!(entity.reaction_timer, 200);
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.ignore(), 1);
        assert_eq!(entity.action_behavior, 7);
        assert_eq!(entity.shadow_half_x, 393, "(Y // 16) + 400");
        assert_eq!(entity.shadow_half_z, 393);

        // The ceiling clamp does the same at -30000.
        game.entities[1].set_ignore(2);
        game.entities[1].pos[1] = -30001;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.pos[1], -29000);
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.ignore(), 1);
        assert_eq!(entity.action_behavior, 10);

        // The shadow queue gate: no shadow while perched, no shadow below the
        // floor-step floor, and no shadow outside the camera zone.
        game.message_flags |= crate::game::MESSAGE_FLAG_MONSTERS;
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(0);
        game.entities[1].action_behavior = 0;
        game.entities[1].pos[1] = -2500;
        game.entities[1].floor_step = 1;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].shadow_half_x, 0, "a perched crow");
        game.entities[1].set_ignore(1);
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].shadow_half_x, 243, "airborne queues");
        // A deep floor step under the crow suppresses the quad: the resolve
        // reports it through the floor channel.
        let mut floor_room = crow_room();
        floor_room.collision.quadrants[0].push(crate::state::CollisionRect {
            x_max: 5000,
            z_max: 5000,
            x_min: 0,
            z_min: 0,
            kind: 0x8300,
            flags: 0x0004,
        });
        assert!(host.update(&mut game, &floor_room, &pack, 1, &clips));
        assert_eq!(game.entities[1].floor_step, -3399);
        assert_eq!(game.entities[1].shadow_half_x, 0, "a deep floor step");
        game.entities[1].pos = [9000, -2500, 9000];
        game.entities[1].saved_pos = Some(game.entities[1].pos);
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].shadow_half_x, 0, "outside the zone");
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_swerve_steers_when_stuck() {
        let pack = pack(&[("enemy/em05.lua", crow_source())]);
        let (clips, _, _) = crow_model();
        let mut game = crow_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        game.entities[0].pos = [0, 0, 0];
        {
            let e = &mut game.entities[1];
            e.set_state(1);
            e.set_ignore(1);
            e.behavior_flags = 0x10;
            e.action_behavior = 5;
            e.action_state = 0;
            e.pos = [1000, -2000, 1000];
            e.angle = 0;
            e.move_speed_current = 100;
            e.reaction_timer = 0;
            e.set_wasp_collision(1);
            e.set_groan_timer(0x1F);
            e.set_crow_ceil_limit(-3500);
            e.set_writhe_amplitude(0);
        }
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(
            entity.groan_timer() as u16,
            0,
            "the stuck counter resets at the swerve"
        );
        assert_eq!(entity.swerve, 0x40, "the right-hand sidestep");
        assert_eq!(entity.swerve_latch, 4, "four frames of held turn");
        assert_eq!(entity.angle & 0x40, 0x40, "the yaw took the delta");

        // While the crow keeps facing geometry, a fast crow instead parks in
        // the ram recoil.
        {
            let e = &mut game.entities[1];
            e.set_wasp_collision(1);
            e.set_groan_timer(0x1F);
            e.move_speed_current = 400;
            e.angle = 0;
        }
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].state(), 2);
        assert_eq!(game.entities[1].action_behavior, 0);

        // A slow blocked crow below the stuck threshold just counts while it
        // is being attacked.
        {
            let e = &mut game.entities[1];
            e.set_state(1);
            e.set_ignore(1);
            e.action_behavior = 5;
            e.set_wasp_collision(2);
            e.set_groan_timer(3);
            e.move_speed_current = 100;
        }
        game.entities[0].is_being_attacked = 1;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        assert_eq!(
            game.entities[1].groan_timer() as u16,
            4,
            "counting, reacting"
        );
        game.entities[0].is_being_attacked = 0;
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_pause_gate_holds_the_machine_but_still_clamps() {
        let pack = pack(&[("enemy/em05.lua", crow_source())]);
        let (clips, _, _) = crow_model();
        let mut game = crow_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        game.message_flags &= !crate::game::MESSAGE_FLAG_MONSTERS;

        game.entities[1].set_state(1);
        game.entities[1].set_ignore(0);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 0;
        game.entities[1].action_ticks_counter = 9;
        game.entities[1].pos[1] = 50;
        assert!(crow_step(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.action_ticks_counter, 9, "the perch is gated");
        assert_eq!(entity.pos[1], -100, "the clamp still runs");
        assert_eq!(entity.action_behavior, 7);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_script_sees_typed_scratch_properties() {
        let pack = pack(&[(
            "enemy/em05.lua",
            r#"
function update(e)
    e.crow_turn_rate = -0x1234
    e.crow_floor_limit = -0x0BB8
    e.crow_ceil_limit = -0x1194
    e.crow_vy = -5
    e.crow_dist = 0xBEEF
    e.crow_coll = 0xCAFE
    e.crow_stuck = 0x1234
    e.crow_phase = -3
    e.crow_path_word = 0x8001
    e.floor_step = -3399
    e.crow_swerve = -0x40
    e.crow_swerve_latch = 0x104
    e.crow_struggle = -129
    e.crow_alt_bias = -400
    assert(e.crow_path_bit == 1)
    assert(e.player_animation_id == 0)
    assert(e.player_animation_frame_id == 0)
    assert(e:player_mashing() == false)

    e.angle = 0
    local before = e.pos_x
    e:move(0, 100)
    local after_move = e.pos_x
    e:advance_speed()
    assert(e.pos_x == after_move + (after_move - before))

    e.crow_swerve = 0
    e.crow_swerve_latch = 0
    local delta = e:swerve(0x40, true, 4)
    assert(delta == 0x40)
    assert(e.crow_swerve == 0x40)
    assert(e.crow_swerve_latch == 4)
end
"#,
        )]);
        let mut game = crow_game(0);
        let mut host = LuaEnemyHost::new();
        assert!(crow_step(&mut host, &mut game, &pack, &[]));
        let entity = &game.entities[1];
        assert_eq!(entity.writhe_velocity(), -0x1234, "turn rate");
        assert_eq!(entity.room_collision(), 0xF448, "floor limit");
        assert_eq!(entity.sca_active(), 0xEE6C, "ceiling limit");
        assert_eq!(entity.reaction_timer, -5, "vertical velocity");
        assert_eq!(entity.wasp_distance(), 0xBEEF, "distance");
        assert_eq!(entity.wasp_collision(), 0xCAFE, "collision");
        assert_eq!(entity.subpixel_pos_x, 0xCAFE_BEEFu32 as i32);
        assert_eq!(entity.groan_timer() as u16, 0x1234, "stuck");
        assert_eq!(entity.sink_wobble(), -3);
        assert_eq!(entity.writhe_amplitude(), 0x8001u16 as i16);
        assert_eq!(entity.floor_step, -3399);
        assert_eq!(entity.swerve, 0x40, "the swerve probe's fresh pick");
        assert_eq!(entity.swerve_latch, 4);
        assert_eq!(entity.struggle, 127, "-129 wraps through the signed byte");
        assert_eq!(entity.alt_bias, -400);
        assert_eq!(entity.tex_bank & 1, 1);
        assert_eq!(entity.pos[0], 1000 + 99 + 99, "move then advance_speed");
    }

    #[cfg(feature = "lua")]
    #[test]
    fn crow_reset_between_frames_preserves_the_scene() {
        let pack = pack(&[("enemy/em05.lua", crow_source())]);
        let (clips, keyframes, skeleton) = crow_model();
        let mut cached_game = crow_game(0);
        let mut reset_game = crow_game(0);
        for game in [&mut cached_game, &mut reset_game] {
            game.entity_anims[1].keyframes = Some(keyframes.clone());
            game.entity_anims[1].skeleton = Some(skeleton.clone());
            game.entities[0].pos = [1500, 0, 1000];
            game.entities[0].action_behavior = 0x14;
            game.entities[0].action_state = 0;
            game.entities[0].set_state(1);
            game.entities[0].set_ignore(3);
            game.rand_state = 0x1234;
        }
        let mut cached = LuaEnemyHost::new();
        let mut reset = LuaEnemyHost::new();

        // A scene spanning init, the perch, the scatter takeoff, the glide,
        // the peck, the grab, the recoil, the landing death and an SCD leg.
        let scenario: &[(u8, u8, u8)] = &[
            (0, 0, 0),
            (1, 0, 0),
            (1, 0, 13),
            (1, 1, 13),
            (1, 1, 1),
            (1, 1, 5),
            (1, 1, 9),
            (1, 1, 11),
            (2, 0, 0),
            (2, 1, 0),
            (3, 0, 0),
            (3, 1, 1),
            (3, 2, 2),
            (3, 4, 4),
            (4, 0, 0),
            (8, 0, 1),
        ];
        for (tick, &(state, ignore, behavior)) in scenario.iter().enumerate() {
            for game in [&mut cached_game, &mut reset_game] {
                game.entities[1].set_state(state);
                game.entities[1].set_ignore(ignore);
                game.entities[1].action_behavior = behavior;
                game.entities[1].action_state = 0;
                if state == 3 {
                    game.entities[1].floor_step = 1;
                }
            }
            assert!(cached.update(&mut cached_game, &crow_room(), &pack, 1, &clips));
            reset.reset();
            assert!(reset.update(&mut reset_game, &crow_room(), &pack, 1, &clips));
            assert_eq!(
                cached_game, reset_game,
                "the crow scene diverged at tick {tick} (state {state})"
            );
        }
    }

    #[cfg(feature = "lua")]
    #[test]
    fn scalar_properties_read_write_and_truncate() {
        let pack = pack(&[(
            "enemy/em13.lua",
            r#"
function update(e)
    e.health = -1
    e.angle = 0x10005
    e.state = -1
    e.status_flags = 0x1FF
    e.shadow_tint = 0x100FFFFFF
    -- Read-only properties refuse assignment.
    assert(not pcall(function() e.id = 5 end))
    assert(not pcall(function() e.player_character = 1 end))
end
"#,
        )]);
        let mut game = spiderweb_game();
        let mut host = LuaEnemyHost::new();
        assert!(update(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.health, -1, "i16 store");
        assert_eq!(entity.angle, 5, "u16 truncation");
        assert_eq!(entity.state(), 0xFF, "u8 truncation");
        assert_eq!(entity.status_flags, 0xFF);
        assert_eq!(entity.shadow_tint, 0x00FF_FFFF, "u32 truncation");
    }

    #[cfg(feature = "lua")]
    #[test]
    fn reset_between_frames_preserves_behaviour() {
        let pack = pack(&[("enemy/em13.lua", spiderweb_source())]);
        let mut cached_game = spiderweb_game();
        let mut reset_game = spiderweb_game();
        let mut cached = LuaEnemyHost::new();
        let mut reset = LuaEnemyHost::new();

        // A scene that walks every state: init, idle, an ignored hit, a knife
        // hit, another idle frame and the destroy state.
        let mut scenario = vec![
            (0u8, 0u8),
            (1, 0),
            (2, 0x10),
            (1, 0),
            (2, 0x08),
            (1, 0),
            (3, 0),
        ];
        scenario.push((4, 0));
        for (state, hit_state) in scenario {
            for game in [&mut cached_game, &mut reset_game] {
                game.entities[1].set_state(state);
                game.entities[1].hit_state = hit_state;
            }
            assert!(update(&mut cached, &mut cached_game, &pack));
            reset.reset();
            assert!(update(&mut reset, &mut reset_game, &pack));
            assert_eq!(
                cached_game, reset_game,
                "state after {state}/{hit_state:#04x}"
            );
        }
    }

    #[cfg(feature = "lua")]
    #[test]
    fn require_loads_pack_modules_relative_and_absolute() {
        let util = "local m = {} function m.set_health(e, v) e.health = v end return m";
        let cases: &[(&str, i16)] = &[
            ("require(\"lib/util\")", 11),
            ("require(\"./lib/util\")", 12),
            ("require(\"/enemy/lib/util\")", 13),
            ("require(\"enemy/lib/util.lua\")", 14),
        ];
        for &(call, expected) in cases {
            let script = format!(
                "local util = {call}\nfunction update(e) util.set_health(e, {expected}) end"
            );
            let pack = pack(&[("enemy/lib/util.lua", util), ("enemy/em13.lua", &script)]);
            let mut game = spiderweb_game();
            let mut host = LuaEnemyHost::new();
            assert!(update(&mut host, &mut game, &pack), "{call}");
            assert_eq!(game.entities[1].health, expected, "{call}");

            // Modules are recreated with the VM, so a reset keeps working.
            game.entities[1].health = 0;
            host.reset();
            assert!(update(&mut host, &mut game, &pack), "{call} after reset");
            assert_eq!(game.entities[1].health, expected, "{call} after reset");
        }
    }

    #[cfg(feature = "lua")]
    #[test]
    fn require_caches_modules_and_supports_nested_and_runtime_imports() {
        // helper is required relatively from inside util; em13 requires util
        // lazily from inside update and checks the two spellings share one
        // cached module.
        let pack = pack(&[
            ("enemy/lib/helper.lua", "return { count = 0 }"),
            (
                "enemy/lib/util.lua",
                "local h = require(\"./helper\")\nlocal m = { helper = h }\nh.count = h.count + 1\nreturn m",
            ),
            (
                "enemy/em13.lua",
                "function update(e)\n local a = require(\"lib/util\")\n local b = require(\"enemy/lib/util\")\n e.health = (a == b and a.helper.count or 0)\nend",
            ),
        ]);
        let mut game = spiderweb_game();
        let mut host = LuaEnemyHost::new();
        assert!(update(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].health, 1, "one cached module instance");

        // The second tick lazily requires the same cached modules again.
        assert!(update(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].health, 1);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn require_errors_on_missing_and_escaping_modules() {
        for name in ["nope", "../outside", "/../outside", ""] {
            let script =
                format!("local m = require(\"{name}\")\nfunction update(e) e.health = 1 end");
            let pack = pack(&[("enemy/em13.lua", script.as_str())]);
            let mut game = spiderweb_game();
            let mut host = LuaEnemyHost::new();
            assert!(!update(&mut host, &mut game, &pack), "module {name:?}");
            assert_eq!(host.failed_scripts(), 1, "module {name:?}");
        }
    }

    #[cfg(feature = "lua")]
    #[test]
    fn a_failing_script_parks_only_its_own_id() {
        let pack = pack(&[
            ("enemy/em0a.lua", "function update(e) error('boom') end"),
            ("enemy/em13.lua", spiderweb_source()),
        ]);
        let mut game = spiderweb_game();
        game.entities[2].id = 0x0A;
        game.entities[2].set_active(true);
        let mut host = LuaEnemyHost::new();

        assert!(!host.update(&mut game, &RoomState::default(), &pack, 2, &[]));
        assert!(
            update(&mut host, &mut game, &pack),
            "the other id still runs"
        );
        assert_eq!(game.entities[1].state(), 1);
        assert_eq!(host.loaded_scripts(), 1);
    }

    /// The checked-in roots script, so the tests exercise the real port.
    #[cfg(feature = "lua")]
    fn roots_source() -> &'static str {
        crate::enemy::ENEMY_SCRIPTS
            .iter()
            .find(|(path, _)| *path == "enemy/em0e.lua")
            .expect("the checked-in roots script")
            .1
    }

    /// One active roots entity in slot 1, spawned in room 40F0's slot 0 pose
    /// with the story's death event.
    #[cfg(feature = "lua")]
    fn roots_game(behavior: u8) -> GameState {
        let mut game = GameState::default();
        let entity = &mut game.entities[1];
        entity.id = 0x0e;
        entity.set_active(true);
        entity.status_flags = 1;
        entity.behavior_flags = behavior;
        entity.variant = 0;
        entity.pos = [6695, -5500, 2896];
        entity.angle = 0x0C00;
        entity.death_event_id = 3;
        game.enemy_count = 1;
        game
    }

    /// The roots rooms' sound-table row (stage 4, room 0x0F, row 102), whose
    /// enemy columns name the two groans.
    #[cfg(feature = "lua")]
    fn roots_room() -> RoomState {
        RoomState {
            stage: 4,
            room: 0x0F,
            ..RoomState::default()
        }
    }

    #[cfg(feature = "lua")]
    fn roots_step(
        host: &mut LuaEnemyHost,
        game: &mut GameState,
        pack: &Pack,
        room: &RoomState,
    ) -> bool {
        host.update(game, room, pack, 1, &[])
    }

    #[cfg(feature = "lua")]
    #[test]
    fn roots_script_initialises_for_every_spawn_kind() {
        let pack = pack(&[("enemy/em0e.lua", roots_source())]);
        for behavior in [0x80u8, 0x01, 0x00] {
            let mut game = roots_game(behavior);
            let mut host = LuaEnemyHost::new();
            assert!(
                roots_step(&mut host, &mut game, &pack, &RoomState::default()),
                "kind {behavior:#04x}"
            );
            let entity = &game.entities[1];
            assert_eq!(entity.state(), 1, "init moves to idle");
            assert_eq!(entity.behavior_flags, behavior, "the spawn kind survives");
            assert_eq!(entity.health, 1);
            assert_eq!(entity.sca_radius, 0x07D0);
            assert_eq!(entity.sca_half_height, 0x1770);
            assert_eq!(entity.sca_offset, [0, 0, 0]);
            assert_eq!(entity.status_flags, (1 & 0x1F) | 0x04);
            assert_eq!(entity.joint_scale, 0x1000);
            assert_eq!(entity.hit_state, 1);
            assert_eq!(entity.stored_pos_x(), 6695);
            assert_eq!(entity.stored_pos_z(), 2896);
            assert_eq!(entity.sink_wobble(), 2);
            assert_eq!(entity.groan_timer(), 0x28);
            assert_eq!(entity.shadow_offset, [0, 0, -0x50]);
            assert_eq!(entity.shadow_half_x, 0xC00);
            assert_eq!(entity.shadow_half_z, 0xC00);
            assert_eq!(entity.shadow_tint, 0x0040_4040);
            assert_eq!((entity.is_moving, entity.move_max_steps), (0, 0));
            assert_eq!(entity.animation_id, 0);
            assert_eq!(entity.animation_frame_id, 0);
            assert_eq!(entity.timing_control, 0);
            assert_eq!(entity.blend_counter, 0);
            assert!(game.entity_sounds.is_empty(), "init queues no cue");
            assert_eq!(entity.model_tint, [0, 0, 0]);
        }
    }

    #[cfg(feature = "lua")]
    #[test]
    fn roots_dormant_kind_never_moves() {
        let pack = pack(&[("enemy/em0e.lua", roots_source())]);
        let mut game = roots_game(0x80);
        let mut host = LuaEnemyHost::new();
        let before = game.rand_state;
        for _ in 0..5 {
            assert!(roots_step(
                &mut host,
                &mut game,
                &pack,
                &RoomState::default()
            ));
        }
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 0, "the phase latch stays empty");
        assert_eq!(entity.joint_scale, 0x1000);
        assert_eq!(entity.health, 1);
        assert_eq!(entity.status_flags & 0xE0, 0xE0, "targetable every frame");
        assert_eq!(game.rand_state, before, "dormant draws nothing");
        assert!(game.entity_sounds.is_empty());
    }

    #[cfg(feature = "lua")]
    #[test]
    fn roots_retract_kind_collapses_once_and_pins_the_spawn_point() {
        let pack = pack(&[("enemy/em0e.lua", roots_source())]);
        let mut game = roots_game(0x01);
        let mut host = LuaEnemyHost::new();
        let room = RoomState::default();
        assert!(roots_step(&mut host, &mut game, &pack, &room));

        // First idle frame: the one-shot retract.
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        let entity = &game.entities[1];
        assert_eq!(
            entity.ignore(),
            1,
            "the phase latch parks the sub-behaviour"
        );
        assert_eq!(entity.joint_scale, 0x9C4);
        assert_eq!(entity.status_flags & 0x02, 0x02);
        assert_eq!(entity.health, -1);
        assert_eq!(entity.tint_queue, [0, -6, -12]);
        assert_eq!(entity.tint_queue_word_a, 0);
        assert_eq!(entity.tint_queue_word_b, 0x100);
        assert!(entity.tint_queue_armed);
        assert_eq!(
            entity.model_tint,
            [0, 0, 0],
            "retargeting the queue does not tint the live model"
        );
        assert_eq!(entity.shadow_half_x, 0xC00 - 2000);
        assert_eq!(entity.shadow_half_z, 0xC00 - 2000);

        // Every later frame falls through the phase latch; the position pin
        // snaps the mass back to the frozen spawn point.
        game.entities[1].pos[0] += 321;
        game.entities[1].pos[2] -= 99;
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        let entity = &game.entities[1];
        assert_eq!(entity.joint_scale, 0x9C4, "the collapse does not repeat");
        assert_eq!(entity.tint_queue, [0, -6, -12]);
        assert_eq!((entity.pos[0], entity.pos[2]), (6695, 2896));
    }

    #[cfg(feature = "lua")]
    #[test]
    fn roots_writhe_cycle_runs_its_phase_table_and_flashes() {
        let pack = pack(&[("enemy/em0e.lua", roots_source())]);
        let mut game = roots_game(0x00);
        let mut host = LuaEnemyHost::new();
        let room = RoomState::default();

        // Call 0 is init; call 1 runs phase 0 (start) plus the first bob.
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 1, "phase 0 bumps to 1");
        assert_eq!(entity.writhe_velocity(), 0);
        assert_eq!(entity.writhe_amplitude(), 0);
        assert_eq!(entity.action_ticks_counter, 8);
        assert_eq!(entity.tint_flashes(), 6);
        assert_eq!(entity.health, -1);
        assert_eq!(entity.status_flags & 0x02, 0x02);

        // Eleven rise ticks ramp the amplitude to the cap and bump the phase.
        for _ in 0..11 {
            assert!(roots_step(&mut host, &mut game, &pack, &room));
        }
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 2);
        assert_eq!(entity.writhe_amplitude(), 0x1600);
        // The bob drives the altitude from the sign-flipped amplitude.
        assert_ne!(entity.pos[1], -5500);

        // The ninth shrink tick packs the cadence word and fires the first
        // tint flash.
        for _ in 0..9 {
            assert!(roots_step(&mut host, &mut game, &pack, &room));
        }
        let entity = &game.entities[1];
        assert_eq!(entity.tint_flashes(), 5);
        assert_eq!(entity.tint_queue, [0, -1, -2]);
        assert_eq!(entity.tint_queue_word_b, 0x100);
        assert_eq!(entity.model_tint, [0, -31, -2], "the flash tints the model");
        assert_eq!(entity.joint_scale, 0x1000 - 8 * 9);

        // Run the rest of the shrink phase; the exit branch parks the phase at
        // 3 with the cadence and flash words collapsed to one.
        while game.entities[1].ignore() == 2 {
            assert!(roots_step(&mut host, &mut game, &pack, &room));
        }
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 3);
        assert_eq!(
            entity.joint_scale,
            0x1000 - 8 * 200,
            "the scale bottoms out"
        );
        assert_eq!(entity.action_ticks_counter, 1);
        assert_eq!(entity.tint_flashes(), 1);

        // The sink phase bleeds the amplitude down, wobbles every fourth tick
        // and raises the death event when it bottoms out.
        while game.entities[1].ignore() == 3 {
            assert!(roots_step(&mut host, &mut game, &pack, &room));
        }
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 4);
        assert!(entity.writhe_amplitude() <= 0);
        assert!(entity.sink_wobble() > 2, "the wobble word ticked up");
        assert!(game.flags[usize::from(crate::game::BANK_ENEMIES)].bit(3));

        // Phase 4 stops the cycle: the machine holds still from here on.
        let latched = *entity;
        for _ in 0..3 {
            assert!(roots_step(&mut host, &mut game, &pack, &room));
        }
        assert_eq!(game.entities[1], latched);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn roots_damage_reset_rewinds_the_cycle() {
        let pack = pack(&[("enemy/em0e.lua", roots_source())]);
        let mut game = roots_game(0x00);
        let mut host = LuaEnemyHost::new();
        let room = RoomState::default();
        for _ in 0..4 {
            assert!(roots_step(&mut host, &mut game, &pack, &room));
        }
        assert_eq!(game.entities[1].ignore(), 1);
        game.entities[1].set_state(2);
        game.entities[1].hit_state = 0x10;
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.hit_state, 0);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn roots_raw_phase_states_dispatch_the_phase_table() {
        let pack = pack(&[("enemy/em0e.lua", roots_source())]);
        let mut game = roots_game(0x80);
        let mut host = LuaEnemyHost::new();
        let room = RoomState::default();
        assert!(roots_step(&mut host, &mut game, &pack, &room));

        // Raw state 6 starts a cycle without the idle bob.
        game.entities[1].set_state(6);
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        assert_eq!(game.entities[1].ignore(), 1);
        assert_eq!(game.entities[1].writhe_amplitude(), 0);

        // Raw state 7 is one rise step; raw state 9 one sink step.
        game.entities[1].set_state(7);
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        assert_eq!(game.entities[1].writhe_amplitude(), 0x200);
        game.entities[1].set_state(9);
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        assert_eq!(game.entities[1].writhe_amplitude(), 0x200 - 0x70);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn roots_groan_timer_draws_in_order_and_queues_the_enemy_bank() {
        let pack = pack(&[("enemy/em0e.lua", roots_source())]);
        let room = roots_room();
        let mut game = roots_game(0x00);
        let mut host = LuaEnemyHost::new();
        assert!(roots_step(&mut host, &mut game, &pack, &room));

        // Park the timer one tick from expiry and run a single writhe frame:
        // two low-bit draws reload it, then the one-in-32 roll draws once.
        game.entities[1].set_groan_timer(1);
        game.rand_state = 1;
        let mut expected_state = 1u32;
        let first = crate::game::platform_rand(&mut expected_state) & 7;
        let second = crate::game::platform_rand(&mut expected_state) & 7;
        let roll = crate::game::platform_rand(&mut expected_state) & 0x1F;
        assert_ne!(roll, 1, "seed 1 picks the single-groan branch");
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        assert_eq!(
            game.entities[1].groan_timer(),
            (first + second + 0xC) as i16
        );
        assert_eq!(game.rand_state, expected_state, "three draws in order");
        assert_eq!(game.entity_sounds.len(), 1);
        let sound = game.entity_sounds[0];
        assert_eq!(sound.name, "pakiB");
        assert_eq!((sound.bank, sound.column), (2, 0x19));
        assert_eq!(sound.pos, game.entities[1].pos);

        // A seed whose roll lands on one queues the second groan too.
        let lucky = (1u32..)
            .find(|&seed| {
                let mut state = seed;
                crate::game::platform_rand(&mut state);
                crate::game::platform_rand(&mut state);
                crate::game::platform_rand(&mut state) & 0x1F == 1
            })
            .unwrap();
        game.entity_sounds.clear();
        game.entities[1].set_groan_timer(1);
        game.rand_state = lucky;
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        assert_eq!(
            game.entity_sounds.len(),
            2,
            "the roll queues a second groan"
        );

        // Kind 0x80 never reaches the writhe sub-behaviour, so no draws.
        let mut game = roots_game(0x80);
        let before = game.rand_state;
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        assert_eq!(game.rand_state, before);
        assert!(game.entity_sounds.is_empty());
    }

    #[cfg(feature = "lua")]
    #[test]
    fn roots_cycle_start_cue_is_out_of_the_enemy_helper_range() {
        let pack = pack(&[("enemy/em0e.lua", roots_source())]);
        let room = roots_room();
        // The script's phase-0 call requests id 0x18; the shared helper only
        // accepts the group-relative ids 0..=9, so it queues nothing.
        let mut game = roots_game(0x00);
        let mut host = LuaEnemyHost::new();
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        assert!(
            game.entity_sounds.is_empty(),
            "the cycle-start cue is inert, exactly like the original"
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn roots_pause_gate_holds_the_machine_but_still_pins() {
        let pack = pack(&[("enemy/em0e.lua", roots_source())]);
        let mut game = roots_game(0x01);
        let mut host = LuaEnemyHost::new();
        let room = RoomState::default();
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        game.message_flags &= !crate::game::MESSAGE_FLAG_MONSTERS;

        // The state machine and the collision pass are gated; the position
        // pin and the camera-zone word still run.
        game.entities[1].pos[0] += 500;
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 0, "move_b did not run");
        assert_eq!(entity.joint_scale, 0x1000);
        assert_eq!(entity.health, 1);
        assert_eq!(entity.pos[0], 6695, "the pin runs outside the gate");

        // Releasing the gate runs the parked first idle frame.
        game.message_flags |= crate::game::MESSAGE_FLAG_MONSTERS;
        assert!(roots_step(&mut host, &mut game, &pack, &room));
        assert_eq!(game.entities[1].ignore(), 1);
        assert_eq!(game.entities[1].joint_scale, 0x9C4);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn roots_reset_between_frames_preserves_the_cycle() {
        let pack = pack(&[("enemy/em0e.lua", roots_source())]);
        let room = roots_room();
        let mut cached_game = roots_game(0x00);
        let mut reset_game = roots_game(0x00);
        let mut cached = LuaEnemyHost::new();
        let mut reset = LuaEnemyHost::new();

        // Twenty-five ticks span the init, the whole rise and the first tint
        // flash, so the reset path re-reads every scratch word.
        for tick in 0..25 {
            assert!(roots_step(&mut cached, &mut cached_game, &pack, &room));
            reset.reset();
            assert!(roots_step(&mut reset, &mut reset_game, &pack, &room));
            assert_eq!(
                cached_game, reset_game,
                "the roots state diverged at tick {tick}"
            );
        }
        assert_eq!(cached_game.entities[1].ignore(), 2);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn roots_script_sees_typed_scratch_properties() {
        let pack = pack(&[(
            "enemy/em0e.lua",
            r#"
function update(e)
    -- Signed words round-trip through the entity scratch bytes.
    e.writhe_velocity = -0x1234
    e.writhe_amplitude = 0x1600
    e.tint_flashes = -2
    e.groan_timer = 0x28
    e.sink_wobble = -3
    e.stored_pos_x = -0x00071234
    e.stored_pos_z = 0x12345678
    e:tint_model(0, -1, -2, 0, 0x100)
    assert(e.model_tint_g == 0)
    assert(e.tint_queue_g == -1)
    assert(e.tint_queue_armed == true)
end
"#,
        )]);
        let mut game = roots_game(0x80);
        let mut host = LuaEnemyHost::new();
        assert!(roots_step(
            &mut host,
            &mut game,
            &pack,
            &RoomState::default()
        ));
        let entity = &game.entities[1];
        assert_eq!(entity.writhe_velocity(), -0x1234);
        assert_eq!(entity.writhe_amplitude(), 0x1600);
        assert_eq!(entity.tint_flashes(), -2);
        assert_eq!(entity.groan_timer(), 0x28);
        assert_eq!(entity.sink_wobble(), -3);
        assert_eq!(entity.stored_pos_x(), -0x0007_1234);
        assert_eq!(entity.stored_pos_z(), 0x1234_5678);
        assert_eq!(entity.tint_queue, [0, -1, -2]);
        assert_eq!(entity.tint_queue_word_b, 0x100);
        // The tint scan only matches within the live enemy count.
        assert_eq!(entity.model_tint, [0, -31, -2]);
    }
}

#[cfg(all(test, feature = "lua"))]
mod pause_tests {
    use super::*;
    use crate::game::{GameState, MESSAGE_FLAG_MONSTERS};
    use crate::pack::{Pack, PackWriter};
    use crate::state::RoomState;

    #[test]
    fn the_monster_pause_bit_gates_only_the_scripts_that_check_it() {
        let mut writer = PackWriter::new();
        writer
            .add(
                "enemy/em13.lua",
                b"function update(e) if e.monster_paused then return end e.health = 9 end".to_vec(),
            )
            .unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();

        let mut game = GameState::default();
        game.entities[1].id = 0x13;
        game.entities[1].set_active(true);
        let room = RoomState::default();
        let mut host = LuaEnemyHost::new();

        game.message_flags &= !MESSAGE_FLAG_MONSTERS;
        assert!(host.update(&mut game, &room, &pack, 1, &[]));
        assert_eq!(game.entities[1].health, 0, "the pause held the dispatch");

        game.message_flags |= MESSAGE_FLAG_MONSTERS;
        assert!(host.update(&mut game, &room, &pack, 1, &[]));
        assert_eq!(game.entities[1].health, 9);
    }
}

#[cfg(all(test, feature = "lua"))]
mod spider_tests {
    use std::sync::Arc;

    use crate::enemy::LuaEnemyHost;
    use crate::game::GameState;
    use crate::model::{Clip, ClipFrame, Keyframe, Skeleton};
    use crate::pack::{Pack, PackWriter};
    use crate::state::{RoomState, Zone};

    fn source(name: &str) -> &'static str {
        crate::enemy::ENEMY_SCRIPTS
            .iter()
            .find(|(path, _)| *path == name)
            .unwrap_or_else(|| panic!("the checked-in {name} script"))
            .1
    }

    fn pack(entries: &[(&str, &str)]) -> Pack {
        let mut writer = PackWriter::new();
        for (path, text) in entries {
            writer.add(path, text.as_bytes().to_vec()).unwrap();
        }
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    /// A 20-joint spider model with 16 one-tick clips.
    fn spider_model() -> (Vec<Clip>, Arc<Vec<Keyframe>>, Arc<Skeleton>) {
        let clips = (0..16)
            .map(|_| Clip {
                frames: (0..32)
                    .map(|_| ClipFrame {
                        keyframe: 0,
                        timing: 1,
                    })
                    .collect(),
            })
            .collect();
        let keyframes = vec![Keyframe {
            offset: [0, 0, 0],
            rotations: vec![[0, 0, 0]; 20],
        }];
        let skeleton = Skeleton {
            relative: vec![[0, 0, 0]; 20],
            children: (0..20)
                .map(|index| if index == 0 { vec![1] } else { vec![] })
                .collect(),
        };
        (clips, Arc::new(keyframes), Arc::new(skeleton))
    }

    fn spider_game(id: u8, kind: u8) -> GameState {
        use crate::effects::fixtures::{block, sprite};
        let mut game = GameState::default();
        for index in [0u8, 0x1E] {
            game.weapon_effects.sprites.push(sprite(
                index,
                std::array::from_fn(|_| vec![vec![block(1, 0, 0)]]),
            ));
        }
        let entity = &mut game.entities[1];
        entity.id = id;
        entity.set_active(true);
        entity.status_flags = 1;
        entity.behavior_flags = kind;
        entity.death_event_id = 3;
        entity.pos = [1000, 0, 1000];
        entity.saved_pos = Some(entity.pos);
        game.enemy_count = 1;
        game
    }

    fn spider_room() -> RoomState {
        RoomState {
            stage: 1,
            room: 1,
            zones: vec![Zone {
                cam_from: 0,
                cam_to: 0,
                corners: [[0, 0], [0, 5000], [5000, 5000], [5000, 0]],
            }],
            ..RoomState::default()
        }
    }

    fn step(host: &mut LuaEnemyHost, game: &mut GameState, pack: &Pack, clips: &[Clip]) -> bool {
        host.update(game, &spider_room(), pack, 1, clips)
    }

    #[test]
    fn webspinner_script_initialises_and_health_rolls() {
        let pack = pack(&[("enemy/em03.lua", source("enemy/em03.lua"))]);
        let mut game = spider_game(0x03, 0);
        game.rand_state = 7;
        let mut expected = 7u32;
        let _discarded = crate::game::platform_rand(&mut expected);
        let index = crate::game::platform_rand(&mut expected) & 0xF;
        let health = [
            0x63u8, 0x63, 0x63, 0x63, 0x77, 0x63, 0x63, 0x77, 0x63, 0x63, 0x63, 0x77, 0x63, 0x59,
            0x63, 0x63,
        ][index as usize];
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &[]));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.health, i16::from(health));
        assert_eq!(entity.sca_radius, 1000);
        assert_eq!(entity.sca_half_height, 180);
        assert_eq!(entity.sca_offset, [0, -180, 0]);
        assert_eq!(entity.joint_scale, 0);
        assert_eq!(entity.shadow_tint, 0x0080_8080);
        assert_eq!(entity.shadow_half_x, 0x4B0);
        assert_eq!(game.rand_state, expected, "init draw order");
    }

    #[test]
    fn webspinner_ceiling_spawn_hangs_and_suppresses_its_shadow() {
        let pack = pack(&[("enemy/em03.lua", source("enemy/em03.lua"))]);
        let mut game = spider_game(0x03, 2);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &[]));
        let entity = &game.entities[1];
        assert_eq!(entity.pos[1], -6136);
        assert_eq!(entity.pitch, 0x0800);
        assert!(entity.shadow_suppressed);
        assert_eq!(entity.behavior_flags, 2);
    }

    #[test]
    fn blacktiger_script_initialises() {
        let pack = pack(&[("enemy/em04.lua", source("enemy/em04.lua"))]);
        let mut game = spider_game(0x04, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &[]));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.health, 0xCC);
        assert_eq!(entity.sca_radius, 2500);
        assert_eq!(entity.sca_offset, [-500, -180, 0]);
        assert_eq!(entity.sca2, None);
        assert_eq!(entity.joint_scale, 0x1B33);
        assert_eq!(entity.shadow_tint, 0x0040_4040);
        assert_eq!(entity.shadow_half_x, 0x9C4);
        assert_eq!(entity.groan_timer(), 0, "the web index");
        assert_eq!(entity.sink_wobble(), 0x2D, "the cooldown");
        assert_eq!(entity.reaction_timer, 0, "the trail");
    }

    #[test]
    fn webspinner_picker_commits_to_the_approach() {
        let pack = pack(&[("enemy/em03.lua", source("enemy/em03.lua"))]);
        let mut game = spider_game(0x03, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &[]));

        // Player close, path clear, delay armed so the picker draws nothing
        // but the approach setup roll.
        game.entities[0].pos = [1500, 0, 1000];
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(0);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 0;
        game.entities[1].set_room_collision(1);
        game.entities[1].set_writhe_amplitude(0);
        game.rand_state = 5;
        let mut expected = 5u32;
        let roll = crate::game::platform_rand(&mut expected) & 0x1F;
        assert!(step(&mut host, &mut game, &pack, &[]));

        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 1);
        assert_eq!(entity.action_behavior, 3);
        assert_eq!(
            entity.action_state, 2,
            "the setup frame falls through to the turn body and snaps"
        );
        assert_eq!(entity.animation_id, 2);
        assert_eq!(entity.writhe_velocity(), 0x80);
        assert_eq!(entity.action_ticks_counter, (roll + 0x50) - 1);
        assert_eq!(game.rand_state, expected, "one setup draw");
        assert_eq!(
            entity.room_collision(),
            0,
            "the delay counted down after the picker"
        );
    }

    #[test]
    fn webspinner_walk_turn_steps_the_speed_and_counts_down() {
        let pack = pack(&[("enemy/em03.lua", source("enemy/em03.lua"))]);
        let mut game = spider_game(0x03, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &[]));

        game.entities[0].pos = [1500, 0, 1000];
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 1;
        game.entities[1].action_state = 0;
        game.entities[1].set_writhe_amplitude(1);
        game.entities[1].angle = 0;
        game.rand_state = 9;
        let mut expected = 9u32;
        let roll = crate::game::platform_rand(&mut expected) & 0x1F;
        assert!(step(&mut host, &mut game, &pack, &[]));

        let entity = &game.entities[1];
        assert_eq!(entity.action_state, 1);
        assert_eq!(
            entity.action_ticks_counter,
            (roll + 0x50) - 1,
            "the dwell counts down the setup frame"
        );
        assert_eq!(
            entity.move_speed_current, 0x3C,
            "the walk speed is added after the stride probe"
        );
        assert_eq!(game.rand_state, expected, "one setup draw");
    }

    #[test]
    fn webspinner_lunge_bites_and_writes_the_player_reaction() {
        let pack = pack(&[("enemy/em03.lua", source("enemy/em03.lua"))]);
        let mut game = spider_game(0x03, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &[]));

        game.entities[0].pos = [1200, 0, 1000];
        game.entities[0].health = 100;
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 6;
        game.entities[1].action_state = 5;
        game.entities[1].action_ticks_counter = 5;
        game.entities[1].set_tint_flashes(1);
        game.entities[1].angle = 0;
        assert!(step(&mut host, &mut game, &pack, &[]));

        let entity = &game.entities[1];
        assert_eq!(entity.action_state, 6, "the bite releases into the shake");
        assert_eq!(game.entities[0].health, 90);
        assert_eq!(game.entities[0].is_being_attacked, 2, "facing + 1");
        assert_eq!(game.entities[0].action_behavior, 0x67);
        assert_eq!(entity.move_speed_current, 300);

        // The second playthrough costs 18.
        let mut game = spider_game(0x03, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &[]));
        game.flags[usize::from(crate::game::BANK_SCENARIO)]
            .apply(crate::game::SCENARIO_FLAG_SECOND_PLAYTHROUGH, 0);
        game.entities[0].pos = [1200, 0, 1000];
        game.entities[0].health = 100;
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 6;
        game.entities[1].action_state = 5;
        game.entities[1].set_tint_flashes(1);
        assert!(step(&mut host, &mut game, &pack, &[]));
        assert_eq!(game.entities[0].health, 100 - 0x12);
    }

    #[test]
    fn webspinner_shoot_a_spawns_and_updates_the_threads() {
        let pack = pack(&[("enemy/em03.lua", source("enemy/em03.lua"))]);
        let mut game = spider_game(0x03, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &[]));

        game.entities[1].set_state(3);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 3;
        game.entities[1].action_ticks_counter = 1;
        game.entities[1].hit_state = 0x04;
        game.entities[1].angle = 0;
        let mut expected = game.rand_state;
        let draws: Vec<u16> = (0..8)
            .map(|_| crate::game::platform_rand(&mut expected) & 3)
            .collect();
        assert!(step(&mut host, &mut game, &pack, &[]));

        assert_eq!(game.entities[1].clone_count, 8);
        assert_eq!(game.entities[1].action_state, 6);
        assert_eq!(game.rand_state, expected, "one draw per clone");
        let mut index = game.entities[1].clone_head;
        let mut angles = Vec::new();
        let mut states = Vec::new();
        while let Some(i) = index {
            let clone = game.web_clones[usize::from(i)].as_ref().unwrap();
            assert_eq!(clone.owner, 1);
            angles.push(clone.entity.angle);
            states.push(clone.entity.action_state);
            index = clone.next;
        }
        assert_eq!(
            angles,
            (1..=8).rev().map(|r| r * 0x100).collect::<Vec<u16>>(),
            "the fan runs from the count down to one"
        );
        assert_eq!(
            states,
            draws
                .iter()
                .map(|draw| u8::from(draw & 3 == 0))
                .collect::<Vec<u8>>()
        );

        // The next frame keeps the threads flying: each state-0 clone draws
        // its four transition values.
        let mut expected = game.rand_state;
        for _ in 0..8 {
            let _ = crate::game::platform_rand(&mut expected) & 0x3F;
            let _ = crate::game::platform_rand(&mut expected) & 0x3F;
            let _ = crate::game::platform_rand(&mut expected) & 1;
            let _ = crate::game::platform_rand(&mut expected) & 0x1F;
        }
        assert!(step(&mut host, &mut game, &pack, &[]));
        assert_eq!(game.rand_state, expected, "four draws per clone state 0");
        assert!(game.web_clones.iter().flatten().all(|clone| clone.live));
        assert!(
            game.web_clones
                .iter()
                .flatten()
                .all(|clone| clone.entity.state() == 1)
        );
    }

    #[test]
    fn webspinner_threads_stick_to_the_moving_player() {
        let pack = pack(&[("enemy/em03.lua", source("enemy/em03.lua"))]);
        let mut game = spider_game(0x03, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &[]));

        game.entities[1].set_state(3);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 3;
        game.entities[1].action_ticks_counter = 1;
        game.entities[1].hit_state = 0x04;
        assert!(step(&mut host, &mut game, &pack, &[]));
        assert_eq!(game.entities[1].clone_count, 8);

        // The player stands on the spider and moves: the next thread tick
        // sticks every fan (they share the parent's position) and parks them.
        game.entities[0].pos = game.entities[1].pos;
        game.entities[0].move_speed_current = 5;
        let health = game.entities[0].health;
        assert!(step(&mut host, &mut game, &pack, &[]));
        let stuck = game
            .web_clones
            .iter()
            .flatten()
            .filter(|clone| clone.entity.status_flags == 0)
            .count();
        assert_eq!(stuck, 8, "every thread stuck to the moving player");
        assert!(game.web_clones.iter().flatten().all(|clone| {
            clone.entity.state() == 6
                && clone.entity.shadow_half_x == 300
                && clone.entity.shadow_tint == 0x00FF_55DF
        }));
        assert_eq!(game.entities[0].health, health, "sticking deals no damage");
        assert!(
            !game.entity_sounds.is_empty(),
            "the sticky cue queued at the clone position"
        );
    }

    #[test]
    fn blacktiger_walk_installs_the_two_volume_profile() {
        let pack = pack(&[("enemy/em04.lua", source("enemy/em04.lua"))]);
        let mut game = spider_game(0x04, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &[]));

        game.entities[0].pos = [1500, 0, 1000];
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 1;
        game.entities[1].action_state = 0;
        game.entities[1].set_writhe_amplitude(1);
        game.entities[1].angle = 0;
        game.rand_state = 11;
        let mut expected = 11u32;
        let roll = crate::game::platform_rand(&mut expected) & 0x1F;
        assert!(step(&mut host, &mut game, &pack, &[]));

        let entity = &game.entities[1];
        assert_eq!(entity.action_state, 1);
        assert_eq!(entity.action_ticks_counter, (roll + 0x50) - 1);
        assert_eq!(entity.wasp_collision(), 6);
        assert_eq!(entity.sca_radius, 2000);
        assert_eq!(entity.sca_half_height, 180);
        assert_eq!(entity.sca_offset, [0, -180, 1000]);
        let second = entity.sca2.expect("the walk profile's rear box");
        assert_eq!(second.radius, 2000);
        assert_eq!(second.offset, [0, -180, -1000]);
        assert_eq!(game.rand_state, expected);
    }

    #[test]
    fn blacktiger_bite_reach_damages_the_player() {
        let pack = pack(&[("enemy/em04.lua", source("enemy/em04.lua"))]);
        let mut game = spider_game(0x04, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &[]));

        // Stage the posed fang joints at the player's position so both reach
        // probes connect.
        game.entities[0].pos = [1200, 0, 1000];
        game.entities[0].health = 100;
        game.joint_worlds[1] = vec![
            crate::anim::Mat4x3 {
                r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                t: [1200, 0, 1000],
            };
            20
        ];
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 0xC;
        game.entities[1].action_state = 5;
        game.entities[1].animation_frame_id = 0xC;
        assert!(step(&mut host, &mut game, &pack, &[]));

        assert_eq!(game.entities[0].health, 80);
        assert_eq!(game.entities[0].is_being_attacked, 2);
        assert_eq!(game.entities[0].action_behavior, 0x67);
    }

    #[test]
    fn blacktiger_shoot_c_spawns_the_variable_thread_count() {
        let pack = pack(&[("enemy/em04.lua", source("enemy/em04.lua"))]);
        let mut game = spider_game(0x04, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &[]));

        game.entities[1].set_state(3);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 1;
        game.entities[1].action_state = 4;
        game.entities[1].hit_state = 0x08; // (8 >> 3) * 5 + 1 = 6 threads
        game.entities[1].angle = 0;
        let mut expected = game.rand_state;
        let draws: Vec<u16> = (0..6)
            .map(|_| crate::game::platform_rand(&mut expected) & 3)
            .collect();
        // The spawn frame falls through to the case-5 flight: six state-0
        // clones each draw their four transitions, then the two steering
        // draws.
        for _ in 0..6 {
            let _ = crate::game::platform_rand(&mut expected) & 0x3F;
            let _ = crate::game::platform_rand(&mut expected) & 0x3F;
            let _ = crate::game::platform_rand(&mut expected) & 1;
            let _ = crate::game::platform_rand(&mut expected) & 0x1F;
        }
        let _ = crate::game::platform_rand(&mut expected) & 7;
        let _ = crate::game::platform_rand(&mut expected);
        assert!(step(&mut host, &mut game, &pack, &[]));

        assert_eq!(game.entities[1].clone_count, 6);
        assert_eq!(game.rand_state, expected, "the spawn and flight draws");
        let mut index = game.entities[1].clone_head;
        let mut count = 0;
        while let Some(i) = index {
            let clone = game.web_clones[usize::from(i)].as_ref().unwrap();
            assert_eq!(clone.entity.action_state, u8::from(draws[count] & 3 == 0));
            count += 1;
            index = clone.next;
        }
        assert_eq!(count, 6);
    }

    #[test]
    fn spider_script_sees_typed_scratch_properties() {
        let pack = pack(&[(
            "enemy/em03.lua",
            r#"
function update(e)
    e.ws_turn = -0x1234
    e.ws_path_word = 0xABCD
    e.ws_touch = -2
    e.ws_delay = -3
    e.ws_splat = -4
    e.ws_trail = -5
    e.ws_count = -6
    e.ws_web_index = -7
    e:set_sca2(111, 22, 1, 2, 3)
    e:arm_joint_effect(19, 0x1E, 0x14, 3)
    e:set_joint_visible(20, false)
    e:set_joint_visible(31, false)
    assert(e:web_joint_use(7, 0) == true)
    assert(e:web_joint_use(7, 1) == false)
    assert(e:web_distance() == 2000)
end
"#,
        )]);
        let mut game = spider_game(0x03, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &[]));
        let entity = &game.entities[1];
        assert_eq!(entity.writhe_velocity(), -0x1234);
        assert_eq!(entity.writhe_amplitude() as u16, 0xABCD);
        assert_eq!(entity.tint_flashes(), -2);
        assert_eq!(entity.room_collision() as i16, -3);
        assert_eq!(entity.sca_active() as i16, -4);
        assert_eq!(entity.reaction_timer, -5);
        assert_eq!(entity.wasp_distance() as i16, -6);
        assert_eq!(entity.wasp_collision() as i16, -7);
        let second = entity.sca2.expect("the second volume");
        assert_eq!(second.radius, 111);
        assert_eq!(second.half_height, 22);
        assert_eq!(second.offset, [1, 2, 3]);
        assert_eq!(entity.joint_flags, (1 << 19) | (1 << 20) | (1 << 31));
        assert_eq!(entity.joint_armed, 1 << 19);
        assert_eq!(game.web_joint_registry[0][0], 7);
        assert_eq!(game.web_joint_registry[1], [0; 8]);
    }

    #[test]
    fn blacktiger_script_sees_typed_scratch_properties() {
        let pack = pack(&[(
            "enemy/em04.lua",
            r#"
function update(e)
    e.bt_turn = -0x4321
    e.bt_path_word = 0x1234
    e.bt_touch = -8
    e.bt_delay = -9
    e.bt_splat = -10
    e.bt_trail = -11
    e.bt_count = -12
    e.bt_cflag = -13
    e.bt_web_index = -14
    e.bt_cooldown = -15
    assert(e:web_joint_use(0x11, 0) == true)
    e:set_sca(100, 0, 0, 0, 0)
end
"#,
        )]);
        let mut game = spider_game(0x04, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &[]));
        let entity = &game.entities[1];
        assert_eq!(entity.writhe_velocity(), -0x4321);
        assert_eq!(entity.writhe_amplitude() as u16, 0x1234);
        assert_eq!(entity.tint_flashes(), -8);
        assert_eq!(entity.room_collision() as i16, -9);
        assert_eq!(entity.sca_active() as i16, -10);
        assert_eq!(entity.reaction_timer, -11);
        assert_eq!(entity.wasp_distance() as i16, -12);
        assert_eq!(entity.wasp_collision() as i16, -13);
        assert_eq!(entity.groan_timer(), -14);
        assert_eq!(entity.sink_wobble(), -15);
        assert_eq!(game.web_joint_registry[0], [0; 8]);
        assert_eq!(game.web_joint_registry[1][0], 0x11);
        assert_eq!(entity.sca_radius, 100);
        assert_eq!(entity.sca2, None, "the single record clears the second box");
    }

    #[test]
    fn webspinner_reset_between_frames_preserves_the_scene() {
        let pack = pack(&[("enemy/em03.lua", source("enemy/em03.lua"))]);
        let (clips, keyframes, skeleton) = spider_model();
        let mut cached_game = spider_game(0x03, 0);
        let mut reset_game = spider_game(0x03, 0);
        for game in [&mut cached_game, &mut reset_game] {
            game.entity_anims[1].keyframes = Some(Arc::clone(&keyframes));
            game.entity_anims[1].skeleton = Some(Arc::clone(&skeleton));
            game.entities[0].pos = [1500, 0, 1000];
            game.rand_state = 0x1234;
        }
        let mut cached = LuaEnemyHost::new();
        let mut reset = LuaEnemyHost::new();

        // Init, the approach, a walk, a lunge bite, a shoot spawn and the
        // thread update, with a fresh damage hit on the shoot frames.
        for tick in 0..80 {
            for game in [&mut cached_game, &mut reset_game] {
                let entity = &mut game.entities[1];
                match tick {
                    4 => {
                        entity.set_state(1);
                        entity.set_ignore(0);
                        entity.action_behavior = 0;
                        entity.action_state = 0;
                        entity.set_room_collision(1);
                        entity.set_writhe_amplitude(0);
                    }
                    10 => {
                        entity.set_state(1);
                        entity.set_ignore(1);
                        entity.action_behavior = 1;
                        entity.action_state = 0;
                        entity.set_writhe_amplitude(1);
                    }
                    16 => {
                        entity.set_state(1);
                        entity.set_ignore(1);
                        entity.action_behavior = 6;
                        entity.action_state = 5;
                        entity.set_tint_flashes(1);
                    }
                    24 => {
                        entity.set_state(3);
                        entity.set_ignore(1);
                        entity.action_behavior = 0;
                        entity.action_state = 3;
                        entity.action_ticks_counter = 1;
                        entity.hit_state = 0x04;
                    }
                    _ => {}
                }
            }
            assert!(cached.update(&mut cached_game, &spider_room(), &pack, 1, &clips));
            reset.reset();
            assert!(reset.update(&mut reset_game, &spider_room(), &pack, 1, &clips));
            assert_eq!(
                cached_game, reset_game,
                "the webspinner scene diverged at tick {tick}"
            );
        }
    }

    #[test]
    fn blacktiger_reset_between_frames_preserves_the_scene() {
        let pack = pack(&[("enemy/em04.lua", source("enemy/em04.lua"))]);
        let (clips, keyframes, skeleton) = spider_model();
        let mut cached_game = spider_game(0x04, 0);
        let mut reset_game = spider_game(0x04, 0);
        for game in [&mut cached_game, &mut reset_game] {
            game.entity_anims[1].keyframes = Some(Arc::clone(&keyframes));
            game.entity_anims[1].skeleton = Some(Arc::clone(&skeleton));
            game.entities[0].pos = [1500, 0, 1000];
            game.rand_state = 0x4321;
        }
        let mut cached = LuaEnemyHost::new();
        let mut reset = LuaEnemyHost::new();

        for tick in 0..80 {
            for game in [&mut cached_game, &mut reset_game] {
                let entity = &mut game.entities[1];
                match tick {
                    4 => {
                        entity.set_state(1);
                        entity.set_ignore(1);
                        entity.action_behavior = 1;
                        entity.action_state = 0;
                        entity.set_writhe_amplitude(1);
                    }
                    12 => {
                        entity.set_state(1);
                        entity.set_ignore(1);
                        entity.action_behavior = 0xC;
                        entity.action_state = 5;
                        entity.animation_frame_id = 0xC;
                    }
                    20 => {
                        entity.set_state(3);
                        entity.set_ignore(1);
                        entity.action_behavior = 1;
                        entity.action_state = 4;
                        entity.hit_state = 0x08;
                    }
                    _ => {}
                }
            }
            assert!(cached.update(&mut cached_game, &spider_room(), &pack, 1, &clips));
            reset.reset();
            assert!(reset.update(&mut reset_game, &spider_room(), &pack, 1, &clips));
            assert_eq!(
                cached_game, reset_game,
                "the blacktiger scene diverged at tick {tick}"
            );
        }
    }
}

#[cfg(all(test, feature = "lua"))]
mod zombie_tests {
    use crate::enemy::LuaEnemyHost;
    use crate::game::{GameState, JointBlood};
    use crate::pack::{Pack, PackWriter};
    use crate::state::{Collision, CollisionRect, RoomState};

    fn source(name: &str) -> &'static str {
        crate::enemy::ENEMY_SCRIPTS
            .iter()
            .find(|(path, _)| *path == name)
            .unwrap_or_else(|| panic!("the checked-in {name} script"))
            .1
    }

    /// The four zombie pack entries: the three id scripts and the shared
    /// driver.
    fn pack() -> Pack {
        let mut writer = PackWriter::new();
        for name in [
            "enemy/em00.lua",
            "enemy/em01.lua",
            "enemy/em11.lua",
            "enemy/lib/zombie.lua",
        ] {
            writer.add(name, source(name).as_bytes().to_vec()).unwrap();
        }
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    fn zombie_game(id: u8, behavior: u8) -> GameState {
        let mut game = GameState::default();
        game.entities[1].id = id;
        game.entities[1].set_active(true);
        game.entities[1].behavior_flags = behavior;
        game.entities[1].status_flags = 1;
        game.entities[0].pos = [1000, 0, 0];
        game
    }

    fn step(host: &mut LuaEnemyHost, game: &mut GameState, pack: &Pack) -> bool {
        host.update(game, &RoomState::default(), pack, 1, &[])
    }

    /// One one-frame clip per id, so `advance_anim` completes on the first
    /// tick whatever animation the handler selects.
    fn clips() -> Vec<crate::model::Clip> {
        (0..32)
            .map(|_| crate::model::Clip {
                frames: vec![crate::model::ClipFrame {
                    keyframe: 0,
                    timing: 1,
                }],
            })
            .collect()
    }

    fn step_clipped(
        host: &mut LuaEnemyHost,
        game: &mut GameState,
        pack: &Pack,
        clips: &[crate::model::Clip],
    ) -> bool {
        host.update(game, &RoomState::default(), pack, 1, clips)
    }

    /// A room with one rectangular boundary around `(0, 0)`, for the floor
    /// probes.
    fn walled_room() -> RoomState {
        RoomState {
            collision: Collision {
                cell_x: 0,
                cell_z: 0,
                quadrants: std::array::from_fn(|_| {
                    vec![CollisionRect {
                        x_max: 400,
                        z_max: 400,
                        x_min: -400i16 as u16,
                        z_min: -400i16 as u16,
                        kind: 1,
                        flags: 0x300,
                    }]
                }),
            },
            ..RoomState::default()
        }
    }

    #[test]
    fn zombie_scripts_load_and_initialise_for_every_id() {
        let pack = pack();
        for (id, radius) in [(0x00u8, 422i16), (0x01, 322), (0x11, 422)] {
            let mut game = zombie_game(id, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack), "id {id:#04x} ran");
            let entity = &game.entities[1];
            assert_eq!(entity.state(), 1);
            assert_eq!(entity.sca_radius, radius);
            assert_eq!(entity.sca_half_height, 1530);
            assert_eq!(entity.sca_offset, [0, -1530, 0]);
            assert_eq!((entity.shadow_half_x, entity.shadow_half_z), (700, 900));
            assert_eq!(entity.shadow_tint, 0x00808080);
            assert_eq!(entity.move_speed_byte, 45);
            assert_eq!(entity.turn_speed, 24);
            assert!((17..=99).contains(&entity.health), "rolled health");
            assert_eq!(host.failed_scripts(), 0);
        }
    }

    #[test]
    fn zombie_init_draws_health_then_uses_the_frame_seed_for_the_tables() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        // Draw one so the two init draws land on known values.
        let first = crate::game::platform_rand(&mut game.rand_state);
        let second = crate::game::platform_rand(&mut game.rand_state);
        game.rand_state = 1;
        game.rand_seed = 0x0A;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        let health_base = [
            59u8, 59, 79, 59, 59, 39, 59, 79, 79, 99, 59, 79, 59, 59, 79, 59,
        ][(first & 0xF) as usize];
        assert_eq!(entity.health, i16::from(health_base) - (second % 22) as i16);
        // The threshold table index is the frame seed's low five bits.
        assert!(matches!(entity.hit_threshold, 1..=4));
        assert_eq!(
            entity.stagger_timer,
            [
                4u8, 3, 5, 3, 4, 4, 3, 4, 3, 5, 4, 4, 5, 3, 4, 5, 4, 3, 3, 4, 4, 3, 4, 4, 5, 3, 5,
                3, 4, 3, 3, 4
            ][0x0A]
        );
        // Two direct draws advanced the stream, no more.
        let mut expected = 1u32;
        crate::game::platform_rand(&mut expected);
        crate::game::platform_rand(&mut expected);
        assert_eq!(game.rand_state, expected);
    }

    #[test]
    fn zombie_init_applies_the_dead_floor_and_vomit_spawn_kinds() {
        let pack = pack();
        // Behaviour 6: a corpse.
        let mut game = zombie_game(0x00, 6);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 3);
        assert_eq!(entity.health, -1);
        assert_eq!(entity.status_flags & 0x0A, 0x0A);
        assert_eq!((entity.ignore(), entity.action_behavior), (1, 4));

        // Behaviour 10: lying on the floor, legs hidden.
        let mut game = zombie_game(0x00, 10);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.status_flags & 0x04, 0x04);
        for joint in 9..=14 {
            assert_eq!(entity.joint_flag(joint) & 1, 0, "leg {joint} hidden");
        }
        assert_eq!(entity.joint_flag(8) & 1, 1, "joint 8 stays visible");

        // Behaviour 0x40: the vomit flag sends the zombie to the SCD path.
        let mut game = zombie_game(0x00, 0x40);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].state(), 8);
    }

    #[test]
    fn zombie_spawn_kind_five_starts_the_eating_sub_state() {
        let pack = pack();
        let mut game = zombie_game(0x00, 5);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].action_state, 2);
        // Behaviour 5 is a standing eater, so the lying-down height stays 0.
        assert_eq!(game.entities[1].pos[1], 0);
    }

    #[test]
    fn zombie_laying_spawn_sits_at_height_one() {
        let pack = pack();
        let mut game = zombie_game(0x00, 2);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].pos[1], 1);
    }

    #[test]
    fn zombie_damage_reaction_reads_the_overlapping_table() {
        let pack = pack();
        // hit_state 3 (a side hit) and behaviour 0: the unaligned table's
        // index 3 picks action_behavior 0 (a short push back).
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(2);
        game.entities[1].hit_state = 3;
        game.entities[1].set_ignore(0);
        game.entities[1].behavior_step = 0;
        game.entities[1].action_speed = 0;
        game.entities[1].action_state = 0;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        // The direction adjust adds the turn's bit 10; with the player to the
        // right the turn step is nonzero for a side hit, so the row may be 1.
        assert!(
            entity.action_behavior == 0 || entity.action_behavior == 1,
            "table row for hit_state 3"
        );
        assert_eq!(entity.ignore(), 1);
    }

    #[test]
    fn zombie_bullet_counter_spends_the_poise_budget() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(2);
        game.entities[1].hit_state = 0x08; // a bullet, no direction bits
        game.entities[1].set_ignore(0);
        game.entities[1].action_speed = 0;
        game.entities[1].hit_threshold = 1;
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].action_speed, 0x80, "threshold spent");

        // The stagger word spends on sustained damage.
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(2);
        game.entities[1].hit_state = 0x10;
        game.entities[1].set_ignore(0);
        game.entities[1].action_speed = 0;
        game.entities[1].stagger_timer = 1;
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].action_speed, 0x80);
    }

    #[test]
    fn zombie_falling_hit_restores_the_mirrored_state() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        // The update mirrors the live block; a hit while the falling bit is
        // set restores it and drops the latch.
        game.entities[1].set_state(2);
        game.entities[1].behavior_step = 0x04;
        game.entities[1].hit_state = 0x08;
        game.entities[1].set_ignore(0);
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].hit_state, 0);
        assert_eq!(game.entities[1].state(), 1, "the mirror restored state 1");
    }

    #[test]
    fn zombie_death_forks_on_direction_and_the_head_flag() {
        let pack = pack();
        // A headshot direction (hit_state & 7 == 4) with the player dead
        // ahead turns the death into the headshot fall.
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(3);
        game.entities[1].set_ignore(0);
        game.entities[1].hit_state = 4;
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].ignore(), 1);
        assert_eq!(game.entities[1].action_behavior, 1);
        assert_eq!(game.entities[1].animation_id, 10);

        // The magnum variant needs the head joint's 0x40 bit and an odd
        // frame seed.
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(3);
        game.entities[1].set_ignore(0);
        game.entities[1].hit_state = 4;
        game.entities[1].set_joint_flag(2, 0x40);
        game.rand_seed = 1;
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].action_behavior, 3);
        assert_eq!(game.entities[1].animation_id, 3, "the magnum pushback");
    }

    #[test]
    fn zombie_death_raises_the_room_event_once() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        game.entities[1].death_event_id = 5;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(3);
        game.entities[1].set_ignore(0);
        game.entities[1].hit_state = 0;
        assert!(step(&mut host, &mut game, &pack));
        assert!(game.flags[usize::from(crate::game::BANK_ENEMIES)].bit(5));
        assert_eq!(game.entities[1].animation_id, 8);
    }

    #[test]
    fn zombie_corpse_counts_down_and_parks() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        // Jump into the corpse hold: the fall is complete, 70 frames held.
        game.entities[1].set_state(3);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 2;
        game.entities[1].animation_id = 8;
        game.entities[1].death_timer = 2;
        game.entities[1].health = 5;
        // Seed 1 fails the 1-in-4 revival roll.
        game.rand_seed = 1;
        game.entities[1].pos = [0, 0, 0];
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].action_state, 3, "no revival, hold starts");
        assert_eq!(game.entities[1].health, -1);
        assert_eq!(game.entities[1].shadow_tint, 0x00FFFF50);
        // The hold's case 3 decrements in the same frame it starts.
        assert_eq!(game.entities[1].death_timer, 1);
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].action_state, 4, "the corpse parks");
        assert_eq!(game.entities[1].death_timer, 0);
    }

    #[test]
    fn zombie_dead_animation_revives_on_the_quarter_roll() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(3);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 2;
        game.entities[1].animation_id = 8;
        game.entities[1].death_timer = 70;
        game.entities[1].health = 5;
        game.rand_seed = 0; // (rand_seed & 3) == 0
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.health, 1);
        assert_eq!(entity.behavior_flags, 3);
        assert_eq!(entity.pos[1], 1);
        assert_eq!(entity.action_behavior, 0);
    }

    #[test]
    fn zombie_dead_animation_keeps_the_corpse_in_the_dining_room() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        game.id.stage = 1;
        game.id.room = 2;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(3);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 2;
        game.entities[1].animation_id = 8;
        game.entities[1].health = 5;
        game.rand_seed = 0;
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(
            game.entities[1].health, -1,
            "the 2F dining room never revives"
        );
    }

    #[test]
    fn zombie_attack_bites_the_player_and_tracks_the_mash() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        game.entities[0].health = 50;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));

        game.entities[0].angle = 0x800;
        game.entities[1].set_state(5);
        game.entities[1].set_ignore(0);
        game.entities[1].action_state = 0;
        game.entities[1].angle = 0;
        assert!(step(&mut host, &mut game, &pack));
        // Case 0: the player is grabbed, the attack rows are committed. The
        // facing test compares the player's yaw against the zombie's, so the
        // reversed player picks direction 0.
        let entity = &game.entities[1];
        assert_eq!(entity.action_state, 1);
        assert_eq!(entity.attacking_direction, 0, "player reversed");
        assert_eq!(entity.animation_id, 0x11);
        assert_eq!(game.entities[0].animation_id, 0x00);
        assert_eq!(game.entities[0].action_ticks_counter, 0x7FFF);
        assert_eq!(game.entities[0].state(), 5);
        assert_eq!(game.entities[0].ignore(), 0);
        assert_eq!(game.entities[0].is_being_attacked, 1);

        // Case 2 starts the bite loop at 105 frames.
        game.entities[1].action_state = 2;
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].action_state, 3);
        assert_eq!((game.entities[1].reaction_timer as u16 >> 8) as u8, 105);

        // Case 3: the first bite lands immediately (tick 0 % 19 == 0).
        let health = game.entities[0].health;
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[0].health, health - 10);
        let timer = (game.entities[1].reaction_timer as u16 >> 8) as u8;
        assert_eq!(timer, 105 - 1, "no mash reduces by one");

        // Mash the pad: the action button and a direction shorten the bite.
        game.dpad_held = crate::game::PAD_ACTION_HELD | 0x0004;
        assert!(step(&mut host, &mut game, &pack));
        let timer2 = (game.entities[1].reaction_timer as u16 >> 8) as u8;
        assert_eq!(timer2, timer.wrapping_sub(6));

        // The latched player runs the state-5 window's recoil machine from the
        // damage bank the loaded monster model installs (its second EDD
        // chunk): the shot is the grabbed-offset snap and the bite clip.
        game.player_damage = Some(std::sync::Arc::new(crate::game::PlayerDamageBank {
            keyframes: std::sync::Arc::new(vec![crate::model::Keyframe::default()]),
            clips: std::sync::Arc::new(
                (0..12)
                    .map(|_| crate::model::Clip {
                        frames: (0..3)
                            .map(|_| crate::model::ClipFrame {
                                keyframe: 0,
                                timing: 1,
                            })
                            .collect(),
                    })
                    .collect(),
            ),
        }));
        let mut player = crate::player::spawn(game.id, &RoomState::default());
        player.pos = game.entities[0].pos;
        crate::player_script::update(&mut game, &mut player, &RoomState::default(), &[], &[], &[]);
        assert_eq!(game.entities[0].state(), 5);
        assert_eq!(player.clip_source, crate::player::ClipSource::Damage);
        assert_eq!(player.anim.clip, 0, "the bite's grabbed clip");
        assert_eq!(
            player.pos, game.entities[0].pos,
            "the snap reached the visible player"
        );
    }

    #[test]
    fn zombie_attack_kills_the_player_into_the_death_pose() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        game.entities[0].health = 5;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(5);
        game.entities[1].set_ignore(0);
        game.entities[1].action_state = 3;
        game.entities[1].attacking_direction = 0;
        game.entities[1].behavior_flags = 0;
        game.entities[1].action_ticks_counter = 0;
        game.entities[1].reaction_timer = 0x0100;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.action_behavior, 5);
        assert_eq!(game.entities[0].state(), 1, "the player death pose");
        assert_eq!(game.entities[0].action_behavior, 200);
    }

    #[test]
    fn zombie_attack_withdraws_and_re_arms_the_hit_latch() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(5);
        game.entities[1].set_ignore(0);
        game.entities[1].action_state = 5;
        game.entities[1].status_flags = 0x0B;
        let clips = clips();
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 2);
        assert_eq!(entity.status_flags & 0x0A, 0, "bits 1 and 3 cleared");
        assert_eq!(entity.hit_state, 1);
    }

    #[test]
    fn zombie_attack_head_bite_explodes_the_head_contract() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(5);
        game.entities[1].set_ignore(0);
        game.entities[1].action_state = 6;
        game.entities[1].attacking_direction = 0;
        // The keyframe table's dir-0 entry is 0, the entry frame.
        game.entities[1].animation_frame_id = 0;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.action_state, 10, "the finish sub-state");
        assert_eq!(entity.joint_flag(2) & 0x28, 0x28, "the head is armed");
        assert!(entity.joint_armed & (1 << 2) != 0);
    }

    #[test]
    fn zombie_attack_vomit_arms_the_blood_scratch() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(5);
        game.entities[1].set_ignore(0);
        game.entities[1].action_state = 8;
        game.entities[1].attacking_direction = 0;
        game.entities[1].animation_frame_id = 0;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.joint_flag(2) & 0x88, 0x88);
        assert_eq!(entity.joint_blood[2].vel_x, 0xFED4u16 as i16);
        assert_eq!(entity.joint_blood[2].vel_y, 0xFA);
        assert_eq!(entity.joint_blood[2].counter, 0);
        assert_eq!(entity.joint_blood[2].flags, 0);
    }

    #[test]
    fn zombie_eating_draws_its_clip_and_stands_on_player_approach() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        game.entities[0].pos = [9000, 0, 0];
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        // Enter the eating behaviour at its first sub-state.
        game.entities[1].set_state(15);
        game.entities[1].action_state = 0;
        game.rand_seed = 0x1F;
        let clips = clips();
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        // Case 0 rolls the clip from the seed and case 1 advances in the
        // same frame; the one-frame clip loops and re-rolls on completion.
        assert_eq!(entity.animation_id, 0x1D, "the seed's low bit picked 0x1D");
        assert_eq!(entity.action_ticks_counter, 0x2D + 0x0F);
        assert_eq!(entity.action_state, 1, "case 0 falls into case 1");

        // The player walks inside 3000: the eater stands up.
        game.entities[0].pos = [100, 0, 0];
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].action_state, 2);
    }

    #[test]
    fn zombie_chase_walk_weaves_in_draw_order() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].behavior_step = 4;
        game.entities[1].action_behavior = 2;
        game.entities[1].action_state = 0;
        game.entities[1].move_speed_byte = 45;
        game.rand_seed = 0x0B;
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].action_state, 1, "case 0 falls into 1");
        assert_eq!(game.entities[1].animation_id, 2);

        game.entities[1].action_state = 2;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.action_state, 3, "case 2 falls into 3");
        assert_eq!(entity.animation_id, 3);
        // Case 3's value-before-decrement runs in the same frame.
        assert_eq!(entity.action_ticks_counter, (0x0B & 0x7F) + 10 - 1);
        assert_eq!(entity.next_turn_timer, 0);
        assert_eq!(entity.move_speed_current, 45);

        // Burn the walk timer to zero: the weave arms with two draws. The
        // value-before-decrement reads the zero on the next frame.
        game.entities[1].action_ticks_counter = 0;
        game.rand_seed = 0x33;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        // The weave decrements its own timer in the arming frame.
        assert_eq!(entity.next_turn_timer, (0x33 & 0x0F) + 0x14 - 1);
        assert_eq!(
            entity.action_ticks_counter,
            ((0x33 & 0x0F) + 0x14) + (0x33 & 0x7F)
        );
        assert_eq!(entity.move_speed_current, 10, "the weave crawls");
    }

    #[test]
    fn zombie_fast_player_facing_turns_and_hands_to_chase_walk() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        // Put the player behind the zombie so the turn step is nonzero.
        game.entities[0].pos = [-1000, 0, 0];
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].behavior_step = 4;
        game.entities[1].action_behavior = 3;
        game.entities[1].action_state = 0;
        game.rand_seed = 0x22;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.action_state, 1);
        assert_eq!(entity.animation_id, 3);
        assert_eq!(entity.action_ticks_counter, (0x22 & 0x3F) + 0x1E - 1);
        assert_eq!(
            entity.angle,
            0u16.wrapping_sub(0x54),
            "the turn step is applied"
        );

        // The timer runs out: hand over to the chase walk with the player as
        // the waypoint.
        game.entities[1].action_ticks_counter = 0;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.action_behavior, 2);
        assert_eq!(entity.action_state, 2);
        assert_eq!(entity.player_pos_x, -1000i16);
    }

    #[test]
    fn zombie_falldown_spends_and_refills_the_poise() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        // The falldown is the action table's entry 7, not a damage row: it
        // dispatches through state 1's action update.
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].behavior_step = 4;
        game.entities[1].action_behavior = 7;
        game.entities[1].action_state = 0;
        game.entities[1].action_speed = 0;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.animation_id, 8);
        assert_eq!(entity.blend_counter, 7);
        assert_eq!(entity.pos[1], 1);
        assert_eq!(entity.action_speed & 0x80, 0, "the fall has not started");

        // Case 1 past frame four keeps the down bit and completes on the
        // one-frame clip: the recovery timer rolls.
        game.entities[1].set_state(1);
        game.entities[1].action_behavior = 7;
        game.entities[1].action_state = 1;
        game.entities[1].animation_frame_id = 5;
        game.rand_seed = 0x0D;
        let clips = clips();
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.action_speed & 0x80, 0x80, "the down bit");
        assert_eq!(entity.action_state, 2);
        assert_eq!(entity.action_ticks_counter, 9 * 30);

        // Case 2 counts down and stands with the get-up cue.
        game.entities[1].set_state(1);
        game.entities[1].action_behavior = 7;
        game.entities[1].action_state = 2;
        game.entities[1].action_ticks_counter = 1;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.action_state, 3);
        assert_eq!(entity.blend_counter, 0);
        assert_eq!(entity.hit_state, 1);

        // Case 3 completes reversed; an empty budget refills from the table.
        game.entities[1].set_state(1);
        game.entities[1].action_behavior = 7;
        game.entities[1].action_state = 3;
        game.entities[1].stagger_timer = 0;
        game.rand_seed = 0x02;
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.stagger_timer, 5);
        assert_eq!(entity.action_speed & 0x80, 0);
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.action_behavior, 3);
        assert_eq!(entity.pos[1], 0);
    }

    #[test]
    fn zombie_short_push_back_severs_an_arm_on_the_roll() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(2);
        game.entities[1].set_ignore(0);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 0;
        game.entities[1].hit_state = 3; // (hit_state & 7) == 3
        game.entities[1].animation_id = 4;
        // (0x40 >> (seed & 7)) & 1: the bit is only set at shift six.
        game.rand_seed = 6;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.joint_flag(4) & 12, 12, "the arm is severed");
        assert_eq!(entity.joint_flag(5) & 0x10, 0x10);
        // The update tail's timer tick runs after the handler.
        assert_eq!(entity.internal_timer, 149);
        assert_eq!(entity.animation_id, 4, "behaviour 0 plays the stagger");
    }

    #[test]
    fn zombie_pushback_action_picks_idle_or_stagger() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        // Keep the player out of the attack gate so the pushback owns the
        // zombie.
        game.entities[0].pos = [9000, 0, 0];
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(12); // pushed back
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 0;
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].animation_id, 9, "pushback idle");

        game.entities[1].set_state(12);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 3;
        game.entities[1].action_state = 0;
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].animation_id, 0x0E, "pushback stagger");
    }

    #[test]
    fn zombie_scd_action_table_runs_the_scripted_handlers() {
        let pack = pack();
        // Action 11 (scd dying) sets animation 10 and counts down the corpse.
        let mut game = zombie_game(0x00, 0x40);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(8);
        game.entities[1].action_behavior = 11;
        game.entities[1].action_state = 0;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.animation_id, 10);
        assert_eq!(entity.action_state, 1);

        // Action 10 (headshot) arms the head and sprays.
        let mut game = zombie_game(0x00, 0x40);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(8);
        game.entities[1].action_behavior = 10;
        game.entities[1].action_state = 0;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.joint_flag(2) & 0x28, 0x28);
        assert_eq!(entity.animation_id, 8);
    }

    #[test]
    fn zombie_scd_vomiting_signals_completion() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0x40);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].set_state(8);
        game.entities[1].action_behavior = 12;
        game.entities[1].action_state = 0;
        game.entities[1].scd_anim_param = 7;
        let clips = clips();
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        // short_push_back completes on the clip and, under the vomit flag,
        // hands the incremented sub-state to the SCD handler, which raises
        // the completion flag and clears the behaviour word.
        let entity = &game.entities[1];
        assert!(game.flags[usize::from(crate::game::BANK_SYSTEM)].bit(7));
        assert_eq!((entity.action_behavior, entity.action_state), (0, 0));
        assert_eq!(entity.animation_id, 4);
    }

    #[test]
    fn zombie_prone_update_runs_the_two_point_floor_probe() {
        let pack = pack();
        let mut game = zombie_game(0x00, 2);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        // Laying down: the tail ORs the probe's bits into the direction
        // control flags. The probe pushes the body out of the walled room.
        let before = game.entities[1].pos;
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].behavior_step = 4;
        game.entities[1].action_behavior = 2;
        game.entities[1].action_state = 4;
        game.entities[1].action_ticks_counter = 5;
        game.entities[1].pos = [0, 0, 0];
        // Keep the player outside the pushed-back attack gate.
        game.entities[0].pos = [9000, 0, 0];
        let room = walled_room();
        assert!(host.update(&mut game, &room, &pack, 1, &[]));
        let _ = before;
        assert_eq!(game.entities[1].state(), 1, "the walk state held");
    }

    #[test]
    fn zombie_reset_between_frames_preserves_the_scene() {
        let pack = pack();
        let run = |reset_every_tick: bool| {
            let mut game = zombie_game(0x00, 0);
            game.entities[0].health = 60;
            game.rand_state = 7;
            let mut host = LuaEnemyHost::new();
            for tick in 0..30u32 {
                game.rand_seed = (tick * 7 + 3) as u16;
                if reset_every_tick {
                    host.reset();
                }
                assert!(step(&mut host, &mut game, &pack));
            }
            game
        };
        let steady = run(false);
        let reset = run(true);
        assert_eq!(steady.entities[1], reset.entities[1]);
        assert_eq!(steady.entities[0], reset.entities[0]);
        assert_eq!(steady.rand_state, reset.rand_state);
        assert_eq!(steady.flags, reset.flags);
    }

    #[test]
    fn zombie_script_sees_typed_scratch_properties() {
        let mut writer = PackWriter::new();
        writer
            .add(
                "enemy/em00.lua",
                br#"
function update(e)
    e.state_word = 0x03020101
    assert(e.state == 1 and e.ignore == 1)
    assert(e.action_behavior == 2 and e.action_state == 3)
    e.state_mirror = 0x07060504
    e.state_word = e.state_mirror
    assert(e.state == 4 and e.ignore == 5)
    assert(e.action_behavior == 6 and e.action_state == 7)

    e.move_speed_byte = 45
    e.turn_speed = 24
    e.internal_timer = 150
    e.stagger_timer = 5
    e.action_speed = 0x80
    e.hit_threshold = 4
    e.behavior_step = 0x04
    e.action_counter = 2
    e.next_turn_timer = 0x1234
    e.attack_timer = 105
    assert(e.attack_timer == 105)

    e.zombie_turn_delta = -8
    assert(e.zombie_turn_delta == -8)
    e.zombie_move_word = 0x0201
    assert(e.is_moving == 1 and e.move_max_steps == 2)
    e.zombie_part_word = 0x0001
    assert(e.splatter_flag == 1 and e.bob_speed == 0)
    e.zombie_wander_word = 0x1234
    assert(e.zombie_wander_word == 0x1234)
    e.zombie_unk_178 = 9
    e.zombie_unk_179 = 8
    assert(e.zombie_unk_178 == 9 and e.zombie_unk_179 == 8)

    e:set_joint_flag(4, 0x0D)
    assert(e:joint_flag(4) == 0x0D)
    e:set_joint_blood(2, 0xFED4, 0xFA, 0, 0)
    assert(e:joint_world_x(2) == e:joint_world_x(2))

    local rx, rz = e:rotate_xz(0, 5000, 0)
    assert(rx ~= 0 and rz == 0)
    e.player_state = 5
    e.player_anim_frame_id = 7
    e.player_attack_anim = 3
    e.player_attack_direction = 0x7FFF
    e.player_attack_timer = 0x0102
    e.player_angle = 0x400
end
"#
                .to_vec(),
            )
            .unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        // The driver's own writes after the script win for the state block.
        assert_eq!(entity.joint_flag(4), 0x0D);
        assert_eq!(entity.joint_blood[2].vel_x, 0xFED4u16 as i16);
        assert_eq!(entity.joint_blood[2].vel_y, 0xFA);
        assert_eq!(game.entities[0].state(), 5);
        assert_eq!(game.entities[0].ignore(), 7);
        assert_eq!(game.entities[0].animation_id, 3);
        assert_eq!(game.entities[0].action_ticks_counter, 0x7FFF);
        assert_eq!(game.entities[0].next_turn_timer, 0x0102);
        assert_eq!(game.entities[0].angle, 0x400);
    }

    #[test]
    fn zombie_body_part_speed_reads_the_leg_chain() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        // With no model loaded the chain is empty and the magnitude is zero;
        // the helper must still run without error and write the field.
        game.entities[1].move_speed_current = 999;
        game.entities[1].set_state(12);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 3;
        game.entities[1].action_state = 2;
        // Outside the pushed-back attack gate, so the stagger runs.
        game.entities[0].pos = [9000, 0, 0];
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].move_speed_current, 0);
    }

    #[test]
    fn zombie_blood_splatter_falls_and_cues() {
        let pack = pack();
        let mut game = zombie_game(0x00, 0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        game.entities[1].joint_blood[2] = JointBlood {
            counter: 0,
            flags: 0,
            vel_x: 0,
            vel_y: 0,
            rot_z: 0,
        };
        game.entities[1].set_zombie_part_word(1);
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].behavior_step = 4;
        game.entities[1].action_behavior = 0;
        game.entities[1].action_state = 1;
        game.entities[1].action_ticks_counter = 100;
        let room = walled_room();
        assert!(host.update(&mut game, &room, &pack, 1, &[]));
        // The head starts at Y 0 (> -100) with frame 0 (< 6): the fall runs,
        // the counter increments and the first gravity step lands at -100.
        assert_eq!(game.entities[1].joint_blood[2].counter, 1);
        assert!(game.entities[1].joint_blood[2].flags & 0x1F >= 1);
    }

    #[test]
    fn zombie_every_dispatch_slot_runs_without_error() {
        let pack = pack();
        let clips = clips();

        // Every behaviour nibble through state 1's behaviour view.
        for behavior in 0..=0x0Fu8 {
            let mut game = zombie_game(0x00, behavior);
            let mut host = LuaEnemyHost::new();
            for tick in 0..8u32 {
                game.rand_seed = (tick * 11 + behavior as u32) as u16;
                assert!(step_clipped(&mut host, &mut game, &pack, &clips));
            }
            assert_eq!(host.failed_scripts(), 0, "behaviour {behavior:#x}");
        }

        // Every action-behaviour slot through the action table (0..=12 map
        // into the shared block; 13+ is out of range).
        for behavior in 0..=13u8 {
            let mut game = zombie_game(0x00, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack));
            game.entities[1].set_state(1);
            game.entities[1].set_ignore(1);
            game.entities[1].behavior_step = 4;
            game.entities[1].action_behavior = behavior;
            game.entities[1].action_state = 0;
            for tick in 0..6u32 {
                game.rand_seed = (tick * 5 + behavior as u32) as u16;
                assert!(step_clipped(&mut host, &mut game, &pack, &clips));
            }
            assert_eq!(host.failed_scripts(), 0, "action {behavior}");
        }

        // Every raw state slot, including the NULL 6/7/9.
        for state in 0..=21u8 {
            let mut game = zombie_game(0x00, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack));
            game.entities[1].set_state(state);
            game.entities[1].set_ignore(1);
            for tick in 0..4u32 {
                game.rand_seed = (tick + state as u32) as u16;
                assert!(step_clipped(&mut host, &mut game, &pack, &clips));
            }
            assert_eq!(host.failed_scripts(), 0, "state {state}");
        }

        // Every damage row: hit_state's low bits and direction bits.
        for hit in 0..=0x7Fu8 {
            let mut game = zombie_game(0x00, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack));
            game.entities[1].set_state(2);
            game.entities[1].set_ignore(0);
            game.entities[1].hit_state = hit;
            game.entities[1].action_speed = 0;
            game.entities[1].behavior_step = 0;
            for tick in 0..4u32 {
                game.rand_seed = (tick + hit as u32) as u16;
                assert!(step_clipped(&mut host, &mut game, &pack, &clips));
            }
            assert_eq!(host.failed_scripts(), 0, "hit {hit:#x}");
        }

        // Every SCD action slot.
        for behavior in 0..=15u8 {
            let mut game = zombie_game(0x00, 0x40);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack));
            game.entities[1].set_state(8);
            game.entities[1].action_behavior = behavior;
            game.entities[1].action_state = 0;
            for tick in 0..4u32 {
                game.rand_seed = (tick + behavior as u32) as u16;
                assert!(step_clipped(&mut host, &mut game, &pack, &clips));
            }
            assert_eq!(host.failed_scripts(), 0, "scd {behavior}");
        }
    }

    #[test]
    fn zombie_corpus_ids_run_without_errors() {
        let pack = pack();
        for id in [0x00u8, 0x01, 0x11] {
            let mut game = zombie_game(id, 0);
            let mut host = LuaEnemyHost::new();
            for tick in 0..20u32 {
                game.rand_seed = tick as u16 * 13 + 5;
                assert!(step(&mut host, &mut game, &pack), "id {id:#04x}");
            }
            assert_eq!(host.failed_scripts(), 0, "id {id:#04x} failed");
        }
    }
}

#[cfg(all(test, feature = "lua"))]
mod cerberus_tests {
    use crate::enemy::LuaEnemyHost;
    use crate::game::GameState;
    use crate::pack::{Pack, PackWriter};
    use crate::state::RoomState;

    fn source(name: &str) -> &'static str {
        crate::enemy::ENEMY_SCRIPTS
            .iter()
            .find(|(path, _)| *path == name)
            .unwrap_or_else(|| panic!("the checked-in {name} script"))
            .1
    }

    fn pack() -> Pack {
        let mut writer = PackWriter::new();
        writer
            .add(
                "enemy/em02.lua",
                source("enemy/em02.lua").as_bytes().to_vec(),
            )
            .unwrap();
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    fn pack_with(entries: &[(&str, &str)]) -> Pack {
        let mut writer = PackWriter::new();
        for (path, source) in entries {
            writer.add(path, source.as_bytes().to_vec()).unwrap();
        }
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    /// One active hound in slot 1 with the given spawn kind.
    fn dog(behavior: u8) -> GameState {
        let mut game = GameState::default();
        let entity = &mut game.entities[1];
        entity.id = 0x02;
        entity.set_active(true);
        entity.status_flags = 1;
        entity.behavior_flags = behavior;
        entity.death_event_id = 3;
        entity.pos = [0, 0, 0];
        entity.saved_pos = Some(entity.pos);
        game.entities[0].pos = [4000, 0, 0];
        game.entities[0].health = 100;
        game.enemy_count = 1;
        game
    }

    /// A minimal room so the enemy-sound cues resolve.
    fn dog_room() -> RoomState {
        RoomState {
            stage: 1,
            room: 1,
            ..RoomState::default()
        }
    }

    fn step(host: &mut LuaEnemyHost, game: &mut GameState, pack: &Pack) -> bool {
        host.update(game, &dog_room(), pack, 1, &[])
    }

    /// One-frame clips for every animation id, so `advance_anim` completes on
    /// every tick.
    fn clips() -> Vec<crate::model::Clip> {
        (0..32)
            .map(|_| crate::model::Clip {
                frames: vec![crate::model::ClipFrame {
                    keyframe: 0,
                    timing: 1,
                }],
            })
            .collect()
    }

    fn step_clipped(
        host: &mut LuaEnemyHost,
        game: &mut GameState,
        pack: &Pack,
        clips: &[crate::model::Clip],
    ) -> bool {
        host.update(game, &dog_room(), pack, 1, clips)
    }

    const HEALTH: [i16; 16] = [
        119, 99, 119, 99, 119, 99, 119, 99, 99, 99, 59, 99, 59, 99, 59, 99,
    ];

    #[test]
    fn cerberus_init_nudges_and_arms_the_record() {
        let pack = pack();
        let mut game = dog(4);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.sca_radius, 400);
        assert_eq!(entity.sca_half_height, 800);
        assert_eq!(entity.sca_offset, [500, -800, 0]);
        assert_eq!(
            (
                entity.shadow_half_x,
                entity.shadow_half_z,
                entity.shadow_tint
            ),
            (0x550, 0x200, 0x0080_8080)
        );
        assert_eq!(entity.cb_behflags(), 1);
        assert_eq!(entity.cb_aiflags(), 0x80);
        assert_eq!(entity.saved_pos, Some([499, 0, 0]), "the spawn nudge");
        assert_eq!(entity.animation_id, 0);
        assert_eq!(entity.animation_frame_id, 0);
        assert!((59..=122).contains(&entity.health));
        assert_eq!(host.failed_scripts(), 0);
    }

    #[test]
    fn cerberus_init_draws_the_health_from_the_platform_stream() {
        let pack = pack();
        let mut game = dog(4);
        game.rand_state = 7;
        let mut expected = 7u32;
        crate::game::platform_rand(&mut expected);
        let index = crate::game::platform_rand(&mut expected) & 0xF;
        let variance = crate::game::platform_rand(&mut expected) & 3;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(
            game.entities[1].health,
            HEALTH[index as usize] + variance as i16
        );
        assert_eq!(game.rand_state, expected, "exactly three draws");
    }

    #[test]
    fn cerberus_patrol_selector_then_pick_turn() {
        let pack = pack();
        let mut game = dog(2);
        game.entities[0].pos = [9000, 0, 0];
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");

        let mut expected = game.rand_state;
        let _frame = crate::game::platform_rand(&mut expected) & 0x1F;
        let swerve = (crate::game::platform_rand(&mut expected) - 2) & 0xF;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 1);
        assert_eq!(entity.animation_id, 14, "the run row");
        assert_eq!(entity.move_speed_current, 130);
        assert_eq!(entity.tint_flashes(), 0x20);
        assert_eq!(entity.cb_behflags(), 0, "the walk/run bit");
        assert_eq!(entity.cb_swerve(), 0, "begin clears the swerve");
        assert_eq!(entity.action_behavior, 1, "the begin sub-state ran");
        assert_eq!(game.rand_state, expected, "frame then swerve");

        // The pick-turn sub-state arms the 0x400 dwell and a +/-turn step.
        let _ = swerve;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.action_behavior, 2);
        assert_eq!(entity.action_ticks_counter, 0x400);
        assert_eq!(entity.cb_turn_step().unsigned_abs(), 0x20);
    }

    #[test]
    fn cerberus_patrol_drops_to_chase_inside_4500() {
        let pack = pack();
        let mut game = dog(2);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");
        assert!(step(&mut host, &mut game, &pack), "selector");
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.behavior_flags, 4);
        assert_eq!(entity.blend_counter, 0xF, "the begin sub-state re-aligned");
    }

    #[test]
    fn cerberus_chase_runs_and_turns_the_animation() {
        let pack = pack();
        let mut game = dog(4);
        game.entities[0].pos = [9000, 0, 0];
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 2);
        assert_eq!(entity.player_pos_x, 9000, "the waypoint latch");
        assert_eq!(entity.player_pos_z, 0);
        assert!(entity.cb_aiflags() & 0x80 != 0, "ACTIVE stays raised");
        assert_eq!(entity.animation_id, 2, "straight-run animation");
        assert_eq!(entity.cb_swerve(), 5, "the swerve floor");
        assert!(entity.tint_flashes() >= 0x3D && entity.tint_flashes() <= 0x44);
    }

    #[test]
    fn cerberus_consider_attack_turns_an_aligned_chase_into_the_leap() {
        let pack = pack();
        let mut game = dog(4);
        // The dog walks to within 5000 of the player and lines straight up:
        // the attack decision hands over to the running leap entrance.
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.behavior_flags, 5);
        assert_eq!(entity.cb_swerve(), 0);
    }

    #[test]
    fn cerberus_bite_selector_waits_for_close_range() {
        let pack = pack();
        let mut game = dog(8);
        game.entities[0].pos = [9000, 0, 0];
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 7);
        assert_eq!(entity.animation_id, 4);
        assert_eq!(entity.blend_counter, 0);
        assert_eq!(entity.hit_state, 1);
        assert_eq!(entity.action_behavior, 0, "the wait sub-state");

        // The player closes inside 4501: the snap advances a clip.
        game.entities[0].pos = [1000, 0, 0];
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.action_behavior, 1);
        assert_eq!(entity.animation_id, 5);
        assert_eq!(entity.animation_frame_id, 0);
        assert_eq!(entity.blend_counter, 0xF);

        // Frame 33 hands back to the chase.
        game.entities[1].animation_frame_id = 0x21;
        game.entities[1].timing_control = 2;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.behavior_flags, 4);
        assert_eq!(entity.hit_state, 0);
    }

    #[test]
    fn cerberus_leap_entrance_crouches_and_launches() {
        let pack = pack();
        let mut game = dog(5);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");
        assert!(step(&mut host, &mut game, &pack), "selector");
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 3);
        assert_eq!(entity.animation_id, 6);
        assert_eq!(entity.move_speed_current, 0x41);
        assert_eq!(entity.cb_launch_vy(), 300);
        assert_eq!(entity.reaction_timer, -40);
        assert_eq!(entity.action_ticks_counter, 7, "the first crouch tick");
        assert_eq!(entity.cb_blood(), 0);

        // Pre-load an air tick so the crouch releases on the first frame and
        // yelps.
        game.entities[1].death_timer = 1;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.action_behavior, 1, "airborne");
        assert_eq!(entity.move_speed_current, 0x118);
        assert_eq!(entity.cb_alert(), 1);
        assert_eq!(game.entity_sounds.len(), 1, "one crouch yelp");
    }

    #[test]
    fn cerberus_leap_airborne_bites_and_tumbles() {
        let pack = pack();
        let mut game = dog(5);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");
        assert!(step(&mut host, &mut game, &pack), "selector");
        game.entities[1].death_timer = 1;
        assert!(step(&mut host, &mut game, &pack), "crouch releases");
        assert_eq!(game.entities[1].action_behavior, 1);

        // A reachable player at full health: an ordinary bite, then the tumble
        // sub-state takes the entity.
        game.entities[0].pos = [0, 0, 0];
        game.entities[0].health = 100;
        game.entities[0].angle = 0;
        game.entities[1].set_groan_timer(0);
        let identity = crate::anim::Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [0, 0, 0],
        };
        game.joint_worlds[1] = vec![identity; 8];
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.action_behavior, 4, "the tumble after a bite");
        assert_eq!(entity.animation_id, 0x11);
        assert_eq!(entity.blend_counter, 0xF);
        assert_eq!(entity.move_speed_current, 0x118 >> 2);
        assert_eq!(entity.cb_blood(), 2, "three owed, the leap spawned one");
        assert_eq!(entity.tint_flashes(), 0x200);
        assert_eq!(
            entity.status_flags & 0x04,
            0,
            "the bite clears the airborne bit"
        );
        assert_eq!(game.entities[0].health, 100 - 0xC);
        assert_eq!(game.entities[0].is_being_attacked, 2, "facing + 1");
        assert_eq!(game.entities[0].action_behavior, 0x66 + 1);
        assert_eq!(game.entities[0].action_state, 1);
        // Both sides store the bite point.
        assert_eq!(entity.unk_c6, 0);
        assert_eq!(game.entities[0].unk_c6, 0);
        assert_eq!(game.rand_state, game.rand_state, "no draw for the bite");
    }

    #[test]
    fn cerberus_leap_airborne_kill_rolls_into_the_maul() {
        let pack = pack();
        let mut game = dog(5);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");
        assert!(step(&mut host, &mut game, &pack), "selector");
        game.entities[1].death_timer = 1;
        assert!(step(&mut host, &mut game, &pack), "crouch releases");

        // A badly hurt player facing away triggers the kill branch; the arc
        // then hands the entity to the maul.
        game.entities[0].pos = [0, 0, 0];
        game.entities[0].health = 5;
        game.entities[0].angle = 0x800;
        game.entities[1].set_groan_timer(0);
        let identity = crate::anim::Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [0, 0, 0],
        };
        game.joint_worlds[1] = vec![identity; 8];
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].groan_timer(), 2, "the kill latch");
        assert_eq!(game.player_flags & 2, 2);
        assert_eq!(game.entities[0].state(), 7);
        assert_eq!(game.entities[0].ignore(), 2);
        assert_eq!(game.entities[0].action_state, 2);
        assert_eq!(game.entities[0].angle, 0x800);
        assert_eq!(game.entities[0].is_being_attacked, 1);

        for _ in 0..80 {
            let _ = step(&mut host, &mut game, &pack);
            if game.entities[1].ignore() == 6 {
                break;
            }
        }
        assert_eq!(game.entities[1].ignore(), 6, "the arc hands over to maul");
        assert_eq!(game.entities[1].action_behavior, 0);
        assert_eq!(game.entities[1].status_flags & 2, 2);
        assert_eq!(game.player_flags & 2, 2);
    }

    #[test]
    fn cerberus_maul_pins_the_player_and_owes_blood() {
        let pack = pack();
        let mut game = dog(4);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(6);
        game.entities[1].action_behavior = 0;
        game.entities[1].pos = [500, 0, 500];
        game.entities[1].saved_pos = Some(game.entities[1].pos);
        game.entities[0].pos = [600, 0, 520];
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.action_behavior, 1);
        assert_eq!(entity.animation_id, 8);
        assert_eq!(entity.move_speed_current, 0);
        assert_eq!(game.player_flags & 6, 6, "no-control flags");
        assert_eq!(game.entities[0].angle, entity.angle, "facing latched");
        assert_eq!(game.entities[0].state(), 7, "the mauled pose word");
        assert_eq!(game.entities[0].ignore(), 2);
        assert_eq!(game.entities[0].action_behavior, 0);
        assert_eq!(game.entities[0].action_state, 0);

        // Frame 0x2A kills, frame 0x66 owes six blood billboards and spends
        // two of them this frame.
        game.entities[1].animation_frame_id = 0x2A;
        game.entities[1].timing_control = 2;
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[0].health, -1);

        game.entities[1].animation_frame_id = 0x66;
        game.entities[1].timing_control = 2;
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].cb_blood(), 4, "six owed, two spawned");
        assert!(
            game.entity_sounds.iter().any(|sound| sound.bank == 3),
            "the 3D kill cue at the player"
        );
    }

    #[test]
    fn cerberus_damaged_stumble_recovers_to_the_chase() {
        let pack = pack();
        let mut game = dog(4);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");

        game.entities[1].set_state(2);
        game.entities[1].set_ignore(0);
        game.entities[1].hit_state = 1;
        game.entities[1].animation_id = 0;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 3, "behaviour 4 takes the knockdown");
        assert_eq!(entity.animation_id, 0x0C);
        assert_eq!(entity.move_speed_current, 0xF0);
        assert_eq!(entity.blend_counter, 0);
        assert!(entity.cb_aiflags() & 0x20 != 0, "the hurt bit");

        // The skid runs its clip and, when the player is straight ahead,
        // climbs out through the get-up and back to the chase.
        let clips = clips();
        for _ in 0..40 {
            assert!(step_clipped(&mut host, &mut game, &pack, &clips));
            if game.entities[1].state() == 1 {
                break;
            }
        }
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1, "recovered");
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.behavior_flags, 4);
        assert_eq!(entity.hit_state, 0);
    }

    #[test]
    fn cerberus_die_shrinks_the_shadow_and_parks() {
        let pack = pack();
        let mut game = dog(4);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");
        game.entities[1].set_state(3);
        game.entities[1].set_ignore(0);
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 2, "behaviour 4 lands first");
        assert!(game.flags[crate::game::BANK_ENEMIES as usize].bit(3));

        let clips = clips();
        for _ in 0..10 {
            let _ = step_clipped(&mut host, &mut game, &pack, &clips);
            if game.entities[1].ignore() == 3 {
                break;
            }
        }
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 3);
        assert_eq!(entity.status_flags & 0x0A, 0x0A, "dead and intangible");
        assert_eq!(entity.shadow_tint, 0x00FF_FF50);
        assert_eq!(entity.move_speed_current, 0);
        assert_eq!(entity.action_ticks_counter, 0x46);

        let before_x = entity.shadow_half_x;
        let _ = step_clipped(&mut host, &mut game, &pack, &clips);
        assert_eq!(game.entities[1].shadow_half_x, before_x + 6);

        for _ in 0..80 {
            let _ = step_clipped(&mut host, &mut game, &pack, &clips);
            if game.entities[1].ignore() == 4 {
                break;
            }
        }
        assert_eq!(game.entities[1].ignore(), 4, "the corpse parks");
    }

    #[test]
    fn cerberus_probe_paths_and_history_age() {
        let pack = pack();
        let mut game = dog(4);
        game.entities[0].pos = [9000, 0, 0];
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");
        // The tail ages the history word and re-probes: both empty-room probes
        // are clear, so only the "did not move" bit can appear.
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].cb_probe() & 0xF, 0);
    }

    #[test]
    fn cerberus_every_spawn_kind_runs_without_errors() {
        let pack = pack();
        for behavior in [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 0x80] {
            let mut game = dog(behavior);
            game.entities[0].pos = [600, 0, 0];
            game.rand_state = u32::from(behavior) * 7 + 3;
            let mut host = LuaEnemyHost::new();
            for tick in 0..25u32 {
                game.rand_seed = (tick * 13 + u32::from(behavior)) as u16;
                assert!(
                    step(&mut host, &mut game, &pack),
                    "behavior {behavior:#04x} tick {tick}"
                );
            }
            assert_eq!(host.failed_scripts(), 0, "behavior {behavior:#04x}");
        }
    }

    #[test]
    fn cerberus_reset_between_frames_preserves_the_scene() {
        let pack = pack();
        let run = |reset_every_tick: bool| {
            let mut game = dog(4);
            game.entities[0].pos = [1200, 0, 300];
            game.entities[0].health = 60;
            game.rand_state = 7;
            let mut host = LuaEnemyHost::new();
            for tick in 0..40u32 {
                game.rand_seed = (tick * 11 + 5) as u16;
                if reset_every_tick {
                    host.reset();
                }
                assert!(step(&mut host, &mut game, &pack));
            }
            game
        };
        let steady = run(false);
        let reset = run(true);
        assert_eq!(steady.entities[1], reset.entities[1]);
        assert_eq!(steady.entities[0], reset.entities[0]);
        assert_eq!(steady.rand_state, reset.rand_state);
        assert_eq!(steady.cerberus_barked, reset.cerberus_barked);
        assert_eq!(steady.scratch_vec, reset.scratch_vec);
    }
    #[test]
    fn cerberus_script_sees_typed_scratch_properties() {
        let pack = pack_with(&[(
            "enemy/em02.lua",
            r#"
function update(e)
    e.cb_dist = -0x12345678
    e.cb_turn_step = -0x1234
    e.cb_launch_vy = 0x1234
    e.cb_probe = -2
    e.cb_path = 0x1FF
    e.cb_swerve = -3
    e.cb_blood = 0x1234
    e.cb_alert = 0x1FF
    e.cb_behflags = 0x1234
    e.cb_behflags_byte = 0xAB
    e.cb_aiflags = -5
    e.cb_repause = 0x2345
    e.saved_x = 0x1234
    e.saved_y = -2
    e.saved_z = 0x5678
    e.scratch_x = 111
    e.scratch_z = -222
    e.cerberus_barked = true
    e.player_flags = 0x106
    e.player_unk_c6 = 0xBEEF
    e.player_unk_c8 = 0xCAFE
end
"#,
        )]);
        let mut game = dog(4);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.cb_dist(), -0x1234_5678);
        assert_eq!(entity.cb_turn_step(), -0x1234);
        assert_eq!(entity.cb_launch_vy(), 0x1234);
        assert_eq!(entity.cb_probe(), -2);
        assert_eq!(entity.cb_path(), 0xFF);
        assert_eq!(entity.cb_swerve(), -3);
        assert_eq!(entity.cb_blood(), 0x1234);
        assert_eq!(entity.cb_alert(), 0xFF);
        assert_eq!(entity.cb_behflags(), 0x12AB, "the byte view composes");
        assert_eq!(entity.cb_behflags_byte(), 0xAB);
        assert_eq!(entity.cb_aiflags(), -5);
        assert_eq!(entity.cb_repause(), 0x2345);
        assert_eq!(entity.saved_pos, Some([0x1234, -2, 0x5678]));
        assert_eq!(game.scratch_vec, [111, 0, -222]);
        assert!(game.cerberus_barked);
        assert_eq!(game.player_flags, 0x06, "u8 truncation");
        assert_eq!(game.entities[0].unk_c6, 0xBEEF);
        assert_eq!(game.entities[0].unk_c8, 0xCAFE);
    }
}

#[cfg(all(test, feature = "lua"))]
mod chimera_tests {
    use crate::enemy::LuaEnemyHost;
    use crate::game::GameState;
    use crate::pack::{Pack, PackWriter};
    use crate::state::{RoomState, Zone};

    fn source(name: &str) -> &'static str {
        crate::enemy::ENEMY_SCRIPTS
            .iter()
            .find(|(path, _)| *path == name)
            .unwrap_or_else(|| panic!("the checked-in {name} script"))
            .1
    }

    fn pack() -> Pack {
        let mut writer = PackWriter::new();
        writer
            .add(
                "enemy/em09.lua",
                source("enemy/em09.lua").as_bytes().to_vec(),
            )
            .unwrap();
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    fn pack_with(entries: &[(&str, &str)]) -> Pack {
        let mut writer = PackWriter::new();
        for (path, source) in entries {
            writer.add(path, source.as_bytes().to_vec()).unwrap();
        }
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    /// One active chimera in slot 1 with the given variant.
    fn chimera(behavior: u8) -> GameState {
        let mut game = GameState::default();
        let entity = &mut game.entities[1];
        entity.id = 0x09;
        entity.set_active(true);
        entity.status_flags = 1;
        entity.behavior_flags = behavior;
        entity.death_event_id = 3;
        entity.pos = [0, 0, 0];
        entity.saved_pos = Some(entity.pos);
        game.entities[0].pos = [4000, 0, 0];
        game.entities[0].health = 100;
        game.enemy_count = 1;
        game
    }

    /// A minimal room so the enemy-sound cues resolve.
    fn chimera_room() -> RoomState {
        RoomState {
            stage: 1,
            room: 1,
            ..RoomState::default()
        }
    }

    fn step(host: &mut LuaEnemyHost, game: &mut GameState, pack: &Pack) -> bool {
        host.update(game, &chimera_room(), pack, 1, &[])
    }

    fn clips() -> Vec<crate::model::Clip> {
        (0..32)
            .map(|_| crate::model::Clip {
                frames: vec![crate::model::ClipFrame {
                    keyframe: 0,
                    timing: 1,
                }],
            })
            .collect()
    }

    fn step_clipped(
        host: &mut LuaEnemyHost,
        game: &mut GameState,
        pack: &Pack,
        clips: &[crate::model::Clip],
    ) -> bool {
        host.update(game, &chimera_room(), pack, 1, clips)
    }

    /// The room the shadow-resize tail needs: one camera zone containing the
    /// origin.
    fn camera_room() -> RoomState {
        RoomState {
            stage: 1,
            room: 1,
            zones: vec![Zone {
                cam_from: 0,
                cam_to: 0,
                corners: [[0, 0], [0, 5000], [5000, 5000], [5000, 0]],
            }],
            ..RoomState::default()
        }
    }

    #[test]
    fn chimera_init_floor_ceiling_and_hard_variants() {
        let pack = pack();
        // Variant 0 parks on the floor.
        let mut game = chimera(0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.pos[1], 0);
        assert_eq!(entity.roll, 0);
        assert_eq!(entity.animation_id, 0, "the idle clip is the variant");
        assert_eq!(entity.sca_radius, 500);
        assert_eq!(entity.sca_half_height, 180);
        assert_eq!(entity.sca_offset, [0, -180, 0]);
        assert_eq!(entity.shadow_tint, 0x0080_8080);
        assert!((80..=122).contains(&entity.health));
        assert_eq!(game.entities[1].sink_wobble(), 0);

        // Variant 2 hangs from the ceiling.
        let mut game = chimera(2);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.pos[1], -6008);
        assert_eq!(entity.roll, 0x800);
        assert_eq!(entity.animation_id, 2);

        // Variant 3 is scripted hard mode: it rerolls to a live variant and
        // latches the hard word.
        let mut game = chimera(3);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert!(entity.behavior_flags < 2);
        assert_eq!(entity.sink_wobble(), 1);
    }

    #[test]
    fn chimera_init_health_uses_three_d3s() {
        let pack = pack();
        let mut game = chimera(0);
        game.rand_state = 9;
        let mut expected = 9u32;
        crate::game::platform_rand(&mut expected);
        let r1 = crate::game::platform_rand(&mut expected);
        let r2 = crate::game::platform_rand(&mut expected);
        let r3 = crate::game::platform_rand(&mut expected);
        let health = 2 * ((r2 & 7) + (r1 & 7) + (r3 & 7)) + 0x50;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].health, health as i16);
        assert_eq!(game.rand_state, expected, "four draws");
    }

    #[test]
    fn chimera_brain_a_walks_on_a_fresh_path() {
        let pack = pack();
        let mut game = chimera(0);
        // Latch a fresh path and keep the player out of range so the walk
        // hand-over is the only branch that fires.
        game.entities[1].set_writhe_amplitude(1);
        game.entities[0].pos = [20000, 0, 0];
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.action_behavior, 1, "the walk owns it");
        assert_eq!(entity.animation_id, 9);
        assert_eq!(entity.move_speed_current, 0xB4);
        assert_eq!(entity.action_state, 1, "walk_timeout armed");
    }

    #[test]
    fn chimera_brain_b_coin_flips_into_the_spit_or_claw() {
        let pack = pack();
        let mut game = chimera(1);
        game.entities[0].pos = [100, 0, 0];
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");
        // Force the attack coin to 1 on the next draw.
        let mut state = game.rand_state;
        loop {
            let mut probe = state;
            if crate::game::platform_rand(&mut probe) & 1 == 1 {
                break;
            }
            state = probe;
        }
        game.rand_state = state;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 1);
        assert_eq!(entity.action_behavior, 0xC, "facing player -> claw");
        assert_eq!(entity.c_far_latch(), 0);
    }

    #[test]
    fn chimera_ceiling_brain_mirrors_and_waits() {
        let pack = pack();
        let mut game = chimera(2);
        game.entities[1].set_writhe_amplitude(1);
        game.entities[0].pos = [100, 0, 0];
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack), "init");
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.angle, 0, "the mirror was folded back");
        // The close-range swipe hand-over fires for the ceiling brain.
        assert_eq!(entity.action_behavior, 3);
        assert_eq!(entity.ignore(), 1);
    }

    #[test]
    fn chimera_drop_descends_the_arc() {
        let pack = pack();
        let mut game = chimera(2);
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 6;
        game.entities[1].action_state = 0;
        game.entities[1].pos[1] = -6008;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.action_state, 2);
        assert_eq!(entity.pos[1], -5068, "arc[32] at frame 0");
        assert_eq!(entity.animation_id, 0x10);
        assert_eq!(entity.status_flags & 2, 2);
        assert!(entity.hit_state == 1);

        // Level out: roll upright, become variant 1 and keep descending.
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.roll, 0);
        assert_eq!(entity.behavior_flags, 1);
        assert_eq!(entity.action_state, 3);
        assert!(game.entity_sounds.iter().any(|sound| sound.column == 3));

        // The descent clamps the arc through frame 0x1f.
        let clips = clips();
        for _ in 0..32 {
            assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        }
        assert!(game.entities[1].pos[1] >= -5068);
    }

    #[test]
    fn chimera_swoop_climbs_the_arc_and_flips_home() {
        let pack = pack();
        let mut game = chimera(2);
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 7;
        game.entities[1].action_state = 0;
        game.entities[1].pos[1] = -6008;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.pos[1], -1540, "arc[0] on the swoop");
        assert_eq!(entity.action_state, 1);
        assert!(entity.collision_flags == 0);
        assert!(game.entity_sounds.iter().any(|sound| sound.column == 1));

        // Frame 0x20 hands the climb home; the next frame flips it back to
        // the ceiling.
        let clips = clips();
        game.entities[1].timing_control = 2;
        game.entities[1].animation_frame_id = 0x20;
        game.entities[1].pos[1] = -5068;
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_state, 2);
        game.entities[1].timing_control = 0;
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.roll, 0x800);
        assert_eq!(entity.behavior_flags, 2);
        assert!(entity.action_state >= 3);
    }

    #[test]
    fn chimera_grab_meter_bleeds_with_the_buttons() {
        let pack = pack();
        let mut game = chimera(0);
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 4;
        game.entities[1].action_state = 0;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.groan_timer(), 0x5A);
        assert_eq!(entity.status_flags & 2, 2, "intangible while grabbing");
        assert_eq!(entity.animation_id, 5);
        assert_eq!(game.entities[0].state(), 6, "the grab window");
        assert_eq!(game.entities[0].ignore(), 9, "the grab sub-window");
        assert_eq!(game.entities[0].action_behavior, 0);
        assert_eq!(game.entities[0].is_being_attacked, 1);

        // Mashing bleeds three frames of the meter per tick; the player's
        // attack direction mirrors it.
        game.dpad_held = 1;
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].groan_timer(), 0x57);
        assert_eq!(game.entities[0].action_ticks_counter, 0x57);

        // Frame 0x22 takes a bite out of the player.
        game.entities[1].animation_frame_id = 0x22;
        game.entities[1].timing_control = 2;
        let health = game.entities[0].health;
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[0].health, health - 10);

        // The grabbed player's own state-6 machine now runs: the root-motion
        // maul, the mashed countdown and the reverse EMW playback hand
        // control back once the player mashes free.
        game.player_damage = Some(std::sync::Arc::new(crate::game::PlayerDamageBank {
            keyframes: std::sync::Arc::new(vec![crate::model::Keyframe::default()]),
            clips: std::sync::Arc::new(
                (0..12)
                    .map(|_| crate::model::Clip {
                        frames: (0..3)
                            .map(|_| crate::model::ClipFrame {
                                keyframe: 0,
                                timing: 1,
                            })
                            .collect(),
                    })
                    .collect(),
            ),
        }));
        let emw: Vec<crate::model::Clip> = (0..8)
            .map(|_| crate::model::Clip {
                frames: (0..3)
                    .map(|_| crate::model::ClipFrame {
                        keyframe: 0,
                        timing: 1,
                    })
                    .collect(),
            })
            .collect();
        let mut player = crate::player::spawn(game.id, &RoomState::default());
        player.pos = game.entities[0].pos;
        for _ in 0..40 {
            crate::player_script::update(
                &mut game,
                &mut player,
                &RoomState::default(),
                &[],
                &emw,
                &[],
            );
            if game.entities[0].state() == 1 {
                break;
            }
        }
        assert_eq!(game.entities[0].state(), 1, "the player mauls free");
        assert_eq!(game.entities[0].ignore(), 0);
        assert_eq!(game.entities[0].action_behavior, 0);
        assert_eq!(game.entities[0].is_being_attacked, 0);
        assert_eq!(player.clip_source, crate::player::ClipSource::Emw);
        assert_eq!(player.anim.clip, 4, "the reverse weapon clip");
    }

    #[test]
    fn chimera_hit_stagger_and_release_rolls() {
        let pack = pack();
        let mut game = chimera(0);
        game.entities[1].set_state(2);
        game.entities[1].set_ignore(0);
        game.entities[1].hit_state = 0;
        game.entities[0].pos = [100, 0, 0];
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 1);
        assert_eq!(entity.action_behavior, 0, "the seed row for hit_state 0");
        assert_eq!(entity.animation_id, 0x0D);
        assert_eq!(entity.move_speed_current, 0xA0);
        assert!(game.entity_sounds.iter().any(|sound| sound.column == 7));

        // The release rolls a coin: a one lands on the swoop.
        game.entities[1].action_state = 2;
        let mut state = game.rand_state;
        loop {
            let mut probe = state;
            if crate::game::platform_rand(&mut probe) & 1 == 1 {
                break;
            }
            state = probe;
        }
        game.rand_state = state;
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.hit_state, 0);
        assert_eq!(entity.ignore(), 1);
        assert_eq!(entity.action_behavior, 7);
    }

    #[test]
    fn chimera_death_dissolve_raises_the_event() {
        let pack = pack();
        let mut game = chimera(0);
        game.entities[1].set_state(3);
        game.entities[1].set_ignore(0);
        game.entities[0].pos = [100, 0, 0];
        let mut host = LuaEnemyHost::new();
        let clips = clips();
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(
            entity.action_behavior, 0,
            "the turn bit lands on the ground death"
        );
        assert_eq!(entity.animation_id, 0x0B);
        assert!(game.entity_sounds.iter().any(|sound| sound.column == 8));

        for _ in 0..160 {
            let _ = step_clipped(&mut host, &mut game, &pack, &clips);
            if game.flags[crate::game::BANK_ENEMIES as usize].bit(3) {
                break;
            }
        }
        let entity = &game.entities[1];
        assert!(game.flags[crate::game::BANK_ENEMIES as usize].bit(3));
        assert_eq!(entity.status_flags & 0x0A, 0x0A);
        assert_eq!(entity.c_fade_freeze(), 1);
        assert_eq!(entity.shadow_tint, 0x00FF_FF50);
        assert_eq!(entity.action_state, 4);
    }

    #[test]
    fn chimera_ceiling_death_drops_before_dissolving() {
        let pack = pack();
        let mut game = chimera(2);
        game.entities[1].set_state(3);
        game.entities[1].set_ignore(0);
        let mut host = LuaEnemyHost::new();
        let clips = clips();
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_behavior, 4, "the drop death");
        assert!(game.entities[1].pos[1] < -4000);
        for _ in 0..200 {
            let _ = step_clipped(&mut host, &mut game, &pack, &clips);
            if game.flags[crate::game::BANK_ENEMIES as usize].bit(3) {
                break;
            }
        }
        assert!(game.flags[crate::game::BANK_ENEMIES as usize].bit(3));
    }

    #[test]
    fn chimera_shadow_resize_reads_joint_three() {
        let pack = pack();
        let mut game = chimera(0);
        let identity = crate::anim::Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [0, 0, 0],
        };
        game.joint_worlds[1] = vec![identity; 8];
        let mut host = LuaEnemyHost::new();
        game.joint_worlds[1] = vec![identity; 8];
        host.update(&mut game, &camera_room(), &pack, 1, &[]);
        // joint Y 0 -> size (0 >> 4) + 800 = 800; (-0 - 0x898) as u32 is huge,
        // so the 600 clamp does not fire; halves are 800 + 0x32 and 1000.
        assert_eq!(game.entities[1].shadow_half_x, 800 + 0x32);
        assert_eq!(game.entities[1].shadow_half_z, 1000);

        // The clamp band: joint Y = -0x898 puts -(y) - 0x898 at 0.
        game.joint_worlds[1] = vec![identity; 8];
        game.joint_worlds[1][3].t[1] = -0x898;
        game.entities[1].set_c_fade_freeze(0);
        host.update(&mut game, &camera_room(), &pack, 1, &[]);
        assert_eq!(game.entities[1].shadow_half_x, 600 + 0x32);

        // A frozen fade keeps the old quad.
        game.entities[1].set_c_fade_freeze(1);
        game.entities[1].shadow_half_x = 77;
        host.update(&mut game, &camera_room(), &pack, 1, &[]);
        assert_eq!(game.entities[1].shadow_half_x, 77);
    }

    #[test]
    fn chimera_every_behaviour_runs_without_errors() {
        let pack = pack();
        let clips = clips();
        for behavior in 0u16..=16 {
            let mut game = chimera((behavior % 2) as u8);
            game.entities[0].pos = [400, 0, 200];
            game.entities[1].set_ignore(1);
            game.entities[1].action_behavior = behavior as u8;
            game.entities[1].action_state = 0;
            game.rand_state = behavior as u32 * 13 + 1;
            let mut host = LuaEnemyHost::new();
            for tick in 0..30u32 {
                game.rand_seed = (tick * 7 + behavior as u32) as u16;
                assert!(
                    step_clipped(&mut host, &mut game, &pack, &clips),
                    "behavior {behavior} tick {tick}"
                );
            }
            assert_eq!(host.failed_scripts(), 0, "behavior {behavior}");
        }
    }

    #[test]
    fn chimera_reset_between_frames_preserves_the_scene() {
        let pack = pack();
        let run = |reset_every_tick: bool| {
            let mut game = chimera(2);
            game.entities[0].pos = [700, 0, 200];
            game.entities[0].health = 70;
            game.rand_state = 5;
            let mut host = LuaEnemyHost::new();
            let clips = clips();
            for tick in 0..45u32 {
                game.rand_seed = (tick * 5 + 1) as u16;
                if reset_every_tick {
                    host.reset();
                }
                assert!(step_clipped(&mut host, &mut game, &pack, &clips));
            }
            game
        };
        let steady = run(false);
        let reset = run(true);
        assert_eq!(steady.entities[1], reset.entities[1]);
        assert_eq!(steady.entities[0], reset.entities[0]);
        assert_eq!(steady.rand_state, reset.rand_state);
        assert_eq!(steady.flags, reset.flags);
    }
    #[test]
    fn chimera_script_sees_typed_scratch_properties() {
        let pack = pack_with(&[(
            "enemy/em09.lua",
            r#"
function update(e)
    e.c_repause = -0x1234
    e.c_fade_freeze = 0x1234
    e.c_wall_frames = -2
    e.c_far_latch = 0x1234
    e.writhe_velocity = -0x1234
    e.writhe_amplitude = 0x1234
    e.tint_flashes = -3
    e.reaction_timer = -4
    e.groan_timer = 0x2345
    e.sink_wobble = -5
    e.player_attack_direction = 0x12345
end
"#,
        )]);
        let mut game = chimera(0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.c_repause(), -0x1234);
        assert_eq!(entity.c_fade_freeze(), 0x1234);
        assert_eq!(entity.c_wall_frames(), -2);
        assert_eq!(entity.c_far_latch(), 0x1234);
        assert_eq!(entity.writhe_velocity(), -0x1234, "the turn step");
        assert_eq!(entity.writhe_amplitude(), 0x1234, "the path latch");
        assert_eq!(entity.tint_flashes(), -3, "the touch latch");
        assert_eq!(entity.reaction_timer, -4, "the wall hit");
        assert_eq!(entity.groan_timer(), 0x2345, "the grab meter");
        assert_eq!(entity.sink_wobble(), -5, "the hard word");
        assert_eq!(game.entities[0].action_ticks_counter, 0x2345);
    }
}

#[cfg(all(feature = "lua", test))]
mod hunter_tests {
    use crate::enemy::LuaEnemyHost;
    use crate::game::GameState;
    use crate::pack::{Pack, PackWriter};
    use crate::state::{RoomId, RoomState};

    fn source(name: &str) -> &'static str {
        crate::enemy::ENEMY_SCRIPTS
            .iter()
            .find(|(path, _)| *path == name)
            .unwrap_or_else(|| panic!("the checked-in {name} script"))
            .1
    }

    fn pack() -> Pack {
        let mut writer = PackWriter::new();
        writer
            .add(
                "enemy/em06.lua",
                source("enemy/em06.lua").as_bytes().to_vec(),
            )
            .unwrap();
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    fn pack_with(entries: &[(&str, &str)]) -> Pack {
        let mut writer = PackWriter::new();
        for (path, source) in entries {
            writer.add(path, source.as_bytes().to_vec()).unwrap();
        }
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    /// One active hunter at `slot` on a stage-1 room.
    fn hunter_at(slot: usize, behavior: u8) -> GameState {
        let mut game = GameState::default();
        game.id = RoomId::parse("1000").unwrap();
        let entity = &mut game.entities[slot];
        entity.id = 0x06;
        entity.set_active(true);
        entity.status_flags = 1;
        entity.behavior_flags = behavior;
        entity.death_event_id = 3;
        entity.pos = [0, 0, 0];
        entity.saved_pos = Some(entity.pos);
        game.entities[0].pos = [4000, 0, 0];
        game.entities[0].health = 100;
        game.enemy_count = 1;
        game
    }

    fn hunter(behavior: u8) -> GameState {
        hunter_at(1, behavior)
    }

    /// The room the intro selects rows on: the return-mansion winding
    /// corridor (directory stage 6, room 9; 0-based stage digit 5).
    fn room() -> RoomState {
        RoomState {
            stage: 6,
            room: 9,
            ..RoomState::default()
        }
    }

    fn step(host: &mut LuaEnemyHost, game: &mut GameState, pack: &Pack, slot: usize) -> bool {
        host.update(game, &room(), pack, slot, &[])
    }

    fn step_clipped(
        host: &mut LuaEnemyHost,
        game: &mut GameState,
        pack: &Pack,
        slot: usize,
        clips: &[crate::model::Clip],
    ) -> bool {
        host.update(game, &room(), pack, slot, clips)
    }

    /// Thirty-two one-frame clips, so every advance completes.
    fn clips() -> Vec<crate::model::Clip> {
        (0..32)
            .map(|_| crate::model::Clip {
                frames: vec![crate::model::ClipFrame {
                    keyframe: 0,
                    timing: 1,
                }],
            })
            .collect()
    }

    /// Thirty-two twelve-frame clips, so a behaviour's advance consumes one
    /// frame without wrapping.
    fn clips_long() -> Vec<crate::model::Clip> {
        (0..32)
            .map(|_| crate::model::Clip {
                frames: (0..12)
                    .map(|_| crate::model::ClipFrame {
                        keyframe: 0,
                        timing: 1,
                    })
                    .collect(),
            })
            .collect()
    }

    /// Aim every joint's world translation at one point, so the reach tests
    /// hit and the track helpers compose from an identity chain.
    fn world_at(game: &mut GameState, slot: usize, count: usize, pos: [i32; 3]) {
        let identity = crate::anim::Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: pos,
        };
        game.joint_worlds[slot] = vec![identity; count];
    }

    /// Point the clock at a clip and frame; the next advance publishes
    /// `frame + 1`, which is the word the hit windows test.
    fn set_frame(game: &mut GameState, slot: usize, animation: u8, frame: u8) {
        let entity = &mut game.entities[slot];
        entity.animation_id = animation;
        entity.animation_frame_id = frame;
        entity.timing_control = 0;
        entity.blend_counter = 7;
    }

    /// The identity skeleton and keyframe tables the track/recenter helpers
    /// compose through: every local transform is the identity.
    fn identity_pose(game: &mut GameState, slot: usize) {
        use std::sync::Arc;
        let keyframes = vec![crate::model::Keyframe {
            offset: [0, 0, 0],
            rotations: vec![[0, 0, 0]; 32],
        }];
        let skeleton = crate::model::Skeleton {
            relative: vec![[0, 0, 0]; 32],
            children: vec![vec![]; 32],
        };
        game.entity_anims[slot].keyframes = Some(Arc::new(keyframes));
        game.entity_anims[slot].skeleton = Some(Arc::new(skeleton));
    }

    /// The spawn health table, index `rand & 0xF`.
    fn health_roll(index: u16) -> i16 {
        const HEALTH: [i16; 16] = [
            0x5F, 0x5F, 0x5F, 0x5F, 0x4F, 0x5F, 0x4F, 0x6F, 0x5F, 0x5F, 0x4F, 0x5F, 0x5F, 0x4F,
            0x5F, 0x5F,
        ];
        HEALTH[index as usize]
    }

    /// The mid-health pounce chance table, index `rand & 0xF`.
    fn pounce_mid(index: u16) -> u8 {
        const MID: [u8; 16] = [1, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1];
        MID[index as usize]
    }

    // -----------------------------------------------------------------------
    // Initialisation, spawn kinds and the partner pick
    // -----------------------------------------------------------------------

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_init_rolls_health_sca_and_partner() {
        let pack = pack();
        let mut game = hunter(0);
        game.entities[2].status_flags = 1;
        let mut expected = game.rand_state;
        let draw = crate::game::platform_rand(&mut expected);
        let health = health_roll(draw & 0xF);

        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.health, health);
        assert_eq!(game.rand_state, expected, "one health draw");
        assert_eq!(entity.hunter_partner, 2, "the pair picks the next slot");
        assert_eq!(entity.behavior_flags, 2, "the pair bit is written back");
        assert_eq!(entity.sca_radius, 900, "stage 1 uses the wide record");
        assert_eq!(entity.sca_half_height, 180);
        assert_eq!(entity.sca_offset, [0, -180, 0]);
        assert_eq!(entity.shadow_tint, 0x0080_8080);
        assert_eq!((entity.shadow_half_x, entity.shadow_half_z), (1000, 1000));
        assert_eq!(entity.shadow_offset, [0, 0, 0]);
        assert_eq!(entity.hunter_path_latch(), 0);
        assert_eq!(entity.hunter_grab_word(), 0);
        assert_eq!(entity.hunter_death_cnt_a(), 0);
        assert_eq!(entity.hunter_death_cnt_b(), 0);

        // The late-game stage swaps in the tight record.
        let mut game = hunter(0x20);
        game.entities[2].status_flags = 1;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1));
        assert_eq!(game.entities[1].sca_radius, 500, "the late-game record");
        // The variant-0 pair roll reads the partner's status bit.
        let mut game = hunter(0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1));
        assert_eq!(game.entities[1].behavior_flags, 2);
        assert_eq!(game.entities[1].hunter_partner, 2);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_init_parks_scripted_unique_and_coward_variants() {
        let pack = pack();
        // The scripted intro kind parks in action behaviour 11.
        let mut game = hunter(0x0A);
        game.entities[2].status_flags = 1;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 1);
        assert_eq!(entity.action_behavior, 0x0B);
        assert_eq!(entity.behavior_flags, 0x0A, "no pair rewrite");

        // A lone hunter in the head slot pairs with slot 2 either way.
        let mut game = hunter(1);
        game.entities[2].status_flags = 1;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1));
        assert_eq!(game.entities[1].hunter_partner, 2);
        assert_eq!(game.entities[1].behavior_flags, 1, "the follower stays");
    }

    // -----------------------------------------------------------------------
    // The variant decisions and their draw order
    // -----------------------------------------------------------------------

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_variant_0_rewrites_the_empty_kind_and_reads_the_frame_seed() {
        let pack = pack();
        // A whole-byte zero is rewritten to the pouncer kind and returns.
        let mut game = hunter(0);
        game.entities[2].status_flags = 1;
        game.entities[2].health = 100;
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(0);
        game.entities[1].behavior_flags = 0;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1));
        assert_eq!(
            game.entities[1].behavior_flags, 2,
            "the empty kind is rewritten"
        );

        // The plain-hunter decision body (the late-game byte) rolls the leap
        // from the frame seed, not the stream.
        let mut game = hunter(0);
        game.entities[2].status_flags = 1;
        game.entities[2].health = 100;
        game.entities[2].set_hunter_speed(0);
        game.entities[0].pos = [400, 0, 0];
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(0);
        game.entities[1].behavior_flags = 0x20;
        game.entities[1].hunter_partner = 2;
        let seed_before = game.rand_state;
        let mut host = LuaEnemyHost::new();
        for bit in 0..2u16 {
            game.entities[1].set_hunter_pounce_latch(1);
            game.entities[1].set_state(1);
            game.entities[1].set_ignore(0);
            game.entities[1].behavior_flags = 0x20;
            game.entities[1].action_behavior = 2;
            game.entities[1].action_state = 0;
            game.entities[1].pos = [0, 0, 0];
            game.entities[1].saved_pos = Some(game.entities[1].pos);
            game.rand_seed = bit;
            assert!(step(&mut host, &mut game, &pack, 1));
            let entity = &game.entities[1];
            assert_eq!(game.rand_state, seed_before, "the leap reads the seed");
            assert_eq!(entity.hunter_joint_sel(), 6 + bit * 3);
            assert_eq!(entity.action_behavior, 5, "handed to the pounce");
            assert_eq!(entity.ignore(), 1);
            let yaw = i32::from(bit) * 300 - 0x96;
            let (rx, rz) = crate::player::rotate_xz(yaw as u16 & 0x0FFF, 400, 0);
            assert_eq!(entity.hunter_target_x(), (rx & 0xFFFF) as u16 as i16);
            assert_eq!(entity.hunter_target_z(), (rz & 0xFFFF) as u16 as i16);
            // The pounce start of the same frame picked its crouch clip.
            assert_eq!(
                entity.animation_id,
                (0x12u16 - entity.hunter_joint_sel()) as u8
            );
        }
    }

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_variant_2_draws_low_then_mid_then_the_leap() {
        let pack = pack();
        let mut game = hunter(2);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1)); // init

        // Find a stream position whose mid-table roll arms the latch.
        let mut probe = 1u32;
        let arm = loop {
            let mut state = probe;
            let _low = crate::game::platform_rand(&mut state) & 0xF;
            let mid = crate::game::platform_rand(&mut state) & 0xF;
            if pounce_mid(mid) == 1 {
                break probe;
            }
            probe += 1;
        };

        game.entities[1].health = 0x20;
        game.entities[0].health = 50;
        game.entities[0].pos = [6000, 0, 0];
        game.rand_state = arm;
        let mut expected = arm;
        let low = crate::game::platform_rand(&mut expected) & 0xF;
        let mid = crate::game::platform_rand(&mut expected) & 0xF;
        let leap = crate::game::platform_rand(&mut expected) & 1;

        assert!(step(&mut host, &mut game, &pack, 1));
        let entity = &game.entities[1];
        assert_eq!(game.rand_state, expected, "low, mid then leap draws");
        assert_eq!(entity.hunter_poise(), 0);
        assert_eq!(entity.action_behavior, 5, "the wounded-player leap fired");
        assert_eq!(entity.hunter_joint_sel(), 6 + leap * 3);
        // The pounce consumption of the same frame clears the latch again.
        assert_eq!(entity.hunter_pounce_latch(), 0);
        // The low roll armed the latch first, so both tables drew.
        let _ = (low, mid);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_variant_2_gates_on_the_player_health_and_distance() {
        let pack = pack();
        let mut game = hunter(2);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1));

        // A healthy player above the gate: no rolls at all.
        game.entities[1].health = 0x20;
        game.entities[0].health = 120;
        game.entities[0].pos = [6000, 0, 0];
        let before = game.rand_state;
        assert!(step(&mut host, &mut game, &pack, 1));
        assert_eq!(game.rand_state, before, "the gate rejects the roll");
        assert_eq!(game.entities[1].hunter_pounce_latch(), 0);

        // Wounded but very close: the clear-latch rule disarms it after the
        // low and mid draws.
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(0);
        game.entities[0].health = 50;
        game.entities[0].pos = [500, 0, 0];
        let mut expected = game.rand_state;
        crate::game::platform_rand(&mut expected);
        crate::game::platform_rand(&mut expected);
        assert!(step(&mut host, &mut game, &pack, 1));
        assert_eq!(game.rand_state, expected, "two draws then the clear");
        assert_eq!(game.entities[1].hunter_pounce_latch(), 0);
    }

    // -----------------------------------------------------------------------
    // The swipe, pounce and dodge action chains
    // -----------------------------------------------------------------------

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_swing_reaches_joint_nine_and_damages_the_player() {
        let pack = pack();
        let clips = clips_long();
        let mut game = hunter(0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1)); // init

        let entity = &mut game.entities[1];
        entity.set_state(1);
        entity.set_ignore(1);
        entity.action_behavior = 4;
        entity.action_state = 1;
        set_frame(&mut game, 1, 5, 4);
        let target = game.entities[0].pos;
        world_at(&mut game, 1, 16, target);
        game.entities[0].health = 100;
        game.entities[0].is_being_attacked = 0;

        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.action_state, 2, "the hit advances the swipe");
        assert_eq!(game.entities[0].health, 90);
        assert_eq!(game.entities[0].state(), 2, "the clawed pose word");
        assert_eq!(game.entities[0].ignore(), 0);
        assert_eq!(game.entities[0].action_behavior, 100);
        assert_eq!(game.entities[0].is_being_attacked, 1);
        assert!(game.entity_sounds.iter().any(|sound| sound.bank == 2));

        // The second playthrough takes 13.
        game.flags[usize::from(crate::game::BANK_SCENARIO)]
            .apply(crate::game::SCENARIO_FLAG_SECOND_PLAYTHROUGH, 0);
        game.entities[0].health = 100;
        game.entities[0].is_being_attacked = 0;
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 4;
        game.entities[1].action_state = 1;
        set_frame(&mut game, 1, 5, 4);
        let target = game.entities[0].pos;
        world_at(&mut game, 1, 16, target);
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert_eq!(game.entities[0].health, 87);

        // Out of the window the swing never reaches.
        game.entities[0].health = 100;
        game.entities[0].is_being_attacked = 0;
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 4;
        game.entities[1].action_state = 1;
        set_frame(&mut game, 1, 5, 20);
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert_eq!(game.entities[0].health, 100);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_pounce_bite_grabs_and_tracks_the_player_head() {
        let pack = pack();
        let clips = clips_long();
        let mut game = hunter(0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1)); // init

        let entity = &mut game.entities[1];
        entity.set_state(1);
        entity.set_ignore(1);
        entity.action_behavior = 5;
        entity.action_state = 1;
        entity.set_hunter_joint_sel(6);
        set_frame(&mut game, 1, 7, 7);
        let target = game.entities[0].pos;
        world_at(&mut game, 1, 16, target);
        game.entities[0].health = 100;
        game.entities[0].is_being_attacked = 0;

        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.action_state, 3, "the bite drags");
        assert_eq!(entity.hunter_ticks(), 4);
        assert_eq!(game.entities[0].state(), 7, "the bitten pose");
        assert_eq!(game.entities[0].ignore(), 6);
        assert_eq!(game.entities[0].action_behavior, 0);
        assert_eq!(game.entities[0].is_being_attacked, 1);

        // The drag sub-state raises the shared one-shot and tracks the chain.
        identity_pose(&mut game, 1);
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 5;
        game.entities[1].pos = [100, 0, 200];
        game.entities[1].angle = 0;
        game.entities[1].action_state = 3;
        set_frame(&mut game, 1, 7, 2);
        game.entities[1].set_hunter_joint_sel(6);
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert!(game.hunter_grab_one_shot, "the grab one-shot is raised");
        let (rx, rz) = crate::player::rotate_xz((11 - 6) * 600, 200, 0);
        assert_eq!(
            game.entities[0].joint_track,
            [100 + rx, 0, 200 + rz, 0],
            "the held head follows the mouth joint"
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_dodge_swipe_walks_its_two_reach_rows() {
        let pack = pack();
        let clips = clips_long();
        let mut game = hunter(0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1)); // init

        // Row 2 (frames 20..24) reaches joint 9 with a 1500 box.
        let entity = &mut game.entities[1];
        entity.set_state(1);
        entity.set_ignore(1);
        entity.action_behavior = 6;
        entity.action_state = 2;
        set_frame(&mut game, 1, 0x13, 20);
        let target = game.entities[0].pos;
        world_at(&mut game, 1, 16, target);
        game.entities[0].health = 100;
        game.entities[0].is_being_attacked = 0;
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert_eq!(game.entities[0].health, 85, "row 2 takes 15");
        assert_eq!(game.entities[1].action_state, 4, "the land sub-state");
        assert_eq!(game.entities[0].is_being_attacked, 3);
        assert_eq!(game.entities[0].action_behavior, 0x67);

        // The clawed pose leaves the player's escape surface live: a held pad
        // reads through the mash helpers while the hunter keeps its grip.
        game.dpad_held = crate::game::PAD_ACTION_HELD | 0x0004;
        assert!(game.player_mashing(), "the pad registers during the hold");
        assert_eq!(game.mash_reduce(), 5, "the face button plus a direction");
        game.dpad_held = 0;

        // Row 1 (frames 18/19) reaches joint 15 with a 600 box.
        game.entities[0].health = 100;
        game.entities[0].is_being_attacked = 0;
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 6;
        game.entities[1].action_state = 2;
        set_frame(&mut game, 1, 0x13, 18);
        let target = game.entities[0].pos;
        world_at(&mut game, 1, 16, [target[0] - 1000, target[1], target[2]]);
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert_eq!(game.entities[0].health, 85, "row 1 takes 15");
        assert_eq!(game.entities[1].action_state, 4);

        // Out of every window the swipe launches ballistically instead.
        game.entities[0].pos = [9000, 0, 9000];
        game.entities[0].health = 100;
        game.entities[0].is_being_attacked = 0;
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 6;
        game.entities[1].action_state = 2;
        set_frame(&mut game, 1, 0x13, 0);
        game.entities[1].set_hunter_speed(0x82);
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.pos[1], -0x244, "the arc's first frame rises");
        assert_eq!(entity.death_timer, 1, "the air tick counts");
        assert_eq!(entity.action_state, 2, "still airborne");
    }

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_pending_grab_cancels_into_the_leap_unless_below_the_floor() {
        let pack = pack();
        let clips = clips();
        let mut game = hunter(2);
        game.entities[2].status_flags = 1;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1)); // init

        // On the floor the pending grab word is consumed and the seeded
        // action restored.
        game.entities[1].set_state(2);
        game.entities[1].set_ignore(0);
        game.entities[1].hit_state = 0;
        game.entities[1].set_hunter_grab_word(1);
        game.entities[1].pos[1] = 0;
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.pos[1], 0);
        assert_eq!(entity.hunter_grab_word(), 0, "the word is consumed");
        assert_ne!(entity.action_behavior, 4, "the leap was cancelled");

        // Below the floor line the cancel is refused and the leap runs.
        game.entities[1].set_state(2);
        game.entities[1].set_ignore(0);
        game.entities[1].hit_state = 0;
        game.entities[1].set_hunter_grab_word(1);
        game.entities[1].pos[1] = -600;
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.action_behavior, 4, "the airborne grab leaps");
        assert_eq!(entity.hunter_grab_word(), 1, "the word survives");
        assert_eq!(entity.pos[1], -600, "and the position is untouched");
    }

    // -----------------------------------------------------------------------
    // Death paths
    // -----------------------------------------------------------------------

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_fall_death_raises_the_event_and_shrinks_the_shadow() {
        let pack = pack();
        let clips = clips_long();
        let mut game = hunter(0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1)); // init

        let entity = &mut game.entities[1];
        entity.set_state(3);
        entity.set_ignore(1);
        entity.action_behavior = 0;
        entity.action_state = 0;
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert_eq!(game.entities[1].health, -1);
        assert_eq!(game.entities[1].action_state, 1);
        assert_eq!(game.entities[1].hunter_ticks(), 0x46);
        assert!(game.entity_sounds.iter().any(|sound| sound.bank == 2));

        // The driver's quad stage raises the death event once, parks the speed
        // and raises the status bits.
        game.entities[1].action_state = 2;
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert_eq!(game.entities[1].shadow_tint, 0x00FF_FF50);
        assert_eq!(game.entities[1].action_state, 3);
        assert_eq!(game.entities[1].status_flags & 0x0A, 0x0A);
        assert!(
            game.flags[usize::from(crate::game::BANK_ENEMIES)].bit(3),
            "the death event rose"
        );
        let (half_x, half_z) = (
            game.entities[1].shadow_half_x,
            game.entities[1].shadow_half_z,
        );
        assert_eq!((half_x, half_z), (900, 900), "the quad shrinks by 100");

        // The shrink ticks down to the script-release stage.
        game.entities[1].set_hunter_ticks(1);
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert_eq!(game.entities[1].shadow_half_x, 912);
        assert_eq!(game.entities[1].action_state, 4);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_death_dispatch_seeds_its_behaviour_and_clears_the_latch() {
        let pack = pack();
        let clips = clips_long();
        let mut game = hunter(0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1)); // init

        let entity = &mut game.entities[1];
        entity.set_state(3);
        entity.set_ignore(0);
        entity.hit_state = 2;
        game.hunter_scream_latch = 1;
        game.entities[1].action_behavior = 0x77;
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 3);
        assert_eq!(entity.ignore(), 1);
        assert_eq!(entity.action_behavior, 2, "hit-state 2 seeds the thrash");
        assert_eq!(game.hunter_scream_latch, 0, "the latch resets on entry");
        assert_ne!(entity.animation_id, 0x77);
    }

    // -----------------------------------------------------------------------
    // The scripted intro
    // -----------------------------------------------------------------------

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_intro_selects_the_row_and_the_camera_variant() {
        let pack = pack();
        let clips = clips_long();
        let mut game = hunter(0x0A);
        game.id = RoomId::parse("6090").unwrap();
        game.attract_room_camera_id = 21;
        game.entities[2].status_flags = 1;
        let mut host = LuaEnemyHost::new();
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 11;
        game.entities[1].action_state = 0;

        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert_eq!(game.hunter_intro_jump_kind, 5, "camera 21 picks row 5");
        let entity = &game.entities[1];
        assert_eq!(entity.pos[0], -384 * 2, "the row-5 shuffle");
        assert_eq!(entity.pos[2], 0);
        assert_eq!(entity.animation_id, 0x00, "the flags-row animation");

        // The animation end snaps the entry yaw, parks in behaviour 2 and
        // drops the scripted-intro byte.
        set_frame(&mut game, 1, 0, 11);
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.action_behavior, 2);
        assert_eq!(entity.behavior_flags, 2);
        assert_eq!(entity.angle, 0, "the flags-0x0A row carries a zero yaw");

        // A different room picks the courtyard row 2.
        let mut game = hunter(0x08);
        game.id = RoomId::parse("30A0").unwrap();
        game.entities[2].status_flags = 1;
        let mut host = LuaEnemyHost::new();
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 11;
        game.entities[1].action_state = 0;
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert_eq!(game.hunter_intro_jump_kind, 2);
        let entity = &game.entities[1];
        assert_eq!(entity.pos[0], 0);
        assert_eq!(entity.pos[2], -486 * 2);
        assert_eq!(entity.animation_id, 0x18, "the flags-8 row animation");
        // Completing the walk adds the flags-8 entry yaw.
        set_frame(&mut game, 1, 0x18, 11);
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert_eq!(game.entities[1].angle, 0x0C00);

        // The west passage selects row 3.
        let mut game = hunter(0x08);
        game.id = RoomId::parse("6030").unwrap();
        game.entities[2].status_flags = 1;
        let mut host = LuaEnemyHost::new();
        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 11;
        game.entities[1].action_state = 0;
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert_eq!(game.hunter_intro_jump_kind, 3);
        assert_eq!(game.entities[1].pos[0], -396 * 2);
    }

    // -----------------------------------------------------------------------
    // The script-controlled table
    // -----------------------------------------------------------------------

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_scd_table_live_slots_all_run() {
        let pack = pack();
        let clips = clips();
        let mut host = LuaEnemyHost::new();
        let live = [
            0, 2, 3, 7, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
            29, 30, 32, 33, 34, 36, 37, 38,
        ];
        for &behavior in &live {
            // A hunter in slot 2 with the enemy-list base in slot 1, so the
            // bite helpers write a real neighbour.
            let mut game = {
                let mut g = GameState::default();
                g.id = RoomId::parse("1000").unwrap();
                g.entities[0].health = 100;
                g.entities[0].pos = [4000, 0, 0];
                g.entities[1].id = 0x06;
                g.entities[1].set_active(true);
                g.entities[1].status_flags = 1;
                g.entities[1].behavior_flags = 0x40;
                g.entities[1].death_event_id = 3;
                g.entities[2].id = 0x06;
                g.entities[2].set_active(true);
                g.entities[2].status_flags = 1;
                g.entities[2].behavior_flags = 0x40;
                g.entities[2].death_event_id = 4;
                g.entities[2].pos = [2000, 0, 0];
                g.entities[2].saved_pos = Some(g.entities[2].pos);
                g.enemy_count = 2;
                g
            };
            let entity = &mut game.entities[2];
            entity.set_state(8);
            entity.set_ignore(1);
            entity.action_behavior = behavior;
            entity.action_state = 0;
            entity.animation_id = 0;
            entity.animation_frame_id = 0;
            world_at(&mut game, 2, 16, [4000, 0, 0]);
            assert!(
                step_clipped(&mut host, &mut game, &pack, 2, &clips),
                "slot {behavior} runs"
            );
        }
        assert_eq!(host.failed_scripts(), 0, "no SCD slot errored");
    }

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_scd_bite_writes_the_target_and_clears_its_latch() {
        let pack = pack();
        let clips = clips_long();
        let mut game = {
            let mut g = GameState::default();
            g.id = RoomId::parse("1000").unwrap();
            g.entities[0].health = 100;
            g.entities[0].pos = [4000, 0, 0];
            g.entities[1].id = 0x06;
            g.entities[1].set_active(true);
            g.entities[1].status_flags = 1;
            g.entities[1].behavior_flags = 0x40;
            g.entities[1].death_event_id = 3;
            g.entities[2].id = 0x06;
            g.entities[2].set_active(true);
            g.entities[2].status_flags = 1;
            g.entities[2].behavior_flags = 0x40;
            g.entities[2].death_event_id = 4;
            g.entities[2].pos = [2000, 0, 0];
            g.entities[2].saved_pos = Some(g.entities[2].pos);
            g.enemy_count = 2;
            g
        };
        let mut host = LuaEnemyHost::new();
        game.entities[2].set_state(8);
        game.entities[2].set_ignore(1);
        game.entities[2].action_behavior = 37;
        game.entities[2].action_state = 1;
        set_frame(&mut game, 2, 1, 7);
        assert!(step_clipped(&mut host, &mut game, &pack, 2, &clips));
        assert_eq!(game.entities[1].hit_state, 1, "the bite latches the target");
        assert_eq!(game.entities[1].state_word(), 0x0002_0001);
        assert_eq!(game.entities[2].action_state, 2);
        assert_eq!(game.entities[2].hunter_ticks(), 4);

        // The bite end raises the flag, hands back through behaviour 7 and
        // clears bit 0 of the target's bite joint.
        game.entities[1].set_joint_flag(1, 0x41);
        game.entities[2].action_behavior = 38;
        game.entities[2].action_state = 0;
        set_frame(&mut game, 2, 1, 11);
        assert!(step_clipped(&mut host, &mut game, &pack, 2, &clips));
        assert_eq!(game.entities[1].joint_flag(1), 0x40, "bit 0 clears");
        assert_eq!(game.entities[2].action_behavior, 7);
        assert!(
            game.flags[usize::from(crate::game::BANK_SYSTEM)].bit(game.entities[2].scd_anim_param),
            "the script flag rose"
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_scd_composite_slots_re_enter_their_sub_ranges() {
        let pack = pack();
        let clips = clips();
        let mut game = hunter(0x40);
        game.entities[2].status_flags = 1;
        let mut host = LuaEnemyHost::new();
        // Slot 10 over the dodge sub-range: action_state 0 runs dodge start.
        game.entities[1].set_state(8);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 10;
        game.entities[1].action_state = 0;
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert_eq!(game.entities[1].action_state, 1);
        assert_eq!(game.entities[1].hunter_grab_word(), 1, "dodge start ran");
        // Slot 12 over the bite sub-range: action_state 1 runs the lunge.
        game.entities[1].set_state(8);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 12;
        game.entities[1].action_state = 0;
        game.entities[1].set_hunter_joint_sel(0);
        game.entities[1].animation_id = 0;
        game.entities[1].animation_frame_id = 0;
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert_eq!(game.entities[1].animation_id, 0xC, "the lunge setup ran");
        assert_eq!(host.failed_scripts(), 0);
    }

    // -----------------------------------------------------------------------
    // The path latch, the reset contract and the typed scratch view
    // -----------------------------------------------------------------------

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_path_latch_arms_the_repause_and_sticks() {
        let pack = pack();
        let clips = clips();
        let mut game = hunter(2);
        game.entities[2].status_flags = 1;
        let mut host = LuaEnemyHost::new();
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips)); // init

        game.entities[1].set_state(1);
        game.entities[1].set_ignore(1);
        game.entities[1].action_behavior = 3;
        for _ in 0..5 {
            assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        }
        assert_eq!(game.entities[1].hunter_path_latch(), 1, "the bit latched");
        assert!(
            game.entities[1].hunter_repause() > 0,
            "the fresh path armed the re-pause"
        );
        let before = game.entities[1].hunter_repause();
        assert!(step_clipped(&mut host, &mut game, &pack, 1, &clips));
        assert_eq!(game.entities[1].hunter_repause(), before - 1, "it ticks");
    }

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_reset_between_frames_preserves_the_scene() {
        let pack = pack();
        let clips = clips_long();
        let mut cached_game = hunter(0x0A);
        let mut reset_game = hunter(0x0A);
        for game in [&mut cached_game, &mut reset_game] {
            game.id = RoomId::parse("6090").unwrap();
            game.entities[2].status_flags = 1;
            game.entities[2].health = 100;
            game.entities[0].pos = [450, 0, 100];
            game.entities[0].health = 100;
            game.rand_state = 0x1234;
        }
        let mut cached = LuaEnemyHost::new();
        let mut reset = LuaEnemyHost::new();

        // A scene spanning init, the intro walk, the pounce chain, the swipe,
        // the dodge, the hold and both death layers.
        let scenario: &[(u8, u8, u8, u8)] = &[
            (0, 0, 0, 0),
            (1, 1, 11, 0),
            (1, 1, 11, 1),
            (1, 1, 5, 0),
            (1, 1, 5, 1),
            (1, 1, 4, 0),
            (1, 1, 4, 1),
            (1, 1, 6, 0),
            (1, 1, 6, 1),
            (1, 1, 7, 0),
            (1, 1, 7, 1),
            (2, 1, 0, 0),
            (2, 1, 0, 1),
            (2, 1, 2, 0),
            (2, 1, 4, 0),
            (2, 1, 5, 0),
            (2, 1, 8, 0),
            (3, 0, 0, 0),
            (3, 1, 0, 0),
            (3, 1, 2, 0),
            (3, 0, 0, 0),
            (8, 1, 0, 0),
            (8, 1, 2, 0),
            (8, 1, 22, 0),
        ];
        for (tick, &(state, ignore, behavior, action)) in scenario.iter().enumerate() {
            for game in [&mut cached_game, &mut reset_game] {
                game.entities[1].set_state(state);
                game.entities[1].set_ignore(ignore);
                game.entities[1].action_behavior = behavior;
                game.entities[1].action_state = action;
                if state == 3 {
                    game.entities[1].hit_state = 2;
                }
            }
            assert!(step_clipped(
                &mut cached,
                &mut cached_game,
                &pack,
                1,
                &clips
            ));
            reset.reset();
            assert!(step_clipped(&mut reset, &mut reset_game, &pack, 1, &clips));
            assert_eq!(
                cached_game, reset_game,
                "the hunter scene diverged at tick {tick} (state {state})"
            );
        }
    }

    #[cfg(feature = "lua")]
    #[test]
    fn hunter_script_sees_typed_scratch_properties() {
        let pack = pack_with(&[(
            "enemy/em06.lua",
            r#"
function update(e)
    e.hunter_speed = -0x1234
    assert(e.hunter_speed == -0x1234)
    e.hunter_ticks = -2
    assert(e.hunter_ticks == -2)
    e.hunter_path_latch = -0x7FFF
    assert(e.hunter_path_latch == -0x7FFF)
    e.hunter_grab_word = -3
    assert(e.hunter_grab_word == -3)
    e.hunter_target_x = -0x1234
    e.hunter_target_z = 0x1234
    assert(e.hunter_target_x == -0x1234)
    assert(e.hunter_target_z == 0x1234)
    -- The two halves preserve each other.
    e.hunter_joint_sel = 0xABCD
    assert(e.hunter_joint_sel == 0xABCD)
    e.hunter_step_word = 0x1234
    assert(e.hunter_step_word == 0x1234)
    e.hunter_room_hit = 0x1FF
    assert(e.hunter_room_hit == 0xFF)
    e.hunter_pounce_latch = 0x1FF
    assert(e.hunter_pounce_latch == 0xFF)
    e.hunter_death_cnt_a = 0x1FF
    assert(e.hunter_death_cnt_a == 0xFF)
    e.hunter_death_cnt_b = 0x1FF
    assert(e.hunter_death_cnt_b == 0xFF)
    e.hunter_strafe_dir = 0x1FF
    assert(e.hunter_strafe_dir == 0xFF)
    e.hunter_approach_cnt = 0x1FF
    assert(e.hunter_approach_cnt == 0xFF)
    e.hunter_poise = 0x1FF
    assert(e.hunter_poise == 0xFF)
    e.hunter_repause = 0x1FF
    assert(e.hunter_repause == 0xFF)
    e.hunter_leap_flag = 0x1FF
    assert(e.hunter_leap_flag == 0xFF)
    e.player_distance_z = -12345
    e.player_displacement = -23456
    e.scaled_down_dist = -34567
    assert(e.player_distance_z == -12345)
    assert(e.player_displacement == -23456)
    assert(e.scaled_down_dist == -34567)
    e.hunter_scream_latch = 1
    assert(e.hunter_scream_latch == 1)
    e.hunter_intro_jump_kind = 5
    assert(e.hunter_intro_jump_kind == 5)
    e.hunter_grab_one_shot = true
    assert(e.hunter_grab_one_shot == true)
    assert(e.attract_room_camera_id == 0x1F)
    assert(e.stage_id == 0)
    -- The line-of-sight helper writes its result byte into the shared
    -- distance scratch, the original's mid-frame overwrite.
    e.player_distance_z = -1
    local los = e:line_of_sight()
    assert(e.player_distance_z == los)
    local slot = e:pick_partner()
    assert(slot == 2)
    assert(e.hunter_partner == 2)
end
"#,
        )]);
        let mut game = hunter(2);
        game.entities[2].status_flags = 1;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, 1));
        assert_eq!(game.entities[1].hunter_speed(), -0x1234);
        assert_eq!(game.entities[1].hunter_ticks(), -2);
        assert_eq!(game.entities[1].hunter_path_latch(), -0x7FFF);
        assert_eq!(game.entities[1].hunter_grab_word(), -3);
        assert_eq!(game.entities[1].hunter_target_x(), -0x1234);
        assert_eq!(game.entities[1].hunter_target_z(), 0x1234);
        assert_eq!(game.entities[1].hunter_joint_sel(), 0xABCD);
        assert_eq!(game.entities[1].hunter_step_word(), 0x1234);
        assert_eq!(game.entities[1].hunter_room_hit(), 0xFF);
        assert_eq!(game.entities[1].hunter_strafe_dir(), 0xFF);
        assert_eq!(game.entities[1].hunter_approach_cnt(), 0xFF);
        assert_eq!(game.entities[1].hunter_poise(), 0xFF);
        assert_eq!(game.entities[1].hunter_repause(), 0xFF);
        assert_eq!(game.entities[1].hunter_leap_flag(), 0xFF);
        assert_eq!(game.entities[1].hunter_partner, 2);
        assert_eq!(game.player_distance_z, 0, "the line-of-sight write");
        assert_eq!(game.player_displacement, -23456);
        assert_eq!(game.scaled_down_dist, -34567);
        assert_eq!(game.hunter_scream_latch, 1);
        assert_eq!(game.hunter_intro_jump_kind, 5);
        assert!(game.hunter_grab_one_shot);
    }
}

#[cfg(all(test, feature = "lua"))]
mod monster_plant_tests {
    use crate::enemy::LuaEnemyHost;
    use crate::game::GameState;
    use crate::pack::{Pack, PackWriter};
    use crate::state::{RoomId, RoomState};

    fn source() -> &'static str {
        crate::enemy::ENEMY_SCRIPTS
            .iter()
            .find(|(path, _)| *path == "enemy/em0f.lua")
            .expect("the checked-in monster plant script")
            .1
    }

    fn pack() -> Pack {
        let mut writer = PackWriter::new();
        writer
            .add("enemy/em0f.lua", source().as_bytes().to_vec())
            .unwrap();
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    /// One active monster plant at slot 1 with the given spawn record.
    fn plant(behavior_flags: u8) -> GameState {
        let mut game = GameState::default();
        game.id = RoomId::parse("10c0").unwrap();
        let entity = &mut game.entities[1];
        entity.id = 0x0f;
        entity.set_active(true);
        entity.status_flags = 1;
        entity.behavior_flags = behavior_flags;
        entity.death_event_id = 5;
        entity.pos = [0, 0, 0];
        entity.saved_pos = Some(entity.pos);
        game.entities[0].pos = [4000, 0, 0];
        game.entities[0].health = 100;
        game.enemy_count = 1;
        game
    }

    fn step(host: &mut LuaEnemyHost, game: &mut GameState, pack: &Pack) -> bool {
        host.update(game, &RoomState::default(), pack, 1, &[])
    }

    fn step_clipped(
        host: &mut LuaEnemyHost,
        game: &mut GameState,
        pack: &Pack,
        clips: &[crate::model::Clip],
    ) -> bool {
        host.update(game, &RoomState::default(), pack, 1, clips)
    }

    /// Thirty-two one-frame clips: every advance completes on the tick it is
    /// called.
    fn clips() -> Vec<crate::model::Clip> {
        (0..32)
            .map(|_| crate::model::Clip {
                frames: vec![crate::model::ClipFrame {
                    keyframe: 0,
                    timing: 1,
                }],
            })
            .collect()
    }

    /// Thirty-two eight-frame clips, so an advance consumes one frame without
    /// wrapping and can land on a specific contact frame.
    fn long_clips() -> Vec<crate::model::Clip> {
        (0..32)
            .map(|_| crate::model::Clip {
                frames: (0..8)
                    .map(|_| crate::model::ClipFrame {
                        keyframe: 0,
                        timing: 1,
                    })
                    .collect(),
            })
            .collect()
    }

    fn draws(state: &mut u32, count: usize) -> Vec<u16> {
        (0..count)
            .map(|_| crate::game::platform_rand(state))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Init and the spawn kinds
    // -----------------------------------------------------------------------

    #[test]
    fn monster_plant_init_sets_up_the_vine() {
        let pack = pack();
        let mut game = plant(0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.state(), 1);
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.action_behavior, 0);
        assert_eq!(entity.action_state, 0);
        assert_eq!(entity.health, 1, "the awake plant starts at one");
        assert_eq!(entity.sca_radius, 0x0190);
        assert_eq!(entity.sca_half_height, 0);
        assert_eq!(entity.sca_offset, [0, 0, 0]);
        assert_eq!(entity.status_flags, (1 & 0x1F) | 10);
        assert_eq!((entity.shadow_half_x, entity.shadow_half_z), (500, 100));
        assert_eq!(entity.shadow_tint, 0x0040_4040);
        assert_eq!(entity.mp_shadow(), 1);
        assert_eq!(entity.mp_alerted(), 1);
        assert_eq!(entity.mp_hits(), 0);
        assert_eq!(entity.mp_fidget(), 0);
        assert_eq!(entity.mp_holdoff(), 0);
        assert_eq!(game.monster_plant_sides, 0);
        assert_eq!(entity.joint_flags, 0, "the vine starts revealed");
        assert_eq!(entity.animation_id, 0);
        assert_eq!(game.rand_state, 1, "init draws no stream values");
    }

    #[test]
    fn monster_plant_sprung_spawn_hides_the_vine() {
        let pack = pack();
        let mut game = plant(2);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.health, -1, "the sprung plant is immortal");
        assert_eq!(entity.mp_shadow(), 0, "and casts no shadow");
        assert_eq!(
            entity.joint_flags & 0x7FFF,
            0x7FFF,
            "all fifteen segments start hidden"
        );
    }

    // -----------------------------------------------------------------------
    // Variant selection and the exact draw order
    // -----------------------------------------------------------------------

    #[test]
    fn monster_plant_variant_0_rolls_the_idle_sway() {
        let pack = pack();
        let mut game = plant(0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let mut expected = game.rand_state;
        let rolls = draws(&mut expected, 2);
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 1, "variant 0 idles");
        assert_eq!(entity.status_flags & 0x08, 0);
        assert_eq!(
            entity.animation_id,
            if rolls[0] & 1 != 0 { 0 } else { 7 },
            "behaviour 1's sway pick overwrites the variant pose"
        );
        assert_eq!(entity.mp_fidget(), (rolls[1] & 0x1F) as i16);
        assert_eq!(game.rand_state, expected, "exactly two draws");
    }

    #[test]
    fn monster_plant_variant_2_retracts_and_dormants() {
        let pack = pack();
        let mut game = plant(2);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let mut expected = game.rand_state;
        let _ = draws(&mut expected, 2);
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 1);
        assert_eq!(entity.status_flags & 0x08, 0x08, "the hidden bit is raised");
        assert!(
            entity.animation_id == 0 || entity.animation_id == 7,
            "behaviour 1's sway pick runs on the variant's entry frame"
        );
        assert_eq!(entity.mp_shadow(), 0);
        assert_eq!(entity.joint_flags & 0x7FFF, 0x7FFF);
        assert_eq!(game.rand_state, expected);
    }

    #[test]
    fn monster_plant_variant_3_winds_up_the_grab() {
        let pack = pack();
        let mut game = plant(3);
        game.entities[1].angle = 0x900;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 2, "the wind-up behaviour");
        assert_eq!(entity.animation_id, 1);
        assert_eq!(entity.blend_counter, 0x1F);
        assert_eq!(
            entity.status_flags & 0xC0,
            0xC0,
            "spawn kind 3 raises the hidden/alerted pair"
        );
    }

    #[test]
    fn monster_plant_variant_4_picks_the_lunge_pose() {
        let pack = pack();
        for (angle, pose) in [(0x0000u16, 9u8), (0x0900, 2), (0x1000, 9), (0x2000, 9)] {
            let mut game = plant(4);
            game.entities[1].angle = angle;
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack));
            assert!(step(&mut host, &mut game, &pack));
            let entity = &game.entities[1];
            assert_eq!(entity.ignore(), 3, "the lunge behaviour");
            assert_eq!(entity.animation_id, pose, "angle {angle:#x}");
            assert_eq!(entity.status_flags & 2, 2, "the lunge is intangible");
        }
    }

    #[test]
    fn monster_plant_variant_6_arms_the_poison_pulse() {
        let pack = pack();
        let mut game = plant(7);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let mut expected = game.rand_state;
        let rolls = draws(&mut expected, 2);
        assert!(step_clipped(&mut host, &mut game, &pack, &long_clips()));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 5, "the wind-down behaviour");
        assert_eq!(entity.health, -1);
        assert_eq!(
            entity.animation_id,
            if rolls[0] & 1 != 0 { 0 } else { 7 },
            "the thrash pose pick"
        );
        assert_eq!(
            entity.animation_frame_id,
            (rolls[1] & 0xF) as u8 + 1,
            "the wind-down advance consumed one frame"
        );
        assert_eq!(entity.action_ticks_counter, 0, "behaviour 5 drains it");
        assert_eq!(entity.tint_queue, [2, -3, 0], "the poison pulse is armed");
        assert!(entity.tint_queue_armed);
        assert_eq!(game.rand_state, expected);
    }

    #[test]
    fn monster_plant_variant_8_flips_a_repeated_side() {
        let pack = pack();
        let mut game = plant(0x29);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        // Last two recorded sides both 1: the register reads 3.
        game.monster_plant_sides = 3;
        // Find a stream position whose first draw is odd, so the raw pick
        // would repeat the side and must be flipped.
        let mut state = 1u32;
        loop {
            let candidate = state;
            if crate::game::platform_rand(&mut state) & 1 == 1 {
                game.rand_state = candidate;
                break;
            }
        }
        let mut expected = game.rand_state;
        let first = crate::game::platform_rand(&mut expected);
        assert_eq!(first & 1, 1);
        let second = crate::game::platform_rand(&mut expected);
        assert!(step(&mut host, &mut game, &pack));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 6);
        assert_eq!(
            entity.animation_id, 0x13,
            "side 1 flipped to 0 selects the other lunge pose"
        );
        assert_eq!(game.monster_plant_sides, 6, "the register shifted");
        let _ = second;
        assert_eq!(
            entity.mp_timer_a(),
            0x27,
            "behaviour 6's first pulse tick already ran"
        );
        assert_eq!(entity.mp_angle_bk(), 2, "two red tint steps owed");
        assert_eq!(entity.mp_swerve(), 3, "three green tint steps owed");
    }

    // -----------------------------------------------------------------------
    // The grab chain, the player hold and the drain
    // -----------------------------------------------------------------------

    #[test]
    fn monster_plant_grab_chain_holds_and_releases_the_player() {
        let pack = pack();
        let mut game = plant(3);
        // Close enough to grab, dead ahead.
        game.entities[0].pos = [1000, 0, 0];
        game.entities[1].angle = 0;
        let clips = clips();
        let mut host = LuaEnemyHost::new();
        // Init.
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        // State check: variant 3 winds up and behaviour 2 runs step 0.
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].ignore(), 2);
        // Step 0: in reach, so step 1 and the grab pose are written.
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        let plant = game.entities[1];
        assert_eq!(plant.action_behavior, 2, "the grab step ran");
        let player = game.entities[0];
        assert_eq!(player.state(), 6, "the player enters the hold window");
        assert_eq!(player.ignore(), 0x0F, "the hold table index");
        assert_eq!(player.animation_id, 3, "the player faces the plant");
        assert_eq!(player.action_behavior, 0);
        assert_eq!(player.action_state, 0);
        assert_eq!(player.is_being_attacked, 1);
        assert_eq!(game.player_flags & 2, 2, "the grabbed flag");
        assert_eq!(plant.status_flags & 2, 2);

        // Step 2: the reel-in hands the drain its timers.
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_behavior, 3, "the drain step");
        assert_eq!(game.entities[1].action_ticks_counter, 0x3C);
        assert_eq!(game.entities[1].mp_timer_a(), 0x0C);

        // The drain: two health every twelve frames. Mashing shortens the
        // hold; run it out.
        let health_before = game.entities[0].health;
        let mut held = 0usize;
        while game.entities[1].action_behavior < 4 && held < 400 {
            assert!(step_clipped(&mut host, &mut game, &pack, &clips));
            held += 1;
        }
        assert!(
            game.entities[1].action_behavior >= 4,
            "the drain finished into the recovery step"
        );
        assert!(
            game.entities[0].health <= health_before - 2,
            "the drain drained health"
        );
        assert_eq!(
            game.entities[0].action_state, 2,
            "the release handler is flagged"
        );

        // The player-side window runs the release clip out and hands control
        // back at frame 0x27.
        let hold_clips: Vec<crate::model::Clip> = (0..4)
            .map(|_| crate::model::Clip {
                frames: (0..0x40)
                    .map(|_| crate::model::ClipFrame {
                        keyframe: 0,
                        timing: 1,
                    })
                    .collect(),
            })
            .collect();
        let mut player_state =
            crate::player::spawn(RoomId::parse("10c0").unwrap(), &RoomState::default());
        player_state.pos = game.entities[0].pos;
        for _ in 0..0x40 {
            crate::player_script::update(
                &mut game,
                &mut player_state,
                &RoomState::default(),
                &hold_clips,
                &[],
                &[],
            );
            if game.entities[0].state() == 1 {
                break;
            }
        }
        assert_eq!(game.entities[0].state(), 1, "the player is released");
        assert_eq!(game.player_flags & 2, 0);
        assert_eq!(game.entities[0].is_being_attacked, 0);
    }

    #[test]
    fn monster_plant_mashing_burns_the_hold_faster() {
        let pack = pack();
        let mut game = plant(3);
        game.entities[0].pos = [1000, 0, 0];
        let clips = clips();
        let mut host = LuaEnemyHost::new();
        for _ in 0..4 {
            assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        }
        assert_eq!(game.entities[1].action_behavior, 3);
        let before = game.entities[1].action_ticks_counter;
        game.dpad_held = crate::game::PAD_ACTION_HELD;
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        assert_eq!(
            game.entities[1].action_ticks_counter,
            before - 1 - 3,
            "mashing burns four frames per tick"
        );
    }

    #[test]
    fn monster_plant_step_0_diverts_to_recovery_when_unaligned() {
        let pack = pack();
        let mut game = plant(3);
        game.entities[0].pos = [0, 0, 1000];
        game.entities[1].angle = 0;
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        assert!(step(&mut host, &mut game, &pack));
        assert!(step(&mut host, &mut game, &pack));
        let plant = game.entities[1];
        assert_eq!(plant.action_behavior, 4, "the recovery step");
        assert_eq!(plant.mp_timer_a(), 2);
        assert_eq!(game.entities[0].state(), 0, "the player is never grabbed");
    }

    // -----------------------------------------------------------------------
    // The bite and the vine reveal/retract stop tables
    // -----------------------------------------------------------------------

    #[test]
    fn monster_plant_bite_lands_on_frame_six() {
        let pack = pack();
        let mut game = plant(4);
        game.entities[0].pos = [600, 0, 0];
        game.entities[1].angle = 0;
        let clips = long_clips();
        let mut host = LuaEnemyHost::new();
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        let plant = game.entities[1];
        assert_eq!(plant.ignore(), 3, "the lunge behaviour");
        // Force the bite's landing frame and re-run step 6; the advance
        // moves the pending frame onto the contact frame 6.
        game.entities[1].action_behavior = 0;
        game.entities[1].animation_id = 0;
        game.entities[1].animation_frame_id = 5;
        game.entities[0].is_being_attacked = 0;
        let health = game.entities[0].health;
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        let player = game.entities[0];
        assert_eq!(player.health, health - 2);
        assert_eq!(player.action_behavior, 0x67, "a front hit");
        assert_eq!(player.is_being_attacked, 2);
        assert_eq!(game.entities[1].mp_holdoff(), 1);
    }

    #[test]
    fn monster_plant_extend_stops_reveal_the_vine_in_order() {
        let pack = pack();
        let mut game = plant(0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        // Park the strike behaviour directly on the extend step.
        let entity = &mut game.entities[1];
        entity.set_ignore(4);
        entity.action_behavior = 0;
        entity.set_mp_seg(14);
        entity.set_mp_timer_a(0);
        entity.action_ticks_counter = 0;
        entity.set_joint_flag(14, 0);
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].mp_seg(), 7, "stop 8 revealed 14..8");
        assert_eq!(game.entities[1].action_ticks_counter, 1);
        assert_eq!(
            game.entities[1].joint_flags & (1 << 8),
            0,
            "segment 8 is revealed"
        );
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].mp_seg(), 3, "stop 4 revealed 7..4");
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].mp_seg(), 1, "stop 2 revealed 3..2");
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].mp_seg(), -1, "stop 0 revealed 1..0");
        assert_eq!(game.entities[1].action_ticks_counter, 4);
        assert_eq!(game.entities[1].joint_flags & 0x7FFF, 0, "the vine is out");
    }

    #[test]
    fn monster_plant_retract_stops_hide_the_vine_in_order() {
        let pack = pack();
        let mut game = plant(0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        let entity = &mut game.entities[1];
        entity.set_ignore(4);
        entity.action_behavior = 3; // step 11
        entity.animation_frame_id = 5;
        entity.set_mp_seg(0);
        entity.action_ticks_counter = 0;
        entity.set_mp_anim_end(0);
        // The original's retract index guard bounds the stop table; the
        // advance moves the pending frame onto 6, past the frame-2 gate.
        assert!(step_clipped(&mut host, &mut game, &pack, &long_clips()));
        assert_eq!(game.entities[1].mp_seg(), 1);
        assert_eq!(game.entities[1].joint_flags & 1, 1, "segment 0 is hidden");
        assert_eq!(game.entities[1].action_ticks_counter, 1);
    }

    // -----------------------------------------------------------------------
    // Damage, death and the head hit override
    // -----------------------------------------------------------------------

    #[test]
    fn monster_plant_damaged_counts_hits_and_raises_the_progress_flag() {
        let pack = pack();
        let mut game = plant(0);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack));
        assert!(step(&mut host, &mut game, &pack));
        let snapshot = game.entities[1].state_word();
        for hit in 1..=4u16 {
            game.entities[1].set_state(2);
            game.entities[1].hit_state = 0x11;
            assert!(step(&mut host, &mut game, &pack));
            assert_eq!(game.entities[1].mp_hits(), hit as i16);
            assert_eq!(game.entities[1].hit_state, 0, "the latch is cleared");
            assert_eq!(
                game.entities[1].state_word(),
                snapshot,
                "the interrupted behaviour is restored"
            );
            if hit <= 3 {
                assert!(
                    !game.flags[0].bit(0x5B),
                    "the progress flag waits for the fourth hit"
                );
            }
        }
        assert!(
            game.flags[0].bit(0x5B),
            "the fourth hit raises the combat-progression flag"
        );
    }

    #[test]
    fn monster_plant_die_consumes_the_exact_draw_order() {
        let pack = pack();
        let mut game = plant(0);
        let clips = clips();
        let mut host = LuaEnemyHost::new();
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));

        // Case 0: two rolls summed into the thrash counter, then case 1 runs
        // on the same tick and burns one off.
        let mut expected = game.rand_state;
        let rolls = draws(&mut expected, 2);
        let ticks = (rolls[0] & 0x1F) + (rolls[1] & 0x1F) + 1;
        // The same tick's case 1 runs the thrash wrap: one roll for the 50%
        // pose gate, then one more when the gate passes.
        let wrap = draws(&mut expected, 1)[0];
        if wrap & 0x80 != 0 {
            let _ = draws(&mut expected, 1);
        }
        game.entities[1].set_state(3);
        game.entities[1].set_ignore(0);
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.health, -1, "the death pins the health");
        assert_eq!(entity.action_ticks_counter, ticks - 1);
        assert_eq!(entity.ignore(), 1);
        assert_eq!(
            game.rand_state, expected,
            "two case-0 draws, then the thrash wrap's rolls"
        );

        // Case 1: run the counter down, then the collapse's four draws with
        // the first discarded.
        game.entities[1].action_ticks_counter = 1;
        let mut expected = game.rand_state;
        let rolls = draws(&mut expected, 4);
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        let entity = &game.entities[1];
        assert_eq!(entity.ignore(), 2, "the twitch phase");
        assert_eq!(entity.animation_id, if rolls[1] & 1 != 0 { 0 } else { 7 });
        assert_eq!(entity.animation_frame_id, (rolls[2] & 0xF) as u8);
        assert_eq!(entity.action_ticks_counter, (rolls[3] & 0xF) + 0x32);
        assert_eq!(entity.mp_timer_a(), 4);
        assert_eq!(entity.mp_timer_b(), 8);
        assert_eq!(game.rand_state, expected, "four draws, one discarded");

        // Case 2: the twitch rate tapers, then the death event fires.
        game.entities[1].action_ticks_counter = 1;
        let before = game.rand_state;
        assert!(step_clipped(&mut host, &mut game, &pack, &clips));
        assert_ne!(game.rand_state, before, "case 2 rolls every frame");
        assert_eq!(game.entities[1].action_ticks_counter, 0);
        assert_eq!(game.entities[1].ignore(), 3);
        assert!(
            game.flags[crate::game::BANK_ENEMIES as usize].bit(5),
            "the spawn record's death event is raised"
        );
    }

    #[test]
    fn monster_plant_head_hit_override_tracks_joint_eleven() {
        let pack = pack();
        let mut game = plant(0);
        let mut host = LuaEnemyHost::new();
        let identity = crate::anim::Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [100, -20, 40],
        };
        game.joint_worlds[1] = vec![identity; 15];
        game.entities[1].pos = [10, 5, 20];
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(
            game.entities[1].sca_hit_delta,
            [90, -25, 20],
            "the hit point is head minus body"
        );

        // A paused plant keeps its last hit point.
        game.message_flags &= !crate::game::MESSAGE_FLAG_MONSTERS;
        game.joint_worlds[1] = vec![
            crate::anim::Mat4x3 {
                r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                t: [900, 0, 900],
            };
            15
        ];
        assert!(step(&mut host, &mut game, &pack));
        assert_eq!(game.entities[1].sca_hit_delta, [90, -25, 20]);
    }

    #[test]
    fn monster_plant_reset_between_frames_matches() {
        let pack = pack();
        let clips = clips();
        let scene = |reset: bool| {
            let mut game = plant(3);
            game.entities[0].pos = [1000, 0, 0];
            let mut host = LuaEnemyHost::new();
            for tick in 0..40 {
                if reset && tick > 0 {
                    host.reset();
                }
                assert!(host.update(&mut game, &RoomState::default(), &pack, 1, &clips));
            }
            (game.entities[1], game.rand_state, game.monster_plant_sides)
        };
        assert_eq!(scene(false), scene(true), "the VM reset changes nothing");
    }
}

#[cfg(all(test, feature = "lua"))]
mod computer_arms_tests {
    use crate::enemy::LuaEnemyHost;
    use crate::game::GameState;
    use crate::pack::{Pack, PackWriter};
    use crate::state::{RoomId, RoomState};

    fn source(path: &str) -> &'static str {
        crate::enemy::ENEMY_SCRIPTS
            .iter()
            .find(|(entry, _)| *entry == path)
            .unwrap_or_else(|| panic!("the checked-in {path} script"))
            .1
    }

    fn pack() -> Pack {
        let mut writer = PackWriter::new();
        for path in ["enemy/em14.lua", "enemy/em15.lua"] {
            writer.add(path, source(path).as_bytes().to_vec()).unwrap();
        }
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    /// One active arm at slot 1 with the given spawn record and character.
    fn spawn_arm(id: u8, behavior_flags: u8, jill: bool) -> GameState {
        let mut game = GameState::default();
        game.id = RoomId {
            player_flag: u8::from(jill),
            ..RoomId::parse("5060").unwrap()
        };
        let entity = &mut game.entities[1];
        entity.id = id;
        entity.set_active(true);
        entity.status_flags = 1;
        entity.behavior_flags = behavior_flags;
        entity.pos = [1000, -1242, 2000];
        entity.saved_pos = Some(entity.pos);
        game.enemy_count = 1;
        game
    }

    fn step(
        host: &mut LuaEnemyHost,
        game: &mut GameState,
        pack: &Pack,
        clips: &[crate::model::Clip],
    ) -> bool {
        host.update(game, &RoomState::default(), pack, 1, clips)
    }

    /// Thirty-two one-frame clips: every advance completes on its tick.
    fn clips() -> Vec<crate::model::Clip> {
        (0..32)
            .map(|_| crate::model::Clip {
                frames: vec![crate::model::ClipFrame {
                    keyframe: 0,
                    timing: 1,
                }],
            })
            .collect()
    }

    /// Thirty-two twelve-frame clips: an advance consumes one frame.
    fn long_clips() -> Vec<crate::model::Clip> {
        (0..32)
            .map(|_| crate::model::Clip {
                frames: (0..12)
                    .map(|_| crate::model::ClipFrame {
                        keyframe: 0,
                        timing: 1,
                    })
                    .collect(),
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Init
    // -----------------------------------------------------------------------

    #[test]
    fn computer_arm_init_records_home_and_fixed_point() {
        let pack = pack();
        let mut game = spawn_arm(0x14, 0x80, false);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips()));
        let right = &game.entities[1];
        assert_eq!(right.state(), 1);
        assert_eq!(right.ignore(), 0);
        assert_eq!(right.health, 1);
        assert_eq!(right.status_flags, (1 & 0x1F) | 4);
        assert_eq!(right.target[0], 1000, "the right arm records home X");
        assert_eq!(right.target[2], 2000, "and home Z");
        assert_eq!(right.target[1], 0, "but no spawn Y");
        assert_eq!(right.arm_pos_x(), 1000 << 16);
        assert_eq!(right.arm_pos_z(), 2000 << 16);
        assert_eq!(right.has_enter_switch_zone, 0, "parked arms stay hidden");

        let mut game = spawn_arm(0x15, 0x80, false);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips()));
        let left = &game.entities[1];
        assert_eq!(left.target[1], -1242, "the left arm records its spawn Y");
        assert_eq!(left.arm_pos_x(), 1000 << 16);
        assert_eq!(left.arm_pos_z(), 2000 << 16);
    }

    // -----------------------------------------------------------------------
    // Command 0 and the protocol latches
    // -----------------------------------------------------------------------

    #[test]
    fn computer_arm_rest_glides_home_and_skips_when_close() {
        let pack = pack();
        let mut game = spawn_arm(0x14, 0, false);
        let clips = clips();
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips));
        // Displace the fixed-point position 400 units past home.
        game.entities[1].set_arm_pos_x(1400 << 16);
        game.entities[1].pos[0] = 1400;
        assert!(step(&mut host, &mut game, &pack, &clips));
        let arm = &game.entities[1];
        assert_eq!(arm.ignore(), 1, "the rest sub-state");
        assert_eq!(arm.action_ticks_counter, 7, "the first glide tick ran");
        assert_eq!(arm.arm_vel_x(), trunc(-400 * 0x10000, 8));
        assert_eq!(arm.pos[0], 1400 + (-400 / 8), "the first glide step");
        assert_eq!(arm.pos[2], 2000, "Z is already home");

        // An arm already within 200 units skips the glide entirely.
        let mut game = spawn_arm(0x14, 0, false);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips));
        game.entities[1].set_arm_pos_x((1000 + 150) << 16);
        game.entities[1].pos[0] = 1150;
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_ticks_counter, 0, "the skip");
        assert_eq!(game.entities[1].pos[0], 1150, "no glide");
    }

    #[test]
    fn computer_arm_command_protocol_runs_the_reach_chain() {
        let pack = pack();
        let mut game = spawn_arm(0x14, 0, false);
        let clips = clips();
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips));

        // Command 1 with the new-command latch.
        game.entities[1].behavior_flags = 0x41;
        assert!(step(&mut host, &mut game, &pack, &clips));
        let arm = &game.entities[1];
        assert_eq!(arm.behavior_flags, 1, "the 0x40 latch is consumed");
        assert_eq!(arm.ignore(), 1, "the reach sub-state");
        assert_eq!(arm.animation_id, 1, "reach A's animation");
        assert_eq!(arm.arm_vel_x(), trunc(-0x28 * 0x10000, 15));
        assert_eq!(arm.arm_vel_z(), trunc(0x28 * 0x10000, 15));

        // The shared move step advances the clip and completes the command.
        assert!(step(&mut host, &mut game, &pack, &clips));
        let arm = &game.entities[1];
        assert_eq!(arm.behavior_flags & 0x20, 0x20, "the done bit is raised");
        assert_eq!(arm.ignore(), 0, "command_done clears the sub-state");

        // The done bit parks the driver.
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].ignore(), 0);

        // Command 2 enters the chain at reach B.
        game.entities[1].behavior_flags = 0x42;
        assert!(step(&mut host, &mut game, &pack, &clips));
        let arm = &game.entities[1];
        assert_eq!(arm.behavior_flags, 2);
        assert_eq!(arm.ignore(), 1);
        assert_eq!(arm.animation_id, 1, "Chris' reach B pose");
        assert_eq!(
            arm.arm_vel_x(),
            trunc((1040 - arm.pos[0]) * 0x10000, 15),
            "the reach starts from the hand's live position"
        );
        assert_eq!(arm.arm_vel_z(), trunc((2000 - arm.pos[2]) * 0x10000, 15));
    }

    #[test]
    fn computer_arm_command_five_is_jills_no_op() {
        let pack = pack();
        let clips = clips();
        let mut game = spawn_arm(0x14, 0, true);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips));
        game.entities[1].behavior_flags = 0x45;
        assert!(step(&mut host, &mut game, &pack, &clips));
        let arm = &game.entities[1];
        assert_eq!(arm.behavior_flags, 5, "the latch is consumed");
        assert_eq!(arm.ignore(), 0, "nothing starts");
        assert_eq!(arm.action_behavior, 0);
        assert_eq!(arm.behavior_flags & 0x20, 0, "and nothing completes");

        // Chris' command 5 enters the lift step.
        let mut game = spawn_arm(0x14, 0, false);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips));
        game.entities[1].behavior_flags = 0x45;
        assert!(step(&mut host, &mut game, &pack, &clips));
        let arm = &game.entities[1];
        assert_eq!(arm.ignore(), 1, "the lift sub-state");
        assert_eq!(arm.action_ticks_counter, 4);
        assert_eq!(arm.arm_vel_z(), trunc(0x50 * 0x10000, 4));
    }

    #[test]
    fn computer_arm_command_eight_falls_off_the_table() {
        let pack = pack();
        let mut game = spawn_arm(0x14, 0, false);
        let clips = clips();
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips));
        game.entities[1].behavior_flags = 0x48;
        for expected in 1..=3 {
            assert!(step(&mut host, &mut game, &pack, &clips));
            let arm = &game.entities[1];
            assert_eq!(arm.ignore(), expected, "reach A re-runs every frame");
            assert_eq!(arm.behavior_flags & 0x20, 0, "and never completes");
        }
    }

    // -----------------------------------------------------------------------
    // The right arm's login/typing voice handshake
    // -----------------------------------------------------------------------

    #[test]
    fn computer_arm_voice_handshake_waits_for_the_engine() {
        let pack = pack();
        let mut game = spawn_arm(0x14, 0, false);
        let clips = clips();
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips));
        game.entities[1].behavior_flags = 0x44;
        // Login: Chris gets the slide and his voice line.
        assert!(step(&mut host, &mut game, &pack, &clips));
        let arm = &game.entities[1];
        assert_eq!(arm.ignore(), 1);
        assert_eq!(arm.action_behavior, 0);
        assert_eq!(arm.animation_id, 4);
        assert_eq!(arm.blend_counter, 7);
        assert_eq!(arm.arm_vel_z(), trunc(200 * 0x10000, 0x12));
        assert!(game.voice.request.is_some(), "the login line is queued");
        assert!(game.voice_playing(), "and the playing bit is raised");
        // The typing press runs, then waits on the bit.
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_behavior, 1, "the voice wait");
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].behavior_flags & 0x20, 0);
        // The engine finishes the line: the command completes.
        game.clear_voice_playing();
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].behavior_flags & 0x20, 0x20);
        assert_eq!(game.entities[1].ignore(), 0);

        // Jill's login queues her line and never slides the hand.
        let mut game = spawn_arm(0x14, 0, true);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips));
        game.entities[1].behavior_flags = 0x44;
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert!(game.voice.request.is_some());
        assert_eq!(game.entities[1].arm_vel_x(), 0, "no slide for Jill");
        assert_eq!(game.entities[1].arm_vel_z(), 0);
    }

    // -----------------------------------------------------------------------
    // The left arm's own chains
    // -----------------------------------------------------------------------

    #[test]
    fn computer_arm_fifteen_command_five_gesture() {
        let pack = pack();
        let clips = clips();
        // Jill: begin -> start animation 5 -> play it out.
        let mut game = spawn_arm(0x15, 0, true);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips));
        game.entities[1].behavior_flags = 0x45;
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].ignore(), 1, "the begin step");
        assert_eq!(game.entities[1].action_behavior, 0);
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_behavior, 1);
        assert_eq!(game.entities[1].animation_id, 5);
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].behavior_flags & 0x20, 0x20);
        assert_eq!(game.entities[1].ignore(), 0);

        // Chris: begin -> slide -> wait -> animation 5 -> play.
        let mut game = spawn_arm(0x15, 0, false);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips));
        game.entities[1].behavior_flags = 0x45;
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_behavior, 1, "the Chris slide");
        assert_eq!(game.entities[1].action_ticks_counter, 4);
        assert_eq!(game.entities[1].arm_vel_z(), trunc(-0x50 * 0x10000, 4));
        for _ in 0..4 {
            assert!(step(&mut host, &mut game, &pack, &clips));
        }
        assert_eq!(game.entities[1].action_behavior, 2, "animation 5 starts");
        assert_eq!(game.entities[1].animation_id, 5);
        for _ in 0..8 {
            if game.entities[1].behavior_flags & 0x20 != 0 {
                break;
            }
            assert!(step(&mut host, &mut game, &pack, &clips));
        }
        assert_eq!(game.entities[1].behavior_flags & 0x20, 0x20);
    }

    #[test]
    fn computer_arm_fifteen_command_seven_drops_and_hangs() {
        let pack = pack();
        let clips = long_clips();
        let mut game = spawn_arm(0x15, 0, false);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips));
        game.entities[1].behavior_flags = 0x47;
        // The hold, shortened to one frame.
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].ignore(), 1);
        assert_eq!(game.entities[1].action_ticks_counter, 0xA0);
        game.entities[1].action_ticks_counter = 1;
        assert!(step(&mut host, &mut game, &pack, &clips));
        let arm = &game.entities[1];
        assert_eq!(arm.ignore(), 2, "the drop");
        assert_eq!(arm.action_ticks_counter, 0x0C);
        assert_eq!(arm.arm_y(), -1242 << 16);
        assert_eq!(arm.arm_vel_y(), -0xC0000);
        assert_eq!(arm.arm_vel_x(), trunc(-100 * 0x10000, 0x0C));
        assert_eq!(arm.arm_vel_z(), trunc(-400 * 0x10000, 0x0C));

        // Gravity falls and clamps on the recorded spawn height.
        let mut saw_gravity = false;
        for _ in 0..0x10 {
            let before = game.entities[1].arm_vel_y();
            assert!(step(&mut host, &mut game, &pack, &clips));
            if game.entities[1].ignore() == 2 {
                assert!(game.entities[1].arm_vel_y() > before, "gravity ramps");
                saw_gravity = true;
            } else {
                break;
            }
        }
        assert!(saw_gravity);
        let arm = &game.entities[1];
        assert_eq!(arm.ignore(), 3, "the hanging loop");
        assert_eq!(arm.pos[1], -1242, "clamped on the spawn height");
        assert_eq!(arm.animation_id, 2);
        assert_eq!(arm.animation_frame_id, 0x0E);
        assert_eq!(arm.action_ticks_counter, 0x5A);

        // The loop's repeat timer draws the stream for Chris.
        game.entities[1].action_ticks_counter = 1;
        let mut expected = game.rand_state;
        let draw = crate::game::platform_rand(&mut expected);
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].action_ticks_counter, (draw & 0x1F) + 0x0A);
        assert_eq!(game.rand_state, expected, "one loop draw");
        assert_eq!(game.entities[1].animation_frame_id, 0, "restarted");
    }

    #[test]
    fn computer_arm_fifteen_command_eight_pulls_the_lever() {
        let pack = pack();
        let clips = clips();
        let mut game = spawn_arm(0x15, 0, false);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips));
        game.entities[1].behavior_flags = 0x48;
        assert!(step(&mut host, &mut game, &pack, &clips));
        let arm = &game.entities[1];
        assert_eq!(arm.ignore(), 1, "the lever reach");
        assert_eq!(arm.animation_id, 6);
        assert_eq!(arm.action_ticks_counter, 4);
        assert_eq!(arm.arm_vel_x(), trunc(-0x28 * 0x10000, 4));
        assert_eq!(arm.arm_vel_z(), trunc(-0x8C * 0x10000, 4));
        // Shorten the glide, then play the pull.
        game.entities[1].action_ticks_counter = 1;
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].ignore(), 2, "the pull");
        assert_eq!(game.entities[1].animation_id, 6);
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert_eq!(
            game.entities[1].behavior_flags & 0x20,
            0x20,
            "the lever pull raises the done bit"
        );
        assert_eq!(
            game.entities[1].ignore(),
            2,
            "but keeps its sub-state until the driver resets it"
        );
        assert!(step(&mut host, &mut game, &pack, &clips));
        assert_eq!(game.entities[1].ignore(), 0, "the parked driver clears it");
    }

    #[test]
    fn computer_arm_division_truncates_toward_zero() {
        let pack = pack();
        let clips = clips();
        let mut game = spawn_arm(0x14, 0, false);
        let mut host = LuaEnemyHost::new();
        assert!(step(&mut host, &mut game, &pack, &clips));
        // Put the hand three units past home so the X delta is negative.
        game.entities[1].set_arm_pos_x((1000 + 3) << 16);
        game.entities[1].pos[0] = 1003;
        game.entities[1].behavior_flags = 0x41;
        assert!(step(&mut host, &mut game, &pack, &clips));
        // (-0x2B << 16) / 15 truncates toward zero, not down.
        assert_eq!(game.entities[1].arm_vel_x(), -187_869);
        assert_eq!(game.entities[1].arm_vel_z(), 174_762);
    }

    #[test]
    fn computer_arms_reset_between_frames_matches() {
        let pack = pack();
        let clips = clips();
        let scene = |reset: bool| {
            let mut game = spawn_arm(0x14, 0, false);
            game.entities[2].id = 0x15;
            game.entities[2].set_active(true);
            game.entities[2].status_flags = 1;
            game.entities[2].behavior_flags = 0;
            game.entities[2].pos = [1000, -1242, 1300];
            game.entities[2].saved_pos = Some(game.entities[2].pos);
            game.enemy_count = 2;
            let mut host = LuaEnemyHost::new();
            for tick in 0..60 {
                if reset && tick > 0 {
                    host.reset();
                }
                let flags = match tick {
                    0..=19 => 0,
                    20..=39 => 0x41,
                    _ => 0x48,
                };
                game.entities[1].behavior_flags = flags;
                game.entities[2].behavior_flags = flags;
                assert!(host.update(&mut game, &RoomState::default(), &pack, 1, &clips));
                assert!(host.update(&mut game, &RoomState::default(), &pack, 2, &clips));
            }
            (
                game.entities[1],
                game.entities[2],
                game.rand_state,
                game.voice.request.map(|request| request.name),
            )
        };
        assert_eq!(scene(false), scene(true), "the VM reset changes nothing");
    }

    /// C integer division truncates toward zero.
    fn trunc(a: i32, b: i32) -> i32 {
        a / b
    }

    mod plant42_tests {
        use crate::enemy::LuaEnemyHost;
        use crate::game::GameState;
        use crate::pack::{Pack, PackWriter};
        use crate::state::{RoomId, RoomState};

        fn source() -> &'static str {
            crate::enemy::ENEMY_SCRIPTS
                .iter()
                .find(|(path, _)| *path == "enemy/em08.lua")
                .expect("the checked-in Plant 42 script")
                .1
        }

        fn pack() -> Pack {
            let mut writer = PackWriter::new();
            writer
                .add("enemy/em08.lua", source().as_bytes().to_vec())
                .unwrap();
            Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
        }

        /// One active Plant 42 at slot 1 with the given spawn record.
        fn plant(behavior_flags: u8) -> GameState {
            let mut game = GameState::default();
            game.id = RoomId::parse("40c0").unwrap();
            let entity = &mut game.entities[1];
            entity.id = 0x08;
            entity.set_active(true);
            entity.status_flags = 1;
            entity.behavior_flags = behavior_flags;
            entity.variant = 0x20;
            entity.death_event_id = 5;
            entity.pos = [0, 0, 0];
            entity.saved_pos = Some(entity.pos);
            game.entities[0].pos = [4000, 0, 0];
            game.entities[0].health = 100;
            game.enemy_count = 1;
            game
        }

        /// The room row the enemy-sound table resolves (the roots tests' row).
        fn room() -> RoomState {
            RoomState {
                stage: 4,
                room: 0x0F,
                ..RoomState::default()
            }
        }

        /// Thirty-two 64-frame clips: an advance consumes one frame without
        /// wrapping across the frame windows the handlers test.
        fn clips() -> Vec<crate::model::Clip> {
            (0..32)
                .map(|_| crate::model::Clip {
                    frames: (0..0x40)
                        .map(|_| crate::model::ClipFrame {
                            keyframe: 0,
                            timing: 1,
                        })
                        .collect(),
                })
                .collect()
        }

        fn step(host: &mut LuaEnemyHost, game: &mut GameState, pack: &Pack) -> bool {
            host.update(game, &room(), pack, 1, &clips())
        }

        /// Give the head joints a low world translation so the idle's
        /// head-rise reroll stays quiet (the port computes the worlds only
        /// with a model; the harness has none).
        fn head_low(game: &mut GameState) {
            let mut worlds = vec![
                crate::anim::Mat4x3 {
                    r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                    t: [0, 0, 0],
                };
                16
            ];
            worlds[14].t = [0, -1000, 0];
            worlds[15].t = [0, -1000, 0];
            game.joint_worlds[1] = worlds;
        }

        fn draws(state: &mut u32, count: usize) -> Vec<u16> {
            (0..count)
                .map(|_| crate::game::platform_rand(state))
                .collect()
        }

        // -------------------------------------------------------------------
        // Init and the spawn kinds
        // -------------------------------------------------------------------

        #[test]
        fn plant42_boss_init_spawns_companions_and_consumes_the_draws() {
            let pack = pack();
            let mut game = plant(0);
            game.rand_state = 7;
            let mut expected = 7u32;
            draws(&mut expected, 1); // discarded
            let life = crate::game::platform_rand(&mut expected) & 0xFF;
            let _body_draw = crate::game::platform_rand(&mut expected) & 3;
            let roots_draw = crate::game::platform_rand(&mut expected) & 3;
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            assert_eq!(game.rand_state, expected, "the four init draws in order");

            let entity = game.entities[1];
            assert_eq!(entity.state(), 1);
            assert_eq!(entity.health, 140, "the boss pool is 0x28 + 100");
            assert_eq!(entity.sca_radius, 2000);
            assert_eq!(entity.sca_half_height, 7000);
            assert_eq!(entity.hit_state, 1);
            assert_eq!(entity.p42_step(), 0);
            assert_eq!(entity.p42_life(), life as i16 + 0x32);
            assert_eq!((entity.shadow_half_x, entity.shadow_half_z), (2000, 300));
            assert_eq!(entity.shadow_tint, 0x0040_4040);
            let body = entity.plant42_body.expect("the body spawned");
            let roots = entity.plant42_roots.expect("the roots spawned");
            assert_eq!(game.plant42_shared_body, Some(body));
            let body_companion = game.companions[usize::from(body)].as_ref().unwrap();
            assert_eq!(body_companion.vine_pool, 4);
            assert_eq!(body_companion.health, 140);
            let roots_companion = game.companions[usize::from(roots)].as_ref().unwrap();
            assert_eq!(roots_companion.action_state, u8::from(roots_draw & 3 == 0));
        }

        #[test]
        fn plant42_split_vine_init_keeps_the_small_record_and_no_companions() {
            let pack = pack();
            let mut game = plant(1);
            game.rand_state = 3;
            let mut expected = 3u32;
            draws(&mut expected, 2); // discard + life only
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            assert_eq!(game.rand_state, expected);
            let entity = game.entities[1];
            assert_eq!(entity.health, 0x28, "the split vine keeps 40");
            assert_eq!(entity.sca_radius, 200);
            assert_eq!(entity.sca_half_height, 250);
            assert_eq!(entity.plant42_body, None);
            assert_eq!(entity.plant42_roots, None);
            assert_eq!(entity.hit_state, 0);
        }

        #[test]
        fn plant42_fallen_core_init_parks_the_companions() {
            let pack = pack();
            let mut game = plant(4);
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            let entity = game.entities[1];
            assert_eq!(entity.state(), 4);
            let body = entity.plant42_body.unwrap();
            let companion = game.companions[usize::from(body)].as_ref().unwrap();
            assert_eq!(companion.health, -1);
            assert_eq!((companion.state, companion.action_state), (2, 3));
            assert_eq!(companion.scale, [0x19DC, 0x0818, 0x19DC]);
            let roots = entity.plant42_roots.unwrap();
            let roots_companion = game.companions[usize::from(roots)].as_ref().unwrap();
            assert_eq!(roots_companion.entity.pos[1], -1300);
            assert_eq!(roots_companion.action_state, 2);
            // The ungated tail parks the switch-zone bit for the cutscene.
            assert_eq!(entity.has_enter_switch_zone, 0);
        }

        #[test]
        fn plant42_chris_hold_init_suspends_the_player() {
            let pack = pack();
            let mut game = plant(6);
            game.entities[0].pos = [0, 0, 0];
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            let entity = game.entities[1];
            assert_eq!((entity.state(), entity.ignore()), (8, 1));
            assert_eq!(entity.action_behavior, 0x0B);
            assert_eq!(entity.action_state, 1);
            assert_eq!(entity.p42_osc_a(), 3);
            assert_eq!(entity.p42_osc_b(), 6);
            assert_eq!(entity.p42_step(), 0, "the setup tick consumed the first");
            assert_eq!(game.entities[0].zone_flags & 0x80, 0x80);
        }

        #[test]
        fn plant42_poison_arena_init_scales_the_model() {
            let pack = pack();
            let mut game = plant(5);
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            assert_eq!(game.entities[1].joint_scale, 0x12C0);
        }

        #[test]
        fn plant42_boss_fight_init_enters_the_scd_state() {
            let pack = pack();
            let mut game = plant(0x40);
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            let entity = game.entities[1];
            assert_eq!(entity.state(), 8);
            // The SCD table's entry 0 idles and keeps the boss in state 8.
            assert!(entity.action_behavior < 17);
        }

        // -------------------------------------------------------------------
        // The idle selector and the action gates
        // -------------------------------------------------------------------

        #[test]
        fn plant42_idle_setup_consumes_five_draws_in_order() {
            let pack = pack();
            let mut game = plant(0);
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack)); // init
            // Park the machine in the idle's setup state.
            game.entities[1].state_field = 1;
            game.entities[1].set_ignore(1);
            game.entities[1].action_behavior = 0;
            game.entities[1].action_state = 0;
            game.rand_state = 11;
            let mut expected = 11u32;
            let roll = draws(&mut expected, 5);
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            let entity = game.entities[1];
            assert_eq!(entity.p42_ticks(), (roll[0] & 0xF) as i16 + 0xF - 1);
            assert_eq!(
                entity.p42_yaw_jitter(),
                (1 - i32::from(roll[3] & 2)) as i8 * (roll[1] & 0xF) as i8
            );
            assert_eq!(
                entity.p42_roll_jitter(),
                (1 - i32::from(roll[4] & 2)) as i8 * (roll[2] & 7) as i8
            );
            assert_eq!(entity.action_state, 1);
        }

        #[test]
        fn plant42_range_gates_select_the_next_action() {
            let pack = pack();
            let mut game = plant(0);
            game.entities[0].pos = [9500, 0, 0];
            game.entities[1].angle = 0;
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            // 9500 is inside the 10000 gate but outside the 9000 gate: the
            // aligned step-0x80 branch writes 0x00010101 and rolls the +1.
            game.rand_state = 5;
            let mut expected = 5u32;
            let roll = crate::game::platform_rand(&mut expected) & 1;
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            let entity = game.entities[1];
            assert_eq!((entity.state(), entity.ignore()), (1, 1));
            assert_eq!(entity.action_behavior, 1 + roll as u8);
            assert_eq!(game.rand_state, expected);
        }

        // -------------------------------------------------------------------
        // Behaviours
        // -------------------------------------------------------------------

        #[test]
        fn plant42_sweep_runs_the_full_chain() {
            let pack = pack();
            let mut game = plant(0);
            game.entities[0].pos = [2000, 0, 0];
            game.entities[1].angle = 0;
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack)); // init
            // Park the sweep's setup.
            game.entities[1].state_field = 1;
            game.entities[1].set_ignore(1);
            game.entities[1].action_behavior = 1;
            game.entities[1].action_state = 0;
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            let entity = game.entities[1];
            assert_eq!(entity.animation_id, 0x0C);
            assert_eq!(entity.p42_ticks(), 0x31, "the fall-through consumed one");
            assert_eq!(entity.action_state, 1, "case 0 fell through into case 1");
            // Wind up: the timer counts down to the B clip.
            for _ in 0..0x32 {
                head_low(&mut game);
                assert!(step(&mut host, &mut game, &pack));
            }
            assert_eq!(game.entities[1].action_state, 2);
            // The B clip's fifteen frames.
            for _ in 0..0x10 {
                head_low(&mut game);
                assert!(step(&mut host, &mut game, &pack));
            }
            assert_eq!(game.entities[1].action_state, 4);
            // The strike accelerates the step through state 6 and re-arms.
            let mut saw_six = false;
            for _ in 0..0x40 {
                head_low(&mut game);
                assert!(step(&mut host, &mut game, &pack));
                if game.entities[1].action_state == 6 {
                    saw_six = true;
                }
            }
            assert!(saw_six, "the strike passed through the contact state");
            assert_eq!(game.entities[1].state(), 1, "the sweep re-arms");
            assert_eq!(game.entities[1].p42_step(), 0);
        }

        #[test]
        fn plant42_spit_hit_drops_the_player_into_the_hit_reaction() {
            let pack = pack();
            let mut game = plant(0);
            game.entities[0].pos = [0, 0, 0];
            game.entities[1].angle = 0;
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            game.entities[1].state_field = 1;
            game.entities[1].set_ignore(1);
            game.entities[1].action_behavior = 2;
            game.entities[1].action_state = 1;
            game.entities[1].animation_id = 7;
            game.entities[1].animation_frame_id = 10;
            game.entities[1].timing_control = 0;
            game.entities[1].blend_counter = 0;
            game.entities[1].set_p42_step(4);
            game.entities[0].health = 100;
            // The mouth joint sits on the player.
            let mut worlds = vec![
                crate::anim::Mat4x3 {
                    r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                    t: [0, -1000, 0],
                };
                16
            ];
            worlds[15].t = [0, 0, 0];
            game.joint_worlds[1] = worlds;
            assert!(step(&mut host, &mut game, &pack));
            let player = game.entities[0];
            assert_eq!(player.state(), 2, "the generic hit reaction");
            assert_eq!(player.ignore(), 0);
            assert_eq!(player.action_behavior, 0x64);
            assert_eq!(player.action_state, 0);
            assert_eq!(player.health, 92, "8 damage on the first playthrough");
            assert_eq!(player.is_being_attacked, 1);
        }

        #[test]
        fn plant42_move_grabs_the_player_and_lifts() {
            let pack = pack();
            let mut game = plant(0);
            game.entities[0].pos = [-600, 0, 0];
            game.entities[1].angle = 0;
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            game.entities[1].state_field = 1;
            game.entities[1].set_ignore(1);
            game.entities[1].action_behavior = 3;
            game.entities[1].action_state = 2;
            game.entities[1].set_p42_ticks(1);
            // The head and grab joints sit near the player.
            let mut worlds = vec![
                crate::anim::Mat4x3 {
                    r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                    t: [0, -1000, 0],
                };
                16
            ];
            worlds[11].t = [-600, -500, 0];
            worlds[14].t = [-600, -1000, 0];
            worlds[12].t = [-600, -1000, 0];
            game.joint_worlds[1] = worlds;
            game.entities[0].pos = [-600, 0, 0];
            assert!(step(&mut host, &mut game, &pack));
            let entity = game.entities[1];
            assert_eq!(entity.action_state, 3, "the grab committed");
            assert_eq!(entity.p42_grab_joint(), 15);
            assert_eq!(entity.animation_id, 0x0E);
            assert_eq!(entity.hit_state, 1);
            let player = game.entities[0];
            assert_eq!(player.state(), 6);
            assert_eq!(player.ignore(), 8);
            assert_eq!(player.action_behavior, 1);
            assert_eq!(player.is_being_attacked, 1);
            assert_ne!(game.player_speed, [0, 0, 0], "the lift vector is armed");
        }

        #[test]
        fn plant42_hold_drops_the_player_and_damages() {
            let pack = pack();
            let mut game = plant(0);
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            // Park the hold's drop state with the player already low.
            game.entities[1].state_field = 1;
            game.entities[1].set_ignore(1);
            game.entities[1].action_behavior = 4;
            game.entities[1].action_state = 8;
            game.entities[1].set_p42_grab_joint(15);
            game.entities[0].pos = [0, -2000, 0];
            game.entities[0].health = 100;
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            let entity = game.entities[1];
            assert_eq!(entity.action_state, 9, "case 8 fell into case 9");
            let player = game.entities[0];
            assert_eq!(player.health, 80, "0x14 drop damage");
            assert_eq!(player.action_behavior, 2);
            assert_eq!(player.state(), 6);
            assert_eq!(player.zone_flags & 0x80, 0, "the grabbed bit is cleared");
            assert_eq!(game.player_flags & 2, 0);
        }

        #[test]
        fn plant42_award_kill_raises_the_flag_and_withers_the_slots() {
            let pack = pack();
            let mut game = plant(0);
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            // Fill the enemy list with live slots 2..=6 and one dead slot.
            for slot in 2..=6 {
                game.entities[slot].id = 0x00;
                game.entities[slot].set_active(true);
                game.entities[slot].health = 10;
                game.enemy_count += 1;
            }
            game.entities[5].health = -1;
            // Drop the vine pool to one and kill the last vine.
            let body = game.entities[1].plant42_body.unwrap();
            game.companions[usize::from(body)]
                .as_mut()
                .unwrap()
                .vine_pool = 2;
            game.entities[1].state_field = 3;
            game.entities[1].set_ignore(0);
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            assert!(
                game.flags[usize::from(crate::game::BANK_SCENARIO)]
                    .bit(crate::game::SCENARIO_FLAG_PLANT42_DEAD)
            );
            assert_eq!(
                game.companions[usize::from(body)]
                    .as_ref()
                    .unwrap()
                    .vine_pool,
                1
            );
            for slot in 1..=6 {
                if slot == 5 {
                    assert_eq!(game.entities[slot].health, -1, "the dead slot is left");
                    continue;
                }
                assert_eq!(game.entities[slot].state(), 1, "slot {slot} withers");
                assert_eq!(game.entities[slot].ignore(), 1);
                assert_eq!(game.entities[slot].action_behavior, 9);
                assert_eq!(game.entities[slot].hit_state, 1);
            }
        }

        #[test]
        fn plant42_death_consumes_the_exact_draw_order() {
            let pack = pack();
            let mut game = plant(0);
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack)); // init
            game.entities[1].state_field = 3;
            game.entities[1].set_ignore(0);
            game.entities[1].action_behavior = 0;
            game.entities[1].action_state = 0;
            game.rand_state = 9;
            let mut expected = 9u32;
            let draws = draws(&mut expected, 7);
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            assert_eq!(game.rand_state, expected, "the seven setup draws in order");
            let entity = game.entities[1];
            assert_eq!(entity.p42_ticks(), 0);
            assert_eq!(entity.p42_pod_counter(), 5);
            assert_eq!(
                entity.p42_death_counter(),
                ((draws[0] % 0xE) as i8 + 0x0F) - 1,
                "the fall consumed one counter"
            );
            let base = (draws[1] & 7) as i16 + 8;
            assert_eq!(
                entity.p42_step(),
                if draws[2] & 1 != 0 { -base } else { base }
            );
            assert_eq!(entity.p42_osc_a(), ((draws[3] & 1) << 11) as i16);
            assert_eq!(entity.p42_osc_b(), ((draws[4] & 1) * 0x800 + 0x400) as i16);
            assert_eq!(
                entity.p42_yaw_jitter(),
                ((draws[5] & 1) as i32 * -0x40 + 0x20) as i8
            );
            assert_eq!(
                entity.p42_roll_jitter(),
                ((draws[6] as i8 as i32) * -0x80 + 0x40) as i8
            );
            assert_eq!(entity.joint_scale, 0x1000 - 0x10);
            assert!(
                game.flags[usize::from(crate::game::BANK_ENEMIES)].bit(5),
                "the death event is raised"
            );
        }

        #[test]
        fn plant42_wither_rewinds_the_scale_and_the_companions() {
            let pack = pack();
            let mut game = plant(0);
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack)); // init
            game.entities[1].state_field = 1;
            game.entities[1].set_ignore(1);
            game.entities[1].action_behavior = 9;
            game.entities[1].action_state = 0;
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            let entity = game.entities[1];
            assert_eq!(entity.joint_scale, 0x1000 - 0x20);
            let body = entity.plant42_body.unwrap();
            let companion = game.companions[usize::from(body)].as_ref().unwrap();
            assert_eq!((companion.state, companion.flag_7e), (1, 7));
            // Case 6 restores the companions and the scale.
            game.entities[1].action_state = 6;
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            let entity = game.entities[1];
            assert_eq!(entity.joint_scale, 0);
            assert_eq!(entity.hit_state, 0);
            let companion = game.companions[usize::from(body)].as_ref().unwrap();
            assert_eq!((companion.state, companion.flag_7e), (0, 0));
            assert_eq!(entity.state(), 1);
        }

        #[test]
        fn plant42_capture_and_hold_round_trip_the_player() {
            let pack = pack();
            let mut game = plant(0);
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack)); // init
            game.entities[1].state_field = 1;
            game.entities[1].set_ignore(1);
            game.entities[1].action_behavior = 4;
            game.entities[1].action_state = 0;
            game.entities[1].set_p42_grab_joint(15);
            game.entities[0].pos = [200, 50, 0];
            game.entities[0].angle = 0;
            // The grabbing joint sits at the plant origin.
            head_low(&mut game);
            let mut worlds = vec![
                crate::anim::Mat4x3 {
                    r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                    t: [0, 0, 0],
                };
                16
            ];
            worlds[15].t = [100, 0, 0];
            game.joint_worlds[1] = worlds;
            assert!(step(&mut host, &mut game, &pack));
            // The hold wrote the capture matrix and re-applied it through the
            // same joint: the player's relative transform is preserved.
            assert_eq!(
                game.plant42_capture.t,
                [-0x60F, 0, 700],
                "the lift offset overwrote the relative translation"
            );
            assert_eq!(game.entities[0].pos, [100 - 0x60F, 0, 700]);
            assert_eq!(game.entities[0].zone_flags & 0x80, 0x80);
        }

        #[test]
        fn plant42_head_hit_override_tracks_joint_fourteen() {
            let pack = pack();
            let mut game = plant(0);
            let mut host = LuaEnemyHost::new();
            head_low(&mut game);
            assert!(step(&mut host, &mut game, &pack));
            let mut worlds = vec![
                crate::anim::Mat4x3 {
                    r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                    t: [0, -1000, 0],
                };
                16
            ];
            worlds[14].t = [123, -456, 789];
            game.joint_worlds[1] = worlds;
            game.entities[1].pos = [10, 20, 30];
            assert!(step(&mut host, &mut game, &pack));
            assert_eq!(game.entities[1].sca_hit_delta, [113, -476, 759]);
        }

        #[test]
        fn plant42_reset_between_frames_preserves_the_scene() {
            let pack = pack();
            let scene = |reset: bool| {
                let mut game = plant(0);
                game.entities[0].pos = [4000, 0, 0];
                let mut host = LuaEnemyHost::new();
                for tick in 0..40 {
                    if reset && tick > 0 {
                        host.reset();
                    }
                    // Alternate between the state check and the damage state
                    // so both the selector and the reaction run.
                    if tick % 7 == 3 {
                        game.entities[1].set_state(2);
                        game.entities[1].hit_state = 0x11;
                    }
                    head_low(&mut game);
                    assert!(step(&mut host, &mut game, &pack));
                }
                (
                    game.entities[1],
                    game.entities[0],
                    game.rand_state,
                    game.plant42_capture,
                )
            };
            assert_eq!(scene(false), scene(true), "the VM reset changes nothing");
        }
    }

    mod yawn_tests {
        use crate::enemy::LuaEnemyHost;
        use crate::game::GameState;
        use crate::model::{Clip, ClipFrame, Keyframe, Skeleton};
        use crate::pack::{Pack, PackWriter};
        use crate::state::{RoomId, RoomState};
        use std::sync::Arc;

        fn source(id: u8) -> &'static str {
            let path = format!("enemy/em{id:02x}.lua");
            crate::enemy::ENEMY_SCRIPTS
                .iter()
                .find(|(entry, _)| *entry == path)
                .expect("the checked-in Yawn script")
                .1
        }

        fn pack() -> Pack {
            let mut writer = PackWriter::new();
            for id in [0x0Du8, 0x12] {
                let path = format!("enemy/em{id:02x}.lua");
                writer.add(&path, source(id).as_bytes().to_vec()).unwrap();
            }
            Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
        }

        /// The synthetic fifteen-joint snake the init poses: a straight chain
        /// hanging down, joint 0 the root, joint 1 a side branch.
        fn model() -> (Arc<Skeleton>, Arc<Vec<Keyframe>>, Vec<Clip>) {
            let mut relative = vec![[0i16; 3]; 15];
            relative[0] = [0, -1700, 0];
            for joint in relative.iter_mut().skip(1) {
                *joint = [0, -300, 0];
            }
            let mut children: Vec<Vec<u8>> = vec![Vec::new(); 15];
            children[0] = vec![1, 2];
            for (joint, child) in children.iter_mut().enumerate().take(14).skip(2) {
                *child = vec![(joint + 1) as u8];
            }
            let skeleton = Skeleton { relative, children };
            let keyframes: Vec<Keyframe> = (0..4)
                .map(|index| Keyframe {
                    offset: [0, -1700, 0],
                    rotations: vec![[0, (index as i16) * 0x40, 0]; 15],
                })
                .collect();
            let clips = (0..32)
                .map(|_| Clip {
                    frames: (0..4)
                        .map(|keyframe| ClipFrame {
                            keyframe,
                            timing: 1,
                        })
                        .collect(),
                })
                .collect();
            (Arc::new(skeleton), Arc::new(keyframes), clips)
        }

        /// One active Yawn in slot 1 with the synthetic model installed.
        fn yawn(id: u8, behavior_flags: u8) -> (GameState, Vec<Clip>) {
            let mut game = GameState::default();
            game.id = RoomId::parse("40C0").unwrap();
            let entity = &mut game.entities[1];
            entity.id = id;
            entity.set_active(true);
            entity.status_flags = 1;
            entity.behavior_flags = behavior_flags;
            entity.variant = 0x20;
            entity.death_event_id = 5;
            entity.pos = [0, 0, 0];
            entity.saved_pos = Some(entity.pos);
            entity.animation_id = 1;
            game.entities[0].pos = [4000, 0, 0];
            game.entities[0].health = 100;
            game.enemy_count = 1;
            let (skeleton, keyframes, clips) = model();
            game.entity_anims[1].skeleton = Some(skeleton);
            game.entity_anims[1].keyframes = Some(keyframes);
            (game, clips)
        }

        fn step(
            host: &mut LuaEnemyHost,
            game: &mut GameState,
            pack: &Pack,
            clips: &[Clip],
        ) -> bool {
            host.update(game, &RoomState::default(), pack, 1, clips)
        }

        // ---------------------------------------------------------------
        // Init and the spawn kinds
        // ---------------------------------------------------------------

        #[test]
        fn yawn_init_builds_the_body_and_the_thirteen_slots() {
            let pack = pack();
            let (mut game, clips) = yawn(0x0D, 2);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            let entity = game.entities[1];
            assert_eq!(entity.state(), 1);
            assert_eq!(entity.health, 0x0BEA);
            assert_eq!(entity.yawn_form(), 1, "an in-room Yawn starts grown");
            assert_eq!(entity.ignore(), 1, "the init locks the selector");
            assert_eq!(entity.action_behavior, 0, "the idle action");
            assert_eq!(entity.sca_radius, 800);
            assert_eq!(entity.sca_half_height, 2000);
            assert_eq!((entity.shadow_half_x, entity.shadow_half_z), (1000, 1000));
            assert_eq!(entity.shadow_tint, 0x0080_8080);
            assert!(game.flags[usize::from(crate::game::BANK_SCENARIO)].bit(0x10));
            assert_eq!(game.enemy_count, 13);
            let segments = crate::enemy::yawn::segment_slots(&game, 1).expect("twelve segments");
            assert_eq!(segments.len(), 12);
            for (index, &slot) in segments.iter().enumerate() {
                assert_eq!(game.entities[slot].yawn_joint, 3 + index as u8);
                assert_eq!(game.entities[slot].behavior_flags, 1);
                assert_eq!(game.entities[slot].health, -1);
            }
            // The chain pose exists and its joints 3-14 are gore-marked.
            assert!(game.entity_anims[1].yawn.is_some());
            for joint in 3..15 {
                assert_eq!(entity.joint_flag(joint) & 8, 8);
            }
            assert_eq!(
                entity.yawn_ground_y(),
                -3493,
                "joint 7's world Y through the saturated hierarchy"
            );
            assert_eq!(
                game.joint_worlds[1].len(),
                15,
                "the host snapshots the chain worlds"
            );
        }

        #[test]
        fn yawn_rematch_health_follows_the_serum_flag() {
            let pack = pack();
            for (serum, expected) in [(false, 0x012C), (true, 0x0190)] {
                let (mut game, clips) = yawn(0x12, 0);
                if serum {
                    game.flags[usize::from(crate::game::BANK_SCENARIO)]
                        .apply(crate::game::SCENARIO_FLAG_SECOND_PLAYTHROUGH, 0);
                }
                let mut host = LuaEnemyHost::new();
                assert!(step(&mut host, &mut game, &pack, &clips));
                assert_eq!(game.entities[1].health, expected);
            }
        }

        #[test]
        fn yawn_scripted_entry_starts_hidden_in_the_ceiling() {
            let pack = pack();
            let (mut game, clips) = yawn(0x0D, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            let entity = game.entities[1];
            assert_eq!(entity.status_flags & 4, 4, "hidden until it crawls in");
            assert_eq!(entity.ignore(), 1);
            assert_eq!(entity.action_behavior, 5);
            assert_eq!(entity.yawn_form(), 0);
        }

        #[test]
        fn yawn_park_flag_enters_state_four() {
            let pack = pack();
            let (mut game, clips) = yawn(0x0D, 0x82);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.entities[1].state(), 4);
            // The parked head ignores the selector until the flag clears.
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.entities[1].state(), 4);
            game.entities[1].behavior_flags = 0x02;
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.entities[1].state(), 1);
        }

        // ---------------------------------------------------------------
        // Selectors and RNG order
        // ---------------------------------------------------------------

        #[test]
        fn yawn_idle_setup_consumes_one_draw() {
            let pack = pack();
            let (mut game, clips) = yawn(0x0D, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            // Park the machine in the idle's setup state.
            game.entities[1].action_behavior = 0;
            game.entities[1].action_state = 0;
            game.rand_state = 9;
            let mut expected = 9u32;
            let roll = crate::game::platform_rand(&mut expected);
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.rand_state, expected, "one idle draw");
            assert_eq!(
                game.entities[1].action_ticks_counter,
                (roll & 0xF) + 0xF - 1
            );
            assert_eq!(game.entities[1].action_state, 1);
        }

        #[test]
        fn yawn_selector_bites_a_close_player_and_locks_the_action() {
            let pack = pack();
            let (mut game, clips) = yawn(0x0D, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            // The player is 1500 units away and in sight: the first clause
            // writes the whole state word.
            game.entities[0].pos = [1500, 0, 0];
            game.entities[1].action_behavior = 0;
            game.entities[1].action_state = 0;
            game.entities[1].set_ignore(0);
            assert!(step(&mut host, &mut game, &pack, &clips));
            let entity = game.entities[1];
            assert_eq!(entity.state(), 1);
            assert_eq!(entity.ignore(), 1, "the action is locked");
            assert_eq!(entity.action_behavior, 2, "the bite was selected");
        }

        #[test]
        fn yawn_nearly_dead_player_forces_the_rear_up() {
            let pack = pack();
            let (mut game, clips) = yawn(0x0D, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            game.entities[0].pos = [100, 0, 0];
            game.entities[0].health = 5;
            game.entities[1].set_yawn_form(0);
            game.entities[1].set_ignore(0);
            game.entities[1].action_behavior = 0;
            assert!(step(&mut host, &mut game, &pack, &clips));
            let entity = game.entities[1];
            assert_eq!(entity.yawn_nearly_dead(), 1);
            assert_eq!(entity.state_word() & 0x00FF0000, 0x00030000);
        }

        // ---------------------------------------------------------------
        // Actions
        // ---------------------------------------------------------------

        #[test]
        fn yawn_bite_connects_and_poisons_the_first_yawn() {
            let pack = pack();
            let (mut game, clips) = yawn(0x0D, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            // Park the bite on its damage frame with the head on the player.
            game.entities[1].action_behavior = 2;
            game.entities[1].action_state = 1;
            game.entities[1].set_yawn_form(0);
            game.entities[1].animation_id = 3;
            game.entities[1].animation_frame_id = 13;
            game.entities[1].timing_control = 0;
            game.entities[1].blend_counter = 0;
            game.entities[0].pos = [1000, 0, 0];
            game.entities[1].pos = [0, 0, 0];
            game.entities[0].health = 100;
            assert!(step(&mut host, &mut game, &pack, &clips));
            let player = game.entities[0];
            assert_eq!(player.health, 90);
            assert_ne!(player.is_being_attacked, 0);
            assert_ne!(player.action_behavior, 0);
            assert_eq!(game.health_status & 0x20, 0x20, "the first Yawn poisons");
            assert!(game.flags[usize::from(crate::game::BANK_SCENARIO2)].bit(0x43));
            assert_eq!(game.entities[1].yawn_bite_dir(), -1);
        }

        #[test]
        fn yawn_rematch_bite_does_not_poison() {
            let pack = pack();
            let (mut game, clips) = yawn(0x12, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            game.entities[1].action_behavior = 2;
            game.entities[1].action_state = 1;
            game.entities[1].set_yawn_form(0);
            game.entities[1].animation_id = 3;
            game.entities[1].animation_frame_id = 13;
            game.entities[1].timing_control = 0;
            game.entities[1].blend_counter = 0;
            game.entities[0].pos = [1000, 0, 0];
            game.entities[1].pos = [0, 0, 0];
            game.entities[0].health = 100;
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.entities[0].health, 90);
            assert_eq!(game.health_status & 0x20, 0, "only the first Yawn poisons");
        }

        #[test]
        fn yawn_swallow_grab_opens_the_player_window() {
            let pack = pack();
            let (mut game, clips) = yawn(0x0D, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            game.entities[1].action_behavior = 4;
            game.entities[1].action_state = 0;
            game.entities[0].pos = [100, 0, 0];
            assert!(step(&mut host, &mut game, &pack, &clips));
            let player = game.entities[0];
            assert_eq!(player.state(), 7, "the swallowed player's own window");
            assert_eq!(player.ignore(), 0x0D);
            assert_eq!(player.action_behavior, 0);
            assert_eq!(player.action_state, 0);
            assert_eq!(game.player_flags & 6, 6);
            assert_eq!(game.entities[1].status_flags & 2, 2);
            assert_eq!(game.entities[1].hit_state, 1);
            assert_eq!(
                game.entities[1].yawn_bite_cool(),
                0x1D,
                "the tick's decrement runs"
            );
        }

        #[test]
        fn yawn_swallow_capture_holds_the_player_in_the_mouth() {
            let pack = pack();
            let (mut game, clips) = yawn(0x0D, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            // Enter the capture state directly, with the head joint 0 at a
            // known point.
            game.entities[1].action_behavior = 4;
            game.entities[1].action_state = 2;
            game.entities[0].pos = [50, 0, 0];
            game.joint_worlds[1][0].t = [1000, 0, 2000];
            assert!(step(&mut host, &mut game, &pack, &clips));
            // The capture transform is the head-relative player transform with
            // the fixed mouth offsets; the hold writes the player position.
            assert_eq!(game.yawn_capture.t, [0x937, 700, 0]);
            assert_ne!(game.entities[0].pos, [50, 0, 0]);
            assert_eq!(game.entities[0].zone_flags & 0x80, 0x80);
            assert_eq!(game.entities[1].yawn_scale_ramp(), 0x1388);
        }

        #[test]
        fn yawn_entry_walks_the_patrol_path() {
            let pack = pack();
            let (mut game, clips) = yawn(0x0D, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            // Start on the first waypoint: the arrival branch advances the
            // index and plays the cue.
            game.entities[1].pos = [4500, 0, 24500];
            game.entities[1].action_state = 1;
            game.entities[1].action_ticks_counter = 0;
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.entities[1].yawn_path_index(), 1);
            assert_eq!(game.entities[1].action_behavior, 6, "hands over to emerge");
        }

        #[test]
        fn yawn_flee_batches_the_segments_off() {
            let pack = pack();
            let (mut game, clips) = yawn(0x0D, 2);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            let segments = crate::enemy::yawn::segment_slots(&game, 1).unwrap();
            // Put the head on flee waypoint 6's threshold: the batch write
            // hides the head and the first three segments.
            game.entities[1].action_behavior = 7;
            game.entities[1].action_state = 1;
            game.entities[1].set_yawn_path_index(6);
            game.entities[1].pos = [2500, 0, 26000];
            game.entities[1].set_yawn_speed(0);
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.entities[1].yawn_path_index(), 7);
            assert_eq!(
                game.entities[1].status_flags & 4,
                0,
                "the head is not in this batch"
            );
            // Waypoint 7's batch is enemy-list 4..7, the segments tracking
            // joints 6..9.
            for slot in &segments[3..=6] {
                assert_eq!(game.entities[*slot].status_flags & 4, 4);
            }
            assert_eq!(game.entities[segments[7]].status_flags & 4, 0);
        }

        #[test]
        fn yawn_death_phases_run_the_dissolve() {
            let pack = pack();
            let (mut game, clips) = yawn(0x0D, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            game.entities[1].set_state(3);
            game.entities[1].health = -1;
            let mut saw_dissolve = false;
            for _ in 0..600 {
                assert!(step(&mut host, &mut game, &pack, &clips));
                if game.entities[1].action_state >= 7 {
                    saw_dissolve = true;
                }
                if game.entities[1].action_state == 9 {
                    break;
                }
            }
            assert!(saw_dissolve, "the death reached the dissolve phases");
            assert_eq!(game.entities[1].action_state, 9);
            assert!(game.entities[1].has_enter_switch_zone & 0x80 != 0);
            assert_eq!(game.entities[1].shadow_tint, 0x00AF_DF9F);
        }

        #[test]
        fn yawn_segment_flag_variants_stay_inert() {
            let pack = pack();
            let (mut game, clips) = yawn(0x0D, 2);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            // Bit 0 set with any other value is neither the head nor a body
            // segment: the original runs neither branch.
            game.entities[1].behavior_flags = 3;
            let before = game.entities[1];
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.entities[1], before);
        }

        #[test]
        fn yawn_reset_between_frames_preserves_the_scene() {
            let pack = pack();
            let scene = |reset: bool| {
                let (mut game, clips) = yawn(0x0D, 0);
                game.entities[0].pos = [3000, 0, 100];
                let mut host = LuaEnemyHost::new();
                for tick in 0..40 {
                    if reset && tick > 0 {
                        host.reset();
                    }
                    if tick % 9 == 4 {
                        game.entities[1].set_state(2);
                        game.entities[1].hit_state = 0x11;
                    }
                    assert!(step(&mut host, &mut game, &pack, &clips));
                    // The segments tick through the same VM.
                    let segments = crate::enemy::yawn::segment_slots(&game, 1).unwrap();
                    for slot in segments {
                        let _ = host.update(&mut game, &RoomState::default(), &pack, slot, &clips);
                    }
                }
                (
                    game.entities[1],
                    game.entities[0],
                    game.rand_state,
                    game.entity_anims[1].yawn.clone(),
                )
            };
            assert_eq!(scene(false), scene(true), "the VM reset changes nothing");
        }
    }

    mod tyrant_tests {
        use crate::enemy::LuaEnemyHost;
        use crate::game::GameState;
        use crate::model::{Clip, ClipFrame, Keyframe, Skeleton};
        use crate::pack::{Pack, PackWriter};
        use crate::state::{RoomId, RoomState};
        use std::sync::Arc;

        fn source(id: u8) -> &'static str {
            let path = format!("enemy/em{id:02x}.lua");
            crate::enemy::ENEMY_SCRIPTS
                .iter()
                .find(|(entry, _)| *entry == path)
                .expect("the checked-in Tyrant script")
                .1
        }

        fn pack() -> Pack {
            let mut writer = PackWriter::new();
            for id in [0x0Cu8, 0x10] {
                let path = format!("enemy/em{id:02x}.lua");
                writer.add(&path, source(id).as_bytes().to_vec()).unwrap();
            }
            Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
        }

        /// A synthetic fifteen-joint claw chain: joint 0 the root, then a
        /// single-path chain.
        fn model() -> (Arc<Skeleton>, Arc<Vec<Keyframe>>, Vec<Clip>) {
            let mut relative = vec![[0i16; 3]; 15];
            relative[0] = [0, -1000, 0];
            for joint in relative.iter_mut().skip(1) {
                *joint = [0, -200, 0];
            }
            let mut children: Vec<Vec<u8>> = vec![Vec::new(); 15];
            for (joint, child) in children.iter_mut().enumerate().take(14) {
                *child = vec![(joint + 1) as u8];
            }
            let skeleton = Skeleton { relative, children };
            let keyframes: Vec<Keyframe> = (0..4)
                .map(|_| Keyframe {
                    offset: [0, -1000, 0],
                    rotations: vec![[0, 0, 0]; 15],
                })
                .collect();
            let clips = (0..32)
                .map(|_| Clip {
                    frames: (0..0x80)
                        .map(|_| ClipFrame {
                            keyframe: 0,
                            timing: 1,
                        })
                        .collect(),
                })
                .collect();
            (Arc::new(skeleton), Arc::new(keyframes), clips)
        }

        /// One active Tyrant in slot 2 with the SCD victim in slot 1 (the
        /// script's enemy-list-head victim).
        fn tyrant(id: u8, behavior_flags: u8) -> (GameState, Vec<Clip>) {
            let mut game = GameState::default();
            game.id = RoomId::parse("40C0").unwrap();
            let entity = &mut game.entities[2];
            entity.id = id;
            entity.set_active(true);
            entity.status_flags = 1;
            entity.behavior_flags = behavior_flags;
            entity.variant = 0x20;
            entity.death_event_id = 5;
            entity.pos = [0, 0, 0];
            entity.saved_pos = Some(entity.pos);
            entity.animation_id = 6;
            let victim = &mut game.entities[1];
            victim.id = 0x21;
            victim.set_active(true);
            victim.status_flags = 1;
            victim.health = 100;
            game.entities[0].pos = [4000, 0, 0];
            game.entities[0].health = 100;
            game.enemy_count = 2;
            let (skeleton, keyframes, clips) = model();
            game.entity_anims[2].skeleton = Some(skeleton);
            game.entity_anims[2].keyframes = Some(keyframes);
            (game, clips)
        }

        fn step(
            host: &mut LuaEnemyHost,
            game: &mut GameState,
            pack: &Pack,
            clips: &[Clip],
        ) -> bool {
            host.update(game, &RoomState::default(), pack, 2, clips)
        }

        // -----------------------------------------------------------------
        // Init
        // -----------------------------------------------------------------

        #[test]
        fn tyrant_lab_init_floats_in_the_pod() {
            let pack = pack();
            let (mut game, clips) = tyrant(0x0C, 0x40);
            game.rand_state = 9;
            let mut expected = 9u32;
            crate::game::platform_rand(&mut expected); // the heart's clone draw
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.rand_state, expected, "one clone draw");
            let entity = game.entities[2];
            assert_eq!(entity.state(), 8, "the lab SCD pod state");
            assert_eq!(entity.action_behavior, 0x0B);
            assert_eq!(entity.health, 0xDC);
            assert_eq!(entity.sca_radius, 800);
            assert_eq!(entity.sca_half_height, 2000);
            assert_eq!(entity.sca_offset, [0, -2000, 0]);
            assert_eq!((entity.shadow_half_x, entity.shadow_half_z), (1000, 1000));
            assert_eq!(entity.shadow_tint, 0x0080_8080);
            assert_eq!(entity.ty_cooldown(), 0xD2);
            assert_eq!(entity.ty_look_mode(), 3);
            assert_eq!(entity.look_at_flags, 2);
            assert_eq!(entity.look_at_joint, 2);
            assert_eq!(entity.look_at_yaw_step, 0xA0);
            assert_eq!(entity.ty_pad_counter(), 1, "the ungated pad counter ticks");
            let heart = entity.tyrant_heart.expect("the heart spawned");
            let companion = game.companions[usize::from(heart)].as_ref().unwrap();
            assert_eq!(
                companion.kind,
                crate::enemy::companion::CompanionKind::Heart
            );
            assert_eq!(companion.wobble, 0x1000);
            assert_eq!(companion.beat, 0x15);
            assert_eq!(companion.local_t, [0x138, -760, -287]);
            // id 0x0c never allocates the ribbon or the ghosts.
            assert!(!game.tyrant.trail.allocated);
        }

        #[test]
        fn tyrant_heliport_init_arms_the_eruption() {
            let pack = pack();
            let (mut game, clips) = tyrant(0x10, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            let entity = game.entities[2];
            assert_eq!(entity.state(), 1);
            assert_eq!(entity.ignore(), 1);
            assert_eq!(entity.action_behavior, 9, "the eruption entrance");
            assert_eq!(entity.health, 600);
            assert!(game.tyrant.trail.allocated);
            assert_eq!(entity.ty_repause(), 0x5A);
        }

        // -----------------------------------------------------------------
        // The claw hit's player window
        // -----------------------------------------------------------------

        #[test]
        fn the_claw_hit_opens_the_tyrant_player_window() {
            let pack = pack();
            let (mut game, clips) = tyrant(0x0C, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips)); // init
            let entity = &mut game.entities[2];
            entity.set_state(1);
            entity.set_ignore(1);
            entity.action_behavior = 3; // swipe
            entity.action_state = 1;
            entity.animation_id = 5;
            entity.animation_frame_id = 5;
            entity.timing_control = 5;
            entity.blend_counter = 0;
            entity.move_speed_current = 200;
            game.entities[0].pos = [0, 0, 0];
            game.entities[0].health = 100;
            let mut worlds = vec![
                crate::anim::Mat4x3 {
                    r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                    t: [0, 0, 0],
                };
                15
            ];
            worlds[8].t = [0, 0, 0];
            game.joint_worlds[2] = worlds;
            assert!(step(&mut host, &mut game, &pack, &clips));
            let player = game.entities[0];
            assert_eq!(player.state(), 6, "the Tyrant state-6 window");
            assert_eq!(player.ignore(), 0x0C, "the 0x0C entry index");
            assert_eq!(player.action_behavior, 1, "the swipe stagger");
            assert_eq!(player.animation_id, 6);
            assert_eq!(game.player_attacker, Some(2));
            assert_eq!(game.entities[2].ty_hit_mask() & 1, 1, "the swing latched");
            // Frame 8 is the damage frame.
            game.entities[2].animation_frame_id = 8;
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.entities[0].health, 90, "10 damage, first playthrough");
        }

        // -----------------------------------------------------------------
        // The two dispatch bases
        // -----------------------------------------------------------------

        #[test]
        fn state_one_dispatches_with_the_plus_two_bias() {
            let pack = pack();
            let (mut game, clips) = tyrant(0x0C, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips)); // init
            // Park state 1 / behaviour 0: the +2 bias selects the pause.
            game.entities[2].set_state(1);
            game.entities[2].set_ignore(1);
            game.entities[2].action_behavior = 0;
            game.entities[2].action_state = 0;
            assert!(step(&mut host, &mut game, &pack, &clips));
            let entity = game.entities[2];
            assert_eq!(entity.action_state, 1, "the pause setup ran");
            assert_eq!(entity.animation_id, 0);
            assert_eq!(entity.action_ticks_counter, 0x3B, "0x3C, decremented");
        }

        #[test]
        fn state_three_dispatches_without_the_bias() {
            let pack = pack();
            let (mut game, clips) = tyrant(0x0C, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips)); // init
            // State 3 / behaviour 0 claims the forced state then runs table[0]
            // (the restrained bound), not table[2] (the pause).
            game.entities[2].set_state(3);
            game.entities[2].set_ignore(0);
            game.entities[2].action_behavior = 0;
            game.entities[2].action_state = 0;
            assert!(step(&mut host, &mut game, &pack, &clips));
            let entity = game.entities[2];
            assert_eq!(entity.health, -1, "the slab Tyrant is unkillable");
            assert_eq!(entity.animation_id, 8);
            assert_eq!(entity.blend_counter, 0x0E, "the setup's 0x0F, consumed");
            assert_eq!(entity.action_state, 1, "no wrap on the setup tick");
        }

        #[test]
        fn the_heliport_forced_state_yields_to_the_scd() {
            let pack = pack();
            let (mut game, clips) = tyrant(0x10, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips));
            game.entities[2].set_state(3);
            game.entities[2].set_ignore(0);
            game.entities[2].action_behavior = 0;
            game.entities[2].action_state = 0;
            assert!(step(&mut host, &mut game, &pack, &clips));
            let entity = game.entities[2];
            assert_eq!(entity.behavior_flags & 0x40, 0x40);
            assert_eq!(entity.state(), 8);
            assert_eq!(entity.action_behavior, 0);
            assert_eq!(entity.action_state, 1);
        }

        // -----------------------------------------------------------------
        // The lab SCD sequence
        // -----------------------------------------------------------------

        #[test]
        fn the_pod_floats_then_lifts_on_the_sysflag() {
            let pack = pack();
            let (mut game, clips) = tyrant(0x0C, 0x40);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips)); // init
            assert!(step(&mut host, &mut game, &pack, &clips)); // pod setup
            let entity = game.entities[2];
            assert_eq!(entity.pos[1], -200);
            assert_eq!(entity.action_state, 1);
            assert_eq!(entity.action_ticks_counter, 4, "5, decremented");
            // The pod activation flag forces the lift; each sub-3 frame rises
            // by 4 and the first over-zero frame raises the SCD flag.
            game.flags[4].apply(0x1F, 0);
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.entities[2].action_state, 3);
            for _ in 0..80 {
                assert!(step(&mut host, &mut game, &pack, &clips));
            }
            assert_eq!(game.entities[2].pos[1], 0);
            // The every-frame release check forces sub 3 again once the flag
            // is set, so the steady state is sub 3 with the height pinned.
            assert_eq!(game.entities[2].action_state, 3);
            assert!(
                game.flags[4].bit(0),
                "the completion flag is raised: y {} sub {} param {}",
                game.entities[2].pos[1],
                game.entities[2].action_state,
                game.entities[2].scd_anim_param
            );
        }

        #[test]
        fn the_scd_impale_writes_the_victim() {
            let pack = pack();
            let (mut game, clips) = tyrant(0x0C, 0x40);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips)); // init
            // Park the scripted impale's setup.
            let entity = &mut game.entities[2];
            entity.set_state(8);
            entity.action_behavior = 12;
            entity.action_state = 0;
            game.entities[0].pos = [500, 0, 0];
            game.entities[1].pos = [600, 0, 0];
            assert!(step(&mut host, &mut game, &pack, &clips));
            let victim = game.entities[1];
            assert_eq!(victim.status_flags & 6, 6);
            assert_eq!(victim.state_word(), 0x00030001);
            assert_eq!(game.entities[2].status_flags & 2, 2);
            assert_eq!(game.entities[2].animation_id, 5);
            assert_eq!(game.entities[2].action_state, 1);
            // The snap copied the Tyrant's grab offsets onto the victim.
            assert_eq!(victim.unk_c6, game.entities[2].unk_c6);
            assert_eq!(victim.unk_c8, game.entities[2].unk_c8);
            // One held frame drags the victim by the Tyrant's root velocity.
            game.entities[2].action_state = 1;
            game.entities[2].move_speed_current = 100;
            game.entities[2].animation_frame_id = 0;
            game.entities[2].timing_control = 5;
            let before = game.entities[1].unk_c6;
            assert!(step(&mut host, &mut game, &pack, &clips));
            let speed_x = game.entities[2].speed[0];
            assert_ne!(speed_x, 0, "the root motion stepped the Tyrant");
            assert_eq!(game.entities[1].unk_c6, before.wrapping_add(speed_x as u16));
            // The completion sub raises SysFlags 0x1E and clears the behaviour.
            game.entities[2].action_state = 2;
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert!(game.flags[4].bit(0x1E));
            assert_eq!(game.entities[2].action_behavior, 0);
            assert_eq!(game.entities[2].action_state, 0);
            assert_eq!(game.entities[2].status_flags & 2, 0);
            // The table falls through to the idle entry, which holds the
            // pose and rearms its sub-state.
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.entities[2].animation_id, 0);
            assert_eq!(game.entities[2].action_state, 1);
            // The impale left a hold running, so the idle's first frame is
            // held and its 0x1F blend counter survives.
            assert_eq!(game.entities[2].blend_counter, 0x1F);
        }

        #[test]
        fn the_live_impale_grabs_the_player() {
            let pack = pack();
            let (mut game, clips) = tyrant(0x0C, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips)); // init
            let entity = &mut game.entities[2];
            entity.set_state(1);
            entity.set_ignore(1);
            entity.action_behavior = 7; // the live impale (table 9 with +2)
            entity.action_state = 0;
            game.entities[0].pos = [200, 0, 0];
            game.entities[0].health = 100;
            assert!(step(&mut host, &mut game, &pack, &clips));
            let entity = game.entities[2];
            assert_eq!(entity.action_state, 1);
            assert_eq!(entity.status_flags & 2, 2, "intangible while holding");
            assert_eq!(entity.hit_state, 1);
            let player = game.entities[0];
            assert_eq!(player.state(), 7, "the held impale window");
            assert_eq!(player.ignore(), 0x0C);
            assert_eq!(player.animation_id, 7);
            assert_eq!(game.player_attacker, Some(2));
            assert_eq!(game.entities[2].ty_hit_mask(), 0);
        }

        // -----------------------------------------------------------------
        // RNG order
        // -----------------------------------------------------------------

        #[test]
        fn the_glass_break_consumes_the_shard_draw_order() {
            let pack = pack();
            let (mut game, clips) = tyrant(0x0C, 0x40);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips)); // init
            let entity = &mut game.entities[2];
            entity.set_state(8);
            entity.action_behavior = 10;
            entity.action_state = 1;
            entity.animation_id = 6;
            entity.animation_frame_id = 0x59;
            entity.timing_control = 5;
            entity.blend_counter = 0;
            game.rand_state = 77;
            let mut expected = 77u32;
            for _ in 0..53 {
                for _ in 0..4 {
                    crate::game::platform_rand(&mut expected);
                }
            }
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.rand_state, expected, "53 shards x 4 draws, in order");
        }

        #[test]
        fn the_eruption_consumes_the_debris_draw_order() {
            let pack = pack();
            let (mut game, clips) = tyrant(0x10, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips)); // init
            assert!(step(&mut host, &mut game, &pack, &clips)); // erupt setup
            let entity = &mut game.entities[2];
            assert_eq!(entity.action_behavior, 9);
            assert_eq!(entity.action_state, 1);
            assert_eq!(entity.action_ticks_counter, 0x3B);
            // One frame inside the debris burst only (t lands on 0x1A after
            // the decrement): 11 x 4 draws, no smoke.
            entity.action_ticks_counter = 0x1B;
            let mut expected = 0xB23u32;
            for _ in 0..(11 * 4) {
                crate::game::platform_rand(&mut expected);
            }
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(game.rand_state, expected);
        }

        #[test]
        fn the_rocket_death_launches_limbs_and_pins_health() {
            let pack = pack();
            let (mut game, clips) = tyrant(0x10, 0x40);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips)); // init
            let entity = &mut game.entities[2];
            entity.set_state(8);
            entity.action_behavior = 13;
            entity.action_state = 0;
            assert!(step(&mut host, &mut game, &pack, &clips));
            let entity = game.entities[2];
            assert_eq!(entity.ty_flags() & 8, 8, "the body is hidden");
            assert_eq!(entity.ty_flags() & 2, 0, "look-at is cleared");
            assert_eq!(entity.action_state, 1);
            assert_eq!(game.rand_state, 1534, "srand(1534) reset the stream");
            for (index, joint) in crate::enemy::tyrant::LIMB_JOINTS.iter().enumerate() {
                assert!(game.tyrant.limbs[index].live, "limb {joint} launched");
                assert_eq!(game.tyrant.limbs[index].bounces, 3, "three bounces each");
            }
            assert_eq!(game.tyrant.limbs[0].vel_y, -500, "the launch velocity");
            // The smoke/limb physics runs and the death pins at health -1.
            game.entities[2].action_state = 2;
            game.entities[2].action_ticks_counter = 0;
            for _ in 0..220 {
                step(&mut host, &mut game, &pack, &clips);
            }
            assert_eq!(game.entities[2].health, -1);
            assert_eq!(game.entities[2].action_state, 3);
        }

        // -----------------------------------------------------------------
        // The ribbon through the script surface
        // -----------------------------------------------------------------

        #[test]
        fn the_ribbon_arm_and_update_run_from_the_tail() {
            let pack = pack();
            let (mut game, clips) = tyrant(0x10, 0);
            let mut host = LuaEnemyHost::new();
            assert!(step(&mut host, &mut game, &pack, &clips)); // init
            // Give the claw a posed world so the arm has an anchor.
            game.joint_worlds[2] = vec![
                crate::anim::Mat4x3 {
                    r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                    t: [0, 0, 0],
                };
                15
            ];
            game.tyrant.trail.timer = 0x8010;
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(
                game.tyrant.trail.timer & 0x8000,
                0,
                "the sweep cleared the arm bit"
            );
            assert_eq!(
                game.tyrant.trail.slots[8].far,
                game.tyrant.trail.slots[8].near
            );
            let before = game.tyrant.trail.slots[7].mat_a;
            assert!(step(&mut host, &mut game, &pack, &clips));
            assert_eq!(
                game.tyrant.trail.slots[8].mat_a, before,
                "the update scrolled the history"
            );
        }

        // -----------------------------------------------------------------
        // Reset equality
        // -----------------------------------------------------------------

        #[test]
        fn tyrant_reset_between_frames_preserves_the_scene() {
            let pack = pack();
            let scene = |reset: bool| {
                let (mut game, clips) = tyrant(0x10, 0);
                game.entities[0].pos = [3000, 0, 100];
                let mut host = LuaEnemyHost::new();
                for tick in 0..40 {
                    if reset && tick > 0 {
                        host.reset();
                    }
                    if tick % 9 == 4 {
                        game.entities[2].set_state(1);
                        game.entities[2].set_ignore(0);
                        game.entities[2].hit_state = 0x11;
                    }
                    assert!(step(&mut host, &mut game, &pack, &clips));
                }
                (
                    game.entities[2],
                    game.entities[0],
                    game.rand_state,
                    game.tyrant.clone(),
                )
            };
            assert_eq!(scene(false), scene(true), "the VM reset changes nothing");
        }
    }
}
