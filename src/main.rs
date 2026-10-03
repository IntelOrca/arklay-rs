use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "arklay",
    version,
    about = "A from-scratch engine for the classic Resident Evil games"
)]
struct Cli {
    /// Game pack to launch
    #[arg(value_name = "PACK")]
    pack: Option<PathBuf>,

    /// Room id, three hex digits: stage digit plus room, e.g. 100 or 11C
    #[arg(long, value_name = "ROOM", requires = "pack")]
    room: Option<String>,

    /// Player/flag digit selecting the RDT variant (0-9)
    #[arg(long, default_value_t = 0, value_name = "N", requires = "pack", value_parser = clap::value_parser!(u8).range(0..=9))]
    player: u8,

    /// Render one frame to a file and exit (headless testing)
    #[arg(long, value_name = "FILE", requires = "pack")]
    capture: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Convert a game installation into an .akpak game pack
    ConvertGame {
        /// Root of the game installation or data directory
        root: PathBuf,

        /// Output pack path
        #[arg(long, default_value = "re1.akpak")]
        out: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::ConvertGame { root, out }) => arklay::convert::convert_game(&root, &out),
        None => {
            let Some(pack) = cli.pack else {
                bail!("a game pack is required (or use `arklay convert-game`)");
            };
            let Some(room) = cli.room else {
                bail!("--room is required when launching a pack");
            };
            let id = arklay::state::RoomId::from_room_and_player(&room, cli.player)?;
            arklay::engine::run(&pack, id, cli.capture.as_deref())
        }
    }
}
