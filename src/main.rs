use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use arklay::pack::Pack;
use arklay::progress::{Progress, format_duration};

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

    /// UI screen to boot straight into instead of a room:
    /// `title`, `select`, `game`, `menu`, `box`, `file`, `view`, `save`, `load` or
    /// `font`
    #[arg(
        long,
        value_name = "SCREEN",
        requires = "pack",
        conflicts_with = "room"
    )]
    ui: Option<String>,

    /// Directory holding `savedat*.dat` (default: `saves/` beside the pack)
    #[arg(long, value_name = "DIR", requires = "pack")]
    save_dir: Option<PathBuf>,

    /// Voice pack (`voice/*.wav`); defaults to the sibling
    /// `<pack stem>.voice.akpak` when present. A run without one skips every
    /// voice line and never waits on F7.
    #[arg(long, value_name = "PATH", requires = "pack")]
    voice: Option<PathBuf>,

    /// Render one frame to a file and exit (headless testing)
    #[arg(long, value_name = "FILE", requires = "pack")]
    capture: Option<PathBuf>,

    /// Run N fixed 30 Hz ticks before a `--room` capture frame is drawn, so
    /// scripted NPC scenes can be captured deterministically and audio-free
    #[arg(
        long,
        value_name = "N",
        default_value_t = 0,
        requires = "capture",
        conflicts_with = "ui"
    )]
    ticks: u32,

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

        /// Game executable holding the text tables (default: a `Bio.exe`
        /// discovered under the root)
        #[arg(long, value_name = "EXE")]
        exe: Option<PathBuf>,

        /// Number of worker threads (default: one per available CPU)
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..))]
        jobs: Option<u16>,

        /// Write the voice WAVs to a second pack
        /// (default `<out stem>.voice.akpak`)
        #[arg(long, value_name = "PATH", conflicts_with_all = ["no_voice", "with_voice"])]
        voice_out: Option<PathBuf>,

        /// Skip the voice pack entirely
        #[arg(long, conflicts_with = "with_voice")]
        no_voice: bool,

        /// Embed the voice WAVs in the main pack instead of a second pack
        #[arg(long)]
        with_voice: bool,
    },

    /// Extract every entry of a game pack into a directory
    Extract {
        /// Game pack to read
        #[arg(value_name = "PACK")]
        pack: PathBuf,

        /// Output directory (created if needed)
        #[arg(long, short, value_name = "OUT_DIR")]
        out: PathBuf,
    },

    /// List every entry of a game pack with its size
    List {
        /// Game pack to read
        #[arg(value_name = "PACK")]
        pack: PathBuf,
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

/// Extract every entry of `pack_path` below `out_dir`, preserving paths.
///
/// The pack reader validates every entry path, so no file can be written
/// outside `out_dir`.
fn extract_pack(pack_path: &Path, out_dir: &Path) -> Result<()> {
    let pack = Pack::open(pack_path)?;
    let mut progress = Progress::new();
    progress.begin("extract", pack.len() as u64, "files");

    let mut bytes = 0u64;
    for entry in pack.entries() {
        let data = pack.read(entry.path())?;
        let destination = out_dir.join(entry.path());
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&destination, data)
            .with_context(|| format!("failed to write {}", destination.display()))?;
        bytes += data.len() as u64;
        progress.advance(entry.path());
    }
    progress.end_phase();

    println!(
        "extracted {} entries, {bytes} bytes to {} in {}",
        pack.len(),
        out_dir.display(),
        format_duration(progress.elapsed())
    );
    Ok(())
}

/// Write one deterministic `list` line per entry: right-aligned size, path.
///
/// Returns the entry count and the total bytes listed.
fn write_list(pack: &Pack, out: &mut impl Write) -> Result<(u64, u64)> {
    let mut count = 0u64;
    let mut bytes = 0u64;
    for entry in pack.entries() {
        writeln!(out, "{:>12} {}", entry.size(), entry.path())?;
        count += 1;
        bytes += entry.size() as u64;
    }
    Ok((count, bytes))
}

/// Print every entry of `pack_path` and a final count/bytes summary.
fn list_pack(pack_path: &Path) -> Result<()> {
    let pack = Pack::open(pack_path)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let (count, bytes) = write_list(&pack, &mut out)?;
    writeln!(out, "{count} entries, {bytes} bytes")?;
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::ConvertGame {
            root,
            out,
            exe,
            jobs,
            voice_out,
            no_voice,
            with_voice,
        }) => {
            let voice = if no_voice {
                arklay::convert::VoicePackOptions::Skip
            } else if with_voice {
                arklay::convert::VoicePackOptions::Embed
            } else {
                arklay::convert::VoicePackOptions::Sibling(voice_out)
            };
            arklay::convert::convert_game_with_voice(
                &root,
                &out,
                exe.as_deref(),
                jobs.map_or(0, usize::from),
                &voice,
            )
        }
        Some(Command::Extract { pack, out }) => extract_pack(&pack, &out),
        Some(Command::List { pack }) => list_pack(&pack),
        Some(Command::Scd { action }) => match action {
            ScdAction::Export { rdt, out, list } => export_scd(&rdt, &out, list),
        },
        None => {
            let Some(pack) = cli.pack else {
                bail!("a game pack is required (or use `arklay convert-game`)");
            };
            let save_dir = cli
                .save_dir
                .unwrap_or_else(|| arklay::save::default_save_dir_for_pack(&pack));
            if let Some(screen) = cli.ui {
                return arklay::engine::run_ui_with_voice(
                    &pack,
                    &screen,
                    cli.capture.as_deref(),
                    &save_dir,
                    cli.player,
                    cli.voice.as_deref(),
                );
            }
            if let Some(room) = cli.room {
                let id = arklay::state::RoomId::from_room_and_player(&room, cli.player)?;
                return arklay::engine::run_with_voice(
                    &pack,
                    id,
                    cli.capture.as_deref(),
                    cli.ticks,
                    cli.voice.as_deref(),
                );
            }
            // No room and no `--ui`: boot the title screen, the app root.
            arklay::engine::run_ui_with_voice(
                &pack,
                "title",
                cli.capture.as_deref(),
                &save_dir,
                cli.player,
                cli.voice.as_deref(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use arklay::pack::PackWriter;

    /// Self-deleting temporary directory unique to this process and label.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("arklay-cli-{}-{label}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// A two-entry pack written to `path`, also opened for comparison.
    fn sample_pack(path: &Path) -> Pack {
        let mut writer = PackWriter::new();
        writer.add("room/1001.rdt", vec![0x11, 0x22, 0x33]).unwrap();
        writer.add("bgm/013.wav", b"RIFF....WAVE".to_vec()).unwrap();
        writer.write(path).unwrap();
        Pack::open(path).unwrap()
    }

    #[test]
    fn extraction_round_trips_every_entry() {
        let dir = TempDir::new("extract");
        let pack_path = dir.path.join("game.akpak");
        let pack = sample_pack(&pack_path);

        let out = dir.path.join("out");
        extract_pack(&pack_path, &out).unwrap();

        for entry in pack.entries() {
            let extracted = fs::read(out.join(entry.path())).unwrap();
            assert_eq!(extracted, pack.read(entry.path()).unwrap());
        }
        assert_eq!(
            fs::read(out.join("room/1001.rdt")).unwrap(),
            pack.read("room/1001.rdt").unwrap()
        );
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn extraction_of_real_pack_matches_reader() {
        let Ok(pack_path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack_path = PathBuf::from(pack_path);
        let pack = Pack::open(&pack_path).unwrap();

        let dir = TempDir::new("real-extract");
        let out = dir.path.join("out");
        extract_pack(&pack_path, &out).unwrap();

        for sample in ["room/1001.rdt", "roomcut/100_000.bmp", "bgm/013.wav"] {
            let extracted = fs::read(out.join(sample)).unwrap();
            assert_eq!(extracted, pack.read(sample).unwrap(), "{sample}");
        }
    }

    #[test]
    fn list_lines_are_right_aligned_and_summarized() {
        let dir = TempDir::new("list");
        let pack_path = dir.path.join("game.akpak");
        let pack = sample_pack(&pack_path);

        let mut out = Vec::new();
        let (count, bytes) = write_list(&pack, &mut out).unwrap();

        assert_eq!(count, 2);
        assert_eq!(bytes, 15);
        let expected = format!(
            "{:>12} {}\n{:>12} {}\n",
            "RIFF....WAVE".len(),
            "bgm/013.wav",
            3,
            "room/1001.rdt"
        );
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }
}
