//! The window: header, sidebar, file table, details, dialogs, background jobs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Align, Align2, Color32, FontId, Id, Layout, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as ph;

use xzip_core::archive::{
    self, Archive, CreateOptions, Entry, ExtractOptions, ExtractReport, Overwrite, Skipped,
};
use xzip_core::{format_size, Filter, FilterMode, Settings};

use crate::theme::{self, Accent, Palette};

const RECENT_MAX: usize = 8;
const SECURITY_NOTES: &str = "\
Before anything is shown
  • The whole file is hashed (SHA-256) and compared with its trailer. No match, no parsing: no header, no file list.
  • Then the header CRC, the manifest hash, every field's bounds and every path. Any failure and the archive is refused.

Before anything is written
  • Every path is validated first: no \"..\", no absolute paths or drive letters, no backslashes, no control or invisible characters, \
no Windows device names, no trailing dots or spaces, no names that differ only by case or Unicode form, no file where a folder is needed.
  • Sizes are declared and enforced. Per-file and total caps apply, and free disk space is checked.

While writing
  • Folders are created one level at a time and proven (canonical path) to be real folders inside the destination. A planted link \
or junction cannot redirect a write.
  • Each file's compressed bytes are hashed before decoding, and the result is hashed before it is written, to a newly created file \
(the write fails if anything already exists there). Existing files are kept unless you choose to replace them; links and folders are never replaced.
  • The format stores regular files and folders only: no links, no devices, no special permissions.

What hashes do not do
  They detect corruption and modification by anyone who does not recompute them. They do not prove who made the archive; that \
would take a signature with a key you hold.";

enum JobResult {
    Opened(Box<Archive>),
    Created(PathBuf, Vec<Skipped>),
    Extracted(ExtractReport, PathBuf),
    Verified(usize),
    Viewed(String, Vec<u8>),
    Failed(String, bool),
}

struct Job {
    name: &'static str,
    progress: Arc<Mutex<(u64, u64, String)>>,
    cancel: Arc<AtomicBool>,
    rx: Receiver<JobResult>,
    started: Instant,
}

struct Toast {
    text: String,
    kind: ToastKind,
    born: Instant,
}

#[derive(Clone, Copy, PartialEq)]
enum ToastKind {
    Ok,
    Warn,
    Error,
}

#[derive(Default)]
enum Dialog {
    #[default]
    None,
    Compress(CompressDialog),
    Extract(ExtractDialog),
    Skipped(Vec<Skipped>),
    View(String, String),
    Settings,
    Security,
    About,
    ConfirmDelete(Vec<String>),
}

struct CompressDialog {
    sources: Vec<PathBuf>,
    destination: PathBuf,
    level: u32,
    exclude: String,
    adding_to_existing: bool,
}

struct ExtractDialog {
    destination: PathBuf,
    overwrite: bool,
    selected_only: bool,
    open_after: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum SortCol {
    Name,
    Size,
    Packed,
    Modified,
}

pub struct App {
    dark: bool,
    accent: Accent,
    pal: Palette,
    archive: Option<Arc<Archive>>,
    folder: String,
    selection: HashSet<String>,
    last_clicked: Option<String>,
    search: String,
    recent: Vec<PathBuf>,
    level: u32,
    threads: usize,
    filter_auto: bool,
    open_after: bool,
    show_details: bool,
    sort: (SortCol, bool),
    job: Option<Job>,
    toasts: Vec<Toast>,
    dialog: Dialog,
    status: String,
    screenshot: Option<PathBuf>,
    frames: u32,
}

#[derive(Clone)]
struct Row {
    path: String,
    name: String,
    is_dir: bool,
    size: u64,
    packed: u64,
    mtime: i64,
    icon: &'static str,
    tint: Tint,
}

#[derive(Clone, Copy)]
enum Tint {
    Folder,
    Image,
    Media,
    Archive,
    Document,
    Code,
    Binary,
    Plain,
}

fn classify(name: &str, is_dir: bool) -> (&'static str, Tint) {
    if is_dir {
        return (ph::FOLDER, Tint::Folder);
    }
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "tiff" | "heic" | "avif" => {
            (ph::IMAGE, Tint::Image)
        }
        "mp4" | "mkv" | "mov" | "avi" | "webm" => (ph::FILM_STRIP, Tint::Media),
        "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" => (ph::MUSIC_NOTE, Tint::Media),
        "zip" | "xzip" | "xz" | "7z" | "rar" | "gz" | "tar" | "zst" | "bz2" => {
            (ph::FILE_ARCHIVE, Tint::Archive)
        }
        "pdf" => (ph::FILE_PDF, Tint::Document),
        "doc" | "docx" | "odt" | "rtf" => (ph::FILE_DOC, Tint::Document),
        "xls" | "xlsx" | "ods" | "csv" => (ph::TABLE, Tint::Document),
        "ppt" | "pptx" => (ph::PRESENTATION, Tint::Document),
        "txt" | "md" | "log" | "json" | "xml" | "yaml" | "yml" | "toml" | "ini" => {
            (ph::FILE_TEXT, Tint::Plain)
        }
        "rs" | "py" | "c" | "cpp" | "h" | "js" | "ts" | "go" | "java" | "cs" | "sh" | "rb"
        | "html" | "css" | "php" | "swift" | "kt" => (ph::FILE_CODE, Tint::Code),
        "exe" | "dll" | "so" | "dylib" | "bin" | "msi" | "app" | "o" | "a" => {
            (ph::CPU, Tint::Binary)
        }
        _ => (ph::FILE, Tint::Plain),
    }
}

fn tint_color(t: Tint, p: &Palette) -> Color32 {
    match t {
        Tint::Folder => p.warn,
        Tint::Image => p.ok,
        Tint::Media => Color32::from_rgb(244, 114, 182),
        Tint::Archive => Color32::from_rgb(251, 146, 60),
        Tint::Document => Color32::from_rgb(96, 165, 250),
        Tint::Code => Color32::from_rgb(167, 139, 250),
        Tint::Binary => p.accent,
        Tint::Plain => p.text_dim,
    }
}

fn kind_for(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "Folder".into();
    }
    match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && ext.len() <= 8 => {
            format!("{} file", ext.to_ascii_uppercase())
        }
        _ => "File".into(),
    }
}

fn mtime_string(t: i64) -> String {
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
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn open_in_file_manager(path: &Path) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

fn shorten(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        format!("…{}", s.chars().skip(n - max + 1).collect::<String>())
    }
}

fn level_hint(level: u32) -> &'static str {
    match level {
        0 => "quickest, barely squeezes",
        1 | 2 => "fast",
        3..=5 => "balanced",
        6 | 7 => "optimal parsing, patient",
        8 => "big dictionary, slow",
        _ => "smallest it can go",
    }
}

/// Tiny glob: `*` any run, `?` one char; enough for excludes like *.log or node_modules.
fn glob_match(pat: &str, text: &str) -> bool {
    fn rec(p: &[char], t: &[char]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some('*'), _) => rec(&p[1..], t) || (!t.is_empty() && rec(p, &t[1..])),
            (Some('?'), Some(_)) => rec(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => rec(&p[1..], &t[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    rec(&p, &t)
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_fonts(theme::fonts());
        let storage = cc.storage;
        let get = |k: &str| storage.and_then(|s| s.get_string(k));
        let dark = get("dark").map(|v| v != "0").unwrap_or(true);
        let accent = Accent::from_name(&get("accent").unwrap_or_default());
        let level = get("level").and_then(|v| v.parse().ok()).unwrap_or(6);
        let threads = get("threads").and_then(|v| v.parse().ok()).unwrap_or(0);
        let filter_auto = get("filter_auto").map(|v| v != "0").unwrap_or(true);
        let open_after = get("open_after").map(|v| v != "0").unwrap_or(true);
        let recent = get("recent")
            .map(|v| {
                v.split('\n')
                    .filter(|l| !l.is_empty())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        let pal = theme::palette(dark, accent);
        theme::apply(&cc.egui_ctx, &pal);
        let mut app = App {
            dark,
            accent,
            pal,
            archive: None,
            folder: String::new(),
            selection: HashSet::new(),
            last_clicked: None,
            search: String::new(),
            recent,
            level,
            threads,
            filter_auto,
            open_after,
            show_details: true,
            sort: (SortCol::Name, false),
            job: None,
            toasts: Vec::new(),
            dialog: Dialog::None,
            status: "Ready when you are.".into(),
            screenshot: None,
            frames: 0,
        };
        // xzip-gui [archive] [--screenshot out.png]  (renders a frame to a file and exits)
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--screenshot" && i + 1 < args.len() {
                app.screenshot = Some(PathBuf::from(&args[i + 1]));
                i += 1;
            } else if Path::new(&args[i]).is_file() {
                app.open_path(PathBuf::from(&args[i]));
            }
            i += 1;
        }
        app
    }

    fn settings(&self) -> Settings {
        let mut s = Settings::level(self.level);
        s.filter = if self.filter_auto {
            FilterMode::Auto
        } else {
            FilterMode::Fixed(Filter::None)
        };
        s
    }

    fn retheme(&mut self, ctx: &egui::Context) {
        self.pal = theme::palette(self.dark, self.accent);
        theme::apply(ctx, &self.pal);
    }

    fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toasts.push(Toast {
            text: text.into(),
            kind,
            born: Instant::now(),
        });
    }

    fn remember(&mut self, path: &Path) {
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(RECENT_MAX);
    }

    // -- jobs ---------------------------------------------------------------------------

    fn start_job<F>(&mut self, name: &'static str, work: F)
    where
        F: FnOnce(&(dyn Fn(u64, u64, &str) -> bool + Sync)) -> JobResult + Send + 'static,
    {
        if self.job.is_some() {
            self.toast(
                ToastKind::Warn,
                "One thing at a time: something is still running.",
            );
            return;
        }
        let progress = Arc::new(Mutex::new((0u64, 0u64, String::new())));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p2, c2) = (progress.clone(), cancel.clone());
        if self.threads > 0 {
            let _ = rayon::ThreadPoolBuilder::new()
                .num_threads(self.threads)
                .build_global();
        }
        std::thread::spawn(move || {
            let report = move |done: u64, total: u64, path: &str| -> bool {
                if let Ok(mut g) = p2.lock() {
                    *g = (done, total, path.to_string());
                }
                !c2.load(Ordering::Relaxed)
            };
            let _ = tx.send(work(&report));
        });
        self.job = Some(Job {
            name,
            progress,
            cancel,
            rx,
            started: Instant::now(),
        });
        self.status = format!("{name}…");
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        ctx.request_repaint_after(Duration::from_millis(60));
        let result = match job.rx.try_recv() {
            Ok(r) => r,
            Err(_) => return,
        };
        let job = self.job.take().unwrap();
        let secs = job.started.elapsed().as_secs_f64();
        match result {
            JobResult::Opened(a) => {
                let files = a.entries.iter().filter(|e| !e.is_dir()).count();
                self.status = format!("Verified. {files} files, {} folders, {} on disk. Nothing has changed since it was written.", a.entries.len() - files, format_size(a.size()));
                let path = a.path.clone();
                self.archive = Some(Arc::new(*a));
                self.folder.clear();
                self.selection.clear();
                if self.screenshot.is_some() {
                    if let Some(a) = &self.archive {
                        if let Some(d) = a.entries.iter().find(|e| e.is_dir()) {
                            self.folder = d.path.clone();
                        }
                        let folder = self.folder.clone();
                        if let Some(f) = a
                            .entries
                            .iter()
                            .find(|e| !e.is_dir() && e.parent() == folder)
                        {
                            self.selection.insert(f.path.clone());
                        }
                    }
                }
                self.remember(&path);
                self.toast(
                    ToastKind::Ok,
                    format!(
                        "Opened {} in {secs:.1}s",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ),
                );
            }
            JobResult::Created(path, skipped) => {
                if !skipped.is_empty() {
                    self.dialog = Dialog::Skipped(skipped);
                }
                self.toast(ToastKind::Ok, format!("Written and sealed in {secs:.1}s."));
                self.open_path(path);
            }
            JobResult::Extracted(report, _dest) => {
                let msg = if report.skipped.is_empty() {
                    format!("Extracted {} entries in {secs:.1}s.", report.written.len())
                } else {
                    format!(
                        "Extracted {} entries; left {} existing files alone.",
                        report.written.len(),
                        report.skipped.len()
                    )
                };
                self.status = msg.clone();
                self.toast(
                    if report.skipped.is_empty() {
                        ToastKind::Ok
                    } else {
                        ToastKind::Warn
                    },
                    msg,
                );
            }
            JobResult::Verified(n) => {
                self.status = format!("All {n} files check out ({secs:.1}s).");
                self.toast(
                    ToastKind::Ok,
                    format!("All {n} files check out: every stream, every size, every hash."),
                );
            }
            JobResult::Viewed(name, data) => {
                let printable = data
                    .iter()
                    .take(4096)
                    .filter(|&&b| (32..127).contains(&b) || b == 9 || b == 10 || b == 13)
                    .count();
                let text = if !data.is_empty() && printable * 100 >= data.len().min(4096) * 95 {
                    String::from_utf8_lossy(&data[..data.len().min(1 << 20)]).to_string()
                } else {
                    data.chunks(16)
                        .take(4096)
                        .enumerate()
                        .map(|(i, c)| {
                            let hx: Vec<String> = c.iter().map(|b| format!("{b:02X}")).collect();
                            let asc: String = c
                                .iter()
                                .map(|&b| {
                                    if (32..127).contains(&b) {
                                        b as char
                                    } else {
                                        '.'
                                    }
                                })
                                .collect();
                            format!("{:08X}  {:<47}  {asc}", i * 16, hx.join(" "))
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.dialog = Dialog::View(name, text);
            }
            JobResult::Failed(msg, integrity) => {
                self.status = if integrity {
                    "Refused: it did not pass verification. Nothing was read from it.".into()
                } else {
                    "That did not work.".into()
                };
                self.toast(ToastKind::Error, msg);
            }
        }
    }

    fn open_path(&mut self, path: PathBuf) {
        self.start_job("Verifying", move |report| {
            report(0, 1, "hashing the whole file first");
            match archive::open_archive(&path) {
                Ok(a) => JobResult::Opened(Box::new(a)),
                Err(e) => JobResult::Failed(e.to_string(), e.is_integrity()),
            }
        });
    }

    fn start_compress(&mut self, d: CompressDialog) {
        let mut settings = Settings::level(d.level);
        settings.filter = if self.filter_auto {
            FilterMode::Auto
        } else {
            FilterMode::Fixed(Filter::None)
        };
        let existing = if d.adding_to_existing {
            self.archive.clone()
        } else {
            None
        };
        let excludes: Vec<String> = d
            .exclude
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let dest = d.destination.clone();
        let sources = d.sources.clone();
        self.start_job("Compressing", move |report| {
            let filter = move |p: &str| -> bool {
                let name = p.rsplit('/').next().unwrap_or(p);
                !excludes
                    .iter()
                    .any(|x| glob_match(x, name) || glob_match(x, p))
            };
            let opts = CreateOptions {
                settings: &settings,
                follow_symlinks: false,
                filter: &filter,
            };
            let result = match &existing {
                Some(old) => {
                    let (items, _) = archive::collect_sources(&sources, false, &filter);
                    let new_keys: HashSet<String> = items
                        .iter()
                        .map(|i| xzip_core::paths::collision_key(&i.archive_path))
                        .collect();
                    let keep: Vec<&Entry> = old
                        .entries
                        .iter()
                        .filter(|e| !new_keys.contains(&xzip_core::paths::collision_key(&e.path)))
                        .collect();
                    archive::rebuild_archive(old, &dest, &keep, &sources, &opts, Some(report))
                }
                None => archive::create_archive(&dest, &sources, &opts, Some(report)),
            };
            match result {
                Ok((_, skipped)) => JobResult::Created(dest, skipped),
                Err(e) => JobResult::Failed(e.to_string(), e.is_integrity()),
            }
        });
    }

    fn start_extract(&mut self, d: ExtractDialog) {
        let Some(a) = self.archive.clone() else {
            return;
        };
        let selected: Option<Vec<String>> = if d.selected_only {
            Some(self.selected_paths(true))
        } else {
            None
        };
        let opts = ExtractOptions {
            overwrite: if d.overwrite {
                Overwrite::Replace
            } else {
                Overwrite::Skip
            },
            ..Default::default()
        };
        let dest = d.destination.clone();
        let open_after = d.open_after;
        self.open_after = open_after;
        self.start_job("Extracting", move |report| {
            let sel: Option<Vec<&Entry>> = selected.as_ref().map(|paths| {
                a.entries
                    .iter()
                    .filter(|e| paths.contains(&e.path))
                    .collect()
            });
            match a.extract(&dest, sel.as_deref(), &opts, Some(report)) {
                Ok(r) => {
                    if open_after {
                        open_in_file_manager(&dest);
                    }
                    JobResult::Extracted(r, dest)
                }
                Err(e) => JobResult::Failed(e.to_string(), e.is_integrity()),
            }
        });
    }

    fn start_verify(&mut self) {
        let Some(a) = self.archive.clone() else {
            return;
        };
        self.start_job("Testing", move |report| match a.verify(Some(report)) {
            Ok(n) => JobResult::Verified(n),
            Err(e) => JobResult::Failed(e.to_string(), e.is_integrity()),
        });
    }

    fn start_view(&mut self, path: String) {
        let Some(a) = self.archive.clone() else {
            return;
        };
        self.start_job("Reading", move |_| {
            let Some(e) = a.find(&path) else {
                return JobResult::Failed("entry not found".into(), false);
            };
            if e.size > 16 << 20 {
                return JobResult::Failed(
                    "That one is over 16 MiB; extract it instead of viewing.".into(),
                    false,
                );
            }
            match a.read(e, 16 << 20) {
                Ok(data) => JobResult::Viewed(path, data),
                Err(err) => JobResult::Failed(err.to_string(), err.is_integrity()),
            }
        });
    }

    fn start_delete(&mut self, paths: Vec<String>) {
        let Some(a) = self.archive.clone() else {
            return;
        };
        let settings = self.settings();
        self.start_job("Rewriting", move |report| {
            let remove: HashSet<&str> = paths.iter().map(String::as_str).collect();
            let keep: Vec<&Entry> = a
                .entries
                .iter()
                .filter(|e| !remove.contains(e.path.as_str()))
                .collect();
            let opts = CreateOptions {
                settings: &settings,
                follow_symlinks: false,
                filter: &|_| true,
            };
            match archive::rebuild_archive(&a, &a.path, &keep, &[], &opts, Some(report)) {
                Ok(_) => JobResult::Created(a.path.clone(), Vec::new()),
                Err(e) => JobResult::Failed(e.to_string(), e.is_integrity()),
            }
        });
    }

    // -- selection helpers ------------------------------------------------------------

    fn selected_paths(&self, recursive: bool) -> Vec<String> {
        let Some(a) = &self.archive else {
            return Vec::new();
        };
        let mut out: Vec<String> = self.selection.iter().cloned().collect();
        if recursive {
            let prefixes: Vec<String> = out
                .iter()
                .filter(|p| a.find(p).is_some_and(|e| e.is_dir()))
                .map(|p| format!("{p}/"))
                .collect();
            for e in &a.entries {
                if prefixes.iter().any(|pre| e.path.starts_with(pre)) && !out.contains(&e.path) {
                    out.push(e.path.clone());
                }
            }
        }
        out
    }

    fn rows(&self) -> Vec<Row> {
        let Some(a) = &self.archive else {
            return Vec::new();
        };
        let q = self.search.trim().to_lowercase();
        let mut rows: Vec<Row> = Vec::new();
        for e in &a.entries {
            let show = if q.is_empty() {
                e.parent() == self.folder
            } else {
                e.path.to_lowercase().contains(&q)
            };
            if !show {
                continue;
            }
            let (size, packed) = if e.is_dir() {
                let pre = format!("{}/", e.path);
                a.entries
                    .iter()
                    .filter(|x| !x.is_dir() && x.path.starts_with(&pre))
                    .fold((0, 0), |(s, p), x| (s + x.size, p + x.data_length))
            } else {
                (e.size, e.data_length)
            };
            let (icon, tint) = classify(e.name(), e.is_dir());
            rows.push(Row {
                path: e.path.clone(),
                name: if q.is_empty() {
                    e.name().to_string()
                } else {
                    e.path.clone()
                },
                is_dir: e.is_dir(),
                size,
                packed,
                mtime: e.mtime,
                icon,
                tint,
            });
        }
        let (col, rev) = self.sort;
        rows.sort_by(|x, y| {
            let o = y.is_dir.cmp(&x.is_dir).then_with(|| match col {
                SortCol::Name => x.name.to_lowercase().cmp(&y.name.to_lowercase()),
                SortCol::Size => x.size.cmp(&y.size),
                SortCol::Packed => x.packed.cmp(&y.packed),
                SortCol::Modified => x.mtime.cmp(&y.mtime),
            });
            if rev {
                o.reverse()
            } else {
                o
            }
        });
        rows
    }

    // -- actions ----------------------------------------------------------------------

    fn action_open(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("xzip archives", &["xzip"])
            .add_filter("All files", &["*"])
            .pick_file()
        {
            self.open_path(p);
        }
    }

    fn action_new(&mut self, sources: Vec<PathBuf>) {
        let suggested = sources
            .first()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_else(|| "archive".into());
        let dir = sources
            .first()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        self.dialog = Dialog::Compress(CompressDialog {
            sources,
            destination: dir.join(format!("{suggested}.xzip")),
            level: self.level,
            exclude: String::new(),
            adding_to_existing: false,
        });
    }

    fn action_add(&mut self, sources: Vec<PathBuf>) {
        match &self.archive {
            Some(a) => {
                let dest = a.path.clone();
                self.dialog = Dialog::Compress(CompressDialog {
                    sources,
                    destination: dest,
                    level: self.level,
                    exclude: String::new(),
                    adding_to_existing: true,
                });
            }
            None => self.action_new(sources),
        }
    }

    fn action_extract(&mut self, selected_only: bool) {
        let Some(a) = &self.archive else { return };
        let default_dir = a.path.parent().map(Path::to_path_buf).unwrap_or_default();
        self.dialog = Dialog::Extract(ExtractDialog {
            destination: default_dir,
            overwrite: false,
            selected_only: selected_only && !self.selection.is_empty(),
            open_after: self.open_after,
        });
    }

    // -- widgets -------------------------------------------------------------------------

    /// The brand tile: rounded accent square with the zipper glyph.
    fn logo(&self, ui: &mut egui::Ui, size: f32) {
        let p = &self.pal;
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
        let painter = ui.painter();
        let r = size * 0.26;
        painter.rect_filled(rect, r, p.accent);
        painter.rect_filled(rect, r, theme::with_alpha(p.accent2, 70));
        let band = Rect::from_min_max(rect.left_top(), Pos2::new(rect.right(), rect.center().y));
        theme::gradient_rect_h(
            painter,
            band.shrink(2.0),
            theme::with_alpha(Color32::WHITE, 34),
            theme::with_alpha(Color32::WHITE, 4),
        );
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            ph::FILE_ARCHIVE,
            FontId::new(size * 0.58, egui::FontFamily::Proportional),
            Color32::WHITE,
        );
    }

    fn primary_button(&self, ui: &mut egui::Ui, text: &str, enabled: bool) -> bool {
        let p = &self.pal;
        let btn = egui::Button::new(
            RichText::new(text)
                .color(if p.dark {
                    Color32::from_rgb(10, 12, 18)
                } else {
                    Color32::WHITE
                })
                .family(theme::semibold()),
        )
        .fill(p.accent)
        .stroke(Stroke::NONE)
        .corner_radius(9)
        .min_size(Vec2::new(0.0, 34.0));
        ui.add_enabled(enabled, btn).clicked()
    }

    fn tool_button(
        &self,
        ui: &mut egui::Ui,
        icon: &str,
        text: &str,
        enabled: bool,
        tip: &str,
    ) -> bool {
        let btn = egui::Button::new(format!("{icon}  {text}"))
            .frame(false)
            .min_size(Vec2::new(0.0, 34.0));
        let resp = ui.add_enabled(enabled, btn);
        let resp = if tip.is_empty() {
            resp
        } else {
            resp.on_hover_text(tip)
        };
        resp.clicked()
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let busy = self.job.is_some();
        let has = self.archive.is_some();
        let sel = !self.selection.is_empty();
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            self.logo(ui, 30.0);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.add_space(1.0);
                ui.label(RichText::new("xZip").size(18.0).family(theme::semibold()));
                ui.label(RichText::new("ARCHIVER").size(8.5).color(self.pal.text_dim));
            });
            ui.add_space(18.0);
            if self.tool_button(
                ui,
                ph::PLUS_CIRCLE,
                "New",
                !busy,
                "Create an archive (Ctrl+N)",
            ) {
                if let Some(files) = rfd::FileDialog::new().pick_files() {
                    self.action_new(files);
                }
            }
            if self.tool_button(
                ui,
                ph::FOLDER_OPEN,
                "Open",
                !busy,
                "Open an archive (Ctrl+O)",
            ) {
                self.action_open();
            }
            ui.separator();
            if self.tool_button(
                ui,
                ph::FILE_PLUS,
                "Add files",
                !busy,
                "Add files to this archive",
            ) {
                if let Some(files) = rfd::FileDialog::new().pick_files() {
                    self.action_add(files);
                }
            }
            if self.tool_button(
                ui,
                ph::FOLDER_PLUS,
                "Add folder",
                !busy,
                "Add a folder, recursively",
            ) {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.action_add(vec![dir]);
                }
            }
            ui.separator();
            if self.tool_button(
                ui,
                ph::SHIELD_CHECK,
                "Test",
                !busy && has,
                "Decode every file and check every hash",
            ) {
                self.start_verify();
            }
            if self.tool_button(
                ui,
                ph::TRASH,
                "Delete",
                !busy && has && sel,
                "Remove the selection (Delete)",
            ) {
                self.dialog = Dialog::ConfirmDelete(self.selected_paths(true));
            }
            ui.add_space(6.0);
            if self.primary_button(
                ui,
                &format!(
                    "{}  {}",
                    ph::EXPORT,
                    if sel { "Extract selection" } else { "Extract" }
                ),
                !busy && has,
            ) {
                self.action_extract(sel);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let p = self.pal;
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(if self.dark { ph::SUN } else { ph::MOON }).size(17.0),
                        )
                        .frame(false),
                    )
                    .on_hover_text("Switch theme")
                    .clicked()
                {
                    self.dark = !self.dark;
                    self.retheme(ui.ctx());
                }
                if ui
                    .add(egui::Button::new(RichText::new(ph::GEAR_SIX).size(17.0)).frame(false))
                    .on_hover_text("Settings")
                    .clicked()
                {
                    self.dialog = Dialog::Settings;
                }
                if ui
                    .add(egui::Button::new(RichText::new(ph::INFO).size(17.0)).frame(false))
                    .on_hover_text("About")
                    .clicked()
                {
                    self.dialog = Dialog::About;
                }
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(if self.show_details {
                                ph::SIDEBAR_SIMPLE
                            } else {
                                ph::SIDEBAR
                            })
                            .size(17.0),
                        )
                        .frame(false),
                    )
                    .on_hover_text("Details panel")
                    .clicked()
                {
                    self.show_details = !self.show_details;
                }
                ui.add_space(6.0);
                egui::Frame::new()
                    .fill(p.bg)
                    .corner_radius(10)
                    .inner_margin(egui::Margin::symmetric(10, 4))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(ph::MAGNIFYING_GLASS).color(p.text_dim));
                            let edit = egui::TextEdit::singleline(&mut self.search)
                                .id(Id::new("search"))
                                .hint_text("Search this archive")
                                .desired_width(210.0)
                                .frame(egui::Frame::new());
                            ui.add(edit);
                            if !self.search.is_empty()
                                && ui.add(egui::Button::new(ph::X).frame(false)).clicked()
                            {
                                self.search.clear();
                            }
                        });
                    });
            });
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        ui.add_space(4.0);
        ui.label(RichText::new("ARCHIVE").small().color(p.text_dim));
        ui.add_space(2.0);
        match self.archive.clone() {
            Some(a) => {
                let files = a.entries.iter().filter(|e| !e.is_dir()).count();
                let size: u64 = a.entries.iter().map(|e| e.size).sum();
                let ratio = if size > 0 {
                    a.size() as f32 / size as f32
                } else {
                    1.0
                };
                egui::Frame::new()
                    .fill(p.card)
                    .corner_radius(12)
                    .inner_margin(14)
                    .stroke(Stroke::new(1.0, p.border))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(
                            egui::Label::new(
                                RichText::new(
                                    a.path.file_name().unwrap_or_default().to_string_lossy(),
                                )
                                .family(theme::semibold())
                                .size(15.0),
                            )
                            .truncate(),
                        );
                        ui.add_space(8.0);
                        ui.vertical_centered(|ui| {
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::splat(76.0), Sense::hover());
                            let painter = ui.painter();
                            theme::ring(
                                painter,
                                rect.center(),
                                32.0,
                                7.0,
                                1.0 - ratio.clamp(0.0, 1.0),
                                p.accent,
                                p.border,
                            );
                            painter.text(
                                rect.center(),
                                Align2::CENTER_CENTER,
                                format!("{:.0}%", ratio * 100.0),
                                FontId::new(15.0, theme::semibold()),
                                p.text,
                            );
                            ui.label(
                                RichText::new("of the original size")
                                    .small()
                                    .color(p.text_dim),
                            );
                        });
                        ui.add_space(8.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!(
                                    "{} on disk · {} inside",
                                    format_size(a.size()),
                                    format_size(size)
                                ))
                                .small()
                                .color(p.text_dim),
                            )
                            .truncate(),
                        );
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!(
                                    "{files} files · {} folders",
                                    a.entries.len() - files
                                ))
                                .small(),
                            )
                            .truncate(),
                        );
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
                            let painter = ui.painter();
                            theme::glow(painter, rect.center(), 18.0, p.ok);
                            painter.circle_filled(rect.center(), 11.0, theme::with_alpha(p.ok, 40));
                            painter.text(
                                rect.center(),
                                Align2::CENTER_CENTER,
                                ph::SHIELD_CHECK,
                                FontId::proportional(16.0),
                                p.ok,
                            );
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new("Verified")
                                        .color(p.ok)
                                        .family(theme::semibold()),
                                );
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(format!(
                                            "SHA-256 {}…",
                                            &hex(&a.archive_hash)[..12]
                                        ))
                                        .small()
                                        .color(p.text_dim),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(hex(&a.archive_hash));
                            });
                        });
                    });
            }
            None => {
                egui::Frame::new().fill(p.card).corner_radius(12).inner_margin(14).stroke(Stroke::new(1.0, p.border)).show(ui, |ui| {
                    ui.label(RichText::new("Nothing open").family(theme::semibold()));
                    ui.label(RichText::new("Drop files or folders anywhere in this window and I'll make an archive out of them.").small().color(p.text_dim));
                });
            }
        }
        ui.add_space(16.0);
        ui.label(RichText::new("RECENT").small().color(p.text_dim));
        ui.add_space(2.0);
        let recent = self.recent.clone();
        let mut open: Option<PathBuf> = None;
        let mut forget: Option<PathBuf> = None;
        if recent.is_empty() {
            ui.label(
                RichText::new("Archives you open will show up here.")
                    .small()
                    .color(p.text_dim),
            );
        }
        for r in recent {
            let name = r
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let exists = r.exists();
            let label = RichText::new(format!("{}  {name}", ph::CLOCK_COUNTER_CLOCKWISE))
                .color(if exists { p.text } else { p.text_dim });
            let resp = ui.add(egui::Button::new(label).frame(false).truncate());
            if resp.on_hover_text(r.display().to_string()).clicked() && self.job.is_none() {
                if exists {
                    open = Some(r.clone());
                } else {
                    forget = Some(r.clone());
                }
            }
        }
        if let Some(r) = open {
            self.open_path(r);
        }
        if let Some(r) = forget {
            self.recent.retain(|x| x != &r);
            self.toast(
                ToastKind::Warn,
                "That file is gone; removed it from the list.",
            );
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(4.0);
            if ui
                .add(egui::Button::new(format!("{}  What gets checked", ph::SHIELD)).frame(false))
                .clicked()
            {
                self.dialog = Dialog::Security;
            }
        });
    }

    fn breadcrumb(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        ui.horizontal(|ui| {
            let can_up = !self.folder.is_empty() && self.search.is_empty();
            if ui
                .add_enabled(
                    can_up,
                    egui::Button::new(ph::ARROW_UP).min_size(Vec2::new(32.0, 30.0)),
                )
                .on_hover_text("Up (Backspace)")
                .clicked()
            {
                self.go_up();
            }
            let name = self
                .archive
                .as_ref()
                .map(|a| {
                    a.path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                })
                .unwrap_or_default();
            let chip = |ui: &mut egui::Ui, text: &str, current: bool| -> bool {
                let rt = RichText::new(text).family(if current {
                    theme::semibold()
                } else {
                    egui::FontFamily::Proportional
                });
                ui.add(
                    egui::Button::new(rt)
                        .fill(if current {
                            p.accent_soft
                        } else {
                            Color32::TRANSPARENT
                        })
                        .stroke(Stroke::NONE)
                        .corner_radius(8),
                )
                .clicked()
            };
            let parts: Vec<String> = if self.folder.is_empty() {
                vec![]
            } else {
                self.folder.split('/').map(String::from).collect()
            };
            if chip(ui, &name, parts.is_empty()) {
                self.folder.clear();
            }
            let mut goto: Option<String> = None;
            for (i, part) in parts.iter().enumerate() {
                ui.label(RichText::new(ph::CARET_RIGHT).color(p.text_dim));
                if chip(ui, part, i + 1 == parts.len()) {
                    goto = Some(parts[..=i].join("/"));
                }
            }
            if let Some(g) = goto {
                self.folder = g;
            }
            if !self.search.is_empty() {
                ui.label(RichText::new(format!("results for “{}”", self.search)).color(p.text_dim));
            }
        });
    }

    fn go_up(&mut self) {
        if let Some(i) = self.folder.rfind('/') {
            self.folder.truncate(i);
        } else {
            self.folder.clear();
        }
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        let rows = self.rows();
        let max_size = rows.iter().map(|r| r.size).max().unwrap_or(0).max(1) as f32;
        if rows.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(ph::GHOST).size(40.0).color(p.text_dim));
                ui.label(
                    RichText::new(if self.search.is_empty() {
                        "Nothing in here."
                    } else {
                        "No matches."
                    })
                    .color(p.text_dim),
                );
            });
            return;
        }
        let mut enter: Option<String> = None;
        let mut view: Option<String> = None;
        let mut toggle: Option<(String, bool, bool)> = None;
        let mut extract_ctx: Option<String> = None;
        let mut delete_ctx: Option<String> = None;
        let mut sort_click: Option<SortCol> = None;
        let height = ui.available_height();
        let header_col = |ui: &mut egui::Ui,
                          label: &str,
                          col: SortCol,
                          sort: (SortCol, bool),
                          out: &mut Option<SortCol>| {
            let arrow = if sort.0 == col {
                if sort.1 {
                    ph::CARET_UP
                } else {
                    ph::CARET_DOWN
                }
            } else {
                ""
            };
            let text = RichText::new(format!("{label} {arrow}"))
                .small()
                .color(p.text_dim);
            if ui.add(egui::Button::new(text).frame(false)).clicked() {
                *out = Some(col);
            }
        };
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(160.0).clip(true))
            .column(Column::initial(120.0).at_least(80.0))
            .column(Column::initial(90.0).at_least(60.0))
            .column(Column::initial(60.0).at_least(50.0))
            .column(Column::initial(120.0).at_least(90.0))
            .min_scrolled_height(height - 10.0)
            .header(26.0, |mut h| {
                h.col(|ui| header_col(ui, "NAME", SortCol::Name, self.sort, &mut sort_click));
                h.col(|ui| header_col(ui, "SIZE", SortCol::Size, self.sort, &mut sort_click));
                h.col(|ui| header_col(ui, "PACKED", SortCol::Packed, self.sort, &mut sort_click));
                h.col(|ui| {
                    ui.label(RichText::new("RATIO").small().color(p.text_dim));
                });
                h.col(|ui| {
                    header_col(
                        ui,
                        "MODIFIED",
                        SortCol::Modified,
                        self.sort,
                        &mut sort_click,
                    )
                });
            })
            .body(|body| {
                body.rows(30.0, rows.len(), |mut row| {
                    let r = &rows[row.index()];
                    row.set_selected(self.selection.contains(&r.path));
                    row.col(|ui| {
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(r.icon)
                                .color(tint_color(r.tint, &p))
                                .size(17.0),
                        );
                        ui.label(RichText::new(&r.name).color(p.text));
                        if r.is_dir {
                            ui.label(RichText::new(ph::CARET_RIGHT).small().color(p.text_dim));
                        }
                    });
                    row.col(|ui| {
                        // the number, over a bar showing its share of the biggest item here
                        let (rect, _) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 20.0),
                            Sense::hover(),
                        );
                        let painter = ui.painter();
                        let frac = (r.size as f32 / max_size).clamp(0.0, 1.0);
                        let w = (rect.width() - 8.0) * frac;
                        let bar = Rect::from_min_max(
                            Pos2::new(rect.right() - 4.0 - w, rect.top() + 3.0),
                            Pos2::new(rect.right() - 4.0, rect.bottom() - 3.0),
                        );
                        painter.rect_filled(
                            bar,
                            4.0,
                            theme::with_alpha(p.accent, if p.dark { 34 } else { 46 }),
                        );
                        painter.text(
                            Pos2::new(rect.right() - 8.0, rect.center().y),
                            Align2::RIGHT_CENTER,
                            format_size(r.size),
                            FontId::proportional(14.0),
                            p.text,
                        );
                    });
                    row.col(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(format_size(r.packed)).color(p.text_dim));
                        });
                    });
                    row.col(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let txt = if r.size > 0 {
                                format!("{:.0}%", r.packed as f64 / r.size as f64 * 100.0)
                            } else {
                                "–".into()
                            };
                            ui.label(RichText::new(txt).color(p.text_dim));
                        });
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(mtime_string(r.mtime)).color(p.text_dim));
                    });
                    let resp = row.response();
                    if resp.double_clicked() {
                        if r.is_dir {
                            enter = Some(r.path.clone());
                        } else {
                            view = Some(r.path.clone());
                        }
                    } else if resp.clicked() {
                        let (ctrl, shift) =
                            resp.ctx.input(|i| (i.modifiers.command, i.modifiers.shift));
                        toggle = Some((r.path.clone(), ctrl, shift));
                    }
                    resp.context_menu(|ui| {
                        if ui.button(format!("{}  Extract…", ph::EXPORT)).clicked() {
                            extract_ctx = Some(r.path.clone());
                            ui.close();
                        }
                        if !r.is_dir && ui.button(format!("{}  View", ph::EYE)).clicked() {
                            view = Some(r.path.clone());
                            ui.close();
                        }
                        if ui.button(format!("{}  Copy path", ph::COPY)).clicked() {
                            ui.ctx().copy_text(r.path.clone());
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .button(
                                RichText::new(format!("{}  Delete from archive", ph::TRASH))
                                    .color(p.danger),
                            )
                            .clicked()
                        {
                            delete_ctx = Some(r.path.clone());
                            ui.close();
                        }
                    });
                });
            });
        if let Some(col) = sort_click {
            self.sort = if self.sort.0 == col {
                (col, !self.sort.1)
            } else {
                (col, false)
            };
        }
        if let Some((path, ctrl, shift)) = toggle {
            if ctrl {
                if !self.selection.remove(&path) {
                    self.selection.insert(path.clone());
                }
            } else if shift {
                if let Some(last) = &self.last_clicked {
                    let (mut a, mut b) = (None, None);
                    for (i, r) in rows.iter().enumerate() {
                        if &r.path == last {
                            a = Some(i);
                        }
                        if r.path == path {
                            b = Some(i);
                        }
                    }
                    if let (Some(a), Some(b)) = (a, b) {
                        for r in &rows[a.min(b)..=a.max(b)] {
                            self.selection.insert(r.path.clone());
                        }
                    }
                }
            } else {
                self.selection.clear();
                self.selection.insert(path.clone());
            }
            self.last_clicked = Some(path);
        }
        if let Some(path) = enter {
            self.folder = path;
            self.selection.clear();
            self.search.clear();
        }
        if let Some(path) = view {
            self.start_view(path);
        }
        if let Some(path) = extract_ctx {
            self.selection.clear();
            self.selection.insert(path);
            self.action_extract(true);
        }
        if let Some(path) = delete_ctx {
            self.selection.clear();
            self.selection.insert(path);
            self.dialog = Dialog::ConfirmDelete(self.selected_paths(true));
        }
    }

    fn details(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        let Some(a) = self.archive.clone() else {
            return;
        };
        ui.add_space(4.0);
        ui.label(RichText::new("DETAILS").small().color(p.text_dim));
        ui.add_space(2.0);
        let mut extract_one = false;
        let mut view_one: Option<String> = None;
        if self.selection.len() == 1 {
            let path = self.selection.iter().next().unwrap().clone();
            if let Some(e) = a.find(&path) {
                let (icon, tint) = classify(e.name(), e.is_dir());
                egui::Frame::new()
                    .fill(p.card)
                    .corner_radius(12)
                    .inner_margin(14)
                    .stroke(Stroke::new(1.0, p.border))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
                            let painter = ui.painter();
                            painter.rect_filled(
                                rect,
                                10.0,
                                theme::with_alpha(tint_color(tint, &p), 30),
                            );
                            painter.text(
                                rect.center(),
                                Align2::CENTER_CENTER,
                                icon,
                                FontId::proportional(24.0),
                                tint_color(tint, &p),
                            );
                            ui.vertical(|ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(e.name())
                                            .family(theme::semibold())
                                            .size(15.0),
                                    )
                                    .truncate(),
                                );
                                ui.label(
                                    RichText::new(kind_for(e.name(), e.is_dir()))
                                        .small()
                                        .color(p.text_dim),
                                );
                            });
                        });
                        ui.add_space(10.0);
                        let kv = |ui: &mut egui::Ui, k: &str, v: String| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(k).small().color(p.text_dim));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.add(egui::Label::new(RichText::new(v).small()).truncate());
                                });
                            });
                        };
                        kv(ui, "Path", e.path.clone());
                        if !e.is_dir() {
                            kv(
                                ui,
                                "Size",
                                format!("{} · {} bytes", format_size(e.size), e.size),
                            );
                            kv(
                                ui,
                                "Packed",
                                format!(
                                    "{} · {:.1}%",
                                    format_size(e.data_length),
                                    if e.size > 0 {
                                        e.data_length as f64 / e.size as f64 * 100.0
                                    } else {
                                        0.0
                                    }
                                ),
                            );
                        }
                        kv(ui, "Modified", mtime_string(e.mtime));
                        if e.executable() {
                            kv(ui, "Mode", "executable".into());
                        }
                        if !e.is_dir() {
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("SHA-256").small().color(p.text_dim));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(RichText::new(ph::COPY).small())
                                                .frame(false),
                                        )
                                        .on_hover_text("Copy hash")
                                        .clicked()
                                    {
                                        ui.ctx().copy_text(hex(&e.sha_data));
                                    }
                                });
                            });
                            egui::Frame::new()
                                .fill(p.bg)
                                .corner_radius(8)
                                .inner_margin(8)
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(hex(&e.sha_data))
                                                .monospace()
                                                .size(11.0)
                                                .color(p.text_dim),
                                        )
                                        .wrap(),
                                    );
                                });
                        }
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if self.primary_button(ui, &format!("{} Extract", ph::EXPORT), true) {
                                extract_one = true;
                            }
                            if !e.is_dir() && ui.button(format!("{} View", ph::EYE)).clicked() {
                                view_one = Some(e.path.clone());
                            }
                        });
                    });
            }
        } else if self.selection.len() > 1 {
            let paths = self.selected_paths(true);
            let size: u64 = paths.iter().filter_map(|p| a.find(p)).map(|e| e.size).sum();
            egui::Frame::new()
                .fill(p.card)
                .corner_radius(12)
                .inner_margin(14)
                .stroke(Stroke::new(1.0, p.border))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(format!("{} items selected", self.selection.len()))
                            .family(theme::semibold()),
                    );
                    ui.label(
                        RichText::new(format!(
                            "{} across {} entries",
                            format_size(size),
                            paths.len()
                        ))
                        .small()
                        .color(p.text_dim),
                    );
                    ui.add_space(8.0);
                    if self.primary_button(ui, &format!("{} Extract selection", ph::EXPORT), true) {
                        extract_one = true;
                    }
                });
        } else {
            egui::Frame::new().fill(p.card).corner_radius(12).inner_margin(14).stroke(Stroke::new(1.0, p.border)).show(ui, |ui| {
                ui.label(RichText::new("Pick a file to see what's inside it.").small().color(p.text_dim));
                ui.add_space(6.0);
                ui.label(RichText::new("Double-click opens folders and previews files. Ctrl-click and Shift-click select more.").small().color(p.text_dim));
            });
        }
        if extract_one {
            self.action_extract(true);
        }
        if let Some(path) = view_one {
            self.start_view(path);
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        ui.horizontal(|ui| {
            if let Some(job) = &self.job {
                let (done, total, path) =
                    job.progress.lock().map(|g| g.clone()).unwrap_or_default();
                let frac = if total > 0 {
                    (done as f32 / total as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let elapsed = job.started.elapsed().as_secs_f64();
                let rate = if elapsed > 0.2 {
                    done as f64 / elapsed
                } else {
                    0.0
                };
                let eta = if rate > 0.0 && total > done {
                    format!(" · {:.0}s left", (total - done) as f64 / rate)
                } else {
                    String::new()
                };
                ui.label(
                    RichText::new(format!("{} {}", job.name, shorten(&path, 36))).color(p.text_dim),
                );
                let (rect, _) = ui.allocate_exact_size(Vec2::new(260.0, 10.0), Sense::hover());
                let painter = ui.painter();
                painter.rect_filled(rect, 5.0, p.border);
                if frac > 0.0 {
                    let filled = Rect::from_min_size(
                        rect.min,
                        Vec2::new(rect.width() * frac, rect.height()),
                    );
                    theme::gradient_rect_h(painter, filled, p.accent, p.accent2);
                }
                ui.label(
                    RichText::new(format!(
                        "{:.0}%  {}/s{eta}",
                        frac * 100.0,
                        format_size(rate as u64)
                    ))
                    .small()
                    .color(p.text_dim),
                );
                if ui
                    .add(egui::Button::new(format!("{} Cancel", ph::X)).frame(false))
                    .clicked()
                {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            } else {
                ui.label(RichText::new(&self.status).color(p.text_dim));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let chip = format!(
                    "Level {} · {} · filter {}",
                    self.level,
                    if self.threads == 0 {
                        "all cores".to_string()
                    } else {
                        format!("{} threads", self.threads)
                    },
                    if self.filter_auto { "auto" } else { "off" }
                );
                egui::Frame::new()
                    .fill(p.card)
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(8, 3))
                    .show(ui, |ui| {
                        ui.label(RichText::new(chip).small().color(p.text_dim));
                    });
            });
        });
    }

    fn hero(&mut self, ui: &mut egui::Ui) {
        let p = self.pal;
        let full = ui.available_rect_before_wrap();
        theme::gradient_rect(ui.painter(), full, p.bg, p.bg2);
        theme::glow(
            ui.painter(),
            Pos2::new(full.center().x, full.top() + full.height() * 0.3),
            full.width().min(full.height()) * 0.45,
            p.accent,
        );
        let mut open = false;
        let mut new_files = false;
        let mut recent_open: Option<PathBuf> = None;
        ui.vertical_centered(|ui| {
            ui.add_space(full.height() * 0.14);
            self.logo(ui, 72.0);
            ui.add_space(14.0);
            ui.label(RichText::new("Your files, sealed and verified.").size(30.0).family(theme::semibold()));
            ui.add_space(4.0);
            ui.label(RichText::new("Drop anything on this window to archive it. Open an .xzip and it is checked, byte for byte, before you see a single file.").size(14.5).color(p.text_dim));
            ui.add_space(26.0);
            ui.horizontal(|ui| {
                let total = 2.0 * 230.0 + 12.0;
                ui.add_space((ui.available_width() - total).max(0.0) / 2.0);
                if self.hero_card(ui, ph::FOLDER_OPEN, "Open an archive", "Browse, extract, test. Ctrl+O", p.accent) {
                    open = true;
                }
                ui.add_space(12.0);
                if self.hero_card(ui, ph::PLUS_CIRCLE, "Make a new one", "Pick files, choose a level. Ctrl+N", p.accent2) {
                    new_files = true;
                }
            });
            if !self.recent.is_empty() {
                ui.add_space(28.0);
                ui.label(RichText::new("RECENT").small().color(p.text_dim));
                ui.add_space(4.0);
                let layout = Layout::left_to_right(Align::Center).with_main_align(Align::Center);
                ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 40.0), layout, |ui| {
                    for r in self.recent.clone() {
                        let name = r.file_name().unwrap_or_default().to_string_lossy().to_string();
                        if ui.add(egui::Button::new(format!("{}  {name}", ph::FILE_ARCHIVE)).corner_radius(14)).on_hover_text(r.display().to_string()).clicked() {
                            recent_open = Some(r.clone());
                        }
                    }
                });
            }
        });
        if open {
            self.action_open();
        }
        if new_files {
            if let Some(files) = rfd::FileDialog::new().pick_files() {
                self.action_new(files);
            }
        }
        if let Some(r) = recent_open {
            self.open_path(r);
        }
    }

    fn hero_card(
        &self,
        ui: &mut egui::Ui,
        icon: &str,
        title: &str,
        sub: &str,
        color: Color32,
    ) -> bool {
        let p = &self.pal;
        let size = Vec2::new(230.0, 92.0);
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        let hover = ui
            .ctx()
            .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
        let painter = ui.painter();
        let rect = rect.translate(Vec2::new(0.0, -2.0 * hover));
        painter.rect_filled(
            rect.translate(Vec2::new(0.0, 4.0)),
            14.0,
            Color32::from_black_alpha((50.0 * hover) as u8),
        );
        painter.rect_filled(rect, 14.0, theme::mix(p.card, p.card_hover, hover));
        painter.rect_stroke(
            rect,
            14.0,
            Stroke::new(1.0, theme::mix(p.border, color, hover * 0.8)),
            egui::StrokeKind::Inside,
        );
        let tile = Rect::from_min_size(rect.min + Vec2::new(16.0, 22.0), Vec2::splat(48.0));
        painter.rect_filled(tile, 12.0, theme::with_alpha(color, 36));
        painter.text(
            tile.center(),
            Align2::CENTER_CENTER,
            icon,
            FontId::proportional(26.0),
            color,
        );
        painter.text(
            rect.min + Vec2::new(78.0, 32.0),
            Align2::LEFT_CENTER,
            title,
            FontId::new(15.0, theme::semibold()),
            p.text,
        );
        painter.text(
            rect.min + Vec2::new(78.0, 56.0),
            Align2::LEFT_CENTER,
            sub,
            FontId::proportional(12.0),
            p.text_dim,
        );
        resp.clicked()
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        let p = self.pal;
        self.toasts.retain(|t| {
            t.born.elapsed()
                < Duration::from_millis(if t.kind == ToastKind::Error {
                    9000
                } else {
                    4200
                })
        });
        if self.toasts.is_empty() {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(40));
        egui::Area::new(Id::new("toasts"))
            .anchor(Align2::RIGHT_BOTTOM, [-16.0, -46.0])
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                for t in &self.toasts {
                    let age = t.born.elapsed().as_secs_f32();
                    let life = if t.kind == ToastKind::Error { 9.0 } else { 4.2 };
                    let a = (age / 0.18).min(1.0) * ((life - age) / 0.4).clamp(0.0, 1.0);
                    let (icon, color) = match t.kind {
                        ToastKind::Ok => (ph::CHECK_CIRCLE, p.ok),
                        ToastKind::Warn => (ph::WARNING, p.warn),
                        ToastKind::Error => (ph::X_CIRCLE, p.danger),
                    };
                    let alpha = (a * 255.0) as u8;
                    egui::Frame::new()
                        .fill(theme::with_alpha(p.card, alpha))
                        .stroke(Stroke::new(1.0, theme::with_alpha(color, alpha)))
                        .corner_radius(12)
                        .inner_margin(12)
                        .shadow(egui::epaint::Shadow {
                            offset: [0, 6],
                            blur: 18,
                            spread: 0,
                            color: Color32::from_black_alpha((110.0 * a) as u8),
                        })
                        .show(ui, |ui| {
                            ui.set_max_width(380.0);
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(icon)
                                        .color(theme::with_alpha(color, alpha))
                                        .size(18.0),
                                );
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&t.text)
                                            .color(theme::with_alpha(p.text, alpha)),
                                    )
                                    .wrap(),
                                );
                            });
                        });
                    ui.add_space(6.0);
                }
            });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let p = self.pal;
        let dialog = std::mem::take(&mut self.dialog);
        let mut next = Dialog::None;
        let close_btn = |ui: &mut egui::Ui| ui.button("Close").clicked();
        match dialog {
            Dialog::None => {}
            Dialog::Compress(mut d) => {
                let mut action: Option<bool> = None;
                let m = egui::Modal::new(Id::new("compress")).show(ctx, |ui| {
                    ui.set_width(580.0);
                    ui.heading(if d.adding_to_existing { "Add to archive" } else { "New archive" });
                    ui.add_space(8.0);
                    ui.label(RichText::new("WHAT GOES IN").small().color(p.text_dim));
                    egui::Frame::new().fill(p.bg).corner_radius(10).inner_margin(8).show(ui, |ui| {
                        egui::ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                            let mut remove: Option<usize> = None;
                            if d.sources.is_empty() {
                                ui.label(RichText::new("Nothing yet. Add files or a folder below, or drop them on the window.").small().color(p.text_dim));
                            }
                            for (i, s) in d.sources.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    let (icon, tint) = classify(&s.file_name().unwrap_or_default().to_string_lossy(), s.is_dir());
                                    ui.label(RichText::new(icon).color(tint_color(tint, &p)));
                                    ui.add(egui::Label::new(s.display().to_string()).truncate());
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if ui.add(egui::Button::new(ph::X).frame(false)).clicked() {
                                            remove = Some(i);
                                        }
                                    });
                                });
                            }
                            if let Some(i) = remove {
                                d.sources.remove(i);
                            }
                        });
                    });
                    ui.horizontal(|ui| {
                        if ui.button(format!("{} Add files", ph::FILE_PLUS)).clicked() {
                            if let Some(f) = rfd::FileDialog::new().pick_files() {
                                d.sources.extend(f);
                            }
                        }
                        if ui.button(format!("{} Add folder", ph::FOLDER_PLUS)).clicked() {
                            if let Some(f) = rfd::FileDialog::new().pick_folder() {
                                d.sources.push(f);
                            }
                        }
                    });
                    ui.add_space(12.0);
                    ui.label(RichText::new("ARCHIVE FILE").small().color(p.text_dim));
                    ui.horizontal(|ui| {
                        let mut dest = d.destination.display().to_string();
                        if ui.add_enabled(!d.adding_to_existing, egui::TextEdit::singleline(&mut dest).desired_width(450.0)).changed() {
                            d.destination = PathBuf::from(dest);
                        }
                        if ui.add_enabled(!d.adding_to_existing, egui::Button::new("Browse…")).clicked() {
                            if let Some(f) = rfd::FileDialog::new().add_filter("xzip archive", &["xzip"]).set_file_name(d.destination.file_name().unwrap_or_default().to_string_lossy()).save_file() {
                                d.destination = f;
                            }
                        }
                    });
                    ui.add_space(12.0);
                    ui.label(RichText::new("HOW HARD TO TRY").small().color(p.text_dim));
                    ui.horizontal(|ui| {
                        ui.add(egui::Slider::new(&mut d.level, 0..=9).show_value(true));
                        ui.label(RichText::new(level_hint(d.level)).color(p.text_dim));
                    });
                    ui.add_space(8.0);
                    ui.label(RichText::new("LEAVE OUT (comma-separated patterns, e.g. *.log, node_modules)").small().color(p.text_dim));
                    ui.add(egui::TextEdit::singleline(&mut d.exclude).desired_width(f32::INFINITY));
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let ok = !d.sources.is_empty() && !d.destination.as_os_str().is_empty();
                            if self.primary_button(ui, &format!("{}  Compress", ph::PACKAGE), ok) {
                                action = Some(true);
                            }
                            if ui.button("Cancel").clicked() {
                                action = Some(false);
                            }
                        });
                    });
                });
                match action {
                    Some(true) => {
                        self.level = d.level;
                        self.start_compress(d);
                    }
                    Some(false) => {}
                    None => {
                        if !m.should_close() {
                            next = Dialog::Compress(d);
                        }
                    }
                }
            }
            Dialog::Extract(mut d) => {
                let mut action: Option<bool> = None;
                let count = if d.selected_only {
                    self.selected_paths(true).len()
                } else {
                    self.archive.as_ref().map(|a| a.entries.len()).unwrap_or(0)
                };
                let m = egui::Modal::new(Id::new("extract")).show(ctx, |ui| {
                    ui.set_width(540.0);
                    ui.heading("Extract");
                    ui.add_space(8.0);
                    ui.label(RichText::new("WHERE TO").small().color(p.text_dim));
                    ui.horizontal(|ui| {
                        let mut dest = d.destination.display().to_string();
                        if ui
                            .add(egui::TextEdit::singleline(&mut dest).desired_width(410.0))
                            .changed()
                        {
                            d.destination = PathBuf::from(dest);
                        }
                        if ui.button("Browse…").clicked() {
                            if let Some(f) = rfd::FileDialog::new().pick_folder() {
                                d.destination = f;
                            }
                        }
                    });
                    ui.add_space(10.0);
                    if !self.selection.is_empty() {
                        ui.radio_value(
                            &mut d.selected_only,
                            true,
                            format!("Just the selection ({count} entries)"),
                        );
                        ui.radio_value(&mut d.selected_only, false, "Everything");
                    } else {
                        ui.label(format!("Everything ({count} entries)"));
                    }
                    ui.add_space(4.0);
                    ui.checkbox(&mut d.overwrite, "Replace files that already exist there");
                    ui.label(
                        RichText::new("Links and folders are never replaced, whatever you tick.")
                            .small()
                            .color(p.text_dim),
                    );
                    ui.checkbox(&mut d.open_after, "Open the folder when it's done");
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if self.primary_button(ui, &format!("{}  Extract", ph::EXPORT), true) {
                                action = Some(true);
                            }
                            if ui.button("Cancel").clicked() {
                                action = Some(false);
                            }
                        });
                    });
                });
                match action {
                    Some(true) => self.start_extract(d),
                    Some(false) => {}
                    None => {
                        if !m.should_close() {
                            next = Dialog::Extract(d);
                        }
                    }
                }
            }
            Dialog::Skipped(list) => {
                let m = egui::Modal::new(Id::new("skipped")).show(ctx, |ui| {
                    ui.set_width(580.0);
                    ui.heading(format!("{} things were left out", list.len()));
                    ui.label(RichText::new("Links, special files and names that could not be extracted on every platform are skipped on purpose.").color(p.text_dim));
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                        for s in &list {
                            ui.label(s.path.display().to_string());
                            ui.label(RichText::new(format!("    {}", s.reason)).small().color(p.warn));
                        }
                    });
                    ui.add_space(8.0);
                    if close_btn(ui) {
                        ui.close();
                    }
                });
                if !m.should_close() {
                    next = Dialog::Skipped(list);
                }
            }
            Dialog::View(name, text) => {
                let m = egui::Modal::new(Id::new("view")).show(ctx, |ui| {
                    ui.set_width(860.0);
                    ui.heading(&name);
                    egui::Frame::new()
                        .fill(p.bg)
                        .corner_radius(10)
                        .inner_margin(10)
                        .show(ui, |ui| {
                            egui::ScrollArea::both().max_height(520.0).show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(RichText::new(&text).monospace().size(12.0))
                                        .selectable(true),
                                );
                            });
                        });
                    ui.add_space(8.0);
                    if close_btn(ui) {
                        ui.close();
                    }
                });
                if !m.should_close() {
                    next = Dialog::View(name, text);
                }
            }
            Dialog::Settings => {
                let (mut dark, mut accent) = (self.dark, self.accent);
                let m = egui::Modal::new(Id::new("settings")).show(ctx, |ui| {
                    ui.set_width(460.0);
                    ui.heading("Settings");
                    ui.add_space(10.0);
                    ui.label(RichText::new("LOOK").small().color(p.text_dim));
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut dark, true, format!("{} Dark", ph::MOON));
                        ui.selectable_value(&mut dark, false, format!("{} Light", ph::SUN));
                        ui.add_space(12.0);
                        for a in Accent::ALL {
                            let (c, _) = a.colors(dark);
                            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
                            let painter = ui.painter();
                            painter.circle_filled(rect.center(), 9.0, c);
                            if a == accent {
                                painter.circle_stroke(rect.center(), 11.0, Stroke::new(2.0, p.text));
                            }
                            if resp.on_hover_text(a.name()).clicked() {
                                accent = a;
                            }
                        }
                    });
                    ui.add_space(10.0);
                    ui.label(RichText::new("COMPRESSION").small().color(p.text_dim));
                    ui.horizontal(|ui| {
                        ui.label("Default level");
                        ui.add(egui::Slider::new(&mut self.level, 0..=9));
                        ui.label(RichText::new(level_hint(self.level)).small().color(p.text_dim));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Threads");
                        ui.add(egui::Slider::new(&mut self.threads, 0..=64));
                        ui.label(RichText::new(if self.threads == 0 { "all cores" } else { "" }).small().color(p.text_dim));
                    });
                    ui.checkbox(&mut self.filter_auto, "Executable filter: detect x86 / ARM64 / ARM / PowerPC / SPARC code and rewrite branch targets so it packs tighter");
                    ui.label(RichText::new("Standard xz filters; the streams still open in xz and 7-Zip. Delta is tried on large non-code files.").small().color(p.text_dim));
                    ui.add_space(10.0);
                    ui.label(RichText::new("AFTER EXTRACTING").small().color(p.text_dim));
                    ui.checkbox(&mut self.open_after, "Open the destination folder");
                    ui.add_space(14.0);
                    if close_btn(ui) {
                        ui.close();
                    }
                });
                if dark != self.dark || accent != self.accent {
                    self.dark = dark;
                    self.accent = accent;
                    self.retheme(ctx);
                }
                if !m.should_close() {
                    next = Dialog::Settings;
                }
            }
            Dialog::Security => {
                let m = egui::Modal::new(Id::new("security")).show(ctx, |ui| {
                    ui.set_width(660.0);
                    ui.heading(format!(
                        "{}  What gets checked, and what gets refused",
                        ph::SHIELD_CHECK
                    ));
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .max_height(470.0)
                        .show(ui, |ui| {
                            ui.label(SECURITY_NOTES);
                        });
                    ui.add_space(8.0);
                    if close_btn(ui) {
                        ui.close();
                    }
                });
                if !m.should_close() {
                    next = Dialog::Security;
                }
            }
            Dialog::About => {
                let m = egui::Modal::new(Id::new("about")).show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.vertical_centered(|ui| {
                        self.logo(ui, 64.0);
                        ui.add_space(8.0);
                        ui.label(RichText::new("xZip Archiver").size(22.0).family(theme::semibold()));
                        ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).color(p.text_dim));
                        ui.add_space(10.0);
                        ui.label("Verified archives. Fast-LZMA2 inside, written from scratch:");
                        ui.label(RichText::new("radix match finder · optimal parser · executable filters · standard .xz streams").small().color(p.text_dim));
                        ui.add_space(12.0);
                        ui.label(RichText::new("Made by Clinton Turner").family(theme::semibold()));
                        ui.label(RichText::new("All rights reserved.").small().color(p.text_dim));
                        ui.add_space(14.0);
                        if close_btn(ui) {
                            ui.close();
                        }
                    });
                });
                if !m.should_close() {
                    next = Dialog::About;
                }
            }
            Dialog::ConfirmDelete(paths) => {
                let mut action: Option<bool> = None;
                let m = egui::Modal::new(Id::new("delete")).show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.heading("Remove from the archive?");
                    ui.label(format!("{} entries go. The archive is rewritten without recompressing anything else.", paths.len()));
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let btn = egui::Button::new(RichText::new(format!("{}  Remove", ph::TRASH)).color(Color32::WHITE).family(theme::semibold())).fill(p.danger).stroke(Stroke::NONE).corner_radius(9).min_size(Vec2::new(0.0, 34.0));
                            if ui.add(btn).clicked() {
                                action = Some(true);
                            }
                            if ui.button("Keep them").clicked() {
                                action = Some(false);
                            }
                        });
                    });
                });
                match action {
                    Some(true) => {
                        self.selection.clear();
                        self.start_delete(paths);
                    }
                    Some(false) => {}
                    None => {
                        if !m.should_close() {
                            next = Dialog::ConfirmDelete(paths);
                        }
                    }
                }
            }
        }
        self.dialog = next;
    }

    fn handle_keys_and_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() && self.job.is_none() {
            if self.archive.is_some() {
                self.action_add(dropped);
            } else {
                self.action_new(dropped);
            }
        }
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let p = self.pal;
            egui::Area::new(Id::new("drop_hint"))
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .order(egui::Order::Foreground)
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::new()
                        .fill(theme::with_alpha(p.panel, 235))
                        .stroke(Stroke::new(2.0, p.accent))
                        .corner_radius(18)
                        .inner_margin(30)
                        .show(ui, |ui| {
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new(ph::DOWNLOAD_SIMPLE)
                                        .size(40.0)
                                        .color(p.accent),
                                );
                                ui.label(
                                    RichText::new(if self.archive.is_some() {
                                        "Drop to add to this archive"
                                    } else {
                                        "Drop to make an archive"
                                    })
                                    .size(20.0)
                                    .family(theme::semibold()),
                                );
                            });
                        });
                });
        }
        if matches!(self.dialog, Dialog::None) && !ctx.egui_wants_keyboard_input() {
            let (open, new, esc, up, del, find) = ctx.input(|i| {
                (
                    i.modifiers.command && i.key_pressed(egui::Key::O),
                    i.modifiers.command && i.key_pressed(egui::Key::N),
                    i.key_pressed(egui::Key::Escape),
                    i.key_pressed(egui::Key::Backspace),
                    i.key_pressed(egui::Key::Delete),
                    i.modifiers.command && i.key_pressed(egui::Key::F),
                )
            });
            if open {
                self.action_open();
            }
            if new {
                if let Some(files) = rfd::FileDialog::new().pick_files() {
                    self.action_new(files);
                }
            }
            if esc {
                if let Some(job) = &self.job {
                    job.cancel.store(true, Ordering::Relaxed);
                } else {
                    self.selection.clear();
                }
            }
            if up {
                self.go_up();
            }
            if del && !self.selection.is_empty() && self.archive.is_some() {
                self.dialog = Dialog::ConfirmDelete(self.selected_paths(true));
            }
            if find {
                ctx.memory_mut(|m| m.request_focus(Id::new("search")));
            }
        }
    }

    fn screenshot_step(&mut self, ctx: &egui::Context) {
        let Some(path) = self.screenshot.clone() else {
            return;
        };
        self.frames += 1;
        if self.frames == 40 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let (w, h) = (img.size[0] as u32, img.size[1] as u32);
            let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            if let Some(buf) = image::RgbaImage::from_raw(w, h, bytes) {
                let _ = buf.save(&path);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint();
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string("dark", if self.dark { "1" } else { "0" }.into());
        storage.set_string("accent", self.accent.name().into());
        storage.set_string("level", self.level.to_string());
        storage.set_string("threads", self.threads.to_string());
        storage.set_string(
            "filter_auto",
            if self.filter_auto { "1" } else { "0" }.into(),
        );
        storage.set_string("open_after", if self.open_after { "1" } else { "0" }.into());
        storage.set_string(
            "recent",
            self.recent
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.poll_job(ctx);
        let p = self.pal;
        egui::Panel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(p.panel)
                    .inner_margin(egui::Margin::symmetric(12, 9)),
            )
            .show(ui, |ui| self.header(ui));
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(p.panel)
                    .inner_margin(egui::Margin::symmetric(14, 7)),
            )
            .show(ui, |ui| self.status_bar(ui));
        if self.archive.is_some() {
            egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(260.0)
                .frame(egui::Frame::new().fill(p.panel).inner_margin(12))
                .show(ui, |ui| self.sidebar(ui));
            if self.show_details {
                egui::Panel::right("details")
                    .resizable(true)
                    .default_size(300.0)
                    .frame(egui::Frame::new().fill(p.panel).inner_margin(12))
                    .show(ui, |ui| self.details(ui));
            }
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(p.bg).inner_margin(12))
                .show(ui, |ui| {
                    self.breadcrumb(ui);
                    ui.add_space(8.0);
                    egui::Frame::new()
                        .fill(p.panel)
                        .corner_radius(14)
                        .inner_margin(6)
                        .stroke(Stroke::new(1.0, p.border))
                        .show(ui, |ui| {
                            self.table(ui);
                        });
                });
        } else {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(p.bg))
                .show(ui, |ui| self.hero(ui));
        }
        self.dialogs(ctx);
        self.toasts(ctx);
        self.handle_keys_and_drops(ctx);
        self.screenshot_step(ctx);
    }
}
