//! The pause menu's map tab.
//!
//! The tab is always available: the original's `menu_tab_map` runs the map
//! state machine without a scenario check (the radio's flag gates only the
//! radio tab). The browser composites the highest available layout's art
//! (`map/map0e.tim`, `map00`, `map0c` or `map0d` - `map_tab_load_map` loads
//! the file of the mask's highest set bit, one page for every browser
//! sprite): the 160x72 base window at (33,32), the four layout markers
//! (the current layout's at full brightness, the others at half), the current
//! area's name row ("Mansion 1F" and friends, 80x16 at (57,104)) and the
//! current floor's number glyph (24x16 at (137,104)), plus the room-step
//! arrows while a neighbouring room is reachable. Every sprite's index-0
//! texels are transparent. Up/down step the room index inside the current
//! layout (only when the neighbouring area is known), left/right step between
//! the layouts the owned maps unlock. Confirm switches to the full floor-plan
//! view of the current area; cancel returns to the browser and then closes
//! the tab.
//!
//! The area/layout/room tables come from `map/tables.bin`, extracted from the
//! executable by the converter exactly like the text tables. The full-display
//! zoom animation and the objective highlight are deliberate approximations:
//! the zoom is a single swap and the objective rule is transcribed from the
//! scenario flags rather than extracted. A pack without the map entries leaves
//! the tab blank with a warning.
//!
//! Everything here is data-driven and draw-only: the room-flags/SCD bit that
//! records a map pick-up is raised by [`crate::game::GameState::pick_up_map`].

use anyhow::Result;

use crate::assets;
use crate::game::{self, GameState};
use crate::model::Texture8;
use crate::pack::Pack;
use crate::render::Framebuffer;
use crate::state::Image;

use super::layout;
use super::main_menu::MenuInput;

/// Pack entry of the extracted map tables.
pub const TABLES_ENTRY: &str = "map/tables.bin";
/// Pack stem of the map backdrop.
pub const BLUE_STEM: &str = "map/blue";
/// Pack entry of the map backdrop.
pub const BLUE_ENTRY: &str = "map/blue.tim";
/// The four browser floor-plan entries, indexed by layout. The browser loads
/// the file of the *highest* available layout (`map_tab_load_map`) and samples
/// every one of its sprites from that single page.
pub const TAB_ENTRIES: [&str; 4] = [
    "map/map0d.tim",
    "map/map0c.tim",
    "map/map00.tim",
    "map/map0e.tim",
];
/// The eleven full-display floor-plan entries, indexed by area.
pub const AREA_ENTRIES: [&str; 11] = [
    "map/map01.tim",
    "map/map02.tim",
    "map/map03.tim",
    "map/map04.tim",
    "map/map05.tim",
    "map/map06.tim",
    "map/map07.tim",
    "map/map08.tim",
    "map/map09.tim",
    "map/map0a.tim",
    "map/map0b.tim",
];

/// Magic of the map-table blob.
const TABLES_MAGIC: [u8; 4] = *b"MAP1";
/// Bytes of the table blob: magic plus the seven tables.
pub const TABLES_BYTES: usize = 4 + 6 + 6 + 16 + 16 + 16 + 4 + 4;

/// The floor-plan bookkeeping tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapTables {
    /// Visited-room bit base per map group (`(stage - 1) % 5`).
    pub stage_room_offsets: [u8; 6],
    /// Room count per map group.
    pub room_counts: [u8; 6],
    /// Area per `(layout, room index)`; `0xFF` is no room.
    pub area: [u8; 16],
    /// Layout per area.
    pub layouts: [u8; 16],
    /// Map group per area.
    pub groups: [u8; 16],
    /// Room count per layout (the index of the trailing exit room).
    pub layout_room_count: [u8; 4],
    /// Floor-number offset per layout.
    pub layout_offset: [u8; 4],
}

impl Default for MapTables {
    fn default() -> Self {
        Self {
            stage_room_offsets: [0, 32, 63, 82, 100, 0],
            room_counts: [32, 31, 19, 18, 24, 0],
            area: [
                2, 0, 1, 0xFF, 4, 3, 0xFF, 0xFF, 6, 5, 0xFF, 0xFF, 10, 9, 8, 7,
            ],
            layouts: [0, 0, 0, 1, 1, 2, 2, 3, 3, 3, 3, 0, 0, 0, 0, 0],
            groups: [0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 4, 4, 0, 3, 2, 2],
            layout_room_count: [0, 3, 2, 2],
            layout_offset: [4, 4, 4, 4],
        }
    }
}

impl MapTables {
    /// Serialize the tables into the `map/tables.bin` layout.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(TABLES_BYTES);
        out.extend_from_slice(&TABLES_MAGIC);
        out.extend_from_slice(&self.stage_room_offsets);
        out.extend_from_slice(&self.room_counts);
        out.extend_from_slice(&self.area);
        out.extend_from_slice(&self.layouts);
        out.extend_from_slice(&self.groups);
        out.extend_from_slice(&self.layout_room_count);
        out.extend_from_slice(&self.layout_offset);
        out
    }

    /// Parse the `map/tables.bin` blob.
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < TABLES_BYTES {
            anyhow::bail!(
                "map table blob is {} bytes, need {TABLES_BYTES}",
                data.len()
            );
        }
        if data[..4] != TABLES_MAGIC {
            anyhow::bail!("map table blob has no {TABLES_MAGIC:?} magic");
        }
        let mut cursor = 4;
        let mut take = |count: usize| {
            let slice = &data[cursor..cursor + count];
            cursor += count;
            slice
        };
        let mut array = |count: usize| -> Vec<u8> { take(count).to_vec() };
        let stage_room_offsets: [u8; 6] = array(6).try_into().unwrap();
        let room_counts: [u8; 6] = array(6).try_into().unwrap();
        let area: [u8; 16] = array(16).try_into().unwrap();
        let layouts: [u8; 16] = array(16).try_into().unwrap();
        let groups: [u8; 16] = array(16).try_into().unwrap();
        let layout_room_count: [u8; 4] = array(4).try_into().unwrap();
        let layout_offset: [u8; 4] = array(4).try_into().unwrap();
        Ok(Self {
            stage_room_offsets,
            room_counts,
            area,
            layouts,
            groups,
            layout_room_count,
            layout_offset,
        })
    }

    /// The area at a layout's room index (`0xFF` when the slot is empty).
    pub fn area_at(&self, layout: usize, room: usize) -> u8 {
        self.area.get(layout * 4 + room).copied().unwrap_or(0xFF)
    }

    /// The layout a room flag group's visited bits live in.
    pub fn layout_for_area(&self, area: u8) -> u8 {
        self.layouts.get(usize::from(area)).copied().unwrap_or(0)
    }

    /// The map group an area belongs to.
    pub fn group_for_area(&self, area: u8) -> u8 {
        self.groups.get(usize::from(area)).copied().unwrap_or(0)
    }

    /// The map index an area's owned bit uses (area minus its gap in the
    /// table), or `None` for the areas that are never "known".
    pub fn map_index(area: u8) -> Option<u8> {
        match area {
            0 => Some(0),
            1 => Some(1),
            3 => Some(2),
            4 => Some(3),
            5 | 6 => Some(4),
            8 | 9 => Some(5),
            _ => None,
        }
    }

    /// Whether the area's map is owned (the `0x7C + index` room-flags bit).
    pub fn area_known(&self, flags: &[game::FlagBank], area: u8) -> bool {
        let Some(index) = Self::map_index(area) else {
            return false;
        };
        flags[usize::from(game::BANK_ROOM_FLAGS)].bit(game::ROOM_FLAG_MAP_BASE + index)
    }

    /// Whether a group's room has been visited.
    pub fn room_visited(&self, flags: &[game::FlagBank], group: usize, room: usize) -> bool {
        let Some(base) = self.stage_room_offsets.get(group) else {
            return false;
        };
        if room >= usize::from(self.room_counts.get(group).copied().unwrap_or(0)) {
            return false;
        }
        flags[usize::from(game::BANK_ROOM_FLAGS)].bit(base.wrapping_add(room as u8))
    }

    /// Build the area mask from the visited rooms and owned maps.
    pub fn area_mask(&self, flags: &[game::FlagBank]) -> u16 {
        let mut mask = 0u16;
        for group in 0..5 {
            for room in 0..usize::from(self.room_counts[group]) {
                if !self.room_visited(flags, group, room) {
                    continue;
                }
                let area = match group {
                    0 => 0,
                    1 => {
                        if room < 0x1A || room == 0x1D {
                            1
                        } else {
                            2
                        }
                    }
                    2 => {
                        if room < 0x06 || room == 0x10 {
                            3
                        } else {
                            4
                        }
                    }
                    3 => {
                        if room < 0x0D {
                            5
                        } else {
                            6
                        }
                    }
                    4 => {
                        if room < 0x02 || room == 0x16 {
                            7
                        } else if room < 0x05 {
                            8
                        } else if room < 0x13 {
                            9
                        } else {
                            10
                        }
                    }
                    _ => 0,
                };
                mask |= 1 << area;
            }
        }
        for area in 0..=10u8 {
            if self.area_known(flags, area) {
                mask |= 1 << area;
            }
        }
        mask
    }

    /// Build the layout-availability mask from the area mask.
    pub fn layout_mask(&self, flags: &[game::FlagBank]) -> u8 {
        let mask = self.area_mask(flags);
        let mut layouts = 0u8;
        for area in 0..=10usize {
            if mask & (1 << area) != 0 {
                layouts |= 1 << self.layouts[area];
            }
        }
        layouts
    }

    /// The initial map view for a room: `(area, layout, room index per
    /// layout)`, transcribed from the original's special-camera cases.
    pub fn initial_view(&self, group: usize, room: u8, cut: u8) -> (u8, u8, [u8; 4]) {
        let mut index = [1u8, 1, 1, 3];
        let area = match group {
            0 => {
                if (room == 0x07 && (2..7).contains(&cut))
                    || (room == 0x0F && (cut == 3 || cut == 4))
                {
                    0
                } else if room == 0x10 && cut == 0 {
                    2
                } else {
                    0
                }
            }
            1 => {
                if room == 0x0F && (cut == 2 || cut == 5) {
                    1
                } else if room == 0x0C && (3..8).contains(&cut) {
                    0
                } else if room < 0x1A {
                    1
                } else {
                    2
                }
            }
            2 => {
                if (room == 0x0B && (4..7).contains(&cut)) || (room == 0x0F && cut == 6) {
                    4
                } else if room < 0x06 {
                    3
                } else {
                    4
                }
            }
            3 => {
                if room < 0x0D {
                    5
                } else {
                    6
                }
            }
            _ => {
                if room == 0x07 && cut < 4 {
                    9
                } else if room < 0x02 {
                    7
                } else if room < 0x05 {
                    8
                } else if room > 0x12 {
                    10
                } else {
                    9
                }
            }
        };
        // The original overwrites each layout's index with the last room slot
        // whose area matches the current one.
        for (layout, slot) in index.iter_mut().enumerate() {
            for room in 0..4 {
                if self.area_at(layout, room) == area {
                    *slot = room as u8;
                }
            }
        }
        (area, self.layout_for_area(area), index)
    }

    /// The right/left move flags of a layout position: bit 0 = a room to the
    /// right exists, bit 1 = a room to the left, both gated by the neighbour's
    /// area being reachable.
    pub fn move_flags(&self, flags: &[game::FlagBank], layout: usize, room: u8) -> u8 {
        let area_mask = self.area_mask(flags);
        let mut move_flags = 0u8;
        let last = self.layout_room_count.get(layout).copied().unwrap_or(0);
        if last.wrapping_sub(room) != 1 {
            let next = self.next_area(layout, room);
            if next != 0xFF
                && (area_mask & (1 << (next & 0x0F)) != 0 || self.area_known(flags, next))
            {
                move_flags |= 1;
            }
        }
        if room != 0 {
            let prev = self.area_at(layout, usize::from(room) - 1);
            if prev != 0xFF
                && (area_mask & (1 << (prev & 0x0F)) != 0 || self.area_known(flags, prev))
            {
                move_flags |= 2;
            }
        }
        move_flags
    }

    /// The area of the room *after* `room`, exactly as the original's
    /// `g_MapAreaTable[layout * 4 + room + 1]` read resolves it. The room
    /// after a layout's last slot leaves the 16-entry table (`0x004d31f0`),
    /// which the executable continues with its layout table (`0x004d3200`):
    /// layouts 0-2 land on the next layout's first room, layout 3's slot lands
    /// on the layout table's own first byte. The reference build resolves that
    /// byte as Mansion 1F and the one after it as an unknown area (the up
    /// arrow is still offered on layout 3's last room, and the step out of it
    /// closes both arrows), so those two reads are modelled here.
    fn next_area(&self, layout: usize, room: u8) -> u8 {
        match usize::from(room) + 1 {
            index if index < 4 => self.area_at(layout, index),
            4 => match layout {
                3 => 0,
                _ => self.area_at(layout + 1, 0),
            },
            _ => 0xFF,
        }
    }
}

/// What a map-tab input asked the engine to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapEvent {
    /// The input was consumed inside the tab.
    None,
    /// The view changed (room, layout or zoom).
    Changed,
    /// Close the tab and return to the status screen.
    Close,
}

/// One browser sprite: the art rect `(u, v, w, h)` drawn at `(x, y)`.
#[derive(Debug, Clone, Copy)]
struct SpriteRect {
    /// Screen x of the sprite's top-left.
    x: i32,
    /// Screen y of the sprite's top-left.
    y: i32,
    /// Sprite width in pixels.
    w: i32,
    /// Sprite height in pixels.
    h: i32,
    /// Art column of the sprite's top-left.
    u: u8,
    /// Art row of the sprite's top-left.
    v: u8,
}

/// The browser's base map window (`g_MapBaseDesc`, 0x004d3228): the 160x72
/// top-left window of the loaded layout art at (33,32).
const BASE_RECT: SpriteRect = SpriteRect {
    x: 33,
    y: 32,
    w: 160,
    h: 72,
    u: 0x00,
    v: 0x00,
};

/// The browser's sprite table (`g_MapSprites`, 0x004d324c..0x004d3348), in
/// draw order: the four layout markers, the area-name row (its `texV` is
/// `layout * 0x10 + 0x48`, the column of area names in the art), the floor
/// number glyph (its `texV` is `(6 - room - 4) * 0x10`) and the two
/// room-step arrows (`texU` 0xa0/0xb0 plus 8 for the blink's second frame).
/// The four markers sit at their own screen rects, so an available layout
/// paints its marker over the base window.
const BROWSER_SPRITES: [SpriteRect; 8] = [
    SpriteRect {
        x: 121,
        y: 32,
        w: 56,
        h: 64,
        u: 0xA0,
        v: 0x00,
    },
    SpriteRect {
        x: 73,
        y: 32,
        w: 80,
        h: 64,
        u: 0x00,
        v: 0x48,
    },
    SpriteRect {
        x: 49,
        y: 32,
        w: 40,
        h: 64,
        u: 0x50,
        v: 0x48,
    },
    SpriteRect {
        x: 121,
        y: 32,
        w: 28,
        h: 64,
        u: 0xC8,
        v: 0x48,
    },
    SpriteRect {
        x: 57,
        y: 104,
        w: 80,
        h: 16,
        u: 0x78,
        v: 0x48,
    },
    SpriteRect {
        x: 137,
        y: 104,
        w: 24,
        h: 16,
        u: 0xE8,
        v: 0x60,
    },
    SpriteRect {
        x: 145,
        y: 96,
        w: 8,
        h: 8,
        u: 0xA0,
        v: 0x40,
    },
    SpriteRect {
        x: 145,
        y: 120,
        w: 8,
        h: 8,
        u: 0xB0,
        v: 0x40,
    },
];

/// The screen rect the browser's composite covers: the base window's
/// 160x72 plus the name and glyph rows below it. The engine re-draws the
/// panel decorations the original paints over that rect
/// (`MainMenu::draw_frames_over`).
pub const BROWSER_RECT: layout::Rect = layout::Rect {
    pos: [33, 32],
    size: [160, 96],
};

/// The `colorMul` the browser's base map, name row, glyph and arrows carry
/// (`map_update_layout_state` / `map_draw_browser`); the non-current layouts'
/// markers carry [`MUL_HALF`] instead.
const MUL_FULL: u8 = 0x80;
/// The `colorMul` of the markers of the layouts the player is not standing in.
const MUL_HALF: u8 = 0x40;

/// One `colorMul`-scaled channel. The original's sprite draw multiplies the
/// texel by `colorMul * 2 / 255` and clamps at 1.0, so `0x80` (the default)
/// is unity - the reference's captures are byte-identical to the art there -
/// and `0x40` is half brightness. The 8-bit result rounds to nearest, like
/// the reference's texture unit.
fn tint(channel: u8, mul: u8) -> u8 {
    let factor = u32::from(mul) * 2;
    let factor = factor.min(255);
    ((u32::from(channel) * factor + 127) / 255) as u8
}

/// The map tab's live state.
pub struct MapScreen {
    /// The extracted tables; defaults when the pack has none.
    pub tables: MapTables,
    /// The current layout (`0..4`).
    layout: u8,
    /// The selected room index per layout.
    room: [u8; 4],
    /// The current area.
    area: u8,
    /// Whether the full floor plan is shown (the original's display zoom).
    zoomed: bool,
    /// The browser art (the highest available layout's `map0d/map0c/map00/
    /// map0e` page), lazily loaded on the first browser frame.
    art: Option<Texture8>,
    /// The layout [`Self::art`] was loaded for (`0xff` when none).
    art_layout: u8,
    /// The full-display floor plans, lazily loaded and keyed by area.
    display: [Option<Texture8>; 11],
    /// The map backdrop.
    backdrop: Option<Texture8>,
    /// Blink phase for the cursor arrows (the original's `MAP_HL_COUNT`,
    /// bumped once per browser frame).
    ticks: u32,
    /// Whether the browser's first frame has been drawn. The original's tab
    /// action runs one frame after its confirm (`menu_handle_input` only sets
    /// `DAT_00ae9f20 = 2` on the confirm frame and calls the tab's state
    /// machine on the next frame), so the screen exists but stays blank until
    /// its first update.
    started: bool,
}

/// Load one map page by stem as indexed art.
///
/// The map browser composites its pages by palette index: the room markers
/// are a fixed index range of the shipped art and the index-0 texels are the
/// transparent key, so a replacement is re-indexed exactly and must keep the
/// shipped page's dimensions.
fn load_indexed(pack: &Pack, stem: &str) -> anyhow::Result<Texture8> {
    let asset = pack
        .read_asset(stem, crate::pack::AssetClass::Image)?
        .ok_or_else(|| anyhow::anyhow!("missing {stem}"))?;
    assets::decode_texture(&asset, assets::Geometry::Shipped)
        .map_err(|err| err.context(format!("invalid {}", asset.path())))
}

impl MapScreen {
    /// Load the tables and art, and pick the initial view from `game`'s room.
    pub fn open(pack: &Pack, game: &GameState) -> Self {
        let tables = match pack.read(TABLES_ENTRY) {
            Ok(data) => match MapTables::parse(&data) {
                Ok(tables) => tables,
                Err(err) => {
                    eprintln!("warning: invalid {TABLES_ENTRY}: {err:#}; the map tab stays blank");
                    MapTables::default()
                }
            },
            Err(err) => {
                eprintln!("warning: missing {TABLES_ENTRY}: {err:#}; the map tab stays blank");
                MapTables::default()
            }
        };
        let mut screen = Self {
            tables,
            layout: 0,
            room: [1, 1, 1, 3],
            area: 0,
            zoomed: false,
            art: None,
            art_layout: 0xFF,
            display: Default::default(),
            backdrop: None,
            ticks: 0,
            started: false,
        };
        screen.backdrop = screen.load_backdrop(pack);
        let group = usize::from(game.id.stage.wrapping_sub(1) % 5).min(4);
        let (area, layout, room) =
            screen
                .tables
                .initial_view(group, game.id.room, game.camera.current_cut as u8);
        screen.area = area;
        screen.layout = layout;
        screen.room = room;
        screen
    }

    fn load_backdrop(&self, pack: &Pack) -> Option<Texture8> {
        match load_indexed(pack, BLUE_STEM) {
            Ok(texture) => Some(texture),
            Err(err) => {
                eprintln!("warning: no map backdrop: {err:#}; the map tab stays blank");
                None
            }
        }
    }

    /// Load the browser art. The original loads one page for the whole
    /// browser: the file of the availability mask's *highest* set layout bit
    /// (`map_tab_load_map` scans all four bits and keeps the last hit), and
    /// reloads it only when that layout changes.
    fn load_art(&mut self, pack: &Pack, flags: &[game::FlagBank]) {
        let mask = self.tables.layout_mask(flags);
        let layout = (0..4).rev().find(|&i| mask & (1 << i) != 0).unwrap_or(0);
        if self.art_layout == layout as u8 {
            return;
        }
        let stem = assets::stem(TAB_ENTRIES[layout]);
        self.art = match load_indexed(pack, stem) {
            Ok(texture) => Some(texture),
            Err(err) => {
                eprintln!("warning: no map page {stem}: {err:#}; the map tab stays blank");
                None
            }
        };
        self.art_layout = layout as u8;
    }

    /// Load the full-display plan of `area` on first use.
    fn load_display(&mut self, pack: &Pack, area: usize) {
        if self.display[area].is_none() {
            let stem = assets::stem(AREA_ENTRIES[area]);
            self.display[area] = match load_indexed(pack, stem) {
                Ok(texture) => Some(texture),
                Err(err) => {
                    eprintln!("warning: no map plan {stem}: {err:#}; the map display stays blank");
                    None
                }
            };
        }
    }

    /// The current room index inside the layout.
    pub fn room_index(&self) -> u8 {
        self.room[usize::from(self.layout)]
    }

    /// The current area.
    pub fn area(&self) -> u8 {
        self.area
    }

    /// Whether the full floor plan is shown.
    pub fn zoomed(&self) -> bool {
        self.zoomed
    }

    /// The screen rect the current view covers, for the panel decorations the
    /// original draws over it.
    pub fn content_rect(&self) -> layout::Rect {
        if self.zoomed {
            layout::Rect {
                pos: [48, 28],
                size: [128, 100],
            }
        } else {
            BROWSER_RECT
        }
    }

    /// The current area's group.
    fn group(&self) -> usize {
        usize::from(self.tables.group_for_area(self.area))
    }

    /// Advance the blink phase.
    ///
    /// The first update after the tab opens is the browser's first drawn
    /// frame: the original runs `map_tab_update` for the first time on the
    /// frame after the confirm, so the counter's first step is its first
    /// frame's `1` (`MapScreen::started`).
    pub fn tick(&mut self) {
        if !self.started {
            self.started = true;
            return;
        }
        self.ticks = self.ticks.saturating_add(1);
    }

    /// Select the area under the current layout position.
    fn refresh_area(&mut self) {
        let area = self
            .tables
            .area_at(usize::from(self.layout), usize::from(self.room_index()));
        if area != 0xFF {
            self.area = area;
        }
    }

    /// One input from the pad.
    pub fn handle_input(&mut self, game: &GameState, input: MenuInput) -> MapEvent {
        match input {
            MenuInput::Cancel => {
                if self.zoomed {
                    self.zoomed = false;
                    MapEvent::Changed
                } else {
                    MapEvent::Close
                }
            }
            MenuInput::Confirm => {
                self.zoomed = !self.zoomed;
                MapEvent::Changed
            }
            MenuInput::Up | MenuInput::Down => {
                if self.zoomed {
                    return MapEvent::None;
                }
                let layout = usize::from(self.layout);
                let mut room = self.room[layout];
                let move_flags = self.tables.move_flags(&game.flags, layout, room);
                if input == MenuInput::Up && move_flags & 1 != 0 {
                    room += 1;
                } else if input == MenuInput::Down && move_flags & 2 != 0 {
                    room -= 1;
                } else {
                    return MapEvent::None;
                }
                self.room[layout] = room;
                self.refresh_area();
                MapEvent::Changed
            }
            MenuInput::Left | MenuInput::Right => {
                if self.zoomed {
                    return MapEvent::None;
                }
                let mask = self.tables.layout_mask(&game.flags);
                let current = self.layout;
                // The D-pad branches read the raw `pad_held_byte1()` bits:
                // 0x80 is the *left* d-pad and 0x20 the right one, whatever
                // `map_tab_navigate`'s own comments call them. Left therefore
                // steps to the next layout and right to the previous one.
                let next = if input == MenuInput::Left {
                    if mask & 8 != 0 {
                        // All four layouts: the original wraps in pairs.
                        match current {
                            0 => 3,
                            1 => 2,
                            2 => 2,
                            3 => 1,
                            _ => current,
                        }
                    } else {
                        let next = current + 1;
                        if current == 2 || mask & (1 << next) == 0 {
                            current
                        } else {
                            next
                        }
                    }
                } else if mask & 8 != 0 {
                    match current {
                        0 | 3 => 0,
                        1 => 3,
                        2 => 1,
                        _ => current,
                    }
                } else if current != 0 && mask & (1 << (current - 1)) != 0 {
                    current - 1
                } else {
                    current
                };
                if next == current {
                    return MapEvent::None;
                }
                self.layout = next;
                self.refresh_area();
                MapEvent::Changed
            }
            _ => MapEvent::None,
        }
    }

    /// The full-display plan image: the blue grid backdrop (`backdrop`, drawn
    /// with its index-0 transparent key) with `plan` over it. The plan's
    /// unvisited room markers stay transparent (the grid shows), the current
    /// room's marker is the original's red highlight (`0x001f` BGR555), and
    /// everything else keeps its palette colour. Both layers draw at the
    /// original's half brightness (`colorMul` 0x80).
    pub fn display_image(
        &self,
        plan: &Texture8,
        backdrop: Option<&Texture8>,
        flags: &[game::FlagBank],
    ) -> Image {
        let width = plan.width;
        let height = plan.height;
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        let dim = |channel: u8| (u16::from(channel) * 0x80 / 0xFF) as u8;
        if let Some(backdrop) = backdrop {
            // The blue grid draws as a solid layer (its index 0 is the dark
            // blue square colour, not the transparent key); the plan over it
            // is the layer that carries the key.
            for (pixel, &index) in rgba.chunks_mut(4).zip(backdrop.indices.iter()) {
                let [r, g, b, _] = backdrop.palette(0, index);
                pixel.copy_from_slice(&[dim(r), dim(g), dim(b), 255]);
            }
        }
        let group = self.group();
        let room_count = usize::from(self.tables.room_counts[group]);
        let layout = usize::from(self.layout);
        let highlight = (self.tables.area_at(layout, usize::from(self.room_index())) == self.area)
            .then(|| self.room_index());
        for (pixel, &index) in rgba.chunks_mut(4).zip(plan.indices.iter()) {
            if index == 0 {
                continue;
            }
            let index = usize::from(index);
            let colour = match index.checked_sub(13).map(|offset| offset / 6) {
                Some(room) if room < room_count => {
                    if highlight == Some(room as u8) {
                        [0xF8, 0, 0, 255]
                    } else if self.tables.room_visited(flags, group, room) {
                        plan.palette(0, index as u8)
                    } else {
                        continue;
                    }
                }
                _ => plan.palette(0, index as u8),
            };
            pixel.copy_from_slice(&[dim(colour[0]), dim(colour[1]), dim(colour[2]), 255]);
        }
        Image {
            width,
            height,
            rgba,
        }
    }

    /// Build the map browser's composite: the loaded layout art's base window,
    /// the marker of every available layout (the current one at full
    /// brightness, the rest at half), the current area's name row, the current
    /// floor's number glyph and the room-step arrows.
    ///
    /// Every sprite's index-0 texels are transparent (the layer beneath shows
    /// through), the arrows blink between the two frames their `texU` selects,
    /// and the floor glyph disappears when its `texV` runs past the page - the
    /// original's `texV` is a byte, so the minus-one room offset wraps to
    /// `0xF0` and the page search rejects the sprite.
    pub fn browser_image(
        &self,
        art: &Texture8,
        flags: &[game::FlagBank],
        width: u32,
        height: u32,
    ) -> Image {
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        self.blit_sprite(&mut rgba, width, art, BASE_RECT, MUL_FULL);
        let mask = self.tables.layout_mask(flags);
        for (layout, rect) in BROWSER_SPRITES[..4].iter().enumerate() {
            if mask & (1 << layout) == 0 {
                continue;
            }
            let mul = if usize::from(self.layout) == layout {
                MUL_FULL
            } else {
                MUL_HALF
            };
            self.blit_sprite(&mut rgba, width, art, *rect, mul);
        }
        let name = SpriteRect {
            v: self.layout.wrapping_mul(0x10).wrapping_add(0x48),
            ..BROWSER_SPRITES[4]
        };
        self.blit_sprite(&mut rgba, width, art, name, MUL_FULL);
        let offset = self
            .tables
            .layout_offset
            .get(usize::from(self.layout))
            .copied()
            .unwrap_or(0);
        let glyph = SpriteRect {
            v: 6u8
                .wrapping_sub(self.room_index())
                .wrapping_sub(offset)
                .wrapping_mul(0x10),
            ..BROWSER_SPRITES[5]
        };
        self.blit_sprite(&mut rgba, width, art, glyph, MUL_FULL);
        // The blink phase is the original's `MAP_HL_COUNT`: bumped once per
        // browser frame, bit 4 set for the dim half of its 64-frame cycle.
        let blink = (self.ticks.wrapping_add(1) & 0x30) != 0;
        let move_flags = self
            .tables
            .move_flags(flags, usize::from(self.layout), self.room_index());
        for (bit, rect) in [(1u8, BROWSER_SPRITES[6]), (2, BROWSER_SPRITES[7])] {
            if move_flags & bit == 0 {
                continue;
            }
            let arrow = SpriteRect {
                u: rect.u.wrapping_add(u8::from(blink) * 8),
                ..rect
            };
            self.blit_sprite(&mut rgba, width, art, arrow, MUL_FULL);
        }
        Image {
            width,
            height,
            rgba,
        }
    }

    /// Blit one browser sprite from `art` into an RGBA composite.
    ///
    /// The original's `DisplayTexture` page search rejects a sprite whose rect
    /// leaves the loaded page, which is what makes the wrapped floor-glyph
    /// `texV` disappear; the tile is drawn with the index-0 colour key and the
    /// `colorMul` scale everything in the map browser carries.
    fn blit_sprite(&self, rgba: &mut [u8], width: u32, art: &Texture8, rect: SpriteRect, mul: u8) {
        if rect.u as i32 + rect.w > art.width as i32 || rect.v as i32 + rect.h > art.height as i32 {
            return;
        }
        let height = rgba.len() as u32 / (width * 4);
        for row in 0..rect.h {
            let y = rect.y + row;
            if y < 0 || y >= height as i32 {
                continue;
            }
            let source = (rect.v as u32 + row as u32) * art.width;
            for column in 0..rect.w {
                let x = rect.x + column;
                if x < 0 || x >= width as i32 {
                    continue;
                }
                let index = art.indices[(source + rect.u as u32 + column as u32) as usize];
                if index == 0 {
                    continue;
                }
                let [r, g, b, _] = art.palette(0, index);
                let offset = ((y as u32 * width + x as u32) * 4) as usize;
                rgba[offset..offset + 4].copy_from_slice(&[
                    tint(r, mul),
                    tint(g, mul),
                    tint(b, mul),
                    255,
                ]);
            }
        }
    }

    /// Draw the tab over the frozen menu frame.
    ///
    /// The menu underneath already draws the panel and the tab row; the
    /// browser composites the layout art's sprites over it, and the engine
    /// re-draws the panel decorations the original paints over them
    /// (`MainMenu::draw_frames_over`).
    pub fn draw(&mut self, framebuffer: &mut Framebuffer, pack: &Pack, game: &GameState) {
        if self.zoomed {
            // The full display: the blue grid backdrop with the area's own
            // 128x100 floor plan over it, both at the original's half
            // brightness (`colorMul` 0x80). The zoom sprites park at
            // screenX 0x80/screenY 0x4e with pivot (0x50, 0x32) and scale
            // 0x1000 (1.0), i.e. the plan lands at (48, 28).
            let area = usize::from(self.area).min(AREA_ENTRIES.len() - 1);
            self.load_display(pack, area);
            if let Some(plan) = self.display[area].as_ref() {
                let image = self.display_image(plan, self.backdrop.as_ref(), &game.flags);
                let width = image.width.min(128) as i32;
                let height = image.height.min(100) as i32;
                framebuffer.draw_rgba_sprite(
                    &image,
                    [0, 0, width, height],
                    [48, 28, width, height],
                    255,
                );
            }
            return;
        }
        // The browser composites over the frozen inventory panel the menu
        // drew; the panel's own frame and decorations stay visible where the
        // layout art's index-0 texels are transparent.
        if !self.started {
            return;
        }
        self.load_art(pack, &game.flags);
        let Some(art) = self.art.as_ref() else {
            return;
        };
        let image = self.browser_image(art, &game.flags, framebuffer.width, framebuffer.height);
        framebuffer.draw_rgba_sprite(
            &image,
            [0, 0, image.width as i32, image.height as i32],
            [0, 0, image.width as i32, image.height as i32],
            255,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::FlagBank;

    fn tables() -> MapTables {
        MapTables::default()
    }

    #[test]
    fn tables_round_trip_through_the_blob() {
        let tables = tables();
        let bytes = tables.encode();
        assert_eq!(bytes.len(), TABLES_BYTES);
        assert_eq!(MapTables::parse(&bytes).unwrap(), tables);
        assert!(MapTables::parse(&bytes[..TABLES_BYTES - 1]).is_err());
        let mut corrupt = bytes;
        corrupt[0] = b'X';
        assert!(MapTables::parse(&corrupt).is_err());
    }

    #[test]
    fn area_table_maps_layout_rooms_to_areas() {
        let tables = tables();
        assert_eq!(tables.area_at(0, 0), 2);
        assert_eq!(tables.area_at(0, 1), 0);
        assert_eq!(tables.area_at(0, 2), 1);
        assert_eq!(tables.area_at(0, 3), 0xFF);
        assert_eq!(tables.area_at(3, 0), 10);
        assert_eq!(tables.area_at(3, 3), 7);
        assert_eq!(tables.layout_for_area(0), 0);
        assert_eq!(tables.layout_for_area(7), 3);
        assert_eq!(tables.group_for_area(6), 3);
    }

    #[test]
    fn owned_map_areas_use_the_documented_bits() {
        let tables = tables();
        let mut flags = vec![FlagBank::default(); 16];
        let bit = |index: u8| game::ROOM_FLAG_MAP_BASE + index;
        assert!(!tables.area_known(&flags, 0));
        flags[usize::from(game::BANK_ROOM_FLAGS)].apply(bit(0), 0);
        assert!(tables.area_known(&flags, 0));
        assert!(!tables.area_known(&flags, 1));
        flags[usize::from(game::BANK_ROOM_FLAGS)].apply(bit(1), 0);
        assert!(tables.area_known(&flags, 1));
        flags[usize::from(game::BANK_ROOM_FLAGS)].apply(bit(4), 0);
        assert!(tables.area_known(&flags, 5) && tables.area_known(&flags, 6));
        assert!(!tables.area_known(&flags, 2));
        assert!(!tables.area_known(&flags, 7));
        assert!(!tables.area_known(&flags, 10));
        assert_eq!(MapTables::map_index(2), None);
        assert_eq!(MapTables::map_index(9), Some(5));
    }

    #[test]
    fn initial_view_follows_the_start_room_and_camera_cases() {
        let tables = tables();
        // Room 1001: stage 1 room 1, first camera -> Mansion 1F layout 0.
        let (area, layout, index) = tables.initial_view(0, 0x01, 0);
        assert_eq!((area, layout), (0, 0));
        assert_eq!(index[0], 1);
        // Gallery cameras 3-6 pin the room index to 0x1D's slot.
        let (area, _, _) = tables.initial_view(0, 0x07, 4);
        assert_eq!(area, 0);
        // Lesson room cameras 4-7 move to the 1F map.
        let (area, layout, _) = tables.initial_view(1, 0x0C, 5);
        assert_eq!((area, layout), (0, 0));
        // Guardhouse splits at the water tank entry.
        assert_eq!(tables.initial_view(3, 0x0C, 0).0, 5);
        assert_eq!(tables.initial_view(3, 0x0D, 0).0, 6);
        // Lab B4 areas.
        assert_eq!(tables.initial_view(4, 0x13, 0).0, 10);
        assert_eq!(tables.initial_view(4, 0x07, 0).0, 9);
        assert_eq!(tables.initial_view(4, 0x01, 0).0, 7);
        assert_eq!(tables.initial_view(4, 0x03, 0).0, 8);
    }

    #[test]
    fn area_and_layout_masks_follow_the_visited_rooms() {
        let tables = tables();
        let mut flags = vec![FlagBank::default(); 16];
        // Room 1 of group 0: area 0 -> layout 0.
        flags[usize::from(game::BANK_ROOM_FLAGS)].apply(1, 0);
        assert_eq!(tables.area_mask(&flags), 1);
        assert_eq!(tables.layout_mask(&flags), 1);
        // A 2F room in the B1 range moves area 2 (layout 0 again).
        let base = u32::from(tables.stage_room_offsets[1]);
        flags[usize::from(game::BANK_ROOM_FLAGS)].apply(base as u8 + 0x1A, 0);
        assert_eq!(tables.area_mask(&flags), 1 | (1 << 2));
    }

    #[test]
    fn move_flags_gate_on_the_neighbouring_area() {
        let tables = tables();
        let mut flags = vec![FlagBank::default(); 16];
        let bank = usize::from(game::BANK_ROOM_FLAGS);
        // Room 1 (area 0) visited: room 2 (area 1) is not in the mask yet,
        // so there is no right step.
        flags[bank].apply(1, 0);
        let mask = tables.move_flags(&flags, 0, 1);
        assert_eq!(mask & 1, 0, "area 1 is not reachable yet");
        // Visit a group-1 room in area 1: the right step opens.
        let base = u32::from(tables.stage_room_offsets[1]);
        flags[bank].apply(base as u8, 0);
        let mask = tables.move_flags(&flags, 0, 1);
        assert_ne!(mask & 1, 0, "room 2 is reachable through area 1");
        // Room 0 (area 2) is not in the mask, so there is no left step.
        assert_eq!(mask & 2, 0, "room 0 is not reachable yet");
        // A visited group-1 room past the B1 passage base puts area 2 in the
        // mask, and the left step opens.
        flags[bank].apply(base as u8 + 0x1A, 0);
        let mask = tables.move_flags(&flags, 0, 1);
        assert_ne!(mask & 2, 0, "the visited area 2 unlocks room 0");
    }

    #[test]
    fn display_image_dims_the_grid_and_reddens_the_current_room() {
        // A synthetic plan: indices 0 (transparent key), 13 (room 0's marker)
        // and 200 (an ordinary palette index). The grid backdrop's index 0 is
        // a solid colour (it is not the plan's key).
        let mut palettes = vec![[0u8; 4]; crate::model::PALETTE_ROW_LEN];
        palettes[0] = [255, 255, 255, 255];
        palettes[13] = [80, 80, 80, 255];
        palettes[200] = [200, 100, 50, 255];
        let plan = Texture8 {
            width: 4,
            height: 1,
            indices: vec![0, 13, 200, 13],
            palettes: palettes.clone(),
            stp: Vec::new(),
        };
        let mut grid_palettes = vec![[0u8; 4]; crate::model::PALETTE_ROW_LEN];
        grid_palettes[0] = [16, 0, 96, 255];
        grid_palettes[1] = [0, 72, 0, 255];
        let grid = Texture8 {
            width: 4,
            height: 1,
            indices: vec![0, 1, 0, 1],
            palettes: grid_palettes,
            stp: Vec::new(),
        };
        let mut flags = vec![FlagBank::default(); 16];
        let screen = MapScreen {
            tables: tables(),
            layout: 0,
            room: [0, 1, 1, 3],
            area: 2,
            zoomed: true,
            art: None,
            art_layout: 0xFF,
            started: true,
            display: Default::default(),
            backdrop: None,
            ticks: 0,
        };
        let image = screen.display_image(&plan, Some(&grid), &flags);
        let dim = |c: u8| (u16::from(c) * 0x80 / 0xFF) as u8;
        // The keyed plan pixel shows the grid square (dimmed), not black.
        assert_eq!(image.rgba[0..4], [dim(16), 0, dim(96), 255]);
        // The current room's marker (layout 0 room 0 is area 2) is red.
        assert_eq!(image.rgba[4..8], [dim(0xF8), 0, 0, 255]);
        // Ordinary indices keep their palette colour, dimmed.
        assert_eq!(image.rgba[8..12], [dim(200), dim(100), dim(50), 255]);
        // The grid line shows through the unvisited marker of room 0 when the
        // current area is not this layout's room area.
        let screen = MapScreen { area: 0, ..screen };
        let image = screen.display_image(&plan, Some(&grid), &flags);
        assert_eq!(image.rgba[4..8], [dim(0), dim(72), 0, 255]);
        // A visited marker keeps its palette colour.
        flags[usize::from(game::BANK_ROOM_FLAGS)].apply(0, 0);
        let image = screen.display_image(&plan, Some(&grid), &flags);
        assert_eq!(image.rgba[4..8], [dim(80), dim(80), dim(80), 255]);
    }

    /// A synthetic browser art: every texel carries index 1, and the probe
    /// texels listed in `probes` carry index 2.
    fn browser_art(probes: &[(u32, u32)]) -> Texture8 {
        let mut palettes = vec![[0u8; 4]; crate::model::PALETTE_ROW_LEN];
        palettes[1] = [100, 120, 140, 255];
        palettes[2] = [200, 100, 50, 255];
        let mut indices = vec![1u8; 256 * 160];
        for &(x, y) in probes {
            indices[(y * 256 + x) as usize] = 2;
        }
        Texture8 {
            width: 256,
            height: 160,
            indices,
            palettes,
            stp: Vec::new(),
        }
    }

    fn browser_screen(layout: u8, room: [u8; 4], ticks: u32) -> MapScreen {
        MapScreen {
            tables: tables(),
            layout,
            room,
            area: 0,
            zoomed: false,
            art: None,
            art_layout: 0xFF,
            started: true,
            display: Default::default(),
            backdrop: None,
            ticks,
        }
    }

    #[test]
    fn browser_composite_draws_the_reference_layers() {
        let full = [100, 120, 140, 255];
        let half = [
            tint(100, MUL_HALF),
            tint(120, MUL_HALF),
            tint(140, MUL_HALF),
            255,
        ];
        let probe = [200, 100, 50, 255];
        // Probe the base window, marker 0, the name row and room 1's glyph.
        let art = browser_art(&[
            (BASE_RECT.u as u32, BASE_RECT.v as u32),
            (0xA0, 0x00),
            (0x78, 0x48),
            (0xE8, 0x10),
        ]);
        // Areas 0/1 (layout 0) and 8/9 (layout 3) owned: the mask has layouts
        // 0 and 3, so both markers draw and the layout 3 marker is dim.
        let mut flags = vec![FlagBank::default(); 16];
        let bank = usize::from(game::BANK_ROOM_FLAGS);
        flags[bank].apply(game::ROOM_FLAG_MAP_BASE, 0);
        flags[bank].apply(game::ROOM_FLAG_MAP_BASE + 1, 0);
        flags[bank].apply(game::ROOM_FLAG_MAP_BASE + 5, 0);
        let screen = browser_screen(0, [1, 1, 1, 3], 0);
        let image = screen.browser_image(&art, &flags, 320, 240);
        let pixel = |x: u32, y: u32| {
            let offset = ((y * 320 + x) * 4) as usize;
            let mut px = [0u8; 4];
            px.copy_from_slice(&image.rgba[offset..offset + 4]);
            px
        };
        // The base window's probe draws at its rect's corner: full brightness.
        assert_eq!(pixel(33, 32), probe);
        // The current layout's marker covers the right of the window (only
        // marker 0 reaches (170,32)), at full brightness.
        assert_eq!(pixel(170, 32), full);
        // Marker 3 draws over marker 0 at the same rect corner with the dim
        // scale, so the last drawn available layout's marker wins there.
        assert_eq!(pixel(121, 32), half);
        // The area-name row is the layout's row of the art (`0x48 + layout *
        // 0x10`); room 1's glyph is `(6 - 1 - 4) * 0x10` into the art.
        assert_eq!(pixel(57, 104), probe);
        assert_eq!(pixel(137, 104), probe);
        // Nothing is drawn outside the sprites' rects.
        assert_eq!(pixel(300, 200), [0, 0, 0, 0]);
        // The room-step arrows follow the move flags: layout 0 room 1 has the
        // up step (area 1 is owned) but no down step.
        let art = browser_art(&[(0xA0, 0x40)]);
        let image = screen.browser_image(&art, &flags, 320, 240);
        let offset = ((96 * 320 + 145) * 4) as usize;
        assert_eq!(&image.rgba[offset..offset + 4], &probe, "the up arrow");
        let offset = ((120 * 320 + 145) * 4) as usize;
        assert_eq!(
            &image.rgba[offset..offset + 4],
            &[0, 0, 0, 0],
            "no down arrow"
        );
        // The blink's second frame shifts the arrow art 8 texels right: the
        // bright half of the 64-frame cycle samples `0xA0` (the plain texel),
        // the dim half `0xA8` (probed above).
        let art = browser_art(&[(0xA8, 0x40)]);
        let image = browser_screen(0, [1, 1, 1, 3], 14).browser_image(&art, &flags, 320, 240);
        let offset = ((96 * 320 + 145) * 4) as usize;
        assert_eq!(&image.rgba[offset..offset + 4], &full, "the bright frame");
        let image = browser_screen(0, [1, 1, 1, 3], 15).browser_image(&art, &flags, 320, 240);
        let offset = ((96 * 320 + 145) * 4) as usize;
        assert_eq!(&image.rgba[offset..offset + 4], &probe, "the dim frame");
    }

    #[test]
    fn browser_glyph_wraps_out_of_the_page() {
        // Layout 3's room index starts at 3, so the glyph's `texV` wraps to
        // 0xF0 and the original's page search rejects the sprite. The glyph
        // rect is below the base window, so nothing at all is drawn there.
        let art = browser_art(&[]);
        let mut flags = vec![FlagBank::default(); 16];
        flags[usize::from(game::BANK_ROOM_FLAGS)].apply(game::ROOM_FLAG_MAP_BASE + 5, 0);
        let screen = browser_screen(3, [1, 1, 1, 3], 0);
        let image = screen.browser_image(&art, &flags, 320, 240);
        let offset = ((104 * 320 + 137) * 4) as usize;
        assert_eq!(&image.rgba[offset..offset + 4], &[0, 0, 0, 0]);
        // Room 2's glyph (`texV` 0x00) is inside the page and draws.
        let art = browser_art(&[(0xE8, 0x00)]);
        let screen = browser_screen(3, [1, 1, 1, 2], 0);
        let image = screen.browser_image(&art, &flags, 320, 240);
        let offset = ((104 * 320 + 137) * 4) as usize;
        assert_eq!(&image.rgba[offset..offset + 4], &[200, 100, 50, 255]);
    }

    #[test]
    fn navigation_steps_rooms_and_layouts() {
        let tables = tables();
        let mut game = GameState::default();
        let bank = usize::from(game::BANK_ROOM_FLAGS);
        // Room 1 of group 0 (area 0) and a group-1 room in area 1 make both
        // layout-0 steps reachable.
        game.flags[bank].apply(1, 0);
        game.flags[bank].apply(u32::from(tables.stage_room_offsets[1]) as u8, 0);
        let mut screen = MapScreen {
            tables,
            layout: 0,
            room: [1, 1, 1, 3],
            area: 0,
            zoomed: false,
            art: None,
            art_layout: 0xFF,
            started: true,
            display: Default::default(),
            backdrop: None,
            ticks: 0,
        };
        assert_eq!(screen.room_index(), 1);
        assert_eq!(screen.handle_input(&game, MenuInput::Up), MapEvent::Changed);
        assert_eq!(screen.room_index(), 2);
        assert_eq!(screen.area(), 1);
        assert_eq!(
            screen.handle_input(&game, MenuInput::Up),
            MapEvent::None,
            "room 2 is the last real room"
        );
        assert_eq!(
            screen.handle_input(&game, MenuInput::Down),
            MapEvent::Changed
        );
        assert_eq!(screen.room_index(), 1);
        // Confirm enters the full display; cancel leaves it, then closes.
        assert_eq!(
            screen.handle_input(&game, MenuInput::Confirm),
            MapEvent::Changed
        );
        assert!(screen.zoomed());
        assert_eq!(
            screen.handle_input(&game, MenuInput::Cancel),
            MapEvent::Changed
        );
        assert!(!screen.zoomed());
        assert_eq!(
            screen.handle_input(&game, MenuInput::Cancel),
            MapEvent::Close
        );
    }
}
