use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

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

    /// Attract-demo reel ordinal for `--ui demo`: the index into the pack's
    /// sorted `demo/<hex>.dat` entries (0 = first reel)
    #[arg(long, default_value_t = 0, value_name = "N", requires = "pack")]
    demo: u8,

    /// UI screen to boot straight into instead of a room:
    /// `title`, `select`, `game`, `demo`, `menu`, `box`, `debug`, `file`, `map`,
    /// `view`, `save`, `load`, `death`, `result`, `epilogue` or `font`.
    /// `--ui game` also accepts `--room` to boot the new game straight into
    /// that room through the session flow (the oracle anchor for room-specific
    /// differentials); `--ui demo` plays the `--demo` attract reel.
    #[arg(long, value_name = "SCREEN", requires = "pack")]
    ui: Option<String>,

    /// Directory holding save files (default: `<config dir>/save`, or
    /// `save_dir` in `arklay.toml`)
    #[arg(long, value_name = "DIR", requires = "pack")]
    save_dir: Option<PathBuf>,

    /// Load the save block (`savedatN.dat`) at FILE and boot its saved game
    /// immediately instead of starting a new one: the saved stage, room,
    /// character, position, health, inventory and flags drive the session.
    /// Use it with `--ui game --load FILE` for the oracle trace, or alone for
    /// the interactive continue
    #[arg(
        long,
        value_name = "FILE",
        requires = "pack",
        conflicts_with_all = ["room", "fmv", "ending"]
    )]
    load: Option<PathBuf>,

    /// Run this Lua chunk once on the freshly booted session, before its first
    /// tick, to author a test state. The chunk receives the game state as its
    /// first argument (`local api = ...`); this is the oracle/authoring seam,
    /// not a mod hook
    #[arg(
        long,
        value_name = "FILE",
        requires = "pack",
        conflicts_with_all = ["fmv", "ending"]
    )]
    setup_script: Option<PathBuf>,

    /// Write the booted session's save block to FILE and exit before the first
    /// tick. The deterministic save generator for the oracle: pair it with
    /// `--setup-script` for an authored state or use it bare for the new-game
    /// state. This is the oracle/authoring seam, not a mod hook
    #[arg(
        long,
        value_name = "FILE",
        requires = "pack",
        conflicts_with_all = ["fmv", "ending"]
    )]
    save_out: Option<PathBuf>,

    /// Play one film id (0-28) standalone and exit; with `--capture` the
    /// frame after `--ticks` fixed ticks is written headlessly
    #[arg(
        long,
        value_name = "ID",
        requires = "pack",
        conflicts_with_all = ["room", "ui", "ending"]
    )]
    fmv: Option<u8>,

    /// Play ending row `ID` (1-7): its film chain, the RESULT screen, the
    /// rocket-launcher epilogue (endings 6/7) and the next-cycle save, then the
    /// title. With `--capture` the frame after `--ticks` fixed ticks is
    /// written headlessly.
    #[arg(
        long,
        value_name = "ID",
        requires = "pack",
        conflicts_with_all = ["room", "ui"],
        value_parser = clap::value_parser!(u8).range(1..=7)
    )]
    ending: Option<u8>,

    /// Character for the `--ui game`/`--ui death` boot, the `--fmv` prologue
    /// cut and the `--ending` chain
    /// (0 Chris, 1 Jill)
    #[arg(
        long,
        value_name = "N",
        default_value_t = 0,
        value_parser = clap::value_parser!(u8).range(0..=1)
    )]
    character: u8,

    /// Language tag for the displayed strings, e.g. `en-GB`; the default is
    /// the OS locale, then `en-US`
    #[arg(long, value_name = "TAG", requires = "pack")]
    language: Option<String>,

    /// Mod pack layers to apply, repeatable (ignored with `--no-mods`)
    #[arg(long = "mod", value_name = "PATH", requires = "pack")]
    mods: Vec<PathBuf>,

    /// Accepted for compatibility; the port-only debug room-select overlay
    /// (F1 while playing) is always enabled and this flag no longer changes
    /// anything
    #[arg(long, requires = "pack")]
    debug_menu: bool,

    /// Ignore the `--mod` layers and the sibling `mods/` directory
    #[arg(long, requires = "pack")]
    no_mods: bool,

    /// Enable Classic REbirth's quick turn for this run: while walking
    /// backward, RUN turns the player 180 degrees in place over eight ticks.
    /// Overrides `[game] quick_turn` in arklay.toml
    #[arg(long, requires = "pack", overrides_with = "no_quick_turn")]
    quick_turn: bool,

    /// Disable the quick turn for this run, whatever `[game] quick_turn` says
    #[arg(long, requires = "pack", overrides_with = "quick_turn")]
    no_quick_turn: bool,

    /// Enable Classic REbirth's tactical reload for this run: while a gun is
    /// aimed, holding RUN reloads it when the clip is not full and the
    /// matching ammunition is held. Overrides `[game] tactical_reload`
    #[arg(
        long,
        requires = "pack",
        conflicts_with_all = ["no_tactical_reload", "fmv"]
    )]
    tactical_reload: bool,

    /// Disable the tactical reload for this run, whatever
    /// `[game] tactical_reload` says
    #[arg(long, requires = "pack", conflicts_with = "fmv")]
    no_tactical_reload: bool,

    /// Render one frame to a file and exit (headless testing)
    #[arg(long, value_name = "FILE", requires = "pack")]
    capture: Option<PathBuf>,

    /// Run N fixed 30 Hz ticks before a capture frame is drawn, so scripted
    /// NPC scenes and `--fmv` frames can be captured deterministically and
    /// audio-free; with `--ui game` and `--trace` it runs the oracle ticks
    #[arg(long, value_name = "N", default_value_t = 0)]
    ticks: u32,

    /// Write the oracle trace v1 (one line per fixed tick) of a `--room` or
    /// `--ui game` headless run to FILE
    #[arg(long, value_name = "FILE", requires = "pack")]
    trace: Option<PathBuf>,

    /// Scripted per-tick input playback for a `--ui game` oracle run: one
    /// `<tick> <button>...` entry per line (`up`, `down`, `left`, `right`,
    /// `confirm`, `cancel`, `run`, `aim`, `fire`, `start`, `page_left`,
    /// `page_right`). Requires `--trace`.
    #[arg(long, value_name = "FILE", requires = "pack")]
    input: Option<PathBuf>,

    /// Write a frame BMP every N traced ticks; requires `--capture-dir` and
    /// `--trace`
    #[arg(
        long,
        value_name = "N",
        requires = "pack",
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    capture_every: Option<u32>,

    /// Directory the `--capture-every` frames are written to as `f%05d.bmp`
    #[arg(long, value_name = "DIR", requires = "pack")]
    capture_dir: Option<PathBuf>,

    /// Print a frame-time report after the run. With `--ticks` the run is
    /// headless: per-tick min/avg/p95/max, the load/update/effect/render phase
    /// totals and the entity/effect high-water marks (with `--frame-rate`,
    /// also the draw time of the frames a display at that rate would present
    /// against its per-frame budget); a `--capture` is not needed. Without `--ticks` the room is
    /// played in the window and the actual presented frame rate and draw times
    /// are printed when it closes
    #[arg(long, conflicts_with_all = ["ui", "ending", "fmv"])]
    stats: bool,

    /// Renderer for the interactive window's gameplay scene (default: the
    /// config's `[graphics] renderer`, else software). `gpu` draws the 3D
    /// scene with SDL_GPU at the window's resolution (it needs a Vulkan driver
    /// and falls back to software without one). Captures, traces and
    /// `--stats` always draw in software
    #[arg(long, value_enum, requires = "pack")]
    renderer: Option<RendererArg>,

    /// The `--renderer gpu` scene's resolution as a percentage of the
    /// viewport, e.g. 50, 75, 100, 150 or 200 (default: the config's
    /// `[graphics] render_scale`, else 100)
    #[arg(
        long,
        value_name = "PERCENT",
        requires = "pack",
        value_parser = clap::value_parser!(u32).range(
            i64::from(*arklay::video::RENDER_SCALE_RANGE.start())
                ..=i64::from(*arklay::video::RENDER_SCALE_RANGE.end())
        )
    )]
    render_scale: Option<u32>,

    /// The `--renderer gpu` scene's multisample anti-aliasing: 1, 2, 4 or 8
    /// samples per pixel (default: the config's `[graphics] msaa`, else 1)
    #[arg(
        long,
        value_name = "SAMPLES",
        requires = "pack",
        value_parser = parse_msaa
    )]
    msaa: Option<u32>,

    /// Presented frames per second, 30 to 1000 (default: the config's `[graphics]
    /// frame_rate`, else 30). The simulation always ticks at 30 Hz; a higher
    /// rate draws the scene between ticks
    #[arg(
        long,
        value_name = "FPS",
        requires = "pack",
        value_parser = clap::value_parser!(u32).range(i64::from(arklay::video::MIN_FRAME_RATE)..=i64::from(arklay::video::MAX_FRAME_RATE))
    )]
    frame_rate: Option<u32>,

    /// The presented aspect ratio, `4:3` or a wider `W:H` such as `16:9`
    /// (default: the config's `[graphics] aspect`, except under --trace,
    /// else 4:3). A wider aspect crops the 4:3 frame's rows; captures take
    /// the cropped shape
    #[arg(long, value_name = "W:H", requires = "pack", value_parser = parse_aspect)]
    aspect: Option<arklay::video::Aspect>,

    #[command(subcommand)]
    command: Option<Command>,
}

/// Parse `--aspect`: `W:H` from 4:3 up to 32:9.
fn parse_aspect(value: &str) -> Result<arklay::video::Aspect, String> {
    arklay::video::Aspect::parse(value)
        .ok_or_else(|| format!("expected W:H from 4:3 to 32:9, got `{value}`"))
}

/// The quick-turn setting for this run.
///
/// An explicit `--quick-turn` (or `--no-quick-turn`) wins. Otherwise the
/// config's `[game] quick_turn` applies, except to a scripted oracle run
/// (`--trace`/`--input`), which stays off so its output is unchanged.
fn quick_turn_enabled(explicit: Option<bool>, configured: bool, scripted: bool) -> bool {
    explicit.unwrap_or(configured && !scripted)
}

/// The tactical-reload setting for this run.
///
/// An explicit `--tactical-reload` (or `--no-tactical-reload`) wins.
/// Otherwise the config's `[game] tactical_reload` applies, except to a
/// scripted oracle run (`--trace`/`--input`), which stays off so its output
/// is unchanged.
fn tactical_reload_enabled(explicit: Option<bool>, configured: bool, scripted: bool) -> bool {
    explicit.unwrap_or(configured && !scripted)
}

/// Parse `--msaa`: one of [`arklay::video::MSAA_COUNTS`].
fn parse_msaa(text: &str) -> Result<u32, String> {
    text.parse::<u32>()
        .ok()
        .filter(|samples| arklay::video::MSAA_COUNTS.contains(samples))
        .ok_or_else(|| "expected 1, 2, 4 or 8".to_string())
}

/// The `--renderer` choices.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum RendererArg {
    Software,
    Gpu,
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

        /// Number of worker threads (default: one per available CPU)
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..))]
        jobs: Option<u16>,

        /// Print one line per packed file with its plain and stored size
        #[arg(long)]
        verbose: bool,
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

    /// Print the HD texture-pack file names (`<class>/<hash>.webp`) that
    /// replace the textures in an original game file
    Hash {
        /// Original file: TIM, PIX, PAK, a model or room file with embedded
        /// TIMs (EMD, EMW, IVM, RDT, ...), or BMP/PNG/WebP artwork
        #[arg(value_name = "FILE")]
        file: PathBuf,

        /// Hash as this class instead of the one the file type implies:
        /// bgd, mask, misc, skin or skin2
        #[arg(long, value_name = "CLASS")]
        class: Option<String>,
    },

    /// List every entry of a game pack with its stored size
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
    let mut progress = Progress::new("extract", 1);
    progress.begin("extract", pack.len() as u64);

    let mut bytes = 0u64;
    for entry in pack.entries() {
        let data = pack.read(entry.path())?;
        let destination = out_dir.join(entry.path());
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        arklay::atomic::write(&destination, &data)
            .with_context(|| format!("failed to write {}", destination.display()))?;
        bytes += data.len() as u64;
        progress.advance(entry.path());
    }
    progress.finish();

    println!(
        "extracted {} entries, {bytes} bytes to {} in {}",
        pack.len(),
        out_dir.display(),
        format_duration(progress.elapsed())
    );
    Ok(())
}

/// Print the HD texture-pack file name of every texture in an original file,
/// e.g. `arklay hash STAGE1/RC1000.pak` -> `bgd/DD17F52F.webp`.
///
/// The hash is taken over the texels as the game uploads them, so the file is
/// decoded the way its loader decodes it: room backgrounds and masks are
/// bit-replicated, everything else is shifted, and a container is scanned for
/// every embedded TIM and CLUT row. See [`arklay::hash`].
fn hash_file(path: &Path, class: Option<&str>) -> Result<()> {
    use arklay::hash::{Class, Expand, Found, Surface};

    let data = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_uppercase();
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let forced = class.map(Class::parse).transpose()?;
    let whole = |surface: Surface| {
        vec![Found {
            offset: 0,
            clut: None,
            surface,
        }]
    };
    // A full-screen 16bpp image is a background; other textures default by
    // the kind of file that carries them.
    let screen = |surface: &Surface| surface.width >= 320 && surface.height >= 240;
    let (found, implied) = match extension.as_str() {
        "pak" => {
            let tim = arklay::lzw::decode(&data)
                .with_context(|| format!("failed to LZW-decode {}", path.display()))?;
            let mask = name.starts_with("OSP");
            let mut found = arklay::hash::scan_tims(&tim, Expand::Replicate, mask);
            found.retain(|found| found.offset == 0);
            (found, if mask { Class::Mask } else { Class::Bgd })
        }
        "pix" => {
            // `DATA/RC1121.PIX` is a room camera and loads like one.
            let expand = if name.starts_with("RC") {
                Expand::Replicate
            } else {
                Expand::Shift
            };
            (whole(arklay::hash::pix_surface(&data, expand)?), Class::Bgd)
        }
        "bmp" | "png" | "webp" => {
            let image = arklay::assets::decode_image_bytes(&extension, &data)
                .with_context(|| format!("failed to decode {}", path.display()))?;
            let surface = Surface::from_rgba(&image);
            let implied = if screen(&surface) {
                Class::Bgd
            } else {
                Class::Misc
            };
            (whole(surface), implied)
        }
        "emd" | "emw" | "ivm" | "plw" | "tmd" => (
            arklay::hash::scan_tims(&data, Expand::Shift, false),
            Class::Skin,
        ),
        "rdt" => (
            arklay::hash::scan_tims(&data, Expand::Shift, false),
            Class::Skin2,
        ),
        _ => (
            arklay::hash::scan_tims(&data, Expand::Shift, false),
            Class::Misc,
        ),
    };
    if found.is_empty() {
        bail!("{} contains no texture", path.display());
    }
    let several = found.len() > 1;
    for texture in &found {
        let class = forced.unwrap_or(if implied == Class::Misc && screen(&texture.surface) {
            Class::Bgd
        } else {
            implied
        });
        let hash = arklay::hash::format_hash(texture.surface.hash(class));
        let mut line = format!("{}/{hash}.webp", class.name());
        if several {
            line.push_str(&format!(
                "  # TIM at 0x{:X}, {}x{}",
                texture.offset, texture.surface.width, texture.surface.height
            ));
            if let Some(clut) = texture.clut {
                line.push_str(&format!(", CLUT {clut}"));
            }
        }
        println!("{line}");
    }
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
            jobs,
            verbose,
        }) => arklay::convert::convert_game_with_options(
            &root,
            &out,
            jobs.map_or(0, usize::from),
            verbose,
        ),
        Some(Command::Extract { pack, out, mods }) => extract_pack(&pack, &out, &mods),
        Some(Command::Hash { file, class }) => hash_file(&file, class.as_deref()),
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
            let tracing = cli.trace.is_some();
            let (config, warnings) = arklay::config::Config::load_default();
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
            // `--quick-turn`/`--no-quick-turn` override the config. Scripted
            // oracle runs force it off unless the user asked for it explicitly,
            // so existing traces and oracle output stay byte-identical.
            let explicit_quick_turn = cli
                .quick_turn
                .then_some(true)
                .or_else(|| cli.no_quick_turn.then_some(false));
            let explicit_tactical_reload = cli
                .tactical_reload
                .then_some(true)
                .or_else(|| cli.no_tactical_reload.then_some(false));
            let player_options = arklay::player::Options {
                quick_turn: quick_turn_enabled(
                    explicit_quick_turn,
                    config.game.quick_turn,
                    tracing || cli.input.is_some(),
                ),
                tactical_reload: tactical_reload_enabled(
                    explicit_tactical_reload,
                    config.game.tactical_reload,
                    tracing || cli.input.is_some(),
                ),
            };
            let mut renderer = config.graphics;
            if let Some(kind) = cli.renderer {
                renderer.renderer = match kind {
                    RendererArg::Software => arklay::video::RendererKind::Software,
                    RendererArg::Gpu => arklay::video::RendererKind::Gpu,
                };
            }
            renderer.render_scale = cli.render_scale.unwrap_or(renderer.render_scale);
            renderer.msaa = cli.msaa.unwrap_or(renderer.msaa);
            renderer.frame_rate = cli.frame_rate.unwrap_or(renderer.frame_rate);
            // A trace compares against the 4:3 reference, so only an explicit
            // `--aspect` changes its shape.
            if tracing {
                renderer.aspect = arklay::video::Aspect::STANDARD;
            }
            renderer.aspect = cli.aspect.unwrap_or(renderer.aspect);
            arklay::video::set_aspect(renderer.aspect);
            // An app-level `--ui` boot plays a scripted `--input` without a
            // trace: its frames are the screens' regression captures.
            let app_script = cli.input.is_some()
                && !tracing
                && cli.capture_dir.is_some()
                && cli
                    .ui
                    .as_deref()
                    .is_some_and(|screen| !matches!(screen, "game" | "demo" | "font"));
            if let Some(screen) = &cli.ui
                && cli.ticks > 0
                && ((screen != "game" && screen != "demo") || !tracing)
                && !app_script
            {
                bail!(
                    "--ticks with --ui is only supported for `--ui game` with --trace, or an \
                     app screen with --input and --capture-dir"
                );
            }
            if cli.ticks > 0 && cli.capture.is_none() && !cli.stats && !tracing && !app_script {
                bail!("--ticks requires --capture (or --stats)");
            }
            if cli.stats && cli.room.is_none() {
                bail!("--stats requires --room");
            }
            if cli.capture_every.is_some() && cli.capture_dir.is_none() {
                bail!("--capture-every requires --capture-dir");
            }
            if cli.capture_every.is_some() && !tracing && !app_script {
                bail!("--capture-every requires --trace");
            }
            if cli.capture_every.is_some() && cli.ticks == 0 {
                bail!("--capture-every requires --ticks > 0");
            }
            if cli.capture_dir.is_some() && cli.capture_every.is_none() {
                bail!("--capture-dir requires --capture-every");
            }
            // `--load` without `--ui` defaults to the game boot, so it is a
            // valid trace target like `--ui game`/`--ui demo`.
            if tracing
                && cli.room.is_none()
                && !matches!(cli.ui.as_deref(), Some("game" | "demo"))
                && (cli.load.is_none() || cli.ui.is_some())
            {
                bail!("--trace is only supported for --room or --ui game/demo");
            }
            if cli.input.is_some() && !tracing && !app_script {
                bail!("--input requires --trace (or an app --ui screen with --capture-dir)");
            }
            if (cli.setup_script.is_some() || cli.save_out.is_some())
                && cli.ui.is_none()
                && cli.room.is_none()
                && cli.load.is_none()
            {
                bail!("--setup-script/--save-out require --room, --load or a --ui boot");
            }
            let input_script = match cli.input.as_deref() {
                Some(path) => Some(arklay::engine::InputScript::from_file(path)?),
                None => None,
            };
            let language = cli.language.clone().or_else(|| config.language.clone());
            let mods = arklay::engine::ModOptions {
                explicit: cli.mods.clone(),
                no_mods: cli.no_mods,
                layers: arklay::pack::LayerOptions {
                    enabled: config.mods.enabled.clone(),
                    disabled: config.mods.disabled.clone(),
                    order: config.mods.order.clone(),
                },
                global_dir: Some(arklay::config::Config::default_mods_dir()),
            };
            let trace_options = arklay::trace::TraceOptions {
                path: cli.trace.as_deref(),
                capture_every: cli.capture_every.unwrap_or(0),
                capture_dir: cli.capture_dir.as_deref(),
                ticks: cli.ticks,
                input: input_script.as_ref(),
            };
            let authoring = arklay::engine::Authoring {
                load: cli.load.as_deref(),
                setup_script: cli.setup_script.as_deref(),
                save_out: cli.save_out.as_deref(),
                renderer,
                stats_frame_rate: cli.frame_rate,
            };
            let save_dir = cli
                .save_dir
                .clone()
                .or_else(|| config.save_dir.clone())
                .unwrap_or_else(arklay::config::Config::default_save_dir);
            if let Some(id) = cli.ending {
                return arklay::engine::run_ending(
                    &pack,
                    id,
                    cli.character,
                    cli.capture.as_deref(),
                    cli.ticks,
                    &mods,
                    renderer,
                    player_options,
                );
            }
            if let Some(id) = cli.fmv {
                return arklay::engine::run_fmv(
                    &pack,
                    id,
                    cli.character,
                    cli.capture.as_deref(),
                    cli.ticks,
                    &mods,
                    renderer,
                );
            }
            if let Some(screen) = cli.ui {
                // `--ui game --room <id>` boots the new game straight into the
                // room through the session flow, the anchor the reference's
                // test-only room boot mirrors.
                let room = cli
                    .room
                    .as_deref()
                    .map(|room| arklay::state::RoomId::from_room_and_player(room, cli.player))
                    .transpose()?;
                if room.is_some() && screen != "game" {
                    bail!("--room with --ui is only supported for `--ui game`");
                }
                return arklay::engine::run_ui_with_mods(
                    &pack,
                    &screen,
                    cli.capture.as_deref(),
                    &save_dir,
                    cli.character,
                    room,
                    cli.demo,
                    &mods,
                    cli.debug_menu,
                    language.as_deref(),
                    player_options,
                    trace_options,
                    authoring,
                );
            }
            // `--load` without `--ui` is the interactive continue: the same
            // game boot `--ui game --load` drives headlessly.
            if cli.load.is_some() {
                return arklay::engine::run_ui_with_mods(
                    &pack,
                    "game",
                    cli.capture.as_deref(),
                    &save_dir,
                    cli.character,
                    None,
                    cli.demo,
                    &mods,
                    cli.debug_menu,
                    language.as_deref(),
                    player_options,
                    trace_options,
                    authoring,
                );
            }
            if let Some(room) = cli.room {
                let id = arklay::state::RoomId::from_room_and_player(&room, cli.player)?;
                return arklay::engine::run_with_options(
                    &pack,
                    id,
                    cli.capture.as_deref(),
                    cli.ticks,
                    &mods,
                    cli.stats,
                    cli.debug_menu,
                    language.as_deref(),
                    player_options,
                    trace_options,
                    authoring,
                );
            }
            // No room and no `--ui`: boot the app root, whose interactive run
            // plays the logos, the title opening and the select prologue. A
            // capture still boots the title directly and plays no films.
            arklay::engine::run_root(
                &pack,
                cli.capture.as_deref(),
                &save_dir,
                &mods,
                cli.debug_menu,
                language.as_deref(),
                renderer,
                player_options,
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
    fn the_quick_turn_override_beats_the_config_but_not_a_trace() {
        // No explicit flag: the config applies to gameplay runs, while a
        // scripted oracle run keeps the reference default.
        assert!(!quick_turn_enabled(None, false, false));
        assert!(quick_turn_enabled(None, true, false));
        assert!(!quick_turn_enabled(None, true, true));
        // An explicit flag wins either way, including over a trace.
        assert!(quick_turn_enabled(Some(true), false, false));
        assert!(quick_turn_enabled(Some(true), false, true));
        assert!(!quick_turn_enabled(Some(false), true, false));
        assert!(!quick_turn_enabled(Some(false), true, true));
    }

    #[test]
    fn the_cli_accepts_the_tactical_reload_flags() {
        let cli = Cli::try_parse_from(["arklay", "game.akpak", "--tactical-reload"]).unwrap();
        assert!(cli.tactical_reload);
        assert!(!cli.no_tactical_reload);
        let cli = Cli::try_parse_from(["arklay", "game.akpak", "--no-tactical-reload"]).unwrap();
        assert!(cli.no_tactical_reload);
        assert!(!cli.tactical_reload);
        // The two are exclusive overrides.
        assert!(
            Cli::try_parse_from([
                "arklay",
                "game.akpak",
                "--tactical-reload",
                "--no-tactical-reload"
            ])
            .is_err()
        );
        // A standalone film has no gameplay to reload.
        assert!(
            Cli::try_parse_from(["arklay", "game.akpak", "--fmv", "0", "--tactical-reload"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["arklay", "game.akpak", "--fmv", "0", "--no-tactical-reload"])
                .is_err()
        );
    }

    #[test]
    fn the_tactical_reload_override_beats_the_config_but_not_a_trace() {
        // No explicit flag: the config applies to gameplay runs, while a
        // scripted oracle run keeps the reference default.
        assert!(!tactical_reload_enabled(None, false, false));
        assert!(tactical_reload_enabled(None, true, false));
        assert!(!tactical_reload_enabled(None, true, true));
        // An explicit flag wins either way, including over a trace.
        assert!(tactical_reload_enabled(Some(true), false, false));
        assert!(tactical_reload_enabled(Some(true), false, true));
        assert!(!tactical_reload_enabled(Some(false), true, false));
        assert!(!tactical_reload_enabled(Some(false), true, true));
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
            assert_eq!(extracted, pack.read(entry.path()).unwrap().as_ref());
        }
        assert_eq!(
            fs::read(out.join("room/1001.rdt")).unwrap(),
            pack.read("room/1001.rdt").unwrap().as_ref()
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
            assert_eq!(extracted, pack.read(sample).unwrap().as_ref(), "{sample}");
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
    fn the_cli_accepts_the_oracle_authoring_flags() {
        let cli = Cli::try_parse_from([
            "arklay",
            "game.akpak",
            "--ui",
            "game",
            "--load",
            "savedat1.dat",
            "--setup-script",
            "setup.lua",
            "--save-out",
            "out.dat",
        ])
        .unwrap();
        assert_eq!(cli.load.as_deref(), Some(Path::new("savedat1.dat")));
        assert_eq!(cli.setup_script.as_deref(), Some(Path::new("setup.lua")));
        assert_eq!(cli.save_out.as_deref(), Some(Path::new("out.dat")));

        // `--load` is the continue boot: it cannot combine with a new-game
        // room or the film/ending boots.
        assert!(
            Cli::try_parse_from(["arklay", "game.akpak", "--load", "s.dat", "--room", "101"])
                .is_err()
        );
    }

    #[test]
    fn the_cli_accepts_the_hash_subcommand() {
        let cli = Cli::try_parse_from(["arklay", "hash", "RC1000.pak", "--class", "bgd"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Hash { file, class: Some(class) })
                if file.as_path() == Path::new("RC1000.pak") && class == "bgd"
        ));
    }

    #[test]
    fn the_cli_accepts_the_debug_menu_flag() {
        let cli =
            Cli::try_parse_from(["arklay", "game.akpak", "--room", "100", "--debug-menu"]).unwrap();
        assert!(cli.debug_menu);

        let cli = Cli::try_parse_from(["arklay", "game.akpak", "--room", "100"]).unwrap();
        assert!(!cli.debug_menu);
    }

    #[test]
    fn the_cli_accepts_verbose_conversions() {
        let cli = Cli::try_parse_from([
            "arklay",
            "convert-game",
            "re1-install",
            "--out",
            "re1.akpak",
            "--verbose",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::ConvertGame { verbose: true, .. })
        ));

        let cli = Cli::try_parse_from([
            "arklay",
            "convert-game",
            "re1-install",
            "--out",
            "re1.akpak",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::ConvertGame { verbose: false, .. })
        ));
    }

    #[test]
    fn runtime_layer_set_discovers_directory_game_packs_and_per_user_mods() {
        use arklay::manifest;
        use arklay::pack::LayerOptions;

        let dir = TempDir::new("runtime-global-mods");
        let game_dir = dir.path.join("game");
        fs::create_dir_all(game_dir.join("room")).unwrap();
        fs::write(
            game_dir.join(manifest::ENTRY),
            manifest::Manifest::base("re1").render(),
        )
        .unwrap();
        fs::write(game_dir.join("room/1000.rdt"), b"base").unwrap();

        let global = dir.path.join("config/mods");
        let one = global.join("one.akpak");
        let two = global.join("two");
        let mod_manifest = |id: &str| {
            manifest::Manifest {
                kind: manifest::PackKind::Mod,
                base: Some("re1".to_string()),
                ..manifest::Manifest::base(id)
            }
            .render()
            .into_bytes()
        };
        let mut writer = PackWriter::new();
        writer.add(manifest::ENTRY, mod_manifest("one")).unwrap();
        writer.add("room/1000.rdt", b"one".to_vec()).unwrap();
        fs::create_dir_all(&global).unwrap();
        writer.write(&one).unwrap();
        fs::create_dir_all(two.join("room")).unwrap();
        fs::write(two.join(manifest::ENTRY), mod_manifest("two")).unwrap();
        fs::write(two.join("room/1000.rdt"), b"two").unwrap();

        // A directory game pack plus the per-user mods directory: only the
        // enabled mod applies.
        let options = arklay::engine::ModOptions {
            global_dir: Some(global.clone()),
            layers: LayerOptions {
                enabled: Some(vec!["one".into()]),
                ..LayerOptions::default()
            },
            ..arklay::engine::ModOptions::default()
        };
        let pack = arklay::engine::open_game_pack(&game_dir, &options, None).unwrap();
        assert!(pack.is_layered());
        assert_eq!(&*pack.read("room/1000.rdt").unwrap(), b"one");

        // Config order names the later layer: `two` wins over `one`.
        let options = arklay::engine::ModOptions {
            global_dir: Some(global),
            layers: LayerOptions {
                order: vec!["one".into(), "two".into()],
                ..LayerOptions::default()
            },
            ..arklay::engine::ModOptions::default()
        };
        let pack = arklay::engine::open_game_pack(&game_dir, &options, None).unwrap();
        assert_eq!(&*pack.read("room/1000.rdt").unwrap(), b"two");

        // `--no-mods` ignores the per-user directory too.
        let options = arklay::engine::ModOptions {
            no_mods: true,
            global_dir: Some(dir.path.join("config/mods")),
            ..arklay::engine::ModOptions::default()
        };
        let vanilla = arklay::engine::open_game_pack(&game_dir, &options, None).unwrap();
        assert!(!vanilla.is_layered());
        assert_eq!(&*vanilla.read("room/1000.rdt").unwrap(), b"base");
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
        let discovered = arklay::engine::open_game_pack(
            &pack_path,
            &arklay::engine::ModOptions::default(),
            None,
        )
        .unwrap();
        assert!(discovered.is_layered());
        assert_eq!(discovered.read("room/1000.rdt").unwrap().as_ref(), b"layer");

        // `--no-mods` ignores both the discovery and an explicit `--mod`.
        let vanilla = arklay::engine::open_game_pack(
            &pack_path,
            &arklay::engine::ModOptions {
                explicit: vec![mod_path],
                no_mods: true,
                ..arklay::engine::ModOptions::default()
            },
            None,
        )
        .unwrap();
        assert!(!vanilla.is_layered());
        assert_eq!(vanilla.read("room/1000.rdt").unwrap().as_ref(), b"base");
        assert_eq!(
            vanilla.paths().collect::<Vec<_>>(),
            Pack::open(&pack_path).unwrap().paths().collect::<Vec<_>>()
        );
    }
}
