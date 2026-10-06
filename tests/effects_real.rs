//! Real-asset effect tests: the RDT sprite pipeline and the room 100
//! chandelier spawn.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test effects_real -- --ignored`

mod common;

use arklay::effects;
use arklay::game::GameState;
use arklay::pack::Pack;
use arklay::state::RoomId;

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room1000_chandelier_effect_spawns() {
    let Some((root, pack_path)) = common::asset_env() else {
        return;
    };
    let data = std::fs::read(root.join("JPN/STAGE1/ROOM1000.RDT")).unwrap();
    let state = arklay::rdt::parse(&data, RoomId::parse("1000").unwrap()).unwrap();

    // The room declares its single sprite (type 38); the chandelier script
    // spawns the global weapon type 9 (smoke) with depth group 7.
    assert_eq!(state.effects.index[0], 38);
    assert!(state.effects.sprite(38).is_some());

    let pack = Pack::open(&pack_path).unwrap();
    let weapon = effects::WeaponEffects::load(&pack);
    assert!(weapon.warnings.is_empty(), "{:?}", weapon.warnings);
    assert_eq!(weapon.slot_of(9), Some(1));

    let mut game = GameState::default();
    game.weapon_effects = weapon;
    let slot = effects::create(
        &mut game,
        &state.effects,
        9,
        7,
        0,
        [4420, -2500, 3800],
        1536,
        0,
    )
    .expect("the chandelier smoke spawns");

    assert_eq!(slot, 63);
    let effect = game.effects.slot(usize::from(slot)).unwrap();
    assert_eq!(effect.effect_type, 9);
    assert_eq!(effect.sprite, Some(9));
    assert_eq!(effect.depth_group, 7);
    assert_eq!(effect.local_offset, [4420, -2500, 3800]);
    assert_eq!(effect.spawn_pos, [4420, -2500, 3800]);
    assert_eq!(effect.yaw, 1536);
    assert_eq!(effect.anim_id, 2);
    assert_eq!(effect.attach, effects::Attach::Identity);
    assert_eq!(game.effects.active_count(), 1);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room_effect_pages_pack_inside_the_loaded_page_count() {
    let Some((root, pack_path)) = common::asset_env() else {
        return;
    };
    // ROOM1010 declares three sprites against esp000 + esp201; they must pack
    // onto pages the runtime loads (0-3) and their UVs must stay inside.
    let data = std::fs::read(root.join("JPN/STAGE1/ROOM1010.RDT")).unwrap();
    let state = arklay::rdt::parse(&data, RoomId::parse("1010").unwrap()).unwrap();
    assert_eq!(state.effects.index[..3], [3, 4, 32][..]);
    assert_eq!(state.effects.sprites.len(), 3);

    let weapon = effects::WeaponEffects::load(&Pack::open(&pack_path).unwrap());
    let mut room = state.effects.clone();
    effects::pages::pack(&weapon, &mut room);
    for sprite in &room.sprites {
        assert!(
            sprite.info.page_index() < 4,
            "sprite {} packed page {}",
            sprite.index,
            sprite.info.page_index()
        );
        assert!(
            u16::from(sprite.info.page_v) + sprite.geometry.height <= 256,
            "sprite {} art region runs past the page",
            sprite.index
        );
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_weapon_install_packs_each_room_once() {
    let Some((root, pack_path)) = common::asset_env() else {
        return;
    };
    // The engine boot installs the global weapon sheets and then resolves the
    // room effects once. A second packing pass would bias every room sprite's
    // UV V byte again, so both rooms must equal a single `pack` over the
    // freshly parsed records.
    let pack = Pack::open(&pack_path).unwrap();
    let weapon = effects::WeaponEffects::load(&pack);
    for name in ["1010", "1080"] {
        let id = RoomId::parse(name).unwrap();
        let sim =
            arklay::engine::simulate_room(&pack, id, 0, arklay::player::Input::default()).unwrap();
        let data = std::fs::read(root.join(format!("JPN/STAGE1/ROOM{name}.RDT"))).unwrap();
        let state = arklay::rdt::parse(&data, id).unwrap();
        let mut expected = state.effects.clone();
        effects::pages::pack(&weapon, &mut expected);
        assert_eq!(
            *sim.game.room_effects, expected,
            "ROOM{name} must be packed exactly once with the weapon sheets installed"
        );
    }
}

// ============================================================================
// M10 slices 3-4: the corpus lifecycle audit and the effect captures.
//
// Run with:
// `ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test effects_real -- --ignored`
// ============================================================================

use std::rc::Rc;

use arklay::engine;
use arklay::player;
use arklay::render::Camera;
use arklay::state::{Image, RoomState};

fn changed_pixels(a: &Image, b: &Image) -> usize {
    a.rgba
        .chunks(4)
        .zip(b.rgba.chunks(4))
        .filter(|(left, right)| left != right)
        .count()
}

fn pixel(image: &Image, x: i32, y: i32) -> [u8; 4] {
    if x < 0 || y < 0 || x >= image.width as i32 || y >= image.height as i32 {
        return [0, 0, 0, 0];
    }
    let offset = ((y as u32 * image.width + x as u32) * 4) as usize;
    image.rgba[offset..offset + 4].try_into().unwrap()
}

/// How a sprite's frame chain ends when stepped like the animation tick: on
/// the `(0, 0)` free entry, on a visited entry (a deliberate loop) or by
/// running off the frame table (a malformed sprite the animation stalls on).
#[derive(Debug, PartialEq, Eq)]
enum Chain {
    Frees,
    Loops,
    RunsOff,
}

fn frame_chain(sprite: &arklay::effects::EffectSprite, entry: u8) -> Chain {
    let frames = &sprite.info.frames;
    let mut visited = Vec::new();
    let mut index = usize::from(entry);
    loop {
        if visited.contains(&index) {
            return Chain::Loops;
        }
        visited.push(index);
        let Some(frame) = frames.get(index) else {
            return Chain::RunsOff;
        };
        if frame.uv_index == 0 && frame.delay == 0 {
            return Chain::Frees;
        }
        index = if frame.delay == 0xFF {
            usize::from(frame.uv_index)
        } else {
            index + 1
        };
        if visited.len() > frames.len() {
            return Chain::RunsOff;
        }
    }
}

/// A room's game after its init script (zero gameplay ticks).
fn booted(pack: &arklay::pack::Pack, id: RoomId) -> engine::SimulatedRoom {
    engine::simulate_room(pack, id, 0, player::Input::default()).unwrap()
}

fn render(pack: &arklay::pack::Pack, id: RoomId, sim: &engine::SimulatedRoom) -> Image {
    let mut game = sim.game.clone();
    engine::render_game_frame(pack, id, &sim.room, &mut game, &sim.player).unwrap()
}

fn cleared_render(pack: &arklay::pack::Pack, id: RoomId, sim: &engine::SimulatedRoom) -> Image {
    let mut cleared = sim.game.clone();
    cleared.effects.clear();
    engine::render_game_frame(pack, id, &sim.room, &mut cleared, &sim.player).unwrap()
}

/// A floor point inside the current camera's switch-zone header that projects
/// on screen. The header can sit behind the camera, so sample its bounding box.
fn visible_zone_point(room: &RoomState, camera: &Camera) -> [i32; 3] {
    let header = room
        .zones
        .iter()
        .find(|zone| zone.cam_from >= 0 && zone.cam_from as usize == room.current_cut)
        .expect("the cut has a switch zone");
    let xs: Vec<i32> = header.corners.iter().map(|c| i32::from(c[0])).collect();
    let zs: Vec<i32> = header.corners.iter().map(|c| i32::from(c[1])).collect();
    let (x_lo, x_hi) = (xs.iter().min().unwrap(), xs.iter().max().unwrap());
    let (z_lo, z_hi) = (zs.iter().min().unwrap(), zs.iter().max().unwrap());
    for i in 0..=16 {
        for j in 0..=16 {
            let x = x_lo + (x_hi - x_lo) * i / 16;
            let z = z_lo + (z_hi - z_lo) * j / 16;
            if !header.contains(x, z) {
                continue;
            }
            if let Some(screen) = camera.project([x, 0, z])
                && (0..320).contains(&screen[0])
                && (0..240).contains(&screen[1])
            {
                return [x, 0, z];
            }
        }
    }
    panic!("the camera's switch zone has no on-screen point");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_effect_corpus_600_ticks_has_no_placeholders() {
    let Some((_, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let mut rooms: Vec<String> = pack
        .paths()
        .filter(|path| path.starts_with("room/") && path.ends_with(".rdt"))
        .map(|path| path[5..path.len() - 4].to_string())
        .collect();
    rooms.sort();
    assert!(rooms.len() > 300, "expected the shipped room corpus");

    let mut simulated = 0usize;
    let mut animating = 0usize;
    for name in &rooms {
        let id = RoomId::parse(name).unwrap();
        let Ok(sim) = engine::simulate_room(&pack, id, 600, player::Input::default()) else {
            // Rooms without camera cuts are unreferenced editor leftovers.
            continue;
        };
        simulated += 1;
        assert_eq!(
            sim.game.effect_placeholder_hits.iter().sum::<u32>(),
            0,
            "ROOM{name} dispatched a placeholder effect behaviour: {:?}",
            sim.game
                .effect_placeholder_hits
                .iter()
                .enumerate()
                .filter(|(_, count)| **count > 0)
                .collect::<Vec<_>>()
        );
        // The packed sprite placements must stay inside the four pages the
        // runtime loads, and inside the page's 256 rows except for a
        // full-page TIM the cursor's overflow reset cannot fit. A second
        // packing pass would push the UV V bytes out of range, so this also
        // guards the boot path against re-packing.
        for sprite in &sim.game.room_effects.sprites {
            if sprite.geometry.height == 0 && sprite.geometry.clut_rows == 0 {
                continue;
            }
            assert!(
                sprite.info.page_index() < 4,
                "ROOM{name} sprite {} packed onto page {}",
                sprite.index,
                sprite.info.page_index()
            );
            if sprite.geometry.height < 256 {
                assert!(
                    u16::from(sprite.info.page_v) + sprite.geometry.height <= 256,
                    "ROOM{name} sprite {} art region runs past the page",
                    sprite.index
                );
            }
        }
        for (_, effect) in sim.game.effects.active() {
            // A live room sprite's stored V is page-absolute, so its frame art
            // must stay inside the 256-row page. A second packing pass would
            // push it past the page for some rooms.
            if let Some(index) = effect.sprite
                && sim.game.room_effects.sprite(index).is_some()
            {
                assert!(
                    u16::from(effect.uv[1]) + u16::from(effect.size[1]) <= 256,
                    "ROOM{name} effect {} V {} + height {} runs past the page",
                    effect.effect_type,
                    effect.uv[1],
                    effect.size[1]
                );
            }
            assert!(
                arklay::effects::behaviour::implemented(effect.anim_id),
                "ROOM{name} slot is running placeholder anim {}",
                effect.anim_id
            );
            if effect.update_id != 0 {
                assert!(
                    arklay::effects::behaviour::implemented(effect.update_id),
                    "ROOM{name} slot is running placeholder update {}",
                    effect.update_id
                );
            }
            // Every animating slot's frame table must reach the free entry;
            // a non-animating slot's lifetime is script/behaviour driven.
            if effect.flags() & 1 != 0 {
                animating += 1;
                let sprite = effect.sprite.and_then(|index| {
                    sim.game
                        .room_effects
                        .sprite(index)
                        .or_else(|| sim.game.weapon_effects.sprite(index))
                });
                let Some(sprite) = sprite else { continue };
                // The animation either frees the slot or is a deliberate
                // loop (ambient art the scripts kill); a chain that runs off
                // the frame table would stall the slot forever.
                assert_ne!(
                    frame_chain(sprite, effect.frame_entry),
                    Chain::RunsOff,
                    "ROOM{name} slot has an animation that runs off its frame table"
                );
            }
        }
    }
    assert!(
        simulated > 300,
        "only {simulated} rooms simulated; the pack is incomplete"
    );
    assert!(animating > 0, "no animating effect survived the corpus");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room1000_chandelier_effects_animate_and_paint() {
    let Some((_, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("1000").unwrap();
    let early = engine::simulate_room(&pack, id, 5, player::Input::default()).unwrap();
    let late = engine::simulate_room(&pack, id, 40, player::Input::default()).unwrap();
    assert!(
        changed_pixels(&early.frame, &late.frame) > 100,
        "the chandelier scene must change between ticks 5 and 40"
    );

    // Isolate the effect pixels by rendering the same state with an empty pool.
    let with = render(&pack, id, &early);
    let without = cleared_render(&pack, id, &early);
    let effect_pixels = changed_pixels(&with, &without);
    assert!(
        effect_pixels > 100,
        "the chandelier smoke painted only {effect_pixels} pixels"
    );

    // The changed pixels sit around the chandelier's projected position.
    let cut = &early.room.cuts[early.room.current_cut];
    let camera = Camera::from_cut(cut);
    let center = camera
        .project([4420, -2500, 3800])
        .expect("chandelier visible");
    let mut nearby = 0usize;
    for y in 0..with.height as i32 {
        for x in 0..with.width as i32 {
            let dx = x - center[0];
            let dy = y - center[1];
            if dx * dx + dy * dy > 160 * 160 {
                continue;
            }
            if pixel(&with, x, y) != pixel(&without, x, y) {
                nearby += 1;
            }
        }
    }
    assert!(
        nearby > 50,
        "only {nearby} chandelier-region pixels changed"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room3030_mass_mask_hides_and_restores() {
    let Some((_, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("3030").unwrap();
    let mut sim = booted(&pack, id);
    let room_effects = Rc::clone(&sim.game.room_effects);
    let camera = Camera::from_cut(&sim.room.cuts[sim.room.current_cut]);
    let focus = visible_zone_point(&sim.room, &camera);
    // The attic scene's effects: type 11 from the global sheet.
    for _ in 0..3 {
        arklay::effects::create(&mut sim.game, &room_effects, 11, 10, 0, focus, 0, 0);
    }
    sim.game.tick_effects(&sim.room);
    let visible = render(&pack, id, &sim);
    let hidden_expected = cleared_render(&pack, id, &sim);
    let visible_pixels = changed_pixels(&visible, &hidden_expected);
    assert!(visible_pixels > 20, "the spawned effects did not paint");

    // The script's mass_mask OR mode hides every live slot.
    sim.game.effects.modify_flags(0, 0x8000);
    let active = sim.game.effects.active_count();
    assert!(active >= 3, "mass_mask must not free the slots");
    let hidden = render(&pack, id, &sim);
    assert_eq!(
        changed_pixels(&hidden, &hidden_expected),
        0,
        "mass_mask must hide every effect"
    );

    // The paired AND-NOT restores them.
    sim.game.effects.modify_flags(1, 0x8000);
    let restored = render(&pack, id, &sim);
    assert!(
        changed_pixels(&restored, &hidden_expected) > 20,
        "mass_mask must restore every effect"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room5080_clut_rows_tint_differently() {
    let Some((_, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("5080").unwrap();

    let mut low = booted(&pack, id);
    let room_effects = Rc::clone(&low.game.room_effects);
    let camera = Camera::from_cut(&low.room.cuts[low.room.current_cut]);
    let focus = visible_zone_point(&low.room, &camera);
    arklay::effects::create(&mut low.game, &room_effects, 10, 0, 0, focus, 0, 0);
    low.game.tick_effects(&low.room);
    let low_frame = render(&pack, id, &low);

    let mut high = booted(&pack, id);
    let room_effects = Rc::clone(&high.game.room_effects);
    arklay::effects::create(&mut high.game, &room_effects, 10, 8, 0, focus, 0, 0);
    high.game.tick_effects(&high.room);
    let high_frame = render(&pack, id, &high);

    let low_cleared = cleared_render(&pack, id, &low);
    let high_cleared = cleared_render(&pack, id, &high);
    let low_pixels = changed_pixels(&low_frame, &low_cleared);
    let high_pixels = changed_pixels(&high_frame, &high_cleared);
    assert!(low_pixels > 0, "the row-0 passcode light painted nothing");
    assert!(high_pixels > 0, "the row-1 passcode light painted nothing");

    // Compare the two tints at the first painted pixel near the light.
    let center = camera.project(focus).unwrap();
    let painted = |frame: &Image, cleared: &Image| -> Option<[u8; 4]> {
        for radius in 0..80 {
            for y in (center[1] - radius)..=(center[1] + radius) {
                for x in (center[0] - radius)..=(center[0] + radius) {
                    if pixel(frame, x, y) != pixel(cleared, x, y) {
                        return Some(pixel(frame, x, y));
                    }
                }
            }
        }
        None
    };
    let low_color = painted(&low_frame, &low_cleared).expect("row 0 painted a pixel");
    let high_color = painted(&high_frame, &high_cleared).expect("row 1 painted a pixel");
    assert_ne!(
        low_color, high_color,
        "the CLUT-row variants must tint differently (row 0 {low_color:?}, row 1 {high_color:?})"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_effect_outside_the_camera_zone_paints_nothing() {
    let Some((_, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("1000").unwrap();
    let mut sim = booted(&pack, id);
    let room_effects = Rc::clone(&sim.game.room_effects);
    let room: &RoomState = &sim.room;
    let camera_index = room.current_cut;
    let header = room
        .zones
        .iter()
        .find(|zone| zone.cam_from >= 0 && zone.cam_from as usize == camera_index)
        .expect("the cut has a switch zone");

    // A point far outside the zone.
    let mut outside = [0i32; 3];
    for candidate in [
        [-3000, 0, -3000],
        [3000, 0, -3000],
        [-3000, 0, 3000],
        [12000, 0, 12000],
        [-12000, 0, -12000],
    ] {
        if !header.contains(candidate[0], candidate[2]) {
            outside = candidate;
            break;
        }
    }
    assert!(!header.contains(outside[0], outside[2]));

    // Inside the zone the same effect paints; outside it must not.
    let camera = Camera::from_cut(&room.cuts[camera_index]);
    let inside = visible_zone_point(room, &camera);
    arklay::effects::create(&mut sim.game, &room_effects, 11, 10, 0, inside, 0, 0);
    let outside_slot =
        arklay::effects::create(&mut sim.game, &room_effects, 11, 10, 0, outside, 0, 0).unwrap();
    sim.game.tick_effects(&sim.room);
    let with = render(&pack, id, &sim);
    let without = cleared_render(&pack, id, &sim);
    let inside_pixels = changed_pixels(&with, &without);
    assert!(inside_pixels > 20, "the in-zone effect did not paint");

    // Keep only one outside slot (the three sites of one create) and render
    // again: it must paint nothing.
    let mut outside_only = sim.game.clone();
    for index in 0..64 {
        if index != usize::from(outside_slot) {
            outside_only.effects.release(index);
        }
    }
    assert_eq!(outside_only.effects.active_count(), 1);
    let outside_frame =
        engine::render_game_frame(&pack, id, &sim.room, &mut outside_only, &sim.player).unwrap();
    assert_eq!(
        changed_pixels(&outside_frame, &without),
        0,
        "an effect outside the switch zone must paint nothing"
    );
}
