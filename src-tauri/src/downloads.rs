//! Save exports in user-visible Downloads on desktop and Android.
use tauri::Manager;

#[cfg(target_os = "android")]
struct Downloads(tauri::plugin::PluginHandle<tauri::Wry>);

pub fn init() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("walk-downloads")
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            app.manage(Downloads(api.register_android_plugin(
                "local.kingsmithcontroller",
                "DownloadsPlugin",
            )?));
            #[cfg(not(target_os = "android"))]
            let _ = (app, api);
            Ok(())
        })
        .build()
}

pub async fn save(app: &tauri::AppHandle, id: &str, content: String) -> Result<String, String> {
    // Defense in depth: the filename must never contain user-supplied paths.
    crate::walks::saved_path(std::path::Path::new(""), id)?;
    let filename = format!("kingsmith-walk-{id}.tcx");
    #[cfg(target_os = "android")]
    {
        #[derive(serde::Deserialize)]
        struct Saved {
            location: String,
        }
        app.state::<Downloads>()
            .0
            .run_mobile_plugin_async::<Saved>(
                "saveTcx",
                serde_json::json!({"filename": filename, "content": content}),
            )
            .await
            .map(|saved| saved.location)
            .map_err(|e| format!("Couldn't download TCX: {e}"))
    }
    #[cfg(not(target_os = "android"))]
    {
        let dir = app
            .path()
            .download_dir()
            .map_err(|e| e.to_string())?
            .join("Kingsmith Controller");
        tauri::async_runtime::spawn_blocking(move || {
            save_desktop(&dir, &filename, content.as_bytes())
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("Couldn't download TCX: {e}"))
    }
}

#[cfg(not(target_os = "android"))]
fn save_desktop(dir: &std::path::Path, filename: &str, content: &[u8]) -> std::io::Result<String> {
    use std::{fs::OpenOptions, io::Write};
    std::fs::create_dir_all(dir)?;
    for number in 0.. {
        let name = if number == 0 {
            filename.to_string()
        } else {
            format!("{} ({number}).tcx", filename.trim_end_matches(".tcx"))
        };
        let path = dir.join(&name);
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        if let Err(e) = file.write_all(content).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
        return Ok(path.display().to_string());
    }
    unreachable!()
}

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    #[test]
    fn repeated_downloads_preserve_previous_files() {
        let dir = std::env::temp_dir().join(format!(
            "kc-downloads-{}-{}",
            std::process::id(),
            crate::heart_rate::unix_ms()
        ));
        let first = super::save_desktop(&dir, "walk.tcx", b"first").unwrap();
        let second = super::save_desktop(&dir, "walk.tcx", b"second").unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(first).unwrap(), b"first");
        assert_eq!(std::fs::read(second).unwrap(), b"second");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
