//! Real-asset save/load tests (slice 3).
//!
//! Run with:
//! `ARKLAY_RE1_PACK=re1.akpak cargo test --test save_real -- --ignored --nocapture`
//!
//! Only an unset `ARKLAY_RE1_PACK` skips. The first test drives the room 100
//! typewriter end to end (prompt, ribbon consumption, save screen, reload) for
//! both characters; the second captures the save and load screens twice and
//! requires each pair to be byte-identical and draw content.

use std::path::{Path, PathBuf};

use arklay::bmp;
use arklay::engine;
use arklay::game::STATE_BYTE_SAVES;
use arklay::items;
use arklay::pack::Pack;
use arklay::save;
use arklay::state::{Image, RoomId};

fn non_black(image: &Image) -> usize {
    image
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)
        .count()
}

fn fresh_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("arklay-save-real-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
fn real_typewriter_save_consumes_a_ribbon_and_reloads() {
    let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
        return;
    };
    let pack = Pack::open(Path::new(&path)).unwrap();

    // Chris at the room 100 typewriter: the prompt names the ink ribbon and
    // the save consumes one.
    let dir = fresh_dir("chris");
    let sim = engine::simulate_typewriter(&pack, RoomId::parse("1000").unwrap(), &dir).unwrap();
    assert_eq!(sim.room, RoomId::parse("1000").unwrap());
    assert_eq!(sim.slot, 0);
    assert!(
        sim.ink_ribbon,
        "Chris's typewriter save must spend a ribbon"
    );
    assert_eq!(sim.saved.used_item, items::ITEM_INK_RIBBONS);
    assert_eq!(sim.saved.character, 0);
    assert_eq!(sim.saved.stage, 1);
    assert_eq!(sim.saved.room, 0);
    assert_eq!(
        sim.saved.total_held, 2,
        "the block holds the knife and the remaining ribbon stack"
    );
    assert_eq!(
        sim.state_after_save.item_count(items::ITEM_INK_RIBBONS),
        2,
        "three ribbons were given and one was consumed"
    );
    assert_eq!(
        sim.state_after_load.item_count(items::ITEM_INK_RIBBONS),
        2,
        "the consumed ribbon stayed consumed through the reload"
    );
    assert_eq!(
        sim.state_after_save.state_bytes[usize::from(STATE_BYTE_SAVES)],
        1,
        "the live counter advanced"
    );
    assert_eq!(
        sim.state_after_load.state_bytes[usize::from(STATE_BYTE_SAVES)],
        0,
        "the block stores the pre-increment counter, as the original does"
    );

    // The reloaded gameplay state matches the state that was saved.
    assert_eq!(sim.state_after_save.id, sim.state_after_load.id);
    assert_eq!(
        sim.state_after_save.inventory,
        sim.state_after_load.inventory
    );
    assert_eq!(sim.state_after_save.item_box, sim.state_after_load.item_box);
    assert_eq!(
        sim.state_after_save.entities[0].pos,
        sim.state_after_load.entities[0].pos
    );
    assert_eq!(
        sim.state_after_save.entities[0].angle,
        sim.state_after_load.entities[0].angle
    );
    assert_eq!(
        sim.state_after_save.entities[0].health,
        sim.state_after_load.entities[0].health
    );
    for bank in [0usize, 1, 2, 3, 7, 8] {
        assert_eq!(
            sim.state_after_save.flags[bank].bytes(),
            sim.state_after_load.flags[bank].bytes(),
            "persisted flag bank {bank} differs"
        );
    }

    // Jill's first playthrough saves for free with the progress prompt.
    let dir = fresh_dir("jill");
    let sim = engine::simulate_typewriter(&pack, RoomId::parse("1001").unwrap(), &dir).unwrap();
    assert!(!sim.ink_ribbon, "Jill's first playthrough saves for free");
    assert_eq!(sim.saved.used_item, 0);
    assert_eq!(sim.saved.character, 1);
    assert_eq!(sim.state_after_save.item_count(items::ITEM_INK_RIBBONS), 3);
    assert_eq!(sim.state_after_load.item_count(items::ITEM_INK_RIBBONS), 3);
}

#[test]
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
fn real_save_and_load_captures_are_deterministic_and_draw_the_screen() {
    let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
        return;
    };
    let dir = fresh_dir("capture");
    let saves = dir.join("saves");
    std::fs::create_dir_all(&saves).unwrap();

    // Seed two slots so the screen draws names, counts and locations.
    let mut file = save::SaveFile {
        stage: 1,
        room: 0,
        character: 0,
        saves: 3,
        ..save::SaveFile::default()
    };
    save::save(&saves, 0, &file).unwrap();
    file.character = 1;
    file.room = 6;
    file.saves = 12;
    save::save(&saves, 2, &file).unwrap();

    for screen in ["save", "load"] {
        let first = dir.join(format!("{screen}_a.bmp"));
        let second = dir.join(format!("{screen}_b.bmp"));
        engine::run_ui_with_options(Path::new(&path), screen, Some(&first), &saves, 0).unwrap();
        engine::run_ui_with_options(Path::new(&path), screen, Some(&second), &saves, 0).unwrap();

        let first_bytes = std::fs::read(&first).unwrap();
        let second_bytes = std::fs::read(&second).unwrap();
        assert_eq!(first_bytes, second_bytes, "two {screen} captures differ");
        let image = bmp::decode(&first_bytes).unwrap();
        assert_eq!((image.width, image.height), (320, 240));
        assert!(
            non_black(&image) > 5000,
            "the {screen} capture is mostly black: {} pixels",
            non_black(&image)
        );
        println!("wrote {}", first.display());
    }
}
