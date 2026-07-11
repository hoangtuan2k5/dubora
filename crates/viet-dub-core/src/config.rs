use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::ffmpeg::FfmpegPaths;

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
}

#[derive(Clone, Debug)]
pub struct DubConfig {
    pub root: PathBuf,
    pub provider: Provider,
    pub azure: Option<AzureConfig>,
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
}
