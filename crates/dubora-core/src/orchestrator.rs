use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::azure_voices::AzureVoice;
use crate::build_audio::{
    build_voiceover_track, build_voiceover_track_from_manifest,
    build_voiceover_track_with_tempo_cap,
};
use crate::build_video::build_dubbed_video;
use crate::clean_rolling::clean_youtube_rolling_subtitles;
use crate::config::{DubConfig, DubOptions, Provider};
use crate::duration_solver::MAX_TIME_STRETCH_TEMPO;
use crate::paths::{
    companion_output_srt, find_matching_srt, find_targets, output_path, safe_work_path,
    video_relative_path,
};
use crate::progress::{ProgressEvent, ProgressReporter, Stage};
use crate::speech_plan::SpeechPlanDocument;
use crate::srt::{SrtSegment, parse_srt, write_srt};
use crate::translate_gemini::{create_speech_plans, translate_srt};
use crate::tts_azure::{
    generate_all_tts, generate_all_tts_for_aligned_utterances, generate_all_tts_for_speech_plans,
};
use crate::tts_cache::tts_cache_key;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SpeechPlanCache {
    cache_key: String,
    document: SpeechPlanDocument,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoInfo {
    pub relative_path: PathBuf,
    pub has_srt: bool,
    pub has_output: bool,
    /// Creation time when available, falling back to modification time.
    pub added_at: u64,
}

pub fn scan_videos(root: &Path) -> Result<Vec<VideoInfo>> {
    find_targets(root, None)?
        .into_iter()
        .map(|video| {
            let relative_path = video_relative_path(root, &video);
            Ok(VideoInfo {
                has_srt: find_matching_srt(root, &video).is_ok(),
                has_output: output_path(root, &video, &relative_path)?.exists(),
                relative_path,
                added_at: video_added_at(&video),
            })
        })
        .collect()
}

fn video_added_at(video: &Path) -> u64 {
    fs::metadata(video)
        .ok()
        .and_then(|metadata| metadata.created().or_else(|_| metadata.modified()).ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs())
}

pub async fn create_dubs(
    config: &DubConfig,
    target: Option<&Path>,
    options: DubOptions,
    progress: &dyn ProgressReporter,
) -> Result<Vec<PathBuf>> {
    if config.provider == Provider::Edge {
        bail!("Edge TTS is not available in the Rust MVP; use --provider azure");
    }

    let videos = find_targets(&config.root, target)?;
    if videos.is_empty() {
        if let Some(target) = target {
            bail!("No videos found for target: {}", target.display());
        }
        bail!(
            "No video files found in {}",
            config.root.join("assets/video").display()
        );
    }

    let mut outputs = Vec::with_capacity(videos.len());
    let mut ffmpeg_verified = false;
    for video in videos {
        if progress.is_cancelled() {
            bail!("Dub job cancelled");
        }
        let relative = video_relative_path(&config.root, &video);
        let label = relative.display().to_string();
        match create_dub_for_video(
            config,
            &video,
            &relative,
            options,
            progress,
            &mut ffmpeg_verified,
        )
        .await
        {
            Ok(output) => outputs.push(output),
            Err(error) => {
                progress.event(ProgressEvent::new(
                    Stage::Error,
                    &label,
                    format!("Failed to process {label}: {error:#}"),
                ));
                return Err(error).with_context(|| format!("Failed to process {label}"));
            }
        }
    }
    Ok(outputs)
}

async fn create_dub_for_video(
    config: &DubConfig,
    video: &Path,
    relative: &Path,
    options: DubOptions,
    progress: &dyn ProgressReporter,
    ffmpeg_verified: &mut bool,
) -> Result<PathBuf> {
    // Resolve the SRT before checking the output, matching the Python runner.
    let srt = find_matching_srt(&config.root, video)?;
    let output = output_path(&config.root, video, relative)?;
    let label = relative.display().to_string();

    if skip_allowed(options) && output.exists() && output_is_up_to_date(&output, video, &srt)? {
        // Backfill companion SRT for older runs that only produced the video.
        let output_srt = companion_output_srt(&output);
        if !output_srt.exists() {
            if let Some(parent) = output_srt.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Err(error) = fs::copy(&srt, &output_srt) {
                progress.event(ProgressEvent::new(
                    Stage::Skip,
                    &label,
                    format!("Skipping {label}, but failed to copy companion SRT: {error}"),
                ));
            }
        }
        progress.event(ProgressEvent::new(
            Stage::Skip,
            &label,
            format!("Skipping {label}, output video already exists and is up to date."),
        ));
        return Ok(output);
    }

    if !*ffmpeg_verified {
        let ffmpeg = config.ffmpeg.clone();
        tokio::task::spawn_blocking(move || ffmpeg.verify())
            .await
            .context("ffmpeg verify task failed")??;
        *ffmpeg_verified = true;
    }

    let job_dir = safe_work_path(&config.root, relative);
    let tts_dir = job_dir.join("tts");
    let input_srt = job_dir.join("input.srt");
    let clean_srt = job_dir.join("clean_final.srt");
    let normalized_srt = job_dir.join("clean_final_tech.srt");
    let vi_srt = job_dir.join("clean_final_vi.srt");
    let speech_plan_cache = job_dir.join("speech_plans.json");
    let render_manifest = job_dir.join("render_manifest.json");
    let voiceover = job_dir.join("vietnamese_voiceover.mp3");

    {
        let srt = srt.clone();
        let input_srt = input_srt.clone();
        let clean_srt = clean_srt.clone();
        let normalized_srt = normalized_srt.clone();
        let job_dir = job_dir.clone();
        let tts_dir = tts_dir.clone();
        let output = output.clone();
        let force_tts = options.force_tts;
        let label_for_clean = label.clone();

        progress.event(ProgressEvent::new(
            Stage::Clean,
            &label,
            format!("Cleaning subtitles for {label}"),
        ));

        let stats = tokio::task::spawn_blocking(move || {
            fs::create_dir_all(&job_dir)
                .with_context(|| format!("Failed to create {}", job_dir.display()))?;
            fs::create_dir_all(output.parent().context("Output path has no parent")?)?;
            if force_tts && tts_dir.exists() {
                fs::remove_dir_all(&tts_dir)
                    .with_context(|| format!("Failed to remove {}", tts_dir.display()))?;
            }
            fs::copy(&srt, &input_srt).with_context(|| {
                format!(
                    "Failed to copy {} to {}",
                    srt.display(),
                    input_srt.display()
                )
            })?;
            let stats = clean_youtube_rolling_subtitles(&input_srt, &clean_srt)?;
            fs::copy(&clean_srt, &normalized_srt)?;
            Ok::<_, anyhow::Error>(stats)
        })
        .await
        .with_context(|| format!("Clean task failed for {label_for_clean}"))??;

        progress.event(ProgressEvent::new(
            Stage::Clean,
            &label,
            format!(
                "Cleaned {} raw -> {} linear -> {} final segments.",
                stats.raw, stats.linear, stats.final_segments
            ),
        ));
    }

    let azure = config
        .azure
        .as_ref()
        .context("Missing AZURE_KEY. Add it to <root>/.env or export it")?;
    if let Some(mode) = options.natural_mode {
        let gemini = config.gemini.as_ref().context(
            "Missing GEMINI_API_KEYS. Add Gemini API keys before enabling Natural Voice",
        )?;
        let styles = (!azure.style.is_empty())
            .then(|| azure.style.clone())
            .into_iter()
            .collect();
        let voice = AzureVoice::from_short_name(azure.voice.clone(), styles);
        let source_for_cache = fs::read(&clean_srt)?;
        let cache_key = tts_cache_key(
            "gemini-speech-plan-v2",
            &format!("{mode:?}:{}", options.translate),
            &voice.styles.join("|"),
            &String::from_utf8_lossy(&source_for_cache),
        );
        let plans = fs::read(&speech_plan_cache)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<SpeechPlanCache>(&bytes).ok())
            .filter(|cache| cache.cache_key == cache_key)
            .map(|cache| cache.document)
            .filter(|document| {
                document.plans.len() == parse_srt(&clean_srt).map_or(0, |segments| segments.len())
                    && document
                        .plans
                        .iter()
                        .all(|plan| plan.validate(&voice.styles).is_ok())
            });
        let plans = if let Some(plans) = plans {
            progress.event(ProgressEvent::new(
                Stage::Tts,
                &label,
                "Loaded cached Natural Voice speech plans.".to_owned(),
            ));
            plans
        } else {
            let plans = create_speech_plans(
                &gemini.keys,
                &clean_srt,
                options.translate,
                mode,
                &voice.styles,
            )
            .await?;
            let _ = fs::write(
                &speech_plan_cache,
                serde_json::to_vec_pretty(&SpeechPlanCache {
                    cache_key,
                    document: plans.clone(),
                })?,
            );
            plans
        };
        let source = parse_srt(&clean_srt)?;
        let display_segments: Result<Vec<_>> = source
            .iter()
            .map(|segment| {
                let plan = plans
                    .plans
                    .iter()
                    .find(|plan| plan.index == segment.index)
                    .context("Natural Voice plan is missing a subtitle segment")?;
                Ok(SrtSegment {
                    index: segment.index,
                    start: segment.start.clone(),
                    end: segment.end.clone(),
                    text: plan.display_text.clone(),
                })
            })
            .collect();
        write_srt(&vi_srt, &display_segments?)?;
        if voice.capabilities.timing_events == crate::azure_voices::CapabilityState::Supported {
            generate_all_tts_for_aligned_utterances(
                azure,
                &config.ffmpeg,
                &vi_srt,
                &plans.plans,
                &gemini.keys,
                &voice,
                &tts_dir,
                &render_manifest,
                &label,
                progress,
            )
            .await?;
        } else {
            let _ = fs::remove_file(&render_manifest);
            generate_all_tts_for_speech_plans(
                azure,
                &config.ffmpeg,
                &vi_srt,
                &plans.plans,
                mode,
                &gemini.keys,
                &voice,
                &tts_dir,
                &label,
                progress,
            )
            .await?;
        }
    } else {
        if options.translate {
            let gemini = config
                .gemini
                .as_ref()
                .context("Missing GEMINI_API_KEYS. Add Gemini API keys before translating")?;
            translate_srt(&gemini.keys, &clean_srt, &vi_srt).await?;
        } else {
            fs::copy(&clean_srt, &vi_srt)?;
        }
        generate_all_tts(azure, &config.ffmpeg, &vi_srt, &tts_dir, &label, progress).await?;
    }

    progress.event(ProgressEvent::new(
        Stage::Audio,
        &label,
        format!("Building voiceover for {label}"),
    ));
    {
        let ffmpeg = config.ffmpeg.clone();
        let vi_srt = vi_srt.clone();
        let tts_dir = tts_dir.clone();
        let voiceover = voiceover.clone();
        let job_dir = job_dir.clone();
        let render_manifest = render_manifest.clone();
        let label_audio = label.clone();
        let natural_mode = options.natural_mode;
        tokio::task::spawn_blocking(move || {
            if natural_mode.is_some() && render_manifest.is_file() {
                build_voiceover_track_from_manifest(
                    &ffmpeg,
                    &vi_srt,
                    &render_manifest,
                    &voiceover,
                    &job_dir,
                    Some(MAX_TIME_STRETCH_TEMPO),
                )
            } else if natural_mode.is_some() {
                build_voiceover_track_with_tempo_cap(
                    &ffmpeg,
                    &vi_srt,
                    &tts_dir,
                    &voiceover,
                    &job_dir,
                    Some(MAX_TIME_STRETCH_TEMPO),
                )
            } else {
                build_voiceover_track(&ffmpeg, &vi_srt, &tts_dir, &voiceover, &job_dir)
            }
        })
        .await
        .with_context(|| format!("Voiceover task failed for {label_audio}"))??;
    }

    progress.event(ProgressEvent::new(
        Stage::Video,
        &label,
        format!("Building dubbed video for {label}"),
    ));
    {
        let ffmpeg = config.ffmpeg.clone();
        let video = video.to_path_buf();
        let voiceover = voiceover.clone();
        let output = output.clone();
        let label_video = label.clone();
        tokio::task::spawn_blocking(move || {
            build_dubbed_video(&ffmpeg, &video, &voiceover, &output)
        })
        .await
        .with_context(|| format!("Mux task failed for {label_video}"))??;
    }

    // Same basename as the dubbed video so VLC (and similar players) auto-detect subtitles.
    let output_srt = companion_output_srt(&output);
    {
        let vi_srt = vi_srt.clone();
        let output_srt = output_srt.clone();
        let label_srt = label.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = output_srt.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create {}", parent.display()))?;
            }
            fs::copy(&vi_srt, &output_srt).with_context(|| {
                format!(
                    "Failed to copy companion SRT {} -> {}",
                    vi_srt.display(),
                    output_srt.display()
                )
            })?;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .with_context(|| format!("Companion SRT task failed for {label_srt}"))??;
    }

    progress.event(ProgressEvent::new(
        Stage::Done,
        &label,
        format!(
            "Done: {} (+ {})",
            output.display(),
            output_srt
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "companion.srt".to_owned())
        ),
    ));
    Ok(output)
}

fn skip_allowed(options: DubOptions) -> bool {
    !options.force && !options.force_tts
}

fn output_is_up_to_date(output: &Path, video: &Path, srt: &Path) -> Result<bool> {
    let output_metadata = output
        .metadata()
        .with_context(|| format!("Failed to read metadata for {}", output.display()))?;
    if output_metadata.len() == 0 {
        return Ok(false);
    }
    let output_mtime = output_metadata
        .modified()
        .with_context(|| format!("Failed to read modification time for {}", output.display()))?;
    Ok(is_at_least_as_new(
        output_mtime,
        modified(video)?,
        modified(srt)?,
    ))
}

fn modified(path: &Path) -> Result<SystemTime> {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .with_context(|| format!("Failed to read modification time for {}", path.display()))
}

fn is_at_least_as_new(output: SystemTime, video: SystemTime, srt: SystemTime) -> bool {
    output >= video && output >= srt
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    #[test]
    fn skip_requires_fresh_output_without_force_flags() {
        let normal = DubOptions::default();
        assert!(skip_allowed(normal));
        assert!(!skip_allowed(DubOptions {
            force: true,
            ..normal
        }));
        assert!(!skip_allowed(DubOptions {
            force_tts: true,
            ..normal
        }));

        let old = UNIX_EPOCH + Duration::from_secs(1);
        let current = UNIX_EPOCH + Duration::from_secs(2);
        assert!(is_at_least_as_new(current, current, old));
        assert!(!is_at_least_as_new(old, current, old));
        assert!(!is_at_least_as_new(old, old, current));
    }
}
