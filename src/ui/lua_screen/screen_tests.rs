//! The shipped engine screens (`games/re1/ui/*.lua`), driven through
//! [`LuaScreen`] over an empty pack so the checked-in scripts run.

use super::*;

use crate::pack::Pack;
use crate::transition::FadeColor;
use crate::ui::{Screen, ScreenAction, ScreenResult, UiCue, UiInput};

/// Self-deleting save directory unique to this process and label.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("arklay-ui-screen-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A pack with no screen art and no `ui/` scripts.
fn empty_pack() -> &'static Pack {
    use std::sync::OnceLock;
    static PACK: OnceLock<Pack> = OnceLock::new();
    PACK.get_or_init(|| {
        let mut writer = crate::pack::PackWriter::new();
        writer.add("unused", Vec::new()).unwrap();
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    })
}

struct Harness {
    dir: TempDir,
}

impl Harness {
    fn new(label: &str) -> Self {
        Self {
            dir: TempDir::new(label),
        }
    }

    fn context(&self) -> UiContext<'_> {
        UiContext {
            pack: empty_pack(),
            save_dir: &self.dir.0,
            font: None,
            text: None,
            ticks: 0,
            cues: Default::default(),
        }
    }

    fn open(&self, id: &str, args: &[(&str, ScreenArg)]) -> LuaScreen {
        let mut cx = self.context();
        LuaScreen::open_screen(id, args, ScreenAction::Quit, &mut cx).unwrap()
    }
}

fn neutral() -> UiInput {
    UiInput::default()
}

fn press(build: impl FnOnce(&mut UiInput)) -> UiInput {
    let mut input = UiInput::default();
    build(&mut input);
    input
}

fn stage(screen: &LuaScreen) -> String {
    screen.test_eval("return M.state.stage")
}

/// Tick until the screen leaves, at most `limit` ticks; the action and the
/// number of ticks taken.
fn run_to_result(
    screen: &mut LuaScreen,
    cx: &UiContext<'_>,
    limit: usize,
) -> (ScreenAction, usize) {
    for tick in 1..=limit {
        if let ScreenResult::Done(action) = screen.update(cx, neutral()) {
            return (action, tick);
        }
    }
    panic!("the screen never finished");
}

// ---- title -----------------------------------------------------------------

/// A title in the option menu.
fn title_menu(harness: &Harness, any_saves: bool) -> LuaScreen {
    let screen = harness.open("title", &[]);
    screen.test_eval::<()>(&format!(
        "M.state.stage = 'menu' M.state.any_saves = {any_saves} \
         M.state.selection = {}",
        if any_saves { 2 } else { 1 }
    ));
    screen
}

#[test]
fn title_any_button_opens_the_option_menu() {
    let harness = Harness::new("title-any");
    let mut screen = harness.open("title", &[]);
    screen.update(&harness.context(), press(|i| i.any = true));
    assert_eq!(stage(&screen), "menu");
}

#[test]
fn title_selection_starts_on_new_game_without_saves() {
    let harness = Harness::new("title-nosaves");
    let screen = harness.open("title", &[]);
    assert!(!screen.test_eval::<bool>("return M.state.any_saves"));
    assert_eq!(screen.test_eval::<u8>("return M.state.selection"), 1);
}

#[test]
fn title_the_folder_probe_follows_a_registered_save_screen() {
    let harness = Harness::new("title-folder");
    // A save that is not one of the eight slots.
    crate::save::save_to_path(
        &harness.dir.0.join("save_01.dat"),
        &crate::save::SaveFile::default(),
    )
    .unwrap();
    let any_saves = |folder: bool| -> (bool, u8) {
        let screen = harness.open("title", &[("folder_saves", ScreenArg::Bool(folder))]);
        (
            screen.test_eval("return M.state.any_saves"),
            screen.test_eval("return M.state.selection"),
        )
    };
    // The ported original's probe only looks at `savedat1..8.dat`.
    assert_eq!(any_saves(false), (false, 1));
    // A registered save screen lists the folder, like CR's replacement for
    // the original's "does a save exist" check.
    assert_eq!(any_saves(true), (true, 2));
    // A junk file alone does not count.
    std::fs::remove_file(harness.dir.0.join("save_01.dat")).unwrap();
    std::fs::write(harness.dir.0.join("notes.txt"), b"junk").unwrap();
    assert_eq!(any_saves(true), (false, 1));
}

#[test]
fn title_load_is_unreachable_without_saves() {
    let harness = Harness::new("title-noload");
    let cx = harness.context();
    let mut screen = title_menu(&harness, false);
    screen.update(&cx, press(|i| i.right = true));
    assert_eq!(screen.test_eval::<u8>("return M.state.selection"), 1);
    screen.update(&cx, press(|i| i.left = true));
    assert_eq!(screen.test_eval::<u8>("return M.state.selection"), 1);
    // Even forced onto the row, confirming LOAD does nothing.
    screen.test_eval::<()>("M.state.selection = 2");
    let result = screen.update(&cx, press(|i| i.confirm = true));
    assert_eq!(result, ScreenResult::Continue);
    assert_eq!(stage(&screen), "menu");
}

#[test]
fn title_cursor_selects_load_and_confirms_when_saves_exist() {
    let harness = Harness::new("title-load");
    let cx = harness.context();
    let mut screen = title_menu(&harness, true);
    screen.update(&cx, press(|i| i.left = true));
    assert_eq!(screen.test_eval::<u8>("return M.state.selection"), 1);
    screen.update(&cx, press(|i| i.right = true));
    assert_eq!(screen.test_eval::<u8>("return M.state.selection"), 2);
    screen.update(&cx, press(|i| i.confirm = true));
    assert_eq!(stage(&screen), "fade_out");
    assert_eq!(screen.value("game_active"), 1, "the flash runs at 30 Hz");
    assert_eq!(
        run_to_result(&mut screen, &cx, 256).0,
        ScreenAction::SaveLoad
    );
}

#[test]
fn title_only_the_confirm_plays_the_title_voice() {
    let harness = Harness::new("title-voice");
    let cx = harness.context();
    let mut screen = harness.open("title", &[]);
    // The PRESS dismissal and the option-menu moves are silent.
    screen.update(&cx, press(|i| i.any = true));
    for input in [
        press(|i| i.left = true),
        press(|i| i.right = true),
        press(|i| i.up = true),
        press(|i| i.down = true),
    ] {
        screen.update(&cx, input);
    }
    assert!(
        cx.cues.borrow().is_empty(),
        "PRESS and moves must be silent"
    );
    screen.update(&cx, press(|i| i.confirm = true));
    assert_eq!(cx.cues.borrow().as_slice(), &[UiCue::Title]);
}

#[test]
fn title_confirm_runs_the_white_flash_then_black_out() {
    let harness = Harness::new("title-flash");
    let cx = harness.context();
    let mut screen = title_menu(&harness, false);
    screen.update(&cx, press(|i| i.confirm = true));
    assert_eq!(screen.overlay(), Some((FadeColor::White, 0)));
    let step = |screen: &mut LuaScreen, expected| {
        screen.update(&cx, neutral());
        assert_eq!(screen.overlay(), expected);
    };
    // Leg 6 reaches white in two ticks; legs 7, 8 and 9 start from full.
    step(&mut screen, Some((FadeColor::White, 254)));
    step(&mut screen, Some((FadeColor::White, 255)));
    step(&mut screen, Some((FadeColor::White, 127)));
    step(&mut screen, Some((FadeColor::White, 255)));
    step(&mut screen, Some((FadeColor::White, 255)));
    // Leg 9 decays 16 alpha steps a tick over 15 ticks.
    step(&mut screen, Some((FadeColor::White, 239)));
    for _ in 0..13 {
        screen.update(&cx, neutral());
    }
    step(&mut screen, Some((FadeColor::White, 15)));
    // Leg 4: the black-out rises to full.
    step(&mut screen, Some((FadeColor::Black, 0)));
    step(&mut screen, Some((FadeColor::Black, 4)));
    let (action, ticks) = run_to_result(&mut screen, &cx, 128);
    assert_eq!(action, ScreenAction::CharSelect);
    assert_eq!(ticks, 52, "the black-out holds for 52 more drawn ticks");
}

#[test]
fn title_appears_clear_and_ramps_the_press_text() {
    let harness = Harness::new("title-ramp");
    let cx = harness.context();
    let mut screen = harness.open("title", &[]);
    // No entry fade: the first frame is clear, only the PRESS text ramps.
    screen.update(&cx, neutral());
    assert_eq!(screen.overlay(), None);
    for _ in 0..64 {
        screen.update(&cx, neutral());
    }
    assert_eq!(screen.overlay(), None);
    assert_eq!(
        screen.test_eval::<u8>("return M.state.press_brightness"),
        0x80
    );
    // The blink alternates between two non-black scales.
    let scales: Vec<u8> = (0..64)
        .map(|_| {
            screen.update(&cx, neutral());
            screen.test_eval("return M.press_scale()")
        })
        .collect();
    assert!(scales.contains(&255));
    assert!(scales.iter().any(|&scale| scale < 255));
}

#[test]
fn title_the_idle_title_times_out_into_the_attract_demo() {
    let harness = Harness::new("title-attract");
    let cx = harness.context();
    let mut screen = harness.open("title", &[]);
    // 32 ramp ticks + the 0x708 countdown + the 0x400 fade (8 alpha a tick;
    // the wrap tick ends it).
    let (action, ticks) = run_to_result(&mut screen, &cx, 4000);
    assert_eq!(action, ScreenAction::Attract);
    assert_eq!(ticks, 1863);
    assert_eq!(stage(&screen), "attract_fade");
}

#[test]
fn title_a_button_cancels_the_attract_fade_back_to_press() {
    let harness = Harness::new("title-cancel");
    let cx = harness.context();
    let mut screen = harness.open("title", &[]);
    screen.test_eval::<()>("M.state.attract_countdown = 1 M.state.press_brightness = 0x80");
    screen.update(&cx, neutral());
    assert_eq!(stage(&screen), "attract_fade");
    screen.update(&cx, press(|i| i.any = true));
    assert_eq!(stage(&screen), "press");
    assert_eq!(screen.overlay(), None, "the fade clears instantly");
    assert_eq!(
        screen.test_eval::<i32>("return M.state.attract_countdown"),
        0x708
    );
}

// ---- character select ------------------------------------------------------

/// Ticks one card swap takes to reach its end poses.
const SWAP_TICKS: usize = 34;

/// A card's (x, y, scale, brightness, tpage).
type Pose = (i32, i32, i32, u8, u8);

/// Card `card`'s pose; 0 Chris, 1 Jill.
fn pose(screen: &LuaScreen, card: usize) -> Pose {
    screen.test_eval(&format!(
        "local p = M.state.poses[{}] return p.x, p.y, p.scale, p.bright, p.tpage",
        card + 1
    ))
}

fn selected(screen: &LuaScreen) -> u8 {
    screen.test_eval("return M.state.selected")
}

fn settle(screen: &mut LuaScreen, cx: &UiContext<'_>) {
    for _ in 0..SWAP_TICKS {
        screen.update(cx, neutral());
    }
}

#[test]
fn select_left_and_right_swap_the_pick_when_the_slide_lands() {
    let harness = Harness::new("select-swap");
    let cx = harness.context();
    let mut screen = harness.open("char_select", &[]);
    assert_eq!(selected(&screen), 0);
    screen.update(&cx, press(|i| i.right = true));
    assert_eq!(stage(&screen), "swapping", "right starts the slide");
    assert_eq!(selected(&screen), 0, "the pick flips only at the end");
    settle(&mut screen, &cx);
    assert_eq!(stage(&screen), "idle");
    assert_eq!(selected(&screen), 1);
    screen.update(&cx, press(|i| i.left = true));
    settle(&mut screen, &cx);
    assert_eq!(selected(&screen), 0);
}

#[test]
fn select_the_slide_follows_the_original_frame_trace() {
    let harness = Harness::new("select-trace");
    let cx = harness.context();
    let mut screen = harness.open("char_select", &[]);
    screen.update(&cx, press(|i| i.right = true));
    // Frame 1: card 0 left the front pose by one acceleration step.
    assert_eq!(pose(&screen, 0), (138, 73, 0xF0, 0x80, 2));
    assert_eq!(pose(&screen, 1), (198, 103, 0x110, 0x50, 3));
    let steps: [(usize, Pose, Option<Pose>); 5] = [
        // Frames 2..: the overshoot builds toward x = 312.
        (1, (142, 75, 0xF0, 0x80, 2), None),
        // Frame 10.
        (8, (240, 124, 0xF0, 0x80, 2), None),
        // Frame 15: the scale/brightness ramp is running.
        (5, (300, 154, 0xF8, 0x74, 2), None),
        // Frame 20: the velocity crosses zero and the tpage flips.
        (5, (310, 159, 0x102, 0x65, 3), Some((26, 17, 0xFE, 0x6B, 2))),
        // Frame 33: the cards sit on their final poses.
        (
            13,
            (200, 104, 0x110, 0x50, 3),
            Some((136, 72, 0xF0, 0x80, 2)),
        ),
    ];
    for (ticks, card0, card1) in steps {
        for _ in 0..ticks {
            screen.update(&cx, neutral());
        }
        assert_eq!(pose(&screen, 0), card0);
        if let Some(card1) = card1 {
            assert_eq!(pose(&screen, 1), card1);
        }
    }
    // Frame 34 flips the pick and holds the same poses.
    screen.update(&cx, neutral());
    assert_eq!(stage(&screen), "idle");
    assert_eq!(selected(&screen), 1);
    assert_eq!(pose(&screen, 0), (200, 104, 0x110, 0x50, 3));
    assert_eq!(pose(&screen, 1), (136, 72, 0xF0, 0x80, 2));
}

#[test]
fn select_confirm_starts_a_new_game_for_the_picked_character() {
    let harness = Harness::new("select-confirm");
    let cx = harness.context();
    let mut screen = harness.open("char_select", &[]);
    screen.update(&cx, press(|i| i.right = true));
    settle(&mut screen, &cx);
    assert_eq!(selected(&screen), 1);
    screen.update(&cx, press(|i| i.confirm = true));
    // 128 fade-in + 128 fade-out + the 240-tick g_selTimer hold, matching the
    // reference's case 6 at frame 496.
    let (action, ticks) = run_to_result(&mut screen, &cx, 700);
    assert_eq!(ticks, 496, "the confirm chain's fixed length");
    assert_eq!(action, ScreenAction::NewGame { character: 1 });
}

#[test]
fn select_cues_follow_the_input_edges() {
    let harness = Harness::new("select-cues");
    let cx = harness.context();
    let mut screen = harness.open("char_select", &[]);
    screen.update(&cx, press(|i| i.right = true));
    assert_eq!(cx.cues.borrow().as_slice(), &[UiCue::SelectCursor]);
    cx.cues.borrow_mut().clear();
    settle(&mut screen, &cx);
    screen.update(&cx, press(|i| i.confirm = true));
    assert_eq!(cx.cues.borrow().as_slice(), &[UiCue::SelectConfirm]);

    // Cancel is silent in the original.
    let mut screen = harness.open("char_select", &[]);
    cx.cues.borrow_mut().clear();
    screen.update(&cx, press(|i| i.cancel = true));
    assert!(cx.cues.borrow().is_empty(), "cancel must not queue a cue");
}

/// The white overlay alpha at the current tick, or `None` when clear.
fn white(screen: &LuaScreen) -> Option<u8> {
    match screen.overlay() {
        Some((FadeColor::White, alpha)) => Some(alpha),
        Some((FadeColor::Black, _)) => panic!("expected a white overlay"),
        None => None,
    }
}

#[test]
fn select_confirm_runs_the_white_flash_chain_before_the_new_game() {
    let harness = Harness::new("select-flash");
    let cx = harness.context();
    let mut screen = harness.open("char_select", &[]);
    // Skip the opening black fade.
    for _ in 0..40 {
        screen.update(&cx, neutral());
    }
    screen.update(&cx, press(|i| i.confirm = true));
    assert_eq!(white(&screen), Some(0), "the confirm tick starts clear");
    assert_eq!(stage(&screen), "confirm_fade_in");

    // The flash rises two alpha steps a tick; the 128th tick wraps the 16-bit
    // accumulator and hands over to the fade-out.
    screen.update(&cx, neutral());
    assert_eq!(white(&screen), Some(2));
    for _ in 0..126 {
        screen.update(&cx, neutral());
    }
    assert_eq!(white(&screen), Some(254));
    assert_eq!(stage(&screen), "confirm_fade_in");
    screen.update(&cx, neutral());
    assert_eq!(stage(&screen), "confirm_fade_out");
    assert_eq!(white(&screen), Some(255), "the fade-out starts from full");

    // The white decays two alpha steps a tick for 128 ticks.
    screen.update(&cx, neutral());
    assert_eq!(white(&screen), Some(253));
    for _ in 0..126 {
        screen.update(&cx, neutral());
    }
    assert_eq!(white(&screen), Some(1));
    assert_eq!(stage(&screen), "confirm_fade_out");

    // The 240-tick hold runs with the frame clear: g_selTimer starts at -16
    // and decrements to zero through the signed-byte wrap.
    screen.update(&cx, neutral());
    assert_eq!(stage(&screen), "confirm_hold");
    assert_eq!(screen.overlay(), None);
    let (action, ticks) = run_to_result(&mut screen, &cx, 240);
    assert_eq!(ticks, 240);
    assert_eq!(action, ScreenAction::NewGame { character: 0 });
}

#[test]
fn select_cancel_black_fades_in_eleven_ticks_back_to_the_title() {
    let harness = Harness::new("select-cancel");
    let cx = harness.context();
    let mut screen = harness.open("char_select", &[]);
    for _ in 0..40 {
        screen.update(&cx, neutral());
    }
    screen.update(&cx, press(|i| i.cancel = true));
    assert_eq!(stage(&screen), "cancel_fade_out");
    assert_eq!(screen.overlay(), Some((FadeColor::Black, 0)));
    screen.update(&cx, neutral());
    assert_eq!(screen.overlay(), Some((FadeColor::Black, 24)));
    for _ in 0..9 {
        screen.update(&cx, neutral());
    }
    assert_eq!(screen.overlay(), Some((FadeColor::Black, 240)));
    assert_eq!(
        screen.update(&cx, neutral()),
        ScreenResult::Done(ScreenAction::Title)
    );
}

#[test]
fn select_the_confirm_and_cancel_fades_draw_over_black() {
    let harness = Harness::new("select-black");
    let cx = harness.context();
    let mut screen = harness.open("char_select", &[]);
    for stage in ["confirm_fade_out", "confirm_hold", "cancel_fade_out"] {
        screen.test_eval::<()>(&format!("M.state.stage = '{stage}'"));
        let mut framebuffer = crate::render::Framebuffer::new();
        framebuffer.fill_rect([0, 0, 320, 240], [9, 9, 9, 255]);
        screen.draw(&cx, &mut framebuffer);
        assert!(
            framebuffer
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[..3] == [0, 0, 0]),
            "{stage} must blank the frame"
        );
    }
}

// ---- ending ----------------------------------------------------------------

fn result_screen(harness: &Harness, ending_id: i64, no_save: bool) -> LuaScreen {
    harness.open(
        "ending_result",
        &[
            ("ending_id", ScreenArg::Int(ending_id)),
            ("no_save", ScreenArg::Bool(no_save)),
            ("background", ScreenArg::Bool(ending_id >= 4)),
        ],
    )
}

fn step(screen: &LuaScreen) -> i64 {
    screen.test_eval("return M.state.step")
}

fn active(screen: &LuaScreen, slot: usize) -> bool {
    screen.test_eval(&format!("return M.state.slots[{slot}].active"))
}

/// Tick neutrally until the RESULT machine reaches `target`, with a cap.
fn drive_to(screen: &mut LuaScreen, cx: &UiContext<'_>, target: i64, cap: usize) {
    for _ in 0..cap {
        if step(screen) == target {
            return;
        }
        screen.update(cx, neutral());
    }
    assert_eq!(step(screen), target, "the machine reaches step {target}");
}

#[test]
fn ending_result_runs_the_reference_step_machine() {
    let harness = Harness::new("result-steps");
    let cx = harness.context();
    let mut screen = result_screen(&harness, 1, false);
    assert_eq!(step(&screen), 0);
    // Step 0 arms the background fade and builds the hidden slots.
    screen.update(&cx, neutral());
    assert_eq!(step(&screen), 1);
    assert!(screen.test_eval::<i64>("return M.state.background.fade_vel") > 0);
    assert!((1..=5).all(|slot| !active(&screen, slot)));
    // Steps 1-2 reveal the plates in pairs.
    drive_to(&mut screen, &cx, 2, 0x80);
    assert!(active(&screen, 1) && active(&screen, 2));
    drive_to(&mut screen, &cx, 3, 0x80);
    assert!(active(&screen, 3) && active(&screen, 4));
    // Step 3 is the reveal hold; step 4 waits for input.
    drive_to(&mut screen, &cx, 4, 0x80);
    screen.update(&cx, neutral());
    assert_eq!(step(&screen), 4, "step 4 holds without input");

    // A button press leaves step 4 (ending 1 -> step 5 -> step 10 -> done).
    screen.update(&cx, press(|i| i.any = true));
    assert_eq!(step(&screen), 5);
    let (action, _) = run_to_result(&mut screen, &cx, 0x400);
    assert_eq!(action, ScreenAction::EndingEpilogue);
}

#[test]
fn ending_result_step_four_times_out_without_input() {
    let harness = Harness::new("result-timeout");
    let cx = harness.context();
    let mut screen = result_screen(&harness, 2, false);
    drive_to(&mut screen, &cx, 4, 0x100);
    drive_to(&mut screen, &cx, 5, 0x710);
}

#[test]
fn ending_six_takes_the_extra_plate_steps() {
    let harness = Harness::new("result-six");
    let cx = harness.context();
    let mut screen = result_screen(&harness, 6, false);
    drive_to(&mut screen, &cx, 4, 0x100);
    screen.update(&cx, press(|i| i.any = true));
    assert_eq!(step(&screen), 6, "ending 6 diverts to the fifth plate");
    // The fifth plate activates after the 0x5A hold.
    drive_to(&mut screen, &cx, 8, 0x100);
    assert!(active(&screen, 5));
    assert!(screen.test_eval::<bool>("return M.state.slots[5].additive"));
    let (action, _) = run_to_result(&mut screen, &cx, 0x400);
    assert_eq!(action, ScreenAction::EndingEpilogue);
}

#[test]
fn ending_result_a_no_save_run_enables_the_confetti_bonus() {
    for no_save in [false, true] {
        let harness = Harness::new("result-confetti");
        let cx = harness.context();
        let mut screen = result_screen(&harness, 1, no_save);
        screen.update(&cx, neutral());
        screen.test_eval::<()>("M.state.step = 10 M.state.timer = 0x1E");
        screen.update(&cx, neutral());
        assert_eq!(step(&screen), 11);
        assert_eq!(screen.test_eval::<bool>("return M.state.effects"), no_save);
        assert_eq!(
            screen.test_eval::<i64>("return #M.state.confetti"),
            if no_save { 48 } else { 0 }
        );
        let cues = cx.cues.borrow();
        if no_save {
            assert_eq!(cues.as_slice(), &[UiCue::Decide]);
        } else {
            assert!(cues.is_empty());
        }
    }
}

#[test]
fn ending_fade_shade_and_scale_ramp_to_full() {
    let harness = Harness::new("ending-shade");
    let screen = result_screen(&harness, 1, false);
    let eval = |body: &str| -> i64 {
        screen.test_eval(&format!("local E = require('lib/ending') return {body}"))
    };
    assert_eq!(eval("E.fade_shade(0)"), 0);
    assert_eq!(eval("E.fade_shade(E.FADE_FULL)"), 0x80);
    assert_eq!(eval("E.fade_shade(-1)"), 0);
    assert_eq!(eval("E.shade_scale(0)"), 0);
    assert_eq!(eval("E.shade_scale(0x80)"), 255);
    assert_eq!(eval("E.shade_scale(0x40)"), 127);
    // The epilogue's fixed backdrop shade (`s_bgSlot->fadeLevel` 0x6000).
    assert_eq!(eval("E.fade_shade(0x6000)"), 0x60);
    assert_eq!(eval("E.shade_scale(E.fade_shade(0x6000))"), 191);
}

#[test]
fn ending_result_lays_the_slots_out_from_the_credit_table() {
    let harness = Harness::new("result-layout");
    let cx = harness.context();
    let slot = |screen: &LuaScreen, n: usize| -> (String, i64, i64) {
        screen.test_eval(&format!(
            "local s = M.state.slots[{n}] return s.kind, s.x, s.y"
        ))
    };
    // The plain rows (endings 1-3) use entries 8-12.
    let mut plain = result_screen(&harness, 1, false);
    plain.update(&cx, neutral());
    assert_eq!(slot(&plain, 2), ("clear_time".into(), 0x80, 0x58));
    assert_eq!(slot(&plain, 4), ("save_count".into(), 0x98, 0xB8));
    // The background rows shift right for Jill.
    let mut jill = harness.open(
        "ending_result",
        &[
            ("ending_id", ScreenArg::Int(4)),
            ("character", ScreenArg::Int(1)),
            ("background", ScreenArg::Bool(true)),
        ],
    );
    jill.update(&cx, neutral());
    assert_eq!(slot(&jill, 1), ("sprite".into(), 0x9C, 0x30));
    assert_eq!(slot(&jill, 2), ("clear_time".into(), 0x30 + 0x9C, 0x50));
}

#[test]
fn ending_epilogue_runs_and_spins_the_model() {
    let harness = Harness::new("epilogue");
    let cx = harness.context();
    let mut screen = harness.open("ending_epilogue", &[]);
    assert_eq!(
        screen.test_eval::<i64>("return M.state.captions[1].fade_level"),
        0x8000
    );
    let (action, ticks) = run_to_result(&mut screen, &cx, 0x1000);
    assert_eq!(action, ScreenAction::EndingSave);
    // The 0x10D hold (plus the build tick and the step past it), then the
    // 16-step fade out.
    assert_eq!(ticks, 1 + 0x10E + 16);
    assert_ne!(
        screen.test_eval::<i64>("return M.state.spin"),
        0,
        "the model spins"
    );
}
