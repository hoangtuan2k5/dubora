import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";

export type AppConfig = {
  projectRoot: string;
  azureKey: string;
  azureRegion: string;
  /** Azure TTS ShortName; default Florian Dragon HD. */
  azureVoice: string;
  /** Optional expressive style supported by the selected Azure voice. */
  azureVoiceStyle: string;
  geminiKeys: string;
  playlists: Playlist[];
};

export type Playlist = {
  id: string;
  projectRoot: string;
  name: string;
  videoPaths: string[];
};

export type AzureVoice = {
  shortName: string;
  displayName: string;
  localName: string;
  gender: string;
  locale: string;
  localeName: string;
  status: string;
  voiceType: string;
  styles: string[];
  secondaryLocales: string[];
  capabilities: VoiceCapabilities;
};

export type VoiceCapabilities = {
  modelFamily: string;
  multilingual: boolean;
  timingEvents: "supported" | "unsupported" | "unknown";
  style: "supported" | "unsupported" | "unknown";
  manualEmphasis: "supported" | "unsupported" | "unknown";
  manualProsody: "supported" | "unsupported" | "unknown";
  semanticBreak: "supported" | "unsupported" | "unknown";
  pronunciation: "supported" | "unsupported" | "unknown";
  automaticDelivery: "supported" | "unsupported" | "unknown";
};

export const DEFAULT_AZURE_VOICE = "de-DE-Florian:DragonHDLatestNeural";

export type VideoInfo = {
  relativePath: string;
  hasSrt: boolean;
  hasOutput: boolean;
  addedAt: number;
};

export type SubtitleCue = {
  start: number;
  end: number;
  text: string;
};

export type VideoPlayback = {
  videoPath: string;
  cues: SubtitleCue[];
};

export type FfmpegStatus = {
  ready: boolean;
  path: string;
  message: string;
};

export type DubProgress = {
  jobId: string;
  stage: "clean" | "tts" | "audio" | "video" | "done" | "error" | "skip";
  video: string;
  current?: number;
  total?: number;
  message: string;
};

export type SaveConfigResult = {
  createdDirs: string[];
};

export type YoutubeImportResult = {
  relativePath: string;
  subtitleLanguage: string;
  videos: VideoInfo[];
};

export type LibraryMutationResult = {
  videos: VideoInfo[];
};

export const api = {
  getConfig: () => invoke<AppConfig>("get_config"),
  saveConfig: (config: AppConfig) => invoke<SaveConfigResult>("save_config", { config }),
  scanVideos: (root: string) => invoke<VideoInfo[]>("scan_videos", { root }),
  importYoutube: (url: string, quality: string) =>
    invoke<YoutubeImportResult>("import_youtube", { request: { url, quality } }),
  renameVideo: (relativePath: string, newStem: string) =>
    invoke<LibraryMutationResult>("rename_video", { request: { relativePath, newStem } }),
  deleteVideo: (relativePath: string) =>
    invoke<LibraryMutationResult>("delete_video", { relativePath }),
  getVideoPlayback: (relativePath: string) =>
    invoke<VideoPlayback>("get_video_playback", { relativePath }),
  listVoices: (azureKey: string, azureRegion: string) =>
    invoke<AzureVoice[]>("list_voices", { request: { azureKey, azureRegion } }),
  defaultAzureVoice: () => invoke<string>("default_azure_voice"),
  startDub: (
    targets: string[],
    force: boolean,
    forceTts: boolean,
    translate: boolean,
    naturalVoice: boolean,
    naturalMode: "sync" | "balanced" | "natural",
    sourceTiming: "caption-derived" | "azure-fast",
    duckOriginal: boolean,
    loudnessNormalize: boolean,
    voiceoverEq: boolean,
  ) =>
    invoke<string>("start_dub", {
      request: {
        targets,
        force,
        forceTts,
        translate,
        naturalVoice,
        naturalMode,
        sourceTiming,
        duckOriginal,
        loudnessNormalize,
        voiceoverEq,
      },
    }),
  cancelDub: () => invoke<void>("cancel_dub"),
  ffmpegStatus: () => invoke<FfmpegStatus>("get_ffmpeg_status"),
  openPath: (path: string) => invoke<void>("open_path", { path }),
  /** Open http(s) links in the system default browser (not the Tauri webview). */
  openUrl: (url: string) => openUrl(url),
  pickDirectory: () => open({ directory: true, multiple: false }),
  onProgress: (handler: (progress: DubProgress) => void): Promise<UnlistenFn> =>
    listen<DubProgress>("dub-progress", ({ payload }) => handler(payload)),
};
