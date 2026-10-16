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
/// The slot-icon source rectangle inside the cover (`g_fileSlotDescs`,
/// 0x004d268c: 4x8 at texU `0xa8`, texV 0).
pub const SLOT_ICON_UV: [i32; 4] = [0xA8, 0, 4, 8];
/// The cursor-arrow source rectangle inside the cover (`DAT_004d27d0`: 4x8 at
/// texU `0xa0`, texV 0).
pub const CURSOR_UV: [i32; 4] = [0xA0, 0, 4, 8];
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
    tim::decode_8bpp(&bytes).with_context(|| format!("invalid {entry}"))
}

/// Which view owns the FILE tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// The book selector.
    List,
    /// The two-page document reader.
    Reader,
}

/// Which reader state is running (the original's `state[2]` in
/// `pickup_screen_render` 0x004823a0, shared by the JPN
/// `jpn_file_screen_render` 0x0042c080).
///
/// The reader slides the page texture in and out on every change: entry eases
/// it in from the right, a page-down first slides it out to the left and then
/// eases the next page in from the right, a page-up mirrors that, and the close
/// slides it out to the left and runs the backdrop/selector tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderState {
    /// The original's 0/1: the FILEI backdrop pages in and its frame pieces
    /// ramp `8` at a time to `0x80`. Nothing is drawn over the black backdrop
    /// until the half-page's TIM has also paged in (state 2), so the whole
    /// sixteen-frame ramp is black.
    Backdrop,
    /// 2: the half-page's TEXTM TIM pages in and the entry slide starts.
    PageLoad,
    /// 3: the page eases onto the page box from the right (`0x128` -> 0).
    SlideIn,
    /// 4: the slide settled; the busy lock releases on this frame.
    Idle,
    /// 5/6: the page slides out to the left, then the next half-page.
    SlideOutLeft,
    /// 7: the next half-page's TIM load frame.
    NextLoad,
    /// 8/9: the page slides out to the right, then the previous half-page.
    SlideOutRight,
    /// 0xa: the previous half-page's unpack frame (a no-op; the load follows).
    UnpackLeft,
    /// 0xb: the previous half-page pages in and falls into 0xc.
    PrevPage,
    /// 0xc: the page eases back in from the left (`-0x128` -> 0).
    SlideInLeft,
    /// 0xd/0xe: the reader slides out to the left on close.
    ExitSlide,
    /// 0xf: the frame pieces ramp back down to black over sixteen frames.
    BackdropOut,
    /// 0x10: the black hold frame before the selector's cover pages back in.
    Hold,
    /// 0x11: the selector's cover pages back into the shared page slot.
    CoverLoad,
    /// 0x12: the hand-off - the selector takes over with its fade mode set
    /// (the original's `b0=0, b1=1, b2=1, b3=2`) and fades back in.
    HandOff,
}

/// The reader's entry/exit slide start offset (the original's `0x128`).
pub const READER_SLIDE_START: i16 = 0x128;
/// The reader's entry/exit slide start speed (the original's `300`).
const READER_SLIDE_SPEED: i16 = 300;
/// The backdrop/frame ramp step (the original's `DAT_004af48c` `+/- 8`).
const READER_BACKDROP_STEP: u8 = 8;
/// The ramp's full value: `DAT_004af48c` tops out at `0x80`, which the
/// descriptor's variant-2 half blend turns into 50% brightness.
pub const READER_BACKDROP_FULL: u8 = 0x80;

/// The six FILEI pieces the reader draws behind the page (`g_jpnReaderFrame`,
/// 0x004af478): the backdrop itself plus the five scraps stacked down the
/// texture's right edge, each a `(src, dst)` rectangle.
const READER_FRAME_PIECES: [([i32; 4], [i32; 4]); 6] = [
    ([0x00, 0x00, 0xD0, 0x78], [0x30, 0x30, 0xD0, 0x78]),
    ([0xD0, 0x00, 0x30, 0x18], [0x30, 0xA8, 0x30, 0x18]),
    ([0xD0, 0x18, 0x30, 0x18], [0x60, 0xA8, 0x30, 0x18]),
    ([0xD0, 0x30, 0x30, 0x18], [0x90, 0xA8, 0x30, 0x18]),
    ([0xD0, 0x48, 0x30, 0x18], [0xC0, 0xA8, 0x30, 0x18]),
    ([0xD0, 0x60, 0x10, 0x18], [0xF0, 0xA8, 0x10, 0x18]),
];
/// The previous/next page arrow source y in the FILEI texture
/// (`DAT_004af550`/`DAT_004af574` 0x004af550: texV `0x60`).
const READER_ARROW_Y: i32 = 0x60;
/// The selector's entry slide (`FUN_00481ae0` 0x00481ae0 / `FUN_00481d00`
/// case 0): the cover starts at `-0x7f` and eases down at speed `0x80`.
const LIST_SLIDE_START: i16 = -0x7f;
const LIST_SLIDE_SPEED: i16 = 0x80;
/// The pause menu's screen area (`0x28,0x18` `0xf0`x`0xc0`). The selector's
/// own depth sits behind the panel's frame parts (820-964), so a sliding cover
/// never spills over the panel's chrome; clipping the selector to this area
/// reproduces that.
const SCREEN_AREA: [i32; 4] = [0x28, 0x18, 0xF0, 0xC0];

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
    /// Vertical slide of the current view in pixels (the entry slide).
    pub slide_y: i16,
    /// The selector's entry slide speed (the original's `state[0xa]`).
    list_speed: i16,
    /// Whether the selector's state machine has run its first frame yet. The
    /// original switches the pause menu's task a frame after the tab row's
    /// confirm, so the frame the FILE tab is chosen still draws the panel
    /// alone and the cover's entry slide starts on the next frame.
    list_pending: bool,
    /// Whether the selector has settled and draws its title, book indicators
    /// and scroll arrows (the original's `FUN_00481d00` display state 1).
    list_ready: bool,
    /// Which reader load/slide step is running.
    pub reader_state: ReaderState,
    /// The reader page's x offset (the original's `state[0xc]`).
    pub reader_slide: i16,
    /// The reader slide's speed (the original's `state[0xa]`).
    reader_speed: i16,
    /// A reader exit requested this frame (the original's FILE_REQUEST `0xd`).
    /// The request is raised after the reader's own render ran, so the state
    /// machine applies it on the next frame and the request frame still draws
    /// the settled reader.
    reader_exit_request: bool,
    /// Whether a half-page TIM is paged in (the original's `DAT_004af600`): the
    /// frame pieces and the page draw only once it is, which is why the
    /// reader's whole backdrop ramp is black.
    pub reader_page_loaded: bool,
    /// The frame pieces' brightness ramp (the original's `DAT_004af48c`):
    /// `0..=0x80`, halved by the descriptor's variant-2 blend.
    pub reader_backdrop: u8,
    /// The reader's input lock (the original's `state[9]`, the FILE_BUSY byte).
    reader_busy: bool,
    /// The busy lock as this frame's input sees it: the original reads
    /// `state[9]` before the render step that clears it, so the lock always
    /// looks one frame older than the state machine's own value.
    reader_busy_frame: bool,
    /// The original's `state[0x10]` (FILE_SAVEFLAG): the last half-page has
    /// been acknowledged, which lights the EXIT label and lets a confirm out.
    pub reader_last_page: bool,
    /// Whether the reader was opened for a document pick-up: its exit ends the
    /// whole flow instead of returning to the selector.
    pickup: bool,
    /// The selector-to-reader white fade ramp (the original's
    /// `pickup_fade_update`: `+/-0x20` per frame). The hand-off holds the busy
    /// lock while it runs: up before the reader, down again after its exit.
    fade: u8,
    /// Whether the white fade hand-off (selector -> reader) is running.
    fade_in: bool,
    /// Whether the white fade is ramping down over the selector after the
    /// reader's exit (the original's mode byte 2).
    fade_out: bool,
    /// Set when a pick-up reader's exit slide has finished.
    reader_exit_done: bool,
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
            list_speed: 1,
            list_pending: false,
            list_ready: true,
            reader_state: ReaderState::Backdrop,
            reader_exit_request: false,
            reader_slide: 0,
            reader_speed: 1,
            reader_page_loaded: false,
            reader_backdrop: 0,
            reader_busy: false,
            reader_busy_frame: false,
            reader_last_page: false,
            pickup: false,
            reader_exit_done: false,
            fade: 0,
            fade_in: false,
            fade_out: false,
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
        self.list_speed = 1;
        // The original's file task starts on the frame *after* the tab row's
        // confirm, and its `FUN_00481ae0` init starts the cover at `-0x7f`
        // with speed `0x80`, running the renderer's first state-0 step on the
        // same frame - which is the next tick here.
        self.list_pending = true;
        self.list_ready = false;
        self.reader_reset();
        self.pickup = false;
        self.fade = 0;
        self.fade_in = false;
        self.fade_out = false;
    }

    /// Open the reader directly on the document item `item` (`0x5F..=0x6E`),
    /// for the room pick-up flow: the original finds the key-item list slot
    /// holding the picked index and enters the reader on it (`menu_update_
    /// status_screen`'s `g_pickupKeyItemList` scan).
    pub fn open_pickup(&mut self, game: &GameState, item: u8) {
        self.mode = FileMode::Reader;
        self.character = game.id.player_flag & 1;
        self.pickup = true;
        let index = item.wrapping_sub(FILE_ITEM_BASE);
        let list = key_item_list(self.character);
        let found = list
            .iter()
            .position(|&raw| resolve_entry(game, raw) == Some(index));
        let (book, slot) = found.map_or((0, 0), |i| {
            ((i / SLOTS_PER_BOOK) as u8, (i % SLOTS_PER_BOOK) as u8)
        });
        self.book = book;
        self.slot = slot;
        self.page = 0;
        self.blink = 0;
        self.slide_x = 0;
        self.slide_y = 0;
        self.list_pending = false;
        self.list_ready = true;
        self.reader_reset();
    }

    /// Reset the reader's state block (the original's `jpn_file_list_init`,
    /// 0x0042bf70: the half-page counter and the last-page flag; the renderer
    /// then runs its state-0 load, which arms the backdrop ramp and the lock).
    fn reader_reset(&mut self) {
        self.reader_state = ReaderState::Backdrop;
        self.reader_exit_request = false;
        self.reader_slide = 0;
        self.reader_speed = 1;
        self.reader_page_loaded = false;
        self.reader_backdrop = 0;
        self.reader_busy = true;
        self.reader_busy_frame = true;
        self.reader_last_page = false;
        self.reader_exit_done = false;
    }

    /// Whether the original's busy lock (`state[9]`, FILE_BUSY) owns this
    /// frame's input: the selector's fade hand-off (MODE 1/2), its entry slide,
    /// or the reader's own lock. The reader's half of the lock is read as the
    /// *previous* frame left it, because the original tests `state[9]` before
    /// the render step that releases it.
    pub fn input_locked(&self) -> bool {
        self.fade_in
            || self.fade_out
            || self.reader_exit_request
            || match self.mode {
                FileMode::List => self.list_pending || !self.list_ready,
                FileMode::Reader => self.reader_busy_frame,
            }
    }

    /// Whether a pick-up reader has finished its exit slide.
    pub fn take_reader_exit(&mut self) -> bool {
        std::mem::take(&mut self.reader_exit_done)
    }

    /// One step of the selector's entry slide (the original's `FUN_00481d00`
    /// case 0: the speed halves and the cover eases down from `-0x7f`).
    fn step_list_entry(&mut self) {
        self.list_speed /= 2;
        self.slide_y += self.list_speed;
        if self.slide_y >= 0 {
            self.slide_y = 0;
            self.list_speed = 1;
            self.list_ready = true;
        }
    }

    /// The original's state 3: the page eases onto the page box from the right,
    /// halving its speed each frame.
    fn step_reader_slide_in(&mut self) {
        self.reader_speed /= 2;
        self.reader_slide -= self.reader_speed;
        if self.reader_slide < 1 {
            self.reader_slide = 0;
            self.reader_speed = 1;
            self.reader_state = ReaderState::Idle;
        }
    }

    /// The original's state 6: the page runs off to the left, doubling its
    /// speed; the half-page advances once the texture is fully clear.
    fn step_reader_out_left(&mut self) {
        self.reader_slide -= self.reader_speed;
        self.reader_speed = self.reader_speed.saturating_mul(2);
        if self.reader_slide < -0x127 {
            self.page = self.page.saturating_add(1);
            self.reader_state = ReaderState::NextLoad;
        }
    }

    /// The original's state 9: the page runs off to the right, doubling its
    /// speed; the previous half-page is entered once it is clear.
    fn step_reader_out_right(&mut self) {
        self.reader_slide += self.reader_speed;
        self.reader_speed = self.reader_speed.saturating_mul(2);
        if self.reader_slide > 0x127 {
            self.page = self.page.saturating_sub(1);
            self.reader_state = ReaderState::UnpackLeft;
        }
    }

    /// The original's state 0xc: the page eases back in from the left.
    fn step_reader_slide_in_left(&mut self) {
        self.reader_speed /= 2;
        self.reader_slide += self.reader_speed;
        if self.reader_slide > -1 {
            self.reader_slide = 0;
            self.reader_speed = 1;
            self.reader_state = ReaderState::Idle;
        }
    }

    /// Arm the reader's entry (the original's FILE_STATE 0 -> 1 hand-off when
    /// the selector's fade reaches full: the state block becomes
    /// `0x00000001`). The reader's own first frame is the renderer's state 0:
    /// it pages the FILEI backdrop in, sets the slide start and locks input.
    fn start_reader_entry(&mut self) {
        self.mode = FileMode::Reader;
        self.page = 0;
        self.reader_reset();
    }

    /// Start a page-down (the original's `jpn_file_list_input` state 5). The
    /// input state falls through into the slide, so the input frame already
    /// moves the page one pixel.
    fn start_reader_next(&mut self) {
        self.reader_busy = true;
        self.reader_slide = 0;
        self.reader_speed = 1;
        self.reader_state = ReaderState::SlideOutLeft;
        self.step_reader_out_left();
    }

    /// Start a page-up (the original's state 8), with the same input-frame
    /// fall-through as [`FileScreen::start_reader_next`].
    fn start_reader_prev(&mut self) {
        self.reader_busy = true;
        self.reader_slide = 0;
        self.reader_speed = 1;
        self.reader_state = ReaderState::SlideOutRight;
        self.step_reader_out_right();
    }

    /// Request the reader's exit (the original's `menu_file_state_machine`
    /// setting FILE_REQUEST to `0xd`). The request is raised after the
    /// reader's own advance ran, so the cancel frame still draws the settled
    /// page and the exit slide's first step lands on the next frame.
    fn start_reader_exit(&mut self) {
        self.reader_busy = true;
        self.reader_exit_request = true;
    }

    /// Whether the current book has a selectable document.
    pub fn has_selection(&self) -> bool {
        self.slot != 0xFF
    }

    /// Advance one tick: the selector's slides or the reader's state machine,
    /// plus the blink counter and the hand-off fades.
    pub fn tick(&mut self) {
        // The original checks the reader's busy lock (`state[9]`) before the
        // render step that maintains it, so this frame's input sees the lock
        // as the previous frame left it.
        self.reader_busy_frame = self.reader_busy;
        // The selector-to-reader hand-off fades the whole screen to white over
        // eight frames (`pickup_fade_update(0)`), then the reading mode takes
        // over on its opaque black backdrop. The frame the ramp completes on
        // still draws black - the original swaps FILE_STATE mid-render and the
        // selector is drawn under the full-strength overlay.
        if self.fade_in {
            self.fade = self.fade.saturating_add(0x20);
            if self.fade == 0xFF {
                self.fade = 0;
                self.fade_in = false;
                self.start_reader_entry();
            }
            return;
        }
        if self.fade_out {
            self.fade = self.fade.saturating_sub(0x20);
            if self.fade == 0 {
                self.fade_out = false;
                // `pickup_fade_update(1)` clears the state block's slide pair
                // as its ramp completes: the selector snaps back onto the
                // panel's screen.
                self.slide_x = 0;
                self.slide_y = 0;
            }
        }
        match self.mode {
            FileMode::List => {
                if self.list_pending {
                    // The task's first frame: the cover's entry slide is armed
                    // and stepped once (`FUN_00481ae0`).
                    self.list_pending = false;
                    self.slide_y = LIST_SLIDE_START;
                    self.list_speed = LIST_SLIDE_SPEED;
                    self.list_ready = false;
                    self.step_list_entry();
                } else if !self.list_ready {
                    self.step_list_entry();
                }
                if self.slide_x > 0 {
                    self.slide_x = (self.slide_x - 9).max(0);
                } else if self.slide_x < 0 {
                    self.slide_x = (self.slide_x + 9).min(0);
                }
            }
            FileMode::Reader => self.tick_reader(),
        }
        // The original's blink counter only runs in the selector's display
        // state and in the reader's settled state 4 (both are the frames whose
        // render draws the blinking arrows).
        let blinking = match self.mode {
            FileMode::List => self.list_ready,
            FileMode::Reader => self.reader_state == ReaderState::Idle,
        };
        if blinking {
            self.blink = self.blink.wrapping_add(1);
        }
    }

    /// Advance the reader one frame, transcribed from the original's
    /// `jpn_file_screen_render` (0x0042c080). Each state's step runs before
    /// the frame draws, and the falling-through states (2->3, 5->6, 8->9,
    /// 0xb->0xc, 0xd->0xe) are kept as single-frame steps here.
    fn tick_reader(&mut self) {
        if self.reader_exit_request {
            // The original's request byte is read by the renderer's state
            // switch: state 0xd clears the slide and speed and falls into
            // 0xe, so the exit's first step lands on this frame.
            self.reader_exit_request = false;
            self.reader_state = ReaderState::ExitSlide;
            self.reader_slide = 0;
            self.reader_speed = 1;
        }
        match self.reader_state {
            ReaderState::Backdrop => {
                // States 0/1: the FILEI backdrop is paged in and the frame
                // pieces ramp 8 at a time to 0x80. Nothing else draws yet, so
                // no frame of the ramp is visible.
                self.reader_backdrop = self
                    .reader_backdrop
                    .saturating_add(READER_BACKDROP_STEP)
                    .min(READER_BACKDROP_FULL);
                if self.reader_backdrop >= READER_BACKDROP_FULL {
                    self.reader_state = ReaderState::PageLoad;
                }
            }
            ReaderState::PageLoad => {
                // State 2: the half-page's TEXTM TIM pages in (arklay holds
                // every page pack decoded) and falls into state 3.
                self.reader_page_loaded = true;
                self.reader_slide = READER_SLIDE_START;
                self.reader_speed = READER_SLIDE_SPEED;
                self.reader_state = ReaderState::SlideIn;
                self.step_reader_slide_in();
            }
            ReaderState::SlideIn => self.step_reader_slide_in(),
            // State 4: the slide settled; the busy lock releases.
            ReaderState::Idle => self.reader_busy = false,
            ReaderState::SlideOutLeft => self.step_reader_out_left(),
            // State 7: the next half-page's load frame.
            ReaderState::NextLoad => self.reader_state = ReaderState::PageLoad,
            ReaderState::SlideOutRight => self.step_reader_out_right(),
            // State 0xa: the previous half-page's unpack frame.
            ReaderState::UnpackLeft => self.reader_state = ReaderState::PrevPage,
            // State 0xb: the previous half-page pages in and falls into 0xc,
            // which starts the left-hand entry at `-0x128`.
            ReaderState::PrevPage => {
                self.reader_slide = -READER_SLIDE_START;
                self.reader_speed = READER_SLIDE_SPEED;
                self.reader_state = ReaderState::SlideInLeft;
                self.step_reader_slide_in_left();
            }
            ReaderState::SlideInLeft => self.step_reader_slide_in_left(),
            ReaderState::ExitSlide => {
                // States 0xd/0xe: the reader runs off to the left, leaving the
                // page texture parked clear of the screen while the backdrop
                // ramps down.
                self.reader_slide -= self.reader_speed;
                self.reader_speed = self.reader_speed.saturating_mul(2);
                if self.reader_slide < -0x127 {
                    if self.pickup {
                        // A pick-up reader's flow ends with the slide: the
                        // engine raises the filed line and drops the layer,
                        // and the original then fades its black overlay away
                        // over the frozen room. The selector tail below belongs
                        // to the pause-menu reader.
                        self.reader_slide = 0;
                        self.reader_speed = 1;
                        self.reader_state = ReaderState::Idle;
                        self.reader_exit_done = true;
                    } else {
                        self.reader_state = ReaderState::BackdropOut;
                    }
                }
            }
            ReaderState::BackdropOut => {
                // State 0xf: the frame pieces ramp back down to black.
                self.reader_backdrop = self.reader_backdrop.saturating_sub(READER_BACKDROP_STEP);
                if self.reader_backdrop == 0 {
                    self.reader_state = ReaderState::Hold;
                }
            }
            // State 0x10: the black hold frame.
            ReaderState::Hold => self.reader_state = ReaderState::CoverLoad,
            ReaderState::CoverLoad => {
                // State 0x11: the selector's cover pages back into the shared
                // page slot, dropping the reader's half-page with it.
                self.reader_page_loaded = false;
                self.reader_state = ReaderState::HandOff;
            }
            ReaderState::HandOff => {
                // State 0x12: `b0=0, b1=1, b2=1, b3=2` - the selector returns
                // in its display state with the mode-2 fade armed. Its overlay
                // is still at full strength (the reader held it there), so this
                // frame draws black too.
                self.mode = FileMode::List;
                // The state block's slide pair is shared with the reader, so
                // the selector inherits the exit slide's offset: the cover and
                // the slot list stay parked clear of the screen for the whole
                // fade, while the fixed-position indicators, arrows and title
                // draw (dimmed) underneath it. `pickup_fade_update(1)` clears
                // the pair when the ramp completes.
                self.slide_x = self.reader_slide;
                self.slide_y = 0;
                self.list_speed = 1;
                self.list_ready = true;
                self.fade = 0xFF;
                self.fade_out = true;
            }
        }
    }

    /// The blink phase used by the arrows and the other-book indicator.
    pub fn blink_phase(&self) -> bool {
        self.blink & 0x30 == 0
    }

    /// Handle one discrete menu input.
    pub fn handle_input(&mut self, game: &mut GameState, input: MenuInput) -> FileEvent {
        // The white hand-off (and its reverse after the reader's exit) holds
        // the busy lock.
        if self.input_locked() {
            return FileEvent::None;
        }
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
                    // The original's MODE byte 1: the fade rect ramps over the
                    // selector from the confirm frame (`pickup_fade_update(0)`),
                    // and the reading mode starts when it reaches full.
                    self.fade_in = true;
                    self.fade = 0;
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
        // The original's `jpn_file_list_input` skips all input while its busy
        // lock (`state[9]`) is up: [`FileScreen::input_locked`] already
        // returned for that, so the reader only reads the pad once the lock is
        // released.
        let count = page_count(entry);
        match input {
            // The reader turns half-pages with the D-pad alone: right
            // (`0x2000`) pages down, left (`0x8000`) pages up. Up, down and
            // L1/R1 are not read here. The last half-page has to be
            // *acknowledged* by a further page-down, which sets
            // FILE_SAVEFLAG (state[0x10]) and lights the EXIT label.
            MenuInput::Right => {
                if self.page + 1 < count {
                    self.start_reader_next();
                } else {
                    self.reader_last_page = true;
                }
                FileEvent::None
            }
            MenuInput::Left => {
                if self.reader_last_page {
                    self.reader_last_page = false;
                } else if self.page > 0 {
                    self.start_reader_prev();
                }
                FileEvent::None
            }
            // A cancel always slides the reader out; a confirm only leaves it
            // once the last half-page has been acknowledged (`menu_file_state_
            // machine`'s FILE_SAVEFLAG check).
            MenuInput::Cancel => {
                self.start_reader_exit();
                FileEvent::None
            }
            MenuInput::Confirm => {
                if self.reader_last_page {
                    self.start_reader_exit();
                }
                FileEvent::None
            }
            MenuInput::Up | MenuInput::Down | MenuInput::PageLeft | MenuInput::PageRight => {
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
        // The book selector draws over the inventory panel (the FILE tab is a
        // sub-screen of it) but under the panel's masking rects and frame, so
        // it is clipped to the panel's screen area. The reader's own layer
        // (depth 500) draws over the chrome unmasked.
        match self.mode {
            FileMode::List => {
                framebuffer.set_clip_rect(Some(SCREEN_AREA));
                self.draw_list(framebuffer, assets, menu, text, game);
                framebuffer.set_clip_rect(None);
            }
            FileMode::Reader => {
                framebuffer.fill_rect([0, 0, 320, 240], [0, 0, 0, 255]);
                self.draw_reader(framebuffer, assets, menu, game);
            }
        }
        if self.fade > 0 {
            // The original's fade rect (blend 3) is an opaque black whose
            // alpha is the ramp byte: it darkens the screen in and out.
            framebuffer.fade_to_black_weighted(self.fade);
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
        if self.list_pending {
            // The tab's first frame: the original's file task has not run
            // yet, so only the panel underneath is drawn.
            return;
        }
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

        // The original's display state (1) is the only one that draws the book
        // indicators, the file title and the scroll arrows; the entry slide and
        // the book changes draw just the cover, slots and cursor.
        let blink = i32::from(self.blink_phase());
        if self.list_ready {
            // The current book's indicator is hidden; the other blinks.
            if self.book != 0 {
                draw_uv(
                    framebuffer,
                    cover,
                    [0xB0 + blink * 8, 0, 8, 8],
                    [0x3C, 0x48],
                );
            }
            if self.book != 1 {
                draw_uv(
                    framebuffer,
                    cover,
                    [0xC0 + blink * 8, 0, 8, 8],
                    [0x9C, 0x48],
                );
            }
        }

        // Seen document icons and the cursor arrow. Both are baked into the
        // cover page, so they follow its slide (the original offsets each
        // descriptor with `state[0xc]`/`state[0xe]`).
        for slot in 0..SLOTS_PER_BOOK {
            if listed_entry(game, self.character, usize::from(self.book), slot).is_none() {
                continue;
            }
            let pos = slot_position(slot as u8);
            draw_uv(
                framebuffer,
                cover,
                SLOT_ICON_UV,
                [pos[0] + self.slide_x, pos[1] + self.slide_y],
            );
        }
        if self.has_selection() {
            let pos = slot_position(self.slot);
            draw_uv(
                framebuffer,
                cover,
                CURSOR_UV,
                [pos[0] + self.slide_x, pos[1] + self.slide_y],
            );
            if let Some(entry) = listed_entry(
                game,
                self.character,
                usize::from(self.book),
                usize::from(self.slot),
            ) {
                if !self.list_ready {
                    return;
                }
                if let Some(name) = text.item_name(u16::from(title_item(entry))) {
                    menu.font.draw_text(
                        framebuffer,
                        i32::from(title_x(entry)) + 0x1E,
                        0xC0,
                        Tint::White,
                        0,
                        name,
                        text.uses_tight_metrics(),
                    );
                }
                // Scroll arrows follow the cursor through the seen slots.
                if previous_listed_slot(game, self.character, usize::from(self.book), self.slot)
                    .is_some()
                {
                    draw_uv(
                        framebuffer,
                        cover,
                        [0xD0 + blink * 8, 0, 8, 8],
                        [0x6F, 0xB7],
                    );
                }
                if next_listed_slot(game, self.character, usize::from(self.book), self.slot)
                    .is_some()
                {
                    draw_uv(
                        framebuffer,
                        cover,
                        [0xE0 + blink * 8, 0, 8, 8],
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
        if self.reader_page_loaded {
            if let Some(backdrop) = assets.backdrop(entry) {
                // The six FILEI pieces (`g_jpnReaderFrame` 0x004af478) draw
                // semi-transparent (the descriptor's variant 2) with the ramp
                // byte as their colour, which over the black backdrop comes out
                // as `colour * ramp / 255`: the fully ramped pieces sit at 50%.
                for (src, dst) in READER_FRAME_PIECES {
                    framebuffer.draw_indexed_sprite_ramp(
                        backdrop,
                        src,
                        dst,
                        0,
                        self.reader_backdrop,
                    );
                }
            }
            if let Some(page) = assets.page(entry, self.page) {
                let half = page_half(self.page);
                framebuffer.draw_indexed_sprite(
                    page,
                    [0, half, i32::from(PAGE_SIZE[0]), i32::from(PAGE_SIZE[1])],
                    [
                        i32::from(PAGE_POS[0]) + i32::from(self.reader_slide),
                        i32::from(PAGE_POS[1]),
                        i32::from(PAGE_SIZE[0]),
                        i32::from(PAGE_SIZE[1]),
                    ],
                    0,
                    0,
                    Tint::White,
                );
            }
        }
        // The arrows, the blink and the EXIT label belong to the settled
        // state 4; the slides draw the page alone.
        if self.reader_state != ReaderState::Idle {
            return;
        }
        let mut blink = i32::from(self.blink_phase());
        if self.reader_last_page {
            // The arrows freeze lit once the last half-page is acknowledged.
            blink = 1;
        }
        let Some(cover) = assets.backdrop(entry) else {
            return;
        };
        // Both arrows are cut from the FILEI art's bottom-right strip (texV
        // 0x60): each blink frame is its own eight-pixel column.
        if self.page != 0 {
            draw_uv(
                framebuffer,
                cover,
                [0xE0 + blink * 8, READER_ARROW_Y, 8, 8],
                PREV_ARROW_POS,
            );
        }
        draw_uv(
            framebuffer,
            cover,
            [0xF0 + blink * 8, READER_ARROW_Y, 8, 8],
            NEXT_ARROW_POS,
        );

        // The "EXIT" label lights up once the last half-page has been
        // acknowledged so the player can confirm out of the reader. The
        // reference stores `0x50` / `0x20` in the descriptor's colour
        // multiplier (`DAT_004d29d0[0x14]`), whose unity is `0x80`, so the
        // drawn scale is twice the byte (`0xA0` bright, `0x40` dim).
        if self.page + 1 >= page_count(entry) {
            let scale = if self.reader_last_page {
                0x50_u32
            } else {
                0x20
            } * 2;
            framebuffer.draw_indexed_sprite_scaled(
                &menu.status,
                [0x80, 0x88, 0x18, 0x10],
                [i32::from(EXIT_POS[0]), i32::from(EXIT_POS[1]), 0x18, 0x10],
                0,
                scale.min(255) as u8,
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
        // The tab's first frame draws only the panel under it; the cover's
        // entry slide starts on the next tick at `-0x7f` with speed `0x80`,
        // and only the settled state reads the pad.
        assert!(screen.list_pending);
        assert!(!screen.list_ready);
        assert_eq!(screen.slide_y, 0);
        screen.tick();
        assert_eq!(screen.slide_y, -0x7f + 0x40);
        settle_list(&mut screen);
        assert_eq!(screen.slide_y, 0);

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

        // Confirm on a listed document starts the fade hand-off; the reader
        // opens at page 0 when the ramp completes.
        assert_eq!(
            screen.handle_input(&mut state, MenuInput::Confirm),
            FileEvent::None
        );
        assert!(screen.fade_in);
        while screen.fade_in {
            screen.tick();
        }
        assert_eq!(screen.mode, FileMode::Reader);
        assert_eq!(screen.page, 0);
    }

    #[test]
    fn the_reader_pages_through_and_closes_on_the_last_page() {
        let mut state = game(0);
        state.set_file_collected(0x0F, true);
        let mut screen = FileScreen::default();
        screen.open(&state);
        settle_list(&mut screen);
        screen.handle_input(&mut state, MenuInput::Confirm);
        settle_reader(&mut screen);
        assert_eq!(screen.mode, FileMode::Reader);

        // List slot 0's raw entry is file index 0x0F, whose page-count table
        // entry is 5 half-pages. Walk to the last page and confirm out.
        let count = page_count(0x0F);
        for page in 1..count {
            screen.handle_input(&mut state, MenuInput::Right);
            settle_reader(&mut screen);
            assert_eq!(screen.page, page);
        }
        // A further page-down only acknowledges the last half-page (the
        // original's FILE_SAVEFLAG); the page holds.
        assert!(!screen.reader_last_page);
        screen.handle_input(&mut state, MenuInput::Right);
        settle_reader(&mut screen);
        assert_eq!(screen.page, count - 1, "the last page holds");
        assert!(screen.reader_last_page, "the last page is acknowledged");

        // Up/down and the paging buttons are not read by the reader.
        screen.handle_input(&mut state, MenuInput::Down);
        settle_reader(&mut screen);
        assert_eq!(screen.page, count - 1);
        screen.handle_input(&mut state, MenuInput::PageRight);
        settle_reader(&mut screen);
        assert_eq!(screen.page, count - 1);

        screen.handle_input(&mut state, MenuInput::Confirm);
        settle_reader(&mut screen);
        assert_eq!(
            screen.mode,
            FileMode::List,
            "confirm exits on the last page"
        );

        // Cancel from the reader returns to the list too.
        screen.handle_input(&mut state, MenuInput::Confirm);
        settle_reader(&mut screen);
        assert_eq!(screen.mode, FileMode::Reader);
        screen.handle_input(&mut state, MenuInput::Cancel);
        settle_reader(&mut screen);
        assert_eq!(screen.mode, FileMode::List);
        while screen.fade_out {
            screen.tick();
        }
        assert_eq!(
            screen.handle_input(&mut state, MenuInput::Cancel),
            FileEvent::Close
        );
    }

    /// Advance the selector until its entry slide settles.
    fn settle_list(screen: &mut FileScreen) {
        for _ in 0..16 {
            if screen.list_ready {
                return;
            }
            screen.tick();
        }
        panic!("the selector entry slide did not settle");
    }

    /// Advance the toggle-only screen until its reader state settles,
    /// running the selector-to-reader fade hand-off first.
    fn settle_reader(screen: &mut FileScreen) {
        for _ in 0..128 {
            if screen.fade_in || screen.fade_out || screen.input_locked() {
                screen.tick();
                continue;
            }
            if screen.mode == FileMode::List
                || (screen.mode == FileMode::Reader && screen.reader_state == ReaderState::Idle)
            {
                return;
            }
            screen.tick();
        }
        panic!("the reader slide did not settle");
    }

    #[test]
    fn the_reader_slides_the_page_on_and_off() {
        let mut state = game(0);
        state.set_file_collected(0x0F, true);
        let mut screen = FileScreen::default();
        screen.open(&state);
        settle_list(&mut screen);

        // Opening the reader fades the selector out first; the reader then
        // spends sixteen black frames ramping its backdrop in before the page
        // even pages in, and only then starts the 0x128 entry slide.
        screen.handle_input(&mut state, MenuInput::Confirm);
        while screen.fade_in {
            screen.tick();
        }
        assert_eq!(screen.mode, FileMode::Reader);
        assert_eq!(screen.reader_state, ReaderState::Backdrop);
        assert_eq!(screen.reader_slide, 0);
        for _ in 0..15 {
            screen.tick();
            assert_eq!(screen.reader_slide, 0, "the page is not up yet");
            assert!(!screen.reader_page_loaded);
        }
        // The sixteenth ramp frame completes the backdrop.
        screen.tick();
        assert_eq!(screen.reader_state, ReaderState::PageLoad);
        assert_eq!(screen.reader_backdrop, READER_BACKDROP_FULL);
        assert!(!screen.reader_page_loaded, "the load frame draws black");
        // The half-page's TIM pages in and falls into the slide's first step.
        screen.tick();
        assert_eq!(screen.reader_state, ReaderState::SlideIn);
        assert!(screen.reader_page_loaded);
        assert_eq!(screen.reader_slide, 146, "300 / 2 slides the first step");
        let mut positions = vec![screen.reader_slide];
        while screen.reader_state != ReaderState::Idle {
            screen.tick();
            positions.push(screen.reader_slide);
        }
        assert_eq!(screen.reader_slide, 0, "the page settles on the box");
        assert_eq!(positions.last(), Some(&0));
        assert_eq!(positions.len(), 8, "state 2 plus the seven state-3 frames");

        // The busy lock releases one frame after the slide lands, and the
        // input is only read the frame after that.
        assert!(screen.reader_busy);
        screen.tick();
        assert!(!screen.reader_busy);
        assert!(screen.reader_busy_frame, "the release frame still locks");
        screen.tick();
        assert!(!screen.reader_busy_frame);

        // A page-down first slides the current page off to the left, then
        // eases the next one in from the right; the page advances only once
        // the old one is clear.
        screen.handle_input(&mut state, MenuInput::Right);
        assert_eq!(screen.reader_state, ReaderState::SlideOutLeft);
        assert_eq!(screen.reader_slide, -1, "the input frame steps the slide");
        let mut min_slide = 0;
        while screen.reader_state == ReaderState::SlideOutLeft {
            screen.tick();
            min_slide = min_slide.min(screen.reader_slide);
            if screen.reader_state == ReaderState::SlideOutLeft {
                assert_eq!(screen.page, 0, "the old page holds through the slide-out");
            }
        }
        assert!(min_slide < -0x80, "the old page left through the left edge");
        assert_eq!(screen.page, 1);
        // One load frame, then the right-hand entry slide.
        screen.tick();
        assert_eq!(screen.reader_state, ReaderState::PageLoad);
        screen.tick();
        assert_eq!(screen.reader_state, ReaderState::SlideIn);
        assert_eq!(screen.reader_slide, 146);
        while screen.reader_state != ReaderState::Idle {
            screen.tick();
        }
        assert_eq!(screen.reader_slide, 0);
        settle_reader(&mut screen);

        // A page-up mirrors it: out to the right, then in from the left.
        screen.handle_input(&mut state, MenuInput::Left);
        assert_eq!(screen.reader_state, ReaderState::SlideOutRight);
        assert_eq!(screen.reader_slide, 1);
        let mut max_slide = 0;
        while screen.reader_state == ReaderState::SlideOutRight {
            screen.tick();
            max_slide = max_slide.max(screen.reader_slide);
        }
        assert!(max_slide > 0x80, "the page left through the right edge");
        assert_eq!(screen.page, 0);
        // The unpack frame (0xa) holds the +511 slide; the next frame is the
        // load, which falls into the left entry's first step.
        assert_eq!(screen.reader_state, ReaderState::UnpackLeft);
        assert_eq!(screen.reader_slide, 511);
        screen.tick();
        assert_eq!(screen.reader_state, ReaderState::PrevPage);
        assert_eq!(screen.reader_slide, 511, "the load frame holds the slide");
        screen.tick();
        assert_eq!(screen.reader_state, ReaderState::SlideInLeft);
        assert_eq!(screen.reader_slide, -146, "state 0xb halves the speed in");
        while screen.reader_state != ReaderState::Idle {
            screen.tick();
        }
        assert_eq!(screen.reader_slide, 0);
        settle_reader(&mut screen);

        // Cancel draws the settled page on its own frame (the original
        // notices the cancel after the reader's advance), then slides out.
        screen.handle_input(&mut state, MenuInput::Cancel);
        assert_eq!(screen.reader_slide, 0, "the cancel frame still holds");
        screen.tick();
        assert_eq!(screen.reader_state, ReaderState::ExitSlide);
        assert_eq!(screen.reader_slide, -1);
        let mut exit_frames = 1;
        while screen.mode != FileMode::List {
            screen.tick();
            exit_frames += 1;
        }
        // One hold frame, one cover-load frame and the hand-off, then the
        // eight-frame fade back over the selector. The selector inherits the
        // exit slide's offset (its cover stays off-screen) until the ramp
        // completes.
        assert_eq!(exit_frames, 9 + 16 + 3);
        assert_eq!(screen.fade, 0xFF);
        assert_eq!(screen.slide_x, -511, "the cover stays parked clear");
        assert_eq!(screen.slide_y, 0);
        while screen.fade_out {
            screen.tick();
        }
        assert_eq!(screen.fade, 0);
        assert_eq!(screen.slide_x, 0, "the ramp completion resets the slide");
    }

    #[test]
    fn a_pickup_reader_reports_its_exit_instead_of_returning_to_the_list() {
        let mut state = game(0);
        state.set_file_collected(0, true);
        state.set_file_collected(1, true);
        let mut screen = FileScreen::default();
        // Item 0x5F is document 0; its list slot is the 0xFE marker, resolved
        // to 0 once the document is collected.
        screen.open_pickup(&state, 0x5F);
        assert_eq!(screen.mode, FileMode::Reader);
        assert_eq!(screen.book, 0);
        assert_eq!(screen.slot, 2, "the 0xFE list slot resolves to file 0");
        assert!(!screen.take_reader_exit());
        settle_reader(&mut screen);

        // The reader's first half-page is document 0's first page; walking
        // past its last half-page ends the flow through the exit slide.
        let count = page_count(0);
        for _ in 1..count {
            screen.handle_input(&mut state, MenuInput::Right);
            settle_reader(&mut screen);
        }
        assert_eq!(screen.page, count - 1);
        assert!(!screen.reader_last_page);
        screen.handle_input(&mut state, MenuInput::Right);
        settle_reader(&mut screen);
        assert!(
            screen.reader_last_page,
            "the last half-page is acknowledged"
        );
        screen.handle_input(&mut state, MenuInput::Confirm);
        settle_reader(&mut screen);
        assert!(
            screen.take_reader_exit(),
            "the pick-up reader reports its finished exit"
        );
        assert_eq!(
            screen.mode,
            FileMode::Reader,
            "a pick-up reader never falls back to the selector"
        );
        assert!(!screen.take_reader_exit(), "the flag is consumed once");
    }
}
