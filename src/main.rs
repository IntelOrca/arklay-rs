use std::path::PathBuf;

use anyhow::{Context, Result, bail};
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

    /// Script tools
    Scd {
        #[command(subcommand)]
        action: ScdAction,
    },
}

#[derive(Subcommand, Debug)]
enum ScdAction {
    /// Export the SCD embedded in an RDT as disassembly or decompilation
    Export {
        /// Input RDT file
        rdt: PathBuf,

        /// Output file: .s for disassembly, .bio for decompilation
        #[arg(long, short)]
        out: PathBuf,

        /// Also write a .lst listing next to the output
        #[arg(long)]
        list: bool,
    },
}

/// Export an RDT's SCD streams to `out` (`.s` disassembly or `.bio`
/// decompilation), optionally writing a `.lst` listing beside it.
fn export_scd(rdt: &std::path::Path, out: &std::path::Path, list: bool) -> Result<()> {
    let data = std::fs::read(rdt).with_context(|| format!("failed to read {}", rdt.display()))?;
    let scripts = arklay::scd::reader::parse(&data)
        .with_context(|| format!("failed to parse the SCD in {}", rdt.display()))?;

    let extension = out
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    let text = if extension.eq_ignore_ascii_case("bio") {
        arklay::scd::decomp::render(&scripts, &data)
    } else if extension.eq_ignore_ascii_case("s") {
        arklay::scd::disasm::render(&scripts, &data)
    } else {
        bail!("output `{}` must end in .s or .bio", out.display());
    };
    std::fs::write(out, text).with_context(|| format!("failed to write {}", out.display()))?;

    if list {
        let listing = arklay::scd::disasm::render_listing(&scripts, &data);
        let listing_path = out.with_extension("lst");
        std::fs::write(&listing_path, listing)
            .with_context(|| format!("failed to write {}", listing_path.display()))?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::ConvertGame { root, out }) => arklay::convert::convert_game(&root, &out),
        Some(Command::Scd { action }) => match action {
            ScdAction::Export { rdt, out, list } => export_scd(&rdt, &out, list),
        },
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
