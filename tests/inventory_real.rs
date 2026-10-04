//! Real-asset inventory/status screen tests.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=assets/re1 ARKLAY_RE1_PACK=target/tmp-test/game.akpak cargo test --test inventory_real -- --ignored --nocapture`
//!
//! Both `ARKLAY_RE1_ROOT` and `ARKLAY_RE1_PACK` must be set together; with
//! neither the tests skip, and with only one they fail. The pack must contain
//! the slice-5 assets (`ui/status.tim`, `ui/blue.tim`, `ui/statface.tim`,
//! `ui/staitem.tim`, `item/item_all.bmp`, `font/font.tim`); a stale pack fails
//! the test rather than silently skipping.
//!
//! The first test freezes ROOM1001's first camera cut and renders the menu
//! over it twice, asserting the two frames are byte-identical and that the
//! screen repainted a meaningful area. The second drives the menu API through
//! a herb heal and a two-herb combine.

mod common;

use arklay::bmp;
use arklay::game::GameState;
use arklay::pack::Pack;
use arklay::rdt;
use arklay::render::Framebuffer;
use arklay::state::{Image, RoomId};
use arklay::text::Text;
use arklay::ui::main_menu::{MainMenu, MenuAssets, MenuEvent, MenuInput};

fn pack() -> Option<Pack> {
    let (_, path) = common::asset_env()?;
    Some(Pack::open(&path).unwrap())
}

fn jill_game(pack: &Pack) -> (RoomId, GameState) {
    let id = RoomId::parse("1001").unwrap();
    let room = rdt::parse(pack.read(&id.rdt_entry()).unwrap(), id).unwrap();
    let mut game = GameState::new(id, &room);
    game.max_health = 96;
    game.entities[0].health = 96;
    (id, game)
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn menu_over_room1001_is_deterministic_and_draws_the_ship_art() {
    let Some(pack) = pack() else {
        return;
    };
    let (id, mut game) = jill_game(&pack);
    let background = bmp::decode(pack.read(&id.cut_entry(0)).unwrap()).unwrap();
    let assets = MenuAssets::load(&pack).expect("the pack has the slice-5 UI assets");
    let text = Text::load(&pack);

    game.entities[0].health = 20;
    game.set_health_status(0x20);
    game.add_item(0x02, 15); // Beretta
    game.add_item(0x44, 1); // green herb
    game.set_equipped(Some(0x02));

    let mut menu = MainMenu::new(game.inventory_capacity());
    menu.open(&mut game);
    // Put the cursor on the empty second slot so the item icon is unobscured.
    menu.handle_input(&mut game, MenuInput::Right);
    // Let the EKG sweep in far enough to show the low-health red trace.
    for _ in 0..40 {
        menu.tick(&mut game);
    }

    let mut first = Framebuffer::new();
    first.blit(&background);
    menu.draw(&mut first, &assets, &text, &game);
    let mut second = Framebuffer::new();
    second.blit(&background);
    menu.draw(&mut second, &assets, &text, &game);
    assert_eq!(first.rgba, second.rgba, "two menu renders differ");

    let changed = first
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(background.rgba.as_chunks::<4>().0)
        .filter(|(a, b)| a != b)
        .count();
    println!("pixels repainted by the menu: {changed}");
    assert!(changed > 5000, "the menu only repainted {changed} pixels");

    // The first slot's green-herb icon must sample item_all.bmp row 64.
    let mut icon_matches = 0;
    for y in 0..30 {
        for x in 0..40 {
            let source = ((1920 + y) * assets.item_all.width as usize + x) * 4;
            let target = ((86 + y) * 320 + (220 + x)) * 4;
            if assets.item_all.rgba[source + 3] == 0 {
                continue;
            }
            if first.rgba[target..target + 4] == assets.item_all.rgba[source..source + 4] {
                icon_matches += 1;
            }
        }
    }
    assert!(
        icon_matches > 50,
        "the green herb icon only matched {icon_matches} atlas pixels"
    );

    // Health 20 of 96 is the red state: the EKG trace inside the monitor
    // window must contain red pixels after the sweep.
    let mut red = 0;
    for y in 150..185 {
        for x in 84..132 {
            let offset = (y * 320 + x) * 4;
            let [r, g, b, _] = first.rgba[offset..offset + 4].try_into().unwrap();
            if r > 150 && g < 100 && b < 100 {
                red += 1;
            }
        }
    }
    assert!(red > 10, "the EKG trace only has {red} red pixels");

    let capture = std::env::temp_dir().join("inventory_room1001_menu.bmp");
    std::fs::create_dir_all(capture.parent().unwrap()).expect("create temp dir");
    bmp::encode(
        &Image {
            width: first.width,
            height: first.height,
            rgba: first.rgba,
        },
        &capture,
    )
    .expect("write the capture");
    println!("wrote {}", capture.display());
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn a_herb_heal_and_a_two_herb_combine_through_the_menu() {
    let Some(pack) = pack() else {
        return;
    };
    let (_, mut game) = jill_game(&pack);
    game.entities[0].health = 20;
    game.add_item(0x44, 1); // green herb
    game.add_item(0x43, 1); // red herb

    let mut menu = MainMenu::new(game.inventory_capacity());
    menu.open(&mut game);

    // USE the green herb: one third of 96 health, and it must be consumed.
    menu.handle_input(&mut game, MenuInput::Confirm);
    assert_eq!(
        menu.handle_input(&mut game, MenuInput::Confirm),
        MenuEvent::Changed
    );
    assert_eq!(game.entities[0].health, 20 + 32);
    assert_eq!(game.inventory.len(), 1);
    assert_eq!(game.inventory[0].id, 0x43);
    assert_eq!(game.last_used_item, Some(0x44));

    // Give the red herb a green partner and combine them.
    game.add_item(0x44, 1);
    menu.open(&mut game);
    // Slot 0 is now the red herb, slot 1 the green one.
    assert_eq!(game.inventory[0].id, 0x43);
    assert_eq!(game.inventory[1].id, 0x44);
    menu.handle_input(&mut game, MenuInput::Confirm);
    menu.handle_input(&mut game, MenuInput::Down);
    menu.handle_input(&mut game, MenuInput::Down);
    assert_eq!(
        menu.handle_input(&mut game, MenuInput::Confirm),
        MenuEvent::None
    );
    menu.handle_input(&mut game, MenuInput::Right);
    assert_eq!(
        menu.handle_input(&mut game, MenuInput::Confirm),
        MenuEvent::Message(arklay::ui::main_menu::MESSAGE_HERB_MIX)
    );
    assert_eq!(game.inventory.len(), 1);
    assert_eq!(game.inventory[0].id, 0x46, "red+green mixed herb");
}
