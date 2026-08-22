use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::azure_timing::{TimingEvent, TimingEventKind};
use crate::speech_plan::SpeechPlan;
use crate::srt::{SrtSegment, time_to_seconds};
#[cfg(test)]
use crate::sync_timeline::build_sync_layout;
use crate::sync_timeline::{
    Coupling, MAX_ANCHOR_DRIFT_MS, SOURCE_TIMELINE_VERSION, SourceTimeline, SyncLayout,
};
use crate::tts_cache::{read_tts_cache_key, tts_cache_key};

const MAX_PARTS: usize = 4;
const MAX_DURATION_MS: u32 = 8_000;
const MAX_WORDS: usize = 36;
const MAX_JOIN_GAP_MS: i64 = 120;
pub const FIRST_WORD_TOLERANCE_MS: i64 = 120;
pub const INTERNAL_ANCHOR_TOLERANCE_MS: i64 = 180;
pub const FINAL_ANCHOR_TOLERANCE_MS: i64 = 150;
const AUDIO_TAIL_TOLERANCE_MS: u32 = 1_200;
pub(crate) const RENDER_MANIFEST_VERSION: u8 = 2;

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
    #[serde(default)]
    pub block_id: String,
    #[serde(default)]
    pub island_id: String,
    #[serde(default)]
    pub anchor_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledAudio {
    pub id: String,
    pub path: PathBuf,
    pub start_ms: u32,
    pub end_ms: u32,
    pub source_segments: Vec<usize>,
    #[serde(default)]
    pub measured_audio_ms: u32,
    #[serde(default)]
    pub rendered_audio_ms: u32,
    #[serde(default = "default_tempo")]
    pub tempo: f64,
    #[serde(default)]
    pub island_id: Option<String>,
    pub cache_identity: String,
    #[serde(default)]
    pub alignment: EntryAlignment,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EntryAlignmentStatus {
    Aligned,
    #[default]
    NotApplicable,
    ReviewRequired,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryAlignment {
    #[serde(default)]
    pub status: EntryAlignmentStatus,
    #[serde(default)]
    pub anchors: Vec<RenderedAnchor>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedAnchor {
    pub id: String,
    pub bookmark: String,
    pub source_ms: u32,
    pub raw_offset_ms: u32,
    pub rendered_ms: u32,
    pub drift_ms: i64,
    pub tolerance_ms: i64,
    pub coupling: Coupling,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestIsland {
    pub id: String,
    pub start_ms: u32,
    pub end_ms: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderManifest {
    pub version: u8,
    pub source_timeline_version: u8,
    pub source_timeline_identity: String,
    pub islands: Vec<ManifestIsland>,
    pub semantic_alignment_supported: bool,
    #[serde(default)]
    pub status: ManifestStatus,
    pub entries: Vec<ScheduledAudio>,
    #[serde(default)]
    pub diagnostics: ManifestDiagnostics,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestDiagnostics {
    #[serde(default)]
    pub anchor_drifts_ms: Vec<i64>,
    #[serde(default)]
    pub prediction_errors_ms: Vec<i64>,
    #[serde(default)]
    pub review_reasons: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ManifestStatus {
    #[default]
    Aligned,
    ReviewRequired,
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
            version: RENDER_MANIFEST_VERSION,
            source_timeline_version: 0,
            source_timeline_identity: String::new(),
            islands: Vec::new(),
            semantic_alignment_supported: false,
            status: ManifestStatus::Aligned,
            entries,
            diagnostics: ManifestDiagnostics::default(),
        }
    }

    pub fn with_diagnostics(mut self, diagnostics: ManifestDiagnostics) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    pub fn with_semantic_alignment_support(mut self, supported: bool) -> Self {
        self.semantic_alignment_supported = supported;
        self
    }

    pub fn with_source_timeline(
        mut self,
        timeline: &SourceTimeline,
        layout: &SyncLayout,
    ) -> Result<Self> {
        self.source_timeline_version = timeline.version;
        self.source_timeline_identity = source_timeline_identity(timeline)?;
        self.islands = layout
            .islands
            .iter()
            .map(|island| ManifestIsland {
                id: island.id.clone(),
                start_ms: island.start_ms,
                end_ms: island.end_ms,
            })
            .collect();
        Ok(self)
    }

    pub fn validate_source_timeline(
        &self,
        timeline: &SourceTimeline,
        layout: &SyncLayout,
    ) -> Result<()> {
        if self.source_timeline_version != timeline.version
            || self.source_timeline_identity != source_timeline_identity(timeline)?
        {
            bail!("Audio render manifest source timeline identity is stale");
        }
        let expected_islands = layout
            .islands
            .iter()
            .map(|island| ManifestIsland {
                id: island.id.clone(),
                start_ms: island.start_ms,
                end_ms: island.end_ms,
            })
            .collect::<Vec<_>>();
        if self.islands != expected_islands {
            bail!("Audio render manifest sync islands do not match the source layout");
        }
        for entry in &self.entries {
            let block = layout
                .blocks
                .iter()
                .find(|block| block.source_segments == entry.source_segments)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Audio render manifest entry {} does not belong to a source block",
                        entry.id
                    )
                })?;
            if entry.island_id.as_deref() != Some(block.island_id.as_str()) {
                bail!(
                    "Audio render manifest entry {} has a stale sync island",
                    entry.id
                );
            }
            let expected_anchors = entry
                .source_segments
                .iter()
                .flat_map(|index| timeline.required_phrases_for_segment(*index))
                .filter(|phrase| timeline.has_semantic_anchor_timing(&phrase.id))
                .collect::<Vec<_>>();
            if !self.semantic_alignment_supported {
                if entry.alignment != EntryAlignment::default() {
                    bail!("Audio render manifest claims semantic alignment without timing support");
                }
                continue;
            }
            if expected_anchors.is_empty() {
                if entry.alignment.status != EntryAlignmentStatus::NotApplicable
                    || !entry.alignment.anchors.is_empty()
                {
                    bail!(
                        "Audio render manifest entry {} invents semantic anchors",
                        entry.id
                    );
                }
                continue;
            }
            if self.status == ManifestStatus::ReviewRequired
                && entry.alignment.status == EntryAlignmentStatus::ReviewRequired
            {
                for actual in &entry.alignment.anchors {
                    let expected = expected_anchors
                        .iter()
                        .find(|expected| expected.id == actual.id)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "Audio render manifest entry {} invents a review anchor",
                                entry.id
                            )
                        })?;
                    if actual.source_ms != expected.preferred_start_ms
                        || actual.tolerance_ms != MAX_ANCHOR_DRIFT_MS
                        || actual.raw_offset_ms > entry.measured_audio_ms
                    {
                        bail!(
                            "Audio render manifest entry {} has stale review evidence",
                            entry.id
                        );
                    }
                }
                continue;
            }
            if entry.alignment.status != EntryAlignmentStatus::Aligned
                || entry.alignment.anchors.len() != expected_anchors.len()
            {
                bail!(
                    "Audio render manifest entry {} omits required semantic anchors",
                    entry.id
                );
            }
            let mut previous_raw_offset = 0;
            for (actual, expected) in entry.alignment.anchors.iter().zip(expected_anchors) {
                if actual.id != expected.id
                    || actual.source_ms != expected.preferred_start_ms
                    || actual.tolerance_ms != MAX_ANCHOR_DRIFT_MS
                    || actual.raw_offset_ms < previous_raw_offset
                    || actual.raw_offset_ms > entry.measured_audio_ms
                {
                    bail!(
                        "Audio render manifest entry {} has stale semantic evidence",
                        entry.id
                    );
                }
                previous_raw_offset = actual.raw_offset_ms;
            }
        }
        Ok(())
    }

    pub fn with_audio_cache_identities(mut self) -> Result<Self> {
        for entry in &mut self.entries {
            entry.cache_identity = read_tts_cache_key(&entry.path)?;
        }
        Ok(self)
    }

    pub fn review_required(mut self) -> Self {
        self.status = ManifestStatus::ReviewRequired;
        self
    }

    pub fn validate(&self, expected_indices: &[usize]) -> Result<()> {
        if self.version != RENDER_MANIFEST_VERSION {
            bail!("Unsupported audio render manifest version {}", self.version);
        }
        if self.source_timeline_version != SOURCE_TIMELINE_VERSION
            || self.source_timeline_identity.trim().is_empty()
            || self.islands.is_empty()
        {
            bail!("Audio render manifest is not bound to the current source timeline");
        }
        let mut island_ids = HashSet::new();
        if self.islands.iter().any(|island| {
            !island_ids.insert(island.id.as_str())
                || island.id.trim().is_empty()
                || island.start_ms >= island.end_ms
        }) {
            bail!("Audio render manifest contains invalid sync islands");
        }
        let island_bounds = self
            .islands
            .iter()
            .map(|island| (island.id.as_str(), (island.start_ms, island.end_ms)))
            .collect::<HashMap<_, _>>();
        if self
            .islands
            .windows(2)
            .any(|pair| pair[1].start_ms < pair[0].end_ms)
        {
            bail!("Audio render manifest sync islands overlap");
        }
        let enforce_aligned_constraints = self.status == ManifestStatus::Aligned;
        let covered = self
            .entries
            .iter()
            .flat_map(|entry| entry.source_segments.iter().copied())
            .collect::<Vec<_>>();
        if covered != expected_indices {
            bail!("Audio render manifest does not cover each subtitle segment exactly once");
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.start_ms >= entry.end_ms || entry.source_segments.is_empty())
        {
            bail!("Audio render manifest contains an invalid entry");
        }
        let mut entry_ids = std::collections::HashSet::new();
        if self
            .entries
            .iter()
            .any(|entry| !entry_ids.insert(&entry.id) || entry.id.trim().is_empty())
        {
            bail!("Audio render manifest contains duplicate or empty entry ids");
        }
        for entry in &self.entries {
            let island_id = entry
                .island_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("Audio render manifest contains an entry without a sync island")
                })?;
            let (island_start, island_end) = island_bounds.get(island_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "Audio render manifest entry {} references an unknown sync island",
                    entry.id
                )
            })?;
            if entry.start_ms < *island_start || entry.end_ms > *island_end {
                bail!(
                    "Audio render manifest entry {} crosses its sync island boundary",
                    entry.id
                );
            }
            if entry.cache_identity.trim().is_empty() {
                bail!(
                    "Audio render manifest entry {} has no cache identity",
                    entry.id
                );
            }
            let mut anchor_ids = HashSet::new();
            for anchor in &entry.alignment.anchors {
                if anchor.id.trim().is_empty() || !anchor_ids.insert(anchor.id.as_str()) {
                    bail!(
                        "Audio render manifest entry {} has invalid anchor ids",
                        entry.id
                    );
                }
                let rendered = u32::try_from(u64::from(entry.start_ms).saturating_add(
                    (f64::from(anchor.raw_offset_ms) / entry.tempo.max(1.0)).round() as u64,
                ))
                .unwrap_or(u32::MAX);
                if rendered.abs_diff(anchor.rendered_ms) > 1
                    || i64::from(anchor.rendered_ms) - i64::from(anchor.source_ms)
                        != anchor.drift_ms
                {
                    bail!(
                        "Audio render manifest entry {} has stale anchor timing",
                        entry.id
                    );
                }
                if enforce_aligned_constraints && anchor.drift_ms.abs() > anchor.tolerance_ms {
                    bail!(
                        "Audio render manifest entry {} has an anchor outside tolerance",
                        entry.id
                    );
                }
            }
            if (entry.alignment.status == EntryAlignmentStatus::Aligned
                && entry.alignment.anchors.is_empty())
                || (enforce_aligned_constraints
                    && entry.alignment.status == EntryAlignmentStatus::ReviewRequired)
            {
                bail!(
                    "Audio render manifest entry {} has invalid alignment status",
                    entry.id
                );
            }
        }
        if self
            .entries
            .windows(2)
            .any(|pair| pair[1].start_ms < pair[0].end_ms)
        {
            bail!("Audio render manifest entries overlap");
        }
        for entry in &self.entries {
            let expected_rendered = if entry.tempo.is_finite() && entry.tempo > 0.0 {
                (f64::from(entry.measured_audio_ms) / entry.tempo).ceil() as u32
            } else {
                0
            };
            if !entry.tempo.is_finite()
                || entry.tempo < 1.0
                // A one-millisecond ceil/rounding difference is not a real timing overrun.
                || (enforce_aligned_constraints && entry.tempo > 1.13 + f64::EPSILON)
                || entry.measured_audio_ms == 0
                || entry.rendered_audio_ms == 0
                || expected_rendered.abs_diff(entry.rendered_audio_ms) > 1
                || (enforce_aligned_constraints
                    && entry.rendered_audio_ms > entry.end_ms.saturating_sub(entry.start_ms))
            {
                bail!(
                    "Audio render manifest entry {} has invalid duration allocation (measured={} rendered={} expected={} tempo={:.3} slot={} status={:?})",
                    entry.id,
                    entry.measured_audio_ms,
                    entry.rendered_audio_ms,
                    expected_rendered,
                    entry.tempo,
                    entry.end_ms.saturating_sub(entry.start_ms),
                    self.status
                );
            }
        }
        Ok(())
    }
}

fn source_timeline_identity(timeline: &SourceTimeline) -> Result<String> {
    Ok(tts_cache_key(
        "source-timeline",
        &timeline.version.to_string(),
        "",
        &serde_json::to_string(timeline)?,
    ))
}

fn default_tempo() -> f64 {
    1.0
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
        block_id: String::new(),
        island_id: String::new(),
        anchor_ids: Vec::new(),
    }
}

/// Group subtitle segments by source-derived narration blocks. Unlike the legacy helper this
/// deliberately permits rolling-caption overlap because the source timeline has already made the
/// speech windows monotonic.
pub fn group_source_aligned_utterances(
    segments: &[SrtSegment],
    plans: &[SpeechPlan],
    timeline: &SourceTimeline,
    layout: &SyncLayout,
) -> Result<Vec<Utterance>> {
    layout.validate(
        &segments
            .iter()
            .map(|segment| segment.index)
            .collect::<Vec<_>>(),
    )?;
    let mut plans = plans
        .iter()
        .map(|plan| (plan.index, plan.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    let mut utterances = Vec::with_capacity(layout.blocks.len());
    for (position, block) in layout.blocks.iter().enumerate() {
        let mut parts = Vec::with_capacity(block.source_segments.len());
        for index in &block.source_segments {
            let segment = segments
                .iter()
                .find(|segment| segment.index == *index)
                .ok_or_else(|| anyhow::anyhow!("Missing subtitle segment {index}"))?;
            let plan = plans
                .remove(index)
                .ok_or_else(|| anyhow::anyhow!("Missing speech plan {index}"))?;
            let (start_ms, end_ms) = timeline
                .segment_window(*index)
                .unwrap_or((timestamp_ms(&segment.start)?, timestamp_ms(&segment.end)?));
            parts.push(UtterancePart {
                index: *index,
                start_ms,
                end_ms: end_ms.max(start_ms + 1),
                plan,
            });
        }
        utterances.push(Utterance {
            id: position + 1,
            start_ms: block.start_ms,
            end_ms: block.end_ms.max(block.start_ms + 1),
            parts,
            block_id: block.id.clone(),
            island_id: block.island_id.clone(),
            anchor_ids: block.anchor_ids.clone(),
        });
    }
    if !plans.is_empty() {
        bail!("Speech plans contain indices absent from the source-aligned layout");
    }
    Ok(utterances)
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

pub fn semantic_bookmark_name(utterance_id: usize, anchor_id: &str) -> String {
    let safe = anchor_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("dubora-u{utterance_id}-a{safe}")
}

/// Validate actual Azure bookmark/word offsets against the original SRT timeline.
pub fn validate_alignment(
    utterance: &Utterance,
    events: &[TimingEvent],
    audio_duration_ms: u32,
) -> AlignmentResult {
    validate_alignment_at(
        utterance,
        events,
        audio_duration_ms,
        utterance.start_ms,
        1.0,
    )
}

/// Validate only event/bookmark structure for rolling-caption timing. Caption-derived cue ends are
/// soft evidence, so their measured drift must not force a segmented fallback.
pub fn validate_bookmark_structure(
    utterance: &Utterance,
    events: &[TimingEvent],
) -> AlignmentResult {
    let words = events
        .iter()
        .filter(|event| event.kind == TimingEventKind::WordBoundary)
        .collect::<Vec<_>>();
    if words.is_empty() {
        return AlignmentResult::reject("missing_word_boundaries");
    }
    if words
        .windows(2)
        .any(|pair| pair[1].offset_ms < pair[0].offset_ms)
    {
        return AlignmentResult::reject("non_monotonic_word_boundaries");
    }
    let mut previous_offset = 0_u64;
    for part in &utterance.parts {
        let name = bookmark_name(utterance.id, part.index);
        let matches = events
            .iter()
            .filter(|event| {
                event.kind == TimingEventKind::Bookmark && event.mark.as_deref() == Some(&name)
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return AlignmentResult::reject(if matches.is_empty() {
                "bookmark_missing"
            } else {
                "bookmark_duplicated"
            });
        }
        if matches[0].offset_ms < previous_offset {
            return AlignmentResult::reject("non_monotonic_bookmarks");
        }
        previous_offset = matches[0].offset_ms;
    }
    AlignmentResult {
        accepted: true,
        worst_anchor: None,
        expected_ms: None,
        actual_ms: None,
        drift_ms: None,
        reason: Some("caption_derived_soft_boundaries".to_owned()),
    }
}

pub fn validate_alignment_at(
    utterance: &Utterance,
    events: &[TimingEvent],
    audio_duration_ms: u32,
    render_start_ms: u32,
    tempo: f64,
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
    let expected_first_ms = utterance
        .parts
        .first()
        .map(|part| {
            (f64::from(part.start_ms.saturating_sub(render_start_ms)) * tempo.max(1.0)).round()
                as i64
        })
        .unwrap_or(0);
    let first_word_ms = words[0].offset_ms as i64;
    if (first_word_ms - expected_first_ms).abs() > FIRST_WORD_TOLERANCE_MS {
        return AlignmentResult {
            accepted: false,
            worst_anchor: None,
            expected_ms: Some(expected_first_ms.max(0) as u32),
            actual_ms: Some(words[0].offset_ms as u32),
            drift_ms: Some(first_word_ms - expected_first_ms),
            reason: Some("first_word_late".to_owned()),
        };
    }

    let mut previous_offset = 0_u64;
    let mut worst: Option<(String, u32, u32, i64)> = None;
    for (position, part) in utterance.parts.iter().enumerate() {
        let expected = (f64::from(part.end_ms.saturating_sub(render_start_ms)) * tempo.max(1.0))
            .round() as u32;
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

/// Validate required semantic phrase bookmarks after the selected Rubber Band tempo is applied.
pub fn validate_semantic_alignment(
    events: &[TimingEvent],
    expected: &[(String, u32, i64)],
    render_start_ms: u32,
    tempo: f64,
) -> AlignmentResult {
    let mut previous_source = 0;
    let mut previous_offset = None;
    let mut worst: Option<(String, u32, u32, i64)> = None;
    let mut observed: Option<(String, u32, u32, i64)> = None;
    for (name, source_ms, tolerance) in expected {
        let matches = events
            .iter()
            .filter(|event| {
                event.kind == TimingEventKind::Bookmark && event.mark.as_deref() == Some(name)
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return AlignmentResult::reject(if matches.is_empty() {
                format!("semantic_anchor_missing_{name}")
            } else {
                format!("semantic_anchor_duplicated_{name}")
            });
        }
        if previous_offset.is_some_and(|previous| matches[0].offset_ms < previous) {
            return AlignmentResult::reject("semantic_anchor_event_order");
        }
        previous_offset = Some(matches[0].offset_ms);
        let rendered = u32::try_from(
            u64::from(render_start_ms)
                .saturating_add((matches[0].offset_ms as f64 / tempo.max(1.0)).round() as u64),
        )
        .unwrap_or(u32::MAX);
        let drift = i64::from(rendered) - i64::from(*source_ms);
        if observed
            .as_ref()
            .is_none_or(|current| drift.abs() > current.3.abs())
        {
            observed = Some((name.clone(), *source_ms, rendered, drift));
        }
        if *source_ms < previous_source {
            return AlignmentResult::reject("semantic_anchor_source_order");
        }
        previous_source = *source_ms;
        if drift.abs() > *tolerance {
            let candidate = (name.clone(), *source_ms, rendered, drift);
            if worst
                .as_ref()
                .is_none_or(|current| drift.abs() > current.3.abs())
            {
                worst = Some(candidate);
            }
        }
    }
    if let Some((worst_anchor, expected_ms, actual_ms, drift_ms)) = worst {
        return AlignmentResult {
            accepted: false,
            worst_anchor: Some(worst_anchor),
            expected_ms: Some(expected_ms),
            actual_ms: Some(actual_ms),
            drift_ms: Some(drift_ms),
            reason: Some("semantic_anchor_drift".to_owned()),
        };
    }
    AlignmentResult {
        accepted: true,
        worst_anchor: None,
        expected_ms: None,
        actual_ms: None,
        drift_ms: observed.map(|(_, _, _, drift)| drift),
        reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_timeline::{
        SourceBarrier, SourcePhrase, SourceToken, TimingConfidence, TimingMode,
    };

    fn bound_manifest(entries: Vec<ScheduledAudio>) -> RenderManifest {
        let start_ms = entries
            .iter()
            .map(|entry| entry.start_ms)
            .min()
            .unwrap_or(0);
        let end_ms = entries.iter().map(|entry| entry.end_ms).max().unwrap_or(1);
        let mut manifest = RenderManifest::new(entries);
        manifest.source_timeline_version = SOURCE_TIMELINE_VERSION;
        manifest.source_timeline_identity = "test-timeline".to_owned();
        manifest.islands = vec![ManifestIsland {
            id: "island-1".to_owned(),
            start_ms,
            end_ms,
        }];
        manifest
    }

    fn segment(index: usize, start: &str, end: &str, text: &str) -> SrtSegment {
        SrtSegment {
            index,
            start: start.to_owned(),
            end: end.to_owned(),
            text: text.to_owned(),
        }
    }

    fn anchored_manifest() -> (RenderManifest, SourceTimeline, SyncLayout) {
        let segments = vec![segment(1, "00:00:00,000", "00:00:01,000", "click Run")];
        let timeline = SourceTimeline {
            version: SOURCE_TIMELINE_VERSION,
            mode: TimingMode::Granular,
            timing_provider: Some("test".to_owned()),
            timing_fallback_reason: None,
            granular_match_count: Some(2),
            granular_source_word_count: Some(2),
            tokens: vec![
                SourceToken {
                    id: "t-1".to_owned(),
                    text: "click".to_owned(),
                    start_ms: 0,
                    end_ms: 400,
                    confidence: TimingConfidence::Granular,
                    source_segment: 1,
                },
                SourceToken {
                    id: "t-2".to_owned(),
                    text: "Run".to_owned(),
                    start_ms: 400,
                    end_ms: 1_000,
                    confidence: TimingConfidence::Granular,
                    source_segment: 1,
                },
            ],
            phrases: vec![SourcePhrase {
                id: "p-1-action-1".to_owned(),
                text: "Run".to_owned(),
                start_token: "t-2".to_owned(),
                end_token: "t-2".to_owned(),
                preferred_start_ms: 400,
                preferred_end_ms: 1_000,
                coupling: Coupling::Anchored,
                confidence: TimingConfidence::Granular,
                owner_index: 1,
                required: true,
            }],
            barriers: vec![
                SourceBarrier {
                    id: "barrier-start".to_owned(),
                    time_ms: 0,
                    reason: "videoStart".to_owned(),
                },
                SourceBarrier {
                    id: "barrier-end".to_owned(),
                    time_ms: 1_000,
                    reason: "videoEnd".to_owned(),
                },
            ],
            duration_ms: 1_000,
            cue_overlap_count: 0,
            projected_anchor_count: 0,
            projected_anchor_ids: Vec::new(),
        };
        let layout = build_sync_layout(&timeline, &segments).unwrap();
        let manifest = RenderManifest::new(vec![ScheduledAudio {
            id: "block-1".to_owned(),
            path: PathBuf::from("block-1.mp3"),
            start_ms: 0,
            end_ms: 1_000,
            source_segments: vec![1],
            measured_audio_ms: 1_000,
            rendered_audio_ms: 1_000,
            tempo: 1.0,
            island_id: Some("island-1".to_owned()),
            cache_identity: "cache".to_owned(),
            alignment: EntryAlignment {
                status: EntryAlignmentStatus::Aligned,
                anchors: vec![RenderedAnchor {
                    id: "p-1-action-1".to_owned(),
                    bookmark: "bookmark".to_owned(),
                    source_ms: 400,
                    raw_offset_ms: 400,
                    rendered_ms: 400,
                    drift_ms: 0,
                    tolerance_ms: MAX_ANCHOR_DRIFT_MS,
                    coupling: Coupling::Anchored,
                }],
            },
        }])
        .with_semantic_alignment_support(true)
        .with_source_timeline(&timeline, &layout)
        .unwrap();
        (manifest, timeline, layout)
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
        let manifest = bound_manifest(vec![ScheduledAudio {
            id: "u1".to_owned(),
            path: PathBuf::from("u1.mp3"),
            start_ms: 0,
            end_ms: 1_000,
            source_segments: vec![1, 1],
            measured_audio_ms: 1_000,
            rendered_audio_ms: 1_000,
            tempo: 1.0,
            island_id: Some("island-1".to_owned()),
            cache_identity: "cache".to_owned(),
            alignment: EntryAlignment::default(),
        }]);
        assert!(manifest.validate(&[1, 2]).is_err());
    }

    #[test]
    fn manifest_rejects_non_monotonic_entry_order() {
        let entries = [(2, 2_000, 3_000), (1, 0, 1_000)]
            .into_iter()
            .map(|(index, start_ms, end_ms)| ScheduledAudio {
                id: format!("block-{index}"),
                path: PathBuf::from(format!("block-{index}.mp3")),
                start_ms,
                end_ms,
                source_segments: vec![index],
                measured_audio_ms: 1_000,
                rendered_audio_ms: 1_000,
                tempo: 1.0,
                island_id: Some("island-1".to_owned()),
                cache_identity: format!("cache-{index}"),
                alignment: EntryAlignment::default(),
            })
            .collect();
        assert!(bound_manifest(entries).validate(&[2, 1]).is_err());
    }

    #[test]
    fn manifest_rejects_stale_anchor_timing_and_source_evidence() {
        let (manifest, timeline, layout) = anchored_manifest();
        assert!(manifest.validate(&[1]).is_ok());
        assert!(
            manifest
                .validate_source_timeline(&timeline, &layout)
                .is_ok()
        );

        let mut mismatched_layout = layout.clone();
        mismatched_layout.blocks[0].source_segments.push(2);
        assert!(
            manifest
                .validate_source_timeline(&timeline, &mismatched_layout)
                .is_err()
        );

        let mut stale_timing = manifest.clone();
        stale_timing.entries[0].alignment.anchors[0].rendered_ms = 399;
        assert!(stale_timing.validate(&[1]).is_err());

        let mut stale_source = manifest;
        stale_source.entries[0].alignment.anchors[0].source_ms = 401;
        stale_source.entries[0].alignment.anchors[0].drift_ms = -1;
        assert!(stale_source.validate(&[1]).is_ok());
        assert!(
            stale_source
                .validate_source_timeline(&timeline, &layout)
                .is_err()
        );
    }

    #[test]
    fn manifest_uses_each_entry_slot_instead_of_next_entry_start() {
        // Regression fixture for the old two-clock bug: the planner approved a 2.0 s block,
        // while the mixer silently borrowed the next cue start at 2.5 s.
        let planner_slot_ms = 2_000;
        let legacy_mixer_slot_ms = 2_500;
        assert_ne!(planner_slot_ms, legacy_mixer_slot_ms);
        let manifest = bound_manifest(vec![
            ScheduledAudio {
                id: "block-1".to_owned(),
                path: PathBuf::from("block-1.mp3"),
                start_ms: 0,
                end_ms: 2_000,
                source_segments: vec![1],
                measured_audio_ms: 1_900,
                rendered_audio_ms: 1_728,
                tempo: 1.1,
                island_id: Some("island-1".to_owned()),
                cache_identity: "cache-1".to_owned(),
                alignment: EntryAlignment::default(),
            },
            ScheduledAudio {
                id: "block-2".to_owned(),
                path: PathBuf::from("block-2.mp3"),
                start_ms: 2_500,
                end_ms: 4_000,
                source_segments: vec![2],
                measured_audio_ms: 1_000,
                rendered_audio_ms: 1_000,
                tempo: 1.0,
                island_id: Some("island-1".to_owned()),
                cache_identity: "cache-2".to_owned(),
                alignment: EntryAlignment::default(),
            },
        ]);
        assert!(manifest.validate(&[1, 2]).is_ok());
        assert_eq!(
            manifest.entries[0].end_ms - manifest.entries[0].start_ms,
            planner_slot_ms
        );
    }

    #[test]
    fn review_manifest_can_record_over_cap_audio_without_becoming_publishable() {
        let entry = ScheduledAudio {
            id: "block-1".to_owned(),
            path: PathBuf::from("block-1.mp3"),
            start_ms: 0,
            end_ms: 1_000,
            source_segments: vec![1],
            measured_audio_ms: 2_000,
            rendered_audio_ms: 1_000,
            tempo: 2.0,
            island_id: Some("island-1".to_owned()),
            cache_identity: "cache".to_owned(),
            alignment: EntryAlignment::default(),
        };
        assert!(bound_manifest(vec![entry.clone()]).validate(&[1]).is_err());
        assert!(
            bound_manifest(vec![entry])
                .review_required()
                .validate(&[1])
                .is_ok()
        );
        let over_slot = ScheduledAudio {
            id: "block-2".to_owned(),
            path: PathBuf::from("block-2.mp3"),
            start_ms: 0,
            end_ms: 1_000,
            source_segments: vec![1],
            measured_audio_ms: 1_001,
            rendered_audio_ms: 1_001,
            tempo: 1.0,
            island_id: Some("island-1".to_owned()),
            cache_identity: "cache".to_owned(),
            alignment: EntryAlignment::default(),
        };
        assert!(
            bound_manifest(vec![over_slot.clone()])
                .validate(&[1])
                .is_err()
        );
        assert!(
            bound_manifest(vec![over_slot])
                .review_required()
                .validate(&[1])
                .is_ok()
        );
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

    #[test]
    fn semantic_anchor_drift_is_measured_after_tempo() {
        let events = vec![TimingEvent {
            kind: TimingEventKind::Bookmark,
            offset_ms: 1_100,
            duration_ms: 0,
            mark: Some("dubora-u1-ap-1-action-1".to_owned()),
            text: None,
        }];
        let accepted = validate_semantic_alignment(
            &events,
            &[("dubora-u1-ap-1-action-1".to_owned(), 1_000, 50)],
            0,
            1.1,
        );
        assert!(accepted.accepted);
        assert_eq!(accepted.drift_ms, Some(0));
        let rejected = validate_semantic_alignment(
            &events,
            &[("dubora-u1-ap-1-action-1".to_owned(), 1_300, 50)],
            0,
            1.1,
        );
        assert_eq!(rejected.reason.as_deref(), Some("semantic_anchor_drift"));
    }

    #[test]
    fn semantic_alignment_rejects_reversed_bookmark_event_order() {
        let events = vec![
            TimingEvent {
                kind: TimingEventKind::Bookmark,
                offset_ms: 1_000,
                duration_ms: 0,
                mark: Some("action".to_owned()),
                text: None,
            },
            TimingEvent {
                kind: TimingEventKind::Bookmark,
                offset_ms: 500,
                duration_ms: 0,
                mark: Some("result".to_owned()),
                text: None,
            },
        ];
        let result = validate_semantic_alignment(
            &events,
            &[
                ("action".to_owned(), 1_000, 50),
                ("result".to_owned(), 1_500, 50),
            ],
            0,
            1.0,
        );
        assert_eq!(
            result.reason.as_deref(),
            Some("semantic_anchor_event_order")
        );
    }
}
