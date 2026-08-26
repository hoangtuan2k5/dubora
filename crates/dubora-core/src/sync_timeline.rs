//! Source evidence and deterministic timing allocation for Natural Voice.
//!
//! This module deliberately contains no provider, filesystem, or FFmpeg code.  It turns the
//! cleaned subtitle source into a monotonic timing artifact and allocates non-overlapping local
//! windows that the TTS and mixer can share.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::clean_rolling::inspect_youtube_rolling_subtitles;
use crate::speech_plan::NaturalMode;
use crate::srt::{SrtSegment, parse_srt, time_to_seconds};

pub const SOURCE_TIMELINE_VERSION: u8 = 2;
pub const SOURCE_TIMELINE_HEADROOM_MS: u32 = 80;
pub const MAX_ANCHOR_DRIFT_MS: i64 = 250;
pub const MAX_SOFT_BOUNDARY_DRIFT_MS: i64 = 500;
pub const MAX_TIMING_TEMPO: f64 = 1.13;
/// Keep timing debt local even when the source captions contain no reliable silence barriers.
pub const MAX_SYNC_ISLAND_DURATION_MS: u32 = 60_000;
const MAX_NARRATION_BLOCK_DURATION_MS: u32 = 18_000;
const MAX_NARRATION_BLOCK_WORDS: usize = 72;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TimingMode {
    #[serde(rename = "granular")]
    Granular,
    #[serde(rename = "captionDerived")]
    CaptionDerived,
    #[serde(rename = "cueOnly")]
    CueOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TimingConfidence {
    #[serde(rename = "granular")]
    Granular,
    #[serde(rename = "captionDerived")]
    CaptionDerived,
    #[serde(rename = "cueOnly")]
    CueOnly,
}

impl TimingConfidence {
    pub fn multiplier(self) -> f64 {
        match self {
            Self::Granular => 1.0,
            Self::CaptionDerived => 0.6,
            Self::CueOnly => 0.25,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Coupling {
    #[serde(rename = "anchored")]
    Anchored,
    #[serde(rename = "soft")]
    Soft,
    #[serde(rename = "elastic")]
    Elastic,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceToken {
    pub id: String,
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
    pub confidence: TimingConfidence,
    pub source_segment: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcePhrase {
    pub id: String,
    pub text: String,
    pub start_token: String,
    pub end_token: String,
    pub preferred_start_ms: u32,
    pub preferred_end_ms: u32,
    pub coupling: Coupling,
    pub confidence: TimingConfidence,
    pub owner_index: usize,
    pub required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceBarrier {
    pub id: String,
    pub time_ms: u32,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceTimeline {
    pub version: u8,
    pub mode: TimingMode,
    #[serde(default)]
    pub timing_provider: Option<String>,
    #[serde(default)]
    pub timing_fallback_reason: Option<String>,
    #[serde(default)]
    pub granular_match_count: Option<usize>,
    #[serde(default)]
    pub granular_source_word_count: Option<usize>,
    pub tokens: Vec<SourceToken>,
    pub phrases: Vec<SourcePhrase>,
    pub barriers: Vec<SourceBarrier>,
    pub duration_ms: u32,
    pub cue_overlap_count: usize,
    /// Number of required anchors whose source-language ASR token supplied a cross-language
    /// timing projection.  This never upgrades the whole timeline to granular timing.
    #[serde(default)]
    pub projected_anchor_count: usize,
    /// Required anchor ids whose timestamps came from source-language ASR projection.  Cue-only
    /// timelines must never treat an unlisted phrase as precise timing evidence.
    #[serde(default)]
    pub projected_anchor_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NarrationBlock {
    pub id: String,
    pub start_ms: u32,
    pub end_ms: u32,
    pub preferred_start_ms: u32,
    pub source_segments: Vec<usize>,
    pub anchor_ids: Vec<String>,
    pub island_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncIsland {
    pub id: String,
    pub start_ms: u32,
    pub end_ms: u32,
    pub block_ids: Vec<String>,
    /// True only when a confirmed source barrier separates this island from the previous one.
    /// A duration cap is an implementation boundary, not permission to discard usable slack.
    pub hard_boundary_before: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllocationGroup {
    pub id: String,
    pub start_ms: u32,
    pub end_ms: u32,
    pub island_ids: Vec<String>,
    pub block_ids: Vec<String>,
    pub crosses_soft_boundary: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncLayout {
    pub blocks: Vec<NarrationBlock>,
    pub islands: Vec<SyncIsland>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AllocatedBlock {
    pub block_id: String,
    pub island_id: String,
    pub render_start_ms: u32,
    pub slot_end_ms: u32,
    pub predicted_audio_ms: u32,
    pub rendered_audio_ms: u32,
    pub tempo: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AllocatedSegment {
    pub segment_index: usize,
    pub island_id: String,
    pub render_start_ms: u32,
    pub slot_end_ms: u32,
    pub predicted_audio_ms: u32,
    pub rendered_audio_ms: u32,
    pub tempo: f64,
}

#[derive(Clone, Debug)]
pub struct MeasuredAnchor {
    pub source_ms: u32,
    pub raw_offset_ms: u32,
    pub tolerance_ms: u32,
    pub confidence: TimingConfidence,
    pub coupling: Coupling,
}

impl SourceTimeline {
    pub fn from_files(source_srt: &Path, clean_srt: &Path) -> Result<Self> {
        let source = parse_srt(clean_srt)
            .with_context(|| format!("Failed to parse cleaned SRT {}", clean_srt.display()))?;
        if source.is_empty() {
            bail!("Cannot build source timeline from an empty SRT");
        }
        let evidence = inspect_youtube_rolling_subtitles(source_srt)?;
        // A rolling cue's raw end is the display lifetime, not the time available for the newly
        // introduced words.  The next cue start is the only reliable local transition we have;
        // spreading over the raw end and clamping later compresses the tail into 1 ms tokens and
        // moves semantic anchors several seconds late.
        let evidence_words = evidence
            .spans
            .iter()
            .enumerate()
            .flat_map(|(position, span)| {
                let transition_end = evidence
                    .spans
                    .get(position + 1)
                    .map(|next| next.start_ms)
                    .filter(|next_start| *next_start > span.start_ms)
                    .unwrap_or(span.end_ms);
                let end_ms = span.end_ms.min(transition_end).max(span.start_ms + 1);
                distribute_span_words(&span.text, span.start_ms, end_ms)
            })
            .collect::<Vec<_>>();
        let mut evidence_cursor = 0;
        let mut tokens = Vec::new();
        let mut mode = if evidence.overlap_count > 0 {
            TimingMode::CaptionDerived
        } else {
            TimingMode::CueOnly
        };

        for (position, segment) in source.iter().enumerate() {
            let start_ms = timestamp_ms(&segment.start)?;
            let own_end_ms = timestamp_ms(&segment.end)?;
            if own_end_ms < start_ms {
                bail!(
                    "Source subtitle {} has a reversed timing window",
                    segment.index
                );
            }
            let next_start_ms = source
                .get(position + 1)
                .map(|next| timestamp_ms(&next.start))
                .transpose()?
                .unwrap_or(own_end_ms);
            let nominal_end_ms = own_end_ms.min(next_start_ms.max(start_ms + 1));
            let words = segment.text.split_whitespace().collect::<Vec<_>>();
            let matched = match_words(&words, &evidence_words, evidence_cursor);
            let mut segment_tokens = if let Some(end_cursor) = matched {
                let values = evidence_words[evidence_cursor..end_cursor].to_vec();
                evidence_cursor = end_cursor;
                values
            } else {
                mode = TimingMode::CueOnly;
                distribute_span_words(&segment.text, start_ms, nominal_end_ms)
            };
            for word in &mut segment_tokens {
                word.start_ms = word.start_ms.clamp(start_ms, nominal_end_ms);
                word.end_ms = word
                    .end_ms
                    .clamp(word.start_ms + 1, nominal_end_ms.max(word.start_ms + 1));
            }
            for word in segment_tokens {
                let token_index = tokens.len() + 1;
                let previous_end = tokens.last().map_or(0, |token: &SourceToken| token.end_ms);
                let token_start = word.start_ms.max(previous_end);
                let token_end = word.end_ms.max(token_start + 1);
                tokens.push(SourceToken {
                    id: format!("t-{token_index}"),
                    text: word.text,
                    start_ms: token_start,
                    end_ms: token_end,
                    confidence: if mode == TimingMode::CaptionDerived {
                        TimingConfidence::CaptionDerived
                    } else {
                        TimingConfidence::CueOnly
                    },
                    source_segment: segment.index,
                });
            }
        }
        if tokens.is_empty() {
            bail!("Source timeline contains no source tokens");
        }

        let mut phrases = Vec::new();
        let mut barriers = vec![SourceBarrier {
            id: "barrier-start".to_owned(),
            time_ms: tokens.first().map_or(0, |token| token.start_ms),
            reason: "videoStart".to_owned(),
        }];
        for (position, segment) in source.iter().enumerate() {
            let segment_tokens = tokens
                .iter()
                .enumerate()
                .filter(|(_, token)| token.source_segment == segment.index)
                .collect::<Vec<_>>();
            let Some((_, first)) = segment_tokens.first() else {
                continue;
            };
            let (_, last) = segment_tokens.last().expect("first implies last");
            let confidence = first.confidence;
            let soft_id = format!("p-{}-boundary", segment.index);
            phrases.push(SourcePhrase {
                id: soft_id,
                text: segment.text.clone(),
                start_token: first.id.clone(),
                end_token: last.id.clone(),
                preferred_start_ms: first.start_ms,
                preferred_end_ms: last.end_ms,
                coupling: Coupling::Soft,
                confidence,
                owner_index: segment.index,
                required: false,
            });
            for (candidate_number, (start, end)) in
                action_ranges(&segment.text).into_iter().enumerate()
            {
                let first_token = segment_tokens
                    .get(start)
                    .map(|(_, token)| *token)
                    .unwrap_or(first);
                let last_token = segment_tokens
                    .get(end)
                    .map(|(_, token)| *token)
                    .unwrap_or(last);
                let phrase_text = segment
                    .text
                    .split_whitespace()
                    .skip(start)
                    .take(end.saturating_sub(start) + 1)
                    .collect::<Vec<_>>()
                    .join(" ");
                if last_token.end_ms.saturating_sub(first_token.start_ms) < 200 {
                    continue;
                }
                phrases.push(SourcePhrase {
                    id: format!("p-{}-action-{}", segment.index, candidate_number + 1),
                    text: phrase_text,
                    start_token: first_token.id.clone(),
                    end_token: last_token.id.clone(),
                    preferred_start_ms: first_token.start_ms,
                    preferred_end_ms: last_token.end_ms,
                    coupling: Coupling::Anchored,
                    confidence,
                    owner_index: segment.index,
                    required: true,
                });
            }
            let segment_end = last.end_ms;
            if let Some(next) = source.get(position + 1) {
                let next_start = timestamp_ms(&next.start)?;
                if next_start.saturating_sub(segment_end) >= 800 && ends_sentence(&segment.text) {
                    barriers.push(SourceBarrier {
                        id: format!("barrier-gap-{}", segment.index),
                        time_ms: next_start,
                        reason: "confirmedPause".to_owned(),
                    });
                }
            }
        }
        let duration_ms = tokens.last().map_or(0, |token| token.end_ms);
        barriers.push(SourceBarrier {
            id: "barrier-end".to_owned(),
            time_ms: duration_ms,
            reason: "videoEnd".to_owned(),
        });
        barriers.sort_by_key(|barrier| barrier.time_ms);
        barriers.dedup_by_key(|barrier| barrier.time_ms);
        let timeline = Self {
            version: SOURCE_TIMELINE_VERSION,
            mode,
            timing_provider: Some(
                match mode {
                    TimingMode::CaptionDerived => "captionDerived",
                    TimingMode::CueOnly => "cueOnly",
                    TimingMode::Granular => "captionDerived",
                }
                .to_owned(),
            ),
            timing_fallback_reason: None,
            granular_match_count: None,
            granular_source_word_count: None,
            tokens,
            phrases,
            barriers,
            duration_ms,
            cue_overlap_count: evidence.overlap_count,
            projected_anchor_count: 0,
            projected_anchor_ids: Vec::new(),
        };
        timeline.validate()?;
        Ok(timeline)
    }

    /// Project only required semantic anchors onto source-language ASR words when the subtitle
    /// text is translated and full token alignment is impossible.  A unique, nearby exact token
    /// such as `Enter` is useful; the rest of the translated timeline remains cue-only.  This
    /// deliberately does not claim granular timing or invent a translation for an unknown label.
    pub fn project_required_anchor_times(&mut self, words: &[GranularSourceWord]) -> usize {
        self.projected_anchor_ids.clear();
        for phrase in self.phrases.iter_mut().filter(|phrase| phrase.required) {
            let keys = phrase
                .text
                .split_whitespace()
                .map(token_key)
                .filter(|key| key.len() >= 4 && !is_anchor_stop_word(key))
                .collect::<Vec<_>>();
            let mut matches = Vec::new();
            for key in keys {
                for (index, word) in words.iter().enumerate() {
                    if token_key(&word.text) != key {
                        continue;
                    }
                    let distance =
                        i64::from(word.start_ms).abs_diff(i64::from(phrase.preferred_start_ms));
                    if distance <= 20_000 {
                        matches.push(index);
                    }
                }
            }
            matches.sort_unstable();
            matches.dedup();
            if matches.len() != 1 {
                continue;
            }
            let word = &words[matches[0]];
            phrase.preferred_start_ms = word.start_ms;
            phrase.preferred_end_ms = word.end_ms.max(word.start_ms + 1);
            self.projected_anchor_ids.push(phrase.id.clone());
        }
        self.projected_anchor_ids.sort();
        self.projected_anchor_ids.dedup();
        self.projected_anchor_count = self.projected_anchor_ids.len();
        self.projected_anchor_count
    }

    pub fn has_projected_anchor_timing(&self) -> bool {
        !self.projected_anchor_ids.is_empty()
    }

    pub fn has_semantic_anchor_timing(&self, anchor_id: &str) -> bool {
        self.mode != TimingMode::CueOnly
            || self
                .projected_anchor_ids
                .iter()
                .any(|projected| projected == anchor_id)
    }

    /// Replace caption-derived token windows with word timestamps from an independently
    /// transcribed copy of the source audio. The SRT text and segment ownership remain the
    /// authority; ASR only contributes timing evidence, so recognition mistakes cannot rewrite
    /// what Gemini or TTS will say.
    pub fn from_files_with_granular(
        source_srt: &Path,
        clean_srt: &Path,
        words: &[GranularSourceWord],
    ) -> Result<Self> {
        let mut timeline = Self::from_files(source_srt, clean_srt)?;
        if words.is_empty() {
            bail!("Granular source timing contains no words");
        }
        let source = parse_srt(clean_srt)
            .with_context(|| format!("Failed to parse cleaned SRT {}", clean_srt.display()))?;
        let (mut matched, matched_count) = align_granular_words(&timeline.tokens, words);
        let minimum_matches = if timeline.tokens.len() < 20 {
            timeline.tokens.len()
        } else {
            timeline.tokens.len() * 60 / 100
        };
        if matched_count < minimum_matches {
            bail!(
                "Granular source timing matched {matched_count}/{} source words; need at least {minimum_matches}",
                timeline.tokens.len()
            );
        }
        fill_unmatched_timing(&mut matched, &timeline.tokens);
        for (token, timing) in timeline.tokens.iter_mut().zip(matched) {
            let Some((start_ms, end_ms)) = timing else {
                bail!("Granular source timing left an unmatched source word");
            };
            token.start_ms = start_ms;
            token.end_ms = end_ms.max(start_ms + 1);
            token.confidence = TimingConfidence::Granular;
        }
        for phrase in &mut timeline.phrases {
            let start = timeline
                .tokens
                .iter()
                .find(|token| token.id == phrase.start_token)
                .context("Granular phrase start token disappeared")?;
            let end = timeline
                .tokens
                .iter()
                .find(|token| token.id == phrase.end_token)
                .context("Granular phrase end token disappeared")?;
            phrase.preferred_start_ms = start.start_ms;
            phrase.preferred_end_ms = end.end_ms.max(start.start_ms + 1);
            phrase.confidence = TimingConfidence::Granular;
        }
        let mut barriers = vec![SourceBarrier {
            id: "barrier-start".to_owned(),
            time_ms: timeline.tokens.first().map_or(0, |token| token.start_ms),
            reason: "videoStart".to_owned(),
        }];
        for (position, segment) in source.iter().enumerate() {
            let current = timeline
                .tokens
                .iter()
                .filter(|token| token.source_segment == segment.index)
                .collect::<Vec<_>>();
            let Some(last) = current.last() else {
                continue;
            };
            if let Some(next) = source.get(position + 1) {
                let next_start = timeline
                    .tokens
                    .iter()
                    .find(|token| token.source_segment == next.index)
                    .map_or(last.end_ms, |token| token.start_ms);
                if next_start.saturating_sub(last.end_ms) >= 800 && ends_sentence(&segment.text) {
                    barriers.push(SourceBarrier {
                        id: format!("barrier-gap-{}", segment.index),
                        time_ms: next_start,
                        reason: "confirmedPause".to_owned(),
                    });
                }
            }
        }
        let duration_ms = timeline.tokens.last().map_or(0, |token| token.end_ms);
        barriers.push(SourceBarrier {
            id: "barrier-end".to_owned(),
            time_ms: duration_ms,
            reason: "videoEnd".to_owned(),
        });
        barriers.sort_by_key(|barrier| barrier.time_ms);
        barriers.dedup_by_key(|barrier| barrier.time_ms);
        timeline.mode = TimingMode::Granular;
        timeline.timing_provider = Some("azureFastTranscription".to_owned());
        timeline.timing_fallback_reason = None;
        timeline.granular_match_count = Some(matched_count);
        timeline.granular_source_word_count = Some(timeline.tokens.len());
        timeline.barriers = barriers;
        timeline.duration_ms = duration_ms;
        timeline.validate()?;
        Ok(timeline)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != SOURCE_TIMELINE_VERSION {
            bail!("Unsupported source timeline version {}", self.version);
        }
        let mut token_ids = HashSet::new();
        let mut previous_start = 0;
        let mut previous_end = 0;
        for token in &self.tokens {
            if !token_ids.insert(&token.id) {
                bail!("Duplicate source token id {}", token.id);
            }
            if token.start_ms < previous_start
                || token.end_ms < previous_end
                || token.end_ms < token.start_ms
            {
                bail!("Source timeline token timestamps are not monotonic");
            }
            previous_start = token.start_ms;
            previous_end = token.end_ms;
        }
        if self.duration_ms < previous_end {
            bail!("Source timeline duration is shorter than its final token");
        }
        let positions = self
            .tokens
            .iter()
            .enumerate()
            .map(|(position, token)| (token.id.as_str(), position))
            .collect::<HashMap<_, _>>();
        let mut phrase_ids = HashSet::new();
        for phrase in &self.phrases {
            if !phrase_ids.insert(&phrase.id) {
                bail!("Duplicate source phrase id {}", phrase.id);
            }
            let Some(&start) = positions.get(phrase.start_token.as_str()) else {
                bail!("Source phrase {} starts at an unknown token", phrase.id);
            };
            let Some(&end) = positions.get(phrase.end_token.as_str()) else {
                bail!("Source phrase {} ends at an unknown token", phrase.id);
            };
            if start > end || phrase.start_token.is_empty() || phrase.end_token.is_empty() {
                bail!(
                    "Source phrase {} is not a contiguous token range",
                    phrase.id
                );
            }
            if phrase.preferred_start_ms > phrase.preferred_end_ms {
                bail!("Source phrase {} has reversed timestamps", phrase.id);
            }
        }
        let required_phrase_ids = self
            .phrases
            .iter()
            .filter(|phrase| phrase.required)
            .map(|phrase| phrase.id.as_str())
            .collect::<HashSet<_>>();
        let projected_ids = self
            .projected_anchor_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        if projected_ids.len() != self.projected_anchor_ids.len()
            || self.projected_anchor_count != self.projected_anchor_ids.len()
            || !projected_ids.is_subset(&required_phrase_ids)
        {
            bail!("Source timeline projected anchor provenance is invalid");
        }
        let mut previous_barrier = 0;
        let mut barrier_ids = HashSet::new();
        for barrier in &self.barriers {
            if !barrier_ids.insert(&barrier.id)
                || barrier.time_ms < previous_barrier
                || barrier.time_ms > self.duration_ms
            {
                bail!("Source timeline barriers are invalid");
            }
            previous_barrier = barrier.time_ms;
        }
        Ok(())
    }

    pub fn required_phrases_for_segment(&self, index: usize) -> Vec<&SourcePhrase> {
        self.phrases
            .iter()
            .filter(|phrase| phrase.owner_index == index && phrase.required)
            .collect()
    }

    /// Compute an anchor's source-relative position from token ownership, not subtitle text.
    /// This remains valid after the display SRT has been translated into another language.
    pub fn phrase_relative_position(&self, phrase: &SourcePhrase) -> Option<f64> {
        let owner_tokens = self
            .tokens
            .iter()
            .filter(|token| token.source_segment == phrase.owner_index)
            .collect::<Vec<_>>();
        let start = owner_tokens
            .iter()
            .position(|token| token.id == phrase.start_token)?;
        Some(start as f64 / owner_tokens.len().max(1) as f64)
    }

    pub fn segment_window(&self, index: usize) -> Option<(u32, u32)> {
        let tokens = self
            .tokens
            .iter()
            .filter(|token| token.source_segment == index)
            .collect::<Vec<_>>();
        Some((tokens.first()?.start_ms, tokens.last()?.end_ms))
    }
}

impl SyncLayout {
    /// Validate that the derived narration graph is a lossless, ordered projection of the SRT.
    /// The mixer never needs to infer coverage from timestamps; this is the single structural
    /// check shared by allocation and TTS orchestration.
    pub fn validate(&self, expected_segments: &[usize]) -> Result<()> {
        if self.blocks.is_empty() || self.islands.is_empty() {
            bail!("Source-aligned layout must contain blocks and sync islands");
        }
        let covered = self
            .blocks
            .iter()
            .flat_map(|block| block.source_segments.iter().copied())
            .collect::<Vec<_>>();
        if covered != expected_segments || covered.windows(2).any(|pair| pair[0] == pair[1]) {
            bail!("Source-aligned blocks do not match the expected subtitle order and coverage");
        }

        let mut block_ids = HashSet::new();
        let mut block_islands = HashMap::new();
        for block in &self.blocks {
            if !block_ids.insert(&block.id)
                || block.start_ms >= block.end_ms
                || block.source_segments.is_empty()
                || block.island_id.is_empty()
            {
                bail!("Source-aligned layout contains an invalid or duplicate block");
            }
            if !block
                .source_segments
                .windows(2)
                .all(|pair| pair[1] > pair[0])
            {
                bail!("Source-aligned block segments are not strictly ordered");
            }
            block_islands.insert(block.id.as_str(), block.island_id.as_str());
        }
        let mut ordered_blocks = self.blocks.iter().collect::<Vec<_>>();
        ordered_blocks.sort_by_key(|block| block.start_ms);
        if ordered_blocks
            .windows(2)
            .any(|pair| pair[1].start_ms < pair[0].end_ms)
        {
            bail!("Source-aligned narration blocks overlap");
        }

        let mut seen_blocks = HashSet::new();
        let mut island_ids = HashSet::new();
        let mut previous_island_end = 0;
        for island in &self.islands {
            if island.id.is_empty()
                || !island_ids.insert(&island.id)
                || island.start_ms >= island.end_ms
                || island.end_ms.saturating_sub(island.start_ms) > MAX_SYNC_ISLAND_DURATION_MS
                || island.start_ms < previous_island_end
                || island.block_ids.is_empty()
            {
                bail!("Source-aligned layout contains an invalid sync island");
            }
            previous_island_end = island.end_ms;
            let mut island_blocks = HashSet::new();
            for block_id in &island.block_ids {
                if !island_blocks.insert(block_id)
                    || !seen_blocks.insert(block_id)
                    || block_islands.get(block_id.as_str()) != Some(&island.id.as_str())
                {
                    bail!(
                        "Sync island {} has invalid or duplicate block coverage",
                        island.id
                    );
                }
                let block = self
                    .blocks
                    .iter()
                    .find(|block| block.id == *block_id)
                    .expect("block_islands contains only known ids");
                if block.start_ms < island.start_ms || block.end_ms > island.end_ms {
                    bail!("Block {} escapes sync island {}", block.id, island.id);
                }
            }
        }
        if seen_blocks.len() != self.blocks.len() {
            bail!("Sync islands do not cover every narration block exactly once");
        }
        Ok(())
    }
}

pub fn build_sync_layout(timeline: &SourceTimeline, segments: &[SrtSegment]) -> Result<SyncLayout> {
    timeline.validate()?;
    if segments.is_empty() {
        bail!("Cannot build sync layout without subtitle segments");
    }
    let source_text = segments
        .iter()
        .map(|segment| {
            let text = timeline
                .tokens
                .iter()
                .filter(|token| token.source_segment == segment.index)
                .map(|token| token.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            (segment.index, text)
        })
        .collect::<HashMap<_, _>>();
    let mut blocks = Vec::new();
    let mut current_segments = Vec::new();
    let mut current_start = 0;
    let mut current_end = 0;
    let mut current_words = 0;
    for (position, segment) in segments.iter().enumerate() {
        // Use the reconstructed token start rather than the rolling cue's raw start.  The raw
        // cue can begin before the previous cue's newly introduced words and would make two
        // otherwise ordered narration blocks overlap at the island boundary.
        let start_ms = timeline
            .segment_window(segment.index)
            .map(|(start, _)| start)
            .unwrap_or(timestamp_ms(&segment.start)?);
        let end_ms = timeline
            .tokens
            .iter()
            .filter(|token| token.source_segment == segment.index)
            .map(|token| token.end_ms)
            .max()
            .unwrap_or(timestamp_ms(&segment.end)?);
        let current_text = source_text
            .get(&segment.index)
            .map(String::as_str)
            .unwrap_or(&segment.text);
        let previous_text = position
            .checked_sub(1)
            .and_then(|previous| source_text.get(&segments[previous].index))
            .map(String::as_str)
            .unwrap_or_default();
        let starts_new = !current_segments.is_empty()
            && (ends_sentence(previous_text)
                // A required visual action starts a fresh narration block. Otherwise an
                // explanatory fragment before "click Run" can consume the action's timing
                // budget before its semantic bookmark is rendered.
                || !timeline
                    .required_phrases_for_segment(segment.index)
                    .is_empty()
                || has_barrier_between(timeline, current_end, start_ms)
                // Rolling captions often expose one sentence across several overlapping cues;
                // split only after a real sentence/anchor/barrier or a genuinely long block.
                // The old 12 s / 45-word limits split normal tutorial sentences and created
                // artificial over-cap slots before the next rolling cue arrived.
                || end_ms.saturating_sub(current_start) > MAX_NARRATION_BLOCK_DURATION_MS
                || current_words + current_text.split_whitespace().count()
                    > MAX_NARRATION_BLOCK_WORDS);
        if starts_new {
            push_block(
                timeline,
                &mut blocks,
                &current_segments,
                current_start,
                current_end,
            );
            current_segments.clear();
            current_words = 0;
            current_end = 0;
        }
        if current_segments.is_empty() {
            current_start = start_ms;
        }
        current_end = current_end.max(end_ms);
        current_words += current_text.split_whitespace().count();
        current_segments.push(segment.index);
    }
    push_block(
        timeline,
        &mut blocks,
        &current_segments,
        current_start,
        current_end,
    );

    let mut islands = Vec::new();
    let mut current_island: Option<SyncIsland> = None;
    let mut hard_boundary_before = false;
    for block in &mut blocks {
        let split = current_island.as_ref().is_some_and(|island| {
            has_barrier_between(timeline, island.end_ms, block.start_ms)
                || block.end_ms.saturating_sub(island.start_ms) > MAX_SYNC_ISLAND_DURATION_MS
        });
        if split {
            hard_boundary_before = current_island
                .as_ref()
                .is_some_and(|island| has_barrier_between(timeline, island.end_ms, block.start_ms));
            islands.push(current_island.take().expect("split implies island"));
        }
        let island = current_island.get_or_insert_with(|| SyncIsland {
            id: format!("island-{}", islands.len() + 1),
            start_ms: block.start_ms,
            end_ms: block.end_ms,
            block_ids: Vec::new(),
            hard_boundary_before,
        });
        hard_boundary_before = false;
        island.end_ms = island.end_ms.max(block.end_ms);
        island.block_ids.push(block.id.clone());
        block.island_id = island.id.clone();
    }
    if let Some(island) = current_island {
        islands.push(island);
    }
    let layout = SyncLayout { blocks, islands };
    layout.validate(
        &segments
            .iter()
            .map(|segment| segment.index)
            .collect::<Vec<_>>(),
    )?;
    Ok(layout)
}

fn push_block(
    timeline: &SourceTimeline,
    blocks: &mut Vec<NarrationBlock>,
    source_segments: &[usize],
    start_ms: u32,
    end_ms: u32,
) {
    if source_segments.is_empty() {
        return;
    }
    let anchor_ids = source_segments
        .iter()
        .flat_map(|index| timeline.required_phrases_for_segment(*index))
        .filter(|phrase| timeline.has_semantic_anchor_timing(&phrase.id))
        .map(|phrase| phrase.id.clone())
        .collect::<Vec<_>>();
    let first = source_segments[0];
    let last = source_segments[source_segments.len() - 1];
    let preferred_start_ms = source_segments
        .iter()
        .flat_map(|index| timeline.required_phrases_for_segment(*index))
        .filter(|phrase| timeline.has_semantic_anchor_timing(&phrase.id))
        .map(|phrase| phrase.preferred_start_ms)
        .min()
        .map_or(start_ms, |anchor_start| anchor_start.min(start_ms));
    blocks.push(NarrationBlock {
        id: format!("block-{first}-{last}"),
        start_ms,
        end_ms: end_ms.max(start_ms + 1),
        preferred_start_ms,
        source_segments: source_segments.to_vec(),
        anchor_ids,
        island_id: String::new(),
    });
}

fn has_barrier_between(timeline: &SourceTimeline, left_ms: u32, right_ms: u32) -> bool {
    timeline
        .barriers
        .iter()
        .any(|barrier| barrier.time_ms > left_ms && barrier.time_ms <= right_ms)
}

#[derive(Clone, Debug)]
struct AllocationSpec {
    id: String,
    preferred_start_ms: u32,
    predicted_audio_ms: u32,
    anchor_count: usize,
    measured_anchors: Vec<MeasuredAnchor>,
}

#[derive(Clone, Debug)]
struct AllocationWindow {
    id: String,
    island_id: String,
    render_start_ms: u32,
    slot_end_ms: u32,
    predicted_audio_ms: u32,
    rendered_audio_ms: u32,
    tempo: f64,
}

fn max_move_ms(mode: NaturalMode) -> u32 {
    match mode {
        NaturalMode::Sync => 0,
        NaturalMode::Balanced => 250,
        NaturalMode::Natural => 500,
    }
}

/// Return allocation units for each bounded sync island.  The 60-second cap is a real locality
/// boundary: slack cannot be borrowed by a later island just because no hard source barrier was
/// detected.
pub fn allocation_groups(layout: &SyncLayout) -> Vec<AllocationGroup> {
    let mut groups = Vec::new();
    let mut current: Option<AllocationGroup> = None;
    for island in &layout.islands {
        let starts_new = current.is_some();
        if starts_new {
            groups.push(current.take().expect("hard boundary implies a group"));
        }
        let group = current.get_or_insert_with(|| AllocationGroup {
            id: island.id.clone(),
            start_ms: island.start_ms,
            end_ms: island.end_ms,
            island_ids: Vec::new(),
            block_ids: Vec::new(),
            crosses_soft_boundary: false,
        });
        group.end_ms = group.end_ms.max(island.end_ms);
        group.island_ids.push(island.id.clone());
        group.block_ids.extend(island.block_ids.iter().cloned());
        group.crosses_soft_boundary |= group.island_ids.len() > 1;
    }
    if let Some(group) = current {
        groups.push(group);
    }
    groups
}

fn allocation_tempo(measured_ms: u32, slot_ms: u32) -> f64 {
    f64::from(measured_ms) / f64::from(slot_ms.max(1))
}

fn allocation_fits_cap(measured_ms: u32, slot_ms: u32) -> bool {
    allocation_tempo(measured_ms, slot_ms) <= MAX_TIMING_TEMPO + f64::EPSILON
}

fn refresh_allocated_block(block: &mut AllocatedBlock, measured_ms: u32) {
    let slot_ms = block
        .slot_end_ms
        .saturating_sub(block.render_start_ms)
        .max(1);
    let tempo = allocation_tempo(measured_ms, slot_ms).max(1.0);
    block.predicted_audio_ms = measured_ms;
    block.tempo = tempo;
    block.rendered_audio_ms = ((f64::from(measured_ms) / tempo).ceil() as u32).min(slot_ms);
}

/// Borrow the previous block's unused tail after TTS is measured.
///
/// The 250 ms preferred-start window cannot use a multi-second silent setup cue. Speech never
/// overlaps, island bounds stay hard, and the stretch cap stays 1.13x. Blocks that already fit
/// are left on their preferred start.
pub fn apply_measured_spillback(
    allocations: &mut HashMap<String, AllocatedBlock>,
    measured_audio_ms: &HashMap<String, u32>,
    layout: &SyncLayout,
) {
    apply_measured_spillback_except(allocations, measured_audio_ms, layout, &HashSet::new());
}

/// Same as [`apply_measured_spillback`], but never moves a listed current block.
///
/// Hard semantic bookmarks keep their measured start. Shrinking the previous unused tail does
/// not move that previous block's start.
pub fn apply_measured_spillback_except(
    allocations: &mut HashMap<String, AllocatedBlock>,
    measured_audio_ms: &HashMap<String, u32>,
    layout: &SyncLayout,
    skip_current_ids: &HashSet<String>,
) {
    for group in allocation_groups(layout) {
        let mut ordered = group.block_ids.clone();
        ordered.sort_by_key(|block_id| {
            allocations
                .get(block_id)
                .map_or(u32::MAX, |block| block.render_start_ms)
        });
        for index in 1..ordered.len() {
            let previous_id = ordered[index - 1].clone();
            let current_id = ordered[index].clone();
            let Some(previous_measured) = measured_audio_ms.get(&previous_id).copied() else {
                continue;
            };
            let Some(current_measured) = measured_audio_ms.get(&current_id).copied() else {
                continue;
            };
            if skip_current_ids.contains(&current_id) {
                continue;
            }
            let (island_id, current_start, current_end, previous_start, previous_tempo) = {
                let Some(previous) = allocations.get(&previous_id) else {
                    continue;
                };
                let Some(current) = allocations.get(&current_id) else {
                    continue;
                };
                if previous.island_id != current.island_id {
                    continue;
                }
                (
                    current.island_id.clone(),
                    current.render_start_ms,
                    current.slot_end_ms,
                    previous.render_start_ms,
                    previous.tempo,
                )
            };
            let current_slot = current_end.saturating_sub(current_start).max(1);
            if allocation_fits_cap(current_measured, current_slot) {
                continue;
            }
            let island_start = layout
                .islands
                .iter()
                .find(|island| island.id == island_id)
                .map_or(0, |island| island.start_ms);
            let previous_speech_end = previous_start.saturating_add(
                (f64::from(previous_measured) / previous_tempo.max(1.0)).ceil() as u32,
            );
            let earliest = island_start.max(previous_speech_end);
            if earliest >= current_start {
                continue;
            }
            let min_previous_slot = (f64::from(previous_measured) / MAX_TIMING_TEMPO).ceil() as u32;
            let earliest = earliest.max(previous_start.saturating_add(min_previous_slot.max(1)));
            if earliest >= current_start {
                continue;
            }
            let borrow_for_unity = current_measured.saturating_sub(current_slot);
            let borrowed = borrow_for_unity.min(current_start.saturating_sub(earliest));
            if borrowed == 0 {
                continue;
            }
            let new_start = current_start - borrowed;
            if let Some(previous) = allocations.get_mut(&previous_id)
                && previous.slot_end_ms > new_start
            {
                previous.slot_end_ms = new_start;
                refresh_allocated_block(previous, previous_measured);
            }
            if let Some(current) = allocations.get_mut(&current_id) {
                current.render_start_ms = new_start;
                refresh_allocated_block(current, current_measured);
            }
        }
    }
}

/// Keep a block's slot covering its measured hard source actions.
///
/// Duration packing and spillback may steal a tail to help a later over-cap neighbor. If that tail
/// holds `nhấn Enter` / click timing, semantic repair cannot recover: the expected source time is
/// then after `slot_end`. Extend the slot and push the next start when the next block is not
/// itself hard-anchored.
pub fn cover_hard_anchor_windows(
    allocations: &mut HashMap<String, AllocatedBlock>,
    measured_audio_ms: &HashMap<String, u32>,
    layout: &SyncLayout,
    measured_anchors: &HashMap<String, Vec<MeasuredAnchor>>,
) {
    let hard_ids = measured_anchors
        .iter()
        .filter(|(_, anchors)| {
            anchors
                .iter()
                .any(|anchor| anchor.coupling == Coupling::Anchored)
        })
        .map(|(block_id, _)| block_id.clone())
        .collect::<HashSet<_>>();
    for group in allocation_groups(layout) {
        let mut ordered = group.block_ids.clone();
        ordered.sort_by_key(|block_id| {
            allocations
                .get(block_id)
                .map_or(u32::MAX, |block| block.render_start_ms)
        });
        let island_end = layout
            .islands
            .iter()
            .find(|island| island.id == group.id)
            .map_or(u32::MAX, |island| island.end_ms);
        for index in 0..ordered.len() {
            let block_id = ordered[index].clone();
            let Some(min_end) = measured_anchors.get(&block_id).and_then(|anchors| {
                anchors
                    .iter()
                    .filter(|anchor| anchor.coupling == Coupling::Anchored)
                    .map(|anchor| anchor.source_ms)
                    .max()
            }) else {
                continue;
            };
            let Some(current) = allocations.get(&block_id) else {
                continue;
            };
            if current.slot_end_ms >= min_end {
                continue;
            }
            let new_end = min_end.min(island_end);
            if new_end <= current.slot_end_ms {
                continue;
            }
            if let Some(next_id) = ordered.get(index + 1) {
                let Some(next) = allocations.get(next_id) else {
                    continue;
                };
                if hard_ids.contains(next_id) && next.render_start_ms < new_end {
                    continue;
                }
                if next.render_start_ms < new_end {
                    let next_measured = measured_audio_ms.get(next_id).copied();
                    if let Some(next) = allocations.get_mut(next_id) {
                        next.render_start_ms = new_end;
                        if next.slot_end_ms <= next.render_start_ms {
                            next.slot_end_ms = next.render_start_ms.saturating_add(1);
                        }
                        if let Some(measured) = next_measured {
                            refresh_allocated_block(next, measured);
                        }
                    }
                }
            }
            if let Some(current) = allocations.get_mut(&block_id) {
                current.slot_end_ms = new_end;
                if let Some(measured) = measured_audio_ms.get(&block_id).copied() {
                    refresh_allocated_block(current, measured);
                }
            }
        }
    }
}

pub(crate) fn group_exceeds_tempo_cap(
    group: &AllocationGroup,
    allocations: &HashMap<String, AllocatedBlock>,
    measured_audio_ms: &HashMap<String, u32>,
) -> bool {
    group.block_ids.iter().any(|block_id| {
        let Some(measured) = measured_audio_ms.get(block_id).copied() else {
            return false;
        };
        let Some(allocation) = allocations.get(block_id) else {
            return false;
        };
        !allocation_fits_cap(
            measured,
            allocation
                .slot_end_ms
                .saturating_sub(allocation.render_start_ms),
        )
    })
}

/// Allocate one local clock. Returned slot ends are allocated values; callers must not replace
/// them with the next block's start. The bounded isotonic pass minimizes weighted start drift
/// for each island, while the outer scan chooses the smallest common tempo cap that is feasible.
pub fn allocate_blocks(
    layout: &SyncLayout,
    predicted_audio_ms: &HashMap<String, u32>,
    mode: NaturalMode,
) -> Result<Vec<AllocatedBlock>> {
    let (allocated, failed_islands) =
        allocate_blocks_best_effort(layout, predicted_audio_ms, mode)?;
    if !failed_islands.is_empty() {
        bail!(
            "Sync islands are infeasible within the {:.2}x timing cap: {}",
            MAX_TIMING_TEMPO,
            failed_islands.join(", ")
        );
    }
    Ok(allocated)
}

/// Allocate every feasible island and return deterministic review placeholders for islands that
/// cannot fit. The aligned TTS path can then preserve unrelated work instead of restarting the
/// whole video; callers must mark failed-island placeholders review-required.
pub fn allocate_blocks_best_effort(
    layout: &SyncLayout,
    predicted_audio_ms: &HashMap<String, u32>,
    mode: NaturalMode,
) -> Result<(Vec<AllocatedBlock>, Vec<String>)> {
    layout.validate(
        &layout
            .blocks
            .iter()
            .flat_map(|block| block.source_segments.iter().copied())
            .collect::<Vec<_>>(),
    )?;
    let mut allocated = Vec::with_capacity(layout.blocks.len());
    let mut failed_islands = Vec::new();
    for group in allocation_groups(layout) {
        let allocation_island = SyncIsland {
            id: group.id.clone(),
            start_ms: group.start_ms,
            end_ms: group.end_ms,
            block_ids: group.block_ids.clone(),
            hard_boundary_before: false,
        };
        let specs = group
            .block_ids
            .iter()
            .map(|block_id| {
                let block = layout
                    .blocks
                    .iter()
                    .find(|block| &block.id == block_id)
                    .with_context(|| format!("Unknown block {block_id}"))?;
                Ok(AllocationSpec {
                    id: block.id.clone(),
                    preferred_start_ms: block.preferred_start_ms,
                    predicted_audio_ms: predicted_audio_ms
                        .get(block_id)
                        .copied()
                        .unwrap_or_else(|| block.end_ms.saturating_sub(block.start_ms).max(100)),
                    anchor_count: block.anchor_ids.len(),
                    measured_anchors: Vec::new(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        match allocate_island_with_move(
            &specs,
            &allocation_island,
            mode,
            group
                .crosses_soft_boundary
                .then_some(group.end_ms.saturating_sub(group.start_ms)),
        ) {
            Ok(windows) => allocated.extend(windows.into_iter().map(|window| AllocatedBlock {
                block_id: window.id,
                island_id: window.island_id,
                render_start_ms: window.render_start_ms,
                slot_end_ms: window.slot_end_ms,
                predicted_audio_ms: window.predicted_audio_ms,
                rendered_audio_ms: window.rendered_audio_ms,
                tempo: window.tempo,
            })),
            Err(_) => {
                failed_islands.extend(group.island_ids.iter().cloned());
                allocated.extend(specs.iter().map(|spec| {
                    let block = layout
                        .blocks
                        .iter()
                        .find(|block| block.id == spec.id)
                        .expect("allocation spec comes from layout");
                    AllocatedBlock {
                        block_id: spec.id.clone(),
                        island_id: group.id.clone(),
                        render_start_ms: block.start_ms,
                        slot_end_ms: block.end_ms.max(block.start_ms + 1),
                        predicted_audio_ms: spec.predicted_audio_ms,
                        rendered_audio_ms: spec.predicted_audio_ms,
                        tempo: 1.0,
                    }
                }));
            }
        }
    }
    allocated.sort_by_key(|block| block.render_start_ms);
    validate_non_overlapping_windows(
        allocated
            .iter()
            .map(|block| (block.render_start_ms, block.slot_end_ms))
            .collect::<Vec<_>>(),
    )?;
    Ok((allocated, failed_islands))
}

/// Reallocate one island after Azure has reported the real whole-utterance durations.
///
/// Preflight allocation is intentionally only a synthesis order/capacity hint.  This helper is
/// the second pass from the plan: callers must provide a duration for every block in the island,
/// then use the returned starts/slots for the manifest and final anchor validation.  Keeping this
/// operation island-local prevents one overlong region from moving unrelated video sections.
pub fn allocate_measured_island(
    layout: &SyncLayout,
    island_id: &str,
    measured_audio_ms: &HashMap<String, u32>,
    mode: NaturalMode,
) -> Result<Vec<AllocatedBlock>> {
    let island = layout
        .islands
        .iter()
        .find(|island| island.id == island_id)
        .with_context(|| format!("Unknown sync island {island_id}"))?;
    allocate_measured_unit(
        layout,
        &island.id,
        island.start_ms,
        island.end_ms,
        &island.block_ids,
        measured_audio_ms,
        &HashMap::new(),
        mode,
        false,
    )
}

/// Reallocate one complete elastic allocation group.  It may span several diagnostic islands
/// separated only by the 60-second cap, but never crosses a confirmed source barrier.
pub fn allocate_measured_group(
    layout: &SyncLayout,
    group_id: &str,
    measured_audio_ms: &HashMap<String, u32>,
    mode: NaturalMode,
) -> Result<Vec<AllocatedBlock>> {
    allocate_measured_group_with_anchors(layout, group_id, measured_audio_ms, &HashMap::new(), mode)
}

/// Reallocate a group using measured semantic bookmark offsets when available.  A bookmark is
/// an internal phrase position, so the block's preferred start is the source anchor time minus
/// the measured offset to that bookmark, not merely the subtitle cue start.
pub fn allocate_measured_group_with_anchors(
    layout: &SyncLayout,
    group_id: &str,
    measured_audio_ms: &HashMap<String, u32>,
    measured_anchors: &HashMap<String, Vec<MeasuredAnchor>>,
    mode: NaturalMode,
) -> Result<Vec<AllocatedBlock>> {
    let group = allocation_groups(layout)
        .into_iter()
        .find(|group| group.id == group_id)
        .with_context(|| format!("Unknown allocation group {group_id}"))?;
    allocate_measured_unit(
        layout,
        &group.id,
        group.start_ms,
        group.end_ms,
        &group.block_ids,
        measured_audio_ms,
        measured_anchors,
        mode,
        group.crosses_soft_boundary,
    )
}

fn allocate_measured_unit(
    layout: &SyncLayout,
    id: &str,
    start_ms: u32,
    end_ms: u32,
    block_ids: &[String],
    measured_audio_ms: &HashMap<String, u32>,
    measured_anchors: &HashMap<String, Vec<MeasuredAnchor>>,
    mode: NaturalMode,
    crosses_soft_boundary: bool,
) -> Result<Vec<AllocatedBlock>> {
    let specs = block_ids
        .iter()
        .map(|block_id| {
            let block = layout
                .blocks
                .iter()
                .find(|block| &block.id == block_id)
                .with_context(|| format!("Unknown block {block_id}"))?;
            let measured = measured_audio_ms
                .get(block_id)
                .copied()
                .with_context(|| format!("Missing measured duration for block {block_id}"))?;
            Ok(AllocationSpec {
                id: block.id.clone(),
                preferred_start_ms: block.preferred_start_ms,
                predicted_audio_ms: measured.max(1),
                anchor_count: block.anchor_ids.len(),
                measured_anchors: measured_anchors.get(block_id).cloned().unwrap_or_default(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let unit = SyncIsland {
        id: id.to_owned(),
        start_ms,
        end_ms,
        block_ids: block_ids.to_vec(),
        hard_boundary_before: false,
    };
    Ok(allocate_island_with_move(
        &specs,
        &unit,
        mode,
        crosses_soft_boundary.then_some(end_ms.saturating_sub(start_ms)),
    )?
    .into_iter()
    .map(|window| AllocatedBlock {
        block_id: window.id,
        island_id: window.island_id,
        render_start_ms: window.render_start_ms,
        slot_end_ms: window.slot_end_ms,
        predicted_audio_ms: window.predicted_audio_ms,
        rendered_audio_ms: window.rendered_audio_ms,
        tempo: window.tempo,
    })
    .collect())
}

fn allocate_island_with_move(
    specs: &[AllocationSpec],
    island: &SyncIsland,
    mode: NaturalMode,
    max_move_override: Option<u32>,
) -> Result<Vec<AllocationWindow>> {
    if specs.is_empty() {
        bail!("Sync island {} has no narration blocks", island.id);
    }
    let span_ms = island.end_ms.saturating_sub(island.start_ms);
    // A cap-only group has no confirmed visual boundary, but it is still local.  Do not let a
    // long group turn a 60-second island cap into whole-video drift: every block keeps the mode's
    // bounded movement window.
    let max_move = max_move_override.map_or_else(
        || max_move_ms(mode),
        |value| value.min(MAX_SOFT_BOUNDARY_DRIFT_MS as u32),
    );
    // A thousandth-of-a-tempo scan is deterministic and keeps the first feasible cap (the
    // primary tie-break). The final manifest still validates measured audio independently.
    for step in 0..=((MAX_TIMING_TEMPO * 1_000.0).round() as u32 - 1_000) {
        let cap = 1.0 + f64::from(step) / 1_000.0;
        let rendered_durations = specs
            .iter()
            .map(|spec| (f64::from(spec.predicted_audio_ms) / cap).ceil().max(1.0) as u32)
            .collect::<Vec<_>>();
        if rendered_durations
            .iter()
            .map(|duration| u64::from(*duration))
            .sum::<u64>()
            > u64::from(span_ms)
        {
            continue;
        }
        let Ok(starts) = bounded_isotonic_starts(specs, &rendered_durations, island, max_move)
        else {
            continue;
        };
        let (rendered_durations, starts) =
            relax_rendered_durations(specs, rendered_durations, starts, island, max_move);
        return Ok(specs
            .iter()
            .zip(rendered_durations)
            .zip(starts)
            .map(|((spec, rendered), start)| AllocationWindow {
                id: spec.id.clone(),
                island_id: island.id.clone(),
                render_start_ms: start,
                slot_end_ms: start.saturating_add(rendered),
                predicted_audio_ms: spec.predicted_audio_ms,
                rendered_audio_ms: rendered,
                tempo: (f64::from(spec.predicted_audio_ms) / f64::from(rendered)).max(1.0),
            })
            .collect());
    }
    bail!(
        "Sync island {} cannot fit {} blocks within the {:.2}x timing cap",
        island.id,
        specs.len(),
        MAX_TIMING_TEMPO
    )
}

fn relax_rendered_durations(
    specs: &[AllocationSpec],
    mut durations: Vec<u32>,
    mut starts: Vec<u32>,
    island: &SyncIsland,
    max_move: u32,
) -> (Vec<u32>, Vec<u32>) {
    // The cap search above finds the smallest feasible maximum tempo. Relax shorter blocks back
    // toward 1.0x first, so only blocks that actually need compression retain that cap.
    let mut order = (0..specs.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| (specs[*index].predicted_audio_ms, *index));
    for index in order {
        let mut low = durations[index];
        let mut high = specs[index].predicted_audio_ms.max(low);
        let mut best_starts = starts.clone();
        while low < high {
            let candidate = low + (high - low).div_ceil(2);
            let previous = durations[index];
            durations[index] = candidate;
            match bounded_isotonic_starts(specs, &durations, island, max_move) {
                Ok(candidate_starts) => {
                    low = candidate;
                    best_starts = candidate_starts;
                }
                Err(_) => high = candidate - 1,
            }
            durations[index] = previous;
        }
        durations[index] = low;
        starts = if low == specs[index].predicted_audio_ms {
            bounded_isotonic_starts(specs, &durations, island, max_move).unwrap_or(best_starts)
        } else {
            best_starts
        };
    }
    (durations, starts)
}

#[derive(Clone, Debug)]
struct IsotonicGroup {
    first: usize,
    last: usize,
    weighted_sum: f64,
    weight: f64,
    lower: i64,
    upper: i64,
    value: f64,
}

fn block_start_model(
    spec: &AllocationSpec,
    duration_ms: u32,
    island: &SyncIsland,
    max_move: u32,
) -> Result<(i64, i64, f64, f64)> {
    let Some(max_start) = island.end_ms.checked_sub(duration_ms) else {
        bail!("Block {} is longer than its sync island", spec.id);
    };
    let preferred = i64::from(spec.preferred_start_ms);
    let allowed_move = if spec.anchor_count > 0 {
        max_move.min(MAX_ANCHOR_DRIFT_MS as u32)
    } else {
        max_move
    };
    let has_hard_anchor = spec
        .measured_anchors
        .iter()
        .any(|anchor| anchor.coupling == Coupling::Anchored);
    let (mut lower, mut upper) = if spec.measured_anchors.is_empty() || !has_hard_anchor {
        (
            i64::from(island.start_ms).max(preferred - i64::from(allowed_move)),
            i64::from(max_start).min(preferred + i64::from(allowed_move)),
        )
    } else {
        (i64::from(island.start_ms), i64::from(max_start))
    };
    let mut weighted_sum = preferred as f64;
    let mut weight = 1.0;
    let tempo = (f64::from(spec.predicted_audio_ms) / f64::from(duration_ms.max(1))).max(1.0);
    for anchor in &spec.measured_anchors {
        let rendered_offset = f64::from(anchor.raw_offset_ms) / tempo;
        let target = f64::from(anchor.source_ms) - rendered_offset;
        let tolerance = f64::from(anchor.tolerance_ms);
        // Soft display boundaries influence the weighted objective but never make an island
        // infeasible. Only an explicitly anchored action phrase owns a hard timing window.
        if anchor.coupling == Coupling::Anchored {
            lower = lower.max((target - tolerance).ceil() as i64);
            upper = upper.min((target + tolerance).floor() as i64);
        }
        let base_weight = match anchor.coupling {
            Coupling::Anchored => 100.0,
            Coupling::Soft => 10.0,
            Coupling::Elastic => 1.0,
        };
        let anchor_weight = base_weight * anchor.confidence.multiplier();
        weighted_sum += target * anchor_weight;
        weight += anchor_weight;
    }
    if lower > upper {
        bail!("Block {} has incompatible semantic anchor windows", spec.id);
    }
    Ok((lower, upper, weighted_sum / weight, weight))
}

fn bounded_isotonic_starts(
    specs: &[AllocationSpec],
    durations: &[u32],
    island: &SyncIsland,
    max_move: u32,
) -> Result<Vec<u32>> {
    if specs.len() != durations.len() {
        bail!("Allocator input lengths do not match");
    }
    let mut raw_lower = Vec::with_capacity(specs.len());
    let mut raw_upper = Vec::with_capacity(specs.len());
    let mut targets = Vec::with_capacity(specs.len());
    let mut weights = Vec::with_capacity(specs.len());
    let mut prefix = 0_i64;
    for (spec, duration) in specs.iter().zip(durations) {
        let (lower_start, upper_start, target, weight) =
            block_start_model(spec, *duration, island, max_move)?;
        raw_lower.push(lower_start - prefix);
        raw_upper.push(upper_start - prefix);
        targets.push(target - prefix as f64);
        weights.push(weight);
        prefix += i64::from(*duration);
    }

    // Propagate feasible bounds for the nondecreasing transformed starts.
    let mut lower = raw_lower.clone();
    for position in 1..lower.len() {
        lower[position] = lower[position].max(lower[position - 1]);
    }
    let mut upper = raw_upper.clone();
    for position in (0..upper.len().saturating_sub(1)).rev() {
        upper[position] = upper[position].min(upper[position + 1]);
    }
    if lower.iter().zip(&upper).any(|(low, high)| low > high) {
        bail!("Sync island has incompatible ordered start bounds");
    }

    let mut groups = Vec::<IsotonicGroup>::with_capacity(specs.len());
    for position in 0..specs.len() {
        let weight = weights[position];
        let value = targets[position].clamp(lower[position] as f64, upper[position] as f64);
        groups.push(IsotonicGroup {
            first: position,
            last: position,
            weighted_sum: value * weight,
            weight,
            lower: lower[position],
            upper: upper[position],
            value,
        });
        while groups.len() >= 2 {
            let right = groups.pop().expect("two groups exist");
            let left = groups.pop().expect("two groups exist");
            if left.value <= right.value + f64::EPSILON {
                groups.push(left);
                groups.push(right);
                break;
            }
            let group_lower = left.lower.max(right.lower);
            let group_upper = left.upper.min(right.upper);
            if group_lower > group_upper {
                bail!("Sync island has incompatible weighted start bounds");
            }
            let weight = left.weight + right.weight;
            let value = (left.weighted_sum + right.weighted_sum) / weight;
            groups.push(IsotonicGroup {
                first: left.first,
                last: right.last,
                weighted_sum: left.weighted_sum + right.weighted_sum,
                weight,
                lower: group_lower,
                upper: group_upper,
                value: value.clamp(group_lower as f64, group_upper as f64),
            });
        }
    }

    let mut transformed = vec![0_i64; specs.len()];
    for group in groups {
        let value = group.value.round() as i64;
        for position in group.first..=group.last {
            transformed[position] = value;
        }
    }
    let mut starts = Vec::with_capacity(specs.len());
    prefix = 0;
    let mut previous_end = i64::from(island.start_ms);
    for (position, duration) in durations.iter().enumerate() {
        let start = transformed[position] + prefix;
        let (lower_start, upper_start, _, _) =
            block_start_model(&specs[position], *duration, island, max_move)?;
        if start < lower_start || start > upper_start || start < previous_end {
            bail!("Rounded weighted start schedule is infeasible");
        }
        starts.push(start as u32);
        previous_end = start + i64::from(*duration);
        prefix += i64::from(*duration);
    }
    Ok(starts)
}

fn validate_non_overlapping_windows(windows: Vec<(u32, u32)>) -> Result<()> {
    let mut ordered = windows;
    ordered.sort_unstable_by_key(|(start, _)| *start);
    if ordered.windows(2).any(|pair| pair[1].0 < pair[0].1) {
        bail!("Allocated source timeline blocks overlap");
    }
    Ok(())
}

/// Allocate independent segments for voices that do not expose reliable timing events.
pub fn allocate_segments(
    timeline: &SourceTimeline,
    layout: &SyncLayout,
    segments: &[SrtSegment],
    predicted_audio_ms: &HashMap<usize, u32>,
    mode: NaturalMode,
) -> Result<Vec<AllocatedSegment>> {
    timeline.validate()?;
    layout.validate(
        &segments
            .iter()
            .map(|segment| segment.index)
            .collect::<Vec<_>>(),
    )?;
    let max_move = match mode {
        NaturalMode::Sync => 0,
        NaturalMode::Balanced => 250,
        NaturalMode::Natural => 500,
    };
    let mut result = Vec::with_capacity(segments.len());
    let mut cursor = 0;
    let mut current_island_id = String::new();
    for segment in segments {
        let (preferred, nominal_end) = timeline
            .segment_window(segment.index)
            .unwrap_or((timestamp_ms(&segment.start)?, timestamp_ms(&segment.end)?));
        let block = layout
            .blocks
            .iter()
            .find(|block| block.source_segments.contains(&segment.index))
            .with_context(|| format!("Missing source block for segment {}", segment.index))?;
        let island = layout
            .islands
            .iter()
            .find(|island| island.id == block.island_id)
            .with_context(|| format!("Missing sync island {}", block.island_id))?;
        if current_island_id != island.id {
            current_island_id = island.id.clone();
            cursor = island.start_ms;
        }
        let earliest = cursor.max(preferred.saturating_sub(max_move));
        // Headroom is already included in the Gemini target-duration budget.  Subtracting it
        // again here recreates the old two-clock under-allocation and inflates tempo needlessly.
        let available = nominal_end.saturating_sub(earliest).max(100);
        let predicted = predicted_audio_ms
            .get(&segment.index)
            .copied()
            .unwrap_or(available);
        let mut tempo = (f64::from(predicted) / f64::from(available)).max(1.0);
        if tempo > MAX_TIMING_TEMPO {
            tempo = MAX_TIMING_TEMPO;
        }
        let rendered = (f64::from(predicted) / tempo).ceil() as u32;
        let slot_end = earliest.saturating_add(rendered);
        if slot_end > island.end_ms {
            bail!(
                "Source timing island {} cannot fit segment {} within the {:.2}x timing cap",
                island.id,
                segment.index,
                MAX_TIMING_TEMPO
            );
        }
        result.push(AllocatedSegment {
            segment_index: segment.index,
            island_id: island.id.clone(),
            render_start_ms: earliest,
            slot_end_ms: slot_end,
            predicted_audio_ms: predicted,
            rendered_audio_ms: rendered,
            tempo,
        });
        cursor = slot_end;
    }
    if result
        .windows(2)
        .any(|pair| pair[1].render_start_ms < pair[0].slot_end_ms)
    {
        bail!("Allocated source timing segments overlap");
    }
    Ok(result)
}

#[derive(Clone, Debug)]
struct EvidenceWord {
    text: String,
    start_ms: u32,
    end_ms: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GranularSourceWord {
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
}

const MAX_ALIGNMENT_BAND: usize = 256;

#[derive(Clone, Copy)]
struct AlignmentCell {
    cost: u16,
    operation: u8,
}

fn align_granular_words(
    source: &[SourceToken],
    asr: &[GranularSourceWord],
) -> (Vec<Option<(u32, u32)>>, usize) {
    let n = source.len();
    let m = asr.len();
    let band = MAX_ALIGNMENT_BAND.max(n.abs_diff(m) + 8);
    let mut rows: Vec<(usize, Vec<AlignmentCell>)> = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let low = i.saturating_sub(band);
        let high = (i + band).min(m);
        let mut row = Vec::with_capacity(high - low + 1);
        for j in low..=high {
            let cell = if i == 0 {
                AlignmentCell {
                    cost: j.min(u16::MAX as usize) as u16,
                    operation: b'I',
                }
            } else if j == 0 {
                AlignmentCell {
                    cost: i.min(u16::MAX as usize) as u16,
                    operation: b'D',
                }
            } else {
                let deletion = alignment_cost(&rows, i - 1, j).saturating_add(1);
                let insertion = j
                    .checked_sub(1)
                    .and_then(|previous_column| previous_column.checked_sub(low))
                    .and_then(|index| row.get(index))
                    .map_or(u16::MAX, |cell: &AlignmentCell| cell.cost.saturating_add(1));
                let equal = token_key(&source[i - 1].text) == token_key(&asr[j - 1].text);
                let diagonal =
                    alignment_cost(&rows, i - 1, j - 1).saturating_add(if equal { 0 } else { 1 });
                if diagonal <= deletion && diagonal <= insertion {
                    AlignmentCell {
                        cost: diagonal,
                        operation: if equal { b'M' } else { b'S' },
                    }
                } else if deletion <= insertion {
                    AlignmentCell {
                        cost: deletion,
                        operation: b'D',
                    }
                } else {
                    AlignmentCell {
                        cost: insertion,
                        operation: b'I',
                    }
                }
            };
            row.push(cell);
        }
        rows.push((low, row));
    }

    let mut matched = vec![None; n];
    let mut matched_count = 0;
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        let Some(cell) = alignment_cell(&rows, i, j) else {
            return (matched, matched_count);
        };
        match cell.operation {
            b'M' => {
                let word = &asr[j - 1];
                matched[i - 1] = Some((word.start_ms, word.end_ms));
                matched_count += 1;
                i -= 1;
                j -= 1;
            }
            b'S' | b'D' => i -= 1,
            b'I' => j -= 1,
            _ => return (matched, matched_count),
        }
    }
    (matched, matched_count)
}

fn alignment_cost(rows: &[(usize, Vec<AlignmentCell>)], row: usize, column: usize) -> u16 {
    alignment_cell(rows, row, column).map_or(u16::MAX / 2, |cell| cell.cost)
}

fn alignment_cell(
    rows: &[(usize, Vec<AlignmentCell>)],
    row: usize,
    column: usize,
) -> Option<AlignmentCell> {
    let (low, cells) = rows.get(row)?;
    cells.get(column.checked_sub(*low)?).copied()
}

fn fill_unmatched_timing(matched: &mut [Option<(u32, u32)>], baseline: &[SourceToken]) {
    let anchors = matched
        .iter()
        .enumerate()
        .filter_map(|(index, timing)| timing.map(|timing| (index, timing)))
        .collect::<Vec<_>>();
    for index in 0..matched.len() {
        if matched[index].is_some() {
            continue;
        }
        let previous = anchors
            .iter()
            .rev()
            .find(|(anchor, _)| *anchor < index)
            .copied();
        let next = anchors.iter().find(|(anchor, _)| *anchor > index).copied();
        let timing = match (previous, next) {
            (Some((left_index, (_, left_end))), Some((right_index, (right_start, _)))) => {
                let span = right_index.saturating_sub(left_index) as u64;
                let position = index.saturating_sub(left_index) as u64;
                let start = left_end.saturating_add(
                    (u64::from(right_start.saturating_sub(left_end)) * position / span) as u32,
                );
                let end = start.saturating_add(baseline.get(index).map_or(1, |token| {
                    token.end_ms.saturating_sub(token.start_ms).max(1)
                }));
                (start, end.max(start + 1))
            }
            (None, Some((right_index, (right_start, _)))) => {
                let distance = right_index.saturating_sub(index) as u32;
                let duration = baseline.get(index).map_or(1, |token| {
                    token.end_ms.saturating_sub(token.start_ms).max(1)
                });
                let end = right_start.saturating_sub(distance.saturating_mul(duration));
                (end.saturating_sub(duration), end.max(1))
            }
            (Some((left_index, (_, left_end))), None) => {
                let offset = index.saturating_sub(left_index) as u32;
                let duration = baseline.get(index).map_or(1, |token| {
                    token.end_ms.saturating_sub(token.start_ms).max(1)
                });
                let start =
                    left_end.saturating_add(offset.saturating_sub(1).saturating_mul(duration));
                (start, start.saturating_add(duration.max(1)))
            }
            (None, None) => baseline.get(index).map_or((0, 1), |token| {
                (token.start_ms, token.end_ms.max(token.start_ms + 1))
            }),
        };
        matched[index] = Some(timing);
    }
    let mut previous_end = 0;
    for timing in matched.iter_mut().flatten() {
        timing.0 = timing.0.max(previous_end);
        timing.1 = timing.1.max(timing.0 + 1);
        previous_end = timing.1;
    }
}

fn distribute_span_words(text: &str, start_ms: u32, end_ms: u32) -> Vec<EvidenceWord> {
    let words = text.split_whitespace().collect::<Vec<_>>();
    if words.is_empty() {
        return Vec::new();
    }
    let total = words
        .iter()
        .map(|word| word.chars().count().max(1))
        .sum::<usize>();
    let duration = end_ms.saturating_sub(start_ms).max(words.len() as u32);
    let mut consumed = 0;
    words
        .into_iter()
        .map(|word| {
            let begin = start_ms + ((u64::from(duration) * consumed as u64) / total as u64) as u32;
            consumed += word.chars().count().max(1);
            let end = start_ms + ((u64::from(duration) * consumed as u64) / total as u64) as u32;
            EvidenceWord {
                text: word.to_owned(),
                start_ms: begin,
                end_ms: end.max(begin + 1).min(end_ms.max(begin + 1)),
            }
        })
        .collect()
}

fn match_words(words: &[&str], evidence: &[EvidenceWord], cursor: usize) -> Option<usize> {
    if words.is_empty() || cursor + words.len() > evidence.len() {
        return None;
    }
    words
        .iter()
        .enumerate()
        .all(|(position, word)| token_key(word) == token_key(&evidence[cursor + position].text))
        .then_some(cursor + words.len())
}

fn token_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_anchor_stop_word(value: &str) -> bool {
    matches!(
        value,
        "this"
            | "that"
            | "then"
            | "here"
            | "there"
            | "with"
            | "from"
            | "into"
            | "your"
            | "you"
            | "the"
            | "and"
            | "nhấn"
            | "bấm"
            | "ấn"
            | "press"
            | "click"
    )
}

fn action_ranges(text: &str) -> Vec<(usize, usize)> {
    let words = text.split_whitespace().collect::<Vec<_>>();
    // Keep hard anchors for visible, imperative actions.  "type" is deliberately excluded:
    // technical narration uses it mostly in phrases such as "type safety" and "type parameter",
    // while the subtitle does not prove that a visible edit is happening at that exact word.
    let indicators = [
        "click", "press", "run", "open", "close", "select", "choose", "enter", "scroll",
        // The Natural Voice input may already be translated. Keep only explicit click/press
        // imperatives here; verbs such as "chạy" and "chọn" are common explanatory prose.
        "nhấn", "bấm", "ấn",
    ];
    let mut ranges = Vec::new();
    for (index, word) in words.iter().enumerate() {
        let key = token_key(word);
        let next = words
            .get(index + 1)
            .map(|word| token_key(word))
            .unwrap_or_default();
        if matches!(
            (key.as_str(), next.as_str()),
            ("application" | "app", "starts")
                | ("result" | "output", "appears")
                | ("browser", "opens")
                | ("terminal" | "console" | "screen", "shows" | "displays")
        ) {
            ranges.push((index, index + 1));
            continue;
        }
        if !indicators.contains(&key.as_str()) {
            continue;
        }
        // A translated imperative such as "nhấn Enter" is one action phrase. Do not create a
        // second anchor for the object token after the Vietnamese verb.
        if index > 0 && is_vietnamese_action_verb(&token_key(words[index - 1])) {
            continue;
        }
        // Trailing comma marks a participle ("are open, the shadows"), not an imperative.
        if word.contains(',') {
            continue;
        }
        // Capability/preposition prose ("I can open", "on Open Router") is not a click-along cue.
        if index > 0 && is_descriptive_action_context(words[index - 1]) {
            continue;
        }
        // These are common explanatory constructions, not visible user actions. Requiring an
        // object-like continuation keeps "run it" and similar prose from becoming hard anchors.
        if matches!(
            (key.as_str(), next.as_str()),
            ("type", "of")
                | ("type", "that")
                | ("type", "and")
                | ("type", "here")
                | ("type", "we")
                | ("type", "youre")
                | ("type", "isnt")
                | ("type", "so")
                | ("run", "our")
                | ("run", "it")
                | ("open", "router")
                | ("open", "code")
                | ("open", "source")
        ) {
            continue;
        }
        let end = (index + 1).min(words.len() - 1);
        ranges.push((index, end));
    }
    ranges
}

fn is_vietnamese_action_verb(value: &str) -> bool {
    matches!(value, "nhấn" | "bấm" | "ấn")
}

fn is_descriptive_action_context(previous: &str) -> bool {
    if ends_sentence(previous) {
        return false;
    }
    matches!(
        token_key(previous).as_str(),
        "can"
            | "could"
            | "will"
            | "would"
            | "should"
            | "may"
            | "might"
            | "to"
            | "are"
            | "is"
            | "was"
            | "were"
            | "be"
            | "been"
            | "being"
            | "has"
            | "have"
            | "had"
            | "also"
            | "and"
            | "or"
            | "but"
            | "on"
            | "in"
            | "at"
            | "from"
            | "with"
            | "of"
            | "for"
            | "the"
            | "a"
            | "an"
    )
}

fn ends_sentence(text: &str) -> bool {
    text.trim_end_matches(|character: char| {
        character.is_whitespace() || "\"'”’)]}".contains(character)
    })
    .chars()
    .last()
    .is_some_and(|character| matches!(character, '.' | '!' | '?' | '…' | '。' | '！' | '？'))
}

fn timestamp_ms(value: &str) -> Result<u32> {
    let millis = (time_to_seconds(value)? * 1_000.0).round();
    if !(0.0..=f64::from(u32::MAX)).contains(&millis) {
        bail!("SRT timestamp is outside the supported range: {value}");
    }
    Ok(millis as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_fixture(name: &str, value: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("dubora-sync-{name}-{}.srt", std::process::id()));
        fs::write(&path, value).unwrap();
        path
    }

    #[test]
    fn reconstructs_overlapping_captions_into_one_monotonic_clock() {
        let source = write_fixture(
            "overlap",
            "1\n00:00:00,080 --> 00:00:07,120\nOpen the terminal and run the app\n\n2\n00:00:05,120 --> 00:00:13,120\nthe terminal and run the app now\n",
        );
        let clean = write_fixture(
            "clean",
            "1\n00:00:00,080 --> 00:00:13,120\nOpen the terminal and run the app now\n",
        );
        let timeline = SourceTimeline::from_files(&source, &clean).unwrap();
        assert_eq!(timeline.mode, TimingMode::CaptionDerived);
        assert_eq!(timeline.timing_provider.as_deref(), Some("captionDerived"));
        assert_eq!(timeline.cue_overlap_count, 1);
        assert!(
            timeline
                .tokens
                .windows(2)
                .all(|pair| pair[1].start_ms >= pair[0].start_ms)
        );
        assert!(timeline.phrases.iter().any(|phrase| phrase.required));
        let _ = fs::remove_file(source);
        let _ = fs::remove_file(clean);
    }

    #[test]
    fn rolling_evidence_uses_the_next_transition_for_word_windows() {
        let source = write_fixture(
            "transition-window",
            "1\n00:00:00,000 --> 00:00:10,000\none two three four five six\n\n2\n00:00:05,000 --> 00:00:15,000\nfour five six seven eight\n",
        );
        let clean = write_fixture(
            "transition-window-clean",
            "1\n00:00:00,000 --> 00:00:05,000\none two three four five six\n\n2\n00:00:05,000 --> 00:00:15,000\nseven eight\n",
        );
        let timeline = SourceTimeline::from_files(&source, &clean).unwrap();
        let first_segment = timeline
            .tokens
            .iter()
            .filter(|token| token.source_segment == 1)
            .collect::<Vec<_>>();
        assert_eq!(first_segment.len(), 6);
        assert!(
            first_segment
                .iter()
                .all(|token| token.end_ms - token.start_ms >= 100)
        );
        assert!(first_segment.last().unwrap().end_ms <= 5_000);
        let _ = fs::remove_file(source);
        let _ = fs::remove_file(clean);
    }

    #[test]
    fn cue_only_source_does_not_invent_word_precision() {
        let source = write_fixture(
            "cue",
            "1\n00:00:00,000 --> 00:00:02,000\nA complete sentence.\n\n2\n00:00:03,000 --> 00:00:05,000\nAnother sentence.\n",
        );
        let clean = source.clone();
        let timeline = SourceTimeline::from_files(&source, &clean).unwrap();
        assert_eq!(timeline.mode, TimingMode::CueOnly);
        assert_eq!(timeline.timing_provider.as_deref(), Some("cueOnly"));
        assert!(timeline.validate().is_ok());
        let _ = fs::remove_file(source);
    }

    #[test]
    fn projects_one_unique_cross_language_anchor_without_upgrading_the_timeline() {
        let source = write_fixture(
            "anchor-projection",
            "1\n00:00:02,000 --> 00:00:06,000\ntruy cập địa chỉ IP và nhấn Enter.\n",
        );
        let clean = source.clone();
        let mut timeline = SourceTimeline::from_files(&source, &clean).unwrap();
        let projected = timeline.project_required_anchor_times(&[GranularSourceWord {
            text: "enter".to_owned(),
            start_ms: 1_200,
            end_ms: 1_500,
        }]);
        assert_eq!(projected, 1);
        assert_eq!(timeline.mode, TimingMode::CueOnly);
        assert_eq!(timeline.projected_anchor_count, 1);
        assert_eq!(timeline.phrases[1].preferred_start_ms, 1_200);
        assert!(timeline.has_projected_anchor_timing());
        assert_eq!(timeline.projected_anchor_ids, vec!["p-1-action-1"]);
        assert!(timeline.has_semantic_anchor_timing("p-1-action-1"));
        assert!(!timeline.has_semantic_anchor_timing("p-1-action-2"));
        let _ = fs::remove_file(source);
    }

    #[test]
    fn granular_source_words_replace_caption_windows_without_rewriting_text() {
        let source = write_fixture(
            "granular-source",
            "1\n00:00:00,000 --> 00:00:04,000\nOpen the terminal and run.\n",
        );
        let words = [
            ("Open", 400, 650),
            ("the", 700, 850),
            ("terminal", 900, 1_350),
            ("and", 1_450, 1_600),
            ("run.", 1_700, 2_050),
        ]
        .into_iter()
        .map(|(text, start_ms, end_ms)| GranularSourceWord {
            text: text.to_owned(),
            start_ms,
            end_ms,
        })
        .collect::<Vec<_>>();
        let timeline = SourceTimeline::from_files_with_granular(&source, &source, &words).unwrap();
        assert_eq!(timeline.mode, TimingMode::Granular);
        assert_eq!(
            timeline.timing_provider.as_deref(),
            Some("azureFastTranscription")
        );
        assert_eq!(timeline.tokens[0].text, "Open");
        assert_eq!(timeline.tokens[0].start_ms, 400);
        assert_eq!(timeline.tokens.last().unwrap().end_ms, 2_050);
        assert!(
            timeline
                .tokens
                .iter()
                .all(|token| token.confidence == TimingConfidence::Granular)
        );
        assert!(timeline.validate().is_ok());
        let _ = fs::remove_file(source);
    }

    #[test]
    fn granular_alignment_resynchronizes_after_contractions_and_substitutions() {
        let source = ["my", "name", "is", "John", "and", "I", "teach"]
            .into_iter()
            .enumerate()
            .map(|(index, text)| SourceToken {
                id: format!("t-{index}"),
                text: text.to_owned(),
                start_ms: index as u32 * 100,
                end_ms: index as u32 * 100 + 80,
                confidence: TimingConfidence::CueOnly,
                source_segment: 1,
            })
            .collect::<Vec<_>>();
        let asr = [
            ("my", 10, 80),
            ("name's", 90, 170),
            ("John", 180, 260),
            ("and", 270, 350),
            ("I", 360, 440),
            ("teach", 450, 530),
        ]
        .into_iter()
        .map(|(text, start_ms, end_ms)| GranularSourceWord {
            text: text.to_owned(),
            start_ms,
            end_ms,
        })
        .collect::<Vec<_>>();
        let (matched, count) = align_granular_words(&source, &asr);
        assert!(count >= 5);
        assert_eq!(matched[0], Some((10, 80)));
        assert_eq!(matched[3], Some((180, 260)));
        assert_eq!(matched[6], Some((450, 530)));
    }

    #[test]
    fn allocator_uses_allocated_slots_without_overlap() {
        let source = write_fixture(
            "alloc-source",
            "1\n00:00:00,000 --> 00:00:03,000\nClick Run.\n\n2\n00:00:03,000 --> 00:00:06,000\nThe result appears.\n",
        );
        let clean = source.clone();
        let timeline = SourceTimeline::from_files(&source, &clean).unwrap();
        let actions = timeline
            .phrases
            .iter()
            .filter(|phrase| phrase.required)
            .map(|phrase| phrase.text.to_ascii_lowercase())
            .collect::<Vec<_>>();
        assert!(actions.iter().any(|phrase| phrase.starts_with("click run")));
        assert!(
            actions
                .iter()
                .any(|phrase| phrase.starts_with("result appears"))
        );
        let segments = parse_srt(&clean).unwrap();
        let layout = build_sync_layout(&timeline, &segments).unwrap();
        let durations = layout
            .blocks
            .iter()
            .map(|block| (block.id.clone(), 1_000))
            .collect::<HashMap<_, _>>();
        let allocated = allocate_blocks(&layout, &durations, NaturalMode::Balanced).unwrap();
        assert!(
            allocated
                .windows(2)
                .all(|pair| pair[1].render_start_ms >= pair[0].slot_end_ms)
        );
        let _ = fs::remove_file(source);
    }

    #[test]
    fn source_layout_does_not_change_with_translated_text() {
        let source = write_fixture(
            "source-layout",
            "1\n00:00:00,000 --> 00:00:02,000\nContinue with the\n\n2\n00:00:02,000 --> 00:00:04,000\nsame sentence.\n",
        );
        let translated = write_fixture(
            "translated-layout",
            "1\n00:00:00,000 --> 00:00:02,000\nDừng lại.\n\n2\n00:00:02,000 --> 00:00:04,000\nMột câu tiếng Việt có ranh giới và độ dài hoàn toàn khác bản nguồn.\n",
        );
        let timeline = SourceTimeline::from_files(&source, &source).unwrap();
        let source_layout = build_sync_layout(&timeline, &parse_srt(&source).unwrap()).unwrap();
        let translated_layout =
            build_sync_layout(&timeline, &parse_srt(&translated).unwrap()).unwrap();

        assert_eq!(translated_layout, source_layout);
        assert_eq!(source_layout.blocks[0].source_segments, vec![1, 2]);
        let _ = fs::remove_file(source);
        let _ = fs::remove_file(translated);
    }

    #[test]
    fn source_layout_rejects_reordered_translated_indices() {
        let source = write_fixture(
            "source-layout-order",
            "1\n00:00:00,000 --> 00:00:02,000\nFirst part\n\n2\n00:00:02,000 --> 00:00:04,000\nsecond part.\n",
        );
        let timeline = SourceTimeline::from_files(&source, &source).unwrap();
        let layout = build_sync_layout(&timeline, &parse_srt(&source).unwrap()).unwrap();

        assert!(layout.validate(&[1, 2]).is_ok());
        assert!(layout.validate(&[2, 1]).is_err());
        let _ = fs::remove_file(source);
    }

    #[test]
    fn cap_only_group_keeps_natural_drift_bounded() {
        let specs = vec![
            AllocationSpec {
                id: "block-1".to_owned(),
                preferred_start_ms: 0,
                predicted_audio_ms: 1_800,
                anchor_count: 0,
                measured_anchors: Vec::new(),
            },
            AllocationSpec {
                id: "block-2".to_owned(),
                preferred_start_ms: 1_000,
                predicted_audio_ms: 1_800,
                anchor_count: 0,
                measured_anchors: Vec::new(),
            },
        ];
        let island = SyncIsland {
            id: "island-cap".to_owned(),
            start_ms: 0,
            end_ms: 4_000,
            block_ids: specs.iter().map(|spec| spec.id.clone()).collect(),
            hard_boundary_before: false,
        };
        assert!(
            allocate_island_with_move(&specs, &island, NaturalMode::Natural, Some(60_000),)
                .is_err()
        );
    }

    #[test]
    fn allocator_only_compresses_the_block_that_needs_it() {
        let specs = vec![
            AllocationSpec {
                id: "short".to_owned(),
                preferred_start_ms: 0,
                predicted_audio_ms: 500,
                anchor_count: 0,
                measured_anchors: Vec::new(),
            },
            AllocationSpec {
                id: "anchored-long".to_owned(),
                preferred_start_ms: 1_000,
                predicted_audio_ms: 4_500,
                anchor_count: 1,
                measured_anchors: Vec::new(),
            },
        ];
        let island = SyncIsland {
            id: "island-1".to_owned(),
            start_ms: 0,
            end_ms: 5_000,
            block_ids: specs.iter().map(|spec| spec.id.clone()).collect(),
            hard_boundary_before: false,
        };

        let windows =
            allocate_island_with_move(&specs, &island, NaturalMode::Balanced, None).unwrap();

        assert_eq!(windows[0].rendered_audio_ms, 500);
        assert_eq!(windows[0].tempo, 1.0);
        assert!(windows[1].tempo > 1.0);
        assert!(windows[1].tempo <= MAX_TIMING_TEMPO + 0.001);
    }

    #[test]
    fn multiple_measured_anchors_constrain_the_same_block() {
        let mut spec = AllocationSpec {
            id: "anchored".to_owned(),
            preferred_start_ms: 0,
            predicted_audio_ms: 3_000,
            anchor_count: 2,
            measured_anchors: vec![
                MeasuredAnchor {
                    source_ms: 1_000,
                    raw_offset_ms: 500,
                    tolerance_ms: 250,
                    confidence: TimingConfidence::Granular,
                    coupling: Coupling::Anchored,
                },
                MeasuredAnchor {
                    source_ms: 2_500,
                    raw_offset_ms: 2_000,
                    tolerance_ms: 250,
                    confidence: TimingConfidence::Granular,
                    coupling: Coupling::Anchored,
                },
            ],
        };
        let island = SyncIsland {
            id: "island-1".to_owned(),
            start_ms: 0,
            end_ms: 4_000,
            block_ids: vec![spec.id.clone()],
            hard_boundary_before: false,
        };

        let start = bounded_isotonic_starts(&[spec.clone()], &[3_000], &island, 500).unwrap()[0];
        assert!(start.abs_diff(500) <= 3);

        spec.measured_anchors[1].source_ms = 3_100;
        assert!(bounded_isotonic_starts(&[spec], &[3_000], &island, 500).is_err());
    }

    #[test]
    fn soft_display_anchor_is_objective_only_and_respects_mode_movement() {
        let spec = AllocationSpec {
            id: "soft".to_owned(),
            preferred_start_ms: 0,
            predicted_audio_ms: 1_000,
            anchor_count: 0,
            measured_anchors: vec![MeasuredAnchor {
                source_ms: 1_000,
                raw_offset_ms: 0,
                tolerance_ms: MAX_SOFT_BOUNDARY_DRIFT_MS as u32,
                confidence: TimingConfidence::Granular,
                coupling: Coupling::Soft,
            }],
        };
        let island = SyncIsland {
            id: "island-soft".to_owned(),
            start_ms: 0,
            end_ms: 3_000,
            block_ids: vec![spec.id.clone()],
            hard_boundary_before: false,
        };
        assert_eq!(
            bounded_isotonic_starts(&[spec], &[1_000], &island, 0).unwrap(),
            vec![0]
        );
    }

    #[test]
    fn measured_island_reallocation_uses_actual_durations_not_preflight_targets() {
        let source = write_fixture(
            "measured-reallocation",
            "1\n00:00:00,000 --> 00:00:03,000\nExplain the setup\n\n2\n00:00:03,000 --> 00:00:06,000\nClick Run.\n",
        );
        let clean = source.clone();
        let timeline = SourceTimeline::from_files(&source, &clean).unwrap();
        let segments = parse_srt(&clean).unwrap();
        let layout = build_sync_layout(&timeline, &segments).unwrap();
        assert_eq!(layout.islands.len(), 1);
        let preflight = layout
            .blocks
            .iter()
            .map(|block| (block.id.clone(), 1_000))
            .collect::<HashMap<_, _>>();
        let initial = allocate_blocks(&layout, &preflight, NaturalMode::Natural).unwrap();
        let measured = layout
            .blocks
            .iter()
            .map(|block| (block.id.clone(), 1_500))
            .collect::<HashMap<_, _>>();
        let actual = allocate_measured_island(
            &layout,
            &layout.islands[0].id,
            &measured,
            NaturalMode::Natural,
        )
        .unwrap();
        assert_ne!(
            initial[0].slot_end_ms, actual[0].slot_end_ms,
            "the second pass must not reuse the preflight duration"
        );
        assert_eq!(actual[0].predicted_audio_ms, 1_500);
        assert!(actual[1].render_start_ms >= actual[0].slot_end_ms);
        let _ = fs::remove_file(source);
    }

    #[test]
    fn sentence_boundary_starts_a_fresh_block_window() {
        let source = write_fixture(
            "fresh-block-window",
            "1\n00:00:00,000 --> 00:00:02,000\nThe first sentence ends here.\n\n2\n00:00:02,100 --> 00:00:04,000\nThe second sentence starts here.\n",
        );
        let timeline = SourceTimeline::from_files(&source, &source).unwrap();
        let segments = parse_srt(&source).unwrap();
        let layout = build_sync_layout(&timeline, &segments).unwrap();
        assert_eq!(layout.blocks.len(), 2);
        assert_eq!(layout.blocks[0].source_segments, vec![1]);
        assert_eq!(layout.blocks[1].source_segments, vec![2]);
        assert!(layout.blocks[1].start_ms >= layout.blocks[0].end_ms);
        assert_eq!(layout.blocks[1].end_ms, 4_000);
        let _ = fs::remove_file(source);
    }

    #[test]
    fn rolling_sentence_is_not_split_by_a_short_block_limit() {
        let source = write_fixture(
            "rolling-sentence-block",
            "1\n00:00:00,080 --> 00:00:07,120\nin this video we're going to talk all about generics in java generics can be super confusing the first time you see\n\n2\n00:00:05,120 --> 00:00:13,120\nthem with all the t's and question marks and angle brackets and k's and v's but i promise by the end of this video you'll\n",
        );
        let timeline = SourceTimeline::from_files(&source, &source).unwrap();
        let segments = parse_srt(&source).unwrap();
        let layout = build_sync_layout(&timeline, &segments).unwrap();
        assert_eq!(layout.blocks.len(), 1);
        assert_eq!(layout.blocks[0].source_segments, vec![1, 2]);
        assert_eq!(layout.blocks[0].start_ms, 80);
        assert_eq!(layout.blocks[0].end_ms, 13_120);
        let _ = fs::remove_file(source);
    }

    #[test]
    fn confirmed_pause_at_next_block_start_creates_hard_island_boundary() {
        let source = write_fixture(
            "hard-pause-boundary",
            "1\n00:00:00,000 --> 00:00:01,000\nThe first sentence ends here.\n\n2\n00:00:01,800 --> 00:00:03,000\nThe next sentence starts here.\n",
        );
        let timeline = SourceTimeline::from_files(&source, &source).unwrap();
        assert!(
            timeline
                .barriers
                .iter()
                .any(|barrier| barrier.time_ms == 1_800)
        );
        let segments = parse_srt(&source).unwrap();
        let layout = build_sync_layout(&timeline, &segments).unwrap();
        assert_eq!(layout.islands.len(), 2);
        assert!(layout.islands[1].hard_boundary_before);
        let _ = fs::remove_file(source);
    }

    #[test]
    fn action_candidates_ignore_generic_nouns_and_tiny_rolling_spans() {
        let source = write_fixture(
            "action-candidates",
            "1\n00:00:00,000 --> 00:00:04,000\nThis line shows the output.\n\n2\n00:00:04,000 --> 00:00:08,000\nOpen the terminal and run the app.\n",
        );
        let timeline = SourceTimeline::from_files(&source, &source).unwrap();
        let actions = timeline
            .phrases
            .iter()
            .filter(|phrase| phrase.required)
            .map(|phrase| phrase.text.to_ascii_lowercase())
            .collect::<Vec<_>>();
        assert!(actions.iter().all(|phrase| !phrase.starts_with("this")));
        assert!(actions.iter().all(|phrase| !phrase.starts_with("type")));
        assert!(actions.iter().any(|phrase| phrase.starts_with("open")));
        let segments = parse_srt(&source).unwrap();
        let layout = build_sync_layout(&timeline, &segments).unwrap();
        assert_eq!(layout.blocks[0].source_segments, vec![1]);
        assert_eq!(layout.blocks[1].source_segments, vec![2]);
        let _ = fs::remove_file(source);
    }

    #[test]
    fn action_candidates_ignore_descriptive_looks_and_named_products() {
        let source = write_fixture(
            "action-descriptive",
            "1\n00:00:00,000 --> 00:00:04,000\nOpen Router and Open Code as a stealth model\n\n2\n00:00:04,000 --> 00:00:08,000\nI can open each cap and enter my name\n\n3\n00:00:08,000 --> 00:00:12,000\nThe pills look pretty good, too.\n\n4\n00:00:12,000 --> 00:00:16,000\nOpen the terminal and run the app.\n",
        );
        let timeline = SourceTimeline::from_files(&source, &source).unwrap();
        let actions = timeline
            .phrases
            .iter()
            .filter(|phrase| phrase.required)
            .map(|phrase| phrase.text.to_ascii_lowercase())
            .collect::<Vec<_>>();
        assert!(
            actions.iter().all(|phrase| !phrase.contains("router")),
            "{actions:?}"
        );
        assert!(
            actions.iter().all(|phrase| !phrase.contains("look")),
            "{actions:?}"
        );
        assert!(
            actions.iter().all(|phrase| !phrase.contains("each")),
            "{actions:?}"
        );
        assert!(
            actions.iter().all(|phrase| !phrase.contains("enter")),
            "{actions:?}"
        );
        assert!(
            actions.iter().any(|phrase| phrase.starts_with("open")),
            "{actions:?}"
        );
        let _ = fs::remove_file(source);
    }

    #[test]
    fn caps_island_length_when_caption_source_has_no_pause_barriers() {
        let mut value = String::new();
        for index in 0..8 {
            let start = index * 10;
            let end = start + 10;
            let start_minute = start / 60;
            let start_second = start % 60;
            let end_minute = end / 60;
            let end_second = end % 60;
            value.push_str(&format!(
                "{index}\n00:{start_minute:02}:{start_second:02},000 --> 00:{end_minute:02}:{end_second:02},000\ncontinue explaining step {index}\n\n"
            ));
        }
        let source = write_fixture("island-cap", &value);
        let timeline = SourceTimeline::from_files(&source, &source).unwrap();
        let segments = parse_srt(&source).unwrap();
        let layout = build_sync_layout(&timeline, &segments).unwrap();
        assert!(layout.islands.len() >= 2);
        assert!(
            layout
                .islands
                .iter()
                .all(|island| island.end_ms.saturating_sub(island.start_ms)
                    <= MAX_SYNC_ISLAND_DURATION_MS)
        );
        let _ = fs::remove_file(source);
    }

    #[test]
    fn rejects_reversed_source_timing_before_allocation() {
        let source = write_fixture(
            "reversed",
            "1\n00:00:02,000 --> 00:00:01,000\nThis is invalid.\n",
        );
        let error = SourceTimeline::from_files(&source, &source).unwrap_err();
        assert!(error.to_string().contains("reversed timing"));
        let _ = fs::remove_file(source);
    }

    fn weighted_start_cost(specs: &[AllocationSpec], starts: &[u32]) -> f64 {
        specs
            .iter()
            .zip(starts)
            .map(|(spec, start)| {
                let weight = 1.0 + 100.0 * spec.anchor_count as f64;
                weight * (f64::from(*start) - f64::from(spec.preferred_start_ms)).powi(2)
            })
            .sum()
    }

    fn brute_force_best(
        specs: &[AllocationSpec],
        durations: &[u32],
        island: &SyncIsland,
        max_move: u32,
    ) -> (f64, Vec<u32>) {
        fn visit(
            position: usize,
            specs: &[AllocationSpec],
            durations: &[u32],
            island: &SyncIsland,
            max_move: u32,
            starts: &mut Vec<u32>,
            best: &mut Option<(f64, Vec<u32>)>,
        ) {
            if position == specs.len() {
                let cost = weighted_start_cost(specs, starts);
                if best.as_ref().is_none_or(|(current, _)| cost < *current) {
                    *best = Some((cost, starts.clone()));
                }
                return;
            }
            let spec = &specs[position];
            let lower = island
                .start_ms
                .max(spec.preferred_start_ms.saturating_sub(max_move));
            let upper = island
                .end_ms
                .saturating_sub(durations[position])
                .min(spec.preferred_start_ms.saturating_add(max_move));
            for start in lower..=upper {
                if starts.last().is_some_and(|previous| {
                    start < previous.saturating_add(durations[position - 1])
                }) {
                    continue;
                }
                starts.push(start);
                visit(
                    position + 1,
                    specs,
                    durations,
                    island,
                    max_move,
                    starts,
                    best,
                );
                starts.pop();
            }
        }

        let mut best = None;
        visit(
            0,
            specs,
            durations,
            island,
            max_move,
            &mut Vec::new(),
            &mut best,
        );
        best.expect("fixture has at least one feasible schedule")
    }

    #[test]
    fn bounded_allocator_is_deterministic_and_matches_small_integer_oracle() {
        let specs = vec![
            AllocationSpec {
                id: "a".to_owned(),
                preferred_start_ms: 0,
                predicted_audio_ms: 3,
                anchor_count: 0,
                measured_anchors: Vec::new(),
            },
            AllocationSpec {
                id: "b".to_owned(),
                preferred_start_ms: 4,
                predicted_audio_ms: 2,
                anchor_count: 1,
                measured_anchors: Vec::new(),
            },
            AllocationSpec {
                id: "c".to_owned(),
                preferred_start_ms: 6,
                predicted_audio_ms: 2,
                anchor_count: 0,
                measured_anchors: Vec::new(),
            },
        ];
        let durations = [3, 2, 2];
        let island = SyncIsland {
            id: "island-1".to_owned(),
            start_ms: 0,
            end_ms: 10,
            block_ids: specs.iter().map(|spec| spec.id.clone()).collect(),
            hard_boundary_before: false,
        };
        let first = bounded_isotonic_starts(&specs, &durations, &island, 2).unwrap();
        let second = bounded_isotonic_starts(&specs, &durations, &island, 2).unwrap();
        let (oracle_cost, _) = brute_force_best(&specs, &durations, &island, 2);
        assert_eq!(first, second);
        assert!(
            first
                .windows(2)
                .enumerate()
                .all(|(position, pair)| pair[1] >= pair[0] + durations[position])
        );
        assert!(weighted_start_cost(&specs, &first) <= oracle_cost + 1.0);
    }

    #[test]
    fn layout_validation_rejects_missing_island_block_coverage() {
        let layout = SyncLayout {
            blocks: vec![NarrationBlock {
                id: "block-1".to_owned(),
                start_ms: 0,
                end_ms: 100,
                preferred_start_ms: 0,
                source_segments: vec![1],
                anchor_ids: Vec::new(),
                island_id: "island-1".to_owned(),
            }],
            islands: vec![SyncIsland {
                id: "island-1".to_owned(),
                start_ms: 0,
                end_ms: 100,
                block_ids: Vec::new(),
                hard_boundary_before: false,
            }],
        };
        assert!(layout.validate(&[1]).is_err());
    }

    #[test]
    fn best_effort_allocator_isolates_one_infeasible_island() {
        let layout = SyncLayout {
            blocks: vec![NarrationBlock {
                id: "block-1".to_owned(),
                start_ms: 0,
                end_ms: 100,
                preferred_start_ms: 0,
                source_segments: vec![1],
                anchor_ids: Vec::new(),
                island_id: "island-1".to_owned(),
            }],
            islands: vec![SyncIsland {
                id: "island-1".to_owned(),
                start_ms: 0,
                end_ms: 100,
                block_ids: vec!["block-1".to_owned()],
                hard_boundary_before: false,
            }],
        };
        let mut predicted = HashMap::new();
        predicted.insert("block-1".to_owned(), 2_000);
        let (allocated, failed) =
            allocate_blocks_best_effort(&layout, &predicted, NaturalMode::Balanced).unwrap();
        assert_eq!(failed, vec!["island-1"]);
        assert_eq!(allocated[0].slot_end_ms, 100);
        assert!(allocate_blocks(&layout, &predicted, NaturalMode::Balanced).is_err());
    }

    fn spillback_layout() -> (
        SyncLayout,
        HashMap<String, AllocatedBlock>,
        HashMap<String, u32>,
    ) {
        let layout = SyncLayout {
            blocks: vec![
                NarrationBlock {
                    id: "block-15".to_owned(),
                    start_ms: 117_051,
                    end_ms: 124_000,
                    preferred_start_ms: 117_051,
                    source_segments: vec![25],
                    anchor_ids: Vec::new(),
                    island_id: "island-3".to_owned(),
                },
                NarrationBlock {
                    id: "block-16".to_owned(),
                    start_ms: 124_000,
                    end_ms: 128_879,
                    preferred_start_ms: 124_000,
                    source_segments: vec![26],
                    anchor_ids: vec!["p-26-action-1".to_owned()],
                    island_id: "island-3".to_owned(),
                },
            ],
            islands: vec![SyncIsland {
                id: "island-3".to_owned(),
                start_ms: 113_522,
                end_ms: 168_879,
                block_ids: vec!["block-15".to_owned(), "block-16".to_owned()],
                hard_boundary_before: false,
            }],
        };
        let mut allocations = HashMap::new();
        allocations.insert(
            "block-15".to_owned(),
            AllocatedBlock {
                block_id: "block-15".to_owned(),
                island_id: "island-3".to_owned(),
                render_start_ms: 117_051,
                slot_end_ms: 124_000,
                predicted_audio_ms: 1_416,
                rendered_audio_ms: 1_416,
                tempo: 1.0,
            },
        );
        allocations.insert(
            "block-16".to_owned(),
            AllocatedBlock {
                block_id: "block-16".to_owned(),
                island_id: "island-3".to_owned(),
                render_start_ms: 124_000,
                slot_end_ms: 128_879,
                predicted_audio_ms: 6_696,
                rendered_audio_ms: 4_879,
                tempo: 1.372_412_379_585_980_8,
            },
        );
        let mut measured = HashMap::new();
        measured.insert("block-15".to_owned(), 1_416);
        measured.insert("block-16".to_owned(), 6_696);
        (layout, allocations, measured)
    }

    #[test]
    fn measured_spillback_uses_the_previous_unused_tail_on_the_caddy_ip_cue() {
        let (layout, mut allocations, measured) = spillback_layout();
        apply_measured_spillback(&mut allocations, &measured, &layout);
        let previous = &allocations["block-15"];
        let current = &allocations["block-16"];
        assert_eq!(current.render_start_ms, 122_183);
        assert_eq!(current.slot_end_ms, 128_879);
        assert!(current.tempo <= MAX_TIMING_TEMPO + f64::EPSILON);
        assert!((current.tempo - 1.0).abs() < 0.001);
        assert_eq!(previous.slot_end_ms, current.render_start_ms);
        assert!(previous.tempo <= 1.0 + f64::EPSILON);
        assert!(current.render_start_ms >= 117_051 + 1_416);
        assert!(!group_exceeds_tempo_cap(
            &allocation_groups(&layout)[0],
            &allocations,
            &measured
        ));
    }

    #[test]
    fn measured_spillback_does_not_move_a_block_that_already_fits() {
        let (layout, mut allocations, mut measured) = spillback_layout();
        measured.insert("block-16".to_owned(), 4_000);
        allocations.get_mut("block-16").unwrap().tempo = 1.0;
        allocations.get_mut("block-16").unwrap().rendered_audio_ms = 4_000;
        allocations.get_mut("block-16").unwrap().predicted_audio_ms = 4_000;
        apply_measured_spillback(&mut allocations, &measured, &layout);
        assert_eq!(allocations["block-16"].render_start_ms, 124_000);
    }

    #[test]
    fn measured_spillback_does_not_cross_an_island_boundary() {
        let (mut layout, mut allocations, measured) = spillback_layout();
        layout.blocks[1].island_id = "island-4".to_owned();
        allocations.get_mut("block-16").unwrap().island_id = "island-4".to_owned();
        layout.islands.push(SyncIsland {
            id: "island-4".to_owned(),
            start_ms: 124_000,
            end_ms: 128_879,
            block_ids: vec!["block-16".to_owned()],
            hard_boundary_before: true,
        });
        layout.islands[0].block_ids = vec!["block-15".to_owned()];
        layout.islands[0].end_ms = 124_000;
        apply_measured_spillback(&mut allocations, &measured, &layout);
        assert_eq!(allocations["block-16"].render_start_ms, 124_000);
    }

    #[test]
    fn measured_spillback_skips_a_hard_anchored_current_block() {
        let (layout, mut allocations, measured) = spillback_layout();
        let skip = HashSet::from(["block-16".to_owned()]);
        apply_measured_spillback_except(&mut allocations, &measured, &layout, &skip);
        assert_eq!(allocations["block-16"].render_start_ms, 124_000);
        assert!((allocations["block-16"].tempo - 1.372).abs() < 0.001);
    }

    #[test]
    fn cover_hard_anchor_windows_restores_a_stolen_enter_tail() {
        let (mut layout, mut allocations, mut measured) = spillback_layout();
        layout.blocks.push(NarrationBlock {
            id: "block-17".to_owned(),
            start_ms: 128_879,
            end_ms: 134_643,
            preferred_start_ms: 128_879,
            source_segments: vec![27],
            anchor_ids: Vec::new(),
            island_id: "island-3".to_owned(),
        });
        layout.islands[0].block_ids.push("block-17".to_owned());
        allocations.insert(
            "block-17".to_owned(),
            AllocatedBlock {
                block_id: "block-17".to_owned(),
                island_id: "island-3".to_owned(),
                render_start_ms: 126_960,
                slot_end_ms: 134_643,
                predicted_audio_ms: 6_000,
                rendered_audio_ms: 6_000,
                tempo: 1.0,
            },
        );
        measured.insert("block-17".to_owned(), 6_000);
        allocations.get_mut("block-16").unwrap().render_start_ms = 121_920;
        allocations.get_mut("block-16").unwrap().slot_end_ms = 126_960;
        refresh_allocated_block(allocations.get_mut("block-16").unwrap(), 5_040);
        let mut anchors = HashMap::new();
        anchors.insert(
            "block-16".to_owned(),
            vec![MeasuredAnchor {
                source_ms: 127_818,
                raw_offset_ms: 4_800,
                tolerance_ms: 250,
                confidence: TimingConfidence::CaptionDerived,
                coupling: Coupling::Anchored,
            }],
        );
        cover_hard_anchor_windows(&mut allocations, &measured, &layout, &anchors);
        assert!(allocations["block-16"].slot_end_ms >= 127_818);
        assert!(allocations["block-17"].render_start_ms >= 127_818);
        assert!(allocations["block-16"].slot_end_ms <= allocations["block-17"].render_start_ms);
    }
}
