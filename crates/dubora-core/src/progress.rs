use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Clean,
    Tts,
    Audio,
    Video,
    Done,
    Error,
    Skip,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub stage: Stage,
    pub video: String,
    pub current: Option<u32>,
    pub total: Option<u32>,
    pub message: String,
}

impl ProgressEvent {
    pub fn new(stage: Stage, video: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            stage,
            video: video.into(),
            current: None,
            total: None,
            message: message.into(),
        }
    }

    pub fn progress(mut self, current: usize, total: usize) -> Self {
        self.current = Some(current as u32);
        self.total = Some(total as u32);
        self
    }
}

pub trait ProgressReporter: Send + Sync {
    fn event(&self, event: ProgressEvent);

    /// Return true when the user requested cancellation.
    fn is_cancelled(&self) -> bool {
        false
    }
}

impl<F> ProgressReporter for F
where
    F: Fn(ProgressEvent) + Send + Sync,
{
    fn event(&self, event: ProgressEvent) {
        self(event);
    }
}

pub struct NoopProgress;

impl ProgressReporter for NoopProgress {
    fn event(&self, _: ProgressEvent) {}
}
