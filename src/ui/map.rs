//! The pause menu's map tab.
//!
//! The tab is gated by the same scenario flag the radio tab reads: without the
//! radio the map tab is inert. With it, the tab browser draws the selected
//! floor plan (`map/map0d.tim`, `map0c`, `map00` or `map0e`, one per available
//! layout) and tints its room markers from the visited-room bits in the
//! room-flags bank: unvisited rooms are hidden, the current room is
//! highlighted. Up/down step the room index inside the current layout (only
//! when the neighbouring area is known), left/right step between the layouts
//! the owned maps unlock. Confirm switches to the full floor-plan view of the
//! current area; cancel returns to the browser and then closes the tab.
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

use crate::game::{self, GameState};
use crate::model::Texture8;
use crate::pack::Pack;
use crate::render::Framebuffer;
use crate::state::Image;
use crate::tim;

use super::main_menu::MenuInput;

/// Pack entry of the extracted map tables.
pub const TABLES_ENTRY: &str = "map/tables.bin";
/// Pack entry of the map backdrop.
pub const BLUE_ENTRY: &str = "map/blue.tim";
/// The four browser floor-plan entries, indexed by layout.
pub const TAB_ENTRIES: [&str; 4] = ["map/map0d.tim", "map/map0c.tim", "map/map00.tim", "map/map0e.tim"];
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
            area: [2, 0, 1, 0xFF, 4, 3, 0xFF, 0xFF, 6, 5, 0xFF, 0xFF, 10, 9, 8, 7],
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
        self.area
            .get(layout * 4 + room)
            .copied()
            .unwrap_or(0xFF)
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
                if room == 0x07 && (2..7).contains(&cut) {
                    0
                } else if room == 0x0F && (cut == 3 || cut == 4) {
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
                if room == 0x0B && (4..7).contains(&cut) {
                    4
                } else if room == 0x0F && cut == 6 {
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
        for layout in 0..4 {
            for room in 0..4 {
                if self.area_at(layout, room) == area {
                    index[layout] = room as u8;
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
            let next = self.area_at(layout, usize::from(room) + 1);
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
    /// The browser floor plans, lazily loaded and keyed by layout.
    plans: [Option<Texture8>; 4],
    /// The map backdrop.
    backdrop: Option<Texture8>,
    /// Blink phase for the cursor arrows.
    ticks: u32,
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
            plans: Default::default(),
            backdrop: None,
            ticks: 0,
        };
        screen.backdrop = screen.load_backdrop(pack);
        let group = usize::from(game.id.stage.wrapping_sub(1) % 5).min(4);
        let (area, layout, room) = screen
            .tables
            .initial_view(group, game.id.room, game.camera.current_cut as u8);
        screen.area = area;
        screen.layout = layout;
        screen.room = room;
        screen
    }

    fn load_backdrop(&self, pack: &Pack) -> Option<Texture8> {
        match pack.read(BLUE_ENTRY) {
            Ok(bytes) => match tim::decode_8bpp(bytes) {
                Ok(texture) => Some(texture),
                Err(err) => {
                    eprintln!("warning: invalid {BLUE_ENTRY}: {err:#}");
                    None
                }
            },
            Err(err) => {
                eprintln!("warning: missing {BLUE_ENTRY}: {err:#}; the map tab stays blank");
                None
            }
        }
    }

    /// Load the browser plan of `layout` on first use.
    fn load_plan(&mut self, pack: &Pack, layout: usize) {
        if self.plans[layout].is_none() {
            let entry = TAB_ENTRIES[layout];
            self.plans[layout] = match pack.read(entry) {
                Ok(bytes) => match tim::decode_8bpp(bytes) {
                    Ok(texture) => Some(texture),
                    Err(err) => {
                        eprintln!("warning: invalid {entry}: {err:#}");
                        None
                    }
                },
                Err(err) => {
                    eprintln!("warning: missing {entry}: {err:#}; the map tab stays blank");
                    None
                }
            };
        }
    }

    /// The current layout.
    pub fn layout(&self) -> u8 {
        self.layout
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

    /// The current area's group.
    fn group(&self) -> usize {
        usize::from(self.tables.group_for_area(self.area))
    }

    /// Advance the blink phase.
    pub fn tick(&mut self) {
        self.ticks = self.ticks.saturating_add(1);
    }

    /// Select the area under the current layout position.
    fn refresh_area(&mut self) {
        let area = self.tables.area_at(usize::from(self.layout), usize::from(self.room_index()));
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
                    return MapEvent::Changed;
                }
                return MapEvent::Close;
            }
            MenuInput::Confirm => {
                self.zoomed = !self.zoomed;
                return MapEvent::Changed;
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
                return MapEvent::Changed;
            }
            MenuInput::Left | MenuInput::Right => {
                if self.zoomed {
                    return MapEvent::None;
                }
                let mask = self.tables.layout_mask(&game.flags);
                let current = self.layout;
                let next = if input == MenuInput::Right {
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
                return MapEvent::Changed;
            }
            _ => return MapEvent::None,
        }
    }

    /// Build the tinted RGBA image of `texture`'s markers: unvisited rooms are
    /// transparent, the current room is highlighted, everything else keeps its
    /// palette colour.
    pub fn plan_image(&self, texture: &Texture8, flags: &[game::FlagBank], layout: usize) -> Image {
        let group = self.group();
        let room_count = usize::from(self.tables.room_counts[group]);
        // The highlighted marker is the layout position when it matches the
        // current area, otherwise no marker is blanked (the original's
        // highlight page only shows the selected room).
        let highlight = if self.tables.area_at(layout, usize::from(self.room_index())) == self.area {
            Some(self.room_index())
        } else {
            None
        };
        let mut rgba = Vec::with_capacity(texture.indices.len() * 4);
        for &index in &texture.indices {
            if index == 0 {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let index = usize::from(index);
            let room = index.checked_sub(13).map(|offset| offset / 6);
            match room {
                Some(room) if room < room_count => {
                    if highlight == Some(room as u8) {
                        rgba.extend_from_slice(&[0x1F, 0x1F, 0x1F, 255]);
                    } else if self.tables.room_visited(flags, group, room) {
                        rgba.extend_from_slice(&texture.palette(0, index as u8));
                    } else {
                        rgba.extend_from_slice(&[0, 0, 0, 0]);
                    }
                }
                _ => rgba.extend_from_slice(&texture.palette(0, index as u8)),
            }
        }
        Image {
            width: texture.width,
            height: texture.height,
            rgba,
        }
    }

    /// Draw the tab over the frozen menu frame.
    ///
    /// The menu underneath already draws the tab row; this paints the blue
    /// backdrop and the tinted floor plan into the middle band.
    pub fn draw(&mut self, framebuffer: &mut Framebuffer, pack: &Pack, game: &GameState) {
        // The blue backdrop frames the plan; both are drawn into the middle
        // band the original's browser uses.
        if let Some(backdrop) = &self.backdrop {
            let image = backdrop_to_image(backdrop);
            framebuffer.draw_rgba_sprite(
                &image,
                [0, 0, image.width as i32, image.height as i32],
                [96, 56, 128, 128],
                255,
            );
        }
        let layout = usize::from(self.layout);
        let width = framebuffer.width as i32;
        self.load_plan(pack, layout);
        let Some(plan) = self.plans[layout].as_ref() else {
            return;
        };
        let image = self.plan_image(plan, &game.flags, layout);
        let x = (width - image.width as i32) / 2;
        let y = (framebuffer.height as i32 - image.height as i32) / 2;
        framebuffer.draw_rgba_sprite(
            &image,
            [0, 0, image.width as i32, image.height as i32],
            [x, y, image.width as i32, image.height as i32],
            255,
        );
    }
}

/// An indexed texture's full-page RGBA image at its own palette row 0.
fn backdrop_to_image(texture: &Texture8) -> Image {
    let mut rgba = Vec::with_capacity(texture.indices.len() * 4);
    for &index in &texture.indices {
        rgba.extend_from_slice(&texture.palette(0, index));
    }
    Image {
        width: texture.width,
        height: texture.height,
        rgba,
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
    fn plan_tint_hides_unvisited_and_highlights_the_current_room() {
        // A synthetic plan: palette entries 13..18 are room 0's six marker
        // colours, indices 13 and 14 occur in the pixel data.
        let mut palettes = vec![[0u8; 4]; crate::model::PALETTE_ROW_LEN];
        palettes[13] = [10, 20, 30, 255];
        palettes[14] = [40, 50, 60, 255];
        let texture = Texture8 {
            width: 4,
            height: 1,
            indices: vec![0, 13, 14, 200],
            palettes,
            stp: Vec::new(),
        };
        let tables = tables();
        let mut flags = vec![FlagBank::default(); 16];
        let screen = |flags: &[FlagBank], area: u8, layout: u8, room: [u8; 4]| {
            let screen = MapScreen {
                tables,
                layout,
                room,
                area,
                zoomed: false,
                plans: Default::default(),
                backdrop: None,
                ticks: 0,
            };
            screen.plan_image(&texture, flags, usize::from(layout))
        };
        // Layout 0 room 0 is area 2, so with area 0 selected nothing is
        // highlighted and the unvisited room-0 markers are transparent.
        let image = screen(&flags, 0, 0, [0, 1, 1, 3]);
        assert_eq!(image.rgba[4..8], [0, 0, 0, 0], "index 13 is a hidden marker");
        assert_eq!(image.rgba[8..12], [0, 0, 0, 0], "index 14 is a hidden marker");
        assert_eq!(image.rgba[12..16], [0, 0, 0, 0], "palette index 200 is black");
        // Visited: the marker shows its palette colour.
        flags[usize::from(game::BANK_ROOM_FLAGS)].apply(0, 0);
        let image = screen(&flags, 0, 0, [0, 1, 1, 3]);
        assert_eq!(image.rgba[4..8], [10, 20, 30, 255]);
        // With area 2 current, the layout's room 0 marker is highlighted.
        let image = screen(&flags, 2, 0, [0, 1, 1, 3]);
        assert_eq!(image.rgba[4..8], [0x1F, 0x1F, 0x1F, 255]);
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
            plans: Default::default(),
            backdrop: None,
            ticks: 0,
        };
        assert_eq!(screen.room_index(), 1);
        assert_eq!(
            screen.handle_input(&game, MenuInput::Up),
            MapEvent::Changed
        );
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
