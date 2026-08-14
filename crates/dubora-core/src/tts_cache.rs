use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::ffmpeg::FfmpegPaths;

pub fn tts_cache_key(provider: &str, voice: &str, style: &str, text: &str) -> String {
    let payload = if style.trim().is_empty() {
        format!("{provider}\n{voice}\n{}", text.trim())
    } else {
        format!("{provider}\n{voice}\n{}\n{}", style.trim(), text.trim())
    };
    format!("{:x}", Sha256::digest(payload.as_bytes()))
}

pub fn tts_cache_key_path(audio_path: &Path) -> PathBuf {
    let mut path = OsString::from(audio_path.as_os_str());
    path.push(".sha256");
    PathBuf::from(path)
}

pub fn write_tts_cache_key(audio_path: &Path, cache_key: &str) -> Result<()> {
    fs::write(tts_cache_key_path(audio_path), cache_key)
        .with_context(|| format!("Failed to write cache key for {}", audio_path.display()))
}

pub fn has_matching_tts_cache_key(audio_path: &Path, cache_key: &str) -> bool {
    fs::read_to_string(tts_cache_key_path(audio_path))
        .is_ok_and(|stored| stored.trim() == cache_key)
}

pub fn is_fresh_audio_file(ffmpeg: &FfmpegPaths, audio_path: &Path, cache_key: &str) -> bool {
    ffmpeg.is_valid_audio(audio_path) && has_matching_tts_cache_key(audio_path, cache_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_matches_python_payload() {
        assert_eq!(
            tts_cache_key(
                "azure",
                "de-DE-Florian:DragonHDLatestNeural",
                "",
                "  Xin chào\n",
            ),
            "8b2c0952701d669d691f33f940935e949f1fe334538ad33cd84003bd1a928d7d"
        );
    }

    #[test]
    fn style_invalidates_the_cache() {
        assert_ne!(
            tts_cache_key("azure", "voice", "", "Xin chào"),
            tts_cache_key("azure", "voice", "chat", "Xin chào")
        );
    }

    #[test]
    fn sidecar_keeps_the_audio_extension() {
        assert_eq!(
            tts_cache_key_path(Path::new("tts/seg_7.mp3")),
            Path::new("tts/seg_7.mp3.sha256")
        );
    }
}
