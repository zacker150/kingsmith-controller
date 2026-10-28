//! Android owns the durable export worker; the UI never supplies health payloads.

#[cfg(target_os = "android")]
use tauri::Manager;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    availability: String,
    enabled: bool,
    permitted: bool,
    synced_walks: u32,
    last_synced_ms: Option<u64>,
    message: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Action {
    Connect,
    Disconnect,
    Settings,
}

#[cfg(target_os = "android")]
struct HealthConnect(tauri::plugin::PluginHandle<tauri::Wry>);

pub fn init() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("health-connect")
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            app.manage(HealthConnect(api.register_android_plugin(
                "local.kingsmithcontroller",
                "HealthConnectPlugin",
            )?));
            #[cfg(not(target_os = "android"))]
            let _ = (app, api);
            Ok(())
        })
        .build()
}

pub async fn status(app: &tauri::AppHandle) -> Result<Snapshot, String> {
    #[cfg(target_os = "android")]
    {
        app.state::<HealthConnect>()
            .0
            .run_mobile_plugin_async("status", serde_json::json!({}))
            .await
            .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(Snapshot {
            availability: "unsupported".into(),
            enabled: false,
            permitted: false,
            synced_walks: 0,
            last_synced_ms: None,
            message: None,
        })
    }
}

pub async fn action(app: &tauri::AppHandle, action: Action) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let method = match action {
            Action::Connect => "connect",
            Action::Disconnect => "disconnect",
            Action::Settings => "settings",
        };
        app.state::<HealthConnect>()
            .0
            .run_mobile_plugin_async::<serde_json::Value>(method, serde_json::json!({}))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, action);
        Err("Health Connect is available on Android only".into())
    }
}

#[cfg(target_os = "android")]
pub fn spawn(
    app: tauri::AppHandle,
    directory: std::path::PathBuf,
    mut walks: tokio::sync::watch::Receiver<crate::walks::Snapshot>,
) {
    tauri::async_runtime::spawn(async move {
        // Configure the app-owned directory once; retry initialization without affecting recording.
        loop {
            let result = app
                .state::<HealthConnect>()
                .0
                .run_mobile_plugin_async::<()>(
                    "configure",
                    serde_json::json!({ "directory": directory }),
                )
                .await;
            if result.is_ok() {
                break;
            }
            crate::applog::line("Health Connect initialization failed; retrying");
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
        let mut recent = Vec::new();
        let mut retry = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            let completed: Vec<String> = walks
                .borrow_and_update()
                .recent
                .iter()
                .map(|w| w.id.clone())
                .collect();
            if completed != recent {
                if app
                    .state::<HealthConnect>()
                    .0
                    .run_mobile_plugin_async::<()>("sync", serde_json::json!({}))
                    .await
                    .is_err()
                {
                    crate::applog::line(
                        "Couldn't schedule Health Connect sync; saved walks remain pending",
                    );
                } else {
                    recent = completed;
                }
            }
            tokio::select! {
                changed = walks.changed() => { if changed.is_err() { break; } }
                _ = retry.tick() => {}
            }
        }
    });
}
