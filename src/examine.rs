//! The item viewer's game rules: the examine check, the turntable spin and
//! the viewer camera.
//!
//! The viewer itself is the pack's `ui/item_view.lua` (the original's
//! `FUN_0044e1b0`, shared by the pause menu's CHECK and the pick-up flows).
//! It owns the stage machine, the arrows and the item name; this module keeps
//! the fixed-point math and the item tables it calls through the screen
//! `api`, so the pose a script reaches is exactly the reference's.

use crate::anim;
use crate::items;
use crate::render::Camera;
use crate::text::Text;

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
/// The description table index the Ingram maps to (1-based).
const INGRAM_DESCRIPTION: u16 = 0x4E;
/// The description table index the Minimi maps to (1-based).
const MINIMI_DESCRIPTION: u16 = 0x4F;
/// Global message: "<ammo> loaded." for a weapon holding rounds.
pub const MESSAGE_WEAPON_LOADED: u8 = 0xF0;
/// Global message for item 0x13 once its scenario flags are up.
pub const MESSAGE_ITEM13: u8 = 0xF1;

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

/// The fixed viewer camera.
///
/// The projection centre is the menu's `(112, 76)`, not the screen centre:
/// the model rests at the viewport box's centre rather than at `(160, 120)`.
pub fn camera() -> Camera {
    let mut camera = Camera::from_points(CAMERA_FROM, CAMERA_TO, VIEWER_FOV);
    // The projection adds the offset to the screen centre (160, 120).
    camera.screen = [MENU_CENTER[0] - 160, MENU_CENTER[1] - 120];
    camera
}

/// Pre-multiply the held `spin` onto the `[pitch, yaw, roll]` base rotation
/// and re-extract the Euler angles (`FUN_0044eb30`).
///
/// The product is written back into the base matrix in place, exactly like
/// the reference's `MulMatrixInPlace`, so its lower rows read the
/// already-updated first row.
// The index loops are the point: each element must read the rows the
// earlier iterations already overwrote.
#[allow(clippy::needless_range_loop)]
pub fn spin(angles: [i32; 3], spin: [i32; 3]) -> [i32; 3] {
    let mut base = anim::rotation_matrix(angles[0], angles[1], angles[2]);
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
    let pitch = angle_of(m12, m22);
    let rest = (0x1000 - shift12(up_x * up_x)).max(0);
    let yaw = angle_of(up_x, sqrt12(rest));
    // Undo the extracted pitch/yaw on the first column; what is left of it
    // is the roll.
    let partial = anim::rotation_matrix(pitch, yaw, 0);
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
    let roll = angle_of(y, x);
    [pitch & 0x0FFF, yaw & 0x0FFF, roll & 0x0FFF]
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
pub fn is_doom_book(item: u8) -> bool {
    items::ITEM_RED_BOOK < item && item < items::ITEM_FIRST_AID_SPRAY
}

/// The global message an item's examine class shows
/// (`g_ItemHealTable[class + 0x51]`).
pub fn class_message(item: u8) -> Option<u8> {
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
pub fn description_bytes(text: &Text, item: u8) -> Option<&[u8]> {
    let id = match item {
        items::ITEM_INGRAM => INGRAM_DESCRIPTION,
        items::ITEM_MINIMI => MINIMI_DESCRIPTION,
        _ => u16::from(item),
    };
    text.description(id)
}

/// The pack entry of the item's `.ivm` model; `None` for an item without a
/// model name.
pub fn model_entry(item: u8) -> Option<String> {
    let name = items::model_name(item)?;
    Some(format!(
        "{ITEM_MODEL_PREFIX}{}.ivm",
        name.to_ascii_lowercase()
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::anim::Mat4x3;
    use crate::ivm::{Ivm, IvmObject, IvmPrim, IvmPrimKind};
    use crate::model::{PALETTE_ROW_LEN, Texture8};
    use crate::pack::{Pack, PackWriter};
    use crate::render::Framebuffer;
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
        // The resting model's origin projects to the menu's projection centre
        // (the original's SetSubpixelOffset(112, 76)), not the screen centre.
        assert_eq!(camera().project([MODEL_REST_X, 0, 0]), Some(MENU_CENTER));
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
    fn left_and_up_spin_the_turntable_the_reference_way() {
        // The spin is pre-multiplied in 12-bit fixed point and the angles
        // re-extracted, so one 0x20 step lands on 0x1F: left turns yaw up,
        // up turns roll down, and the opposite keys undo them.
        let left = spin([0, 0, 0], [0, 0x20, 0]);
        assert_eq!(left, [0, 0x1F, 0]);
        assert_eq!(spin(left, [0, -0x20, 0]), [0, 0, 0]);
        let up = spin([0, 0, 0], [0, 0, -0x20]);
        assert_eq!(up, [0, 0, 0x0FE1]);
        assert_eq!(spin(up, [0, 0, 0x20]), [0, 0, 0]);
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
    fn the_placeholder_model_name_resolves_to_an_absent_entry() {
        // Item 0x6E maps to the placeholder model name "m": the viewer looks
        // it up like any other and finds nothing, and the description index
        // is past the table.
        let dir = TempDir::new();
        let pack = empty_pack(&dir);
        let entry = model_entry(0x6E).unwrap();
        assert_eq!(entry, "item/m.ivm");
        assert!(pack.read(&entry).is_err());
        assert_eq!(description_bytes(&Text::default(), 0x6E), None);
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
