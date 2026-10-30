//! Wolf2 Tray — a tray icon, a menu, and the engine. No window.
//!
//! What this program does, in one paragraph (the README says the same): it
//! reads your own aircraft from the Tacview exporter on localhost, writes it
//! into one-minute files, and sends those to wolf2.net while a recording is
//! live, signed in as you. Nothing about other players leaves this PC.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod auth;
mod config;
mod delivery;
mod exporter;
mod ownship;
mod segment;
mod spool;
mod state;

use std::sync::Arc;

use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tokio::sync::{Notify, watch};
use tray_icon::{Icon, TrayIconBuilder};

use crate::state::State;

const APP_NAME: &str = "Wolf2 Tray";
const GIT_SHA: &str = env!("WOLF2_TRAY_GIT_SHA");
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "Wolf2 Tray";

fn edition() -> String {
    format!("{} ({})", env!("CARGO_PKG_VERSION"), GIT_SHA)
}

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
    Status(String),
}

fn main() {
    let _log_guard = init_logging();
    tracing::info!(edition = edition(), "starting");

    let config = config::load();
    let (state, mut status_rx) = State::new();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let wake = Arc::new(Notify::new());

    let api = match api::Api::new(config.base_url()) {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("http client: {e}");
            return;
        }
    };
    let engine = delivery::Engine {
        config: config.clone(),
        state: state.clone(),
        spool: spool::Spool::new(config::spool_dir()),
        api,
        edition: edition(),
        wake: wake.clone(),
    };

    // The engine lives on its own runtime thread; the UI thread stays free.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let engine_handle = runtime.spawn(engine.run(shutdown_rx));

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Status lines reach the menu through the event loop.
    let status_proxy = proxy.clone();
    runtime.spawn(async move {
        while status_rx.changed().await.is_ok() {
            let line = status_rx.borrow_and_update().clone();
            if status_proxy.send_event(UserEvent::Status(line)).is_err() {
                break;
            }
        }
    });
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));

    let menu = Menu::new();
    let status_item = MenuItem::new(state.snapshot().line(), false, None);
    let sign_in = MenuItem::new("Sign in with Discord", true, None);
    let sign_out = MenuItem::new("Sign out", true, None);
    let autostart = muda::CheckMenuItem::new("Start with Windows", true, autostart_enabled(), None);
    let open_log = MenuItem::new("Open log", true, None);
    let open_config = MenuItem::new("Open settings folder", true, None);
    let about = MenuItem::new(format!("{APP_NAME} {}", edition()), false, None);
    let quit = MenuItem::new("Quit", true, None);
    for item in [
        &status_item as &dyn muda::IsMenuItem,
        &PredefinedMenuItem::separator(),
        &sign_in,
        &sign_out,
        &PredefinedMenuItem::separator(),
        &autostart,
        &open_log,
        &open_config,
        &PredefinedMenuItem::separator(),
        &about,
        &quit,
    ] {
        if let Err(e) = menu.append(item) {
            tracing::error!("menu: {e}");
        }
    }

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(format!("{APP_NAME} · {}", state.snapshot().line()))
        .with_icon(icon())
        .build();
    let tray = match tray {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("tray icon: {e}");
            return;
        }
    };

    let signing_in = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sign_in_id = sign_in.id().clone();
    let sign_out_id = sign_out.id().clone();
    let autostart_id = autostart.id().clone();
    let open_log_id = open_log.id().clone();
    let open_config_id = open_config.id().clone();
    let quit_id = quit.id().clone();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Status(line)) => {
                status_item.set_text(&line);
                let _ = tray.set_tooltip(Some(format!("{APP_NAME} · {line}")));
            }
            Event::UserEvent(UserEvent::Menu(MenuEvent { id })) => {
                if id == sign_in_id {
                    if signing_in.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    let state = state.clone();
                    let wake = wake.clone();
                    let flag = signing_in.clone();
                    std::thread::spawn(move || {
                        state.update(|f| f.last_error = None);
                        match auth::run_oauth_dance() {
                            Ok(_) => {
                                tracing::info!("signed in");
                                state.update(|f| {
                                    f.signed_in = true;
                                    f.sign_in_needed = false;
                                });
                                wake.notify_one();
                            }
                            Err(e) => {
                                tracing::warn!("sign-in failed: {e}");
                                state.update(|f| f.last_error = Some(e));
                            }
                        }
                        flag.store(false, std::sync::atomic::Ordering::SeqCst);
                    });
                } else if id == sign_out_id {
                    if let Err(e) = auth::sign_out() {
                        tracing::warn!("sign-out: {e}");
                    }
                    state.update(|f| {
                        f.signed_in = false;
                        f.sign_in_needed = false;
                        f.live_recording = None;
                    });
                } else if id == autostart_id {
                    let want = autostart.is_checked();
                    if let Err(e) = set_autostart(want) {
                        tracing::warn!("autostart: {e}");
                        autostart.set_checked(!want);
                    }
                } else if id == open_log_id {
                    open_path(&config::log_path());
                } else if id == open_config_id {
                    open_path(&config::data_dir());
                } else if id == quit_id {
                    let _ = shutdown_tx.send(true);
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::LoopDestroyed => {
                let _ = shutdown_tx.send(true);
                engine_handle.abort();
            }
            _ => {}
        }
    });
}

fn init_logging() -> tracing_appender::non_blocking::WorkerGuard {
    let dir = config::data_dir();
    let appender = tracing_appender::rolling::daily(&dir, "tray.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(false)
        .with_max_level(tracing::Level::INFO)
        .init();
    guard
}

/// A plain solid icon drawn in code: no image decoder in the tree.
fn icon() -> Icon {
    const SIZE: u32 = 32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let c = (SIZE as f32 - 1.0) / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let r = (dx * dx + dy * dy).sqrt();
            let inside = r <= c;
            let ring = inside && r >= c - 3.0;
            let (red, green, blue, alpha) = if ring {
                (226, 59, 59, 255)
            } else if inside {
                (24, 24, 26, 255)
            } else {
                (0, 0, 0, 0)
            };
            rgba.extend_from_slice(&[red, green, blue, alpha]);
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("icon")
}

fn open_path(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

#[cfg(target_os = "windows")]
fn autostart_enabled() -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>(RUN_VALUE))
        .is_ok()
}

#[cfg(target_os = "windows")]
fn set_autostart(enable: bool) -> std::io::Result<()> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(RUN_KEY)?;
    if enable {
        let exe = std::env::current_exe()?;
        key.set_value(RUN_VALUE, &format!("\"{}\"", exe.display()))
    } else {
        match key.delete_value(RUN_VALUE) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn autostart_enabled() -> bool {
    false
}

#[cfg(not(target_os = "windows"))]
fn set_autostart(_enable: bool) -> std::io::Result<()> {
    Err(std::io::Error::other("autostart is Windows-only"))
}
