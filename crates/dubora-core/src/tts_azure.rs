use std::collections::HashMap;
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

use crate::azure_voices::{AzureVoice, CapabilityState};
use crate::config::AzureConfig;
use crate::duration_solver::{DurationDecision, DurationTarget, solve_duration};
use crate::ffmpeg::FfmpegPaths;
use crate::progress::{ProgressEvent, ProgressReporter, Stage};
use crate::speech_plan::{EmphasisLevel, NaturalMode, SpeechPlan};
use crate::srt::{SrtSegment, parse_srt, time_to_seconds};
use crate::translate_gemini::revise_speech_plan;
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

/// Generate capability-aware Natural Voice segments and allow one timing retry per segment.
pub async fn generate_all_tts_for_speech_plans(
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    input_srt: &Path,
    plans: &[SpeechPlan],
    mode: NaturalMode,
    gemini_keys: &[String],
    voice: &AzureVoice,
    output_dir: &Path,
    video: &str,
    progress: &dyn ProgressReporter,
) -> Result<()> {
    let segments = parse_srt(input_srt)?;
    let plans: HashMap<_, _> = plans
        .iter()
        .map(|plan| (plan.index, plan.clone()))
        .collect();
    if segments.len() != plans.len()
        || segments
            .iter()
            .any(|segment| !plans.contains_key(&segment.index))
    {
        bail!("Natural Voice plans do not match the subtitle segments");
    }

    fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create {}", output_dir.display()))?;
    progress.event(ProgressEvent::new(
        Stage::Tts,
        video,
        format!("Generating {} Natural Voice segments.", segments.len()),
    ));
    let client = Client::new();

    for (position, segment) in segments.iter().enumerate() {
        let mut plan = plans
            .get(&segment.index)
            .expect("plans were matched above")
            .clone();
        let target = duration_target(&segments, position, mode)?;
        let mut result =
            generate_plan_segment(&client, config, ffmpeg, output_dir, &plan, voice, 0).await?;
        if result.fallback {
            progress.event(ProgressEvent::new(
                Stage::Tts,
                video,
                format!("Natural Voice segment {} fell back to silence.", plan.index),
            ));
            continue;
        }

        let output = segment_path(output_dir, plan.index);
        let decision = solve_duration(target, duration_ms(ffmpeg, &output)?, false)?;
        match decision {
            DurationDecision::Accept => {}
            DurationDecision::RegenerateWithProsody { rate_percent }
                if voice.capabilities.manual_prosody == CapabilityState::Supported =>
            {
                result = generate_plan_segment(
                    &client,
                    config,
                    ffmpeg,
                    output_dir,
                    &plan,
                    voice,
                    rate_percent,
                )
                .await?;
            }
            DurationDecision::RegenerateWithProsody { .. }
            | DurationDecision::RegenerateSpeech { .. } => {
                plan = revise_speech_plan(gemini_keys, &plan, target.effective_ms(), &voice.styles)
                    .await?;
                result =
                    generate_plan_segment(&client, config, ffmpeg, output_dir, &plan, voice, 0)
                        .await?;
            }
            DurationDecision::TimeStretch { .. } | DurationDecision::Reject { .. } => {
                unreachable!("the first duration decision never stretches or rejects")
            }
        }

        if !result.fallback {
            match solve_duration(target, duration_ms(ffmpeg, &output)?, true)? {
                DurationDecision::Accept => {}
                DurationDecision::TimeStretch { tempo } => progress.event(ProgressEvent::new(
                    Stage::Tts,
                    video,
                    format!(
                        "Segment {} uses bounded {tempo:.2}x timing stretch.",
                        plan.index
                    ),
                )),
                DurationDecision::Reject { required_tempo } => bail!(
                    "Natural Voice segment {} still needs {required_tempo:.2}x stretch after its one retry",
                    plan.index
                ),
                DurationDecision::RegenerateWithProsody { .. }
                | DurationDecision::RegenerateSpeech { .. } => {
                    bail!("Natural Voice duration solver requested more than one retry")
                }
            }
        }
        progress.event(
            ProgressEvent::new(
                Stage::Tts,
                video,
                format!("Generated Natural Voice segment {}", plan.index),
            )
            .progress(position + 1, segments.len()),
        );
    }
    Ok(())
}

fn duration_target(
    segments: &[SrtSegment],
    position: usize,
    mode: NaturalMode,
) -> Result<DurationTarget> {
    let segment = &segments[position];
    let slot_ms = ((time_to_seconds(&segment.end)? - time_to_seconds(&segment.start)?) * 1_000.0)
        .round() as u32;
    let following_silence_ms = segments
        .get(position + 1)
        .map(|next| -> Result<u32> {
            Ok(
                ((time_to_seconds(&next.start)? - time_to_seconds(&segment.end)? - 0.08).max(0.0)
                    * 1_000.0)
                    .round() as u32,
            )
        })
        .transpose()?
        .unwrap_or(0);
    Ok(DurationTarget {
        slot_ms,
        following_silence_ms,
        mode,
    })
}

fn duration_ms(ffmpeg: &FfmpegPaths, path: &Path) -> Result<u32> {
    let duration = ffmpeg.audio_duration(path)?;
    if duration <= 0.0 {
        bail!("Generated audio has no duration: {}", path.display());
    }
    Ok((duration * 1_000.0).round() as u32)
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

async fn generate_plan_segment(
    client: &Client,
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    output_dir: &Path,
    plan: &SpeechPlan,
    voice: &AzureVoice,
    rate_percent: i8,
) -> Result<SegmentResult> {
    let output = segment_path(output_dir, plan.index);
    let plan = prepared_speech_plan(plan);
    let ssml = render_speech_plan_ssml_with_rate(voice, &plan, rate_percent)?;
    let cache_key = speech_plan_cache_key_with_rate(voice, &plan, rate_percent)?;
    if is_fresh_audio_file(ffmpeg, &output, &cache_key) {
        return Ok(SegmentResult {
            index: plan.index,
            fallback: false,
            attempt_errors: Vec::new(),
        });
    }

    let tmp = temporary_path(&output);
    let mut attempt_errors = Vec::new();
    let mut fallback_allowed = false;
    for attempt in 1..=RETRIES {
        let _ = fs::remove_file(&tmp);
        match synthesize_ssml_once(client, config, ffmpeg, &ssml, &tmp, &output, &cache_key).await {
            Ok(()) => {
                return Ok(SegmentResult {
                    index: plan.index,
                    fallback: false,
                    attempt_errors,
                });
            }
            Err(error) => {
                fallback_allowed = error.fallback_allowed;
                attempt_errors.push(format!(
                    "Azure error at Natural Voice segment {} (attempt {attempt}/{RETRIES}): {:#}",
                    plan.index, error.error
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
            "Azure TTS failed for Natural Voice segment {} after {RETRIES} attempts: {}",
            plan.index,
            attempt_errors
                .last()
                .map(String::as_str)
                .unwrap_or("unknown error")
        );
    }
    ffmpeg.create_silence(&output)?;
    if !ffmpeg.is_valid_audio(&output) {
        bail!(
            "Silent fallback is invalid for Natural Voice segment {}",
            plan.index
        );
    }
    write_tts_cache_key(&output, &cache_key)?;
    Ok(SegmentResult {
        index: plan.index,
        fallback: true,
        attempt_errors,
    })
}

fn prepared_speech_plan(plan: &SpeechPlan) -> SpeechPlan {
    let mut prepared = plan.clone();
    prepared.spoken_text = prepare_tts_text(&prepared.spoken_text);
    for emphasis in &mut prepared.emphasis {
        emphasis.text = prepare_tts_text(&emphasis.text);
    }
    for pause in &mut prepared.pauses {
        pause.after = prepare_tts_text(&pause.after);
    }
    prepared
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
    let ssml = build_ssml(&config.voice, &config.style, text);
    synthesize_ssml_once(client, config, ffmpeg, &ssml, tmp, output, cache_key).await
}

async fn synthesize_ssml_once(
    client: &Client,
    config: &AzureConfig,
    ffmpeg: &FfmpegPaths,
    ssml: &str,
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
        .body(ssml.to_owned())
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
            Regex::new(r"(?i)(?:[0-9a-f.]*:){2,}[0-9a-f.]*|\b(?:\d{1,3}\.){3}\d{1,3}\b").unwrap()
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
    render_speech_plan_ssml_with_rate(voice, plan, 0)
}

/// Render a plan with one small timing correction requested by the duration solver.
pub fn render_speech_plan_ssml_with_rate(
    voice: &AzureVoice,
    plan: &SpeechPlan,
    rate_percent: i8,
) -> Result<String> {
    plan.validate(&voice.styles)?;
    if plan.style.is_some() && voice.capabilities.style != CapabilityState::Supported {
        bail!(
            "Azure voice {} does not support speaking styles",
            voice.short_name
        );
    }

    let supports_emphasis = voice.capabilities.manual_emphasis == CapabilityState::Supported;
    let supports_prosody = voice.capabilities.manual_prosody == CapabilityState::Supported;
    let supports_break = voice.capabilities.semantic_break == CapabilityState::Supported;
    if rate_percent != 0 && !supports_prosody {
        bail!(
            "Azure voice {} does not support manual rate adjustment",
            voice.short_name
        );
    }
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
    if rate_percent != 0 {
        body = format!("<prosody rate=\"{rate_percent:+}%\">{body}</prosody>");
    }
    Ok(format!(
        "<speak version=\"1.0\" xmlns=\"http://www.w3.org/2001/10/synthesis\" xmlns:mstts=\"http://www.w3.org/2001/mstts\" xml:lang=\"vi-VN\"><voice name=\"{}\">{body}</voice></speak>",
        escape_xml(&voice.short_name)
    ))
}

/// Cache identity for a plan includes the exact capability-aware SSML payload.
pub fn speech_plan_cache_key(voice: &AzureVoice, plan: &SpeechPlan) -> Result<String> {
    speech_plan_cache_key_with_rate(voice, plan, 0)
}

fn speech_plan_cache_key_with_rate(
    voice: &AzureVoice,
    plan: &SpeechPlan,
    rate_percent: i8,
) -> Result<String> {
    Ok(tts_cache_key(
        SPEECH_PLAN_CACHE_PROVIDER,
        &voice.short_name,
        "",
        &render_speech_plan_ssml_with_rate(voice, plan, rate_percent)?,
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
    fn renders_duration_rate_only_for_voices_with_prosody() {
        let standard = voice(standard_capabilities());
        let ssml = render_speech_plan_ssml_with_rate(&standard, &plan(), 8).unwrap();
        assert!(ssml.contains("<prosody rate=\"+8%\">"));

        let mut hd = standard_capabilities();
        hd.manual_prosody = CapabilityState::Unsupported;
        assert!(render_speech_plan_ssml_with_rate(&voice(hd), &plan(), 8).is_err());
    }

    #[test]
    fn natural_duration_target_caps_the_following_silence_for_balanced_mode() {
        let segments = vec![
            SrtSegment {
                index: 1,
                start: "00:00:00,000".to_owned(),
                end: "00:00:02,000".to_owned(),
                text: "Một".to_owned(),
            },
            SrtSegment {
                index: 2,
                start: "00:00:03,000".to_owned(),
                end: "00:00:04,000".to_owned(),
                text: "Hai".to_owned(),
            },
        ];
        let target = duration_target(&segments, 0, NaturalMode::Balanced).unwrap();
        assert_eq!(target.slot_ms, 2_000);
        assert_eq!(target.following_silence_ms, 920);
        assert_eq!(target.effective_ms(), 2_250);
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
