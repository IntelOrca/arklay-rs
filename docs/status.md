# Arklay status

A milestone-by-milestone feature table, the parser/format coverage a fresh
`arklay verify` reports, the verification-spine checklist with what is still
missing, the known `TODO(parity)` sites and their dispositions, and the
completed enemy milestone's closing state. Numbers below were measured on 2026-10-10 with the
workspace install (`/home/ted/openre/assets/re1`) and its converted pack.

## Milestones

| Milestone | Scope | State |
|---|---|---|
| M0 First Light | Convert room 1000, decode cuts, display | done |
| M1 Whole-game conversion + music | All rooms/categories into a pack; BGM playback | done |
| M2 Player | Player model, locomotion, collision, camera cuts | done |
| M3 SCD reading | SCD reader, VM skeletons, `scd export` | done |
| M4 SCD semantics | Full command/event opcode semantics | done |
| M5 Doors, masks, footsteps | `.dor` transitions, room masks, footstep zones | done |
| M6 Game flow and UI | Fonts, messages, inventory, menus, save/load | done |
| M9 Scripted characters | NPC models, animation, escort/walk behaviours (no enemies) | done |
| M10 Effects | Billboards, effect pool, sprite sheets, fire handler | done |
| M11 World interactions | Pushables, mirrors, item-box lid, ladders | done |
| M12 World items | Floor items, sparkles, desks, item-view models | done |
| M13 Voice and audio | Dialogue, per-room BGM state machine, 3D SE dispatch | done |
| M14 FMV | AVI/Cinepak, `movie_on` hand-off, opening/ending flow | done |
| M15 Mods and authoring | Manifest, layered packs, text-SCD assembler, Lua hooks, demo pack | done |
| M16 Fidelity sweep | Exact fixed-point presentation, scripting/UI parity, harness leftovers | done |
| M17 Hardening | Budgets, torture, fuzz, `verify`, atomic CLI, `--stats`, soak, CI, docs | done |
| E1 Enemy scripting | `enemy` namespace, sandboxed enemy-script VM from the pack, monster spawn/model dispatch, SpiderWeb (0x13) | done |
| E2 NPC port | Character states 0/1/8/9 in `enemy/lib/npc.lua` + per-id data scripts; native driver kept as the differential oracle | done |
| E3 Roots + shared helpers | Plant 42's roots (0x0e) spawn-kind/phase machine; named scratch words, joint-scale render, per-entity SCA record, enemy sound bank, additive model tint, direct RNG draw | done |
| E4 Shared damage layer | Combat tables decoded from the install's executable into `data/combat.bin`; the shared weapon-damage pipeline with hit detectors, hit records and reactions; player-hurt helpers; saved-enemy door snapshots | done |
| E5 Adder + Wasp | The adder (0x0a): four-state machine over the four spawn-kind decisions and seven behaviours, the ballistic drop, the joint-1 bite reach, the tail-hiding flinch and both death paths with the retreat's self-respawn. The wasp (0x07): the nest/ignore/behaviour three-level machine, hover drift, contact sting and cutscene grab, the per-character player poses, the big-wasp variant and the nest respawn cycle. Per-frame joint world matrices and joint-anchored billboards, the range/LOS and path-keep helpers they need | done |
| E6 Crow + spiders | The crow (0x05): the 14-behaviour flight machine, perching/take-off scatter, the altitude envelope and off-map death, obstacle swerve/latch steering, the floor-step channel, the stored-speed vector and the player animation/mash accessors. The WebSpinner (0x03) and BlackTiger (0x04): the three spawn-kind pickers and twelve behaviours, the three web shooters, the shared Rust web-clone arena with its own seven-state machine, room teardown and render pass, the two-volume SCA profile, 32-bit joint flags, the leg-reach probe and the per-type web-joint registries | done |
| E7 Cerberus + Chimera | The hound (0x02): the 16-slot state/behaviour block read through two bases, the five sub-tables and the wall-follow switch, all ten behaviours, the forward/turn collision probes with the `position`-word save/restore, the head-track steering and the bite→tumble/maul forks. The chimera (0x09): the four-state table with the hit/death layers, the seventeen behaviours (three doubling as the variant brains), the dive-arc drop/swoop/knockdown/death drivers, the ceiling attachment and the grab with its mash meter | done |
| E8 Zombies | The zombies (ids `0x00` standard, `0x01` naked, `0x11` green): the five overlapping dispatch tables with their exact alias bases, the wander/chase/eating behaviours, the pushback and get-up machines, the eleven-sub-state grab/bite/vomit attack, the four death variants with the corpse hold and the 1-in-4 revival, the severed-limb and head-explosion gore state contract, and the shared `enemy/lib/zombie.lua` driver behind three thin per-id scripts carrying the collision records | done |
| E9 Hunter | The hunter (id `0x06`): the nine-slot state table, the eight-variant AI layer (the plain/pack-follower/pouncer/coward/scripted-walk-in decisions), the twelve AI behaviours, the nine-slot action layer, the seven death paths, the 39-slot script-controlled table with its four self-re-entrant sub-range wrappers, the pack partner slot, the leap/dodge/scream/grab chains and the exact direct-RNG order | done |
| E10a Set-piece smalls | The monster plant (id `0x0f`): the five overlapping tables (state/behaviour/variant/sub/step) with the three step-chain entry bases, the head-joint SCA retarget, the full grab→drain→release chain over the shared player-held window, the bite and strike chains with both vine stop tables, the exact draw order of the die thrash and poison pulse, and the room-shared lunge-side register. The computer arms (ids `0x14`/`0x15`): the terminal command protocol, the 16.16 fixed-point integration, the per-arm reach/step/command chains, the voice handshake and the documented command-8 fall-off. The Forest zombie (`0x16`, absent from this install) stays a documented inert stub. The player-held reaction framework, `e:srand`/`e:lfsr_step`, `e:play_voice`/`e:play_sfx`, `e:set_sca_hit_point`, `e:raise_flag` and the boss scratch views land with it; Neptune, Yawn and the Tyrant stay pending | done |
| E10b Plant 42 | Plant 42 (id `0x08`): the state/action machine (init, state check, damaged, die, the SCD table), the sixteen action handlers with the sweep/spit/grab-hold-release-kill/wither chains, the range-gate selector, the ambient room effects and the shared vine pool with the last-vine award. The custom skeleton animator (`e:plant42_advance`: sub-frame blend, blend-before-use, hold-gated frame advance), the companion arena (the flower body and root ball clones with their own machines, render objects and scale, the room-shared body every split-vine record reads) and the room-40C0 set-ups (fallen core, poison arena, Chris hold, boss fight, poison body) land with it. The state-6 attacked-flag window (knockdown / grabbed / thrown, the capture-matrix slide) and the state-2 generic hit reaction join the player-held framework; `e:plant42_capture_setup`/`e:plant42_hold_player`, `e:apply_matrix_lv`, `e:vector_normal`, `e:set_player_pos`/`e:set_player_speed`/`e:add_player_speed` and the `p42_*` scratch views are the new surface. Neptune, Yawn and the two Tyrants remain for the later E10 groups | done |
| E10c Yawn | Yawn (ids `0x0d` first fight, `0x12` rematch): the five-state head machine (init, state check, damaged, die, wait), the nine actions (idle, move, bite with the per-form damage window and the first-fight poison, rear/form toggle, the swallow grab with its capture matrix and player window, the scripted entry crawl, the emerge/hiss ceiling burst, the flee path with its three segment batches, the wall-stuck reposition), the two decision trees and the exact direct-RNG order. The head's own fixed-point chain animator (pose init, `<<9` chain accumulation, the joint-13 anchor, the `reverse` swallow playback, chain follow/align and the post-move collision drag), the thirteen-slot segment machinery (`e:yawn_spawn_segments`/`e:yawn_segment_tick` with cross-slot damage routing and cleanup), the cross-slot scalar accessors, the swallow capture/player-held state-7 window and the segment render pass (mesh skip, body shadows, `& 0x7f` gate) land with it; `em12.lua` is the thin alias. Neptune and the two Tyrants remain for the later E10 groups | done |
| E10d Tyrant | The Tyrant (ids `0x0c` lab, `0x10` heliport; one shared `enemy/em0c.lua` machine plus the thin `em10.lua` alias): the ten-slot state table (init, think+act, the hit reaction's `+0x174` state restore, the forced state and the SCD state), the two behaviour dispatch bases (state 1 adds the +2 bias, state 3 runs it unbent with no pathfinding), the sixteen behaviours (restrained slab, SCD yield, 60-frame pause, waypoint walk with the walk/run root-motion sets, swipe/slash/thrust/backhand with their reach windows, damage frames and kill latches, the impale grab, charge and rush with their save/restore collision probes, the eruption entrance, rise, and the two per-id decision layers) and the eighteen-slot SCD table (pod → glass break with its 53-entry shard draw order → the scripted Wesker impale → idle, the walk/strike/face/animation handlers, and the heliport rocket death that launches five joints as free bodies and stops at `health = -1` / `SysFlags 0x1F`). The root-motion extractor (`e:tyrant_root_motion`) turns the claw chains into the frame's speed and slides the entity; the ribbon history (`e:tyrant_trail_alloc`/`e:tyrant_trail_arm`/`e:tyrant_trail_update`, the nine matrix/midpoint/near-far slots with the `0x8000` arm bit), the two claw ghosts (`e:tyrant_ghosts_tick`, the signed-byte 3000..6000 scale pair) and the limb physics (`e:tyrant_limb_launch`/`e:tyrant_limb_update`) keep the original's exact state; the exposed heart rides joint 1 through the companion arena with its own beat ramp and rocket-death drop. The player-side Tyrant stagger table (backhand/swipe stagger and the knock-back with the wall slam and reverse get-up) runs in the shared state-6 window; `e:latch_player_attacker`, `e:snap_grab_slot`, `e:add_entity_anim_offset`, the `ty_*` scratch views, `e:angle_quadrant`, `e:store_camera_speed` and the travelled-distance third return of `e:resolve_collision` are the new surface. The corpus census now finds twenty-one scripted monster ids (adding `0x0c` and `0x10`) with no script errors, and the real-asset tests boot ROOM5130's pod Tyrant with its heart and ROOM3030's suspended heliport Tyrant | done |
| E11a Player weapons | The player weapon runtime: the aim/fire/reload state machine (0x12–0x1a guns, the knife's own aim/hold/swing/holster/turn) driven by the new aim/fire pad keys, the per-weapon `W*.EMW` clip selection and the knife/gun/projectile fire paths through `apply_weapon_damage_with`, the clip/reserve ammo model (`weapon_autoaim_check`, the reload transfer and the volley latch), the fire data tables (fire frames, sounds, muzzle and secondary flash offsets, end frames, special frame windows, aim heights), the held weapon drawn at joint 14 with the player/character texture page, the muzzle/secondary flashes and shells attached to the weapon joints, the state-8 held-weapon joint refresh, and the flamethrower enemy-bank cues | done |
| E11b Player death | The player damage/death state machine: the state-1 entry checks (the HP<0 death trigger with the back-shot facing flip and the cannot-die guard, the hit pre-emption into the generic reaction, the poison status drain), the death fall (state 3: the scream, the body clip, the slide, the blood pool that reuses the ground quad and grows 16 a frame for 148 frames, and the blocked state), the game-over machine (the 90-frame delay, the room-variant immediate fades for the two Yawn rooms and the water tank, the scripted-death rooms that skip the pool, the white-out and the black fade-out), and the DIED screen (`src/ui/death.rs`: the `died.tim` mirrored static backdrop, the fixed camera over the corpse with the 8-unit-per-frame spin, the 256-column sine-wavy strip with its exact phase/amplitude ramps, the black overlay ramp and the confirm skip), returning to the title; `ui/died.tim` is packed and `--ui death` joins the debug boot list | done |
| E12 Enemy verification | The closing gate over the whole arc: the corpus spawn census re-measured across all 348 rooms with twenty-one scripted monster ids and zero script errors, the per-id real-asset boots (`enemy_real`), the real weapon/damage kills (`weapons_real`/`combat_real`), the shipped DIED-screen run (`death_real`), the NPC/actor/walk/entities/fire regression suites, `verify` on the reconverted pack (the new `combat`/`emw`/`tmd` rows green, `tim` 122/122), and the status/deviations close-out | done |

Enemy behaviour now lives in the pack as `enemy/em{id:02x}.lua` scripts (with
shared `enemy/lib/**.lua` modules) behind the same sandbox as the mod hooks;
the scripts are stateless and all their mutable state sits on the Rust entity.
The first ported monster is the spider web (id `0x13`), which validates spawn,
model, script load/reset, damage reaction and the death flag end to end. The
scripted characters (`0x20..=0x2E`) now run their whole driver from
`enemy/lib/npc.lua` with per-id data scripts: the state-0 init, state-1 idle
behaviours, state-8 scripted-action handlers (weapon fire, scripted turns and
walks) and the state-9 follow/pathfind driver, each checked against the native
oracle field for field, tick for tick. The native driver stays as the fallback
and differential oracle. The second monster is Plant 42's root mass (id
`0x0e`), whose `enemy/em0e.lua` transcribes the dormant, retracted and full
writhe spawn kinds, the four phase handlers that double as state-table slots,
the exact groan-cue timer draw order and the shrink/tint/shadow cadence; E3
also paid the shared helper backlog it needed: named Rust accessors for the
scratch words that sit over adjacent bytes and the stored-position dword, the
`pad_ca` joint world-matrix scale wired into the renderer, a per-entity SCA
record consumed by the shared separation pass, the per-room enemy sound bank
behind `e:play_enemy_sound`/`e:play_3d_sound`, the additive model tint with
its mesh hook, and `e:random()` for the monsters that consume the platform
stream directly. The roots' damage reactions (the state-2 reset and the
inert states 3-5) are transcribed; the combat layer that enters them landed
with E4. Every monster id the corpus spawns is now scripted — the full
roster, the player weapons and the death/game-over flow are summarised in the
milestone table and the per-milestone notes below. The attract demo, the
endings, the DC-only Forest zombie and the documented render-contract items
remain deferred; every one is listed below or in the milestone deviations
files.

E4 added the shared damage/death layer those scripts consume. The converter
now decodes the install's own combat data — the per-weapon hit ranges, both
playthroughs' hit records, the hit-joint lists, the per-type hit reactions,
the reference type's init profile and collision records, the shared player
collision records and the entity-model path table — validates every region's
shape and pointer partitions, and packs it as `data/combat.bin`; a pack
without the entry falls back to the built-in documented tables.
`GameState::apply_weapon_damage` runs the original's per-shot pipeline in
slot order (knife/gun/projectile detectors, the two arcade cones, the
line-of-sight layers below weapon slot 5, first/second-playthrough record
selection, the 16-bit health wrap, the `hit_state` composition, the per-weapon
post-hit callback and per-type reaction FX) and writes the `state = 3` /
surviving `state = 2` contract with the action bytes zeroed; a killing blow
raises the spawn record's death flag. The player-side helpers
(`apply_player_hurt` with the clamp/lethal rules, the poison status/timer, the
grab pose) are exposed as `e:hurt_player`, `e:poison_player`,
`e:grab_player`, the player read/write properties and the animation override
methods. Door transitions now snapshot the outgoing room's live enemies into
the 16-slot TTL table and restore a matching snapshot in `spawn_enemy` when
the record lacks the force byte, so enemies left mid-fight persist across a
door and death events stay permanent. The declared joint-attached gore/tints
and the knife-hand joint origin are documented approximations until the
joint-matrix layer lands (see the module docs in `src/combat.rs`).

E5 ports the adder (id `0x0a`) as `enemy/em0a.lua`. The script transcribes the
four-entry state table, the four spawn-kind decisions (floor, ceiling-near,
ceiling ambush with its pose/fuse trigger and 100-unit teleport, hidden) and
the seven behaviours (chase with its per-frame wobble draws, the quadratic
ceiling drop, turn away, the joint-1 bite with its frame-0xB hit window,
victory, idle, emerge), plus the flinch and the two death paths: the knife
writhe that ends in the blood pool and the death flag, and the firearm retreat
that sprays blood, hides every joint, parks out of sight and resets to state 0
with a fresh health roll and a ceiling spawn kind (kind 1 in the water-gate
room, 2 elsewhere). The shared helpers it needed landed with it: per-frame
joint world matrices composed from the posed EMD skeleton with the full
rotation triple and the `pad_ca` scale, exposed as `e:joint_world_x/y/z` and
the square `e:reach_test`; joint-anchored billboards through a new
`Attach::Joint` effect target; the flat visual/alert range checks and the
facing test; the path-result keep byte; the room-collision result code behind
`e:resolve_collision`; the player-pair touch flag from `e:separate`; and the
cross-enemy special-weapon check.

E5 also ports the wasp (id `0x07`) as `enemy/em07.lua`, the last of the four
simplest monsters. The script transcribes the four-entry state table and the
three-level control (the `ignore` drift gate, the nest state machine encoded in
`behavior_flags`, and the seven-entry behaviour table with `action_state`
sub-states): the dormant nest's dwell roll and proximity arm, the 24-frame
takeoff spiral, the hover cruise (the one-in-seven wing blink and wingbeat cue,
the 0x23..0x96 bob ramp, two randomised altitude flips a frame and the
touch/range/aim/altitude attack gate), the contact sting with its
per-playthrough damage and the big wasp's scripted kill, the victory circle,
the pin-and-sting grab (per-character clip and hit frame, the poison coin, the
self-killing payout) and the emerge. Death forks on the weapon in `hit_state`:
a big or heavy-weapon kill is removed at once, anything else leaves a corpse
that crawls 20 frames and respawns at the fixed nest coordinates, every tenth
respawn as the big variant. The helpers it added: the wasp's typed scratch
views (`e.wasp_drift`/`touch`/`climb`/`timer`/`bob`/`distance`/`collision`/
`speed`/`dwell`/`anim_done`/`big`/`respawns`/`sound_latch`), the +0x16E
path-result keep behind `e:wasp_path_keep`/`e.wasp_path_bit`,
`e:apply_anim_vertex` for the grab's root-motion snap, `e:player_facing_entity`
and the player-side `e.player_move_speed`/`e.second_playthrough` reads. The
corpus census now finds four scripted monster ids (`0x07`, `0x0a`, `0x0e`,
`0x13`) and no script errors, and the real-asset tests boot the seven ceiling
adders of ROOM3010 and the seven dormant wasp nests of ROOM4080 through init.

E6 ports the crow (id `0x05`) as `enemy/em05.lua`. The script transcribes the
nine-entry state table (init, driver, recoil, death and the five SCD-flight
slots), the four anim helpers (hold, step, banking turn, grab) and all
fourteen flight behaviours: the perched countdown and its three idle clips,
the wingbeat's climb/sink arc, the dive and its rolled ceiling bound, the
random-rate banking turn, the strike run and dive attack with their
distance-scaled climb velocities, the descending glide and climb-out (both
steered by the pathfinder bit and handing to the peck at head height), the
approach, the peck that bites through the shared player helpers and latches
on, the level-out, the grab with its signed mash-scaled struggle counter and
180-degree release, the victory descent and the staggered take-off. Death
forks on the room resolve's floor step: real floor fades the shadow yellow
and plays the landing, an off-map step bursts every joint and removes the
entity. The helpers the crow needed landed with it: the stored `Add_speedXZ`
velocity with `e:advance_speed`, the `entity_swerve_around_obstacle`
steering behind `e:swerve` with its quadrant fix-up and latch countdown, the
floor/step branch of the room collision (returned as the second value of
`e:resolve_collision`), the crow's typed scratch views and its own
swerve/latch/struggle/alt-bias/floor-step fields, and the player's animation
id/frame reads plus `e:player_mashing()` for the grab shake. The corpus
census now finds five scripted monster ids (`0x05`, `0x07`, `0x0a`, `0x0e`,
`0x13`) and no script errors.

E6 also ports the WebSpinner (id `0x03`) as `enemy/em03.lua` and the Black
Tiger (id `0x04`) as `enemy/em04.lua`, closing the milestone. The WebSpinner
transcribes the ten-slot state table (six states, the last four aliasing the
three spawn-kind behaviour pickers), the twelve behaviours (the two idle
speeds, three walks, the strafe, two approaches, the lunge with its direct
damage and facing reaction, the spit, the ceiling web drop, the no-op and the
circle) and both web shooters; the Black Tiger the four-slot state table, its
single picker with the post-attack cooldown and chase counter, the seven
behaviours (idle, walk, strafe, approach, circle, the two-fang bite and the
three-glob spit) and its three shooters. Both scripts drive the shared
web-clone subsystem: a Rust arena of full entity copies outside the enemy
list, spawned with a quarter-turn fan per thread, walked by their own
seven-state machine (swing, ballistic flight, sticky contact, dangle), draining
the player's health on contact, resolved against the room with the spider's
inherited SCA radius and reset with the room. The clone render pass draws the
live threads after the player with the frozen spawn pose, and spent threads
keep their web-coloured ground shadow. The original draws each thread as a 2D
sprite of the spider's joint-bound web object; the port submits the spider's
whole mesh at the clone's transform instead (the thread art cannot be
isolated from the model), so a thread reads as a small spider rather than a
strand until that object split lands. The helpers the spiders needed landed
with them: 32-bit joint visibility with `e:set_joint_visible` and the
minimal `e:arm_joint_effect`, the second SCA volume behind `e:set_sca2` and
the nested volume-pair separation, the `e:leg_reach` stride probe (both chain
shapes) over the same-call posed skeleton, the per-type web-joint registries
in the game state, the clone chain head on the entity, the ballistic step, and
the spiders' typed scratch views. The corpus census now finds seven scripted
monster ids (`0x03`, `0x04`, `0x05`, `0x07`, `0x0a`, `0x0e`, `0x13`) and no
script errors, and the real-asset tests boot ROOM30C0's Black Tiger and
ROOM4040's two WebSpinners through their scripts.

E8 ports the zombies (ids `0x00`, `0x01`, `0x11`) as `enemy/lib/zombie.lua`
with three thin per-id scripts (`em00.lua`, `em01.lua`, `em11.lua`) that carry
only the variant's collision record. The shared driver transcribes the five
overlapping dispatch tables with their exact alias bases — the 22-slot states
block read through the behaviour base `states + 10`, the 28-slot
damage/action/move block read through `+0`, `+8` and `+16`, and the separate
14-slot SCD action table — so the aliasing is the original's one array, not
copies with matching values. It carries the full machine: the two spawn rolls
(health `17..99` from the table and the frame-seed threshold/stagger budgets),
the dead/floor/vomit spawn kinds, the wander/chase/eating behaviours with the
weave draws and the wander-turn stuck latch, the six damage reactions (the
short push back with its one-in-eight arm severing, the two staggers, the
knock-down, the leg explosion that crawls away or dies, and the long shove),
the eight action behaviours (idle head turns, slow walk, chase walk, fast
facing, bend-down eating, the falling lunge, vomit, falldown), the pushed-back
idle/stagger pair with the leg-chain crawl speed, the eleven-sub-state attack
(the grab anchor, the 105-frame bite loop with its mash shortening and
per-playthrough damage, the release's withdraw/head-bite/vomit forks, the
head explosion and headless death), the four death variants (plain fall with
the one-in-four revival, headshot, prone lay-down, magnum pushback), the
corpse hold and parking, and the scripted roar/headshot/dying/vomit handlers.
The gore state contract is complete: the joint flag bytes (active bit, the
`0x28` arm/explosion, the `0x0C` severed arm, the `0x88` vomiting head), the
`0xCC`/`0x40` head tests, the blood scratch the vomit arms and the
`blood_splatter_physics` fall with its wall/floor billboards and impact cue
all run; the per-joint colour tints and the async joint-object chunks stay
with the renderer's documented deferral, and the Director's Cut-only branches
(third SCA record, double-step nibbles, behaviour 11) stay dormant with no DC
mode in the port. The helpers the zombies needed landed with them: the state
dword/mirror aliases and the typed `+0x170`..`+0x188` scratch views, the
`checkAngularViewAndDistance` wedge and `check_line_of_sight` ray, the
`entity_update_wander_turn` out-parameter steering, the entity
`check_room_collision_two_point` prone probe with its per-end bits and
rollback, the radius-zero boundary point query, the per-joint blood scratch
and `zombie_body_part_physics` leg stride, the player-state/attack-word
accessors and the mash reduction. The corpus census now finds ten scripted
monster ids (`0x00`, `0x01`, `0x03`, `0x04`, `0x05`, `0x07`, `0x0a`, `0x0e`,
`0x11`, `0x13`) and no script errors, and the real-asset tests boot the 1F
dining room's zombies through their scripts.

E9 ports the hunter (id `0x06`) as `enemy/em06.lua`, the last of the common
roster monsters. The script transcribes the three stacked dispatch layers: the
nine-slot state table (spawn, AI driver, action layer, death, a bare return and
the script-controlled state the spawn's control bit forces), the twelve-entry
AI behaviour table, the nine-entry action table and the seven-entry death
table. The AI layer carries the eight-variant decision block (the plain
hunter's one-frame kind rewrite, the pack follower's scripted formation
mirror, the health-gated pouncer with its low/mid pounce-chance tables and the
wounded-player gate, the walk-in variants that drop their kind bit, and the
coward that clears its own bit), the shared leap roll (the plain flavour reads
the frame seed, the pouncer flavour draws the stream), the path-latch/re-pause
driver and all twelve behaviours: the variant-selected stand/walk idle, the
chase with its wander steering and footfall cues, the 90-frame approach, the
nop, the swing, the aimed pounce, the dodge, the four-sub-state hold with its
head-joint tracker, the formation jump-down, the scream with the one-per-room
howl latch, the sidestep with its signed direction byte, and the scripted
walk-in with its stage/room/camera row selection, per-frame shuffle, mid-walk
howl and entry-yaw snap. The action layer seeds from the hit-state roll (with
the pending-grab cancel and the status/flag overrides), runs the swipe with
its joint-9 reach window and per-playthrough damage, the lunge, the
player-dragging recovery and the chain re-roll; the leap attack with its
ballistic arc, randomised hover, flurry re-roll, dive drag and leap-flag
pounce hand-over; the pounce chain with its three charge rates and swipe
re-arm; the claw flurry with its per-frame arm billboard; and the
player-holding loop. The death layer has the shared fall driver (the health
pin, the corpse-quad tint and shrink, the death event and the script release),
the standard fall with the howl and second cry, the thrash, the settle, the
collapse and the mid-pounce crash. The script-controlled table is spelled once
with all 39 slots, including the four wrappers that re-enter the same array at
the dodge/pounce/bite/swipe sub-range bases indexed by `action_state`, plus
the walk/run waypoint handlers, the scripted death with its target tracking,
the stalk window and the scripted bite that latches the enemy-list base
entity. The pack partner slot is picked at init (the enemy-list head, or the
next slot when the hunter is the head) and read through named partner
accessors. The helpers the hunter needed landed with it: the named `+0xC2`
through `+0x18A` scratch views and the partner slot, the hunter-flavoured
wander-turn steering over its own control byte and path-latch counter, the
partner and cross-slot joint/state accessors, the mouth-joint tracking and
body-recenter helpers over the posed skeleton, the player drag and the grab
one-shot, the shared `player_distance_z` scratch the line-of-sight helper
writes into mid-frame, the per-room howl latch, the intro row and the
outgoing-room camera id the same-stage door load stores. The corpus census now
finds thirteen scripted monster ids (`0x00`..`0x07`, `0x09`, `0x0a`, `0x0e`,
`0x11`, `0x13`) and no script errors, and the real-asset tests boot ROOM30A0's
scripted hunter pair through their scripts.

E10a opens the boss roster with the two smallest set pieces. The monster plant
(id `0x0f`) is `enemy/em0f.lua`: the five overlapping tables are spelled once
(the five-entry state table, the seven-entry behaviour table indexed by the
`ignore` byte, the ten-entry variant table, the four-entry per-variant
approach table with its genuine NULL slot guarded, and the twelve-case step
chain entered at three bases), the grab/bite/strike chains run the drain over
the new player-held window (the plant writes player state 6 with window index
`0x0F`, the Rust window advances the grab clip and the drain flags the release
that spins a back grab and returns control), the vine reveal/hide tables walk
the fifteen segment bits exactly (including the spent cursor wrapping the
timer), and the die thrash's discard-first four-draw burst and the poison
pulse's two-timer tint ramp keep the original's draw order. The computer arms
(ids `0x14`/`0x15`) are `enemy/em14.lua`/`em15.lua`: the terminal's
`behavior_flags` command protocol (parked, new-command and done bits), the
16.16 fixed-point integration with truncating division and arithmetic shifts,
the per-arm reach offsets and step chains, the right arm's voice handshake
(queued through the new `e:play_voice` and polled through `e.voice_playing`),
the left arm's success gesture, lever pull and hanging drop, and the
documented command-8 fall-off that re-runs reach A forever on the right arm.
The Forest zombie (`0x16`) stays a documented stub: this install ships no
`0x16` spawn records and no model, so a modded spawn parks inert with the
missing-model warning rather than a fabricated script. The shared pieces the
group landed are the player-held window in `player_script.rs`, the per-frame
`e:set_sca_hit_point` SCA retarget (the separation pass now adds a world-space
bias to the primary volume), the `e:srand`/`e:lfsr_step` randomness specials,
`e:raise_flag`, `e:play_sfx`, `e:play_voice`/`e.voice_playing`,
`GameState.monster_plant_sides` and the typed `mp_*`/`arm_*` scratch views
over the shared bytes. The corpus census now finds sixteen scripted monster
ids (adding `0x0f`, `0x14` and `0x15`) with no script errors, and the
real-asset tests boot ROOM10C0's six-vine bed and ROOM5060's parked arm pair
through their scripts. Plant 42, Neptune, Yawn and the two Tyrants remain for
the later E10 groups.

E10b is Plant 42 (id `0x08`), `enemy/em08.lua` over three new Rust pieces. The
script spells the state dispatch (init / state check / damaged / die / the
separate SCD table), the sixteen action handlers, the two range-gate selectors
and the ambient effects exactly as the original, and drives the companion
clones and the player-hold window through named methods. The custom skeleton
animator (`enemy/custom_anim.rs`) is the plant's own clock: it always poses,
sub-frame blends `(0x1000 / blendStep) / (timing_control + 1)` once the blend
counter has run out, consumes the counter before use and advances the frame
only when the hold expires; it also carries the per-axis `ScaleMatrixCols` and
the fixed-orientation matrix derivation. The companion arena
(`enemy/companion.rs`) holds the flower body and root ball as full entity
clones outside the enemy list: `e:plant42_spawn` builds them (consuming the two
clone draws in order, including the poison-body tint and the room-40C0 parked
pose), `e:plant42_tick` runs their machines with the original's draw order and
the body's `+0x70` vine pool is the boss's real health pool, and the render
pass draws each clone's own mesh object (16 the flower, 17 the root ball) at
its transform and scale. The room-40C0 set-ups (fallen core, poison arena,
Chris hold with the fixed 3x3 matrix, boss fight, poison body) select through
`behavior_flags` at init. On the player side the state-6 attacked-flag window
(knockdown / grabbed / thrown, with the thrown slide reading the shared capture
matrix's `t[0]`) and the state-2 generic hit reaction the acid spit triggers
now run in `player_script.rs`, so a spit, sweep, grab or drop can no longer
freeze the player; `e:plant42_capture_setup`/`e:plant42_hold_player`,
`e:apply_matrix_lv`, `e:vector_normal`, `e:set_player_pos`,
`e:set_player_speed`/`e:add_player_speed` and the `p42_*` scratch views are the
new Lua surface. The corpus census now finds seventeen scripted monster ids
(adding `0x08`) with no script errors, and the real-asset tests boot ROOM40C0's
boss, split vines and companions through their scripts. Neptune, Yawn and the
two Tyrants remain for the later E10 groups.

E10c is Yawn (ids `0x0d`/`0x12`), `enemy/em0d.lua` plus the thin `em12.lua`
alias over two new Rust pieces. The script spells the five-state head machine
(init, state check, damaged, die, the `0x80` park), the nine actions — idle,
move, the bite with its per-form damage window and the first fight's poison,
the rear-up form toggle, the swallow grab, the scripted entry crawl, the
emerge/hiss with the ten-puff ceiling burst, the flee path with its three
segment batches, and the wall-stuck reposition — the two decision trees
(`behavior_flags & 7`: free-roaming vs scripted) and the selector's whole-word
writes exactly as the original. The head's own fixed-point chain animator
(`enemy/custom_anim.rs`, the `YawnPose`) poses joints 0-2 from the animation,
stacks yaw/roll onto the previous joint for 3-14, accumulates the chain in
`<<9` space, anchors joint 13 back on its own previous world position, adds
the leftover XZ delta to the entity and rebuilds joints 0/2 in the post-move
pass;
the render recomposes the first three joints from their transforms the way the
original's render walk does. The thirteen-slot segment machinery
(`enemy/yawn.rs`) builds the twelve body slots from the head's record
(`e:yawn_spawn_segments`, linked to the joints they mirror and counted into
`game.enemy_count`), runs their whole per-frame branch in Rust
(`e:yawn_segment_tick`: mirror, visual range, ceiling bit, head damage routing
with the heavy/light split, room collision with the chain drag and the joint-3
carry, the switch-zone tail and the dead-head park), writes the head's batched
segment statuses (`e:yawn_set_status_range`) and recolours the body shadows at
the dissolve's phase 8. The render pass skips the segment meshes (the original
returns early for the sub-type-1 clones) while keeping their ground quads, and
the shadow gate now honours the `& 0x7f` switch-zone mask the death park sets.
The swallow's player side is the state-7 window in `player_script.rs` (the
carried animation, then the head-joint capture hold every frame, releasing on
the head's death), and the cross-slot scalar accessors (`e:entity_status`,
`e:entity_health`, `e:entity_pos`, `e:entity_angle` and the setters) plus the
`yawn_*` scratch views are the new Lua surface. The corpus census now finds
nineteen scripted monster ids (adding `0x0d` and `0x12`) with no script errors,
and the real-asset tests boot ROOM210's attic head with its twelve segments and
ROOM70C's rematch through their scripts. Neptune and the two Tyrants remain for
the later E10 groups.

E10d closes the boss roster with the Tyrant (ids `0x0c` lab and `0x10`
heliport), `enemy/em0c.lua` plus the thin `em10.lua` alias over four Rust
additions. The script spells the ten-slot state table (init, think+act, the
hit reaction that restores the `+0x174` state backup, the no-pathfind forced
state and the SCD state), the two dispatch bases that make one behaviour byte
mean different things in state 1 and state 3, the sixteen behaviours and the
eighteen-slot SCD table exactly as the original: the lab's pod floats between
`y = -200` and `-350` until `SysFlags 0x1F` lifts it, the capsule break fires
its 53-shard burst (four draws per shard, in table order) with Wesker's line
and voice bit, the scripted impale writes the victim slot's status/state word
and drags it with the root motion, and the heliport rocket death seeds
`srand(1534)`, launches joints 2/4/7/10/12 as free bodies and smoulders to
`health = -1` and `SysFlags 0x1F` where the milestone stops (the ending films
remain the ending milestone's). `enemy/custom_anim.rs` gains the Tyrant's
root-motion extractor: it composes the entity yaw with the walk (`set 0`) or
run (`set 1`) claw chain, subtracts the chain end's previous-frame world
translation, stores the planar speed and optionally slides the entity, which
is what makes the walk, thrust and impale read as root motion. The new
`enemy/tyrant.rs` holds the three render-only machines: the nine-slot ribbon
history (matrix pairs, element-wise midpoints, near/far blades, the store and
scroll branches and the `0x8000` arm sweep), the two claw ghosts with their
signed-byte 3000..6000 scale pair, and the five severed limbs' free-body
physics (X/Y/Z velocity, gravity, bounce budget and tumble rotation). The
ribbon's and ghosts' state is exact and unit-tested; their rasterisation and
the per-joint static tints they and the claw hits apply remain the documented
approximations (the claw ghosts draw as two claw mesh objects, the ribbon's
quads are computed but not rasterised). The exposed heart is a companion-arena
clone riding joint 1 with its own 22-entry beat ramp, wobble scale and
rocket-death drop; the render pass draws ghost, limb and heart meshes from
their scratch matrices. On the player side the Tyrant's own state-6 table now
runs in `player_script.rs`: the backhand/swipe staggers (with the claw and
player blood sheets and the decaying slide) and the full knock-back, whose
room probe aborts the slide into the wall slam and whose reverse get-up hands
the player back. `e:tyrant_root_motion`, `e:tyrant_heart_spawn`/
`e:tyrant_heart_tick`/`e:tyrant_heart_drop`, `e:tyrant_ghosts_tick`,
`e:tyrant_trail_alloc`/`e:tyrant_trail_arm`/`e:tyrant_trail_update`,
`e:tyrant_wander_turn`, `e:tyrant_limb_launch`/`e:tyrant_limb_update`/
`e:tyrant_limb_pos`, `e:latch_player_attacker`, `e:set_player_attacker`,
`e:snap_grab_slot`, `e:add_entity_anim_offset`, `e:angle_quadrant`,
`e:store_camera_speed` and the `ty_*` scratch views are the new Lua surface;
`e:resolve_collision` now also returns the travelled distance the walk
steering reads. Documented deviations: the look-at mode targets the player's
position rather than its joint-1 world, the impale drags the held player by
the same per-frame velocity instead of through the player's own root vertex,
`TexturePage_DeleteSet`/`StMask` are inert (no page-gardening consumer), the
room-action probe is a no-op (only observable in the ending sequence), and
the shared ghost-scale statics reset with the room instead of persisting
across encounters. The corpus census now finds twenty-one scripted monster ids
(adding `0x0c` and `0x10`) with no script errors, and the real-asset tests
boot ROOM5130's pod Tyrant with its heart companion and ROOM3030's suspended
heliport Tyrant through their scripts. With E10d the E10a–d boss roster is
complete; Neptune (id `0x0b`), which the E10 grouping left unassigned, and the
Forest zombie stub remain documented follow-ups.

E11a lands the player weapon runtime. The converter now packs every shipped
weapon asset — the 30 `W*.EMW` animations the locomotion pair did not already
carry, as `player/w{id:02x}.emw`, and the seven `WS*.TMD` held-weapon meshes
as `player/ws{id:03x}.tmd` — and `arklay verify` reports the new `emw`/`tmd`
rows green. `src/weapons.rs` transcribes the original's aim/fire machine: the
entry from the aim input, the raise/hold poses, the reticle scan and lock-on
turn, the auto-aim fire with its ammo frame, damage frame, big muzzle flash,
second flash and shotgun double-fire, the quick-fire and reversed recover, the
holster, the empty-clip click and the reload motion with its per-weapon FX
routines, plus the knife's aim/hold/swing/holster/turn. The fire tables (fire
frames, sound ids, billboard offsets and depths, end frames, special-weapon
frame windows, aim heights) select the packed clips and time the FX; the
damage rides the existing pipeline through `apply_weapon_damage_with`. The
ammo model matches the original: the equipped weapon's own quantity byte is
the clip, the matching ammunition item (`weapon + 9`) is the reserve, the
reload moves rounds from the largest stack, and the special weapons and the
flagged rocket launcher refill their clips. The held weapon draws at joint 14
with the player's texture page (the `WS*.TMD` meshes with their character's),
the muzzle/secondary flashes attach to the same joint, and the state-8 tail
now stores the posed joint worlds so the character fire handler's FX ride the
weapon hand and the two flamethrower cues queue through the room's enemy bank.
The player-side inputs gain port-only aim (`C`) and fire (`Z`) bindings, and
the engine's tick routes the unfrozen player through `weapons::update` with
the locomotion machine as the fall-through. Documented deviations: the
detached beretta magazine mesh, the joint-anchored body correction
(`EntityUpdateWeaponJoint`, subsumed by the port's per-frame pose composition)
and the projectile effect behaviours' own damage (the flamethrower, acid,
flame and rocket rounds) remain as before. The synthetic suite covers the
weapon matrix (each class fires and damages, ammo decrements, the empty clip
clicks, the reload refills), the aim/holster locomotion hand-off, the
hand-joint FX attach and the flamethrower cues; the real-asset
`tests/weapons_real.rs` parses every packed weapon asset, kills ROOM30C0's web
through the weapon input path, and captures the raised weapon in a shipped
room.

E11b closes the death flow. The state-1 entry now runs the original's three
pre-control checks: a player whose health went negative flips the facing when
the back-shot sentinel is set, enters the death state and is held at one
health inside an effect zone (the cannot-die guard); a raised attacked flag
pre-empts control into the generic hit reaction; and the poison status drains
two health on its shared timer, with only the fast variant allowed to kill.
The collision callback byte now mirrors the live health-status byte before the
player's physics, so a script that clears the `0x10` bit really stops skipping
the shape-5 floor volumes. The death fall (player state 3) screams, plays the
body clip, slides the corpse, re-colours and shrinks the ground quad into the
blood pool and grows it 16 units a frame for 148 frames before blocking input;
the three scripted-death rooms (the armor room, the drug storehouse and the
morgue) skip the pool. The game-over machine waits the original's 90 frames,
fades the screen to white over 128 frames and opens the DIED screen; the two
Yawn rooms and the Neptune water tank fade immediately instead, and the
attract/death-variant flags skip the screen. `src/ui/death.rs` transcribes the
screen: the `died.tim` page is drawn as four mirrored 160x120 static
quadrants, the camera is built from the death position (7000 above, looking
1000 short of the body) and the corpse spins 8 units a frame, the "YOU DIED"
strip draws 256 one-pixel columns with the original's exact sine phase
(`0x100` per column) and amplitude ramp (59 down to 4, then back up during
the fade-out), the black overlay ramps 3 a frame, and the confirm button skips
to the title. The first death screen hides the player's head joint through the
shared grab/death one-shot, whose original image value is non-zero. The
converter packs `ui/died.tim` (66,080 bytes, 8bpp) and `arklay verify` reports
the new `tim` row green; `--ui death` boots the deterministic capture over
ROOM101 with `--character` selecting the corpse. The synthetic suite covers
the control gate (death, hit pre-emption, poison), the fall and pool, the
room-variant dispatch and the screen machine's frame counts and strip math;
the real-asset `tests/death_real.rs` decodes the shipped page, captures the
deterministic DIED screen, and kills ROOM100's player through the real damage
helper to reach the screen and return to the title. Documented deviations: the
Plant 42 DIED-screen joint-hide variant, the attract-demo replay and the
countdown-timer death branch stay deferred (see `docs/m18-deviations.md`).

E7 ports the hound (id `0x02`) as `enemy/em02.lua` and the chimera (id `0x09`)
as `enemy/em09.lua`. The hound script spells out the single 16-slot dispatch
block once and reads it through both bases — the state view indexed by `state`
and the behaviour view biased by five and indexed by `ignore` — then runs the
five sub-machines (patrol's begin/pick-turn/turn, chase's run/wall-follow/skid,
the five leap sub-states, the strafe's hold/accel/spring, the stalk's
begin/arc/reface/wall-follow) and the wall-follow switch that reduces each
sub-step's return to a sign. Its ten behaviours cover the idle walk/run patrol,
the chase with its pathfinder give-up into the stalk, the scripted entrances
(running leap, window crash, ceiling drop), the strafe, the bark/alert pause,
the maul, the close-range bite, the scripted idle and the stalking patience
meter with the room-shared bark latch. Nearly all steering runs through the
Rust probes: the forward probe steps 500 units along the full rotation vector,
resolves the room at radius 400 and steps back; the turn probe scales the live
speed, steers a yaw delta, probes and restores the accepted `position` words,
the local translation and the speed. The head tracker integrates the neck
yaw into the swerve word with its `+/-0x100` clamp, the leap/bite/maul chain
runs the shared ballistic step, the kill branch forces the player's maul
animation and the maul pins both sides' grab offsets, greys the player at
frame 0x66 and owes its blood billboards. The helpers the hound needed landed
with it: the probe pair with the `0x6C` save/restore, the spawn nudge, the
projectile-arc exposure, the head-track integration, the shared probe scratch
vector, the `+0x16C`..`+0x18A` typed scratch views, the 16-bit `position`-word
accessors, the room-shared bark latch and the player flags/offset writes.

The chimera script transcribes the four-state table (spawn, AI driver, hit
layer, death layer), the three stacked index bytes and the seventeen-entry
behaviour table whose last three slots are the same pointers as the variant
brain table. The three brains run the floor chase/walk hand-over, the coin-flip
spit or claw roll gated on the angular view and line of sight, and the ceiling
variant's mirrored yaw (the whole body flips 180 degrees and back) with its
flee, turn-back, swipe, drop and hit-fall hand-overs; hard mode injects the
variant toggle and the harder damage/threshold rows. The behaviours cover the
floor and ceiling idles, the zone-path walk, the timed turn, the double-claw
swipe with its two reach tests and the scratch overwrite quirk, the grab with
its mash meter bleeding into the player's attack direction, both variant
toggles, the drop and swoop dive arcs read in opposite directions, the acid
spit, the flee and turn-back (yaw-flipped steering), the claw with its wall-bit
grab cancel and hard-mode touch, the hit-fall and the stagger/knockdown
drivers, and both death dissolves with the shadow recolour/growth and the
death-event flag. The ceiling attachment parks the model at `-6008` with the
180-degree roll, and the per-frame shadow resize reads joint 3's world height
with the 600-clamp band and the freeze gate. The helpers the chimera needed
landed with it: the four chimera scratch views, the player's attack-direction
and animation-offset accessors, the torso/claw joint-attached billboards and
the entity-matrix acid fan. The corpus census now finds twelve scripted
monster ids (`0x00`..`0x05`, `0x07`, `0x09`, `0x0a`, `0x0e`, `0x11`, `0x13`)
and no script errors.

E12 closes the arc with the verification sweep. The corpus census replays
every `enemy` record in all 348 rooms into a fresh `GameState` and finds
twenty-one scripted monster ids (`0x00`-`0x15`, the DC-only `0x16` warning
park) allocating their recorded id/behaviour/position/animation/slot with
zero script errors and an exact deferred set; `enemy_real` boots one shipped
room per id through its script; `weapons_real` fires the real weapon set
through the input path at a shipped web and captures the held weapon in the
hand; `combat_real` re-decodes the packed combat tables against a fresh
decode and kills through the shared pipeline; `death_real` reaches the
shipped DIED screen from a real room and returns to the title; the NPC
differential and the actor/walk/entities/fire suites stay green, so the
character port and the monster scripts share one verified runtime. The pack
reconverts deterministically with the `combat`/`emw`/`tmd` rows green and
the `tim` row at 122/122; the remaining 28 `rdt` failures are the documented
four-byte stub rooms. The milestone deviations (render-contract items, the
attract demo, the endings, the DC branches) are catalogued in
`docs/m18-deviations.md` and the module docs.

The F1 room-select overlay is a port-only development aid the retail game
never shipped. It is always available (`--debug-menu` is accepted for
compatibility only); it opens only on F1, so captures and goldens that never
press the key are unaffected.

The in-game F9 return-to-title prompt follows the original: the first F9
freezes the room, dims the frame and pauses the game sounds; a second F9
returns to the title screen and any other key cancels and resumes the sounds.

## Format coverage

`arklay verify <pack>` classifies every entry by path/extension, runs it
through the reader the engine uses, and reports a per-format table. The
converted base pack on the reference install reports:

```
pack: re1.akpak
3206 entries, 769251500 bytes
format       entries        ok    failed          bytes
manifest           1         1         0            101
rdt              348       320        28      107997096
roomcut          842       842         0      175695340
roommask         601       601         0       29133338
bmp               12        12         0        1784964
tim              122       122         0        6401234
ivm               77        77         0        6525012
emd               63        63         0        8624580
emw               32        32         0         784348
tmd                7         7         0          18824
dor               34        34         0        2279820
wav              992       992         0      177670174
text               5         5         0           6581
maptables          1         1         0             72
combat             1         1         0           6924
biocard            1         1         0           1052
avi               27        27         0      251624740
lua               38        38         0         639312
opaque             2         2         0          57988
verify: 3206 entries, 3178 ok, 28 failed, 2 opaque
```

The 28 failing `room/*.rdt` entries are the converter's four-byte stub rooms
whose header declares no camera cuts: the engine refuses them on load and the
soak skips them, so `verify` reports them instead of green-lighting rooms the
engine cannot load. Every other entry parses. The two opaque entries are
`data/core00.esp` and `data/core00.etm`, the effect tables whose pair parser is
not in the path classifier; `--strict` turns them into failures by design.

The one pack carries everything: the 517 referenced voice `wav` entries
(101,697,368 bytes) and the 27 shipped `avi` films (251,624,740 bytes) sit
beside the room, audio and UI entries, and all three formats verify clean.

## Verification spine

| Commitment | Where | State |
|---|---|---|
| Golden capture harness | `tests/m16_ui_real.rs` (ignored, local goldens) | present; goldens are regression cross-checks, not an oracle |
| Movie decode goldens | `tests/m14_real.rs` (ignored) | present |
| Deterministic captures/states/saves | M16 tests, `tests/save_real.rs`, soak round-trip | present |
| Save round-trip | `tests/save_real.rs`, `tests/soak_real.rs`, synthetic M16 slice-10 test | present |
| Stable torture matrix (>=10k mutated inputs) | `tests/torture.rs` (parallel budget slice) | present, 18 formats |
| cargo-fuzz targets and seeds | `fuzz/` package (parallel fuzz slice) | present, 19 targets with committed synthetic seeds |
| Enemy spawn census | `tests/enemy_census.rs` (ignored) | present; every corpus record allocates, scripted set exact, zero script errors |
| Per-id enemy boots | `tests/enemy_real.rs` (ignored) | present; one shipped room per scripted id |
| Weapon/damage/death suites | `tests/weapons_real.rs`, `tests/combat_real.rs`, `tests/death_real.rs` (ignored) | present; real input kills, table decode, shipped DIED screen |
| Scheduled fuzz workflow | `.github/workflows/fuzz.yml` | present, nightly, non-gating |
| Decode budgets and memory ceilings | `src/budget.rs` (parallel budget slice), `tests/memory.rs` | present |
| `arklay verify` | `src/verify.rs`, `tests/cli.rs` | present |
| CLI error paths and atomic outputs | `src/atomic.rs`, `tests/cli.rs` | present |
| `--stats` frame-time report | `src/stats.rs`, `tests/cli.rs`, `tests/perf_real.rs` | present |
| Full-game soak | `tests/soak_real.rs` (ignored) | present |
| CI matrix (Linux/Windows/release/MSRV/artifact) | `.github/workflows/ci.yml` | present |
| Local gate | `scripts/check.sh` | present |
| `docs/architecture.md`, `docs/status.md`, `docs/m17-deviations.md` | this docs set | present |

Still missing by design:

- The cargo-fuzz runs need nightly and run on a weekly schedule, never as a
  pull-request gate; every change is covered by the always-run torture matrix
  and the stable `cargo check --manifest-path fuzz/Cargo.toml` compile gate.
- The real-asset tests, goldens and budgets need a local install and are never
  run in CI with game data; they are ignored tests behind
  `ARKLAY_RE1_ROOT`/`ARKLAY_RE1_PACK`.
- Performance budgets are machine-specific policy; the numbers and the
  reference machine are recorded in `docs/performance.md`.

## `TODO(parity)` dispositions

2 sites (`rg -n "TODO\(parity\)" src`):

| Group | Count | Sites | Disposition |
|---|---|---|---|
| Gameplay | 1 | backward clip `player.rs:62` | actor/world or death work |
| UI | 1 | title idle timer `ui/title.rs:237` | blocked on the attract demo, which replays inputs over monster rooms |

E4 closed the force-init/saved-state restore site: `spawn_enemy` now consults
the saved-enemy snapshot before re-initialising an occupied slot.

M18 closed nine of the sites this table used to list and removed their
comments: the collision-resolved walk, zone-graph walker, SCA resolve and
obstacle pathfinder in `enemy/walk.rs`; the idle init in `enemy/idle.rs`; the
spawn SCA-record TODO in `game.rs`; the pan law and per-bank one-shot restart
in `audio.rs`; and the per-record background blend weight in `engine.rs`. See
`docs/m18-deviations.md` for the replacements and their capture re-baselines.

E11a closed three more of the listed sites: the weapon-joint refresh
(`enemy/scd.rs`) now stores the posed weapon-hand joint worlds, the
flamethrower's two cues (`enemy/scd.rs`) queue through the room's enemy bank,
and the held-weapon TMD conversion (`convert.rs`) packs every `W*.EMW` and
`WS*.TMD`. The remaining table matches the current `rg` output.

E11b closed the health-byte aliasing site: the engine mirrors
`health_status` into the player's collision-callback byte before the player's
physics each tick, so a script that clears the spawn's `0x10` bit stops
skipping the shape-5 floor volumes exactly like the original.

## Road to enemies

The enemy milestone is complete. Every monster id the corpus spawns is
scripted in the game pack (ids `0x00`-`0x15`; the DC-only Forest zombie
`0x16` parks inert), the scripted characters are fully ported, the player
weapons and the death/game-over flow are live, and the verification spine
covers them end to end: the spawn census across all 348 rooms, the per-id
real-asset boots, the real weapon/damage/death suites, the reconverted
`verify` table with the combat and weapon rows green, and the always-run
synthetic suites for every machine.

Still open, by design:

- The attract demo and the title idle timer (the four recorded reels replay
  inputs over boss rooms; the title timer has no destination without them).
- The ending path (`--ending`-only until the post-fight sequences land).
- The DC-only Forest zombie (`0x16`) and the DC branches the port does not
  model.
- The documented render contracts: per-joint colour tints and joint-object
  gore chunks/trails, the ribbon rasterisation, the Plant 42 DIED-screen
  joint-hide pose, and the flamethrower/acid/rocket per-frame projectile
  damage. Each is catalogued in the module docs and
  `docs/m18-deviations.md`.

No RE2/RE3 runtime, format, pack or migration work is planned until those
RE1 gaps close: there is no RE2 corpus in the workspace, and the canonical
re-base cannot be validated without the full roster. The M17 harnesses
already key off the manifest's `rdt_version`/`scd_version` and per-entry path
prefixes, so a second dialect adds parser rows rather than rewriting them.
