use std::collections::HashSet;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const SPEECH_PLAN_VERSION: u8 = 1;
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
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechPlanDocument {
    pub version: u8,
    pub plans: Vec<SpeechPlan>,
}

impl SpeechPlanDocument {
    pub fn new(plans: Vec<SpeechPlan>) -> Self {
        Self {
            version: SPEECH_PLAN_VERSION,
            plans,
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
        }
    }

    pub fn validate(&self, allowed_styles: &[String]) -> Result<()> {
        let display_text = self.display_text.trim();
        let spoken_text = self.spoken_text.trim();
        if display_text.is_empty() || spoken_text.is_empty() {
            bail!("Speech plan {} must include display and spoken text", self.index);
        }
        if self.target_duration_ms < 100 {
            bail!("Speech plan {} has an invalid target duration", self.index);
        }
        validate_spoken_length(self.index, display_text, spoken_text)?;
        reject_added_fillers(self.index, display_text, spoken_text)?;

        if self.emphasis.len() > MAX_EMPHASES {
            bail!("Speech plan {} has more than {MAX_EMPHASES} emphasis phrases", self.index);
        }
        if self.pauses.len() > MAX_PAUSES {
            bail!("Speech plan {} has more than {MAX_PAUSES} pauses", self.index);
        }

        let mut emphasis_positions = Vec::with_capacity(self.emphasis.len());
        let mut emphasis_texts = HashSet::new();
        for emphasis in &self.emphasis {
            let phrase = emphasis.text.trim();
            if phrase.is_empty() || !emphasis_texts.insert(phrase) {
                bail!("Speech plan {} has an invalid emphasis phrase", self.index);
            }
            let position = unique_phrase_position(spoken_text, phrase).ok_or_else(|| {
                anyhow::anyhow!("Speech plan {} emphasis phrase must occur exactly once", self.index)
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
        let total_pause_ms: u32 = self.pauses.iter().map(|pause| pause.duration_ms as u32).sum();
        if total_pause_ms * 100 > self.target_duration_ms * 33 {
            bail!("Speech plan {} allocates too much time to pauses", self.index);
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
                bail!("Speech plan {} selects an unavailable voice style", self.index);
            }
        }
        Ok(())
    }
}

fn validate_spoken_length(index: usize, display: &str, spoken: &str) -> Result<()> {
    let display_len = display.chars().filter(|character| !character.is_whitespace()).count();
    let spoken_len = spoken.chars().filter(|character| !character.is_whitespace()).count();
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

fn unique_phrase_position(text: &str, phrase: &str) -> Option<usize> {
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
}
