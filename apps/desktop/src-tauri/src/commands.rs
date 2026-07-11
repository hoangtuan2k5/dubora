use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use viet_dub_core::ffmpeg::FfmpegPaths;
use viet_dub_core::{
    AzureConfig, DubConfig, DubOptions, ProgressEvent, ProgressReporter, Provider, Stage,
    VideoInfo, create_dubs,
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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            project_root: String::new(),
            azure_key: String::new(),
            azure_region: viet_dub_core::config::DEFAULT_AZURE_REGION.to_owned(),
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

#[tauri::command]
pub fn save_config(app: AppHandle, config: AppConfig) -> Result<(), String> {
    write_config(&config_path(&app)?, &config)
}

#[tauri::command]
pub async fn scan_videos(root: String) -> Result<Vec<VideoInfo>, String> {
    if root.trim().is_empty() {
        return Err("Chưa chọn thư mục dự án".to_owned());
    }
    viet_dub_core::scan_videos(Path::new(&root)).map_err(|error| format!("{error:#}"))
}

#[tauri::command]
pub fn start_dub(app: AppHandle, request: StartDubRequest) -> Result<String, String> {
    let saved = read_config(&config_path(&app)?)?;
    if saved.project_root.trim().is_empty() {
        return Err("Chưa chọn thư mục dự án".to_owned());
    }

    let azure = AzureConfig::new(saved.azure_key, saved.azure_region)
        .map_err(|error| format!("{error:#}"))?;
    let resource_dir = app.path().resource_dir().ok();
    let config = DubConfig {
        root: PathBuf::from(saved.project_root),
        provider: Provider::Azure,
        azure: Some(azure),
        ffmpeg: FfmpegPaths::resolve(resource_dir.as_deref()),
    };

    if DUB_RUNNING.swap(true, Ordering::AcqRel) {
        return Err("Một tác vụ lồng tiếng khác đang chạy".to_owned());
    }
    DUB_CANCEL.store(false, Ordering::Release);

    let job_id = format!("job-{}", NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed));
    let task_job_id = job_id.clone();
    tauri::async_runtime::spawn(async move {
        let progress_app = app.clone();
        let progress_job_id = task_job_id.clone();
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
            app: progress_app,
            job_id: progress_job_id,
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
                    create_dubs(&config, Some(Path::new(&target)), options, &reporter).await
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
    if !path.exists() {
        return Err(format!("Không tìm thấy đường dẫn: {}", path.display()));
    }

    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Không thể mở Explorer: {error}"))
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("Ứng dụng desktop chỉ hỗ trợ Windows".to_owned())
    }
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
        let directory = std::env::temp_dir().join(format!("viet-dub-app-{unique}"));
        let path = directory.join("config.json");
        assert_eq!(read_config(&path).unwrap(), AppConfig::default());

        let config = AppConfig {
            project_root: "C:\\Videos".to_owned(),
            azure_key: "secret".to_owned(),
            azure_region: "southeastasia".to_owned(),
        };
        write_config(&path, &config).unwrap();
        assert_eq!(read_config(&path).unwrap(), config);
        fs::remove_dir_all(directory).unwrap();
    }
}
