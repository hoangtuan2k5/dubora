use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json;

use crate::duration_solver::{
    MAX_TIME_STRETCH_TOLERANCE_MS, SEGMENT_HEADROOM_MS, spillback_schedule,
};
use crate::ffmpeg::FfmpegPaths;
use crate::srt::{parse_srt, time_to_seconds};
use crate::sync_timeline::{SourceTimeline, SyncLayout};
use crate::tts_cache::has_matching_tts_cache_key;
use crate::utterance::{ManifestStatus, RenderManifest};

const JOIN_FADE_MS: u32 = 30;
const PAD_ONLY_TOLERANCE_MS: u32 = 26;
// Rollback switch: DUBORA_INTERNAL_JOIN_FADES=0 restores the pre-PR4 clip joins.
const JOIN_FADES_ENV: &str = "DUBORA_INTERNAL_JOIN_FADES";

#[derive(Clone, Copy, Debug, PartialEq)]
struct PadClamp {
    available_s: f64,
    speech_end_s: Option<f64>,
    atrim_duration_s: Option<f64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrimMode {
    Manifest,
    Always,
}

#[derive(Clone, Copy, Debug)]
struct EntryFilterSpec {
    tempo: f64,
    rendered_audio_ms: u32,
    available_ms: u32,
    actual_ms: u32,
    speech_end_ms: Option<u32>,
    delay_ms: u64,
    trim_mode: TrimMode,
}

fn join_fades_enabled() -> bool {
    std::env::var(JOIN_FADES_ENV)
        .map(|value| value.trim() != "0")
        .unwrap_or(true)
}

fn ceil_stretched_ms(duration_ms: u32, tempo: f64) -> u32 {
    (f64::from(duration_ms) / tempo.max(f64::MIN_POSITIVE)).ceil() as u32
}

fn pad_clamp(
    available_ms: u32,
    actual_ms: u32,
    tempo: f64,
    speech_end_ms: Option<u32>,
) -> Result<PadClamp> {
    let probed_ms = ceil_stretched_ms(actual_ms, tempo);
    let speech_end_stretched_ms = speech_end_ms.map(|value| ceil_stretched_ms(value, tempo));
    let atrim_duration_s = if probed_ms <= available_ms {
        None
    } else if speech_end_ms.is_none() {
        Some(f64::from(available_ms) / 1_000.0)
    } else if probed_ms.saturating_sub(available_ms) <= PAD_ONLY_TOLERANCE_MS
        && speech_end_stretched_ms.is_some_and(|value| value <= available_ms)
    {
        Some(f64::from(available_ms) / 1_000.0)
    } else {
        bail!(
            "Audio would trim speech: probed={}ms available={}ms speechEnd={:?}ms",
            probed_ms,
            available_ms,
            speech_end_stretched_ms
        )
    };
    Ok(PadClamp {
        available_s: f64::from(available_ms) / 1_000.0,
        speech_end_s: speech_end_stretched_ms.map(|value| f64::from(value) / 1_000.0),
        atrim_duration_s,
    })
}

fn entry_filter_chain(spec: EntryFilterSpec, fades_enabled: bool) -> Result<String> {
    let clamp = match spec.trim_mode {
        TrimMode::Manifest => Some(pad_clamp(
            spec.available_ms,
            spec.actual_ms,
            spec.tempo,
            spec.speech_end_ms,
        )?),
        TrimMode::Always => None,
    };
    let available_s = clamp.as_ref().map_or_else(
        || f64::from(spec.available_ms) / 1_000.0,
        |value| value.available_s,
    );
    let mut chain = Vec::with_capacity(6);
    if spec.tempo > 1.0 {
        chain.push(format!("rubberband=tempo={:.6}", spec.tempo));
    }
    if fades_enabled {
        let fade_ms = JOIN_FADE_MS.min(spec.rendered_audio_ms / 2);
        if fade_ms > 0 {
            let fade_s = f64::from(fade_ms) / 1_000.0;
            let fade_start_s = (f64::from(spec.rendered_audio_ms) / 1_000.0 - fade_s).max(0.0);
            chain.push(format!("afade=t=in:st=0:d={fade_s:.3}"));
            chain.push(format!("afade=t=out:st={fade_start_s:.3}:d={fade_s:.3}"));
        }
    }
    match spec.trim_mode {
        TrimMode::Manifest => {
            if let Some(duration_s) = clamp.and_then(|value| value.atrim_duration_s) {
                chain.push(format!("atrim=duration={duration_s:.3}"));
            }
        }
        TrimMode::Always => chain.push(format!("atrim=duration={available_s:.3}")),
    }
    chain.extend([
        "asetpts=PTS-STARTPTS".to_owned(),
        format!("adelay={}:all=1", spec.delay_ms),
    ]);
    Ok(chain.join(","))
}

pub fn build_voiceover_track(
    ffmpeg: &FfmpegPaths,
    input_srt: &Path,
    tts_dir: &Path,
    output_audio: &Path,
    work_dir: &Path,
) -> Result<()> {
    build_voiceover_track_with_tempo_cap(ffmpeg, input_srt, tts_dir, output_audio, work_dir, None)
}

/// Build a voiceover from a mixed utterance/segment render manifest.
pub fn build_voiceover_track_from_manifest(
    ffmpeg: &FfmpegPaths,
    input_srt: &Path,
    source_timeline: &SourceTimeline,
    sync_layout: &SyncLayout,
    manifest_path: &Path,
    output_audio: &Path,
    work_dir: &Path,
    max_tempo: Option<f64>,
) -> Result<()> {
    let manifest: RenderManifest = serde_json::from_slice(
        &fs::read(manifest_path)
            .with_context(|| format!("Failed to read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("Invalid render manifest {}", manifest_path.display()))?;
    if manifest.entries.is_empty() {
        bail!("Audio render manifest is empty");
    }
    if manifest.status == ManifestStatus::ReviewRequired {
        bail!("Audio render manifest requires review and cannot be published");
    }
    let segments = parse_srt(input_srt)?;
    let expected = segments
        .iter()
        .map(|segment| segment.index)
        .collect::<Vec<_>>();
    sync_layout.validate(&expected)?;
    manifest.validate(&expected)?;
    manifest.validate_source_timeline(source_timeline, sync_layout)?;

    let entries = manifest.entries;
    if entries
        .windows(2)
        .any(|pair| pair[1].start_ms < pair[0].start_ms)
    {
        bail!("Audio render manifest entries are not ordered");
    }
    let inputs: Vec<_> = entries.iter().map(|entry| entry.path.clone()).collect();
    if let Some(missing) = inputs.iter().find(|path| !path.is_file()) {
        bail!("Missing manifest audio file: {}", missing.display());
    }
    if let Some(stale) = entries
        .iter()
        .find(|entry| !has_matching_tts_cache_key(&entry.path, &entry.cache_identity))
    {
        bail!("Manifest audio cache identity changed: {}", stale.id);
    }

    let join_fades = join_fades_enabled();
    let mut filters = Vec::with_capacity(entries.len());
    for (position, (entry, input)) in entries.iter().zip(&inputs).enumerate() {
        let available_ms = entry.end_ms.saturating_sub(entry.start_ms);
        let actual_duration = ffmpeg.audio_duration(input)?;
        let actual_ms = (actual_duration * 1_000.0).ceil() as u32;
        let manifest_duration_ms = entry.measured_audio_ms;
        if actual_ms.abs_diff(manifest_duration_ms) > 150 {
            bail!(
                "Manifest audio {} duration changed after allocation ({}ms vs {}ms)",
                entry.id,
                actual_ms,
                manifest_duration_ms
            );
        }
        let tempo = entry.tempo;
        if let Some(cap) = max_tempo.filter(|cap| tempo > *cap + f64::EPSILON) {
            bail!(
                "Manifest audio {} requires {tempo:.2}x stretch, above the {cap:.2}x Natural Voice cap",
                entry.id
            );
        }
        if entry.rendered_audio_ms > available_ms {
            bail!(
                "Manifest audio {} does not fit its allocated slot",
                entry.id
            );
        }
        let chain = entry_filter_chain(
            EntryFilterSpec {
                tempo,
                rendered_audio_ms: entry.rendered_audio_ms,
                available_ms,
                actual_ms,
                speech_end_ms: entry.speech_end_ms,
                delay_ms: u64::from(entry.start_ms),
                trim_mode: TrimMode::Manifest,
            },
            join_fades,
        )?;
        filters.push(format!("[{position}:a]{chain}[a{position}]"));
    }

    let mixed = (0..entries.len())
        .map(|position| format!("[a{position}]"))
        .collect::<String>();
    filters.push(format!(
        "{mixed}amix=inputs={}:duration=longest:dropout_transition=0:normalize=0,alimiter=limit=0.95[outa]",
        entries.len()
    ));

    fs::create_dir_all(work_dir)?;
    let filter_path = work_dir.join("aligned_filter_complex.txt");
    fs::write(&filter_path, filters.join(";"))?;
    let mut command = ffmpeg.ffmpeg_command();
    command.arg("-y");
    for input in &inputs {
        command.arg("-i").arg(input);
    }
    command
        .arg("-filter_complex_script")
        .arg(&filter_path)
        .args(["-map", "[outa]", "-c:a", "libmp3lame", "-q:a", "2"])
        .arg(output_audio);
    ffmpeg
        .checked(&mut command, "build aligned voiceover")
        .with_context(|| format!("Failed to create {}", output_audio.display()))
}

/// Build a Natural Voice track without permitting the legacy unlimited tempo correction.
pub fn build_voiceover_track_with_tempo_cap(
    ffmpeg: &FfmpegPaths,
    input_srt: &Path,
    tts_dir: &Path,
    output_audio: &Path,
    work_dir: &Path,
    max_tempo: Option<f64>,
) -> Result<()> {
    let segments = parse_srt(input_srt)?;
    if segments.is_empty() {
        bail!("No Vietnamese subtitle segments were found");
    }

    let inputs: Vec<_> = segments
        .iter()
        .map(|segment| tts_dir.join(format!("seg_{}.mp3", segment.index)))
        .collect();
    if let Some(missing) = inputs.iter().find(|path| !path.is_file()) {
        bail!(
            "Missing TTS segments. First missing file: {}",
            missing.display()
        );
    }

    let join_fades = join_fades_enabled();
    let mut filters = Vec::with_capacity(segments.len());
    let mut previous_rendered_end = None;
    for (position, (segment, mp3)) in segments.iter().zip(&inputs).enumerate() {
        let start = time_to_seconds(&segment.start)?;
        let slot_end = if let Some(next) = segments.get(position + 1) {
            time_to_seconds(&next.start)?
        } else {
            time_to_seconds(&segment.end)?
        };
        let available = (slot_end - start - f64::from(SEGMENT_HEADROOM_MS) / 1_000.0).max(0.1);
        let duration = ffmpeg.audio_duration(mp3)?;
        let (render_start, available) =
            spillback_schedule(start, available, duration, previous_rendered_end, max_tempo);
        let tempo = (duration / available).max(1.0);
        if let Some(cap) = max_tempo.filter(|cap| {
            duration > available * *cap + f64::from(MAX_TIME_STRETCH_TOLERANCE_MS) / 1_000.0
        }) {
            bail!(
                "Segment {} requires {tempo:.2}x stretch, above the {cap:.2}x Natural Voice cap",
                segment.index
            );
        }
        let available_ms = (available * 1_000.0).round().max(1.0) as u32;
        let actual_ms = (duration * 1_000.0).ceil().max(1.0) as u32;
        let rendered_audio_ms = (duration / tempo * 1_000.0).ceil().max(1.0) as u32;
        let chain = entry_filter_chain(
            EntryFilterSpec {
                tempo,
                rendered_audio_ms,
                available_ms,
                actual_ms,
                speech_end_ms: None,
                delay_ms: (render_start * 1_000.0) as u64,
                trim_mode: TrimMode::Always,
            },
            join_fades,
        )?;
        filters.push(format!("[{position}:a]{chain}[a{position}]"));
        previous_rendered_end = Some(render_start + (duration / tempo).min(available));
    }

    let mixed = (0..segments.len())
        .map(|position| format!("[a{position}]"))
        .collect::<String>();
    filters.push(format!(
        "{mixed}amix=inputs={}:duration=longest:dropout_transition=0:normalize=0,alimiter=limit=0.95[outa]",
        segments.len()
    ));

    fs::create_dir_all(work_dir)?;
    let filter_path = work_dir.join("filter_complex.txt");
    fs::write(&filter_path, filters.join(";"))?;

    let mut command = ffmpeg.ffmpeg_command();
    command.arg("-y");
    for input in &inputs {
        command.arg("-i").arg(input);
    }
    command
        .arg("-filter_complex_script")
        .arg(&filter_path)
        .args(["-map", "[outa]", "-c:a", "libmp3lame", "-q:a", "2"])
        .arg(output_audio);

    ffmpeg
        .checked(&mut command, "build voiceover")
        .with_context(|| format!("Failed to create {}", output_audio.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_a_small_encoded_pad_tail_without_cutting_speech() {
        let clamp = pad_clamp(2_000, 2_020, 1.0, Some(1_980)).unwrap();
        assert_eq!(clamp.available_s, 2.0);
        assert_eq!(clamp.speech_end_s, Some(1.98));
        assert_eq!(clamp.atrim_duration_s, Some(2.0));
        let chain = entry_filter_chain(
            EntryFilterSpec {
                tempo: 1.0,
                rendered_audio_ms: 2_000,
                available_ms: 2_000,
                actual_ms: 2_020,
                speech_end_ms: Some(1_980),
                delay_ms: 0,
                trim_mode: TrimMode::Manifest,
            },
            false,
        )
        .unwrap();
        assert!(chain.contains("atrim=duration=2.000"));
        assert!(!chain.contains("acrossfade"));
    }

    #[test]
    fn fades_are_inside_the_clip_and_use_st_not_ss() {
        let chain = entry_filter_chain(
            EntryFilterSpec {
                tempo: 1.0,
                rendered_audio_ms: 2_000,
                available_ms: 2_000,
                actual_ms: 2_000,
                speech_end_ms: Some(1_980),
                delay_ms: 500,
                trim_mode: TrimMode::Manifest,
            },
            true,
        )
        .unwrap();
        assert!(chain.contains("afade=t=in:st=0:d=0.030"));
        assert!(chain.contains("afade=t=out:st=1.970:d=0.030"));
        assert!(!chain.contains("afade=t=in:ss="));
        assert!(chain.ends_with("asetpts=PTS-STARTPTS,adelay=500:all=1"));
    }

    #[test]
    fn missing_speech_end_keeps_the_historical_always_trim_fallback() {
        let chain = entry_filter_chain(
            EntryFilterSpec {
                tempo: 1.0,
                rendered_audio_ms: 2_000,
                available_ms: 2_000,
                actual_ms: 2_050,
                speech_end_ms: None,
                delay_ms: 0,
                trim_mode: TrimMode::Manifest,
            },
            false,
        )
        .unwrap();
        assert!(chain.contains("atrim=duration=2.000"));
    }

    #[test]
    fn unlimited_mode_keeps_always_trim_without_pad_tolerance() {
        let chain = entry_filter_chain(
            EntryFilterSpec {
                tempo: 1.0,
                rendered_audio_ms: 2_000,
                available_ms: 2_000,
                actual_ms: 2_050,
                speech_end_ms: Some(1_980),
                delay_ms: 0,
                trim_mode: TrimMode::Always,
            },
            false,
        )
        .unwrap();
        assert!(chain.contains("atrim=duration=2.000"));
    }
}
