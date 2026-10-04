//! FILE tab: the two-book document selector and the JPN page reader.
//!
//! The tab shows `file000.tim`/`file001.tim` as book covers, each with eight
//! document slots whose icons and cursor are baked into the same cover page.
//! A document is listed once its FILE-collected bit is raised by the pickup
//! (room-flags bank 8, `0x82 + (item - 0x5F)`). Confirming a listed document
//! opens the reader: the JPN build draws a scanned `filei*.tim` backdrop
//! behind each page and stores every page as its own `textm_*.tim` TIM that
//! packs two 256x128 half-pages. The book/page tables below are the Japanese
//! build's per-document start/count/first-page tables; the reader advances one
//! half-page per press and closes on confirm at the last page.
//!
//! The map tab and the radio tab's model viewer are out of M6 scope: the
//! engine's Tab cycle can still land on them, where map is inert and the radio
//! uses the carried radio through the scenario flag.

use anyhow::{Context, Result};

use crate::font::Tint;
use crate::game::GameState;
use crate::model::Texture8;
use crate::pack::Pack;
use crate::render::Framebuffer;
use crate::text::Text;
use crate::tim;

use super::main_menu::{MenuAssets, MenuInput};

/// Character half of the key-item list: Chris.
pub const CHARACTER_CHRIS: u8 = 0;
/// Character half of the key-item list: Jill.
pub const CHARACTER_JILL: u8 = 1;
/// First document item id (`FILE_ITEM_MIN`).
pub const FILE_ITEM_BASE: u8 = 0x5F;

/// Chris's 16-entry key-item list: two books of eight. `0xFE` is the
/// scenario-variant marker, `0xFF` an empty slot.
pub const KEY_ITEM_LIST_CHRIS: [u8; 16] = [
    0x0F, 0x02, 0xFE, 0x03, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0C, 0x0D, 0x0E, 0xFF, 0xFF, 0xFF,
];
/// Jill's 16-entry key-item list.
pub const KEY_ITEM_LIST_JILL: [u8; 16] = [
    0x0F, 0x02, 0xFE, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0xFF,
];

/// Per-document FILEI backdrop index into [`JPN_FILEI_NAMES`].
pub const JPN_FILEI_INDEX: [u8; 16] = [1, 0, 2, 3, 8, 12, 3, 5, 7, 0, 4, 11, 6, 9, 10, 13];
/// Per-document reading length in half-pages (both 2n and 2n-1 display n
/// pages; parity only decides which half of the last TIM is last).
pub const JPN_FILE_PAGE_COUNT: [u8; 16] = [4, 6, 11, 3, 2, 6, 7, 7, 8, 5, 5, 4, 2, 2, 2, 5];
/// Per-document title x offset (JPN build).
pub const JPN_FILE_TITLE_X: [u8; 16] = [
    42, 42, 42, 21, 42, 21, 49, 28, 0, 42, 14, 42, 35, 35, 35, 63,
];
/// Per-document start index into [`JPN_TEXTM_NAMES`].
pub const JPN_FILE_PAGE_START: [u8; 16] =
    [0, 2, 5, 11, 13, 14, 17, 21, 25, 29, 32, 35, 37, 38, 39, 40];

/// The 43 per-page TIM names in table order.
pub const JPN_TEXTM_NAMES: [&str; 43] = [
    "textm_l0", "textm_l1", "textm_m0", "textm_m1", "textm_m2", "textm_n0", "textm_n1", "textm_n2",
    "textm_n3", "textm_n4", "textm_n5", "textm_o0", "textm_o1", "textm_p0", "textm_q0", "textm_q1",
    "textm_q2", "textm_r0", "textm_r1", "textm_r2", "textm_r3", "textm_s0", "textm_s1", "textm_s2",
    "textm_s3", "textm_t0", "textm_t1", "textm_t2", "textm_t3", "textm_u0", "textm_u1", "textm_u2",
    "textm_v0", "textm_v1", "textm_v2", "textm_w0", "textm_w1", "textm_x0", "textm_y0", "textm_z0",
    "textm_00", "textm_01", "textm_02",
];

/// The 14 used per-document backdrop TIM names in [`JPN_FILEI_INDEX`] order;
/// the tree has no `filei01/02/05`, so those indices are unused.
pub const JPN_FILEI_NAMES: [&str; 14] = [
    "filei03", "filei04", "filei06", "filei07", "filei08", "filei09", "filei10", "filei11",
    "filei12", "filei13", "filei14", "filei15", "filei16", "filei17",
];

/// Number of books in the selector.
pub const BOOKS: usize = 2;
/// Selectable slots per book.
pub const SLOTS_PER_BOOK: usize = 8;

/// The book cover draw position.
pub const COVER_POS: [i16; 2] = [0x20, 0x18];
/// The book cover source size (the cover page is the whole selector art).
pub const COVER_SIZE: [i16; 2] = [0xA0, 0x68];
/// The slot-icon source rectangle inside the cover.
pub const SLOT_ICON_UV: [i32; 4] = [0, 0xA8, 4, 8];
/// The cursor-arrow source rectangle inside the cover.
pub const CURSOR_UV: [i32; 4] = [0, 0xA0, 4, 8];
/// The slot-icon list's x base.
pub const SLOT_X: i16 = 0x8C;
/// The slot-icon list's y base.
pub const SLOT_Y: i16 = 0x29;
/// Vertical step between the eight slot rows.
pub const SLOT_STEP: i16 = 9;

/// The reader page position.
pub const PAGE_POS: [i16; 2] = [0x18, 0x34];
/// The reader page size (one half of a TEXTM TIM).
pub const PAGE_SIZE: [i16; 2] = [0x100, 0x80];
/// The FILEI backdrop position.
pub const BACKDROP_POS: [i16; 2] = [0x30, 0x30];
/// The FILEI backdrop size.
pub const BACKDROP_SIZE: [i16; 2] = [0xD0, 0x78];
/// Previous-page arrow position.
pub const PREV_ARROW_POS: [i16; 2] = [0x18, 0x74];
/// Next-page arrow position.
pub const NEXT_ARROW_POS: [i16; 2] = [0x110, 0x74];
/// The EXIT label position inside the reader.
pub const EXIT_POS: [i16; 2] = [0x118, 0x6F];

/// The character's 16-entry key-item list.
pub fn key_item_list(character: u8) -> &'static [u8; 16] {
    if character & 1 == CHARACTER_JILL {
        &KEY_ITEM_LIST_JILL
    } else {
        &KEY_ITEM_LIST_CHRIS
    }
}

/// The raw key-item list entry of `book` (`0..2`) slot `slot` (`0..8`).
pub fn raw_entry(character: u8, book: usize, slot: usize) -> Option<u8> {
    let book = book.min(BOOKS - 1);
    let slot = slot.min(SLOTS_PER_BOOK - 1);
    key_item_list(character)
        .get(book * SLOTS_PER_BOOK + slot)
        .copied()
}

/// Resolve a raw list entry against the collected flags. `0xFF` is empty and
/// `0xFE` picks document `0` or `1` once one of them has been collected;
/// otherwise the entry is a document index that must itself be collected.
pub fn resolve_entry(game: &GameState, entry: u8) -> Option<u8> {
    match entry {
        0xFF => None,
        0xFE => {
            if game.file_collected(0) {
                Some(0)
            } else if game.file_collected(1) {
                Some(1)
            } else {
                None
            }
        }
        index if usize::from(index) < JPN_FILE_PAGE_COUNT.len() && game.file_collected(index) => {
            Some(index)
        }
        _ => None,
    }
}

/// The document index of `book`/`slot`, resolved against the collected flags.
pub fn listed_entry(game: &GameState, character: u8, book: usize, slot: usize) -> Option<u8> {
    resolve_entry(game, raw_entry(character, book, slot)?)
}

/// The first listed slot of `book`, or `0xFF` when nothing is collected.
pub fn first_listed_slot(game: &GameState, character: u8, book: usize) -> u8 {
    for slot in 0..SLOTS_PER_BOOK {
        if listed_entry(game, character, book, slot).is_some() {
            return slot as u8;
        }
    }
    0xFF
}

/// The nearest listed slot strictly above `slot` in `book`.
pub fn previous_listed_slot(game: &GameState, character: u8, book: usize, slot: u8) -> Option<u8> {
    (0..slot)
        .rev()
        .find(|&candidate| listed_entry(game, character, book, usize::from(candidate)).is_some())
}

/// The nearest listed slot strictly below `slot` in `book`.
pub fn next_listed_slot(game: &GameState, character: u8, book: usize, slot: u8) -> Option<u8> {
    ((slot as usize + 1)..SLOTS_PER_BOOK)
        .find(|&candidate| listed_entry(game, character, book, candidate).is_some())
        .map(|candidate| candidate as u8)
}

/// Number of reader half-pages of document `entry`.
pub fn page_count(entry: u8) -> u8 {
    JPN_FILE_PAGE_COUNT
        .get(usize::from(entry))
        .copied()
        .unwrap_or(0)
}

/// The TEXTM table index of half-page `page` of document `entry`, or `None`
/// for an out-of-range document/page.
pub fn page_index(entry: u8, page: u8) -> Option<usize> {
    let start = usize::from(*JPN_FILE_PAGE_START.get(usize::from(entry))?);
    let count = usize::from(page_count(entry));
    if usize::from(page) >= count {
        return None;
    }
    Some(start + usize::from(page) / 2)
}

/// The source y of half-page `page` inside its TEXTM TIM: `0` for the first
/// page, `0x80` for the second.
pub fn page_half(page: u8) -> i32 {
    if page.is_multiple_of(2) { 0 } else { 0x80 }
}

/// The FILEI backdrop index of document `entry`, or `None` when out of range.
pub fn filei_index(entry: u8) -> Option<usize> {
    JPN_FILEI_INDEX
        .get(usize::from(entry))
        .map(|&index| usize::from(index))
}

/// The title item id of document `entry` (`entry + 0x5F`).
pub fn title_item(entry: u8) -> u8 {
    entry.wrapping_add(FILE_ITEM_BASE)
}

/// The title x of document `entry` in the JPN reader.
pub fn title_x(entry: u8) -> i16 {
    JPN_FILE_TITLE_X
        .get(usize::from(entry))
        .map_or(0, |&x| i16::from(x))
}

/// The document reader's loaded art.
pub struct FileAssets {
    /// The two book covers (`file000.tim`, `file001.tim`).
    pub covers: [Texture8; 2],
    /// The 14 FILEI backdrops in [`JPN_FILEI_INDEX`] order.
    pub filei: Vec<Texture8>,
    /// The 43 TEXTM page packs in [`JPN_TEXTM_NAMES`] order.
    pub textm: Vec<Texture8>,
}

impl FileAssets {
    /// Load every cover, backdrop and page pack from `pack`.
    pub fn load(pack: &Pack) -> Result<Self> {
        let cover0 = decode(pack, "file/file000.tim")?;
        let cover1 = decode(pack, "file/file001.tim")?;
        let filei = JPN_FILEI_NAMES
            .iter()
            .map(|name| decode(pack, &format!("file/{name}.tim")))
            .collect::<Result<Vec<_>>>()?;
        let textm = JPN_TEXTM_NAMES
            .iter()
            .map(|name| decode(pack, &format!("file/{name}.tim")))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            covers: [cover0, cover1],
            filei,
            textm,
        })
    }

    /// The backdrop of document `entry`.
    pub fn backdrop(&self, entry: u8) -> Option<&Texture8> {
        self.filei.get(filei_index(entry)?)
    }

    /// The page pack holding half-page `page` of document `entry`.
    pub fn page(&self, entry: u8, page: u8) -> Option<&Texture8> {
        self.textm.get(page_index(entry, page)?)
    }
}

fn decode(pack: &Pack, entry: &str) -> Result<Texture8> {
    let bytes = pack
        .read(entry)
        .with_context(|| format!("missing {entry}"))?;
    tim::decode_8bpp(bytes).with_context(|| format!("invalid {entry}"))
}

/// Which view owns the FILE tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// The book selector.
    List,
    /// The two-page document reader.
    Reader,
}

/// What a handled FILE input asks the engine to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEvent {
    /// The input was consumed.
    None,
    /// Close the FILE tab and return to the inventory menu.
    Close,
}

/// The FILE tab's screen state.
#[derive(Debug, Clone)]
pub struct FileScreen {
    /// Which view is up.
    pub mode: FileMode,
    /// Character half of the key-item list (`0` Chris, `1` Jill).
    pub character: u8,
    /// Current book (`0`/`1`).
    pub book: u8,
    /// Selected slot (`0..8`), or `0xFF` when the book has no document.
    pub slot: u8,
    /// Reader half-page index.
    pub page: u8,
    /// Blink counter for the arrows and cursor.
    pub blink: u8,
    /// Horizontal slide of the current view in pixels (book changes).
    pub slide_x: i16,
    /// Vertical slide of the current view in pixels (opening/closing).
    pub slide_y: i16,
}

impl Default for FileScreen {
    fn default() -> Self {
        Self {
            mode: FileMode::List,
            character: CHARACTER_CHRIS,
            book: 0,
            slot: 0xFF,
            page: 0,
            blink: 0,
            slide_x: 0,
            slide_y: 0,
        }
    }
}

impl FileScreen {
    /// Open the selector on the engine's current character and the first
    /// collected document of book 1.
    pub fn open(&mut self, game: &GameState) {
        self.mode = FileMode::List;
        self.character = game.id.player_flag & 1;
        self.book = 0;
        self.slot = first_listed_slot(game, self.character, 0);
        self.page = 0;
        self.blink = 0;
        self.slide_x = 0;
        self.slide_y = 0;
    }

    /// Whether the current book has a selectable document.
    pub fn has_selection(&self) -> bool {
        self.slot != 0xFF
    }

    /// Advance one tick: blink and the book-change slide.
    pub fn tick(&mut self) {
        self.blink = self.blink.wrapping_add(1);
        if self.slide_x > 0 {
            self.slide_x = (self.slide_x - 9).max(0);
        } else if self.slide_x < 0 {
            self.slide_x = (self.slide_x + 9).min(0);
        }
    }

    /// The blink phase used by the arrows and the other-book indicator.
    pub fn blink_phase(&self) -> bool {
        self.blink & 0x30 == 0
    }

    /// Handle one discrete menu input.
    pub fn handle_input(&mut self, game: &mut GameState, input: MenuInput) -> FileEvent {
        match self.mode {
            FileMode::List => self.list_input(game, input),
            FileMode::Reader => self.reader_input(game, input),
        }
    }

    fn list_input(&mut self, game: &mut GameState, input: MenuInput) -> FileEvent {
        match input {
            MenuInput::Cancel => FileEvent::Close,
            MenuInput::Up => {
                if self.has_selection()
                    && let Some(slot) = previous_listed_slot(
                        game,
                        self.character,
                        usize::from(self.book),
                        self.slot,
                    )
                {
                    self.slot = slot;
                }
                FileEvent::None
            }
            MenuInput::Down => {
                if self.has_selection()
                    && let Some(slot) =
                        next_listed_slot(game, self.character, usize::from(self.book), self.slot)
                {
                    self.slot = slot;
                }
                FileEvent::None
            }
            MenuInput::Left => {
                if self.book != 0 {
                    self.book = 0;
                    self.slot = first_listed_slot(game, self.character, 0);
                    self.slide_x = -0x78;
                }
                FileEvent::None
            }
            MenuInput::Right => {
                if self.book + 1 < BOOKS as u8 {
                    self.book = 1;
                    self.slot = first_listed_slot(game, self.character, 1);
                    self.slide_x = 0x78;
                }
                FileEvent::None
            }
            MenuInput::Confirm => {
                if self.has_selection() {
                    self.mode = FileMode::Reader;
                    self.page = 0;
                }
                FileEvent::None
            }
            MenuInput::PageLeft | MenuInput::PageRight => FileEvent::None,
        }
    }

    fn reader_input(&mut self, game: &mut GameState, input: MenuInput) -> FileEvent {
        let entry = listed_entry(
            game,
            self.character,
            usize::from(self.book),
            usize::from(self.slot),
        )
        .unwrap_or(0xFF);
        if entry == 0xFF {
            self.mode = FileMode::List;
            return FileEvent::None;
        }
        let count = page_count(entry);
        match input {
            MenuInput::Cancel => {
                self.mode = FileMode::List;
                FileEvent::None
            }
            MenuInput::Down | MenuInput::Right | MenuInput::PageRight => {
                if self.page + 1 < count {
                    self.page += 1;
                }
                FileEvent::None
            }
            MenuInput::Up | MenuInput::Left | MenuInput::PageLeft => {
                self.page = self.page.saturating_sub(1);
                FileEvent::None
            }
            MenuInput::Confirm => {
                if self.page + 1 >= count {
                    self.mode = FileMode::List;
                } else {
                    self.page += 1;
                }
                FileEvent::None
            }
        }
    }

    /// Draw the tab over the framebuffer's current contents.
    pub fn draw(
        &self,
        framebuffer: &mut Framebuffer,
        assets: &FileAssets,
        menu: &MenuAssets,
        text: &Text,
        game: &GameState,
    ) {
        framebuffer.fill_rect([0, 0, 320, 240], [0, 0, 0, 255]);
        match self.mode {
            FileMode::List => self.draw_list(framebuffer, assets, menu, text, game),
            FileMode::Reader => self.draw_reader(framebuffer, assets, menu, game),
        }
    }

    fn draw_list(
        &self,
        framebuffer: &mut Framebuffer,
        assets: &FileAssets,
        menu: &MenuAssets,
        text: &Text,
        game: &GameState,
    ) {
        let cover = assets
            .covers
            .get(usize::from(self.book))
            .unwrap_or(&assets.covers[0]);
        framebuffer.draw_indexed_sprite(
            cover,
            [0, 0, i32::from(COVER_SIZE[0]), i32::from(COVER_SIZE[1])],
            [
                i32::from(COVER_POS[0]) + i32::from(self.slide_x),
                i32::from(COVER_POS[1]) + i32::from(self.slide_y),
                i32::from(COVER_SIZE[0]),
                i32::from(COVER_SIZE[1]),
            ],
            0,
            0,
            Tint::White,
        );

        // The current book's indicator is hidden; the other blinks.
        let blink = i32::from(self.blink_phase());
        if self.book != 0 {
            draw_uv(
                framebuffer,
                cover,
                [0, 0xB0 + blink * 8, 8, 8],
                [0x3C, 0x48],
            );
        }
        if self.book != 1 {
            draw_uv(
                framebuffer,
                cover,
                [0, 0xC0 + blink * 8, 8, 8],
                [0x9C, 0x48],
            );
        }

        // Seen document icons and the cursor arrow.
        for slot in 0..SLOTS_PER_BOOK {
            if listed_entry(game, self.character, usize::from(self.book), slot).is_none() {
                continue;
            }
            let pos = slot_position(slot as u8);
            draw_uv(framebuffer, cover, SLOT_ICON_UV, pos);
        }
        if self.has_selection() {
            draw_uv(framebuffer, cover, CURSOR_UV, slot_position(self.slot));
            if let Some(entry) = listed_entry(
                game,
                self.character,
                usize::from(self.book),
                usize::from(self.slot),
            ) {
                if let Some(name) = text.item_name(u16::from(title_item(entry))) {
                    menu.font.draw_text(
                        framebuffer,
                        i32::from(title_x(entry)) + 0x1E,
                        0xC0,
                        Tint::White,
                        0,
                        name,
                    );
                }
                // Scroll arrows follow the cursor through the seen slots.
                if previous_listed_slot(game, self.character, usize::from(self.book), self.slot)
                    .is_some()
                {
                    draw_uv(
                        framebuffer,
                        cover,
                        [0, 0xD0 + blink * 8, 8, 8],
                        [0x6F, 0xB7],
                    );
                }
                if next_listed_slot(game, self.character, usize::from(self.book), self.slot)
                    .is_some()
                {
                    draw_uv(
                        framebuffer,
                        cover,
                        [0, 0xE0 + blink * 8, 8, 8],
                        [0x6F, 0xCF],
                    );
                }
            }
        }
    }

    fn draw_reader(
        &self,
        framebuffer: &mut Framebuffer,
        assets: &FileAssets,
        menu: &MenuAssets,
        game: &GameState,
    ) {
        let Some(entry) = listed_entry(
            game,
            self.character,
            usize::from(self.book),
            usize::from(self.slot),
        ) else {
            return;
        };
        if let Some(backdrop) = assets.backdrop(entry) {
            framebuffer.draw_indexed_sprite(
                backdrop,
                [
                    0,
                    0,
                    i32::from(BACKDROP_SIZE[0]),
                    i32::from(BACKDROP_SIZE[1]),
                ],
                [
                    i32::from(BACKDROP_POS[0]),
                    i32::from(BACKDROP_POS[1]),
                    i32::from(BACKDROP_SIZE[0]),
                    i32::from(BACKDROP_SIZE[1]),
                ],
                0,
                0,
                Tint::White,
            );
        }
        if let Some(page) = assets.page(entry, self.page) {
            let half = page_half(self.page);
            framebuffer.draw_indexed_sprite(
                page,
                [0, half, i32::from(PAGE_SIZE[0]), i32::from(PAGE_SIZE[1])],
                [
                    i32::from(PAGE_POS[0]),
                    i32::from(PAGE_POS[1]),
                    i32::from(PAGE_SIZE[0]),
                    i32::from(PAGE_SIZE[1]),
                ],
                0,
                0,
                Tint::White,
            );
        }
        let blink = i32::from(self.blink_phase());
        let cover = assets
            .covers
            .get(usize::from(self.book))
            .unwrap_or(&assets.covers[0]);
        if self.page != 0 {
            draw_uv(
                framebuffer,
                cover,
                [0, 0xE0 + blink * 8, 8, 8],
                PREV_ARROW_POS,
            );
        }
        draw_uv(
            framebuffer,
            cover,
            [0, 0xF0 + blink * 8, 8, 8],
            NEXT_ARROW_POS,
        );

        // The "EXIT" label lights up on the last half-page so the player can
        // confirm out of the reader.
        if self.page + 1 >= page_count(entry) {
            framebuffer.draw_indexed_sprite(
                &menu.status,
                [0x80, 0x88, 0x18, 0x10],
                [i32::from(EXIT_POS[0]), i32::from(EXIT_POS[1]), 0x18, 0x10],
                0,
                20,
                Tint::White,
            );
        }
    }
}

fn slot_position(slot: u8) -> [i16; 2] {
    [
        SLOT_X + i16::from(slot >> 1),
        SLOT_Y + i16::from(slot) * SLOT_STEP,
    ]
}

fn draw_uv(framebuffer: &mut Framebuffer, texture: &Texture8, src: [i32; 4], pos: [i16; 2]) {
    framebuffer.draw_indexed_sprite(
        texture,
        src,
        [i32::from(pos[0]), i32::from(pos[1]), src[2], src[3]],
        0,
        0,
        Tint::White,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{RoomId, RoomState};

    fn game(character: u8) -> GameState {
        let mut id = RoomId::parse("1001").unwrap();
        id.player_flag = character;
        GameState::new(id, &RoomState::default())
    }

    #[test]
    fn the_key_item_lists_match_the_shipped_tables() {
        assert_eq!(key_item_list(0), &KEY_ITEM_LIST_CHRIS);
        assert_eq!(key_item_list(1), &KEY_ITEM_LIST_JILL);
        assert_eq!(raw_entry(0, 0, 0), Some(0x0F));
        assert_eq!(raw_entry(0, 1, 0), Some(0x09));
        assert_eq!(raw_entry(1, 1, 6), Some(0x0E));
        assert_eq!(raw_entry(1, 1, 7), Some(0xFF));
        assert_eq!(
            raw_entry(9, 9, 9),
            Some(0xFF),
            "out-of-range rows are clamped"
        );
    }

    #[test]
    fn list_entries_resolve_against_the_file_flags() {
        let mut state = game(0);
        assert_eq!(listed_entry(&state, 0, 0, 0), None, "uncollected");
        assert_eq!(listed_entry(&state, 0, 0, 13), None, "0xFF is empty");
        assert_eq!(
            listed_entry(&state, 0, 0, 2),
            None,
            "0xFE needs a seen file"
        );

        state.set_file_collected(0x0F, true);
        assert_eq!(listed_entry(&state, 0, 0, 0), Some(0x0F));
        state.set_file_collected(0, true);
        assert_eq!(listed_entry(&state, 0, 0, 2), Some(0));
        state.set_file_collected(0, false);
        state.set_file_collected(1, true);
        assert_eq!(listed_entry(&state, 0, 0, 2), Some(1));

        // The first listed slot walks past empty rows; 0xFF means none.
        state.set_file_collected(1, false);
        state.set_file_collected(0x0F, true);
        assert_eq!(first_listed_slot(&state, 0, 0), 0);
        assert_eq!(first_listed_slot(&state, 1, 1), 0xFF);
    }

    #[test]
    fn page_index_math_follows_the_start_and_count_tables() {
        assert_eq!(page_count(0), 4);
        assert_eq!(page_count(2), 11);
        assert_eq!(page_index(0, 0), Some(0));
        assert_eq!(page_index(0, 1), Some(0));
        assert_eq!(page_index(0, 2), Some(1));
        assert_eq!(page_index(1, 0), Some(2));
        assert_eq!(page_index(2, 10), Some(10));
        assert_eq!(page_index(2, 11), None, "past the page count");
        assert_eq!(page_half(0), 0);
        assert_eq!(page_half(1), 0x80);
        assert_eq!(page_half(2), 0);

        // Every document's last half-page lands inside the 43-TIM table.
        for entry in 0..16u8 {
            let count = page_count(entry);
            let last = page_index(entry, count - 1).unwrap();
            assert!(last < JPN_TEXTM_NAMES.len(), "entry {entry} overruns");
        }
        // The starts are the cumulative ceil(count/2), ending at 40 + 2.
        let mut expected = 0usize;
        for entry in 0..16u8 {
            assert_eq!(
                JPN_FILE_PAGE_START[usize::from(entry)] as usize,
                expected,
                "entry {entry}"
            );
            expected += usize::from(page_count(entry)).div_ceil(2);
        }
        assert_eq!(expected, JPN_TEXTM_NAMES.len());

        assert_eq!(filei_index(0), Some(1));
        assert_eq!(filei_index(2), Some(2));
        assert_eq!(filei_index(15), Some(13));
        assert_eq!(filei_index(16), None);
        assert_eq!(title_item(0), 0x5F);
        assert_eq!(title_item(15), 0x6E);
        assert_eq!(title_x(0), 42);
        assert_eq!(title_x(15), 63);
    }

    #[test]
    fn selector_navigation_skips_uncollected_documents() {
        let mut state = game(0);
        // Book 1: slots 0 (file 0x0F) and 3 (file 0x03) are collected.
        state.set_file_collected(0x0F, true);
        state.set_file_collected(0x03, true);
        let mut screen = FileScreen::default();
        screen.open(&state);
        assert_eq!(screen.character, 0);
        assert_eq!(screen.book, 0);
        assert_eq!(screen.slot, 0);

        screen.handle_input(&mut state, MenuInput::Down);
        assert_eq!(screen.slot, 3, "down jumps to the next collected row");
        screen.handle_input(&mut state, MenuInput::Down);
        assert_eq!(screen.slot, 3, "nowhere else to go");

        screen.handle_input(&mut state, MenuInput::Right);
        assert_eq!(screen.book, 1);
        assert_eq!(screen.slot, 0xFF, "book 2 has nothing collected");
        screen.handle_input(&mut state, MenuInput::Left);
        assert_eq!(screen.book, 0);
        assert_eq!(screen.slot, 0);

        // Confirm on a listed document opens the reader at page 0.
        assert_eq!(
            screen.handle_input(&mut state, MenuInput::Confirm),
            FileEvent::None
        );
        assert_eq!(screen.mode, FileMode::Reader);
        assert_eq!(screen.page, 0);
    }

    #[test]
    fn the_reader_pages_through_and_closes_on_the_last_page() {
        let mut state = game(0);
        state.set_file_collected(0x0F, true);
        let mut screen = FileScreen::default();
        screen.open(&state);
        screen.handle_input(&mut state, MenuInput::Confirm);
        assert_eq!(screen.mode, FileMode::Reader);

        // List slot 0's raw entry is file index 0x0F, whose page-count table
        // entry is 5 half-pages. Walk to the last page and confirm.
        let count = page_count(0x0F);
        for page in 1..count {
            screen.handle_input(&mut state, MenuInput::Down);
            assert_eq!(screen.page, page);
        }
        screen.handle_input(&mut state, MenuInput::Down);
        assert_eq!(screen.page, count - 1, "the last page holds");
        screen.handle_input(&mut state, MenuInput::Confirm);
        assert_eq!(
            screen.mode,
            FileMode::List,
            "confirm exits on the last page"
        );

        // Cancel from the reader returns to the list too.
        screen.handle_input(&mut state, MenuInput::Confirm);
        assert_eq!(screen.mode, FileMode::Reader);
        screen.handle_input(&mut state, MenuInput::Cancel);
        assert_eq!(screen.mode, FileMode::List);
        assert_eq!(
            screen.handle_input(&mut state, MenuInput::Cancel),
            FileEvent::Close
        );
    }
}
