//! The handle-based art and drawing half of the screen `api`.
//!
//! `api:picture(stem)` and `api:page(stem)` load pack art once and return a
//! small integer handle; the draw methods take that handle and map one to one
//! onto the [`Framebuffer`] primitives the original screens use, so a ported
//! screen draws exactly the pixels its Rust predecessor did. Rectangles are
//! flat `x, y, w, h` arguments in the layout space `api:width()` and
//! `api:height()` report.

use std::collections::HashMap;

use mlua::{UserDataMethods, Value};

use crate::assets::{Artwork, Geometry};
use crate::budget;
use crate::pack::{AssetClass, Pack};
use crate::state::Image;

use super::{Api, colour_rgb, colour_tint, coord, no_draw};

/// One loaded piece of art.
enum Asset {
    /// A decoded picture (straight RGBA), drawn as a backdrop or RGBA sprite.
    Picture(Image),
    /// A sprite page in its native shape: indexed with CLUT rows, or RGBA.
    Page(Artwork),
}

impl Asset {
    fn size(&self) -> (u32, u32) {
        match self {
            Self::Picture(image) => (image.width, image.height),
            Self::Page(art) => (art.width(), art.height()),
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
        let (width, height) = asset.size();
        let bytes = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(4);
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
            Asset::Page(_) => Err(mlua::Error::RuntimeError(format!(
                "art handle {handle} is a page, not a picture"
            ))),
        }
    }

    fn page(&self, handle: usize) -> mlua::Result<&Artwork> {
        match self.get(handle)? {
            Asset::Page(art) => Ok(art),
            Asset::Picture(_) => Err(mlua::Error::RuntimeError(format!(
                "art handle {handle} is a picture, not a page"
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
        |_lua, api, (stem, key): (String, Option<Value>)| {
            let key = match key {
                Some(Value::Nil) | None => None,
                Some(value) => Some(colour_rgb(&value)?),
            };
            let pack: &Pack = api.cx.pack;
            let cache_key = format!("picture:{stem}:{key:?}");
            Ok(loaded(api.host.assets.load(cache_key, || {
                crate::ui::load_picture(pack, &stem, Geometry::Free, key).map(Asset::Picture)
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
