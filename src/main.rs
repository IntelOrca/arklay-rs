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
    /// `title`, `select`, `game`, `menu`, `box`, `file`, `map`, `view`, `save`,
    /// `load` or `font`
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

    /// Movie pack (`movie/*.avi`); defaults to the sibling
    /// `<pack stem>.movie.akpak` when present. A run without one advances
    /// every film request instead of playing it.
    #[arg(long, value_name = "PATH", requires = "pack")]
    movie: Option<PathBuf>,

    /// Play one film id (0-28) standalone and exit; with `--capture` the
    /// frame after `--ticks` fixed ticks is written headlessly
    #[arg(
        long,
        value_name = "ID",
        requires = "pack",
        conflicts_with_all = ["room", "ui", "ending"]
    )]
    fmv: Option<u8>,

    /// Play ending row `ID` (1-7) as its film chain and exit; with `--capture`
    /// the frame after `--ticks` fixed ticks is written headlessly
    #[arg(
        long,
        value_name = "ID",
        requires = "pack",
        conflicts_with_all = ["room", "ui"],
        value_parser = clap::value_parser!(u8).range(1..=7)
    )]
    ending: Option<u8>,

    /// Character for the `--fmv` prologue cut and the `--ending` chain
    /// (0 Chris, 1 Jill)
    #[arg(
        long,
        value_name = "N",
        default_value_t = 0,
        value_parser = clap::value_parser!(u8).range(0..=1)
    )]
    character: u8,

    /// Mod pack layers to apply, repeatable (ignored with `--no-mods`)
    #[arg(long = "mod", value_name = "PATH", requires = "pack")]
    mods: Vec<PathBuf>,

    /// Ignore the `--mod` layers and the sibling `mods/` directory
    #[arg(long, requires = "pack")]
    no_mods: bool,

    /// Render one frame to a file and exit (headless testing)
    #[arg(long, value_name = "FILE", requires = "pack")]
    capture: Option<PathBuf>,

    /// Run N fixed 30 Hz ticks before a capture frame is drawn, so scripted
    /// NPC scenes and `--fmv` frames can be captured deterministically and
    /// audio-free
    #[arg(long, value_name = "N", default_value_t = 0, conflicts_with = "ui")]
    ticks: u32,

    /// Print a frame-time report after the run: per-tick min/avg/p95/max, the
    /// load/update/effect/render phase totals and the entity/effect high-water
    /// marks. Requires `--ticks`; a `--capture` is not needed.
    #[arg(long, requires = "ticks", conflicts_with = "ui")]
    stats: bool,

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

        /// Write the film AVIs to a second pack
        /// (default `<out stem>.movie.akpak`)
        #[arg(long, value_name = "PATH", conflicts_with_all = ["no_movie", "with_movie"])]
        movie_out: Option<PathBuf>,

        /// Skip the movie pack entirely
        #[arg(long, conflicts_with = "with_movie")]
        no_movie: bool,

        /// Embed the film AVIs in the main pack instead of a second pack
        #[arg(long)]
        with_movie: bool,
    },

    /// Extract every entry of a game pack into a directory
    Extract {
        /// Game pack to read
        #[arg(value_name = "PACK")]
        pack: PathBuf,

        /// Output directory (created if needed)
        #[arg(long, short, value_name = "OUT_DIR")]
        out: PathBuf,

        /// Mod pack layers to apply, repeatable
        #[arg(long = "mod", value_name = "PATH")]
        mods: Vec<PathBuf>,
    },

    /// List every entry of a game pack with its size
    List {
        /// Game pack to read
        #[arg(value_name = "PACK")]
        pack: PathBuf,

        /// Mod pack layers to apply, repeatable
        #[arg(long = "mod", value_name = "PATH")]
        mods: Vec<PathBuf>,
    },

    /// Validate a game pack: parse every known-format entry and report a
    /// per-format table. Unknown entries are counted opaque and do not fail
    /// the run unless `--strict` is set.
    Verify {
        /// Game pack to read
        #[arg(value_name = "PACK")]
        pack: PathBuf,

        /// Mod pack layers to apply, repeatable
        #[arg(long = "mod", value_name = "PATH")]
        mods: Vec<PathBuf>,

        /// Treat every entry whose format is not in the verifier's table as a
        /// failure
        #[arg(long)]
        strict: bool,
    },

    /// Script tools
    Scd {
        #[command(subcommand)]
        action: ScdAction,
    },

    /// Mod authoring tools
    Mod {
        #[command(subcommand)]
        action: ModAction,
    },

    /// Pack inspection tools
    Pack {
        #[command(subcommand)]
        action: PackAction,
    },
}

#[derive(Subcommand, Debug)]
enum ModAction {
    /// Build a mod source directory into an .akpak mod pack
    Build {
        /// Mod source directory, which must hold manifest.toml
        #[arg(value_name = "DIR")]
        dir: PathBuf,

        /// Output mod pack path
        #[arg(long, value_name = "PATH")]
        out: PathBuf,

        /// Base pack checking the declared base id and the override paths
        #[arg(long, value_name = "PATH")]
        base: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum PackAction {
    /// Print a pack's manifest, applied layers and merged entries
    Info {
        /// Game pack to read
        #[arg(value_name = "PACK")]
        pack: PathBuf,

        /// Mod pack layers to apply, repeatable
        #[arg(long = "mod", value_name = "PATH")]
        mods: Vec<PathBuf>,
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

    /// Assemble a text .s source into a standalone .scd container
    Build {
        /// Input .s source
        input: PathBuf,

        /// Output .scd container
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
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
    arklay::atomic::write(out, text.as_bytes())
        .with_context(|| format!("failed to write {}", out.display()))?;

    if list {
        let listing = arklay::scd::disasm::render_listing(&scripts, &data);
        let listing_path = out.with_extension("lst");
        arklay::atomic::write(&listing_path, listing.as_bytes())
            .with_context(|| format!("failed to write {}", listing_path.display()))?;
    }
    Ok(())
}

/// Assemble a text `.s` source into a standalone `.scd` container.
fn build_scd(input: &Path, out: &Path) -> Result<()> {
    let text =
        fs::read_to_string(input).with_context(|| format!("failed to read {}", input.display()))?;
    let assembled = arklay::scd::asm::assemble(&text)
        .with_context(|| format!("failed to assemble {}", input.display()))?;
    let container = assembled
        .to_container()
        .with_context(|| format!("failed to lay out {}", input.display()))?;
    arklay::atomic::write(out, &container)
        .with_context(|| format!("failed to write {}", out.display()))?;
    println!(
        "assembled {} -> {} ({} bytes)",
        input.display(),
        out.display(),
        container.len()
    );
    Ok(())
}

/// Build a mod source directory and print its summary and aggregated warnings.
fn build_mod_pack(dir: &Path, out: &Path, base: Option<&Path>) -> Result<()> {
    let summary = arklay::modding::build_mod(dir, out, base)?;
    for warning in &summary.warnings {
        println!("warning: {warning}");
    }
    println!(
        "built {} ({} entries, {} bytes)",
        out.display(),
        summary.entries.len(),
        summary.bytes
    );
    Ok(())
}

/// Open `pack_path`, applying the `mods` layers and reporting their warnings
/// on stderr so `list`/`extract` stdout stays a clean machine-readable table.
fn open_game_pack(pack_path: &Path, mods: &[PathBuf]) -> Result<Pack> {
    if mods.is_empty() {
        return Pack::open(pack_path);
    }
    let pack = Pack::open_layered(pack_path, mods)?;
    for warning in pack.warnings() {
        eprintln!("warning: {warning}");
    }
    Ok(pack)
}

/// Extract every entry of `pack_path` below `out_dir`, preserving paths.
///
/// The pack reader validates every entry path, so no file can be written
/// outside `out_dir`.
fn extract_pack(pack_path: &Path, out_dir: &Path, mods: &[PathBuf]) -> Result<()> {
    let pack = open_game_pack(pack_path, mods)?;
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
        arklay::atomic::write(&destination, data)
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
fn list_pack(pack_path: &Path, mods: &[PathBuf]) -> Result<()> {
    let pack = open_game_pack(pack_path, mods)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let (count, bytes) = write_list(&pack, &mut out)?;
    writeln!(out, "{count} entries, {bytes} bytes")?;
    Ok(())
}

/// Validate every known-format entry of `pack_path` and print the report.
///
/// The report goes to stdout; the error summary goes to stderr, and the exit
/// status is non-zero when any known-format entry failed (or, under
/// `--strict`, any unknown entry).
fn verify_pack(pack_path: &Path, mods: &[PathBuf], strict: bool) -> Result<()> {
    // `open_game_pack` reports layer warnings on stderr, keeping the report
    // stdout clean.
    let pack = open_game_pack(pack_path, mods)?;
    let report = arklay::verify::verify_pack(&pack, strict);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    report.write(pack_path, &mut out)?;
    out.flush()?;
    if !report.valid() {
        bail!("{} pack entries failed verification", report.failed);
    }
    Ok(())
}

/// Write the `pack info` report: manifest, applied layers and merged entries.
fn write_pack_info(pack_path: &Path, pack: &Pack, out: &mut impl Write) -> Result<()> {
    writeln!(out, "pack: {}", pack_path.display())?;
    match pack.manifest() {
        Some(manifest) => {
            writeln!(out, "manifest: {} ({})", manifest.id, manifest.kind)?;
            writeln!(out, "  format: {}", manifest.format)?;
            if let Some(name) = &manifest.name {
                writeln!(out, "  name: {name}")?;
            }
            if let Some(version) = &manifest.version {
                writeln!(out, "  version: {version}")?;
            }
            if let Some(engine) = &manifest.engine {
                writeln!(out, "  engine: {engine}")?;
            }
            writeln!(out, "  rdt: {}", manifest.rdt_version)?;
            writeln!(out, "  scd: {}", manifest.scd_version)?;
            if !manifest.lua.is_empty() {
                writeln!(out, "  lua: {}", manifest.lua.join(", "))?;
            }
        }
        None => writeln!(out, "manifest: none")?,
    }
    let layers: Vec<&Pack> = pack.overrides().collect();
    writeln!(out, "layers: {}", layers.len())?;
    for (index, layer) in layers.iter().enumerate() {
        let path = layer
            .source()
            .map_or_else(|| "<memory>".to_string(), |path| path.display().to_string());
        match layer.manifest() {
            Some(manifest) => writeln!(
                out,
                "  {index}: {path} (id {}, load_order {}, base {})",
                manifest.id,
                manifest.load_order,
                manifest.base.as_deref().unwrap_or("-")
            )?,
            None => writeln!(out, "  {index}: {path}")?,
        }
    }
    writeln!(out, "entries:")?;
    let (count, bytes) = write_list(pack, out)?;
    writeln!(out, "{count} entries, {bytes} bytes")?;
    Ok(())
}

/// Print the `pack info` report for `pack_path` plus the `mods` layers.
fn pack_info(pack_path: &Path, mods: &[PathBuf]) -> Result<()> {
    // `open_game_pack` already reports any layer warnings on stderr.
    let pack = open_game_pack(pack_path, mods)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    write_pack_info(pack_path, &pack, &mut out)
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
            movie_out,
            no_movie,
            with_movie,
        }) => {
            let voice = if no_voice {
                arklay::convert::VoicePackOptions::Skip
            } else if with_voice {
                arklay::convert::VoicePackOptions::Embed
            } else {
                arklay::convert::VoicePackOptions::Sibling(voice_out)
            };
            let movie = if no_movie {
                arklay::convert::MoviePackOptions::Skip
            } else if with_movie {
                arklay::convert::MoviePackOptions::Embed
            } else {
                arklay::convert::MoviePackOptions::Sibling(movie_out)
            };
            arklay::convert::convert_game_with_packs(
                &root,
                &out,
                exe.as_deref(),
                jobs.map_or(0, usize::from),
                &voice,
                &movie,
            )
        }
        Some(Command::Extract { pack, out, mods }) => extract_pack(&pack, &out, &mods),
        Some(Command::List { pack, mods }) => list_pack(&pack, &mods),
        Some(Command::Verify { pack, mods, strict }) => verify_pack(&pack, &mods, strict),
        Some(Command::Scd { action }) => match action {
            ScdAction::Export { rdt, out, list } => export_scd(&rdt, &out, list),
            ScdAction::Build { input, out } => build_scd(&input, &out),
        },
        Some(Command::Mod { action }) => match action {
            ModAction::Build { dir, out, base } => build_mod_pack(&dir, &out, base.as_deref()),
        },
        Some(Command::Pack { action }) => match action {
            PackAction::Info { pack, mods } => pack_info(&pack, &mods),
        },
        None => {
            let Some(pack) = cli.pack else {
                bail!("a game pack is required (or use `arklay convert-game`)");
            };
            if cli.ticks > 0 && cli.capture.is_none() && !cli.stats {
                bail!("--ticks requires --capture (or --stats)");
            }
            if cli.stats && cli.room.is_none() {
                bail!("--stats requires --room");
            }
            let save_dir = cli
                .save_dir
                .unwrap_or_else(|| arklay::save::default_save_dir_for_pack(&pack));
            if let Some(id) = cli.ending {
                return arklay::engine::run_ending(
                    &pack,
                    cli.movie.as_deref(),
                    id,
                    cli.character,
                    cli.capture.as_deref(),
                    cli.ticks,
                    &cli.mods,
                    cli.no_mods,
                );
            }
            if let Some(id) = cli.fmv {
                return arklay::engine::run_fmv(
                    &pack,
                    cli.movie.as_deref(),
                    id,
                    cli.character,
                    cli.capture.as_deref(),
                    cli.ticks,
                    &cli.mods,
                    cli.no_mods,
                );
            }
            if let Some(screen) = cli.ui {
                return arklay::engine::run_ui_with_voice_and_movie(
                    &pack,
                    &screen,
                    cli.capture.as_deref(),
                    &save_dir,
                    cli.player,
                    cli.voice.as_deref(),
                    cli.movie.as_deref(),
                    &cli.mods,
                    cli.no_mods,
                );
            }
            if let Some(room) = cli.room {
                let id = arklay::state::RoomId::from_room_and_player(&room, cli.player)?;
                return arklay::engine::run_with_voice_and_movie(
                    &pack,
                    id,
                    cli.capture.as_deref(),
                    cli.ticks,
                    cli.voice.as_deref(),
                    cli.movie.as_deref(),
                    &cli.mods,
                    cli.no_mods,
                    cli.stats,
                );
            }
            // No room and no `--ui`: boot the app root, whose interactive run
            // plays the logos, the title opening and the select prologue. A
            // capture still boots the title directly and plays no films.
            arklay::engine::run_root_with_voice_and_movie(
                &pack,
                cli.capture.as_deref(),
                &save_dir,
                cli.voice.as_deref(),
                cli.movie.as_deref(),
                &cli.mods,
                cli.no_mods,
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
        extract_pack(&pack_path, &out, &[]).unwrap();

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
        extract_pack(&pack_path, &out, &[]).unwrap();

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

    #[test]
    fn list_shows_the_merged_view_of_two_layers() {
        use arklay::manifest;

        let dir = TempDir::new("layered-list");
        let base_path = dir.path.join("base.akpak");
        let first_path = dir.path.join("first.akpak");
        let second_path = dir.path.join("second.akpak");

        let mut base = PackWriter::new();
        base.add(
            manifest::ENTRY,
            manifest::Manifest::base("re1").render().into_bytes(),
        )
        .unwrap();
        base.add("room/1000.rdt", b"base".to_vec()).unwrap();
        base.write(&base_path).unwrap();

        let manifest_bytes = |id: &str, load_order: i32| {
            manifest::Manifest {
                kind: manifest::PackKind::Mod,
                base: Some("re1".to_string()),
                load_order,
                ..manifest::Manifest::base(id)
            }
            .render()
            .into_bytes()
        };
        let mut first = PackWriter::new();
        first
            .add(manifest::ENTRY, manifest_bytes("first", 0))
            .unwrap();
        first.add("room/1000.rdt", b"first".to_vec()).unwrap();
        first.write(&first_path).unwrap();
        let second_manifest = manifest_bytes("second", 10);
        let mut second = PackWriter::new();
        second
            .add(manifest::ENTRY, second_manifest.clone())
            .unwrap();
        second.add("room/1000.rdt", b"second".to_vec()).unwrap();
        second.write(&second_path).unwrap();

        let pack = open_game_pack(&base_path, &[first_path, second_path]).unwrap();
        let mut out = Vec::new();
        let (count, bytes) = write_list(&pack, &mut out).unwrap();

        assert_eq!(count, 2);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "{:>12} manifest.toml\n{:>12} room/1000.rdt\n",
                second_manifest.len(),
                "second".len()
            )
        );
        assert_eq!(bytes as usize, second_manifest.len() + "second".len());
    }

    #[test]
    fn scd_build_writes_a_container_the_reader_parses() {
        let dir = TempDir::new("scd-build");
        let input = dir.path.join("1000.s");
        let out = dir.path.join("1000.scd");
        fs::write(
            &input,
            ".version 1\n\n.main\n.block\n    nop                     0\n\n.event event_00\n    evt_finish\n",
        )
        .unwrap();

        build_scd(&input, &out).unwrap();

        let bytes = fs::read(&out).unwrap();
        let scripts = arklay::scd::reader::parse(&bytes).unwrap();
        assert_eq!(scripts.main.len(), 1);
        assert_eq!(scripts.main[0].insns[0].bytes, [0x0E, 0x00]);
        assert_eq!(scripts.events.len(), 1);
        assert_eq!(scripts.events[0].insns[0].bytes, [0xFF]);
    }

    #[test]
    fn pack_info_reports_the_manifest_layers_and_merged_entries() {
        use arklay::manifest;

        let dir = TempDir::new("pack-info");
        let base_path = dir.path.join("base.akpak");
        let mod_path = dir.path.join("demo.akpak");

        let base_manifest = manifest::Manifest {
            name: Some("Base".to_string()),
            version: Some("1.0".to_string()),
            engine: Some("arklay test".to_string()),
            ..manifest::Manifest::base("re1")
        };
        let mut base = PackWriter::new();
        base.add(manifest::ENTRY, base_manifest.render().into_bytes())
            .unwrap();
        base.add("room/1000.rdt", b"base".to_vec()).unwrap();
        base.write(&base_path).unwrap();

        let mod_manifest = manifest::Manifest {
            kind: manifest::PackKind::Mod,
            base: Some("re1".to_string()),
            load_order: 7,
            lua: vec!["lua/demo.lua".to_string()],
            ..manifest::Manifest::base("demo")
        };
        let mut layer = PackWriter::new();
        layer
            .add(manifest::ENTRY, mod_manifest.render().into_bytes())
            .unwrap();
        layer.add("lua/demo.lua", b"return 1".to_vec()).unwrap();
        layer.write(&mod_path).unwrap();

        let pack = open_game_pack(&base_path, std::slice::from_ref(&mod_path)).unwrap();
        let mut out = Vec::new();
        write_pack_info(&base_path, &pack, &mut out).unwrap();
        let lua_size = 8;
        let manifest_size = mod_manifest.render().len();
        let bytes = lua_size + manifest_size + 4;
        let expected = format!(
            "\
pack: {base}
manifest: re1 (base)
  format: 1
  name: Base
  version: 1.0
  engine: arklay test
  rdt: re1
  scd: re1
layers: 1
  0: {mod_path} (id demo, load_order 7, base re1)
entries:
{lua_size:>12} lua/demo.lua
{manifest_size:>12} manifest.toml
{room_size:>12} room/1000.rdt
3 entries, {bytes} bytes
",
            base = base_path.display(),
            mod_path = mod_path.display(),
            room_size = 4,
        );
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    #[test]
    fn pack_info_reports_a_manifest_less_pack() {
        let dir = TempDir::new("pack-info-bare");
        let pack_path = dir.path.join("re1.akpak");
        let pack = sample_pack(&pack_path);
        let mut out = Vec::new();
        write_pack_info(&pack_path, &pack, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("manifest: none"), "{text}");
        assert!(text.contains("layers: 0"), "{text}");
        assert!(text.contains("2 entries, 15 bytes"), "{text}");
    }

    #[test]
    fn the_cli_accepts_the_runtime_mod_flags() {
        let cli = Cli::try_parse_from([
            "arklay",
            "game.akpak",
            "--room",
            "100",
            "--mod",
            "a.akpak",
            "--mod",
            "b.akpak",
            "--no-mods",
        ])
        .unwrap();
        assert_eq!(
            cli.mods,
            [PathBuf::from("a.akpak"), PathBuf::from("b.akpak")]
        );
        assert!(cli.no_mods);
        assert_eq!(cli.room.as_deref(), Some("100"));

        // The subcommands keep their own `--mod` flags.
        let list =
            Cli::try_parse_from(["arklay", "list", "game.akpak", "--mod", "x.akpak"]).unwrap();
        assert!(matches!(
            list.command,
            Some(Command::List { mods, .. }) if mods == [PathBuf::from("x.akpak")]
        ));
    }

    #[test]
    fn runtime_layer_set_discovers_siblings_and_honours_no_mods() {
        use arklay::manifest;
        use arklay::pack::Pack;

        let dir = TempDir::new("runtime-layers");
        let game_dir = dir.path.join("game");
        fs::create_dir_all(game_dir.join("mods")).unwrap();
        let pack_path = game_dir.join("re1.akpak");

        let mut base = PackWriter::new();
        base.add(
            manifest::ENTRY,
            manifest::Manifest::base("re1").render().into_bytes(),
        )
        .unwrap();
        base.add("room/1000.rdt", b"base".to_vec()).unwrap();
        base.write(&pack_path).unwrap();

        let mod_path = game_dir.join("mods/demo.akpak");
        let mod_manifest = manifest::Manifest {
            kind: manifest::PackKind::Mod,
            base: Some("re1".to_string()),
            load_order: 10,
            ..manifest::Manifest::base("demo")
        };
        let mut layer = PackWriter::new();
        layer
            .add(manifest::ENTRY, mod_manifest.render().into_bytes())
            .unwrap();
        layer.add("room/1000.rdt", b"layer".to_vec()).unwrap();
        layer.write(&mod_path).unwrap();

        // The sibling directory is discovered.
        let discovered = arklay::engine::open_game_pack(&pack_path, &[], false).unwrap();
        assert!(discovered.is_layered());
        assert_eq!(discovered.read("room/1000.rdt").unwrap(), b"layer");

        // `--no-mods` ignores both the discovery and an explicit `--mod`.
        let vanilla =
            arklay::engine::open_game_pack(&pack_path, std::slice::from_ref(&mod_path), true)
                .unwrap();
        assert!(!vanilla.is_layered());
        assert_eq!(vanilla.read("room/1000.rdt").unwrap(), b"base");
        assert_eq!(
            vanilla.paths().collect::<Vec<_>>(),
            Pack::open(&pack_path).unwrap().paths().collect::<Vec<_>>()
        );
    }
}
