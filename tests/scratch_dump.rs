mod common;
use std::path::PathBuf;
use arklay::pack::Pack;
use arklay::rdt;
use arklay::state::RoomId;

fn pack_path() -> Option<PathBuf> { Some(common::asset_env()?.1) }

#[test]
#[ignore]
fn dump_cuts() {
    let Some(path) = pack_path() else { return };
    let pack = Pack::open(&path).unwrap();
    for name in ["1060", "1070", "40D0"] {
        let id = RoomId::parse(name).unwrap();
        let bytes = pack.read(&id.rdt_entry()).unwrap();
        let room = rdt::parse(bytes, id).unwrap();
        println!("=== {name} ===");
        for (i, cut) in room.cuts.iter().take(6).enumerate() {
            println!("cut {i}: pos={:?} look_at={:?} fov={}", cut.pos, cut.look_at, cut.fov);
        }
        for (i, z) in room.zones.iter().take(4).enumerate() {
            println!("zone {i}: cam {}->{} corners={:?}", z.cam_from, z.cam_to, z.corners);
        }
    }
}
