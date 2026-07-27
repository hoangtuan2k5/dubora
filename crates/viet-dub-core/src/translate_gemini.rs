use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::srt::{SrtSegment, parse_srt, write_srt};

const ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/interactions";
const MODEL: &str = "gemini-3.5-flash-lite";

#[derive(Serialize)]
struct InputSegment<'a> { index: usize, text: &'a str }

#[derive(Deserialize)]
struct Output { segments: Vec<OutputSegment> }

#[derive(Deserialize)]
struct OutputSegment { index: usize, text: String }

pub async fn translate_srt(keys: &[String], input: &Path, output: &Path) -> Result<()> {
    let source = parse_srt(input)?;
    if source.is_empty() { bail!("No subtitle segments were found for translation"); }
    let client = reqwest::Client::new();
    let mut translated = Vec::with_capacity(source.len());

    for (batch_number, batch) in source.chunks(20).enumerate() {
        let result = translate_batch(&client, &keys[batch_number % keys.len()], batch).await?;
        translated.extend(result);
    }
    write_srt(output, &translated)
}

async fn translate_batch(client: &reqwest::Client, key: &str, batch: &[SrtSegment]) -> Result<Vec<SrtSegment>> {
    let input = serde_json::to_string(
        &batch.iter().map(|segment| InputSegment { index: segment.index, text: &segment.text }).collect::<Vec<_>>(),
    )?;
    let prompt = format!(
        "{}Input JSON: {input}",
        concat!(
            "Translate these subtitle segments from English to natural Vietnamese. Return every input index exactly once. ",
            "Keep each answer concise, remove repeated caption wording, and improve sentence boundaries. Do not add commentary. "
        )
    );
    let body = serde_json::json!({
        "model": MODEL,
        "input": prompt,
        "store": false,
        "response_format": {
            "type": "text",
            "mime_type": "application/json",
            "schema": {
                "type": "object",
                "properties": { "segments": { "type": "array", "items": {
                    "type": "object", "properties": {
                        "index": { "type": "integer" }, "text": { "type": "string" }
                    }, "required": ["index", "text"]
                }}},
                "required": ["segments"]
            }
        }
    });
    let response = client.post(ENDPOINT)
        .header("x-goog-api-key", key)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body)?)
        .send().await.context("Gemini request failed")?;
    let status = response.status();
    let payload: serde_json::Value = serde_json::from_str(&response.text().await.context("Invalid Gemini response")?)
        .context("Invalid Gemini JSON response")?;
    if !status.is_success() { bail!("Gemini request failed with {status}: {}", payload); }
    let text = payload["steps"].as_array().into_iter().flatten()
        .filter(|step| step["type"] == "model_output")
        .flat_map(|step| step["content"].as_array().into_iter().flatten())
        .filter(|content| content["type"] == "text")
        .filter_map(|content| content["text"].as_str())
        .collect::<String>();
    let output: Output = serde_json::from_str(&text).context("Gemini did not return subtitle JSON")?;
    let mut by_index = output.segments.into_iter().map(|segment| (segment.index, segment.text.trim().to_owned())).collect::<std::collections::HashMap<_, _>>();
    batch.iter().map(|segment| {
        let text = by_index.remove(&segment.index).filter(|text| !text.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Gemini omitted subtitle {}", segment.index))?;
        Ok(SrtSegment { index: segment.index, start: segment.start.clone(), end: segment.end.clone(), text })
    }).collect()
}

pub fn parse_keys(raw: &str) -> Vec<String> {
    raw.split([',', '\n']).map(str::trim).filter(|key| !key.is_empty()).map(str::to_owned).collect()
}

pub fn load_env_keys(root: &Path) -> Vec<String> {
    std::env::var("GEMINI_API_KEYS").ok().or_else(|| fs::read_to_string(root.join(".env")).ok()
        .and_then(|contents| contents.lines().find_map(|line| line.strip_prefix("GEMINI_API_KEYS=").map(str::to_owned))))
        .map(|keys| parse_keys(&keys)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::parse_keys;
    #[test]
    fn parses_comma_or_newline_separated_keys() { assert_eq!(parse_keys(" one, two\nthree "), ["one", "two", "three"]); }
}
