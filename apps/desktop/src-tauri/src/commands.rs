use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use dubora_core::ffmpeg::FfmpegPaths;
use dubora_core::{
    AzureConfig, AzureVoice, DubConfig, DubOptions, GeminiConfig, ProgressEvent, ProgressReporter, Provider,
    Stage, VideoInfo, AZURE_VOICE, create_dubs, list_azure_voices,
};

static DUB_RUNNING: AtomicBool = AtomicBool::new(false);
static DUB_CANCEL: AtomicBool = AtomicBool::new(false);
static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppConfig {
    pub project_root: String,
    pub azure_key: String,
    pub azure_region: String,
    /// Azure TTS ShortName; empty or missing falls back to Florian Dragon HD.
    pub azure_voice: String,
    pub gemini_keys: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            project_root: String::new(),
            azure_key: String::new(),
            azure_region: dubora_core::config::DEFAULT_AZURE_REGION.to_owned(),
            azure_voice: AZURE_VOICE.to_owned(),
            gemini_keys: String::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartDubRequest {
    pub targets: Vec<String>,
    pub force: bool,
    pub force_tts: bool,
    pub translate: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DubProgressEvent {
    pub job_id: String,
    #[serde(flatten)]
    pub event: ProgressEvent,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FfmpegStatus {
    pub ready: bool,
    pub path: String,
    pub message: String,
}

#[tauri::command]
pub fn get_config(app: AppHandle) -> Result<AppConfig, String> {
    read_config(&config_path(&app)?)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveConfigResult {
    pub created_dirs: Vec<String>,
}

#[tauri::command]
pub fn save_config(app: AppHandle, config: AppConfig) -> Result<SaveConfigResult, String> {
    let root = config.project_root.trim();
    if root.is_empty() {
        return Err("Chưa chọn thư mục dự án".to_owned());
    }
    let created_dirs = dubora_core::ensure_project_layout(Path::new(root))
        .map_err(|error| format!("{error:#}"))?;
    write_config(&config_path(&app)?, &config)?;
    Ok(SaveConfigResult { created_dirs })
}

#[tauri::command]
pub async fn scan_videos(root: String) -> Result<Vec<VideoInfo>, String> {
    if root.trim().is_empty() {
        return Err("Chưa chọn thư mục dự án".to_owned());
    }
    let path = Path::new(root.trim());
    dubora_core::ensure_project_layout(path).map_err(|error| format!("{error:#}"))?;
    dubora_core::scan_videos(path).map_err(|error| format!("{error:#}"))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListVoicesRequest {
    pub azure_key: String,
    pub azure_region: String,
}

#[tauri::command]
pub async fn list_voices(request: ListVoicesRequest) -> Result<Vec<AzureVoice>, String> {
    list_azure_voices(&request.azure_key, &request.azure_region)
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
pub fn default_azure_voice() -> String {
    AZURE_VOICE.to_owned()
}

#[tauri::command]
pub fn start_dub(app: AppHandle, request: StartDubRequest) -> Result<String, String> {
    let saved = read_config(&config_path(&app)?)?;
    if saved.project_root.trim().is_empty() {
        return Err("Chưa chọn thư mục dự án".to_owned());
    }

    let root = PathBuf::from(saved.project_root.trim());
    dubora_core::ensure_project_layout(&root).map_err(|error| format!("{error:#}"))?;

    let azure = AzureConfig::new(saved.azure_key, saved.azure_region)
        .and_then(|config| config.with_voice(saved.azure_voice))
        .map_err(|error| format!("{error:#}"))?;
    let resource_dir = app.path().resource_dir().ok();
    let config = DubConfig {
        root,
        provider: Provider::Azure,
        azure: Some(azure),
        gemini: request.translate.then(|| GeminiConfig::new(dubora_core::translate_gemini::parse_keys(&saved.gemini_keys))).transpose().map_err(|error| format!("{error:#}"))?,
        ffmpeg: FfmpegPaths::resolve(resource_dir.as_deref()),
    };

    if DUB_RUNNING.swap(true, Ordering::AcqRel) {
        return Err("Một tác vụ lồng tiếng khác đang chạy".to_owned());
    }
    DUB_CANCEL.store(false, Ordering::Release);

    let job_id = format!("job-{}", NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed));
    let task_job_id = job_id.clone();

    // Run the full pipeline on a dedicated OS thread + private Tokio runtime so
    // heavy FFmpeg / ffprobe work never starves the Tauri/WebView2 event loop
    // (that starvation often shows up as a solid black window on Windows).
    let spawn_result = std::thread::Builder::new()
        .name("dubora-job".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .thread_name("dubora-worker")
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    DUB_RUNNING.store(false, Ordering::Release);
                    DUB_CANCEL.store(false, Ordering::Release);
                    let _ = app.emit(
                        "dub-progress",
                        DubProgressEvent {
                            job_id: task_job_id,
                            event: ProgressEvent::new(
                                Stage::Error,
                                "",
                                format!("Không tạo được runtime xử lý: {error}"),
                            ),
                        },
                    );
                    return;
                }
            };

            runtime.block_on(async move {
                struct JobProgress {
                    app: AppHandle,
                    job_id: String,
                }
                impl ProgressReporter for JobProgress {
                    fn event(&self, event: ProgressEvent) {
                        let _ = self.app.emit(
                            "dub-progress",
                            DubProgressEvent {
                                job_id: self.job_id.clone(),
                                event,
                            },
                        );
                    }
                    fn is_cancelled(&self) -> bool {
                        DUB_CANCEL.load(Ordering::Acquire)
                    }
                }

                let reporter = JobProgress {
                    app: app.clone(),
                    job_id: task_job_id.clone(),
                };
                let options = DubOptions {
                    force: request.force,
                    force_tts: request.force_tts,
                    translate: request.translate,
                };

                let result = if request.targets.is_empty() {
                    create_dubs(&config, None, options, &reporter)
                        .await
                        .map(|_| ())
                } else {
                    let mut result = Ok(());
                    for target in request.targets {
                        if reporter.is_cancelled() {
                            result = Err(anyhow::anyhow!("Dub job cancelled"));
                            break;
                        }
                        if let Err(error) =
                            create_dubs(&config, Some(Path::new(&target)), options, &reporter)
                                .await
                        {
                            result = Err(error);
                            break;
                        }
                    }
                    result
                };

                let event = match result {
                    Ok(()) => ProgressEvent::new(Stage::Done, "", "Tác vụ lồng tiếng đã hoàn tất"),
                    Err(error) if format!("{error:#}").contains("cancelled") => {
                        ProgressEvent::new(Stage::Error, "", "Đã hủy tác vụ lồng tiếng".to_owned())
                    }
                    Err(error) => ProgressEvent::new(Stage::Error, "", format!("{error:#}")),
                };
                DUB_RUNNING.store(false, Ordering::Release);
                DUB_CANCEL.store(false, Ordering::Release);
                reporter.event(event);
            });
        });

    if let Err(error) = spawn_result {
        DUB_RUNNING.store(false, Ordering::Release);
        DUB_CANCEL.store(false, Ordering::Release);
        return Err(format!("Không khởi chạy được job dub: {error}"));
    }

    Ok(job_id)
}

#[tauri::command]
pub fn cancel_dub() -> Result<(), String> {
    if !DUB_RUNNING.load(Ordering::Acquire) {
        return Err("Không có tác vụ lồng tiếng đang chạy".to_owned());
    }
    DUB_CANCEL.store(true, Ordering::Release);
    Ok(())
}

#[tauri::command]
pub async fn get_ffmpeg_status(app: AppHandle) -> FfmpegStatus {
    let resource_dir = app.path().resource_dir().ok();
    let ffmpeg = FfmpegPaths::resolve(resource_dir.as_deref());
    let path = ffmpeg.ffmpeg.display().to_string();
    match ffmpeg.verify() {
        Ok(()) => FfmpegStatus {
            ready: true,
            path,
            message: "FFmpeg đã sẵn sàng với rubberband".to_owned(),
        },
        Err(error) => FfmpegStatus {
            ready: false,
            path,
            message: format!("{error:#}"),
        },
    }
}

#[tauri::command]
pub fn open_path(path: PathBuf) -> Result<(), String> {
    // Normalize mixed / and \ from the webview so exists()/Explorer see a real path.
    let path = normalize_user_path(&path);
    if !path.exists() {
        return Err(format!("Không tìm thấy đường dẫn: {}", path.display()));
    }

    #[cfg(windows)]
    {
        open_in_explorer(&path)
    }
    #[cfg(not(windows))]
    {
        open_path_unix(&path)
    }
}

/// Collapse mixed separators from the UI into a clean OS path.
fn normalize_user_path(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    // Keep Windows drive prefix (D:) intact; rewrite all separators via Path components.
    let unified = raw.replace('\\', "/");
    let mut normalized = PathBuf::new();
    for component in Path::new(&unified).components() {
        normalized.push(component.as_os_str());
    }
    if normalized.exists() {
        normalized.canonicalize().unwrap_or(normalized)
    } else {
        normalized
    }
}

#[cfg(windows)]
fn open_in_explorer(path: &Path) -> Result<(), String> {
    // Prefer absolute Windows path with backslashes for Explorer.
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let display = absolute.to_string_lossy().replace('/', "\\");
    // Strip UNC long-path prefix \\?\ if present — Explorer dislikes it.
    let display = display
        .strip_prefix(r"\\?\")
        .unwrap_or(display.as_str())
        .to_owned();

    let mut command = std::process::Command::new("explorer.exe");
    if absolute.is_file() {
        // Open the parent folder and select the output file.
        // Two-arg form is more reliable with spaces than `/select,path` as one string.
        command.arg("/select,").arg(&display);
    } else {
        command.arg(&display);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Không thể mở Explorer: {error}"))
}

#[cfg(not(windows))]
fn open_path_unix(path: &Path) -> Result<(), String> {
    // Dev/fallback: open the folder (or parent of a file).
    let target = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(target)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Không thể mở đường dẫn: {error}"))
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|directory| directory.join("config.json"))
        .map_err(|error| error.to_string())
}

fn read_config(path: &Path) -> Result<AppConfig, String> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .map_err(|error| format!("Không thể đọc {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AppConfig::default()),
        Err(error) => Err(format!("Không thể đọc {}: {error}", path.display())),
    }
}

fn write_config(path: &Path, config: &AppConfig) -> Result<(), String> {
    fs::create_dir_all(path.parent().ok_or("Đường dẫn config không hợp lệ")?)
        .map_err(|error| format!("Không thể tạo thư mục config: {error}"))?;
    let contents = serde_json::to_vec_pretty(config)
        .map_err(|error| format!("Không thể mã hóa config: {error}"))?;
    fs::write(path, contents).map_err(|error| format!("Không thể lưu config: {error}"))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn config_defaults_and_round_trips() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("dubora-app-{unique}"));
        let path = directory.join("config.json");
        assert_eq!(read_config(&path).unwrap(), AppConfig::default());

        let config = AppConfig {
            project_root: "C:\\Videos".to_owned(),
            azure_key: "secret".to_owned(),
            azure_region: "southeastasia".to_owned(),
            azure_voice: AZURE_VOICE.to_owned(),
            gemini_keys: "key1,key2".to_owned(),
        };
        write_config(&path, &config).unwrap();
        assert_eq!(read_config(&path).unwrap(), config);

        // Old configs without azureVoice still deserialize and get the default.
        let legacy = br#"{"projectRoot":"D:\\x","azureKey":"k","azureRegion":"eastus"}"#;
        let legacy_path = directory.join("legacy.json");
        fs::write(&legacy_path, legacy).unwrap();
        let loaded = read_config(&legacy_path).unwrap();
        assert_eq!(loaded.azure_voice, AZURE_VOICE);
        fs::remove_dir_all(directory).unwrap();
    }
}
