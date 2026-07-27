use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::ffmpeg::FfmpegPaths;

/// Default Azure TTS voice for Vietnamese course dubbing (Florian Dragon HD).
pub const AZURE_VOICE: &str = "de-DE-Florian:DragonHDLatestNeural";
pub const DEFAULT_AZURE_REGION: &str = "southeastasia";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Azure,
    Edge,
}

#[derive(Clone, Debug)]
pub struct AzureConfig {
    pub key: String,
    pub region: String,
    pub voice: String,
}

#[derive(Clone, Debug)]
pub struct GeminiConfig { pub keys: Vec<String> }

impl GeminiConfig {
    pub fn new(keys: Vec<String>) -> Result<Self> {
        let keys: Vec<_> = keys.into_iter().map(|key| key.trim().to_owned()).filter(|key| !key.is_empty()).collect();
        if keys.is_empty() { bail!("Missing GEMINI_API_KEYS. Add one or more Gemini API keys"); }
        Ok(Self { keys })
    }
}

impl AzureConfig {
    pub fn new(key: impl Into<String>, region: impl Into<String>) -> Result<Self> {
        let key = key.into();
        if key.trim().is_empty() {
            bail!("Missing AZURE_KEY. Add it to <root>/.env or export it");
        }
        let region = region.into();
        let region = if region.trim().is_empty() {
            DEFAULT_AZURE_REGION.to_owned()
        } else {
            region
        };
        if !region
            .chars()
            .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-')
            || region.starts_with('-')
            || region.ends_with('-')
        {
            bail!("Invalid AZURE_REGION: use lowercase letters, digits, and hyphens only");
        }
        Ok(Self {
            key,
            region,
            voice: AZURE_VOICE.to_owned(),
        })
    }

    /// Override TTS voice. Empty / whitespace keeps the default Florian Dragon HD voice.
    pub fn with_voice(mut self, voice: impl Into<String>) -> Result<Self> {
        let voice = voice.into();
        let voice = voice.trim();
        if voice.is_empty() {
            self.voice = AZURE_VOICE.to_owned();
            return Ok(self);
        }
        validate_azure_voice(voice)?;
        self.voice = voice.to_owned();
        Ok(self)
    }
}

/// Azure ShortName-safe characters (e.g. `de-DE-Florian:DragonHDLatestNeural`).
pub fn validate_azure_voice(voice: &str) -> Result<()> {
    if voice.is_empty() {
        bail!("Azure voice cannot be empty");
    }
    if voice.len() > 128 {
        bail!("Azure voice name is too long");
    }
    if !voice
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
    {
        bail!("Invalid Azure voice name: use letters, digits, '-', '_', ':', '.' only");
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct DubConfig {
    pub root: PathBuf,
    pub provider: Provider,
    pub azure: Option<AzureConfig>,
    pub gemini: Option<GeminiConfig>,
    pub ffmpeg: FfmpegPaths,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DubOptions {
    pub force: bool,
    pub force_tts: bool,
    pub translate: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_region_before_it_can_enter_the_azure_url() {
        assert!(AzureConfig::new("key", "southeastasia").is_ok());
        assert!(AzureConfig::new("key", "westus2").is_ok());
        assert!(AzureConfig::new("key", "attacker.example#").is_err());
        assert!(AzureConfig::new("key", "evil@host").is_err());
    }

    #[test]
    fn default_voice_is_florian_dragon_hd() {
        let config = AzureConfig::new("key", "southeastasia").unwrap();
        assert_eq!(config.voice, AZURE_VOICE);
        assert_eq!(config.voice, "de-DE-Florian:DragonHDLatestNeural");
    }

    #[test]
    fn empty_voice_keeps_default() {
        let config = AzureConfig::new("key", "southeastasia")
            .unwrap()
            .with_voice("  ")
            .unwrap();
        assert_eq!(config.voice, AZURE_VOICE);
    }

    #[test]
    fn accepts_hd_voice_short_names() {
        let config = AzureConfig::new("key", "southeastasia")
            .unwrap()
            .with_voice("vi-VN-HoaiMyNeural")
            .unwrap();
        assert_eq!(config.voice, "vi-VN-HoaiMyNeural");
        assert!(
            AzureConfig::new("key", "southeastasia")
                .unwrap()
                .with_voice("de-DE-Florian:DragonHDLatestNeural")
                .is_ok()
        );
        assert!(
            AzureConfig::new("key", "southeastasia")
                .unwrap()
                .with_voice("bad voice!")
                .is_err()
        );
    }
}
