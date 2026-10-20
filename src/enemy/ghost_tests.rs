//! The GHOST cheat against every hostile script: with the player hidden, no
//! script may depend on where the player is. Each enemy boots through every
//! spawn behaviour with the hidden player close by and again far away, and
//! the two trajectories must match frame for frame.

use crate::enemy::LuaEnemyHost;
use crate::game::{Entity, GameState};
use crate::model::{Clip, ClipFrame};
use crate::pack::{Pack, PackWriter};
use crate::state::{RoomId, RoomState};

/// Inside every acquire range the scripts use, and off the default heading so
/// a patrolling enemy does not walk into the spot.
const NEAR: [[i32; 3]; 3] = [[-2500, 0, -600], [-800, 0, 2400], [300, 0, -3500]];
/// Beyond every acquire range, but close enough that no distance sum
/// overflows.
const FAR: [i32; 3] = [-9000, 0, 9000];
const FRAMES: usize = 200;

fn pack() -> Pack {
    let mut writer = PackWriter::new();
    for (path, text) in crate::enemy::ENEMY_SCRIPTS {
        writer.add(path, text.as_bytes().to_vec()).unwrap();
    }
    Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
}

fn clips() -> Vec<Clip> {
    (0..48)
        .map(|_| Clip {
            frames: vec![ClipFrame {
                keyframe: 0,
                timing: 2,
            }],
        })
        .collect()
}

fn game(id: u8, behavior: u8, player: [i32; 3], hidden: bool) -> GameState {
    let mut game = GameState::default();
    game.id = RoomId::parse("1000").unwrap();
    let e = &mut game.entities[1];
    e.id = id;
    e.set_active(true);
    e.status_flags = 1;
    e.behavior_flags = behavior;
    e.saved_pos = Some(e.pos);
    game.entities[0].pos = player;
    game.entities[0].health = 100;
    game.entities[0].hidden_from_enemies = hidden;
    // Non-solid, so a body-to-body push is not mistaken for tracking: the
    // cheat hides the player from the AI, not from collision.
    game.entities[0].status_flags |= 2;
    game.enemy_count = 1;
    game
}

type Observed = ([i32; 3], u16, u8, u8, u8, u8, u8);

fn observe(e: &Entity) -> Observed {
    (
        e.pos,
        e.angle,
        e.state(),
        e.action_state,
        e.action_behavior,
        e.status_flags,
        e.animation_id,
    )
}

struct Rig {
    pack: Pack,
    clips: Vec<Clip>,
    host: LuaEnemyHost,
}

impl Rig {
    fn new() -> Self {
        Self {
            pack: pack(),
            clips: clips(),
            host: LuaEnemyHost::new(),
        }
    }

    fn run(&mut self, id: u8, behavior: u8, player: [i32; 3], hidden: bool) -> Vec<Observed> {
        self.host.reset();
        let room = RoomState::default();
        let mut game = game(id, behavior, player, hidden);
        (0..FRAMES)
            .map(|frame| {
                game.rand_seed = 12345 + frame as u16;
                self.host
                    .update(&mut game, &room, &self.pack, 1, &self.clips);
                observe(&game.entities[1])
            })
            .collect()
    }
}

fn assert_ignores_hidden_player(id: u8) {
    let mut rig = Rig::new();
    let mut reacts = false;
    for behavior in 0u8..=0x20 {
        let far = rig.run(id, behavior, FAR, true);
        let seen_far = rig.run(id, behavior, FAR, false);
        for player in NEAR {
            let near = rig.run(id, behavior, player, true);
            if let Some(frame) = near.iter().zip(&far).position(|(a, b)| a != b) {
                panic!(
                    "id {id:#04x} behaviour {behavior:#04x}, hidden player at {player:?}: \
                     frame {frame} is {:?}, not {:?} as with the player far away",
                    near[frame], far[frame]
                );
            }
            reacts |= rig.run(id, behavior, player, false) != seen_far;
        }
    }
    assert!(
        reacts,
        "id {id:#04x}: a visible player close by changes nothing"
    );
    assert_eq!(
        rig.host.failed_scripts(),
        0,
        "id {id:#04x}: a script failed"
    );
}

macro_rules! ghost_tests {
    ($($name:ident: $id:expr,)*) => {$(
        #[test]
        fn $name() {
            assert_ignores_hidden_player($id);
        }
    )*};
}

ghost_tests! {
    ghost_hides_from_the_zombie_em00: 0x00,
    ghost_hides_from_the_zombie_em01: 0x01,
    ghost_hides_from_the_cerberus: 0x02,
    ghost_hides_from_the_web_spinner: 0x03,
    ghost_hides_from_the_black_tiger: 0x04,
    ghost_hides_from_the_crow: 0x05,
    ghost_hides_from_the_hunter: 0x06,
    ghost_hides_from_the_wasp: 0x07,
    ghost_hides_from_plant_42: 0x08,
    ghost_hides_from_the_chimera: 0x09,
    ghost_hides_from_the_adder: 0x0a,
    ghost_hides_from_neptune: 0x0b,
    ghost_hides_from_the_tyrant: 0x0c,
    ghost_hides_from_yawn: 0x0d,
    ghost_hides_from_the_monster_plant: 0x0f,
    ghost_hides_from_the_zombie_em11: 0x11,
}
