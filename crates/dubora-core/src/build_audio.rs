use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::ffmpeg::FfmpegPaths;
use crate::srt::{parse_srt, time_to_seconds};

pub fn build_voiceover_track(
    ffmpeg: &FfmpegPaths,
    input_srt: &Path,
    tts_dir: &Path,
    output_audio: &Path,
    work_dir: &Path,
) -> Result<()> {
    build_voiceover_track_with_tempo_cap(ffmpeg, input_srt, tts_dir, output_audio, work_dir, None)
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
    for (position, (segment, mp3)) in segments.iter().zip(&inputs).enumerate() {
        let start = time_to_seconds(&segment.start)?;
        let slot_end = if let Some(next) = segments.get(position + 1) {
            time_to_seconds(&next.start)?
        } else {
            time_to_seconds(&segment.end)?
        };
        let available = (slot_end - start - 0.08).max(0.1);
        let tempo = (ffmpeg.audio_duration(mp3)? / available).max(1.0);
        if let Some(cap) = max_tempo.filter(|cap| tempo > *cap) {
            bail!(
                "Segment {} requires {tempo:.2}x stretch, above the {cap:.2}x Natural Voice cap",
                segment.index
            );
        }
        let delay_ms = (start * 1000.0) as u64;

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
