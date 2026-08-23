import { useEffect, useMemo, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import {
  IconAlertTriangle,
  IconArrowRight,
  IconBrandAzure,
  IconCheck,
  IconCircleCheckFilled,
  IconClock,
  IconEye,
  IconEyeOff,
  IconBook,
  IconBrandYoutube,
  IconExternalLink,
  IconFileText,
  IconFilter,
  IconFolder,
  IconFolderOpen,
  IconKey,
  IconLibrary,
  IconLayoutGrid,
  IconLoader2,
  IconMaximize,
  IconMoon,
  IconMovie,
  IconPlayerPlayFilled,
  IconPlayerPauseFilled,
  IconPlayerStopFilled,
  IconPencil,
  IconPlaylistAdd,
  IconPlus,
  IconRefresh,
  IconSearch,
  IconSettings,
  IconSortAscendingLetters,
  IconSortDescendingLetters,
  IconSun,
  IconTerminal2,
  IconTrash,
  IconVideo,
  IconWaveSine,
  IconX,
} from "@tabler/icons-react";
import {
  api,
  DEFAULT_AZURE_VOICE,
  type AppConfig,
  type AzureVoice,
  type DubProgress,
  type FfmpegStatus,
  type Playlist,
  type VideoPlayback,
  type VideoInfo,
} from "./tauri";
import duboraLogoDark from "./assets/dubora-logo-dark.svg";
import duboraLogoLight from "./assets/dubora-logo-light.svg";

const DEFAULT_CONFIG: AppConfig = {
  projectRoot: "",
  azureKey: "",
  azureRegion: "southeastasia",
  azureVoice: DEFAULT_AZURE_VOICE,
  azureVoiceStyle: "",
  geminiKeys: "",
  playlists: [],
};

const POPULAR_LOCALES = ["vi-VN", "en-US", "de-DE", "zh-CN", "ja-JP", "ko-KR", "fr-FR", "en-GB"] as const;

type VoiceSortKey = "name" | "locale" | "gender";
type VoiceSortDir = "asc" | "desc";

type VoiceFeatureFilter = {
  multilingual: boolean;
  hd: boolean;
  female: boolean;
  male: boolean;
  gaOnly: boolean;
};

const EMPTY_VOICE_FEATURES: VoiceFeatureFilter = {
  multilingual: false,
  hd: false,
  female: false,
  male: false,
  gaOnly: false,
};

function isMultilingualVoice(voice: AzureVoice) {
  const blob = `${voice.shortName} ${voice.displayName} ${voice.localName} ${voice.secondaryLocales.join(" ")}`.toLowerCase();
  return blob.includes("multilingual") || blob.includes("multi");
}

function supportsVoiceLocale(voice: AzureVoice, locale: string) {
  return voice.locale === locale || voice.secondaryLocales.includes(locale) || (locale === "vi-VN" && isMultilingualVoice(voice));
}

function isHdVoice(voice: AzureVoice) {
  const blob = voice.shortName.toLowerCase();
  return blob.includes("hd") || blob.includes("dragon");
}

function voiceSortLabel(key: VoiceSortKey) {
  if (key === "locale") return "Ngôn ngữ";
  if (key === "gender") return "Giới tính";
  return "Tên";
}

const STAGES = [
  ["clean", "Làm sạch"],
  ["tts", "Tạo giọng"],
  ["audio", "Phối âm"],
  ["video", "Đóng gói"],
] as const;

const stagePercent: Record<DubProgress["stage"], number> = {
  clean: 12,
  tts: 20,
  audio: 75,
  video: 92,
  done: 100,
  skip: 100,
  error: 100,
};

function describeError(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

/** Join project paths using the separator style of `root` (Windows-friendly). */
function joinPath(...parts: string[]) {
  const usesWin = parts.some((part) => part.includes("\\"));
  const sep = usesWin ? "\\" : "/";
  const segments = parts
    .filter(Boolean)
    .flatMap((part) => part.replace(/[/\\]+/g, "/").split("/"))
    .filter((segment) => segment.length > 0);
  if (usesWin && segments[0]?.endsWith(":")) {
    return `${segments[0]}${sep}${segments.slice(1).join(sep)}`;
  }
  return segments.join(sep);
}

/** Project output path for a video (file), or the outputs root when relativePath is omitted. */
function outputPath(root: string, relativePath?: string) {
  if (!relativePath) return joinPath(root, "outputs");
  const file = relativePath.replace(/\.[^./\\]+$/, ".vi-dub.mp4");
  return joinPath(root, "outputs", file);
}

type LibraryView = "list" | "grid";
type LibrarySort = "name" | "added";
type LibraryGroup = "none" | "name" | "date" | "course";

function videoName(video: VideoInfo) {
  return video.relativePath.split(/[\\/]/).at(-1) ?? video.relativePath;
}

function videoCourse(video: VideoInfo) {
  const parts = video.relativePath.split(/[\\/]/);
  return parts.length > 1 ? parts[0] : "Chưa phân khóa học";
}

function formatAddedAt(seconds: number) {
  return seconds ? new Intl.DateTimeFormat("vi-VN", { dateStyle: "medium" }).format(new Date(seconds * 1000)) : "Không rõ";
}

function formatDuration(seconds: number) {
  if (!Number.isFinite(seconds)) return "0:00";
  const minutes = Math.floor(seconds / 60);
  return `${minutes}:${Math.floor(seconds % 60).toString().padStart(2, "0")}`;
}

function releaseVideo(element: HTMLVideoElement | null) {
  if (!element) return;
  element.pause();
  element.removeAttribute("src");
  element.load();
}

function VideoPlayer({
  video,
  playback,
  loading,
  onClose,
}: {
  video: VideoInfo | null;
  playback: VideoPlayback | null;
  loading: boolean;
  onClose: () => void;
}) {
  const videoRef = useRef<HTMLVideoElement>(null);
  const panelRef = useRef<HTMLElement>(null);
  const [playing, setPlaying] = useState(false);
  const [currentTime, setCurrentTime] = useState(0);
  const [duration, setDuration] = useState(0);
  const [rate, setRate] = useState(1);

  useEffect(() => {
    const element = videoRef.current;
    if (!element || !playback) return;
    if (!element.hasAttribute("src")) {
      element.src = convertFileSrc(playback.videoPath);
      element.load();
    }
    const track = element.addTextTrack("subtitles", "Tiếng Việt", "vi");
    track.mode = "showing";
    playback.cues.forEach((cue) => track.addCue(new VTTCue(cue.start, cue.end, cue.text)));
    element.play().catch(() => {});
    return () => {
      track.mode = "disabled";
      Array.from(track.cues ?? []).forEach((cue) => track.removeCue(cue));
      releaseVideo(element);
    };
  }, [playback]);

  if (!video) {
    return <div className="player-empty"><IconPlayerPlayFilled size={28} /><p>Chọn video để bắt đầu xem.</p></div>;
  }

  const togglePlay = () => {
    const element = videoRef.current;
    if (!element) return;
    if (element.paused) element.play().catch(() => {});
    else element.pause();
  };

  const seek = (value: number) => {
    const element = videoRef.current;
    if (element) element.currentTime = value;
  };

  const changeRate = (nextRate: number) => {
    const element = videoRef.current;
    if (element) element.playbackRate = nextRate;
    setRate(nextRate);
  };

  const toggleFullscreen = () => {
    const panel = panelRef.current;
    if (!panel) return;
    if (document.fullscreenElement) void document.exitFullscreen();
    else void panel.requestFullscreen();
  };

  return (
    <section className="media-player" ref={panelRef} aria-label={`Trình phát ${videoName(video)}`}>
      <div className="media-player-head">
        <div><span>Đang phát</span><strong>{videoName(video)}</strong></div>
        <button className="icon-button" onClick={() => { releaseVideo(videoRef.current); onClose(); }} aria-label="Đóng trình phát" data-tooltip="Đóng"><IconX size={18} /></button>
      </div>
      <div className="media-frame">
        {playback ? (
          <video
            key={playback.videoPath}
            ref={videoRef}
            src={convertFileSrc(playback.videoPath)}
            controls
            onPlay={() => setPlaying(true)}
            onPause={() => setPlaying(false)}
            onTimeUpdate={(event) => setCurrentTime(event.currentTarget.currentTime)}
            onLoadedMetadata={(event) => setDuration(event.currentTarget.duration)}
          />
        ) : (
          <div className="player-loading">{loading ? <IconLoader2 className="spin" size={24} /> : <IconAlertTriangle size={24} />} {loading ? "Đang mở video…" : "Không thể mở video."}</div>
        )}
      </div>
      {playback && (
        <div className="media-controls">
          <button className="icon-button" onClick={togglePlay} aria-label={playing ? "Tạm dừng" : "Phát"} data-tooltip={playing ? "Tạm dừng" : "Phát"}>
            {playing ? <IconPlayerPauseFilled size={19} /> : <IconPlayerPlayFilled size={19} />}
          </button>
          <span>{formatDuration(currentTime)}</span>
          <input aria-label="Tiến độ video" type="range" min="0" max={duration || 0} step="0.1" value={currentTime} onChange={(event) => seek(Number(event.target.value))} />
          <span>{formatDuration(duration)}</span>
          <select aria-label="Tốc độ phát" value={rate} onChange={(event) => changeRate(Number(event.target.value))}>
            {[0.5, 0.75, 1, 1.25, 1.5, 2].map((value) => <option key={value} value={value}>{value}×</option>)}
          </select>
          <button className="icon-button" onClick={toggleFullscreen} aria-label="Toàn màn hình" data-tooltip="Toàn màn hình"><IconMaximize size={19} /></button>
        </div>
      )}
    </section>
  );
}

const TIP_DELAY_MS = 500;

type OptionChipProps = {
  id: string;
  label: string;
  title: string;
  body: string;
  hint: string;
  checked: boolean;
  disabled?: boolean;
  onChange?: (checked: boolean) => void;
};

function OptionChip({ id, label, title, body, hint, checked, disabled, onChange }: OptionChipProps) {
  const [showTip, setShowTip] = useState(false);
  const [tipBelow, setTipBelow] = useState(false);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const clearTimer = () => {
    if (timerRef.current != null) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
  };

  const scheduleShow = (target: HTMLElement) => {
    clearTimer();
    const box = target.getBoundingClientRect();
    setTipBelow(box.top + box.height / 2 < window.innerHeight / 2);
    timerRef.current = setTimeout(() => setShowTip(true), TIP_DELAY_MS);
  };

  const hide = () => {
    clearTimer();
    setShowTip(false);
  };

  useEffect(
    () => () => {
      if (timerRef.current != null) clearTimeout(timerRef.current);
    },
    [],
  );

  return (
    <button
      type="button"
      id={id}
      role="switch"
      aria-checked={checked}
      aria-describedby={`${id}-tip`}
      disabled={disabled}
      className={`option-chip${checked ? " on" : ""}${showTip ? " show-tip" : ""}${tipBelow ? " tip-below" : ""}${disabled ? " is-disabled" : ""}`}
      onPointerEnter={(event) => scheduleShow(event.currentTarget)}
      onPointerLeave={hide}
      onFocus={(event) => scheduleShow(event.currentTarget)}
      onBlur={hide}
      onClick={() => {
        if (disabled || !onChange) return;
        onChange(!checked);
      }}
    >
      <span className="option-chip-label">{label}</span>
      <span className="option-chip-toggle" aria-hidden="true" />
      <span className="option-chip-tip" id={`${id}-tip`} role="tooltip">
        <strong>{title}</strong>
        {body}
        <em>{hint}</em>
      </span>
    </button>
  );
}

type TreeKind = "root" | "folder" | "video" | "srt" | "output" | "note";

type TreeNode = {
  name: string;
  kind: TreeKind;
  tag?: string;
  children?: TreeNode[];
};

const PROJECT_TREE: TreeNode = {
  name: "project-root",
  kind: "root",
  tag: "Bạn chọn trong Thiết lập",
  children: [
    {
      name: "assets",
      kind: "folder",
      tag: "Bạn chuẩn bị",
      children: [
        {
          name: "video",
          kind: "folder",
          tag: "Input video",
          children: [
            {
              name: "Khoa-hoc",
              kind: "folder",
              children: [
                {
                  name: "part1",
                  kind: "folder",
                  children: [{ name: "01. Bai hoc.mp4", kind: "video" }],
                },
              ],
            },
          ],
        },
        {
          name: "srt",
          kind: "folder",
          tag: "Phụ đề khớp path",
          children: [
            {
              name: "Khoa-hoc",
              kind: "folder",
              children: [
                {
                  name: "part1",
                  kind: "folder",
                  children: [{ name: "01. Bai hoc.srt", kind: "srt" }],
                },
              ],
            },
            {
              name: "CC-Khoa-hoc / … (tuỳ chọn)",
              kind: "note",
              tag: "Fallback nếu không có path chuẩn",
            },
          ],
        },
      ],
    },
    {
      name: "work",
      kind: "folder",
      tag: "App tự tạo · file tạm",
    },
    {
      name: "outputs",
      kind: "folder",
      tag: "App tự tạo · kết quả",
      children: [
        {
          name: "Khoa-hoc",
          kind: "folder",
          children: [
            {
              name: "part1",
              kind: "folder",
              children: [{ name: "01. Bai hoc.vi-dub.mp4", kind: "output" }],
            },
          ],
        },
      ],
    },
  ],
};

function treeIcon(kind: TreeKind) {
  const size = 18;
  switch (kind) {
    case "root":
      return <IconFolderOpen size={size} stroke={1.7} />;
    case "video":
      return <IconMovie size={size} stroke={1.7} />;
    case "srt":
      return <IconFileText size={size} stroke={1.7} />;
    case "output":
      return <IconVideo size={size} stroke={1.7} />;
    case "note":
      return <IconFileText size={size} stroke={1.6} />;
    default:
      return <IconFolder size={size} stroke={1.7} />;
  }
}

function FolderTreeNode({ node, depth = 0 }: { node: TreeNode; depth?: number }) {
  return (
    <li className={`tree-node kind-${node.kind}`} style={{ ["--depth" as string]: depth }}>
      <div className="tree-row">
        <span className="tree-icon" aria-hidden>
          {treeIcon(node.kind)}
        </span>
        <span className="tree-name">{node.name}</span>
        {node.tag ? <span className="tree-tag">{node.tag}</span> : null}
      </div>
      {node.children?.length ? (
        <ul className="tree-children">
          {node.children.map((child) => (
            <FolderTreeNode key={`${node.name}/${child.name}`} node={child} depth={depth + 1} />
          ))}
        </ul>
      ) : null}
    </li>
  );
}

/** Open https links in the OS default browser (Tauri webview blocks normal anchors). */
function ExternalLink({
  href,
  children,
  className = "guide-link",
}: {
  href: string;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <button
      type="button"
      className={`${className} inline-link`}
      onClick={() => {
        void api.openUrl(href).catch((error) => {
          console.error("Failed to open URL", href, error);
        });
      }}
    >
      {children}
    </button>
  );
}

function ProjectStructureDiagram() {
  return (
    <div className="structure-diagram" aria-label="Sơ đồ cấu trúc thư mục project">
      <div className="structure-legend" aria-hidden>
        <span className="leg video">
          <IconMovie size={14} /> Video
        </span>
        <span className="leg srt">
          <IconFileText size={14} /> SRT
        </span>
        <span className="leg output">
          <IconVideo size={14} /> Output
        </span>
        <span className="leg folder">
          <IconFolder size={14} /> Thư mục
        </span>
      </div>
      <div className="structure-flow">
        <div className="flow-chip in">INPUT</div>
        <IconArrowRight size={16} className="flow-arrow" />
        <div className="flow-chip mid">Dubora</div>
        <IconArrowRight size={16} className="flow-arrow" />
        <div className="flow-chip out">OUTPUT</div>
      </div>
      <ul className="folder-tree">
        <FolderTreeNode node={PROJECT_TREE} />
      </ul>
      <p className="structure-caption">
        Path tương đối của video và SRT phải <strong>khớp nhau</strong> (cùng folder lồng nhau, cùng
        tên file, khác đuôi <code>.mp4</code> / <code>.srt</code>).
      </p>
    </div>
  );
}

function App() {
  const [view, setView] = useState<"hub" | "library" | "settings" | "guide">("library");
  const [theme, setTheme] = useState<"dark" | "light">(() =>
    localStorage.getItem("theme") === "light" ? "light" : "dark",
  );
  const [config, setConfig] = useState(DEFAULT_CONFIG);
  const [savedConfig, setSavedConfig] = useState(DEFAULT_CONFIG);
  const [videos, setVideos] = useState<VideoInfo[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [query, setQuery] = useState("");
  const [libraryView, setLibraryView] = useState<LibraryView>("list");
  const [librarySort, setLibrarySort] = useState<LibrarySort>("name");
  const [libraryGroup, setLibraryGroup] = useState<LibraryGroup>("none");
  const [activePlaylist, setActivePlaylist] = useState("all");
  const [activeVideo, setActiveVideo] = useState<VideoInfo | null>(null);
  const [playback, setPlayback] = useState<VideoPlayback | null>(null);
  const [loadingPlayback, setLoadingPlayback] = useState(false);
  const [force, setForce] = useState(false);
  const [forceTts, setForceTts] = useState(false);
  const [translate, setTranslate] = useState(false);
  const [naturalVoice, setNaturalVoice] = useState(false);
  const [naturalMode, setNaturalMode] = useState<"sync" | "balanced" | "natural">("balanced");
  const [sourceTiming, setSourceTiming] = useState<"caption-derived" | "azure-fast">("caption-derived");
  const [youtubeUrl, setYoutubeUrl] = useState("");
  const [youtubeQuality, setYoutubeQuality] = useState("highest");
  const [importingYoutube, setImportingYoutube] = useState(false);
  const [showKey, setShowKey] = useState(false);
  const [showGeminiKeys, setShowGeminiKeys] = useState(false);
  const [ffmpeg, setFfmpeg] = useState<FfmpegStatus | null>(null);
  const [progress, setProgress] = useState<DubProgress | null>(null);
  const [logs, setLogs] = useState<DubProgress[]>([]);
  const [busy, setBusy] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState("");
  const [voices, setVoices] = useState<AzureVoice[]>([]);
  const [loadingVoices, setLoadingVoices] = useState(false);
  const [voiceQuery, setVoiceQuery] = useState("");
  const [voiceLocale, setVoiceLocale] = useState<string | "all">("all");
  const [voiceFeatures, setVoiceFeatures] = useState<VoiceFeatureFilter>(EMPTY_VOICE_FEATURES);
  const [voiceSortKey, setVoiceSortKey] = useState<VoiceSortKey>("name");
  const [voiceSortDir, setVoiceSortDir] = useState<VoiceSortDir>("asc");
  const [voiceFilterOpen, setVoiceFilterOpen] = useState(false);
  const [voiceSortOpen, setVoiceSortOpen] = useState(false);
  const voiceFilterRef = useRef<HTMLDivElement>(null);
  const voiceSortRef = useRef<HTMLDivElement>(null);
  const logEndRef = useRef<HTMLDivElement>(null);
  const projectRootRef = useRef("");
  const scanRequestRef = useRef(0);
  const playRequestRef = useRef(0);

  const scan = async (root = config.projectRoot): Promise<boolean> => {
    const requestId = ++scanRequestRef.current;
    if (!root) {
      setVideos([]);
      setSelected(new Set());
      return false;
    }
    setScanning(true);
    setNotice("");
    try {
      const found = await api.scanVideos(root);
      if (requestId !== scanRequestRef.current) return false;
      setVideos(found);
      setSelected((current) =>
        new Set([...current].filter((path) => found.some((video) => video.relativePath === path))),
      );
      return true;
    } catch (error) {
      if (requestId !== scanRequestRef.current) return false;
      setVideos([]);
      setSelected(new Set());
      setNotice(`Không thể quét thư viện: ${describeError(error)}`);
      return false;
    } finally {
      if (requestId === scanRequestRef.current) setScanning(false);
    }
  };

  const refreshVideos = (found: VideoInfo[]) => {
    setVideos(found);
    setSelected((current) =>
      new Set([...current].filter((path) => found.some((video) => video.relativePath === path))),
    );
  };

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem("theme", theme);
  }, [theme]);

  useEffect(() => {
    const setTooltipSide = (event: PointerEvent) => {
      const target = (event.target as HTMLElement).closest<HTMLElement>("[data-tooltip]");
      if (!target) return;
      const box = target.getBoundingClientRect();
      target.dataset.tooltipSide = box.top + box.height / 2 < window.innerHeight / 2 ? "below" : "above";
    };
    document.addEventListener("pointerover", setTooltipSide);
    return () => document.removeEventListener("pointerover", setTooltipSide);
  }, []);

  useEffect(() => {
    projectRootRef.current = savedConfig.projectRoot;
  }, [savedConfig.projectRoot]);

  useEffect(() => {
    if ((view !== "library" && view !== "hub") || !config.projectRoot) return;
    const timer = window.setInterval(() => {
      void api.scanVideos(config.projectRoot).then(refreshVideos).catch(() => {});
    }, 5_000);
    return () => window.clearInterval(timer);
  }, [view, config.projectRoot]);

  useEffect(() => {
    let active = true;
    Promise.all([api.getConfig(), api.ffmpegStatus()])
      .then(([saved, status]) => {
        if (!active) return;
        const loaded = { ...DEFAULT_CONFIG, ...saved };
        setConfig(loaded);
        setSavedConfig(loaded);
        setFfmpeg(status);
        void scan(saved.projectRoot);
      })
      .catch((error) => active && setNotice(`Không thể khởi tạo ứng dụng: ${describeError(error)}`));

    let unlisten: (() => void) | undefined;
    // Throttle UI updates so rapid TTS progress events cannot freeze/black WebView2.
    let logBuffer: DubProgress[] = [];
    let flushTimer: ReturnType<typeof setTimeout> | null = null;
    let latestProgress: DubProgress | null = null;
    let progressTimer: ReturnType<typeof setTimeout> | null = null;

    const flushLogs = () => {
      flushTimer = null;
      if (!logBuffer.length) return;
      const batch = logBuffer;
      logBuffer = [];
      setLogs((current) => [...current, ...batch].slice(-120));
    };

    const flushProgress = () => {
      progressTimer = null;
      if (!latestProgress) return;
      const event = latestProgress;
      latestProgress = null;
      setProgress((current) =>
        event.stage === "tts" && event.current == null && current?.stage === "tts"
          ? { ...event, current: current.current, total: current.total }
          : event,
      );
    };

    void api
      .onProgress((event) => {
        if (!active) return;
        const terminal =
          event.video === "" && (event.stage === "done" || event.stage === "error");

        latestProgress = event;
        if (terminal) {
          if (progressTimer) {
            clearTimeout(progressTimer);
            progressTimer = null;
          }
          flushProgress();
        } else if (!progressTimer) {
          progressTimer = setTimeout(flushProgress, 120);
        }

        logBuffer.push(event);
        if (terminal) {
          if (flushTimer) {
            clearTimeout(flushTimer);
            flushTimer = null;
          }
          flushLogs();
        } else if (!flushTimer) {
          flushTimer = setTimeout(flushLogs, 200);
        }

        if (terminal) {
          setBusy(false);
          const root = projectRootRef.current;
          if (root) void scan(root);
        }
      })
      .then((stop) => (active ? (unlisten = stop) : stop()))
      .catch((error) => setNotice(`Không thể theo dõi tiến độ: ${describeError(error)}`));
    return () => {
      active = false;
      if (flushTimer) clearTimeout(flushTimer);
      if (progressTimer) clearTimeout(progressTimer);
      unlisten?.();
    };
    // Tauri listeners and initial config load once for the app lifetime.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    // Avoid smooth scrolling on every batch — can contribute to WebView jank.
    logEndRef.current?.scrollIntoView({ behavior: "auto", block: "end" });
  }, [logs]);

  const projectPlaylists = useMemo(
    () => config.playlists.filter((playlist) => playlist.projectRoot === config.projectRoot),
    [config.playlists, config.projectRoot],
  );

  const visibleVideos = useMemo(() => {
    const term = query.trim().toLocaleLowerCase("vi");
    const playlist = projectPlaylists.find((item) => item.id === activePlaylist);
    const list = videos.filter((video) =>
      (!term || video.relativePath.toLocaleLowerCase("vi").includes(term))
      && (!playlist || playlist.videoPaths.includes(video.relativePath)),
    );
    return list.sort((left, right) =>
      librarySort === "added"
        ? right.addedAt - left.addedAt || videoName(left).localeCompare(videoName(right), "vi")
        : videoName(left).localeCompare(videoName(right), "vi"),
    );
  }, [activePlaylist, librarySort, projectPlaylists, query, videos]);

  const videoGroups = useMemo(() => {
    if (libraryGroup === "none") return [{ label: "", videos: visibleVideos }];
    const groups = new Map<string, VideoInfo[]>();
    visibleVideos.forEach((video) => {
      const label = libraryGroup === "course"
        ? videoCourse(video)
        : libraryGroup === "date"
          ? formatAddedAt(video.addedAt)
          : (videoName(video).trim().charAt(0).toLocaleUpperCase("vi") || "#");
      groups.set(label, [...(groups.get(label) ?? []), video]);
    });
    return [...groups.entries()].map(([label, groupedVideos]) => ({ label, videos: groupedVideos }));
  }, [libraryGroup, visibleVideos]);

  const progressValue = progress
    ? progress.stage === "tts" && progress.current != null && progress.total
      ? 20 + Math.round((progress.current / progress.total) * 48)
      : stagePercent[progress.stage]
    : 0;

  const save = async () => {
    setSaving(true);
    setNotice("");
    try {
      const result = await api.saveConfig(config);
      setSavedConfig(config);
      const status = await api.ffmpegStatus();
      setFfmpeg(status);
      const scanned = await scan(config.projectRoot);
      if (scanned) {
        const created = result.createdDirs ?? [];
        setNotice(
          created.length
            ? `Đã lưu thiết lập và tạo thư mục: ${created.join(", ")}.`
            : "Đã lưu thiết lập. Cấu trúc assets/video, assets/srt, work, outputs đã sẵn sàng.",
        );
      }
      setView("library");
    } catch (error) {
      setNotice(`Không thể lưu thiết lập: ${describeError(error)}`);
    } finally {
      setSaving(false);
    }
  };

  const browse = async () => {
    try {
      const root = await api.pickDirectory();
      if (typeof root === "string") setConfig((current) => ({ ...current, projectRoot: root }));
    } catch (error) {
      setNotice(`Không thể mở hộp thoại: ${describeError(error)}`);
    }
  };

  const cancelSettings = () => {
    setConfig(savedConfig);
    setView("library");
  };

  const loadVoices = async () => {
    if (!config.azureKey.trim()) {
      setNotice("Nhập Azure Speech key trước khi tải danh sách giọng.");
      return;
    }
    setLoadingVoices(true);
    setNotice("");
    try {
      const list = await api.listVoices(config.azureKey, config.azureRegion);
      setVoices(list);
      const selected = config.azureVoice.trim() || DEFAULT_AZURE_VOICE;
      if (!list.some((voice) => voice.shortName === selected) && list.length) {
        // Keep user's ShortName even if not in list (e.g. region lag); just inform.
        setNotice(
          `Đã tải ${list.length} giọng. Giọng đang chọn (${selected}) có thể không có trong region này — kiểm tra lại trước khi dub.`,
        );
      } else {
        setNotice(`Đã tải ${list.length} giọng Azure.`);
      }
      if (!config.azureVoice.trim()) {
        setConfig((current) => ({ ...current, azureVoice: DEFAULT_AZURE_VOICE, azureVoiceStyle: "" }));
      } else {
        const voice = list.find((item) => item.shortName === config.azureVoice);
        if (voice && config.azureVoiceStyle && !voice.styles.includes(config.azureVoiceStyle)) {
          setConfig((current) => ({ ...current, azureVoiceStyle: "" }));
        }
      }
    } catch (error) {
      setNotice(`Không tải được danh sách giọng: ${describeError(error)}`);
    } finally {
      setLoadingVoices(false);
    }
  };

  const voiceLocales = useMemo(() => {
    const counts = new Map<string, number>();
    for (const voice of voices) {
      if (!voice.locale) continue;
      counts.set(voice.locale, (counts.get(voice.locale) ?? 0) + 1);
    }
    const fromData = [...counts.entries()]
      .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
      .map(([locale]) => locale);
    const preferred = POPULAR_LOCALES.filter((locale) => counts.has(locale));
    const preferredSet = new Set<string>(preferred);
    const rest = fromData.filter((locale) => !preferredSet.has(locale));
    return [...preferred, ...rest];
  }, [voices]);

  const activeVoiceFilterCount = useMemo(() => {
    let count = 0;
    if (voiceLocale !== "all") count += 1;
    if (voiceFeatures.multilingual) count += 1;
    if (voiceFeatures.hd) count += 1;
    if (voiceFeatures.female) count += 1;
    if (voiceFeatures.male) count += 1;
    if (voiceFeatures.gaOnly) count += 1;
    return count;
  }, [voiceLocale, voiceFeatures]);

  const selectedVoice = useMemo(
    () => voices.find((voice) => voice.shortName === (config.azureVoice || DEFAULT_AZURE_VOICE)),
    [voices, config.azureVoice],
  );

  const filteredVoices = useMemo(() => {
    const q = voiceQuery.trim().toLowerCase();
    let list = voices.filter((voice) => {
      if (voiceLocale !== "all" && !supportsVoiceLocale(voice, voiceLocale)) return false;
      if (voiceFeatures.multilingual && !isMultilingualVoice(voice)) return false;
      if (voiceFeatures.hd && !isHdVoice(voice)) return false;
      if (voiceFeatures.female && voice.gender.toLowerCase() !== "female") return false;
      if (voiceFeatures.male && voice.gender.toLowerCase() !== "male") return false;
      if (voiceFeatures.gaOnly && voice.status && voice.status.toUpperCase() !== "GA") return false;
      if (!q) return true;
      const hay = [
        voice.shortName,
        voice.displayName,
        voice.localName,
        voice.gender,
        voice.locale,
        voice.localeName,
        voice.status,
        voice.voiceType,
        voice.styles.join(" "),
        voice.secondaryLocales.join(" "),
      ]
        .join(" ")
        .toLowerCase();
      return hay.includes(q);
    });

    const dir = voiceSortDir === "asc" ? 1 : -1;
    list = [...list].sort((a, b) => {
      const nameA = (a.localName || a.displayName || a.shortName).toLocaleLowerCase("vi");
      const nameB = (b.localName || b.displayName || b.shortName).toLocaleLowerCase("vi");
      if (voiceSortKey === "locale") {
        return (
          a.locale.localeCompare(b.locale, "vi") * dir ||
          nameA.localeCompare(nameB, "vi") * dir
        );
      }
      if (voiceSortKey === "gender") {
        return (
          a.gender.localeCompare(b.gender, "vi") * dir ||
          nameA.localeCompare(nameB, "vi") * dir
        );
      }
      return nameA.localeCompare(nameB, "vi") * dir || a.shortName.localeCompare(b.shortName) * dir;
    });
    return list;
  }, [voices, voiceQuery, voiceLocale, voiceFeatures, voiceSortKey, voiceSortDir]);

  useEffect(() => {
    if (!voiceFilterOpen && !voiceSortOpen) return;
    const onPointerDown = (event: PointerEvent) => {
      const target = event.target as Node;
      if (voiceFilterOpen && voiceFilterRef.current && !voiceFilterRef.current.contains(target)) {
        setVoiceFilterOpen(false);
      }
      if (voiceSortOpen && voiceSortRef.current && !voiceSortRef.current.contains(target)) {
        setVoiceSortOpen(false);
      }
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setVoiceFilterOpen(false);
        setVoiceSortOpen(false);
      }
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [voiceFilterOpen, voiceSortOpen]);

  const clearVoiceFilters = () => {
    setVoiceLocale("all");
    setVoiceFeatures(EMPTY_VOICE_FEATURES);
    setVoiceQuery("");
  };

  const toggleVoiceFeature = (key: keyof VoiceFeatureFilter) => {
    setVoiceFeatures((current) => {
      const next = { ...current, [key]: !current[key] };
      // Female/Male exclusive when both would be on.
      if (key === "female" && next.female) next.male = false;
      if (key === "male" && next.male) next.female = false;
      return next;
    });
  };

  const cycleVoiceSortDir = () => {
    setVoiceSortDir((current) => (current === "asc" ? "desc" : "asc"));
  };

  const openPath = (path: string) => {
    void api.openPath(path).catch((error) => setNotice(`Không thể mở đường dẫn: ${describeError(error)}`));
  };

  const importYoutube = async () => {
    if (!youtubeUrl.trim() || importingYoutube) return;
    if (!config.projectRoot) {
      setNotice("Chọn và lưu thư mục dự án trước khi tải từ YouTube.");
      return;
    }
    setImportingYoutube(true);
    setNotice("");
    try {
      const result = await api.importYoutube(youtubeUrl.trim(), youtubeQuality);
      setYoutubeUrl("");
      setVideos(result.videos);
      setSelected(new Set([result.relativePath]));
      setNotice(`Đã tải ${result.relativePath} + SRT ${result.subtitleLanguage}. Có thể chọn Dub ngay.`);
    } catch (error) {
      setNotice(`Không thể tải từ YouTube: ${describeError(error)}`);
    } finally {
      setImportingYoutube(false);
    }
  };

  const savePlaylists = async (playlists: Playlist[]) => {
    const updated = { ...savedConfig, playlists };
    await api.saveConfig(updated);
    setConfig(updated);
    setSavedConfig(updated);
  };

  const createPlaylist = async () => {
    if (!config.projectRoot) return;
    const name = window.prompt("Tên playlist mới")?.trim();
    if (!name) return;
    const playlist: Playlist = {
      id: globalThis.crypto.randomUUID(),
      projectRoot: config.projectRoot,
      name,
      videoPaths: [],
    };
    try {
      await savePlaylists([...config.playlists, playlist]);
      setActivePlaylist(playlist.id);
      setNotice(`Đã tạo playlist “${name}”.`);
    } catch (error) {
      setNotice(`Không thể lưu playlist: ${describeError(error)}`);
    }
  };

  const addSelectedToPlaylist = async () => {
    const playlist = projectPlaylists.find((item) => item.id === activePlaylist);
    if (!playlist || !selected.size) return;
    const videoPaths = [...new Set([...playlist.videoPaths, ...selected])];
    try {
      await savePlaylists(config.playlists.map((item) => item.id === playlist.id ? { ...item, videoPaths } : item));
      setNotice(`Đã thêm ${selected.size} video vào “${playlist.name}”.`);
    } catch (error) {
      setNotice(`Không thể cập nhật playlist: ${describeError(error)}`);
    }
  };

  const closePlayer = () => {
    ++playRequestRef.current;
    setActiveVideo(null);
    setPlayback(null);
    setLoadingPlayback(false);
  };

  const playVideo = async (video: VideoInfo) => {
    const requestId = ++playRequestRef.current;
    setActiveVideo(video);
    setPlayback(null);
    setLoadingPlayback(true);
    try {
      const nextPlayback = await api.getVideoPlayback(video.relativePath);
      if (requestId === playRequestRef.current) setPlayback(nextPlayback);
    } catch (error) {
      if (requestId === playRequestRef.current) setNotice(`Không thể mở video: ${describeError(error)}`);
    } finally {
      if (requestId === playRequestRef.current) setLoadingPlayback(false);
    }
  };

  const renameVideo = async (video: VideoInfo) => {
    const filename = video.relativePath.split(/[\\/]/).at(-1) ?? "";
    const extension = filename.lastIndexOf(".");
    const currentStem = extension > 0 ? filename.slice(0, extension) : filename;
    const newStem = window.prompt("Tên mới của video (không gồm phần mở rộng)", currentStem)?.trim();
    if (!newStem || newStem === currentStem) return;
    try {
      const result = await api.renameVideo(video.relativePath, newStem);
      setVideos(result.videos);
      setSelected((current) => new Set([...current].filter((path) => path !== video.relativePath)));
      const newPath = video.relativePath.slice(0, -filename.length) + `${newStem}${filename.slice(extension)}`;
      await savePlaylists(config.playlists.map((playlist) => ({
        ...playlist,
        videoPaths: playlist.videoPaths.map((path) => path === video.relativePath ? newPath : path),
      })));
      if (activeVideo?.relativePath === video.relativePath) closePlayer();
      setNotice(`Đã đổi tên video${video.hasSrt ? " và SRT tương ứng" : ""}.`);
    } catch (error) {
      setNotice(`Không thể đổi tên video: ${describeError(error)}`);
    }
  };

  const deleteVideo = async (video: VideoInfo) => {
    if (!window.confirm(`Xóa ${video.relativePath}${video.hasSrt ? " và SRT tương ứng" : ""}?\n\nOutput đã dub sẽ được giữ lại.`)) return;
    try {
      const result = await api.deleteVideo(video.relativePath);
      setVideos(result.videos);
      setSelected((current) => new Set([...current].filter((path) => path !== video.relativePath)));
      await savePlaylists(config.playlists.map((playlist) => ({
        ...playlist,
        videoPaths: playlist.videoPaths.filter((path) => path !== video.relativePath),
      })));
      if (activeVideo?.relativePath === video.relativePath) closePlayer();
      setNotice(`Đã xóa video${video.hasSrt ? " và SRT tương ứng" : ""}.`);
    } catch (error) {
      setNotice(`Không thể xóa video: ${describeError(error)}`);
    }
  };

  const run = async (targets: string[]) => {
    if (!targets.length || busy) return;
    setBusy(true);
    setLogs([]);
    setProgress(null);
    setNotice("");
    try {
      await api.startDub(targets, force, forceTts, translate, naturalVoice, naturalMode, sourceTiming);
    } catch (error) {
      setBusy(false);
      setNotice(`Job thất bại: ${describeError(error)}`);
    }
  };

  const cancelJob = async () => {
    if (!busy) return;
    try {
      await api.cancelDub();
      setNotice("Đã gửi yêu cầu hủy. Job sẽ dừng giữa các video.");
    } catch (error) {
      setNotice(`Không thể hủy: ${describeError(error)}`);
    }
  };

  const toggleSelected = (path: string) => {
    setSelected((current) => {
      const next = new Set(current);
      next.has(path) ? next.delete(path) : next.add(path);
      return next;
    });
  };

  const toggleAllVisible = () => {
    const everyVisibleSelected = visibleVideos.every((video) => selected.has(video.relativePath));
    setSelected((current) => {
      const next = new Set(current);
      visibleVideos.forEach((video) =>
        everyVisibleSelected ? next.delete(video.relativePath) : next.add(video.relativePath),
      );
      return next;
    });
  };

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand" aria-label="Dubora">
          <img
            className="brand-logo"
            src={theme === "dark" ? duboraLogoDark : duboraLogoLight}
            alt="Dubora"
          />
          <span>Every story, in every voice.</span>
        </div>

        <nav className="nav" aria-label="Điều hướng chính">
          <button className={view === "hub" ? "active" : ""} onClick={() => setView("hub")}>
            <IconPlayerPlayFilled size={19} /> Dubora Hub
          </button>
          <button
            className={view === "library" ? "active" : ""}
            onClick={() => {
              setConfig(savedConfig);
              setView("library");
            }}
          >
            <IconLibrary size={19} /> Thư viện <span>{videos.length}</span>
          </button>
          <button className={view === "settings" ? "active" : ""} onClick={() => setView("settings")}>
            <IconSettings size={19} /> Thiết lập
          </button>
          <button className={view === "guide" ? "active" : ""} onClick={() => setView("guide")}>
            <IconBook size={19} /> Hướng dẫn
          </button>
        </nav>

        <div className="sidebar-meter" aria-label="Trạng thái hệ thống">
          <div className="meter-heading"><span>Hệ thống</span><IconTerminal2 size={16} /></div>
          <div className={`system-line ${ffmpeg?.ready ? "ok" : "warn"}`}>
            <span className="status-dot" /> FFmpeg + rubberband
            <b>{ffmpeg?.ready ? "OK" : "LỖI"}</b>
          </div>
          {ffmpeg?.path && <small data-tooltip={ffmpeg.path}>{ffmpeg.path}</small>}
        </div>

        <button
          className="theme-button"
          onClick={() => setTheme(theme === "dark" ? "light" : "dark")}
          aria-label={theme === "dark" ? "Bật giao diện sáng" : "Bật giao diện tối"}
        >
          {theme === "dark" ? <IconSun size={18} /> : <IconMoon size={18} />}
          {theme === "dark" ? "Giao diện sáng" : "Giao diện tối"}
        </button>
      </aside>

      <main>
        <header className="topbar">
          <div className="breadcrumb" aria-live="polite">
            {view === "hub" ? "Dubora Hub" : view === "library" ? "Thư viện" : view === "settings" ? "Thiết lập" : "Hướng dẫn"}
          </div>
          <div className="root-chip" data-tooltip={config.projectRoot || "Chưa chọn project root"}>
            <IconFolder size={16} />
            <span>{config.projectRoot || "Chưa chọn project root"}</span>
          </div>
        </header>

        {notice && (
          <div className="notice" role="status">
            <IconAlertTriangle size={18} /> <span>{notice}</span>
            <button onClick={() => setNotice("")} aria-label="Đóng thông báo"><IconX size={16} /></button>
          </div>
        )}

        {view === "guide" ? (
          <section className="guide-page page-enter" aria-label="Hướng dẫn">
            <p className="page-lead">
              Tạo video lồng tiếng Việt từ file video + phụ đề SRT khớp nhau. Giọng mặc định:
              Florian Dragon HD (đổi trong Thiết lập).
            </p>

            <div className="guide-grid">
              <article className="panel guide-card full">
                <div className="panel-title">
                  <span className="number">01</span>
                  <div>
                    <h2>Quy trình nhanh</h2>
                    <p className="lead">Làm theo thứ tự để dub được video đầu tiên.</p>
                  </div>
                </div>
                <ol className="guide-steps">
                  <li>
                    <div>
                      <strong>Chuẩn bị thư mục dự án</strong>
                      <span>
                        Chọn một folder bất kỳ (ví dụ <code>D:\Courses\my-course</code>). Khi lưu
                        Thiết lập, app tự tạo <code>assets/video</code>, <code>assets/srt</code>,{" "}
                        <code>work</code>, <code>outputs</code> nếu chưa có.
                      </span>
                    </div>
                  </li>
                  <li>
                    <div>
                      <strong>Lấy Azure Speech key</strong>
                      <span>
                        Tạo resource Azure Speech trên portal Microsoft, copy Key và Region vào
                        Thiết lập.
                      </span>
                    </div>
                  </li>
                  <li>
                    <div>
                      <strong>Mở Thiết lập trong app</strong>
                      <span>
                        Chọn project root, dán Azure key + region (mặc định{" "}
                        <code>southeastasia</code>), bấm Lưu.
                      </span>
                    </div>
                  </li>
                  <li>
                    <div>
                      <strong>Thêm Gemini key nếu cần dịch hoặc Natural Voice</strong>
                      <span>
                        Tạo key ở Google AI Studio, dán vào mục Gemini API keys trong Thiết lập.
                        Key này cần cho <strong>Dịch bằng Gemini</strong> và <strong>Natural Voice</strong>.
                      </span>
                    </div>
                  </li>
                  <li>
                    <div>
                      <strong>Quét thư viện và Dub</strong>
                      <span>
                        Vào Thư viện → chọn video đã có SRT → bật tùy chọn nếu cần → bấm{" "}
                        <strong>Dub</strong>. Kết quả nằm trong <code>outputs/</code>.
                      </span>
                    </div>
                  </li>
                </ol>
                <button type="button" className="guide-link" onClick={() => setView("settings")}>
                  Đi tới Thiết lập <IconArrowRight size={16} />
                </button>
              </article>

              <article className="panel guide-card full">
                <div className="panel-title">
                  <span className="number">02</span>
                  <div>
                    <h2>Cấu trúc thư mục (bắt buộc)</h2>
                    <p className="lead">
                      Video và SRT phải cùng đường dẫn tương đối (basename giống nhau, khác phần
                      mở rộng). Sơ đồ dưới đây là layout chuẩn app hiểu được.
                    </p>
                  </div>
                </div>
                <ProjectStructureDiagram />
                <div className="guide-tip">
                  <strong>Khớp SRT:</strong> app thử{" "}
                  <code>assets/srt/&lt;đường-dẫn-giống-video&gt;.srt</code> trước; nếu không có thì
                  thử prefix <code>CC-</code> ở tên khóa học (ví dụ{" "}
                  <code>CC-Khoa-hoc/...</code>). Định dạng video hỗ trợ:{" "}
                  <code>.mp4</code>, <code>.mov</code>, <code>.mkv</code>, <code>.webm</code>.
                </div>
              </article>

              <article className="panel guide-card">
                <div className="panel-title">
                  <span className="number">03</span>
                  <div>
                    <h2>Tải từ YouTube</h2>
                    <p className="lead">Tự tải video và một SRT khớp tên vào thư viện.</p>
                  </div>
                </div>
                <ol>
                  <li>Chọn và lưu Project root trong Thiết lập trước.</li>
                  <li>Vào <strong>Thư viện</strong>, dán link <code>youtube.com</code> hoặc <code>youtu.be</code> vào ô vàng ở đầu màn hình.</li>
                  <li>Bấm <strong>Tải từ YouTube</strong>. Video được lưu vào <code>assets/video</code>; SRT được lưu vào <code>assets/srt</code> với cùng tên để sẵn sàng Dub.</li>
                </ol>
                <div className="guide-tip">
                  <strong>Ưu tiên phụ đề:</strong> English → ngôn ngữ châu Âu → Chinese → Portuguese
                  → Japanese → Korean → Indonesian → Hindi → Arabic → ngôn ngữ khác. App chỉ tải
                  một ngôn ngữ đầu tiên có sẵn.
                </div>
              </article>

              <article className="panel guide-card">
                <div className="panel-title">
                  <span className="number">04</span>
                  <div>
                    <h2>Lấy Azure Speech key</h2>
                    <p className="lead">Key dùng để sinh giọng nói từng câu phụ đề.</p>
                  </div>
                </div>
                <ol>
                  <li>
                    Đăng nhập{" "}
                    <ExternalLink href="https://portal.azure.com">
                      Azure Portal <IconExternalLink size={14} />
                    </ExternalLink>
                    .
                  </li>
                  <li>
                    Tạo resource <strong>Speech</strong> (hoặc Cognitive Services / AI Services có
                    Speech).
                  </li>
                  <li>
                    Vào resource → <strong>Keys and Endpoint</strong> → copy <strong>KEY 1</strong>{" "}
                    và <strong>Location/Region</strong> (ví dụ <code>southeastasia</code>,{" "}
                    <code>eastasia</code>).
                  </li>
                  <li>
                    Dán vào app: menu <strong>Thiết lập</strong> → Azure Speech key + Region →{" "}
                    <strong>Lưu thiết lập</strong>.
                  </li>
                </ol>
                <div className="guide-tip">
                  <strong>Bảo mật:</strong> key chỉ lưu cục bộ trên máy bạn (thư mục config app).
                  Không chia sẻ key hay commit lên Git.
                </div>
              </article>

              <article className="panel guide-card">
                <div className="panel-title">
                  <span className="number">05</span>
                  <div>
                    <h2>Lấy Gemini API key</h2>
                    <p className="lead">Cần cho dịch SRT tiếng Anh và Natural Voice.</p>
                  </div>
                </div>
                <ol>
                  <li>
                    Mở{" "}
                    <ExternalLink href="https://aistudio.google.com/app/apikey">
                      Google AI Studio <IconExternalLink size={14} />
                    </ExternalLink>
                    {" "}và đăng nhập tài khoản Google.
                  </li>
                  <li>
                    Chọn <strong>Create API key</strong>, sau đó copy key vừa tạo.
                  </li>
                  <li>
                    Trong Dubora, vào <strong>Thiết lập</strong> → <strong>Google AI Studio</strong>
                    → dán key vào <strong>Gemini API keys</strong> → <strong>Lưu thiết lập</strong>.
                  </li>
                  <li>
                    Có thể dán nhiều key, mỗi key một dòng hoặc phân cách bằng dấu phẩy. Dubora
                    xoay vòng key theo từng request.
                  </li>
                </ol>
                <div className="guide-tip">
                  <strong>Khi dùng:</strong> quay lại Thư viện, bật <strong>Dịch bằng Gemini</strong>
                  {" "}hoặc <strong>Natural Voice</strong> trước khi bấm Dub. App dùng
                  {" "}<code>gemini-3.5-flash-lite</code> để dịch, chỉnh câu SRT và lập Speech Plan.
                </div>
              </article>

              <article className="panel guide-card">
                <div className="panel-title">
                  <span className="number">06</span>
                  <div>
                    <h2>Natural Voice</h2>
                    <p className="lead">Tạo cách nói tự nhiên hơn nhưng vẫn tôn trọng timing video.</p>
                  </div>
                </div>
                <ol>
                  <li>Thêm ít nhất một Gemini API key trong <strong>Thiết lập</strong>.</li>
                  <li>Vào <strong>Thư viện</strong> và bật <strong>Natural Voice</strong>.</li>
                  <li>Chọn nhịp: <strong>Đồng bộ sát</strong>, <strong>Cân bằng</strong> (mặc định), hoặc <strong>Tự nhiên tối đa</strong>.</li>
                </ol>
                <div className="guide-tip">
                  <strong>Cách Dubora xử lý:</strong> Gemini tạo Speech Plan (cách nói, điểm nhấn,
                  ngắt câu), Azure chỉ nhận SSML mà voice đang chọn hỗ trợ, rồi Dubora đo thời lượng
                  audio và anchor nội bộ khi voice hỗ trợ timing events. Chế độ <strong>Cân bằng</strong>
                  cho phép dịch chuyển một block giải thích tối đa 250 ms trong cùng sync island;
                  action anchor vẫn phải nằm trong cửa sổ 250 ms. Dubora đo toàn island trước khi
                  chọn đúng một nhánh bổ sung cho mỗi whole utterance: timing repair hoặc một concise
                  revision theo batch rồi synthesize lại toàn utterance đúng một lần. Nếu vẫn không
                  đạt, video chuyển sang <strong>reviewRequired</strong> và không xuất đè output cũ.
                  Aligned path không tách utterance thành các file segment, không có fallback 1.35×,
                  không chồng lời và không cắt cụt. Work artifact sẽ ghi rõ timing mode:
                  <strong>granular</strong>, <strong>captionDerived</strong> hoặc <strong>cueOnly</strong>.
                </div>
                <div className="guide-tip">
                  <strong>Nếu cần kiểm tra:</strong> Dubora ghi timeline nguồn, manifest, báo cáo timing
                  cần review và bộ đếm request/cache không chứa key vào thư mục <code>work/</code>.
                  Gemini chỉ đề xuất câu nói; Rust giữ quyền quyết định timestamp, anchor và SSML.
                  Manifest <strong>reviewRequired</strong> sẽ dừng trước FFmpeg để không ghi đè output cũ;
                  progress và <code>sync_report.json</code> nêu entry cùng khoảng <code>mm:ss.mmm</code>
                  cần nghe/xem lại.
                </div>
                <div className="guide-tip">
                  <strong>Nguồn timing:</strong> desktop mặc định dùng <strong>captionDerived</strong>
                  {" "}hoặc <strong>cueOnly</strong>. Khi cần căn theo từng từ, bật
                  <strong>Azure Fast granular</strong> trong nhóm Natural Voice; app sẽ trích audio
                  nguồn và lấy word timestamps trước khi dựng giọng.
                </div>
                <div className="guide-tip">
                  <strong>Chọn voice:</strong> sau khi bấm <strong>Tải danh sách giọng</strong> trong
                  Thiết lập, app hiển thị khả năng nhấn mạnh, nhịp/độ cao, ngắt câu và phong cách.
                  Với Dragon HD, Azure tự điều khiển delivery; vì voice này không trả timing events,
                  Dubora dùng per-segment Natural Voice path và không tuyên bố semantic-anchor alignment.
                </div>
              </article>

              <article className="panel guide-card">
                <div className="panel-title">
                  <span className="number">07</span>
                  <div>
                    <h2>Dùng màn Thư viện</h2>
                    <p className="lead">Theo dõi video, SRT và output; chạy job dub.</p>
                  </div>
                </div>
                <ul>
                  <li>
                    <strong>Quét lại</strong> — làm mới danh sách sau khi thêm file.
                  </li>
                  <li>
                    <strong>Nút Phát</strong> trên từng hàng — mở video trong app, kèm SRT khớp tên.
                  </li>
                  <li>
                    <strong>Danh sách / lưới</strong>, sắp xếp theo tên hoặc ngày thêm; nhóm theo tên,
                    ngày hoặc khóa học (folder cấp một trong <code>assets/video</code>).
                  </li>
                  <li>
                    Cột <strong>SRT</strong> / <strong>Output</strong> — biết file nào sẵn sàng
                    dub / đã có bản lồng tiếng.
                  </li>
                  <li>
                    Chọn một hoặc nhiều video → <strong>Dub</strong>.
                  </li>
                  <li>
                    <strong>Xử lý lại</strong> — force dù output đã mới hơn nguồn.
                  </li>
                  <li>
                    <strong>Sinh lại TTS</strong> — xóa cache giọng và gọi Azure lại (tốn quota).
                  </li>
                  <li>
                    <strong>Mở outputs</strong> — mở folder chứa file{" "}
                    <code>*.vi-dub.mp4</code>.
                  </li>
                  <li>
                    Job monitor bên phải — theo dõi clean → TTS → phối âm → đóng gói.
                  </li>
                </ul>
                <button type="button" className="guide-link" onClick={() => setView("library")}>
                  Mở Thư viện <IconArrowRight size={16} />
                </button>
              </article>

              <article className="panel guide-card">
                <div className="panel-title">
                  <span className="number">08</span>
                  <div>
                    <h2>Dubora Hub</h2>
                    <p className="lead">Không gian xem và tổ chức video trong project.</p>
                  </div>
                </div>
                <ul>
                  <li><strong>Phát video:</strong> dùng player tích hợp, tự nạp SRT trùng tên nếu có.</li>
                  <li><strong>Tốc độ và toàn màn hình:</strong> chọn 0.5×–2× dưới player; nút góc phải đưa toàn bộ player vào toàn màn hình.</li>
                  <li><strong>Playlist:</strong> tạo playlist, chọn nó, rồi chọn video ở Thư viện và bấm <strong>Thêm</strong>. Playlist chỉ lưu cục bộ theo Project root.</li>
                  <li><strong>Khóa học:</strong> để video trong <code>assets/video/Ten-khoa-hoc/</code> để nhóm theo khóa học.</li>
                </ul>
                <button type="button" className="guide-link" onClick={() => setView("hub")}>
                  Mở Dubora Hub <IconArrowRight size={16} />
                </button>
              </article>

              <article className="panel guide-card">
                <div className="panel-title">
                  <span className="number">09</span>
                  <div>
                    <h2>Tác giả</h2>
                    <p className="lead">Thông tin liên hệ và mã nguồn.</p>
                  </div>
                </div>
                <ul>
                  <li>
                    <strong>Tác giả:</strong> Hoàng Chiều Nguyễn Tuấn
                  </li>
                  <li>
                    <strong>Email:</strong> <code>tuan@hoangtuan.dev</code>
                  </li>
                  <li>
                    <strong>GitHub:</strong>{" "}
                    <ExternalLink href="https://github.com/hoangtuan2k5">
                      hoangtuan2k5 <IconExternalLink size={14} />
                    </ExternalLink>
                  </li>
                  <li>
                    <strong>Repo:</strong>{" "}
                    <ExternalLink href="https://github.com/hoangtuan2k5/dubora">
                      hoangtuan2k5/dubora <IconExternalLink size={14} />
                    </ExternalLink>
                  </li>
                </ul>
              </article>

              <article className="panel guide-card">
                <div className="panel-title">
                  <span className="number">10</span>
                  <div>
                    <h2>Yêu cầu hệ thống &amp; lưu ý</h2>
                    <p className="lead">Những điều cần biết trước khi chạy batch lớn.</p>
                  </div>
                </div>
                <ul>
                  <li>
                    <strong>FFmpeg + rubberband:</strong> bản cài Windows bundle sẵn. Sidebar hiển
                    thị READY khi engine ổn.
                  </li>
                  <li>
                    <strong>Phụ đề mặc định là tiếng Việt.</strong> Với SRT tiếng Anh, thêm Gemini
                    API key trong Thiết lập rồi bật <strong>Dịch bằng Gemini</strong> khi chạy.
                  </li>
                  <li>
                    <strong>Natural Voice là opt-in.</strong> Cần Gemini API key; mặc định vẫn là
                    pipeline Azure nhanh và ổn định như trước.
                  </li>
                  <li>
                    <strong>Cache TTS:</strong> segment đã sinh sẽ được tái dùng nếu text/giọng
                    không đổi — re-run course thường nhanh hơn lần đầu.
                  </li>
                  <li>
                    <strong>Hủy job:</strong> khi đang chạy, bấm <strong>Hủy job</strong> — app dừng
                    giữa các video (không cắt ngang giữa segment TTS đang gọi API).
                  </li>
                  <li>
                    <strong>Tên folder đặc biệt:</strong> hỗ trợ khoảng trắng và dấu{" "}
                    <code>[ ]</code> (kiểu tên khóa Udemy).
                  </li>
                </ul>
                <div className="guide-tip">
                  <strong>Lỗi thường gặp:</strong> “Thiếu SRT” → kiểm tra đường dẫn khớp video;
                  “Missing AZURE_KEY” → lưu key trong Thiết lập; FFmpeg CHECK → cài lại bằng
                  installer chính thức hoặc kiểm tra lại engine trong Thiết lập.
                </div>
              </article>

              <p className="guide-footer">
                Dubora v0.1.0 · © 2026 Hoàng Chiều Nguyễn Tuấn · tuan@hoangtuan.dev ·{" "}
                github.com/hoangtuan2k5
              </p>
            </div>
          </section>
        ) : view === "hub" ? (
          <section className="hub-page page-enter" aria-label="Dubora Hub">
            <div className="hub-layout">
              <article className="panel hub-intro">
                <div>
                  <span className="eyebrow">DUBORA HUB</span>
                  <h1>Xem, sắp xếp, tiếp tục.</h1>
                  <p>Trình phát nội bộ dùng video và SRT trong project hiện tại.</p>
                </div>
                <div className="hub-playlist-actions">
                  <label className="sr-only" htmlFor="hub-playlist">Playlist đang xem</label>
                  <select id="hub-playlist" value={activePlaylist} onChange={(event) => setActivePlaylist(event.target.value)}>
                    <option value="all">Toàn bộ thư viện</option>
                    {projectPlaylists.map((playlist) => <option key={playlist.id} value={playlist.id}>{playlist.name}</option>)}
                  </select>
                  <button className="secondary-button" onClick={() => void createPlaylist()} disabled={!config.projectRoot}><IconPlus size={18} /> Playlist</button>
                  <button className="secondary-button" onClick={() => void addSelectedToPlaylist()} disabled={activePlaylist === "all" || !selected.size}><IconPlaylistAdd size={18} /> Thêm đã chọn</button>
                </div>
              </article>

              <VideoPlayer video={activeVideo} playback={playback} loading={loadingPlayback} onClose={closePlayer} />

              <article className="panel hub-queue">
                <div className="panel-title">
                  <span className="number">{String(visibleVideos.length).padStart(2, "0")}</span>
                  <div><h2>{activePlaylist === "all" ? "Video trong thư viện" : projectPlaylists.find((item) => item.id === activePlaylist)?.name}</h2><p>Chọn một video để phát ngay.</p></div>
                </div>
                <div className="hub-video-list">
                  {visibleVideos.map((video) => (
                    <button key={video.relativePath} className={activeVideo?.relativePath === video.relativePath ? "hub-video active" : "hub-video"} onClick={() => void playVideo(video)}>
                      <span className="hub-video-icon"><IconPlayerPlayFilled size={17} /></span>
                      <span><strong>{videoName(video)}</strong><small>{videoCourse(video)} · {formatAddedAt(video.addedAt)}</small></span>
                      <span className={`badge ${video.hasSrt ? "success" : "danger"}`}>{video.hasSrt ? "SRT" : "Thiếu SRT"}</span>
                    </button>
                  ))}
                  {!visibleVideos.length && <div className="player-empty"><IconVideo size={28} /><p>Playlist này chưa có video.</p></div>}
                </div>
              </article>
            </div>
          </section>
        ) : view === "settings" ? (
          <section className="settings-page page-enter" aria-label="Thiết lập">
            <div className="settings-grid">
              <article className="panel config-panel">
                <div className="panel-title"><span className="number">01</span><div><h2>Thư mục dự án</h2><p>Nơi chứa assets, work và outputs.</p></div></div>
                <label htmlFor="project-root">Project root</label>
                <div className="input-action">
                  <div className="input-wrap"><IconFolder size={18} /><input id="project-root" value={config.projectRoot} onChange={(event) => setConfig({ ...config, projectRoot: event.target.value })} placeholder="D:\Courses\dubora" /></div>
                  <button className="secondary-button" onClick={browse}><IconFolderOpen size={18} /> Chọn thư mục</button>
                </div>
                <div className="path-preview"><span>INPUT</span> {config.projectRoot ? `${config.projectRoot}/assets/video` : "—"}</div>
                <div className="path-preview"><span>OUTPUT</span> {config.projectRoot ? `${config.projectRoot}/outputs` : "—"}</div>
              </article>

              <article className="panel config-panel">
                <div className="panel-title"><span className="number">02</span><div><h2>Azure Speech</h2><p>Khóa được lưu cục bộ trên máy.</p></div></div>
                <label htmlFor="azure-key">Azure Speech key</label>
                <div className="input-wrap"><IconKey size={18} /><input id="azure-key" type={showKey ? "text" : "password"} value={config.azureKey} onChange={(event) => setConfig({ ...config, azureKey: event.target.value })} placeholder="Nhập khóa Azure" autoComplete="off" /><button className="icon-button" onClick={() => setShowKey(!showKey)} aria-label={showKey ? "Ẩn Azure key" : "Hiện Azure key"}>{showKey ? <IconEyeOff size={18} /> : <IconEye size={18} />}</button></div>
                <label htmlFor="azure-region">Region</label>
                <div className="input-wrap"><IconBrandAzure size={18} /><input id="azure-region" value={config.azureRegion} onChange={(event) => setConfig({ ...config, azureRegion: event.target.value })} placeholder="southeastasia" /></div>

                <label htmlFor="azure-voice">Giọng lồng tiếng</label>
                <div className="voice-toolbar">
                  <button
                    type="button"
                    className="secondary-button"
                    onClick={() => void loadVoices()}
                    disabled={loadingVoices || !config.azureKey.trim()}
                  >
                    {loadingVoices ? <IconLoader2 className="spin" size={18} /> : <IconRefresh size={18} />}
                    Tải danh sách giọng
                  </button>
                  <button
                    type="button"
                    className="text-button"
                    onClick={() => setConfig((current) => ({ ...current, azureVoice: DEFAULT_AZURE_VOICE, azureVoiceStyle: "" }))}
                  >
                    Mặc định Florian HD
                  </button>
                </div>
                {voices.length > 0 ? (
                  <>
                    <div className="voice-tools">
                      <div className="input-wrap voice-search">
                        <IconSearch size={18} />
                        <input
                          id="voice-filter"
                          type="search"
                          value={voiceQuery}
                          onChange={(event) => setVoiceQuery(event.target.value)}
                          placeholder="Tìm tên giọng…"
                          aria-label="Tìm giọng"
                        />
                      </div>

                      <div className="voice-menu" ref={voiceFilterRef}>
                        <button
                          type="button"
                          className={`tool-button${voiceFilterOpen || activeVoiceFilterCount ? " active" : ""}`}
                          aria-expanded={voiceFilterOpen}
                          aria-haspopup="dialog"
                          onClick={() => {
                            setVoiceFilterOpen((open) => !open);
                            setVoiceSortOpen(false);
                          }}
                        >
                          <IconFilter size={17} />
                          Lọc
                          {activeVoiceFilterCount > 0 && (
                            <span className="tool-badge">{activeVoiceFilterCount}</span>
                          )}
                        </button>
                        {voiceFilterOpen && (
                          <div className="voice-popover" role="dialog" aria-label="Bộ lọc giọng">
                            <div className="voice-popover-head">
                              <strong>Bộ lọc</strong>
                              <button type="button" className="text-button" onClick={clearVoiceFilters}>
                                Xóa lọc
                              </button>
                            </div>

                            <p className="voice-popover-label">Ngôn ngữ</p>
                            <div className="filter-chips">
                              <button
                                type="button"
                                className={`filter-chip${voiceLocale === "all" ? " on" : ""}`}
                                onClick={() => setVoiceLocale("all")}
                              >
                                Tất cả
                              </button>
                              {voiceLocales.slice(0, 12).map((locale) => (
                                <button
                                  key={locale}
                                  type="button"
                                  className={`filter-chip${voiceLocale === locale ? " on" : ""}`}
                                  onClick={() => setVoiceLocale(locale)}
                                >
                                  {locale}
                                </button>
                              ))}
                            </div>

                            <p className="voice-popover-label">Tính năng</p>
                            <div className="filter-chips">
                              <button
                                type="button"
                                className={`filter-chip${voiceFeatures.multilingual ? " on" : ""}`}
                                onClick={() => toggleVoiceFeature("multilingual")}
                              >
                                Multilingual
                              </button>
                              <button
                                type="button"
                                className={`filter-chip${voiceFeatures.hd ? " on" : ""}`}
                                onClick={() => toggleVoiceFeature("hd")}
                              >
                                HD / Dragon
                              </button>
                              <button
                                type="button"
                                className={`filter-chip${voiceFeatures.female ? " on" : ""}`}
                                onClick={() => toggleVoiceFeature("female")}
                              >
                                Nữ
                              </button>
                              <button
                                type="button"
                                className={`filter-chip${voiceFeatures.male ? " on" : ""}`}
                                onClick={() => toggleVoiceFeature("male")}
                              >
                                Nam
                              </button>
                              <button
                                type="button"
                                className={`filter-chip${voiceFeatures.gaOnly ? " on" : ""}`}
                                onClick={() => toggleVoiceFeature("gaOnly")}
                              >
                                Chỉ GA
                              </button>
                            </div>
                          </div>
                        )}
                      </div>

                      <div className="voice-menu" ref={voiceSortRef}>
                        <button
                          type="button"
                          className={`tool-button${voiceSortOpen ? " active" : ""}`}
                          aria-expanded={voiceSortOpen}
                          aria-haspopup="menu"
                          data-tooltip="Sắp xếp — bấm mũi tên để đảo tăng/giảm"
                          onClick={() => {
                            setVoiceSortOpen((open) => !open);
                            setVoiceFilterOpen(false);
                          }}
                        >
                          {voiceSortDir === "asc" ? (
                            <IconSortAscendingLetters size={17} />
                          ) : (
                            <IconSortDescendingLetters size={17} />
                          )}
                          {voiceSortLabel(voiceSortKey)}
                          <span className="sort-dir">{voiceSortDir === "asc" ? "A→Z" : "Z→A"}</span>
                        </button>
                        <button
                          type="button"
                          className="tool-button icon-only"
                          aria-label={
                            voiceSortDir === "asc" ? "Đảo sang giảm dần" : "Đảo sang tăng dần"
                          }
                          data-tooltip="Đảo tăng/giảm dần"
                          onClick={cycleVoiceSortDir}
                        >
                          {voiceSortDir === "asc" ? (
                            <IconSortAscendingLetters size={17} />
                          ) : (
                            <IconSortDescendingLetters size={17} />
                          )}
                        </button>
                        {voiceSortOpen && (
                          <div className="voice-popover sort-popover" role="menu" aria-label="Sắp xếp giọng">
                            <p className="voice-popover-label">Xếp theo</p>
                            {([
                              ["name", "Tên (A–Z)"],
                              ["locale", "Ngôn ngữ"],
                              ["gender", "Giới tính"],
                            ] as const).map(([key, label]) => (
                              <button
                                key={key}
                                type="button"
                                role="menuitemradio"
                                aria-checked={voiceSortKey === key}
                                className={`sort-option${voiceSortKey === key ? " on" : ""}`}
                                onClick={() => {
                                  if (voiceSortKey === key) {
                                    cycleVoiceSortDir();
                                  } else {
                                    setVoiceSortKey(key);
                                    setVoiceSortDir("asc");
                                  }
                                  setVoiceSortOpen(false);
                                }}
                              >
                                <span>{label}</span>
                                {voiceSortKey === key && (
                                  <em>{voiceSortDir === "asc" ? "Tăng dần" : "Giảm dần"}</em>
                                )}
                              </button>
                            ))}
                            <p className="voice-popover-note">
                              Bấm lại cùng tiêu chí hoặc nút mũi tên để đảo tăng ↔ giảm.
                            </p>
                          </div>
                        )}
                      </div>
                    </div>

                    {activeVoiceFilterCount > 0 && (
                      <div className="active-filters" aria-label="Bộ lọc đang bật">
                        {voiceLocale !== "all" && (
                          <button type="button" className="active-chip" onClick={() => setVoiceLocale("all")}>
                            {voiceLocale} <IconX size={13} />
                          </button>
                        )}
                        {voiceFeatures.multilingual && (
                          <button type="button" className="active-chip" onClick={() => toggleVoiceFeature("multilingual")}>
                            Multilingual <IconX size={13} />
                          </button>
                        )}
                        {voiceFeatures.hd && (
                          <button type="button" className="active-chip" onClick={() => toggleVoiceFeature("hd")}>
                            HD <IconX size={13} />
                          </button>
                        )}
                        {voiceFeatures.female && (
                          <button type="button" className="active-chip" onClick={() => toggleVoiceFeature("female")}>
                            Nữ <IconX size={13} />
                          </button>
                        )}
                        {voiceFeatures.male && (
                          <button type="button" className="active-chip" onClick={() => toggleVoiceFeature("male")}>
                            Nam <IconX size={13} />
                          </button>
                        )}
                        {voiceFeatures.gaOnly && (
                          <button type="button" className="active-chip" onClick={() => toggleVoiceFeature("gaOnly")}>
                            GA <IconX size={13} />
                          </button>
                        )}
                      </div>
                    )}

                    <div className="input-wrap select-wrap">
                      <IconWaveSine size={18} />
                      <select
                        id="azure-voice"
                        value={config.azureVoice || DEFAULT_AZURE_VOICE}
                        onChange={(event) =>
                          setConfig((current) => ({ ...current, azureVoice: event.target.value, azureVoiceStyle: "" }))
                        }
                      >
                        {!filteredVoices.some((v) => v.shortName === (config.azureVoice || DEFAULT_AZURE_VOICE)) && (
                          <option value={config.azureVoice || DEFAULT_AZURE_VOICE}>
                            {config.azureVoice || DEFAULT_AZURE_VOICE} (đang chọn)
                          </option>
                        )}
                        {filteredVoices.map((voice) => (
                          <option key={voice.shortName} value={voice.shortName}>
                            {voice.localName || voice.displayName} · {voice.gender} · {voice.locale}
                            {isMultilingualVoice(voice) ? " · Multi" : ""}
                            {isHdVoice(voice) ? " · HD" : ""}
                            {voice.status && voice.status !== "GA" ? ` · ${voice.status}` : ""} — {voice.shortName}
                          </option>
                        ))}
                      </select>
                    </div>
                    <p className="field-hint">
                      {filteredVoices.length}/{voices.length} giọng · ShortName:{" "}
                      <code>{config.azureVoice || DEFAULT_AZURE_VOICE}</code>
                    </p>
                  </>
                ) : (
                  <>
                    <div className="input-wrap">
                      <IconWaveSine size={18} />
                      <input
                        id="azure-voice"
                        value={config.azureVoice || DEFAULT_AZURE_VOICE}
                        onChange={(event) =>
                          setConfig((current) => ({ ...current, azureVoice: event.target.value, azureVoiceStyle: "" }))
                        }
                        placeholder={DEFAULT_AZURE_VOICE}
                        spellCheck={false}
                      />
                    </div>
                    <p className="field-hint">
                      Mặc định: <code>{DEFAULT_AZURE_VOICE}</code>. Bấm “Tải danh sách giọng” để chọn từ Azure.
                    </p>
                  </>
                )}
                <label htmlFor="azure-voice-style">Tông giọng</label>
                {selectedVoice?.styles.length ? (
                  <>
                    <div className="input-wrap select-wrap">
                      <IconWaveSine size={18} />
                      <select
                        id="azure-voice-style"
                        value={config.azureVoiceStyle}
                        onChange={(event) =>
                          setConfig((current) => ({ ...current, azureVoiceStyle: event.target.value }))
                        }
                      >
                        <option value="">Mặc định</option>
                        {selectedVoice.styles.map((style) => <option key={style} value={style}>{style}</option>)}
                      </select>
                    </div>
                    <p className="field-hint">Chỉ áp dụng khi Azure hỗ trợ tone này cho giọng đã chọn.</p>
                  </>
                ) : (
                  <p className="field-hint">Tải danh sách giọng để xem các tone được Azure hỗ trợ.</p>
                )}
                {selectedVoice && (
                  <p className="field-hint voice-capabilities">
                    Natural Voice: timing {selectedVoice.capabilities.timingEvents}, nhấn mạnh {selectedVoice.capabilities.manualEmphasis}, nhịp/độ cao {selectedVoice.capabilities.manualProsody}, ngắt câu {selectedVoice.capabilities.semanticBreak}, phong cách {selectedVoice.capabilities.style}.
                  </p>
                )}
              </article>

              <article className="panel config-panel">
                <div className="panel-title"><span className="number">03</span><div><h2>Google AI Studio</h2><p>Cần cho dịch phụ đề và Natural Voice.</p></div></div>
                <label htmlFor="gemini-keys">Gemini API keys</label>
                <div className="gemini-key-editor">
                  <div className="gemini-key-editor-head">
                    <span><IconKey size={16} /> Key pool</span>
                    <button
                      type="button"
                      className="gemini-key-visibility"
                      onClick={() => setShowGeminiKeys((current) => !current)}
                      aria-label={showGeminiKeys ? "Ẩn Gemini API keys" : "Hiện Gemini API keys"}
                      title={showGeminiKeys ? "Ẩn Gemini API keys" : "Hiện Gemini API keys"}
                    >
                      <small>Round Robin</small>
                      {showGeminiKeys ? <IconEyeOff size={16} /> : <IconEye size={16} />}
                    </button>
                  </div>
                  <textarea
                    id="gemini-keys"
                    className={showGeminiKeys ? undefined : "secret-value"}
                    value={config.geminiKeys}
                    onChange={(event) => setConfig({ ...config, geminiKeys: event.target.value })}
                    placeholder="Dán một key mỗi dòng, hoặc phân cách bằng dấu phẩy"
                    rows={5}
                    autoComplete="off"
                    spellCheck={false}
                  />
                </div>
                <p className="field-hint">Hiển thị 5 key; thêm nhiều hơn thì cuộn xuống. Dùng <code>gemini-3.5-flash-lite</code>; key được xoay theo từng batch.</p>
              </article>

              <article className="panel status-panel">
                <div className="panel-title"><span className="number">04</span><div><h2>FFmpeg engine</h2><p>Yêu cầu filter rubberband.</p></div></div>
                <div className={`engine-status ${ffmpeg?.ready ? "ready" : "missing"}`}>
                  {ffmpeg?.ready ? <IconCircleCheckFilled size={28} /> : <IconAlertTriangle size={28} />}
                  <div><strong>{ffmpeg?.ready ? "Sẵn sàng xử lý" : "Chưa sẵn sàng"}</strong><span>{ffmpeg?.message || "Đang kiểm tra…"}</span></div>
                </div>
                <button className="secondary-button" onClick={() => void api.ffmpegStatus().then(setFfmpeg)}><IconRefresh size={18} /> Kiểm tra lại</button>
              </article>
            </div>

            <div className="settings-actions">
              <button className="secondary-button" onClick={cancelSettings}>Hủy</button>
              <button className="primary-button" onClick={save} disabled={saving || !config.projectRoot || !config.azureKey}>
                {saving ? <IconLoader2 className="spin" size={18} /> : <IconCheck size={18} />} Lưu thiết lập
              </button>
            </div>
          </section>
        ) : (
          <section className="library-page page-enter" aria-label="Thư viện">
            <div className="library-layout">
              <div className="panel control-bar">
                <div className="youtube-import">
                  <IconBrandYoutube size={19} />
                  <label className="sr-only" htmlFor="youtube-url">Link YouTube</label>
                  <input
                    id="youtube-url"
                    type="url"
                    value={youtubeUrl}
                    onChange={(event) => setYoutubeUrl(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter") void importYoutube();
                    }}
                    placeholder="Dán link YouTube để tải video và SRT…"
                    autoComplete="off"
                    disabled={importingYoutube}
                  />
                  <label className="sr-only" htmlFor="youtube-quality">Chất lượng video</label>
                  <select
                    id="youtube-quality"
                    value={youtubeQuality}
                    onChange={(event) => setYoutubeQuality(event.target.value)}
                    disabled={importingYoutube}
                    title="Chất lượng MP4 tối đa"
                  >
                    <option value="lowest">Thấp nhất</option>
                    <option value="144p">144p</option>
                    <option value="240p">240p</option>
                    <option value="360p">360p</option>
                    <option value="480p">480p</option>
                    <option value="720p">720p</option>
                    <option value="1080p">1080p</option>
                    <option value="highest">Cao nhất</option>
                  </select>
                  <button
                    type="button"
                    className="secondary-button youtube-button"
                    onClick={() => void importYoutube()}
                    disabled={!youtubeUrl.trim() || importingYoutube}
                  >
                    {importingYoutube ? <IconLoader2 className="spin" size={18} /> : <IconBrandYoutube size={18} />}
                    {importingYoutube ? "Đang tải…" : "Tải từ YouTube"}
                  </button>
                </div>
                <div className="search">
                  <IconSearch size={18} />
                  <label className="sr-only" htmlFor="video-search">Tìm video</label>
                  <input
                    id="video-search"
                    type="search"
                    value={query}
                    onChange={(event) => setQuery(event.target.value)}
                    placeholder="Tìm khóa học hoặc video…"
                  />
                </div>
                <div className="library-count">
                  <strong>{selected.size}</strong> đã chọn <span>/</span> {videos.length} video
                </div>
                <div className="library-tools" aria-label="Tổ chức thư viện">
                  <button className={`tool-button icon-only ${libraryView === "list" ? "active" : ""}`} onClick={() => setLibraryView("list")} aria-label="Dạng danh sách" data-tooltip="Dạng danh sách"><IconLibrary size={17} /></button>
                  <button className={`tool-button icon-only ${libraryView === "grid" ? "active" : ""}`} onClick={() => setLibraryView("grid")} aria-label="Dạng lưới" data-tooltip="Dạng lưới"><IconLayoutGrid size={17} /></button>
                  <label className="sr-only" htmlFor="library-sort">Sắp xếp</label>
                  <select id="library-sort" value={librarySort} onChange={(event) => setLibrarySort(event.target.value as LibrarySort)}>
                    <option value="name">Tên A–Z</option>
                    <option value="added">Mới thêm</option>
                  </select>
                  <label className="sr-only" htmlFor="library-group">Nhóm video</label>
                  <select id="library-group" value={libraryGroup} onChange={(event) => setLibraryGroup(event.target.value as LibraryGroup)}>
                    <option value="none">Không nhóm</option>
                    <option value="course">Theo khóa học</option>
                    <option value="date">Theo ngày thêm</option>
                    <option value="name">Theo tên</option>
                  </select>
                  <label className="sr-only" htmlFor="library-playlist">Lọc playlist</label>
                  <select id="library-playlist" value={activePlaylist} onChange={(event) => setActivePlaylist(event.target.value)}>
                    <option value="all">Mọi playlist</option>
                    {projectPlaylists.map((playlist) => <option key={playlist.id} value={playlist.id}>{playlist.name}</option>)}
                  </select>
                  <button className="tool-button" onClick={() => void createPlaylist()} disabled={!config.projectRoot}><IconPlus size={16} /> Playlist</button>
                  <button className="tool-button" onClick={() => void addSelectedToPlaylist()} disabled={activePlaylist === "all" || !selected.size}><IconPlaylistAdd size={16} /> Thêm</button>
                </div>
                <div className="option-chips" role="group" aria-label="Tùy chọn dub">
                  <OptionChip
                    id="force"
                    label="Xử lý lại"
                    title="Xử lý lại video đã xong?"
                    body="Bật khi bạn muốn làm lại từ đầu dù file lồng tiếng đã có sẵn."
                    hint="Tắt = bỏ qua video đã xong, tiết kiệm thời gian."
                    checked={force}
                    onChange={setForce}
                  />
                  <OptionChip
                    id="force-tts"
                    label="Sinh lại TTS"
                    title="Tạo lại giọng nói?"
                    body="Bật khi muốn Azure đọc lại phụ đề (đổi giọng, sửa SRT, hoặc giọng cũ bị lỗi)."
                    hint="Tắt = dùng lại giọng đã tạo trước đó (nhanh, ít tốn API)."
                    checked={forceTts}
                    onChange={setForceTts}
                  />
                  <OptionChip
                    id="translate"
                    label="Dịch bằng Gemini"
                    title="Tự dịch phụ đề sang tiếng Việt"
                    body="Gemini dịch và chỉnh câu sau bước clean subtitle, trước khi tạo giọng."
                    hint="Cần nhập ít nhất một Gemini API key trong Thiết lập."
                    checked={translate}
                    disabled={!config.geminiKeys.trim()}
                    onChange={setTranslate}
                  />
                  <OptionChip
                    id="natural-voice"
                    label="Natural Voice"
                    title="Lập cách nói tự nhiên?"
                    body="Gemini lập Speech Plan, Azure nhận SSML theo khả năng giọng và Dubora chỉ kéo giãn âm thanh trong giới hạn an toàn."
                    hint="Cần ít nhất một Gemini API key. Mặc định Cân bằng: block giải thích chỉ được dịch chuyển tối đa 250 ms trong cùng sync island."
                    checked={naturalVoice}
                    disabled={!config.geminiKeys.trim()}
                    onChange={setNaturalVoice}
                  />
                  {naturalVoice && (
                    <>
                      <label className="natural-mode">
                        Nhịp Natural Voice
                        <select value={naturalMode} onChange={(event) => setNaturalMode(event.target.value as typeof naturalMode)}>
                          <option value="sync">Đồng bộ sát</option>
                          <option value="balanced">Cân bằng</option>
                          <option value="natural">Tự nhiên tối đa</option>
                        </select>
                      </label>
                      <label className="natural-mode">
                        Nguồn timing
                        <select
                          value={sourceTiming}
                          title="Azure Fast đọc audio nguồn để lấy timestamp từng từ; chậm và tốn thêm quota hơn caption timing."
                          onChange={(event) => setSourceTiming(event.target.value as typeof sourceTiming)}
                        >
                          <option value="caption-derived">Caption timing</option>
                          <option value="azure-fast">Azure Fast granular</option>
                        </select>
                      </label>
                    </>
                  )}
                </div>
                <div className="control-bar-actions">
                  <button
                    type="button"
                    className="secondary-button"
                    onClick={() => openPath(outputPath(config.projectRoot))}
                    disabled={!config.projectRoot}
                  >
                    <IconFolderOpen size={18} /> Outputs
                  </button>
                  <button
                    type="button"
                    className="secondary-button"
                    onClick={() => void scan()}
                    disabled={scanning || !config.projectRoot}
                  >
                    {scanning ? <IconLoader2 className="spin" size={18} /> : <IconRefresh size={18} />}
                    Quét lại
                  </button>
                  {busy ? (
                    <button type="button" className="secondary-button" onClick={() => void cancelJob()}>
                      <IconPlayerStopFilled size={18} /> Hủy job
                    </button>
                  ) : (
                    <button
                      type="button"
                      className="text-button"
                      disabled={!videos.length || !ffmpeg?.ready}
                      onClick={() => void run(videos.map((video) => video.relativePath))}
                    >
                      Dub tất cả {videos.length ? `(${videos.length})` : ""}
                    </button>
                  )}
                  <button
                    type="button"
                    className="primary-button run-button"
                    disabled={busy || !selected.size || !ffmpeg?.ready}
                    onClick={() => void run([...selected])}
                  >
                    {busy ? <IconLoader2 className="spin" size={19} /> : <IconPlayerPlayFilled size={19} />}
                    {busy ? "Đang xử lý…" : `Dub ${selected.size || 0}`}
                    <IconArrowRight size={18} />
                  </button>
                </div>
              </div>

              <article className="panel library-panel">
                {!config.projectRoot ? (
                  <div className="empty-state"><IconFolder size={38} /><h2>Chọn project root để bắt đầu</h2><p>Trong Thiết lập, chọn folder dự án — app sẽ tự tạo assets/video, assets/srt, outputs.</p><button className="primary-button" onClick={() => setView("settings")}><IconSettings size={18} /> Mở thiết lập</button></div>
                ) : !videos.length && !scanning ? (
                  <div className="empty-state"><IconVideo size={38} /><h2>Chưa tìm thấy video</h2><p>Bỏ file vào assets/video (và SRT khớp trong assets/srt), rồi quét lại.</p></div>
                ) : (
                  !visibleVideos.length ? (
                    <div className="empty-state"><IconFilter size={38} /><h2>Không có video phù hợp</h2><p>Đổi playlist, từ khóa tìm kiếm hoặc cách lọc để xem lại video.</p></div>
                  ) : libraryView === "list" ? (
                    <div className="table-scroll">
                      <table>
                        <thead><tr><th className="check-cell"><input type="checkbox" aria-label="Chọn tất cả video đang hiển thị" checked={visibleVideos.every((video) => selected.has(video.relativePath))} onChange={toggleAllVisible} /></th><th>Video / đường dẫn tương đối</th><th>SRT</th><th>Output</th><th><span className="sr-only">Tác vụ</span></th></tr></thead>
                        <tbody>
                          {videoGroups.flatMap((group) => [
                            ...(group.label ? [<tr className="library-group-row" key={`group-${group.label}`}><td colSpan={5}>{group.label} <span>{group.videos.length}</span></td></tr>] : []),
                            ...group.videos.map((video) => (
                              <tr key={video.relativePath} className={selected.has(video.relativePath) ? "selected" : ""}>
                                <td className="check-cell"><input type="checkbox" aria-label={`Chọn ${video.relativePath}`} checked={selected.has(video.relativePath)} onChange={() => toggleSelected(video.relativePath)} /></td>
                                <td><button className="video-name" onClick={() => toggleSelected(video.relativePath)}><span className="file-icon"><IconVideo size={18} /></span><span><strong>{videoName(video)}</strong><small>{video.relativePath} · Thêm {formatAddedAt(video.addedAt)}</small></span></button></td>
                                <td><span className={`badge ${video.hasSrt ? "success" : "danger"}`}>{video.hasSrt ? <IconCheck size={14} /> : <IconX size={14} />}{video.hasSrt ? "Có SRT" : "Thiếu"}</span></td>
                                <td><span className={`badge ${video.hasOutput ? "cyan" : "muted"}`}><span className="mini-dot" />{video.hasOutput ? "Đã dub" : "Chưa có"}</span></td>
                                <td><div className="row-actions">
                                  <button className="icon-button row-action" onClick={() => void playVideo(video)} aria-label={`Phát ${video.relativePath}`} data-tooltip="Phát trong Dubora"><IconPlayerPlayFilled size={18} /></button>
                                  <button className="icon-button row-action" disabled={busy} onClick={() => void renameVideo(video)} aria-label={`Đổi tên ${video.relativePath}`} data-tooltip="Đổi tên video và SRT"><IconPencil size={18} /></button>
                                  <button className="icon-button row-action danger" disabled={busy} onClick={() => void deleteVideo(video)} aria-label={`Xóa ${video.relativePath}`} data-tooltip="Xóa video và SRT"><IconTrash size={18} /></button>
                                  <button className="icon-button row-action" disabled={!video.hasOutput} onClick={() => openPath(outputPath(config.projectRoot, video.relativePath))} aria-label={`Mở output của ${video.relativePath}`} data-tooltip="Mở output"><IconFolderOpen size={18} /></button>
                                </div></td>
                              </tr>
                            )),
                          ])}
                        </tbody>
                      </table>
                    </div>
                  ) : (
                    <div className="video-grid-scroll">
                      {videoGroups.map((group) => (
                        <section key={group.label || "all"} className="video-grid-group">
                          {group.label && <h2>{group.label}<span>{group.videos.length}</span></h2>}
                          <div className="video-grid">
                            {group.videos.map((video) => (
                              <article key={video.relativePath} className={selected.has(video.relativePath) ? "video-card selected" : "video-card"}>
                                <button className="video-card-preview" onClick={() => void playVideo(video)} aria-label={`Phát ${videoName(video)}`}>
                                  <video muted playsInline preload="metadata" src={convertFileSrc(joinPath(config.projectRoot, "assets", "video", video.relativePath))} onLoadedMetadata={(event) => { event.currentTarget.currentTime = Math.min(0.1, event.currentTarget.duration / 10); }} />
                                  <span><IconPlayerPlayFilled size={24} /></span>
                                </button>
                                <div className="video-card-body"><div><strong>{videoName(video)}</strong><small>{videoCourse(video)} · {formatAddedAt(video.addedAt)}</small></div><input type="checkbox" aria-label={`Chọn ${video.relativePath}`} checked={selected.has(video.relativePath)} onChange={() => toggleSelected(video.relativePath)} /></div>
                              </article>
                            ))}
                          </div>
                        </section>
                      ))}
                    </div>
                  )
                )}
              </article>

              <article className="panel progress-panel job-monitor" aria-live="polite">
                <div className="job-col job-col-status">
                  <div className="progress-head">
                    <div>
                      <span>Tiến độ</span>
                      <strong>
                        {busy ? "ĐANG CHẠY" : progress?.stage === "error" ? "LỖI" : progress ? "HOÀN TẤT" : "CHỜ"}
                      </strong>
                    </div>
                    <span className="job-time"><IconClock size={15} /> Trực tiếp</span>
                  </div>
                  <div className="current-file" data-tooltip={progress?.video}>
                    {progress?.video || progress?.message || "Chưa có job nào"}
                  </div>
                  <div className="progress-value">
                    <strong>{progressValue.toString().padStart(2, "0")}</strong>
                    <span>%</span>
                    <small>
                      {progress?.current != null && progress.total ? `${progress.current} / ${progress.total}` : ""}
                    </small>
                  </div>
                  <div className="progress-track"><span style={{ width: `${progressValue}%` }} /></div>
                </div>
                <div className="job-col job-col-stages">
                  <div className="job-col-label">Giai đoạn</div>
                  <div className="stages">
                    {STAGES.map(([stage, label]) => {
                      const currentIndex = STAGES.findIndex(([key]) => key === progress?.stage);
                      const index = STAGES.findIndex(([key]) => key === stage);
                      const complete =
                        progress?.stage === "done" || progress?.stage === "skip" || index < currentIndex;
                      return (
                        <div
                          key={stage}
                          className={progress?.stage === stage ? "active" : complete ? "complete" : ""}
                        >
                          <span>{complete ? <IconCheck size={12} /> : index + 1}</span>
                          {label}
                        </div>
                      );
                    })}
                  </div>
                </div>
                <div className="job-col job-col-console">
                  <div className="job-col-label">Nhật ký</div>
                  <div className="console" aria-label="Nhật ký job" tabIndex={0}>
                    {logs.length ? (
                      logs.map((line, index) => (
                        <div key={`${line.jobId}-${index}`} className={line.stage}>
                          <span>{String(index + 1).padStart(2, "0")}</span>
                          <p>{line.message}</p>
                        </div>
                      ))
                    ) : (
                      <div className="placeholder">
                        <IconTerminal2 size={17} />
                        <p>Nhật ký xử lý sẽ xuất hiện tại đây.</p>
                      </div>
                    )}
                    <div ref={logEndRef} />
                  </div>
                </div>
              </article>
            </div>
            {activeVideo && (
              <div className="library-player-overlay" role="dialog" aria-modal="true" aria-label="Trình phát video">
                <VideoPlayer video={activeVideo} playback={playback} loading={loadingPlayback} onClose={closePlayer} />
              </div>
            )}
          </section>
        )}
      </main>
    </div>
  );
}

export default App;
