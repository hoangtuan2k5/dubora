use std::fs;
use std::path::{Path, PathBuf};

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
const MAX_MIX_INPUTS: usize = 32;
// Leave room for Windows argument quoting and the fixed FFmpeg/output arguments.
const MAX_MIX_COMMAND_UNITS: usize = 16_000;

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

struct MixClip {
    path: PathBuf,
    filter: EntryFilterSpec,
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
        && speech_end_stretched_ms
            .is_some_and(|value| value.saturating_sub(available_ms) <= PAD_ONLY_TOLERANCE_MS)
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

    let mut clips = Vec::with_capacity(entries.len());
    for (entry, input) in entries.iter().zip(&inputs) {
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
        clips.push(MixClip {
            path: input.clone(),
            filter: EntryFilterSpec {
                tempo,
                rendered_audio_ms: entry.rendered_audio_ms,
                available_ms,
                actual_ms,
                speech_end_ms: entry.speech_end_ms,
                delay_ms: u64::from(entry.start_ms),
                trim_mode: TrimMode::Manifest,
            },
        });
    }
    render_voiceover(
        ffmpeg,
        &clips,
        output_audio,
        work_dir,
        "aligned_filter_complex.txt",
    )
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

    let mut clips = Vec::with_capacity(segments.len());
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
        clips.push(MixClip {
            path: mp3.clone(),
            filter: EntryFilterSpec {
                tempo,
                rendered_audio_ms,
                available_ms,
                actual_ms,
                speech_end_ms: None,
                delay_ms: (render_start * 1_000.0) as u64,
                trim_mode: TrimMode::Always,
            },
        });
        previous_rendered_end = Some(render_start + (duration / tempo).min(available));
    }

    render_voiceover(ffmpeg, &clips, output_audio, work_dir, "filter_complex.txt")
}

fn path_command_units(path: &Path) -> usize {
    // Counting every UTF-16 code unit twice also bounds backslash/quote escaping.
    path.as_os_str().to_string_lossy().encode_utf16().count() * 2 + 8
}

fn mix_groups(clips: &[MixClip], fixed_units: usize) -> Result<Vec<std::ops::Range<usize>>> {
    let mut groups = Vec::new();
    let mut start = 0;
    let mut units = fixed_units;
    for (position, clip) in clips.iter().enumerate() {
        let added = path_command_units(&clip.path);
        if fixed_units + added > MAX_MIX_COMMAND_UNITS {
            bail!(
                "Audio path exceeds the safe FFmpeg command length: {}",
                clip.path.display()
            );
        }
        if position > start
            && (position - start == MAX_MIX_INPUTS || units + added > MAX_MIX_COMMAND_UNITS)
        {
            groups.push(start..position);
            start = position;
            units = fixed_units;
        }
        units += added;
    }
    if start < clips.len() {
        groups.push(start..clips.len());
    }
    Ok(groups)
}

fn mix_filter(
    clips: &[MixClip],
    origin_ms: u64,
    end_ms: Option<u64>,
    limiter: bool,
) -> Result<String> {
    let mut filters = Vec::with_capacity(clips.len() + 1);
    for (position, clip) in clips.iter().enumerate() {
        let mut spec = clip.filter;
        spec.delay_ms = spec
            .delay_ms
            .checked_sub(origin_ms)
            .context("Audio clips are not ordered at the chunk boundary")?;
        let audio_ms = ceil_stretched_ms(spec.actual_ms, spec.tempo).min(spec.available_ms);
        if let Some(end_ms) = end_ms
            && clip.filter.delay_ms + u64::from(audio_ms) > end_ms
        {
            bail!("Audio chunk boundary would cut rendered speech");
        }
        let chain = entry_filter_chain(spec, join_fades_enabled())?;
        filters.push(format!("[{position}:a]{chain}[a{position}]"));
    }
    let mixed = (0..clips.len())
        .map(|position| format!("[a{position}]"))
        .collect::<String>();
    let mut mix = format!(
        "{mixed}amix=inputs={}:duration=longest:dropout_transition=0:normalize=0",
        clips.len()
    );
    if let Some(end_ms) = end_ms {
        let samples = end_ms
            .checked_sub(origin_ms)
            .context("Invalid audio chunk interval")?
            * 48;
        // Exact PCM lengths preserve silence between chunks without MP3 encoder padding.
        mix.push_str(&format!(
            ",aresample=48000,apad=whole_len={samples},atrim=end_sample={samples}"
        ));
    }
    if limiter {
        mix.push_str(",alimiter=limit=0.95");
    }
    filters.push(format!("{mix}[outa]"));
    Ok(filters.join(";"))
}

fn render_voiceover(
    ffmpeg: &FfmpegPaths,
    clips: &[MixClip],
    output_audio: &Path,
    work_dir: &Path,
    filter_name: &str,
) -> Result<()> {
    fs::create_dir_all(work_dir)?;
    let filter_path = work_dir.join(filter_name);
    let fixed_units = 512
        + path_command_units(&ffmpeg.ffmpeg)
        + path_command_units(&filter_path)
        + path_command_units(output_audio);
    let groups = mix_groups(clips, fixed_units)?;
    // Validate every clip before starting potentially expensive renders.
    for clip in clips {
        entry_filter_chain(clip.filter, join_fades_enabled())?;
    }
    if groups.len() == 1 {
        fs::write(&filter_path, mix_filter(clips, 0, None, true)?)?;
        return render_mix(ffmpeg, clips, &filter_path, output_audio, false);
    }

    let chunk_dir = work_dir.join("voiceover-chunks");
    fs::create_dir_all(&chunk_dir)?;
    let mut chunks = Vec::with_capacity(groups.len());
    let mut list = String::new();
    for (index, group) in groups.iter().enumerate() {
        let origin_ms = if index == 0 {
            0
        } else {
            clips[group.start].filter.delay_ms
        };
        let end_ms = clips.get(group.end).map(|clip| clip.filter.delay_ms);
        let filter = chunk_dir.join(format!("chunk-{index:05}.txt"));
        fs::write(
            &filter,
            mix_filter(&clips[group.clone()], origin_ms, end_ms, false)?,
        )?;
        let audio = chunk_dir.join(format!("chunk-{index:05}.wav"));
        render_mix(ffmpeg, &clips[group.clone()], &filter, &audio, true)?;
        list.push_str(&format!("file 'voiceover-chunks/chunk-{index:05}.wav'\n"));
        chunks.push(audio);
    }
    let concat = work_dir.join("voiceover-concat.txt");
    fs::write(&concat, list)?;
    let mut command = ffmpeg.ffmpeg_command();
    command
        .arg("-y")
        .args(["-f", "concat", "-safe", "0", "-i"])
        .arg(&concat)
        .args([
            "-af",
            "alimiter=limit=0.95",
            "-c:a",
            "libmp3lame",
            "-q:a",
            "2",
        ])
        .arg(output_audio);
    ffmpeg.checked(&mut command, "concatenate voiceover chunks")?;
    for chunk in chunks {
        let _ = fs::remove_file(chunk);
    }
    Ok(())
}

fn render_mix(
    ffmpeg: &FfmpegPaths,
    clips: &[MixClip],
    filter: &Path,
    output: &Path,
    pcm: bool,
) -> Result<()> {
    let mut command = ffmpeg.ffmpeg_command();
    command.args(["-y", "-filter_complex_threads", "1"]);
    for clip in clips {
        command.arg("-i").arg(&clip.path);
    }
    command
        .arg("-filter_complex_script")
        .arg(filter)
        .args(["-map", "[outa]"]);
    if pcm {
        command.args([
            "-ar",
            "48000",
            "-ac",
            "1",
            "-c:a",
            "pcm_f32le",
            "-rf64",
            "auto",
        ]);
    } else {
        command.args(["-c:a", "libmp3lame", "-q:a", "2"]);
    }
    command.arg(output);
    ffmpeg
        .checked(&mut command, "build voiceover")
        .with_context(|| format!("Failed to create {}", output.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(path: PathBuf, delay_ms: u64) -> MixClip {
        MixClip {
            path,
            filter: EntryFilterSpec {
                tempo: 1.0,
                rendered_audio_ms: 120,
                available_ms: 200,
                actual_ms: 120,
                speech_end_ms: Some(120),
                delay_ms,
                trim_mode: TrimMode::Manifest,
            },
        }
    }

    #[test]
    fn two_hour_groups_bound_input_count_and_quoted_unicode_paths() {
        let clips = (0..1200)
            .map(|i| {
                clip(
                    PathBuf::from(format!(
                        "C:/Khóa học có khoảng trắng/{}/utterance_{i}.fitted.mp3",
                        "Long folder name/".repeat(50)
                    )),
                    i * 6_000,
                )
            })
            .collect::<Vec<_>>();
        let groups = mix_groups(&clips, 1024).unwrap();
        assert_eq!(groups.first().unwrap().start, 0);
        assert_eq!(groups.last().unwrap().end, 1200);
        for group in &groups {
            assert!(group.len() <= MAX_MIX_INPUTS);
            assert!(
                1024 + clips[group.clone()]
                    .iter()
                    .map(|c| path_command_units(&c.path))
                    .sum::<usize>()
                    <= MAX_MIX_COMMAND_UNITS
            );
        }
        assert!(groups.windows(2).all(|g| g[0].end == g[1].start));
    }

    #[test]
    fn chunks_preserve_leading_gap_and_use_local_sample_exact_boundaries() {
        let clips = vec![
            clip("a.mp3".into(), 3_600_050),
            clip("b.mp3".into(), 3_600_300),
        ];
        let local = mix_filter(&clips, 3_600_000, Some(3_601_000), false).unwrap();
        assert!(local.contains("adelay=50:all=1"));
        assert!(local.contains("adelay=300:all=1"));
        assert!(local.contains("apad=whole_len=48000,atrim=end_sample=48000"));
        assert!(!local.contains("alimiter"));
        let first = mix_filter(&clips[..1], 0, Some(3_600_300), false).unwrap();
        assert!(first.contains("adelay=3600050:all=1"));
        assert!(mix_filter(&clips, 3_600_000, Some(3_600_400), false).is_err());
        let mut short = clip("short.mp3".into(), 0);
        short.filter.available_ms = 1000;
        assert!(mix_filter(&[short], 0, Some(500), false).is_ok());
    }

    #[test]
    #[ignore = "requires local FFmpeg with libmp3lame"]
    fn lossless_chunks_match_single_mix_without_missing_or_repeating_boundary_speech() -> Result<()>
    {
        let dir = std::env::temp_dir().join(format!("dubora-chunk-audio-{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        let ffmpeg = FfmpegPaths::resolve(None);
        let tone = dir.join("tone.wav");
        let mut generate = ffmpeg.ffmpeg_command();
        generate
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=0.12",
            ])
            .arg(&tone);
        ffmpeg.checked(&mut generate, "generate tone")?;
        let clips = (0..65)
            .map(|i| clip(tone.clone(), 50 + i * 250))
            .collect::<Vec<_>>();
        let reference_filter = dir.join("reference.txt");
        fs::write(&reference_filter, mix_filter(&clips, 0, None, true)?)?;
        let reference = dir.join("reference.mp3");
        render_mix(&ffmpeg, &clips, &reference_filter, &reference, false)?;
        let chunked = dir.join("chunked.mp3");
        render_voiceover(&ffmpeg, &clips, &chunked, &dir, "filter.txt")?;
        let decode = |path: &Path| -> Result<Vec<f32>> {
            let mut command = ffmpeg.ffmpeg_command();
            let output = command
                .arg("-i")
                .arg(path)
                .args(["-f", "f32le", "-acodec", "pcm_f32le", "-"])
                .output()?;
            assert!(output.status.success());
            Ok(output
                .stdout
                .chunks_exact(4)
                .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()))
                .collect())
        };
        let expected = decode(&reference)?;
        let actual = decode(&chunked)?;
        assert_eq!(actual.len(), expected.len());
        let difference = actual
            .iter()
            .zip(&expected)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f32, f32::max);
        assert!(
            difference < 0.002,
            "chunk boundary waveform difference: {difference}"
        );
        assert!(!dir.join("voiceover-chunks/chunk-00000.wav").exists());
        fs::remove_dir_all(dir)?;
        Ok(())
    }

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
    fn allows_one_millisecond_speech_end_rounding_past_the_slot() {
        let clamp = pad_clamp(15_041, 15_042, 1.0, Some(15_042)).unwrap();
        assert_eq!(clamp.available_s, 15.041);
        assert_eq!(clamp.atrim_duration_s, Some(15.041));
        assert!(pad_clamp(15_041, 15_200, 1.0, Some(15_200)).is_err());
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
