use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use dubora_core::ffmpeg::FfmpegPaths;
use dubora_core::paths::{companion_output_srt, find_matching_srt, output_path};
use dubora_core::srt::{parse_srt, time_to_seconds};
use dubora_core::youtube::select_subtitle_language;
use dubora_core::{
    AZURE_VOICE, AzureConfig, AzureVoice, DubConfig, DubOptions, GeminiConfig, NaturalMode,
    ProgressEvent, ProgressReporter, Provider, Stage, VideoInfo, create_dubs, delete_input_video,
    list_azure_voices, rename_input_video, repair_adjacent_youtube_subtitles,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

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
    /// Optional expressive style supported by the selected Azure voice.
    pub azure_voice_style: String,
    pub gemini_keys: String,
    pub playlists: Vec<Playlist>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Playlist {
    pub id: String,
    pub project_root: String,
    pub name: String,
    pub video_paths: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            project_root: String::new(),
            azure_key: String::new(),
            azure_region: dubora_core::config::DEFAULT_AZURE_REGION.to_owned(),
            azure_voice: AZURE_VOICE.to_owned(),
            azure_voice_style: String::new(),
            gemini_keys: String::new(),
            playlists: Vec::new(),
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
    #[serde(default)]
    pub natural_voice: bool,
    #[serde(default)]
    pub natural_mode: NaturalMode,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YoutubeImportRequest {
    pub url: String,
    #[serde(default)]
    pub quality: YoutubeQuality,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum YoutubeQuality {
    Lowest,
    #[serde(rename = "144p")]
    P144,
    #[serde(rename = "240p")]
    P240,
    #[serde(rename = "360p")]
    P360,
    #[serde(rename = "480p")]
    P480,
    #[serde(rename = "720p")]
    P720,
    #[serde(rename = "1080p")]
    P1080,
    #[default]
    Highest,
}

impl YoutubeQuality {
    fn format(&self) -> &'static str {
        match self {
            Self::Lowest => "worstvideo[ext=mp4]+worstaudio[ext=m4a]/worst[ext=mp4]",
            Self::P144 => {
                "bestvideo[ext=mp4][height<=144]+bestaudio[ext=m4a]/best[ext=mp4][height<=144]"
            }
            Self::P240 => {
                "bestvideo[ext=mp4][height<=240]+bestaudio[ext=m4a]/best[ext=mp4][height<=240]"
            }
            Self::P360 => {
                "bestvideo[ext=mp4][height<=360]+bestaudio[ext=m4a]/best[ext=mp4][height<=360]"
            }
            Self::P480 => {
                "bestvideo[ext=mp4][height<=480]+bestaudio[ext=m4a]/best[ext=mp4][height<=480]"
            }
            Self::P720 => {
                "bestvideo[ext=mp4][height<=720]+bestaudio[ext=m4a]/best[ext=mp4][height<=720]"
            }
            Self::P1080 => {
                "bestvideo[ext=mp4][height<=1080]+bestaudio[ext=m4a]/best[ext=mp4][height<=1080]"
            }
            Self::Highest => "bestvideo[ext=mp4]+bestaudio[ext=m4a]/best[ext=mp4]",
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct YoutubeImportResult {
    pub relative_path: String,
    pub subtitle_language: String,
    pub videos: Vec<VideoInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameVideoRequest {
    pub relative_path: String,
    pub new_stem: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryMutationResult {
    pub videos: Vec<VideoInfo>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleCue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoPlayback {
    pub video_path: String,
    pub cues: Vec<SubtitleCue>,
}

#[derive(Deserialize)]
struct YoutubeMetadata {
    #[serde(default)]
    subtitles: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    automatic_captions: std::collections::BTreeMap<String, serde_json::Value>,
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
    repair_adjacent_youtube_subtitles(path).map_err(|error| format!("{error:#}"))?;
    dubora_core::scan_videos(path).map_err(|error| format!("{error:#}"))
}

#[tauri::command]
pub fn rename_video(
    app: AppHandle,
    request: RenameVideoRequest,
) -> Result<LibraryMutationResult, String> {
    let root = configured_project_root(&app)?;
    rename_input_video(&root, Path::new(&request.relative_path), &request.new_stem)
        .map_err(|error| format!("{error:#}"))?;
    let videos = dubora_core::scan_videos(&root).map_err(|error| format!("{error:#}"))?;
    Ok(LibraryMutationResult { videos })
}

#[tauri::command]
pub fn delete_video(
    app: AppHandle,
    relative_path: String,
) -> Result<LibraryMutationResult, String> {
    let root = configured_project_root(&app)?;
    delete_input_video(&root, Path::new(&relative_path)).map_err(|error| format!("{error:#}"))?;
    let videos = dubora_core::scan_videos(&root).map_err(|error| format!("{error:#}"))?;
    Ok(LibraryMutationResult { videos })
}

#[tauri::command]
pub fn get_video_playback(app: AppHandle, relative_path: String) -> Result<VideoPlayback, String> {
    let root = configured_project_root(&app)?;
    let relative = Path::new(relative_path.trim());
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("Đường dẫn video không hợp lệ".to_owned());
    }

    let video_dir = root
        .join("assets/video")
        .canonicalize()
        .map_err(|error| format!("Không thể mở thư mục video: {error}"))?;
    let video = video_dir
        .join(relative)
        .canonicalize()
        .map_err(|_| "Không tìm thấy video".to_owned())?;
    if !video.starts_with(&video_dir) || !video.is_file() {
        return Err("Video phải nằm trong assets/video".to_owned());
    }

    // Prefer the Vietnamese dubbed output when it exists; source is the fallback for previews.
    let relative = video.strip_prefix(&video_dir).unwrap_or(relative);
    let output = output_path(&root, &video, relative).map_err(|error| format!("{error:#}"))?;
    let (playback_video, subtitle) = if output.is_file() {
        let subtitle = companion_output_srt(&output);
        (output, subtitle)
    } else {
        let subtitle = find_matching_srt(&root, &video).unwrap_or_default();
        (video, subtitle)
    };

    let cues = parse_srt(&subtitle)
        .ok()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|segment| {
            Some(SubtitleCue {
                start: time_to_seconds(&segment.start).ok()?,
                end: time_to_seconds(&segment.end).ok()?,
                text: segment.text,
            })
        })
        .collect();

    Ok(VideoPlayback {
        video_path: playback_video.display().to_string(),
        cues,
    })
}

fn configured_project_root(app: &AppHandle) -> Result<PathBuf, String> {
    let saved = read_config(&config_path(app)?)?;
    if saved.project_root.trim().is_empty() {
        return Err("Chưa chọn thư mục dự án".to_owned());
    }
    let root = PathBuf::from(saved.project_root.trim());
    dubora_core::ensure_project_layout(&root).map_err(|error| format!("{error:#}"))?;
    Ok(root)
}

#[tauri::command]
pub async fn import_youtube(
    app: AppHandle,
    request: YoutubeImportRequest,
) -> Result<YoutubeImportResult, String> {
    tauri::async_runtime::spawn_blocking(move || import_youtube_blocking(&app, request))
        .await
        .map_err(|error| format!("Không khởi chạy được yt-dlp: {error}"))?
}

fn import_youtube_blocking(
    app: &AppHandle,
    request: YoutubeImportRequest,
) -> Result<YoutubeImportResult, String> {
    let url = request.url.trim();
    if !is_youtube_url(url) {
        return Err("Chỉ hỗ trợ link youtube.com hoặc youtu.be hợp lệ".to_owned());
    }

    let saved = read_config(&config_path(app)?)?;
    let root = PathBuf::from(saved.project_root.trim());
    if saved.project_root.trim().is_empty() {
        return Err("Chọn và lưu thư mục dự án trước khi tải từ YouTube".to_owned());
    }
    dubora_core::ensure_project_layout(&root).map_err(|error| format!("{error:#}"))?;

    let yt_dlp = yt_dlp_path(app);
    let metadata = run_yt_dlp(
        &yt_dlp,
        [
            "--dump-single-json",
            "--skip-download",
            "--no-playlist",
            "--no-warnings",
        ],
        url,
    )?;
    let metadata: YoutubeMetadata = serde_json::from_slice(&metadata.stdout)
        .map_err(|error| format!("yt-dlp không trả metadata hợp lệ: {error}"))?;
    let language = select_subtitle_language(
        metadata
            .subtitles
            .keys()
            .chain(metadata.automatic_captions.keys())
            .filter(|language| language.as_str() != "live_chat")
            .cloned(),
    )
    .ok_or("Video này không có phụ đề để tải")?;

    let video_dir = root.join("assets").join("video");
    let srt_dir = root.join("assets").join("srt");
    let output_template = video_dir.join("%(title)s [%(id)s].%(ext)s");
    let ffmpeg = FfmpegPaths::resolve(app.path().resource_dir().ok().as_deref());
    let ffmpeg_dir = ffmpeg.ffmpeg.parent().unwrap_or(Path::new("."));
    let output = yt_dlp_command(&yt_dlp)
        .args([
            "--no-playlist",
            "--no-warnings",
            "--format",
            request.quality.format(),
            "--merge-output-format",
            "mp4",
        ])
        .args([
            "--write-subs",
            "--write-auto-subs",
            "--sub-langs",
            &language,
            "--sub-format",
            "srt",
            "--convert-subs",
            "srt",
        ])
        .arg("--output")
        .arg(output_template)
        .arg("--ffmpeg-location")
        .arg(ffmpeg_dir)
        .args(["--print", "after_move:filepath", "--"])
        .arg(url)
        .output()
        .map_err(|error| format!("Không chạy được {}: {error}", yt_dlp.display()))?;
    if !output.status.success() {
        return Err(yt_dlp_error(&output));
    }

    let video = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .next_back()
        .map(PathBuf::from)
        .filter(|path| {
            path.is_file()
                && path.extension().and_then(|extension| extension.to_str()) == Some("mp4")
        })
        .ok_or("yt-dlp tải xong nhưng không tìm thấy file video")?;
    let video_stem = video
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or("Tên video không hợp lệ")?;
    let subtitle = [srt_dir.as_path(), video_dir.as_path()]
        .into_iter()
        .find_map(|directory| {
            fs::read_dir(directory)
                .ok()?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    path.extension().and_then(|extension| extension.to_str()) == Some("srt")
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| {
                                name.starts_with(&format!("{video_stem}.{language}"))
                            })
                })
        })
        .ok_or("yt-dlp không tải được SRT đã chọn")?;
    let companion = srt_dir.join(format!("{video_stem}.srt"));
    if companion.exists() {
        fs::remove_file(&companion).map_err(|error| format!("Không thể thay SRT cũ: {error}"))?;
    }
    fs::rename(&subtitle, &companion).map_err(|error| format!("Không thể đổi tên SRT: {error}"))?;

    let relative_path = video
        .strip_prefix(&video_dir)
        .unwrap_or(&video)
        .to_string_lossy()
        .replace('\\', "/");
    let videos = dubora_core::scan_videos(&root).map_err(|error| format!("{error:#}"))?;
    Ok(YoutubeImportResult {
        relative_path,
        subtitle_language: language,
        videos,
    })
}

fn is_youtube_url(url: &str) -> bool {
    [
        "https://youtube.com/",
        "https://www.youtube.com/",
        "https://m.youtube.com/",
        "https://youtu.be/",
    ]
    .iter()
    .any(|prefix| url.starts_with(prefix))
}

fn yt_dlp_path(app: &AppHandle) -> PathBuf {
    let binary = if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    };
    app.path()
        .resource_dir()
        .ok()
        .map(|directory| directory.join("yt-dlp").join(binary))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(binary))
}

fn yt_dlp_command(path: &Path) -> Command {
    let mut command = Command::new(path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
}

fn run_yt_dlp<const N: usize>(
    yt_dlp: &Path,
    args: [&str; N],
    url: &str,
) -> Result<std::process::Output, String> {
    let output = yt_dlp_command(yt_dlp)
        .args(args)
        .args(["--", url])
        .output()
        .map_err(|error| format!("Không chạy được {}: {error}", yt_dlp.display()))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(yt_dlp_error(&output))
    }
}

fn yt_dlp_error(output: &std::process::Output) -> String {
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if message.is_empty() {
        "yt-dlp không thể tải video này".to_owned()
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::YoutubeQuality;

    #[test]
    fn youtube_quality_uses_mp4_formats() {
        for quality in [
            YoutubeQuality::Lowest,
            YoutubeQuality::P144,
            YoutubeQuality::P240,
            YoutubeQuality::P360,
            YoutubeQuality::P480,
            YoutubeQuality::P720,
            YoutubeQuality::P1080,
            YoutubeQuality::Highest,
        ] {
            assert!(quality.format().contains("ext=mp4"));
        }
        assert!(YoutubeQuality::Highest.format().starts_with("bestvideo"));
        let selected: YoutubeQuality = serde_json::from_str("\"1080p\"").unwrap();
        assert!(selected.format().contains("height<=1080"));
    }
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
        .and_then(|config| config.with_style(saved.azure_voice_style))
        .map_err(|error| format!("{error:#}"))?;
    let resource_dir = app.path().resource_dir().ok();
    let config = DubConfig {
        root,
        provider: Provider::Azure,
        azure: Some(azure),
        gemini: (request.translate || request.natural_voice)
            .then(|| {
                GeminiConfig::new(dubora_core::translate_gemini::parse_keys(
                    &saved.gemini_keys,
                ))
            })
            .transpose()
            .map_err(|error| format!("{error:#}"))?,
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
                    natural_mode: request.natural_voice.then_some(request.natural_mode),
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
            azure_voice_style: "chat".to_owned(),
            gemini_keys: "key1,key2".to_owned(),
            playlists: Vec::new(),
        };
        write_config(&path, &config).unwrap();
        assert_eq!(read_config(&path).unwrap(), config);

        // Old configs without azureVoice still deserialize and get the default.
        let legacy = br#"{"projectRoot":"D:\\x","azureKey":"k","azureRegion":"eastus"}"#;
        let legacy_path = directory.join("legacy.json");
        fs::write(&legacy_path, legacy).unwrap();
        let loaded = read_config(&legacy_path).unwrap();
        assert_eq!(loaded.azure_voice, AZURE_VOICE);
        assert!(loaded.azure_voice_style.is_empty());
        assert!(loaded.playlists.is_empty());
        fs::remove_dir_all(directory).unwrap();
    }
}
