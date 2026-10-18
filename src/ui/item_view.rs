//! The item viewer (examine screen).
//!
//! [`ItemViewScreen`] owns everything it draws: the selected item's `.ivm`
//! model and texture, the rotation arrows and the item name. It is the
//! original's `FUN_0044e1b0` item model viewer, shared by three flows:
//!
//! - [`ViewMode::Examine`], the pause menu's CHECK action. The engine keeps it
//!   on the session next to the live menu and drives it one frame at a time
//!   with [`ItemViewScreen::step_examine`]; the examine messages and the item
//!   description go through the game's message window like every other menu
//!   message.
//! - [`ViewMode::Take`]/[`ViewMode::GotItem`], the pick-up flows. Those start on
//!   the same spin-in intro ([`ItemViewScreen::step_intro`]) and the engine
//!   hands them the closing animation ([`ItemViewScreen::begin_exit`]) once
//!   its message resolves, so they never read the pad themselves.
//!
//! The model renders through [`Framebuffer::draw_ivm`] with the viewer's three
//! fixed lights over a dim ambient. The intro ramps the lights up from black
//! and the exit ramps them back down; the screen itself never fades.
//!
//! # Input
//!
//! While the model rests, left/right turn it about the screen's vertical axis
//! and up/down tip it toward or away from the camera, `0x20` 12-bit units per
//! 30 Hz frame while the key is held. The original builds a small rotation
//! from the held keys and pre-multiplies it onto the model's rotation matrix,
//! then re-extracts the Euler angles the examine check reads
//! (`apply_spin`). Confirm runs the item's examine check and
//! cancel starts the closing animation. A missing model (the placeholder
//! name `m`, or a pack without `item_m2` art) logs a warning and leaves that
//! layer empty instead of failing the screen.

use anyhow::Result;

use crate::anim::{self, Mat4x3};
use crate::assets::Artwork;
use crate::font::{Font, Tint};
use crate::items;
use crate::ivm::{self, Ivm};
use crate::pack::Pack;
use crate::render::{Camera, EntityLight, EntityLightSource, Framebuffer};
use crate::text::Text;

use super::layout;
use super::main_menu::item_name_bytes;
use super::{Screen, ScreenResult, UiContext, UiInput};

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
/// The viewer's ambient light, `setBackColor(0x199, 0x199, 0x199)` in the
/// rasterizer's `0..=255` scale.
const VIEWER_AMBIENT: f64 = ((0x199 * 255) / 4096) as f64;
/// The viewer's three light positions (`main_menu`'s `DAT_00d22700` records):
/// two from the upper left and right, one from below.
const VIEWER_LIGHTS: [[i32; 3]; 3] = [[100, 0x50, 0x50], [-100, 0x50, 0x50], [0, -100, 0x50]];
/// The examine spin accumulator's target: the book zoom opens its message
/// once object 1's yaw passes it.
const ZOOM_TARGET: i32 = 0x300;
/// The status-sheet arrows `FUN_0044eca0` draws round the model, in its draw
/// order: left, down, right, up, as screen position, sheet `v` and the held
/// direction that lights it.
const ARROWS: [([i32; 2], i32, Direction); 4] = [
    ([0x22, 0x49], 0x7C, Direction::Left),
    ([0x6D, 0x76], 0x70, Direction::Down),
    ([0xB6, 0x49], 0x64, Direction::Right),
    ([0x6D, 0x1A], 0x58, Direction::Up),
];
/// Sheet `u` of a lit (held) arrow; an idle arrow is the next cell over.
const ARROW_LIT_U: i32 = 0x80;
/// Sheet `u` of an idle arrow.
const ARROW_IDLE_U: i32 = 0x8C;
/// The arrow sprites' size.
const ARROW_SIZE: i32 = 9;
/// Item id of the first doom book; the second follows it.
const ITEM_DOOM_BOOK_2: u8 = 0x3F;
/// The medal the first doom book turns into; the eagle medal follows it.
const ITEM_WOLF_MEDAL: u8 = 0x24;
/// Bit of the examined bank the red book's read zoom sets.
const RED_BOOK_READ_BIT: u8 = 0x0D;
/// Global message: "<ammo> loaded." for a weapon holding rounds.
const MESSAGE_WEAPON_LOADED: u8 = 0xF0;
/// Global message for item 0x13 once its scenario flags are up.
const MESSAGE_ITEM13: u8 = 0xF1;

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

/// The viewer's state, the original's `DAT_00ae9f49`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewStage {
    /// State 0: the setup runs at the start of the next frame and falls into
    /// the intro (the doom-book medal swap parks the viewer here).
    Setup,
    /// State 1: the model spins in from far away while the lights ramp up.
    Intro,
    /// State 2: the resting display, running the [`ViewAction`] table.
    Display,
    /// State 5: the closing animation, the intro in reverse.
    Exit,
    /// The exit has finished; nothing is drawn.
    Closed,
}

/// The display state's action, the original's `g_bItemViewerActionIndex`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewAction {
    /// Action 0: turntable input and the examine confirm.
    Turn,
    /// Action 1: the book zoom spin.
    Zoom,
    /// Action 2: an examine message or the description is up.
    Message,
    /// Action 3: the doom book closes out before turning into a medal.
    Medal,
}

/// One of the four held rotation directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Right,
    Down,
    Left,
}

/// The pad as the examine viewer reads it on one frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExamineInput {
    /// Confirm went down this frame.
    pub confirm: bool,
    /// Cancel went down this frame.
    pub cancel: bool,
    /// The held direction keys.
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    /// Whether the game's message window is up (`g_menu_choice_id & 0x80`).
    pub message_active: bool,
}

/// What the examine viewer asks the engine to do after a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExamineEvent {
    /// Nothing.
    None,
    /// Confirm on the resting model: run [`ItemViewScreen::confirm`] with the
    /// game state and apply its [`ConfirmOutcome`].
    Confirm,
    /// The book zoom finished: show global message `id`.
    Message(u8),
    /// The red book's read zoom is acknowledged: set examined bit `0x0D`.
    MarkExaminedBit(u8),
    /// A doom book turned into medal `item`: rewrite the cursor slot to it
    /// with quantity 1 and reload the viewer with
    /// [`ItemViewScreen::open_with`].
    Medal(u8),
    /// The closing animation finished; drop the viewer.
    Closed,
}

/// The game state the examine check reads beyond the model's pose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExamineContext {
    /// The quantity in the examined item's inventory slot.
    pub slot_quantity: u8,
    /// Whether scenario flag `0x13` or `0x00` is set (item 0x13's message).
    pub item13_message: bool,
}

/// What the examine check decided (`FUN_0044ed40`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// The item cannot be examined: show its plain description.
    Description,
    /// A book whose pose matched: the zoom spin runs first.
    Zoom,
    /// Show global message `id`. `mark_examined` marks the item's
    /// examined-bank class; `select` is the item the message's `\i` names.
    Message {
        id: u8,
        mark_examined: bool,
        select: Option<u8>,
    },
}

/// The item viewer's screen state.
#[derive(Debug, Clone)]
pub struct ItemViewScreen {
    /// The examined item id.
    item: u8,
    /// Which flow opened the viewer.
    mode: ViewMode,
    /// The item's encoded description, resolved at open.
    description: Option<Vec<u8>>,
    /// The parsed model; `None` when the item has no shipped file.
    model: Option<Ivm>,
    /// The model's base rotation, the original's `DAT_00ae9f64/66/68`: x
    /// (pitch), y (yaw, the turntable axis the intro spins) and z (roll), in
    /// 12-bit angle units.
    pub pitch: i32,
    pub yaw: i32,
    pub roll: i32,
    /// The model's translation toward the camera; the intro/exit animation
    /// drives it between [`INTRO_START_X`] and [`MODEL_REST_X`].
    model_x: i32,
    /// The viewer state.
    stage: ViewStage,
    /// The display state's action.
    action: ViewAction,
    /// The intro/exit/zoom frame counter (`g_bItemViewerZoomTimer`).
    timer: i32,
    /// The two per-object rotations (`DAT_00ae9f54`/`DAT_00ae9f5c`). The book
    /// zoom spins object 1's yaw (`DAT_00ae9f5e`, the spin accumulator the red
    /// book's re-examine reads) and counter-spins object 0's.
    obj_rot: [[i32; 3]; 2],
    /// Object 0's local matrix angles as the render left them (`Sca[0]`,
    /// `None` for the setup's identity). The original rebuilds it only after
    /// drawing object 0, so object 0 always shows the previous frame's angles.
    obj0_sca: Option<[i32; 3]>,
    /// The object-0 angles this frame's draw composes.
    obj0_draw: Option<[i32; 3]>,
    /// The three lights' colour byte (`0x80` is full scale).
    light: u8,
    /// The held directions the rotation arrows show, latched on the turntable
    /// frames that draw them.
    held: [bool; 4],
    /// Whether this frame draws the rotation arrows.
    arrows: bool,
    /// Whether this frame draws the item name.
    show_name: bool,
}

impl ItemViewScreen {
    /// A viewer for `item`, still without any loaded art. It opens on its
    /// setup frame: the first [`ItemViewScreen::step_examine`] starts the
    /// intro.
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
            description: None,
            model: None,
            pitch: 0,
            yaw: 0,
            roll: 0,
            model_x: MODEL_REST_X,
            stage: ViewStage::Setup,
            action: ViewAction::Turn,
            timer: 0,
            obj_rot: [[0; 3]; 2],
            obj0_sca: None,
            obj0_draw: None,
            light: 0,
            held: [false; 4],
            arrows: false,
            show_name: false,
        };
        if mode != ViewMode::Examine {
            screen.restart_intro();
        }
        screen
    }

    /// The setup state (state 0): park the model far away on the identity
    /// pose and arm the `0x40`-frame intro.
    fn restart_intro(&mut self) {
        self.yaw = 0;
        self.pitch = 0;
        self.roll = 0;
        self.obj_rot = [[0; 3]; 2];
        self.obj0_sca = None;
        self.model_x = INTRO_START_X;
        self.stage = ViewStage::Intro;
        self.timer = INTRO_FRAMES;
    }

    /// Load the item's model and description from `pack` and `text`.
    ///
    /// Every failure is a warning: a missing `item/{name}.ivm` leaves
    /// [`ItemViewScreen::model`] empty and the screen still shows the text
    /// layers, which is how ING/MINI items and stale packs stay usable.
    pub fn open_with(&mut self, pack: &Pack, text: &Text) {
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

    /// The item's encoded description bytes, when the table has an entry.
    pub fn description(&self) -> Option<&[u8]> {
        self.description.as_deref()
    }

    /// Which flow opened the viewer.
    pub fn mode(&self) -> ViewMode {
        self.mode
    }

    /// The lights' current colour byte.
    pub fn light(&self) -> u8 {
        self.light
    }

    /// Whether the viewer is on its resting display with the turntable
    /// action (the examine confirm and cancel are live).
    pub fn turntable_ready(&self) -> bool {
        self.stage == ViewStage::Display && self.action == ViewAction::Turn
    }

    /// The root matrix: the base rotation and the model's current
    /// translation toward the camera.
    pub fn root_matrix(&self) -> Mat4x3 {
        Mat4x3 {
            r: anim::rotation_matrix(self.pitch, self.yaw, self.roll),
            t: [self.model_x, 0, 0],
        }
    }

    /// The world matrix of each model object (`FUN_0044ea50`): object 0 is
    /// `root * R(obj1) * R(obj0, last frame)`, object 1 `root * R(obj1)`; a
    /// one-object model uses the first form with the identity for object 0.
    fn object_matrices(&self) -> Vec<Mat4x3> {
        let root = self.root_matrix();
        let rot = |angles: [i32; 3]| Mat4x3 {
            r: anim::rotation_matrix(angles[0], angles[1], angles[2]),
            t: [0; 3],
        };
        let identity = Mat4x3 {
            r: [[0x1000, 0, 0], [0, 0x1000, 0], [0, 0, 0x1000]],
            t: [0; 3],
        };
        let count = self.model.as_ref().map_or(1, |model| model.objects.len());
        let spin = anim::compose(&root, &rot(self.obj_rot[1]));
        let lagged = match self.obj0_draw {
            Some(angles) if count >= 2 => rot(angles),
            _ => identity,
        };
        let mut matrices = vec![anim::compose(&spin, &lagged)];
        if count >= 2 {
            matrices.push(spin);
        }
        // Any further object (none ship) rides the root.
        while matrices.len() < count {
            matrices.push(root);
        }
        matrices
    }

    /// Whether the take/got-item opening animation is still running.
    pub fn in_intro(&self) -> bool {
        matches!(self.stage, ViewStage::Setup | ViewStage::Intro)
    }

    /// Advance the opening animation one frame; returns `true` when it has
    /// finished and the model has settled on its resting pose.
    ///
    /// The original's `FUN_0044e1b0` state 1: `0x40` frames of translation
    /// toward the camera, yaw and roll, with the lights ramping up as
    /// `0xFF - (timer << 2)`.
    pub fn step_intro(&mut self) -> bool {
        if self.stage == ViewStage::Setup {
            self.restart_intro();
        }
        if self.stage != ViewStage::Intro {
            return true;
        }
        self.model_x += INTRO_ZOOM_STEP;
        self.yaw += INTRO_YAW_STEP;
        self.roll += INTRO_ROLL_STEP;
        self.timer -= 1;
        self.light = (0xFF - (self.timer << 2)) as u8;
        self.mask_angles();
        self.latch_render();
        if self.timer > 0 {
            return false;
        }
        self.stage = ViewStage::Display;
        self.action = ViewAction::Turn;
        true
    }

    /// Start the closing animation: the reverse of the opener.
    pub fn begin_exit(&mut self) {
        self.stage = ViewStage::Exit;
        self.timer = INTRO_FRAMES;
    }

    /// Whether the closing animation is running.
    pub fn in_exit(&self) -> bool {
        self.stage == ViewStage::Exit
    }

    /// Advance the closing animation one frame; returns `true` when it has
    /// finished and the viewer may close.
    ///
    /// The original's exit (`FUN_0044e1b0` state 5) dims the three viewer
    /// lights as `(timer << 2) - 1`; it does not paint a screen fade. The
    /// frame the timer runs out draws nothing.
    pub fn step_exit(&mut self) -> bool {
        if self.stage == ViewStage::Closed {
            return true;
        }
        if self.stage != ViewStage::Exit {
            return true;
        }
        self.timer -= 1;
        if self.timer <= 0 {
            self.stage = ViewStage::Closed;
            return true;
        }
        self.step_out();
        self.latch_render();
        false
    }

    /// One frame of the zoom-out the exit and the medal swap share.
    fn step_out(&mut self) {
        self.model_x -= INTRO_ZOOM_STEP;
        self.yaw -= INTRO_YAW_STEP;
        self.roll -= INTRO_ROLL_STEP;
        self.light = ((self.timer << 2) - 1) as u8;
        self.mask_angles();
    }

    /// The render's `& 0xFFF` on the three base angles.
    fn mask_angles(&mut self) {
        self.pitch &= 0x0FFF;
        self.yaw &= 0x0FFF;
        self.roll &= 0x0FFF;
    }

    /// Run one examine-viewer frame, the original's `FUN_0044e1b0` with the
    /// status-screen mode (`DAT_00ae9f10 == 0`).
    pub fn step_examine(&mut self, input: ExamineInput) -> ExamineEvent {
        self.arrows = false;
        self.show_name = false;
        let event = match self.stage {
            ViewStage::Setup | ViewStage::Intro => {
                self.step_intro();
                ExamineEvent::None
            }
            ViewStage::Display => {
                let event = self.step_action(input);
                // The medal swap parks the viewer on its setup state, which
                // draws nothing until the next frame's intro.
                if self.stage != ViewStage::Setup {
                    self.latch_render();
                }
                event
            }
            ViewStage::Exit => {
                if self.step_exit() {
                    return ExamineEvent::Closed;
                }
                ExamineEvent::None
            }
            ViewStage::Closed => return ExamineEvent::Closed,
        };
        if self.action == ViewAction::Turn {
            self.show_name = true;
        }
        event
    }

    /// The display state's action table (`PTR_FUN_004bf310`).
    fn step_action(&mut self, input: ExamineInput) -> ExamineEvent {
        match self.action {
            ViewAction::Turn => {
                if input.confirm {
                    return ExamineEvent::Confirm;
                }
                if input.cancel {
                    self.stage = ViewStage::Exit;
                    self.timer = INTRO_FRAMES;
                    self.arrows = true;
                    self.held = [input.up, input.right, input.down, input.left];
                    return ExamineEvent::None;
                }
                // Left/right drive the spin's y (yaw) axis, up/down its z
                // axis; the x axis belongs to the shift-held roll the port
                // leaves out.
                let yaw = match (input.left, input.right) {
                    (true, _) => ROTATE_STEP,
                    (false, true) => -ROTATE_STEP,
                    _ => 0,
                };
                let tip = match (input.up, input.down) {
                    (true, _) => -ROTATE_STEP,
                    (false, true) => ROTATE_STEP,
                    _ => 0,
                };
                if yaw != 0 || tip != 0 {
                    self.apply_spin([0, yaw, tip]);
                }
                self.arrows = true;
                self.held = [input.up, input.right, input.down, input.left];
                ExamineEvent::None
            }
            ViewAction::Zoom => {
                self.timer += 1;
                let step = i32::from(((self.timer * 4) as u8).min(0x20));
                self.obj_rot[1][1] = (self.obj_rot[1][1] + step) & 0x0FFF;
                self.obj_rot[0][1] = (self.obj_rot[0][1] - step * 2) & 0x0FFF;
                if self.obj_rot[1][1] > ZOOM_TARGET {
                    self.action = ViewAction::Message;
                    return match class_message(self.item) {
                        Some(id) => ExamineEvent::Message(id),
                        None => ExamineEvent::None,
                    };
                }
                self.show_name = true;
                ExamineEvent::None
            }
            ViewAction::Message => {
                if input.message_active {
                    return ExamineEvent::None;
                }
                let spun = self.obj_rot[1][1] != 0;
                if self.item == items::ITEM_RED_BOOK {
                    self.action = ViewAction::Turn;
                    if spun {
                        return ExamineEvent::MarkExaminedBit(RED_BOOK_READ_BIT);
                    }
                } else if is_doom_book(self.item) && spun {
                    self.action = ViewAction::Medal;
                    self.timer = INTRO_FRAMES;
                } else {
                    self.action = ViewAction::Turn;
                }
                ExamineEvent::None
            }
            ViewAction::Medal => {
                self.timer -= 1;
                self.step_out();
                if self.timer != 0 {
                    return ExamineEvent::None;
                }
                // The book becomes its medal and the viewer restarts on it;
                // the action stays on the swap so the intro shows no name.
                self.item = self.item - ITEM_DOOM_BOOK_2 + ITEM_WOLF_MEDAL;
                self.stage = ViewStage::Setup;
                ExamineEvent::Medal(self.item)
            }
        }
    }

    /// The examine check (`FUN_0044ed40`) for the resting model's pose. A
    /// [`ConfirmOutcome::Zoom`] starts the zoom here; the engine shows the
    /// other outcomes' messages, after which the viewer waits for the window
    /// to close.
    pub fn confirm(&mut self, context: ExamineContext) -> ConfirmOutcome {
        let outcome = examine_check(
            self.item,
            [self.pitch, self.yaw, self.roll],
            self.obj_rot[1][1],
            context,
        );
        match outcome {
            ConfirmOutcome::Zoom => {
                self.action = ViewAction::Zoom;
                self.timer = 0;
                // The confirm frame names the item once more.
                self.show_name = true;
            }
            ConfirmOutcome::Description | ConfirmOutcome::Message { .. } => {
                self.action = ViewAction::Message;
                self.show_name = false;
            }
        }
        outcome
    }

    /// Pre-multiply the held spin onto the base rotation and re-extract the
    /// Euler angles (`FUN_0044eb30`).
    ///
    /// The product is written back into the base matrix in place, exactly
    /// like the reference's `MulMatrixInPlace`, so its lower rows read the
    /// already-updated first row.
    // The index loops are the point: each element must read the rows the
    // earlier iterations already overwrote.
    #[allow(clippy::needless_range_loop)]
    fn apply_spin(&mut self, spin: [i32; 3]) {
        let mut base = anim::rotation_matrix(self.pitch, self.yaw, self.roll);
        let spin = anim::rotation_matrix(spin[0], spin[1], spin[2]);
        for i in 0..3 {
            for j in 0..3 {
                let sum: i64 = (0..3).map(|k| fixed_mul12(spin[i][k], base[k][j])).sum();
                base[i][j] = i32::from(sum as i16);
            }
        }
        let m02 = i64::from(base[0][2]);
        let m12 = i64::from(base[1][2]);
        let m22 = i64::from(base[2][2]);
        let length = shift12(m02 * m02 + m22 * m22 + m12 * m12);
        let length = if length == 0 { 1 } else { length };
        let up_x = (m02 << 12) / length;
        self.pitch = angle_of(m12, m22);
        let rest = (0x1000 - shift12(up_x * up_x)).max(0);
        self.yaw = angle_of(up_x, sqrt12(rest));
        // Undo the extracted pitch/yaw on the first column; what is left of
        // it is the roll.
        let partial = anim::rotation_matrix(self.pitch, self.yaw, 0);
        let source = [
            i64::from(base[0][0]),
            i64::from(base[1][0]),
            i64::from(base[2][0]),
        ];
        let column = |row: usize| -> i64 {
            shift12((0..3).map(|k| i64::from(partial[k][row]) * source[k]).sum())
        };
        let x = column(0);
        let y = -column(1);
        self.roll = angle_of(y, x);
        self.mask_angles();
    }

    /// The render's per-object matrix build (`FUN_0044ea50`): this frame
    /// draws object 0 with the angles `Sca[0]` already holds, then `Sca[0]`
    /// takes the current ones.
    fn latch_render(&mut self) {
        self.obj0_draw = self.obj0_sca;
        self.obj0_sca = Some(self.obj_rot[0]);
    }

    /// The viewer's three lights at the current light level.
    fn lights(&self) -> EntityLight {
        let color = self.light.min(0x80);
        EntityLight {
            ambient: [VIEWER_AMBIENT; 3],
            sources: VIEWER_LIGHTS.map(|position| {
                let length = position
                    .iter()
                    .map(|&value| f64::from(value).powi(2))
                    .sum::<f64>()
                    .sqrt()
                    .max(1.0);
                EntityLightSource {
                    dir: [
                        (f64::from(position[0]) / length * 4096.0) as i32,
                        (-f64::from(position[1]) / length * 4096.0) as i32,
                        (f64::from(position[2]) / length * 4096.0) as i32,
                    ],
                    color: [color; 3],
                }
            }),
        }
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

    /// Draw the lit model, unless the viewer is between states.
    pub fn draw_model(&self, framebuffer: &mut Framebuffer) {
        if matches!(self.stage, ViewStage::Setup | ViewStage::Closed) {
            return;
        }
        if let Some(model) = &self.model {
            framebuffer.draw_ivm(
                model,
                &self.object_matrices(),
                &self.camera(),
                Some(&self.lights()),
            );
        }
    }

    /// Draw the examine viewer: the model, the rotation arrows from the
    /// menu's status sheet and the item name on the `0xBA` line.
    pub fn draw_examine(
        &self,
        framebuffer: &mut Framebuffer,
        status: Option<&Artwork>,
        font: Option<&Font>,
        text: &Text,
        examined: &[u8; 4],
    ) {
        self.draw_model(framebuffer);
        if self.arrows
            && let Some(status) = status
        {
            for (position, v, direction) in ARROWS {
                let u = if self.held[direction as usize] {
                    ARROW_LIT_U
                } else {
                    ARROW_IDLE_U
                };
                framebuffer.draw_page_sprite(
                    status,
                    [u, v, ARROW_SIZE, ARROW_SIZE],
                    [position[0], position[1], ARROW_SIZE, ARROW_SIZE],
                    0,
                    0,
                    Tint::White,
                );
            }
        }
        if self.show_name
            && let Some(font) = font
            && let Some(name) = item_name_bytes(text, self.item, examined)
        {
            font.draw_text(
                framebuffer,
                font.metrics.left_margin,
                layout::ITEM_NAME_Y,
                Tint::White,
                0,
                name,
                text.uses_tight_metrics(),
            );
        }
    }
}

/// `(a * b) >> 12` rounded toward zero, the reference's `GteFixedMul12`.
fn fixed_mul12(a: i32, b: i32) -> i64 {
    let value = i64::from(a) * i64::from(b);
    (value + ((value >> 63) & 0xFFF)) >> 12
}

/// A signed `>> 12` rounded toward zero.
fn shift12(value: i64) -> i64 {
    (value + ((value >> 63) & 0xFFF)) >> 12
}

/// `FUN_0040a990`: `atan2` in 12-bit angle units, truncated.
fn angle_of(y: i64, x: i64) -> i32 {
    let radians = (y as f64).atan2(x as f64);
    (radians * 57.29577791868204 * 11.377777777777778) as i32
}

/// `FUN_0040a530`: the 20.12 fixed-point square root.
fn sqrt12(value: i64) -> i64 {
    if value < 0 {
        return 0;
    }
    ((value as f64 * 0.000244140625).sqrt() * 4096.0) as i64
}

/// Whether `item` is one of the two doom books.
fn is_doom_book(item: u8) -> bool {
    items::ITEM_RED_BOOK < item && item < items::ITEM_FIRST_AID_SPRAY
}

/// The global message an item's examine class shows
/// (`g_ItemHealTable[class + 0x51]`).
fn class_message(item: u8) -> Option<u8> {
    let class = items::record(item)?.name_class;
    items::HEAL_TABLE.get(usize::from(class) + 0x51).copied()
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

/// The item examine check (`FUN_0044ed40`) for the model's `[pitch, yaw,
/// roll]` pose. `zoom_spin` is object 1's yaw, which gates the red book's
/// zoom once it has run.
///
/// An item whose lookup name class has its top bit clear is examinable: a
/// class with no combo records shows its class message at once (and marks the
/// class examined), and one with records needs the pose inside one of them.
/// A matching book takes the zoom instead. A top-bit item is one of the
/// ammo-loaded weapons, item 0x13 with its scenario flags, or plain.
/// Everything else shows the item's description.
pub fn examine_check(
    item: u8,
    angles: [i32; 3],
    zoom_spin: i32,
    context: ExamineContext,
) -> ConfirmOutcome {
    let Some(record) = items::record(item) else {
        return ConfirmOutcome::Description;
    };
    let class = record.name_class;
    if class & 0x80 == 0 {
        let message = ConfirmOutcome::Message {
            id: class_message(item).unwrap_or(0),
            mark_examined: true,
            select: None,
        };
        let Some(ex_type) = items::examine_type(class) else {
            return ConfirmOutcome::Description;
        };
        if ex_type & 0xF0 == 0 {
            return message;
        }
        let count = usize::from(ex_type >> 4);
        let first = usize::from(ex_type & 0x0F);
        let matched = (0..count).any(|offset| {
            items::examine_combo(first + offset).is_some_and(|combo| {
                combo
                    .iter()
                    .zip(angles)
                    .all(|((target, tolerance), angle)| window_matches(angle, *target, *tolerance))
            })
        });
        if !matched {
            return ConfirmOutcome::Description;
        }
        if (item == items::ITEM_RED_BOOK && zoom_spin == 0) || is_doom_book(item) {
            return ConfirmOutcome::Zoom;
        }
        return message;
    }
    match item {
        // The Colt Pythons and the bazookas name the rounds they hold.
        4 | 5 | 7 | 8 | 9 if context.slot_quantity != 0 => ConfirmOutcome::Message {
            id: MESSAGE_WEAPON_LOADED,
            mark_examined: false,
            select: Some(crate::weapons::weapon_ammo_item_id(item)),
        },
        0x13 if context.item13_message => ConfirmOutcome::Message {
            id: MESSAGE_ITEM13,
            mark_examined: false,
            select: None,
        },
        _ => ConfirmOutcome::Description,
    }
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
        Ok(data) => match ivm::parse(&data) {
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

/// The pick-up flows draw the viewer through the [`Screen`] hook; the engine
/// steps their animation itself, so the viewer never reads the pad.
impl Screen for ItemViewScreen {
    fn open(&mut self, cx: &mut UiContext<'_>) -> Result<()> {
        let empty = Text::default();
        self.open_with(cx.pack, cx.text.unwrap_or(&empty));
        Ok(())
    }

    fn update(&mut self, _cx: &UiContext<'_>, _input: UiInput) -> ScreenResult {
        ScreenResult::Continue
    }

    fn draw(&mut self, _cx: &UiContext<'_>, framebuffer: &mut Framebuffer) {
        self.draw_model(framebuffer);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

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

    /// Runs the viewer with no input until the turntable is up.
    fn settle(screen: &mut ItemViewScreen) -> u32 {
        for frame in 1..=0x100 {
            screen.step_examine(ExamineInput::default());
            if screen.turntable_ready() {
                return frame;
            }
        }
        panic!("the intro never settled");
    }

    fn held(left: bool, right: bool, up: bool, down: bool) -> ExamineInput {
        ExamineInput {
            left,
            right,
            up,
            down,
            ..ExamineInput::default()
        }
    }

    #[test]
    fn the_setup_frame_and_the_intro_take_0x40_frames_and_ramp_the_lights() {
        let mut screen = ItemViewScreen::new(0x01);
        assert!(screen.in_intro());
        // The setup runs on the first frame together with the first intro
        // step, so the turntable is up on frame 0x40.
        screen.step_examine(ExamineInput::default());
        assert_eq!(screen.light(), 0xFF - (0x3F << 2));
        assert_eq!(settle(&mut screen), 0x3F);
        assert_eq!(screen.light(), 0xFF);
        // The intro's yaw and roll wrap back to the resting pose.
        assert_eq!([screen.pitch, screen.yaw, screen.roll], [0, 0, 0]);
        assert_eq!(screen.root_matrix().t[0], MODEL_REST_X);
    }

    #[test]
    fn left_and_up_spin_the_turntable_the_reference_way() {
        let mut screen = ItemViewScreen::new(0x01);
        settle(&mut screen);
        let mut poses = Vec::new();
        for input in [
            held(true, false, false, false),
            held(false, true, false, false),
            held(false, false, true, false),
            held(false, false, false, true),
        ] {
            screen.step_examine(input);
            poses.push([screen.pitch, screen.yaw, screen.roll]);
        }
        // The spin is pre-multiplied in 12-bit fixed point and the angles
        // re-extracted, so one 0x20 step lands on 0x1F: left turns yaw up,
        // up turns roll down, and the opposite keys undo them.
        assert_eq!(poses, [[0, 0x1F, 0], [0, 0, 0], [0, 0, 0x0FE1], [0, 0, 0]]);
    }

    #[test]
    fn cancel_runs_the_exit_and_closes_after_0x40_frames() {
        let mut screen = ItemViewScreen::new(0x01);
        settle(&mut screen);
        assert_eq!(
            screen.step_examine(ExamineInput {
                cancel: true,
                ..ExamineInput::default()
            }),
            ExamineEvent::None
        );
        assert!(screen.in_exit());
        for frame in 1..0x40 {
            assert_eq!(
                screen.step_examine(ExamineInput::default()),
                ExamineEvent::None,
                "frame {frame}"
            );
            assert_eq!(i32::from(screen.light()), ((0x40 - frame) << 2) - 1);
        }
        assert_eq!(
            screen.step_examine(ExamineInput::default()),
            ExamineEvent::Closed
        );
    }

    #[test]
    fn confirm_reports_and_waits_for_the_message_to_close() {
        let mut screen = ItemViewScreen::new(0x01);
        settle(&mut screen);
        assert_eq!(
            screen.step_examine(ExamineInput {
                confirm: true,
                ..ExamineInput::default()
            }),
            ExamineEvent::Confirm
        );
        assert_eq!(
            screen.confirm(ExamineContext::default()),
            ConfirmOutcome::Description
        );
        // Cancel and the arrows are ignored while the description is up.
        let waiting = ExamineInput {
            cancel: true,
            left: true,
            message_active: true,
            ..ExamineInput::default()
        };
        screen.step_examine(waiting);
        assert!(!screen.turntable_ready());
        assert_eq!(screen.yaw, 0);
        assert!(!screen.in_exit());
        screen.step_examine(ExamineInput::default());
        assert!(screen.turntable_ready());
    }

    #[test]
    fn examine_windows_map_the_combo_records() {
        let check = |item, angles| examine_check(item, angles, 0, ExamineContext::default());
        let class_message = |item| ConfirmOutcome::Message {
            id: class_message(item).unwrap(),
            mark_examined: true,
            select: None,
        };
        // Crank (0x1D): one record, yaw centred on 0xC00 with a 0x12C window.
        assert_eq!(check(0x1D, [0, 0x0BF0, 0]), class_message(0x1D));
        assert_eq!(check(0x1D, [0, 0, 0]), ConfirmOutcome::Description);
        // Lab key A (0x37) has no records: its class message shows at any
        // pose.
        assert_eq!(check(0x37, [0x123, 0x456, 0x789]), class_message(0x37));
        // The knife's class has the top bit set: plain description.
        assert_eq!(check(0x01, [0, 0, 0]), ConfirmOutcome::Description);
        // The red book's record matches only around yaw 0x270..0x590, and
        // zooms only until the zoom spin has run.
        assert_eq!(check(0x3E, [0, 0x400, 0]), ConfirmOutcome::Zoom);
        assert_eq!(check(0x3E, [0, 0, 0]), ConfirmOutcome::Description);
        assert_eq!(
            examine_check(0x3E, [0, 0x400, 0], 0x310, ExamineContext::default()),
            class_message(0x3E)
        );
    }

    #[test]
    fn loaded_weapons_name_their_rounds() {
        let loaded = ExamineContext {
            slot_quantity: 6,
            ..ExamineContext::default()
        };
        assert_eq!(
            examine_check(0x04, [0, 0, 0], 0, loaded),
            ConfirmOutcome::Message {
                id: MESSAGE_WEAPON_LOADED,
                mark_examined: false,
                select: Some(crate::weapons::weapon_ammo_item_id(0x04)),
            }
        );
        assert_eq!(
            examine_check(0x04, [0, 0, 0], 0, ExamineContext::default()),
            ConfirmOutcome::Description
        );
    }

    #[test]
    fn the_red_book_zooms_then_marks_the_read_bit() {
        let mut screen = ItemViewScreen::new(items::ITEM_RED_BOOK);
        settle(&mut screen);
        screen.yaw = 0x400;
        assert_eq!(
            screen.confirm(ExamineContext::default()),
            ConfirmOutcome::Zoom
        );
        let mut frames = 0;
        let message = loop {
            frames += 1;
            match screen.step_examine(ExamineInput::default()) {
                ExamineEvent::None => {}
                event => break event,
            }
        };
        assert_eq!(
            message,
            ExamineEvent::Message(class_message(items::ITEM_RED_BOOK).unwrap())
        );
        // The step ramps by four up to 0x20: 4+8+..+32 = 144 after eight
        // frames, then 0x20 a frame until object 1 passes 0x300.
        assert_eq!(frames, 8 + (0x300 - 144) / 0x20 + 1);
        screen.step_examine(ExamineInput {
            message_active: true,
            ..ExamineInput::default()
        });
        assert_eq!(
            screen.step_examine(ExamineInput::default()),
            ExamineEvent::MarkExaminedBit(RED_BOOK_READ_BIT)
        );
        // The spin has run: a second confirm goes straight to the message.
        assert!(matches!(
            screen.confirm(ExamineContext::default()),
            ConfirmOutcome::Message { .. }
        ));
    }

    #[test]
    fn a_doom_book_turns_into_its_medal() {
        let mut screen = ItemViewScreen::new(ITEM_DOOM_BOOK_2);
        settle(&mut screen);
        screen.yaw = 0x400;
        assert_eq!(
            screen.confirm(ExamineContext::default()),
            ConfirmOutcome::Zoom
        );
        while let ExamineEvent::None = screen.step_examine(ExamineInput::default()) {}
        // The message closes, then the book zooms back out for 0x40 frames.
        assert_eq!(
            screen.step_examine(ExamineInput::default()),
            ExamineEvent::None
        );
        for frame in 1..=0x40 {
            let event = screen.step_examine(ExamineInput::default());
            if frame < 0x40 {
                assert_eq!(event, ExamineEvent::None, "frame {frame}");
            } else {
                assert_eq!(event, ExamineEvent::Medal(ITEM_WOLF_MEDAL));
            }
        }
        assert_eq!(screen.item(), ITEM_WOLF_MEDAL);
        assert!(screen.in_intro(), "the medal plays the intro again");
        assert_eq!(settle(&mut screen), 0x40);
    }

    #[test]
    fn missing_models_and_descriptions_stay_empty() {
        let dir = TempDir::new();
        let pack = empty_pack(&dir);

        // Item 0x6E maps to the placeholder model name "m" and a description
        // index past the table: both layers stay empty and drawing does not
        // panic.
        let text = Text::default();
        let mut screen = ItemViewScreen::new(0x6E);
        screen.open_with(&pack, &text);
        assert!(screen.model().is_none());
        assert!(screen.description().is_none());

        settle(&mut screen);
        let mut framebuffer = Framebuffer::new();
        screen.draw_examine(&mut framebuffer, None, None, &text, &[0; 4]);
        assert_eq!(painted(&framebuffer), 0);
    }

    #[test]
    fn ingram_and_minimi_descriptions_use_their_own_entries() {
        // A language file where the 1-based item ids 0x4E/0x4F have
        // descriptions but the direct item-id entries do not.
        let dir = TempDir::new();
        let path = dir.0.join("game.akpak");
        let mut writer = PackWriter::new();
        writer
            .add(
                "language/en-US.txt",
                b"ITEM_4E_DESCRIPTION = \"0\"\nITEM_4F_DESCRIPTION = \"1\"\n".to_vec(),
            )
            .unwrap();
        writer.write(&path).unwrap();
        let pack = Pack::open(&path).unwrap();
        let text = Text::load(&pack, Some("en-US"));

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
