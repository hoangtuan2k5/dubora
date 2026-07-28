import { useEffect, useMemo, useRef, useState } from "react";
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
  IconLoader2,
  IconMoon,
  IconMovie,
  IconPlayerPlayFilled,
  IconPlayerStopFilled,
  IconRefresh,
  IconSearch,
  IconSettings,
  IconSortAscendingLetters,
  IconSortDescendingLetters,
  IconSun,
  IconTerminal2,
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
  type VideoInfo,
} from "./tauri";

const DEFAULT_CONFIG: AppConfig = {
  projectRoot: "",
  azureKey: "",
  azureRegion: "southeastasia",
  azureVoice: DEFAULT_AZURE_VOICE,
  geminiKeys: "",
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
  const blob = `${voice.shortName} ${voice.displayName} ${voice.localName}`.toLowerCase();
  return blob.includes("multilingual") || blob.includes("multi");
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
  const [view, setView] = useState<"library" | "settings" | "guide">("library");
  const [theme, setTheme] = useState<"dark" | "light">(() =>
    localStorage.getItem("theme") === "light" ? "light" : "dark",
  );
  const [config, setConfig] = useState(DEFAULT_CONFIG);
  const [savedConfig, setSavedConfig] = useState(DEFAULT_CONFIG);
  const [videos, setVideos] = useState<VideoInfo[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [query, setQuery] = useState("");
  const [force, setForce] = useState(false);
  const [forceTts, setForceTts] = useState(false);
  const [translate, setTranslate] = useState(false);
  const [youtubeUrl, setYoutubeUrl] = useState("");
  const [importingYoutube, setImportingYoutube] = useState(false);
  const [showKey, setShowKey] = useState(false);
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

  const visibleVideos = useMemo(() => {
    const term = query.trim().toLocaleLowerCase("vi");
    return term
      ? videos.filter((video) => video.relativePath.toLocaleLowerCase("vi").includes(term))
      : videos;
  }, [query, videos]);

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
        setConfig((current) => ({ ...current, azureVoice: DEFAULT_AZURE_VOICE }));
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

  const filteredVoices = useMemo(() => {
    const q = voiceQuery.trim().toLowerCase();
    let list = voices.filter((voice) => {
      if (voiceLocale !== "all" && voice.locale !== voiceLocale) return false;
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
      const result = await api.importYoutube(youtubeUrl.trim());
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

  const run = async (targets: string[]) => {
    if (!targets.length || busy) return;
    setBusy(true);
    setLogs([]);
    setProgress(null);
    setNotice("");
    try {
      await api.startDub(targets, force, forceTts, translate);
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
          <div className="brand-mark"><IconWaveSine size={25} stroke={1.7} /></div>
          <div><strong>DUBORA</strong><span>Every story, in every voice.</span></div>
        </div>

        <nav className="nav" aria-label="Điều hướng chính">
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
            {view === "library" ? "Thư viện" : view === "settings" ? "Thiết lập" : "Hướng dẫn"}
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
                      <strong>Thêm Gemini key nếu cần dịch</strong>
                      <span>
                        Với SRT tiếng Anh, tạo key ở Google AI Studio, dán vào mục Gemini API keys
                        trong Thiết lập, rồi bật <strong>Dịch bằng Gemini</strong> khi chạy dub.
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
                    <p className="lead">Chỉ cần khi muốn dịch và tối ưu SRT tiếng Anh.</p>
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
                    xoay vòng key theo từng batch dịch.
                  </li>
                </ol>
                <div className="guide-tip">
                  <strong>Khi dùng:</strong> quay lại Thư viện, bật <strong>Dịch bằng Gemini</strong>
                  {" "}trước khi bấm Dub. App dùng <code>gemini-3.5-flash-lite</code> để dịch và
                  chỉnh câu SRT.
                </div>
              </article>

              <article className="panel guide-card">
                <div className="panel-title">
                  <span className="number">06</span>
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
                  <span className="number">07</span>
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
                  <span className="number">08</span>
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
                    onClick={() => setConfig((current) => ({ ...current, azureVoice: DEFAULT_AZURE_VOICE }))}
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
                          setConfig((current) => ({ ...current, azureVoice: event.target.value }))
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
                          setConfig((current) => ({ ...current, azureVoice: event.target.value }))
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
              </article>

              <article className="panel config-panel">
                <div className="panel-title"><span className="number">03</span><div><h2>Google AI Studio</h2><p>Chỉ cần khi bật dịch phụ đề bằng Gemini.</p></div></div>
                <label htmlFor="gemini-keys">Gemini API keys</label>
                <textarea
                  id="gemini-keys"
                  value={config.geminiKeys}
                  onChange={(event) => setConfig({ ...config, geminiKeys: event.target.value })}
                  placeholder="Một key mỗi dòng, hoặc phân cách bằng dấu phẩy"
                  autoComplete="off"
                  spellCheck={false}
                />
                <p className="field-hint">Dùng <code>gemini-3.5-flash-lite</code>; các key được xoay theo từng batch dịch.</p>
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
                  <div className="table-scroll">
                    <table>
                      <thead><tr><th className="check-cell"><input type="checkbox" aria-label="Chọn tất cả video đang hiển thị" checked={visibleVideos.length > 0 && visibleVideos.every((video) => selected.has(video.relativePath))} onChange={toggleAllVisible} /></th><th>Video / đường dẫn tương đối</th><th>SRT</th><th>Output</th><th><span className="sr-only">Tác vụ</span></th></tr></thead>
                      <tbody>
                        {visibleVideos.map((video) => (
                          <tr key={video.relativePath} className={selected.has(video.relativePath) ? "selected" : ""}>
                            <td className="check-cell"><input type="checkbox" aria-label={`Chọn ${video.relativePath}`} checked={selected.has(video.relativePath)} onChange={() => toggleSelected(video.relativePath)} /></td>
                            <td><button className="video-name" onClick={() => toggleSelected(video.relativePath)}><span className="file-icon"><IconVideo size={18} /></span><span><strong>{video.relativePath.split(/[\\/]/).at(-1)}</strong><small>{video.relativePath}</small></span></button></td>
                            <td><span className={`badge ${video.hasSrt ? "success" : "danger"}`}>{video.hasSrt ? <IconCheck size={14} /> : <IconX size={14} />}{video.hasSrt ? "Có SRT" : "Thiếu"}</span></td>
                            <td><span className={`badge ${video.hasOutput ? "cyan" : "muted"}`}><span className="mini-dot" />{video.hasOutput ? "Đã dub" : "Chưa có"}</span></td>
                            <td><button className="icon-button row-action" disabled={!video.hasOutput} onClick={() => openPath(outputPath(config.projectRoot, video.relativePath))} aria-label={`Mở output của ${video.relativePath}`} data-tooltip="Mở output"><IconFolderOpen size={18} /></button></td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
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
          </section>
        )}
      </main>
    </div>
  );
}

export default App;
