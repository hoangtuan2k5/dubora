use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::speech_plan::{NaturalMode, SpeechPlan, SpeechPlanDocument};
use crate::srt::{SrtSegment, parse_srt, time_to_seconds, write_srt};

const ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/interactions";
const MODEL: &str = "gemini-3.5-flash-lite";
const TRANSLATE_BATCH_SIZE: usize = 200;
const PLAN_BATCH_SIZE: usize = 50;

#[derive(Serialize)]
struct InputSegment<'a> { index: usize, text: &'a str }

#[derive(Deserialize)]
struct Output { segments: Vec<OutputSegment> }

#[derive(Deserialize)]
struct OutputSegment { index: usize, text: String }

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanInputSegment<'a> {
    index: usize,
    text: &'a str,
    start: &'a str,
    end: &'a str,
    target_duration_ms: u32,
    previous_text: Option<&'a str>,
    next_text: Option<&'a str>,
}

#[derive(Deserialize)]
struct PlanOutput { plans: Vec<SpeechPlan> }

pub async fn translate_srt(keys: &[String], input: &Path, output: &Path) -> Result<()> {
    let source = parse_srt(input)?;
    if source.is_empty() { bail!("No subtitle segments were found for translation"); }
    if keys.is_empty() { bail!("At least one Gemini API key is required"); }
    let client = reqwest::Client::new();
    let mut translated = Vec::with_capacity(source.len());

    for (batch_number, batch) in source.chunks(TRANSLATE_BATCH_SIZE).enumerate() {
        let result = translate_batch(&client, &keys[batch_number % keys.len()], batch).await?;
        translated.extend(result);
    }
    write_srt(output, &translated)
}

/// Ask Gemini for bounded performance plans without allowing it to produce SSML.
///
/// When `translate` is false, the displayed subtitle stays identical to the input and Gemini
/// only decides how it should be spoken. When it is true, Gemini returns the Vietnamese display
/// text and the matching spoken text in the same request.
pub async fn create_speech_plans(
    keys: &[String],
    input: &Path,
    translate: bool,
    mode: NaturalMode,
    allowed_styles: &[String],
) -> Result<SpeechPlanDocument> {
    let source = parse_srt(input)?;
    if source.is_empty() { bail!("No subtitle segments were found for speech planning"); }
    if keys.is_empty() { bail!("At least one Gemini API key is required"); }

    let client = reqwest::Client::new();
    let mut plans = Vec::with_capacity(source.len());
    for (batch_number, batch) in source.chunks(PLAN_BATCH_SIZE).enumerate() {
        plans.extend(
            plan_batch(
                &client,
                &keys[batch_number % keys.len()],
                &source,
                batch,
                translate,
                mode,
                allowed_styles,
            )
            .await?,
        );
    }
    Ok(SpeechPlanDocument::new(plans))
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
    let text = request_model_output(client, key, body).await?;
    let output: Output = serde_json::from_str(&text).context("Gemini did not return subtitle JSON")?;
    let mut by_index = output.segments.into_iter().map(|segment| (segment.index, segment.text.trim().to_owned())).collect::<std::collections::HashMap<_, _>>();
    batch.iter().map(|segment| {
        let text = by_index.remove(&segment.index).filter(|text| !text.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Gemini omitted subtitle {}", segment.index))?;
        Ok(SrtSegment { index: segment.index, start: segment.start.clone(), end: segment.end.clone(), text })
    }).collect()
}

async fn plan_batch(
    client: &reqwest::Client,
    key: &str,
    source: &[SrtSegment],
    batch: &[SrtSegment],
    translate: bool,
    mode: NaturalMode,
    allowed_styles: &[String],
) -> Result<Vec<SpeechPlan>> {
    let input = batch.iter().map(|segment| {
        let position = source.iter().position(|candidate| candidate.index == segment.index)
            .expect("batch segment must belong to source");
        Ok(PlanInputSegment {
            index: segment.index,
            text: &segment.text,
            start: &segment.start,
            end: &segment.end,
            target_duration_ms: target_duration_ms(segment)?,
            previous_text: position.checked_sub(1).map(|index| source[index].text.as_str()),
            next_text: source.get(position + 1).map(|segment| segment.text.as_str()),
        })
    }).collect::<Result<Vec<_>>>()?;
    let input = serde_json::to_string(&input)?;
    let styles = serde_json::to_string(allowed_styles)?;
    let translation_instruction = if translate {
        "Translate displayText and spokenText into natural Vietnamese."
    } else {
        "Keep displayText identical to the input text; spokenText may be a concise, natural rephrase."
    };
    let prompt = format!(
        "Create one speech performance plan for every target subtitle. {translation_instruction} \
         Preserve meaning. Never add fillers. Use at most two emphasis phrases and two semantic pauses. \
         Pauses must be 120-800 ms and phrase text must occur exactly once in spokenText. \
         Use delivery neutral, serious, reflective, or question. Use style only from {styles}, otherwise null. \
         Natural mode is {mode:?}. Return JSON only; do not return SSML, markup, explanations, or audio instructions. \
         Input JSON: {input}"
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
                "properties": { "plans": { "type": "array", "items": {
                    "type": "object", "properties": {
                        "index": { "type": "integer" },
                        "displayText": { "type": "string" },
                        "spokenText": { "type": "string" },
                        "targetDurationMs": { "type": "integer" },
                        "delivery": { "type": "string" },
                        "emphasis": { "type": "array" },
                        "pauses": { "type": "array" },
                        "style": { "type": ["string", "null"] }
                    },
                    "required": ["index", "displayText", "spokenText", "targetDurationMs"]
                }}},
                "required": ["plans"]
            }
        }
    });
    let text = request_model_output(client, key, body).await?;
    parse_plan_response(source, batch, &text, translate, allowed_styles)
}

fn parse_plan_response(
    source: &[SrtSegment],
    batch: &[SrtSegment],
    text: &str,
    translate: bool,
    allowed_styles: &[String],
) -> Result<Vec<SpeechPlan>> {
    let output: PlanOutput = serde_json::from_str(text).context("Gemini did not return speech plan JSON")?;
    let mut by_index = output.plans.into_iter()
        .map(|plan| (plan.index, plan))
        .collect::<std::collections::HashMap<_, _>>();
    if by_index.len() != batch.len() {
        bail!("Gemini returned duplicate or unexpected speech plans");
    }

    batch.iter().map(|segment| {
        let mut plan = by_index.remove(&segment.index)
            .ok_or_else(|| anyhow::anyhow!("Gemini omitted speech plan {}", segment.index))?;
        let source_segment = source.iter().find(|candidate| candidate.index == segment.index)
            .expect("batch segment must belong to source");
        if !translate { plan.display_text = source_segment.text.clone(); }
        plan.target_duration_ms = target_duration_ms(source_segment)?;
        plan.validate(allowed_styles)?;
        Ok(plan)
    }).collect()
}

fn target_duration_ms(segment: &SrtSegment) -> Result<u32> {
    let duration = (time_to_seconds(&segment.end)? - time_to_seconds(&segment.start)?) * 1_000.0;
    if duration < 100.0 { bail!("Subtitle {} is too short for speech planning", segment.index); }
    Ok(duration.round() as u32)
}

async fn request_model_output(client: &reqwest::Client, key: &str, body: serde_json::Value) -> Result<String> {
    let response = client.post(ENDPOINT)
        .header("x-goog-api-key", key)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body)?)
        .send().await.context("Gemini request failed")?;
    let status = response.status();
    let payload: serde_json::Value = serde_json::from_str(&response.text().await.context("Invalid Gemini response")?)
        .context("Invalid Gemini JSON response")?;
    if !status.is_success() { bail!("Gemini request failed with {status}: {}", payload); }
    Ok(payload["steps"].as_array().into_iter().flatten()
        .filter(|step| step["type"] == "model_output")
        .flat_map(|step| step["content"].as_array().into_iter().flatten())
        .filter(|content| content["type"] == "text")
        .filter_map(|content| content["text"].as_str())
        .collect())
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
    use super::{parse_keys, parse_plan_response};
    use crate::srt::SrtSegment;

    #[test]
    fn parses_comma_or_newline_separated_keys() { assert_eq!(parse_keys(" one, two\nthree "), ["one", "two", "three"]); }

    fn segments() -> Vec<SrtSegment> {
        vec![SrtSegment {
            index: 1,
            start: "00:00:01,000".to_owned(),
            end: "00:00:03,000".to_owned(),
            text: "Spring Boot is very important.".to_owned(),
        }]
    }

    #[test]
    fn keeps_original_display_text_when_translation_is_disabled() {
        let source = segments();
        let json = r#"{"plans":[{"index":1,"displayText":"ignored","spokenText":"Spring Boot is important.","targetDurationMs":1,"delivery":"serious","emphasis":[{"text":"important","level":"strong"}],"pauses":[],"style":null}]}"#;
        let plans = parse_plan_response(&source, &source, json, false, &[]).unwrap();
        assert_eq!(plans[0].display_text, "Spring Boot is very important.");
        assert_eq!(plans[0].target_duration_ms, 2_000);
    }

    #[test]
    fn keeps_gemini_vietnamese_display_text_when_translation_is_enabled() {
        let source = segments();
        let json = r#"{"plans":[{"index":1,"displayText":"Spring Boot rất quan trọng.","spokenText":"Spring Boot rất quan trọng.","targetDurationMs":1,"delivery":"serious","emphasis":[{"text":"rất quan trọng","level":"strong"}],"pauses":[],"style":null}]}"#;
        let plans = parse_plan_response(&source, &source, json, true, &[]).unwrap();
        assert_eq!(plans[0].display_text, "Spring Boot rất quan trọng.");
    }

    #[test]
    fn rejects_missing_speech_plans() {
        let source = segments();
        assert!(parse_plan_response(&source, &source, r#"{"plans":[]}"#, false, &[]).is_err());
    }
}
