use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::ffmpeg::FfmpegPaths;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudioPolishOptions {
    pub duck_original: bool,
    pub loudness_normalize: bool,
    pub voiceover_eq: bool,
}

pub fn build_dubbed_video(
    ffmpeg: &FfmpegPaths,
    input_video: &Path,
    voiceover_audio: &Path,
    output_video: &Path,
    options: AudioPolishOptions,
) -> Result<()> {
    for path in [input_video, voiceover_audio] {
        if !path.is_file() {
            bail!("Input file does not exist: {}", path.display());
        }
    }

    let temporary_video = output_video.with_extension("tmp.mp4");
    let _ = fs::remove_file(&temporary_video);
    let source_has_audio = if options.duck_original {
        let has_audio = ffmpeg.has_audio_stream(input_video)?;
        if !has_audio {
            eprintln!("Source video has no audio; ignoring --duck-original.");
        }
        has_audio
    } else {
        false
    };
    let loudness_normalize =
        options.loudness_normalize && should_apply_loudness_normalization(ffmpeg, voiceover_audio);
    let mut command = ffmpeg.ffmpeg_command();
    command
        .arg("-y")
        .arg("-i")
        .arg(input_video)
        .arg("-i")
        .arg(voiceover_audio)
        .args(audio_mux_args(
            source_has_audio,
            options,
            loudness_normalize,
        ))
        .arg(&temporary_video);

    if let Err(error) = ffmpeg.checked(&mut command, "build dubbed video") {
        let _ = fs::remove_file(&temporary_video);
        return Err(error).with_context(|| format!("Failed to create {}", output_video.display()));
    }
    replace_output(&temporary_video, output_video)
}

fn audio_mux_args(
    source_has_audio: bool,
    options: AudioPolishOptions,
    loudness_normalize: bool,
) -> Vec<String> {
    let duck_original = options.duck_original && source_has_audio;
    let mut voiceover_filters = Vec::new();
    if options.voiceover_eq {
        voiceover_filters.push("highpass=f=80:poles=1");
    }
    if loudness_normalize {
        voiceover_filters.push("loudnorm=I=-17:LRA=11:TP=-1.5");
    }

    if duck_original {
        let voiceover = if voiceover_filters.is_empty() {
            "anull".to_owned()
        } else {
            voiceover_filters.join(",")
        };
        vec![
            "-filter_complex".to_owned(),
            format!(
                "[0:a:0]volume=-22dB[bed];[1:a:0]{voiceover}[vo];[bed][vo]amix=inputs=2:duration=first:dropout_transition=0:normalize=0,alimiter=limit=0.95[a]"
            ),
            "-map".to_owned(),
            "0:v:0".to_owned(),
            "-map".to_owned(),
            "[a]".to_owned(),
            "-c:v".to_owned(),
            "copy".to_owned(),
            "-c:a".to_owned(),
            "aac".to_owned(),
            "-b:a".to_owned(),
            "192k".to_owned(),
            "-shortest".to_owned(),
            "-metadata:s:a:0".to_owned(),
            "language=vie".to_owned(),
            "-disposition:a:0".to_owned(),
            "default".to_owned(),
        ]
    } else {
        voiceover_filters.push("apad");
        vec![
            "-map".to_owned(),
            "0:v:0".to_owned(),
            "-map".to_owned(),
            "1:a:0".to_owned(),
            "-c:v".to_owned(),
            "copy".to_owned(),
            "-c:a".to_owned(),
            "aac".to_owned(),
            "-b:a".to_owned(),
            "192k".to_owned(),
            "-af".to_owned(),
            voiceover_filters.join(","),
            "-shortest".to_owned(),
            "-metadata:s:a:0".to_owned(),
            "language=vie".to_owned(),
            "-disposition:a:0".to_owned(),
            "default".to_owned(),
        ]
    }
}

fn should_apply_loudness_normalization(ffmpeg: &FfmpegPaths, voiceover_audio: &Path) -> bool {
    let mut command = ffmpeg.ffmpeg_command();
    let output = command
        .arg("-i")
        .arg(voiceover_audio)
        .args([
            "-af",
            "loudnorm=I=-17:LRA=11:TP=-1.5:print_format=json",
            "-f",
            "null",
            "-",
        ])
        .output();
    let Ok(output) = output else {
        eprintln!("Could not measure voiceover loudness; applying --loudness-normalize.");
        return true;
    };
    if !output.status.success() {
        eprintln!("Could not measure voiceover loudness; applying --loudness-normalize.");
        return true;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let Some(input_lufs) = parse_input_integrated_loudness(&stderr) else {
        eprintln!("Could not parse voiceover loudness; applying --loudness-normalize.");
        return true;
    };
    !(-18.0..=-16.0).contains(&input_lufs)
}

fn parse_input_integrated_loudness(stderr: &str) -> Option<f64> {
    let start = stderr.rfind('{')?;
    let end = stderr.rfind('}')?;
    let report: Value = serde_json::from_str(&stderr[start..=end]).ok()?;
    report.get("input_i")?.as_str()?.parse().ok()
}

fn replace_output(temporary: &Path, output: &Path) -> Result<()> {
    let backup = output.with_extension("previous.mp4");
    match fs::rename(temporary, output) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(_error) if output.exists() => {
            if backup.exists() {
                fs::remove_file(&backup)
                    .with_context(|| format!("Failed to remove {}", backup.display()))?;
            }
            fs::rename(output, &backup)
                .with_context(|| format!("Failed to preserve {}", output.display()))?;
            match fs::rename(temporary, output) {
                Ok(()) => {
                    let _ = fs::remove_file(backup);
                    Ok(())
                }
                Err(replace_error) => {
                    fs::rename(&backup, output).with_context(|| {
                        format!("Failed to restore previous output after {replace_error}")
                    })?;
                    Err(replace_error)
                        .with_context(|| format!("Failed to replace {}", output.display()))
                }
            }
        }
        Err(error) => Err(error)
            .with_context(|| format!("Failed to move completed video to {}", output.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn replaces_an_existing_output_only_after_temp_is_ready() -> Result<()> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("dubora-video-{unique}"));
        fs::create_dir_all(&directory)?;
        let output = directory.join("lesson.vi-dub.mp4");
        let temporary = directory.join("lesson.vi-dub.tmp.mp4");
        fs::write(&output, b"old")?;
        fs::write(&temporary, b"new")?;

        replace_output(&temporary, &output)?;
        assert_eq!(fs::read(&output)?, b"new");
        assert!(!temporary.exists());

        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn default_mux_keeps_the_voiceover_only_command() {
        let args = audio_mux_args(false, AudioPolishOptions::default(), false);
        assert!(args.windows(2).any(|pair| pair == ["-map", "1:a:0"]));
        assert!(args.windows(2).any(|pair| pair == ["-af", "apad"]));
        assert!(!args.iter().any(|arg| arg == "-filter_complex"));
        assert!(!args.iter().any(|arg| arg.contains("loudnorm")));
    }

    #[test]
    fn duck_flag_matches_the_default_mux_when_source_has_no_audio() {
        let args = audio_mux_args(
            false,
            AudioPolishOptions {
                duck_original: true,
                ..AudioPolishOptions::default()
            },
            false,
        );
        assert_eq!(
            args,
            audio_mux_args(false, AudioPolishOptions::default(), false)
        );
    }

    #[test]
    fn duck_graph_mixes_a_quiet_source_bed_after_voiceover_filters() {
        let args = audio_mux_args(
            true,
            AudioPolishOptions {
                duck_original: true,
                loudness_normalize: true,
                voiceover_eq: true,
            },
            true,
        );
        let graph = args
            .windows(2)
            .find(|pair| pair[0] == "-filter_complex")
            .map(|pair| pair[1].as_str())
            .unwrap();
        assert!(graph.contains("volume=-22dB"));
        assert!(graph.contains("highpass=f=80:poles=1,loudnorm=I=-17:LRA=11:TP=-1.5"));
        assert!(graph.contains("[bed][vo]amix=inputs=2:duration=first:dropout_transition=0:normalize=0,alimiter=limit=0.95[a]"));
        assert!(args.windows(2).any(|pair| pair == ["-map", "[a]"]));
        assert!(!graph.contains("[a]loudnorm"));
    }

    #[test]
    fn loudness_parser_reads_the_loudnorm_input_value() {
        assert_eq!(
            parse_input_integrated_loudness("noise {\"input_i\": \"-17.2\"}"),
            Some(-17.2)
        );
        assert_eq!(parse_input_integrated_loudness("no report"), None);
    }
}
