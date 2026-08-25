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
            ffmpeg: std::env::var_os("DUBORA_FFMPEG")
                .map(PathBuf::from)
                .or(bundled_ffmpeg)
                .unwrap_or_else(|| PathBuf::from(ffmpeg_name())),
            ffprobe: std::env::var_os("DUBORA_FFPROBE")
                .map(PathBuf::from)
                .or(bundled_ffprobe)
                .unwrap_or_else(|| PathBuf::from(ffprobe_name())),
        }
    }

    pub fn verify_rubberband(&self) -> Result<()> {
        let output = base_command(&self.ffmpeg)
            .args(["-hide_banner", "-filters"])
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
        let output = base_command(&self.ffprobe)
            .arg("-version")
            .output()
            .with_context(|| format!("Failed to run {}", self.ffprobe.display()))?;
        ensure_success(&output, "ffprobe -version")
    }

    pub fn audio_duration(&self, path: &Path) -> Result<f64> {
        let output = base_command(&self.ffprobe)
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

    pub(crate) fn has_audio_stream(&self, path: &Path) -> Result<bool> {
        let output = base_command(&self.ffprobe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "a:0",
                "-show_entries",
                "stream=index",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .with_context(|| format!("Failed to probe source audio {}", path.display()))?;
        ensure_success(&output, "ffprobe source audio")?;
        Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
    }

    pub fn is_valid_audio(&self, path: &Path) -> bool {
        path.metadata().is_ok_and(|metadata| metadata.len() > 0)
            && self
                .audio_duration(path)
                .is_ok_and(|duration| duration > 0.05)
    }

    pub fn create_silence(&self, output: &Path) -> Result<()> {
        let result = self
            .ffmpeg_command()
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

    /// Extract a small, deterministic mono PCM source track for opt-in word timing analysis.
    pub fn extract_mono_wav(&self, input: &Path, output: &Path) -> Result<()> {
        let result = self
            .ffmpeg_command()
            .args(["-y", "-i"])
            .arg(input)
            .args([
                "-map",
                "0:a:0",
                "-vn",
                "-ac",
                "1",
                "-ar",
                "16000",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(output)
            .output()
            .with_context(|| format!("Failed to extract source audio from {}", input.display()))?;
        ensure_success(&result, "extract mono source WAV")
    }

    pub(crate) fn checked(&self, command: &mut Command, label: &str) -> Result<()> {
        let output = command
            .output()
            .with_context(|| format!("Failed to run {}", self.ffmpeg.display()))?;
        ensure_success(&output, label)
    }

    /// FFmpeg process for encode/filter work (CPU only on Windows NVIDIA hybrid).
    pub(crate) fn ffmpeg_command(&self) -> Command {
        let mut command = base_command(&self.ffmpeg);
        // Force software path: no D3D11/CUDA hwaccel fighting WebView2 on dGPU.
        command.args(["-hide_banner", "-nostdin", "-hwaccel", "none"]);
        command
    }
}

fn base_command(program: &Path) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        let mut command = Command::new(program);
        // Hide console flashes that can disrupt WebView2 composition.
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        // Keep child tools off the discrete NVIDIA GPU when possible. Hybrid
        // laptops (RTX 2050 + Intel iGPU) often black-out Tauri/WebView2 if
        // FFmpeg also binds the dGPU. Pipeline is CPU audio + stream-copy.
        command.env("CUDA_VISIBLE_DEVICES", "-1");
        command.env_remove("CUDA_DEVICE_ORDER");
        command
    }
    #[cfg(not(windows))]
    {
        Command::new(program)
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
