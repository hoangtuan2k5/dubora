import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

export type AppConfig = {
  projectRoot: string;
  azureKey: string;
  azureRegion: string;
};

export type VideoInfo = {
  relativePath: string;
  hasSrt: boolean;
  hasOutput: boolean;
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

export const api = {
  getConfig: () => invoke<AppConfig>("get_config"),
  saveConfig: (config: AppConfig) => invoke<void>("save_config", { config }),
  scanVideos: (root: string) => invoke<VideoInfo[]>("scan_videos", { root }),
  startDub: (targets: string[], force: boolean, forceTts: boolean) =>
    invoke<string>("start_dub", {
      request: { targets, force, forceTts, translate: false },
    }),
  cancelDub: () => invoke<void>("cancel_dub"),
  ffmpegStatus: () => invoke<FfmpegStatus>("get_ffmpeg_status"),
  openPath: (path: string) => invoke<void>("open_path", { path }),
  pickDirectory: () => open({ directory: true, multiple: false }),
  onProgress: (handler: (progress: DubProgress) => void): Promise<UnlistenFn> =>
    listen<DubProgress>("dub-progress", ({ payload }) => handler(payload)),
};
