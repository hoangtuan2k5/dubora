use anyhow::{Context, Result, bail};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

use crate::config::{DEFAULT_AZURE_REGION, validate_azure_voice};

/// One Azure TTS voice from the regional voices/list API.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureVoice {
    pub short_name: String,
    pub display_name: String,
    pub local_name: String,
    pub gender: String,
    pub locale: String,
    pub locale_name: String,
    pub status: String,
    pub voice_type: String,
    pub styles: Vec<String>,
    pub secondary_locales: Vec<String>,
    pub words_per_minute: Option<u16>,
    pub capabilities: VoiceCapabilities,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityState {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VoiceModelFamily {
    Standard,
    DragonHd,
    DragonHdOmni,
    DragonHdFlash,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceCapabilities {
    pub model_family: VoiceModelFamily,
    pub multilingual: bool,
    pub style: CapabilityState,
    pub manual_emphasis: CapabilityState,
    pub manual_prosody: CapabilityState,
    pub semantic_break: CapabilityState,
    pub pronunciation: CapabilityState,
    pub automatic_delivery: CapabilityState,
}

#[derive(Debug, Deserialize)]
struct AzureVoiceDto {
    #[serde(rename = "ShortName", default)]
    short_name: String,
    #[serde(rename = "DisplayName", default)]
    display_name: String,
    #[serde(rename = "LocalName", default)]
    local_name: String,
    #[serde(rename = "Gender", default)]
    gender: String,
    #[serde(rename = "Locale", default)]
    locale: String,
    #[serde(rename = "LocaleName", default)]
    locale_name: String,
    #[serde(rename = "Status", default)]
    status: String,
    #[serde(rename = "VoiceType", default)]
    voice_type: String,
    #[serde(rename = "StyleList", default)]
    styles: Option<Vec<String>>,
    #[serde(rename = "SecondaryLocaleList", default)]
    secondary_locales: Option<Vec<String>>,
    #[serde(rename = "WordsPerMinute", default)]
    words_per_minute: Option<WordsPerMinute>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WordsPerMinute {
    Number(u16),
    Text(String),
}

impl WordsPerMinute {
    fn parse(&self) -> Option<u16> {
        match self {
            Self::Number(value) => Some(*value),
            Self::Text(value) => value.trim().parse().ok(),
        }
    }
}

impl AzureVoice {
    fn capabilities(
        short_name: &str,
        styles: &[String],
        style_metadata_available: bool,
        secondary_locales: &[String],
    ) -> VoiceCapabilities {
        let model_family = model_family(short_name);
        let style = if style_metadata_available {
            if styles.is_empty() {
                CapabilityState::Unsupported
            } else {
                CapabilityState::Supported
            }
        } else {
            CapabilityState::Unknown
        };
        let multilingual = secondary_locales.iter().any(|locale| locale == "vi-VN")
            || short_name.to_ascii_lowercase().contains("multilingual")
            || matches!(model_family, VoiceModelFamily::DragonHd | VoiceModelFamily::DragonHdOmni);
        let (manual_emphasis, manual_prosody, semantic_break, pronunciation, automatic_delivery) =
            match model_family {
                VoiceModelFamily::Standard => (
                    CapabilityState::Supported,
                    CapabilityState::Supported,
                    CapabilityState::Supported,
                    CapabilityState::Supported,
                    CapabilityState::Unknown,
                ),
                VoiceModelFamily::DragonHd => (
                    CapabilityState::Unknown,
                    CapabilityState::Unknown,
                    CapabilityState::Supported,
                    CapabilityState::Supported,
                    CapabilityState::Supported,
                ),
                VoiceModelFamily::DragonHdOmni => (
                    CapabilityState::Unsupported,
                    CapabilityState::Unsupported,
                    CapabilityState::Unsupported,
                    CapabilityState::Supported,
                    CapabilityState::Supported,
                ),
                VoiceModelFamily::DragonHdFlash | VoiceModelFamily::Unknown => (
                    CapabilityState::Unknown,
                    CapabilityState::Unknown,
                    CapabilityState::Unknown,
                    CapabilityState::Unknown,
                    CapabilityState::Unknown,
                ),
            };
        VoiceCapabilities {
            model_family,
            multilingual,
            style,
            manual_emphasis,
            manual_prosody,
            semantic_break,
            pronunciation,
            automatic_delivery,
        }
    }
}

fn model_family(short_name: &str) -> VoiceModelFamily {
    let name = short_name.to_ascii_lowercase();
    if name.contains(":dragonhdomni") {
        VoiceModelFamily::DragonHdOmni
    } else if name.contains(":dragonhdflash") {
        VoiceModelFamily::DragonHdFlash
    } else if name.contains(":dragonhd") {
        VoiceModelFamily::DragonHd
    } else if name.ends_with("neural") {
        VoiceModelFamily::Standard
    } else {
        VoiceModelFamily::Unknown
    }
}

/// Fetch voices available for the Speech resource region.
pub async fn list_azure_voices(key: &str, region: &str) -> Result<Vec<AzureVoice>> {
    let key = key.trim();
    if key.is_empty() {
        bail!("Missing Azure Speech key");
    }
    let region = {
        let region = region.trim();
        if region.is_empty() {
            DEFAULT_AZURE_REGION
        } else {
            region
        }
    };
    if !region
        .chars()
        .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-')
        || region.starts_with('-')
        || region.ends_with('-')
    {
        bail!("Invalid AZURE_REGION: use lowercase letters, digits, and hyphens only");
    }

    let url = format!("https://{region}.tts.speech.microsoft.com/cognitiveservices/voices/list");
    let client = Client::new();
    let response = client
        .get(&url)
        .header("Ocp-Apim-Subscription-Key", key)
        .send()
        .await
        .with_context(|| format!("Failed to request voice list from {url}"))?;

    if response.status() != StatusCode::OK {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(300).collect();
        bail!("Azure voices/list failed: {status} {detail}");
    }

    let body = response
        .text()
        .await
        .context("Failed to read Azure voices/list body")?;
    let rows: Vec<AzureVoiceDto> =
        serde_json::from_str(&body).context("Failed to parse Azure voices/list JSON")?;

    let mut voices: Vec<AzureVoice> = rows
        .into_iter()
        .filter_map(|row| {
            let short_name = row.short_name.trim().to_owned();
            if short_name.is_empty() || validate_azure_voice(&short_name).is_err() {
                return None;
            }
            let style_metadata_available = row.styles.is_some();
            let styles: Vec<_> = row
                .styles
                .unwrap_or_default()
                .into_iter()
                .map(|style| style.trim().to_owned())
                .filter(|style| !style.is_empty())
                .collect();
            let secondary_locales: Vec<_> = row
                .secondary_locales
                .unwrap_or_default()
                .into_iter()
                .map(|locale| locale.trim().to_owned())
                .filter(|locale| !locale.is_empty())
                .collect();
            let capabilities = AzureVoice::capabilities(
                &short_name,
                &styles,
                style_metadata_available,
                &secondary_locales,
            );
            Some(AzureVoice {
                short_name,
                display_name: row.display_name.trim().to_owned(),
                local_name: row.local_name.trim().to_owned(),
                gender: row.gender.trim().to_owned(),
                locale: row.locale.trim().to_owned(),
                locale_name: row.locale_name.trim().to_owned(),
                status: row.status.trim().to_owned(),
                voice_type: row.voice_type.trim().to_owned(),
                styles,
                secondary_locales,
                words_per_minute: row.words_per_minute.as_ref().and_then(WordsPerMinute::parse),
                capabilities,
            })
        })
        .collect();

    voices.sort_by(|a, b| {
        a.locale
            .cmp(&b.locale)
            .then_with(|| a.local_name.cmp(&b.local_name))
            .then_with(|| a.short_name.cmp(&b.short_name))
    });
    Ok(voices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample_azure_voice_row() {
        let json = r#"[
          {
            "Name": "Microsoft Server Speech Text to Speech Voice (en-US, JennyNeural)",
            "DisplayName": "Jenny",
            "LocalName": "Jenny",
            "ShortName": "en-US-JennyNeural",
            "Gender": "Female",
            "Locale": "en-US",
            "LocaleName": "English (United States)",
            "VoiceType": "Neural",
            "Status": "GA",
            "StyleList": ["chat", "newscast-casual"],
            "SecondaryLocaleList": ["vi-VN"],
            "WordsPerMinute": "152"
          },
          {
            "ShortName": "de-DE-Florian:DragonHDLatestNeural",
            "DisplayName": "Florian",
            "LocalName": "Florian",
            "Gender": "Male",
            "Locale": "de-DE",
            "LocaleName": "German (Germany)",
            "VoiceType": "Neural",
            "Status": "GA"
          }
        ]"#;
        let rows: Vec<AzureVoiceDto> = serde_json::from_str(json).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].short_name, "de-DE-Florian:DragonHDLatestNeural");
        assert_eq!(
            rows[0].styles.as_ref().unwrap(),
            &vec!["chat".to_owned(), "newscast-casual".to_owned()]
        );
        assert_eq!(
            rows[0].secondary_locales.as_ref().unwrap(),
            &vec!["vi-VN".to_owned()]
        );
        assert_eq!(rows[0].words_per_minute.as_ref().unwrap().parse(), Some(152));
    }

    #[test]
    fn derives_effective_capabilities_without_overclaiming_hd_controls() {
        let standard = AzureVoice::capabilities(
            "vi-VN-HoaiMyNeural",
            &["chat".to_owned()],
            true,
            &[],
        );
        assert_eq!(standard.model_family, VoiceModelFamily::Standard);
        assert_eq!(standard.manual_prosody, CapabilityState::Supported);

        let hd = AzureVoice::capabilities(
            "de-DE-Florian:DragonHDLatestNeural",
            &[],
            true,
            &[],
        );
        assert_eq!(hd.model_family, VoiceModelFamily::DragonHd);
        assert_eq!(hd.manual_emphasis, CapabilityState::Unknown);
        assert_eq!(hd.semantic_break, CapabilityState::Supported);

        let omni = AzureVoice::capabilities(
            "en-US-Ava:DragonHDOmniLatestNeural",
            &[],
            false,
            &[],
        );
        assert_eq!(omni.model_family, VoiceModelFamily::DragonHdOmni);
        assert_eq!(omni.style, CapabilityState::Unknown);
        assert_eq!(omni.manual_prosody, CapabilityState::Unsupported);
    }
}
