//! xzip - the command line archiver.
//!
//! Conventions, borrowed from the tools people already know:
//!   - subcommands with short aliases (a/x/l/t/d/i), like 7z and zip
//!   - -C to change directory, --strip-components, --exclude, like tar
//!   - -l 0..9 compression levels, -T threads, like xz and zstd
//!   - results on stdout, diagnostics on stderr; --json for scripts
//!   - progress only on a terminal; NO_COLOR, --color, -q all respected
//!   - exit codes: 0 ok, 1 ok with warnings, 2 usage, 3 integrity failure,
//!     4 I/O or other error, 130 interrupted
//!
//! Copyright (c) Clinton Turner.

use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anstyle::{AnsiColor, Style};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use globset::{Glob, GlobSet, GlobSetBuilder};
use indicatif::{ProgressBar, ProgressStyle};
use serde_json::json;

use xzip_core::archive::{self, Archive, CreateOptions, Entry, ExtractOptions, Overwrite, Skipped};
use xzip_core::{format_size, Error, Settings, Strategy};

const EXIT_OK: i32 = 0;
const EXIT_WARN: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_INTEGRITY: i32 = 3;
const EXIT_ERROR: i32 = 4;
const EXIT_INTERRUPTED: i32 = 130;

#[derive(Parser)]
#[command(
    name = "xzip",
    version,
    about = "xzip archiver - verified .xzip archives with Fast-LZMA2 compression",
    long_about = "Create, extract, list and verify .xzip archives.\n\nEvery archive is hashed before it is parsed and every file is hashed before it is written. \
Paths are validated so an archive from anywhere extracts safely anywhere.",
    after_help = "Examples:\n  xzip a backup.xzip ~/Documents -l 7\n  xzip x backup.xzip -C /tmp/restore\n  xzip l backup.xzip --long\n  xzip t backup.xzip\n  tar cf - src | xzip a - --files-from - > out.xzip   (archive a list from stdin)",
    author
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Only errors on stderr
    #[arg(short, long, global = true)]
    quiet: bool,
    /// More detail (repeatable)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    /// Machine-readable JSON on stdout
    #[arg(long, global = true)]
    json: bool,
    /// Never show a progress bar
    #[arg(long, global = true)]
    no_progress: bool,
    /// Colour output
    #[arg(long, global = true, value_enum, default_value_t = ColorMode::Auto)]
    color: ColorMode,
    /// Worker threads (default: all cores)
    #[arg(short = 'T', long, global = true, env = "XZIP_THREADS")]
    threads: Option<usize>,
    /// Assume yes to questions
    #[arg(short = 'y', long, global = true)]
    yes: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create an archive, or add to one that exists
    #[command(visible_aliases = ["a", "c", "create"])]
    Add(AddArgs),
    /// Extract files (all, or the ones matching PATHS/globs)
    #[command(visible_aliases = ["x", "e"])]
    Extract(ExtractArgs),
    /// List contents
    #[command(visible_aliases = ["l", "ls"])]
    List(ListArgs),
    /// Verify the archive and every file in it
    #[command(visible_aliases = ["t", "verify"])]
    Test(ArchiveArg),
    /// Remove entries (the archive is rebuilt without recompressing)
    #[command(visible_aliases = ["d", "rm"])]
    Delete(DeleteArgs),
    /// Show archive details
    #[command(visible_aliases = ["i"])]
    Info(ArchiveArg),
    /// Print shell completions (bash, zsh, fish, powershell, elvish)
    Completions { shell: clap_complete::Shell },
    /// Print the manual page (roff)
    Man,
}

#[derive(Args)]
struct ArchiveArg {
    /// Archive file ("-" reads stdin)
    archive: PathBuf,
}

#[derive(Args)]
struct AddArgs {
    /// Archive to create or add to ("-" writes to stdout)
    archive: PathBuf,
    /// Files and directories to add (directories are recursive)
    paths: Vec<PathBuf>,
    /// Compression level 0 (fastest) to 9 (smallest)
    #[arg(short = 'l', long, default_value_t = 6, value_parser = clap::value_parser!(u32).range(0..=9))]
    level: u32,
    /// Change to DIR before adding (paths are then relative to it)
    #[arg(short = 'C', long = "directory", value_name = "DIR")]
    directory: Option<PathBuf>,
    /// Skip paths matching GLOB (repeatable; matches names, paths or parents)
    #[arg(short = 'x', long = "exclude", value_name = "GLOB")]
    exclude: Vec<String>,
    /// Only add paths matching GLOB (repeatable)
    #[arg(long = "include", value_name = "GLOB")]
    include: Vec<String>,
    /// Read more paths from FILE, one per line ("-" for stdin)
    #[arg(long = "files-from", value_name = "FILE")]
    files_from: Option<PathBuf>,
    /// Follow symbolic links instead of skipping them
    #[arg(long)]
    follow_symlinks: bool,
    /// Show what would be added without writing anything
    #[arg(long)]
    dry_run: bool,
    /// Parser: fast (greedy) or opt (optimal); default comes from the level
    #[arg(long, value_enum)]
    strategy: Option<StrategyArg>,
    /// Dictionary size, e.g. 16M (default comes from the level)
    #[arg(long, value_name = "SIZE")]
    dict: Option<String>,
    /// Fix lc/lp/pb instead of picking them by trial
    #[arg(long)]
    no_auto_props: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum StrategyArg {
    Fast,
    Opt,
}

#[derive(Args)]
struct ExtractArgs {
    /// Archive file ("-" reads stdin)
    archive: PathBuf,
    /// Entries to extract: exact paths, folders, or globs (default: everything)
    paths: Vec<String>,
    /// Destination directory (created if missing)
    #[arg(
        short = 'C',
        long = "directory",
        value_name = "DIR",
        default_value = "."
    )]
    directory: PathBuf,
    /// Replace existing files (default: keep them and report)
    #[arg(short = 'f', long)]
    overwrite: bool,
    /// Drop the first N path components on extraction
    #[arg(long, value_name = "N", default_value_t = 0)]
    strip_components: usize,
    /// Largest single file to extract, e.g. 4G
    #[arg(long, value_name = "SIZE")]
    max_file_size: Option<String>,
    /// Do not check free disk space first
    #[arg(long)]
    no_space_check: bool,
    /// List what would be extracted without writing anything
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct ListArgs {
    /// Archive file ("-" reads stdin)
    archive: PathBuf,
    /// Only entries matching these paths or globs
    paths: Vec<String>,
    /// Long format: packed size, hashes, flags
    #[arg(short = 'l', long)]
    long: bool,
    /// Indented tree instead of full paths
    #[arg(long)]
    tree: bool,
}

#[derive(Args)]
struct DeleteArgs {
    archive: PathBuf,
    /// Entries to remove: exact paths, folders, or globs
    #[arg(required = true)]
    paths: Vec<String>,
}

struct Ui {
    quiet: bool,
    verbose: u8,
    json: bool,
    progress: bool,
    cancel: Arc<AtomicBool>,
    warnings: usize,
}

impl Ui {
    fn note(&self, msg: &str) {
        if !self.quiet && !self.json {
            eprintln!("{msg}");
        }
    }

    fn warn(&mut self, msg: &str) {
        self.warnings += 1;
        if !self.json {
            let s = Style::new().fg_color(Some(AnsiColor::Yellow.into())).bold();
            anstream::eprintln!("{s}warning:{s:#} {msg}");
        }
    }

    fn bar(&self, total: u64, label: &str) -> ProgressBar {
        if !self.progress {
            return ProgressBar::hidden();
        }
        let pb = ProgressBar::new(total.max(1));
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg:<28!} [{bar:30.cyan/dim}] {bytes}/{total_bytes} {bytes_per_sec} ({eta})")
                .unwrap()
                .progress_chars("━╸─"),
        );
        pb.set_message(label.to_string());
        pb
    }
}

fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim().to_ascii_lowercase().replace(',', "");
    let (num, mult) = if let Some(n) = t
        .strip_suffix("gib")
        .or(t.strip_suffix("gb"))
        .or(t.strip_suffix('g'))
    {
        (n, 1u64 << 30)
    } else if let Some(n) = t
        .strip_suffix("mib")
        .or(t.strip_suffix("mb"))
        .or(t.strip_suffix('m'))
    {
        (n, 1 << 20)
    } else if let Some(n) = t
        .strip_suffix("kib")
        .or(t.strip_suffix("kb"))
        .or(t.strip_suffix('k'))
    {
        (n, 1 << 10)
    } else {
        (t.as_str(), 1)
    };
    num.trim()
        .parse::<f64>()
        .map(|v| (v * mult as f64) as u64)
        .map_err(|_| format!("not a size: {s:?}"))
}

fn build_globs(patterns: &[String]) -> Result<Option<GlobSet>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).map_err(|e| e.to_string())?);
    }
    b.build().map(Some).map_err(|e| e.to_string())
}

/// A pattern matches an archive path if it matches the whole path, any parent path
/// or any single component. "node_modules" therefore excludes every node_modules
/// folder at any depth, "*.log" every log file, "src/**" a whole tree.
fn path_matches(set: &GlobSet, path: &str) -> bool {
    if set.is_match(path) {
        return true;
    }
    let comps: Vec<&str> = path.split('/').collect();
    for i in 1..comps.len() {
        if set.is_match(comps[..i].join("/")) {
            return true;
        }
    }
    comps.iter().any(|c| set.is_match(c))
}

/// Entry selection for extract/list/delete: exact path, folder (and its contents), or glob.
fn select_entries<'a>(archive: &'a Archive, patterns: &[String]) -> Result<Vec<&'a Entry>, String> {
    if patterns.is_empty() {
        return Ok(archive.entries.iter().collect());
    }
    let set = build_globs(patterns)?.unwrap();
    let mut out: Vec<&Entry> = Vec::new();
    for e in &archive.entries {
        let hit = patterns.iter().any(|p| {
            let p = p.trim_end_matches('/');
            e.path == p || e.path.starts_with(&format!("{p}/"))
        }) || set.is_match(&e.path);
        if hit {
            out.push(e);
        }
    }
    if out.is_empty() {
        return Err(format!(
            "nothing in the archive matches {}",
            patterns.join(", ")
        ));
    }
    Ok(out)
}

fn read_archive(path: &Path) -> Result<Archive, Error> {
    if path.as_os_str() == "-" {
        let mut buf = Vec::new();
        io::stdin().read_to_end(&mut buf).map_err(|e| Error::Io {
            context: "reading stdin".into(),
            source: e,
        })?;
        archive::load_archive(buf, Path::new("<stdin>"))
    } else {
        archive::open_archive(path)
    }
}

fn mtime_string(t: i64) -> String {
    // civil date from days since the epoch (UTC); no timezone database needed
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60
    )
}

fn settings_from(a: &AddArgs) -> Result<Settings, String> {
    let mut s = Settings::level(a.level);
    if let Some(st) = a.strategy {
        s.strategy = match st {
            StrategyArg::Fast => Strategy::Fast,
            StrategyArg::Opt => Strategy::Opt,
        };
    }
    if let Some(d) = &a.dict {
        s.dict_size = parse_size(d)?.min(u32::MAX as u64) as u32;
    }
    if a.no_auto_props {
        s.auto_props = false;
    }
    s.validate().map_err(|e| e.to_string())?;
    Ok(s)
}

fn report_skipped(ui: &mut Ui, skipped: &[Skipped]) {
    for s in skipped {
        ui.warn(&format!("skipped {}: {}", s.path.display(), s.reason));
    }
}

fn cmd_add(ui: &mut Ui, a: AddArgs) -> Result<i32, Error> {
    let settings = settings_from(&a).map_err(Error::Settings)?;
    if let Some(dir) = &a.directory {
        std::env::set_current_dir(dir).map_err(|e| Error::Io {
            context: format!("changing to {}", dir.display()),
            source: e,
        })?;
    }
    let mut paths = a.paths.clone();
    if let Some(ff) = &a.files_from {
        let text = if ff.as_os_str() == "-" {
            let mut s = String::new();
            io::stdin().read_to_string(&mut s).map_err(|e| Error::Io {
                context: "reading stdin".into(),
                source: e,
            })?;
            s
        } else {
            std::fs::read_to_string(ff).map_err(|e| Error::Io {
                context: format!("reading {}", ff.display()),
                source: e,
            })?
        };
        paths.extend(
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(PathBuf::from),
        );
    }
    if paths.is_empty() {
        return Err(Error::Settings(
            "nothing to add: give paths or --files-from".into(),
        ));
    }
    let exclude = build_globs(&a.exclude).map_err(Error::Settings)?;
    let include = build_globs(&a.include).map_err(Error::Settings)?;
    let filter = move |p: &str| -> bool {
        if let Some(x) = &exclude {
            if path_matches(x, p) {
                return false;
            }
        }
        match &include {
            Some(i) => path_matches(i, p) || i.is_match(p),
            None => true,
        }
    };
    let to_stdout = a.archive.as_os_str() == "-";
    let out_path = if to_stdout {
        std::env::temp_dir().join(format!(".xzip-stdout-{}.xzip", std::process::id()))
    } else {
        a.archive.clone()
    };

    if a.dry_run {
        let (items, skipped) = archive::collect_sources(&paths, a.follow_symlinks, &filter);
        report_skipped(ui, &skipped);
        if ui.json {
            println!(
                "{}",
                json!({"dry_run": true, "entries": items.iter().map(|i| json!({"path": i.archive_path, "dir": i.is_dir, "size": i.size})).collect::<Vec<_>>()})
            );
        } else {
            for i in &items {
                println!("{}{}", i.archive_path, if i.is_dir { "/" } else { "" });
            }
            ui.note(&format!(
                "{} entries, {} would be compressed",
                items.len(),
                format_size(items.iter().map(|i| i.size).sum())
            ));
        }
        return Ok(if ui.warnings > 0 { EXIT_WARN } else { EXIT_OK });
    }

    let opts = CreateOptions {
        settings: &settings,
        follow_symlinks: a.follow_symlinks,
        filter: &filter,
    };
    let bar = ui.bar(1, "compressing");
    let cancel = ui.cancel.clone();
    let bar_ref = &bar;
    let progress = move |done: u64, total: u64, path: &str| -> bool {
        bar_ref.set_length(total.max(1));
        bar_ref.set_position(done);
        bar_ref.set_message(shorten(path, 28));
        !cancel.load(Ordering::Relaxed)
    };
    let started = Instant::now();
    let existing = (!to_stdout && out_path.exists())
        .then(|| archive::open_archive(&out_path))
        .transpose()?;
    let (entries, skipped) = match &existing {
        Some(old) => {
            // adding to an archive: new items replace same-named old ones
            let (items, _) = archive::collect_sources(&paths, a.follow_symlinks, &filter);
            let new_keys: std::collections::HashSet<String> = items
                .iter()
                .map(|i| xzip_core::paths::collision_key(&i.archive_path))
                .collect();
            let keep: Vec<&Entry> = old
                .entries
                .iter()
                .filter(|e| !new_keys.contains(&xzip_core::paths::collision_key(&e.path)))
                .collect();
            archive::rebuild_archive(old, &out_path, &keep, &paths, &opts, Some(&progress))?
        }
        None => archive::create_archive(&out_path, &paths, &opts, Some(&progress))?,
    };
    bar.finish_and_clear();
    report_skipped(ui, &skipped);
    let packed = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    if to_stdout {
        let data = std::fs::read(&out_path).map_err(|e| Error::Io {
            context: "reading temp archive".into(),
            source: e,
        })?;
        let _ = std::fs::remove_file(&out_path);
        io::stdout().write_all(&data).map_err(|e| Error::Io {
            context: "writing stdout".into(),
            source: e,
        })?;
    }
    let files = entries.iter().filter(|e| !e.is_dir()).count();
    let size: u64 = entries.iter().map(|e| e.size).sum();
    if ui.json {
        println!(
            "{}",
            json!({"archive": a.archive, "files": files, "folders": entries.len() - files, "size": size, "packed": packed,
            "ratio": if size > 0 { packed as f64 / size as f64 } else { 1.0 }, "seconds": started.elapsed().as_secs_f64(),
            "skipped": skipped.iter().map(|s| json!({"path": s.path, "reason": s.reason})).collect::<Vec<_>>(),
            "lc": settings.lc, "lp": settings.lp, "pb": settings.pb, "level": a.level})
        );
    } else if !to_stdout {
        let pct = if size > 0 {
            packed as f64 / size as f64 * 100.0
        } else {
            0.0
        };
        ui.note(&format!(
            "{}: {files} files, {} folders, {} -> {} ({pct:.1}%) in {:.1}s",
            a.archive.display(),
            entries.len() - files,
            format_size(size),
            format_size(packed),
            started.elapsed().as_secs_f64()
        ));
    }
    Ok(if ui.warnings > 0 { EXIT_WARN } else { EXIT_OK })
}

fn shorten(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        format!("…{}", s.chars().skip(n - max + 1).collect::<String>())
    }
}

fn cmd_extract(ui: &mut Ui, x: ExtractArgs) -> Result<i32, Error> {
    let archive = read_archive(&x.archive)?;
    let selected = select_entries(&archive, &x.paths).map_err(Error::Settings)?;
    let total: u64 = selected
        .iter()
        .filter(|e| !e.is_dir())
        .map(|e| e.size)
        .sum();
    if x.dry_run {
        if ui.json {
            println!(
                "{}",
                json!({"dry_run": true, "destination": x.directory, "entries": selected.iter().map(|e| json!({"path": e.path, "dir": e.is_dir(), "size": e.size})).collect::<Vec<_>>()})
            );
        } else {
            for e in &selected {
                println!("{}{}", e.path, if e.is_dir() { "/" } else { "" });
            }
            ui.note(&format!(
                "{} entries, {} would be written to {}",
                selected.len(),
                format_size(total),
                x.directory.display()
            ));
        }
        return Ok(EXIT_OK);
    }
    let opts = ExtractOptions {
        overwrite: if x.overwrite {
            Overwrite::Replace
        } else {
            Overwrite::Skip
        },
        max_entry_size: x
            .max_file_size
            .as_deref()
            .map(parse_size)
            .transpose()
            .map_err(Error::Settings)?
            .unwrap_or(archive::DEFAULT_MAX_ENTRY_SIZE),
        max_total_size: archive::DEFAULT_MAX_TOTAL_SIZE,
        strip_components: x.strip_components,
        check_disk_space: !x.no_space_check,
    };
    let bar = ui.bar(total, "extracting");
    let cancel = ui.cancel.clone();
    let bar_ref = &bar;
    let progress = move |done: u64, _total: u64, path: &str| -> bool {
        bar_ref.set_position(done);
        bar_ref.set_message(shorten(path, 28));
        !cancel.load(Ordering::Relaxed)
    };
    let started = Instant::now();
    let report = archive.extract(&x.directory, Some(&selected), &opts, Some(&progress))?;
    bar.finish_and_clear();
    if ui.verbose > 0 && !ui.json {
        for p in &report.written {
            println!("{p}");
        }
    }
    for p in &report.skipped {
        ui.warn(&format!("exists, not replaced (use -f): {p}"));
    }
    if ui.json {
        println!(
            "{}",
            json!({"destination": x.directory, "written": report.written.len(), "skipped": report.skipped, "bytes": total, "seconds": started.elapsed().as_secs_f64()})
        );
    } else {
        ui.note(&format!(
            "extracted {} entries ({}) to {} in {:.1}s",
            report.written.len(),
            format_size(total),
            x.directory.display(),
            started.elapsed().as_secs_f64()
        ));
    }
    Ok(if report.skipped.is_empty() {
        EXIT_OK
    } else {
        EXIT_WARN
    })
}

fn cmd_list(ui: &Ui, l: ListArgs) -> Result<i32, Error> {
    let archive = read_archive(&l.archive)?;
    let selected = select_entries(&archive, &l.paths).map_err(Error::Settings)?;
    if ui.json {
        let entries: Vec<_> = selected
            .iter()
            .map(|e| {
                json!({"path": e.path, "dir": e.is_dir(), "size": e.size, "packed": e.data_length, "mtime": e.mtime,
                       "executable": e.executable(), "sha256": if e.is_dir() { String::new() } else { hex(&e.sha_data) }})
            })
            .collect();
        println!(
            "{}",
            json!({"archive": l.archive, "sha256": hex(&archive.archive_hash), "entries": entries})
        );
        return Ok(EXIT_OK);
    }
    let dim = Style::new().dimmed();
    let dir_style = Style::new().fg_color(Some(AnsiColor::Blue.into())).bold();
    if l.long {
        anstream::println!(
            "{dim}{:>10} {:>10} {:>5}  {:16}  {:16}  Name{dim:#}",
            "Size",
            "Packed",
            "Ratio",
            "Modified",
            "SHA-256"
        );
    } else {
        anstream::println!("{dim}{:>10}  {:16}  Name{dim:#}", "Size", "Modified");
    }
    let mut files = 0;
    let mut size = 0u64;
    for e in &selected {
        let name = if l.tree {
            let depth = e.path.matches('/').count();
            format!("{}{}", "  ".repeat(depth), e.name())
        } else {
            e.path.clone()
        };
        if e.is_dir() {
            if l.long {
                anstream::println!(
                    "{:>10} {:>10} {:>5}  {:16}  {:16}  {dir_style}{name}/{dir_style:#}",
                    "<DIR>",
                    "",
                    "",
                    mtime_string(e.mtime),
                    ""
                );
            } else {
                anstream::println!(
                    "{:>10}  {:16}  {dir_style}{name}/{dir_style:#}",
                    "<DIR>",
                    mtime_string(e.mtime)
                );
            }
        } else {
            files += 1;
            size += e.size;
            let ratio = if e.size > 0 {
                format!("{:.0}%", e.data_length as f64 / e.size as f64 * 100.0)
            } else {
                "-".into()
            };
            if l.long {
                let flag = if e.executable() { "*" } else { "" };
                anstream::println!(
                    "{:>10} {:>10} {:>5}  {:16}  {:16}  {name}{flag}",
                    format_size(e.size),
                    format_size(e.data_length),
                    ratio,
                    mtime_string(e.mtime),
                    &hex(&e.sha_data)[..16]
                );
            } else {
                anstream::println!(
                    "{:>10}  {:16}  {name}",
                    format_size(e.size),
                    mtime_string(e.mtime)
                );
            }
        }
    }
    anstream::println!(
        "{dim}{files} files, {} folders, {} uncompressed; archive {} on disk, SHA-256 {}{dim:#}",
        selected.len() - files,
        format_size(size),
        format_size(archive.size()),
        &hex(&archive.archive_hash)[..16]
    );
    Ok(EXIT_OK)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn cmd_test(ui: &Ui, t: ArchiveArg) -> Result<i32, Error> {
    let archive = read_archive(&t.archive)?;
    let total: u64 = archive.entries.iter().map(|e| e.size).sum();
    let bar = ui.bar(total, "verifying");
    let cancel = ui.cancel.clone();
    let bar_ref = &bar;
    let progress = move |done: u64, _t: u64, path: &str| -> bool {
        bar_ref.set_position(done);
        bar_ref.set_message(shorten(path, 28));
        !cancel.load(Ordering::Relaxed)
    };
    let n = archive.verify(Some(&progress))?;
    bar.finish_and_clear();
    if ui.json {
        println!(
            "{}",
            json!({"archive": t.archive, "ok": true, "files": n, "sha256": hex(&archive.archive_hash)})
        );
    } else {
        let ok = Style::new().fg_color(Some(AnsiColor::Green.into())).bold();
        anstream::println!("{ok}OK{ok:#}  {n} files verified: container hash, manifest, every stream and every file's contents match.\nSHA-256 {}", hex(&archive.archive_hash));
    }
    Ok(EXIT_OK)
}

fn cmd_info(ui: &Ui, t: ArchiveArg) -> Result<i32, Error> {
    let archive = read_archive(&t.archive)?;
    let files: Vec<&Entry> = archive.entries.iter().filter(|e| !e.is_dir()).collect();
    let size: u64 = files.iter().map(|e| e.size).sum();
    let packed: u64 = files.iter().map(|e| e.data_length).sum();
    let longest = archive
        .entries
        .iter()
        .map(|e| e.path.len())
        .max()
        .unwrap_or(0);
    if ui.json {
        println!(
            "{}",
            json!({"archive": t.archive, "bytes": archive.size(), "files": files.len(), "folders": archive.entries.len() - files.len(),
            "size": size, "packed": packed, "sha256": hex(&archive.archive_hash), "longest_path": longest, "format_version": archive::VERSION})
        );
    } else {
        println!("Archive:      {}", t.archive.display());
        println!("On disk:      {}", format_size(archive.size()));
        println!("Files:        {}", files.len());
        println!("Folders:      {}", archive.entries.len() - files.len());
        println!("Contents:     {}", format_size(size));
        if size > 0 {
            println!(
                "Packed:       {} ({:.1}%)",
                format_size(packed),
                packed as f64 / size as f64 * 100.0
            );
        }
        println!("SHA-256:      {}", hex(&archive.archive_hash));
        println!("Longest path: {longest} characters (format allows 65535 bytes)");
        println!("Verified:     container hash, header CRC, manifest hash and every path, on open");
    }
    Ok(EXIT_OK)
}

fn cmd_delete(ui: &mut Ui, d: DeleteArgs) -> Result<i32, Error> {
    let archive = read_archive(&d.archive)?;
    let remove = select_entries(&archive, &d.paths).map_err(Error::Settings)?;
    let remove_set: std::collections::HashSet<&str> =
        remove.iter().map(|e| e.path.as_str()).collect();
    let keep: Vec<&Entry> = archive
        .entries
        .iter()
        .filter(|e| !remove_set.contains(e.path.as_str()))
        .collect();
    let settings = Settings::level(6);
    let opts = CreateOptions {
        settings: &settings,
        follow_symlinks: false,
        filter: &|_| true,
    };
    let bar = ui.bar(1, "rebuilding");
    let bar_ref = &bar;
    let progress = move |done: u64, total: u64, _p: &str| -> bool {
        bar_ref.set_length(total.max(1));
        bar_ref.set_position(done);
        true
    };
    archive::rebuild_archive(&archive, &d.archive, &keep, &[], &opts, Some(&progress))?;
    bar.finish_and_clear();
    if ui.json {
        println!(
            "{}",
            json!({"archive": d.archive, "removed": remove.len(), "remaining": keep.len()})
        );
    } else {
        ui.note(&format!(
            "removed {} entries, {} remain",
            remove.len(),
            keep.len()
        ));
    }
    Ok(EXIT_OK)
}

fn main() {
    let cli = Cli::parse();
    match cli.color {
        ColorMode::Always => anstream::ColorChoice::Always.write_global(),
        ColorMode::Never => anstream::ColorChoice::Never.write_global(),
        ColorMode::Auto => {}
    }
    if let Some(n) = cli.threads {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(n.max(1))
            .build_global();
    }
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let c = cancel.clone();
        let _ = ctrlc::set_handler(move || c.store(true, Ordering::Relaxed));
    }
    let mut ui = Ui {
        quiet: cli.quiet,
        verbose: cli.verbose,
        json: cli.json,
        progress: !cli.quiet && !cli.no_progress && !cli.json && io::stderr().is_terminal(),
        cancel,
        warnings: 0,
    };
    let result = match cli.cmd {
        Cmd::Add(a) => cmd_add(&mut ui, a),
        Cmd::Extract(x) => cmd_extract(&mut ui, x),
        Cmd::List(l) => cmd_list(&ui, l),
        Cmd::Test(t) => cmd_test(&ui, t),
        Cmd::Delete(d) => cmd_delete(&mut ui, d),
        Cmd::Info(i) => cmd_info(&ui, i),
        Cmd::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "xzip", &mut io::stdout());
            Ok(EXIT_OK)
        }
        Cmd::Man => {
            let man = clap_mangen::Man::new(Cli::command());
            let mut buf = Vec::new();
            man.render(&mut buf).ok();
            io::stdout().write_all(&buf).ok();
            Ok(EXIT_OK)
        }
    };
    let code = match result {
        Ok(code) => code,
        Err(Error::Cancelled) => {
            eprintln!("cancelled");
            EXIT_INTERRUPTED
        }
        Err(e) => {
            let red = Style::new().fg_color(Some(AnsiColor::Red.into())).bold();
            if ui.json {
                println!(
                    "{}",
                    json!({"error": e.to_string(), "kind": if e.is_integrity() { "integrity" } else { "error" }})
                );
            } else {
                anstream::eprintln!("{red}error:{red:#} {e}");
            }
            match e {
                Error::Settings(_) => EXIT_USAGE,
                ref e if e.is_integrity() => EXIT_INTEGRITY,
                _ => EXIT_ERROR,
            }
        }
    };
    std::process::exit(code);
}
