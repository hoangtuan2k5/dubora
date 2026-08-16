use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::azure_timing::{TimingEvent, TimingEventKind};
use crate::speech_plan::SpeechPlan;
use crate::srt::{SrtSegment, time_to_seconds};

const MAX_PARTS: usize = 4;
const MAX_DURATION_MS: u32 = 8_000;
const MAX_WORDS: usize = 36;
const MAX_JOIN_GAP_MS: i64 = 120;
pub const FIRST_WORD_TOLERANCE_MS: i64 = 120;
pub const INTERNAL_ANCHOR_TOLERANCE_MS: i64 = 180;
pub const FINAL_ANCHOR_TOLERANCE_MS: i64 = 150;
const AUDIO_TAIL_TOLERANCE_MS: u32 = 1_200;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UtterancePart {
    pub index: usize,
    pub start_ms: u32,
    pub end_ms: u32,
    pub plan: SpeechPlan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Utterance {
    pub id: usize,
    pub start_ms: u32,
    pub end_ms: u32,
    pub parts: Vec<UtterancePart>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledAudio {
    pub id: String,
    pub path: PathBuf,
    pub start_ms: u32,
    pub end_ms: u32,
    pub source_segments: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderManifest {
    pub version: u8,
    pub entries: Vec<ScheduledAudio>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlignmentResult {
    pub accepted: bool,
    pub worst_anchor: Option<String>,
    pub expected_ms: Option<u32>,
    pub actual_ms: Option<u32>,
    pub drift_ms: Option<i64>,
    pub reason: Option<String>,
}

impl AlignmentResult {
    fn reject(reason: impl Into<String>) -> Self {
        Self {
            accepted: false,
            worst_anchor: None,
            expected_ms: None,
            actual_ms: None,
            drift_ms: None,
            reason: Some(reason.into()),
        }
    }
}

impl RenderManifest {
    pub fn new(entries: Vec<ScheduledAudio>) -> Self {
        Self {
            version: 1,
            entries,
        }
    }

    pub fn validate(&self, expected_indices: &[usize]) -> Result<()> {
        let mut covered = self
            .entries
            .iter()
            .flat_map(|entry| entry.source_segments.iter().copied())
            .collect::<Vec<_>>();
        covered.sort_unstable();
        let mut expected = expected_indices.to_vec();
        expected.sort_unstable();
        if covered.windows(2).any(|pair| pair[0] == pair[1]) || covered != expected {
            bail!("Audio render manifest does not cover each subtitle segment exactly once");
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.start_ms >= entry.end_ms || entry.source_segments.is_empty())
        {
            bail!("Audio render manifest contains an invalid entry");
        }
        Ok(())
    }
}

/// Conservatively group adjacent subtitle fragments that are likely one spoken thought.
pub fn group_speech_plans(segments: &[SrtSegment], plans: &[SpeechPlan]) -> Result<Vec<Utterance>> {
    let mut plans = plans
        .iter()
        .map(|plan| (plan.index, plan.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    let mut utterances = Vec::new();
    let mut current: Vec<UtterancePart> = Vec::new();

    for segment in segments {
        let plan = plans
            .remove(&segment.index)
            .ok_or_else(|| anyhow::anyhow!("Missing speech plan {}", segment.index))?;
        let part = UtterancePart {
            index: segment.index,
            start_ms: timestamp_ms(&segment.start)?,
            end_ms: timestamp_ms(&segment.end)?,
            plan,
        };
        if current.is_empty() {
            current.push(part);
            continue;
        }
        let previous = current.last().expect("current is not empty");
        if can_join(previous, &part, &current) {
            current.push(part);
        } else {
            utterances.push(make_utterance(utterances.len() + 1, current));
            current = vec![part];
        }
    }
    if !current.is_empty() {
        utterances.push(make_utterance(utterances.len() + 1, current));
    }
    if !plans.is_empty() {
        bail!("Speech plans contain indices absent from the subtitle source");
    }
    Ok(utterances)
}

fn make_utterance(id: usize, parts: Vec<UtterancePart>) -> Utterance {
    Utterance {
        id,
        start_ms: parts.first().map_or(0, |part| part.start_ms),
        end_ms: parts.last().map_or(0, |part| part.end_ms),
        parts,
    }
}

fn can_join(previous: &UtterancePart, next: &UtterancePart, current: &[UtterancePart]) -> bool {
    if current.len() >= MAX_PARTS
        || next.end_ms.saturating_sub(current[0].start_ms) > MAX_DURATION_MS
    {
        return false;
    }
    let gap_ms = i64::from(next.start_ms) - i64::from(previous.end_ms);
    if !(0..=MAX_JOIN_GAP_MS).contains(&gap_ms) {
        return false;
    }
    let previous_text = previous.plan.display_text.trim();
    let next_text = next.plan.display_text.trim();
    if previous_text.is_empty() || next_text.is_empty() || ends_sentence(previous_text) {
        return false;
    }
    if word_count(current) + next_text.split_whitespace().count() > MAX_WORDS {
        return false;
    }
    starts_lowercase(next_text) || starts_with_connector(next_text) || ends_fragment(previous_text)
}

fn word_count(parts: &[UtterancePart]) -> usize {
    parts
        .iter()
        .map(|part| part.plan.display_text.split_whitespace().count())
        .sum()
}

fn ends_sentence(text: &str) -> bool {
    text.chars()
        .rev()
        .find(|character| !character.is_whitespace())
        .is_some_and(|character| matches!(character, '.' | '!' | '?' | '。' | '！' | '？' | '…'))
}

fn ends_fragment(text: &str) -> bool {
    text.chars()
        .rev()
        .find(|character| !character.is_whitespace())
        .is_some_and(|character| matches!(character, ',' | ':' | ';' | '-' | '–' | '—'))
}

fn starts_lowercase(text: &str) -> bool {
    text.chars()
        .find(|character| !character.is_whitespace())
        .is_some_and(char::is_lowercase)
}

fn starts_with_connector(text: &str) -> bool {
    let first = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|character: char| !character.is_alphanumeric())
        .to_ascii_lowercase();
    matches!(
        first.as_str(),
        "mà" | "và" | "nên" | "để" | "khi" | "với" | "trong" | "là" | "nhưng" | "rồi"
    )
}

fn timestamp_ms(value: &str) -> Result<u32> {
    let millis = (time_to_seconds(value)? * 1_000.0).round();
    if !(0.0..=f64::from(u32::MAX)).contains(&millis) {
        bail!("SRT timestamp is outside the supported range: {value}");
    }
    Ok(millis as u32)
}

pub fn bookmark_name(utterance_id: usize, segment_index: usize) -> String {
    format!("dubora-u{utterance_id}-s{segment_index}-end")
}

/// Validate actual Azure bookmark/word offsets against the original SRT timeline.
pub fn validate_alignment(
    utterance: &Utterance,
    events: &[TimingEvent],
    audio_duration_ms: u32,
) -> AlignmentResult {
    let words: Vec<_> = events
        .iter()
        .filter(|event| event.kind == TimingEventKind::WordBoundary)
        .collect();
    if words.is_empty() {
        return AlignmentResult::reject("missing_first_word_boundary");
    }
    if words
        .windows(2)
        .any(|pair| pair[1].offset_ms < pair[0].offset_ms)
    {
        return AlignmentResult::reject("non_monotonic_word_boundaries");
    }
    let first_word_ms = words[0].offset_ms as i64;
    if first_word_ms > FIRST_WORD_TOLERANCE_MS {
        return AlignmentResult {
            accepted: false,
            worst_anchor: None,
            expected_ms: Some(0),
            actual_ms: Some(words[0].offset_ms as u32),
            drift_ms: Some(first_word_ms),
            reason: Some("first_word_late".to_owned()),
        };
    }

    let mut previous_offset = 0_u64;
    let mut worst: Option<(String, u32, u32, i64)> = None;
    for (position, part) in utterance.parts.iter().enumerate() {
        let expected = part.end_ms.saturating_sub(utterance.start_ms);
        let name = bookmark_name(utterance.id, part.index);
        let matches: Vec<_> = events
            .iter()
            .filter(|event| {
                event.kind == TimingEventKind::Bookmark && event.mark.as_deref() == Some(&name)
            })
            .collect();
        if matches.len() != 1 {
            return AlignmentResult::reject(format!(
                "bookmark_{}",
                if matches.is_empty() {
                    "missing"
                } else {
                    "duplicated"
                }
            ));
        }
        let actual = matches[0].offset_ms;
        if actual < previous_offset {
            return AlignmentResult::reject("non_monotonic_bookmarks");
        }
        previous_offset = actual;
        let drift = i64::from(actual as u32) - i64::from(expected);
        let tolerance = if position + 1 == utterance.parts.len() {
            FINAL_ANCHOR_TOLERANCE_MS
        } else {
            INTERNAL_ANCHOR_TOLERANCE_MS
        };
        let outside_tolerance = if position + 1 == utterance.parts.len() {
            // A final part may finish early; the mixer preserves the remaining subtitle slot as
            // silence. Only late final speech can collide with the next visual cue.
            drift > tolerance
        } else {
            drift.abs() > tolerance
        };
        if outside_tolerance {
            let candidate = (name, expected, actual as u32, drift);
            if worst
                .as_ref()
                .is_none_or(|current| drift.abs() > current.3.abs())
            {
                worst = Some(candidate);
            }
        }
    }
    let Some((worst_anchor, expected_ms, actual_ms, drift_ms)) = worst else {
        let final_anchor = previous_offset as u32;
        if audio_duration_ms < final_anchor.saturating_sub(150)
            || audio_duration_ms > final_anchor.saturating_add(AUDIO_TAIL_TOLERANCE_MS)
        {
            return AlignmentResult::reject("audio_duration_does_not_match_final_anchor");
        }
        return AlignmentResult {
            accepted: true,
            worst_anchor: None,
            expected_ms: None,
            actual_ms: None,
            drift_ms: None,
            reason: None,
        };
    };
    AlignmentResult {
        accepted: false,
        worst_anchor: Some(worst_anchor),
        expected_ms: Some(expected_ms),
        actual_ms: Some(actual_ms),
        drift_ms: Some(drift_ms),
        reason: Some("anchor_drift".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(index: usize, start: &str, end: &str, text: &str) -> SrtSegment {
        SrtSegment {
            index,
            start: start.to_owned(),
            end: end.to_owned(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn joins_a_sentence_continuation_and_preserves_indices() {
        let segments = vec![
            segment(22, "00:01:46,640", "00:01:51,522", "với cú pháp mới"),
            segment(
                23,
                "00:01:51,522",
                "00:01:53,522",
                "mà chúng ta sẽ tìm hiểu trong video hôm nay.",
            ),
            segment(
                24,
                "00:01:54,000",
                "00:01:56,000",
                "Tiếp theo là phần khác.",
            ),
        ];
        let plans = segments
            .iter()
            .map(|segment| SpeechPlan::plain(segment.index, segment.text.clone(), 2_000))
            .collect::<Vec<_>>();
        let utterances = group_speech_plans(&segments, &plans).unwrap();
        assert_eq!(utterances.len(), 2);
        assert_eq!(
            utterances[0]
                .parts
                .iter()
                .map(|part| part.index)
                .collect::<Vec<_>>(),
            vec![22, 23]
        );
        assert_eq!(utterances[1].parts[0].index, 24);
    }

    #[test]
    fn keeps_complete_sentences_and_large_gaps_separate() {
        let segments = vec![
            segment(1, "00:00:00,000", "00:00:01,000", "Một câu hoàn chỉnh."),
            segment(2, "00:00:01,050", "00:00:02,000", "Câu mới."),
            segment(3, "00:00:03,000", "00:00:04,000", "một phần tiếp theo"),
        ];
        let plans = segments
            .iter()
            .map(|segment| SpeechPlan::plain(segment.index, segment.text.clone(), 1_000))
            .collect::<Vec<_>>();
        let utterances = group_speech_plans(&segments, &plans).unwrap();
        assert_eq!(utterances.len(), 3);
    }

    #[test]
    fn manifest_rejects_duplicate_or_missing_coverage() {
        let manifest = RenderManifest::new(vec![ScheduledAudio {
            id: "u1".to_owned(),
            path: PathBuf::from("u1.mp3"),
            start_ms: 0,
            end_ms: 1_000,
            source_segments: vec![1, 1],
        }]);
        assert!(manifest.validate(&[1, 2]).is_err());
    }

    #[test]
    fn alignment_rejects_missing_or_late_anchors_and_accepts_valid_offsets() {
        let segments = vec![segment(
            1,
            "00:00:01,000",
            "00:00:02,000",
            "Một câu tiếp nối",
        )];
        let plan = SpeechPlan::plain(1, segments[0].text.clone(), 1_000);
        let utterance = group_speech_plans(&segments, &[plan]).unwrap().remove(0);
        let name = bookmark_name(utterance.id, 1);
        let valid = vec![
            TimingEvent {
                kind: TimingEventKind::WordBoundary,
                offset_ms: 30,
                duration_ms: 100,
                mark: None,
                text: Some("Một".to_owned()),
            },
            TimingEvent {
                kind: TimingEventKind::Bookmark,
                offset_ms: 990,
                duration_ms: 0,
                mark: Some(name.clone()),
                text: None,
            },
        ];
        assert!(validate_alignment(&utterance, &valid, 1_000).accepted);
        assert!(!validate_alignment(&utterance, &valid[0..1], 1_000).accepted);
        let mut late = valid;
        late[1].offset_ms = 1_250;
        assert_eq!(
            validate_alignment(&utterance, &late, 1_300)
                .reason
                .as_deref(),
            Some("anchor_drift")
        );
        late[1].offset_ms = 600;
        assert!(validate_alignment(&utterance, &late, 900).accepted);
    }
}
