//! The app shell: wires the `walkingpad` session to `tauri-plugin-blec` and
//! the UI.
//!
//! - Pad intents and `pad-state` snapshots remain separate from heart-rate
//!   intents/snapshots and local walk history. The UI never talks directly to
//!   Bluetooth: the capability grants no blec permissions.
//! - Settings (remembered pad, units, presets, Stop method) live in
//!   `settings.json` in the app config dir and are saved when they change.
//! - Closing the window quits the app. blec drops the link on exit; nothing
//!   is sent to the pad, so a moving belt keeps moving (design §7).

mod applog;
mod ble;
mod discovery;
mod heart_rate;
#[cfg(feature = "mock")]
mod mock_pad;
mod settings;
mod walks;

use std::sync::Arc;

use tauri::{Emitter, Manager, State};
use walkingpad::session::{self, Intent, SessionHandle, Snapshot};

use settings::Settings;

struct AppState {
    session: SessionHandle,
    heart: heart_rate::Handle,
    walks: tokio::sync::watch::Receiver<walks::Snapshot>,
}

#[tauri::command]
fn heart_rate_intent(state: State<'_, AppState>, intent: heart_rate::Intent) -> Result<(), String> {
    state.heart.send(intent)
}

#[tauri::command]
fn heart_rate_snapshot(state: State<'_, AppState>) -> heart_rate::Snapshot {
    state.heart.state.borrow().clone()
}

#[tauri::command]
fn walks_snapshot(state: State<'_, AppState>) -> walks::Snapshot {
    state.walks.borrow().clone()
}

/// Every user action: connect, start, stop, speed, mode, presets, units,
/// visibility.
#[tauri::command]
fn intent(state: State<'_, AppState>, intent: Intent) -> Result<(), String> {
    if state.session.send(intent) {
        Ok(())
    } else {
        Err("the session has ended".into())
    }
}

/// The current snapshot, for the first render.
#[tauri::command]
fn snapshot(state: State<'_, AppState>) -> Snapshot {
    state.session.snapshot()
}

/// Opens the system's Bluetooth privacy settings (macOS). On Android the
/// permission prompt itself sends the user to the app's settings.
#[tauri::command]
fn open_bluetooth_settings() {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Bluetooth")
        .spawn();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Registers blec's Android side and disconnects on exit. With the
        // `mock` feature, blec runs on its in-process mock backend.
        .plugin(tauri_plugin_blec::init())
        .setup(|app| {
            let path = Settings::path(&app.path().app_config_dir()?);
            let settings = Settings::load(&path);
            applog::line(&format!("settings: {}", path.display()));

            #[cfg(feature = "mock")]
            {
                applog::line("MOCK BUILD: simulated pad, no Bluetooth");
                mock_pad::install();
                mock_pad::install_heart_rate();
            }

            let (events_tx, events_rx) = walkingpad::transport::event_channel();
            let discovery = discovery::Discovery::new(events_tx.clone());
            let transport = ble::BlecTransport::new(events_tx, discovery.clone());
            let mut config = settings.session_config();
            config.log = Some(Arc::new(|line: &str| applog::line(line)));
            let (session, handle) = session::new(transport, events_rx, config);
            tauri::async_runtime::spawn(session.run());

            let history_dir = app.path().app_data_dir()?.join(if cfg!(feature = "mock") {
                "walks-mock"
            } else {
                "walks"
            });
            let (walks, samples) = walks::spawn(history_dir, handle.watch());
            let heart = heart_rate::spawn(discovery, samples);
            let mut heart_watch = heart.state.clone();
            let mut walks_watch = walks.clone();
            let heart_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    let _ =
                        heart_app.emit("heart-rate-state", heart_watch.borrow_and_update().clone());
                    if heart_watch.changed().await.is_err() {
                        break;
                    }
                }
            });
            let walks_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    let _ = walks_app.emit("walks-state", walks_watch.borrow_and_update().clone());
                    if walks_watch.changed().await.is_err() {
                        break;
                    }
                }
            });

            // Publish every snapshot; persist what the user changed.
            let mut watch = handle.watch();
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut saved = settings;
                loop {
                    let snap = watch.borrow_and_update().clone();
                    let _ = app_handle.emit("pad-state", &snap);
                    let next = Settings {
                        pad: snap.remembered.clone(),
                        display_units: snap.units.display,
                        presets: snap.preset_store,
                        ..saved.clone()
                    };
                    if next != saved {
                        if let Err(e) = next.save(&path) {
                            applog::line(&format!("saving settings failed: {e}"));
                        }
                        saved = next;
                    }
                    if watch.changed().await.is_err() {
                        break;
                    }
                }
            });

            app.manage(AppState {
                session: handle,
                heart,
                walks,
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window ends the app without a prompt and without
            // stopping the belt (your choice, design §7).
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                window.app_handle().exit(0);
            }
        })
        .invoke_handler(tauri::generate_handler![
            intent,
            snapshot,
            heart_rate_intent,
            heart_rate_snapshot,
            walks_snapshot,
            open_bluetooth_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running the app");
}

#[cfg(all(test, feature = "mock"))]
mod mock_tests;
