//! The game-rule calls of the pause menu: the player's health, the
//! equipped item, and the USE/EQUIP/MOVE actions that change the
//! inventory. The rules stay in [`GameState`]; the menu script decides
//! when to run them. The changing calls need the game state the engine
//! lends with [`super::LuaScreen::update_with_game`] and friends.

use mlua::{IntoLuaMulti, UserDataMethods};

use crate::game::{CombineResult, GameState, STATE_BYTE_CHARACTER, UseResult, platform_rand};
use crate::items::{self, UseCategory};

use super::Api;

pub(super) fn add_methods<'a, M: UserDataMethods<Api<'a>>>(methods: &mut M) {
    // ---- queries -------------------------------------------------------
    // `player_health()`: the player's health, maximum health and status
    // byte (the poison flags are `0x22`); `0, 0, 0` outside a session.
    methods.add_method("player_health", |_lua, api, ()| {
        Ok(api.game().map_or((0, 0, 0), |game| {
            (
                i64::from(game.entities[0].health),
                i64::from(game.max_health),
                i64::from(game.health_status),
            )
        }))
    });

    // `character()`: the character byte's low two bits (`0` Chris, `1`
    // Jill, `2`/`3` the alternates), which pick the portrait.
    methods.add_method("character", |_lua, api, ()| {
        Ok(api.game().map_or(0, |game| {
            game.state_bytes[usize::from(STATE_BYTE_CHARACTER)] & 3
        }))
    });

    // `has_radio()`: whether the radio tab is live.
    methods.add_method("has_radio", |_lua, api, ()| {
        Ok(api.game().is_some_and(GameState::has_radio))
    });

    // `equipped()`: the equipped item, `0` for none.
    methods.add_method("equipped", |_lua, api, ()| {
        Ok(api.game().and_then(|game| game.equipped).unwrap_or(0))
    });

    // `item_count(item)`: how many `item` the inventory holds in total.
    methods.add_method("item_count", |_lua, api, item: u8| {
        Ok(api.game().map_or(0, |game| game.item_count(item)))
    });

    // ---- changes -------------------------------------------------------
    // `rand()`: the next draw of the game's shared `rand()` stream (the
    // one the room scripts use), `0..=0x7FFF`.
    methods.add_method_mut("rand", |_lua, api, ()| {
        let game = api.game_mut("rand")?;
        Ok(platform_rand(&mut game.rand_state))
    });

    // `select_item(item)`: mirror the item under the cursor into the state
    // byte the scripts test; `0` (or nil) for none.
    methods.add_method_mut("select_item", |_lua, api, item: Option<u8>| {
        let game = api.game_mut("select_item")?;
        game.select_item(item.filter(|&item| item != 0));
        Ok(())
    });

    // `equip(item)`: equip `item`, or unequip with `0` (or nil).
    methods.add_method_mut("equip", |_lua, api, item: Option<u8>| {
        let game = api.game_mut("equip")?;
        game.set_equipped(item.filter(|&item| item != 0));
        Ok(())
    });

    // `use_item(slot)`: USE the item in inventory slot `slot`. On success
    // the item is recorded as used and one is consumed, and it returns
    // `true, healed, cured, closes` (`closes`: a key, special, chemical or
    // book use that exits the whole menu). A refusal returns `false,
    // category` (the use-category index the refusal message adds to
    // `0xF7`); an empty slot returns nothing.
    methods.add_method_mut("use_item", |lua, api, slot: usize| {
        let game = api.game_mut("use_item")?;
        let item = game.inventory.get(slot).map_or(0, |stack| stack.id);
        if item == 0 {
            return Ok(mlua::MultiValue::new());
        }
        let values = match game.use_item(item) {
            UseResult::Used { healed, cured } => {
                game.record_used_item(item);
                consume_slot(game, slot);
                (true, healed, cured, closes_inventory(item)).into_lua_multi(lua)?
            }
            UseResult::Unusable => (false, category_index(item)).into_lua_multi(lua)?,
        };
        Ok(values)
    });

    // `combine(slot, target)`: MOVE the item in `slot` onto `target`.
    // Returns `"applied", herb`, `"drug_store"` (chemicals need the drug
    // store), `"herb_no_recipe"` or `"none"` (no recipe, or `target` past
    // the held items).
    methods.add_method_mut("combine", |_lua, api, (slot, target): (usize, usize)| {
        let game = api.game_mut("combine")?;
        if target >= game.inventory.len() {
            return Ok(("none", false));
        }
        Ok(match game.combine_slots(slot, target) {
            CombineResult::Applied { herb, .. } => ("applied", herb),
            CombineResult::NeedsDrugStore => ("drug_store", false),
            CombineResult::HerbNoRecipe => ("herb_no_recipe", false),
            CombineResult::NoRecipe => ("none", false),
        })
    });

    // `show_message(id)`: open the game's message window on global message
    // `id`; refused while one is already up.
    methods.add_method_mut("show_message", |_lua, api, id: u8| {
        let game = api.game_mut("show_message")?;
        game.show_message(id, 0);
        Ok(())
    });
}

/// Whether a successful use of `item` closes the inventory afterwards: the
/// key, special, chemical and book dispatch slots set the original's
/// `DAT_00ae9f13` bit 0x80, which the main menu's case-1 test turns into a
/// full menu exit. (Books only reach here as the red book, the only usable
/// one.)
fn closes_inventory(item: u8) -> bool {
    matches!(
        items::use_category(item),
        UseCategory::Chemicals | UseCategory::Special | UseCategory::Keys | UseCategory::Books
    )
}

/// The original use-category dispatch index (weapons through unusable).
fn category_index(item: u8) -> u8 {
    match items::use_category(item) {
        UseCategory::Weapons => 0,
        UseCategory::Ammo => 1,
        UseCategory::Bottle => 2,
        UseCategory::Chemicals => 3,
        UseCategory::Special => 4,
        UseCategory::Keys => 5,
        UseCategory::Books => 6,
        UseCategory::Heal => 7,
        UseCategory::Always => 8,
        UseCategory::Unusable => 9,
    }
}

/// Remove one item from `slot`, compacting the inventory.
fn consume_slot(game: &mut GameState, slot: usize) {
    if let Some(stack) = game.inventory.get_mut(slot) {
        if stack.quantity > 0 {
            stack.quantity -= 1;
        }
        if stack.quantity == 0 {
            stack.id = 0;
        }
    }
    game.rebuild_slots();
    if game.equipped.is_some_and(|item| !game.has_item(item)) {
        game.set_equipped(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closes_inventory_follows_the_use_dispatch() {
        // Key, special, chemical and book slots set the close bit; heal,
        // ammo, weapons and always-usable items do not.
        assert!(closes_inventory(0x33)); // sword key
        assert!(closes_inventory(0x1D)); // crank (special)
        assert!(closes_inventory(0x15)); // UMB No.2 (chemical)
        assert!(closes_inventory(0x3E)); // red book
        assert!(!closes_inventory(0x44)); // green herb
        assert!(!closes_inventory(0x02)); // beretta
        assert!(!closes_inventory(0x0B)); // handgun ammo
    }
}
