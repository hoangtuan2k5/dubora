//! Per-video provider counters used by sync diagnostics.
//!
//! The counters intentionally live in the core process rather than in the UI.  They contain no
//! credentials or source text and are reset for each video, so a work-directory report is enough
//! to audit a paid run without adding another persistence dependency.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

#[derive(Clone, Copy, Debug)]
pub enum GeminiRequestKind {
    Planning,
    Translation,
    Revision,
}

static GEMINI_REQUESTS: AtomicU64 = AtomicU64::new(0);
static GEMINI_PLANNING_REQUESTS: AtomicU64 = AtomicU64::new(0);
static GEMINI_TRANSLATION_REQUESTS: AtomicU64 = AtomicU64::new(0);
static GEMINI_REVISION_REQUESTS: AtomicU64 = AtomicU64::new(0);
static GEMINI_INPUT_CHARS: AtomicU64 = AtomicU64::new(0);
static GEMINI_OUTPUT_CHARS: AtomicU64 = AtomicU64::new(0);
static GEMINI_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static AZURE_LOGICAL_GENERATIONS: AtomicU64 = AtomicU64::new(0);
static AZURE_REQUESTS: AtomicU64 = AtomicU64::new(0);
static AZURE_INPUT_CHARS: AtomicU64 = AtomicU64::new(0);
static AZURE_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static AZURE_TRANSCRIPTION_REQUESTS: AtomicU64 = AtomicU64::new(0);
static AZURE_TRANSCRIPTION_INPUT_BYTES: AtomicU64 = AtomicU64::new(0);
static AZURE_TRANSCRIPTION_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static AZURE_TRANSCRIPTION_AUDIO_MS: AtomicU64 = AtomicU64::new(0);
static AZURE_TRANSCRIPTION_LATENCY_MS: AtomicU64 = AtomicU64::new(0);

pub fn reset() {
    for counter in [
        &GEMINI_REQUESTS,
        &GEMINI_PLANNING_REQUESTS,
        &GEMINI_TRANSLATION_REQUESTS,
        &GEMINI_REVISION_REQUESTS,
        &GEMINI_INPUT_CHARS,
        &GEMINI_OUTPUT_CHARS,
        &GEMINI_CACHE_HITS,
        &AZURE_LOGICAL_GENERATIONS,
        &AZURE_REQUESTS,
        &AZURE_INPUT_CHARS,
        &AZURE_CACHE_HITS,
        &AZURE_TRANSCRIPTION_REQUESTS,
        &AZURE_TRANSCRIPTION_INPUT_BYTES,
        &AZURE_TRANSCRIPTION_CACHE_HITS,
        &AZURE_TRANSCRIPTION_AUDIO_MS,
        &AZURE_TRANSCRIPTION_LATENCY_MS,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

pub fn record_gemini_request(kind: GeminiRequestKind, input_chars: usize) {
    GEMINI_REQUESTS.fetch_add(1, Ordering::Relaxed);
    GEMINI_INPUT_CHARS.fetch_add(input_chars as u64, Ordering::Relaxed);
    match kind {
        GeminiRequestKind::Planning => GEMINI_PLANNING_REQUESTS.fetch_add(1, Ordering::Relaxed),
        GeminiRequestKind::Translation => {
            GEMINI_TRANSLATION_REQUESTS.fetch_add(1, Ordering::Relaxed)
        }
        GeminiRequestKind::Revision => GEMINI_REVISION_REQUESTS.fetch_add(1, Ordering::Relaxed),
    };
}

pub fn record_gemini_response(output_chars: usize) {
    GEMINI_OUTPUT_CHARS.fetch_add(output_chars as u64, Ordering::Relaxed);
}

pub fn record_gemini_cache_hit() {
    GEMINI_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_azure_request(input_chars: usize) {
    AZURE_REQUESTS.fetch_add(1, Ordering::Relaxed);
    AZURE_INPUT_CHARS.fetch_add(input_chars as u64, Ordering::Relaxed);
}

pub fn record_azure_logical_generation() {
    AZURE_LOGICAL_GENERATIONS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_azure_cache_hit() {
    AZURE_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_azure_transcription(input_bytes: usize) {
    AZURE_TRANSCRIPTION_REQUESTS.fetch_add(1, Ordering::Relaxed);
    AZURE_TRANSCRIPTION_INPUT_BYTES.fetch_add(input_bytes as u64, Ordering::Relaxed);
}

pub fn record_azure_transcription_cache_hit() {
    AZURE_TRANSCRIPTION_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_azure_transcription_audio_ms(duration_ms: u64) {
    AZURE_TRANSCRIPTION_AUDIO_MS.fetch_add(duration_ms, Ordering::Relaxed);
}

pub fn record_azure_transcription_latency_ms(duration_ms: u64) {
    AZURE_TRANSCRIPTION_LATENCY_MS.fetch_add(duration_ms, Ordering::Relaxed);
}

fn estimated_tokens(characters: u64) -> u64 {
    characters.saturating_add(3) / 4
}

pub fn snapshot() -> Value {
    let gemini_input_chars = GEMINI_INPUT_CHARS.load(Ordering::Relaxed);
    let gemini_output_chars = GEMINI_OUTPUT_CHARS.load(Ordering::Relaxed);
    let input_price = std::env::var("DUBORA_GEMINI_INPUT_USD_PER_MILLION")
        .ok()
        .and_then(|value| value.parse::<f64>().ok());
    let output_price = std::env::var("DUBORA_GEMINI_OUTPUT_USD_PER_MILLION")
        .ok()
        .and_then(|value| value.parse::<f64>().ok());
    let estimated_cost = input_price.zip(output_price).map(|(input, output)| {
        (estimated_tokens(gemini_input_chars) as f64 / 1_000_000.0) * input
            + (estimated_tokens(gemini_output_chars) as f64 / 1_000_000.0) * output
    });
    let transcription_audio_ms = AZURE_TRANSCRIPTION_AUDIO_MS.load(Ordering::Relaxed);
    let transcription_rate = std::env::var("DUBORA_AZURE_STT_USD_PER_AUDIO_HOUR")
        .ok()
        .and_then(|value| value.parse::<f64>().ok());
    let transcription_cost =
        transcription_rate.map(|rate| transcription_audio_ms as f64 / 3_600_000.0 * rate);
    serde_json::json!({
        "geminiLogicalRequests": GEMINI_REQUESTS.load(Ordering::Relaxed),
        "geminiPlanningRequests": GEMINI_PLANNING_REQUESTS.load(Ordering::Relaxed),
        "geminiTranslationRequests": GEMINI_TRANSLATION_REQUESTS.load(Ordering::Relaxed),
        "geminiRevisionRequests": GEMINI_REVISION_REQUESTS.load(Ordering::Relaxed),
        "geminiCacheHits": GEMINI_CACHE_HITS.load(Ordering::Relaxed),
        "geminiInputCharacters": gemini_input_chars,
        "geminiOutputCharacters": gemini_output_chars,
        "geminiEstimatedInputTokens": estimated_tokens(gemini_input_chars),
        "geminiEstimatedOutputTokens": estimated_tokens(gemini_output_chars),
        "geminiEstimatedCostUsd": estimated_cost,
        "costEstimateStatus": if estimated_cost.is_some() { "configured_rates" } else { "provider_rates_unavailable" },
        "azureLogicalGenerations": AZURE_LOGICAL_GENERATIONS.load(Ordering::Relaxed),
        "azureSynthesisAttempts": AZURE_REQUESTS.load(Ordering::Relaxed),
        "azureInputCharacters": AZURE_INPUT_CHARS.load(Ordering::Relaxed),
        "azureCacheHits": AZURE_CACHE_HITS.load(Ordering::Relaxed),
        "azureTranscriptionAttempts": AZURE_TRANSCRIPTION_REQUESTS.load(Ordering::Relaxed),
        "azureTranscriptionInputBytes": AZURE_TRANSCRIPTION_INPUT_BYTES.load(Ordering::Relaxed),
        "azureTranscriptionCacheHits": AZURE_TRANSCRIPTION_CACHE_HITS.load(Ordering::Relaxed),
        "azureTranscriptionAudioMs": transcription_audio_ms,
        "azureTranscriptionLatencyMs": AZURE_TRANSCRIPTION_LATENCY_MS.load(Ordering::Relaxed),
        "azureTranscriptionEstimatedCostUsd": transcription_cost,
        "azureTranscriptionCostEstimateStatus": if transcription_cost.is_some() {
            "configured_rate"
        } else {
            "provider_rate_unavailable"
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_contains_only_aggregate_provider_counters() {
        reset();
        record_gemini_request(GeminiRequestKind::Planning, 8);
        record_gemini_response(4);
        record_gemini_cache_hit();
        record_azure_logical_generation();
        for _ in 0..3 {
            record_azure_request(6);
        }
        record_azure_transcription(1024);
        record_azure_transcription_cache_hit();
        record_azure_transcription_audio_ms(3_600_000);
        record_azure_transcription_latency_ms(250);
        let value = snapshot();
        assert_eq!(value["geminiLogicalRequests"], 1);
        assert_eq!(value["geminiInputCharacters"], 8);
        assert_eq!(value["geminiEstimatedInputTokens"], 2);
        assert_eq!(value["geminiCacheHits"], 1);
        assert_eq!(value["azureLogicalGenerations"], 1);
        assert_eq!(value["azureSynthesisAttempts"], 3);
        assert_eq!(value["azureTranscriptionAttempts"], 1);
        assert_eq!(value["azureTranscriptionInputBytes"], 1024);
        assert_eq!(value["azureTranscriptionCacheHits"], 1);
        assert_eq!(value["azureTranscriptionAudioMs"], 3_600_000);
        assert_eq!(value["azureTranscriptionLatencyMs"], 250);
        assert!(value.get("key").is_none());
        reset();
    }
}
