//! Archive format: round trips, tampering, hostile manifests, extraction traps.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use xzip_core::archive::*;
use xzip_core::{Error, Settings};

fn settings() -> Settings {
    Settings {
        auto_props: false,
        ..Settings::level(1)
    }
}

fn opts(s: &Settings) -> CreateOptions<'_> {
    CreateOptions {
        settings: s,
        follow_symlinks: false,
        filter: &|_| true,
    }
}

fn pseudo_random(n: usize, mut x: u64) -> Vec<u8> {
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

struct Tree {
    dir: tempfile::TempDir,
    files: Vec<(String, Vec<u8>)>,
}

fn make_tree() -> Tree {
    let dir = tempfile::tempdir().unwrap();
    let long_name = "x".repeat(200);
    let long_rel = format!("project/{0}/{0}/{0}/deep.txt", long_name);
    let files = vec![
        (
            "project/readme.txt".to_string(),
            b"hello xzip\n".repeat(200),
        ),
        (
            "project/data/nested/binary.bin".to_string(),
            pseudo_random(50_000, 9),
        ),
        ("project/data/nested/empty".to_string(), Vec::new()),
        ("project/data/zeros.dat".to_string(), vec![0u8; 100_000]),
        (
            "project/caf\u{e9} r\u{e9}sum\u{e9}.txt".to_string(),
            "unicode \u{fc}\u{f1}\u{ee}\u{e7}\u{f8}\u{2603}"
                .repeat(50)
                .into_bytes(),
        ),
        (long_rel, b"deep file\n".to_vec()),
    ];
    for (rel, data) in &files {
        let p = dir
            .path()
            .join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, data).unwrap();
    }
    Tree { dir, files }
}

fn read_tree_file(root: &Path, rel: &str) -> Option<Vec<u8>> {
    fs::read(root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))).ok()
}

#[test]
fn round_trip_and_rebuild() {
    let t = make_tree();
    let s = settings();
    let arc = t.dir.path().join("test.xzip");
    let (entries, skipped) =
        create_archive(&arc, &[t.dir.path().join("project")], &opts(&s), None).unwrap();
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(
        entries.iter().filter(|e| !e.is_dir()).count(),
        t.files.len()
    );
    assert!(
        entries.iter().any(|e| e.path.len() > 600),
        "long path stored"
    );

    let a = open_archive(&arc).unwrap();
    assert_eq!(a.verify(None).unwrap(), t.files.len());
    let out = t.dir.path().join("out");
    let r = a
        .extract(&out, None, &ExtractOptions::default(), None)
        .unwrap();
    assert_eq!(r.written.len(), a.entries.len());
    assert!(r.skipped.is_empty());
    for (rel, data) in &t.files {
        assert_eq!(
            read_tree_file(&out, rel).as_deref(),
            Some(data.as_slice()),
            "{rel}"
        );
    }
    // existing files are skipped, then replaced on request
    let r2 = a
        .extract(&out, None, &ExtractOptions::default(), None)
        .unwrap();
    assert_eq!(r2.skipped.len(), t.files.len());
    let r3 = a
        .extract(
            &out,
            None,
            &ExtractOptions {
                overwrite: Overwrite::Replace,
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert!(r3.skipped.is_empty());
    // subset
    let sel: Vec<&Entry> = a
        .entries
        .iter()
        .filter(|e| e.path.starts_with("project/data/nested"))
        .collect();
    let out2 = t.dir.path().join("out2");
    let r4 = a
        .extract(&out2, Some(&sel), &ExtractOptions::default(), None)
        .unwrap();
    assert_eq!(r4.written.len(), sel.len());
    assert!(out2.join("project/data/nested/binary.bin").exists());
    // strip components
    let out3 = t.dir.path().join("out3");
    a.extract(
        &out3,
        Some(&sel),
        &ExtractOptions {
            strip_components: 2,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    assert!(out3.join("nested/binary.bin").exists());
    // direct read
    let e = a.find("project/readme.txt").unwrap();
    assert_eq!(a.read(e, u64::MAX).unwrap(), t.files[0].1);
    // rebuild: drop one, add one
    let keep: Vec<&Entry> = a
        .entries
        .iter()
        .filter(|e| e.path != "project/data/zeros.dat")
        .collect();
    let extra = t.dir.path().join("extra.txt");
    fs::write(&extra, b"added later").unwrap();
    let arc2 = t.dir.path().join("test2.xzip");
    rebuild_archive(&a, &arc2, &keep, &[extra], &opts(&s), None).unwrap();
    let a2 = open_archive(&arc2).unwrap();
    assert!(a2.find("project/data/zeros.dat").is_none());
    assert_eq!(
        a2.read(a2.find("extra.txt").unwrap(), u64::MAX).unwrap(),
        b"added later"
    );
    assert_eq!(a2.verify(None).unwrap(), t.files.len());
}

#[test]
fn tampering_is_refused_before_parsing() {
    let t = make_tree();
    let arc = t.dir.path().join("t.xzip");
    create_archive(
        &arc,
        &[t.dir.path().join("project")],
        &opts(&settings()),
        None,
    )
    .unwrap();
    let blob = fs::read(&arc).unwrap();
    let n = blob.len();
    for (where_, label) in [
        (HEADER_SIZE + 10, "payload"),
        (5, "header"),
        (n - TRAILER_SIZE - 10, "manifest"),
        (n - 20, "trailer hash"),
    ] {
        let mut b = blob.clone();
        b[where_] ^= 1;
        match load_archive(b, &arc) {
            Err(Error::Integrity(_)) => {}
            other => panic!("tampered {label}: {:?}", other.map(|_| ())),
        }
    }
    assert!(matches!(
        load_archive(blob[..n - 1].to_vec(), &arc),
        Err(Error::Integrity(_))
    ));
    let mut appended = blob.clone();
    appended.push(0);
    assert!(matches!(
        load_archive(appended, &arc),
        Err(Error::Integrity(_))
    ));
    assert!(matches!(
        load_archive(Vec::new(), &arc),
        Err(Error::Integrity(_))
    ));
    assert!(matches!(
        load_archive(pseudo_random(500, 1), &arc),
        Err(Error::Integrity(_))
    ));

    // consistent re-hash of a tampered payload: the per-stream hash catches it at read time
    let a = load_archive(blob.clone(), &arc).unwrap();
    let e0 = a
        .entries
        .iter()
        .find(|e| !e.is_dir() && e.size > 0)
        .unwrap()
        .clone();
    let mut b = blob.clone();
    b[e0.data_offset as usize + 20] ^= 0xFF;
    let body = b[..n - TRAILER_SIZE].to_vec();
    let mut rehashed = body.clone();
    rehashed.extend_from_slice(TRAILER_MAGIC);
    rehashed.extend_from_slice(&Sha256::digest(&body));
    rehashed.extend_from_slice(&(n as u64).to_le_bytes());
    let a3 = load_archive(rehashed, &arc).unwrap();
    assert!(matches!(a3.read(&e0, u64::MAX), Err(Error::Integrity(_))));
    assert!(matches!(
        a3.extract(
            &t.dir.path().join("o"),
            Some(&[&e0]),
            &ExtractOptions::default(),
            None
        ),
        Err(Error::Integrity(_))
    ));
}

/// Hand-build an archive from raw entries so hostile manifests can be tested.
fn build_raw(entries: &[(Entry, Vec<u8>)]) -> Vec<u8> {
    let mut payload = Vec::new();
    let mut ents = Vec::new();
    for (e, stream) in entries {
        let mut e = e.clone();
        if e.kind == KIND_FILE {
            e.data_offset = (HEADER_SIZE + payload.len()) as u64;
            e.data_length = stream.len() as u64;
            e.sha_stream = Sha256::digest(stream).into();
            payload.extend_from_slice(stream);
        }
        ents.push(e);
    }
    let mut manifest = (ents.len() as u32).to_le_bytes().to_vec();
    for e in &ents {
        manifest.push(e.kind);
        manifest.push(e.flags);
        manifest.extend_from_slice(&e.mtime.to_le_bytes());
        manifest.extend_from_slice(&(e.path.len() as u16).to_le_bytes());
        manifest.extend_from_slice(e.path.as_bytes());
        if e.kind == KIND_FILE {
            manifest.push(e.method);
            manifest.extend_from_slice(&e.data_offset.to_le_bytes());
            manifest.extend_from_slice(&e.data_length.to_le_bytes());
            manifest.extend_from_slice(&e.size.to_le_bytes());
            manifest.extend_from_slice(&e.sha_stream);
            manifest.extend_from_slice(&e.sha_data);
        }
    }
    let mut header = Vec::new();
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&VERSION.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    header.extend_from_slice(&((HEADER_SIZE + payload.len()) as u64).to_le_bytes());
    header.extend_from_slice(&(manifest.len() as u64).to_le_bytes());
    header.extend_from_slice(&Sha256::digest(&manifest));
    let crc = crc32fast::hash(&header);
    header.extend_from_slice(&crc.to_le_bytes());
    let mut body = header;
    body.extend_from_slice(&payload);
    body.extend_from_slice(&manifest);
    let total = body.len() + TRAILER_SIZE;
    let mut out = body.clone();
    out.extend_from_slice(TRAILER_MAGIC);
    out.extend_from_slice(&Sha256::digest(&body));
    out.extend_from_slice(&(total as u64).to_le_bytes());
    out
}

fn dir_entry(path: &str) -> Entry {
    Entry {
        path: path.into(),
        kind: KIND_DIR,
        flags: 0,
        mtime: 0,
        method: METHOD_XZ,
        data_offset: 0,
        data_length: 0,
        size: 0,
        sha_stream: [0; 32],
        sha_data: [0; 32],
    }
}

fn file_entry(path: &str, data: &[u8], size: u64) -> (Entry, Vec<u8>) {
    let (stream, _, _) =
        xzip_core::xz::compress(data, &settings(), xzip_core::Check::Crc64, None).unwrap();
    let e = Entry {
        path: path.into(),
        kind: KIND_FILE,
        flags: 0,
        mtime: 0,
        method: METHOD_XZ,
        data_offset: 0,
        data_length: 0,
        size,
        sha_stream: [0; 32],
        sha_data: Sha256::digest(data).into(),
    };
    (e, stream)
}

#[test]
fn hostile_manifests() {
    let p = PathBuf::from("x.xzip");
    for bad in [
        "../evil",
        "a/../../evil",
        "/abs/path",
        "C:/win",
        "a\\b",
        "a/./b",
        "",
        "a//b",
        "CON",
        "dir/NUL.txt",
        "LPT1.anything",
        "name.",
        "name ",
        "ok/bad:name",
        "a?b",
        "x|y",
        "tab\there",
        "nul\0byte",
        "a\u{202e}b",
        "zero\u{200b}width",
    ] {
        let blob = build_raw(&[(dir_entry(bad), Vec::new())]);
        assert!(
            matches!(load_archive(blob, &p), Err(Error::UnsafePath(_))),
            "{bad:?}"
        );
    }
    let too_long = "x".repeat(256);
    assert!(load_archive(build_raw(&[(dir_entry(&too_long), Vec::new())]), &p).is_err());
    assert!(matches!(
        load_archive(
            build_raw(&[(dir_entry("Readme"), vec![]), (dir_entry("readme"), vec![])]),
            &p
        ),
        Err(Error::UnsafePath(_))
    ));
    assert!(matches!(
        load_archive(
            build_raw(&[(dir_entry("fi"), vec![]), (dir_entry("\u{fb01}"), vec![])]),
            &p
        ),
        Err(Error::UnsafePath(_))
    ));
    let (f, st) = file_entry("a", b"xxxxxxxxxx", 10);
    assert!(matches!(
        load_archive(build_raw(&[(f, st), (dir_entry("a/b"), vec![])]), &p),
        Err(Error::UnsafePath(_))
    ));

    // size lie: refused at read time
    let (liar, st) = file_entry("liar", b"xxxxxxxxxx", 11);
    let a = load_archive(build_raw(&[(liar, st)]), &p).unwrap();
    assert!(matches!(
        a.read(&a.entries[0], u64::MAX),
        Err(Error::Integrity(_)) | Err(Error::Corrupt(_))
    ));
    // caps
    let (f, st) = file_entry("bomb", b"xxxxxxxxxx", 10);
    let a = load_archive(build_raw(&[(f, st)]), &p).unwrap();
    let d = tempfile::tempdir().unwrap();
    assert!(matches!(
        a.extract(
            d.path(),
            None,
            &ExtractOptions {
                max_entry_size: 5,
                ..Default::default()
            },
            None
        ),
        Err(Error::Archive(_))
    ));
    assert!(matches!(
        a.extract(
            d.path(),
            None,
            &ExtractOptions {
                max_total_size: 5,
                ..Default::default()
            },
            None
        ),
        Err(Error::Archive(_))
    ));
    // absurd declared size
    let (f, st) = file_entry("huge", b"xxxxxxxxxx", 1 << 50);
    assert!(matches!(
        load_archive(build_raw(&[(f, st)]), &p),
        Err(Error::Integrity(_))
    ));
    // overlapping streams: second entry pointing at the first stream
    let (f1, s1) = file_entry("f1", b"xxxxxxxxxx", 10);
    let (f2, s2) = file_entry("f2", b"xxxxxxxxxx", 10);
    let mut blob = build_raw(&[(f1.clone(), s1.clone()), (f2.clone(), s2.clone())]);
    // forge by rewriting entry 2's offset inside the manifest and re-sealing
    let a = load_archive(blob.clone(), &p).unwrap();
    let m_off = a.manifest_offset as usize;
    let needle = (a.entries[1].data_offset).to_le_bytes();
    let idx = blob[m_off..].windows(8).position(|w| w == needle).unwrap() + m_off;
    blob[idx..idx + 8].copy_from_slice(&a.entries[0].data_offset.to_le_bytes());
    let n = blob.len();
    let body = blob[..n - TRAILER_SIZE].to_vec();
    let mut forged = body.clone();
    // header manifest hash must be re-sealed too
    let mh: [u8; 32] = Sha256::digest(&body[m_off..]).into();
    forged[28..60].copy_from_slice(&mh);
    let crc = crc32fast::hash(&forged[..HEADER_SIZE - 4]);
    forged[HEADER_SIZE - 4..HEADER_SIZE].copy_from_slice(&crc.to_le_bytes());
    let body2 = forged.clone();
    forged.extend_from_slice(TRAILER_MAGIC);
    forged.extend_from_slice(&Sha256::digest(&body2));
    forged.extend_from_slice(&(n as u64).to_le_bytes());
    assert!(matches!(load_archive(forged, &p), Err(Error::Integrity(_))));
}

#[test]
fn destination_traps() {
    let t = make_tree();
    let arc = t.dir.path().join("t.xzip");
    create_archive(
        &arc,
        &[t.dir.path().join("project")],
        &opts(&settings()),
        None,
    )
    .unwrap();
    let a = open_archive(&arc).unwrap();
    // destination is a file
    let f = t.dir.path().join("afile");
    fs::write(&f, b"x").unwrap();
    assert!(matches!(
        a.extract(&f, None, &ExtractOptions::default(), None),
        Err(Error::Archive(_))
    ));
    // a planted link where a folder should go
    let trap = t.dir.path().join("trap");
    let victim = t.dir.path().join("victim");
    fs::create_dir_all(&trap).unwrap();
    fs::create_dir_all(&victim).unwrap();
    let linked = make_dir_link(&victim, &trap.join("project"));
    if linked {
        assert!(matches!(
            a.extract(&trap, None, &ExtractOptions::default(), None),
            Err(Error::UnsafePath(_))
        ));
        assert!(
            fs::read_dir(&victim).unwrap().next().is_none(),
            "victim untouched"
        );
    } else {
        eprintln!("(link test skipped: cannot create links here)");
    }
}

#[cfg(windows)]
fn make_dir_link(target: &Path, link: &Path) -> bool {
    if std::os::windows::fs::symlink_dir(target, link).is_ok() {
        return true;
    }
    // junctions need no privilege
    std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn make_dir_link(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

#[test]
fn non_ascii_names_round_trip() {
    // Chinese, Japanese, fullwidth punctuation, accents: stored as NFC UTF-8, extracted intact
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("\u{8d44}\u{6599}"); // 资料
    fs::create_dir_all(src.join("\u{5199}\u{771f}")).unwrap(); // 写真
    let names = [
        "\u{62a5}\u{544a}\u{ff04}100.txt",
        "caf\u{e9} \u{2013} r\u{e9}sum\u{e9}.md",
        "\u{5199}\u{771f}/\u{6771}\u{4eac}.jpg",
        "\u{d55c}\u{ae00}.bin",
    ];
    for n in names {
        fs::write(
            src.join(n.replace('/', std::path::MAIN_SEPARATOR_STR)),
            n.as_bytes(),
        )
        .unwrap();
    }
    let arc = d.path().join("u.xzip");
    let (entries, skipped) =
        create_archive(&arc, std::slice::from_ref(&src), &opts(&settings()), None).unwrap();
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(entries.iter().filter(|e| !e.is_dir()).count(), names.len());
    let a = open_archive(&arc).unwrap();
    let out = d.path().join("out");
    a.extract(&out, None, &ExtractOptions::default(), None)
        .unwrap();
    for n in names {
        let p = out
            .join("\u{8d44}\u{6599}")
            .join(n.replace('/', std::path::MAIN_SEPARATOR_STR));
        assert_eq!(fs::read(&p).unwrap(), n.as_bytes(), "{n}");
    }
    assert!(a
        .find("\u{8d44}\u{6599}/\u{62a5}\u{544a}\u{ff04}100.txt")
        .is_some());
}

#[test]
fn creation_enforces_portable_names() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("good.txt"), b"ok").unwrap();
    #[cfg(unix)]
    {
        fs::write(src.join("bad:colon"), b"x").unwrap();
        fs::write(src.join("NUL"), b"x").unwrap();
        std::os::unix::fs::symlink(src.join("good.txt"), src.join("link")).unwrap();
    }
    let arc = d.path().join("p.xzip");
    let (entries, skipped) =
        create_archive(&arc, std::slice::from_ref(&src), &opts(&settings()), None).unwrap();
    assert!(entries
        .iter()
        .all(|e| xzip_core::paths::check_portable_path(&e.path).is_ok()));
    #[cfg(unix)]
    assert!(skipped.len() >= 3, "{skipped:?}");
    #[cfg(not(unix))]
    assert!(skipped.is_empty());
}
