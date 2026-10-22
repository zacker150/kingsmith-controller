//! The app shell: wires the `walkingpad` session to `tauri-plugin-blec` and
//! the UI.
//!
//! - The UI sends intents through one command (`intent`) and renders the
//!   `pad-state` event, a full snapshot after every change. It never talks to
//!   Bluetooth: the capability grants no blec permissions.
//! - Settings (remembered pad, units, presets, Stop method) live in
//!   `settings.json` in the app config dir and are saved when they change.
//! - Closing the window quits the app. blec drops the link on exit; nothing
//!   is sent to the pad, so a moving belt keeps moving (design §7).

mod applog;
mod ble;
#[cfg(feature = "mock")]
mod mock_pad;
mod settings;

use std::sync::Arc;

use tauri::{Emitter, Manager, State};
use walkingpad::session::{self, Intent, SessionHandle, Snapshot};

use settings::Settings;

struct AppState {
    session: SessionHandle,
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
            }

            let (events_tx, events_rx) = walkingpad::transport::event_channel();
            let transport = ble::BlecTransport::new(events_tx);
            let mut config = settings.session_config();
            config.log = Some(Arc::new(|line: &str| applog::line(line)));
            let (session, handle) = session::new(transport, events_rx, config);
            tauri::async_runtime::spawn(session.run());

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

            app.manage(AppState { session: handle });
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
            open_bluetooth_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running the app");
}

#[cfg(all(test, feature = "mock"))]
mod mock_tests;
