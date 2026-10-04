//! M6 wiring: message/menu integration against the real converted pack.
//!
//! These tests need `ARKLAY_RE1_PACK`; without it they return early. They run
//! the same public seams the engine's `--ui menu` capture, the door
//! transition path and the locked-door message path use.

use std::path::Path;

use arklay::bmp;
use arklay::engine;
use arklay::font::Font;
use arklay::game::{GameState, ScdGameHost};
use arklay::message::MessageInput;
use arklay::pack::Pack;
use arklay::render::Framebuffer;
use arklay::state::{Image, RoomId};
use arklay::text::Text;
use arklay::tim;

fn non_black(image: &Image) -> usize {
    image
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)
        .count()
}

fn assert_capture_has_content(bytes: &[u8]) {
    let image = bmp::decode(bytes).unwrap();
    assert_eq!((image.width, image.height), (320, 240));
    assert!(
        non_black(&image) > 5000,
        "the capture is mostly black: {} pixels",
        non_black(&image)
    );
}

#[test]
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
fn real_menu_capture_over_room_1001_is_deterministic() {
    let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("arklay-m6-menu-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let saves = dir.join("saves");
    let first = dir.join("menu_a.bmp");
    let second = dir.join("menu_b.bmp");

    engine::run_ui_with_options(Path::new(&path), "menu", Some(&first), &saves, 0).unwrap();
    engine::run_ui_with_options(Path::new(&path), "menu", Some(&second), &saves, 0).unwrap();

    let first_bytes = std::fs::read(&first).unwrap();
    let second_bytes = std::fs::read(&second).unwrap();
    assert_eq!(first_bytes, second_bytes, "two menu captures differ");
    assert_capture_has_content(&first_bytes);
}

#[test]
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
fn real_save_room_door_walk_runs_the_destination_message_path() {
    let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
        return;
    };
    let pack = Pack::open(Path::new(&path)).unwrap();
    let source = RoomId::parse("1001").unwrap();
    let sim = engine::simulate_door(&pack, source, 0, None).unwrap();
    assert_eq!(sim.target, RoomId::parse("1011").unwrap());
    assert_eq!(sim.game.id, sim.target);

    // The destination's runtime resolves messages through its own room and
    // the pack's text tables. Any door-animation request left over from the
    // transition resolves; none may leave the window stuck without a source.
    let text = Text::load(&pack);
    let mut game = sim.game.clone();
    for _ in 0..600 {
        game.update_message(MessageInput::default(), &sim.room, &text);
        if !game.message.active || game.message.has_source() {
            break;
        }
    }
    assert!(
        !game.message.active || game.message.has_source(),
        "the destination message path left an unresolved window"
    );

    // A global message requested in the destination room resolves and draws
    // over the frozen gameplay frame the transition left behind.
    let font = Font::new(
        tim::decode_4bpp(pack.read("font/font.tim").unwrap()).expect("font/font.tim decodes"),
    );
    game.cancel_message();
    game.show_message(0x40, 0x145);
    for _ in 0..600 {
        game.update_message(MessageInput::default(), &sim.room, &text);
        if game.message.has_source() {
            break;
        }
    }
    assert!(game.message.active);
    assert!(!game.message.source().is_empty(), "the stream is not empty");

    let mut framebuffer = Framebuffer::new();
    framebuffer.blit(&sim.gameplay_frame);
    let before = framebuffer.rgba.clone();
    game.message.draw(&mut framebuffer, &font, &text);
    let changed = before
        .iter()
        .zip(&framebuffer.rgba)
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        changed > 100,
        "the message drew only {changed} pixels over the frozen frame"
    );
}

#[test]
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
fn real_locked_door_message_renders_over_a_frozen_frame() {
    let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
        return;
    };
    let pack = Pack::open(Path::new(&path)).unwrap();
    let id = RoomId::parse("1010").unwrap();

    // Probe the sword-key door with no key: the door handler requests its
    // locked message through the same window path the engine drives.
    let data = pack.read(&id.rdt_entry()).unwrap();
    let room = arklay::rdt::parse(data, id).unwrap();
    let scripts = arklay::scd::reader::parse(data).unwrap();
    let mut state = GameState::new(id, &room);
    {
        let mut host = ScdGameHost::new(&mut state);
        let mut vm = arklay::scd::vm::CommandVm::new(&scripts);
        vm.run_init(&mut host);
    }
    let door = state.doors[1].expect("ROOM1010's key door");
    let center_x = i32::from(door.zone[0]) + i32::from(door.zone[2]) / 2;
    let center_z = i32::from(door.zone[1]) + i32::from(door.zone[3]) / 2;
    let (dx, dz) = arklay::player::reach_offset(0);
    state.interact([center_x - dx, 0, center_z - dz], 0, true);
    assert_eq!(state.message.id, Some(201), "the locked message requested");

    let text = Text::load(&pack);
    for _ in 0..600 {
        state.update_message(MessageInput::default(), &room, &text);
        if state.message.has_source() {
            break;
        }
    }
    assert!(state.message.has_source(), "the locked message resolved");
    assert!(state.message.active);

    // Freeze the room as one rendered frame and paint the window over it.
    let dir = std::env::temp_dir().join(format!("arklay-m6-door-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let capture = dir.join("room1010.bmp");
    engine::run(Path::new(&path), id, Some(&capture)).unwrap();
    let frozen = bmp::decode(&std::fs::read(&capture).unwrap()).unwrap();

    let font = Font::new(
        tim::decode_4bpp(pack.read("font/font.tim").unwrap()).expect("font/font.tim decodes"),
    );
    let mut framebuffer = Framebuffer::new();
    framebuffer.blit(&frozen);
    let before = framebuffer.rgba.clone();
    state.message.draw(&mut framebuffer, &font, &text);
    let changed = before
        .iter()
        .zip(&framebuffer.rgba)
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        changed > 100,
        "the locked door message drew only {changed} pixels over the frozen frame"
    );
}
