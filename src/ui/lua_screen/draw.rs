//! The handle-based art and drawing half of the screen `api`.
//!
//! `api:picture(stem)` and `api:page(stem)` load pack art once and return a
//! small integer handle; the draw methods take that handle and map one to one
//! onto the [`Framebuffer`] primitives the original screens use, so a ported
//! screen draws exactly the pixels its Rust predecessor did. Rectangles are
//! flat `x, y, w, h` arguments in the layout space `api:width()` and
//! `api:height()` report.

use std::collections::HashMap;

use mlua::{Table, UserDataMethods, Value};

use crate::anim::{self, Mat4x3};
use crate::assets::{Artwork, Geometry};
use crate::budget;
use crate::font::{GLYPH_HEIGHT, Tint};
use crate::ivm::{self, Ivm};
use crate::pack::{AssetClass, Pack};
use crate::render::{Camera, EntityLight, EntityLightSource, Framebuffer};
use crate::state::Image;

use super::{Api, colour_rgb, colour_tint, coord, no_draw};

/// One loaded piece of art.
enum Asset {
    /// A decoded picture (straight RGBA), drawn as a backdrop or RGBA sprite.
    Picture(Image),
    /// A sprite page in its native shape: indexed with CLUT rows, or RGBA.
    Page(Artwork),
    /// An item-view model and the size of the file it was parsed from.
    Model(Box<Ivm>, usize),
}

impl Asset {
    fn size(&self) -> (u32, u32) {
        match self {
            Self::Picture(image) => (image.width, image.height),
            Self::Page(art) => (art.width(), art.height()),
            Self::Model(model, _) => (model.texture.width, model.texture.height),
        }
    }

    /// The bytes the asset holds, against the screen's art budget.
    fn bytes(&self) -> usize {
        let (width, height) = self.size();
        let pixels = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(4);
        match self {
            Self::Model(_, file) => pixels.saturating_add(*file),
            _ => pixels,
        }
    }
}

/// The art a screen loaded, by handle.
#[derive(Default)]
pub(super) struct Assets {
    list: Vec<Asset>,
    /// Handle by load key, so asking twice returns the same art.
    by_key: HashMap<String, usize>,
    /// Decoded bytes held, against [`budget::MAX_SCREEN_IMAGE_BYTES`].
    bytes: usize,
}

impl Assets {
    /// Load `key`'s art with `load` once, returning its 1-based handle. The
    /// set is bounded by count and by the pixels it holds.
    fn load(
        &mut self,
        key: String,
        load: impl FnOnce() -> anyhow::Result<Asset>,
    ) -> anyhow::Result<usize> {
        if let Some(&handle) = self.by_key.get(&key) {
            return Ok(handle);
        }
        budget::check_len(
            self.list.len() + 1,
            budget::MAX_SCREEN_IMAGES,
            "screen art count",
        )?;
        let asset = load()?;
        let bytes = asset.bytes();
        budget::check_len(
            self.bytes.saturating_add(bytes),
            budget::MAX_SCREEN_IMAGE_BYTES,
            "screen art bytes",
        )?;
        self.bytes = self.bytes.saturating_add(bytes);
        self.list.push(asset);
        let handle = self.list.len();
        self.by_key.insert(key, handle);
        Ok(handle)
    }

    fn get(&self, handle: usize) -> mlua::Result<&Asset> {
        handle
            .checked_sub(1)
            .and_then(|index| self.list.get(index))
            .ok_or_else(|| mlua::Error::RuntimeError(format!("invalid art handle {handle}")))
    }

    fn picture(&self, handle: usize) -> mlua::Result<&Image> {
        match self.get(handle)? {
            Asset::Picture(image) => Ok(image),
            _ => Err(mlua::Error::RuntimeError(format!(
                "art handle {handle} is not a picture"
            ))),
        }
    }

    fn page(&self, handle: usize) -> mlua::Result<&Artwork> {
        match self.get(handle)? {
            Asset::Page(art) => Ok(art),
            _ => Err(mlua::Error::RuntimeError(format!(
                "art handle {handle} is not a page"
            ))),
        }
    }

    fn model(&self, handle: usize) -> mlua::Result<&Ivm> {
        match self.get(handle)? {
            Asset::Model(model, _) => Ok(model),
            _ => Err(mlua::Error::RuntimeError(format!(
                "art handle {handle} is not a model"
            ))),
        }
    }
}

/// A `(handle)` or `(nil, message)` load result.
fn loaded(result: anyhow::Result<usize>) -> (Option<usize>, Option<String>) {
    match result {
        Ok(handle) => (Some(handle), None),
        Err(err) => (None, Some(format!("{err:#}"))),
    }
}

/// A clamped `[x, y, w, h]`.
fn rect(x: i32, y: i32, w: i32, h: i32) -> [i32; 4] {
    [coord(x), coord(y), coord(w), coord(h)]
}

/// A colour channel or alpha byte.
fn byte(value: i64) -> u8 {
    value.clamp(0, 255) as u8
}

/// An integer field of `table`, `0` when absent.
fn field(table: &Table, key: &str) -> mlua::Result<i32> {
    Ok(table.get::<Option<i32>>(key)?.unwrap_or(0))
}

/// A `{x, y, z}` triple.
fn triple(table: &Table, key: &str) -> mlua::Result<[i32; 3]> {
    let value: Table = table.get(key)?;
    Ok([value.get(1)?, value.get(2)?, value.get(3)?])
}

/// The joint matrix a `joints` list composes, root first. Each joint is
/// `{rx, ry, rz, tx, ty, tz}` in the engine's 12-bit angles.
fn joint_matrix(joints: Option<Table>) -> mlua::Result<Mat4x3> {
    // The chain starts at the root joint itself: composing onto the table's
    // zero rotation would skew it, as that is not an exact 4.12 identity.
    let mut matrix: Option<Mat4x3> = None;
    if let Some(joints) = joints {
        for joint in joints.sequence_values::<Table>() {
            let joint = joint?;
            let local = Mat4x3 {
                r: anim::rotation_matrix(
                    field(&joint, "rx")?,
                    field(&joint, "ry")?,
                    field(&joint, "rz")?,
                ),
                t: [
                    field(&joint, "tx")?,
                    field(&joint, "ty")?,
                    field(&joint, "tz")?,
                ],
            };
            matrix = Some(match matrix {
                Some(parent) => anim::compose(&parent, &local),
                None => local,
            });
        }
    }
    Ok(matrix.unwrap_or(Mat4x3 {
        r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
        t: [0; 3],
    }))
}

/// A light record: `{ambient = 12-bit level, sources = {{pos = {x, y, z},
/// color = {r, g, b}}, ...}}`. As the reference's `FUN_0040ac80` builds it,
/// each position is a direction, normalized to 4.12 with Y negated, and each
/// colour byte caps at the `0x80` full scale; the ambient converts by
/// `(value * 255) / 4096`. Missing sources are dark.
fn light(table: &Table) -> mlua::Result<EntityLight> {
    let ambient = f64::from(field(table, "ambient")?.clamp(0, 0xFFFF) * 255 / 4096);
    let dark = EntityLightSource {
        dir: [0; 3],
        color: [0; 3],
    };
    let mut sources = [dark, dark, dark];
    if let Some(list) = table.get::<Option<Table>>("sources")? {
        for (slot, source) in sources.iter_mut().zip(list.sequence_values::<Table>()) {
            let source = source?;
            let pos = triple(&source, "pos")?.map(f64::from);
            let length = pos.iter().map(|v| v * v).sum::<f64>().sqrt().max(1.0);
            let color = triple(&source, "color")?;
            *slot = EntityLightSource {
                dir: [
                    (pos[0] / length * 4096.0) as i32,
                    (-pos[1] / length * 4096.0) as i32,
                    (pos[2] / length * 4096.0) as i32,
                ],
                color: color.map(|c| c.clamp(0, 0x80) as u8),
            };
        }
    }
    Ok(EntityLight {
        ambient: [ambient; 3],
        sources,
    })
}

/// Draw `bytes` through the font sheet's own grid addressing: `u = (c % 18)
/// * glyph_w`, `v = (c / 18) * 14`, no header offset (the reference's
/// `ending_draw_text`, `EndingScreen.cpp:296-324`). The bytes are raw sheet
/// cells, so a `0x5E` lands on the sheet's own separator cell.
fn draw_grid_text(
    framebuffer: &mut Framebuffer,
    font: &crate::font::Font,
    origin: [i32; 2],
    scale: u8,
    bytes: &[u8],
) {
    let glyph_w = font.metrics.glyph_w;
    if glyph_w <= 0 {
        return;
    }
    let [mut pen, y] = origin;
    for &byte in bytes {
        if byte != b' ' {
            let u = i32::from(byte % 18) * glyph_w;
            let v = i32::from(byte / 18) * GLYPH_HEIGHT;
            framebuffer.draw_indexed_sprite_scaled(
                &font.texture,
                [u, v, glyph_w, GLYPH_HEIGHT],
                [pen, y, glyph_w, GLYPH_HEIGHT],
                0,
                scale,
                Tint::White,
            );
        }
        pen = pen.saturating_add(glyph_w);
    }
}

/// Register the handle-based art and drawing methods.
pub(super) fn add_methods<'a, M: UserDataMethods<Api<'a>>>(methods: &mut M) {
    // ---- layout --------------------------------------------------------
    methods.add_method("width", |_lua, api, ()| Ok(api.host.layout.0));
    methods.add_method("height", |_lua, api, ()| Ok(api.host.layout.1));

    // ---- art -----------------------------------------------------------
    methods.add_method("has_image", |_lua, api, stem: String| {
        Ok(api.cx.pack.has_asset(&stem, AssetClass::Image))
    });

    methods.add_method_mut(
        "picture",
        |_lua, api, (stem, key, geometry): (String, Option<Value>, Option<String>)| {
            let key = match key {
                Some(Value::Nil) | None => None,
                Some(value) => Some(colour_rgb(&value)?),
            };
            // "shipped" pins the art to the original sheet's size; the
            // default lets a replacement be authored at any size.
            let geometry = match geometry.as_deref() {
                None | Some("free") => Geometry::Free,
                Some("shipped") => Geometry::Shipped,
                Some(other) => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "unknown picture geometry {other:?}"
                    )));
                }
            };
            let pack: &Pack = api.cx.pack;
            let cache_key = format!("picture:{stem}:{key:?}:{geometry:?}");
            Ok(loaded(api.host.assets.load(cache_key, || {
                crate::ui::load_picture(pack, &stem, geometry, key).map(Asset::Picture)
            })))
        },
    );

    methods.add_method_mut("page", |_lua, api, stem: String| {
        let pack: &Pack = api.cx.pack;
        let cache_key = format!("page:{stem}");
        Ok(loaded(api.host.assets.load(cache_key, || {
            crate::ui::load_page(pack, &stem).map(Asset::Page)
        })))
    });

    methods.add_method_mut("ivm", |_lua, api, entry: String| {
        let pack: &Pack = api.cx.pack;
        let cache_key = format!("ivm:{entry}");
        Ok(loaded(api.host.assets.load(cache_key, || {
            let bytes = pack.read(&entry)?;
            let model = ivm::parse(&bytes)?;
            Ok(Asset::Model(Box::new(model), bytes.len()))
        })))
    });

    methods.add_method("art_size", |_lua, api, handle: usize| {
        Ok(api.host.assets.get(handle)?.size())
    });

    // ---- primitives ----------------------------------------------------
    methods.add_method_mut("clear", |_lua, api, ()| {
        api.framebuffer.as_deref_mut().ok_or_else(no_draw)?.clear();
        Ok(())
    });

    methods.add_method_mut(
        "fill_rect",
        |_lua, api, (x, y, w, h, r, g, b, a): (i32, i32, i32, i32, i64, i64, i64, Option<i64>)| {
            let framebuffer = api.framebuffer.as_deref_mut().ok_or_else(no_draw)?;
            framebuffer.fill_rect(
                rect(x, y, w, h),
                [byte(r), byte(g), byte(b), byte(a.unwrap_or(255))],
            );
            Ok(())
        },
    );

    // `draw_ivm(model, {joints = {...}, camera = {from, to, fov}, light})`:
    // every object of the model takes the one composed joint matrix.
    methods.add_method_mut("draw_ivm", |_lua, api, (handle, spec): (usize, Table)| {
        let Api {
            host, framebuffer, ..
        } = api;
        let framebuffer = framebuffer.as_deref_mut().ok_or_else(no_draw)?;
        let model = host.assets.model(handle)?;
        let joint = joint_matrix(spec.get("joints")?)?;
        let camera: Table = spec.get("camera")?;
        let camera = Camera::from_points(
            triple(&camera, "from")?,
            triple(&camera, "to")?,
            field(&camera, "fov")?,
        );
        let light = match spec.get::<Option<Table>>("light")? {
            Some(table) => Some(light(&table)?),
            None => None,
        };
        let joints = vec![joint; model.objects.len()];
        framebuffer.draw_ivm(model, &joints, &camera, light.as_ref());
        Ok(())
    });

    // `grid_text(x, y, scale, bytes)`: raw font-sheet cells, see
    // [`draw_grid_text`]. Without a font nothing draws.
    methods.add_method_mut(
        "grid_text",
        |_lua, api, (x, y, scale, text): (i32, i32, i64, mlua::String)| {
            let Some(font) = api.cx.font else {
                return Ok(());
            };
            let framebuffer = api.framebuffer.as_deref_mut().ok_or_else(no_draw)?;
            draw_grid_text(
                framebuffer,
                font,
                [coord(x), coord(y)],
                byte(scale),
                &text.as_bytes(),
            );
            Ok(())
        },
    );

    methods.add_method_mut(
        "blend_black_rect",
        |_lua, api, (x, y, w, h, alpha): (i32, i32, i32, i32, i64)| {
            let framebuffer = api.framebuffer.as_deref_mut().ok_or_else(no_draw)?;
            framebuffer.blend_black_rect(rect(x, y, w, h), byte(alpha));
            Ok(())
        },
    );

    #[allow(clippy::type_complexity)]
    methods.add_method_mut(
        "picture_sprite_scaled",
        |_lua,
         api,
         (handle, sx, sy, sw, sh, dx, dy, dw, dh, scale): (
            usize,
            i32,
            i32,
            i32,
            i32,
            i32,
            i32,
            i32,
            i32,
            i64,
        )| {
            let Api {
                host, framebuffer, ..
            } = api;
            let framebuffer = framebuffer.as_deref_mut().ok_or_else(no_draw)?;
            let image = host.assets.picture(handle)?;
            framebuffer.draw_rgba_sprite_scaled(
                image,
                rect(sx, sy, sw, sh),
                rect(dx, dy, dw, dh),
                byte(scale),
            );
            Ok(())
        },
    );

    methods.add_method_mut(
        "draw_background",
        |_lua, api, (handle, x, y, r, g, b): (usize, i32, i32, Option<i64>, Option<i64>, Option<i64>)| {
            let Api {
                host, framebuffer, ..
            } = api;
            let framebuffer = framebuffer.as_deref_mut().ok_or_else(no_draw)?;
            let image = host.assets.picture(handle)?;
            let colour = [
                byte(r.unwrap_or(255)),
                byte(g.unwrap_or(255)),
                byte(b.unwrap_or(255)),
            ];
            framebuffer.draw_background(image, [coord(x), coord(y)], colour);
            Ok(())
        },
    );

    #[allow(clippy::type_complexity)]
    methods.add_method_mut(
        "page_sprite_scaled",
        |_lua,
         api,
         (handle, sx, sy, sw, sh, dx, dy, dw, dh, clut, scale, tint): (
            usize,
            i32,
            i32,
            i32,
            i32,
            i32,
            i32,
            i32,
            i32,
            usize,
            i64,
            Option<Value>,
        )| {
            let tint = match tint {
                Some(Value::Nil) | None => crate::font::Tint::White,
                Some(value) => colour_tint(&value)?,
            };
            let Api {
                host, framebuffer, ..
            } = api;
            let framebuffer = framebuffer.as_deref_mut().ok_or_else(no_draw)?;
            let art = host.assets.page(handle)?;
            framebuffer.draw_page_sprite_scaled(
                art,
                rect(sx, sy, sw, sh),
                rect(dx, dy, dw, dh),
                clut.min(255),
                byte(scale),
                tint,
            );
            Ok(())
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epilogue_light_is_the_reference_record() {
        // `ending_epilogue_build`: `FUN_0040ac80(0, (100,100,100 | 0xFFFFFF))`
        // then `setBackColor(0xFFF, 0xFFF, 0xFFF)`. The direction is the
        // normalized position with its Y negated, the colour caps at 0x80 and
        // the 12-bit ambient converts with the reference's `* 255 / 4096`.
        let lua = mlua::Lua::new();
        let table: Table = lua
            .load(
                "return { ambient = 0xFFF, sources = { \
                 { pos = { 100, 100, 100 }, color = { 0xFF, 0xFF, 0xFF } } } }",
            )
            .eval()
            .unwrap();
        let light = light(&table).unwrap();
        assert_eq!(light.ambient, [254.0; 3]);
        assert_eq!(light.sources[0].dir, [2364, -2364, 2364]);
        assert_eq!(light.sources[0].color, [0x80; 3]);
        // The unset slots stay dark.
        assert!(light.sources[1..].iter().all(|s| s.color == [0; 3]));
    }

    #[test]
    fn a_joint_chain_starts_at_its_root_joint() {
        let lua = mlua::Lua::new();
        let joints: Table = lua
            .load("return { { rx = 0x95C, ty = 500 }, { rz = 0x120 } }")
            .eval()
            .unwrap();
        let root = Mat4x3 {
            r: anim::rotation_matrix(0x95C, 0, 0),
            t: [0, 500, 0],
        };
        let spin = Mat4x3 {
            r: anim::rotation_matrix(0, 0, 0x120),
            t: [0; 3],
        };
        let expected = anim::compose(&root, &spin);
        let actual = joint_matrix(Some(joints)).unwrap();
        assert_eq!((actual.r, actual.t), (expected.r, expected.t));
    }
}
