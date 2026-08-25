use std::collections::HashSet;
use std::sync::OnceLock;

use anyhow::{Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};

pub const SPEECH_PLAN_VERSION: u8 = 2;
const MAX_EMPHASES: usize = 2;
const MAX_PAUSES: usize = 2;
const MIN_PAUSE_MS: u16 = 120;
const MAX_PAUSE_MS: u16 = 800;
const MIN_SPOKEN_RATIO_PERCENT: usize = 45;
const MAX_SPOKEN_RATIO_PERCENT: usize = 180;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NaturalMode {
    Sync,
    #[default]
    Balanced,
    Natural,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    #[default]
    Neutral,
    Serious,
    Reflective,
    Question,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmphasisLevel {
    Light,
    Clear,
    Strong,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Emphasis {
    pub text: String,
    pub level: EmphasisLevel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PauseReason {
    Transition,
    Conclusion,
    Punchline,
    Reflection,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pause {
    pub after: String,
    pub duration_ms: u16,
    pub reason: PauseReason,
}

/// A source phrase mapped to one exact phrase in the Vietnamese performance text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticAnchor {
    pub anchor_id: String,
    pub spoken_phrase: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechPlan {
    pub index: usize,
    pub display_text: String,
    pub spoken_text: String,
    pub target_duration_ms: u32,
    #[serde(default)]
    pub delivery: Delivery,
    #[serde(default)]
    pub emphasis: Vec<Emphasis>,
    #[serde(default)]
    pub pauses: Vec<Pause>,
    #[serde(default)]
    pub style: Option<String>,
    #[serde(default)]
    pub semantic_anchors: Vec<SemanticAnchor>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub semantic_anchor_invalid: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechPlanDocument {
    pub version: u8,
    pub plans: Vec<SpeechPlan>,
    #[serde(default)]
    pub review_required: bool,
}

impl SpeechPlanDocument {
    pub fn new(plans: Vec<SpeechPlan>) -> Self {
        Self {
            version: SPEECH_PLAN_VERSION,
            plans,
            review_required: false,
        }
    }
}

impl SpeechPlan {
    pub fn plain(index: usize, text: impl Into<String>, target_duration_ms: u32) -> Self {
        let text = text.into();
        Self {
            index,
            display_text: text.clone(),
            spoken_text: text,
            target_duration_ms,
            delivery: Delivery::Neutral,
            emphasis: Vec::new(),
            pauses: Vec::new(),
            style: None,
            semantic_anchors: Vec::new(),
            semantic_anchor_invalid: false,
        }
    }

    pub fn validate(&self, allowed_styles: &[String]) -> Result<()> {
        let display_text = self.display_text.trim();
        let spoken_text = self.spoken_text.trim();
        if display_text.is_empty() || spoken_text.is_empty() {
            bail!(
                "Speech plan {} must include display and spoken text",
                self.index
            );
        }
        if self.target_duration_ms < 100 {
            bail!("Speech plan {} has an invalid target duration", self.index);
        }
        validate_spoken_length(self.index, display_text, spoken_text)?;
        reject_added_fillers(self.index, display_text, spoken_text)?;

        if self.emphasis.len() > MAX_EMPHASES {
            bail!(
                "Speech plan {} has more than {MAX_EMPHASES} emphasis phrases",
                self.index
            );
        }
        if self.pauses.len() > MAX_PAUSES {
            bail!(
                "Speech plan {} has more than {MAX_PAUSES} pauses",
                self.index
            );
        }

        let mut emphasis_positions = Vec::with_capacity(self.emphasis.len());
        let mut emphasis_texts = HashSet::new();
        for emphasis in &self.emphasis {
            let phrase = emphasis.text.trim();
            if phrase.is_empty() || !emphasis_texts.insert(phrase) {
                bail!("Speech plan {} has an invalid emphasis phrase", self.index);
            }
            let position = unique_phrase_position(spoken_text, phrase).ok_or_else(|| {
                anyhow::anyhow!(
                    "Speech plan {} emphasis phrase must occur exactly once",
                    self.index
                )
            })?;
            emphasis_positions.push((position, phrase.len()));
        }
        emphasis_positions.sort_unstable();
        if emphasis_positions
            .windows(2)
            .any(|pair| pair[1].0 <= pair[0].0 + pair[0].1 + 1)
        {
            bail!("Speech plan {} has adjacent emphasis phrases", self.index);
        }

        let mut pause_texts = HashSet::new();
        let total_pause_ms: u32 = self
            .pauses
            .iter()
            .map(|pause| pause.duration_ms as u32)
            .sum();
        if total_pause_ms * 100 > self.target_duration_ms * 33 {
            bail!(
                "Speech plan {} allocates too much time to pauses",
                self.index
            );
        }
        for pause in &self.pauses {
            let phrase = pause.after.trim();
            if phrase.is_empty()
                || !pause_texts.insert(phrase)
                || !(MIN_PAUSE_MS..=MAX_PAUSE_MS).contains(&pause.duration_ms)
                || unique_phrase_position(spoken_text, phrase).is_none()
            {
                bail!("Speech plan {} has an invalid pause", self.index);
            }
        }

        if let Some(style) = self.style.as_deref() {
            let style = style.trim();
            if style.is_empty() || !allowed_styles.iter().any(|allowed| allowed == style) {
                bail!(
                    "Speech plan {} selects an unavailable voice style",
                    self.index
                );
            }
        }
        Ok(())
    }

    /// Recovers an invalid model phrase only when its words can be split into exact substrings.
    pub(crate) fn repair_model_emphasis(&mut self) {
        let original = std::mem::take(&mut self.emphasis);
        let mut repaired = Vec::with_capacity(MAX_EMPHASES);

        for emphasis in original {
            if repaired.len() == MAX_EMPHASES {
                break;
            }
            let phrase = emphasis.text.trim();
            if can_add_emphasis(&self.spoken_text, &repaired, phrase) {
                repaired.push(Emphasis {
                    text: phrase.to_owned(),
                    level: emphasis.level,
                });
                continue;
            }

            let split = split_emphasis_phrase(
                &self.spoken_text,
                phrase,
                emphasis.level,
                MAX_EMPHASES - repaired.len(),
            );
            let mut candidate_set = repaired.clone();
            let mut valid_split = true;
            for candidate in &split {
                if can_add_emphasis(&self.spoken_text, &candidate_set, &candidate.text) {
                    candidate_set.push(candidate.clone());
                } else {
                    valid_split = false;
                    break;
                }
            }
            if valid_split {
                repaired = candidate_set;
            }
        }

        repaired.sort_by_key(|emphasis| {
            unique_phrase_position(&self.spoken_text, emphasis.text.trim()).unwrap_or(usize::MAX)
        });
        self.emphasis = repaired;
    }

    /// Keeps an LLM's invalidly short or long performance text from changing subtitle meaning.
    pub(crate) fn repair_model_spoken_length(&mut self) {
        if validate_spoken_length(
            self.index,
            self.display_text.trim(),
            self.spoken_text.trim(),
        )
        .is_err()
        {
            self.spoken_text = self.display_text.clone();
            self.emphasis.clear();
            self.pauses.clear();
            self.semantic_anchors.clear();
        }
    }

    /// Remove only optional conversational framing that delays a required visual action.  The
    /// displayed subtitle remains untouched; this keeps phrases such as "nhấn Finish" close to
    /// the source action without asking Gemini for another paid revision.
    pub(crate) fn compact_anchor_leads(&mut self) {
        if self.semantic_anchors.is_empty() {
            return;
        }
        for prefix in [
            "Sau đó, bạn có thể ",
            "sau đó, bạn có thể ",
            "Bạn có thể ",
            "bạn có thể ",
            "Bạn hãy ",
            "bạn hãy ",
            "Giờ bạn có thể ",
            "giờ bạn có thể ",
        ] {
            if self.spoken_text.starts_with(prefix) {
                let candidate = self.spoken_text[prefix.len()..].trim_start().to_owned();
                if self.semantic_anchors.iter().all(|anchor| {
                    unique_phrase_position(&candidate, &anchor.spoken_phrase).is_some()
                }) {
                    self.spoken_text = candidate;
                    self.emphasis.retain(|item| {
                        unique_phrase_position(&self.spoken_text, &item.text).is_some()
                    });
                    self.pauses.retain(|pause| {
                        unique_phrase_position(&self.spoken_text, &pause.after).is_some()
                    });
                }
                break;
            }
        }
    }

    /// Remove only exact empty tutorial framing while keeping the display text unchanged.
    /// Prefixes and suffixes are compacted only when the spoken/display ratio remains valid.
    pub(crate) fn compact_optional_framing(&mut self) {
        const PREFIXES: [&str; 20] = [
            "Sau đó, bạn có thể ",
            "sau đó, bạn có thể ",
            "Bạn có thể ",
            "bạn có thể ",
            "Bạn hãy ",
            "bạn hãy ",
            "Giờ bạn có thể ",
            "giờ bạn có thể ",
            "Các bạn có thể ",
            "các bạn có thể ",
            "Chúng ta sẽ ",
            "chúng ta sẽ ",
            "Thì chúng ta sẽ ",
            "thì chúng ta sẽ ",
            "Trong video hôm nay, ",
            "trong video hôm nay, ",
            "Trong video này, ",
            "trong video này, ",
            "Thì chúng ta sẽ tiến hành ",
            "thì chúng ta sẽ tiến hành ",
        ];
        const SUFFIXES: [&str; 6] = [
            " trong video hôm nay",
            " trong video ngày hôm nay",
            " trong video này",
            ", trong video hôm nay",
            ", trong video ngày hôm nay",
            ", trong video này",
        ];

        let original = self.spoken_text.clone();
        for prefix in PREFIXES {
            if let Some(candidate) = self.spoken_text.strip_prefix(prefix)
                && self.accepts_compacted_spoken_text(candidate)
                && self.compacted_text_keeps_anchors(candidate)
            {
                self.spoken_text = candidate.trim_start().to_owned();
                break;
            }
        }
        for suffix in SUFFIXES {
            let candidate = self
                .spoken_text
                .strip_suffix(suffix)
                .map(str::to_owned)
                .or_else(|| {
                    self.spoken_text
                        .strip_suffix('.')
                        .and_then(|text| text.strip_suffix(suffix))
                        .map(|text| format!("{text}."))
                });
            if let Some(candidate) = candidate
                && self.accepts_compacted_spoken_text(&candidate)
                && self.compacted_text_keeps_anchors(&candidate)
            {
                self.spoken_text = candidate.trim_end().to_owned();
                break;
            }
        }
        if self.spoken_text != original {
            self.emphasis
                .retain(|item| unique_phrase_position(&self.spoken_text, &item.text).is_some());
            self.pauses
                .retain(|pause| unique_phrase_position(&self.spoken_text, &pause.after).is_some());
        }
    }

    /// Replace on-screen IPs, URLs, and file paths with a short pointer when the line is too long.
    ///
    /// Display text stays unchanged so the subtitle still shows the number. Action phrases such
    /// as "nhấn Enter" must remain unique substrings.
    pub(crate) fn compact_onscreen_technical_tokens(&mut self) -> bool {
        let candidate = replace_onscreen_technical_tokens(&self.spoken_text);
        if candidate == self.spoken_text
            || !self.accepts_compacted_spoken_text(&candidate)
            || !self.compacted_text_keeps_anchors(&candidate)
        {
            return false;
        }
        self.spoken_text = candidate;
        self.emphasis
            .retain(|item| unique_phrase_position(&self.spoken_text, &item.text).is_some());
        self.pauses
            .retain(|pause| unique_phrase_position(&self.spoken_text, &pause.after).is_some());
        true
    }

    fn accepts_compacted_spoken_text(&self, candidate: &str) -> bool {
        !candidate.trim().is_empty()
            && validate_spoken_length(self.index, &self.display_text, candidate).is_ok()
    }

    fn compacted_text_keeps_anchors(&self, candidate: &str) -> bool {
        self.semantic_anchors
            .iter()
            .all(|anchor| unique_phrase_position(candidate, &anchor.spoken_phrase).is_some())
    }

    /// Validate the structured planner's source-anchor mappings after text normalization.
    pub fn validate_semantic_anchors(
        &self,
        required: &[(String, String)],
        normalized_spoken_text: &str,
        normalize: impl Fn(&str) -> String,
    ) -> Result<()> {
        let mut previous_position = None;
        let mut seen = HashSet::new();
        for (anchor_id, source_phrase) in required {
            let anchor = self
                .semantic_anchors
                .iter()
                .find(|anchor| anchor.anchor_id == *anchor_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Speech plan {} is missing required semantic anchor {}",
                        self.index,
                        anchor_id
                    )
                })?;
            if !seen.insert(anchor_id) {
                bail!(
                    "Speech plan {} contains duplicate semantic anchor {}",
                    self.index,
                    anchor_id
                );
            }
            let normalized = normalize(&anchor.spoken_phrase);
            let position =
                unique_phrase_position(normalized_spoken_text, &normalized).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Speech plan {} semantic anchor {} is not an exact unique phrase",
                        self.index,
                        anchor_id
                    )
                })?;
            if previous_position.is_some_and(|previous| position <= previous) {
                bail!(
                    "Speech plan {} semantic anchors are out of source order",
                    self.index
                );
            }
            let source_terms = technical_terms(source_phrase);
            let target_terms = words(&anchor.spoken_phrase);
            if source_terms
                .iter()
                .any(|term| term.len() > 2 && !target_terms.contains(term))
            {
                bail!(
                    "Speech plan {} semantic anchor {} drops a source technical term",
                    self.index,
                    anchor_id
                );
            }
            previous_position = Some(position);
        }
        if self
            .semantic_anchors
            .iter()
            .any(|anchor| !required.iter().any(|(id, _)| id == &anchor.anchor_id))
        {
            bail!(
                "Speech plan {} contains an unknown semantic anchor",
                self.index
            );
        }
        Ok(())
    }
}

fn is_technical_term(term: &str) -> bool {
    term.chars()
        .any(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
        || term.contains(['/', ':', '.', '_', '-'])
}

fn technical_terms(text: &str) -> HashSet<String> {
    text.split_whitespace()
        .map(|term| term.trim_matches(|character: char| !character.is_alphanumeric()))
        .filter(|term| !term.is_empty() && is_technical_term(term))
        .map(str::to_lowercase)
        .collect()
}

fn can_add_emphasis(text: &str, existing: &[Emphasis], phrase: &str) -> bool {
    let Some(position) = unique_phrase_position(text, phrase) else {
        return false;
    };
    existing.iter().all(|emphasis| {
        let Some(other_position) = unique_phrase_position(text, emphasis.text.trim()) else {
            return false;
        };
        phrase != emphasis.text.trim()
            && (position > other_position + emphasis.text.trim().len() + 1
                || other_position > position + phrase.len() + 1)
    })
}

fn split_emphasis_phrase(
    spoken_text: &str,
    phrase: &str,
    level: EmphasisLevel,
    limit: usize,
) -> Vec<Emphasis> {
    let words = phrase
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    if words.len() < 2 || limit == 0 {
        return Vec::new();
    }

    let mut used = vec![false; words.len()];
    let mut recovered = Vec::new();
    for length in (1..words.len()).rev() {
        for start in 0..=words.len() - length {
            let end = start + length;
            if used[start..end].iter().any(|used| *used) || recovered.len() == limit {
                continue;
            }
            let candidate = words[start..end].join(" ");
            if unique_phrase_position(spoken_text, &candidate).is_some() {
                used[start..end].fill(true);
                recovered.push(Emphasis {
                    text: candidate,
                    level,
                });
            }
        }
    }

    used.iter()
        .all(|used| *used)
        .then_some(recovered)
        .unwrap_or_default()
}

fn validate_spoken_length(index: usize, display: &str, spoken: &str) -> Result<()> {
    let display_len = display
        .chars()
        .filter(|character| !character.is_whitespace())
        .count();
    let spoken_len = spoken
        .chars()
        .filter(|character| !character.is_whitespace())
        .count();
    if spoken_len * 100 < display_len * MIN_SPOKEN_RATIO_PERCENT
        || spoken_len * 100 > display_len * MAX_SPOKEN_RATIO_PERCENT
    {
        bail!("Speech plan {index} changes spoken text length too much");
    }
    Ok(())
}

fn reject_added_fillers(index: usize, display: &str, spoken: &str) -> Result<()> {
    const FILLERS: [&str; 4] = ["ừm", "ừ", "uh", "um"];
    let display_words = words(display);
    let spoken_words = words(spoken);
    if FILLERS
        .iter()
        .any(|filler| spoken_words.contains(*filler) && !display_words.contains(*filler))
    {
        bail!("Speech plan {index} adds a filler that is absent from the subtitle");
    }
    Ok(())
}

fn words(text: &str) -> HashSet<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| word.to_lowercase())
        .collect()
}

fn replace_onscreen_technical_tokens(text: &str) -> String {
    static URL: OnceLock<Regex> = OnceLock::new();
    static IP: OnceLock<Regex> = OnceLock::new();
    let with_urls = URL
        .get_or_init(|| {
            Regex::new(r"(?i)https?://[^\s,;]+|www\.[^\s,;]+").expect("URL deixis regex")
        })
        .replace_all(text, |captures: &regex::Captures<'_>| {
            let start = captures.get(0).expect("URL match").start();
            url_pointer(&text[..start])
        });
    let with_urls = with_urls.into_owned();
    let with_ips = IP
        .get_or_init(|| {
            Regex::new(r"(?i)(?:[0-9a-f.]*:){2,}[0-9a-f.]*|\b(?:\d{1,3}\.){3}\d{1,3}\b")
                .expect("IP deixis regex")
        })
        .replace_all(&with_urls, |captures: &regex::Captures<'_>| {
            let start = captures.get(0).expect("IP match").start();
            ip_pointer(&with_urls[..start])
        });
    let with_ips = with_ips.into_owned();
    static PATH: OnceLock<Regex> = OnceLock::new();
    let with_paths = PATH
        .get_or_init(|| {
            Regex::new(r"/(?:[A-Za-z0-9._-]+/){1,}[A-Za-z0-9._-]+").expect("path deixis regex")
        })
        .replace_all(&with_ips, |captures: &regex::Captures<'_>| {
            let start = captures.get(0).expect("path match").start();
            path_pointer(&with_ips[..start])
        });
    with_paths.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn ip_pointer(before: &str) -> &'static str {
    let trimmed = before.trim_end();
    if trimmed.ends_with("IP") || trimmed.ends_with("ip") {
        "này"
    } else {
        "địa chỉ IP này"
    }
}

fn url_pointer(before: &str) -> &'static str {
    let trimmed = before.trim_end().to_ascii_lowercase();
    if trimmed.ends_with("url") || trimmed.ends_with("đường dẫn") {
        "này"
    } else {
        "đường dẫn trên màn hình"
    }
}

fn path_pointer(before: &str) -> &'static str {
    let trimmed = before.trim_end().to_ascii_lowercase();
    if trimmed.ends_with("thư mục")
        || trimmed.ends_with("thu muc")
        || trimmed.ends_with("đường dẫn")
        || trimmed.ends_with("duong dan")
        || trimmed.ends_with("file")
        || trimmed.ends_with("folder")
    {
        "này"
    } else {
        "thư mục trên màn hình"
    }
}

pub(crate) fn unique_phrase_position(text: &str, phrase: &str) -> Option<usize> {
    let mut positions = text.match_indices(phrase).filter_map(|(position, _)| {
        let before = text[..position].chars().next_back();
        let after = text[position + phrase.len()..].chars().next();
        (!before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric))
            .then_some(position)
    });
    let position = positions.next()?;
    positions.next().is_none().then_some(position)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> SpeechPlan {
        SpeechPlan {
            index: 4,
            display_text: "Spring Boot là rất quan trọng.".to_owned(),
            spoken_text: "Spring Boot là rất quan trọng.".to_owned(),
            target_duration_ms: 2_400,
            delivery: Delivery::Serious,
            emphasis: vec![Emphasis {
                text: "rất quan trọng".to_owned(),
                level: EmphasisLevel::Clear,
            }],
            pauses: vec![Pause {
                after: "Spring Boot".to_owned(),
                duration_ms: 180,
                reason: PauseReason::Transition,
            }],
            style: Some("serious".to_owned()),
            semantic_anchors: Vec::new(),
            semantic_anchor_invalid: false,
        }
    }

    #[test]
    fn accepts_a_bounded_plan() {
        plan().validate(&["serious".to_owned()]).unwrap();
    }

    #[test]
    fn rejects_ambiguous_or_excessive_intent() {
        let mut plan = plan();
        plan.emphasis.push(Emphasis {
            text: "Spring Boot".to_owned(),
            level: EmphasisLevel::Light,
        });
        plan.emphasis.push(Emphasis {
            text: "là".to_owned(),
            level: EmphasisLevel::Light,
        });
        assert!(plan.validate(&["serious".to_owned()]).is_err());

        let mut ambiguous = SpeechPlan::plain(1, "rất quan trọng, rất quan trọng", 2_000);
        ambiguous.emphasis.push(Emphasis {
            text: "rất quan trọng".to_owned(),
            level: EmphasisLevel::Strong,
        });
        assert!(ambiguous.validate(&[]).is_err());
    }

    #[test]
    fn rejects_unavailable_style_and_added_filler() {
        let mut plan = plan();
        assert!(plan.validate(&[]).is_err());
        plan.style = None;
        plan.spoken_text = "Ừm, Spring Boot là rất quan trọng.".to_owned();
        assert!(plan.validate(&[]).is_err());
    }

    #[test]
    fn plain_plan_has_a_stable_schema_version() {
        let document = SpeechPlanDocument::new(vec![SpeechPlan::plain(1, "Xin chào", 1_000)]);
        assert_eq!(document.version, SPEECH_PLAN_VERSION);
        document.plans[0].validate(&[]).unwrap();
        assert_eq!(NaturalMode::default(), NaturalMode::Balanced);
    }

    #[test]
    fn semantic_anchor_requires_one_exact_phrase_and_preserves_technical_terms() {
        let mut plan = plan();
        plan.semantic_anchors = vec![SemanticAnchor {
            anchor_id: "p-4-action-1".to_owned(),
            spoken_phrase: "Spring Boot".to_owned(),
        }];
        plan.validate_semantic_anchors(
            &[("p-4-action-1".to_owned(), "open Spring Boot".to_owned())],
            &plan.spoken_text,
            str::to_owned,
        )
        .unwrap();

        plan.semantic_anchors[0].spoken_phrase = "Spring".to_owned();
        assert!(
            plan.validate_semantic_anchors(
                &[("p-4-action-1".to_owned(), "open Spring Boot".to_owned())],
                &plan.spoken_text,
                str::to_owned,
            )
            .is_err()
        );
    }

    #[test]
    fn compacts_optional_lead_before_visual_anchor() {
        let mut plan = SpeechPlan::plain(10, "Bạn có thể nhấn Finish để tạo nó.", 4_000);
        plan.semantic_anchors = vec![SemanticAnchor {
            anchor_id: "p-10-action-1".to_owned(),
            spoken_phrase: "nhấn Finish".to_owned(),
        }];
        plan.compact_anchor_leads();
        assert_eq!(plan.spoken_text, "nhấn Finish để tạo nó.");
        assert!(plan.emphasis.is_empty());
        assert!(plan.pauses.is_empty());
    }

    #[test]
    fn compacts_caddy_suffix_but_does_not_treat_chung_ta_se_as_a_prefix() {
        let mut plan = SpeechPlan::plain(23, "mà chúng ta sẽ tìm hiểu trong video hôm nay.", 4_000);
        plan.compact_optional_framing();
        assert_eq!(plan.spoken_text, "mà chúng ta sẽ tìm hiểu.");
    }

    #[test]
    fn skips_framing_strip_when_the_candidate_is_empty_or_too_short() {
        let mut empty = SpeechPlan::plain(24, "Chúng ta sẽ ", 2_000);
        empty.compact_optional_framing();
        assert_eq!(empty.spoken_text, "Chúng ta sẽ ");

        let mut short = SpeechPlan::plain(25, "Chúng ta sẽ biết.", 2_000);
        short.compact_optional_framing();
        assert_eq!(short.spoken_text, "Chúng ta sẽ biết.");
    }

    #[test]
    fn optional_framing_does_not_remove_an_anchor_phrase() {
        let mut plan = SpeechPlan::plain(26, "Bạn có thể nhấn Run để chạy.", 2_000);
        plan.semantic_anchors = vec![SemanticAnchor {
            anchor_id: "p-26-action-1".to_owned(),
            spoken_phrase: "Bạn có thể".to_owned(),
        }];
        plan.compact_optional_framing();
        assert_eq!(plan.spoken_text, "Bạn có thể nhấn Run để chạy.");
    }

    #[test]
    fn compact_optional_framing_keeps_emphasis_when_the_phrase_survives() {
        let mut plan = SpeechPlan::plain(23, "mà chúng ta sẽ tìm hiểu trong video hôm nay.", 4_000);
        plan.emphasis = vec![Emphasis {
            text: "tìm hiểu".to_owned(),
            level: EmphasisLevel::Strong,
        }];
        plan.compact_optional_framing();
        assert_eq!(plan.spoken_text, "mà chúng ta sẽ tìm hiểu.");
        assert_eq!(plan.emphasis[0].text, "tìm hiểu");
    }

    #[test]
    fn compact_onscreen_ip_keeps_the_action_anchor() {
        let mut plan = SpeechPlan::plain(
            26,
            "và truy cập vào địa chỉ IP 143.198.135.219, nhấn Enter.",
            4_799,
        );
        plan.semantic_anchors = vec![SemanticAnchor {
            anchor_id: "p-26-action-1".to_owned(),
            spoken_phrase: "nhấn Enter".to_owned(),
        }];
        assert!(plan.compact_onscreen_technical_tokens());
        assert_eq!(
            plan.spoken_text,
            "và truy cập vào địa chỉ IP này, nhấn Enter."
        );
        assert_eq!(
            plan.display_text,
            "và truy cập vào địa chỉ IP 143.198.135.219, nhấn Enter."
        );
        assert!(unique_phrase_position(&plan.spoken_text, "nhấn Enter").is_some());
    }

    #[test]
    fn compact_onscreen_url_uses_a_short_pointer() {
        let mut plan = SpeechPlan::plain(8, "Mở https://caddyserver.com/docs ngay.", 3_000);
        assert!(plan.compact_onscreen_technical_tokens());
        assert_eq!(plan.spoken_text, "Mở đường dẫn trên màn hình ngay.");
        assert_eq!(plan.display_text, "Mở https://caddyserver.com/docs ngay.");
    }

    #[test]
    fn compact_onscreen_tokens_leaves_version_numbers_alone() {
        let mut plan = SpeechPlan::plain(9, "Caddy phiên bản 2.0 vẫn chạy.", 2_000);
        assert!(!plan.compact_onscreen_technical_tokens());
        assert_eq!(plan.spoken_text, "Caddy phiên bản 2.0 vẫn chạy.");
    }

    #[test]
    fn compact_onscreen_unix_path_uses_a_short_pointer() {
        let mut plan = SpeechPlan::plain(
            37,
            "Mình có thể đặt file website ở /var/www/html, trước hết xem qua Caddyfile đã.",
            5_352,
        );
        assert!(plan.compact_onscreen_technical_tokens());
        assert_eq!(
            plan.spoken_text,
            "Mình có thể đặt file website ở thư mục trên màn hình, trước hết xem qua Caddyfile đã."
        );
        assert!(plan.display_text.contains("/var/www/html"));
    }
}
