use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::azure_voices::{AzureVoice, CapabilityState};
use crate::build_audio::{build_voiceover_track, build_voiceover_track_from_manifest};
use crate::build_video::build_dubbed_video;
use crate::clean_rolling::clean_youtube_rolling_subtitles;
use crate::config::{DubConfig, DubOptions, Provider, SourceTiming};
use crate::duration_solver::MAX_TIME_STRETCH_TEMPO;
use crate::metrics;
use crate::paths::{
    companion_output_srt, find_matching_srt, find_targets, output_path, safe_work_path,
    video_relative_path,
};
use crate::progress::{ProgressEvent, ProgressReporter, Stage};
use crate::source_transcription::transcribe_audio;
use crate::speech_plan::SpeechPlanDocument;
use crate::srt::{SrtSegment, parse_srt, write_srt};
use crate::sync_timeline::{
    GranularSourceWord, SourceTimeline, SyncLayout, TimingMode, build_sync_layout,
};
use crate::translate_gemini::{create_speech_plans, translate_srt};
use crate::tts_azure::{
    generate_all_tts, generate_all_tts_for_aligned_utterances, generate_all_tts_for_speech_plans,
    generate_all_tts_for_unanchored_utterances, load_duration_calibration_chars_per_second,
};
use crate::tts_cache::tts_cache_key;
use crate::utterance::{
    ManifestStatus, RENDER_MANIFEST_VERSION, RenderManifest, ScheduledAudio, TimingPipelineId,
};

// One-release internal escape hatch; remove after source-aligned rollout acceptance.
const NATURAL_TIMING_PIPELINE_ENV: &str = "DUBORA_INTERNAL_NATURAL_TIMING_PIPELINE";
// Set to 0 to keep raw TTS files; DUBORA_INTERNAL_PAUSE_FIT_INTERNAL remains reserved for a
// later paid-listen rollout and is intentionally not read here.
const PAUSE_FIT_ENV: &str = "DUBORA_INTERNAL_PAUSE_FIT";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NaturalTimingPipeline {
    SourceAligned,
    UnanchoredContinuous,
    SegmentedRollback,
}

fn parse_natural_timing_pipeline(value: Option<&str>) -> Result<NaturalTimingPipeline> {
    match value.map(str::trim) {
        None | Some("") | Some("source-aligned") => Ok(NaturalTimingPipeline::SourceAligned),
        Some("segmented-rollback") => Ok(NaturalTimingPipeline::SegmentedRollback),
        Some(value) => bail!(
            "Invalid {NATURAL_TIMING_PIPELINE_ENV}={value:?}; expected source-aligned or segmented-rollback"
        ),
    }
}

fn natural_timing_pipeline(timing_events: CapabilityState) -> Result<NaturalTimingPipeline> {
    let configured = match std::env::var(NATURAL_TIMING_PIPELINE_ENV) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("{NATURAL_TIMING_PIPELINE_ENV} must contain valid Unicode")
        }
    };
    Ok(select_natural_timing_pipeline(
        timing_events,
        parse_natural_timing_pipeline(configured.as_deref())?,
    ))
}

fn select_natural_timing_pipeline(
    timing_events: CapabilityState,
    configured: NaturalTimingPipeline,
) -> NaturalTimingPipeline {
    if timing_events == CapabilityState::Supported {
        configured
    } else if configured == NaturalTimingPipeline::SegmentedRollback {
        NaturalTimingPipeline::SegmentedRollback
    } else {
        NaturalTimingPipeline::UnanchoredContinuous
    }
}

fn timing_pipeline_id(pipeline: NaturalTimingPipeline) -> TimingPipelineId {
    match pipeline {
        NaturalTimingPipeline::SourceAligned => TimingPipelineId::SourceAligned,
        NaturalTimingPipeline::UnanchoredContinuous => TimingPipelineId::UnanchoredContinuous,
        NaturalTimingPipeline::SegmentedRollback => TimingPipelineId::SegmentedRollback,
    }
}

fn pause_fit_enabled(options: DubOptions) -> Result<bool> {
    if !options.pause_fit {
        return Ok(false);
    }
    match std::env::var(PAUSE_FIT_ENV) {
        Ok(value) => Ok(value.trim() != "0"),
        Err(std::env::VarError::NotPresent) => Ok(true),
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("{PAUSE_FIT_ENV} must contain valid Unicode")
        }
    }
}

fn invalidate_mismatched_render_manifest(
    path: &Path,
    pipeline: NaturalTimingPipeline,
) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let matches_pipeline = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<RenderManifest>(&bytes).ok())
        .is_some_and(|manifest| {
            manifest.version == RENDER_MANIFEST_VERSION
                && manifest.timing_pipeline == timing_pipeline_id(pipeline)
                && manifest.semantic_alignment_supported
                    == (pipeline == NaturalTimingPipeline::SourceAligned)
        });
    if matches_pipeline {
        return Ok(false);
    }
    fs::remove_file(path)
        .with_context(|| format!("Failed to remove stale render manifest {}", path.display()))?;
    Ok(true)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SpeechPlanCache {
    cache_key: String,
    document: SpeechPlanDocument,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceTranscriptionCache {
    version: u8,
    cache_key: String,
    words: Vec<GranularSourceWord>,
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

fn percentile(values: &mut [f64], percentile: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let position = ((values.len() - 1) as f64 * percentile).round() as usize;
    values.get(position).copied()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewWindow {
    entry_id: String,
    island_id: Option<String>,
    start_ms: u32,
    end_ms: u32,
    reasons: Vec<String>,
}

fn review_reason_matches_entry(target: &str, entry: &ScheduledAudio) -> bool {
    if entry.id == target {
        return true;
    }
    if let Some(utterance) = target.strip_prefix("utterance-")
        && entry.id == format!("utterance-fallback-{utterance}")
    {
        return true;
    }
    if let Some(index) = target
        .strip_prefix("segment-")
        .and_then(|value| value.parse::<usize>().ok())
    {
        return entry.source_segments.contains(&index);
    }
    if target.strip_prefix("island-").is_some() {
        return entry
            .island_id
            .as_deref()
            .is_some_and(|entry_island| entry_island == target);
    }
    false
}

fn review_windows(manifest: &RenderManifest) -> Vec<ReviewWindow> {
    let mut windows = Vec::new();
    for reason in &manifest.diagnostics.review_reasons {
        let target = reason.split(':').next().unwrap_or(reason);
        if target.starts_with("island-") {
            let entries = manifest
                .entries
                .iter()
                .filter(|entry| review_reason_matches_entry(target, entry))
                .collect::<Vec<_>>();
            if !entries.is_empty() {
                if let Some(window) = windows
                    .iter_mut()
                    .find(|window: &&mut ReviewWindow| window.entry_id == target)
                {
                    window.reasons.push(reason.clone());
                } else {
                    windows.push(ReviewWindow {
                        entry_id: target.to_owned(),
                        island_id: Some(target.to_owned()),
                        start_ms: entries
                            .iter()
                            .map(|entry| entry.start_ms)
                            .min()
                            .unwrap_or(0),
                        end_ms: entries.iter().map(|entry| entry.end_ms).max().unwrap_or(0),
                        reasons: vec![reason.clone()],
                    });
                }
            }
            continue;
        }
        for entry in &manifest.entries {
            if !review_reason_matches_entry(target, entry) {
                continue;
            }
            if let Some(window) = windows
                .iter_mut()
                .find(|window: &&mut ReviewWindow| window.entry_id == entry.id)
            {
                window.reasons.push(reason.clone());
            } else {
                windows.push(ReviewWindow {
                    entry_id: entry.id.clone(),
                    island_id: entry.island_id.clone(),
                    start_ms: entry.start_ms,
                    end_ms: entry.end_ms,
                    reasons: vec![reason.clone()],
                });
            }
        }
    }
    windows.sort_by_key(|window| (window.start_ms, window.entry_id.clone()));
    windows
}

fn format_timestamp_ms(milliseconds: u32) -> String {
    let total_seconds = milliseconds / 1_000;
    format!(
        "{:02}:{:02}.{:03}",
        total_seconds / 60,
        total_seconds % 60,
        milliseconds % 1_000
    )
}

fn format_review_windows(manifest: &RenderManifest) -> String {
    let windows = review_windows(manifest);
    if windows.is_empty() {
        return "see sync_report.json for the affected plan/anchor".to_owned();
    }
    windows
        .iter()
        .map(|window| {
            format!(
                "{} [{}-{}]",
                window.entry_id,
                format_timestamp_ms(window.start_ms),
                format_timestamp_ms(window.end_ms)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn manifest_diagnostics(manifest: &RenderManifest) -> serde_json::Value {
    let mut tempos = manifest
        .entries
        .iter()
        .map(|entry| entry.tempo)
        .filter(|tempo| tempo.is_finite())
        .collect::<Vec<_>>();
    let review_entry_ids = manifest
        .entries
        .iter()
        .filter(|entry| {
            entry.tempo > crate::sync_timeline::MAX_TIMING_TEMPO + f64::EPSILON
                || entry.rendered_audio_ms > entry.end_ms.saturating_sub(entry.start_ms)
        })
        .map(|entry| entry.id.clone())
        .collect::<Vec<_>>();
    let review_islands = manifest
        .entries
        .iter()
        .filter(|entry| {
            entry.tempo > crate::sync_timeline::MAX_TIMING_TEMPO + f64::EPSILON
                || entry.rendered_audio_ms > entry.end_ms.saturating_sub(entry.start_ms)
        })
        .filter_map(|entry| {
            entry.island_id.as_ref().map(|island_id| {
                serde_json::json!({
                    "islandId": island_id,
                    "startMs": entry.start_ms,
                    "endMs": entry.end_ms,
                })
            })
        })
        .collect::<Vec<_>>();
    let stretch_count = manifest
        .entries
        .iter()
        .filter(|entry| entry.tempo > 1.0 + f64::EPSILON)
        .count();
    let over_slot_count = manifest
        .entries
        .iter()
        .filter(|entry| entry.rendered_audio_ms > entry.end_ms.saturating_sub(entry.start_ms))
        .count();
    let technically_safe = manifest.entries.iter().all(|entry| {
        entry.tempo <= crate::sync_timeline::MAX_TIMING_TEMPO + f64::EPSILON
            && entry.rendered_audio_ms <= entry.end_ms.saturating_sub(entry.start_ms)
    });
    let mut anchor_drifts = manifest
        .diagnostics
        .anchor_drifts_ms
        .iter()
        .map(|drift| (*drift as f64).abs())
        .collect::<Vec<_>>();
    let mut prediction_errors = manifest
        .diagnostics
        .prediction_errors_ms
        .iter()
        .map(|error| (*error as f64).abs())
        .collect::<Vec<_>>();
    let review_windows = review_windows(manifest)
        .into_iter()
        .map(|window| {
            serde_json::json!({
                "entryId": window.entry_id,
                "islandId": window.island_id,
                "startMs": window.start_ms,
                "endMs": window.end_ms,
                "reasons": window.reasons,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "manifestVersion": manifest.version,
        "manifestStatus": manifest.status,
        "timingPipeline": manifest.timing_pipeline,
        "entryCount": manifest.entries.len(),
        "stretchCount": stretch_count,
        "overSlotCount": over_slot_count,
        "technicallySafe": technically_safe,
        "maxTempo": tempos.iter().copied().reduce(f64::max),
        "tempoP50": percentile(&mut tempos, 0.50),
        "tempoP95": percentile(&mut tempos, 0.95),
        "anchorDriftP50Ms": percentile(&mut anchor_drifts, 0.50),
        "anchorDriftP95Ms": percentile(&mut anchor_drifts, 0.95),
        "predictionErrorCount": prediction_errors.len(),
        "predictionErrorP50Ms": percentile(&mut prediction_errors, 0.50),
        "predictionErrorP95Ms": percentile(&mut prediction_errors, 0.95),
        "predictionErrorMaxMs": prediction_errors.iter().copied().reduce(f64::max),
        "reviewReasons": manifest.diagnostics.review_reasons.clone(),
        "reviewEntryIds": review_entry_ids,
        "reviewIslands": review_islands,
        "reviewWindows": review_windows,
    })
}

fn sync_report(
    timeline: &SourceTimeline,
    layout: &crate::sync_timeline::SyncLayout,
    semantic_alignment: &str,
    status: &str,
    manifest: Option<&RenderManifest>,
) -> serde_json::Value {
    let diagnostics = manifest.map(manifest_diagnostics);
    serde_json::json!({
        "version": 2,
        "timingMode": timeline.mode,
        "timingProvider": timeline.timing_provider,
        "timingFallbackReason": timeline.timing_fallback_reason,
        "granularMatchCount": timeline.granular_match_count,
        "granularSourceWordCount": timeline.granular_source_word_count,
        "cueOverlapCount": timeline.cue_overlap_count,
        "tokenCount": timeline.tokens.len(),
        "blockCount": layout.blocks.len(),
        "islandCount": layout.islands.len(),
        "semanticAlignment": semantic_alignment,
        "timingPipeline": manifest.map(|value| value.timing_pipeline),
        "status": status,
        "providerMetrics": metrics::snapshot(),
        "diagnostics": diagnostics,
    })
}

fn source_transcription_cache_key(video: &Path, locale: &str, region: &str) -> Result<String> {
    let metadata = fs::metadata(video)
        .with_context(|| format!("Failed to inspect source video {}", video.display()))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|time| format!("{}:{}", time.as_secs(), time.subsec_nanos()))
        .unwrap_or_else(|| "unknown".to_owned());
    Ok(tts_cache_key(
        "azure-fast-source-v1",
        locale,
        region,
        &format!("{}:{}:{modified}", metadata.len(), video.display()),
    ))
}

fn translation_cache_key(input: &Path) -> Result<String> {
    let bytes = fs::read(input)
        .with_context(|| format!("Failed to read subtitle source {}", input.display()))?;
    Ok(tts_cache_key(
        "gemini-translation-v2",
        "",
        "",
        &String::from_utf8_lossy(&bytes),
    ))
}

async fn source_timeline_for_video(
    config: &DubConfig,
    video: &Path,
    input_srt: &Path,
    clean_srt: &Path,
    job_dir: &Path,
    options: DubOptions,
    progress: &dyn ProgressReporter,
    label: &str,
) -> Result<SourceTimeline> {
    if options.source_timing != SourceTiming::AzureFast {
        return SourceTimeline::from_files(input_srt, clean_srt);
    }
    let azure = config
        .azure
        .as_ref()
        .context("Missing AZURE_KEY for Azure Fast Transcription")?;
    let locale = std::env::var("DUBORA_SOURCE_LOCALE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if options.translate {
                "en-US".to_owned()
            } else {
                "vi-VN".to_owned()
            }
        });
    let cache_path = job_dir.join("source_transcript.json");
    let audio_path = job_dir.join("source_audio.wav");
    let cache_key = source_transcription_cache_key(video, &locale, &azure.region)?;
    let cached = fs::read(&cache_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SourceTranscriptionCache>(&bytes).ok())
        .filter(|cache| cache.version == 1 && cache.cache_key == cache_key)
        .map(|cache| cache.words);
    let words = if let Some(words) = cached {
        metrics::record_azure_transcription_cache_hit();
        if let Ok(duration) = config.ffmpeg.audio_duration(&audio_path) {
            metrics::record_azure_transcription_audio_ms((duration * 1_000.0) as u64);
        }
        progress.event(ProgressEvent::new(
            Stage::Tts,
            label,
            format!(
                "Loaded cached Azure Fast Transcription timing ({} words).",
                words.len()
            ),
        ));
        words
    } else {
        progress.event(ProgressEvent::new(
            Stage::Tts,
            label,
            format!("Extracting source audio for Azure Fast Transcription ({locale})."),
        ));
        let ffmpeg = config.ffmpeg.clone();
        let video = video.to_owned();
        let audio_path_for_extract = audio_path.clone();
        tokio::task::spawn_blocking(move || {
            ffmpeg.extract_mono_wav(&video, &audio_path_for_extract)
        })
        .await
        .context("Source audio extraction task failed")??;
        if let Ok(duration) = config.ffmpeg.audio_duration(&audio_path) {
            metrics::record_azure_transcription_audio_ms((duration * 1_000.0) as u64);
        }
        if progress.is_cancelled() {
            bail!("Dub job cancelled");
        }
        progress.event(ProgressEvent::new(
            Stage::Tts,
            label,
            "Requesting Azure Fast Transcription word timestamps.".to_owned(),
        ));
        let words = transcribe_audio(azure, &audio_path, &locale).await?;
        fs::write(
            &cache_path,
            serde_json::to_vec_pretty(&SourceTranscriptionCache {
                version: 1,
                cache_key,
                words: words.clone(),
            })?,
        )
        .with_context(|| format!("Failed to write {}", cache_path.display()))?;
        words
    };
    match SourceTimeline::from_files_with_granular(input_srt, clean_srt, &words) {
        Ok(timeline) => Ok(timeline),
        Err(error) => {
            let mut fallback = SourceTimeline::from_files(input_srt, clean_srt)?;
            let projected = fallback.project_required_anchor_times(&words);
            let reason = format!(
                "{}; granular timing matched below the configured threshold",
                error
            );
            fallback.timing_provider = if projected > 0 {
                Some("azureFastAnchorProjection".to_owned())
            } else {
                Some("captionDerivedFallback".to_owned())
            };
            fallback.timing_fallback_reason = Some(if projected > 0 {
                format!(
                    "{reason}; projected {projected} required anchor(s) from source-language ASR"
                )
            } else {
                reason
            });
            Ok(fallback)
        }
    }
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
    let mut review_labels = Vec::new();
    let mut failed_labels = Vec::new();
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
                if progress.is_cancelled() || error.to_string() == "Dub job cancelled" {
                    return Err(error);
                }
                progress.event(ProgressEvent::new(
                    Stage::Error,
                    &label,
                    format!("Failed to process {label}: {error:#}"),
                ));
                if error.to_string().starts_with("review_required:") {
                    review_labels.push(label);
                } else {
                    failed_labels.push(format!("{label}: {error:#}"));
                }
            }
        }
    }
    if !review_labels.is_empty() || !failed_labels.is_empty() {
        bail!(
            "Batch incomplete: review required [{}]; failed [{}] ({} video(s) completed successfully)",
            review_labels.join(", "),
            failed_labels.join("; "),
            outputs.len()
        );
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
    let output_srt = companion_output_srt(&output);
    let label = relative.display().to_string();
    let pause_fit = pause_fit_enabled(options)?;

    if skip_allowed(options) && output_pair_is_up_to_date(&output, &output_srt, video, &srt)? {
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
    let source_timeline_path = job_dir.join("source_timeline.json");
    let sync_report_path = job_dir.join("sync_report.json");
    let provider_metrics_path = job_dir.join("provider_metrics.json");
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
    let mut rendered_sync_layout: Option<SyncLayout> = None;
    if let Some(mode) = options.natural_mode {
        metrics::reset();
        let gemini = config.gemini.as_ref().context(
            "Missing GEMINI_API_KEYS. Add Gemini API keys before enabling Natural Voice",
        )?;
        let styles = (!azure.style.is_empty())
            .then(|| azure.style.clone())
            .into_iter()
            .collect();
        let voice = AzureVoice::from_short_name(azure.voice.clone(), styles);
        let calibrated_chars_per_second =
            load_duration_calibration_chars_per_second(&tts_dir, &voice);
        let source_timeline = match source_timeline_for_video(
            config, video, &input_srt, &clean_srt, &job_dir, options, progress, &label,
        )
        .await
        {
            Ok(timeline) => timeline,
            Err(error) if options.source_timing == SourceTiming::AzureFast => {
                let safe_reason = error.to_string().chars().take(300).collect::<String>();
                progress.event(ProgressEvent::new(
                    Stage::Tts,
                    &label,
                    format!(
                        "Azure Fast Transcription unavailable; falling back to caption-derived timing: {safe_reason}"
                    ),
                ));
                let mut fallback = SourceTimeline::from_files(&input_srt, &clean_srt)?;
                fallback.timing_provider = Some("captionDerivedFallback".to_owned());
                fallback.timing_fallback_reason = Some(safe_reason);
                fallback
            }
            Err(error) => return Err(error),
        };
        let source_segments = parse_srt(&clean_srt)?;
        let sync_layout = build_sync_layout(&source_timeline, &source_segments)?;
        let source_timeline_bytes = serde_json::to_vec(&source_timeline)?;
        // Translation is a separate indexed stage.  Feeding the model a translated, fixed-index
        // SRT prevents a combined translate+plan response from moving a rolling-caption tail
        // into its neighbour; the speech planner now only chooses delivery for text that already
        // belongs to the current index.
        let plan_input_srt = if options.translate {
            let translated = job_dir.join("translated_source.srt");
            let cache_path = job_dir.join("translated_source.srt.sha256");
            let cache_key = translation_cache_key(&clean_srt)?;
            let cache_hit = translated.is_file()
                && fs::read_to_string(&cache_path).is_ok_and(|stored| stored.trim() == cache_key);
            if !cache_hit {
                progress.event(ProgressEvent::new(
                    Stage::Tts,
                    &label,
                    "Translating indexed subtitle text before Natural Voice planning.".to_owned(),
                ));
                translate_srt(&gemini.keys, &clean_srt, &translated).await?;
                fs::write(&cache_path, cache_key)?;
            }
            translated
        } else {
            clean_srt.clone()
        };
        let source = parse_srt(&plan_input_srt)?;
        sync_layout
            .validate(
                &source
                    .iter()
                    .map(|segment| segment.index)
                    .collect::<Vec<_>>(),
            )
            .context("Translated subtitle indices changed from the source layout")?;
        let mut source_for_cache = fs::read(&plan_input_srt)?;
        source_for_cache.extend_from_slice(&source_timeline_bytes);
        let cache_key = tts_cache_key(
            // v23 invalidates plans before conversational-register and framing compaction.
            "gemini-speech-plan-v23",
            // Keep this comment adjacent to the version so future prompt/input changes bump it.
            &format!("{mode:?}:{}", options.translate),
            &format!("{}|{}", voice.short_name, voice.styles.join("|")),
            &String::from_utf8_lossy(&source_for_cache),
        );
        fs::write(
            &source_timeline_path,
            serde_json::to_vec_pretty(&source_timeline)?,
        )
        .with_context(|| {
            format!(
                "Failed to write source timeline {}",
                source_timeline_path.display()
            )
        })?;
        let semantic_alignment = if source_timeline.has_projected_anchor_timing() {
            "anchorProjected"
        } else if source_timeline.mode != TimingMode::CueOnly
            && voice.capabilities.timing_events == crate::azure_voices::CapabilityState::Supported
        {
            "validated"
        } else {
            "unsupported"
        };
        progress.event(ProgressEvent::new(
            Stage::Tts,
            &label,
            format!(
                "Reconstructed source timing: {:?}, {} source words, {} blocks in {} sync islands.",
                source_timeline.mode,
                source_timeline.tokens.len(),
                sync_layout.blocks.len(),
                sync_layout.islands.len()
            ),
        ));
        fs::write(
            &sync_report_path,
            serde_json::to_vec_pretty(&sync_report(
                &source_timeline,
                &sync_layout,
                semantic_alignment,
                "planned",
                None,
            ))?,
        )?;
        let plans = fs::read(&speech_plan_cache)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<SpeechPlanCache>(&bytes).ok())
            .filter(|cache| cache.cache_key == cache_key)
            .map(|cache| cache.document)
            .filter(|document| {
                document.plans.len() == source_segments.len()
                    && document
                        .plans
                        .iter()
                        .all(|plan| plan.validate(&voice.styles).is_ok())
            });
        let plans = if let Some(plans) = plans {
            metrics::record_gemini_cache_hit();
            progress.event(ProgressEvent::new(
                Stage::Tts,
                &label,
                "Loaded cached Natural Voice speech plans.".to_owned(),
            ));
            plans
        } else {
            let plans_result = create_speech_plans(
                &gemini.keys,
                &plan_input_srt,
                false,
                mode,
                &voice.styles,
                Some(&source_timeline),
                calibrated_chars_per_second,
            )
            .await;
            let plans = match plans_result {
                Ok(plans) => plans,
                Err(error) => {
                    let _ = fs::write(
                        &sync_report_path,
                        serde_json::to_vec_pretty(&sync_report(
                            &source_timeline,
                            &sync_layout,
                            semantic_alignment,
                            "failed",
                            None,
                        ))?,
                    );
                    let _ = fs::write(
                        &provider_metrics_path,
                        serde_json::to_vec_pretty(&metrics::snapshot())?,
                    );
                    return Err(error);
                }
            };
            let _ = fs::write(
                &speech_plan_cache,
                serde_json::to_vec_pretty(&SpeechPlanCache {
                    cache_key,
                    document: plans.clone(),
                })?,
            );
            plans
        };
        fs::write(
            &provider_metrics_path,
            serde_json::to_vec_pretty(&metrics::snapshot())?,
        )?;
        let display_segments = display_segments_from_plans(&source, &plans)?;
        sync_layout
            .validate(
                &display_segments
                    .iter()
                    .map(|segment| segment.index)
                    .collect::<Vec<_>>(),
            )
            .context("Translated subtitle indices changed from the source layout")?;
        write_srt(&vi_srt, &display_segments)?;
        let timing_pipeline = natural_timing_pipeline(voice.capabilities.timing_events)?;
        if invalidate_mismatched_render_manifest(&render_manifest, timing_pipeline)? {
            progress.event(ProgressEvent::new(
                Stage::Tts,
                &label,
                "Discarded a render manifest from a different Natural Voice timing pipeline."
                    .to_owned(),
            ));
        }
        if timing_pipeline == NaturalTimingPipeline::SegmentedRollback
            && voice.capabilities.timing_events == CapabilityState::Supported
        {
            progress.event(ProgressEvent::new(
                Stage::Tts,
                &label,
                "Using the one-release segmented Natural Voice rollback pipeline.".to_owned(),
            ));
        }
        let tts_result = match timing_pipeline {
            NaturalTimingPipeline::SourceAligned => {
                generate_all_tts_for_aligned_utterances(
                    azure,
                    &config.ffmpeg,
                    &vi_srt,
                    &plans.plans,
                    mode,
                    &source_timeline,
                    &sync_layout,
                    &gemini.keys,
                    &voice,
                    &tts_dir,
                    &render_manifest,
                    pause_fit,
                    &label,
                    progress,
                )
                .await
            }
            NaturalTimingPipeline::SegmentedRollback => {
                generate_all_tts_for_speech_plans(
                    azure,
                    &config.ffmpeg,
                    &vi_srt,
                    &plans.plans,
                    mode,
                    &source_timeline,
                    &sync_layout,
                    &gemini.keys,
                    &voice,
                    &tts_dir,
                    &render_manifest,
                    pause_fit,
                    &label,
                    progress,
                )
                .await
            }
            NaturalTimingPipeline::UnanchoredContinuous => {
                generate_all_tts_for_unanchored_utterances(
                    azure,
                    &config.ffmpeg,
                    &vi_srt,
                    &plans.plans,
                    mode,
                    &source_timeline,
                    &sync_layout,
                    &gemini.keys,
                    &voice,
                    &tts_dir,
                    &render_manifest,
                    pause_fit,
                    &label,
                    progress,
                )
                .await
            }
        };
        if let Err(error) = tts_result {
            let manifest = fs::read(&render_manifest)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<RenderManifest>(&bytes).ok());
            let _ = fs::write(
                &sync_report_path,
                serde_json::to_vec_pretty(&sync_report(
                    &source_timeline,
                    &sync_layout,
                    semantic_alignment,
                    "failed",
                    manifest.as_ref(),
                ))?,
            );
            let _ = fs::write(
                &provider_metrics_path,
                serde_json::to_vec_pretty(&metrics::snapshot())?,
            );
            return Err(error);
        }
        if plans.review_required {
            let bytes = fs::read(&render_manifest)?;
            let mut manifest: RenderManifest =
                serde_json::from_slice(&bytes).context("Invalid Natural Voice render manifest")?;
            manifest.status = ManifestStatus::ReviewRequired;
            manifest
                .diagnostics
                .review_reasons
                .push("speech_plan:semantic_anchor_mapping_review".to_owned());
            for plan in plans
                .plans
                .iter()
                .filter(|plan| !plan.semantic_anchors.is_empty())
            {
                manifest.diagnostics.review_reasons.push(format!(
                    "segment-{}:semantic_anchor_mapping_review",
                    plan.index
                ));
            }
            fs::write(&render_manifest, serde_json::to_vec_pretty(&manifest)?)?;
        }
        if let Ok(bytes) = fs::read(&render_manifest)
            && let Ok(manifest) = serde_json::from_slice::<RenderManifest>(&bytes)
            && manifest.status == ManifestStatus::ReviewRequired
        {
            let _ = fs::write(
                &sync_report_path,
                serde_json::to_vec_pretty(&sync_report(
                    &source_timeline,
                    &sync_layout,
                    semantic_alignment,
                    "reviewRequired",
                    Some(&manifest),
                ))?,
            );
            let _ = fs::write(
                &provider_metrics_path,
                serde_json::to_vec_pretty(&metrics::snapshot())?,
            );
            progress.event(ProgressEvent::new(
                Stage::Error,
                &label,
                format!(
                    "Review required for {label}: the source-aligned draft is structurally safe, but duration or semantic constraints still need review. Check timestamps: {}.",
                    format_review_windows(&manifest)
                ),
            ));
            bail!("review_required: {label}");
        }
        if let Ok(bytes) = fs::read(&render_manifest)
            && let Ok(manifest) = serde_json::from_slice::<RenderManifest>(&bytes)
            && matches!(
                manifest.status,
                ManifestStatus::Aligned | ManifestStatus::UnanchoredContinuous
            )
        {
            let status = if manifest.status == ManifestStatus::Aligned {
                "aligned"
            } else {
                "unanchoredContinuous"
            };
            let _ = fs::write(
                &sync_report_path,
                serde_json::to_vec_pretty(&sync_report(
                    &source_timeline,
                    &sync_layout,
                    semantic_alignment,
                    status,
                    Some(&manifest),
                ))?,
            );
            let _ = fs::write(
                &provider_metrics_path,
                serde_json::to_vec_pretty(&metrics::snapshot())?,
            );
        }
        rendered_sync_layout = Some(sync_layout);
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

    let natural_render = options.natural_mode.is_some();
    require_natural_render_manifest(natural_render, &render_manifest)?;
    progress.event(ProgressEvent::new(
        Stage::Audio,
        &label,
        format!("Building voiceover for {label}"),
    ));
    let rendered_source_timeline = if natural_render {
        Some(
            serde_json::from_slice::<SourceTimeline>(&fs::read(&source_timeline_path)?)
                .context("Invalid source timeline artifact before audio render")?,
        )
    } else {
        None
    };
    {
        let ffmpeg = config.ffmpeg.clone();
        let vi_srt = vi_srt.clone();
        let tts_dir = tts_dir.clone();
        let voiceover = voiceover.clone();
        let job_dir = job_dir.clone();
        let render_manifest = render_manifest.clone();
        let source_timeline = rendered_source_timeline;
        let sync_layout = rendered_sync_layout;
        let label_audio = label.clone();
        let natural_render = natural_render;
        tokio::task::spawn_blocking(move || {
            if natural_render {
                build_voiceover_track_from_manifest(
                    &ffmpeg,
                    &vi_srt,
                    source_timeline
                        .as_ref()
                        .context("Missing source timeline for manifest render")?,
                    sync_layout
                        .as_ref()
                        .context("Missing source layout for manifest render")?,
                    &render_manifest,
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
        let output_srt = output_srt.clone();
        let vi_srt = vi_srt.clone();
        let label_video = label.clone();
        tokio::task::spawn_blocking(move || {
            let staged_output = output.with_extension("pending.mp4");
            let staged_output_srt = output_srt.with_extension("pending.srt");
            let _ = fs::remove_file(&staged_output);
            let _ = fs::remove_file(&staged_output_srt);

            fs::copy(&vi_srt, &staged_output_srt).with_context(|| {
                format!(
                    "Failed to stage companion SRT {} -> {}",
                    vi_srt.display(),
                    staged_output_srt.display()
                )
            })?;
            if let Err(error) = build_dubbed_video(&ffmpeg, &video, &voiceover, &staged_output) {
                let _ = fs::remove_file(&staged_output_srt);
                return Err(error);
            }
            replace_output_pair(&staged_output, &staged_output_srt, &output, &output_srt)
        })
        .await
        .with_context(|| format!("Mux task failed for {label_video}"))??;
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

fn replace_output_pair(
    staged_video: &Path,
    staged_srt: &Path,
    output_video: &Path,
    output_srt: &Path,
) -> Result<()> {
    for staged in [staged_video, staged_srt] {
        if !staged.is_file() {
            bail!("Staged output does not exist: {}", staged.display());
        }
    }

    let backup_video = output_video.with_extension("previous.mp4");
    let backup_srt = output_srt.with_extension("previous.srt");
    for (output, backup) in [(output_video, &backup_video), (output_srt, &backup_srt)] {
        if backup.exists() {
            if output.exists() {
                fs::remove_file(backup).with_context(|| {
                    format!("Failed to remove stale backup {}", backup.display())
                })?;
            } else {
                fs::rename(backup, output).with_context(|| {
                    format!("Failed to recover previous output {}", output.display())
                })?;
            }
        }
    }

    let had_video = output_video.exists();
    let had_srt = output_srt.exists();
    if had_video {
        fs::rename(output_video, &backup_video)
            .with_context(|| format!("Failed to preserve {}", output_video.display()))?;
    }
    if had_srt && let Err(error) = fs::rename(output_srt, &backup_srt) {
        if had_video {
            fs::rename(&backup_video, output_video).with_context(|| {
                format!("Failed to restore {} after {error}", output_video.display())
            })?;
        }
        return Err(error).with_context(|| format!("Failed to preserve {}", output_srt.display()));
    }

    let publish =
        fs::rename(staged_video, output_video).and_then(|()| fs::rename(staged_srt, output_srt));
    if let Err(error) = publish {
        let _ = fs::remove_file(output_video);
        let _ = fs::remove_file(output_srt);
        if had_video {
            fs::rename(&backup_video, output_video).with_context(|| {
                format!("Failed to restore {} after {error}", output_video.display())
            })?;
        }
        if had_srt {
            fs::rename(&backup_srt, output_srt).with_context(|| {
                format!("Failed to restore {} after {error}", output_srt.display())
            })?;
        }
        return Err(error).context("Failed to publish dubbed video and companion SRT together");
    }

    let _ = fs::remove_file(backup_video);
    let _ = fs::remove_file(backup_srt);
    Ok(())
}

fn skip_allowed(options: DubOptions) -> bool {
    !options.force && !options.force_tts
}

fn require_natural_render_manifest(natural_render: bool, manifest: &Path) -> Result<()> {
    if natural_render && !manifest.is_file() {
        bail!(
            "Natural Voice render manifest is missing: {}",
            manifest.display()
        );
    }
    Ok(())
}

fn display_segments_from_plans(
    source: &[SrtSegment],
    document: &SpeechPlanDocument,
) -> Result<Vec<SrtSegment>> {
    source
        .iter()
        .map(|segment| {
            let plan = document
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
        .collect()
}

fn output_pair_is_up_to_date(
    output_video: &Path,
    output_srt: &Path,
    input_video: &Path,
    input_srt: &Path,
) -> Result<bool> {
    if !output_video.is_file() || !output_srt.is_file() {
        return Ok(false);
    }
    Ok(output_is_up_to_date(output_video, input_video, input_srt)?
        && output_is_up_to_date(output_srt, input_video, input_srt)?)
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
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::speech_plan::NaturalMode;

    static FAKE_PROVIDER_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[test]
    fn shared_natural_timing_selector_reaches_both_timing_capable_branches() -> Result<()> {
        assert_eq!(
            parse_natural_timing_pipeline(None)?,
            NaturalTimingPipeline::SourceAligned
        );
        let aligned = parse_natural_timing_pipeline(Some("source-aligned"))?;
        let rollback = parse_natural_timing_pipeline(Some("segmented-rollback"))?;

        assert_eq!(
            select_natural_timing_pipeline(CapabilityState::Supported, aligned),
            NaturalTimingPipeline::SourceAligned
        );
        assert_eq!(
            select_natural_timing_pipeline(CapabilityState::Supported, rollback),
            NaturalTimingPipeline::SegmentedRollback
        );
        assert_eq!(
            select_natural_timing_pipeline(CapabilityState::Unsupported, aligned),
            NaturalTimingPipeline::UnanchoredContinuous
        );
        assert_eq!(
            select_natural_timing_pipeline(CapabilityState::Unknown, aligned),
            NaturalTimingPipeline::UnanchoredContinuous
        );
        assert!(parse_natural_timing_pipeline(Some("legacy-ish")).is_err());
        Ok(())
    }

    #[test]
    fn mismatched_pipeline_or_manifest_version_is_regenerated() -> Result<()> {
        let directory = test_directory("natural-pipeline-manifest");
        fs::create_dir_all(&directory)?;
        let path = directory.join("render_manifest.json");

        let mut old = RenderManifest::new(
            ManifestStatus::Aligned,
            TimingPipelineId::SourceAligned,
            Vec::new(),
        )
        .with_semantic_alignment_support(true);
        old.version = RENDER_MANIFEST_VERSION - 1;
        fs::write(&path, serde_json::to_vec(&old)?)?;
        assert!(invalidate_mismatched_render_manifest(
            &path,
            NaturalTimingPipeline::SourceAligned
        )?);
        assert!(!path.exists());
        assert!(require_natural_render_manifest(true, &path).is_err());

        let aligned = RenderManifest::new(
            ManifestStatus::Aligned,
            TimingPipelineId::SourceAligned,
            Vec::new(),
        )
        .with_semantic_alignment_support(true);
        fs::write(&path, serde_json::to_vec(&aligned)?)?;
        assert!(!invalidate_mismatched_render_manifest(
            &path,
            NaturalTimingPipeline::SourceAligned
        )?);
        assert!(path.exists());
        assert!(invalidate_mismatched_render_manifest(
            &path,
            NaturalTimingPipeline::SegmentedRollback
        )?);
        assert!(!path.exists());

        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn skip_requires_fresh_output_without_force_flags() {
        let normal = DubOptions::default();
        assert!(normal.pause_fit);
        assert!(
            !pause_fit_enabled(DubOptions {
                pause_fit: false,
                ..normal
            })
            .unwrap()
        );
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

    #[test]
    fn natural_voice_requires_a_manifest_before_mixing() -> Result<()> {
        let directory = test_directory("missing-natural-manifest");
        fs::create_dir_all(&directory)?;
        let manifest = directory.join("render_manifest.json");

        assert!(require_natural_render_manifest(false, &manifest).is_ok());
        assert!(
            require_natural_render_manifest(true, &manifest)
                .unwrap_err()
                .to_string()
                .contains("render manifest is missing")
        );
        fs::write(&manifest, b"{}")?;
        assert!(require_natural_render_manifest(true, &manifest).is_ok());

        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn fake_provider_runs_both_natural_timing_branches_with_budget() -> Result<()> {
        let _lock = FAKE_PROVIDER_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("fake provider test lock poisoned");
        let directory = test_directory("fake-provider-budget");
        fs::create_dir_all(&directory)?;
        let input_srt = directory.join("input.srt");
        fs::write(
            &input_srt,
            "1\n00:00:00,000 --> 00:00:03,000\nĐây là một lời giải thích.\n",
        )?;

        let ffprobe = directory.join("fake-ffprobe.sh");
        fs::write(
            &ffprobe,
            "#!/bin/sh\ncase \"$*\" in *revision-20000*) printf '4.000\\n' ;; *) printf '1.000\\n' ;; esac\n",
        )?;
        fs::set_permissions(&ffprobe, fs::Permissions::from_mode(0o755))?;
        let ffmpeg = crate::ffmpeg::FfmpegPaths {
            ffmpeg: PathBuf::from("/bin/true"),
            ffprobe,
        };

        let _gemini =
            crate::translate_gemini::install_test_gemini_responder(Box::new(|_, kind| {
                Ok(match kind {
                    crate::metrics::GeminiRequestKind::Planning => serde_json::json!({
                        "plans": [{
                            "index": 1,
                            "displayText": "Đây là một lời giải thích.",
                            "spokenText": "Đây là một lời giải thích.",
                            "targetDurationMs": 1_920,
                            "delivery": "neutral",
                            "emphasis": [],
                            "pauses": [],
                            "style": null,
                            "semanticAnchors": []
                        }]
                    })
                    .to_string(),
                    crate::metrics::GeminiRequestKind::Revision => serde_json::json!({
                        "plans": [{
                            "index": 1,
                            "displayText": "Đây là một lời giải thích.",
                            "spokenText": "Lời giải thích.",
                            "targetDurationMs": 1_920,
                            "delivery": "neutral",
                            "emphasis": [],
                            "pauses": [],
                            "style": null,
                            "semanticAnchors": []
                        }]
                    })
                    .to_string(),
                    crate::metrics::GeminiRequestKind::Translation => {
                        bail!("translation was not expected in the source-aligned fixture")
                    }
                })
            }));
        let _azure = crate::azure_timing::install_test_timing_responder(Box::new(|ssml| {
            let mut events = vec![crate::azure_timing::TimingEvent {
                kind: crate::azure_timing::TimingEventKind::WordBoundary,
                offset_ms: 1,
                duration_ms: 1,
                mark: None,
                text: Some("fake".to_owned()),
            }];
            let mut remainder = ssml;
            // The base deliberately overruns its one-part slot; the concise revision moves the
            // display-boundary bookmark back into the source window so this tests the branch
            // ceiling without leaving a soft-anchor conflict in the accepted candidate.
            let bookmark_offset = if ssml.contains("Lời giải thích.") {
                100
            } else {
                3_500
            };
            while let Some(start) = remainder.find("<bookmark mark=\"") {
                let value = &remainder[start + "<bookmark mark=\"".len()..];
                let Some(end) = value.find("\"/>") else {
                    break;
                };
                events.push(crate::azure_timing::TimingEvent {
                    kind: crate::azure_timing::TimingEventKind::Bookmark,
                    offset_ms: bookmark_offset + events.len() as u64 * 100,
                    duration_ms: 1,
                    mark: Some(value[..end].to_owned()),
                    text: None,
                });
                remainder = &value[end + 3..];
            }
            Ok(crate::azure_timing::TimedAudio {
                audio: vec![0; 44],
                events,
            })
        }));
        let _rest = crate::tts_azure::install_test_rest_responder(Box::new(|_| Ok(vec![0; 44])));

        crate::metrics::reset();
        let source_timeline =
            crate::sync_timeline::SourceTimeline::from_files(&input_srt, &input_srt)?;
        let segments = crate::srt::parse_srt(&input_srt)?;
        let layout = crate::sync_timeline::build_sync_layout(&source_timeline, &segments)?;
        let voice = AzureVoice::from_short_name("vi-VN-NamMinhNeural".to_owned(), Vec::new());
        assert_eq!(voice.capabilities.timing_events, CapabilityState::Supported);
        let plans = crate::translate_gemini::create_speech_plans(
            &["fake-gemini".to_owned()],
            &input_srt,
            false,
            NaturalMode::Balanced,
            &[],
            Some(&source_timeline),
            None,
        )
        .await?;
        let azure = crate::config::AzureConfig::new("fake-azure", "southeastasia")?
            .with_voice("vi-VN-NamMinhNeural")?;
        let aligned_manifest = directory.join("aligned-render-manifest.json");
        let aligned_output_dir = directory.join("aligned-tts");
        match select_natural_timing_pipeline(
            CapabilityState::Supported,
            NaturalTimingPipeline::SourceAligned,
        ) {
            NaturalTimingPipeline::SourceAligned => {
                crate::tts_azure::generate_all_tts_for_aligned_utterances(
                    &azure,
                    &ffmpeg,
                    &input_srt,
                    &plans.plans,
                    NaturalMode::Balanced,
                    &source_timeline,
                    &layout,
                    &["fake-gemini".to_owned()],
                    &voice,
                    &aligned_output_dir,
                    &aligned_manifest,
                    false,
                    "fake-provider-budget",
                    &crate::progress::NoopProgress,
                )
                .await?;
            }
            NaturalTimingPipeline::UnanchoredContinuous => unreachable!(),
            NaturalTimingPipeline::SegmentedRollback => unreachable!(),
        }
        let aligned_metrics = crate::metrics::snapshot();
        assert_eq!(aligned_metrics["geminiPlanningRequests"], 1);
        assert_eq!(aligned_metrics["geminiRevisionRequests"], 1);
        assert_eq!(aligned_metrics["azureLogicalGenerations"], 2);
        assert_eq!(aligned_metrics["azureSynthesisAttempts"], 2);

        let rollback_manifest = directory.join("rollback-render-manifest.json");
        let rollback_output_dir = directory.join("rollback-tts");
        match select_natural_timing_pipeline(
            CapabilityState::Supported,
            NaturalTimingPipeline::SegmentedRollback,
        ) {
            NaturalTimingPipeline::SourceAligned => unreachable!(),
            NaturalTimingPipeline::UnanchoredContinuous => unreachable!(),
            NaturalTimingPipeline::SegmentedRollback => {
                crate::tts_azure::generate_all_tts_for_speech_plans(
                    &azure,
                    &ffmpeg,
                    &input_srt,
                    &plans.plans,
                    NaturalMode::Balanced,
                    &source_timeline,
                    &layout,
                    &["fake-gemini".to_owned()],
                    &voice,
                    &rollback_output_dir,
                    &rollback_manifest,
                    false,
                    "fake-provider-budget",
                    &crate::progress::NoopProgress,
                )
                .await?;
            }
        }

        let metrics = crate::metrics::snapshot();
        assert_eq!(metrics["geminiPlanningRequests"], 1);
        assert_eq!(metrics["geminiRevisionRequests"], 1);
        assert_eq!(metrics["azureLogicalGenerations"], 3);
        assert_eq!(metrics["azureSynthesisAttempts"], 3);
        let aligned: RenderManifest = serde_json::from_slice(&fs::read(&aligned_manifest)?)?;
        assert_eq!(aligned.status, ManifestStatus::Aligned);
        assert_eq!(aligned.entries.len(), 1);
        assert!(aligned.entries[0].path.is_file());
        let rollback: RenderManifest = serde_json::from_slice(&fs::read(&rollback_manifest)?)?;
        assert_eq!(rollback.status, ManifestStatus::UnanchoredContinuous);
        assert_eq!(rollback.entries.len(), 1);
        assert!(!rollback.semantic_alignment_supported);
        assert!(rollback.entries[0].path.is_file());

        let hd_manifest = directory.join("hd-render-manifest.json");
        let hd_output_dir = directory.join("hd-tts");
        let hd_voice = AzureVoice::from_short_name(
            "de-DE-Florian:DragonHDLatestNeural".to_owned(),
            Vec::new(),
        );
        let hd_azure = crate::config::AzureConfig::new("fake-azure", "southeastasia")?
            .with_voice("de-DE-Florian:DragonHDLatestNeural")?;
        crate::tts_azure::generate_all_tts_for_unanchored_utterances(
            &hd_azure,
            &ffmpeg,
            &input_srt,
            &plans.plans,
            NaturalMode::Balanced,
            &source_timeline,
            &layout,
            &["fake-gemini".to_owned()],
            &hd_voice,
            &hd_output_dir,
            &hd_manifest,
            false,
            "fake-provider-hd",
            &crate::progress::NoopProgress,
        )
        .await?;
        let hd: RenderManifest = serde_json::from_slice(&fs::read(&hd_manifest)?)?;
        assert_eq!(hd.status, ManifestStatus::UnanchoredContinuous);
        assert_eq!(hd.timing_pipeline, TimingPipelineId::UnanchoredContinuous);
        assert_eq!(hd.entries.len(), 1);
        assert_eq!(hd.entries[0].source_segments, vec![1]);
        assert!(!hd.semantic_alignment_supported);
        assert!(hd.entries[0].path.is_file());
        let metrics = crate::metrics::snapshot();
        assert_eq!(metrics["azureLogicalGenerations"], 4);
        assert_eq!(metrics["azureSynthesisAttempts"], 4);

        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn skip_requires_a_fresh_companion_srt() -> Result<()> {
        let directory = test_directory("skip-pair");
        fs::create_dir_all(&directory)?;
        let video = directory.join("lesson.mp4");
        let srt = directory.join("lesson.srt");
        let output = directory.join("lesson.vi-dub.mp4");
        let output_srt = directory.join("lesson.vi-dub.srt");
        fs::write(&video, b"video")?;
        fs::write(&srt, b"source subtitles")?;
        fs::write(&output, b"dubbed video")?;

        assert!(!output_pair_is_up_to_date(
            &output,
            &output_srt,
            &video,
            &srt
        )?);
        fs::write(&output_srt, b"translated subtitles")?;
        assert!(output_pair_is_up_to_date(
            &output,
            &output_srt,
            &video,
            &srt
        )?);

        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn pair_publish_restores_the_previous_video_when_srt_publish_fails() -> Result<()> {
        let directory = test_directory("publish-pair");
        fs::create_dir_all(&directory)?;
        let staged_video = directory.join("staged.mp4");
        let staged_srt = directory.join("staged.srt");
        let output_video = directory.join("lesson.vi-dub.mp4");
        let blocked_parent = directory.join("blocked");
        let output_srt = blocked_parent.join("lesson.vi-dub.srt");
        fs::write(&staged_video, b"new video")?;
        fs::write(&staged_srt, b"new subtitles")?;
        fs::write(&output_video, b"old video")?;
        fs::write(&blocked_parent, b"not a directory")?;

        assert!(
            replace_output_pair(&staged_video, &staged_srt, &output_video, &output_srt).is_err()
        );
        assert_eq!(fs::read(&output_video)?, b"old video");
        assert!(!output_srt.exists());

        fs::remove_dir_all(directory)?;
        Ok(())
    }

    fn test_directory(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("dubora-orchestrator-{name}-{unique}"))
    }

    #[test]
    fn natural_voice_keeps_display_srt_timestamps_unchanged() {
        let source = vec![SrtSegment {
            index: 7,
            start: "00:00:01,120".to_owned(),
            end: "00:00:03,450".to_owned(),
            text: "source text".to_owned(),
        }];
        let document = SpeechPlanDocument::new(vec![crate::speech_plan::SpeechPlan::plain(
            7,
            "spoken text".to_owned(),
            2_330,
        )]);
        let displayed = display_segments_from_plans(&source, &document).unwrap();
        assert_eq!(displayed[0].start, source[0].start);
        assert_eq!(displayed[0].end, source[0].end);
        assert_eq!(displayed[0].text, "spoken text");
    }

    #[test]
    fn review_diagnostics_expose_timestamp_for_segment_reason() {
        let manifest = RenderManifest::new(
            ManifestStatus::ReviewRequired,
            TimingPipelineId::SegmentedRollback,
            vec![ScheduledAudio {
                id: "segment-26".to_owned(),
                path: PathBuf::from("segment-26.mp3"),
                start_ms: 124_000,
                end_ms: 128_799,
                source_segments: vec![26],
                measured_audio_ms: 4_799,
                speech_end_ms: None,
                rendered_audio_ms: 4_799,
                tempo: 1.0,
                island_id: Some("island-fallback-26".to_owned()),
                cache_identity: "cache".to_owned(),
                alignment: crate::utterance::EntryAlignment::default(),
            }],
        )
        .with_semantic_alignment_support(false)
        .with_diagnostics(crate::utterance::ManifestDiagnostics {
            prediction_errors_ms: vec![-120, 240],
            review_reasons: vec!["segment-26:fallback".to_owned()],
            ..crate::utterance::ManifestDiagnostics::default()
        })
        .review_required();
        let diagnostics = manifest_diagnostics(&manifest);
        assert_eq!(diagnostics["reviewWindows"][0]["startMs"], 124_000);
        assert!(format_review_windows(&manifest).contains("02:04.000-02:08.799"));
        assert!(!review_reason_matches_entry(
            "segment-2",
            &manifest.entries[0]
        ));
        assert!(!review_reason_matches_entry(
            "island-6",
            &manifest.entries[0]
        ));
        assert!(review_reason_matches_entry(
            "island-fallback-26",
            &manifest.entries[0]
        ));
        assert_eq!(diagnostics["predictionErrorP95Ms"], 240.0);
        assert_eq!(diagnostics["predictionErrorMaxMs"], 240.0);
    }

    #[test]
    fn review_diagnostics_collapse_an_island_to_one_window() {
        let entries = [1_u32, 2]
            .into_iter()
            .map(|position| ScheduledAudio {
                id: format!("utterance-{position}"),
                path: PathBuf::from(format!("utterance-{position}.mp3")),
                start_ms: position * 1_000,
                end_ms: position * 1_000 + 900,
                source_segments: vec![position as usize],
                measured_audio_ms: 900,
                speech_end_ms: None,
                rendered_audio_ms: 900,
                tempo: 1.0,
                island_id: Some("island-1".to_owned()),
                cache_identity: "cache".to_owned(),
                alignment: crate::utterance::EntryAlignment::default(),
            })
            .collect();
        let manifest = RenderManifest::new(
            ManifestStatus::ReviewRequired,
            TimingPipelineId::SegmentedRollback,
            entries,
        )
        .with_diagnostics(crate::utterance::ManifestDiagnostics {
            review_reasons: vec!["island-1:infeasible".to_owned()],
            ..crate::utterance::ManifestDiagnostics::default()
        })
        .review_required();

        let windows = review_windows(&manifest);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].entry_id, "island-1");
        assert_eq!((windows[0].start_ms, windows[0].end_ms), (1_000, 2_900));
    }
}
