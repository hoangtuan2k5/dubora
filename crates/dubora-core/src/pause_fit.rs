use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::ffmpeg::FfmpegPaths;
use crate::tts_cache::{
    is_fresh_audio_file, read_tts_cache_key, tts_cache_key, write_tts_cache_key,
};

pub const PAUSE_FIT_VERSION: u8 = 1;
pub const INTERNAL_NOISE_DB: f64 = -40.0;
pub const TAIL_NOISE_DB: f64 = -45.0;
pub const DETECT_MIN_MS: u32 = 80;
pub const PAUSE_KEEP_FLOOR_MS: u32 = 100;
pub const LEAD_KEEP_MS: u32 = 40;
pub const TAIL_KEEP_MS: u32 = 40;
pub const CUT_FADE_MS: u32 = 5;
pub const MIN_SPEECH_ISLAND_MS: u32 = 30;
pub const FITTED_ENCODER: &str = "libmp3lame-q2";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PauseFitMode {
    Off,
    TailOnly,
    WithInternal,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct PauseFitProfile {
    pub version: u8,
    pub internal_noise_db: f64,
    pub tail_noise_db: f64,
    pub detect_min_ms: u32,
    pub pause_keep_floor_ms: u32,
    pub lead_keep_ms: u32,
    pub tail_keep_ms: u32,
    pub cut_fade_ms: u32,
    pub min_speech_island_ms: u32,
    pub encoder: &'static str,
}

impl Default for PauseFitProfile {
    fn default() -> Self {
        Self {
            version: PAUSE_FIT_VERSION,
            internal_noise_db: INTERNAL_NOISE_DB,
            tail_noise_db: TAIL_NOISE_DB,
            detect_min_ms: DETECT_MIN_MS,
            pause_keep_floor_ms: PAUSE_KEEP_FLOOR_MS,
            lead_keep_ms: LEAD_KEEP_MS,
            tail_keep_ms: TAIL_KEEP_MS,
            cut_fade_ms: CUT_FADE_MS,
            min_speech_island_ms: MIN_SPEECH_ISLAND_MS,
            encoder: FITTED_ENCODER,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SilenceInterval {
    pub start_ms: u32,
    pub end_ms: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PauseFitPlan {
    pub keep: Vec<(u32, u32)>,
    pub recovered_ms: u32,
    pub tail_recovered_ms: u32,
    pub internal_recovered_ms: u32,
    pub lead_recovered_ms: u32,
    pub speech_end_ms: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PauseFitStats {
    pub path: PathBuf,
    pub recovered_ms: u32,
    pub tail_recovered_ms: u32,
    pub lead_recovered_ms: u32,
    pub internal_recovered_ms: u32,
    pub speech_end_ms: u32,
    pub measured_audio_ms: u32,
}

pub fn parse_silencedetect(stderr: &str, duration_ms: u32) -> Result<Vec<SilenceInterval>> {
    let mut intervals = Vec::new();
    let mut open_start = None;

    for line in stderr.lines() {
        if let Some(value) = line.split_once("silence_start:").map(|(_, value)| value) {
            if open_start.is_some() {
                bail!("silencedetect output starts a second silence before ending the first");
            }
            open_start = Some(parse_seconds(value)?);
            continue;
        }

        if let Some(value) = line.split_once("silence_end:").map(|(_, value)| value) {
            let start = open_start.take().ok_or_else(|| {
                anyhow::anyhow!("silencedetect output ends silence before starting it")
            })?;
            let end = parse_seconds(value)?;
            intervals.push(seconds_to_interval(start, end, duration_ms)?);
        }
    }

    if let Some(start) = open_start {
        intervals.push(seconds_to_interval(
            start,
            duration_ms as f64 / 1000.0,
            duration_ms,
        )?);
    }

    Ok(merge_intervals(intervals))
}

pub fn plan_pause_fit(
    duration_ms: u32,
    intervals: &[SilenceInterval],
    speech_end_hint_ms: Option<u32>,
    mode: PauseFitMode,
    profile: PauseFitProfile,
) -> Result<PauseFitPlan> {
    if matches!(mode, PauseFitMode::WithInternal) && speech_end_hint_ms.is_some() {
        bail!("internal pause fitting cannot remap bookmark timing");
    }

    let silences = classify_silences(duration_ms, intervals, profile)?;
    let speech_end_ms =
        speech_end_hint_ms.unwrap_or_else(|| speech_end_from_silences(duration_ms, &silences));

    if duration_ms == 0 || matches!(mode, PauseFitMode::Off) {
        return Ok(PauseFitPlan {
            keep: non_empty_keep_windows(vec![(0, duration_ms)], duration_ms),
            recovered_ms: 0,
            tail_recovered_ms: 0,
            internal_recovered_ms: 0,
            lead_recovered_ms: 0,
            speech_end_ms,
        });
    }

    let mut drops = Vec::new();
    let mut lead_recovered_ms = 0;
    let mut tail_recovered_ms = 0;
    let mut internal_recovered_ms = 0;

    if let Some(hint) = speech_end_hint_ms {
        let hint = hint.min(duration_ms);
        let keep_end = hint.saturating_add(profile.tail_keep_ms).min(duration_ms);
        if keep_end < duration_ms {
            drops.push((keep_end, duration_ms));
            tail_recovered_ms = duration_ms - keep_end;
        }
    } else {
        for silence in &silences {
            let length = silence.end_ms - silence.start_ms;
            let leading = silence.start_ms <= 1;
            let trailing = silence.end_ms.saturating_add(1) >= duration_ms;

            if leading {
                let keep = profile.lead_keep_ms.min(length);
                let drop_end = silence.end_ms - keep;
                if drop_end > silence.start_ms {
                    drops.push((silence.start_ms, drop_end));
                    lead_recovered_ms += drop_end - silence.start_ms;
                }
            }

            if trailing && !leading {
                let keep = profile.tail_keep_ms.min(length);
                let drop_start = silence.start_ms + keep;
                if drop_start < silence.end_ms {
                    drops.push((drop_start, silence.end_ms));
                    tail_recovered_ms += silence.end_ms - drop_start;
                }
            }

            if !leading
                && !trailing
                && matches!(mode, PauseFitMode::WithInternal)
                && length > profile.pause_keep_floor_ms
            {
                let keep = profile.pause_keep_floor_ms;
                let left = (length - keep) / 2;
                let keep_start = silence.start_ms + left;
                let keep_end = keep_start + keep;
                if keep_start > silence.start_ms {
                    drops.push((silence.start_ms, keep_start));
                }
                if keep_end < silence.end_ms {
                    drops.push((keep_end, silence.end_ms));
                }
                internal_recovered_ms += length - keep;
            }
        }
    }

    let drops = merge_intervals(
        drops
            .into_iter()
            .map(|(start_ms, end_ms)| SilenceInterval { start_ms, end_ms })
            .collect(),
    );
    let keep = complement_windows(duration_ms, &drops);
    let recovered_ms = duration_ms.saturating_sub(keep.iter().map(|(s, e)| e - s).sum());
    let fitted_speech_end = fitted_speech_end_ms(&keep, speech_end_ms);

    // Every removable window comes from detected silence. A bookmark hint is the
    // only deliberate exception: samples after the last bookmark are exempt.
    if speech_end_hint_ms.is_none() && overlaps_speech(&drops, &silences, duration_ms) {
        return Ok(PauseFitPlan {
            keep: vec![(0, duration_ms)],
            recovered_ms: 0,
            tail_recovered_ms: 0,
            internal_recovered_ms: 0,
            lead_recovered_ms: 0,
            speech_end_ms: fitted_speech_end_ms(&[(0, duration_ms)], speech_end_ms),
        });
    }

    Ok(PauseFitPlan {
        keep,
        recovered_ms,
        tail_recovered_ms,
        internal_recovered_ms,
        lead_recovered_ms,
        speech_end_ms: fitted_speech_end,
    })
}

pub fn fitted_audio_path(input: &Path) -> PathBuf {
    input.with_file_name(format!(
        "{}.fitted{}",
        input
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("audio"),
        input.extension().map_or_else(String::new, |extension| {
            format!(".{}", extension.to_string_lossy())
        })
    ))
}

pub(crate) fn fitted_sidecar_path(input: &Path) -> PathBuf {
    fitted_audio_path(input).with_extension("json")
}

pub fn fitted_cache_key(
    raw_cache_key: &str,
    mode: PauseFitMode,
    speech_end_hint_ms: Option<u32>,
    profile: PauseFitProfile,
) -> String {
    let mode = match mode {
        PauseFitMode::Off => "off",
        PauseFitMode::TailOnly => "tail-only",
        PauseFitMode::WithInternal => "with-internal",
    };
    let material = format!(
        "version={};mode={mode};noise={};tail={};detect={};floor={};lead={};tailKeep={};cutFade={};minSpeechIsland={};encoder={};hint={speech_end_hint_ms:?}",
        profile.version,
        profile.internal_noise_db,
        profile.tail_noise_db,
        profile.detect_min_ms,
        profile.pause_keep_floor_ms,
        profile.lead_keep_ms,
        profile.tail_keep_ms,
        profile.cut_fade_ms,
        profile.min_speech_island_ms,
        profile.encoder,
    );
    tts_cache_key("pause-fit-v1", raw_cache_key, "", &material)
}

pub fn fit_tts_file(
    ffmpeg: &FfmpegPaths,
    input: &Path,
    speech_end_hint_ms: Option<u32>,
    mode: PauseFitMode,
) -> Result<PauseFitStats> {
    let duration_ms = audio_duration_ms(ffmpeg, input)?;
    if matches!(mode, PauseFitMode::Off) && speech_end_hint_ms.is_some() {
        remove_fitted_artifacts(input);
        return Ok(PauseFitStats {
            path: input.to_owned(),
            recovered_ms: 0,
            tail_recovered_ms: 0,
            lead_recovered_ms: 0,
            internal_recovered_ms: 0,
            speech_end_ms: speech_end_hint_ms.unwrap().min(duration_ms),
            measured_audio_ms: duration_ms,
        });
    }

    let profile = PauseFitProfile::default();
    let intervals = detect_silences(ffmpeg, input, duration_ms, profile)?;
    let plan = plan_pause_fit(duration_ms, &intervals, speech_end_hint_ms, mode, profile)?;
    if plan.recovered_ms == 0 || matches!(mode, PauseFitMode::Off) {
        remove_fitted_artifacts(input);
        return Ok(PauseFitStats {
            path: input.to_owned(),
            recovered_ms: 0,
            tail_recovered_ms: 0,
            lead_recovered_ms: 0,
            internal_recovered_ms: 0,
            speech_end_ms: plan.speech_end_ms,
            measured_audio_ms: duration_ms,
        });
    }

    let raw_cache_key = read_tts_cache_key(input)?;
    let fitted = fitted_audio_path(input);
    let cache_key = fitted_cache_key(&raw_cache_key, mode, speech_end_hint_ms, profile);
    if !is_fresh_audio_file(ffmpeg, &fitted, &cache_key) {
        let temporary = temporary_fitted_path(&fitted);
        let _ = fs::remove_file(&temporary);
        let _ = fs::remove_file(crate::tts_cache::tts_cache_key_path(&temporary));
        apply_pause_fit(ffmpeg, input, &temporary, &plan)?;
        if fitted.exists() {
            fs::remove_file(&fitted)
                .with_context(|| format!("Failed to replace {}", fitted.display()))?;
        }
        fs::rename(&temporary, &fitted)
            .with_context(|| format!("Failed to promote {}", fitted.display()))?;
        write_tts_cache_key(&fitted, &cache_key)?;
    }
    write_fitted_sidecar(&fitted, input, &plan, mode, profile)?;
    let measured_audio_ms = audio_duration_ms(ffmpeg, &fitted)?;
    Ok(PauseFitStats {
        path: fitted,
        recovered_ms: plan.recovered_ms,
        tail_recovered_ms: plan.tail_recovered_ms,
        lead_recovered_ms: plan.lead_recovered_ms,
        internal_recovered_ms: plan.internal_recovered_ms,
        speech_end_ms: plan.speech_end_ms,
        measured_audio_ms,
    })
}

pub fn detect_speech_end_ms(
    ffmpeg: &FfmpegPaths,
    input: &Path,
    speech_end_hint_ms: Option<u32>,
) -> Result<Option<u32>> {
    let duration_ms = audio_duration_ms(ffmpeg, input)?;
    if let Some(hint) = speech_end_hint_ms {
        return Ok(Some(hint.min(duration_ms)));
    }
    let profile = PauseFitProfile::default();
    let intervals = detect_silences(ffmpeg, input, duration_ms, profile)?;
    let plan = plan_pause_fit(duration_ms, &intervals, None, PauseFitMode::Off, profile)?;
    Ok(Some(plan.speech_end_ms))
}

pub fn apply_pause_fit(
    ffmpeg: &FfmpegPaths,
    input: &Path,
    output: &Path,
    plan: &PauseFitPlan,
) -> Result<()> {
    if plan.keep.is_empty() {
        bail!("Pause-fit plan has no audio to keep");
    }
    let filter = if plan.keep.len() == 1 {
        let (start_ms, end_ms) = plan.keep[0];
        let trim = if start_ms == 0 {
            format!("atrim=end={}", seconds(end_ms))
        } else {
            format!("atrim=start={}:end={}", seconds(start_ms), seconds(end_ms))
        };
        format!("[0:a]{trim},asetpts=PTS-STARTPTS[outa]")
    } else {
        let mut filter = String::new();
        for (index, (start_ms, end_ms)) in plan.keep.iter().copied().enumerate() {
            let duration_ms = end_ms - start_ms;
            let fade_ms = CUT_FADE_MS.min(duration_ms / 2);
            filter.push_str(&format!(
                "[0:a]atrim=start={}:end={},asetpts=PTS-STARTPTS",
                seconds(start_ms),
                seconds(end_ms)
            ));
            if fade_ms > 0 {
                let fade = seconds(fade_ms);
                let fade_start = seconds(duration_ms - fade_ms);
                filter.push_str(&format!(
                    ",afade=t=in:st=0:d={fade},afade=t=out:st={fade_start}:d={fade}"
                ));
            }
            filter.push_str(&format!("[pause{index}];"));
        }
        let labels = (0..plan.keep.len())
            .map(|index| format!("[pause{index}]"))
            .collect::<String>();
        filter.push_str(&format!(
            "{labels}concat=n={}:v=0:a=1[outa]",
            plan.keep.len()
        ));
        filter
    };
    let mut command = ffmpeg.ffmpeg_command();
    command
        .args(["-y", "-i"])
        .arg(input)
        .args([
            "-filter_complex",
            &filter,
            "-map",
            "[outa]",
            "-vn",
            "-c:a",
            "libmp3lame",
            "-q:a",
            "2",
        ])
        .arg(output);
    ffmpeg.checked(&mut command, "apply pause-fit")
}

fn detect_silences(
    ffmpeg: &FfmpegPaths,
    input: &Path,
    duration_ms: u32,
    profile: PauseFitProfile,
) -> Result<Vec<SilenceInterval>> {
    let internal = run_silencedetect(ffmpeg, input, profile.internal_noise_db, duration_ms)?;
    let tail = run_silencedetect(ffmpeg, input, profile.tail_noise_db, duration_ms)?;
    let mut intervals = internal;
    if let Some(tail) = tail
        .iter()
        .find(|interval| interval.end_ms.saturating_add(1) >= duration_ms)
        && let Some(existing) = intervals
            .iter_mut()
            .find(|interval| interval.end_ms.saturating_add(1) >= duration_ms)
    {
        existing.start_ms = existing.start_ms.min(tail.start_ms);
        existing.end_ms = duration_ms;
    }
    Ok(intervals)
}

fn run_silencedetect(
    ffmpeg: &FfmpegPaths,
    input: &Path,
    noise_db: f64,
    duration_ms: u32,
) -> Result<Vec<SilenceInterval>> {
    let mut command = ffmpeg.ffmpeg_command();
    let filter = format!("silencedetect=noise={noise_db}dB:d=0.080");
    let output = command
        .args(["-i"])
        .arg(input)
        .args(["-af", &filter, "-f", "null", "-"])
        .output()
        .with_context(|| format!("Failed to run silencedetect on {}", input.display()))?;
    if !output.status.success() {
        bail!(
            "silencedetect failed for {}: {}",
            input.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_silencedetect(&String::from_utf8_lossy(&output.stderr), duration_ms)
}

fn audio_duration_ms(ffmpeg: &FfmpegPaths, input: &Path) -> Result<u32> {
    let duration = ffmpeg.audio_duration(input)?;
    if duration <= 0.0 {
        bail!("Audio has no duration: {}", input.display());
    }
    Ok((duration * 1_000.0).round() as u32)
}

fn temporary_fitted_path(output: &Path) -> PathBuf {
    output.with_file_name(format!(
        "{}.tmp.{}",
        output
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("audio"),
        output.extension().map_or_else(
            || "mp3".to_owned(),
            |extension| extension.to_string_lossy().into_owned()
        )
    ))
}

pub(crate) fn remove_fitted_artifacts(input: &Path) {
    let fitted = fitted_audio_path(input);
    let _ = fs::remove_file(&fitted);
    let _ = fs::remove_file(crate::tts_cache::tts_cache_key_path(&fitted));
    let _ = fs::remove_file(fitted_sidecar_path(input));
}

fn write_fitted_sidecar(
    fitted: &Path,
    source: &Path,
    plan: &PauseFitPlan,
    mode: PauseFitMode,
    profile: PauseFitProfile,
) -> Result<()> {
    let mode = match mode {
        PauseFitMode::Off => "off",
        PauseFitMode::TailOnly => "tail-only",
        PauseFitMode::WithInternal => "with-internal",
    };
    fs::write(
        fitted_sidecar_path(source),
        serde_json::to_vec_pretty(&serde_json::json!({
            "recoveredMs": plan.recovered_ms,
            "tailRecoveredMs": plan.tail_recovered_ms,
            "leadRecoveredMs": plan.lead_recovered_ms,
            "internalRecoveredMs": plan.internal_recovered_ms,
            "speechEndMs": plan.speech_end_ms,
            "mode": mode,
            "profile": profile,
            "sourcePath": source,
            "fittedPath": fitted,
        }))?,
    )
    .with_context(|| {
        format!(
            "Failed to write pause-fit metadata for {}",
            fitted.display()
        )
    })
}

fn seconds(milliseconds: u32) -> String {
    format!("{:.3}", f64::from(milliseconds) / 1_000.0)
}

fn parse_seconds(value: &str) -> Result<f64> {
    value
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("silencedetect line has no timestamp"))?
        .parse::<f64>()
        .map_err(Into::into)
}

fn seconds_to_interval(start_s: f64, end_s: f64, duration_ms: u32) -> Result<SilenceInterval> {
    if !start_s.is_finite() || !end_s.is_finite() || start_s < 0.0 || end_s < start_s {
        bail!("invalid silencedetect interval {start_s}..{end_s}");
    }
    let start_ms = (start_s * 1000.0).round() as u64;
    let end_ms = (end_s * 1000.0).round() as u64;
    Ok(SilenceInterval {
        start_ms: start_ms.min(duration_ms as u64) as u32,
        end_ms: end_ms.min(duration_ms as u64) as u32,
    })
}

fn classify_silences(
    duration_ms: u32,
    intervals: &[SilenceInterval],
    profile: PauseFitProfile,
) -> Result<Vec<SilenceInterval>> {
    let mut normalized = Vec::new();
    for interval in intervals {
        if interval.start_ms > interval.end_ms {
            bail!("silence interval starts after it ends");
        }
        let start_ms = interval.start_ms.min(duration_ms);
        let end_ms = interval.end_ms.min(duration_ms);
        if end_ms > start_ms {
            normalized.push(SilenceInterval { start_ms, end_ms });
        }
    }

    normalized = merge_intervals(normalized);
    let mut merged_short_speech: Vec<SilenceInterval> = Vec::new();
    for interval in normalized {
        if let Some(previous) = merged_short_speech.last_mut()
            && interval.start_ms - previous.end_ms < profile.min_speech_island_ms
        {
            previous.end_ms = interval.end_ms;
        } else {
            merged_short_speech.push(interval);
        }
    }

    Ok(merged_short_speech
        .into_iter()
        .filter(|interval| interval.end_ms - interval.start_ms >= profile.detect_min_ms)
        .collect())
}

fn merge_intervals(mut intervals: Vec<SilenceInterval>) -> Vec<SilenceInterval> {
    intervals.sort_unstable_by_key(|interval| (interval.start_ms, interval.end_ms));
    let mut merged: Vec<SilenceInterval> = Vec::with_capacity(intervals.len());
    for interval in intervals {
        if let Some(previous) = merged.last_mut()
            && interval.start_ms <= previous.end_ms
        {
            previous.end_ms = previous.end_ms.max(interval.end_ms);
        } else {
            merged.push(interval);
        }
    }
    merged
}

fn speech_end_from_silences(duration_ms: u32, silences: &[SilenceInterval]) -> u32 {
    let mut cursor = 0;
    let mut speech_end = 0;
    for silence in silences {
        if silence.start_ms > cursor {
            speech_end = silence.start_ms;
        }
        cursor = cursor.max(silence.end_ms);
    }
    if cursor < duration_ms {
        duration_ms.max(speech_end)
    } else {
        speech_end
    }
}

fn complement_windows(duration_ms: u32, drops: &[SilenceInterval]) -> Vec<(u32, u32)> {
    let mut keep = Vec::new();
    let mut cursor = 0;
    for drop in drops {
        if drop.start_ms > cursor {
            keep.push((cursor, drop.start_ms));
        }
        cursor = cursor.max(drop.end_ms);
    }
    if cursor < duration_ms {
        keep.push((cursor, duration_ms));
    }
    keep
}

fn non_empty_keep_windows(windows: Vec<(u32, u32)>, duration_ms: u32) -> Vec<(u32, u32)> {
    if duration_ms == 0 {
        Vec::new()
    } else {
        windows
            .into_iter()
            .filter(|(start_ms, end_ms)| end_ms > start_ms)
            .collect()
    }
}

fn fitted_speech_end_ms(keep: &[(u32, u32)], raw_speech_end_ms: u32) -> u32 {
    keep.iter()
        .map(|(start_ms, end_ms)| {
            let start_ms = (*start_ms).min(raw_speech_end_ms);
            let end_ms = (*end_ms).min(raw_speech_end_ms);
            end_ms.saturating_sub(start_ms)
        })
        .sum()
}

fn overlaps_speech(
    drops: &[SilenceInterval],
    silences: &[SilenceInterval],
    duration_ms: u32,
) -> bool {
    let mut speech_start = 0;
    let mut speech = Vec::new();
    for silence in silences {
        if speech_start < silence.start_ms {
            speech.push(SilenceInterval {
                start_ms: speech_start,
                end_ms: silence.start_ms,
            });
        }
        speech_start = silence.end_ms;
    }
    if speech_start < duration_ms {
        speech.push(SilenceInterval {
            start_ms: speech_start,
            end_ms: duration_ms,
        });
    }
    drops.iter().any(|drop| {
        speech
            .iter()
            .any(|island| drop.start_ms < island.end_ms && drop.end_ms > island.start_ms)
    })
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::Path;

    use super::*;

    fn interval(start_ms: u32, end_ms: u32) -> SilenceInterval {
        SilenceInterval { start_ms, end_ms }
    }

    #[test]
    fn parses_closed_and_eof_open_silences() {
        let stderr = "silence_start: 0.000\nsilence_end: 0.200 | silence_duration: 0.200\nsilence_start: 1.200";
        assert_eq!(
            parse_silencedetect(&stderr, 1450).unwrap(),
            vec![interval(0, 200), interval(1200, 1450)]
        );
    }

    #[test]
    fn tail_only_recovers_lead_and_eof_tail_by_interval_length() {
        let plan = plan_pause_fit(
            1450,
            &[interval(0, 200), interval(1200, 1450)],
            None,
            PauseFitMode::TailOnly,
            PauseFitProfile::default(),
        )
        .unwrap();
        assert_eq!(plan.recovered_ms, 370);
        assert_eq!(plan.lead_recovered_ms, 160);
        assert_eq!(plan.tail_recovered_ms, 210);
        assert_eq!(plan.keep, vec![(160, 1240)]);
        assert_eq!(plan.speech_end_ms, 1040);
    }

    #[test]
    fn hinted_tail_never_cuts_the_lead_or_moves_the_bookmark() {
        let plan = plan_pause_fit(
            5500,
            &[interval(0, 200), interval(5250, 5500)],
            Some(5000),
            PauseFitMode::TailOnly,
            PauseFitProfile::default(),
        )
        .unwrap();
        assert_eq!(plan.keep.first(), Some(&(0, 5040)));
        assert_eq!(plan.speech_end_ms, 5000);
        assert_eq!(plan.lead_recovered_ms, 0);
        assert_eq!(plan.internal_recovered_ms, 0);
    }

    #[test]
    fn hinted_tail_keeps_internal_holes() {
        let plan = plan_pause_fit(
            5500,
            &[interval(2000, 2400), interval(5250, 5500)],
            Some(5000),
            PauseFitMode::TailOnly,
            PauseFitProfile::default(),
        )
        .unwrap();
        assert!(
            plan.keep
                .iter()
                .any(|&(start, end)| start <= 2000 && end >= 2400)
        );
        assert!(plan.keep.iter().all(|&(start, _)| start < 5000));
        assert!(
            plan.keep
                .iter()
                .any(|&(start, end)| start == 0 && end >= 5000)
        );
    }

    #[test]
    fn hinted_tail_exempts_shy_detector_after_bookmark() {
        let plan = plan_pause_fit(
            1600,
            &[interval(0, 200), interval(1050, 1600)],
            Some(1000),
            PauseFitMode::TailOnly,
            PauseFitProfile::default(),
        )
        .unwrap();
        assert_eq!(plan.keep, vec![(0, 1040)]);
        assert_eq!(plan.tail_recovered_ms, 560);
        assert_eq!(plan.speech_end_ms, 1000);
    }

    #[test]
    fn with_internal_keeps_a_centered_pause_floor() {
        let plan = plan_pause_fit(
            2150,
            &[interval(1000, 1400), interval(1900, 2150)],
            None,
            PauseFitMode::WithInternal,
            PauseFitProfile::default(),
        )
        .unwrap();
        assert_eq!(plan.internal_recovered_ms, 300);
        assert_eq!(plan.tail_recovered_ms, 210);
        assert_eq!(plan.recovered_ms, 510);
        assert_eq!(plan.speech_end_ms, 1600);
    }

    #[test]
    fn internal_pause_shorter_than_floor_stays_whole() {
        let plan = plan_pause_fit(
            1090,
            &[interval(500, 590), interval(790, 1090)],
            None,
            PauseFitMode::WithInternal,
            PauseFitProfile::default(),
        )
        .unwrap();
        assert_eq!(plan.internal_recovered_ms, 0);
        assert_eq!(plan.recovered_ms, 260);
    }

    #[test]
    fn short_speech_blip_between_silences_is_absorbed_before_fitting() {
        assert_eq!(
            classify_silences(
                1000,
                &[interval(0, 300), interval(320, 1000)],
                PauseFitProfile::default(),
            )
            .unwrap(),
            vec![interval(0, 1000)]
        );
    }

    #[test]
    fn off_mode_keeps_the_raw_timeline_but_still_classifies_speech_end() {
        let plan = plan_pause_fit(
            1450,
            &[interval(0, 200), interval(1200, 1450)],
            None,
            PauseFitMode::Off,
            PauseFitProfile::default(),
        )
        .unwrap();
        assert_eq!(plan.keep, vec![(0, 1450)]);
        assert_eq!(plan.recovered_ms, 0);
        assert_eq!(plan.speech_end_ms, 1200);
    }

    #[test]
    fn write_tone_silence_wav_writes_a_valid_pcm_fixture() {
        let path =
            std::env::temp_dir().join(format!("dubora-pause-fit-{}.wav", std::process::id()));
        write_tone_silence_wav(&path, 48_000, &[(200, Some(-6.0)), (250, None)]).unwrap();
        let mut file = File::open(&path).unwrap();
        let mut header = [0_u8; 12];
        file.read_exact(&mut header).unwrap();
        assert_eq!(&header[0..4], b"RIFF");
        assert_eq!(&header[8..12], b"WAVE");
        file.seek(SeekFrom::Start(46)).unwrap();
        let mut sample = [0_u8; 2];
        file.read_exact(&mut sample).unwrap();
        assert_ne!(sample, [0, 0]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fitted_cache_key_changes_for_every_sample_changing_profile_value() {
        let raw = "raw-cache-key";
        let profile = PauseFitProfile::default();
        let key = |profile| fitted_cache_key(raw, PauseFitMode::TailOnly, Some(5000), profile);
        let mut changed = profile;
        changed.version += 1;
        assert_ne!(key(profile), key(changed));
        let mut changed = profile;
        changed.internal_noise_db -= 1.0;
        assert_ne!(key(profile), key(changed));
        let mut changed = profile;
        changed.tail_noise_db -= 1.0;
        assert_ne!(key(profile), key(changed));
        let mut changed = profile;
        changed.detect_min_ms += 1;
        assert_ne!(key(profile), key(changed));
        let mut changed = profile;
        changed.pause_keep_floor_ms += 1;
        assert_ne!(key(profile), key(changed));
        let mut changed = profile;
        changed.lead_keep_ms += 1;
        assert_ne!(key(profile), key(changed));
        let mut changed = profile;
        changed.tail_keep_ms += 1;
        assert_ne!(key(profile), key(changed));
        let mut changed = profile;
        changed.cut_fade_ms += 1;
        assert_ne!(key(profile), key(changed));
        let mut changed = profile;
        changed.min_speech_island_ms += 1;
        assert_ne!(key(profile), key(changed));
        let mut changed = profile;
        changed.encoder = "other-encoder";
        assert_ne!(key(profile), key(changed));
        assert_ne!(
            fitted_cache_key(raw, PauseFitMode::TailOnly, Some(5000), profile),
            fitted_cache_key(raw, PauseFitMode::WithInternal, Some(5000), profile)
        );
        assert_ne!(
            fitted_cache_key(raw, PauseFitMode::TailOnly, Some(5000), profile),
            fitted_cache_key(raw, PauseFitMode::TailOnly, Some(5001), profile)
        );
    }

    #[test]
    #[ignore = "requires local FFmpeg with silencedetect and libmp3lame"]
    fn fits_an_encoded_lead_and_tail_without_overwriting_the_raw_audio() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "dubora-pause-fit-integration-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory)?;
        let wav = directory.join("raw.wav");
        let raw = directory.join("raw.mp3");
        write_tone_silence_wav(
            &wav,
            48_000,
            &[(200, None), (1_000, Some(-6.0)), (250, None)],
        )?;
        let ffmpeg = crate::ffmpeg::FfmpegPaths::resolve(None);
        let mut command = ffmpeg.ffmpeg_command();
        command
            .args(["-y", "-i"])
            .arg(&wav)
            .args(["-c:a", "libmp3lame", "-q:a", "2"])
            .arg(&raw);
        ffmpeg.checked(&mut command, "encode pause-fit fixture")?;
        write_tts_cache_key(&raw, "raw-pause-fit-fixture")?;

        let stats = fit_tts_file(&ffmpeg, &raw, None, PauseFitMode::TailOnly)?;
        assert!(stats.recovered_ms > 0);
        assert_eq!(stats.path, fitted_audio_path(&raw));
        assert!(raw.is_file());
        assert!(stats.path.is_file());
        assert!(stats.measured_audio_ms < 1_600);

        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    fn write_tone_silence_wav(
        path: &Path,
        sample_rate: u32,
        parts: &[(u32, Option<f32>)],
    ) -> Result<()> {
        let channels = 1_u16;
        let bits_per_sample = 16_u16;
        let samples = parts
            .iter()
            .map(|(duration_ms, _)| (*duration_ms as u64 * sample_rate as u64 / 1000) as usize)
            .sum::<usize>();
        let data_len = samples * 2;
        let byte_rate = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
        let block_align = channels * bits_per_sample / 8;

        let mut file = File::create(path)?;
        file.write_all(b"RIFF")?;
        file.write_all(&(36_u32 + data_len as u32).to_le_bytes())?;
        file.write_all(b"WAVEfmt ")?;
        file.write_all(&16_u32.to_le_bytes())?;
        file.write_all(&1_u16.to_le_bytes())?;
        file.write_all(&channels.to_le_bytes())?;
        file.write_all(&sample_rate.to_le_bytes())?;
        file.write_all(&byte_rate.to_le_bytes())?;
        file.write_all(&block_align.to_le_bytes())?;
        file.write_all(&bits_per_sample.to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&(data_len as u32).to_le_bytes())?;

        for (duration_ms, dbfs) in parts {
            let sample_count = (*duration_ms as u64 * sample_rate as u64 / 1000) as usize;
            let amplitude = dbfs.map(|db| 10_f32.powf(db / 20.0)).unwrap_or(0.0);
            for index in 0..sample_count {
                let value = (amplitude
                    * i16::MAX as f32
                    * (TAU * 440.0 * index as f32 / sample_rate as f32).sin())
                    as i16;
                file.write_all(&value.to_le_bytes())?;
            }
        }
        Ok(())
    }
}
