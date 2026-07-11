use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug)]
pub struct FfmpegPaths {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl FfmpegPaths {
    pub fn resolve(resource_dir: Option<&Path>) -> Self {
        let bundled_ffmpeg = resource_dir
            .map(|dir| dir.join("ffmpeg").join(ffmpeg_name()))
            .filter(|path| path.is_file());
        let bundled_ffprobe = resource_dir
            .map(|dir| dir.join("ffmpeg").join(ffprobe_name()))
            .filter(|path| path.is_file());
        Self {
            ffmpeg: std::env::var_os("VIET_DUB_FFMPEG")
                .map(PathBuf::from)
                .or(bundled_ffmpeg)
                .unwrap_or_else(|| PathBuf::from(ffmpeg_name())),
            ffprobe: std::env::var_os("VIET_DUB_FFPROBE")
                .map(PathBuf::from)
                .or(bundled_ffprobe)
                .unwrap_or_else(|| PathBuf::from(ffprobe_name())),
        }
    }

    pub fn verify_rubberband(&self) -> Result<()> {
        let output = Command::new(&self.ffmpeg)
            .arg("-filters")
            .output()
            .with_context(|| format!("Failed to run {}", self.ffmpeg.display()))?;
        ensure_success(&output, "ffmpeg -filters")?;
        if !has_rubberband_filter(&String::from_utf8_lossy(&output.stdout)) {
            bail!(
                "FFmpeg at {} does not include the required rubberband filter",
                self.ffmpeg.display()
            );
        }
        Ok(())
    }

    pub fn verify(&self) -> Result<()> {
        self.verify_rubberband()?;
        let output = Command::new(&self.ffprobe)
            .arg("-version")
            .output()
            .with_context(|| format!("Failed to run {}", self.ffprobe.display()))?;
        ensure_success(&output, "ffprobe -version")
    }

    pub fn audio_duration(&self, path: &Path) -> Result<f64> {
        let output = Command::new(&self.ffprobe)
            .args([
                OsString::from("-v"),
                OsString::from("error"),
                OsString::from("-show_entries"),
                OsString::from("format=duration"),
                OsString::from("-of"),
                OsString::from("default=noprint_wrappers=1:nokey=1"),
            ])
            .arg(path)
            .output()
            .with_context(|| format!("Failed to probe {}", path.display()))?;
        ensure_success(&output, "ffprobe duration")?;
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .with_context(|| format!("Invalid audio duration for {}", path.display()))
    }

    pub fn is_valid_audio(&self, path: &Path) -> bool {
        path.metadata().is_ok_and(|metadata| metadata.len() > 0)
            && self
                .audio_duration(path)
                .is_ok_and(|duration| duration > 0.05)
    }

    pub fn create_silence(&self, output: &Path) -> Result<()> {
        let result = Command::new(&self.ffmpeg)
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=mono",
                "-t",
                "0.1",
                "-q:a",
                "9",
                "-acodec",
                "libmp3lame",
            ])
            .arg(output)
            .output()
            .with_context(|| format!("Failed to run {}", self.ffmpeg.display()))?;
        ensure_success(&result, "create silent MP3")
    }

    pub(crate) fn checked(&self, command: &mut Command, label: &str) -> Result<()> {
        let output = command
            .output()
            .with_context(|| format!("Failed to run {}", self.ffmpeg.display()))?;
        ensure_success(&output, label)
    }
}

fn ensure_success(output: &Output, label: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{label} failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn has_rubberband_filter(filters: &str) -> bool {
    filters.lines().any(|line| {
        let mut columns = line.split_whitespace();
        columns.next().is_some() && columns.next() == Some("rubberband")
    })
}

#[cfg(windows)]
fn ffmpeg_name() -> &'static str {
    "ffmpeg.exe"
}

#[cfg(not(windows))]
fn ffmpeg_name() -> &'static str {
    "ffmpeg"
}

#[cfg(windows)]
fn ffprobe_name() -> &'static str {
    "ffprobe.exe"
}

#[cfg(not(windows))]
fn ffprobe_name() -> &'static str {
    "ffprobe"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_filter_row_not_build_flags() {
        assert!(has_rubberband_filter(
            " ..C rubberband        A->A       Apply time-stretching"
        ));
        assert!(!has_rubberband_filter(
            "configuration: --disable-librubberband"
        ));
    }
}
