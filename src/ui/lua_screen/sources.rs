//! The script sources one screen VM can load, and its sandboxed `require`.
//!
//! A screen's sources are collected when it opens: every `ui/**.lua` entry the
//! pack (with its mod layers) carries, the checked-in [`UI_SCRIPTS`] for any
//! `ui/` entry the pack lacks (a pack converted before the screens moved to
//! Lua), and the screen's own entry wherever it lives. `require` then
//! resolves against that set with the same path rules as the enemy runtime
//! ([`crate::lua::resolve_module`]); every module runs once per VM and its
//! value is cached.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use anyhow::{Context, Result};
use mlua::{Function, Lua, Table, Value};

use crate::budget;
use crate::pack::Pack;
use crate::ui::UI_SCRIPTS;

/// The `ui/` prefix every shared screen script lives under.
const UI_PREFIX: &str = "ui/";

/// The Lua sources a screen VM may load, keyed by lowercased pack entry.
pub(super) struct Sources {
    map: HashMap<String, Vec<u8>>,
}

impl Sources {
    /// Collect the screen sources for a screen whose script is `entry`.
    pub(super) fn collect(pack: &Pack, entry: &str) -> Result<Self> {
        let mut map: HashMap<String, Vec<u8>> = HashMap::new();
        let add = |map: &mut HashMap<String, Vec<u8>>, name: &str, bytes: Vec<u8>| -> Result<()> {
            budget::check_len(
                bytes.len(),
                budget::MAX_LUA_CHUNK,
                &format!("lua chunk {name} size"),
            )?;
            budget::check_len(map.len() + 1, budget::MAX_LUA_CHUNKS, "screen script count")?;
            map.insert(name.to_ascii_lowercase(), bytes);
            Ok(())
        };
        let mut paths: Vec<&str> = pack
            .paths()
            .filter(|path| {
                let lower = path.to_ascii_lowercase();
                lower.starts_with(UI_PREFIX) && lower.ends_with(".lua")
            })
            .collect();
        paths.sort_unstable();
        paths.dedup();
        for path in paths {
            let bytes = pack
                .read(path)
                .with_context(|| format!("cannot read screen script {path}"))?;
            add(&mut map, path, bytes.into_owned())?;
        }
        for (name, source) in UI_SCRIPTS {
            if !map.contains_key(&name.to_ascii_lowercase()) {
                add(&mut map, name, source.as_bytes().to_vec())?;
            }
        }
        let key = entry.to_ascii_lowercase();
        if !map.contains_key(&key) {
            let bytes = pack
                .read(entry)
                .with_context(|| format!("cannot read screen script {entry}"))?;
            add(&mut map, entry, bytes.into_owned())?;
        }
        Ok(Self { map })
    }

    /// Whether `entry` (lowercased) is a known source.
    fn contains(&self, entry: &str) -> bool {
        self.map.contains_key(entry)
    }

    /// The source of `entry`, matched case-insensitively.
    pub(super) fn get(&self, entry: &str) -> Option<&[u8]> {
        self.map.get(&entry.to_ascii_lowercase()).map(Vec::as_slice)
    }
}

/// The module loader shared by every environment of one VM.
#[derive(Clone)]
pub(super) struct Loader {
    sources: Rc<Sources>,
    /// Resolved modules by entry; a module returning nothing caches `true`.
    modules: Rc<RefCell<HashMap<String, Value>>>,
    /// Modules being loaded right now, for cycle detection.
    loading: Rc<RefCell<HashSet<String>>>,
}

impl Loader {
    pub(super) fn new(sources: Sources) -> Self {
        Self {
            sources: Rc::new(sources),
            modules: Rc::default(),
            loading: Rc::default(),
        }
    }

    /// The source of `entry`.
    pub(super) fn source(&self, entry: &str) -> Option<&[u8]> {
        self.sources.get(entry)
    }

    /// A chunk environment for `entry`: a fresh table falling back to the
    /// VM's globals, carrying a `require` bound to the entry's directory.
    pub(super) fn env(&self, lua: &Lua, entry: &str) -> mlua::Result<Table> {
        let env = lua.create_table()?;
        let meta = lua.create_table()?;
        meta.set("__index", lua.globals())?;
        env.set_metatable(Some(meta));
        env.set("require", self.require(lua, crate::lua::dir_of(entry))?)?;
        Ok(env)
    }

    /// A `require` resolving relative to `base_dir`.
    fn require(&self, lua: &Lua, base_dir: String) -> mlua::Result<Function> {
        let loader = self.clone();
        lua.create_function(move |lua, name: String| loader.load(lua, &base_dir, &name))
    }

    /// The `require(name)` implementation.
    fn load(&self, lua: &Lua, base_dir: &str, name: &str) -> mlua::Result<Value> {
        let entry =
            crate::lua::resolve_module(|entry| self.sources.contains(entry), base_dir, name)
                .ok_or_else(|| mlua::Error::RuntimeError(format!("module not found: {name}")))?;
        if let Some(value) = self.modules.borrow().get(&entry) {
            return Ok(value.clone());
        }
        if !self.loading.borrow_mut().insert(entry.clone()) {
            return Err(mlua::Error::RuntimeError(format!("loop requiring {entry}")));
        }
        let result = self.run(lua, &entry);
        self.loading.borrow_mut().remove(&entry);
        let value = result?;
        self.modules.borrow_mut().insert(entry, value.clone());
        Ok(value)
    }

    /// Execute one module, returning its value (or `true` when it returns
    /// nothing).
    fn run(&self, lua: &Lua, entry: &str) -> mlua::Result<Value> {
        let source = self
            .sources
            .get(entry)
            .ok_or_else(|| mlua::Error::RuntimeError(format!("module not found: {entry}")))?;
        let env = self.env(lua, entry)?;
        let value: Value = lua
            .load(source)
            .set_name(entry.to_string())
            .set_environment(env)
            .eval()?;
        Ok(if value.is_nil() {
            Value::Boolean(true)
        } else {
            value
        })
    }
}
