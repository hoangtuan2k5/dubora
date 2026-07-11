import { useEffect, useMemo, useRef, useState } from "react";
import {
  IconAdjustmentsHorizontal,
  IconAlertTriangle,
  IconArrowRight,
  IconBrandAzure,
  IconCheck,
  IconCircleCheckFilled,
  IconClock,
  IconEye,
  IconEyeOff,
  IconBook,
  IconExternalLink,
  IconFileText,
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
  IconSun,
  IconTerminal2,
  IconVideo,
  IconWaveSine,
  IconX,
} from "@tabler/icons-react";
import {
  api,
  type AppConfig,
  type DubProgress,
  type FfmpegStatus,
  type VideoInfo,
} from "./tauri";

const DEFAULT_CONFIG: AppConfig = {
  projectRoot: "",
  azureKey: "",
  azureRegion: "southeastasia",
};

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

function outputPath(root: string, relativePath?: string) {
  if (!relativePath) return `${root.replace(/[\\/]$/, "")}/outputs`;
  const file = relativePath.replace(/\.[^./\\]+$/, ".vi-dub.mp4");
  return `${root.replace(/[\\/]$/, "")}/outputs/${file}`;
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
        <div className="flow-chip mid">viet-dub</div>
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
  const [showKey, setShowKey] = useState(false);
  const [ffmpeg, setFfmpeg] = useState<FfmpegStatus | null>(null);
  const [progress, setProgress] = useState<DubProgress | null>(null);
  const [logs, setLogs] = useState<DubProgress[]>([]);
  const [busy, setBusy] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState("");
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
    void api.onProgress((event) => {
      setProgress((current) =>
        event.stage === "tts" && event.current == null && current?.stage === "tts"
          ? { ...event, current: current.current, total: current.total }
          : event,
      );
      setLogs((current) => [...current.slice(-199), event]);
      if (event.video === "" && (event.stage === "done" || event.stage === "error")) {
        setBusy(false);
        const root = projectRootRef.current;
        if (root) void scan(root);
      }
    })
      .then((stop) => (active ? (unlisten = stop) : stop()))
      .catch((error) => setNotice(`Không thể theo dõi tiến độ: ${describeError(error)}`));
    return () => {
      active = false;
      unlisten?.();
    };
    // Tauri listeners and initial config load once for the app lifetime.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => logEndRef.current?.scrollIntoView({ behavior: "smooth" }), [logs]);

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

  const openPath = (path: string) => {
    void api.openPath(path).catch((error) => setNotice(`Không thể mở đường dẫn: ${describeError(error)}`));
  };

  const run = async (targets: string[]) => {
    if (!targets.length || busy) return;
    setBusy(true);
    setLogs([]);
    setProgress(null);
    setNotice("");
    try {
      await api.startDub(targets, force, forceTts);
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
        <div className="brand" aria-label="viet-dub">
          <div className="brand-mark"><IconWaveSine size={25} stroke={1.7} /></div>
          <div><strong>VIET//DUB</strong><span>VOICE WORKSTATION</span></div>
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
          <div className="meter-heading"><span>SYSTEM CHECK</span><IconTerminal2 size={16} /></div>
          <div className={`system-line ${ffmpeg?.ready ? "ok" : "warn"}`}>
            <span className="status-dot" /> FFmpeg + rubberband
            <b>{ffmpeg?.ready ? "READY" : "CHECK"}</b>
          </div>
          {ffmpeg?.path && <small title={ffmpeg.path}>{ffmpeg.path}</small>}
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
          <div className="breadcrumb">
            <span>DESKTOP /</span>{" "}
            {view === "library" ? "THƯ VIỆN" : view === "settings" ? "THIẾT LẬP" : "HƯỚNG DẪN"}
          </div>
          <div className="root-chip" title={config.projectRoot || "Chưa chọn project root"}>
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
          <section className="guide-page page-enter" aria-labelledby="guide-title">
            <div className="page-heading">
              <div>
                <p className="eyebrow">USER GUIDE</p>
                <h1 id="guide-title">Hướng dẫn sử dụng</h1>
              </div>
              <p>
                Viet-dub tạo video lồng tiếng Việt từ file video + phụ đề SRT khớp nhau, dùng giọng
                Azure Speech (Florian Dragon HD).
              </p>
            </div>

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
                  <span className="number">04</span>
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
                  <span className="number">05</span>
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
                    <ExternalLink href="https://github.com/hoangtuan2k5/viet-dub">
                      hoangtuan2k5/viet-dub <IconExternalLink size={14} />
                    </ExternalLink>
                  </li>
                </ul>
              </article>

              <article className="panel guide-card">
                <div className="panel-title">
                  <span className="number">06</span>
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
                    <strong>Phụ đề mặc định là tiếng Việt.</strong> Dịch EN→VI chưa có trên desktop
                    (Coming soon). Hãy dùng SRT đã dịch sẵn.
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
                viet-dub v0.1.0 · © 2026 Hoàng Chiều Nguyễn Tuấn · tuan@hoangtuan.dev ·{" "}
                github.com/hoangtuan2k5
              </p>
            </div>
          </section>
        ) : view === "settings" ? (
          <section className="settings-page page-enter" aria-labelledby="settings-title">
            <div className="page-heading">
              <div><p className="eyebrow">WORKSPACE CONFIGURATION</p><h1 id="settings-title">Thiết lập hệ thống</h1></div>
              <p>Liên kết thư mục dự án, Azure Speech và bộ công cụ xử lý âm thanh.</p>
            </div>

            <div className="settings-grid">
              <article className="panel config-panel">
                <div className="panel-title"><span className="number">01</span><div><h2>Thư mục dự án</h2><p>Nơi chứa assets, work và outputs.</p></div></div>
                <label htmlFor="project-root">Project root</label>
                <div className="input-action">
                  <div className="input-wrap"><IconFolder size={18} /><input id="project-root" value={config.projectRoot} onChange={(event) => setConfig({ ...config, projectRoot: event.target.value })} placeholder="D:\Courses\viet-dub" /></div>
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
              </article>

              <article className="panel status-panel">
                <div className="panel-title"><span className="number">03</span><div><h2>FFmpeg engine</h2><p>Yêu cầu filter rubberband.</p></div></div>
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
          <section className="library-page page-enter" aria-labelledby="library-title">
            <div className="page-heading compact">
              <div><p className="eyebrow">PROJECT LIBRARY</p><h1 id="library-title">Bàn dựng lồng tiếng</h1></div>
              <div className="heading-actions">
                <button className="secondary-button" onClick={() => openPath(outputPath(config.projectRoot))} disabled={!config.projectRoot}><IconFolderOpen size={18} /> Mở outputs</button>
                <button className="secondary-button" onClick={() => void scan()} disabled={scanning || !config.projectRoot}>{scanning ? <IconLoader2 className="spin" size={18} /> : <IconRefresh size={18} />} Quét lại</button>
              </div>
            </div>

            <div className="workspace-grid">
              <article className="panel library-panel">
                <div className="library-toolbar">
                  <div className="search"><IconSearch size={18} /><label className="sr-only" htmlFor="video-search">Tìm video</label><input id="video-search" type="search" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Tìm khóa học hoặc video…" /></div>
                  <div className="library-count"><strong>{selected.size}</strong> đã chọn <span>/</span> {videos.length} video</div>
                </div>

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
                            <td><button className="icon-button row-action" disabled={!video.hasOutput} onClick={() => openPath(outputPath(config.projectRoot, video.relativePath))} aria-label={`Mở output của ${video.relativePath}`} title="Mở output"><IconFolderOpen size={18} /></button></td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                )}
              </article>

              <aside className="runner-column">
                <article className="panel run-panel">
                  <div className="panel-title"><span className="live-mark" /><div><h2>Dub control</h2><p>Azure · Florian Dragon HD</p></div><IconAdjustmentsHorizontal size={20} /></div>
                  <div className="switch-row"><label htmlFor="force"><span><strong>Xử lý lại</strong><small>Dù output đã mới nhất</small></span><input id="force" type="checkbox" role="switch" checked={force} onChange={(event) => setForce(event.target.checked)} /></label></div>
                  <div className="switch-row"><label htmlFor="force-tts"><span><strong>Sinh lại TTS</strong><small>Xóa cache giọng hiện tại</small></span><input id="force-tts" type="checkbox" role="switch" checked={forceTts} onChange={(event) => setForceTts(event.target.checked)} /></label></div>
                  <div className="switch-row disabled"><label htmlFor="translate"><span><strong>Dịch EN → VI</strong><small>Coming soon</small></span><input id="translate" type="checkbox" role="switch" disabled /></label></div>
                  <button className="primary-button run-button" disabled={busy || !selected.size || !ffmpeg?.ready} onClick={() => void run([...selected])}>
                    {busy ? <IconLoader2 className="spin" size={19} /> : <IconPlayerPlayFilled size={19} />} {busy ? "Đang xử lý…" : `Dub ${selected.size || "video đã chọn"}`} <IconArrowRight size={18} />
                  </button>
                  {busy ? (
                    <button className="secondary-button" onClick={() => void cancelJob()}>
                      <IconPlayerStopFilled size={18} /> Hủy job
                    </button>
                  ) : (
                    <button className="text-button" disabled={!videos.length || !ffmpeg?.ready} onClick={() => void run(videos.map((video) => video.relativePath))}>
                      Dub tất cả {videos.length ? `(${videos.length})` : ""}
                    </button>
                  )}
                </article>

                <article className="panel progress-panel" aria-live="polite">
                  <div className="progress-head"><div><span>JOB MONITOR</span><strong>{busy ? "ĐANG CHẠY" : progress?.stage === "error" ? "LỖI" : progress ? "HOÀN TẤT" : "CHỜ"}</strong></div><span className="job-time"><IconClock size={15} /> LIVE</span></div>
                  <div className="current-file" title={progress?.video}>{progress?.video || progress?.message || "Chưa có job nào"}</div>
                  <div className="progress-value"><strong>{progressValue.toString().padStart(2, "0")}</strong><span>%</span><small>{progress?.current != null && progress.total ? `${progress.current} / ${progress.total}` : ""}</small></div>
                  <div className="progress-track"><span style={{ width: `${progressValue}%` }} /></div>
                  <div className="stages">
                    {STAGES.map(([stage, label]) => {
                      const currentIndex = STAGES.findIndex(([key]) => key === progress?.stage);
                      const index = STAGES.findIndex(([key]) => key === stage);
                      const complete = progress?.stage === "done" || progress?.stage === "skip" || index < currentIndex;
                      return <div key={stage} className={progress?.stage === stage ? "active" : complete ? "complete" : ""}><span>{complete ? <IconCheck size={12} /> : index + 1}</span>{label}</div>;
                    })}
                  </div>
                  <div className="console" aria-label="Nhật ký job" tabIndex={0}>
                    {logs.length ? logs.map((line, index) => <div key={`${line.jobId}-${index}`} className={line.stage}><span>{String(index + 1).padStart(2, "0")}</span><p>{line.message}</p></div>) : <div className="placeholder"><IconTerminal2 size={17} /><p>Nhật ký xử lý sẽ xuất hiện tại đây.</p></div>}
                    <div ref={logEndRef} />
                  </div>
                </article>
              </aside>
            </div>
          </section>
        )}
      </main>
    </div>
  );
}

export default App;
