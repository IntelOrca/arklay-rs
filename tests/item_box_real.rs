//! Real-asset item box tests.
//!
//! Run with:
//! `ARKLAY_RE1_PACK=re1.akpak cargo test --test item_box_real -- --ignored --nocapture`
//!
//! The pack must carry the slice-5/6 art (`ui/itemboxn.tim`, the menu sheets,
//! `item/item_all.bmp`, `font/font.tim`) and `data/bio_card.dat`; a stale pack
//! fails rather than silently skipping. Only an unset `ARKLAY_RE1_PACK`
//! skips.
//!
//! The first test deposits a stack in room 1001, changes rooms, withdraws it
//! again and round-trips the whole state through a save block. The second
//! renders the `--ui box` capture twice and requires it to be deterministic
//! and non-empty.

use std::path::Path;

use arklay::bmp;
use arklay::engine;
use arklay::game::{GameState, InventoryItem};
use arklay::pack::Pack;
use arklay::rdt;
use arklay::save::{SAVE_PREFIX_ENTRY, SaveFile};
use arklay::state::RoomId;

const SPRAY: u8 = 0x41;
const GREEN_HERB: u8 = 0x44;

fn pack() -> Option<Pack> {
    let path = std::env::var("ARKLAY_RE1_PACK").ok()?;
    Some(Pack::open(Path::new(&path)).unwrap())
}

fn room_game(pack: &Pack, id: RoomId) -> GameState {
    let room = rdt::parse(pack.read(&id.rdt_entry()).unwrap(), id).unwrap();
    GameState::new(id, &room)
}

fn non_black(image: &arklay::state::Image) -> usize {
    image
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)
        .count()
}

#[test]
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
fn real_item_box_deposit_survives_room_changes_and_a_save_round_trip() {
    let Some(pack) = pack() else {
        return;
    };
    let id = RoomId::parse("1001").unwrap();
    let mut game = room_game(&pack, id);
    game.add_item(SPRAY, 1);
    game.set_equipped(Some(SPRAY));
    assert_eq!(game.inventory[0].id, SPRAY);

    // Deposit the spray into box slot 10.
    assert!(game.item_box_swap(10, 0));
    assert_eq!(
        game.item_box[10],
        InventoryItem {
            id: SPRAY,
            quantity: 1
        }
    );
    assert!(game.inventory.is_empty());
    assert_eq!(
        game.equipped, None,
        "the spray was equipped before depositing"
    );

    // The box survives a room change in full. A real door keeps the player's
    // variant, so the destination carries the current player flag.
    let destination = RoomId::parse("1140").unwrap();
    let other = RoomId {
        player_flag: id.player_flag,
        ..destination
    };
    let other_room = rdt::parse(pack.read(&other.rdt_entry()).unwrap(), other).unwrap();
    game.enter_room(other, &other_room);
    assert_eq!(game.item_box[10].id, SPRAY);

    // Withdraw it inside the new room: the herb in slot 0 moves to the box.
    game.add_item(GREEN_HERB, 1);
    assert!(game.item_box_swap(10, 0));
    assert_eq!(game.item_box[10].id, GREEN_HERB);
    assert!(game.has_item(SPRAY));
    assert!(!game.has_item(GREEN_HERB));

    // The whole block round-trips through the save format.
    let prefix = pack
        .read(SAVE_PREFIX_ENTRY)
        .expect("bio_card.dat in the pack");
    let file = SaveFile::from_state_with_prefix(&game, prefix).unwrap();
    let parsed = SaveFile::from_bytes(&file.to_bytes()).unwrap();
    let mut restored = GameState::default();
    parsed.apply_to(&mut restored);
    assert_eq!(restored.item_box, game.item_box);
    assert_eq!(restored.inventory, game.inventory);
    assert_eq!(restored.id, game.id);

    // The room-flag save block carries the file bits too: 0x82 + 3 = 0x85 is
    // the room-flags word at byte 16, MSB-first bit 5, i.e. byte 19 bit 2.
    game.set_file_collected(3, true);
    let file = SaveFile::from_state_with_prefix(&game, prefix).unwrap();
    assert!(file.room_flags[19] & 0x04 != 0, "0x85 lands in byte 19");
    let restored = SaveFile::from_bytes(&file.to_bytes()).unwrap();
    let mut state = GameState::default();
    restored.apply_to(&mut state);
    assert!(state.file_collected(3));
}

#[test]
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
fn real_item_box_capture_is_deterministic_and_draws_the_frame() {
    let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("arklay-itembox-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let saves = dir.join("saves");
    let first = dir.join("box_a.bmp");
    let second = dir.join("box_b.bmp");

    engine::run_ui_with_options(Path::new(&path), "box", Some(&first), &saves, 0).unwrap();
    engine::run_ui_with_options(Path::new(&path), "box", Some(&second), &saves, 0).unwrap();

    let first_bytes = std::fs::read(&first).unwrap();
    let second_bytes = std::fs::read(&second).unwrap();
    assert_eq!(first_bytes, second_bytes, "two item-box captures differ");
    let image = bmp::decode(&first_bytes).unwrap();
    assert_eq!((image.width, image.height), (320, 240));
    assert!(
        non_black(&image) > 5000,
        "the item-box capture is mostly black: {} pixels",
        non_black(&image)
    );
    println!("wrote {}", first.display());
}
