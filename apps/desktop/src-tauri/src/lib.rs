mod commands;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::save_config,
            commands::scan_videos,
            commands::start_dub,
            commands::cancel_dub,
            commands::get_ffmpeg_status,
            commands::open_path,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run viet-dub");
}
