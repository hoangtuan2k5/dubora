use std::fs;
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::{Client, StatusCode};
use std::sync::OnceLock;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::sleep;

use crate::config::AzureConfig;
use crate::azure_voices::{AzureVoice, CapabilityState};
use crate::ffmpeg::FfmpegPaths;
use crate::progress::{ProgressEvent, ProgressReporter, Stage};
use crate::speech_plan::{EmphasisLevel, SpeechPlan};
use crate::srt::{SrtSegment, parse_srt};
use crate::tts_cache::{is_fresh_audio_file, tts_cache_key, write_tts_cache_key};

const CONCURRENCY: usize = 4;
const RETRIES: usize = 3;
const SPEECH_PLAN_CACHE_PROVIDER: &str = "azure-speech-plan-v1";

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
            let text = prepare_tts_text(&segment.text);
            let key = tts_cache_key("azure", &config.voice, &config.style, &text);
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
    let text = prepare_tts_text(&segment.text);
    let cache_key = tts_cache_key("azure", &config.voice, &config.style, &text);
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
        match synthesize_once(client, config, ffmpeg, &text, &tmp, &output, &cache_key).await {
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
            attempt_errors
                .last()
                .map(String::as_str)
                .unwrap_or("unknown error")
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
        .header("User-Agent", "dubora")
        .body(build_ssml(&config.voice, &config.style, text))
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

fn prepare_tts_text(text: &str) -> String {
    static IP_ADDRESS: OnceLock<Regex> = OnceLock::new();
    IP_ADDRESS
        .get_or_init(|| {
            Regex::new(r"(?i)(?:[0-9a-f.]*:){2,}[0-9a-f.]*|\b(?:\d{1,3}\.){3}\d{1,3}\b")
                .unwrap()
        })
        .replace_all(text, |captures: &regex::Captures<'_>| {
            let ip = &captures[0];
            if ip.contains(':') {
                return ip
                    .parse::<Ipv6Addr>()
                    .map(|_| spell_ipv6(ip))
                    .unwrap_or_else(|_| ip.to_owned());
            }
            let octets: Vec<_> = ip.split('.').collect();
            if octets.iter().any(|octet| octet.parse::<u8>().is_err()) {
                return ip.to_owned();
            }
            octets
                .into_iter()
                .map(|octet| octet.chars().map(spell_digit).collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>()
                .join(" chấm ")
        })
        .into_owned()
}

fn spell_digit(digit: char) -> &'static str {
    match digit {
        '0' => "không",
        '1' => "một",
        '2' => "hai",
        '3' => "ba",
        '4' => "bốn",
        '5' => "năm",
        '6' => "sáu",
        '7' => "bảy",
        '8' => "tám",
        '9' => "chín",
        _ => unreachable!(),
    }
}

fn spell_ipv6(ip: &str) -> String {
    ip.chars()
        .map(|character| match character {
            '0'..='9' => spell_digit(character),
            'a' | 'A' => "a",
            'b' | 'B' => "bê",
            'c' | 'C' => "xê",
            'd' | 'D' => "đê",
            'e' | 'E' => "e",
            'f' | 'F' => "ép",
            ':' => "hai chấm",
            '.' => "chấm",
            _ => unreachable!(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn build_ssml(voice: &str, style: &str, text: &str) -> String {
    let text = escape_xml(text);
    let text = if style.is_empty() {
        text
    } else {
        format!(
            "<mstts:express-as style=\"{}\">{text}</mstts:express-as>",
            escape_xml(style)
        )
    };
    format!(
        "<speak version=\"1.0\" xmlns=\"http://www.w3.org/2001/10/synthesis\" xmlns:mstts=\"http://www.w3.org/2001/mstts\" xml:lang=\"vi-VN\"><voice name=\"{voice}\">{text}</voice></speak>",
    )
}

/// Render a validated Speech Plan using only SSML elements supported by this Azure voice.
pub fn render_speech_plan_ssml(voice: &AzureVoice, plan: &SpeechPlan) -> Result<String> {
    plan.validate(&voice.styles)?;
    if plan.style.is_some() && voice.capabilities.style != CapabilityState::Supported {
        bail!("Azure voice {} does not support speaking styles", voice.short_name);
    }

    let supports_emphasis = voice.capabilities.manual_emphasis == CapabilityState::Supported;
    let supports_prosody = voice.capabilities.manual_prosody == CapabilityState::Supported;
    let supports_break = voice.capabilities.semantic_break == CapabilityState::Supported;
    let mut boundaries = vec![0, plan.spoken_text.len()];
    let mut emphasis = Vec::new();
    let mut pauses = Vec::new();

    if supports_emphasis {
        for item in &plan.emphasis {
            let (start, end) = phrase_span(&plan.spoken_text, &item.text)?;
            boundaries.extend([start, end]);
            emphasis.push((start, end, item.level));
        }
    }
    if supports_break {
        for pause in &plan.pauses {
            let (_, end) = phrase_span(&plan.spoken_text, &pause.after)?;
            boundaries.push(end);
            pauses.push((end, pause.duration_ms));
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut body = String::new();
    let mut previous = 0;
    for boundary in boundaries {
        body.push_str(&escape_xml(&plan.spoken_text[previous..boundary]));
        for (_, _, level) in emphasis.iter().filter(|(_, end, _)| *end == boundary) {
            body.push_str("</emphasis>");
            if *level == EmphasisLevel::Strong && supports_prosody {
                body.push_str("</prosody>");
            }
        }
        for (_, duration_ms) in pauses.iter().filter(|(end, _)| *end == boundary) {
            body.push_str(&format!("<break time=\"{duration_ms}ms\"/>"));
        }
        for (_, _, level) in emphasis.iter().filter(|(start, _, _)| *start == boundary) {
            if *level == EmphasisLevel::Strong && supports_prosody {
                body.push_str("<prosody rate=\"-3%\" pitch=\"+3%\">");
            }
            body.push_str(&format!("<emphasis level=\"{}\">", emphasis_level(*level)));
        }
        previous = boundary;
    }

    if let Some(style) = plan.style.as_deref() {
        body = format!(
            "<mstts:express-as style=\"{}\">{body}</mstts:express-as>",
            escape_xml(style)
        );
    }
    Ok(format!(
        "<speak version=\"1.0\" xmlns=\"http://www.w3.org/2001/10/synthesis\" xmlns:mstts=\"http://www.w3.org/2001/mstts\" xml:lang=\"vi-VN\"><voice name=\"{}\">{body}</voice></speak>",
        escape_xml(&voice.short_name)
    ))
}

/// Cache identity for a plan includes the exact capability-aware SSML payload.
pub fn speech_plan_cache_key(voice: &AzureVoice, plan: &SpeechPlan) -> Result<String> {
    Ok(tts_cache_key(
        SPEECH_PLAN_CACHE_PROVIDER,
        &voice.short_name,
        "",
        &render_speech_plan_ssml(voice, plan)?,
    ))
}

fn phrase_span(text: &str, phrase: &str) -> Result<(usize, usize)> {
    let mut matches = text.match_indices(phrase);
    let (start, matched) = matches
        .next()
        .ok_or_else(|| anyhow::anyhow!("Speech plan phrase is absent from spoken text"))?;
    if matches.next().is_some() {
        bail!("Speech plan phrase occurs more than once in spoken text");
    }
    Ok((start, start + matched.len()))
}

fn emphasis_level(level: EmphasisLevel) -> &'static str {
    match level {
        EmphasisLevel::Light => "moderate",
        EmphasisLevel::Clear | EmphasisLevel::Strong => "strong",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure_voices::{VoiceCapabilities, VoiceModelFamily};
    use crate::speech_plan::{Delivery, Emphasis, Pause, PauseReason};

    fn voice(capabilities: VoiceCapabilities) -> AzureVoice {
        AzureVoice {
            short_name: "vi-VN-HoaiMyNeural".to_owned(),
            display_name: "Hoai My".to_owned(),
            local_name: "Hoài My".to_owned(),
            gender: "Female".to_owned(),
            locale: "vi-VN".to_owned(),
            locale_name: "Vietnamese".to_owned(),
            status: "GA".to_owned(),
            voice_type: "Neural".to_owned(),
            styles: vec!["serious".to_owned()],
            secondary_locales: Vec::new(),
            words_per_minute: Some(150),
            capabilities,
        }
    }

    fn standard_capabilities() -> VoiceCapabilities {
        VoiceCapabilities {
            model_family: VoiceModelFamily::Standard,
            multilingual: false,
            style: CapabilityState::Supported,
            manual_emphasis: CapabilityState::Supported,
            manual_prosody: CapabilityState::Supported,
            semantic_break: CapabilityState::Supported,
            pronunciation: CapabilityState::Supported,
            automatic_delivery: CapabilityState::Unknown,
        }
    }

    fn plan() -> SpeechPlan {
        SpeechPlan {
            index: 1,
            display_text: "Spring Boot rất quan trọng.".to_owned(),
            spoken_text: "Spring Boot rất quan trọng.".to_owned(),
            target_duration_ms: 2_000,
            delivery: Delivery::Serious,
            emphasis: vec![Emphasis {
                text: "rất quan trọng".to_owned(),
                level: EmphasisLevel::Strong,
            }],
            pauses: vec![Pause {
                after: "Spring Boot".to_owned(),
                duration_ms: 180,
                reason: PauseReason::Transition,
            }],
            style: Some("serious".to_owned()),
        }
    }

    #[test]
    fn escapes_xml_without_changing_pronunciation() {
        assert_eq!(
            escape_xml("API & <tag> \"quote\" 'apostrophe'"),
            "API &amp; &lt;tag&gt; &quot;quote&quot; &apos;apostrophe&apos;"
        );
    }

    #[test]
    fn ssml_matches_azure_contract() {
        let ssml = build_ssml("voice", "", "API & SQL");
        assert!(ssml.contains("xml:lang=\"vi-VN\""));
        assert!(ssml.contains("<voice name=\"voice\">"));
        assert!(ssml.contains("<voice name=\"voice\">API &amp; SQL</voice>"));
        assert!(!ssml.contains("<prosody"));
    }

    #[test]
    fn style_and_ip_addresses_are_sent_as_spoken_text() {
        assert_eq!(
            prepare_tts_text("Mở 192.168.0.1 nhé."),
            "Mở một chín hai chấm một sáu tám chấm không chấm một nhé."
        );
        assert_eq!(prepare_tts_text("999.1.1.1"), "999.1.1.1");
        assert_eq!(
            prepare_tts_text("Mở [2001:db8::1] nhé."),
            "Mở [hai không không một hai chấm đê bê tám hai chấm hai chấm một] nhé."
        );
        assert_eq!(
            prepare_tts_text("::ffff:192.0.2.128"),
            "hai chấm hai chấm ép ép ép ép hai chấm một chín hai chấm không chấm hai chấm một hai tám"
        );
        assert_eq!(prepare_tts_text("1:2:3"), "1:2:3");
        let ssml = build_ssml("voice", "chat", "Xin chào");
        assert!(ssml.contains("<mstts:express-as style=\"chat\">Xin chào</mstts:express-as>"));
    }

    #[test]
    fn renders_only_supported_speech_plan_controls() {
        let ssml = render_speech_plan_ssml(&voice(standard_capabilities()), &plan()).unwrap();
        assert!(ssml.contains("<mstts:express-as style=\"serious\">"));
        assert!(ssml.contains("<break time=\"180ms\"/>"));
        assert!(ssml.contains("<emphasis level=\"strong\">rất quan trọng</emphasis>"));
        assert!(ssml.contains("<prosody rate=\"-3%\" pitch=\"+3%\">"));

        let mut hd = standard_capabilities();
        hd.model_family = VoiceModelFamily::DragonHd;
        hd.style = CapabilityState::Unsupported;
        hd.manual_emphasis = CapabilityState::Unsupported;
        let mut hd_voice = voice(hd);
        hd_voice.styles.clear();
        let mut hd_plan = plan();
        hd_plan.style = None;
        let ssml = render_speech_plan_ssml(&hd_voice, &hd_plan).unwrap();
        assert!(ssml.contains("<break time=\"180ms\"/>"));
        assert!(!ssml.contains("<emphasis"));
        assert!(!ssml.contains("<prosody"));
    }

    #[test]
    fn speech_plan_cache_key_tracks_rendered_controls() {
        let voice = voice(standard_capabilities());
        let key = speech_plan_cache_key(&voice, &plan()).unwrap();
        let mut changed = plan();
        changed.pauses[0].duration_ms = 220;
        assert_ne!(key, speech_plan_cache_key(&voice, &changed).unwrap());
    }

    #[test]
    fn only_segment_specific_bad_requests_allow_silence() {
        assert!(status_allows_fallback(StatusCode::BAD_REQUEST));
        assert!(!status_allows_fallback(StatusCode::UNAUTHORIZED));
        assert!(!status_allows_fallback(StatusCode::TOO_MANY_REQUESTS));
        assert!(!status_allows_fallback(StatusCode::INTERNAL_SERVER_ERROR));
    }
}
