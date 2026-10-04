//! Real-asset FILE tab tests.
//!
//! Run with:
//! `ARKLAY_RE1_PACK=re1.akpak cargo test --test file_real -- --ignored --nocapture`
//!
//! The pack must carry the 62 `file/*.tim` entries (two covers, 17 backdrops,
//! 43 page packs) plus the menu sheets and font. The first test loads every
//! TIM, checks the sizes the JPN reader indexes, and pages a collected
//! document through to its last half-page. The second renders the `--ui file`
//! capture twice and requires it to be deterministic and non-empty.

use std::path::Path;

use arklay::bmp;
use arklay::engine;
use arklay::game::{FILE_ITEM_MIN, GameState};
use arklay::pack::Pack;
use arklay::render::Framebuffer;
use arklay::state::{Image, RoomId};
use arklay::text::Text;
use arklay::ui::file::{
    FileAssets, FileMode, FileScreen, JPN_FILEI_NAMES, JPN_TEXTM_NAMES, page_count, page_index,
};
use arklay::ui::main_menu::MenuAssets;

fn pack() -> Option<Pack> {
    let path = std::env::var("ARKLAY_RE1_PACK").ok()?;
    Some(Pack::open(Path::new(&path)).unwrap())
}

fn non_black(image: &Image) -> usize {
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
fn real_file_art_loads_and_a_collected_document_pages_through() {
    let Some(pack) = pack() else {
        return;
    };
    let assets = FileAssets::load(&pack).expect("the pack has the slice-8 file art");
    assert_eq!(assets.covers.len(), 2);
    assert_eq!(assets.covers[0].width, 256);
    assert_eq!(assets.covers[0].height, 256);
    assert_eq!(assets.filei.len(), JPN_FILEI_NAMES.len());
    for (index, filei) in assets.filei.iter().enumerate() {
        assert_eq!(filei.width, 256, "backdrop {index}");
        assert_eq!(filei.height, 120, "backdrop {index}");
    }
    assert_eq!(assets.textm.len(), JPN_TEXTM_NAMES.len());
    for (index, page) in assets.textm.iter().enumerate() {
        assert_eq!((page.width, page.height), (256, 256), "page pack {index}");
    }

    // Room 1001 is Jill's; list slot 0 is file index 0x0F in both lists, and
    // the page tables give it five half-pages.
    let id = RoomId::parse("1001").unwrap();
    let room = arklay::rdt::parse(pack.read(&id.rdt_entry()).unwrap(), id).unwrap();
    let mut game = GameState::new(id, &room);
    game.set_file_collected(0x0F, true);
    assert!(game.document_collected(FILE_ITEM_MIN + 0x0F));

    let menu = MenuAssets::load(&pack).expect("the pack has the menu art");
    let text = Text::load(&pack);
    let mut screen = FileScreen::default();
    screen.open(&game);
    assert_eq!(screen.mode, FileMode::List);
    assert_eq!(screen.slot, 0);

    // Two list renders are byte-identical and repaint a meaningful area.
    let mut first = Framebuffer::new();
    let mut second = Framebuffer::new();
    screen.draw(&mut first, &assets, &menu, &text, &game);
    screen.draw(&mut second, &assets, &menu, &text, &game);
    assert_eq!(first.rgba, second.rgba, "two list renders differ");
    let mut background = Framebuffer::new();
    background.clear();
    assert!(
        non_black(&Image {
            width: first.width,
            height: first.height,
            rgba: first.rgba.clone(),
        }) > 5000
    );

    // Open the reader and walk every half-page; each one draws content and
    // consecutive pages differ.
    screen.handle_input(&mut game, arklay::ui::main_menu::MenuInput::Confirm);
    assert_eq!(screen.mode, FileMode::Reader);
    let count = page_count(0x0F);
    let mut previous: Option<Vec<u8>> = None;
    for page in 0..count {
        assert_eq!(screen.page, page);
        let Some(_) = page_index(0x0F, page) else {
            panic!("page {page} has no TEXTM index");
        };
        let mut frame = Framebuffer::new();
        screen.draw(&mut frame, &assets, &menu, &text, &game);
        let drawn = non_black(&Image {
            width: frame.width,
            height: frame.height,
            rgba: frame.rgba.clone(),
        });
        assert!(drawn > 5000, "page {page} drew only {drawn} pixels");
        if let Some(previous) = previous {
            assert_ne!(previous, frame.rgba, "page {page} did not change");
        }
        previous = Some(frame.rgba);
        if page + 1 < count {
            screen.handle_input(&mut game, arklay::ui::main_menu::MenuInput::Down);
        }
    }
    // Leave a deterministic reader capture behind for inspection.
    let capture =
        std::env::temp_dir().join(format!("arklay-file-reader-{}.bmp", std::process::id()));
    let final_page = previous.expect("at least one page");
    bmp::encode(
        &Image {
            width: 320,
            height: 240,
            rgba: final_page,
        },
        &capture,
    )
    .unwrap();
    println!("wrote {}", capture.display());

    screen.handle_input(&mut game, arklay::ui::main_menu::MenuInput::Confirm);
    assert_eq!(
        screen.mode,
        FileMode::List,
        "confirm exits on the last page"
    );
}

#[test]
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
fn real_file_capture_is_deterministic_and_draws_the_list() {
    let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("arklay-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let saves = dir.join("saves");
    let first = dir.join("file_a.bmp");
    let second = dir.join("file_b.bmp");

    engine::run_ui_with_options(Path::new(&path), "file", Some(&first), &saves, 0).unwrap();
    engine::run_ui_with_options(Path::new(&path), "file", Some(&second), &saves, 0).unwrap();

    let first_bytes = std::fs::read(&first).unwrap();
    let second_bytes = std::fs::read(&second).unwrap();
    assert_eq!(first_bytes, second_bytes, "two file captures differ");
    let image = bmp::decode(&first_bytes).unwrap();
    assert_eq!((image.width, image.height), (320, 240));
    assert!(
        non_black(&image) > 5000,
        "the file capture is mostly black: {} pixels",
        non_black(&image)
    );
    println!("wrote {}", first.display());
}
