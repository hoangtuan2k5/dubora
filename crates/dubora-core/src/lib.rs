pub mod azure_timing;
pub mod azure_voices;
pub mod build_audio;
pub mod build_video;
pub mod clean_rolling;
pub mod config;
pub mod duration_solver;
pub mod ffmpeg;
pub mod metrics;
pub mod orchestrator;
pub mod paths;
pub mod pause_fit;
pub mod progress;
pub mod source_transcription;
pub mod speech_plan;
pub mod srt;
pub mod sync_timeline;
pub mod translate_gemini;
pub mod tts_azure;
pub mod tts_cache;
pub mod utterance;
pub mod youtube;

pub use azure_voices::{
    AzureVoice, CapabilityState, VoiceCapabilities, VoiceModelFamily, list_azure_voices,
};
pub use config::{
    AZURE_VOICE, AzureConfig, DubConfig, DubOptions, GeminiConfig, Provider, SourceTiming,
};
pub use orchestrator::{VideoInfo, create_dubs, scan_videos};
pub use paths::{
    PROJECT_LAYOUT_DIRS, delete_input_video, ensure_project_layout, rename_input_video,
    repair_adjacent_youtube_subtitles,
};
pub use progress::{ProgressEvent, ProgressReporter, Stage};
pub use speech_plan::NaturalMode;
