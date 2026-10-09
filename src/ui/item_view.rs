//! The item viewer (examine screen).
//!
//! [`ItemViewScreen`] owns everything it draws: the selected item's `.ivm`
//! model and texture, the item name and the `text/idesc.bin` description
//! window. As an examine viewer it is a [`Screen`] the engine installs as a
//! gameplay modal over the frozen pause menu; leaving it returns to the menu
//! unchanged. The same screen also serves the engine's pick-up flow in
//! [`ViewMode::Take`]/[`ViewMode::GotItem`]: those modes start on the model's
//! spin-in intro ([`ItemViewScreen::step_intro`]) and the engine hands them
//! back a closing animation ([`ItemViewScreen::begin_exit`]) once its message
//! resolves, so they never read the pad themselves.
//!
//! The model renders through [`Framebuffer::draw_ivm_unlit`]: full-bright,
//! one 256-colour CLUT row with direct UVs and backface culling disabled, one
//! root matrix for every object. The name and the description both draw on
//! the original's item line (`0xBA`, left margin `0x22` for the Japanese sheet).
//!
//! # Input
//!
//! Left/right turn the model's turntable, up/down pitch it, at the original's
//! `0x20` 12-bit units per 30 Hz frame while the key is held; the angles wrap
//! at `0x1000`. Confirm runs the item's examine check: the pose must fall
//! inside one of the item's `g_ItemExamineCombos` windows for the description
//! to open, and the red book (and the two doom books) take the zoom spin path
//! first. An item with no combo records opens immediately. Confirm again
//! dismisses the description, and cancel leaves the viewer. A missing model
//! (the placeholder name `m`, or a pack without `item_m2` art) or a missing
//! description logs a warning and leaves that layer empty instead of failing
//! the screen.

use anyhow::Result;

use crate::anim::{self, Mat4x3};
use crate::font::Tint;
use crate::items;
use crate::ivm::{self, Ivm};
use crate::message::{MessageInput, MessageWindow};
use crate::pack::Pack;
use crate::render::{Camera, Framebuffer};
use crate::text::Text;

use super::layout;
use super::main_menu::item_name_bytes;
use super::{Screen, ScreenAction, ScreenResult, UiContext, UiCue, UiInput};

/// Pack path prefix of the shipped item-view models.
pub const ITEM_MODEL_PREFIX: &str = "item/";
/// Viewer camera position: on the +X axis, looking at the origin.
pub const CAMERA_FROM: [i32; 3] = [15000, 0, 0];
/// Viewer camera look-at point.
pub const CAMERA_TO: [i32; 3] = [0, 0, 0];
/// The model's resting translation on +X, in front of the camera.
pub const MODEL_REST_X: i32 = 0x1980;
/// The menu's projection centre (the original's `SetSubpixelOffset(112, 76)`
/// while `main_menu` owns the screen).
pub const MENU_CENTER: [i32; 2] = [112, 76];
/// Viewer focal length in pixels.
pub const VIEWER_FOV: i32 = 0xC0;
/// Rotation step per 30 Hz frame, in 12-bit angle units.
pub const ROTATE_STEP: i32 = 0x20;
/// The description table index the Ingram maps to (1-based).
const INGRAM_DESCRIPTION: u16 = 0x4E;
/// The description table index the Minimi maps to (1-based).
const MINIMI_DESCRIPTION: u16 = 0x4F;
/// The intro/exit duration in 30 Hz frames.
const INTRO_FRAMES: i32 = 0x40;
/// The intro/exit model spin step per frame, in 12-bit angle units.
const INTRO_YAW_STEP: i32 = 0xC0;
/// The intro/exit model roll step per frame, in 12-bit angle units.
const INTRO_ROLL_STEP: i32 = 0x80;
/// The intro/exit translation step per frame. `0x40` frames of it land
/// exactly on [`MODEL_REST_X`].
const INTRO_ZOOM_STEP: i32 = 0x322;
/// The model's translation before the intro starts, far from the camera.
const INTRO_START_X: i32 = MODEL_REST_X - INTRO_FRAMES * INTRO_ZOOM_STEP;

/// Which flow opened the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// The pause-menu CHECK path: turntable input, examine check and the
    /// description window. This is the default.
    Examine,
    /// A ground pick-up: the model intro, then the engine's global 0xC0
    /// yes/no prompt. The viewer itself takes no pad input.
    Take,
    /// A scripted `give_item`: the model intro, then the engine's global 0xC1
    /// line, with the award when the mode closes.
    GotItem,
}

/// Where the viewer's model animation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewStage {
    /// The resting display: the direction keys drive the turntable.
    Display,
    /// The red/doom book zoom: the entry spin-in runs to `0x300` degrees and
    /// then opens the description.
    Zoom,
    /// The take/got-item opening animation: the model spins in from far away
    /// while the screen fades up.
    Intro,
    /// The take/got-item closing animation: the intro in reverse.
    Exit,
}

/// What the item examine check asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExamineOutcome {
    /// The pose is inside a window (or the item has no windows): open the
    /// description.
    Open,
    /// A book whose pose matched: run the zoom spin before the description.
    Zoom,
    /// The pose is outside every window: the examine is refused.
    Refused,
}

/// The item viewer's screen state.
#[derive(Debug, Clone)]
pub struct ItemViewScreen {
    /// The examined item id.
    item: u8,
    /// Which flow opened the viewer.
    mode: ViewMode,
    /// The item's encoded name (empty when the pack has no name table).
    name: Vec<u8>,
    /// The item's encoded description, resolved at open.
    description: Option<Vec<u8>>,
    /// The parsed model; `None` when the item has no shipped file.
    model: Option<Ivm>,
    /// Turntable yaw in 12-bit angle units.
    pub yaw: i32,
    /// Turntable pitch in 12-bit angle units.
    pub pitch: i32,
    /// Turntable roll in 12-bit angle units. The port has no roll key, but the
    /// examine windows test it, so it stays a field.
    pub roll: i32,
    /// The model's translation toward the camera; the intro/exit animation
    /// drives it between [`INTRO_START_X`] and [`MODEL_REST_X`].
    model_x: i32,
    /// Which stage of the model animation is running.
    stage: ViewStage,
    /// Ticks the zoom/intro/exit has run.
    zoom_timer: i32,
    /// The zoom's spin accumulator, the original's `DAT_00ae9f5e`.
    zoom_spin: i32,
    /// The zoom/intro/exit ramp, drawn as a black fade over the screen. The
    /// original ramps its three viewer lights; the port approximates that
    /// with the screen fade.
    entry_fade: u8,
    /// The description window, drawn on the menu line.
    message: MessageWindow,
}

impl ItemViewScreen {
    /// A viewer for `item`, still without any loaded art.
    pub fn new(item: u8) -> Self {
        Self::with_mode(item, ViewMode::Examine)
    }

    /// A viewer for a ground pick-up: it opens on the model intro and the
    /// engine ends it with [`ItemViewScreen::begin_exit`] once the yes/no
    /// prompt resolves.
    pub fn new_take(item: u8) -> Self {
        Self::with_mode(item, ViewMode::Take)
    }

    /// A viewer for a scripted `give_item` award; same intro as
    /// [`ItemViewScreen::new_take`] but the engine shows global 0xC1 instead
    /// of the yes/no prompt.
    pub fn new_got_item(item: u8) -> Self {
        Self::with_mode(item, ViewMode::GotItem)
    }

    fn with_mode(item: u8, mode: ViewMode) -> Self {
        let mut screen = Self {
            item,
            mode,
            name: Vec::new(),
            description: None,
            model: None,
            yaw: 0,
            pitch: 0,
            roll: 0,
            model_x: MODEL_REST_X,
            stage: ViewStage::Display,
            zoom_timer: 0,
            zoom_spin: 0,
            entry_fade: 0,
            message: MessageWindow::default(),
        };
        if mode != ViewMode::Examine {
            screen.restart_intro();
        }
        screen
    }

    /// Reset the state for the take/got-item opening animation.
    fn restart_intro(&mut self) {
        self.yaw = 0;
        self.pitch = 0;
        self.roll = 0;
        self.model_x = INTRO_START_X;
        self.stage = ViewStage::Intro;
        self.zoom_timer = INTRO_FRAMES;
        self.entry_fade = 255;
    }

    /// Load the item's model, name and description from `pack` and `text`.
    ///
    /// Every failure is a warning: a missing `item/{name}.ivm` leaves
    /// [`ItemViewScreen::model`] empty and the screen still shows the text
    /// layers, which is how ING/MINI items and stale packs stay usable.
    /// `examined` is the game state's examined-item bank, so an unexamined
    /// item still draws its generic name.
    pub fn open_with(&mut self, pack: &Pack, text: &Text, examined: &[u8; 4]) {
        self.name = item_name_bytes(text, self.item, examined)
            .unwrap_or(&[])
            .to_vec();
        self.description = description_bytes(text, self.item).map(<[u8]>::to_vec);
        self.model = load_model(pack, self.item);
    }

    /// The examined item id.
    pub fn item(&self) -> u8 {
        self.item
    }

    /// The loaded model, when the pack carries one.
    pub fn model(&self) -> Option<&Ivm> {
        self.model.as_ref()
    }

    /// The item's encoded name bytes.
    pub fn name(&self) -> &[u8] {
        &self.name
    }

    /// The item's encoded description bytes, when the table has an entry.
    pub fn description(&self) -> Option<&[u8]> {
        self.description.as_deref()
    }

    /// Whether the description window is up.
    pub fn message_active(&self) -> bool {
        self.message.active
    }

    /// Which flow opened the viewer.
    pub fn mode(&self) -> ViewMode {
        self.mode
    }

    /// The root matrix of every model object: the turntable rotation and the
    /// model's current translation toward the camera (animated by the
    /// intro/exit in the take/got-item modes).
    pub fn root_matrix(&self) -> Mat4x3 {
        Mat4x3 {
            r: anim::rotation_matrix(self.pitch, self.yaw, self.roll),
            t: [self.model_x, 0, 0],
        }
    }

    /// Whether the take/got-item opening animation is still running.
    pub fn in_intro(&self) -> bool {
        self.stage == ViewStage::Intro
    }

    /// Advance the opening animation one frame; returns `true` when it has
    /// finished and the model has settled on its resting pose.
    ///
    /// The original's `FUN_0044e1b0` state 1: `0x40` frames of translation
    /// toward the camera, yaw and roll, with the lights ramping up. The port
    /// approximates the light ramp with the full-screen fade.
    pub fn step_intro(&mut self) -> bool {
        if self.stage != ViewStage::Intro {
            return true;
        }
        self.model_x += INTRO_ZOOM_STEP;
        self.yaw = (self.yaw + INTRO_YAW_STEP) & 0x0FFF;
        self.roll = (self.roll + INTRO_ROLL_STEP) & 0x0FFF;
        self.zoom_timer -= 1;
        self.entry_fade = (self.zoom_timer * 4).clamp(0, 255) as u8;
        if self.zoom_timer > 0 {
            return false;
        }
        // The intro's rotation is a whole number of turns, so it lands back
        // on the identity pose; snap there in case of a stray angle.
        self.model_x = MODEL_REST_X;
        self.yaw = 0;
        self.pitch = 0;
        self.roll = 0;
        self.entry_fade = 0;
        self.stage = ViewStage::Display;
        true
    }

    /// Start the closing animation: the reverse of the opener.
    pub fn begin_exit(&mut self) {
        self.stage = ViewStage::Exit;
        self.zoom_timer = INTRO_FRAMES;
    }

    /// Whether the closing animation is running.
    pub fn in_exit(&self) -> bool {
        self.stage == ViewStage::Exit
    }

    /// Advance the closing animation one frame; returns `true` when it has
    /// finished and the viewer may close.
    pub fn step_exit(&mut self) -> bool {
        if self.stage != ViewStage::Exit {
            return true;
        }
        self.zoom_timer -= 1;
        if self.zoom_timer <= 0 {
            self.entry_fade = 255;
            return true;
        }
        self.model_x -= INTRO_ZOOM_STEP;
        self.yaw = (self.yaw - INTRO_YAW_STEP) & 0x0FFF;
        self.roll = (self.roll - INTRO_ROLL_STEP) & 0x0FFF;
        // The original's exit dims the model's three viewer lights: the
        // screen fade is the port's stand-in, so it must grow from clear to
        // black as the timer counts down, never flash bright first.
        self.entry_fade = (255 - ((self.zoom_timer << 2) - 1)).clamp(0, 255) as u8;
        false
    }

    /// The examine check's result for the model's current pose.
    ///
    /// The red book's spin accumulator gates the zoom: once one spin-in has
    /// completed (`zoom_spin != 0`) a later examine opens the description
    /// straight away instead of replaying the spin. The doom books have no
    /// such gate.
    pub fn examine(&self) -> ExamineOutcome {
        match examine_check(self.item, self.yaw, self.pitch, self.roll) {
            ExamineOutcome::Zoom if self.item == items::ITEM_RED_BOOK && self.zoom_spin != 0 => {
                ExamineOutcome::Open
            }
            outcome => outcome,
        }
    }

    /// Whether the zoom spin is running.
    pub fn zooming(&self) -> bool {
        self.stage == ViewStage::Zoom
    }

    /// The fixed viewer camera.
    ///
    /// The projection centre is the menu's `(112, 76)`, not the screen
    /// centre: the model rests at the viewport box's centre rather than at
    /// `(160, 120)`.
    pub fn camera(&self) -> Camera {
        let mut camera = Camera::from_points(CAMERA_FROM, CAMERA_TO, VIEWER_FOV);
        // The projection adds the offset to the screen centre (160, 120).
        camera.screen = [MENU_CENTER[0] - 160, MENU_CENTER[1] - 120];
        camera
    }

    /// Open the description window on the item's own line. Refused while one
    /// is already up, and a no-op when the item has no description.
    pub fn start_description(&mut self) {
        if self.message.active {
            return;
        }
        if let Some(description) = &self.description {
            self.message.start(self.item, 0, description, true);
        }
    }
}

/// Advance one 12-bit angle by one frame of spin input; the angle wraps at a
/// full turn.
pub fn spin(angle: i32, positive: bool) -> i32 {
    (angle + if positive { ROTATE_STEP } else { -ROTATE_STEP }) & 0x0FFF
}

/// Whether `angle` falls inside the `(target, tolerance)` exam window,
/// transcribed from the item examine check: the target is added to the angle,
/// the tolerance subtracted, and the result must land inside `2 * target`.
/// A zero target always passes.
pub fn window_matches(angle: i32, target: u16, tolerance: u16) -> bool {
    if target == 0 {
        return true;
    }
    let shifted = (angle + i32::from(target)) & 0x0FFF;
    ((shifted - i32::from(tolerance)) & 0x0FFF) <= i32::from(target) * 2
}

/// Run the item's examine check against the model's yaw/pitch/roll angles.
///
/// The lookup record's name-class byte is the flag index into the examine-type
/// table; items with its top bit set (the port has no weapon-ammo examine
/// messages) and items whose type byte has no records open immediately, as do
/// the ids past the item table (the PC machineguns keep their descriptions).
/// A pose inside one of the type byte's records opens the description, except
/// the books (red book 0x3E and the two doom books 0x3F/0x40), which take the
/// zoom path.
pub fn examine_check(item: u8, yaw: i32, pitch: i32, roll: i32) -> ExamineOutcome {
    let Some(record) = items::record(item) else {
        return ExamineOutcome::Open;
    };
    if record.name_class & 0x80 != 0 {
        return ExamineOutcome::Open;
    }
    let Some(ex_type) = items::examine_type(record.name_class) else {
        return ExamineOutcome::Open;
    };
    if ex_type & 0xF0 == 0 {
        return ExamineOutcome::Open;
    }
    let count = usize::from(ex_type >> 4);
    let first = usize::from(ex_type & 0x0F);
    // The record pairs are the model's x/y/z Euler angles: the pitch (x
    // rotation, `DAT_00ae9f64`), the yaw (`DAT_00ae9f66`) and the roll
    // (`DAT_00ae9f68`) the viewer's recompute extracts from its matrix.
    let angles = [pitch, yaw, roll];
    for offset in 0..count {
        let Some(combo) = items::examine_combo(first + offset) else {
            continue;
        };
        let matched = combo
            .iter()
            .zip(angles)
            .all(|((target, tolerance), angle)| window_matches(angle, *target, *tolerance));
        if matched {
            if item == items::ITEM_RED_BOOK
                || (items::ITEM_RED_BOOK < item && item < items::ITEM_FIRST_AID_SPRAY)
            {
                return ExamineOutcome::Zoom;
            }
            return ExamineOutcome::Open;
        }
    }
    ExamineOutcome::Refused
}

/// The item's description-table entry: descriptions are item id - 1, except
/// the two PC-only items, which have entries past the item table.
fn description_bytes(text: &Text, item: u8) -> Option<&[u8]> {
    let id = match item {
        items::ITEM_INGRAM => INGRAM_DESCRIPTION,
        items::ITEM_MINIMI => MINIMI_DESCRIPTION,
        _ => u16::from(item),
    };
    text.description(id)
}

/// Parse the item's `.ivm` from the pack, warning and returning `None` when
/// the model name is the placeholder or the file is missing or malformed.
fn load_model(pack: &Pack, item: u8) -> Option<Ivm> {
    let name = items::model_name(item)?;
    let entry = format!("{ITEM_MODEL_PREFIX}{}.ivm", name.to_ascii_lowercase());
    match pack.read(&entry) {
        Ok(data) => match ivm::parse(data) {
            Ok(model) => Some(model),
            Err(err) => {
                eprintln!("warning: invalid {entry}: {err:#}");
                None
            }
        },
        Err(err) => {
            eprintln!("warning: missing {entry}: {err:#}");
            None
        }
    }
}

impl Screen for ItemViewScreen {
    fn open(&mut self, cx: &mut UiContext<'_>) -> Result<()> {
        let empty = Text::default();
        self.open_with(cx.pack, cx.text.unwrap_or(&empty), &[0; 4]);
        self.zoom_spin = 0;
        if self.mode == ViewMode::Examine {
            self.yaw = 0;
            self.pitch = 0;
            self.roll = 0;
            self.model_x = MODEL_REST_X;
            self.stage = ViewStage::Display;
            self.zoom_timer = 0;
            self.entry_fade = 0;
        } else {
            self.restart_intro();
        }
        Ok(())
    }

    fn update(&mut self, cx: &UiContext<'_>, input: UiInput) -> ScreenResult {
        // The take/got-item flows are driven by the engine, which steps the
        // model intro/exit itself so it can show the game's message window
        // between them; the viewer never reads the pad in these modes.
        if self.mode != ViewMode::Examine {
            return ScreenResult::Continue;
        }

        // The description window owns the input while it is up: confirm
        // advances or dismisses it, and the model holds its pose.
        if self.message.active {
            let empty = Text::default();
            self.message.update(
                MessageInput {
                    action: input.confirm,
                    left: input.left,
                    right: input.right,
                },
                cx.text.unwrap_or(&empty),
                self.item,
            );
            return ScreenResult::Continue;
        }

        // The zoom spin runs to its target before the description opens; the
        // original ignores the pad for its duration.
        if self.stage == ViewStage::Zoom {
            self.zoom_timer += 1;
            let step = (self.zoom_timer * 4).min(0x20);
            self.zoom_spin += step;
            self.yaw = (self.yaw + step) & 0x0FFF;
            self.pitch = (self.pitch - step * 2) & 0x0FFF;
            self.entry_fade = self.entry_fade.saturating_sub(12);
            if self.zoom_spin > 0x300 {
                self.stage = ViewStage::Display;
                self.entry_fade = 0;
                self.start_description();
            }
            return ScreenResult::Continue;
        }

        if input.cancel {
            cx.play_cue(UiCue::Cancel);
            return ScreenResult::Done(ScreenAction::Resume);
        }
        if input.confirm {
            cx.play_cue(UiCue::Decide);
            match self.examine() {
                ExamineOutcome::Open => self.start_description(),
                ExamineOutcome::Zoom => {
                    self.stage = ViewStage::Zoom;
                    self.zoom_timer = 0;
                    self.zoom_spin = 0;
                    self.entry_fade = 255;
                }
                ExamineOutcome::Refused => {}
            }
            return ScreenResult::Continue;
        }

        // A direction held spins the model one step per tick, like the
        // original's raw-held spin vector; the edge flags cover taps shorter
        // than a tick.
        if input.left || input.held_left {
            self.yaw = spin(self.yaw, false);
        }
        if input.right || input.held_right {
            self.yaw = spin(self.yaw, true);
        }
        if input.up || input.held_up {
            self.pitch = spin(self.pitch, false);
        }
        if input.down || input.held_down {
            self.pitch = spin(self.pitch, true);
        }
        ScreenResult::Continue
    }

    fn draw(&mut self, cx: &UiContext<'_>, framebuffer: &mut Framebuffer) {
        if let Some(model) = &self.model {
            let joints = vec![self.root_matrix(); model.objects.len()];
            framebuffer.draw_ivm_unlit(model, &joints, &self.camera());
        }
        let Some(font) = cx.font else {
            return;
        };
        if self.message.active {
            let empty = Text::default();
            self.message
                .draw(framebuffer, font, cx.text.unwrap_or(&empty));
        } else if self.mode == ViewMode::Examine && !self.name.is_empty() {
            // The take/got-item flows name the item in their message instead.
            font.draw_text(
                framebuffer,
                font.metrics.left_margin,
                layout::ITEM_NAME_Y,
                Tint::White,
                0,
                &self.name,
            );
        }
    }

    fn fade(&self) -> u8 {
        self.entry_fade
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::ivm::{IvmObject, IvmPrim, IvmPrimKind};
    use crate::model::{PALETTE_ROW_LEN, Texture8};
    use crate::pack::PackWriter;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("arklay-view-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An empty pack: the viewer's text and model layers stay empty.
    fn empty_pack(dir: &TempDir) -> Pack {
        let path = dir.0.join("game.akpak");
        PackWriter::new().write(&path).unwrap();
        Pack::open(&path).unwrap()
    }

    /// A 256x256 all-white texture and one triangle spanning the view axis
    /// plane, so the viewer camera sees it face-on.
    fn synthetic_model() -> Ivm {
        let mut palettes = vec![[0u8, 0, 0, 0]; PALETTE_ROW_LEN];
        palettes[1] = [255, 255, 255, 255];
        Ivm {
            texture: Texture8 {
                width: 256,
                height: 256,
                indices: vec![1; 256 * 256],
                palettes,
                stp: Vec::new(),
            },
            objects: vec![IvmObject {
                vertices: vec![[0, -1000, -1000], [0, -1000, 1000], [0, 1000, 0]],
                normals: vec![[4096, 0, 0]],
                prims: vec![IvmPrim {
                    kind: IvmPrimKind::TexturedGouraud,
                    vertex_count: 3,
                    vertices: [0, 1, 2, 0],
                    normals: [0, 0, 0, 0],
                    uv: [[0, 0], [255, 0], [0, 255], [0, 0]],
                    colors: [[255, 255, 255]; 3],
                    clut: 0,
                    tsb: 0,
                    blend: false,
                }],
            }],
        }
    }

    fn painted(framebuffer: &Framebuffer) -> usize {
        framebuffer
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[3] != 0)
            .count()
    }

    fn context<'a>(pack: &'a Pack, text: &'a Text) -> UiContext<'a> {
        UiContext {
            pack,
            save_dir: Path::new("."),
            font: None,
            text: Some(text),
            ticks: 0,
            cues: Default::default(),
        }
    }

    #[test]
    fn the_viewer_camera_centres_the_model_in_the_menu_viewport() {
        let screen = ItemViewScreen::new(0x01);
        // The resting model's origin projects to the menu's projection centre
        // (the original's SetSubpixelOffset(112, 76)), not the screen centre.
        assert_eq!(
            screen.camera().project([MODEL_REST_X, 0, 0]),
            Some(MENU_CENTER)
        );
    }

    #[test]
    fn synthetic_ivm_renders_non_empty_and_stable() {
        let model = synthetic_model();
        let camera = Camera::from_points(CAMERA_FROM, CAMERA_TO, VIEWER_FOV);
        let joints = [Mat4x3 {
            r: anim::rotation_matrix(0, 0, 0),
            t: [MODEL_REST_X, 0, 0],
        }];

        let mut first = Framebuffer::new();
        first.draw_ivm_unlit(&model, &joints, &camera);
        let first_painted = painted(&first);
        assert!(
            first_painted > 500,
            "the synthetic model painted only {first_painted} pixels"
        );

        let mut second = Framebuffer::new();
        second.draw_ivm_unlit(&model, &joints, &camera);
        assert_eq!(first.rgba, second.rgba, "two synthetic renders differ");

        // A quarter turn moves the model; the frame must change.
        let rotated = [Mat4x3 {
            r: anim::rotation_matrix(0, 0x400, 0),
            t: [MODEL_REST_X, 0, 0],
        }];
        let mut turned = Framebuffer::new();
        turned.draw_ivm_unlit(&model, &rotated, &camera);
        assert_ne!(first.rgba, turned.rgba, "the turntable angle is ignored");
    }

    #[test]
    fn spin_advances_at_the_original_step_and_wraps() {
        assert_eq!(spin(0, true), 0x20);
        assert_eq!(spin(0, false), 0x0FE0);
        assert_eq!(spin(0x0FF0, true), 0x10);
        assert_eq!(spin(0x10, false), 0x0FF0);
        assert_eq!(spin(0x0FFF, true), 0x1F);
    }

    #[test]
    fn examine_windows_map_the_combo_records() {
        // Crank (0x1D): one record, yaw centred on 0x12C with a 0x0C00 window.
        assert_eq!(examine_check(0x1D, 0x0BF0, 0, 0), ExamineOutcome::Open);
        assert_eq!(examine_check(0x1D, 0, 0, 0), ExamineOutcome::Refused);
        // Sword key (0x33): record 1 wants yaw/pitch within +-0x230 and the
        // roll window at +-0x5D0..0xA30.
        assert_eq!(
            examine_check(0x33, 0x100, 0x0F00, 0x800),
            ExamineOutcome::Open
        );
        assert_eq!(
            examine_check(0x33, 0x100, 0x0F00, 0),
            ExamineOutcome::Refused
        );
        // Lab key A (0x37) has no records: it opens at any pose, like every
        // item whose record has the top name bit set (the knife).
        assert_eq!(
            examine_check(0x37, 0x123, 0x456, 0x789),
            ExamineOutcome::Open
        );
        assert_eq!(examine_check(0x01, 0, 0, 0), ExamineOutcome::Open);
        // The red book's record 3 matches only around yaw 0x270..0x550.
        assert_eq!(examine_check(0x3E, 0x400, 0, 0), ExamineOutcome::Zoom);
        assert_eq!(examine_check(0x3E, 0, 0, 0), ExamineOutcome::Refused);
        // The PC machineguns keep their descriptions.
        assert_eq!(examine_check(0x6F, 0, 0, 0), ExamineOutcome::Open);
    }

    #[test]
    fn red_book_skips_the_zoom_once_the_spin_accumulator_has_run() {
        let mut screen = ItemViewScreen::new(items::ITEM_RED_BOOK);
        screen.yaw = 0x400;
        assert_eq!(screen.examine(), ExamineOutcome::Zoom);

        // A finished spin-in parks the accumulator above its 0x300 target;
        // the same pose then opens the description instead of replaying the
        // zoom. The doom books have no such gate.
        screen.zoom_spin = 0x301;
        assert_eq!(screen.examine(), ExamineOutcome::Open);

        let mut doom = ItemViewScreen::new(items::ITEM_RED_BOOK + 1);
        doom.yaw = 0x400;
        assert_eq!(doom.examine(), ExamineOutcome::Zoom);
        doom.zoom_spin = 0x301;
        assert_eq!(doom.examine(), ExamineOutcome::Zoom);
    }

    #[test]
    fn held_directions_spin_the_model_every_tick() {
        let dir = TempDir::new();
        let pack = empty_pack(&dir);
        let text = Text::default();
        let cx = context(&pack, &text);
        let mut screen = ItemViewScreen::new(1);
        for _ in 0..3 {
            screen.update(
                &cx,
                UiInput {
                    held_right: true,
                    ..UiInput::default()
                },
            );
        }
        assert_eq!(screen.yaw, 3 * ROTATE_STEP);
        screen.update(
            &cx,
            UiInput {
                held_up: true,
                ..UiInput::default()
            },
        );
        assert_eq!(screen.pitch, 0x0FE0);
    }

    #[test]
    fn misaligned_confirm_refuses_and_aligned_opens() {
        let dir = TempDir::new();
        let pack = empty_pack(&dir);
        let text = Text::default();
        let cx = context(&pack, &text);

        // The crank's window needs a turned pose: the resting pose refuses.
        let mut screen = ItemViewScreen::new(0x1D);
        screen.description = Some(vec![0x0C, 0x01, 0x00]);
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..UiInput::default()
            },
        );
        assert!(!screen.message_active(), "the misaligned pose is refused");

        screen.yaw = 0x0BF0;
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..UiInput::default()
            },
        );
        assert!(screen.message_active(), "the aligned pose opens");
    }

    #[test]
    fn the_red_book_zooms_before_the_description() {
        let dir = TempDir::new();
        let pack = empty_pack(&dir);
        let text = Text::default();
        let cx = context(&pack, &text);

        let mut screen = ItemViewScreen::new(0x3E);
        screen.description = Some(vec![0x0C, 0x01, 0x00]);
        screen.yaw = 0x400;
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..UiInput::default()
            },
        );
        assert!(screen.zooming(), "the matched red book takes the zoom path");
        assert!(!screen.message_active());

        let mut ticks = 0;
        while screen.zooming() && ticks < 200 {
            screen.update(&cx, UiInput::default());
            ticks += 1;
        }
        assert!(!screen.zooming(), "the zoom finishes");
        assert!(screen.message_active(), "the zoom opens the description");
        assert_eq!(screen.entry_fade, 0, "the entry ramp reaches full");
    }

    #[test]
    fn turntable_input_rotates_and_cancel_resumes() {
        let dir = TempDir::new();
        let pack = empty_pack(&dir);
        let text = Text::default();
        let cx = context(&pack, &text);

        let mut screen = ItemViewScreen::new(1);
        assert_eq!(screen.yaw, 0);
        assert_eq!(
            screen.update(
                &cx,
                UiInput {
                    right: true,
                    ..UiInput::default()
                },
            ),
            ScreenResult::Continue
        );
        assert_eq!(screen.yaw, 0x20, "right turns the turntable one step");
        screen.update(
            &cx,
            UiInput {
                up: true,
                ..UiInput::default()
            },
        );
        assert_eq!(screen.pitch, 0x0FE0, "up pitches the model one step back");
        assert_eq!(
            screen.update(
                &cx,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
            ),
            ScreenResult::Done(ScreenAction::Resume)
        );
    }

    #[test]
    fn confirm_opens_the_description_window_and_confirm_dismisses_it() {
        let dir = TempDir::new();
        let pack = empty_pack(&dir);
        let text = Text::default();
        let cx = context(&pack, &text);

        let mut screen = ItemViewScreen::new(1);
        screen.description = Some(vec![0x0C, 0x01, 0x00]);
        assert_eq!(
            screen.update(
                &cx,
                UiInput {
                    confirm: true,
                    ..UiInput::default()
                },
            ),
            ScreenResult::Continue
        );
        assert!(screen.message_active(), "confirm opened the description");

        // Reveal the one-glyph stream, then dismiss it with another confirm.
        for _ in 0..4 {
            screen.update(&cx, UiInput::default());
        }
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..UiInput::default()
            },
        );
        assert!(!screen.message_active(), "confirm dismissed the window");
    }

    #[test]
    fn the_take_intro_settles_on_the_resting_pose_and_the_exit_mirrors_it() {
        let mut screen = ItemViewScreen::new_take(1);
        assert_eq!(screen.mode(), ViewMode::Take);
        assert!(screen.in_intro());
        assert_eq!(screen.fade(), 255, "the intro starts black");

        let mut frames = 0;
        while screen.in_intro() {
            screen.step_intro();
            frames += 1;
            assert!(frames <= INTRO_FRAMES + 1, "the intro never finished");
        }
        assert_eq!(frames, INTRO_FRAMES, "the intro is 0x40 frames");
        assert_eq!(
            screen.root_matrix().t[0],
            MODEL_REST_X,
            "the model lands at its resting translation"
        );
        assert_eq!(screen.yaw, 0, "three whole turns land on the identity");
        assert_eq!(screen.fade(), 0, "the intro reaches full brightness");

        // The exit is the intro in reverse and reports completion. Its fade
        // grows from clear to black, never flashing bright first.
        screen.begin_exit();
        assert!(screen.in_exit());
        let mut frames = 0;
        let mut fade = 0;
        while !screen.step_exit() {
            assert!(
                screen.entry_fade >= fade,
                "the exit fade went backwards: {} after {fade}",
                screen.entry_fade
            );
            fade = screen.entry_fade;
            frames += 1;
            assert!(frames <= INTRO_FRAMES + 1, "the exit never finished");
        }
        assert_eq!(fade, 252, "the last step before the close is nearly black");
        assert_eq!(screen.entry_fade, 255, "the exit reaches full black");
        assert_eq!(frames, INTRO_FRAMES - 1);

        // A got-item viewer opens the same intro but takes no pad input.
        let dir = TempDir::new();
        let pack = empty_pack(&dir);
        let text = Text::default();
        let cx = context(&pack, &text);
        let mut screen = ItemViewScreen::new_got_item(1);
        assert_eq!(screen.mode(), ViewMode::GotItem);
        assert_eq!(
            screen.update(
                &cx,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
            ),
            ScreenResult::Continue,
            "the engine, not the pad, closes a pickup viewer"
        );
        assert!(screen.in_intro());
    }

    #[test]
    fn missing_models_and_descriptions_stay_empty() {
        let dir = TempDir::new();
        let pack = empty_pack(&dir);

        // Item 0x6E maps to the placeholder model name "m" and a description
        // index past the table: both layers stay empty and drawing does not
        // panic.
        let mut screen = ItemViewScreen::new(0x6E);
        screen.open_with(&pack, &Text::default(), &[0; 4]);
        assert!(screen.model().is_none());
        assert!(screen.description().is_none());

        let text = Text::default();
        let cx = context(&pack, &text);
        let mut framebuffer = Framebuffer::new();
        screen.draw(&cx, &mut framebuffer);
        assert_eq!(painted(&framebuffer), 0);
    }

    #[test]
    fn ingram_and_minimi_descriptions_use_their_own_entries() {
        // A table where entries 0x4D and 0x4E (zero-based; the 1-based
        // lookups 0x4E/0x4F) are present but the direct item-id entries are
        // not.
        let dir = TempDir::new();
        let path = dir.0.join("game.akpak");
        let mut descriptions: Vec<Option<Vec<u8>>> = (0..79).map(|_| None).collect();
        descriptions[0x4D] = Some(vec![0x0C, 0x01, 0x00]);
        descriptions[0x4E] = Some(vec![0x0D, 0x01, 0x00]);
        let mut writer = PackWriter::new();
        writer
            .add(
                crate::text::DESCRIPTIONS_ENTRY,
                crate::text::encode_table(&descriptions),
            )
            .unwrap();
        writer.write(&path).unwrap();
        let pack = Pack::open(&path).unwrap();
        let text = Text::load(&pack);

        assert_eq!(
            description_bytes(&text, items::ITEM_INGRAM),
            Some([0x0C, 0x01, 0x00].as_slice())
        );
        assert_eq!(
            description_bytes(&text, items::ITEM_MINIMI),
            Some([0x0D, 0x01, 0x00].as_slice())
        );
        assert_eq!(description_bytes(&text, 0x6E), None);
    }
}
