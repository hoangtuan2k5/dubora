use anyhow::{bail, Result};

use crate::speech_plan::NaturalMode;

pub const ACCEPTABLE_ERROR_RATIO: f64 = 0.05;
pub const PROSODY_REGEN_ERROR_RATIO: f64 = 0.10;
pub const MAX_TIME_STRETCH_TEMPO: f64 = 1.12;
pub const MAX_BALANCED_BORROW_MS: u32 = 250;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurationTarget {
    pub slot_ms: u32,
    pub following_silence_ms: u32,
    pub mode: NaturalMode,
}

impl DurationTarget {
    pub fn effective_ms(self) -> u32 {
        let borrowed = match self.mode {
            NaturalMode::Sync => 0,
            NaturalMode::Balanced => self.following_silence_ms.min(MAX_BALANCED_BORROW_MS),
            NaturalMode::Natural => self.following_silence_ms,
        };
        self.slot_ms.saturating_add(borrowed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DurationDecision {
    Accept,
    RegenerateWithProsody { rate_percent: i8 },
    RegenerateSpeech { target_ms: u32 },
    TimeStretch { tempo: f64 },
    Reject { required_tempo: f64 },
}

/// Decide how to fit a synthesized segment into its target slot.
///
/// `regenerated` must be true after the single permitted TTS retry. The caller must never
/// time-stretch when this returns `Reject`; that would either exceed the naturalness cap or
/// truncate speech.
pub fn solve_duration(
    target: DurationTarget,
    actual_ms: u32,
    regenerated: bool,
) -> Result<DurationDecision> {
    let target_ms = target.effective_ms();
    if target_ms == 0 || actual_ms == 0 {
        bail!("Duration solver requires non-zero target and audio durations");
    }

    let tempo = actual_ms as f64 / target_ms as f64;
    let error_ratio = (tempo - 1.0).abs();
    if error_ratio <= ACCEPTABLE_ERROR_RATIO {
        return Ok(DurationDecision::Accept);
    }
    if !regenerated {
        if error_ratio <= PROSODY_REGEN_ERROR_RATIO {
            return Ok(DurationDecision::RegenerateWithProsody {
                rate_percent: ((tempo - 1.0) * 100.0).round() as i8,
            });
        }
        return Ok(DurationDecision::RegenerateSpeech { target_ms });
    }

    if (1.0 / MAX_TIME_STRETCH_TEMPO..=MAX_TIME_STRETCH_TEMPO).contains(&tempo) {
        return Ok(DurationDecision::TimeStretch { tempo });
    }
    Ok(DurationDecision::Reject {
        required_tempo: tempo,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(mode: NaturalMode) -> DurationTarget {
        DurationTarget {
            slot_ms: 1_000,
            following_silence_ms: 600,
            mode,
        }
    }

    #[test]
    fn accepts_small_timing_error() {
        assert_eq!(
            solve_duration(target(NaturalMode::Sync), 1_049, false).unwrap(),
            DurationDecision::Accept
        );
    }

    #[test]
    fn retries_once_with_a_small_rate_change() {
        assert_eq!(
            solve_duration(target(NaturalMode::Sync), 1_080, false).unwrap(),
            DurationDecision::RegenerateWithProsody { rate_percent: 8 }
        );
        assert_eq!(
            solve_duration(target(NaturalMode::Sync), 920, false).unwrap(),
            DurationDecision::RegenerateWithProsody { rate_percent: -8 }
        );
    }

    #[test]
    fn asks_the_planner_to_rephrase_large_errors_before_retry() {
        assert_eq!(
            solve_duration(target(NaturalMode::Sync), 1_150, false).unwrap(),
            DurationDecision::RegenerateSpeech { target_ms: 1_000 }
        );
    }

    #[test]
    fn uses_only_bounded_stretch_after_the_retry() {
        assert_eq!(
            solve_duration(target(NaturalMode::Sync), 1_080, true).unwrap(),
            DurationDecision::TimeStretch { tempo: 1.08 }
        );
        assert_eq!(
            solve_duration(target(NaturalMode::Sync), 1_150, true).unwrap(),
            DurationDecision::Reject {
                required_tempo: 1.15
            }
        );
    }

    #[test]
    fn respects_the_mode_specific_silent_gap_budget() {
        assert_eq!(target(NaturalMode::Sync).effective_ms(), 1_000);
        assert_eq!(target(NaturalMode::Balanced).effective_ms(), 1_250);
        assert_eq!(target(NaturalMode::Natural).effective_ms(), 1_600);
    }
}
