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

    let mut filters = Vec::with_capacity(entries.len());
    for (position, (entry, input)) in entries.iter().zip(&inputs).enumerate() {
        let available = f64::from(entry.end_ms.saturating_sub(entry.start_ms)) / 1_000.0;
        let actual_duration = ffmpeg.audio_duration(input)?;
        let manifest_duration = f64::from(entry.measured_audio_ms) / 1_000.0;
        if (actual_duration - manifest_duration).abs() > 0.15 {
            bail!(
                "Manifest audio {} duration changed after allocation ({actual_duration:.3}s vs {manifest_duration:.3}s)",
                entry.id
            );
        }
        let tempo = entry.tempo;
        if let Some(cap) = max_tempo.filter(|cap| tempo > *cap + f64::EPSILON) {
            bail!(
                "Manifest audio {} requires {tempo:.2}x stretch, above the {cap:.2}x Natural Voice cap",
                entry.id
            );
        }
        if f64::from(entry.rendered_audio_ms) / 1_000.0 > available + 0.002 {
            bail!(
                "Manifest audio {} does not fit its allocated slot",
                entry.id
            );
        }
        if actual_duration / tempo > available + 0.002 {
            bail!(
                "Manifest audio {} would be truncated after the allocated tempo",
                entry.id
            );
        }
        let delay_ms = entry.start_ms;
        let mut chain = Vec::with_capacity(4);
        if tempo > 1.0 {
            chain.push(format!("rubberband=tempo={tempo:.6}"));
        }
        chain.extend([
            format!("atrim=duration={available:.3}"),
            "asetpts=PTS-STARTPTS".to_owned(),
            format!("adelay={delay_ms}:all=1"),
        ]);
        filters.push(format!("[{position}:a]{}[a{position}]", chain.join(",")));
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
        let delay_ms = (render_start * 1000.0) as u64;

        let mut chain = Vec::with_capacity(4);
        if tempo > 1.0 {
            chain.push(format!("rubberband=tempo={tempo:.6}"));
        }
        chain.extend([
            format!("atrim=duration={available:.3}"),
            "asetpts=PTS-STARTPTS".to_owned(),
            format!("adelay={delay_ms}:all=1"),
        ]);
        filters.push(format!("[{position}:a]{}[a{position}]", chain.join(",")));
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
