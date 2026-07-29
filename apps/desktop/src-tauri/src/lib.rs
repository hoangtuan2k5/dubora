mod commands;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // Config `maximized: true` can fail on some Windows builds; force it on launch.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.maximize();
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::save_config,
            commands::scan_videos,
            commands::rename_video,
            commands::delete_video,
            commands::get_video_playback,
            commands::import_youtube,
            commands::list_voices,
            commands::default_azure_voice,
            commands::start_dub,
            commands::cancel_dub,
            commands::get_ffmpeg_status,
            commands::open_path,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Dubora");
}
