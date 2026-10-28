//! Android foreground service lifetime follows native BLE state, not the UI.

#[cfg(target_os = "android")]
use tauri::Manager;

#[derive(Default)]
pub struct Status(pub std::sync::Mutex<Option<String>>);

#[cfg(target_os = "android")]
struct Service(tauri::plugin::PluginHandle<tauri::Wry>);

pub fn init() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("connection-service")
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            app.manage(Service(api.register_android_plugin(
                "local.kingsmithcontroller",
                "ConnectionPlugin",
            )?));
            #[cfg(not(target_os = "android"))]
            let _ = (app, api);
            Ok(())
        })
        .build()
}

#[cfg(target_os = "android")]
pub fn spawn(
    app: tauri::AppHandle,
    mut pad: tokio::sync::watch::Receiver<walkingpad::session::Snapshot>,
    mut heart: tokio::sync::watch::Receiver<crate::heart_rate::Snapshot>,
) {
    use tauri::Emitter;
    tauri::async_runtime::spawn(async move {
        let mut applied = false;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            let active = pad.borrow().has_link
                || pad.borrow().link == walkingpad::session::LinkPhase::Connecting
                || matches!(
                    heart.borrow().phase,
                    "connecting" | "connected" | "reconnecting" | "disconnecting"
                );
            if active != applied {
                let plugin = app.state::<Service>();
                let result = plugin
                    .0
                    .run_mobile_plugin_async::<()>(
                        "setActive",
                        serde_json::json!({ "active": active }),
                    )
                    .await;
                let error = result.as_ref().err().map(ToString::to_string);
                if let Some(error) = &error {
                    crate::applog::line(&format!("background: {error}"));
                }
                *app.state::<Status>().0.lock().unwrap() = error.clone();
                let _ = app.emit("background-error", &error);
                if result.is_ok() {
                    applied = active;
                }
            }
            tokio::select! {
                Ok(()) = pad.changed() => { pad.borrow_and_update(); }
                Ok(()) = heart.changed() => { heart.borrow_and_update(); }
                _ = tick.tick() => {}
            }
        }
    });
}
