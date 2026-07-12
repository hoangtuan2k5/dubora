use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::ffmpeg::FfmpegPaths;

pub fn build_dubbed_video(
    ffmpeg: &FfmpegPaths,
    input_video: &Path,
    voiceover_audio: &Path,
    output_video: &Path,
) -> Result<()> {
    for path in [input_video, voiceover_audio] {
        if !path.is_file() {
            bail!("Input file does not exist: {}", path.display());
        }
    }

    let temporary_video = output_video.with_extension("tmp.mp4");
    let _ = fs::remove_file(&temporary_video);
    let mut command = ffmpeg.ffmpeg_command();
    command
        .arg("-y")
        .arg("-i")
        .arg(input_video)
        .arg("-i")
        .arg(voiceover_audio)
        .args([
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c:v",
            "copy",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-af",
            "apad",
            "-shortest",
            "-metadata:s:a:0",
            "language=vie",
            "-disposition:a:0",
            "default",
        ])
        .arg(&temporary_video);

    if let Err(error) = ffmpeg.checked(&mut command, "build dubbed video") {
        let _ = fs::remove_file(&temporary_video);
        return Err(error).with_context(|| format!("Failed to create {}", output_video.display()));
    }
    replace_output(&temporary_video, output_video)
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
        let directory = std::env::temp_dir().join(format!("viet-dub-video-{unique}"));
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
}
