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
struct InputSegment<'a> {
    index: usize,
    text: &'a str,
}

#[derive(Deserialize)]
struct Output {
    segments: Vec<OutputSegment>,
}

#[derive(Deserialize)]
struct OutputSegment {
    index: usize,
    text: String,
}

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
struct PlanOutput {
    plans: Vec<SpeechPlan>,
}

pub async fn translate_srt(keys: &[String], input: &Path, output: &Path) -> Result<()> {
    let source = parse_srt(input)?;
    if source.is_empty() {
        bail!("No subtitle segments were found for translation");
    }
    if keys.is_empty() {
        bail!("At least one Gemini API key is required");
    }
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
    if source.is_empty() {
        bail!("No subtitle segments were found for speech planning");
    }
    if keys.is_empty() {
        bail!("At least one Gemini API key is required");
    }

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

/// Rephrase one already-planned segment after its first TTS result misses timing by more than
/// the prosody range. The caller keeps the displayed subtitle stable and allows one retry only.
pub async fn revise_speech_plan(
    keys: &[String],
    plan: &SpeechPlan,
    target_duration_ms: u32,
    allowed_styles: &[String],
) -> Result<SpeechPlan> {
    if keys.is_empty() {
        bail!("At least one Gemini API key is required for speech-plan revision");
    }
    let input = serde_json::json!({
        "index": plan.index,
        "displayText": plan.display_text,
        "spokenText": plan.spoken_text,
        "targetDurationMs": target_duration_ms,
    });
    let styles = serde_json::to_string(allowed_styles)?;
    let prompt = format!(
        "Revise this one Vietnamese speech performance plan so spokenText fits targetDurationMs. \
         Preserve meaning and displayText. Never add fillers. Keep at most two emphasis phrases \
         and two semantic pauses. Pauses must be 120-800 ms and phrase text must occur exactly \
         once in spokenText. Each emphasis must be an object with text and level (light, clear, or strong), never a string. \
         Use delivery neutral, serious, reflective, or question. Use style \
         only from {styles}, otherwise null. Return JSON only with {{\"plans\":[{{\"spokenText\":\"...\",\"delivery\":\"neutral\",\"emphasis\":[],\"pauses\":[],\"style\":null}}]}}. \
         Do not return index, displayText, or targetDurationMs; Dubora keeps them locally. Input JSON: {input}"
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
                    "type": "object",
                    "properties": {
                        "spokenText": { "type": "string" },
                        "delivery": { "type": "string" },
                        "emphasis": { "type": "array", "items": { "type": "object", "properties": {
                            "text": { "type": "string" }, "level": { "type": "string", "enum": ["light", "clear", "strong"] }
                        }, "required": ["text", "level"] } },
                        "pauses": { "type": "array", "items": { "type": "object", "properties": {
                            "after": { "type": "string" }, "durationMs": { "type": "integer" }, "reason": { "type": "string", "enum": ["transition", "conclusion", "punchline", "reflection"] }
                        }, "required": ["after", "durationMs", "reason"] } },
                        "style": { "type": ["string", "null"] }
                    },
                    "required": ["spokenText", "delivery", "emphasis", "pauses", "style"]
                } } },
                "required": ["plans"]
            }
        }
    });
    let client = reqwest::Client::new();
    let key = &keys[plan.index % keys.len()];
    let text = request_model_output(&client, key, body).await?;
    parse_revised_speech_plan(plan, target_duration_ms, allowed_styles, &text)
}

fn parse_revised_speech_plan(
    plan: &SpeechPlan,
    target_duration_ms: u32,
    allowed_styles: &[String],
    text: &str,
) -> Result<SpeechPlan> {
    let mut response: serde_json::Value =
        serde_json::from_str(text).context("Gemini did not return revised speech plan JSON")?;
    let plans = response["plans"]
        .as_array_mut()
        .filter(|plans| plans.len() == 1)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Gemini must return exactly one revised speech plan for segment {}",
                plan.index
            )
        })?;
    let revised = plans[0].as_object_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "Gemini returned an invalid revised speech plan for segment {}",
            plan.index
        )
    })?;
    revised.insert("index".to_owned(), serde_json::json!(plan.index));
    revised.insert(
        "displayText".to_owned(),
        serde_json::json!(plan.display_text),
    );
    revised.insert(
        "targetDurationMs".to_owned(),
        serde_json::json!(target_duration_ms),
    );

    let revised: PlanOutput = serde_json::from_value(response)
        .context("Gemini did not return a usable revised speech plan")?;
    let revised = revised
        .plans
        .into_iter()
        .next()
        .expect("one validated plan");
    revised.validate(allowed_styles)?;
    Ok(revised)
}

async fn translate_batch(
    client: &reqwest::Client,
    key: &str,
    batch: &[SrtSegment],
) -> Result<Vec<SrtSegment>> {
    let input = serde_json::to_string(
        &batch
            .iter()
            .map(|segment| InputSegment {
                index: segment.index,
                text: &segment.text,
            })
            .collect::<Vec<_>>(),
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
    let output: Output =
        serde_json::from_str(&text).context("Gemini did not return subtitle JSON")?;
    let mut by_index = output
        .segments
        .into_iter()
        .map(|segment| (segment.index, segment.text.trim().to_owned()))
        .collect::<std::collections::HashMap<_, _>>();
    batch
        .iter()
        .map(|segment| {
            let text = by_index
                .remove(&segment.index)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| anyhow::anyhow!("Gemini omitted subtitle {}", segment.index))?;
            Ok(SrtSegment {
                index: segment.index,
                start: segment.start.clone(),
                end: segment.end.clone(),
                text,
            })
        })
        .collect()
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
    let input = batch
        .iter()
        .map(|segment| {
            let position = source
                .iter()
                .position(|candidate| candidate.index == segment.index)
                .expect("batch segment must belong to source");
            Ok(PlanInputSegment {
                index: segment.index,
                text: &segment.text,
                start: &segment.start,
                end: &segment.end,
                target_duration_ms: target_duration_ms(segment)?,
                previous_text: position
                    .checked_sub(1)
                    .map(|index| source[index].text.as_str()),
                next_text: source
                    .get(position + 1)
                    .map(|segment| segment.text.as_str()),
            })
        })
        .collect::<Result<Vec<_>>>()?;
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
         Each emphasis must be an object with text and level (light, clear, or strong), never a string. \
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
                        "emphasis": { "type": "array", "items": { "type": "object", "properties": {
                            "text": { "type": "string" }, "level": { "type": "string", "enum": ["light", "clear", "strong"] }
                        }, "required": ["text", "level"] } },
                        "pauses": { "type": "array", "items": { "type": "object", "properties": {
                            "after": { "type": "string" }, "durationMs": { "type": "integer" }, "reason": { "type": "string", "enum": ["transition", "conclusion", "punchline", "reflection"] }
                        }, "required": ["after", "durationMs", "reason"] } },
                        "style": { "type": ["string", "null"] }
                    },
                    "required": ["index", "displayText", "spokenText", "targetDurationMs", "delivery", "emphasis", "pauses", "style"]
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
    let response: serde_json::Value =
        serde_json::from_str(text).context("Gemini did not return speech plan JSON")?;
    let output: PlanOutput =
        serde_json::from_value(response).context("Gemini did not return speech plan JSON")?;
    let mut by_index = output
        .plans
        .into_iter()
        .map(|plan| (plan.index, plan))
        .collect::<std::collections::HashMap<_, _>>();
    if by_index.len() != batch.len() {
        bail!("Gemini returned duplicate or unexpected speech plans");
    }

    batch
        .iter()
        .map(|segment| {
            let mut plan = by_index
                .remove(&segment.index)
                .ok_or_else(|| anyhow::anyhow!("Gemini omitted speech plan {}", segment.index))?;
            let source_segment = source
                .iter()
                .find(|candidate| candidate.index == segment.index)
                .expect("batch segment must belong to source");
            if !translate {
                plan.display_text = source_segment.text.clone();
            }
            plan.target_duration_ms = target_duration_ms(source_segment)?;
            plan.validate(allowed_styles)?;
            Ok(plan)
        })
        .collect()
}

fn target_duration_ms(segment: &SrtSegment) -> Result<u32> {
    let duration = (time_to_seconds(&segment.end)? - time_to_seconds(&segment.start)?) * 1_000.0;
    if duration < 100.0 {
        bail!(
            "Subtitle {} is too short for speech planning",
            segment.index
        );
    }
    Ok(duration.round() as u32)
}

async fn request_model_output(
    client: &reqwest::Client,
    key: &str,
    body: serde_json::Value,
) -> Result<String> {
    let response = client
        .post(ENDPOINT)
        .header("x-goog-api-key", key)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await
        .context("Gemini request failed")?;
    let status = response.status();
    let payload: serde_json::Value =
        serde_json::from_str(&response.text().await.context("Invalid Gemini response")?)
            .context("Invalid Gemini JSON response")?;
    if !status.is_success() {
        bail!("Gemini request failed with {status}: {}", payload);
    }
    Ok(payload["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|step| step["type"] == "model_output")
        .flat_map(|step| step["content"].as_array().into_iter().flatten())
        .filter(|content| content["type"] == "text")
        .filter_map(|content| content["text"].as_str())
        .collect())
}

pub fn parse_keys(raw: &str) -> Vec<String> {
    raw.split([',', '\n'])
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn load_env_keys(root: &Path) -> Vec<String> {
    std::env::var("GEMINI_API_KEYS")
        .ok()
        .or_else(|| {
            fs::read_to_string(root.join(".env"))
                .ok()
                .and_then(|contents| {
                    contents
                        .lines()
                        .find_map(|line| line.strip_prefix("GEMINI_API_KEYS=").map(str::to_owned))
                })
        })
        .map(|keys| parse_keys(&keys))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{parse_keys, parse_plan_response, parse_revised_speech_plan};
    use crate::{speech_plan::SpeechPlan, srt::SrtSegment};

    #[test]
    fn parses_comma_or_newline_separated_keys() {
        assert_eq!(parse_keys(" one, two\nthree "), ["one", "two", "three"]);
    }

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

    #[test]
    fn keeps_local_metadata_when_a_revision_only_returns_delivery_fields() {
        let plan = SpeechPlan::plain(7, "Spring Boot rất quan trọng.", 2_000);
        let json = r#"{"plans":[{"spokenText":"Spring Boot rất quan trọng.","delivery":"serious","emphasis":[],"pauses":[],"style":null}]}"#;
        let revised = parse_revised_speech_plan(&plan, 1_600, &[], json).unwrap();

        assert_eq!(revised.index, 7);
        assert_eq!(revised.display_text, "Spring Boot rất quan trọng.");
        assert_eq!(revised.target_duration_ms, 1_600);
    }
}
