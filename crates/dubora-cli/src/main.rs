use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use dubora_core::ffmpeg::FfmpegPaths;
use dubora_core::translate_gemini::load_env_keys;
use dubora_core::{
    AzureConfig, DubConfig, DubOptions, GeminiConfig, ProgressEvent, ProgressReporter, Provider,
    NaturalMode, create_dubs,
};

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum ProviderArg {
    #[default]
    Azure,
    Edge,
}

impl From<ProviderArg> for Provider {
    fn from(value: ProviderArg) -> Self {
        match value {
            ProviderArg::Azure => Self::Azure,
            ProviderArg::Edge => Self::Edge,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum NaturalModeArg {
    Sync,
    #[default]
    Balanced,
    Natural,
}

impl From<NaturalModeArg> for NaturalMode {
    fn from(value: NaturalModeArg) -> Self {
        match value {
            NaturalModeArg::Sync => Self::Sync,
            NaturalModeArg::Balanced => Self::Balanced,
            NaturalModeArg::Natural => Self::Natural,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "dubora",
    about = "Create Vietnamese dubbed videos from matching video and SRT files",
    author = "Hoàng Chiều Nguyễn Tuấn <tuan@hoangtuan.dev>",
    long_about = "Dubora — Vietnamese video dubbing CLI.\nAuthor: Hoàng Chiều Nguyễn Tuấn <tuan@hoangtuan.dev>\nGitHub: https://github.com/hoangtuan2k5/dubora"
)]
struct Args {
    /// Video filename, path, or directory under assets/video; omit to process all.
    target: Option<PathBuf>,

    /// Project root containing assets/video and assets/srt.
    #[arg(long)]
    root: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t)]
    provider: ProviderArg,

    /// Process even when the output is up to date.
    #[arg(long)]
    force: bool,

    /// Delete cached TTS segments before processing.
    #[arg(long)]
    force_tts: bool,

    /// Translate and polish English SRT to Vietnamese with Gemini.
    #[arg(long)]
    translate: bool,

    /// Use Gemini Speech Plans and capability-aware Azure SSML for more natural delivery.
    #[arg(long)]
    natural_voice: bool,

    /// Natural Voice timing preference; only used with --natural-voice.
    #[arg(long, value_enum, default_value_t)]
    natural_mode: NaturalModeArg,

    /// Gemini API key; repeat this option to round-robin multiple keys.
    #[arg(long)]
    gemini_key: Vec<String>,

    /// Azure TTS voice ShortName (default: de-DE-Florian:DragonHDLatestNeural).
    #[arg(long)]
    voice: Option<String>,

    /// Expressive style supported by the selected Azure voice (for example, chat).
    #[arg(long)]
    voice_style: Option<String>,
}

struct StdoutProgress;

impl ProgressReporter for StdoutProgress {
    fn event(&self, event: ProgressEvent) {
        match (event.current, event.total) {
            (Some(current), Some(total)) => println!("{} [{current}/{total}]", event.message),
            _ => println!("{}", event.message),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let root = match args.root {
        Some(root) => root,
        None => std::env::current_dir()?,
    };
    let provider = args.provider.into();
    if matches!(provider, Provider::Edge) {
        anyhow::bail!("Edge TTS is not available in the Rust MVP; use --provider azure");
    }
    let mut gemini_keys = load_env_keys(&root);
    gemini_keys.extend(args.gemini_key);

    let mut azure = load_azure_config(&root)?
        .ok_or_else(|| anyhow::anyhow!("Missing AZURE_KEY. Add it to <root>/.env or export it"))?;
    if let Some(voice) = args.voice.as_deref() {
        azure = azure.with_voice(voice)?;
    }
    if let Some(style) = args.voice_style.as_deref() {
        azure = azure.with_style(style)?;
    }
    let config = DubConfig {
        root,
        provider,
        azure: Some(azure),
        gemini: (args.translate || args.natural_voice)
            .then(|| GeminiConfig::new(gemini_keys))
            .transpose()?,
        ffmpeg: FfmpegPaths::resolve(None),
    };
    let options = DubOptions {
        force: args.force,
        force_tts: args.force_tts,
        translate: args.translate,
        natural_mode: args.natural_voice.then(|| args.natural_mode.into()),
    };

    create_dubs(&config, args.target.as_deref(), options, &StdoutProgress).await?;
    Ok(())
}

fn load_azure_config(root: &std::path::Path) -> Result<Option<AzureConfig>> {
    let mut key = std::env::var("AZURE_KEY").ok();
    let mut region = std::env::var("AZURE_REGION").ok();
    let mut voice = std::env::var("AZURE_VOICE").ok();
    let mut style = std::env::var("AZURE_VOICE_STYLE").ok();

    if let Ok(contents) = std::fs::read_to_string(root.join(".env")) {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((name, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches(['\'', '"']).to_owned();
            match name.trim() {
                "AZURE_KEY" if key.is_none() => key = Some(value),
                "AZURE_REGION" if region.is_none() => region = Some(value),
                "AZURE_VOICE" if voice.is_none() => voice = Some(value),
                "AZURE_VOICE_STYLE" if style.is_none() => style = Some(value),
                _ => {}
            }
        }
    }

    key.map(|key| {
        let config = AzureConfig::new(key, region.unwrap_or_default())?;
        let config = match voice {
            Some(voice) => config.with_voice(voice),
            None => Ok(config),
        }?;
        match style {
            Some(style) => config.with_style(style),
            None => Ok(config),
        }
    })
    .transpose()
}
