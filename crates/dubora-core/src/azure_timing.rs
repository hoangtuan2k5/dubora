use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::time::timeout;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

use crate::config::AzureConfig;
use crate::metrics::{record_azure_logical_generation, record_azure_request};

const OUTPUT_FORMAT: &str = "audio-48khz-192kbitrate-mono-mp3";
const SYNTHESIS_TIMEOUT: Duration = Duration::from_secs(70);

/// Audio metadata emitted by the Azure Speech WebSocket transport.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimingEvent {
    pub kind: TimingEventKind,
    pub offset_ms: u64,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimingEventKind {
    Bookmark,
    WordBoundary,
    SentenceBoundary,
}

#[derive(Debug)]
pub struct TimedAudio {
    pub audio: Vec<u8>,
    pub events: Vec<TimingEvent>,
}

#[cfg(test)]
type TestTimingResponder = Box<dyn Fn(&str) -> Result<TimedAudio> + Send + Sync>;

#[cfg(test)]
static TEST_TIMING_RESPONDER: OnceLock<Mutex<Option<TestTimingResponder>>> = OnceLock::new();

#[cfg(test)]
pub(crate) struct TestTimingResponderGuard;

#[cfg(test)]
pub(crate) fn install_test_timing_responder(
    responder: TestTimingResponder,
) -> TestTimingResponderGuard {
    *TEST_TIMING_RESPONDER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("test Azure responder lock poisoned") = Some(responder);
    TestTimingResponderGuard
}

#[cfg(test)]
impl Drop for TestTimingResponderGuard {
    fn drop(&mut self) {
        if let Some(responder) = TEST_TIMING_RESPONDER.get() {
            *responder
                .lock()
                .expect("test Azure responder lock poisoned") = None;
        }
    }
}

/// Synthesize one SSML document and collect Azure's audio metadata events.
///
/// The REST endpoint used by the legacy path returns audio only. This WebSocket path mirrors the
/// supported Speech SDK transport so callers can validate internal bookmarks before accepting a
/// grouped utterance.
pub async fn synthesize_ssml_with_timing(config: &AzureConfig, ssml: &str) -> Result<TimedAudio> {
    record_azure_logical_generation();
    record_azure_request(ssml.chars().count());
    #[cfg(test)]
    if let Some(responder) = TEST_TIMING_RESPONDER
        .get()
        .and_then(|responder| responder.lock().ok())
        .and_then(|responder| responder.as_ref().map(|responder| responder(ssml)))
    {
        return responder;
    }
    let connection_id = unique_id();
    let request_id = unique_id();
    let url = format!(
        "wss://{}.tts.speech.microsoft.com/tts/cognitiveservices/websocket/v1",
        config.region
    );
    let mut request = url
        .into_client_request()
        .context("Failed to create Azure Speech WebSocket request")?;
    request.headers_mut().insert(
        "Ocp-Apim-Subscription-Key",
        config
            .key
            .parse()
            .context("Invalid Azure Speech key header")?,
    );
    request.headers_mut().insert(
        "X-ConnectionId",
        connection_id
            .parse()
            .context("Invalid Azure Speech connection id header")?,
    );

    let (mut socket, _) = connect_async(request)
        .await
        .context("Failed to connect to Azure Speech timing WebSocket")?;
    let context = json!({
        "synthesis": {
            "audio": {
                "metadataOptions": {
                    "bookmarkEnabled": true,
                    "wordBoundaryEnabled": true,
                    "punctuationBoundaryEnabled": false,
                    "sentenceBoundaryEnabled": true,
                    "sessionEndEnabled": true,
                    "visemeEnabled": false
                },
                "outputFormat": OUTPUT_FORMAT
            },
            "language": {"autoDetection": false}
        }
    });
    socket
        .send(Message::Text(
            protocol_message(
                "synthesis.context",
                &request_id,
                "application/json",
                &context.to_string(),
            )
            .into(),
        ))
        .await
        .context("Failed to send Azure synthesis context")?;
    socket
        .send(Message::Text(
            protocol_message("ssml", &request_id, "application/ssml+xml", ssml).into(),
        ))
        .await
        .context("Failed to send Azure SSML over timing WebSocket")?;

    let result = timeout(SYNTHESIS_TIMEOUT, async {
        let mut audio = Vec::new();
        let mut events = Vec::new();
        let mut completed = false;
        while let Some(message) = socket.next().await {
            let message = message.context("Azure Speech timing WebSocket message failed")?;
            match message {
                Message::Binary(bytes) => {
                    let frame = parse_binary_frame(&bytes)?;
                    debug_frame("binary", &frame);
                    if frame.path.eq_ignore_ascii_case("audio") {
                        audio.extend_from_slice(frame.body);
                    }
                }
                Message::Text(text) => {
                    let frame = parse_text_frame(text.as_ref());
                    debug_frame("text", &frame);
                    if frame.path.eq_ignore_ascii_case("audio.metadata") {
                        events.extend(parse_timing_events(frame.body)?);
                    } else if frame.path.eq_ignore_ascii_case("turn.end") {
                        completed = true;
                        break;
                    } else if frame.path.eq_ignore_ascii_case("response") {
                        ensure_response_is_healthy(frame.body)?;
                    }
                }
                Message::Close(_) => break,
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
                Message::Pong(_) | Message::Frame(_) => {}
            }
        }
        if !completed {
            bail!("Azure Speech timing WebSocket ended before turn.end");
        }
        if audio.is_empty() {
            bail!("Azure Speech timing WebSocket returned no audio");
        }
        Ok(TimedAudio { audio, events })
    })
    .await
    .context("Azure Speech timing synthesis timed out")??;

    let _ = socket.close(None).await;
    Ok(result)
}

#[derive(Debug)]
struct Frame<'a> {
    path: String,
    body: &'a [u8],
}

fn protocol_message(path: &str, request_id: &str, content_type: &str, body: &str) -> String {
    format!(
        "Path: {path}\r\nX-RequestId: {request_id}\r\nX-Timestamp: {}\r\nContent-Type: {content_type}\r\n\r\n{body}",
        timestamp_millis()
    )
}

fn parse_text_frame(payload: &str) -> Frame<'_> {
    let (headers, body) = payload.split_once("\r\n\r\n").unwrap_or((payload, ""));
    let headers = parse_headers(headers);
    Frame {
        path: headers.get("path").cloned().unwrap_or_default(),
        body: body.as_bytes(),
    }
}

fn parse_binary_frame(payload: &[u8]) -> Result<Frame<'_>> {
    if payload.len() < 2 {
        bail!("Azure Speech binary frame is missing its header length");
    }
    let header_len = usize::from(u16::from_be_bytes([payload[0], payload[1]]));
    let body_start = 2 + header_len;
    if payload.len() < body_start {
        bail!("Azure Speech binary frame has a truncated header");
    }
    let headers = std::str::from_utf8(&payload[2..body_start])
        .context("Azure Speech binary frame headers are not UTF-8")?;
    let headers = parse_headers(headers);
    Ok(Frame {
        path: headers.get("path").cloned().unwrap_or_default(),
        body: &payload[body_start..],
    })
}

fn parse_headers(value: &str) -> HashMap<String, String> {
    value
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect()
}

fn debug_frame(kind: &str, frame: &Frame<'_>) {
    if std::env::var_os("DUBORA_TIMING_DEBUG").is_none() {
        return;
    }
    if kind == "binary" {
        eprintln!(
            "Azure timing frame binary: path={}, body_bytes={}",
            frame.path,
            frame.body.len()
        );
        return;
    }
    let body = String::from_utf8_lossy(frame.body);
    let body: String = body.chars().take(500).collect();
    eprintln!(
        "Azure timing frame {kind}: path={}, body={body}",
        frame.path
    );
}

fn parse_timing_events(body: &[u8]) -> Result<Vec<TimingEvent>> {
    let envelope: MetadataEnvelope =
        serde_json::from_slice(body).context("Failed to parse Azure Speech audio.metadata JSON")?;
    let events = envelope
        .metadata
        .into_iter()
        .filter_map(|metadata| {
            let kind = match metadata.kind.to_ascii_lowercase().as_str() {
                "bookmark" => TimingEventKind::Bookmark,
                "wordboundary" => TimingEventKind::WordBoundary,
                "sentenceboundary" => TimingEventKind::SentenceBoundary,
                _ => return None,
            };
            Some(TimingEvent {
                kind,
                offset_ms: metadata.data.offset_100ns / 10_000,
                duration_ms: metadata.data.duration_100ns / 10_000,
                mark: metadata.data.bookmark,
                text: metadata.data.text.map(|text| text.text),
            })
        })
        .collect::<Vec<_>>();
    Ok(events)
}

fn ensure_response_is_healthy(body: &[u8]) -> Result<()> {
    let value: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return Ok(()),
    };
    let Some(status) = value.get("status") else {
        return Ok(());
    };
    if status.as_i64().is_some_and(|status| status >= 400) {
        let detail = value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Azure Speech synthesis response failed");
        bail!("Azure Speech timing response failed: {detail}");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct MetadataEnvelope {
    #[serde(rename = "Metadata", default)]
    metadata: Vec<MetadataItem>,
}

#[derive(Debug, Deserialize)]
struct MetadataItem {
    #[serde(rename = "Type")]
    kind: String,
    #[serde(rename = "Data")]
    data: MetadataData,
}

#[derive(Debug, Default, Deserialize)]
struct MetadataData {
    #[serde(rename = "Offset", default)]
    offset_100ns: u64,
    #[serde(rename = "Duration", default)]
    duration_100ns: u64,
    #[serde(rename = "Bookmark", default)]
    bookmark: Option<String>,
    #[serde(rename = "text", default)]
    text: Option<MetadataText>,
}

#[derive(Debug, Deserialize)]
struct MetadataText {
    #[serde(rename = "Text", default)]
    text: String,
}

fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn unique_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{timestamp:016x}{counter:016x}")
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;

    #[test]
    fn parses_text_and_binary_protocol_frames() {
        let text = parse_text_frame("Path: audio.metadata\r\nX-RequestId: abc\r\n\r\n{}");
        assert_eq!(text.path, "audio.metadata");
        assert_eq!(text.body, b"{}");

        let headers = b"Path: audio\r\nX-RequestId: abc\r\n";
        let mut binary = Vec::with_capacity(2 + headers.len() + 3);
        binary.extend_from_slice(&(headers.len() as u16).to_be_bytes());
        binary.extend_from_slice(headers);
        binary.extend_from_slice(b"mp3");
        let frame = parse_binary_frame(&binary).unwrap();
        assert_eq!(frame.path, "audio");
        assert_eq!(frame.body, b"mp3");
    }

    #[test]
    fn parses_bookmark_and_word_metadata_in_service_shape() {
        let body = br#"{
          "Metadata": [
            {"Type":"Bookmark","Data":{"Offset":12000000,"Duration":0,"Bookmark":"dubora-u1-s1-end"}},
            {"Type":"WordBoundary","Data":{"Offset":15000000,"Duration":500000,"text":{"Text":"Xin","Length":3}}}
          ]
        }"#;
        let events = parse_timing_events(body).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, TimingEventKind::Bookmark);
        assert_eq!(events[0].offset_ms, 1200);
        assert_eq!(events[0].mark.as_deref(), Some("dubora-u1-s1-end"));
        assert_eq!(events[1].kind, TimingEventKind::WordBoundary);
        assert_eq!(events[1].offset_ms, 1500);
        assert_eq!(events[1].text.as_deref(), Some("Xin"));
    }

    #[test]
    fn ids_are_guid_shaped_without_secrets() {
        let id = unique_id();
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[tokio::test]
    #[ignore = "requires a live Azure Speech key"]
    async fn live_four_voice_timing_spike() {
        let key = env::var("AZURE_KEY").expect("AZURE_KEY is required for the live spike");
        let region = env::var("AZURE_REGION").unwrap_or_else(|_| "southeastasia".to_owned());
        let repeats = env::var("DUBORA_TIMING_REPEATS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(1);
        let voices = [
            "de-DE-Florian:DragonHDLatestNeural",
            "de-DE-Seraphina:DragonHDLatestNeural",
            "vi-VN-NamMinhNeural",
            "vi-VN-HoaiMyNeural",
        ];

        let selected_voice = env::var("DUBORA_TIMING_VOICE").ok();
        for voice in voices.into_iter().filter(|voice| {
            selected_voice
                .as_deref()
                .is_none_or(|selected| selected == *voice)
        }) {
            let config = AzureConfig::new(&key, &region)
                .unwrap()
                .with_voice(voice)
                .unwrap();
            for _ in 0..repeats {
                let ssml = format!(
                    "<speak version=\"1.0\" xmlns=\"http://www.w3.org/2001/10/synthesis\" xml:lang=\"vi-VN\"><voice name=\"{voice}\">Đây là phần một.<bookmark mark=\"dubora-spike-one\"/>Đây là phần hai.<bookmark mark=\"dubora-spike-two\"/></voice></speak>"
                );
                let result = synthesize_ssml_with_timing(&config, &ssml).await.unwrap();
                let bookmarks: Vec<_> = result
                    .events
                    .iter()
                    .filter(|event| event.kind == TimingEventKind::Bookmark)
                    .collect();
                if voice.contains("DragonHD") {
                    assert!(
                        bookmarks.is_empty(),
                        "{voice} unexpectedly returned bookmark events; re-check its capability profile"
                    );
                    eprintln!(
                        "timing spike {voice}: bookmark events unsupported, audio_bytes={}, events={}",
                        result.audio.len(),
                        result.events.len()
                    );
                    continue;
                }
                assert_eq!(bookmarks.len(), 2, "{voice} returned unexpected bookmarks");
                assert!(bookmarks[0].offset_ms < bookmarks[1].offset_ms);
                assert!(!result.audio.is_empty());
                eprintln!(
                    "timing spike {voice}: audio_bytes={}, bookmark_offsets={}ms,{}ms, events={}",
                    result.audio.len(),
                    bookmarks[0].offset_ms,
                    bookmarks[1].offset_ms,
                    result.events.len()
                );
            }
        }
    }
}
