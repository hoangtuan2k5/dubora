use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use viet_dub_core::ffmpeg::FfmpegPaths;
use viet_dub_core::{
    AzureConfig, DubConfig, DubOptions, ProgressEvent, ProgressReporter, Provider, create_dubs,
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

#[derive(Debug, Parser)]
#[command(
    name = "viet-dub",
    about = "Create Vietnamese dubbed videos from matching video and SRT files",
    author = "Hoàng Chiều Nguyễn Tuấn <tuan@hoangtuan.dev>",
    long_about = "viet-dub — Vietnamese video dubbing CLI.\nAuthor: Hoàng Chiều Nguyễn Tuấn <tuan@hoangtuan.dev>\nGitHub: https://github.com/hoangtuan2k5/viet-dub"
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

    /// Translate English SRT to Vietnamese (not available in the Rust MVP).
    #[arg(long)]
    translate: bool,

    /// Azure TTS voice ShortName (default: de-DE-Florian:DragonHDLatestNeural).
    #[arg(long)]
    voice: Option<String>,
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
    if args.translate {
        anyhow::bail!("English-to-Vietnamese translation is not available in the Rust MVP");
    }

    let mut azure = load_azure_config(&root)?
        .ok_or_else(|| anyhow::anyhow!("Missing AZURE_KEY. Add it to <root>/.env or export it"))?;
    if let Some(voice) = args.voice.as_deref() {
        azure = azure.with_voice(voice)?;
    }
    let config = DubConfig {
        root,
        provider,
        azure: Some(azure),
        ffmpeg: FfmpegPaths::resolve(None),
    };
    let options = DubOptions {
        force: args.force,
        force_tts: args.force_tts,
        translate: args.translate,
    };

    create_dubs(&config, args.target.as_deref(), options, &StdoutProgress).await?;
    Ok(())
}

fn load_azure_config(root: &std::path::Path) -> Result<Option<AzureConfig>> {
    let mut key = std::env::var("AZURE_KEY").ok();
    let mut region = std::env::var("AZURE_REGION").ok();
    let mut voice = std::env::var("AZURE_VOICE").ok();

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
                _ => {}
            }
        }
    }

    key.map(|key| {
        let config = AzureConfig::new(key, region.unwrap_or_default())?;
        match voice {
            Some(voice) => config.with_voice(voice),
            None => Ok(config),
        }
    })
    .transpose()
}
