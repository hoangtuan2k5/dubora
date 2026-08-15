use anyhow::{Result, bail};

use crate::speech_plan::NaturalMode;

pub const ACCEPTABLE_ERROR_RATIO: f64 = 0.05;
pub const PROSODY_REGEN_ERROR_RATIO: f64 = 0.10;
pub const MAX_TIME_STRETCH_TEMPO: f64 = 1.13;
pub const MAX_TIME_STRETCH_TOLERANCE_MS: u32 = 2;
pub const MAX_BALANCED_BORROW_MS: u32 = 250;
pub const SEGMENT_HEADROOM_MS: u32 = 80;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurationTarget {
    pub slot_ms: u32,
    pub following_silence_ms: u32,
    pub spillback_ms: u32,
    pub mode: NaturalMode,
}

impl DurationTarget {
    pub fn effective_ms(self) -> u32 {
        let borrowed = match self.mode {
            NaturalMode::Sync => 0,
            NaturalMode::Balanced => self.following_silence_ms.min(MAX_BALANCED_BORROW_MS),
            NaturalMode::Natural => self.following_silence_ms,
        };
        self.slot_ms
            .saturating_add(borrowed)
            .saturating_add(self.spillback_ms)
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

/// Decide how to fit a synthesized segment into its maximum target slot.
///
/// Shorter audio is safe and leaves natural trailing silence. `regenerated` is true after the
/// caller exhausts its permitted rephrases. The caller must never time-stretch when this returns
/// `Reject`; that would exceed the naturalness cap or truncate speech.
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
    if tempo <= 1.0 + ACCEPTABLE_ERROR_RATIO {
        return Ok(DurationDecision::Accept);
    }
    if !regenerated && tempo <= 1.0 + PROSODY_REGEN_ERROR_RATIO {
        return Ok(DurationDecision::RegenerateWithProsody {
            rate_percent: ((tempo - 1.0) * 100.0).round() as i8,
        });
    }
    let capped_duration_ms = (target_ms as f64 * MAX_TIME_STRETCH_TEMPO).ceil() as u32;
    if actual_ms <= capped_duration_ms.saturating_add(MAX_TIME_STRETCH_TOLERANCE_MS) {
        return Ok(DurationDecision::TimeStretch { tempo });
    }
    if !regenerated {
        return Ok(DurationDecision::RegenerateSpeech { target_ms });
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
            spillback_ms: 0,
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
    fn accepts_shorter_audio_without_padding_or_retry() {
        assert_eq!(
            solve_duration(target(NaturalMode::Sync), 780, false).unwrap(),
            DurationDecision::Accept
        );
    }

    #[test]
    fn retries_once_with_a_small_rate_change() {
        assert_eq!(
            solve_duration(target(NaturalMode::Sync), 1_080, false).unwrap(),
            DurationDecision::RegenerateWithProsody { rate_percent: 8 }
        );
    }

    #[test]
    fn stretches_a_bounded_overrun_without_an_ai_retry() {
        assert_eq!(
            solve_duration(target(NaturalMode::Sync), 1_132, false).unwrap(),
            DurationDecision::TimeStretch { tempo: 1.132 }
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
            solve_duration(target(NaturalMode::Sync), 1_133, true).unwrap(),
            DurationDecision::Reject {
                required_tempo: 1.133
            }
        );
    }

    #[test]
    fn respects_the_mode_specific_silent_gap_budget() {
        assert_eq!(target(NaturalMode::Sync).effective_ms(), 1_000);
        assert_eq!(target(NaturalMode::Balanced).effective_ms(), 1_250);
        assert_eq!(target(NaturalMode::Natural).effective_ms(), 1_600);
    }

    #[test]
    fn adds_safe_spillback_to_the_effective_slot() {
        let mut target = target(NaturalMode::Balanced);
        target.spillback_ms = 1_500;
        assert_eq!(target.effective_ms(), 2_750);
    }
}
