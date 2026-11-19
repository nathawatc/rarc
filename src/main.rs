mod archive;
mod ui;

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gtk4 as gtk;
use gtk::gio;
use gtk::prelude::*;

use crate::archive::{
    extract_archive, list_archive, resolve_extract_here_dir, spawn_worker, ArchiveError,
    OperationSummary,
};
use crate::ui::dialogs::{run_worker_with_progress, show_create_dialog};
use crate::ui::show_error_dialog;

#[derive(Clone, Copy, Debug)]
enum TapAction {
    ExtractHere,
    ExtractTo,
    Create,
}

#[derive(Debug)]
enum AppMode {
    Standalone(Option<PathBuf>),
    Tap(TapAction, Vec<PathBuf>),
    Help,
    Version,
}

fn parse_args() -> Result<AppMode, lexopt::Error> {
    use lexopt::prelude::*;

    let mut parser = lexopt::Parser::from_env();
    let mut tap_action: Option<TapAction> = None;
    let mut paths: Vec<PathBuf> = Vec::new();

    while let Some(arg) = parser.next()? {
        match arg {
            Short('h') | Long("help") => return Ok(AppMode::Help),
            Short('V') | Long("version") => return Ok(AppMode::Version),
            Long("tap") => {
                let val = parser.value()?;
                let s = val.to_string_lossy();
                tap_action = Some(match s.as_ref() {
                    "extract-here" => TapAction::ExtractHere,
                    "extract-to" => TapAction::ExtractTo,
                    "create" => TapAction::Create,
                    other => {
                        return Err(lexopt::Error::UnexpectedValue {
                            option: "--tap".to_string(),
                            value: other.into(),
                        });
                    }
                });
            }
            Value(val) => paths.push(PathBuf::from(val)),
            _ => return Err(arg.unexpected()),
        }
    }

    if let Some(action) = tap_action {
        if paths.is_empty() {
            return Err(lexopt::Error::MissingValue {
                option: Some("paths".to_string()),
            });
        }
        Ok(AppMode::Tap(action, paths))
    } else {
        if paths.len() > 1 {
            return Err(lexopt::Error::UnexpectedArgument(
                paths[1].as_os_str().to_os_string(),
            ));
        }
        Ok(AppMode::Standalone(paths.into_iter().next()))
    }
}

fn main() -> ExitCode {
    let mode = match parse_args() {
        Ok(m) => m,
        Err(err) => {
            eprintln!("rarc: {err}\nTry 'rarc --help' for usage.");
            return ExitCode::from(2);
        }
    };

    match mode {
        AppMode::Help => {
            println!(
                "Usage:\n  rarc [ARCHIVE]\n  rarc --tap <extract-here|extract-to|create> <PATHS...>"
            );
            ExitCode::SUCCESS
        }
        AppMode::Version => {
            println!("rarc {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        mode => {
            // NON_UNIQUE ensures --tap invocations always run in their own lifecycle without
            // delegating to an already-open standalone viewer instance.
            let app = gtk::Application::builder()
                .application_id("org.rarc.ArchiveManager")
                .flags(gio::ApplicationFlags::NON_UNIQUE)
                .build();

            let mode_cell = Rc::new(RefCell::new(Some(mode)));
            app.connect_activate(move |app| {
                ui::load_css();
                let Some(taken) = mode_cell.borrow_mut().take() else {
                    return;
                };
                match taken {
                    AppMode::Standalone(path) => ui::viewer::launch_standalone(app, path),
                    AppMode::Tap(action, paths) => run_tap_mode(app, action, paths),
                    AppMode::Help | AppMode::Version => {}
                }
            });

            let status = app.run_with_args::<&str>(&[]);
            ui::cleanup_session_temp_dir();
            status.into()
        }
    }
}

fn run_tap_mode(app: &gtk::Application, action: TapAction, paths: Vec<PathBuf>) {
    match action {
        TapAction::Create => {
            let cwd = std::env::current_dir().ok();
            show_create_dialog(Some(app), None, paths, cwd, |_| {});
        }
        TapAction::ExtractHere => {
            let cancel = Arc::new(AtomicBool::new(false));
            let cancel_worker = cancel.clone();
            let rx = spawn_worker(move |tx| {
                for archive in paths {
                    let abs_archive = absolutize_path(&archive);
                    let listing = list_archive(&abs_archive, None, Some(tx), &cancel_worker)?;
                    let dest_dir = resolve_extract_here_dir(&listing.entries, &abs_archive);
                    extract_archive(
                        &abs_archive,
                        &dest_dir,
                        None,
                        None,
                        listing.password.as_deref(),
                        Some(tx),
                        &cancel_worker,
                    )?;
                }
                Ok(OperationSummary::Extracted { password: None })
            });

            run_worker_with_progress(Some(app), None, "Extracting Here", rx, cancel, |res| {
                if let Err(err) = res
                    && !matches!(err, ArchiveError::Cancelled)
                {
                    show_error_dialog(None::<&gtk::Window>, "Extraction Failed", &err.to_string());
                }
            });
        }
        TapAction::ExtractTo => {
            let app_hold = Rc::new(RefCell::new(Some(app.hold())));
            let app_cloned = app.clone();
            let initial_dir = absolutize_path(&paths[0])
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(gio::File::for_path);
            let mut builder = gtk::FileDialog::builder().title("Extract To Folder");
            if let Some(ref folder) = initial_dir {
                builder = builder.initial_folder(folder);
            }
            let dialog = builder.build();

            dialog.select_folder(None::<&gtk::Window>, gio::Cancellable::NONE, move |res| {
                let Ok(folder) = res else {
                    *app_hold.borrow_mut() = None;
                    return;
                };
                let Some(dest_dir) = folder.path() else {
                    *app_hold.borrow_mut() = None;
                    return;
                };

                let cancel = Arc::new(AtomicBool::new(false));
                let cancel_worker = cancel.clone();
                let dest_worker = dest_dir.clone();
                let rx = spawn_worker(move |tx| {
                    for archive in paths {
                        let abs_archive = absolutize_path(&archive);
                        extract_archive(
                            &abs_archive,
                            &dest_worker,
                            None,
                            None,
                            None,
                            Some(tx),
                            &cancel_worker,
                        )?;
                    }
                    Ok(OperationSummary::Extracted { password: None })
                });

                run_worker_with_progress(
                    Some(&app_cloned),
                    None,
                    "Extracting Archive",
                    rx,
                    cancel,
                    |res| {
                        if let Err(err) = res
                            && !matches!(err, ArchiveError::Cancelled)
                        {
                            show_error_dialog(
                                None::<&gtk::Window>,
                                "Extraction Failed",
                                &err.to_string(),
                            );
                        }
                    },
                );
                // Progress window now holds the application; release the initial dialog hold.
                *app_hold.borrow_mut() = None;
            });
        }
    }
}

fn absolutize_path(path: &PathBuf) -> PathBuf {
    if path.is_absolute() {
        path.clone()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path.clone()
    }
}
