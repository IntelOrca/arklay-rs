//! HD texture replacement by content hash (`htexture/`).
//!
//! A mod pack may carry any number of images anywhere under `htexture/`,
//! each named by the [`crate::hash`] CRC of the texture it replaces
//! (`htexture/bgd/0259741A.webp`, `htexture/a/b/C5157461_3B.png`): the first
//! eight hex digits of the file stem are the hash and anything after an `_`
//! is ignored. The directories carry no meaning, so a TeamX-style
//! `hires/<class>/` tree can be dropped in as it is.
//!
//! The lookup is the last step of loading a texture. The usual routing has
//! already picked the file and decoded it; when that texture reaches the GPU
//! renderer its texels are hashed, and only a hash that names a file in the
//! table swaps in the HD image. A texture a mod has replaced by path
//! therefore keeps its own art, and an HD image can never land on the wrong
//! texels.
//!
//! The PC game uploaded each texture in its own conversion, so one texture can
//! carry any of a few hashes ([`image_candidates`], [`page_candidates`]); the
//! first one in the table wins. Each candidate also states its frame: the
//! extent, in source texels, that the HD image covers. A room mask hashes a
//! 256 x 256 window whatever its page size, so a 1024 x 1024 replacement of a
//! 128 x 128 page draws its top-left quarter.
//!
//! Only the GPU renderer draws replacements; the software rasterizer stays
//! the 320 x 240 reference. The 2D screens (files, item pictures, the status
//! screen, maps, the select and epilogue backgrounds) are software overlays, so a replaced piece of
//! their art is drawn on the GPU as an [`Overlay`] quad while the overlay
//! leaves a hole for it (see [`Overlay`]).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result};

use crate::hash::{Expand, Surface, texel};
use crate::model::{PALETTE_ROW_LEN, Texture8};
use crate::pack::Pack;
use crate::state::Image;

/// The pack directory replacements are found under, at any depth.
pub const ROOT: &str = "htexture/";

/// One hash a texture may be known by, and the source-texel extent an HD
/// replacement found by it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub hash: u32,
    pub frame: [u32; 2],
}

/// A replacement found for a texture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// The pack entry holding the HD image.
    pub path: String,
    /// The source-texel extent the HD image covers.
    pub frame: [u32; 2],
}

/// A decoded replacement's file and frame.
type DecodedKey = (String, [u32; 2]);

/// The `htexture/` replacements of a pack and its mod layers, by hash.
#[derive(Debug, Default)]
pub struct HdTable {
    files: HashMap<u32, String>,
    /// Textures already reported as having no replacement.
    reported: Mutex<HashSet<u32>>,
    /// Decoded 2D replacements by path and frame, held weakly: while any
    /// screen or the GPU's texture cache keeps one alive, every lookup gets
    /// that same [`Replacement`], so a reopened screen is neither decoded
    /// nor uploaded again.
    decoded: Mutex<HashMap<DecodedKey, Weak<Replacement>>>,
}

impl HdTable {
    /// Index every image under [`ROOT`] in the merged view of `pack`. A stem
    /// with a suffix (`C5157461_3B`) only fills a hash no bare stem names.
    pub fn scan(pack: &Pack) -> Self {
        let mut files = HashMap::new();
        let mut suffixed = HashMap::new();
        for path in pack.paths() {
            let lower = path.to_ascii_lowercase();
            if !lower.starts_with(ROOT) {
                continue;
            }
            let Some((stem, extension)) = lower.rsplit('/').next().and_then(|n| n.rsplit_once('.'))
            else {
                continue;
            };
            if !matches!(extension, "webp" | "png" | "bmp") {
                continue;
            }
            let (digits, bare) = match stem.split_once('_') {
                Some((digits, _)) => (digits, false),
                None => (stem, true),
            };
            if digits.len() != 8 {
                continue;
            }
            let Ok(hash) = u32::from_str_radix(digits, 16) else {
                continue;
            };
            let slot = if bare { &mut files } else { &mut suffixed };
            slot.entry(hash).or_insert_with(|| path.to_string());
        }
        for (hash, path) in suffixed {
            files.entry(hash).or_insert(path);
        }
        HdTable {
            files,
            reported: Mutex::new(HashSet::new()),
            decoded: Mutex::new(HashMap::new()),
        }
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The replacement of the first candidate the table names.
    ///
    /// With a non-empty table a texture none of whose candidates match is
    /// reported once on stderr, naming `what` and its hashes, so a pack's
    /// coverage gaps show up while playing.
    pub fn find(&self, candidates: &[Candidate], what: impl FnOnce() -> String) -> Option<Found> {
        let found = self.find_quietly(candidates);
        if found.is_none()
            && !self.files.is_empty()
            && let Some(first) = candidates.first()
            && self
                .reported
                .lock()
                .is_ok_and(|mut reported| reported.insert(first.hash))
        {
            let hashes: Vec<String> = candidates
                .iter()
                .map(|candidate| format!("{:08X}", candidate.hash))
                .collect();
            eprintln!(
                "htexture: no replacement for {}: {}",
                what(),
                hashes.join(" ")
            );
        }
        found
    }

    /// [`HdTable::find`] without reporting a miss.
    pub fn find_quietly(&self, candidates: &[Candidate]) -> Option<Found> {
        candidates.iter().find_map(|candidate| {
            self.files.get(&candidate.hash).map(|path| Found {
                path: path.clone(),
                frame: candidate.frame,
            })
        })
    }
}

/// A decoded HD image and the source-texel extent it covers.
pub struct Replacement {
    pub image: Image,
    pub frame: [u32; 2],
}

impl std::fmt::Debug for Replacement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Replacement({}x{} over {}x{})",
            self.image.width, self.image.height, self.frame[0], self.frame[1]
        )
    }
}

/// The decoded replacement of the first of `candidates` `pack` names; a
/// miss is reported like [`HdTable::find`], a file that fails to decode on
/// stderr.
pub fn replacement(
    pack: &Pack,
    candidates: &[Candidate],
    what: impl FnOnce() -> String,
) -> Option<Arc<Replacement>> {
    let table = pack.htexture();
    let found = table.find(candidates, what)?;
    let frame = [found.frame[0].max(1), found.frame[1].max(1)];
    let key = (found.path, frame);
    if let Some(kept) = table
        .decoded
        .lock()
        .ok()
        .and_then(|decoded| decoded.get(&key).and_then(Weak::upgrade))
    {
        return Some(kept);
    }
    match load(pack, &key.0) {
        Ok(image)
            if image.width > 0
                && image.height > 0
                && image.rgba.len() >= (image.width * image.height * 4) as usize =>
        {
            let replacement = Arc::new(Replacement { image, frame });
            if let Ok(mut decoded) = table.decoded.lock() {
                decoded.retain(|_, kept| kept.strong_count() > 0);
                decoded.insert(key, Arc::downgrade(&replacement));
            }
            Some(replacement)
        }
        Ok(_) => None,
        Err(err) => {
            eprintln!("htexture: {err:#}");
            None
        }
    }
}

/// One replaced 2D draw: the HD image drawn over the GPU scene where the
/// software art would have been.
#[derive(Clone, Debug)]
pub struct Quad {
    pub image: Arc<Replacement>,
    /// `[x0, y0, x1, y1]` in frame pixels.
    pub dst: [f32; 4],
    /// `[u0, v0, u1, v1]` at the corners, normalised over the image; a
    /// mirrored draw swaps a pair, and values past `0..=1` repeat the edge.
    pub uv: [f32; 4],
    /// The draw's colour multiplier (above one for the FILE backdrop's ramp).
    pub colour: [f32; 3],
    /// `[x, y, w, h]` in frame pixels the quad is cut to.
    pub clip: [i32; 4],
}

/// The 2D replacements of one frame, captured while its overlay is drawn.
///
/// A replaced draw still runs in software, but each texel it would write is
/// the hole grey instead: `0` in the pass over a black scene and `255` in
/// the pass over a white one. The composite then shows the scene there,
/// which the GPU renderer draws with the frame's quads on top in order, and
/// whatever the overlay draws later (text, fades) still lands over it. A
/// blended draw (the FILE backdrop's half alpha) blends the hole grey, which
/// is exact over art the scene does not show through.
///
/// Each punched texel also records its quad in `coverage`, and the GPU
/// keeps a quad's pixels only where it wrote last, so the HD art covers
/// exactly what the original did even where replaced draws overlap.
#[derive(Debug, Default)]
pub struct Overlay {
    pub quads: Vec<Quad>,
    /// The hole grey of the pass being drawn.
    pub hole: u8,
    /// Whether a draw is punching its hole right now.
    pub punching: bool,
    /// The frame position of the framebuffer being drawn into: a band or
    /// the 4:3 columns of a canvas draw at an offset.
    pub offset: [i32; 2],
    /// Whether the gameplay scene lies under the overlay; without it the
    /// quads are drawn over black.
    pub scene: bool,
    /// The `[width, height]` of the frame captured.
    pub size: [u32; 2],
    /// Per frame pixel, row-major: one more than the index of the quad that
    /// punched it last, or `0`.
    pub coverage: Vec<u16>,
}

impl Overlay {
    /// A `size` frame's capture for the pass whose hole grey is `hole`.
    pub fn new(hole: u8, size: [u32; 2]) -> Self {
        let mut overlay = Self::default();
        overlay.reset(hole, size);
        overlay
    }

    /// Start over as [`Overlay::new`], keeping the allocations.
    pub fn reset(&mut self, hole: u8, size: [u32; 2]) {
        self.quads.clear();
        self.hole = hole;
        self.punching = false;
        self.offset = [0, 0];
        self.scene = false;
        self.resize(size);
    }

    /// Clear the coverage for a `size` frame.
    pub fn resize(&mut self, size: [u32; 2]) {
        self.size = size;
        self.coverage.clear();
        self.coverage.resize(size[0] as usize * size[1] as usize, 0);
    }

    /// Record that the draw being punched wrote pixel `(x, y)` of the
    /// framebuffer at `offset`.
    pub fn cover(&mut self, x: i32, y: i32) {
        let (x, y) = (x + self.offset[0], y + self.offset[1]);
        if x < 0 || y < 0 || x as u32 >= self.size[0] || y as u32 >= self.size[1] {
            return;
        }
        let quad = self.quads.len().min(usize::from(u16::MAX)) as u16;
        self.coverage[y as usize * self.size[0] as usize + x as usize] = quad;
    }
}

impl Clone for Overlay {
    fn clone(&self) -> Self {
        let mut overlay = Self::default();
        overlay.clone_from(self);
        overlay
    }

    /// Copies into the existing buffers, as the session's frame is copied
    /// to the app's every frame.
    fn clone_from(&mut self, source: &Self) {
        self.quads.clone_from(&source.quads);
        self.hole = source.hole;
        self.punching = source.punching;
        self.offset = source.offset;
        self.scene = source.scene;
        self.size = source.size;
        self.coverage.clone_from(&source.coverage);
    }
}

/// Decode the HD image at `path`.
pub fn load(pack: &Pack, path: &str) -> Result<Image> {
    let data = pack
        .read(path)
        .with_context(|| format!("failed to read {path}"))?;
    let extension = path.rsplit_once('.').map_or("", |(_, ext)| ext);
    crate::assets::decode_image_bytes(&extension.to_ascii_lowercase(), &data)
        .with_context(|| format!("failed to decode {path}"))
}

/// A 15-bit colour back from the decoders' 8-bit channels. Every decoder
/// widens a 5-bit channel to `v * 255 / 31`, `v << 3` or `v << 3 | v >> 2`,
/// all of which keep `v` in the top five bits.
fn colour_of(rgb: [u8; 3]) -> u16 {
    u16::from(rgb[0] >> 3) | u16::from(rgb[1] >> 3) << 5 | u16::from(rgb[2] >> 3) << 10
}

/// The misc/model finish: black (`1`) and transparent (`0`) keep a clear
/// alpha, every other texel is opaque.
fn finish_model(texel: u32) -> u32 {
    if texel <= 1 {
        texel
    } else {
        texel | 0xff00_0000
    }
}

/// The mask finish: everything but a transparent texel is opaque.
fn finish_mask(texel: u32) -> u32 {
    if texel == 0 { 0 } else { texel | 0xff00_0000 }
}

/// The hashes an RGBA image (background, mask page, effect sheet) may carry,
/// most specific first.
pub fn image_candidates(image: &Image) -> Vec<Candidate> {
    let (width, height) = (image.width as usize, image.height as usize);
    let pixels = image.rgba.as_chunks::<4>().0;
    if width == 0 || height == 0 || pixels.len() < width * height {
        return Vec::new();
    }
    let pixels = &pixels[..width * height];
    let surface = |expand: Expand, black_clear: bool| Surface {
        width,
        height,
        texels: pixels
            .iter()
            .map(|&[r, g, b, a]| {
                let colour = colour_of([r, g, b]);
                if a == 0 || (black_clear && colour == 0) {
                    0
                } else {
                    texel(colour, expand)
                }
            })
            .collect(),
    };
    let size = [image.width, image.height];
    let mut candidates = Vec::with_capacity(7);
    let replicated = surface(Expand::Replicate, false);
    // A camera background hashes its left 256 x 240 but is drawn whole; a
    // `DATA/*.PIX` screen the same way, but widened like a menu texture.
    if width >= 256 {
        candidates.push(Candidate {
            hash: replicated.window_crc(256, 240, |t| t | 0xff00_0000),
            frame: size,
        });
        candidates.push(Candidate {
            hash: surface(Expand::Shift, false).window_crc(256, 240, |t| t | 0xff00_0000),
            frame: size,
        });
    }
    candidates.push(Candidate {
        hash: replicated.window_crc(256, 256, finish_mask),
        frame: [256, 256],
    });
    for black_clear in [false, true] {
        let shifted = surface(Expand::Shift, black_clear);
        candidates.push(Candidate {
            hash: shifted.window_crc(256, 256, finish_model),
            frame: [256, 256],
        });
        if (width, height) != (256, 256) {
            candidates.push(Candidate {
                hash: shifted.window_crc(width, height, finish_model),
                frame: size,
            });
        }
    }
    candidates
}

/// Which colours of an indexed page the PC upload made transparent.
#[derive(Clone, Copy)]
enum Key {
    /// None: every texel is drawn.
    None,
    /// Colour `0x0000`, the PlayStation's transparent texel.
    Zero,
    /// Any black, with or without the STP bit.
    Black,
}

/// The hashes CLUT row `row` of an indexed model page may carry, most common
/// first.
///
/// The PC game uploads a model page 256 texels wide: a 128-wide page is
/// stretched to twice its width and hashed in a 256 x 256 window, so its
/// replacement covers 128 x 256 source texels. A menu page (the status
/// screen, the files, the item box) is uploaded unstretched with its black
/// clear, in a 256 x 256 window or, below that, at its own size.
pub fn page_candidates(texture: &Texture8, row: usize) -> Vec<Candidate> {
    let (width, height) = (texture.width as usize, texture.height as usize);
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let colours: Vec<u16> = (0..PALETTE_ROW_LEN)
        .map(|index| {
            let [r, g, b, _] = texture.palette(row, index as u8);
            let stp = texture.palette_stp(row, index as u8);
            colour_of([r, g, b]) | if stp { 0x8000 } else { 0 }
        })
        .collect();
    let index_at = |x: usize, y: usize| texture.indices.get(y * width + x).copied().unwrap_or(0);
    let surface = |key: Key, stretch: usize| {
        let mut texels = Vec::with_capacity(width * stretch * height);
        for y in 0..height {
            for x in 0..width * stretch {
                let colour = colours[usize::from(index_at(x / stretch, y))];
                let clear = match key {
                    Key::None => false,
                    Key::Zero => colour == 0,
                    Key::Black => colour & 0x7fff == 0,
                };
                texels.push(if clear {
                    0
                } else {
                    texel(colour, Expand::Shift)
                });
            }
        }
        Surface {
            width: width * stretch,
            height,
            texels,
        }
    };
    let stretch = if width < 256 && 256 % width == 0 {
        256 / width
    } else {
        1
    };
    let window = [(256 / stretch) as u32, 256];
    let mut candidates = Vec::with_capacity(6);
    for key in [Key::Zero, Key::Black, Key::None] {
        candidates.push(Candidate {
            hash: surface(key, stretch).window_crc(256, 256, finish_model),
            frame: window,
        });
    }
    // The original whole-texture hash some item and enemy pages carry.
    candidates.push(Candidate {
        hash: surface(Key::None, 1).window_crc(width, height, finish_model),
        frame: [texture.width, texture.height],
    });
    if stretch > 1 {
        candidates.push(Candidate {
            hash: surface(Key::Black, 1).window_crc(256, 256, finish_model),
            frame: [256, 256],
        });
    }
    if width < 256 && height < 256 {
        candidates.push(Candidate {
            hash: surface(Key::Black, 1).window_crc(width, height, finish_model),
            frame: [texture.width, texture.height],
        });
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::{Class, crc32};

    fn pack_of(paths: &[&str]) -> Pack {
        let mut writer = crate::pack::PackWriter::new();
        for path in paths {
            writer.add(path, Vec::new()).unwrap();
        }
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    #[test]
    fn the_table_indexes_every_hex_stem_under_htexture() {
        let pack = pack_of(&[
            "htexture/bgd/0259741A.webp",
            "htexture/deep/er/C5157461_3B.png",
            "htexture/deep/C5157461.png",
            "htexture/misc/ABCDEF01_8E.webp",
            "htexture/notes.txt",
            "htexture/misc/XYZ.webp",
            "roomcut/12345678.webp",
        ]);
        let table = HdTable::scan(&pack);
        assert_eq!(table.len(), 3);
        let find = |hash| {
            table
                .find(
                    &[Candidate {
                        hash,
                        frame: [1, 1],
                    }],
                    String::new,
                )
                .map(|found| found.path)
        };
        assert_eq!(
            find(0x0259_741a).as_deref(),
            Some("htexture/bgd/0259741A.webp")
        );
        // The bare stem wins over a suffixed copy of the same hash.
        assert_eq!(
            find(0xc515_7461).as_deref(),
            Some("htexture/deep/C5157461.png")
        );
        assert_eq!(
            find(0xabcd_ef01).as_deref(),
            Some("htexture/misc/ABCDEF01_8E.webp")
        );
        assert_eq!(find(0x1234_5678), None);
    }

    #[test]
    fn the_first_named_candidate_wins_with_its_frame() {
        let pack = pack_of(&["htexture/00000002.webp", "htexture/00000003.webp"]);
        let table = HdTable::scan(&pack);
        let candidates = [
            Candidate {
                hash: 1,
                frame: [1, 1],
            },
            Candidate {
                hash: 3,
                frame: [3, 3],
            },
            Candidate {
                hash: 2,
                frame: [2, 2],
            },
        ];
        let found = table.find(&candidates, String::new).unwrap();
        assert_eq!(found.path, "htexture/00000003.webp");
        assert_eq!(found.frame, [3, 3]);
    }

    #[test]
    fn a_replacement_is_shared_while_any_holder_keeps_it() {
        let bmp = crate::bmp::encode_to_vec(&Image {
            width: 2,
            height: 2,
            rgba: vec![255; 16],
        })
        .unwrap();
        let mut writer = crate::pack::PackWriter::new();
        writer.add("htexture/misc/00000007.bmp", bmp).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        let candidates = [Candidate {
            hash: 7,
            frame: [4, 4],
        }];
        let first = replacement(&pack, &candidates, String::new).unwrap();
        let again = replacement(&pack, &candidates, String::new).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        drop((first, again));
        // Once dropped, the next lookup decodes afresh.
        let decoded = replacement(&pack, &candidates, String::new).unwrap();
        assert_eq!(decoded.frame, [4, 4]);
        assert_eq!(decoded.image.width, 2);
    }

    #[test]
    fn a_background_image_carries_its_retail_hash() {
        // A 320x240 picture decoded the way the converter does
        // (`v * 255 / 31`) hashes like the 15-bit original.
        let colours: Vec<u16> = (0..320 * 240).map(|i| (i % 0x7fff) as u16).collect();
        let widen = |v: u16| ((v & 31) * 255 / 31) as u8;
        let image = Image {
            width: 320,
            height: 240,
            rgba: colours
                .iter()
                .flat_map(|&c| [widen(c), widen(c >> 5), widen(c >> 10), 255])
                .collect(),
        };
        let retail = Surface::from_bgr555(320, 240, &colours, Expand::Replicate).hash(Class::Bgd);
        let candidates = image_candidates(&image);
        assert_eq!(
            candidates[0],
            Candidate {
                hash: retail,
                frame: [320, 240]
            }
        );
    }

    #[test]
    fn a_128_wide_model_page_hashes_stretched_into_a_256_window() {
        // Index 0 is colour 0x0000 (transparent), index 1 is pure red.
        let mut palettes = vec![[0, 0, 0, 255]; PALETTE_ROW_LEN];
        palettes[1] = [255, 0, 0, 255];
        let texture = Texture8 {
            width: 128,
            height: 1,
            indices: (0..128).map(|x| (x % 2) as u8).collect(),
            palettes,
            stp: vec![false; PALETTE_ROW_LEN],
        };
        let mut expected = Vec::new();
        for x in 0..256 {
            let texel: u32 = if (x / 2) % 2 == 1 { 0xfff8_0000 } else { 0 };
            expected.extend_from_slice(&texel.to_le_bytes());
        }
        expected.resize(256 * 256 * 4, 0);
        let candidates = page_candidates(&texture, 0);
        assert_eq!(
            candidates[0],
            Candidate {
                hash: crc32(&expected),
                frame: [128, 256]
            }
        );
    }

    #[test]
    fn a_pix_screen_also_hashes_widened_like_a_menu_texture() {
        let colours: Vec<u16> = (0..320 * 240).map(|i| (i % 0x7fff) as u16).collect();
        let widen = |v: u16| ((v & 31) * 255 / 31) as u8;
        let image = Image {
            width: 320,
            height: 240,
            rgba: colours
                .iter()
                .flat_map(|&c| [widen(c), widen(c >> 5), widen(c >> 10), 255])
                .collect(),
        };
        let shifted =
            Surface::from_bgr555(320, 240, &colours, Expand::Shift)
                .window_crc(256, 240, |t| t | 0xff00_0000);
        assert_eq!(
            image_candidates(&image)[1],
            Candidate {
                hash: shifted,
                frame: [320, 240]
            }
        );
    }

    #[test]
    fn a_menu_page_hashes_unstretched_with_its_black_clear() {
        // Index 0 is black with the STP bit (cleared only by the black key),
        // index 1 pure red.
        let mut palettes = vec![[0, 0, 0, 255]; PALETTE_ROW_LEN];
        palettes[1] = [255, 0, 0, 255];
        let mut stp = vec![false; PALETTE_ROW_LEN];
        stp[0] = true;
        let texture = Texture8 {
            width: 128,
            height: 1,
            indices: (0..128).map(|x| (x % 2) as u8).collect(),
            palettes,
            stp,
        };
        let mut row = Vec::new();
        for x in 0..128 {
            let texel: u32 = if x % 2 == 1 { 0xfff8_0000 } else { 0 };
            row.extend_from_slice(&texel.to_le_bytes());
        }
        let mut window = row.clone();
        window.resize(256 * 256 * 4, 0);
        let candidates = page_candidates(&texture, 0);
        assert!(candidates.contains(&Candidate {
            hash: crc32(&window),
            frame: [256, 256]
        }));
        // Below 256 x 256 the page also hashes at its own size.
        assert_eq!(
            candidates.last(),
            Some(&Candidate {
                hash: crc32(&row),
                frame: [128, 1]
            })
        );
    }
}
