//! xZip Archiver - desktop app.
//!
//! Copyright (c) 2026 Clinton Turner.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod explorer;
mod theme;

use std::path::PathBuf;

/// What to do when the window opens, from the command line or an Explorer verb.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pending {
    ExtractHere,
    ExtractTo,
    Test,
}

#[derive(Default, Debug)]
pub struct Launch {
    pub open: Option<PathBuf>,
    pub add: Vec<PathBuf>,
    pub pending: Option<Pending>,
    pub screenshot: Option<PathBuf>,
}

fn message(title: &str, text: &str, error: bool) {
    rfd::MessageDialog::new()
        .set_title(title)
        .set_description(text)
        .set_level(if error {
            rfd::MessageLevel::Error
        } else {
            rfd::MessageLevel::Info
        })
        .show();
}

/// Registry switches run and exit; everything else becomes a Launch.
fn parse_args() -> Option<Launch> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut launch = Launch::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let next = |i: usize| args.get(i + 1).map(PathBuf::from);
        match a {
            "--register-explorer"
            | "--register-file-type"
            | "--register-context-menu"
            | "--unregister-explorer"
            | "--unregister-context-menu" => {
                let mut want = explorer::status();
                match a {
                    "--register-explorer" => {
                        want = explorer::Integration {
                            file_type: true,
                            archive_menu: true,
                            add_menu: true,
                        }
                    }
                    "--register-file-type" => want.file_type = true,
                    "--register-context-menu" => {
                        want.archive_menu = true;
                        want.add_menu = true;
                    }
                    "--unregister-explorer" => want = explorer::Integration::default(),
                    _ => {
                        want.archive_menu = false;
                        want.add_menu = false;
                    }
                }
                let quiet = args.iter().any(|x| x == "--quiet" || x == "-q");
                match explorer::apply(want) {
                    Ok(()) => {
                        if !quiet {
                            message(
                                "xZip Archiver",
                                &format!("Explorer integration updated.\n\n{}", describe(want)),
                                false,
                            );
                        }
                        std::process::exit(0);
                    }
                    Err(e) => {
                        if !quiet {
                            message(
                                "xZip Archiver",
                                &format!("Could not update Explorer integration:\n{e}"),
                                true,
                            );
                        }
                        std::process::exit(1);
                    }
                }
            }
            "--explorer-status" => {
                message("xZip Archiver", &describe(explorer::status()), false);
                std::process::exit(0);
            }
            "--help" | "-h" => {
                message(
                    "xZip Archiver",
                    &format!("xzip-gui [ARCHIVE] [switch]\n\n{}", explorer::LAUNCH_HELP),
                    false,
                );
                std::process::exit(0);
            }
            "--screenshot" => {
                launch.screenshot = next(i);
                i += 1;
            }
            "--extract-here" | "--extract-to" | "--test" | "--open" => {
                launch.open = next(i);
                launch.pending = match a {
                    "--extract-here" => Some(Pending::ExtractHere),
                    "--extract-to" => Some(Pending::ExtractTo),
                    "--test" => Some(Pending::Test),
                    _ => None,
                };
                i += 1;
            }
            "--add" => {
                let mine: Vec<PathBuf> = args[i + 1..].iter().map(PathBuf::from).collect();
                launch.add = explorer::gather_add_paths(mine)?;
                break;
            }
            "--quiet" | "-q" => {}
            _ => {
                if launch.open.is_none() && std::path::Path::new(a).is_file() {
                    launch.open = Some(PathBuf::from(a));
                }
            }
        }
        i += 1;
    }
    Some(launch)
}

fn describe(s: explorer::Integration) -> String {
    let yn = |b: bool| if b { "yes" } else { "no" };
    format!(
        ".xzip file type and icon: {}\nExtract / Test menu on .xzip files: {}\n\"Add to xZip archive\" on files and folders: {}",
        yn(s.file_type),
        yn(s.archive_menu),
        yn(s.add_menu)
    )
}

fn main() -> eframe::Result {
    let Some(launch) = parse_args() else {
        return Ok(());
    };
    let icon = {
        let img = image::load_from_memory(include_bytes!("../../../assets/icon.png"))
            .expect("embedded icon")
            .to_rgba8();
        let (w, h) = img.dimensions();
        egui::IconData {
            rgba: img.into_raw(),
            width: w,
            height: h,
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("xZip Archiver")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([820.0, 520.0])
            .with_icon(icon)
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "xZip Archiver",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, launch)))),
    )
}
