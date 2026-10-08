//! The .xzip archive: files and folders, each file an .xz stream, in a container
//! that is verified before it is parsed.
//!
//! Layout (all little-endian):
//!   Header   64 B   magic, version, flags, manifest offset + length, SHA-256 of
//!                   the manifest, CRC32 of the header
//!   Payload         the files' .xz streams, back to back
//!   Manifest        entries: kind, flags, mtime, path (UTF-8, <= 65535 bytes) and
//!                   for files: method, stream offset + length, size, SHA-256 of
//!                   the stream, SHA-256 of the contents
//!   Trailer  48 B   magic, SHA-256 of everything before it, total length
//!
//! Reading order: whole-file hash first (nothing is interpreted until it matches),
//! then header CRC, manifest hash, strict bounds on every field, every path. A
//! file's stream is hashed before it goes to the decoder, and the output is hashed
//! before it is written. See paths.rs and extract() for what is refused.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::error::{io_err, io_path, Error, Result};
use crate::lzma2::Progress;
use crate::paths::{archive_path_from_os, check_portable_path, collision_key};
use crate::settings::Settings;
use crate::xz::{self, Check};

pub const MAGIC: &[u8; 6] = b"XZIP\x00\x01";
pub const VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 64;
pub const TRAILER_MAGIC: &[u8; 8] = b"XZIPEND\x00";
pub const TRAILER_SIZE: usize = 48;
const ENTRY_FIXED: usize = 1 + 1 + 8 + 2;
const FILE_EXTRA: usize = 1 + 8 + 8 + 8 + 32 + 32;
pub const KIND_FILE: u8 = 0;
pub const KIND_DIR: u8 = 1;
pub const FLAG_EXECUTABLE: u8 = 1;
pub const METHOD_XZ: u8 = 1;
const MAX_ENTRIES: u64 = 10_000_000;
const MAX_MTIME: i64 = 1 << 40;
pub const DEFAULT_MAX_ENTRY_SIZE: u64 = 1 << 31;
pub const DEFAULT_MAX_TOTAL_SIZE: u64 = 1 << 40;

/// Reports (bytes done, bytes total, current path); return false to cancel.
pub type ArchiveProgress<'p> = &'p (dyn Fn(u64, u64, &str) -> bool + Sync);

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub kind: u8,
    pub flags: u8,
    pub mtime: i64,
    pub method: u8,
    pub data_offset: u64,
    pub data_length: u64,
    pub size: u64,
    pub sha_stream: [u8; 32],
    pub sha_data: [u8; 32],
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == KIND_DIR
    }

    pub fn components(&self) -> Vec<&str> {
        self.path.split('/').collect()
    }

    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or("")
    }

    pub fn parent(&self) -> &str {
        match self.path.rfind('/') {
            Some(i) => &self.path[..i],
            None => "",
        }
    }

    pub fn executable(&self) -> bool {
        self.flags & FLAG_EXECUTABLE != 0
    }

    fn pack(&self, out: &mut Vec<u8>) {
        let pb = self.path.as_bytes();
        out.push(self.kind);
        out.push(self.flags);
        out.extend_from_slice(&self.mtime.to_le_bytes());
        out.extend_from_slice(&(pb.len() as u16).to_le_bytes());
        out.extend_from_slice(pb);
        if self.kind == KIND_FILE {
            out.push(self.method);
            out.extend_from_slice(&self.data_offset.to_le_bytes());
            out.extend_from_slice(&self.data_length.to_le_bytes());
            out.extend_from_slice(&self.size.to_le_bytes());
            out.extend_from_slice(&self.sha_stream);
            out.extend_from_slice(&self.sha_data);
        }
    }
}

/// Rules that involve more than one entry.
pub fn validate_entry_set(entries: &[Entry]) -> Result<()> {
    let mut seen: std::collections::HashMap<String, &str> =
        std::collections::HashMap::with_capacity(entries.len());
    let mut files: std::collections::HashSet<String> = std::collections::HashSet::new();
    for e in entries {
        let key = collision_key(&e.path);
        if let Some(other) = seen.get(&key) {
            return Err(Error::UnsafePath(format!(
                "duplicate (or case/normalisation-colliding) entries: {other:?} and {:?}",
                e.path
            )));
        }
        seen.insert(key.clone(), &e.path);
        if !e.is_dir() {
            files.insert(key);
        }
    }
    for e in entries {
        let comps = e.components();
        for i in 1..comps.len() {
            let prefix = collision_key(&comps[..i].join("/"));
            if files.contains(&prefix) {
                return Err(Error::UnsafePath(format!(
                    "{:?} needs a directory where entry {:?} is a file",
                    e.path,
                    comps[..i].join("/")
                )));
            }
        }
    }
    Ok(())
}

fn clamp_mtime(t: SystemTime) -> i64 {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    secs.clamp(-MAX_MTIME, MAX_MTIME)
}

fn mtime_to_system(t: i64) -> SystemTime {
    if t >= 0 {
        UNIX_EPOCH + Duration::from_secs(t as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs((-t) as u64)
    }
}

// ---- Writing ------------------------------------------------------------------------

/// Writes an archive incrementally to a temp file beside the target and renames it
/// into place at the end, so a crash never leaves a half-written .xzip.
pub struct ArchiveWriter {
    out_path: PathBuf,
    settings: Settings,
    entries: Vec<Entry>,
    keys: std::collections::HashSet<String>,
    tmp: Option<tempfile::NamedTempFile>,
    pos: u64,
}

impl ArchiveWriter {
    pub fn new(out_path: &Path, settings: &Settings) -> Result<Self> {
        settings.validate()?;
        let dir = out_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut tmp = tempfile::Builder::new()
            .prefix(".xzip-")
            .tempfile_in(&dir)
            .map_err(io_path("creating temp file in", &dir))?;
        tmp.write_all(&[0u8; HEADER_SIZE])
            .map_err(io_err("writing archive"))?;
        Ok(ArchiveWriter {
            out_path: out_path.to_path_buf(),
            settings: settings.clone(),
            entries: Vec::new(),
            keys: Default::default(),
            tmp: Some(tmp),
            pos: HEADER_SIZE as u64,
        })
    }

    fn check_new(&mut self, path: &str) -> Result<()> {
        check_portable_path(path)?;
        let key = collision_key(path);
        if !self.keys.insert(key) {
            return Err(Error::UnsafePath(format!("duplicate entry {path:?}")));
        }
        Ok(())
    }

    pub fn add_dir(&mut self, path: &str, mtime: i64) -> Result<()> {
        self.check_new(path)?;
        self.entries.push(Entry {
            path: path.into(),
            kind: KIND_DIR,
            flags: 0,
            mtime: mtime.clamp(-MAX_MTIME, MAX_MTIME),
            method: METHOD_XZ,
            data_offset: 0,
            data_length: 0,
            size: 0,
            sha_stream: [0; 32],
            sha_data: [0; 32],
        });
        Ok(())
    }

    pub fn add_bytes(
        &mut self,
        path: &str,
        data: &[u8],
        mtime: i64,
        executable: bool,
        progress: Option<Progress>,
    ) -> Result<&Entry> {
        self.check_new(path)?;
        let (stream, _, _) = xz::compress(data, &self.settings, Check::Crc64, progress)?;
        self.write_stream(
            path,
            &stream,
            data.len() as u64,
            Sha256::digest(data).into(),
            mtime,
            if executable { FLAG_EXECUTABLE } else { 0 },
        )
    }

    pub fn add_file(
        &mut self,
        path: &str,
        source: &Path,
        progress: Option<Progress>,
    ) -> Result<&Entry> {
        let meta = fs::metadata(source).map_err(io_path("reading", source))?;
        let data = fs::read(source).map_err(io_path("reading", source))?;
        let mtime = meta.modified().map(clamp_mtime).unwrap_or(0);
        #[cfg(unix)]
        let exec = {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o100 != 0
        };
        #[cfg(not(unix))]
        let exec = false;
        self.add_bytes(path, &data, mtime, exec, progress)
    }

    /// Copy an already-compressed stream from another archive (verified first).
    pub fn add_stream(&mut self, entry: &Entry, stream: &[u8]) -> Result<&Entry> {
        if <[u8; 32]>::from(Sha256::digest(stream)) != entry.sha_stream {
            return Err(Error::Integrity(format!(
                "stream for {:?} does not match its hash",
                entry.path
            )));
        }
        self.check_new(&entry.path)?;
        self.write_stream(
            &entry.path,
            stream,
            entry.size,
            entry.sha_data,
            entry.mtime,
            entry.flags,
        )
    }

    fn write_stream(
        &mut self,
        path: &str,
        stream: &[u8],
        size: u64,
        sha_data: [u8; 32],
        mtime: i64,
        flags: u8,
    ) -> Result<&Entry> {
        let tmp = self.tmp.as_mut().expect("writer is open");
        tmp.write_all(stream).map_err(io_err("writing archive"))?;
        self.entries.push(Entry {
            path: path.into(),
            kind: KIND_FILE,
            flags,
            mtime: mtime.clamp(-MAX_MTIME, MAX_MTIME),
            method: METHOD_XZ,
            data_offset: self.pos,
            data_length: stream.len() as u64,
            size,
            sha_stream: Sha256::digest(stream).into(),
            sha_data,
        });
        self.pos += stream.len() as u64;
        Ok(self.entries.last().unwrap())
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Write manifest, header and trailer, then rename into place.
    pub fn finish(mut self) -> Result<Vec<Entry>> {
        validate_entry_set(&self.entries)?;
        let mut tmp = self.tmp.take().expect("writer is open");
        let mut manifest = (self.entries.len() as u32).to_le_bytes().to_vec();
        for e in &self.entries {
            e.pack(&mut manifest);
        }
        let manifest_offset = self.pos;
        tmp.write_all(&manifest)
            .map_err(io_err("writing archive"))?;
        let mut header = Vec::with_capacity(HEADER_SIZE);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&VERSION.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&manifest_offset.to_le_bytes());
        header.extend_from_slice(&(manifest.len() as u64).to_le_bytes());
        header.extend_from_slice(&Sha256::digest(&manifest));
        let crc = crc32fast::hash(&header);
        header.extend_from_slice(&crc.to_le_bytes());
        debug_assert_eq!(header.len(), HEADER_SIZE);
        let f = tmp.as_file_mut();
        f.seek(SeekFrom::Start(0))
            .map_err(io_err("writing archive"))?;
        f.write_all(&header).map_err(io_err("writing archive"))?;
        f.flush().map_err(io_err("writing archive"))?;
        // trailer: hash of everything so far, streamed back from disk
        f.seek(SeekFrom::Start(0))
            .map_err(io_err("reading back archive"))?;
        let mut h = Sha256::new();
        let mut total = 0u64;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf).map_err(io_err("reading back archive"))?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            total += n as u64;
        }
        f.seek(SeekFrom::End(0))
            .map_err(io_err("writing archive"))?;
        f.write_all(TRAILER_MAGIC)
            .map_err(io_err("writing archive"))?;
        f.write_all(&h.finalize())
            .map_err(io_err("writing archive"))?;
        f.write_all(&(total + TRAILER_SIZE as u64).to_le_bytes())
            .map_err(io_err("writing archive"))?;
        f.sync_all().map_err(io_err("writing archive"))?;
        tmp.persist(&self.out_path).map_err(|e| Error::Io {
            context: format!("saving {}", self.out_path.display()),
            source: e.error,
        })?;
        Ok(self.entries)
    }
}

// ---- Sources --------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SourceItem {
    pub archive_path: String,
    pub os_path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

/// Walk files and directories. Each source is stored under its own name (a folder
/// "photos" becomes "photos/..."). Symlinks, devices, sockets and fifos are skipped:
/// the format stores regular files and directories only, by design.
pub fn collect_sources(
    sources: &[PathBuf],
    follow_symlinks: bool,
    filter: &dyn Fn(&str) -> bool,
) -> (Vec<SourceItem>, Vec<Skipped>) {
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut add = |arc: String,
                   os: PathBuf,
                   is_dir: bool,
                   size: u64,
                   items: &mut Vec<SourceItem>,
                   skipped: &mut Vec<Skipped>|
     -> bool {
        if !filter(&arc) {
            return false;
        }
        if let Err(e) = check_portable_path(&arc) {
            skipped.push(Skipped {
                path: os,
                reason: e.to_string(),
            });
            return false;
        }
        if !seen.insert(collision_key(&arc)) {
            skipped.push(Skipped {
                path: os,
                reason: format!("name collides with another entry: {arc}"),
            });
            return false;
        }
        items.push(SourceItem {
            archive_path: arc,
            os_path: os,
            is_dir,
            size,
        });
        true
    };
    for src in sources {
        let src_abs = fs::canonicalize(src).unwrap_or_else(|_| src.clone());
        let base = src
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| {
                src_abs
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "root".into())
            });
        let meta = match fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(e) => {
                skipped.push(Skipped {
                    path: src.clone(),
                    reason: e.to_string(),
                });
                continue;
            }
        };
        if meta.file_type().is_symlink() && !follow_symlinks {
            skipped.push(Skipped {
                path: src.clone(),
                reason: "symbolic link".into(),
            });
            continue;
        }
        if meta.is_dir() || (meta.file_type().is_symlink() && src.is_dir()) {
            let root_arc = archive_path_from_os(Path::new(&base));
            if root_arc.is_empty()
                || !add(
                    root_arc.clone(),
                    src.clone(),
                    true,
                    0,
                    &mut items,
                    &mut skipped,
                )
            {
                continue;
            }
            let walker = walkdir::WalkDir::new(src)
                .follow_links(follow_symlinks)
                .sort_by_file_name()
                .min_depth(1);
            for entry in walker {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        skipped.push(Skipped {
                            path: e.path().map(Path::to_path_buf).unwrap_or_default(),
                            reason: e.to_string(),
                        });
                        continue;
                    }
                };
                let rel = entry.path().strip_prefix(src).unwrap_or(entry.path());
                let arc = format!("{root_arc}/{}", archive_path_from_os(rel));
                let ft = entry.file_type();
                if ft.is_symlink() {
                    skipped.push(Skipped {
                        path: entry.path().to_path_buf(),
                        reason: "symbolic link".into(),
                    });
                    continue;
                }
                if ft.is_dir() {
                    add(
                        arc,
                        entry.path().to_path_buf(),
                        true,
                        0,
                        &mut items,
                        &mut skipped,
                    );
                } else if ft.is_file() {
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    add(
                        arc,
                        entry.path().to_path_buf(),
                        false,
                        size,
                        &mut items,
                        &mut skipped,
                    );
                } else {
                    skipped.push(Skipped {
                        path: entry.path().to_path_buf(),
                        reason: "not a regular file".into(),
                    });
                }
            }
        } else if meta.is_file() {
            add(
                archive_path_from_os(Path::new(&base)),
                src.clone(),
                false,
                meta.len(),
                &mut items,
                &mut skipped,
            );
        } else {
            skipped.push(Skipped {
                path: src.clone(),
                reason: "not a file or directory".into(),
            });
        }
    }
    (items, skipped)
}

pub struct CreateOptions<'a> {
    pub settings: &'a Settings,
    pub follow_symlinks: bool,
    pub filter: &'a dyn Fn(&str) -> bool,
}

/// Archive files/directories into `out_path`. Returns (entries, skipped).
pub fn create_archive(
    out_path: &Path,
    sources: &[PathBuf],
    opts: &CreateOptions,
    progress: Option<ArchiveProgress>,
) -> Result<(Vec<Entry>, Vec<Skipped>)> {
    let (items, skipped) = collect_sources(sources, opts.follow_symlinks, opts.filter);
    let total: u64 = items.iter().map(|i| i.size).sum();
    let mut done = 0u64;
    let mut w = ArchiveWriter::new(out_path, opts.settings)?;
    for item in &items {
        if let Some(p) = progress {
            if !p(done, total, &item.archive_path) {
                return Err(Error::Cancelled);
            }
        }
        if item.is_dir {
            let mtime = fs::metadata(&item.os_path)
                .and_then(|m| m.modified())
                .map(clamp_mtime)
                .unwrap_or(0);
            w.add_dir(&item.archive_path, mtime)?;
        } else {
            let base = done;
            let inner = |d: u64| progress.is_none_or(|p| p(base + d, total, &item.archive_path));
            let inner_ref: &(dyn Fn(u64) -> bool + Sync) = &inner;
            w.add_file(
                &item.archive_path,
                &item.os_path,
                progress.map(|_| inner_ref),
            )?;
            done += item.size;
        }
    }
    if let Some(p) = progress {
        p(total, total, "");
    }
    let entries = w.finish()?;
    Ok((entries, skipped))
}

// ---- Reading ------------------------------------------------------------------------

pub struct Archive {
    pub path: PathBuf,
    pub blob: Vec<u8>,
    pub entries: Vec<Entry>,
    pub manifest_offset: u64,
    pub manifest_length: u64,
    pub archive_hash: [u8; 32],
}

fn integrity<T>(msg: &str) -> Result<T> {
    Err(Error::Integrity(msg.to_string()))
}

fn rd_u16(b: &[u8], p: usize) -> u16 {
    u16::from_le_bytes([b[p], b[p + 1]])
}
fn rd_u32(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(b[p..p + 4].try_into().unwrap())
}
fn rd_u64(b: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(b[p..p + 8].try_into().unwrap())
}
fn rd_i64(b: &[u8], p: usize) -> i64 {
    i64::from_le_bytes(b[p..p + 8].try_into().unwrap())
}

pub fn open_archive(path: &Path) -> Result<Archive> {
    let blob = fs::read(path).map_err(io_path("reading", path))?;
    load_archive(blob, path)
}

/// Verify and parse an archive held in memory. Nothing is interpreted until the
/// whole-file hash matches.
pub fn load_archive(blob: Vec<u8>, path: &Path) -> Result<Archive> {
    let n = blob.len();
    if n < HEADER_SIZE + TRAILER_SIZE {
        return integrity("file is too small to be an .xzip archive");
    }
    let t = n - TRAILER_SIZE;
    if &blob[t..t + 8] != TRAILER_MAGIC || rd_u64(&blob, t + 40) != n as u64 {
        return integrity(
            "trailer missing or length mismatch: not an .xzip archive, or truncated/modified",
        );
    }
    let archive_hash: [u8; 32] = Sha256::digest(&blob[..t]).into();
    if archive_hash[..] != blob[t + 8..t + 40] {
        return integrity(
            "archive hash mismatch: the file was modified or corrupted; nothing was read from it",
        );
    }
    if &blob[..6] != MAGIC {
        return integrity("bad header magic");
    }
    if crc32fast::hash(&blob[..HEADER_SIZE - 4]) != rd_u32(&blob, HEADER_SIZE - 4) {
        return integrity("header CRC mismatch");
    }
    let version = rd_u16(&blob, 6);
    if version != VERSION {
        return Err(Error::Archive(format!(
            "unsupported archive version {version}"
        )));
    }
    if rd_u32(&blob, 8) != 0 {
        return Err(Error::Archive("unknown header flags".into()));
    }
    let m_off = rd_u64(&blob, 12);
    let m_len = rd_u64(&blob, 20);
    if m_off < HEADER_SIZE as u64 || m_len < 4 || m_off.checked_add(m_len) != Some(t as u64) {
        return integrity("manifest offset/length do not fit the file");
    }
    let manifest = &blob[m_off as usize..(m_off + m_len) as usize];
    if Sha256::digest(manifest)[..] != blob[28..60] {
        return integrity("manifest hash mismatch");
    }
    let count = rd_u32(manifest, 0) as u64;
    if count > MAX_ENTRIES || count.saturating_mul(ENTRY_FIXED as u64) > m_len {
        return integrity("entry count does not fit the manifest");
    }
    let mut entries = Vec::with_capacity(count as usize);
    let mut pos = 4usize;
    let mut next_offset = HEADER_SIZE as u64;
    let ml = manifest.len();
    for i in 0..count {
        if pos + ENTRY_FIXED > ml {
            return integrity(&format!("entry {i} is truncated"));
        }
        let kind = manifest[pos];
        let flags = manifest[pos + 1];
        let mtime = rd_i64(manifest, pos + 2);
        let plen = rd_u16(manifest, pos + 10) as usize;
        pos += ENTRY_FIXED;
        if pos + plen > ml {
            return integrity(&format!("entry {i} path is truncated"));
        }
        let path_str = std::str::from_utf8(&manifest[pos..pos + plen])
            .map_err(|_| Error::Integrity(format!("entry {i} path is not valid UTF-8")))?
            .to_string();
        pos += plen;
        if (kind != KIND_FILE && kind != KIND_DIR) || flags & !FLAG_EXECUTABLE != 0 {
            return integrity(&format!("entry {i} has an unknown kind or flags"));
        }
        if !(-MAX_MTIME..=MAX_MTIME).contains(&mtime) {
            return integrity(&format!("entry {i} has an out-of-range timestamp"));
        }
        check_portable_path(&path_str)?;
        let mut e = Entry {
            path: path_str,
            kind,
            flags,
            mtime,
            method: METHOD_XZ,
            data_offset: 0,
            data_length: 0,
            size: 0,
            sha_stream: [0; 32],
            sha_data: [0; 32],
        };
        if kind == KIND_FILE {
            if pos + FILE_EXTRA > ml {
                return integrity(&format!("entry {i} file record is truncated"));
            }
            e.method = manifest[pos];
            e.data_offset = rd_u64(manifest, pos + 1);
            e.data_length = rd_u64(manifest, pos + 9);
            e.size = rd_u64(manifest, pos + 17);
            e.sha_stream.copy_from_slice(&manifest[pos + 25..pos + 57]);
            e.sha_data.copy_from_slice(&manifest[pos + 57..pos + 89]);
            pos += FILE_EXTRA;
            if e.method != METHOD_XZ {
                return Err(Error::Archive(format!(
                    "entry {i} uses unknown method {}",
                    e.method
                )));
            }
            // streams must be back to back and inside the payload: no overlap, no gaps
            if e.data_offset != next_offset
                || e.data_length < 32
                || e.data_offset
                    .checked_add(e.data_length)
                    .is_none_or(|end| end > m_off)
            {
                return integrity(&format!("entry {i} stream placement is invalid"));
            }
            next_offset = e.data_offset + e.data_length;
            if e.size > DEFAULT_MAX_TOTAL_SIZE {
                return integrity(&format!("entry {i} declares an absurd size"));
            }
        }
        entries.push(e);
    }
    if pos != ml {
        return integrity("manifest has trailing bytes");
    }
    if next_offset != m_off {
        return integrity("payload has bytes that belong to no entry");
    }
    validate_entry_set(&entries)?;
    Ok(Archive {
        path: path.to_path_buf(),
        blob,
        entries,
        manifest_offset: m_off,
        manifest_length: m_len,
        archive_hash,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overwrite {
    Skip,
    Replace,
}

pub struct ExtractOptions {
    pub overwrite: Overwrite,
    pub max_entry_size: u64,
    pub max_total_size: u64,
    pub strip_components: usize,
    pub check_disk_space: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        ExtractOptions {
            overwrite: Overwrite::Skip,
            max_entry_size: DEFAULT_MAX_ENTRY_SIZE,
            max_total_size: DEFAULT_MAX_TOTAL_SIZE,
            strip_components: 0,
            check_disk_space: true,
        }
    }
}

#[derive(Default, Debug)]
pub struct ExtractReport {
    pub written: Vec<String>,
    pub skipped: Vec<String>,
}

impl Archive {
    pub fn size(&self) -> u64 {
        self.blob.len() as u64
    }

    pub fn find(&self, path: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.path == path)
    }

    /// The verified compressed stream of a file entry.
    pub fn stream(&self, e: &Entry) -> Result<&[u8]> {
        let data = &self.blob[e.data_offset as usize..(e.data_offset + e.data_length) as usize];
        if <[u8; 32]>::from(Sha256::digest(data)) != e.sha_stream {
            return Err(Error::Integrity(format!(
                "compressed data of {:?} does not match its hash",
                e.path
            )));
        }
        Ok(data)
    }

    /// Verified contents of a file: stream hash checked before decoding, exact size
    /// enforced by the decoder, contents hash checked after.
    pub fn read(&self, e: &Entry, max_size: u64) -> Result<Vec<u8>> {
        if e.is_dir() {
            return Err(Error::Archive(format!("{:?} is a directory", e.path)));
        }
        if e.size > max_size {
            return Err(Error::Archive(format!(
                "{:?} would expand to {} bytes, above the limit of {}",
                e.path, e.size, max_size
            )));
        }
        let stream = self.stream(e)?;
        let (data, _) = xz::decompress(stream, e.size as usize)?;
        if data.len() as u64 != e.size {
            return Err(Error::Integrity(format!(
                "{:?} decompressed to {} bytes, manifest says {}",
                e.path,
                data.len(),
                e.size
            )));
        }
        if <[u8; 32]>::from(Sha256::digest(&data)) != e.sha_data {
            return Err(Error::Integrity(format!(
                "contents of {:?} do not match their hash",
                e.path
            )));
        }
        Ok(data)
    }

    /// Decode every file and check it. Returns the number of files checked.
    pub fn verify(&self, progress: Option<ArchiveProgress>) -> Result<usize> {
        let files: Vec<&Entry> = self.entries.iter().filter(|e| !e.is_dir()).collect();
        let total: u64 = files.iter().map(|e| e.size).sum::<u64>().max(1);
        let mut done = 0;
        for e in &files {
            if let Some(p) = progress {
                if !p(done, total, &e.path) {
                    return Err(Error::Cancelled);
                }
            }
            self.read(e, u64::MAX)?;
            done += e.size;
        }
        Ok(files.len())
    }

    /// Extract entries (all if None) into `dest`. Every path is validated and every
    /// size checked before the first byte is written.
    pub fn extract(
        &self,
        dest: &Path,
        entries: Option<&[&Entry]>,
        opts: &ExtractOptions,
        progress: Option<ArchiveProgress>,
    ) -> Result<ExtractReport> {
        let selected: Vec<&Entry> = match entries {
            Some(list) => list.to_vec(),
            None => self.entries.iter().collect(),
        };
        let owned: Vec<Entry> = selected.iter().map(|e| (*e).clone()).collect();
        validate_entry_set(&owned)?;
        let total: u64 = selected
            .iter()
            .filter(|e| !e.is_dir())
            .map(|e| e.size)
            .sum();
        for e in &selected {
            if e.size > opts.max_entry_size {
                return Err(Error::Archive(format!(
                    "{:?} would expand to {} bytes, above the limit of {}",
                    e.path, e.size, opts.max_entry_size
                )));
            }
        }
        if total > opts.max_total_size {
            return Err(Error::Archive(format!(
                "archive would expand to {total} bytes, above the limit of {}",
                opts.max_total_size
            )));
        }
        let root = if dest.is_absolute() {
            dest.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(io_err("current directory"))?
                .join(dest)
        };
        if let Ok(meta) = fs::symlink_metadata(&root) {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(Error::Archive(format!(
                    "destination {} exists and is not a directory",
                    root.display()
                )));
            }
        }
        fs::create_dir_all(&root).map_err(io_path("creating destination", &root))?;
        let root_real = fs::canonicalize(&root).map_err(io_path("resolving destination", &root))?;
        if opts.check_disk_space {
            if let Ok(free) = fs4::available_space(&root) {
                if total.saturating_add(1 << 20) > free {
                    return Err(Error::Archive(format!(
                        "not enough free space: need {total} bytes, have {free}"
                    )));
                }
            }
        }
        let mut report = ExtractReport::default();
        let mut done = 0u64;
        let mut dir_times: Vec<(PathBuf, i64)> = Vec::new();
        let mut order: Vec<&Entry> = selected.clone();
        order.sort_by_key(|e| (!e.is_dir(), e.components().len(), e.path.clone()));
        for e in order {
            if let Some(p) = progress {
                if !p(done, total, &e.path) {
                    return Err(Error::Cancelled);
                }
            }
            let comps = e.components();
            if comps.len() <= opts.strip_components {
                continue;
            }
            let comps = &comps[opts.strip_components..];
            let parent_comps = if e.is_dir() {
                comps
            } else {
                &comps[..comps.len() - 1]
            };
            let parent = make_dirs_safely(&root, &root_real, parent_comps)?;
            if e.is_dir() {
                dir_times.push((parent, e.mtime));
                report.written.push(e.path.clone());
                continue;
            }
            let target = parent.join(comps[comps.len() - 1]);
            if let Ok(meta) = fs::symlink_metadata(&target) {
                if meta.file_type().is_symlink() || !meta.is_file() {
                    return Err(Error::UnsafePath(format!(
                        "refusing to replace a link or directory at {}",
                        target.display()
                    )));
                }
                if opts.overwrite == Overwrite::Skip {
                    report.skipped.push(e.path.clone());
                    done += e.size;
                    continue;
                }
            }
            let data = self.read(e, opts.max_entry_size)?;
            // fresh temp file beside the target, renamed into place: never written
            // through an existing file or link
            let mut tmp = tempfile::Builder::new()
                .prefix(".xzip-")
                .tempfile_in(&parent)
                .map_err(io_path("creating file in", &parent))?;
            tmp.write_all(&data).map_err(io_path("writing", &target))?;
            #[cfg(unix)]
            if e.executable() {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755));
            }
            let f = tmp.persist(&target).map_err(|err| Error::Io {
                context: format!("saving {}", target.display()),
                source: err.error,
            })?;
            let _ = f.set_modified(mtime_to_system(e.mtime));
            report.written.push(e.path.clone());
            done += e.size;
        }
        for (d, mtime) in dir_times {
            if let Ok(f) = File::open(&d) {
                let _ = f.set_modified(mtime_to_system(mtime));
            }
        }
        if let Some(p) = progress {
            p(total, total, "");
        }
        Ok(report)
    }
}

/// Create root/components one level at a time with mkdir (never through a link) and
/// prove the result is the real directory we meant: its canonical path must equal
/// the expected path under the canonical root. A link or junction planted in the
/// destination fails this.
fn make_dirs_safely(root: &Path, root_real: &Path, components: &[&str]) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    for comp in components {
        path.push(comp);
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(Error::UnsafePath(format!(
                        "{} exists and is not a real directory",
                        path.display()
                    )));
                }
            }
            Err(_) => fs::create_dir(&path).map_err(io_path("creating directory", &path))?,
        }
    }
    let mut expected = root_real.to_path_buf();
    for comp in components {
        expected.push(comp);
    }
    let actual = fs::canonicalize(&path).map_err(io_path("resolving", &path))?;
    if normalize_for_compare(&actual) != normalize_for_compare(&expected) {
        return Err(Error::UnsafePath(format!(
            "{} resolves outside the destination ({})",
            path.display(),
            actual.display()
        )));
    }
    Ok(path)
}

fn normalize_for_compare(p: &Path) -> String {
    let s = p.to_string_lossy().to_string();
    let s = s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s);
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// Write a new archive from kept entries of an existing one (streams copied after
/// verification, no recompression) plus newly added sources.
pub fn rebuild_archive(
    archive: &Archive,
    out_path: &Path,
    keep: &[&Entry],
    add: &[PathBuf],
    opts: &CreateOptions,
    progress: Option<ArchiveProgress>,
) -> Result<(Vec<Entry>, Vec<Skipped>)> {
    let (items, skipped) = collect_sources(add, opts.follow_symlinks, opts.filter);
    let total: u64 =
        items.iter().map(|i| i.size).sum::<u64>() + keep.iter().map(|e| e.data_length).sum::<u64>();
    let mut done = 0u64;
    let mut w = ArchiveWriter::new(out_path, opts.settings)?;
    for e in keep {
        if let Some(p) = progress {
            if !p(done, total, &e.path) {
                return Err(Error::Cancelled);
            }
        }
        if e.is_dir() {
            w.add_dir(&e.path, e.mtime)?;
        } else {
            w.add_stream(e, archive.stream(e)?)?;
            done += e.data_length;
        }
    }
    for item in &items {
        if let Some(p) = progress {
            if !p(done, total, &item.archive_path) {
                return Err(Error::Cancelled);
            }
        }
        if item.is_dir {
            let mtime = fs::metadata(&item.os_path)
                .and_then(|m| m.modified())
                .map(clamp_mtime)
                .unwrap_or(0);
            w.add_dir(&item.archive_path, mtime)?;
        } else {
            let base = done;
            let inner = |d: u64| progress.is_none_or(|p| p(base + d, total, &item.archive_path));
            let inner_ref: &(dyn Fn(u64) -> bool + Sync) = &inner;
            w.add_file(
                &item.archive_path,
                &item.os_path,
                progress.map(|_| inner_ref),
            )?;
            done += item.size;
        }
    }
    let entries = w.finish()?;
    Ok((entries, skipped))
}
