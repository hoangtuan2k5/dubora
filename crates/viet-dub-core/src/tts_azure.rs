use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{Client, StatusCode};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::sleep;

use crate::config::AzureConfig;
use crate::ffmpeg::FfmpegPaths;
use crate::progress::{ProgressEvent, ProgressReporter, Stage};
use crate::srt::{SrtSegment, parse_srt};
use crate::tts_cache::{is_fresh_audio_file, tts_cache_key, write_tts_cache_key};

const CONCURRENCY: usize = 4;
const RETRIES: usize = 3;

pub async fn generate_all_tts(
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    input_srt: &Path,
    output_dir: &Path,
    video: &str,
    progress: &dyn ProgressReporter,
) -> Result<()> {
    let segments = parse_srt(input_srt)?;
    progress.event(ProgressEvent::new(
        Stage::Tts,
        video,
        format!("Loaded {} segments for TTS generation.", segments.len()),
    ));

    fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create {}", output_dir.display()))?;
    let missing: Vec<_> = segments
        .into_iter()
        .filter(|segment| {
            let output = segment_path(output_dir, segment.index);
            let key = tts_cache_key("azure", &config.voice, &segment.text);
            !is_fresh_audio_file(ffmpeg, &output, &key)
        })
        .collect();

    progress.event(ProgressEvent::new(
        Stage::Tts,
        video,
        format!(
            "Need to generate {} missing or invalid segments.",
            missing.len()
        ),
    ));
    if missing.is_empty() {
        return Ok(());
    }

    let total = missing.len();
    let semaphore = Arc::new(Semaphore::new(CONCURRENCY));
    let client = Client::new();
    let mut tasks = JoinSet::new();

    for segment in missing {
        let semaphore = Arc::clone(&semaphore);
        let client = client.clone();
        let config = config.clone();
        let ffmpeg = ffmpeg.clone();
        let output_dir = output_dir.to_owned();
        tasks.spawn(async move {
            let _permit = semaphore.acquire_owned().await?;
            sleep(Duration::from_millis(200)).await;
            generate_segment(&client, &config, &ffmpeg, &output_dir, segment).await
        });
    }

    let mut completed = 0;
    while let Some(result) = tasks.join_next().await {
        let result = result.context("TTS worker task failed")??;
        completed += 1;
        for message in result.attempt_errors {
            progress.event(ProgressEvent::new(Stage::Tts, video, message));
        }
        let message = if result.fallback {
            format!(
                "Failed to generate segment {}. Created a silent MP3 fallback.",
                result.index
            )
        } else {
            format!(
                "Generated {completed}/{total} with Azure ({}): segment {}",
                config.voice, result.index
            )
        };
        progress.event(ProgressEvent::new(Stage::Tts, video, message).progress(completed, total));
    }

    Ok(())
}

struct SegmentResult {
    index: usize,
    fallback: bool,
    attempt_errors: Vec<String>,
}

async fn generate_segment(
    client: &Client,
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    output_dir: &Path,
    segment: SrtSegment,
) -> Result<SegmentResult> {
    let output = segment_path(output_dir, segment.index);
    let cache_key = tts_cache_key("azure", &config.voice, &segment.text);
    if is_fresh_audio_file(ffmpeg, &output, &cache_key) {
        return Ok(SegmentResult {
            index: segment.index,
            fallback: false,
            attempt_errors: Vec::new(),
        });
    }

    let tmp = temporary_path(&output);
    let mut attempt_errors = Vec::new();
    let mut fallback_allowed = false;
    for attempt in 1..=RETRIES {
        let _ = fs::remove_file(&tmp);
        match synthesize_once(
            client,
            config,
            ffmpeg,
            &segment.text,
            &tmp,
            &output,
            &cache_key,
        )
        .await
        {
            Ok(()) => {
                return Ok(SegmentResult {
                    index: segment.index,
                    fallback: false,
                    attempt_errors,
                });
            }
            Err(error) => {
                fallback_allowed = error.fallback_allowed;
                attempt_errors.push(format!(
                    "Azure error at segment {} (attempt {attempt}/{RETRIES}): {:#}",
                    segment.index, error.error
                ));
            }
        }
        let _ = fs::remove_file(&tmp);
        if attempt < RETRIES {
            sleep(Duration::from_secs(1)).await;
        }
    }

    if !fallback_allowed {
        bail!(
            "Azure TTS failed for segment {} after {RETRIES} attempts: {}",
            segment.index,
            attempt_errors.last().map(String::as_str).unwrap_or("unknown error")
        );
    }

    ffmpeg.create_silence(&output)?;
    if !ffmpeg.is_valid_audio(&output) {
        bail!("Silent fallback is invalid for segment {}", segment.index);
    }
    write_tts_cache_key(&output, &cache_key)?;
    Ok(SegmentResult {
        index: segment.index,
        fallback: true,
        attempt_errors,
    })
}

async fn synthesize_once(
    client: &Client,
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    text: &str,
    tmp: &Path,
    output: &Path,
    cache_key: &str,
) -> std::result::Result<(), SynthesisError> {
    let url = format!(
        "https://{}.tts.speech.microsoft.com/cognitiveservices/v1",
        config.region
    );
    let response = client
        .post(url)
        .header("Ocp-Apim-Subscription-Key", &config.key)
        .header("Content-Type", "application/ssml+xml")
        .header(
            "X-Microsoft-OutputFormat",
            "audio-48khz-192kbitrate-mono-mp3",
        )
        .header("User-Agent", "viet-dub")
        .body(build_ssml(&config.voice, text))
        .timeout(Duration::from_secs(70))
        .send()
        .await
        .map_err(|error| SynthesisError::fatal(error.into()))?;

    if response.status() != StatusCode::OK {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(300).collect();
        return Err(SynthesisError {
            error: anyhow::anyhow!("{status} {detail}"),
            fallback_allowed: status_allows_fallback(status),
        });
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|error| SynthesisError::fatal(error.into()))?;
    fs::write(tmp, bytes)
        .with_context(|| format!("Failed to write {}", tmp.display()))
        .map_err(SynthesisError::fatal)?;
    if !ffmpeg.is_valid_audio(tmp) {
        return Err(SynthesisError {
            error: anyhow::anyhow!("Azure returned invalid audio"),
            fallback_allowed: true,
        });
    }

    if output.exists() {
        fs::remove_file(output)
            .with_context(|| format!("Failed to replace {}", output.display()))
            .map_err(SynthesisError::fatal)?;
    }
    fs::rename(tmp, output)
        .with_context(|| format!("Failed to replace {}", output.display()))
        .map_err(SynthesisError::fatal)?;
    write_tts_cache_key(output, cache_key).map_err(SynthesisError::fatal)
}

fn status_allows_fallback(status: StatusCode) -> bool {
    status == StatusCode::BAD_REQUEST
}

struct SynthesisError {
    error: anyhow::Error,
    fallback_allowed: bool,
}

impl SynthesisError {
    fn fatal(error: anyhow::Error) -> Self {
        Self {
            error,
            fallback_allowed: false,
        }
    }
}

fn segment_path(output_dir: &Path, index: usize) -> PathBuf {
    output_dir.join(format!("seg_{index}.mp3"))
}

fn temporary_path(output: &Path) -> PathBuf {
    let mut path = output.as_os_str().to_owned();
    path.push(".tmp");
    PathBuf::from(path)
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn build_ssml(voice: &str, text: &str) -> String {
    format!(
        "<speak version=\"1.0\" xmlns=\"http://www.w3.org/2001/10/synthesis\" xmlns:mstts=\"http://www.w3.org/2001/mstts\" xml:lang=\"vi-VN\"><voice name=\"{voice}\"><prosody rate=\"0%\">{}</prosody></voice></speak>",
        escape_xml(text)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_xml_without_changing_pronunciation() {
        assert_eq!(
            escape_xml("API & <tag> \"quote\" 'apostrophe'"),
            "API &amp; &lt;tag&gt; &quot;quote&quot; &apos;apostrophe&apos;"
        );
    }

    #[test]
    fn ssml_matches_azure_contract() {
        let ssml = build_ssml("voice", "API & SQL");
        assert!(ssml.contains("xml:lang=\"vi-VN\""));
        assert!(ssml.contains("<voice name=\"voice\">"));
        assert!(ssml.contains("<prosody rate=\"0%\">API &amp; SQL</prosody>"));
    }

    #[test]
    fn only_segment_specific_bad_requests_allow_silence() {
        assert!(status_allows_fallback(StatusCode::BAD_REQUEST));
        assert!(!status_allows_fallback(StatusCode::UNAUTHORIZED));
        assert!(!status_allows_fallback(StatusCode::TOO_MANY_REQUESTS));
        assert!(!status_allows_fallback(StatusCode::INTERNAL_SERVER_ERROR));
    }
}
