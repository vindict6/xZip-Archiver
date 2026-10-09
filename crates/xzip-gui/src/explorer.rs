//! Windows Explorer integration: file type, icon and context menu verbs, written
//! to the current user's registry (HKCU\Software\Classes) so no elevation is needed.
//! The verbs just launch this executable with a switch; see main.rs.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Integration {
    /// .xzip opens with xZip and shows its icon
    pub file_type: bool,
    /// Extract here / Extract to folder / Test on .xzip files
    pub archive_menu: bool,
    /// "Add to xZip archive…" on every file and folder
    pub add_menu: bool,
}

pub const LAUNCH_HELP: &str = "\
--register-explorer         file type + both context menus
--register-file-type        .xzip association and icon only
--register-context-menu     context menus only
--unregister-explorer       remove everything
--unregister-context-menu   remove the context menus only
--explorer-status           show what is registered
--extract-here ARCHIVE      extract next to the archive
--extract-to ARCHIVE        extract into a folder named after it
--test ARCHIVE              open and run the integrity check
--add PATH...               start a new archive from these paths";

#[cfg(windows)]
mod imp {
    use super::Integration;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
    use winreg::RegKey;

    const PROGID: &str = "xZip.Archive";

    fn classes() -> std::io::Result<RegKey> {
        RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey("Software\\Classes")
            .map(|(k, _)| k)
    }

    fn exe() -> std::io::Result<String> {
        Ok(std::env::current_exe()?.to_string_lossy().to_string())
    }

    fn set(root: &RegKey, path: &str, values: &[(&str, String)]) -> std::io::Result<()> {
        let (k, _) = root.create_subkey(path)?;
        for (name, v) in values {
            k.set_value(name, v)?;
        }
        Ok(())
    }

    fn verb(
        root: &RegKey,
        base: &str,
        id: &str,
        label: &str,
        args: &str,
        exe: &str,
    ) -> std::io::Result<()> {
        let path = format!("{base}\\shell\\{id}");
        set(
            root,
            &path,
            &[("", label.to_string()), ("Icon", format!("\"{exe}\",0"))],
        )?;
        set(
            root,
            &format!("{path}\\command"),
            &[("", format!("\"{exe}\" {args}"))],
        )
    }

    fn remove(root: &RegKey, path: &str) {
        let _ = root.delete_subkey_all(path);
    }

    fn exists(root: &RegKey, path: &str) -> bool {
        root.open_subkey_with_flags(path, KEY_READ).is_ok()
    }

    pub fn status() -> Integration {
        let Ok(root) = classes() else {
            return Integration::default();
        };
        let assoc: Option<String> = root
            .open_subkey_with_flags(".xzip", KEY_READ)
            .and_then(|k| k.get_value(""))
            .ok();
        Integration {
            file_type: assoc.as_deref() == Some(PROGID)
                && exists(&root, &format!("{PROGID}\\shell\\open\\command")),
            archive_menu: exists(&root, &format!("{PROGID}\\shell\\xzip.extracthere")),
            add_menu: exists(&root, "*\\shell\\xzip.add"),
        }
    }

    pub fn apply(want: Integration) -> Result<(), String> {
        let run = || -> std::io::Result<()> {
            let root = classes()?;
            let exe = exe()?;
            if want.file_type || want.archive_menu {
                set(
                    &root,
                    ".xzip",
                    &[
                        ("", PROGID.into()),
                        ("Content Type", "application/x-xzip".into()),
                    ],
                )?;
                set(&root, ".xzip\\OpenWithProgids", &[(PROGID, String::new())])?;
                set(
                    &root,
                    PROGID,
                    &[
                        ("", "xZip Archive".into()),
                        ("FriendlyTypeName", "xZip Archive".into()),
                    ],
                )?;
                set(
                    &root,
                    &format!("{PROGID}\\DefaultIcon"),
                    &[("", format!("\"{exe}\",0"))],
                )?;
                set(
                    &root,
                    &format!("{PROGID}\\shell\\open"),
                    &[
                        ("", "Open with xZip".into()),
                        ("Icon", format!("\"{exe}\",0")),
                    ],
                )?;
                set(
                    &root,
                    &format!("{PROGID}\\shell\\open\\command"),
                    &[("", format!("\"{exe}\" \"%1\""))],
                )?;
            } else {
                remove(&root, PROGID);
                remove(&root, ".xzip");
            }
            if want.archive_menu {
                verb(
                    &root,
                    PROGID,
                    "xzip.extracthere",
                    "Extract here",
                    "--extract-here \"%1\"",
                    &exe,
                )?;
                verb(
                    &root,
                    PROGID,
                    "xzip.extractto",
                    "Extract to folder",
                    "--extract-to \"%1\"",
                    &exe,
                )?;
                verb(
                    &root,
                    PROGID,
                    "xzip.test",
                    "Test archive",
                    "--test \"%1\"",
                    &exe,
                )?;
            } else {
                for id in ["xzip.extracthere", "xzip.extractto", "xzip.test"] {
                    remove(&root, &format!("{PROGID}\\shell\\{id}"));
                }
            }
            if want.add_menu {
                verb(
                    &root,
                    "*",
                    "xzip.add",
                    "Add to xZip archive…",
                    "--add \"%1\"",
                    &exe,
                )?;
                verb(
                    &root,
                    "Directory",
                    "xzip.add",
                    "Add to xZip archive…",
                    "--add \"%1\"",
                    &exe,
                )?;
            } else {
                remove(&root, "*\\shell\\xzip.add");
                remove(&root, "Directory\\shell\\xzip.add");
            }
            Ok(())
        };
        run().map_err(|e| e.to_string())?;
        refresh();
        Ok(())
    }

    /// Tell Explorer the associations changed so icons update without a sign-out.
    fn refresh() {
        #[link(name = "shell32")]
        extern "system" {
            fn SHChangeNotify(event: i32, flags: u32, item1: *const u8, item2: *const u8);
        }
        const SHCNE_ASSOCCHANGED: i32 = 0x0800_0000;
        // SAFETY: documented call with null item pointers, which this event allows.
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, 0, std::ptr::null(), std::ptr::null()) };
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Integration;

    pub fn status() -> Integration {
        Integration::default()
    }

    pub fn apply(_want: Integration) -> Result<(), String> {
        Err("Explorer integration is a Windows feature.".into())
    }
}

pub use imp::{apply, status};

/// Collect the paths of a multi-selection "Add to archive". Explorer starts one
/// process per selected item, so each one drops its path into a spool file and the
/// first to grab the lock waits briefly, gathers them all and carries on; the
/// others exit.
pub fn gather_add_paths(mine: Vec<std::path::PathBuf>) -> Option<Vec<std::path::PathBuf>> {
    use std::io::Write;
    let dir = std::env::temp_dir();
    let spool = dir.join("xzip-add-spool.txt");
    let lock = dir.join("xzip-add-spool.lock");
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&spool)
        .ok()?;
    for p in &mine {
        let _ = writeln!(f, "{}", p.display());
    }
    drop(f);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
    {
        Ok(_) => {}
        Err(_) => return None, // another instance owns the batch
    }
    std::thread::sleep(std::time::Duration::from_millis(800));
    let batch = dir.join(format!("xzip-add-{}.txt", std::process::id()));
    let _ = std::fs::rename(&spool, &batch);
    let _ = std::fs::remove_file(&lock);
    let text = std::fs::read_to_string(&batch).unwrap_or_default();
    let _ = std::fs::remove_file(&batch);
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let p = std::path::PathBuf::from(line.trim());
        if !out.contains(&p) {
            out.push(p);
        }
    }
    if out.is_empty() {
        out = mine;
    }
    Some(out)
}
