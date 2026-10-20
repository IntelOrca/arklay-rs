//! The 40x30 inventory item icons for the Rust-drawn debug overlay. The Lua
//! screens draw their icons through the screen API instead.

use anyhow::Result;

use crate::assets::{Artwork, Geometry};
use crate::font::Tint;
use crate::items;
use crate::pack::Pack;
use crate::render::Framebuffer;
use crate::state::Image;

/// `ui/staitem.tim`: the Ingram and Minimi icons.
const STAITEM_STEM: &str = "ui/staitem";

/// `item/item_all.bmp`: the 40x30 icon atlas (black keyed transparent).
const ITEM_ALL_STEM: &str = "item/item_all";

/// The two icon sheets.
pub struct ItemIcons {
    item_all: Image,
    staitem: Artwork,
}

impl ItemIcons {
    /// Load both icon sheets from `pack`.
    pub fn load(pack: &Pack) -> Result<Self> {
        Ok(Self {
            item_all: super::load_picture(pack, ITEM_ALL_STEM, Geometry::Shipped, Some([0, 0, 0]))?,
            staitem: super::load_page(pack, STAITEM_STEM)?,
        })
    }

    /// Draw `item`'s icon at `position`: `item/item_all.bmp` for ids below
    /// `0x6F`, the `ui/staitem.tim` special rows for the Ingram and Minimi.
    pub fn draw(&self, framebuffer: &mut Framebuffer, item: u8, position: [i32; 2]) {
        let dst = [position[0], position[1], 40, 30];
        if item < 0x6F {
            let class = items::image_class(item);
            let row = usize::from(class.wrapping_sub(1));
            if class == 0 || row >= items::ITEM_ALL_ROWS {
                return;
            }
            let v = (row * items::ICON_HEIGHT as usize) as i32;
            framebuffer.draw_rgba_sprite(&self.item_all, [0, v, 40, 30], dst, 0);
        } else {
            let row = i32::from(item - 0x6F);
            framebuffer.draw_page_sprite(
                &self.staitem,
                [0, row * 30, 40, 30],
                dst,
                0,
                0,
                Tint::White,
            );
        }
    }
}
