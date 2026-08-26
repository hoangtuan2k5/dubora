use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::sleep;

use crate::azure_timing::{TimingEvent, TimingEventKind, synthesize_ssml_with_timing};
use crate::azure_voices::{AzureVoice, CapabilityState, VoiceModelFamily};
use crate::config::AzureConfig;
#[cfg(test)]
use crate::duration_solver::SEGMENT_HEADROOM_MS;
use crate::duration_solver::{
    DurationDecision, DurationTarget, MAX_TIME_STRETCH_TEMPO, MAX_TIME_STRETCH_TOLERANCE_MS,
    solve_duration,
};
use crate::ffmpeg::FfmpegPaths;
use crate::metrics::{
    record_azure_cache_hit, record_azure_logical_generation, record_gemini_cache_hit,
};
use crate::progress::{ProgressEvent, ProgressReporter, Stage};
use crate::speech_plan::{EmphasisLevel, NaturalMode, SpeechPlan, unique_phrase_position};
#[cfg(test)]
use crate::srt::time_to_seconds;
use crate::srt::{SrtSegment, parse_srt};
use crate::sync_timeline::{
    AllocatedBlock, Coupling, MAX_ANCHOR_DRIFT_MS, MAX_SOFT_BOUNDARY_DRIFT_MS, MeasuredAnchor,
    SOURCE_TIMELINE_VERSION, SourceTimeline, SyncLayout, allocate_blocks_best_effort,
    allocate_measured_group, allocate_measured_group_with_anchors, allocate_segments,
    allocation_groups, apply_measured_spillback, apply_measured_spillback_except,
    cover_hard_anchor_windows, group_exceeds_tempo_cap,
};
use crate::translate_gemini::{
    SpeechPlanRevisionRequest, revise_speech_plan, revise_speech_plans_batch, revision_word_budget,
};
use crate::tts_cache::{is_fresh_audio_file, tts_cache_key, write_tts_cache_key};
use crate::utterance::{
    AlignmentResult, EntryAlignment, EntryAlignmentStatus, ManifestDiagnostics, ManifestStatus,
    RenderManifest, RenderedAnchor, ScheduledAudio, TimingPipelineId, Utterance, bookmark_name,
    group_source_aligned_utterances, semantic_bookmark_name, validate_bookmark_structure,
    validate_semantic_alignment,
};

const CONCURRENCY: usize = 4;
const RETRIES: usize = 3;
const SPEECH_PLAN_CACHE_PROVIDER: &str = "azure-speech-plan-v2";
const MAX_SPEECH_PLAN_REVISIONS: u8 = 1;
const UTTERANCE_CACHE_PROVIDER: &str = "azure-natural-utterance-v3";
const HD_UTTERANCE_CACHE_PROVIDER: &str = "azure-hd-continuous-v1";
// Keep one concise-revision pass with bounded payloads. Each block appears in at most one
// revision request; 20 preserves the lower request count without the instability seen in smaller
// stochastic batches.
const CONCISE_REVISION_BATCH_SIZE: usize = 20;
const DURATION_CALIBRATION_VERSION: u8 = 1;
const MIN_DURATION_CALIBRATION_SAMPLES: usize = 5;
const MAX_DURATION_CALIBRATION_SAMPLES: usize = 64;
const MAX_DURATION_RATE_REPAIR_PERCENT: i8 = 20;

#[cfg(test)]
type TestRestResponder = Box<dyn Fn(&str) -> Result<Vec<u8>> + Send + Sync>;

#[cfg(test)]
static TEST_REST_RESPONDER: OnceLock<std::sync::Mutex<Option<TestRestResponder>>> = OnceLock::new();

#[cfg(test)]
pub(crate) struct TestRestResponderGuard;

#[cfg(test)]
pub(crate) fn install_test_rest_responder(responder: TestRestResponder) -> TestRestResponderGuard {
    *TEST_REST_RESPONDER
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .expect("test Azure REST responder lock poisoned") = Some(responder);
    TestRestResponderGuard
}

#[cfg(test)]
impl Drop for TestRestResponderGuard {
    fn drop(&mut self) {
        if let Some(responder) = TEST_REST_RESPONDER.get() {
            *responder
                .lock()
                .expect("test Azure REST responder lock poisoned") = None;
        }
    }
}

fn semantic_anchor_timing_available(timeline: &SourceTimeline) -> bool {
    timeline.mode != crate::sync_timeline::TimingMode::CueOnly
        || timeline.has_projected_anchor_timing()
}

#[derive(Clone)]
struct PendingUtterance {
    utterance: Utterance,
    path: PathBuf,
    measured_audio_ms: u32,
    speech_end_ms: Option<u32>,
    events: Vec<TimingEvent>,
    semantic_expected: Vec<(String, u32, i64)>,
    structural_issue: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AlignedFollowUp {
    Accept,
    Concise,
    DurationRepair,
    Repair,
    Review,
}

fn aligned_follow_up(
    structural_issue: bool,
    duration_feasible: bool,
    technically_fit: bool,
    semantic_repair_feasible: bool,
    semantic_aligned: bool,
    duration_rate_repair: bool,
) -> AlignedFollowUp {
    if structural_issue {
        AlignedFollowUp::Review
    } else if !semantic_aligned && technically_fit && semantic_repair_feasible {
        AlignedFollowUp::Repair
    } else if !duration_feasible && duration_rate_repair && semantic_aligned {
        AlignedFollowUp::DurationRepair
    } else if !duration_feasible {
        AlignedFollowUp::Concise
    } else if !semantic_aligned && semantic_repair_feasible {
        AlignedFollowUp::Repair
    } else if semantic_aligned {
        AlignedFollowUp::Accept
    } else {
        AlignedFollowUp::Review
    }
}

fn is_provisional_review_reason(reason: &str) -> bool {
    [
        ":measured_allocation_infeasible:",
        ":measured_anchor_allocation_infeasible:",
        ":concise_revision_unresolved",
        ":duration_repair_unresolved",
        ":semantic_repair_unavailable",
        ":semantic_repair_unresolved",
    ]
    .iter()
    .any(|marker| reason.contains(marker))
}

#[derive(Clone)]
struct SemanticEvidence {
    events: Vec<TimingEvent>,
    expected: Vec<(String, u32, i64)>,
}

fn rendered_anchor_evidence(
    events: &[TimingEvent],
    expected: &[(String, u32, i64)],
    render_start_ms: u32,
    tempo: f64,
) -> (AlignmentResult, Vec<RenderedAnchor>) {
    let alignment = validate_semantic_alignment(events, expected, render_start_ms, tempo);
    let anchors = expected
        .iter()
        .filter_map(|(name, source_ms, tolerance_ms)| {
            let mut matches = events.iter().filter(|event| {
                event.kind == TimingEventKind::Bookmark && event.mark.as_deref() == Some(name)
            });
            let event = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            let raw_offset_ms = u32::try_from(event.offset_ms).ok()?;
            let rendered_ms = u32::try_from(
                u64::from(render_start_ms)
                    .saturating_add((f64::from(raw_offset_ms) / tempo.max(1.0)).round() as u64),
            )
            .unwrap_or(u32::MAX);
            Some(RenderedAnchor {
                id: name
                    .split_once("-a")
                    .map_or_else(|| name.clone(), |(_, id)| id.to_owned()),
                bookmark: name.clone(),
                source_ms: *source_ms,
                raw_offset_ms,
                rendered_ms,
                drift_ms: i64::from(rendered_ms) - i64::from(*source_ms),
                tolerance_ms: *tolerance_ms,
                coupling: Coupling::Anchored,
            })
        })
        .collect();
    (alignment, anchors)
}

fn measured_anchors(
    candidates: &[PendingUtterance],
    timeline: &SourceTimeline,
) -> HashMap<String, Vec<MeasuredAnchor>> {
    candidates
        .iter()
        .filter(|candidate| candidate.structural_issue.is_none())
        .filter_map(|candidate| {
            let mut anchors = candidate
                .semantic_expected
                .iter()
                .filter_map(|(mark, source_ms, tolerance_ms)| {
                    let event = candidate.events.iter().find(|event| {
                        event.kind == TimingEventKind::Bookmark
                            && event.mark.as_deref() == Some(mark)
                    })?;
                    let anchor_id = mark.split_once("-a")?.1;
                    let phrase = timeline
                        .phrases
                        .iter()
                        .find(|phrase| phrase.id == anchor_id)?;
                    Some(MeasuredAnchor {
                        source_ms: *source_ms,
                        raw_offset_ms: u32::try_from(event.offset_ms).ok()?,
                        tolerance_ms: u32::try_from(*tolerance_ms).ok()?,
                        confidence: phrase.confidence,
                        coupling: Coupling::Anchored,
                    })
                })
                .collect::<Vec<_>>();
            // Display-part bookmarks are soft evidence: they keep a translated block near the
            // source cue boundary, but their lower confidence and wider window must never make a
            // cue-only timeline claim semantic alignment.
            for part in &candidate.utterance.parts {
                let phrase = timeline.phrases.iter().find(|phrase| {
                    phrase.owner_index == part.index && phrase.coupling == Coupling::Soft
                });
                let Some(phrase) = phrase else { continue };
                let mark = bookmark_name(candidate.utterance.id, part.index);
                let Some(event) = candidate.events.iter().find(|event| {
                    event.kind == TimingEventKind::Bookmark
                        && event.mark.as_deref() == Some(mark.as_str())
                }) else {
                    continue;
                };
                let Ok(raw_offset_ms) = u32::try_from(event.offset_ms) else {
                    continue;
                };
                anchors.push(MeasuredAnchor {
                    source_ms: phrase.preferred_start_ms,
                    raw_offset_ms,
                    tolerance_ms: MAX_SOFT_BOUNDARY_DRIFT_MS as u32,
                    confidence: phrase.confidence,
                    coupling: Coupling::Soft,
                });
            }
            anchors.sort_by_key(|anchor| (anchor.source_ms, anchor.raw_offset_ms));
            (!anchors.is_empty()).then(|| (candidate.utterance.block_id.clone(), anchors))
        })
        .collect()
}

fn semantic_expectations(
    utterance: &Utterance,
    timeline: &SourceTimeline,
) -> Vec<(String, u32, i64)> {
    utterance
        .parts
        .iter()
        .flat_map(|part| {
            part.plan.semantic_anchors.iter().filter_map(|anchor| {
                if !timeline.has_semantic_anchor_timing(&anchor.anchor_id) {
                    return None;
                }
                timeline
                    .phrases
                    .iter()
                    .find(|phrase| phrase.id == anchor.anchor_id)
                    .map(|phrase| {
                        (
                            semantic_bookmark_name(utterance.id, &anchor.anchor_id),
                            phrase.preferred_start_ms,
                            MAX_ANCHOR_DRIFT_MS,
                        )
                    })
            })
        })
        .collect()
}

fn measured_part_durations(candidate: &PendingUtterance) -> HashMap<usize, u32> {
    let mut previous = 0_u32;
    candidate
        .utterance
        .parts
        .iter()
        .enumerate()
        .filter_map(|(position, part)| {
            let mark = bookmark_name(candidate.utterance.id, part.index);
            let offset = candidate.events.iter().find_map(|event| {
                (event.kind == TimingEventKind::Bookmark
                    && event.mark.as_deref() == Some(mark.as_str()))
                .then(|| u32::try_from(event.offset_ms).ok())
                .flatten()
            })?;
            // Azure's final subtitle bookmark marks the end of the last planned part, not
            // necessarily the end of the audio payload. Keep the trailing tail with that part;
            // otherwise a whole utterance can look feasible during revision preflight and only
            // become over-cap after the final manifest measures the complete file.
            let duration = if position + 1 == candidate.utterance.parts.len() {
                candidate
                    .measured_audio_ms
                    .saturating_sub(previous)
                    .max(offset.saturating_sub(previous))
            } else {
                offset.saturating_sub(previous)
            }
            .max(1);
            previous = offset;
            Some((part.index, duration))
        })
        .collect()
}

async fn synthesize_revised_utterance(
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    voice: &AzureVoice,
    utterance: Utterance,
    timeline: &SourceTimeline,
    output: &Path,
    rate_percent: i8,
    semantic_delays: &[(String, u16)],
    pause_fit: bool,
) -> Result<PendingUtterance> {
    let prepared = utterance
        .parts
        .iter()
        .map(|part| prepared_speech_plan(&part.plan))
        .collect::<Vec<_>>();
    let bookmarks = utterance
        .parts
        .iter()
        .map(|part| bookmark_name(utterance.id, part.index))
        .collect::<Vec<_>>();
    let semantic_bookmarks = utterance
        .parts
        .iter()
        .zip(&prepared)
        .flat_map(|(part, prepared)| {
            prepared.semantic_anchors.iter().filter_map(|anchor| {
                timeline
                    .has_semantic_anchor_timing(&anchor.anchor_id)
                    .then(|| {
                        (
                            part.index,
                            anchor.spoken_phrase.clone(),
                            semantic_bookmark_name(utterance.id, &anchor.anchor_id),
                        )
                    })
            })
        })
        .collect::<Vec<_>>();
    let timing_bookmarks = bookmarks
        .iter()
        .cloned()
        .chain(semantic_bookmarks.iter().map(|(_, _, mark)| mark.clone()))
        .collect::<Vec<_>>();
    let ssml = render_utterance_ssml_with_semantic_anchors_and_delays(
        voice,
        &prepared,
        &bookmarks,
        &semantic_bookmarks,
        semantic_delays,
        rate_percent,
    )?;
    let (events, measured_audio_ms, audio_path, speech_end_ms) = generate_timed_utterance(
        config,
        ffmpeg,
        output,
        &ssml,
        &voice.short_name,
        &prepared,
        &timing_bookmarks,
        pause_fit,
    )
    .await?;
    let structure = validate_bookmark_structure(&utterance, &events);
    if !structure.accepted {
        bail!(
            "Revised utterance {} has invalid timing events: {}",
            utterance.id,
            structure.reason.as_deref().unwrap_or("unknown")
        );
    }
    let semantic_expected = semantic_expectations(&utterance, timeline);
    if !semantic_bookmarks_are_complete(&events, &semantic_expected) {
        bail!(
            "Revised utterance {} omitted a required semantic bookmark",
            utterance.id
        );
    }
    Ok(PendingUtterance {
        utterance,
        path: audio_path,
        measured_audio_ms,
        speech_end_ms,
        events,
        semantic_expected,
        structural_issue: None,
    })
}

fn pending_entry(
    candidate: PendingUtterance,
    allocation: &AllocatedBlock,
) -> (ScheduledAudio, Option<(String, SemanticEvidence)>) {
    let entry_id = format!("utterance-{}", candidate.utterance.id);
    let slot_ms = allocation
        .slot_end_ms
        .saturating_sub(allocation.render_start_ms)
        .max(1);
    let (tempo, rendered_audio_ms) = fit_tempo_to_slot(candidate.measured_audio_ms, slot_ms);
    let evidence = (!candidate.semantic_expected.is_empty()).then(|| {
        (
            entry_id.clone(),
            SemanticEvidence {
                events: candidate.events.clone(),
                expected: candidate.semantic_expected.clone(),
            },
        )
    });
    (
        ScheduledAudio {
            id: entry_id,
            path: candidate.path,
            start_ms: allocation.render_start_ms,
            end_ms: allocation.slot_end_ms,
            source_segments: candidate
                .utterance
                .parts
                .iter()
                .map(|part| part.index)
                .collect(),
            measured_audio_ms: candidate.measured_audio_ms,
            speech_end_ms: candidate.speech_end_ms,
            rendered_audio_ms,
            tempo,
            island_id: Some(candidate.utterance.island_id),
            cache_identity: String::new(),
            alignment: EntryAlignment::default(),
        },
        evidence,
    )
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct DurationCalibration {
    version: u8,
    voice: String,
    style: String,
    // Kept for calibration-file compatibility. Loading intentionally keys only by voice and style.
    renderer: String,
    chars_per_second: Vec<f64>,
}

impl DurationCalibration {
    fn load(output_dir: &Path, voice: &AzureVoice) -> Self {
        let path = output_dir.join(".duration-calibration-v1.json");
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .filter(|calibration| {
                calibration.version == DURATION_CALIBRATION_VERSION
                    && calibration.voice == voice.short_name
                    && calibration.style == voice.styles.join("|")
            })
            .unwrap_or_else(|| Self {
                version: DURATION_CALIBRATION_VERSION,
                voice: voice.short_name.clone(),
                style: voice.styles.join("|"),
                renderer: UTTERANCE_CACHE_PROVIDER.to_owned(),
                chars_per_second: Vec::new(),
            })
    }

    fn median_chars_per_second(&self) -> Option<f64> {
        if self.chars_per_second.len() < MIN_DURATION_CALIBRATION_SAMPLES {
            return None;
        }
        let mut values = self.chars_per_second.clone();
        values.sort_by(f64::total_cmp);
        values.get(values.len() / 2).copied()
    }

    fn observe(&mut self, plan: &SpeechPlan, measured_audio_ms: u32) {
        self.observe_plans(std::slice::from_ref(plan), measured_audio_ms);
    }

    fn observe_plans(&mut self, plans: &[SpeechPlan], measured_audio_ms: u32) {
        let prepared = plans.iter().map(prepared_speech_plan).collect::<Vec<_>>();
        let chars = prepared
            .iter()
            .map(|plan| {
                plan.spoken_text
                    .chars()
                    .filter(|character| !character.is_whitespace())
                    .count()
            })
            .sum::<usize>();
        let pauses_ms = prepared
            .iter()
            .flat_map(|plan| plan.pauses.iter())
            .map(|pause| u32::from(pause.duration_ms))
            .sum::<u32>();
        let spoken_ms = measured_audio_ms.saturating_sub(pauses_ms);
        if chars < 4 || spoken_ms < 100 {
            return;
        }
        let chars_per_second = chars as f64 * 1_000.0 / f64::from(spoken_ms);
        if !(2.0..=30.0).contains(&chars_per_second) {
            return;
        }
        self.chars_per_second.push(chars_per_second);
        if self.chars_per_second.len() > MAX_DURATION_CALIBRATION_SAMPLES {
            let excess = self.chars_per_second.len() - MAX_DURATION_CALIBRATION_SAMPLES;
            self.chars_per_second.drain(..excess);
        }
    }

    fn write(&self, output_dir: &Path) -> Result<()> {
        if self.chars_per_second.is_empty() {
            return Ok(());
        }
        fs::write(
            output_dir.join(".duration-calibration-v1.json"),
            serde_json::to_vec_pretty(self)?,
        )?;
        Ok(())
    }
}

pub(crate) fn load_duration_calibration_chars_per_second(
    output_dir: &Path,
    voice: &AzureVoice,
) -> Option<f64> {
    DurationCalibration::load(output_dir, voice).median_chars_per_second()
}

#[cfg(test)]
fn predict_plan_audio_ms(plan: &SpeechPlan, voice: &AzureVoice) -> u32 {
    predict_plan_audio_ms_with_calibration(plan, voice, None)
}

fn predict_plan_audio_ms_with_calibration(
    plan: &SpeechPlan,
    _voice: &AzureVoice,
    calibrated_chars_per_second: Option<f64>,
) -> u32 {
    let prepared = prepared_speech_plan(plan);
    let chars = prepared
        .spoken_text
        .chars()
        .filter(|character| !character.is_whitespace())
        .count()
        .max(1) as f64;
    let cold_start_cps = if plan.semantic_anchors.is_empty() {
        11.0
    } else {
        13.0
    };
    let spoken_ms = calibrated_chars_per_second
        .filter(|cps| cps.is_finite() && *cps > 0.0)
        .unwrap_or(cold_start_cps)
        .recip()
        .mul_add(chars * 1_000.0, 0.0)
        .ceil() as u64;
    let explicit_pause_ms = prepared
        .pauses
        .iter()
        .map(|pause| u64::from(pause.duration_ms))
        .sum::<u64>();
    // A small cold-start margin is deliberately conservative. Actual Azure measurements remain
    // the only acceptance oracle and replace this estimate in the second allocation pass.
    u32::try_from(
        (spoken_ms + explicit_pause_ms)
            .saturating_mul(105)
            .div_ceil(100),
    )
    .unwrap_or(u32::MAX)
    .max(100)
}

pub async fn generate_all_tts(
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    input_srt: &Path,
    output_dir: &Path,
    video: &str,
    progress: &dyn ProgressReporter,
) -> Result<()> {
    let segments = parse_srt(input_srt)?;
    progress.event(ProgressEvent::new(
        Stage::Tts,
        video,
        format!("Loaded {} segments for TTS generation.", segments.len()),
    ));

    fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create {}", output_dir.display()))?;
    let missing: Vec<_> = segments
        .into_iter()
        .filter(|segment| {
            let output = segment_path(output_dir, segment.index);
            let text = prepare_tts_text(&segment.text);
            let key = tts_cache_key("azure", &config.voice, &config.style, &text);
            !is_fresh_audio_file(ffmpeg, &output, &key)
        })
        .collect();

    progress.event(ProgressEvent::new(
        Stage::Tts,
        video,
        format!(
            "Need to generate {} missing or invalid segments.",
            missing.len()
        ),
    ));
    if missing.is_empty() {
        return Ok(());
    }

    let total = missing.len();
    let semaphore = Arc::new(Semaphore::new(CONCURRENCY));
    let client = Client::new();
    let mut tasks = JoinSet::new();

    for segment in missing {
        let semaphore = Arc::clone(&semaphore);
        let client = client.clone();
        let config = config.clone();
        let ffmpeg = ffmpeg.clone();
        let output_dir = output_dir.to_owned();
        tasks.spawn(async move {
            let _permit = semaphore.acquire_owned().await?;
            sleep(Duration::from_millis(200)).await;
            generate_segment(&client, &config, &ffmpeg, &output_dir, segment).await
        });
    }

    let mut completed = 0;
    while let Some(result) = tasks.join_next().await {
        let result = result.context("TTS worker task failed")??;
        completed += 1;
        for message in result.attempt_errors {
            progress.event(ProgressEvent::new(Stage::Tts, video, message));
        }
        let message = if result.fallback {
            format!(
                "Failed to generate segment {}. Created a silent MP3 fallback.",
                result.index
            )
        } else {
            format!(
                "Generated {completed}/{total} with Azure ({}): segment {}",
                config.voice, result.index
            )
        };
        progress.event(ProgressEvent::new(Stage::Tts, video, message).progress(completed, total));
    }

    Ok(())
}

/// Generate capability-aware Natural Voice segments and allow one timing retry per segment.
pub async fn generate_all_tts_for_speech_plans(
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    input_srt: &Path,
    plans: &[SpeechPlan],
    mode: NaturalMode,
    timeline: &SourceTimeline,
    layout: &SyncLayout,
    gemini_keys: &[String],
    voice: &AzureVoice,
    output_dir: &Path,
    manifest_path: &Path,
    pause_fit: bool,
    video: &str,
    progress: &dyn ProgressReporter,
) -> Result<()> {
    let segments = parse_srt(input_srt)?;
    let plans: HashMap<_, _> = plans
        .iter()
        .map(|plan| (plan.index, plan.clone()))
        .collect();
    if segments.len() != plans.len()
        || segments
            .iter()
            .any(|segment| !plans.contains_key(&segment.index))
    {
        bail!("Natural Voice plans do not match the subtitle segments");
    }
    fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create {}", output_dir.display()))?;
    let mut duration_calibration = DurationCalibration::load(output_dir, voice);
    let calibrated_chars_per_second = duration_calibration.median_chars_per_second();
    let predicted = segments
        .iter()
        .map(|segment| {
            (
                segment.index,
                plans.get(&segment.index).map_or(100, |plan| {
                    predict_plan_audio_ms_with_calibration(plan, voice, calibrated_chars_per_second)
                }),
            )
        })
        .collect::<HashMap<_, _>>();
    let allocations = allocate_segments(timeline, layout, &segments, &predicted, mode)
        .map_err(|error| anyhow!("review_required: source timing allocation: {error:#}"))?
        .into_iter()
        .map(|allocation| (allocation.segment_index, allocation))
        .collect::<HashMap<_, _>>();

    progress.event(ProgressEvent::new(
        Stage::Tts,
        video,
        format!("Generating {} Natural Voice segments.", segments.len()),
    ));
    let client = Client::new();
    let mut generated_durations = HashMap::new();
    let mut generated_paths = HashMap::new();
    let mut generated_speech_ends = HashMap::new();
    let mut calibrated_plans = Vec::new();
    let mut review_required = false;

    for (position, segment) in segments.iter().enumerate() {
        if progress.is_cancelled() {
            bail!("Dub job cancelled");
        }
        let mut plan = plans
            .get(&segment.index)
            .expect("plans were matched above")
            .clone();
        let allocation = allocations
            .get(&segment.index)
            .with_context(|| format!("Missing source allocation for segment {}", segment.index))?;
        let target = DurationTarget {
            slot_ms: allocation
                .slot_end_ms
                .saturating_sub(allocation.render_start_ms)
                .max(100),
            following_silence_ms: 0,
            spillback_ms: 0,
            mode,
        };
        let mut speech_plan_revisions = 0;
        let mut result = generate_plan_segment(
            &client, config, ffmpeg, output_dir, &plan, voice, 0, pause_fit,
        )
        .await?;
        if result.fallback {
            generated_durations.insert(plan.index, result.measured_audio_ms);
            generated_paths.insert(plan.index, result.path.clone());
            generated_speech_ends.insert(plan.index, result.speech_end_ms);
            progress.event(ProgressEvent::new(
                Stage::Tts,
                video,
                format!("Natural Voice segment {} fell back to silence.", plan.index),
            ));
            continue;
        }

        let output = segment_path(output_dir, plan.index);
        let actual_duration_ms = result.measured_audio_ms;
        let decision = solve_duration(target, actual_duration_ms, false)?;
        match decision {
            DurationDecision::Accept => {}
            DurationDecision::RegenerateWithProsody { rate_percent }
                if voice.capabilities.manual_prosody == CapabilityState::Supported =>
            {
                result = generate_plan_segment(
                    &client,
                    config,
                    ffmpeg,
                    output_dir,
                    &plan,
                    voice,
                    rate_percent,
                    pause_fit,
                )
                .await?;
                if !result.fallback {
                    // The regenerated raw file has a new cache identity; refresh its sibling.
                    let stats = fit_audio_file(ffmpeg, &output, None, pause_fit)?;
                    result.path = stats.path;
                    result.measured_audio_ms = stats.measured_audio_ms;
                    result.speech_end_ms = stats.speech_end_ms;
                }
            }
            DurationDecision::RegenerateWithProsody { .. } => {
                // This voice cannot express prosody; the bounded stretch below avoids an AI retry.
            }
            DurationDecision::RegenerateSpeech { .. } => {
                let candidates = revise_speech_plan(
                    gemini_keys,
                    &plan,
                    actual_duration_ms,
                    target.effective_ms(),
                    &voice.styles,
                    1,
                )
                .await?;
                let (revised, _) = select_revision_candidate(
                    &client,
                    config,
                    ffmpeg,
                    &output,
                    &plan,
                    actual_duration_ms,
                    candidates,
                    voice,
                    target,
                    pause_fit,
                )
                .await?;
                plan = revised;
                speech_plan_revisions = 1;
                let stats = fit_audio_file(ffmpeg, &output, None, pause_fit)?;
                result.path = stats.path;
                result.measured_audio_ms = stats.measured_audio_ms;
                result.speech_end_ms = stats.speech_end_ms;
            }
            DurationDecision::TimeStretch { .. } => {}
            DurationDecision::Reject { .. } => {
                unreachable!("the first duration decision never rejects")
            }
        }

        if result.fallback {
            generated_durations.insert(plan.index, result.measured_audio_ms);
            generated_paths.insert(plan.index, result.path.clone());
            generated_speech_ends.insert(plan.index, result.speech_end_ms);
            continue;
        }

        if !result.fallback {
            let mut final_duration_ms = result.measured_audio_ms;
            if matches!(
                solve_duration(target, final_duration_ms, true)?,
                DurationDecision::Reject { .. }
            ) && speech_plan_revisions < MAX_SPEECH_PLAN_REVISIONS
            {
                progress.event(ProgressEvent::new(
                    Stage::Tts,
                    video,
                    format!(
                        "Segment {} remains over its timing limit; requesting its final concise revision.",
                        plan.index
                    ),
                ));
                let candidates = revise_speech_plan(
                    gemini_keys,
                    &plan,
                    final_duration_ms,
                    target.effective_ms(),
                    &voice.styles,
                    2,
                )
                .await?;
                let (revised, revised_duration_ms) = select_revision_candidate(
                    &client,
                    config,
                    ffmpeg,
                    &output,
                    &plan,
                    final_duration_ms,
                    candidates,
                    voice,
                    target,
                    pause_fit,
                )
                .await?;
                plan = revised;
                speech_plan_revisions = 2;
                final_duration_ms = revised_duration_ms;
                let stats = fit_audio_file(ffmpeg, &output, None, pause_fit)?;
                result.path = stats.path;
                result.measured_audio_ms = stats.measured_audio_ms;
                result.speech_end_ms = stats.speech_end_ms;
            }
            match solve_duration(target, final_duration_ms, true)? {
                DurationDecision::Accept => {}
                DurationDecision::TimeStretch { tempo } => progress.event(ProgressEvent::new(
                    Stage::Tts,
                    video,
                    format!(
                        "Segment {} uses bounded {tempo:.2}x timing stretch.",
                        plan.index
                    ),
                )),
                DurationDecision::Reject { required_tempo } => {
                    review_required = true;
                    progress.event(ProgressEvent::new(
                        Stage::Tts,
                        video,
                        format!(
                            "Segment {} still needs {required_tempo:.2}x stretch after {speech_plan_revisions} speech-plan revision(s); marking the video review-required.",
                            plan.index
                        ),
                    ));
                }
                DurationDecision::RegenerateWithProsody { .. }
                | DurationDecision::RegenerateSpeech { .. } => {
                    review_required = true;
                    progress.event(ProgressEvent::new(
                        Stage::Tts,
                        video,
                        format!(
                            "Segment {} requested another timing retry; marking the video review-required.",
                            plan.index
                        ),
                    ));
                }
            }
            generated_durations.insert(plan.index, final_duration_ms);
            generated_paths.insert(plan.index, result.path.clone());
            generated_speech_ends.insert(plan.index, result.speech_end_ms);
            calibrated_plans.push((plan.clone(), final_duration_ms));
        }
        progress.event(
            ProgressEvent::new(
                Stage::Tts,
                video,
                format!("Generated Natural Voice segment {}", plan.index),
            )
            .progress(position + 1, segments.len()),
        );
    }
    for (plan, measured_audio_ms) in calibrated_plans {
        duration_calibration.observe(&plan, measured_audio_ms);
    }
    duration_calibration.write(output_dir)?;
    let mut entries = segments
        .iter()
        .map(|segment| {
            let allocation = allocations
                .get(&segment.index)
                .expect("allocations cover every segment");
            let measured_audio_ms = generated_durations
                .get(&segment.index)
                .copied()
                .ok_or_else(|| {
                    anyhow::anyhow!("Missing generated duration for segment {}", segment.index)
                })?;
            let slot_ms = allocation
                .slot_end_ms
                .saturating_sub(allocation.render_start_ms);
            let tempo = (f64::from(measured_audio_ms) / f64::from(slot_ms.max(1))).max(1.0);
            if tempo > MAX_TIME_STRETCH_TEMPO + f64::EPSILON {
                review_required = true;
                progress.event(ProgressEvent::new(
                    Stage::Tts,
                    video,
                    format!(
                        "Segment {} requires {tempo:.2}x stretch; marking the video review-required instead of publishing it.",
                        segment.index
                    ),
                ));
            }
            Ok(ScheduledAudio {
                id: format!("segment-{}", segment.index),
                path: generated_paths
                    .get(&segment.index)
                    .cloned()
                    .with_context(|| format!("Missing generated path for segment {}", segment.index))?,
                start_ms: allocation.render_start_ms,
                end_ms: allocation.slot_end_ms,
                source_segments: vec![segment.index],
                measured_audio_ms,
                speech_end_ms: generated_speech_ends
                    .get(&segment.index)
                    .copied()
                    .flatten(),
                rendered_audio_ms: ((f64::from(measured_audio_ms) / tempo).ceil() as u32)
                    .min(slot_ms.max(1)),
                tempo,
                island_id: Some(allocation.island_id.clone()),
                cache_identity: String::new(),
                alignment: EntryAlignment::default(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| (entry.start_ms, entry.source_segments[0]));
    let manifest = RenderManifest::new(
        if review_required {
            crate::utterance::ManifestStatus::ReviewRequired
        } else {
            crate::utterance::ManifestStatus::UnanchoredContinuous
        },
        crate::utterance::TimingPipelineId::SegmentedRollback,
        entries,
    )
    .with_semantic_alignment_support(false)
    .with_source_timeline(timeline, layout)?
    .with_audio_cache_identities()?;
    let expected = segments
        .iter()
        .map(|segment| segment.index)
        .collect::<Vec<_>>();
    manifest
        .validate(&expected)
        .map_err(|error| anyhow!("review_required: Natural Voice manifest: {error:#}"))?;
    manifest
        .validate_source_timeline(timeline, layout)
        .map_err(|error| anyhow!("review_required: Natural Voice source alignment: {error:#}"))?;
    fs::write(manifest_path, serde_json::to_vec_pretty(&manifest)?).with_context(|| {
        format!(
            "Failed to write render manifest {}",
            manifest_path.display()
        )
    })?;
    Ok(())
}

#[derive(Clone)]
struct ContinuousAudio {
    utterance: Utterance,
    path: PathBuf,
    measured_audio_ms: u32,
    speech_end_ms: Option<u32>,
}

/// Generate one DragonHD REST utterance for each source-derived narration block.
pub async fn generate_all_tts_for_unanchored_utterances(
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    input_srt: &Path,
    plans: &[SpeechPlan],
    mode: NaturalMode,
    timeline: &SourceTimeline,
    layout: &SyncLayout,
    gemini_keys: &[String],
    voice: &AzureVoice,
    output_dir: &Path,
    manifest_path: &Path,
    pause_fit: bool,
    video: &str,
    progress: &dyn ProgressReporter,
) -> Result<()> {
    let segments = parse_srt(input_srt)?;
    let utterances = group_source_aligned_utterances(&segments, plans, timeline, layout)?;
    fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create {}", output_dir.display()))?;
    let mut duration_calibration = DurationCalibration::load(output_dir, voice);
    let calibrated_chars_per_second = duration_calibration.median_chars_per_second();
    let predicted = utterances
        .iter()
        .map(|utterance| {
            (
                utterance.block_id.clone(),
                utterance
                    .parts
                    .iter()
                    .map(|part| {
                        predict_plan_audio_ms_with_calibration(
                            &part.plan,
                            voice,
                            calibrated_chars_per_second,
                        )
                    })
                    .sum::<u32>()
                    .max(100),
            )
        })
        .collect::<HashMap<_, _>>();
    let (allocated, failed_islands) = allocate_blocks_best_effort(layout, &predicted, mode)
        .map_err(|error| anyhow!("review_required: source timing allocation: {error:#}"))?;
    let initial_allocations = allocated
        .into_iter()
        .map(|allocation| (allocation.block_id.clone(), allocation))
        .collect::<HashMap<_, _>>();
    let mut review_reasons = Vec::new();
    if !failed_islands.is_empty() {
        progress.event(ProgressEvent::new(
            Stage::Tts,
            video,
            format!(
                "Preflight timing is tight for {}; measuring real speech before deciding review.",
                failed_islands.join(", ")
            ),
        ));
    }
    let mut failed_blocks = HashMap::<usize, String>::new();
    let client = Client::new();
    let mut audio = Vec::with_capacity(utterances.len());

    progress.event(ProgressEvent::new(
        Stage::Tts,
        video,
        format!(
            "Grouped {} subtitle segments into {} unanchored utterances.",
            segments.len(),
            utterances.len()
        ),
    ));
    for (position, utterance) in utterances.iter().enumerate() {
        if progress.is_cancelled() {
            bail!("Dub job cancelled");
        }
        let output = output_dir.join(format!("utterance_{}.mp3", utterance.id));
        let plans = utterance
            .parts
            .iter()
            .map(|part| prepared_speech_plan(&part.plan))
            .collect::<Vec<_>>();
        let ssml = render_unanchored_utterance_ssml(voice, &plans)?;
        let stats = match synthesize_continuous_audio(
            &client, config, ffmpeg, &output, &ssml, voice, pause_fit,
        )
        .await
        {
            Ok(stats) => stats,
            Err(error) => {
                let reason = format!("utterance-{}:hd_synthesis_review:{error:#}", utterance.id);
                remove_audio_artifacts(&output);
                ffmpeg.create_silence(&output)?;
                write_tts_cache_key(
                    &output,
                    &tts_cache_key("azure-hd-review-v1", &voice.short_name, "", &reason),
                )?;
                failed_blocks.insert(position, reason.clone());
                review_reasons.push(reason);
                AudioFit {
                    path: output.clone(),
                    measured_audio_ms: duration_ms(ffmpeg, &output)?,
                    speech_end_ms: None,
                }
            }
        };
        audio.push(ContinuousAudio {
            utterance: utterance.clone(),
            path: stats.path,
            measured_audio_ms: stats.measured_audio_ms,
            speech_end_ms: stats.speech_end_ms,
        });
        progress.event(
            ProgressEvent::new(
                Stage::Tts,
                video,
                format!("Generated HD utterance {}.", utterance.id),
            )
            .progress(position + 1, utterances.len()),
        );
    }

    let mut allocations = initial_allocations;
    let (measured_allocations, allocation_failures) =
        measured_continuous_allocations(layout, &audio, mode, &allocations);
    allocations = measured_allocations;
    replace_measured_allocation_failures(&mut review_reasons, allocation_failures);

    let mut over_cap = continuous_over_cap_positions(&audio, &allocations, &failed_blocks);
    if !over_cap.is_empty() {
        let mut deixis_changed = false;
        for position in over_cap.clone() {
            if failed_blocks.contains_key(&position) {
                continue;
            }
            let mut revised = audio[position].utterance.clone();
            if !compact_utterance_onscreen_tokens(&mut revised) {
                continue;
            }
            let plans = revised
                .parts
                .iter()
                .map(|part| prepared_speech_plan(&part.plan))
                .collect::<Vec<_>>();
            let output = output_dir.join(format!("utterance_{}.mp3", revised.id));
            match synthesize_continuous_audio(
                &client,
                config,
                ffmpeg,
                &output,
                &render_unanchored_utterance_ssml(voice, &plans)?,
                voice,
                pause_fit,
            )
            .await
            {
                Ok(stats) => {
                    audio[position] = ContinuousAudio {
                        utterance: revised,
                        path: stats.path,
                        measured_audio_ms: stats.measured_audio_ms,
                        speech_end_ms: stats.speech_end_ms,
                    };
                    deixis_changed = true;
                    progress.event(ProgressEvent::new(
                        Stage::Tts,
                        video,
                        format!(
                            "HD utterance {} replaced an on-screen IP or URL with a short pointer.",
                            audio[position].utterance.id
                        ),
                    ));
                }
                Err(error) => {
                    let reason = format!(
                        "utterance-{}:hd_deixis_resynthesis_failed:{error:#}",
                        revised.id
                    );
                    failed_blocks.insert(position, reason.clone());
                    review_reasons.push(reason);
                }
            }
        }
        if deixis_changed {
            let (measured_allocations, allocation_failures) =
                measured_continuous_allocations(layout, &audio, mode, &allocations);
            allocations = measured_allocations;
            replace_measured_allocation_failures(&mut review_reasons, allocation_failures);
            over_cap = continuous_over_cap_positions(&audio, &allocations, &failed_blocks);
        }
    }

    if !over_cap.is_empty() {
        if gemini_keys.is_empty() {
            for position in &over_cap {
                let reason = format!(
                    "utterance-{}:hd_over_cap_without_revision",
                    audio[*position].utterance.id
                );
                failed_blocks.insert(*position, reason.clone());
                review_reasons.push(reason);
            }
        } else {
            let requests = over_cap
                .iter()
                .flat_map(|position| {
                    let candidate = &audio[*position];
                    let allocation = allocations
                        .get(&candidate.utterance.block_id)
                        .expect("over-cap block has an allocation");
                    let slot_ms = allocation
                        .slot_end_ms
                        .saturating_sub(allocation.render_start_ms)
                        .max(1);
                    let target_total = slot_ms.saturating_mul(113).div_ceil(100);
                    let predicted_total = candidate
                        .utterance
                        .parts
                        .iter()
                        .map(|part| {
                            predict_plan_audio_ms_with_calibration(
                                &part.plan,
                                voice,
                                calibrated_chars_per_second,
                            )
                        })
                        .sum::<u32>()
                        .max(1);
                    candidate.utterance.parts.iter().map(move |part| {
                        let predicted = predict_plan_audio_ms_with_calibration(
                            &part.plan,
                            voice,
                            calibrated_chars_per_second,
                        )
                        .max(1);
                        let actual = candidate
                            .measured_audio_ms
                            .saturating_mul(predicted)
                            .checked_div(predicted_total)
                            .unwrap_or(candidate.measured_audio_ms)
                            .max(100);
                        let target = target_total
                            .saturating_mul(predicted)
                            .checked_div(predicted_total)
                            .unwrap_or(target_total)
                            .max(100);
                        SpeechPlanRevisionRequest {
                            plan: part.plan.clone(),
                            actual_duration_ms: actual,
                            target_duration_ms: target,
                            allowed_styles: voice.styles.clone(),
                            maximum_words: revision_word_budget(
                                &part.plan,
                                actual,
                                target,
                                voice.words_per_minute.unwrap_or(130),
                            ),
                        }
                    })
                })
                .collect::<Vec<_>>();
            match revise_speech_plans_batch(gemini_keys, &requests, 1).await {
                Ok(revisions) => {
                    let revisions = revisions
                        .into_iter()
                        .map(|plan| (plan.index, plan))
                        .collect::<HashMap<_, _>>();
                    for position in &over_cap {
                        let mut revised = audio[*position].utterance.clone();
                        let mut changed = false;
                        for part in &mut revised.parts {
                            if let Some(plan) = revisions.get(&part.index) {
                                if plan.index == part.index
                                    && plan.spoken_text != part.plan.spoken_text
                                {
                                    part.plan = plan.clone();
                                    changed = true;
                                }
                            }
                        }
                        if !changed && compact_utterance_onscreen_tokens(&mut revised) {
                            changed = true;
                        }
                        if !changed {
                            let reason =
                                format!("utterance-{}:hd_concise_revision_unresolved", revised.id);
                            failed_blocks.insert(*position, reason.clone());
                            review_reasons.push(reason);
                            continue;
                        }
                        let plans = revised
                            .parts
                            .iter()
                            .map(|part| prepared_speech_plan(&part.plan))
                            .collect::<Vec<_>>();
                        let output = output_dir.join(format!("utterance_{}.mp3", revised.id));
                        match synthesize_continuous_audio(
                            &client,
                            config,
                            ffmpeg,
                            &output,
                            &render_unanchored_utterance_ssml(voice, &plans)?,
                            voice,
                            pause_fit,
                        )
                        .await
                        {
                            Ok(stats) => {
                                audio[*position] = ContinuousAudio {
                                    utterance: revised,
                                    path: stats.path,
                                    measured_audio_ms: stats.measured_audio_ms,
                                    speech_end_ms: stats.speech_end_ms,
                                };
                            }
                            Err(error) => {
                                let reason = format!(
                                    "utterance-{}:hd_concise_revision_failed:{error:#}",
                                    revised.id
                                );
                                failed_blocks.insert(*position, reason.clone());
                                review_reasons.push(reason);
                            }
                        }
                    }
                }
                Err(error) => {
                    for position in &over_cap {
                        let reason = format!(
                            "utterance-{}:hd_concise_revision_failed:{error:#}",
                            audio[*position].utterance.id
                        );
                        failed_blocks.insert(*position, reason.clone());
                        review_reasons.push(reason);
                    }
                }
            }
        }
        let (measured_allocations, allocation_failures) =
            measured_continuous_allocations(layout, &audio, mode, &allocations);
        allocations = measured_allocations;
        replace_measured_allocation_failures(&mut review_reasons, allocation_failures);
    }

    for (position, candidate) in audio.iter().enumerate() {
        if failed_blocks.contains_key(&position) {
            continue;
        }
        let Some(allocation) = allocations.get(&candidate.utterance.block_id) else {
            continue;
        };
        let slot_ms = allocation
            .slot_end_ms
            .saturating_sub(allocation.render_start_ms)
            .max(1);
        if f64::from(candidate.measured_audio_ms) / f64::from(slot_ms)
            > MAX_TIME_STRETCH_TEMPO + f64::EPSILON
        {
            continue;
        }
        let plans = candidate
            .utterance
            .parts
            .iter()
            .map(|part| part.plan.clone())
            .collect::<Vec<_>>();
        duration_calibration.observe_plans(&plans, candidate.measured_audio_ms);
    }
    duration_calibration.write(output_dir)?;

    let mut entries = audio
        .iter()
        .enumerate()
        .map(|(position, candidate)| {
            let allocation = allocations
                .get(&candidate.utterance.block_id)
                .expect("final allocations cover every narration block");
            let slot_ms = allocation
                .slot_end_ms
                .saturating_sub(allocation.render_start_ms)
                .max(1);
            let tempo = (f64::from(candidate.measured_audio_ms) / f64::from(slot_ms)).max(1.0);
            if tempo > MAX_TIME_STRETCH_TEMPO + f64::EPSILON
                && !failed_blocks.contains_key(&position)
            {
                review_reasons.push(format!(
                    "utterance-{}:hd_over_cap:required={tempo:.3}:allowed={MAX_TIME_STRETCH_TEMPO:.2}",
                    candidate.utterance.id
                ));
            }
            ScheduledAudio {
                id: format!("utterance-{}", candidate.utterance.id),
                path: candidate.path.clone(),
                start_ms: allocation.render_start_ms,
                end_ms: allocation.slot_end_ms,
                source_segments: candidate
                    .utterance
                    .parts
                    .iter()
                    .map(|part| part.index)
                    .collect(),
                measured_audio_ms: candidate.measured_audio_ms,
                speech_end_ms: candidate.speech_end_ms,
                rendered_audio_ms: ((f64::from(candidate.measured_audio_ms) / tempo).ceil() as u32)
                    .min(slot_ms),
                tempo,
                island_id: Some(candidate.utterance.island_id.clone()),
                cache_identity: String::new(),
                alignment: EntryAlignment::default(),
            }
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| (entry.start_ms, entry.source_segments[0]));
    let status = if review_reasons.is_empty() {
        ManifestStatus::UnanchoredContinuous
    } else {
        ManifestStatus::ReviewRequired
    };
    let manifest = RenderManifest::new(status, TimingPipelineId::UnanchoredContinuous, entries)
        .with_semantic_alignment_support(false)
        .with_diagnostics(ManifestDiagnostics {
            review_reasons,
            ..ManifestDiagnostics::default()
        })
        .with_source_timeline(timeline, layout)?
        .with_audio_cache_identities()?;
    let expected = segments
        .iter()
        .map(|segment| segment.index)
        .collect::<Vec<_>>();
    manifest
        .validate(&expected)
        .map_err(|error| anyhow!("review_required: Natural Voice manifest: {error:#}"))?;
    manifest
        .validate_source_timeline(timeline, layout)
        .map_err(|error| anyhow!("review_required: Natural Voice source alignment: {error:#}"))?;
    fs::write(manifest_path, serde_json::to_vec_pretty(&manifest)?).with_context(|| {
        format!(
            "Failed to write unanchored render manifest {}",
            manifest_path.display()
        )
    })?;
    Ok(())
}

fn measured_continuous_allocations(
    layout: &SyncLayout,
    audio: &[ContinuousAudio],
    mode: NaturalMode,
    base: &HashMap<String, AllocatedBlock>,
) -> (HashMap<String, AllocatedBlock>, Vec<String>) {
    let measured = audio
        .iter()
        .map(|candidate| {
            (
                candidate.utterance.block_id.clone(),
                candidate.measured_audio_ms,
            )
        })
        .collect::<HashMap<_, _>>();
    let mut allocations = base.clone();
    for group in allocation_groups(layout) {
        if let Ok(values) = allocate_measured_group(layout, &group.id, &measured, mode) {
            for value in values {
                allocations.insert(value.block_id.clone(), value);
            }
        }
    }
    apply_measured_spillback(&mut allocations, &measured, layout);
    let failures = allocation_groups(layout)
        .into_iter()
        .filter(|group| group_exceeds_tempo_cap(group, &allocations, &measured))
        .map(|group| format!("{}:measured_allocation_infeasible", group.id))
        .collect::<Vec<_>>();
    (allocations, failures)
}

fn continuous_over_cap_positions(
    audio: &[ContinuousAudio],
    allocations: &HashMap<String, AllocatedBlock>,
    failed_blocks: &HashMap<usize, String>,
) -> Vec<usize> {
    audio
        .iter()
        .enumerate()
        .filter(|(position, candidate)| {
            if failed_blocks.contains_key(position) {
                return false;
            }
            let Some(allocation) = allocations.get(&candidate.utterance.block_id) else {
                return false;
            };
            let slot_ms = allocation
                .slot_end_ms
                .saturating_sub(allocation.render_start_ms)
                .max(1);
            f64::from(candidate.measured_audio_ms) / f64::from(slot_ms)
                > MAX_TIME_STRETCH_TEMPO + f64::EPSILON
        })
        .map(|(position, _)| position)
        .collect()
}

fn compact_utterance_onscreen_tokens(utterance: &mut Utterance) -> bool {
    let mut changed = false;
    for part in &mut utterance.parts {
        changed |= part.plan.compact_onscreen_technical_tokens();
    }
    changed
}

fn replace_measured_allocation_failures(review_reasons: &mut Vec<String>, failures: Vec<String>) {
    review_reasons.retain(|reason| !reason.contains(":measured_allocation_infeasible"));
    review_reasons.extend(failures);
}

/// Generate grouped utterances for voices that expose reliable Azure timing metadata.
pub async fn generate_all_tts_for_aligned_utterances(
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    input_srt: &Path,
    plans: &[SpeechPlan],
    mode: NaturalMode,
    timeline: &SourceTimeline,
    layout: &SyncLayout,
    gemini_keys: &[String],
    voice: &AzureVoice,
    output_dir: &Path,
    manifest_path: &Path,
    pause_fit: bool,
    video: &str,
    progress: &dyn ProgressReporter,
) -> Result<()> {
    if voice.capabilities.timing_events != CapabilityState::Supported {
        bail!(
            "Azure voice {} does not support aligned timing events",
            voice.short_name
        );
    }
    let segments = parse_srt(input_srt)?;
    let utterances = group_source_aligned_utterances(&segments, plans, timeline, layout)?;
    fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create {}", output_dir.display()))?;
    let mut duration_calibration = DurationCalibration::load(output_dir, voice);
    let calibrated_chars_per_second = duration_calibration.median_chars_per_second();
    let predicted = utterances
        .iter()
        .map(|utterance| {
            (
                utterance.block_id.clone(),
                utterance
                    .parts
                    .iter()
                    .map(|part| {
                        predict_plan_audio_ms_with_calibration(
                            &part.plan,
                            voice,
                            calibrated_chars_per_second,
                        )
                    })
                    .sum::<u32>()
                    .max(100),
            )
        })
        .collect::<HashMap<_, _>>();
    let (allocated_blocks, failed_islands) = allocate_blocks_best_effort(layout, &predicted, mode)
        .map_err(|error| anyhow!("review_required: source timing allocation: {error:#}"))?;
    let allocations = allocated_blocks
        .into_iter()
        .map(|allocation| (allocation.block_id.clone(), allocation))
        .collect::<HashMap<_, _>>();
    progress.event(ProgressEvent::new(
        Stage::Tts,
        video,
        format!(
            "Grouped {} subtitle segments into {} aligned utterances.",
            segments.len(),
            utterances.len()
        ),
    ));
    let mut utterance_order = (0..utterances.len()).collect::<Vec<_>>();
    utterance_order.sort_by_key(|index| {
        let utterance = &utterances[*index];
        let allocation = allocations
            .get(&utterance.block_id)
            .expect("allocations cover every aligned utterance");
        (
            allocation
                .slot_end_ms
                .saturating_sub(allocation.render_start_ms)
                .saturating_sub(allocation.rendered_audio_ms),
            allocation.island_id.clone(),
            utterance.id,
        )
    });
    progress.event(ProgressEvent::new(
        Stage::Tts,
        video,
        "Synthesizing tighter sync islands first; source order remains the render clock."
            .to_owned(),
    ));

    let mut entries = Vec::with_capacity(utterances.len());
    let mut semantic_evidence = HashMap::<String, SemanticEvidence>::new();
    let mut pending_utterances = Vec::with_capacity(utterances.len());
    let mut diagnostics = ManifestDiagnostics::default();
    if !failed_islands.is_empty() {
        progress.event(ProgressEvent::new(
            Stage::Tts,
            video,
            format!(
                "Preflight timing is tight for {}; measuring real speech before deciding review.",
                failed_islands.join(", ")
            ),
        ));
    }
    for (position, utterance_index) in utterance_order.iter().enumerate() {
        let utterance = &utterances[*utterance_index];
        if progress.is_cancelled() {
            bail!("Dub job cancelled");
        }
        progress.event(ProgressEvent::new(
            Stage::Tts,
            video,
            format!(
                "Synthesizing utterance {} from subtitle segments {}-{}.",
                utterance.id,
                utterance.parts.first().map_or(0, |part| part.index),
                utterance.parts.last().map_or(0, |part| part.index)
            ),
        ));
        let utterance_output = output_dir.join(format!("utterance_{}.mp3", utterance.id));
        let candidate_output = revision_candidate_path(&utterance_output, 20_000);
        let prepared = utterance
            .parts
            .iter()
            .map(|part| prepared_speech_plan(&part.plan))
            .collect::<Vec<_>>();
        let bookmarks = utterance
            .parts
            .iter()
            .map(|part| bookmark_name(utterance.id, part.index))
            .collect::<Vec<_>>();
        let semantic_bookmarks = if semantic_anchor_timing_available(timeline) {
            utterance
                .parts
                .iter()
                .zip(&prepared)
                .flat_map(|(part, prepared)| {
                    prepared.semantic_anchors.iter().filter_map(|anchor| {
                        timeline
                            .has_semantic_anchor_timing(&anchor.anchor_id)
                            .then(|| {
                                (
                                    part.index,
                                    anchor.spoken_phrase.clone(),
                                    semantic_bookmark_name(utterance.id, &anchor.anchor_id),
                                )
                            })
                    })
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let timing_bookmarks = bookmarks
            .iter()
            .cloned()
            .chain(semantic_bookmarks.iter().map(|(_, _, mark)| mark.clone()))
            .collect::<Vec<_>>();
        let ssml = render_utterance_ssml_with_semantic_anchors(
            voice,
            &prepared,
            &bookmarks,
            &semantic_bookmarks,
            0,
        )?;
        let (events, duration_ms, audio_path, speech_end_ms) = match generate_timed_utterance(
            config,
            ffmpeg,
            &candidate_output,
            &ssml,
            &voice.short_name,
            &prepared,
            &timing_bookmarks,
            pause_fit,
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                remove_audio_artifacts(&candidate_output);
                bail!(
                    "review_required: utterance {} produced no valid timed audio draft: {error:#}",
                    utterance.id
                );
            }
        };
        let display_alignment = match timeline.mode {
            crate::sync_timeline::TimingMode::CueOnly => AlignmentResult {
                accepted: true,
                worst_anchor: None,
                expected_ms: None,
                actual_ms: None,
                drift_ms: None,
                reason: Some(if timeline.has_projected_anchor_timing() {
                    "cue_only_display_boundaries".to_owned()
                } else {
                    "cue_only_timing".to_owned()
                }),
            },
            crate::sync_timeline::TimingMode::CaptionDerived
            | crate::sync_timeline::TimingMode::Granular => {
                validate_bookmark_structure(utterance, &events)
            }
        };
        let semantic_expected = semantic_expectations(utterance, timeline);
        let structural_issue = if !display_alignment.accepted {
            display_alignment.reason
        } else if !semantic_bookmarks_are_complete(&events, &semantic_expected) {
            Some("semantic_bookmark_missing_or_duplicated".to_owned())
        } else {
            None
        };
        if structural_issue.is_some() {
            progress.event(ProgressEvent::new(
                Stage::Tts,
                video,
                format!(
                    "Utterance {} has incomplete timing evidence; keeping one review draft.",
                    utterance.id
                ),
            ));
        }
        pending_utterances.push(PendingUtterance {
            utterance: utterance.clone(),
            path: audio_path,
            measured_audio_ms: duration_ms,
            speech_end_ms,
            events,
            semantic_expected,
            structural_issue,
        });
        progress.event(
            ProgressEvent::new(
                Stage::Tts,
                video,
                format!(
                    "Generated base audio for utterance {}; queued for measured island allocation.",
                    utterance.id
                ),
            )
            .progress(position + 1, utterances.len()),
        );
    }

    for candidate in &pending_utterances {
        let part_durations = measured_part_durations(candidate);
        for part in &candidate.utterance.parts {
            if let Some(measured_audio_ms) = part_durations.get(&part.index) {
                duration_calibration.observe(&part.plan, *measured_audio_ms);
            }
        }
    }
    duration_calibration.write(output_dir)?;

    // Second pass: replace preflight slots with actual Azure durations, allocation group by
    // allocation group.  A group may cross only artificial duration caps; confirmed source
    // barriers remain independent clocks.  A group is reallocated only when every block has a
    // whole-utterance candidate; otherwise its original schedule remains the safe fallback clock.
    let measured_by_block = pending_utterances
        .iter()
        .map(|candidate| {
            (
                candidate.utterance.block_id.clone(),
                candidate.measured_audio_ms,
            )
        })
        .collect::<HashMap<_, _>>();
    let mut measured_anchor_map = measured_anchors(&pending_utterances, timeline);
    for (block_id, measured_audio_ms) in &measured_by_block {
        if let Some(allocation) = allocations.get(block_id) {
            diagnostics
                .prediction_errors_ms
                .push(i64::from(*measured_audio_ms) - i64::from(allocation.predicted_audio_ms));
        }
    }
    let mut pending_by_block = pending_utterances
        .into_iter()
        .map(|candidate| (candidate.utterance.block_id.clone(), candidate))
        .collect::<HashMap<_, _>>();
    let mut render_allocations = allocations.clone();
    let mut concise_candidates = Vec::new();
    for group in allocation_groups(layout) {
        let block_ids = &group.block_ids;
        let complete = block_ids
            .iter()
            .all(|block_id| pending_by_block.contains_key(block_id));
        let measured = block_ids
            .iter()
            .filter_map(|block_id| {
                pending_by_block
                    .get(block_id)
                    .map(|candidate| (block_id.clone(), candidate.measured_audio_ms))
            })
            .collect::<HashMap<_, _>>();
        let duration_allocated = complete
            && match allocate_measured_group(layout, &group.id, &measured, mode) {
                Ok(values) => {
                    for value in &values {
                        render_allocations.insert(value.block_id.clone(), value.clone());
                    }
                    progress.event(ProgressEvent::new(
                        Stage::Tts,
                        video,
                        format!(
                            "Reallocated measured durations for {} ({} blocks).",
                            group.id,
                            values.len()
                        ),
                    ));
                    true
                }
                Err(error) => {
                    diagnostics.review_reasons.push(format!(
                        "{}:measured_allocation_infeasible:{error}",
                        group.id
                    ));
                    false
                }
            };
        if duration_allocated {
            match allocate_measured_group_with_anchors(
                layout,
                &group.id,
                &measured,
                &measured_anchor_map,
                mode,
            ) {
                Ok(values) => {
                    for value in values {
                        render_allocations.insert(value.block_id.clone(), value);
                    }
                }
                Err(error) => diagnostics.review_reasons.push(format!(
                    "{}:measured_anchor_allocation_infeasible:{error}",
                    group.id
                )),
            }
        }
        apply_measured_spillback_except(
            &mut render_allocations,
            &measured,
            layout,
            &hard_anchored_block_ids(&measured_anchor_map),
        );
        cover_hard_anchor_windows(
            &mut render_allocations,
            &measured,
            layout,
            &measured_anchor_map,
        );
        let duration_feasible =
            complete && !group_exceeds_tempo_cap(&group, &render_allocations, &measured);

        for block_id in block_ids {
            let Some(base) = pending_by_block.remove(block_id) else {
                continue;
            };
            let allocation = render_allocations
                .get(block_id)
                .cloned()
                .with_context(|| format!("Missing final allocation for {block_id}"))?;
            let slot_ms = allocation
                .slot_end_ms
                .saturating_sub(allocation.render_start_ms)
                .max(1);
            let tempo = (f64::from(base.measured_audio_ms) / f64::from(slot_ms)).max(1.0);
            let rendered_audio_ms = (f64::from(base.measured_audio_ms) / tempo).ceil() as u32;
            let technically_fit =
                tempo <= MAX_TIME_STRETCH_TEMPO + f64::EPSILON && rendered_audio_ms <= slot_ms;
            let semantic_alignment = if base.semantic_expected.is_empty() {
                AlignmentResult {
                    accepted: true,
                    worst_anchor: None,
                    expected_ms: None,
                    actual_ms: None,
                    drift_ms: None,
                    reason: None,
                }
            } else {
                validate_semantic_alignment(
                    &base.events,
                    &base.semantic_expected,
                    allocation.render_start_ms,
                    tempo,
                )
            };
            let semantic_delays = if voice.capabilities.semantic_break == CapabilityState::Supported
                && tempo <= MAX_TIME_STRETCH_TEMPO + f64::EPSILON
            {
                semantic_anchor_delay_repairs(
                    &base.events,
                    &base.semantic_expected,
                    allocation.render_start_ms,
                    tempo,
                )
            } else {
                Vec::new()
            };
            let semantic_repair_feasible = semantic_delays.is_empty()
                || u64::from(base.measured_audio_ms).saturating_add(
                    semantic_delays
                        .iter()
                        .map(|(_, delay)| u64::from(*delay))
                        .sum::<u64>(),
                ) <= (f64::from(slot_ms) * MAX_TIME_STRETCH_TEMPO).floor() as u64;
            let duration_rate_repair = semantic_alignment.accepted
                && semantic_delays.is_empty()
                && voice.capabilities.manual_prosody == CapabilityState::Supported
                && duration_repair_rate(base.measured_audio_ms, slot_ms).is_some();
            let follow_up = aligned_follow_up(
                base.structural_issue.is_some(),
                duration_feasible && technically_fit,
                technically_fit,
                semantic_repair_feasible,
                semantic_alignment.accepted,
                duration_rate_repair,
            );
            match follow_up {
                AlignedFollowUp::Accept => {
                    let (entry, evidence) = pending_entry(base, &allocation);
                    if let Some((id, evidence)) = evidence {
                        semantic_evidence.insert(id, evidence);
                    }
                    entries.push(entry);
                }
                AlignedFollowUp::Concise => concise_candidates.push(base),
                AlignedFollowUp::Review => {
                    diagnostics.review_reasons.push(format!(
                        "utterance-{}:timing_structure:{}",
                        base.utterance.id,
                        base.structural_issue.as_deref().unwrap_or("unknown")
                    ));
                    let (entry, evidence) = pending_entry(base, &allocation);
                    if let Some((id, evidence)) = evidence {
                        semantic_evidence.insert(id, evidence);
                    }
                    entries.push(entry);
                }
                AlignedFollowUp::DurationRepair | AlignedFollowUp::Repair => {
                    let duration_repair = follow_up == AlignedFollowUp::DurationRepair;
                    let rate_percent = if duration_repair {
                        duration_repair_rate(base.measured_audio_ms, slot_ms).unwrap_or(0)
                    } else if semantic_delays.is_empty()
                        && voice.capabilities.manual_prosody == CapabilityState::Supported
                    {
                        alignment_repair_rate(&semantic_alignment, &base.utterance)
                    } else {
                        0
                    };
                    if semantic_delays.is_empty() && rate_percent == 0 {
                        diagnostics.review_reasons.push(format!(
                            "utterance-{}:{}_repair_unavailable",
                            base.utterance.id,
                            if duration_repair {
                                "duration"
                            } else {
                                "semantic"
                            },
                        ));
                        let (entry, evidence) = pending_entry(base, &allocation);
                        if let Some((id, evidence)) = evidence {
                            semantic_evidence.insert(id, evidence);
                        }
                        entries.push(entry);
                        continue;
                    }
                    progress.event(ProgressEvent::new(
                        Stage::Tts,
                        video,
                        format!(
                            "Applying one measured {} timing repair to utterance {}.",
                            if duration_repair {
                                "duration"
                            } else {
                                "semantic"
                            },
                            base.utterance.id,
                        ),
                    ));
                    let candidate_path = revision_candidate_path(
                        &output_dir.join(format!("utterance_{}.mp3", base.utterance.id)),
                        if duration_repair { 20_002 } else { 20_001 },
                    );
                    let repair = synthesize_revised_utterance(
                        config,
                        ffmpeg,
                        voice,
                        base.utterance.clone(),
                        timeline,
                        &candidate_path,
                        rate_percent,
                        &semantic_delays,
                        pause_fit,
                    )
                    .await;
                    let repaired = match repair {
                        Ok(candidate) => {
                            let repaired_tempo = (f64::from(candidate.measured_audio_ms)
                                / f64::from(slot_ms))
                            .max(1.0);
                            let accepted = repaired_tempo <= MAX_TIME_STRETCH_TEMPO + f64::EPSILON
                                && validate_semantic_alignment(
                                    &candidate.events,
                                    &candidate.semantic_expected,
                                    allocation.render_start_ms,
                                    repaired_tempo,
                                )
                                .accepted;
                            if accepted { Some(candidate) } else { None }
                        }
                        Err(error) => {
                            progress.event(ProgressEvent::new(
                                Stage::Tts,
                                video,
                                format!(
                                    "Semantic repair for utterance {} was discarded: {error:#}",
                                    base.utterance.id
                                ),
                            ));
                            None
                        }
                    };
                    if let Some(repaired) = repaired {
                        for (id, anchors) in
                            measured_anchors(std::slice::from_ref(&repaired), timeline)
                        {
                            measured_anchor_map.insert(id, anchors);
                        }
                        let (entry, evidence) = pending_entry(repaired, &allocation);
                        if let Some((id, evidence)) = evidence {
                            semantic_evidence.insert(id, evidence);
                        }
                        entries.push(entry);
                    } else if duration_repair {
                        // Azure rate undershoot still leaves the slot over 1.13x. Fall through
                        // to a Gemini concise rewrite instead of fail-closing on SSML alone.
                        diagnostics.review_reasons.push(format!(
                            "utterance-{}:duration_repair_unresolved",
                            base.utterance.id
                        ));
                        concise_candidates.push(base);
                    } else {
                        diagnostics.review_reasons.push(format!(
                            "utterance-{}:semantic_repair_unresolved",
                            base.utterance.id
                        ));
                        let (entry, evidence) = pending_entry(base, &allocation);
                        if let Some((id, evidence)) = evidence {
                            semantic_evidence.insert(id, evidence);
                        }
                        entries.push(entry);
                    }
                }
            }
        }
    }

    let revision_cache_path = output_dir.join(".gemini-whole-revisions-v3.json");
    let mut revision_cache = fs::read(&revision_cache_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<RevisionCache>(&bytes).ok())
        .unwrap_or_default();
    let mut revision_cache_dirty = false;
    let mut requests = Vec::new();
    for candidate in &concise_candidates {
        let allocation = render_allocations
            .get(&candidate.utterance.block_id)
            .with_context(|| {
                format!(
                    "Missing concise allocation for {}",
                    candidate.utterance.block_id
                )
            })?;
        let slot_ms = allocation
            .slot_end_ms
            .saturating_sub(allocation.render_start_ms)
            .max(1);
        let measured_parts = measured_part_durations(candidate);
        let measured_total = measured_parts
            .values()
            .map(|value| u64::from(*value))
            .sum::<u64>();
        for part in &candidate.utterance.parts {
            let actual = measured_parts.get(&part.index).copied().unwrap_or_else(|| {
                let predicted = predict_plan_audio_ms_with_calibration(
                    &part.plan,
                    voice,
                    calibrated_chars_per_second,
                );
                let predicted_total = candidate
                    .utterance
                    .parts
                    .iter()
                    .map(|part| {
                        u64::from(predict_plan_audio_ms_with_calibration(
                            &part.plan,
                            voice,
                            calibrated_chars_per_second,
                        ))
                    })
                    .sum::<u64>()
                    .max(1);
                u32::try_from(
                    u64::from(candidate.measured_audio_ms) * u64::from(predicted) / predicted_total,
                )
                .unwrap_or(candidate.measured_audio_ms)
                .max(1)
            });
            let target = if measured_total == 0 {
                slot_ms
            } else {
                u32::try_from(u64::from(slot_ms) * u64::from(actual) / measured_total)
                    .unwrap_or(slot_ms)
                    .max(100)
            };
            if f64::from(actual)
                > f64::from(target) * MAX_TIME_STRETCH_TEMPO
                    + f64::from(MAX_TIME_STRETCH_TOLERANCE_MS)
            {
                requests.push(SpeechPlanRevisionRequest {
                    plan: part.plan.clone(),
                    actual_duration_ms: actual,
                    target_duration_ms: target,
                    allowed_styles: voice.styles.clone(),
                    maximum_words: revision_word_budget(
                        &part.plan,
                        actual,
                        target,
                        voice.words_per_minute.unwrap_or(130),
                    ),
                });
            }
        }
    }
    let mut revised_by_index = HashMap::new();
    if !requests.is_empty() && !gemini_keys.is_empty() {
        progress.event(ProgressEvent::new(
            Stage::Tts,
            video,
            format!(
                "Requesting one batched concise revision pass for {} overlong speech parts.",
                requests.len()
            ),
        ));
        for request_batch in requests.chunks(CONCISE_REVISION_BATCH_SIZE) {
            let cache_key = revision_cache_key(request_batch, 1)?;
            let revisions = if let Some(cached) = revision_cache.entries.get(&cache_key) {
                record_gemini_cache_hit();
                cached.clone()
            } else {
                match revise_speech_plans_batch(gemini_keys, request_batch, 1).await {
                    Ok(revisions) => {
                        let returned = revisions
                            .iter()
                            .map(|plan| plan.index)
                            .collect::<HashSet<_>>();
                        if request_batch
                            .iter()
                            .all(|request| returned.contains(&request.plan.index))
                        {
                            revision_cache.entries.insert(cache_key, revisions.clone());
                            revision_cache_dirty = true;
                        }
                        revisions
                    }
                    Err(error) => {
                        progress.event(ProgressEvent::new(
                            Stage::Tts,
                            video,
                            format!(
                                "Batched concise revision unavailable; preserving base audio: {error:#}"
                            ),
                        ));
                        Vec::new()
                    }
                }
            };
            revised_by_index.extend(
                revisions
                    .into_iter()
                    .map(|revision| (revision.index, revision)),
            );
            let missing = request_batch
                .iter()
                .filter(|request| !revised_by_index.contains_key(&request.plan.index))
                .cloned()
                .collect::<Vec<_>>();
            for request in missing {
                match revise_speech_plans_batch(gemini_keys, std::slice::from_ref(&request), 1)
                    .await
                {
                    Ok(revisions) => {
                        revised_by_index.extend(
                            revisions
                                .into_iter()
                                .map(|revision| (revision.index, revision)),
                        );
                    }
                    Err(error) => {
                        progress.event(ProgressEvent::new(
                            Stage::Tts,
                            video,
                            format!(
                                "Concise revision for speech part {} was unavailable: {error:#}",
                                request.plan.index
                            ),
                        ));
                    }
                }
            }
        }
    }
    if revision_cache_dirty {
        fs::write(
            &revision_cache_path,
            serde_json::to_vec_pretty(&revision_cache)?,
        )?;
    }
    for mut base in concise_candidates {
        let allocation = render_allocations
            .get(&base.utterance.block_id)
            .cloned()
            .with_context(|| format!("Missing final allocation for {}", base.utterance.block_id))?;
        let mut revised_utterance = base.utterance.clone();
        let mut changed = false;
        for part in &mut revised_utterance.parts {
            if let Some(revision) = revised_by_index.get(&part.index) {
                part.plan = revision.clone();
                changed = true;
            }
        }
        if !changed {
            changed = compact_utterance_onscreen_tokens(&mut revised_utterance);
        }
        let mut revision_applied = false;
        if changed {
            let candidate_path = revision_candidate_path(
                &output_dir.join(format!("utterance_{}.mp3", base.utterance.id)),
                30_000,
            );
            match synthesize_revised_utterance(
                config,
                ffmpeg,
                voice,
                revised_utterance,
                timeline,
                &candidate_path,
                0,
                &[],
                pause_fit,
            )
            .await
            {
                Ok(revision) if revision.measured_audio_ms < base.measured_audio_ms => {
                    base = revision;
                    revision_applied = true;
                    progress.event(ProgressEvent::new(
                        Stage::Tts,
                        video,
                        format!(
                            "Applied one whole-utterance concise revision for utterance {}.",
                            base.utterance.id
                        ),
                    ));
                }
                Ok(_) => {}
                Err(error) => {
                    remove_audio_artifacts(&candidate_path);
                    progress.event(ProgressEvent::new(
                        Stage::Tts,
                        video,
                        format!(
                            "Concise revision for utterance {} was discarded: {error:#}",
                            base.utterance.id
                        ),
                    ));
                }
            }
        }
        if !revision_applied {
            let mut deixis_utterance = base.utterance.clone();
            if compact_utterance_onscreen_tokens(&mut deixis_utterance) {
                let candidate_path = revision_candidate_path(
                    &output_dir.join(format!("utterance_{}.mp3", base.utterance.id)),
                    30_001,
                );
                match synthesize_revised_utterance(
                    config,
                    ffmpeg,
                    voice,
                    deixis_utterance,
                    timeline,
                    &candidate_path,
                    0,
                    &[],
                    pause_fit,
                )
                .await
                {
                    Ok(revision) if revision.measured_audio_ms < base.measured_audio_ms => {
                        base = revision;
                        revision_applied = true;
                        progress.event(ProgressEvent::new(
                            Stage::Tts,
                            video,
                            format!(
                                "HD-aligned utterance {} replaced an on-screen path, IP, or URL with a short pointer.",
                                base.utterance.id
                            ),
                        ));
                    }
                    Ok(_) => remove_audio_artifacts(&candidate_path),
                    Err(_) => remove_audio_artifacts(&candidate_path),
                }
            }
        }
        if !revision_applied {
            diagnostics.review_reasons.push(format!(
                "utterance-{}:concise_revision_unresolved",
                base.utterance.id
            ));
        } else {
            let part_durations = measured_part_durations(&base);
            for part in &base.utterance.parts {
                if let Some(measured_audio_ms) = part_durations.get(&part.index) {
                    duration_calibration.observe(&part.plan, *measured_audio_ms);
                }
            }
        }
        for (block_id, anchors) in measured_anchors(std::slice::from_ref(&base), timeline) {
            measured_anchor_map.insert(block_id, anchors);
        }
        let (entry, evidence) = pending_entry(base, &allocation);
        if let Some((id, evidence)) = evidence {
            semantic_evidence.insert(id, evidence);
        }
        entries.push(entry);
    }
    duration_calibration.write(output_dir)?;
    // Reallocate once more after concise revisions. Revisions may change measured durations
    // substantially, so a pre-revision infeasibility is only provisional evidence.
    let final_failed_groups = finalize_measured_schedule(
        layout,
        &mut entries,
        &mut render_allocations,
        &measured_anchor_map,
        mode,
    );
    diagnostics
        .review_reasons
        .retain(|reason| !is_provisional_review_reason(reason));
    let mut review_required = diagnostics
        .review_reasons
        .iter()
        .any(|reason| reason.contains(":timing_structure:"));
    if !final_failed_groups.is_empty() {
        review_required = true;
        diagnostics.review_reasons.extend(
            final_failed_groups
                .iter()
                .map(|group| format!("{group}:measured_allocation_infeasible_after_revision")),
        );
    }

    for entry in &entries {
        if entry.tempo > MAX_TIME_STRETCH_TEMPO + f64::EPSILON {
            review_required = true;
            diagnostics.review_reasons.push(format!(
                "{}:final_timing:required={:.3}:allowed={MAX_TIME_STRETCH_TEMPO:.2}",
                entry.id, entry.tempo
            ));
        }
    }

    if apply_final_semantic_evidence(&mut entries, &semantic_evidence, &mut diagnostics) {
        review_required = true;
    }
    entries.sort_by_key(|entry| (entry.start_ms, entry.source_segments[0]));

    let manifest = RenderManifest::new(
        if review_required {
            crate::utterance::ManifestStatus::ReviewRequired
        } else {
            crate::utterance::ManifestStatus::Aligned
        },
        crate::utterance::TimingPipelineId::SourceAligned,
        entries,
    )
    .with_semantic_alignment_support(true)
    .with_diagnostics(diagnostics)
    .with_source_timeline(timeline, layout)?
    .with_audio_cache_identities()?;
    let expected = segments
        .iter()
        .map(|segment| segment.index)
        .collect::<Vec<_>>();
    manifest
        .validate(&expected)
        .map_err(|error| anyhow!("review_required: Natural Voice manifest: {error:#}"))?;
    manifest
        .validate_source_timeline(timeline, layout)
        .map_err(|error| anyhow!("review_required: Natural Voice source alignment: {error:#}"))?;
    fs::write(manifest_path, serde_json::to_vec_pretty(&manifest)?).with_context(|| {
        format!(
            "Failed to write aligned render manifest {}",
            manifest_path.display()
        )
    })?;
    Ok(())
}

fn apply_final_semantic_evidence(
    entries: &mut [ScheduledAudio],
    evidence: &HashMap<String, SemanticEvidence>,
    diagnostics: &mut ManifestDiagnostics,
) -> bool {
    diagnostics.anchor_drifts_ms.clear();
    diagnostics
        .review_reasons
        .retain(|reason| !reason.contains(":final_semantic_anchor:"));
    let mut review_required = false;
    for entry in entries {
        let Some(evidence) = evidence.get(&entry.id) else {
            entry.alignment = EntryAlignment::default();
            continue;
        };
        if evidence.expected.is_empty() {
            entry.alignment = EntryAlignment::default();
            continue;
        }
        let (alignment, anchors) = rendered_anchor_evidence(
            &evidence.events,
            &evidence.expected,
            entry.start_ms,
            entry.tempo,
        );
        diagnostics
            .anchor_drifts_ms
            .extend(anchors.iter().map(|anchor| anchor.drift_ms));
        let duration_safe = entry.tempo <= MAX_TIME_STRETCH_TEMPO + f64::EPSILON
            && entry.rendered_audio_ms <= entry.end_ms.saturating_sub(entry.start_ms);
        let early_drift = alignment.drift_ms.is_some_and(|drift| drift < 0)
            && alignment.reason.as_deref() == Some("semantic_anchor_drift");
        entry.alignment = EntryAlignment {
            status: if alignment.accepted || (duration_safe && early_drift) {
                EntryAlignmentStatus::Aligned
            } else {
                review_required = true;
                diagnostics.review_reasons.push(format!(
                    "{}:final_semantic_anchor:{}",
                    entry.id,
                    alignment.reason.as_deref().unwrap_or("outside_tolerance")
                ));
                EntryAlignmentStatus::ReviewRequired
            },
            anchors,
        };
    }
    review_required
}

fn finalize_measured_schedule(
    layout: &crate::sync_timeline::SyncLayout,
    entries: &mut [ScheduledAudio],
    allocations: &mut HashMap<String, AllocatedBlock>,
    measured_anchors: &HashMap<String, Vec<MeasuredAnchor>>,
    mode: NaturalMode,
) -> Vec<String> {
    let mut failed_groups = Vec::new();
    for group in allocation_groups(layout) {
        let mut measured = HashMap::<String, u32>::new();
        for entry in entries.iter() {
            if let Some(block) = layout
                .blocks
                .iter()
                .find(|block| block.source_segments == entry.source_segments)
            {
                measured.insert(block.id.clone(), entry.measured_audio_ms);
            }
        }
        let complete = group
            .block_ids
            .iter()
            .all(|block_id| measured.contains_key(block_id));
        if !complete {
            failed_groups.push(group.id);
            continue;
        }
        // Duration feasibility is authoritative for a group. Display-boundary anchors are soft
        // objectives and must not turn an otherwise safe schedule into reviewRequired; only a
        // hard semantic anchor conflict remains a final allocation failure. If the 250 ms
        // preferred-start window cannot fit measured audio, spillback may still use the previous
        // block's unused tail.
        if let Ok(values) = allocate_measured_group(layout, &group.id, &measured, mode) {
            for value in values {
                allocations.insert(value.block_id.clone(), value);
            }
        }
        match allocate_measured_group_with_anchors(
            layout,
            &group.id,
            &measured,
            measured_anchors,
            mode,
        ) {
            Ok(values) => {
                for value in values {
                    allocations.insert(value.block_id.clone(), value);
                }
            }
            Err(_) => {}
        }
        apply_measured_spillback_except(
            allocations,
            &measured,
            layout,
            &hard_anchored_block_ids(measured_anchors),
        );
        cover_hard_anchor_windows(allocations, &measured, layout, measured_anchors);
        for entry in entries.iter_mut() {
            let Some(block) = layout
                .blocks
                .iter()
                .find(|block| block.source_segments == entry.source_segments)
            else {
                continue;
            };
            let Some(allocation) = allocations.get(&block.id) else {
                continue;
            };
            entry.start_ms = allocation.render_start_ms;
            entry.end_ms = allocation.slot_end_ms;
            let (tempo, rendered) = fit_tempo_to_slot(
                entry.measured_audio_ms,
                entry.end_ms.saturating_sub(entry.start_ms).max(1),
            );
            entry.tempo = tempo;
            entry.rendered_audio_ms = rendered;
        }
        if group_exceeds_tempo_cap(&group, allocations, &measured) {
            failed_groups.push(group.id);
        }
    }
    failed_groups
}

fn hard_anchored_block_ids(
    measured_anchors: &HashMap<String, Vec<MeasuredAnchor>>,
) -> HashSet<String> {
    measured_anchors
        .iter()
        .filter(|(_, anchors)| {
            anchors
                .iter()
                .any(|anchor| anchor.coupling == Coupling::Anchored)
        })
        .map(|(block_id, _)| block_id.clone())
        .collect()
}

fn fit_tempo_to_slot(measured_audio_ms: u32, slot_ms: u32) -> (f64, u32) {
    let slot_ms = slot_ms.max(1);
    let tempo = (f64::from(measured_audio_ms) / f64::from(slot_ms)).max(1.0);
    (tempo, measured_audio_ms.min(slot_ms))
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct RevisionCache {
    entries: HashMap<String, Vec<SpeechPlan>>,
}

fn revision_cache_key(
    requests: &[SpeechPlanRevisionRequest],
    revision_number: u8,
) -> Result<String> {
    let material = requests
        .iter()
        .map(|request| {
            serde_json::json!({
                "plan": request.plan,
                "actualDurationMs": request.actual_duration_ms,
                "targetDurationMs": request.target_duration_ms,
                "allowedStyles": request.allowed_styles,
            })
        })
        .collect::<Vec<_>>();
    Ok(tts_cache_key(
        "gemini-whole-revision-v3",
        &revision_number.to_string(),
        "",
        &serde_json::to_string(&material)?,
    ))
}

async fn generate_timed_utterance(
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    output: &Path,
    ssml: &str,
    voice_name: &str,
    plans: &[SpeechPlan],
    bookmarks: &[String],
    pause_fit: bool,
) -> Result<(Vec<TimingEvent>, u32, PathBuf, Option<u32>)> {
    let cache_material = format!("source-timeline-v{SOURCE_TIMELINE_VERSION}\n{ssml}");
    let cache_key = tts_cache_key(UTTERANCE_CACHE_PROVIDER, voice_name, "", &cache_material);
    let sidecar = timing_sidecar_path(output);
    let events = if is_fresh_audio_file(ffmpeg, output, &cache_key)
        && let Ok(cached) = fs::read(&sidecar)
        && let Ok(cached) = serde_json::from_slice::<TimingSidecar>(&cached)
        && cached.cache_key == cache_key
        && cached.bookmarks == bookmarks
    {
        record_azure_cache_hit();
        cached.events
    } else {
        // Do not let a failed new synthesis accidentally reuse audio from another voice or SSML.
        let _ = fs::remove_file(output);
        let _ = fs::remove_file(&sidecar);
        crate::pause_fit::remove_fitted_artifacts(output);
        let timed = synthesize_ssml_with_timing(config, ssml).await?;
        let tmp = temporary_path(output);
        fs::write(&tmp, &timed.audio)
            .with_context(|| format!("Failed to write {}", tmp.display()))?;
        if !ffmpeg.is_valid_audio(&tmp) {
            let _ = fs::remove_file(&tmp);
            bail!("Azure timing synthesis returned invalid audio");
        }
        if output.exists() {
            fs::remove_file(output)
                .with_context(|| format!("Failed to replace {}", output.display()))?;
        }
        fs::rename(&tmp, output)
            .with_context(|| format!("Failed to promote {}", output.display()))?;
        write_tts_cache_key(output, &cache_key)?;
        fs::write(
            &sidecar,
            serde_json::to_vec_pretty(&TimingSidecar {
                cache_key,
                bookmarks: bookmarks.to_vec(),
                events: timed.events.clone(),
                plan_count: plans.len(),
            })?,
        )?;
        timed.events
    };
    let speech_end_hint_ms = bookmarks.last().and_then(|bookmark| {
        events.iter().find_map(|event| {
            (event.kind == TimingEventKind::Bookmark
                && event.mark.as_deref() == Some(bookmark.as_str()))
            .then(|| u32::try_from(event.offset_ms).ok())
            .flatten()
        })
    });
    let stats = fit_audio_file(ffmpeg, output, speech_end_hint_ms, pause_fit)?;
    Ok((
        events,
        stats.measured_audio_ms,
        stats.path,
        stats.speech_end_ms,
    ))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct TimingSidecar {
    cache_key: String,
    bookmarks: Vec<String>,
    events: Vec<TimingEvent>,
    plan_count: usize,
}

fn timing_sidecar_path(output: &Path) -> PathBuf {
    let mut path = output.as_os_str().to_owned();
    path.push(".timing.json");
    PathBuf::from(path)
}

fn alignment_repair_rate(alignment: &AlignmentResult, utterance: &Utterance) -> i8 {
    let Some(drift_ms) = alignment.drift_ms else {
        return 0;
    };
    let duration_ms = utterance.end_ms.saturating_sub(utterance.start_ms).max(1);
    let raw = ((drift_ms as f64 / f64::from(duration_ms)) * 100.0).round() as i8;
    raw.clamp(-3, 5)
}

fn duration_repair_rate(measured_audio_ms: u32, slot_ms: u32) -> Option<i8> {
    let required_tempo = f64::from(measured_audio_ms) / f64::from(slot_ms.max(1));
    if required_tempo <= MAX_TIME_STRETCH_TEMPO + f64::EPSILON {
        return None;
    }
    let rate = ((required_tempo / MAX_TIME_STRETCH_TEMPO - 1.0) * 100.0).ceil() as i8;
    (1..=MAX_DURATION_RATE_REPAIR_PERCENT)
        .contains(&rate)
        .then_some(rate)
}

fn semantic_bookmarks_are_complete(
    events: &[TimingEvent],
    expected: &[(String, u32, i64)],
) -> bool {
    expected.iter().all(|(name, _, _)| {
        events
            .iter()
            .filter(|event| {
                event.kind == TimingEventKind::Bookmark && event.mark.as_deref() == Some(name)
            })
            .count()
            == 1
    })
}

/// Return one bounded delay per early semantic anchor.  The first TTS pass supplies the only
/// provider measurement; Rust converts that measurement into deterministic SSML for the single
/// allowed repair.  Keep the total small so an anchor cannot turn a natural pause into a second
/// narration track.
fn semantic_anchor_delay_repairs(
    events: &[TimingEvent],
    expected: &[(String, u32, i64)],
    render_start_ms: u32,
    tempo: f64,
) -> Vec<(String, u16)> {
    let mut candidates = expected
        .iter()
        .filter_map(|(name, source_ms, tolerance)| {
            let event = events.iter().find(|event| {
                event.kind == TimingEventKind::Bookmark && event.mark.as_deref() == Some(name)
            })?;
            let rendered = u32::try_from(
                u64::from(render_start_ms)
                    .saturating_add((event.offset_ms as f64 / tempo.max(1.0)).round() as u64),
            )
            .unwrap_or(u32::MAX);
            let drift = i64::from(rendered) - i64::from(*source_ms);
            if drift >= -*tolerance {
                return None;
            }
            let delay = u16::try_from((-drift).clamp(120, 700)).unwrap_or(700);
            Some((name.clone(), delay, drift.abs()))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.0.cmp(&right.0)));
    let mut remaining = 800_u16;
    let mut repairs = Vec::new();
    for (name, delay, _) in candidates {
        if remaining == 0 {
            break;
        }
        let delay = delay.min(remaining);
        repairs.push((name, delay));
        remaining = remaining.saturating_sub(delay);
    }
    repairs
}

#[cfg(test)]
fn duration_target(
    segments: &[SrtSegment],
    position: usize,
    mode: NaturalMode,
) -> Result<DurationTarget> {
    let segment = &segments[position];
    let slot_ms = (((time_to_seconds(&segment.end)? - time_to_seconds(&segment.start)?) * 1_000.0)
        .round() as u32)
        .saturating_sub(SEGMENT_HEADROOM_MS);
    let following_silence_ms = segments
        .get(position + 1)
        .map(|next| -> Result<u32> {
            Ok(
                ((time_to_seconds(&next.start)? - time_to_seconds(&segment.end)?).max(0.0)
                    * 1_000.0)
                    .round() as u32,
            )
        })
        .transpose()?
        .unwrap_or(0);
    Ok(DurationTarget {
        slot_ms,
        following_silence_ms,
        spillback_ms: 0,
        mode,
    })
}

fn duration_ms(ffmpeg: &FfmpegPaths, path: &Path) -> Result<u32> {
    let duration = ffmpeg.audio_duration(path)?;
    if duration <= 0.0 {
        bail!("Generated audio has no duration: {}", path.display());
    }
    Ok((duration * 1_000.0).round() as u32)
}

struct SegmentResult {
    index: usize,
    fallback: bool,
    attempt_errors: Vec<String>,
    path: PathBuf,
    measured_audio_ms: u32,
    speech_end_ms: Option<u32>,
}

struct AudioFit {
    path: PathBuf,
    measured_audio_ms: u32,
    speech_end_ms: Option<u32>,
}

fn fit_audio_file(
    ffmpeg: &FfmpegPaths,
    input: &Path,
    speech_end_hint_ms: Option<u32>,
    pause_fit: bool,
) -> Result<AudioFit> {
    let mode = if pause_fit {
        crate::pause_fit::PauseFitMode::TailOnly
    } else {
        crate::pause_fit::PauseFitMode::Off
    };
    match crate::pause_fit::fit_tts_file(ffmpeg, input, speech_end_hint_ms, mode) {
        Ok(stats) => Ok(AudioFit {
            path: stats.path,
            measured_audio_ms: stats.measured_audio_ms,
            speech_end_ms: Some(stats.speech_end_ms),
        }),
        Err(error) if error.to_string().contains("silencedetect") => Ok(AudioFit {
            path: input.to_owned(),
            measured_audio_ms: duration_ms(ffmpeg, input)?,
            speech_end_ms: speech_end_hint_ms,
        }),
        Err(error) => Err(error),
    }
}

fn hd_utterance_cache_key(voice: &AzureVoice, ssml: &str) -> String {
    tts_cache_key(
        HD_UTTERANCE_CACHE_PROVIDER,
        &voice.short_name,
        "",
        &format!("source-timeline-v{SOURCE_TIMELINE_VERSION}\n{ssml}"),
    )
}

fn strip_ssml_breaks(ssml: &str) -> String {
    static BREAK_TAG: OnceLock<Regex> = OnceLock::new();
    BREAK_TAG
        .get_or_init(|| Regex::new(r#"<break\b[^>]*/>"#).expect("valid SSML break regex"))
        .replace_all(ssml, "")
        .into_owned()
}

async fn synthesize_continuous_audio(
    client: &Client,
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    output: &Path,
    ssml: &str,
    voice: &AzureVoice,
    pause_fit: bool,
) -> Result<AudioFit> {
    let stripped = strip_ssml_breaks(ssml);
    for candidate in [ssml, stripped.as_str()] {
        let cache_key = hd_utterance_cache_key(voice, candidate);
        if is_fresh_audio_file(ffmpeg, output, &cache_key) {
            record_azure_cache_hit();
            return fit_audio_file(ffmpeg, output, None, pause_fit);
        }
    }

    record_azure_logical_generation();
    remove_audio_artifacts(output);
    let tmp = temporary_path(output);
    let first_key = hd_utterance_cache_key(voice, ssml);
    match synthesize_ssml_once(client, config, ffmpeg, ssml, &tmp, output, &first_key).await {
        Ok(()) => fit_audio_file(ffmpeg, output, None, pause_fit),
        Err(first_error) if first_error.fallback_allowed && stripped != ssml => {
            remove_audio_artifacts(output);
            let stripped_key = hd_utterance_cache_key(voice, &stripped);
            match synthesize_ssml_once(
                client,
                config,
                ffmpeg,
                &stripped,
                &tmp,
                output,
                &stripped_key,
            )
            .await
            {
                Ok(()) => fit_audio_file(ffmpeg, output, None, pause_fit),
                Err(second_error) => bail!(
                    "DragonHD continuous synthesis failed after break-stripped retry: {:#}; {:#}",
                    first_error.error,
                    second_error.error
                ),
            }
        }
        Err(error) => bail!("DragonHD continuous synthesis failed: {:#}", error.error),
    }
}

async fn generate_segment(
    client: &Client,
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    output_dir: &Path,
    segment: SrtSegment,
) -> Result<SegmentResult> {
    let output = segment_path(output_dir, segment.index);
    let text = prepare_tts_text(&segment.text);
    let cache_key = tts_cache_key("azure", &config.voice, &config.style, &text);
    if is_fresh_audio_file(ffmpeg, &output, &cache_key) {
        record_azure_cache_hit();
        return Ok(SegmentResult {
            index: segment.index,
            fallback: false,
            attempt_errors: Vec::new(),
            path: output.clone(),
            measured_audio_ms: duration_ms(ffmpeg, &output)?,
            speech_end_ms: None,
        });
    }
    record_azure_logical_generation();

    let tmp = temporary_path(&output);
    let mut attempt_errors = Vec::new();
    let mut fallback_allowed = false;
    for attempt in 1..=RETRIES {
        let _ = fs::remove_file(&tmp);
        match synthesize_once(client, config, ffmpeg, &text, &tmp, &output, &cache_key).await {
            Ok(()) => {
                return Ok(SegmentResult {
                    index: segment.index,
                    fallback: false,
                    attempt_errors,
                    path: output.clone(),
                    measured_audio_ms: duration_ms(ffmpeg, &output)?,
                    speech_end_ms: None,
                });
            }
            Err(error) => {
                fallback_allowed = error.fallback_allowed;
                attempt_errors.push(format!(
                    "Azure error at segment {} (attempt {attempt}/{RETRIES}): {:#}",
                    segment.index, error.error
                ));
            }
        }
        let _ = fs::remove_file(&tmp);
        if attempt < RETRIES {
            sleep(Duration::from_secs(1)).await;
        }
    }

    if !fallback_allowed {
        bail!(
            "Azure TTS failed for segment {} after {RETRIES} attempts: {}",
            segment.index,
            attempt_errors
                .last()
                .map(String::as_str)
                .unwrap_or("unknown error")
        );
    }

    ffmpeg.create_silence(&output)?;
    if !ffmpeg.is_valid_audio(&output) {
        bail!("Silent fallback is invalid for segment {}", segment.index);
    }
    write_tts_cache_key(&output, &cache_key)?;
    Ok(SegmentResult {
        index: segment.index,
        fallback: true,
        attempt_errors,
        path: output.clone(),
        measured_audio_ms: duration_ms(ffmpeg, &output)?,
        speech_end_ms: None,
    })
}

async fn generate_plan_segment(
    client: &Client,
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    output_dir: &Path,
    plan: &SpeechPlan,
    voice: &AzureVoice,
    rate_percent: i8,
    pause_fit: bool,
) -> Result<SegmentResult> {
    let output = segment_path(output_dir, plan.index);
    generate_plan_segment_to_path(
        client,
        config,
        ffmpeg,
        &output,
        plan,
        voice,
        rate_percent,
        pause_fit,
    )
    .await
}

async fn generate_plan_segment_to_path(
    client: &Client,
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    output: &Path,
    plan: &SpeechPlan,
    voice: &AzureVoice,
    rate_percent: i8,
    pause_fit: bool,
) -> Result<SegmentResult> {
    let plan = prepared_speech_plan(plan);
    let ssml = render_speech_plan_ssml_with_rate(voice, &plan, rate_percent)?;
    let cache_key = speech_plan_cache_key_with_rate(voice, &plan, rate_percent)?;
    if is_fresh_audio_file(ffmpeg, &output, &cache_key) {
        record_azure_cache_hit();
        let stats = fit_audio_file(ffmpeg, output, None, pause_fit)?;
        return Ok(SegmentResult {
            index: plan.index,
            fallback: false,
            attempt_errors: Vec::new(),
            path: stats.path,
            measured_audio_ms: stats.measured_audio_ms,
            speech_end_ms: stats.speech_end_ms,
        });
    }
    record_azure_logical_generation();
    crate::pause_fit::remove_fitted_artifacts(output);

    let tmp = temporary_path(&output);
    let mut attempt_errors = Vec::new();
    let mut fallback_allowed = false;
    let mut synthesized = false;
    for attempt in 1..=RETRIES {
        let _ = fs::remove_file(&tmp);
        match synthesize_ssml_once(client, config, ffmpeg, &ssml, &tmp, &output, &cache_key).await {
            Ok(()) => {
                synthesized = true;
                break;
            }
            Err(error) => {
                fallback_allowed = error.fallback_allowed;
                attempt_errors.push(format!(
                    "Azure error at Natural Voice segment {} (attempt {attempt}/{RETRIES}): {:#}",
                    plan.index, error.error
                ));
            }
        }
        let _ = fs::remove_file(&tmp);
        if attempt < RETRIES {
            sleep(Duration::from_secs(1)).await;
        }
    }

    if !synthesized && !fallback_allowed {
        bail!(
            "Azure TTS failed for Natural Voice segment {} after {RETRIES} attempts: {}",
            plan.index,
            attempt_errors
                .last()
                .map(String::as_str)
                .unwrap_or("unknown error")
        );
    }
    if !synthesized {
        ffmpeg.create_silence(&output)?;
        if !ffmpeg.is_valid_audio(&output) {
            bail!(
                "Silent fallback is invalid for Natural Voice segment {}",
                plan.index
            );
        }
        write_tts_cache_key(&output, &cache_key)?;
    }
    let stats = fit_audio_file(ffmpeg, output, None, pause_fit)?;
    Ok(SegmentResult {
        index: plan.index,
        fallback: !synthesized,
        attempt_errors,
        path: stats.path,
        measured_audio_ms: stats.measured_audio_ms,
        speech_end_ms: stats.speech_end_ms,
    })
}

async fn select_revision_candidate(
    client: &Client,
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    output: &Path,
    current_plan: &SpeechPlan,
    current_duration_ms: u32,
    candidates: Vec<SpeechPlan>,
    voice: &AzureVoice,
    target: DurationTarget,
    pause_fit: bool,
) -> Result<(SpeechPlan, u32)> {
    let mut best: Option<(SpeechPlan, PathBuf, u32)> = None;

    for (position, candidate) in candidates.into_iter().enumerate() {
        let candidate_output = revision_candidate_path(output, position);
        remove_audio_artifacts(&candidate_output);
        let result = generate_plan_segment_to_path(
            client,
            config,
            ffmpeg,
            &candidate_output,
            &candidate,
            voice,
            0,
            pause_fit,
        )
        .await?;
        if result.fallback {
            remove_audio_artifacts(&candidate_output);
            continue;
        }

        let duration_ms = result.measured_audio_ms;
        if duration_ms >= current_duration_ms {
            remove_audio_artifacts(&candidate_output);
            continue;
        }
        if !matches!(
            solve_duration(target, duration_ms, true)?,
            DurationDecision::Reject { .. }
        ) {
            promote_audio_candidate(&candidate_output, output)?;
            return Ok((candidate, duration_ms));
        }
        if best
            .as_ref()
            .is_none_or(|(_, _, best_duration_ms)| duration_ms < *best_duration_ms)
        {
            if let Some((_, previous, _)) = best.take() {
                remove_audio_artifacts(&previous);
            }
            best = Some((candidate, candidate_output, duration_ms));
        } else {
            remove_audio_artifacts(&candidate_output);
        }
    }

    if let Some((candidate, candidate_output, duration_ms)) = best {
        promote_audio_candidate(&candidate_output, output)?;
        return Ok((candidate, duration_ms));
    }
    Ok((current_plan.clone(), current_duration_ms))
}

fn revision_candidate_path(output: &Path, position: usize) -> PathBuf {
    output.with_file_name(format!(
        "{}.revision-{position}.mp3",
        output
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("segment")
    ))
}

fn cache_key_path(output: &Path) -> PathBuf {
    let mut path = output.as_os_str().to_owned();
    path.push(".sha256");
    PathBuf::from(path)
}

fn remove_audio_artifacts(output: &Path) {
    let _ = fs::remove_file(output);
    let _ = fs::remove_file(cache_key_path(output));
    let _ = fs::remove_file(timing_sidecar_path(output));
    crate::pause_fit::remove_fitted_artifacts(output);
}

fn promote_audio_candidate(candidate_output: &Path, output: &Path) -> Result<()> {
    let candidate_cache = cache_key_path(candidate_output);
    if !candidate_output.is_file() || !candidate_cache.is_file() {
        bail!(
            "Incomplete audio candidate cannot replace {}",
            output.display()
        );
    }
    let backup = output.with_file_name(format!(
        "{}.previous",
        output
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("audio")
    ));
    remove_audio_artifacts(&backup);
    let output_cache = cache_key_path(output);
    let output_timing = timing_sidecar_path(output);
    let candidate_fitted = crate::pause_fit::fitted_audio_path(candidate_output);
    let output_fitted = crate::pause_fit::fitted_audio_path(output);
    let output_fitted_cache = cache_key_path(&output_fitted);
    if output.is_file() != output_cache.is_file() {
        bail!(
            "Existing audio/cache pair is incomplete: {}",
            output.display()
        );
    }
    crate::pause_fit::remove_fitted_artifacts(output);
    let backup_result = (|| -> Result<()> {
        if output.is_file() {
            fs::rename(output, &backup)?;
            fs::rename(&output_cache, cache_key_path(&backup))?;
        }
        if output_timing.is_file() {
            fs::rename(&output_timing, timing_sidecar_path(&backup))?;
        }
        Ok(())
    })();
    if let Err(error) = backup_result {
        if backup.is_file() {
            let _ = fs::rename(&backup, output);
        }
        if cache_key_path(&backup).is_file() {
            let _ = fs::rename(cache_key_path(&backup), &output_cache);
        }
        if timing_sidecar_path(&backup).is_file() {
            let _ = fs::rename(timing_sidecar_path(&backup), &output_timing);
        }
        return Err(error);
    }

    let candidate_timing = timing_sidecar_path(candidate_output);
    let promoted = (|| -> Result<()> {
        fs::rename(candidate_output, output)
            .with_context(|| format!("Failed to promote {}", candidate_output.display()))?;
        fs::rename(candidate_cache, &output_cache).with_context(|| {
            format!("Failed to promote cache identity for {}", output.display())
        })?;
        if candidate_timing.is_file() {
            fs::rename(&candidate_timing, &output_timing).with_context(|| {
                format!("Failed to promote timing metadata for {}", output.display())
            })?;
        }
        if candidate_fitted.is_file() {
            fs::rename(&candidate_fitted, &output_fitted).with_context(|| {
                format!("Failed to promote fitted audio for {}", output.display())
            })?;
            fs::rename(cache_key_path(&candidate_fitted), &output_fitted_cache).with_context(
                || format!("Failed to promote fitted cache for {}", output.display()),
            )?;
        }
        Ok(())
    })();
    if let Err(error) = promoted {
        remove_audio_artifacts(output);
        if backup.is_file() {
            let _ = fs::rename(&backup, output);
        }
        if cache_key_path(&backup).is_file() {
            let _ = fs::rename(cache_key_path(&backup), &output_cache);
        }
        if timing_sidecar_path(&backup).is_file() {
            let _ = fs::rename(timing_sidecar_path(&backup), &output_timing);
        }
        return Err(error);
    }
    remove_audio_artifacts(&backup);
    Ok(())
}

fn prepared_speech_plan(plan: &SpeechPlan) -> SpeechPlan {
    let mut prepared = plan.clone();
    prepared.display_text = prepare_tts_text(&prepared.display_text);
    prepared.spoken_text = prepare_tts_text(&prepared.spoken_text);
    for emphasis in &mut prepared.emphasis {
        emphasis.text = prepare_tts_text(&emphasis.text);
    }
    for pause in &mut prepared.pauses {
        pause.after = prepare_tts_text(&pause.after);
    }
    for anchor in &mut prepared.semantic_anchors {
        anchor.spoken_phrase = prepare_tts_text(&anchor.spoken_phrase);
    }
    prepared
}

async fn synthesize_once(
    client: &Client,
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    text: &str,
    tmp: &Path,
    output: &Path,
    cache_key: &str,
) -> std::result::Result<(), SynthesisError> {
    let ssml = build_ssml(&config.voice, &config.style, text);
    synthesize_ssml_once(client, config, ffmpeg, &ssml, tmp, output, cache_key).await
}

async fn synthesize_ssml_once(
    client: &Client,
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    ssml: &str,
    tmp: &Path,
    output: &Path,
    cache_key: &str,
) -> std::result::Result<(), SynthesisError> {
    crate::metrics::record_azure_request(ssml.chars().count());
    #[cfg(test)]
    if let Some(responder) = TEST_REST_RESPONDER
        .get()
        .and_then(|responder| responder.lock().ok())
        .and_then(|responder| responder.as_ref().map(|responder| responder(ssml)))
    {
        let bytes = responder.map_err(SynthesisError::fatal)?;
        fs::write(tmp, bytes)
            .with_context(|| format!("Failed to write {}", tmp.display()))
            .map_err(SynthesisError::fatal)?;
        if !ffmpeg.is_valid_audio(tmp) {
            return Err(SynthesisError {
                error: anyhow::anyhow!("Test Azure REST responder returned invalid audio"),
                fallback_allowed: true,
            });
        }
        if output.exists() {
            fs::remove_file(output)
                .with_context(|| format!("Failed to replace {}", output.display()))
                .map_err(SynthesisError::fatal)?;
        }
        fs::rename(tmp, output)
            .with_context(|| format!("Failed to replace {}", output.display()))
            .map_err(SynthesisError::fatal)?;
        write_tts_cache_key(output, cache_key).map_err(SynthesisError::fatal)?;
        return Ok(());
    }
    let url = format!(
        "https://{}.tts.speech.microsoft.com/cognitiveservices/v1",
        config.region
    );
    let response = client
        .post(url)
        .header("Ocp-Apim-Subscription-Key", &config.key)
        .header("Content-Type", "application/ssml+xml")
        .header(
            "X-Microsoft-OutputFormat",
            "audio-48khz-192kbitrate-mono-mp3",
        )
        .header("User-Agent", "dubora")
        .body(ssml.to_owned())
        .timeout(Duration::from_secs(70))
        .send()
        .await
        .map_err(|error| SynthesisError::fatal(error.into()))?;

    if response.status() != StatusCode::OK {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(300).collect();
        return Err(SynthesisError {
            error: anyhow::anyhow!("{status} {detail}"),
            fallback_allowed: status_allows_fallback(status),
        });
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|error| SynthesisError::fatal(error.into()))?;
    fs::write(tmp, bytes)
        .with_context(|| format!("Failed to write {}", tmp.display()))
        .map_err(SynthesisError::fatal)?;
    if !ffmpeg.is_valid_audio(tmp) {
        return Err(SynthesisError {
            error: anyhow::anyhow!("Azure returned invalid audio"),
            fallback_allowed: true,
        });
    }

    if output.exists() {
        fs::remove_file(output)
            .with_context(|| format!("Failed to replace {}", output.display()))
            .map_err(SynthesisError::fatal)?;
    }
    fs::rename(tmp, output)
        .with_context(|| format!("Failed to replace {}", output.display()))
        .map_err(SynthesisError::fatal)?;
    write_tts_cache_key(output, cache_key).map_err(SynthesisError::fatal)
}

fn status_allows_fallback(status: StatusCode) -> bool {
    status == StatusCode::BAD_REQUEST
}

struct SynthesisError {
    error: anyhow::Error,
    fallback_allowed: bool,
}

impl SynthesisError {
    fn fatal(error: anyhow::Error) -> Self {
        Self {
            error,
            fallback_allowed: false,
        }
    }
}

fn segment_path(output_dir: &Path, index: usize) -> PathBuf {
    output_dir.join(format!("seg_{index}.mp3"))
}

fn temporary_path(output: &Path) -> PathBuf {
    let mut path = output.as_os_str().to_owned();
    path.push(".tmp");
    PathBuf::from(path)
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub(crate) fn prepare_tts_text(text: &str) -> String {
    static IP_ADDRESS: OnceLock<Regex> = OnceLock::new();
    static TECHNICAL_TOKEN: OnceLock<Regex> = OnceLock::new();
    let with_spoken_ips = IP_ADDRESS
        .get_or_init(|| {
            Regex::new(r"(?i)(?:[0-9a-f.]*:){2,}[0-9a-f.]*|\b(?:\d{1,3}\.){3}\d{1,3}\b").unwrap()
        })
        .replace_all(text, |captures: &regex::Captures<'_>| {
            let ip = &captures[0];
            if ip.contains(':') {
                return ip
                    .parse::<Ipv6Addr>()
                    .map(|_| spell_ipv6(ip))
                    .unwrap_or_else(|_| ip.to_owned());
            }
            let octets: Vec<_> = ip.split('.').collect();
            if octets.iter().any(|octet| octet.parse::<u8>().is_err()) {
                return ip.to_owned();
            }
            octets
                .into_iter()
                .map(|octet| octet.chars().map(spell_digit).collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>()
                .join(" chấm ")
        })
        .into_owned();
    TECHNICAL_TOKEN
        .get_or_init(|| {
            Regex::new(
                r"https?://[A-Za-z0-9][A-Za-z0-9._~:/?@!$&'()*+=%#-]*[A-Za-z0-9/#]|/[A-Za-z0-9_.-]*[A-Za-z0-9_-](?:/[A-Za-z0-9_.-]*[A-Za-z0-9_-])+|\b[A-Za-z][A-Za-z0-9.-]*:\d+\b|\b[A-Za-z_][A-Za-z0-9_]*(?:[/:;,\x5C][A-Za-z0-9_./:,\x5C-]*[A-Za-z0-9_/-])+",
            )
            .unwrap()
        })
        .replace_all(&with_spoken_ips, |captures: &regex::Captures<'_>| {
            spell_technical_token(&captures[0])
        })
        .into_owned()
}

fn spell_technical_token(token: &str) -> String {
    token
        .replace("\\\\", " hai gạch chéo ngược ")
        .replace("//", " hai gạch chéo ")
        .replace('\\', " gạch chéo ngược ")
        .replace('/', " gạch chéo ")
        .replace(':', " hai chấm ")
        .replace(';', " chấm phẩy ")
        .replace(',', " dấu phẩy ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn spell_digit(digit: char) -> &'static str {
    match digit {
        '0' => "không",
        '1' => "một",
        '2' => "hai",
        '3' => "ba",
        '4' => "bốn",
        '5' => "năm",
        '6' => "sáu",
        '7' => "bảy",
        '8' => "tám",
        '9' => "chín",
        _ => unreachable!(),
    }
}

fn spell_ipv6(ip: &str) -> String {
    ip.chars()
        .map(|character| match character {
            '0'..='9' => spell_digit(character),
            'a' | 'A' => "a",
            'b' | 'B' => "bê",
            'c' | 'C' => "xê",
            'd' | 'D' => "đê",
            'e' | 'E' => "e",
            'f' | 'F' => "ép",
            ':' => "hai chấm",
            '.' => "chấm",
            _ => unreachable!(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn build_ssml(voice: &str, style: &str, text: &str) -> String {
    let text = escape_xml(text);
    let text = if style.is_empty() {
        text
    } else {
        format!(
            "<mstts:express-as style=\"{}\">{text}</mstts:express-as>",
            escape_xml(style)
        )
    };
    format!(
        "<speak version=\"1.0\" xmlns=\"http://www.w3.org/2001/10/synthesis\" xmlns:mstts=\"http://www.w3.org/2001/mstts\" xml:lang=\"vi-VN\"><voice name=\"{voice}\">{text}</voice></speak>",
    )
}

/// Render a validated Speech Plan using only SSML elements supported by this Azure voice.
pub fn render_speech_plan_ssml(voice: &AzureVoice, plan: &SpeechPlan) -> Result<String> {
    render_speech_plan_ssml_with_rate(voice, plan, 0)
}

/// Render a plan with one small timing correction requested by the duration solver.
pub fn render_speech_plan_ssml_with_rate(
    voice: &AzureVoice,
    plan: &SpeechPlan,
    rate_percent: i8,
) -> Result<String> {
    let body = render_speech_plan_body(voice, plan, rate_percent)?;
    Ok(wrap_voice_ssml(voice, &body))
}

/// Render one continuous utterance and place a bookmark after each source subtitle part.
pub fn render_utterance_ssml(
    voice: &AzureVoice,
    plans: &[SpeechPlan],
    bookmarks: &[String],
    rate_percent: i8,
) -> Result<String> {
    render_utterance_ssml_with_semantic_anchors(voice, plans, bookmarks, &[], rate_percent)
}

/// Render one unanchored continuous HD utterance without timing bookmarks or fake prosody.
pub fn render_unanchored_utterance_ssml(
    voice: &AzureVoice,
    plans: &[SpeechPlan],
) -> Result<String> {
    if plans.is_empty() {
        bail!("Unanchored utterances must contain at least one speech plan");
    }
    let mut body = String::new();
    for (position, plan) in plans.iter().enumerate() {
        if position > 0 {
            body.push_str(&part_join_separator(voice, plans.get(position - 1)));
        }
        body.push_str(&render_speech_plan_body(voice, plan, 0)?);
    }
    Ok(wrap_voice_ssml(voice, &body))
}

pub fn render_utterance_ssml_with_semantic_anchors(
    voice: &AzureVoice,
    plans: &[SpeechPlan],
    bookmarks: &[String],
    semantic_bookmarks: &[(usize, String, String)],
    rate_percent: i8,
) -> Result<String> {
    render_utterance_ssml_with_semantic_anchors_and_delays(
        voice,
        plans,
        bookmarks,
        semantic_bookmarks,
        &[],
        rate_percent,
    )
}

pub fn render_utterance_ssml_with_semantic_anchors_and_delays(
    voice: &AzureVoice,
    plans: &[SpeechPlan],
    bookmarks: &[String],
    semantic_bookmarks: &[(usize, String, String)],
    semantic_delays: &[(String, u16)],
    rate_percent: i8,
) -> Result<String> {
    if plans.is_empty() || plans.len() != bookmarks.len() {
        bail!("Utterance plans and timing bookmarks must have the same non-zero length");
    }
    let mut body = String::new();
    for (position, (plan, bookmark)) in plans.iter().zip(bookmarks).enumerate() {
        if bookmark.is_empty()
            || !bookmark
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            bail!("Utterance bookmark names must contain only letters, digits, and hyphens");
        }
        if position > 0 {
            body.push_str(&part_join_separator(voice, plans.get(position - 1)));
        }
        let anchors = semantic_bookmarks
            .iter()
            .filter(|(index, _, _)| *index == plan.index)
            .map(|(_, phrase, mark)| {
                let delay = semantic_delays
                    .iter()
                    .find(|(candidate, _)| candidate == mark)
                    .map_or(0, |(_, delay)| *delay);
                (phrase.clone(), mark.clone(), delay)
            })
            .collect::<Vec<_>>();
        body.push_str(&render_speech_plan_body_with_semantic_bookmarks_and_delays(
            voice,
            plan,
            &anchors,
            rate_percent,
        )?);
        body.push_str(&format!("<bookmark mark=\"{}\"/>", escape_xml(bookmark)));
    }
    Ok(wrap_voice_ssml(voice, &body))
}

fn render_speech_plan_body(
    voice: &AzureVoice,
    plan: &SpeechPlan,
    rate_percent: i8,
) -> Result<String> {
    render_speech_plan_body_with_semantic_bookmarks(voice, plan, &[], rate_percent)
}

fn render_speech_plan_body_with_semantic_bookmarks(
    voice: &AzureVoice,
    plan: &SpeechPlan,
    semantic_bookmarks: &[(String, String)],
    rate_percent: i8,
) -> Result<String> {
    let semantic_bookmarks = semantic_bookmarks
        .iter()
        .map(|(phrase, mark)| (phrase.clone(), mark.clone(), 0))
        .collect::<Vec<_>>();
    render_speech_plan_body_with_semantic_bookmarks_and_delays(
        voice,
        plan,
        &semantic_bookmarks,
        rate_percent,
    )
}

fn render_speech_plan_body_with_semantic_bookmarks_and_delays(
    voice: &AzureVoice,
    plan: &SpeechPlan,
    semantic_bookmarks: &[(String, String, u16)],
    rate_percent: i8,
) -> Result<String> {
    plan.validate(&voice.styles)?;
    if plan.style.is_some() && voice.capabilities.style != CapabilityState::Supported {
        bail!(
            "Azure voice {} does not support speaking styles",
            voice.short_name
        );
    }

    let supports_emphasis = voice.capabilities.manual_emphasis == CapabilityState::Supported;
    let supports_prosody = voice.capabilities.manual_prosody == CapabilityState::Supported;
    let supports_break = voice.capabilities.semantic_break == CapabilityState::Supported;
    if rate_percent != 0 && !supports_prosody {
        bail!(
            "Azure voice {} does not support manual rate adjustment",
            voice.short_name
        );
    }
    let mut boundaries = vec![0, plan.spoken_text.len()];
    let mut emphasis = Vec::new();
    let mut pauses = Vec::new();
    let mut pace = Vec::new();
    let mut semantic_marks = Vec::new();

    if supports_emphasis || supports_prosody {
        for item in &plan.emphasis {
            let (start, end) = phrase_span(&plan.spoken_text, &item.text)?;
            boundaries.extend([start, end]);
            emphasis.push((start, end, item.level));
        }
        if supports_prosody {
            for anchor in &plan.semantic_anchors {
                let Ok((start, end)) = phrase_span(&plan.spoken_text, &anchor.spoken_phrase) else {
                    continue;
                };
                if emphasis.iter().any(|(existing_start, existing_end, _)| {
                    start < *existing_end && end > *existing_start
                }) {
                    continue;
                }
                boundaries.extend([start, end]);
                emphasis.push((start, end, EmphasisLevel::Strong));
            }
        }
    }
    if supports_break {
        for pause in &plan.pauses {
            let (_, end) = phrase_span(&plan.spoken_text, &pause.after)?;
            boundaries.push(end);
            pauses.push((end, pause.duration_ms));
        }
        for (end, duration_ms) in implicit_clause_breaks(&plan.spoken_text) {
            if pauses
                .iter()
                .any(|(existing, _)| end.abs_diff(*existing) <= 2)
            {
                continue;
            }
            boundaries.push(end);
            pauses.push((end, duration_ms));
        }
        if pauses.len() < 2 && optional_breaks_fit(plan) {
            for (end, duration_ms) in optional_rhythm_breaks(&plan.spoken_text) {
                if pauses.len() >= 2 {
                    break;
                }
                if end < 12 || plan.spoken_text.len().saturating_sub(end) < 12 {
                    continue;
                }
                if pauses
                    .iter()
                    .any(|(existing, _)| end.abs_diff(*existing) <= 8)
                {
                    continue;
                }
                boundaries.push(end);
                pauses.push((end, duration_ms));
            }
        }
    }
    if supports_prosody && rate_percent == 0 {
        let forbidden: Vec<(usize, usize)> = emphasis
            .iter()
            .map(|(start, end, _)| (*start, *end))
            .collect();
        let splits = pace_split_offsets(&plan.spoken_text, &forbidden);
        if !splits.is_empty() {
            let mut start = 0;
            let mut parts = Vec::new();
            for offset in splits {
                parts.push((start, offset));
                start = offset;
            }
            parts.push((start, plan.spoken_text.len()));
            if !plan_is_tight(plan)
                && let Some(&(start, end)) = parts.last()
                && end.saturating_sub(start) >= 8
            {
                boundaries.extend([start, end]);
                pace.push((start, end, -4));
            }
        }
    }
    for (phrase, mark, delay) in semantic_bookmarks {
        if mark.is_empty()
            || !mark
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            bail!("Semantic bookmark names must contain only letters, digits, and hyphens");
        }
        let (start, end) = phrase_span(&plan.spoken_text, phrase)?;
        boundaries.extend([start, end]);
        // The source anchor represents the onset of the action phrase, so bookmark its first
        // spoken character rather than its end. This keeps "click Run" aligned to the click
        // explanation, not to the sentence tail that follows it.
        semantic_marks.push((start, mark, *delay));
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut body = String::new();
    let mut previous = 0;
    for boundary in boundaries {
        body.push_str(&escape_xml(&plan.spoken_text[previous..boundary]));
        for (_, _, level) in emphasis.iter().filter(|(_, end, _)| *end == boundary) {
            if supports_emphasis {
                body.push_str("</emphasis>");
            }
            if supports_prosody && emphasis_prosody(*level).is_some() {
                body.push_str("</prosody>");
            }
        }
        for (_, duration_ms) in pauses.iter().filter(|(end, _)| *end == boundary) {
            body.push_str(&format!("<break time=\"{duration_ms}ms\"/>"));
        }
        for (_, mark, delay) in semantic_marks.iter().filter(|(end, _, _)| *end == boundary) {
            if *delay > 0 && supports_break {
                body.push_str(&format!("<break time=\"{}ms\"/>", delay));
            }
            body.push_str(&format!("<bookmark mark=\"{}\"/>", escape_xml(mark)));
        }
        for _ in pace.iter().filter(|(_, end, _)| *end == boundary) {
            body.push_str("</prosody>");
        }
        for (_, _, rate) in pace.iter().filter(|(start, _, _)| *start == boundary) {
            body.push_str(&format!("<prosody rate=\"{rate:+}%\">"));
        }
        for (_, _, level) in emphasis.iter().filter(|(start, _, _)| *start == boundary) {
            if supports_prosody && let Some(prosody) = emphasis_prosody(*level) {
                body.push_str(prosody);
            }
            if supports_emphasis {
                body.push_str(&format!("<emphasis level=\"{}\">", emphasis_level(*level)));
            }
        }
        previous = boundary;
    }

    if let Some(style) = plan.style.as_deref() {
        body = format!(
            "<mstts:express-as style=\"{}\">{body}</mstts:express-as>",
            escape_xml(style)
        );
    }
    if rate_percent != 0 {
        body = format!("<prosody rate=\"{rate_percent:+}%\">{body}</prosody>");
    }
    Ok(body)
}

fn wrap_voice_ssml(voice: &AzureVoice, body: &str) -> String {
    let name = escape_xml(&voice.short_name);
    let voice_open = match voice.capabilities.model_family {
        VoiceModelFamily::DragonHd => {
            format!("<voice name=\"{name}\" parameters=\"enhancePronunciation=true\">")
        }
        _ => format!("<voice name=\"{name}\">"),
    };
    format!(
        "<speak version=\"1.0\" xmlns=\"http://www.w3.org/2001/10/synthesis\" xmlns:mstts=\"http://www.w3.org/2001/mstts\" xml:lang=\"vi-VN\">{voice_open}{body}</voice></speak>",
    )
}

fn part_join_separator(voice: &AzureVoice, previous: Option<&SpeechPlan>) -> String {
    if voice.capabilities.semantic_break != CapabilityState::Supported {
        return " ".to_owned();
    }
    let ms = match previous.map(|plan| plan.spoken_text.trim_end()) {
        Some(text) if text.ends_with(',') => 90,
        Some(text)
            if text.ends_with('.')
                || text.ends_with('!')
                || text.ends_with('?')
                || text.ends_with('…') =>
        {
            170
        }
        _ => 140,
    };
    format!("<break time=\"{ms}ms\"/>")
}

fn implicit_clause_breaks(text: &str) -> Vec<(usize, u16)> {
    const SENTENCE_MS: u16 = 180;
    const DASH_MS: u16 = 150;
    const MAX_IMPLICIT: usize = 2;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut breaks = Vec::new();
    for (index, &(offset, ch)) in chars.iter().enumerate() {
        let previous = index
            .checked_sub(1)
            .and_then(|previous| chars.get(previous).map(|(_, character)| *character));
        let next = chars.get(index + 1).map(|(_, character)| *character);
        let end = offset + ch.len_utf8();
        if matches!(ch, '—' | '–') {
            breaks.push((end, DASH_MS));
            continue;
        }
        let sentence_end = matches!(ch, '.' | '!' | '?' | '…');
        if !sentence_end {
            continue;
        }
        if next.is_none() {
            continue;
        }
        if ch == '.'
            && previous.is_some_and(|character| character.is_ascii_digit())
            && next.is_some_and(|character| character.is_ascii_digit())
        {
            continue;
        }
        if previous.is_some_and(|character| {
            character.is_alphanumeric() || matches!(character, '"' | '\'' | ')' | ']')
        }) && next.is_some_and(char::is_whitespace)
        {
            breaks.push((end, SENTENCE_MS));
        }
        if breaks.len() == MAX_IMPLICIT {
            break;
        }
    }
    breaks.truncate(MAX_IMPLICIT);
    breaks
}

fn optional_breaks_fit(plan: &SpeechPlan) -> bool {
    let chars = plan
        .spoken_text
        .chars()
        .filter(|character| !character.is_whitespace())
        .count() as u32;
    let likely_ms = chars.saturating_mul(80);
    likely_ms.saturating_add(250) < plan.target_duration_ms
}

fn optional_rhythm_breaks(text: &str) -> Vec<(usize, u16)> {
    let mut breaks = comma_or_semicolon_breaks(text);
    for offset in connector_split_offsets(text) {
        if breaks
            .iter()
            .any(|(existing, _)| offset.abs_diff(*existing) <= 8)
        {
            continue;
        }
        breaks.push((offset, 90));
    }
    breaks.sort_by_key(|(offset, _)| *offset);
    breaks
}

fn comma_or_semicolon_breaks(text: &str) -> Vec<(usize, u16)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut breaks = Vec::new();
    for (index, &(offset, ch)) in chars.iter().enumerate() {
        if !matches!(ch, ',' | ';') {
            continue;
        }
        let previous = index
            .checked_sub(1)
            .and_then(|previous| chars.get(previous).map(|(_, character)| *character));
        let next = chars.get(index + 1).map(|(_, character)| *character);
        if previous.is_some_and(|character| character.is_ascii_digit())
            && next.is_some_and(|character| character.is_ascii_digit())
        {
            continue;
        }
        if next.is_some_and(char::is_whitespace) {
            breaks.push((offset + ch.len_utf8(), 110));
        }
    }
    breaks
}

fn connector_split_offsets(text: &str) -> Vec<usize> {
    const CONNECTORS: [&str; 5] = [" và ", " rồi ", " thì ", " nên ", " nhưng "];
    let lowered = text.to_lowercase();
    let mut offsets = Vec::new();
    for connector in CONNECTORS {
        let mut search = 0;
        while let Some(found) = lowered[search..].find(connector) {
            let start = search + found;
            if start >= 12 && start + connector.len() + 12 <= text.len() {
                offsets.push(start);
            }
            search = start + connector.len();
        }
    }
    offsets.sort_unstable();
    offsets.dedup();
    offsets
}

fn pace_split_offsets(text: &str, forbidden: &[(usize, usize)]) -> Vec<usize> {
    let mut candidates: Vec<(u8, usize)> = implicit_clause_breaks(text)
        .into_iter()
        .map(|(offset, _)| (0, offset))
        .chain(
            comma_or_semicolon_breaks(text)
                .into_iter()
                .map(|(offset, _)| (1, offset)),
        )
        .chain(
            connector_split_offsets(text)
                .into_iter()
                .map(|offset| (2, offset)),
        )
        .collect();
    candidates.sort_by_key(|(priority, offset)| (*offset, *priority));
    let mut selected = Vec::new();
    for (_, offset) in candidates {
        if offset < 12 || text.len().saturating_sub(offset) < 12 {
            continue;
        }
        if forbidden
            .iter()
            .any(|(start, end)| offset > *start && offset < *end)
        {
            continue;
        }
        if selected
            .iter()
            .any(|existing: &usize| offset.abs_diff(*existing) < 12)
        {
            continue;
        }
        selected.push(offset);
        if selected.len() == 2 {
            break;
        }
    }
    if selected.is_empty()
        && let Some(mid) = midpoint_space(text, forbidden)
    {
        selected.push(mid);
    }
    selected
}

fn midpoint_space(text: &str, forbidden: &[(usize, usize)]) -> Option<usize> {
    if !forbidden.is_empty() {
        return None;
    }
    if text.len() < 28 {
        return None;
    }
    let target = text.len() * 3 / 5;
    let mut best: Option<(usize, usize)> = None;
    for (offset, character) in text.char_indices() {
        if character != ' ' || offset < 12 || text.len().saturating_sub(offset) < 12 {
            continue;
        }
        let distance = offset.abs_diff(target);
        if best.is_none_or(|(_, best_distance)| distance < best_distance) {
            best = Some((offset, distance));
        }
    }
    best.map(|(offset, _)| offset)
}

fn plan_is_tight(plan: &SpeechPlan) -> bool {
    let chars = plan
        .spoken_text
        .chars()
        .filter(|character| !character.is_whitespace())
        .count() as u32;
    let likely_ms = chars.saturating_mul(75);
    likely_ms.saturating_add(400) >= plan.target_duration_ms
}

/// Cache identity for a plan includes the exact capability-aware SSML payload.
pub fn speech_plan_cache_key(voice: &AzureVoice, plan: &SpeechPlan) -> Result<String> {
    speech_plan_cache_key_with_rate(voice, plan, 0)
}

fn speech_plan_cache_key_with_rate(
    voice: &AzureVoice,
    plan: &SpeechPlan,
    rate_percent: i8,
) -> Result<String> {
    Ok(tts_cache_key(
        SPEECH_PLAN_CACHE_PROVIDER,
        &voice.short_name,
        "",
        &render_speech_plan_ssml_with_rate(voice, plan, rate_percent)?,
    ))
}

fn phrase_span(text: &str, phrase: &str) -> Result<(usize, usize)> {
    let start = unique_phrase_position(text, phrase).ok_or_else(|| {
        anyhow::anyhow!("Speech plan phrase is absent or not unique in spoken text")
    })?;
    Ok((start, start + phrase.len()))
}

fn emphasis_level(level: EmphasisLevel) -> &'static str {
    match level {
        EmphasisLevel::Light => "moderate",
        EmphasisLevel::Clear | EmphasisLevel::Strong => "strong",
    }
}

fn emphasis_prosody(level: EmphasisLevel) -> Option<&'static str> {
    match level {
        EmphasisLevel::Light => None,
        EmphasisLevel::Clear => Some("<prosody rate=\"-4%\" pitch=\"+3%\" volume=\"+2%\">"),
        EmphasisLevel::Strong => Some("<prosody rate=\"-7%\" pitch=\"+5%\" volume=\"+4%\">"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure_voices::{VoiceCapabilities, VoiceModelFamily};
    use crate::speech_plan::{Delivery, Emphasis, Pause, PauseReason, SemanticAnchor};

    fn voice(capabilities: VoiceCapabilities) -> AzureVoice {
        AzureVoice {
            short_name: "vi-VN-HoaiMyNeural".to_owned(),
            display_name: "Hoai My".to_owned(),
            local_name: "Hoài My".to_owned(),
            gender: "Female".to_owned(),
            locale: "vi-VN".to_owned(),
            locale_name: "Vietnamese".to_owned(),
            status: "GA".to_owned(),
            voice_type: "Neural".to_owned(),
            styles: vec!["serious".to_owned()],
            secondary_locales: Vec::new(),
            words_per_minute: Some(150),
            capabilities,
        }
    }

    fn standard_capabilities() -> VoiceCapabilities {
        VoiceCapabilities {
            model_family: VoiceModelFamily::Standard,
            multilingual: false,
            timing_events: CapabilityState::Supported,
            style: CapabilityState::Supported,
            manual_emphasis: CapabilityState::Supported,
            manual_prosody: CapabilityState::Supported,
            semantic_break: CapabilityState::Supported,
            pronunciation: CapabilityState::Supported,
            automatic_delivery: CapabilityState::Unknown,
        }
    }

    fn plan() -> SpeechPlan {
        SpeechPlan {
            index: 1,
            display_text: "Spring Boot rất quan trọng.".to_owned(),
            spoken_text: "Spring Boot rất quan trọng.".to_owned(),
            target_duration_ms: 2_000,
            delivery: Delivery::Serious,
            emphasis: vec![Emphasis {
                text: "rất quan trọng".to_owned(),
                level: EmphasisLevel::Strong,
            }],
            pauses: vec![Pause {
                after: "Spring Boot".to_owned(),
                duration_ms: 180,
                reason: PauseReason::Transition,
            }],
            style: Some("serious".to_owned()),
            semantic_anchors: Vec::new(),
            semantic_anchor_invalid: false,
        }
    }

    #[test]
    fn escapes_xml_without_changing_pronunciation() {
        assert_eq!(
            escape_xml("API & <tag> \"quote\" 'apostrophe'"),
            "API &amp; &lt;tag&gt; &quot;quote&quot; &apos;apostrophe&apos;"
        );
    }

    #[test]
    fn ssml_matches_azure_contract() {
        let ssml = build_ssml("voice", "", "API & SQL");
        assert!(ssml.contains("xml:lang=\"vi-VN\""));
        assert!(ssml.contains("<voice name=\"voice\">"));
        assert!(ssml.contains("<voice name=\"voice\">API &amp; SQL</voice>"));
        assert!(!ssml.contains("<prosody"));
    }

    #[test]
    fn renders_one_bookmark_after_each_utterance_part() {
        let first = plan();
        let mut second = plan();
        second.index = 2;
        second.spoken_text = "và phần tiếp theo.".to_owned();
        second.display_text = second.spoken_text.clone();
        second.emphasis.clear();
        second.pauses.clear();
        second.style = None;
        let ssml = render_utterance_ssml(
            &voice(standard_capabilities()),
            &[first, second],
            &["dubora-u1-s1-end".to_owned(), "dubora-u1-s2-end".to_owned()],
            0,
        )
        .unwrap();
        assert_eq!(ssml.matches("<bookmark mark=").count(), 2);
        assert!(ssml.contains("<bookmark mark=\"dubora-u1-s1-end\"/>"));
        assert!(ssml.contains("<bookmark mark=\"dubora-u1-s2-end\"/>"));
    }

    #[test]
    fn renders_semantic_bookmark_inside_the_exact_target_phrase() {
        let mut first = plan();
        first.semantic_anchors = vec![SemanticAnchor {
            anchor_id: "p-1-action-1".to_owned(),
            spoken_phrase: "Spring Boot".to_owned(),
        }];
        let ssml = render_utterance_ssml_with_semantic_anchors(
            &voice(standard_capabilities()),
            &[first],
            &["dubora-u1-s1-end".to_owned()],
            &[(
                1,
                "Spring Boot".to_owned(),
                "dubora-u1-ap-1-action-1".to_owned(),
            )],
            0,
        )
        .unwrap();
        let phrase = ssml.find("Spring Boot").unwrap();
        let bookmark = ssml
            .find("bookmark mark=\"dubora-u1-ap-1-action-1\"")
            .unwrap();
        assert!(bookmark < phrase);
    }

    #[test]
    fn renders_one_bounded_delay_before_an_early_semantic_anchor() {
        let mut first = plan();
        first.semantic_anchors = vec![SemanticAnchor {
            anchor_id: "p-1-action-1".to_owned(),
            spoken_phrase: "Spring Boot".to_owned(),
        }];
        let ssml = render_utterance_ssml_with_semantic_anchors_and_delays(
            &voice(standard_capabilities()),
            &[first],
            &["dubora-u1-s1-end".to_owned()],
            &[(
                1,
                "Spring Boot".to_owned(),
                "dubora-u1-ap-1-action-1".to_owned(),
            )],
            &[("dubora-u1-ap-1-action-1".to_owned(), 240)],
            0,
        )
        .unwrap();
        let delay = ssml.find("<break time=\"240ms\"/>").unwrap();
        let bookmark = ssml
            .find("bookmark mark=\"dubora-u1-ap-1-action-1\"")
            .unwrap();
        let phrase = ssml.find("Spring Boot").unwrap();
        assert!(delay < bookmark && bookmark < phrase);
    }

    #[test]
    fn phrase_span_does_not_confuse_a_short_phrase_with_a_word_prefix() {
        let (start, end) =
            phrase_span("có kiểu t, nhưng kiểu trả về vẫn giữ nguyên", "kiểu t").unwrap();
        assert_eq!(
            &"có kiểu t, nhưng kiểu trả về vẫn giữ nguyên"[start..end],
            "kiểu t"
        );
    }

    #[test]
    fn style_and_ip_addresses_are_sent_as_spoken_text() {
        assert_eq!(
            prepare_tts_text("Mở 192.168.0.1 nhé."),
            "Mở một chín hai chấm một sáu tám chấm không chấm một nhé."
        );
        assert_eq!(prepare_tts_text("999.1.1.1"), "999.1.1.1");
        assert_eq!(
            prepare_tts_text("Mở [2001:db8::1] nhé."),
            "Mở [hai không không một hai chấm đê bê tám hai chấm hai chấm một] nhé."
        );
        assert_eq!(
            prepare_tts_text("::ffff:192.0.2.128"),
            "hai chấm hai chấm ép ép ép ép hai chấm một chín hai chấm không chấm hai chấm một hai tám"
        );
        assert_eq!(prepare_tts_text("1:2:3"), "1:2:3");
        let ssml = build_ssml("voice", "chat", "Xin chào");
        assert!(ssml.contains("<mstts:express-as style=\"chat\">Xin chào</mstts:express-as>"));
    }

    #[test]
    fn speaks_symbols_only_inside_technical_tokens() {
        assert_eq!(
            prepare_tts_text("Mở /var/www/html."),
            "Mở gạch chéo var gạch chéo www gạch chéo html."
        );
        assert_eq!(
            prepare_tts_text(r"C:\app\logs;localhost:8080"),
            "C hai chấm gạch chéo ngược app gạch chéo ngược logs chấm phẩy localhost hai chấm 8080"
        );
        assert_eq!(
            prepare_tts_text("https://example.com/docs, rồi lưu."),
            "https hai chấm hai gạch chéo example.com gạch chéo docs, rồi lưu."
        );
        assert_eq!(
            prepare_tts_text("Lưu ý: hãy lưu lại, nhé."),
            "Lưu ý: hãy lưu lại, nhé."
        );
    }

    #[test]
    fn prepared_plan_keeps_validation_consistent_after_ip_expansion() {
        let mut plan = SpeechPlan::plain(1, "Mở 192.168.0.1 nhé.", 2_000);
        plan.spoken_text = "Mở 192.168.0.1 nhé.".to_owned();

        let prepared = prepared_speech_plan(&plan);

        prepared.validate(&[]).unwrap();
    }

    #[test]
    fn prepared_plan_keeps_emphasis_after_technical_token_expansion() {
        let mut plan = SpeechPlan::plain(1, "Mở /var/www/html nhé.", 2_000);
        plan.spoken_text = "Mở /var/www/html nhé.".to_owned();
        plan.emphasis = vec![Emphasis {
            text: "/var/www/html".to_owned(),
            level: EmphasisLevel::Strong,
        }];

        let prepared = prepared_speech_plan(&plan);

        prepared.validate(&[]).unwrap();
        assert_eq!(
            prepared.emphasis[0].text,
            "gạch chéo var gạch chéo www gạch chéo html"
        );
    }

    #[test]
    fn renders_only_supported_speech_plan_controls() {
        let ssml = render_speech_plan_ssml(&voice(standard_capabilities()), &plan()).unwrap();
        assert!(ssml.contains("<mstts:express-as style=\"serious\">"));
        assert!(ssml.contains("<break time=\"180ms\"/>"));
        assert!(ssml.contains("<emphasis level=\"strong\">rất quan trọng</emphasis>"));
        assert!(ssml.contains("<prosody rate=\"-7%\" pitch=\"+5%\" volume=\"+4%\">"));

        let mut hd = standard_capabilities();
        hd.model_family = VoiceModelFamily::DragonHd;
        hd.style = CapabilityState::Unsupported;
        hd.manual_emphasis = CapabilityState::Unsupported;
        hd.manual_prosody = CapabilityState::Unsupported;
        let mut hd_voice = voice(hd);
        hd_voice.styles.clear();
        let mut hd_plan = plan();
        hd_plan.style = None;
        let ssml = render_speech_plan_ssml(&hd_voice, &hd_plan).unwrap();
        assert!(ssml.contains("<break time=\"180ms\"/>"));
        assert!(!ssml.contains("<emphasis"));
        assert!(!ssml.contains("<prosody"));
    }

    #[test]
    fn renders_hd_continuous_utterances_without_fake_timing_controls() {
        let mut capabilities = standard_capabilities();
        capabilities.model_family = VoiceModelFamily::DragonHd;
        capabilities.manual_emphasis = CapabilityState::Unsupported;
        capabilities.manual_prosody = CapabilityState::Unsupported;
        let mut hd_voice = voice(capabilities);
        hd_voice.short_name = "de-DE-Florian:DragonHDLatestNeural".to_owned();
        hd_voice.styles.clear();
        let mut first = plan();
        first.style = None;
        let ssml = render_unanchored_utterance_ssml(&hd_voice, &[first]).unwrap();
        assert!(ssml.contains("<break time=\"180ms\"/>"));
        assert!(ssml.contains("parameters=\"enhancePronunciation=true\""));
        assert!(!ssml.contains("<bookmark"));
        assert!(!ssml.contains("<emphasis"));
        assert!(!ssml.contains("<prosody"));
    }

    #[test]
    fn inserts_clause_breaks_and_neural_action_prosody_without_hd_pitch() {
        let mut neural = SpeechPlan::plain(1, "Chạy lệnh đi. Rồi nhấn Enter để xong.", 4_000);
        neural.semantic_anchors = vec![SemanticAnchor {
            anchor_id: "p-1-action-1".to_owned(),
            spoken_phrase: "nhấn Enter".to_owned(),
        }];
        let ssml = render_speech_plan_ssml(&voice(standard_capabilities()), &neural).unwrap();
        assert!(ssml.contains("<break time=\"180ms\"/>"));
        assert!(ssml.contains(
            "<prosody rate=\"-7%\" pitch=\"+5%\" volume=\"+4%\"><emphasis level=\"strong\">nhấn Enter</emphasis></prosody>"
        ));

        let mut hd = standard_capabilities();
        hd.model_family = VoiceModelFamily::DragonHd;
        hd.manual_emphasis = CapabilityState::Unsupported;
        hd.manual_prosody = CapabilityState::Unsupported;
        let mut hd_voice = voice(hd);
        hd_voice.short_name = "de-DE-Florian:DragonHDLatestNeural".to_owned();
        let hd_ssml = render_unanchored_utterance_ssml(&hd_voice, &[neural.clone()]).unwrap();
        assert!(hd_ssml.contains("<break time=\"180ms\"/>"));
        assert!(!hd_ssml.contains("<prosody"));
        assert!(!hd_ssml.contains("<emphasis"));
    }

    #[test]
    fn neural_pace_slows_the_landing_clause_without_hd_rate() {
        let plan = SpeechPlan::plain(
            1,
            "Mình sẽ đăng nhập vào đó, và nếu các bạn muốn làm theo.",
            5_000,
        );
        let ssml = render_speech_plan_ssml(&voice(standard_capabilities()), &plan).unwrap();
        assert!(ssml.contains("<prosody rate=\"-4%\">"));
        assert!(!ssml.contains("<prosody rate=\"+4%\">"));
        assert!(ssml.contains("<break time=\"110ms\"/>"));

        let mut hd = standard_capabilities();
        hd.model_family = VoiceModelFamily::DragonHd;
        hd.manual_emphasis = CapabilityState::Unsupported;
        hd.manual_prosody = CapabilityState::Unsupported;
        let mut hd_voice = voice(hd);
        hd_voice.short_name = "de-DE-Florian:DragonHDLatestNeural".to_owned();
        let hd_ssml = render_unanchored_utterance_ssml(&hd_voice, &[plan]).unwrap();
        assert!(hd_ssml.contains("<break time=\"110ms\"/>"));
        assert!(!hd_ssml.contains("<prosody"));
    }

    #[test]
    fn duration_repair_rate_skips_local_pace_wraps() {
        let plan = SpeechPlan::plain(
            1,
            "Mình sẽ đăng nhập vào đó, và nếu các bạn muốn làm theo.",
            5_000,
        );
        let ssml =
            render_speech_plan_ssml_with_rate(&voice(standard_capabilities()), &plan, 8).unwrap();
        assert!(ssml.contains("<prosody rate=\"+8%\">"));
        assert!(!ssml.contains("<prosody rate=\"+4%\">"));
        assert!(!ssml.contains("<prosody rate=\"-4%\">"));
    }

    #[test]
    fn tight_slots_do_not_slow_the_landing_clause() {
        let plan = SpeechPlan::plain(
            1,
            "Mình sẽ đăng nhập vào đó, và nếu các bạn muốn làm theo.",
            2_000,
        );
        let ssml = render_speech_plan_ssml(&voice(standard_capabilities()), &plan).unwrap();
        assert!(!ssml.contains("<prosody rate=\"+4%\">"));
        assert!(!ssml.contains("<prosody rate=\"-4%\">"));
    }

    #[test]
    fn comma_inside_an_ip_address_is_not_a_rhythm_break() {
        let plan = SpeechPlan::plain(1, "Mở 192.168.0.1 rồi xong.", 2_000);
        let ssml = render_speech_plan_ssml(&voice(standard_capabilities()), &plan).unwrap();
        assert!(!ssml.contains("<break time=\"110ms\"/>"));
    }

    #[test]
    fn joins_unanchored_parts_with_a_supported_break() {
        let mut capabilities = standard_capabilities();
        capabilities.model_family = VoiceModelFamily::DragonHd;
        capabilities.manual_prosody = CapabilityState::Unsupported;
        capabilities.manual_emphasis = CapabilityState::Unsupported;
        let mut hd_voice = voice(capabilities);
        hd_voice.short_name = "de-DE-Florian:DragonHDLatestNeural".to_owned();
        let first = SpeechPlan::plain(1, "Mở cửa sổ ẩn danh", 2_000);
        let second = SpeechPlan::plain(2, "rồi nhấn Enter.", 2_000);
        let ssml = render_unanchored_utterance_ssml(&hd_voice, &[first, second]).unwrap();
        assert!(ssml.contains("<break time=\"140ms\"/>"));
    }

    #[test]
    fn compacted_hd_revision_text_changes_the_utterance_cache_payload() {
        let mut capabilities = standard_capabilities();
        capabilities.model_family = VoiceModelFamily::DragonHd;
        capabilities.manual_emphasis = CapabilityState::Unsupported;
        capabilities.manual_prosody = CapabilityState::Unsupported;
        let mut hd_voice = voice(capabilities);
        hd_voice.short_name = "de-DE-Florian:DragonHDLatestNeural".to_owned();
        hd_voice.styles.clear();

        let mut compacted =
            SpeechPlan::plain(23, "mà chúng ta sẽ tìm hiểu trong video hôm nay.", 4_000);
        compacted.compact_optional_framing();
        let compacted_ssml =
            render_unanchored_utterance_ssml(&hd_voice, &[prepared_speech_plan(&compacted)])
                .unwrap();
        let raw_ssml = render_unanchored_utterance_ssml(
            &hd_voice,
            &[prepared_speech_plan(&SpeechPlan::plain(
                23,
                "mà chúng ta sẽ tìm hiểu trong video hôm nay.",
                4_000,
            ))],
        )
        .unwrap();
        assert!(!compacted_ssml.contains("trong video hôm nay"));
        assert_ne!(
            hd_utterance_cache_key(&hd_voice, &raw_ssml),
            hd_utterance_cache_key(&hd_voice, &compacted_ssml)
        );
    }

    #[test]
    fn strips_only_supported_break_tags_for_hd_retry() {
        let ssml = "<speak><break time=\"180ms\"/>text &amp; more</speak>";
        assert_eq!(strip_ssml_breaks(ssml), "<speak>text &amp; more</speak>");
    }

    #[test]
    fn renders_duration_rate_only_for_voices_with_prosody() {
        let standard = voice(standard_capabilities());
        let ssml = render_speech_plan_ssml_with_rate(&standard, &plan(), 8).unwrap();
        assert!(ssml.contains("<prosody rate=\"+8%\">"));

        let mut hd = standard_capabilities();
        hd.manual_prosody = CapabilityState::Unsupported;
        assert!(render_speech_plan_ssml_with_rate(&voice(hd), &plan(), 8).is_err());
    }

    #[test]
    fn natural_duration_target_caps_the_following_silence_for_balanced_mode() {
        let segments = vec![
            SrtSegment {
                index: 1,
                start: "00:00:00,000".to_owned(),
                end: "00:00:02,000".to_owned(),
                text: "Một".to_owned(),
            },
            SrtSegment {
                index: 2,
                start: "00:00:03,000".to_owned(),
                end: "00:00:04,000".to_owned(),
                text: "Hai".to_owned(),
            },
        ];
        let target = duration_target(&segments, 0, NaturalMode::Balanced).unwrap();
        assert_eq!(target.slot_ms, 1_920);
        assert_eq!(target.following_silence_ms, 1_000);
        assert_eq!(target.effective_ms(), 2_170);
    }

    #[test]
    fn speech_plan_cache_key_tracks_rendered_controls() {
        let voice = voice(standard_capabilities());
        let key = speech_plan_cache_key(&voice, &plan()).unwrap();
        let mut changed = plan();
        changed.pauses[0].duration_ms = 220;
        assert_ne!(key, speech_plan_cache_key(&voice, &changed).unwrap());
    }

    #[test]
    fn duration_prediction_uses_spoken_words_and_voice_rate_not_the_srt_slot() {
        let voice = voice(standard_capabilities());
        let mut short = SpeechPlan::plain(1, "một hai", 500);
        let mut same_words_different_slot = short.clone();
        same_words_different_slot.target_duration_ms = 20_000;
        let long = SpeechPlan::plain(2, "một hai ba bốn năm sáu", 500);

        assert_eq!(
            predict_plan_audio_ms(&short, &voice),
            predict_plan_audio_ms(&same_words_different_slot, &voice)
        );
        assert!(predict_plan_audio_ms(&long, &voice) > predict_plan_audio_ms(&short, &voice));

        short.pauses.push(Pause {
            after: "một".to_owned(),
            duration_ms: 300,
            reason: PauseReason::Transition,
        });
        assert!(predict_plan_audio_ms(&short, &voice) > 800);
    }

    #[test]
    fn duration_prediction_uses_a_robust_voice_calibration_after_enough_samples() {
        let voice = voice(standard_capabilities());
        let plan = SpeechPlan::plain(1, "một hai ba bốn", 500);
        let calibration = DurationCalibration {
            version: DURATION_CALIBRATION_VERSION,
            voice: voice.short_name.clone(),
            style: voice.styles.join("|"),
            renderer: UTTERANCE_CACHE_PROVIDER.to_owned(),
            chars_per_second: vec![10.0, 11.0, 12.0, 13.0, 90.0],
        };
        assert_eq!(calibration.median_chars_per_second(), Some(12.0));
        assert!(
            predict_plan_audio_ms_with_calibration(
                &plan,
                &voice,
                calibration.median_chars_per_second()
            ) < predict_plan_audio_ms(&plan, &voice)
        );
    }

    #[test]
    fn duration_calibration_counts_a_continuous_utterance_as_one_sample() {
        let voice = voice(standard_capabilities());
        let mut calibration = DurationCalibration {
            version: DURATION_CALIBRATION_VERSION,
            voice: voice.short_name.clone(),
            style: voice.styles.join("|"),
            renderer: UTTERANCE_CACHE_PROVIDER.to_owned(),
            chars_per_second: Vec::new(),
        };
        let plans = vec![
            SpeechPlan::plain(1, "một hai", 500),
            SpeechPlan::plain(2, "ba bốn", 500),
        ];
        calibration.observe_plans(&plans, 1_000);
        assert_eq!(calibration.chars_per_second, vec![11.0]);
    }

    #[test]
    fn only_segment_specific_bad_requests_allow_silence() {
        assert!(status_allows_fallback(StatusCode::BAD_REQUEST));
        assert!(!status_allows_fallback(StatusCode::UNAUTHORIZED));
        assert!(!status_allows_fallback(StatusCode::TOO_MANY_REQUESTS));
        assert!(!status_allows_fallback(StatusCode::INTERNAL_SERVER_ERROR));
    }

    #[test]
    fn final_schedule_keeps_the_bookmark_derived_anchor_start() {
        let block_id = "block-1-1".to_owned();
        let layout = crate::sync_timeline::SyncLayout {
            blocks: vec![crate::sync_timeline::NarrationBlock {
                id: block_id.clone(),
                start_ms: 0,
                end_ms: 5_000,
                preferred_start_ms: 2_000,
                source_segments: vec![1],
                anchor_ids: vec!["p-1-action-1".to_owned()],
                island_id: "island-1".to_owned(),
            }],
            islands: vec![crate::sync_timeline::SyncIsland {
                id: "island-1".to_owned(),
                start_ms: 0,
                end_ms: 5_000,
                block_ids: vec![block_id.clone()],
                hard_boundary_before: false,
            }],
        };
        let mut entries = vec![ScheduledAudio {
            id: "utterance-1".to_owned(),
            path: PathBuf::from("utterance-1.mp3"),
            start_ms: 2_000,
            end_ms: 3_000,
            source_segments: vec![1],
            measured_audio_ms: 1_000,
            speech_end_ms: None,
            rendered_audio_ms: 1_000,
            tempo: 1.0,
            island_id: Some("island-1".to_owned()),
            cache_identity: "cache".to_owned(),
            alignment: EntryAlignment::default(),
        }];
        let mut allocations = HashMap::from([(
            block_id.clone(),
            AllocatedBlock {
                block_id: block_id.clone(),
                island_id: "island-1".to_owned(),
                render_start_ms: 2_000,
                slot_end_ms: 3_000,
                predicted_audio_ms: 1_000,
                rendered_audio_ms: 1_000,
                tempo: 1.0,
            },
        )]);
        let anchors = HashMap::from([(
            block_id,
            vec![MeasuredAnchor {
                source_ms: 1_500,
                raw_offset_ms: 1_000,
                tolerance_ms: MAX_ANCHOR_DRIFT_MS as u32,
                confidence: crate::sync_timeline::TimingConfidence::Granular,
                coupling: Coupling::Anchored,
            }],
        )]);

        let failed = finalize_measured_schedule(
            &layout,
            &mut entries,
            &mut allocations,
            &anchors,
            NaturalMode::Natural,
        );

        assert!(failed.is_empty());
        assert!(entries[0].start_ms.abs_diff(500) <= MAX_ANCHOR_DRIFT_MS as u32);
    }

    #[test]
    fn final_anchor_gate_uses_the_final_start_and_tempo() {
        let bookmark = "dubora-u1-ap-1-action-1".to_owned();
        let evidence = HashMap::from([(
            "utterance-1".to_owned(),
            SemanticEvidence {
                events: vec![TimingEvent {
                    kind: TimingEventKind::Bookmark,
                    offset_ms: 1_000,
                    duration_ms: 0,
                    mark: Some(bookmark.clone()),
                    text: None,
                }],
                expected: vec![(bookmark, 1_000, MAX_ANCHOR_DRIFT_MS)],
            },
        )]);
        let mut entries = vec![ScheduledAudio {
            id: "utterance-1".to_owned(),
            path: PathBuf::from("utterance-1.mp3"),
            start_ms: 0,
            end_ms: 1_000,
            source_segments: vec![1],
            measured_audio_ms: 1_000,
            speech_end_ms: None,
            rendered_audio_ms: 1_000,
            tempo: 1.0,
            island_id: Some("island-1".to_owned()),
            cache_identity: "cache".to_owned(),
            alignment: EntryAlignment::default(),
        }];
        let mut diagnostics = ManifestDiagnostics::default();

        assert!(!apply_final_semantic_evidence(
            &mut entries,
            &evidence,
            &mut diagnostics
        ));
        assert_eq!(entries[0].alignment.status, EntryAlignmentStatus::Aligned);
        assert_eq!(entries[0].alignment.anchors[0].rendered_ms, 1_000);

        entries[0].start_ms = 400;
        entries[0].tempo = 1.0;
        assert!(apply_final_semantic_evidence(
            &mut entries,
            &evidence,
            &mut diagnostics
        ));
        assert_eq!(
            entries[0].alignment.status,
            EntryAlignmentStatus::ReviewRequired
        );
        assert_eq!(entries[0].alignment.anchors[0].drift_ms, 400);
    }

    #[test]
    fn final_anchor_gate_muxes_duration_safe_early_tail_actions() {
        let bookmark = "dubora-u16-ap-26-action-1".to_owned();
        let evidence = HashMap::from([(
            "utterance-16".to_owned(),
            SemanticEvidence {
                events: vec![TimingEvent {
                    kind: TimingEventKind::Bookmark,
                    offset_ms: 4_000,
                    duration_ms: 0,
                    mark: Some(bookmark.clone()),
                    text: None,
                }],
                expected: vec![(bookmark, 127_818, MAX_ANCHOR_DRIFT_MS)],
            },
        )]);
        let mut entries = vec![ScheduledAudio {
            id: "utterance-16".to_owned(),
            path: PathBuf::from("utterance-16.mp3"),
            start_ms: 121_920,
            end_ms: 126_960,
            source_segments: vec![26],
            measured_audio_ms: 5_040,
            speech_end_ms: None,
            rendered_audio_ms: 5_040,
            tempo: 1.0,
            island_id: Some("island-3".to_owned()),
            cache_identity: "cache".to_owned(),
            alignment: EntryAlignment::default(),
        }];
        let mut diagnostics = ManifestDiagnostics::default();
        assert!(!apply_final_semantic_evidence(
            &mut entries,
            &evidence,
            &mut diagnostics
        ));
        assert_eq!(entries[0].alignment.status, EntryAlignmentStatus::Aligned);
        assert!(entries[0].alignment.anchors[0].drift_ms < 0);
    }

    #[test]
    fn final_anchor_gate_leaves_unanchored_entries_not_applicable() {
        let mut entries = vec![ScheduledAudio {
            id: "utterance-1".to_owned(),
            path: PathBuf::from("utterance-1.mp3"),
            start_ms: 0,
            end_ms: 1_000,
            source_segments: vec![1],
            measured_audio_ms: 1_000,
            speech_end_ms: None,
            rendered_audio_ms: 1_000,
            tempo: 1.0,
            island_id: Some("island-1".to_owned()),
            cache_identity: "cache".to_owned(),
            alignment: EntryAlignment::default(),
        }];
        let mut diagnostics = ManifestDiagnostics::default();

        assert!(!apply_final_semantic_evidence(
            &mut entries,
            &HashMap::new(),
            &mut diagnostics
        ));
        assert_eq!(
            entries[0].alignment.status,
            EntryAlignmentStatus::NotApplicable
        );
        assert!(entries[0].alignment.anchors.is_empty());
    }

    #[test]
    fn measured_outcome_selects_one_follow_up_branch() {
        let cases = [
            (
                true,
                false,
                false,
                false,
                false,
                false,
                AlignedFollowUp::Review,
            ),
            (
                false,
                false,
                false,
                false,
                false,
                false,
                AlignedFollowUp::Concise,
            ),
            (
                false,
                true,
                true,
                true,
                false,
                false,
                AlignedFollowUp::Repair,
            ),
            (
                false,
                true,
                true,
                true,
                true,
                false,
                AlignedFollowUp::Accept,
            ),
            (
                false,
                false,
                false,
                true,
                true,
                true,
                AlignedFollowUp::DurationRepair,
            ),
        ];
        for (
            structural,
            duration_feasible,
            technically_fit,
            semantic_repair_feasible,
            semantic_aligned,
            duration_rate_repair,
            expected,
        ) in cases
        {
            let decision = aligned_follow_up(
                structural,
                duration_feasible,
                technically_fit,
                semantic_repair_feasible,
                semantic_aligned,
                duration_rate_repair,
            );
            assert_eq!(decision, expected);
            let logical_generations = 1 + usize::from(matches!(
                decision,
                AlignedFollowUp::Concise
                    | AlignedFollowUp::DurationRepair
                    | AlignedFollowUp::Repair
            ));
            assert!(logical_generations <= 2);
        }
        assert_eq!(
            aligned_follow_up(false, false, true, false, false, false),
            AlignedFollowUp::Concise
        );
        assert_eq!(
            aligned_follow_up(false, false, true, true, false, false),
            AlignedFollowUp::Repair
        );
        assert_eq!(
            aligned_follow_up(false, false, false, true, true, true),
            AlignedFollowUp::DurationRepair
        );
        assert_eq!(duration_repair_rate(1_242, 1_000), Some(10));
        assert_eq!(duration_repair_rate(1_318, 1_000), Some(17));
        assert!(duration_repair_rate(1_400, 1_000).is_none());
    }

    #[test]
    fn measured_part_durations_assigns_trailing_audio_to_the_last_part() {
        let candidate = PendingUtterance {
            utterance: Utterance {
                id: 1,
                start_ms: 0,
                end_ms: 4_000,
                parts: vec![
                    crate::utterance::UtterancePart {
                        index: 1,
                        start_ms: 0,
                        end_ms: 2_000,
                        plan: SpeechPlan::plain(1, "Phần một.", 2_000),
                    },
                    crate::utterance::UtterancePart {
                        index: 2,
                        start_ms: 2_000,
                        end_ms: 4_000,
                        plan: SpeechPlan::plain(2, "Phần hai.", 2_000),
                    },
                ],
                block_id: "block-1".to_owned(),
                island_id: "island-1".to_owned(),
                anchor_ids: Vec::new(),
            },
            path: PathBuf::from("utterance-1.mp3"),
            measured_audio_ms: 4_000,
            speech_end_ms: None,
            events: vec![
                TimingEvent {
                    kind: TimingEventKind::Bookmark,
                    offset_ms: 1_000,
                    duration_ms: 0,
                    mark: Some("dubora-u1-s1-end".to_owned()),
                    text: None,
                },
                TimingEvent {
                    kind: TimingEventKind::Bookmark,
                    offset_ms: 3_000,
                    duration_ms: 0,
                    mark: Some("dubora-u1-s2-end".to_owned()),
                    text: None,
                },
            ],
            semantic_expected: Vec::new(),
            structural_issue: None,
        };

        let durations = measured_part_durations(&candidate);
        assert_eq!(durations.get(&1), Some(&1_000));
        assert_eq!(durations.get(&2), Some(&3_000));
        assert_eq!(durations.values().sum::<u32>(), candidate.measured_audio_ms);
    }

    #[test]
    fn provisional_failures_do_not_survive_the_final_gate() {
        for reason in [
            "island-1:measured_allocation_infeasible:base",
            "island-1:measured_anchor_allocation_infeasible:anchor",
            "utterance-1:concise_revision_unresolved",
            "utterance-1:duration_repair_unresolved",
            "utterance-1:semantic_repair_unavailable",
            "utterance-1:semantic_repair_unresolved",
        ] {
            assert!(is_provisional_review_reason(reason));
        }
        assert!(!is_provisional_review_reason(
            "utterance-1:timing_structure:bookmark_missing"
        ));
        assert!(!is_provisional_review_reason(
            "island-1:measured_allocation_infeasible_after_revision"
        ));
    }

    #[test]
    fn duration_fit_with_incompatible_anchors_selects_repair() {
        let block_id = "block-1-1".to_owned();
        let layout = SyncLayout {
            blocks: vec![crate::sync_timeline::NarrationBlock {
                id: block_id.clone(),
                start_ms: 0,
                end_ms: 5_000,
                preferred_start_ms: 1_000,
                source_segments: vec![1],
                anchor_ids: vec!["first".to_owned(), "second".to_owned()],
                island_id: "island-1".to_owned(),
            }],
            islands: vec![crate::sync_timeline::SyncIsland {
                id: "island-1".to_owned(),
                start_ms: 0,
                end_ms: 5_000,
                block_ids: vec![block_id.clone()],
                hard_boundary_before: false,
            }],
        };
        let measured = HashMap::from([(block_id.clone(), 1_000)]);
        let anchors = HashMap::from([(
            block_id,
            vec![
                MeasuredAnchor {
                    source_ms: 1_000,
                    raw_offset_ms: 0,
                    tolerance_ms: 250,
                    confidence: crate::sync_timeline::TimingConfidence::Granular,
                    coupling: Coupling::Anchored,
                },
                MeasuredAnchor {
                    source_ms: 4_000,
                    raw_offset_ms: 0,
                    tolerance_ms: 250,
                    confidence: crate::sync_timeline::TimingConfidence::Granular,
                    coupling: Coupling::Anchored,
                },
            ],
        )]);

        assert!(
            allocate_measured_group(&layout, "island-1", &measured, NaturalMode::Natural).is_ok()
        );
        assert!(
            allocate_measured_group_with_anchors(
                &layout,
                "island-1",
                &measured,
                &anchors,
                NaturalMode::Natural,
            )
            .is_err()
        );
        assert_eq!(
            aligned_follow_up(false, true, true, true, false, false),
            AlignedFollowUp::Repair
        );
    }

    #[test]
    fn promotes_audio_cache_and_timing_metadata_together() {
        let directory =
            std::env::temp_dir().join(format!("dubora-fallback-candidate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let output = directory.join("segment.mp3");
        let candidate_path = directory.join("segment.revision.mp3");
        fs::write(&output, b"old-audio").unwrap();
        fs::write(cache_key_path(&output), b"old-cache").unwrap();
        fs::write(timing_sidecar_path(&output), b"old-timing").unwrap();
        fs::write(&candidate_path, b"new-audio").unwrap();
        fs::write(cache_key_path(&candidate_path), b"new-cache").unwrap();
        fs::write(timing_sidecar_path(&candidate_path), b"new-timing").unwrap();
        promote_audio_candidate(&candidate_path, &output).unwrap();

        assert_eq!(fs::read(&output).unwrap(), b"new-audio");
        assert_eq!(fs::read(cache_key_path(&output)).unwrap(), b"new-cache");
        assert_eq!(
            fs::read(timing_sidecar_path(&output)).unwrap(),
            b"new-timing"
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
