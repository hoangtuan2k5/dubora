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
            Some(AzureVoice {
                short_name,
                display_name: row.display_name.trim().to_owned(),
                local_name: row.local_name.trim().to_owned(),
                gender: row.gender.trim().to_owned(),
                locale: row.locale.trim().to_owned(),
                locale_name: row.locale_name.trim().to_owned(),
                status: row.status.trim().to_owned(),
                voice_type: row.voice_type.trim().to_owned(),
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
            "Status": "GA"
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
    }
}
