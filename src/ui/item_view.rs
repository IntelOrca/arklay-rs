//! The item viewer (examine screen).
//!
//! [`ItemViewScreen`] owns everything it draws: the selected item's `.ivm`
//! model and texture, the item name and the `text/idesc.bin` description
//! window. It is a [`Screen`] so the engine installs it as a gameplay modal
//! over the frozen pause menu; leaving it returns to the menu unchanged.
//!
//! The model renders through [`Framebuffer::draw_ivm_unlit`]: full-bright,
//! one 256-colour CLUT row with direct UVs and backface culling disabled, one
//! root matrix for every object. The name and the description both draw on the
//! original's item line (`0xBA`, left margin `0x22` for the Japanese sheet).
//!
//! # Input
//!
//! Left/right turn the model's turntable, up/down pitch it, at the original's
//! `0x20` 12-bit units per 30 Hz frame; the angles wrap at `0x1000`. Confirm
//! opens the description window, confirm again dismisses it, and cancel leaves
//! the viewer. A missing model (the placeholder name `m`, or a pack without
//! `item_m2` art) or a missing description logs a warning and leaves that
//! layer empty instead of failing the screen.

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
use super::{Screen, ScreenAction, ScreenResult, UiContext, UiInput};

/// Pack path prefix of the shipped item-view models.
pub const ITEM_MODEL_PREFIX: &str = "item/";
/// Viewer camera position: on the +X axis, looking at the origin.
pub const CAMERA_FROM: [i32; 3] = [15000, 0, 0];
/// Viewer camera look-at point.
pub const CAMERA_TO: [i32; 3] = [0, 0, 0];
/// The model's resting translation on +X, in front of the camera.
pub const MODEL_REST_X: i32 = 0x1980;
/// Viewer focal length in pixels.
pub const VIEWER_FOV: i32 = 0xC0;
/// Rotation step per 30 Hz frame, in 12-bit angle units.
pub const ROTATE_STEP: i32 = 0x20;
/// The description table index the Ingram maps to (1-based).
const INGRAM_DESCRIPTION: u16 = 0x4E;
/// The description table index the Minimi maps to (1-based).
const MINIMI_DESCRIPTION: u16 = 0x4F;

/// The item viewer's screen state.
#[derive(Debug, Clone)]
pub struct ItemViewScreen {
    /// The examined item id.
    item: u8,
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
    /// The description window, drawn on the menu line.
    message: MessageWindow,
}

impl ItemViewScreen {
    /// A viewer for `item`, still without any loaded art.
    pub fn new(item: u8) -> Self {
        Self {
            item,
            name: Vec::new(),
            description: None,
            model: None,
            yaw: 0,
            pitch: 0,
            message: MessageWindow::default(),
        }
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

    /// The root matrix of every model object: the turntable rotation and the
    /// resting translation toward the camera.
    pub fn root_matrix(&self) -> Mat4x3 {
        Mat4x3 {
            r: anim::rotation_matrix(self.pitch, self.yaw, 0),
            t: [MODEL_REST_X, 0, 0],
        }
    }

    /// The fixed viewer camera.
    pub fn camera(&self) -> Camera {
        Camera::from_points(CAMERA_FROM, CAMERA_TO, VIEWER_FOV)
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
        Ok(())
    }

    fn update(&mut self, cx: &UiContext<'_>, input: UiInput) -> ScreenResult {
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

        if input.cancel {
            return ScreenResult::Done(ScreenAction::Resume);
        }
        if input.confirm {
            // TODO(parity): (UI) the original confirm first runs the examine
            // check: for the examinable items it compares the model's current
            // yaw/pitch against the `g_ItemExamineCombos` windows and only
            // opens the description (or the red-book zoom) when the rotation
            // matches. The port opens the description unconditionally, so the
            // rotation puzzle items skip their check. The original also plays
            // an entry zoom + light ramp (`g_bItemViewerZoomTimer`) and spins
            // the model while a direction is held; the port steps the angle
            // once per key edge and draws full-bright.
            self.start_description();
            return ScreenResult::Continue;
        }

        if input.left {
            self.yaw = spin(self.yaw, false);
        }
        if input.right {
            self.yaw = spin(self.yaw, true);
        }
        if input.up {
            self.pitch = spin(self.pitch, false);
        }
        if input.down {
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
        } else if !self.name.is_empty() {
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
                    color: [255, 255, 255],
                    clut: 0,
                    tsb: 0,
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
        }
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
