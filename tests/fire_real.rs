//! Real-asset tests for the NPC fire handler, the stalled scenes and the
//! effect-pool room lifecycle.
//!
//! The dining/lab fire scenes in ROOM1051/ROOM6051 are event cutscenes: the
//! character's state-8 handler 08 plays the scripted animation, spawns the
//! muzzle/shell billboards and raises the system bit the script's `dountil`
//! wait tests. The harness here boots the real room, starts the real event
//! script that contains the firing block and drives the public room tick seam
//! (command VM, event VM, entity driver, effect pool) so the release can be
//! observed end to end. The opening pose waits of the scene are skipped by
//! satisfying their `0x20` bit, because this port does not run state-8
//! handlers on the player slot; the wait under test is the firing `0x21` one.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test fire_real -- --ignored`

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use arklay::game::{BANK_SYSTEM, GameState, ScdGameHost};
use arklay::npc::EntityModelCache;
use arklay::pack::Pack;
use arklay::player;
use arklay::render::Camera;
use arklay::scd::reader;
use arklay::scd::vm::{CommandVm, EventVm};
use arklay::state::{Image, RoomId, RoomState};
use arklay::{effects, engine};

/// The system bit the dining-room scene's `dountil` waits on.
const FIRE_WAIT_BIT: u8 = 0x21;
/// The event-script entry that carries the firing block of ROOM1051/6051.
const FIRE_EVENT: u8 = 14;

/// One booted room plus the script/entity drivers the tick loop needs.
struct FireScene {
    room: RoomState,
    game: GameState,
    command_vm: CommandVm,
    event_vm: EventVm,
    models: EntityModelCache,
}

impl FireScene {
    /// Boot `id` and start `event` in the fresh event VM.
    fn boot(pack: &Pack, id: RoomId, event: u8) -> Self {
        let loaded = engine::simulate_room(pack, id, 0, player::Input::default()).unwrap();
        let data = pack
            .read(&format!("room/{:04x}.rdt", id.rdt_number()))
            .unwrap();
        let scripts = reader::parse(data).unwrap();
        let command_vm = CommandVm::new(&scripts);
        let mut event_vm = EventVm::new(&scripts);
        event_vm.start(0, event);
        let mut game = loaded.game;
        // Satisfy the opening pose wait; see the module comment.
        game.apply_flag(BANK_SYSTEM, 0x20, 0);
        Self {
            room: loaded.room,
            game,
            command_vm,
            event_vm,
            models: EntityModelCache::default(),
        }
    }

    /// The room tick's script/entity/effect order, without rendering or the
    /// player's walk physics.
    fn tick(&mut self, pack: &Pack) {
        {
            let mut host = ScdGameHost::new(&mut self.game);
            self.command_vm.run_main(&mut host);
        }
        let pending = std::mem::take(&mut self.game.pending_event_requests);
        for request in pending {
            self.event_vm.apply(request);
        }
        {
            let mut host = ScdGameHost::new(&mut self.game);
            self.event_vm.step(&mut host);
        }
        if self.game.message.active {
            self.game.cancel_message();
        }
        // The harness has no mixer, so release the voice wait the same way the
        // engine's device-less tick does; a cutscene line must not stall the
        // scene on an F7 that no audio device will ever clear.
        self.game.voice.request = None;
        self.game.voice.stop_requested = false;
        self.game.clear_voice_playing();
        self.game.apply_flag(BANK_SYSTEM, 0x20, 0);
        // The private engine camera pass: a scripted cut lock selects the cut
        // the effect zone test reads.
        if self.game.camera.locked && self.game.camera.current_cut < self.room.cuts.len() {
            self.room.current_cut = self.game.camera.current_cut;
        }
        self.game.tick_entities(&self.room, &mut self.models, pack);
        self.game.tick_effects(&self.room);
    }
}

/// How many pixels two same-shape frames disagree on.
fn changed_pixels(a: &Image, b: &Image) -> usize {
    assert_eq!((a.width, a.height), (b.width, b.height));
    a.rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.rgba.as_chunks::<4>().0)
        .filter(|(left, right)| left != right)
        .count()
}

/// A private capture directory for one test.
fn capture_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("arklay-fire-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run the CLI's `--room ... --ticks ... --capture` path and return the BMP.
fn cli_capture(pack: &Path, room: &str, ticks: u32, out: &Path) -> Vec<u8> {
    let status = Command::new(env!("CARGO_BIN_EXE_arklay"))
        .arg(pack)
        .arg("--room")
        .arg(room)
        .arg("--player")
        .arg("0")
        .arg("--ticks")
        .arg(ticks.to_string())
        .arg("--capture")
        .arg(out)
        .status()
        .expect("spawn arklay");
    assert!(status.success(), "the CLI capture failed");
    std::fs::read(out).expect("read the CLI capture")
}

/// The fire scene's outcome for one room.
#[derive(Debug, Default)]
struct FireOutcome {
    /// The entity reached action behavior 8.
    entered_fire: bool,
    /// The wait bit was cleared the moment behavior 8 appeared, so the
    /// release below can only come from the handler.
    wait_cleared: bool,
    /// The muzzle sheet (type 0x11) was live.
    saw_muzzle: bool,
    /// The handler's state 2 raised the wait bit again.
    handler_raised: bool,
    /// The script left the firing block afterwards.
    advanced: bool,
}

/// Drive the firing event and report the stall release.
fn run_fire_scene(pack: &Pack, name: &str) -> FireOutcome {
    let id = RoomId::parse(name).unwrap();
    let mut scene = FireScene::boot(pack, id, FIRE_EVENT);
    let mut outcome = FireOutcome::default();
    let mut fire_start = None;
    for tick in 0..300 {
        scene.tick(pack);
        let entity = scene.game.entities[1];
        if entity.action_behavior == 8 {
            if !outcome.wait_cleared {
                // The scene may have raised the bit for an earlier step; clear
                // it so the firing completion has to raise it again.
                scene.game.flags[usize::from(BANK_SYSTEM)].apply(FIRE_WAIT_BIT, 1);
                outcome.wait_cleared = true;
            }
            outcome.entered_fire = true;
            if fire_start.is_none() {
                fire_start = Some(tick);
            }
            outcome.saw_muzzle |= scene
                .game
                .effects
                .active()
                .any(|(_, effect)| effect.effect_type == 0x11);
            if entity.action_state == 2
                && scene.game.flags[usize::from(BANK_SYSTEM)].bit(FIRE_WAIT_BIT)
            {
                outcome.handler_raised = true;
            }
        } else if outcome.handler_raised
            && fire_start.is_some_and(|start| tick > start + 5)
            && !outcome.advanced
        {
            // After the handler raises the bit the script's `dountil` exits and
            // re-poses the character out of the firing state.
            outcome.advanced = true;
        }
    }
    outcome
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room1051_fire_handler_releases_the_stalled_scene() {
    let Some((_, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let outcome = run_fire_scene(&pack, "1051");
    assert!(outcome.entered_fire, "the scene reached the fire block");
    assert!(outcome.wait_cleared, "the stale wait bit was cleared");
    assert!(outcome.saw_muzzle, "the muzzle billboard spawned");
    assert!(
        outcome.handler_raised,
        "handler 08 state 2 raised the system bit the script waits on"
    );
    assert!(
        outcome.advanced,
        "the script advanced past the firing dountil"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room6051_fire_handler_releases_the_stalled_scene() {
    let Some((_, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let outcome = run_fire_scene(&pack, "6051");
    assert!(outcome.entered_fire, "the scene reached the fire block");
    assert!(outcome.saw_muzzle, "the muzzle billboard spawned");
    assert!(
        outcome.handler_raised,
        "handler 08 raised the wait bit for ROOM6051"
    );
    assert!(outcome.advanced, "the script advanced past the dountil");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room1051_capture_shows_the_muzzle_pixels() {
    let Some((_, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("1051").unwrap();
    let sim = engine::simulate_room(&pack, id, 0, player::Input::default()).unwrap();
    let room = sim.room.clone();
    let mut game = sim.game.clone();
    // Pose Barry exactly as the firing script does, on the boot camera that
    // frames him.
    {
        let entity = &mut game.entities[1];
        entity.set_state(8);
        entity.action_behavior = 8;
        entity.action_state = 0;
        entity.animation_id = 0x11;
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.scd_anim_param = 0x21;
        entity.flags = 0;
    }
    let mut models = EntityModelCache::default();
    let mut capture = None;
    for _ in 0..8 {
        game.tick_entities(&room, &mut models, &pack);
        game.tick_effects(&room);
        let muzzle = game
            .effects
            .active()
            .any(|(_, effect)| effect.effect_type == 0x11);
        let secondary = game
            .effects
            .active()
            .any(|(_, effect)| effect.effect_type == 9);
        if muzzle && secondary {
            capture = Some(game.clone());
            break;
        }
    }
    let during = capture.expect("the muzzle and the secondary flash were live");
    let mut cleared = during.clone();
    cleared.effects.clear();
    let with = engine::render_game_frame(&pack, id, &room, &during, &sim.player).unwrap();
    let without = engine::render_game_frame(&pack, id, &room, &cleared, &sim.player).unwrap();
    let painted = changed_pixels(&with, &without);
    assert!(
        painted >= 8,
        "the muzzle/shell billboards painted only {painted} pixels"
    );

    // The paint must sit around the character's projected position, not at the
    // world origin: the attach translation is what puts the flash on Barry.
    let camera = Camera::from_cut(&room.cuts[room.current_cut]);
    let center = camera
        .project(during.entities[1].pos)
        .expect("Barry is on screen");
    let mut nearby = 0usize;
    for y in (center[1] - 64).max(0)..(center[1] + 64).min(with.height as i32) {
        for x in (center[0] - 64).max(0)..(center[0] + 64).min(with.width as i32) {
            let offset = ((y as u32 * with.width + x as u32) * 4) as usize;
            if with.rgba[offset..offset + 4] != without.rgba[offset..offset + 4] {
                nearby += 1;
            }
        }
    }
    assert!(
        nearby >= 8 && nearby * 2 >= painted,
        "the fire paint must sit around the character ({nearby} of {painted} pixels in the window)"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room40c1_flamethrower_runs_without_the_wait_flag() {
    let Some((_, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("40C1").unwrap();
    let sim = engine::simulate_room(&pack, id, 0, player::Input::default()).unwrap();
    let room = sim.room;
    let mut game = sim.game;

    // The room's flamethrower character (behavior_flags 5 -> weapon 3) as its
    // own script poses it. Nothing here seeds the wait bit.
    let slot = game
        .entities
        .iter()
        .position(|entity| entity.active() && entity.behavior_flags == 5)
        .expect("ROOM40C1 spawns the flamethrower character");
    {
        let entity = &mut game.entities[slot];
        entity.set_state(8);
        entity.action_behavior = 8;
        entity.action_state = 0;
        entity.animation_id = 0x17;
        entity.animation_frame_id = 0;
        entity.scd_anim_param = 0x21;
    }
    let mut models = EntityModelCache::default();
    let mut sprayed = false;
    for _ in 0..200 {
        game.tick_entities(&room, &mut models, &pack);
        game.tick_effects(&room);
        sprayed |= game
            .effects
            .active()
            .any(|(_, effect)| effect.effect_type == 0x0C);
    }
    assert_eq!(game.entities[slot].action_state, 5, "the flame loop");
    assert!(sprayed, "the flamethrower sprayed without a script flag");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room3091_runs_its_fire_events_without_flag_dependence() {
    let Some((_, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let sim = engine::simulate_room(
        &pack,
        RoomId::parse("3091").unwrap(),
        300,
        player::Input::default(),
    )
    .unwrap();
    assert_eq!(sim.game.frame, 300, "the room ticked without stalling");
    assert_eq!(
        sim.game.npc_placeholders.get(&8),
        None,
        "the fire behaviour is implemented"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_effect_pool_resets_across_a_room_transition() {
    let Some((_, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let first = RoomId::parse("1000").unwrap();
    let second = RoomId::parse("1010").unwrap();
    let mut sim = engine::simulate_room(&pack, first, 0, player::Input::default()).unwrap();
    let room_effects = std::rc::Rc::clone(&sim.game.room_effects);
    effects::create(
        &mut sim.game,
        &room_effects,
        9,
        7,
        0,
        [4420, -2500, 3800],
        0,
        0,
    )
    .unwrap();
    assert!(sim.game.effects.active_count() > 0);

    let next = engine::simulate_room(&pack, second, 0, player::Input::default()).unwrap();
    sim.game.enter_room(second, &next.room);
    assert_eq!(
        sim.game.effects.active_count(),
        0,
        "no slot leaks across a door"
    );
    assert_eq!(
        sim.game.effects.free_slots(),
        effects::EFFECT_POOL_SIZE as u8
    );
    sim.game.tick_effects(&sim.room);
    assert_eq!(sim.game.effects.active_count(), 0);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room1051_ticks_capture_is_deterministic() {
    let Some((_, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = pack_path;
    let dir = capture_dir("determinism");
    let first = cli_capture(&pack, "105", 30, &dir.join("first.bmp"));
    let second = cli_capture(&pack, "105", 30, &dir.join("second.bmp"));
    assert!(!first.is_empty());
    assert_eq!(first, second, "--ticks captures must be byte-identical");
}
