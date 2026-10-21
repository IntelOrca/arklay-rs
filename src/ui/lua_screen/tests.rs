use super::*;

use crate::pack::Pack;
use crate::save;
use crate::save::SaveFile;
use crate::text::Text;

/// Self-deleting temporary directory unique to this process and label.
struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("arklay-lua-screen-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A 4x4 RGBA image whose top-left pixel is red, encoded as a BMP.
fn test_bmp() -> Vec<u8> {
    let mut rgba = Vec::with_capacity(4 * 4 * 4);
    for index in 0..16 {
        if index == 0 {
            rgba.extend_from_slice(&[255, 0, 0, 255]);
        } else {
            rgba.extend_from_slice(&[0, 255, 0, 255]);
        }
    }
    crate::bmp::encode_to_vec(&crate::state::Image {
        width: 4,
        height: 4,
        rgba,
    })
    .unwrap()
}

/// A 320x240 RGBA PNG, blue with one half-transparent pixel, so the
/// screen's exact-size background blit can be told from the stretch path
/// that forces opaque alpha.
#[cfg(feature = "hd")]
fn full_png() -> Vec<u8> {
    let (width, height) = (320u32, 240u32);
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.copy_from_slice(&[0, 0, 255, 255]);
    }
    let index = (100 * width as usize + 100) * 4;
    rgba[index + 3] = 128;
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&rgba).unwrap();
    }
    bytes
}

/// A 4x4 4bpp TIM whose palette index 1 is blue.
fn test_tim() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0x10u32.to_le_bytes());
    bytes.extend_from_slice(&0x08u32.to_le_bytes());
    bytes.extend_from_slice(&44u32.to_le_bytes());
    bytes.extend_from_slice(&0i16.to_le_bytes());
    bytes.extend_from_slice(&0i16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    for entry in 0..16u16 {
        let word = if entry == 1 { 0x7C00u16 } else { 0x0000 };
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes.extend_from_slice(&28u32.to_le_bytes());
    bytes.extend_from_slice(&0i16.to_le_bytes());
    bytes.extend_from_slice(&0i16.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    for _ in 0..8u32 {
        bytes.extend_from_slice(&0x1111u16.to_le_bytes());
    }
    bytes
}

/// Build a pack with `screen/save.lua` (the source), a language file and
/// the two test images.
fn pack(source: &str) -> &'static Pack {
    use crate::pack::PackWriter;

    let mut writer = PackWriter::new();
    writer
        .add("screen/save.lua", source.as_bytes().to_vec())
        .unwrap();
    writer.add("art/bg.bmp", test_bmp()).unwrap();
    writer.add("art/dot.tim", test_tim()).unwrap();
    #[cfg(feature = "hd")]
    writer.add("art/full.png", full_png()).unwrap();
    writer
        .add(
            "language/en-US.txt",
            b"UI_SAVE_CR_SAVE = \"Save\"\nROOM_100_NAME_SAVE = \"M. Room 1F\"\n".to_vec(),
        )
        .unwrap();
    let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
    Box::leak(Box::new(pack))
}

/// A synthetic font sheet: every cell is a solid block, so a drawn glyph
/// is measurable without shipping art in the test pack.
fn test_font() -> crate::font::Font {
    let (width, height) = (256u32, 224u32);
    let texture = crate::model::Texture8 {
        width,
        height,
        indices: vec![1; (width * height) as usize],
        palettes: (0..256).map(|index| [index as u8, 0, 0, 255]).collect(),
        stp: vec![false; 256],
    };
    crate::font::Font::new(texture)
}

/// The shared test setup: a pack, the text tables, a font and a temp save
/// directory.
struct Fixture {
    pack: &'static Pack,
    text: Text,
    font: crate::font::Font,
    dir: TempDir,
}

impl Fixture {
    fn new(label: &str, source: &str) -> Self {
        let pack = pack(source);
        let text = Text::load(pack, Some("en-US"));
        Self {
            pack,
            text,
            font: test_font(),
            dir: TempDir::new(label),
        }
    }

    fn context(&self) -> crate::ui::UiContext<'_> {
        crate::ui::UiContext {
            pack: self.pack,
            save_dir: &self.dir.path,
            font: Some(&self.font),
            text: Some(&self.text),
            ticks: 0,
            cues: std::cell::RefCell::new(Vec::new()),
            game: None,
        }
    }

    fn load(&self) -> LuaScreen {
        let mut cx = self.context();
        LuaScreen::open_load("screen/save.lua", &mut cx).unwrap()
    }

    fn save(&self, file: SaveFile, ink_ribbon: bool) -> LuaScreen {
        let mut cx = self.context();
        LuaScreen::open_save("screen/save.lua", &mut cx, file, ink_ribbon).unwrap()
    }
}

use crate::ui::{Screen, ScreenAction, ScreenResult, UiInput};

fn draw(screen: &mut LuaScreen, cx: &crate::ui::UiContext<'_>) -> crate::render::Framebuffer {
    let mut framebuffer = crate::render::Framebuffer::new();
    screen.draw(cx, &mut framebuffer);
    framebuffer
}

#[test]
fn the_script_draws_rects_and_text() {
    let fixture = Fixture::new(
        "draw",
        r##"
function update(api, input) end

function draw(api)
api:rect(0, 0, 320, 240, 0x204060)
api:rect(10, 10, 20, 20, "#FF0000", 128)
api:text(8, 8, "Hello " .. api:string("UI_SAVE_CR_SAVE"))
width = api:text_width("Hello")
api:fade(64)
end
"##,
    );
    let mut screen = fixture.load();
    let cx = fixture.context();
    let framebuffer = draw(&mut screen, &cx);
    assert_eq!(&framebuffer.rgba[..4], &[0x20, 0x40, 0x60, 255]);
    let pixel = |x: usize, y: usize| {
        let index = (y * 320 + x) * 4;
        [
            framebuffer.rgba[index],
            framebuffer.rgba[index + 1],
            framebuffer.rgba[index + 2],
        ]
    };
    assert_ne!(pixel(15, 15), pixel(60, 60), "the translucent rect blended");
    let text_pixels = (0..30)
        .flat_map(|y| (0..160).map(move |x| (x, y)))
        .filter(|&(x, y)| pixel(x, y) != [0x20, 0x40, 0x60])
        .count();
    assert!(text_pixels > 20, "the text drew {text_pixels} pixels");
    assert!(
        screen.test_global::<i32>("width") > 0,
        "text_width measures"
    );
    assert_eq!(screen.fade(), 64);
}

#[test]
fn sprites_and_backgrounds_draw_from_pack_images() {
    let fixture = Fixture::new(
        "sprites",
        r##"
function update(api, input) end

function draw(api)
api:background("art/bg.bmp")
api:sprite("art/dot.tim", 10, 20)
api:sprite_region("art/dot.tim", 0, 0, 2, 2, 40, 40, 8, 8)
size_w, size_h = api:image_size("art/dot.tim")
api:sprite("art/none.bmp", 0, 0)
end
"##,
    );
    let mut screen = fixture.load();
    let cx = fixture.context();
    let framebuffer = draw(&mut screen, &cx);
    let pixel = |x: usize, y: usize| {
        let index = (y * 320 + x) * 4;
        [
            framebuffer.rgba[index],
            framebuffer.rgba[index + 1],
            framebuffer.rgba[index + 2],
        ]
    };
    // The 4x4 BMP stretched over the screen keeps its top-left red.
    assert_eq!(pixel(0, 0), [255, 0, 0]);
    // The TIM sprite drew its blue palette at (10, 20).
    assert_eq!(pixel(11, 21), [0, 0, 255]);
    // The stretched background's second column is green there.
    assert_eq!(pixel(100, 100), [0, 255, 0]);
    // The scaled sub-region drew blue at (40, 40) and (47, 47).
    assert_eq!(pixel(40, 40), [0, 0, 255]);
    assert_eq!(pixel(47, 47), [0, 0, 255]);
    assert_eq!(screen.test_global::<u32>("size_w"), 8);
    assert_eq!(screen.test_global::<u32>("size_h"), 4);
}

#[test]
fn a_missing_image_leaves_the_screen() {
    let fixture = Fixture::new(
        "no-image",
        r#"
function update(api, input) end
function draw(api)
api:sprite("art/none.bmp", 0, 0)
end
"#,
    );
    let mut screen = fixture.load();
    let cx = fixture.context();
    let _ = draw(&mut screen, &cx);
    assert_eq!(
        screen.update(&cx, UiInput::default()),
        ScreenResult::Done(ScreenAction::Title)
    );
}

#[test]
fn saves_lists_the_directory_with_parsed_metadata() {
    let fixture = Fixture::new(
        "list",
        r#"
function update(api)
local list = api:saves()
count = #list
first = list[1]
second = list[2]
end
"#,
    );
    let file = SaveFile {
        character: 1,
        saves: 7,
        stage: 0,
        room: 0x00,
        ..SaveFile::default()
    };
    save::save_to_path(&fixture.dir.path.join("beta.dat"), &file).unwrap();
    std::fs::write(fixture.dir.path.join("notes.txt"), b"junk").unwrap();

    let mut screen = fixture.load();
    let cx = fixture.context();
    screen.update(&cx, UiInput::default());
    assert_eq!(screen.test_global::<i32>("count"), 2);
    let first: mlua::Table = screen.test_global("first");
    assert_eq!(first.get::<String>("name").unwrap(), "beta.dat");
    assert_eq!(first.get::<String>("base").unwrap(), "beta");
    assert!(first.get::<bool>("loadable").unwrap());
    assert_eq!(first.get::<u8>("character").unwrap(), 1);
    assert_eq!(first.get::<u8>("saves").unwrap(), 7);
    assert_eq!(first.get::<u8>("stage").unwrap(), 0);
    assert_eq!(first.get::<String>("location").unwrap(), "M. Room 1F");
    assert!(first.get::<i64>("timestamp").unwrap() > 0);
    let second: mlua::Table = screen.test_global("second");
    assert_eq!(second.get::<String>("name").unwrap(), "notes.txt");
    assert!(!second.get::<bool>("loadable").unwrap());
}

#[test]
fn save_writes_the_pending_block_and_records_the_outcome() {
    let fixture = Fixture::new(
        "save",
        r#"function update(api) ok, err = api:save("new_01.dat") end"#,
    );
    let mut file = SaveFile {
        character: 0,
        saves: 3,
        total_held: 1,
        ..SaveFile::default()
    };
    file.player_slots[0] = crate::game::InventoryItem {
        id: crate::items::ITEM_INK_RIBBONS,
        quantity: 2,
    };

    let mut screen = fixture.save(file, true);
    let cx = fixture.context();
    screen.update(&cx, UiInput::default());
    assert!(screen.test_global::<bool>("ok"));
    assert_eq!(screen.test_global::<Option<String>>("err"), None);

    // The written block consumed one ribbon.
    let written = save::load_file(&fixture.dir.path.join("new_01.dat")).unwrap();
    assert_eq!(written.player_slots[0].quantity, 1);
    assert_eq!(written.used_item, crate::items::ITEM_INK_RIBBONS);
    assert_eq!(
        screen.take_outcome(),
        Some(crate::ui::save_load::SaveOutcome {
            slot: None,
            ink_ribbon: true
        })
    );

    // The scan saw the new file.
    let names: Vec<String> = fixture
        .dir
        .path
        .read_dir()
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, ["new_01.dat"]);
}

#[test]
fn save_rejects_bad_names_and_the_wrong_mode() {
    let fixture = Fixture::new(
        "save-bad",
        r#"
function update(api)
ok1, err1 = api:save("../escape.dat")
ok2, err2 = api:save("")
end
"#,
    );
    let mut screen = fixture.save(SaveFile::default(), false);
    let cx = fixture.context();
    screen.update(&cx, UiInput::default());
    assert!(!screen.test_global::<bool>("ok1"));
    assert!(screen.test_global::<Option<String>>("err1").is_some());
    assert!(!screen.test_global::<bool>("ok2"));
    assert!(screen.take_outcome().is_none());

    // Load mode refuses a save.
    let fixture = Fixture::new(
        "save-mode",
        r#"function update(api) ok = api:save("x.dat") end"#,
    );
    let mut screen = fixture.load();
    let cx = fixture.context();
    screen.update(&cx, UiInput::default());
    assert!(!screen.test_global::<bool>("ok"));
}

#[test]
fn load_requests_the_chosen_file_and_exit_leaves() {
    let fixture = Fixture::new(
        "load",
        r#"
function update(api, input)
if input.confirm then
    ok = api:load("slot.dat")
elseif input.cancel then
    api:exit()
end
end
"#,
    );
    save::save_to_path(&fixture.dir.path.join("slot.dat"), &SaveFile::default()).unwrap();
    let mut screen = fixture.load();
    let cx = fixture.context();

    let result = screen.update(
        &cx,
        UiInput {
            confirm: true,
            ..UiInput::default()
        },
    );
    assert_eq!(
        result,
        ScreenResult::Done(ScreenAction::LoadFile(fixture.dir.path.join("slot.dat")))
    );

    // A second screen exits back to the title in load mode.
    let mut screen = fixture.load();
    let result = screen.update(
        &cx,
        UiInput {
            cancel: true,
            ..UiInput::default()
        },
    );
    assert_eq!(result, ScreenResult::Done(ScreenAction::Title));
}

#[test]
fn save_mode_exit_resumes_and_remove_deletes() {
    let fixture = Fixture::new(
        "remove",
        r#"
function update(api, input)
if input.confirm then
    ok = api:remove("old.dat")
    api:exit()
end
end
"#,
    );
    save::save_to_path(&fixture.dir.path.join("old.dat"), &SaveFile::default()).unwrap();
    let mut screen = fixture.save(SaveFile::default(), false);
    let cx = fixture.context();
    let result = screen.update(
        &cx,
        UiInput {
            confirm: true,
            ..UiInput::default()
        },
    );
    assert_eq!(result, ScreenResult::Done(ScreenAction::Resume));
    assert!(screen.test_global::<bool>("ok"));
    assert!(!fixture.dir.path.join("old.dat").exists());
}

#[test]
fn input_edges_reach_the_script() {
    let fixture = Fixture::new(
        "input",
        r#"
function update(api, input)
pad = input.pad
up = input.up
confirm = input.confirm
end
"#,
    );
    let mut screen = fixture.load();
    let cx = fixture.context();
    screen.update(
        &cx,
        UiInput {
            up: true,
            confirm: true,
            ..UiInput::default()
        },
    );
    assert_eq!(screen.test_global::<u16>("pad"), 0x0011);
    assert!(screen.test_global::<bool>("up"));
    assert!(screen.test_global::<bool>("confirm"));
}

#[test]
fn on_open_sounds_queue_before_the_first_tick() {
    let fixture = Fixture::new(
        "on-open-sound",
        r#"
function on_open(api)
api:sound("save_confirm")
end

function update(api, input) end
"#,
    );
    let mut cx = fixture.context();
    let _screen = LuaScreen::open_load("screen/save.lua", &mut cx).unwrap();
    assert_eq!(
        cx.cues.borrow().as_slice(),
        &[crate::ui::UiCue::SaveConfirm]
    );
}

#[test]
fn sounds_queue_the_screen_cues() {
    let fixture = Fixture::new(
        "sounds",
        r#"
function update(api, input)
if input.up then api:sound("save_cursor") end
if input.confirm then api:sound("save_confirm") end
if input.cancel then api:sound("error") end
end
"#,
    );
    let mut screen = fixture.load();
    let cx = fixture.context();
    for input in [
        UiInput {
            up: true,
            ..UiInput::default()
        },
        UiInput {
            confirm: true,
            ..UiInput::default()
        },
        UiInput {
            cancel: true,
            ..UiInput::default()
        },
    ] {
        screen.update(&cx, input);
    }
    assert_eq!(
        cx.cues.borrow().as_slice(),
        &[
            crate::ui::UiCue::SaveCursor,
            crate::ui::UiCue::SaveConfirm,
            crate::ui::UiCue::Cancel
        ]
    );
}

#[test]
fn a_failing_update_leaves_the_screen() {
    let fixture = Fixture::new("fail", r#"function update(api) error("boom") end"#);
    let mut screen = fixture.load();
    let cx = fixture.context();
    assert_eq!(
        screen.update(&cx, UiInput::default()),
        ScreenResult::Done(ScreenAction::Title)
    );
}

#[test]
fn a_failing_draw_marks_the_screen_and_it_leaves_next_tick() {
    let fixture = Fixture::new(
        "draw-fail",
        r#"function update(api, input) end
function draw(api) error("boom") end"#,
    );
    let mut screen = fixture.save(SaveFile::default(), false);
    let cx = fixture.context();
    let _ = draw(&mut screen, &cx);
    assert_eq!(
        screen.update(&cx, UiInput::default()),
        ScreenResult::Done(ScreenAction::Resume)
    );
}

#[test]
fn the_sandbox_has_no_io_or_os() {
    let fixture = Fixture::new(
        "sandbox",
        r#"
function update(api)
io_absent = io == nil
os_absent = os == nil
api:log("sandbox ok")
end
"#,
    );
    let mut screen = fixture.load();
    let cx = fixture.context();
    screen.update(&cx, UiInput::default());
    assert!(screen.test_global::<bool>("io_absent"));
    assert!(screen.test_global::<bool>("os_absent"));
}

#[test]
fn a_missing_script_is_an_error() {
    let fixture = Fixture::new("missing", "local x = 1");
    let mut cx = fixture.context();
    assert!(LuaScreen::open_load("screen/none.lua", &mut cx).is_err());
}

#[test]
fn a_script_without_update_is_an_error() {
    let fixture = Fixture::new("no-update", r#"function draw(api) end"#);
    let mut cx = fixture.context();
    let Err(err) = LuaScreen::open_load("screen/save.lua", &mut cx) else {
        panic!("a script without update must be rejected");
    };
    let message = format!("{err:#}");
    assert!(message.contains("update"), "{message}");
}

#[test]
fn a_broken_script_is_an_error() {
    let fixture = Fixture::new("broken", "function update(api) return");
    let mut cx = fixture.context();
    assert!(LuaScreen::open_load("screen/save.lua", &mut cx).is_err());
}

#[test]
fn pad_words_match_the_original_layout() {
    assert_eq!(super::pad_word(UiInput::default()), 0);
    assert_eq!(
        super::pad_word(UiInput {
            up: true,
            down: true,
            confirm: true,
            cancel: true,
            start: true,
            page_left: true,
            page_right: true,
            ..UiInput::default()
        }),
        0x0C3B
    );
    assert_eq!(
        super::held_word(UiInput {
            held_up: true,
            held_right: true,
            ..UiInput::default()
        }),
        0x0090
    );
}

#[test]
fn colour_parsing_covers_names_numbers_and_hex() {
    use super::{colour_rgb, colour_tint};
    use crate::font::Tint;
    let lua = mlua::Lua::new();
    assert_eq!(colour_tint(&mlua::Value::Integer(1)).unwrap(), Tint::Green);
    assert_eq!(
        colour_tint(&mlua::Value::String(lua.create_string("grey").unwrap())).unwrap(),
        Tint::Grey
    );
    assert_eq!(
        colour_rgb(&mlua::Value::Integer(0x123456)).unwrap(),
        [0x12, 0x34, 0x56]
    );
    assert_eq!(
        colour_rgb(&mlua::Value::String(lua.create_string("#A0B0C0").unwrap())).unwrap(),
        [0xA0, 0xB0, 0xC0]
    );
    assert_eq!(
        colour_tint(&mlua::Value::String(lua.create_string("#3F7DFA").unwrap())).unwrap(),
        Tint::Rgb(0x3F, 0x7D, 0xFA)
    );
    assert!(colour_rgb(&mlua::Value::String(lua.create_string("#zz").unwrap())).is_err());
    // An eight-byte string whose `é` straddles the bytes a slice would
    // split: rejected, not a panic.
    assert!(colour_rgb(&mlua::Value::String(lua.create_string("#0é0000").unwrap())).is_err());
}

#[test]
fn hex_colour_parsing_never_slices_a_multibyte_string() {
    use super::parse_hex_colour;
    // Six bytes with `é` occupying bytes 1..3: a `str` slice at byte 2
    // would split the character and panic.
    assert_eq!(parse_hex_colour("0é0000"), None);
    assert_eq!(parse_hex_colour("123456"), Some([0x12, 0x34, 0x56]));
    // The alpha byte of the eight-digit form is ignored.
    assert_eq!(parse_hex_colour("12345678"), Some([0x12, 0x34, 0x56]));
    assert_eq!(parse_hex_colour("12345"), None);
}

#[test]
fn extreme_coordinates_clamp_instead_of_overflowing() {
    let fixture = Fixture::new(
        "coords",
        r#"
function update(api, input) end
function draw(api)
api:rect(0, 0, 2147483647, 2147483647, 0x101010)
api:text(2147483647, -2147483648, "edge")
api:sprite("art/bg.bmp", 2147483647, -2147483648)
api:sprite_region("art/dot.tim", 2147483647, -2147483648, 2147483647, -2147483648,
    2147483647, -2147483648, 2147483647, 2147483647)
end
"#,
    );
    let mut screen = fixture.load();
    let cx = fixture.context();
    // Before the clamp, the sprite and font cursors overflowed on these
    // values and the process panicked; the screen must keep running.
    let _ = draw(&mut screen, &cx);
    assert_eq!(
        screen.update(&cx, UiInput::default()),
        ScreenResult::Continue
    );
}

#[test]
fn a_pack_without_a_font_draws_no_text_and_keeps_running() {
    let fixture = Fixture::new(
        "no-font",
        r#"
function update(api, input) end
function draw(api)
api:text(8, 8, "Hello")
width = api:text_width("Hello")
end
"#,
    );
    let mut screen = fixture.load();
    // The engine passes no font for a pack that carries none; the screen
    // must degrade to drawing no text rather than leave.
    let mut cx = fixture.context();
    cx.font = None;
    let _ = draw(&mut screen, &cx);
    assert_eq!(
        screen.update(&cx, UiInput::default()),
        ScreenResult::Continue
    );
    assert_eq!(screen.test_global::<i32>("width"), 0);
}

#[test]
fn the_blend_option_mixes_a_sprite_with_the_backdrop() {
    let fixture = Fixture::new(
        "blend",
        r#"
function update(api, input) end
function draw(api)
api:rect(0, 0, 320, 240, 0x808080)
api:sprite("art/bg.bmp", 10, 10, {blend = "half"})
api:sprite("art/bg.bmp", 20, 10)
end
"#,
    );
    let mut screen = fixture.load();
    let cx = fixture.context();
    let framebuffer = draw(&mut screen, &cx);
    let pixel = |x: usize, y: usize| {
        let index = (y * 320 + x) * 4;
        [
            framebuffer.rgba[index],
            framebuffer.rgba[index + 1],
            framebuffer.rgba[index + 2],
        ]
    };
    // The BMP's top-left is red: "half" averages it with the grey
    // backdrop per channel, the default blend overwrites it.
    assert_eq!(pixel(10, 10), [191, 64, 64]);
    assert_eq!(pixel(20, 10), [255, 0, 0]);
}

#[cfg(feature = "hd")]
#[test]
fn an_exact_size_background_is_blitted_with_its_alpha() {
    let fixture = Fixture::new(
        "blit",
        r#"
function update(api, input) end
function draw(api)
api:background("art/full.png")
end
"#,
    );
    let mut screen = fixture.load();
    let cx = fixture.context();
    let framebuffer = draw(&mut screen, &cx);
    let pixel = |x: usize, y: usize| {
        let index = (y * 320 + x) * 4;
        framebuffer.rgba[index..index + 4].to_vec()
    };
    assert_eq!(pixel(10, 10), [0, 0, 255, 255]);
    // The stretch path forces opaque alpha; the 320x240 blit keeps it.
    assert_eq!(pixel(100, 100), [0, 0, 255, 128]);
}

#[test]
fn replaced_art_leaves_a_hole_and_a_quad_while_capturing() {
    use crate::pack::PackWriter;

    let source = r#"
function update(api, input) end

function draw(api)
local picture = api:picture("art/bg")
api:picture_sprite_scaled(picture, 0, 0, 4, 4, 10, 20, 40, 40, 255)
end
"#;
    let build = |hd: Option<(u32, Vec<u8>)>| {
        let mut writer = PackWriter::new();
        writer
            .add("screen/save.lua", source.as_bytes().to_vec())
            .unwrap();
        writer.add("art/bg.bmp", test_bmp()).unwrap();
        if let Some((hash, bytes)) = hd {
            writer
                .add(&format!("htexture/misc/{hash:08X}.bmp"), bytes)
                .unwrap();
        }
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    };
    let picture =
        crate::ui::load_picture(&build(None), "art/bg", crate::assets::Geometry::Free, None)
            .unwrap();
    let candidate = crate::htexture::image_candidates(&picture)[0];
    let hd = crate::bmp::encode_to_vec(&crate::state::Image {
        width: 8,
        height: 8,
        rgba: [0, 0, 255, 255].repeat(64),
    })
    .unwrap();
    let pack: &'static Pack = Box::leak(Box::new(build(Some((candidate.hash, hd)))));
    let fixture = Fixture {
        pack,
        text: Text::load(pack, Some("en-US")),
        font: test_font(),
        dir: TempDir::new("hd-art"),
    };
    let mut screen = fixture.load();
    let cx = fixture.context();
    let pixel = |framebuffer: &crate::render::Framebuffer, x: usize, y: usize| {
        let index = (y * 320 + x) * 4;
        [
            framebuffer.rgba[index],
            framebuffer.rgba[index + 1],
            framebuffer.rgba[index + 2],
        ]
    };

    // Without a capture the original art draws.
    let plain = draw(&mut screen, &cx);
    assert_eq!(pixel(&plain, 10, 20), [255, 0, 0]);

    for hole in [0, 255] {
        let mut framebuffer = crate::render::Framebuffer::new();
        framebuffer.capture_hd(hole);
        screen.draw(&cx, &mut framebuffer);
        assert_eq!(pixel(&framebuffer, 10, 20), [hole; 3]);
        assert_eq!(pixel(&framebuffer, 49, 59), [hole; 3]);
        assert_eq!(pixel(&framebuffer, 50, 60), [0; 3]);
        let quads = framebuffer.hd_quads();
        assert_eq!(quads.len(), 1);
        let [fw, fh] = candidate.frame.map(|v| v as f32);
        assert_eq!(quads[0].dst, [10.0, 20.0, 50.0, 60.0]);
        assert_eq!(quads[0].uv, [0.0, 0.0, 4.0 / fw, 4.0 / fh]);
        assert_eq!(quads[0].colour, [1.0; 3]);
        assert_eq!(quads[0].clip, [0, 0, 320, 240]);
        assert_eq!(quads[0].image.image.width, 8);
    }
}
