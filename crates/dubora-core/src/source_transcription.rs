//! Optional Azure Fast Transcription source timing.
//!
//! This module is deliberately opt-in.  The normal source-aligned path uses the downloaded
//! captions and does not upload source audio.  When requested, this client asks Azure for
//! word-level timestamps and returns only the small timing projection consumed by
//! `sync_timeline`.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::AzureConfig;
use crate::metrics;
use crate::sync_timeline::GranularSourceWord;

const API_VERSION: &str = "2025-10-15";
const MAX_AUDIO_BYTES: u64 = 250 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscribeResult {
    #[serde(default)]
    phrases: Vec<TranscribePhrase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscribePhrase {
    #[serde(default)]
    words: Vec<TranscribeWord>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscribeWord {
    text: String,
    offset_milliseconds: u64,
    duration_milliseconds: u64,
}

/// Transcribe a local audio file with Azure's synchronous word-timestamp endpoint.
pub async fn transcribe_audio(
    config: &AzureConfig,
    audio_path: &Path,
    locale: &str,
) -> Result<Vec<GranularSourceWord>> {
    let metadata = tokio::fs::metadata(audio_path)
        .await
        .with_context(|| format!("Failed to inspect source audio {}", audio_path.display()))?;
    if metadata.len() == 0 {
        bail!("Source audio is empty");
    }
    if metadata.len() > MAX_AUDIO_BYTES {
        bail!(
            "Source audio is {} bytes; Azure Fast Transcription accepts at most {} bytes",
            metadata.len(),
            MAX_AUDIO_BYTES
        );
    }
    let audio = tokio::fs::read(audio_path)
        .await
        .with_context(|| format!("Failed to read source audio {}", audio_path.display()))?;
    let locale = locale.trim();
    if locale.is_empty() {
        bail!("Source transcription locale cannot be empty");
    }

    let endpoint = std::env::var("AZURE_SPEECH_ENDPOINT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("https://{}.api.cognitive.microsoft.com", config.region));
    let url = format!(
        "{}/speechtotext/transcriptions:transcribe?api-version={API_VERSION}",
        endpoint.trim_end_matches('/')
    );
    let definition = serde_json::json!({
        "locales": [locale],
        "wordLevelTimestampsEnabled": true,
        "profanityFilterMode": "None",
        "punctuationMode": "Automatic"
    });
    let definition =
        reqwest::multipart::Part::text(definition.to_string()).mime_str("application/json")?;
    let audio_part = reqwest::multipart::Part::bytes(audio)
        .file_name("source.wav")
        .mime_str("audio/wav")?;
    let form = reqwest::multipart::Form::new()
        .part("definition", definition)
        .part("audio", audio_part);

    metrics::record_azure_transcription(metadata.len() as usize);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .context("Failed to create Azure transcription client")?;
    let started = Instant::now();
    let response = client
        .post(url)
        .header("Ocp-Apim-Subscription-Key", &config.key)
        .multipart(form)
        .send()
        .await
        .context("Azure Fast Transcription request failed")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Failed to read Azure Fast Transcription response")?;
    metrics::record_azure_transcription_latency_ms(started.elapsed().as_millis() as u64);
    if !status.is_success() {
        let safe_body = body
            .replace(&config.key, "<redacted>")
            .chars()
            .take(300)
            .collect::<String>();
        bail!("Azure Fast Transcription returned {status}: {safe_body}");
    }
    let result: TranscribeResult =
        serde_json::from_str(&body).context("Azure Fast Transcription returned invalid JSON")?;
    let mut words = result
        .phrases
        .into_iter()
        .flat_map(|phrase| phrase.words)
        .filter_map(|word| {
            let start_ms = u32::try_from(word.offset_milliseconds).ok()?;
            let duration_ms = u32::try_from(word.duration_milliseconds).ok()?;
            let end_ms = start_ms.saturating_add(duration_ms);
            (!word.text.trim().is_empty() && end_ms > start_ms).then(|| GranularSourceWord {
                text: word.text,
                start_ms,
                end_ms,
            })
        })
        .collect::<Vec<_>>();
    words.sort_by_key(|word| (word.start_ms, word.end_ms));
    if words.is_empty() {
        bail!("Azure Fast Transcription returned no word timestamps");
    }
    Ok(words)
}
