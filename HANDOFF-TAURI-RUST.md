# HANDOFF: Port viet-dub → Rust core + Linux CLI + Windows Tauri

Tài liệu này là **spec triển khai đầy đủ** để AI/agent khác làm mà **không cần** hội thoại trước.

**Nguồn sự thật hành vi:** code Python reference trong `python_reference/dubora_reference/`; root-level scripts chỉ là compatibility shims.
**Mục tiêu kiến trúc:** Phương án **B** — port full Rust, **không** bọc Python sidecar lâu dài.  
**Hai sản phẩm ship:**

| Sản phẩm | Platform | Hình thức |
|----------|----------|-----------|
| **viet-dub CLI** | **Linux** (VPS hiện tại + server headless) | Binary `viet-dub` (clap), thay `python3 run_dub.py` |
| **viet-dub Desktop** | **Windows only** | Tauri 2 `.exe` + installer, UI hiện đại |

**Nguyên tắc:** **một core pipeline dùng chung** (`viet-dub-core`). CLI và Tauri chỉ là frontend khác nhau.  
**Tối ưu hiệu năng:** làm **sau** khi parity end-to-end đạt (Phase 6).  
**Thứ tự dev khuyến nghị:** **core + CLI trên Linux trước** (dễ so parity với Python trên VPS) → rồi bọc Tauri Windows.

---

## 0. Quyết định đã chốt (LOCKED)

| Hạng mục | Quyết định |
|----------|------------|
| Tên app / binary | `viet-dub` |
| Desktop | **Windows only** — Tauri 2 `.exe` + installer |
| CLI | **Linux** (VPS) — binary release `x86_64-unknown-linux-gnu` (glibc). Musl optional sau. |
| Shared code | Crate library **`viet-dub-core`**: paths, clean, TTS, ffmpeg audio/video, orchestrator |
| CLI crate | **`viet-dub-cli`**: clap args, parity flags với `run_dub.py` |
| Desktop crate | **`viet-dub-app` / `src-tauri`**: Tauri commands gọi core |
| Icons (desktop) | [Tabler Icons](https://github.com/tabler/tabler-icons) |
| FFmpeg Windows | **Nhúng vào installer** (kèm filter `rubberband`) |
| FFmpeg Linux CLI | **Dùng FFmpeg hệ thống trên PATH** (VPS đã có / user cài). App resolve `ffmpeg`/`ffprobe` từ env `VIET_DUB_FFMPEG` optional override. **Không** bắt buộc bundle FFmpeg vào binary Linux MVP. |
| Pipeline | Port logic Python → Rust core; vẫn gọi **FFmpeg binary** cho audio/video |
| TTS mặc định | Azure Speech, voice `de-DE-Florian:DragonHDLatestNeural` |
| SRT mặc định | Đã là tiếng Việt; **translate en→vi là opt-in** (MVP có thể để sau) |
| Azure credentials | CLI: env `AZURE_KEY` / `AZURE_REGION` + file `.env` ở project root (parity Python). Desktop: Settings UI lưu local. |
| Ngôn ngữ UI desktop | Tiếng Việt |
| CLI messages | English OK (giống README) hoặc bilingual; log stage rõ |
| Commit/docs kỹ thuật | English present tense nếu cập nhật README |

**Không chọn:** wrap Python lâu dài; tối ưu concurrency trước parity; re-encode video; hard-code Azure key; logic pipeline copy-paste hai lần giữa CLI và Tauri.

---

## 1. Bối cảnh sản phẩm

`viet-dub` tạo video lồng tiếng Việt từ:

- video trong `assets/video/**`
- file SRT khớp trong `assets/srt/**`

Pipeline:

```
assets/video/<rel>.mp4  +  assets/srt/<rel>.srt
  → clean rolling subtitles
  → [optional] translate en → vi          # MVP: có thể stub/skip
  → TTS từng segment (Azure mặc định)
  → ghép voiceover (ffmpeg rubberband + adelay + amix)
  → mux video gốc + voiceover AAC
  → outputs/<rel>.vi-dub.mp4
```

Entry point Python compatibility hiện tại: `run_dub.py`; implementation nằm trong
`python_reference/dubora_reference/run_dub.py`.

---

## 2. Tham chiếu code nguồn (bắt buộc đọc trước khi port)

| File | Vai trò | ~LOC |
|------|---------|------|
| `python_reference/dubora_reference/run_dub.py` | Orchestrator, path matching, skip logic, flags | ~240 |
| `python_reference/dubora_reference/clean_rolling.py` | Linearize YouTube rolling captions + merge câu | ~140 |
| `python_reference/dubora_reference/generate_tts.py` | Azure/Edge/gTTS, cache sha256, concurrent TTS | ~364 |
| `python_reference/dubora_reference/build_audio.py` | FFmpeg filter_complex voiceover | ~168 |
| `python_reference/dubora_reference/build_video.py` | Mux video copy + AAC | ~45 |
| `python_reference/dubora_reference/translate_srt.py` | Dịch Gemini + protect tech terms | ~217 — **MVP optional** |
| `python_reference/dubora_reference/drive_helper.py` | Google Drive — **OUT OF SCOPE** |
| `fuzzy_clean.py` | Không nằm pipeline chính — **OUT OF SCOPE** |
| `scratch/` | Test scripts — **OUT OF SCOPE** |
| `README.md`, `AGENTS.md` | Hành vi user-facing + gotchas | — |

**Yêu cầu agent:** port **hành vi**, không “cải tiến” thuật toán clean/TTS/timing trừ khi user yêu cầu.

---

## 3. Cấu trúc thư mục runtime (giữ nguyên contract)

**Desktop:** user chọn **project root** trong Settings.  
**CLI:** cwd = project root (giống `cd ~/viet-dub && python3 run_dub.py`), hoặc flag `--root <path>`.

Trong root:

```text
<project_root>/
  assets/
    video/   # input videos (recursive)
    srt/     # matching SRT (recursive)
  work/      # intermediate (safe names)
  outputs/   # final *.vi-dub.mp4
```

Video extensions: `.mp4`, `.mov`, `.mkv`, `.webm`.

Matching SRT cho `assets/video/<rel>.ext`:

1. `assets/srt/<rel>.srt`
2. Fallback: nếu `rel` = `course/rest...` thì thử `assets/srt/CC-course/rest....srt`

Không tìm thấy → lỗi rõ, liệt kê path đã thử.

### Work path sanitizer (parity với Python)

```text
safe_work_name(stem):
  giữ alnum, '-', '_', '.'
  space → '_'
  strip '._'
  empty → "job"

safe_work_path(relative_video_path without ext):
  work / join(safe_work_name(each part))
```

Ví dụ: `[NEW] Spring Boot.../part1/1. Introduction.mp4`  
→ work folder sanitized từng segment path.

### Output path

```text
outputs/<same relative path as video>
  filename: <video_stem>.vi-dub.mp4
```

### Job files trong work dir

```text
work/<safe_rel>/
  input.srt              # copy từ assets/srt
  clean_final.srt        # sau clean_rolling
  clean_final_tech.srt   # MVP: copy clean (translate sau)
  clean_final_vi.srt     # SRT đưa vào TTS (mặc định = cleaned VI)
  tts/
    seg_<index>.mp3
    seg_<index>.mp3.sha256
  vietnamese_voiceover.mp3
  filter_complex.txt     # debug
  run_ffmpeg_audio.sh    # optional debug (Windows: .ps1 hoặc bỏ)
```

---

## 4. Spec parity từng module

### 4.1 Orchestrator (`run_dub.py`)

**Per video:**

1. Resolve SRT (`find_matching_srt`).
2. Nếu **không** force và output tồn tại và `mtime(output) >= mtime(video)` và `mtime(output) >= mtime(srt)` → **skip**.
3. Tạo job dir; optional `force_tts` → xóa cả `tts/`.
4. Copy SRT → `input.srt`.
5. `clean_youtube_rolling_subtitles(input → clean_final)`.
6. Nếu translate: dịch → `clean_final_vi` (+ normalized).  
   Else: copy clean → `clean_final_vi` và `clean_final_tech`.
7. Generate TTS cho mọi segment missing/stale (retry passes đến đủ hoặc fail).
8. `build_voiceover_track(vi_srt, tts_dir, voiceover, job_dir)`.
9. `build_dubbed_video(video, voiceover, output)`.

**Flags — CLI parity với Python + Desktop UI map:**

| Flag Python / CLI Rust | Desktop UI |
|------------------------|------------|
| `[target]` optional | Chọn 1 video / folder / all |
| `--provider azure\|edge` (default `azure`) | Settings |
| `--force` | “Xử lý lại dù đã có output” |
| `--force-tts` | “Xóa cache TTS và sinh lại” |
| `--translate` | Toggle “Dịch EN→VI” (MVP có thể ẩn/disable) |
| `--root <path>` (CLI only, optional) | Project root in Settings |
| env `AZURE_KEY`, `AZURE_REGION` + `.env` | Settings fields |

### CLI UX (Linux) — parity `run_dub.py`

```bash
# Từ project root (giống Python)
cd /path/to/viet-dub-project
viet-dub
viet-dub "course/part/lesson.mp4"
viet-dub --force-tts "course/part/lesson.mp4"
viet-dub --provider edge --translate
viet-dub --root /data/courses/my-course   # nếu binary không nằm trong root

# Credentials
export AZURE_KEY=...
export AZURE_REGION=southeastasia
# hoặc file <root>/.env  (KEY=VALUE, không commit)
```

Exit codes:

- `0` success (kể cả toàn skip up-to-date)
- non-zero nếu bất kỳ job fail / missing SRT / missing Azure / ffmpeg fail

Stdout: log từng stage (prefix `+` command optional, progress TTS `Generated x/y`).  
**MVP:** xử lý tuần tự từng video. Desktop: progress events; CLI: print tương đương.

### 4.2 Clean rolling (`clean_rolling.py`) — PORT BẮT BUỘC, 1:1

Hai giai:

**A. Parse original YouTube-style SRT** (`parse_original_youtube_srt`)  
- Không giống `parse_srt` chuẩn index/time/text block 100%.  
- Duyệt line-by-line, nhận diện dòng timing `HH:MM:SS,mmm --> HH:MM:SS,mmm`.  
- Lấy text lines cho đến timing/index kế.  
- Output list `{start, end, text}` (chưa có index).

**B. Linearize rolling**  
- Tích lũy words; mỗi segment mới tìm **max suffix-prefix word overlap** với `accumulated_words`.  
- Chỉ giữ `new_words` sau overlap.  
- Nếu `end - start < 0.5s` → `end = start + 2.0`.

**C. Merge sentences**  
Constants (đúng code hiện tại, **không đổi**):

- merge nếu `(gap < 2.0 OR curr_duration < 4.5)` **AND** `combined_words <= 22` **AND** `combined_duration <= 8.0`
- gap = `next.start - curr.end`
- combined_duration = `next.end - curr.start`
- khi merge: text nối space; `end = max(curr.end, next.end)`

**D. Write SRT chuẩn**  
- index 1..N  
- `seconds_to_time` format `HH:MM:SS,mmm`

**Tests gợi ý:** fixture SRT rolling thật → so hash/text với output Python `clean_rolling.py`.

### 4.3 Parse SRT chuẩn (dùng cho TTS + build_audio)

Pattern block:

```text
<index>
HH:MM:SS,mmm --> HH:MM:SS,mmm
text line(s)...
```

`time_to_seconds`: `h*3600 + m*60 + s + ms/1000`.

**Lưu ý:** `parse_srt` và `parse_original_youtube_srt` **khác nhau** — không gộp.

### 4.4 TTS Azure (`generate_tts.py`) — PORT BẮT BUỘC

#### Config

- `AZURE_KEY` (bắt buộc) — lưu trong app config (os string / file local), **không commit**.
- `AZURE_REGION` default: `southeastasia`.
- Voice default: `de-DE-Florian:DragonHDLatestNeural`.
- Endpoint:  
  `https://{region}.tts.speech.microsoft.com/cognitiveservices/v1`

#### Headers

```http
Ocp-Apim-Subscription-Key: <key>
Content-Type: application/ssml+xml
X-Microsoft-OutputFormat: audio-48khz-192kbitrate-mono-mp3
User-Agent: viet-dub
```

#### SSML (parity)

```xml
<speak version="1.0"
  xmlns="http://www.w3.org/2001/10/synthesis"
  xmlns:mstts="http://www.w3.org/2001/mstts"
  xml:lang="vi-VN">
  <voice name="de-DE-Florian:DragonHDLatestNeural">
    <prosody rate="0%">
      {escaped_text}
    </prosody>
  </voice>
</speak>
```

Escape XML: `& < > " '` → entities.

**Azure:** gửi **text SRT thẳng**, **không** qua `TTS_PRONUNCIATIONS`.  
Pronunciation map chỉ cho Edge (MVP có thể port Edge sau).

#### Output files

- `tts/seg_{index}.mp3`
- sidecar `tts/seg_{index}.mp3.sha256` chứa hex sha256

#### Cache key

```text
payload = "{provider}\n{voice}\n{text.strip()}"
  voice = AZURE_VOICE nếu provider=="azure" else ""
cache_key = sha256(payload) as hex
```

#### Freshness (`is_fresh_audio_file`)

File fresh nếu:

1. tồn tại, size > 0  
2. duration > 0.05s (Python dùng ffprobe; Rust có thể ffprobe hoặc parser mp3)  
3. nếu có `cache_key`: nội dung sidecar `.sha256` khớp  

Nếu không fresh → regenerate.

#### Concurrency MVP

- Semaphore **4** (parity Python).  
- Retry **3**, sleep ~1s giữa lần fail.  
- **Chưa** tối ưu (bỏ sleep 0.2, tăng workers) — để Phase 6.

Python có `await asyncio.sleep(0.2)` mỗi job — MVP có thể giữ hoặc bỏ; ghi rõ trong changelog nếu bỏ.

#### Fail soft

Python: segment fail → silent 0.1s mp3 fallback (có bug path ` {index}.mp3` vs `seg_{index}.mp3` — **khi port hãy ghi đúng `seg_{index}.mp3`**).

Orchestrator: lặp pass TTS tối đa ~8 lần cho đến khi không còn missing.

### 4.5 Build audio (`build_audio.py`) — PORT BẮT BUỘC

Với mỗi segment `i` (index trong list 0..N-1, file `seg_{srt_index}.mp3`):

```text
start = time_to_seconds(seg.start)
slot_end = start_of_next_segment OR seg.end (nếu last)
available = max(0.1, slot_end - start - 0.08)
audio_duration = duration(mp3)
tempo = max(1.0, audio_duration / available)
delay_ms = int(start * 1000)
```

Filter chain per input:

```text
[optional rubberband=tempo=X]
atrim=duration={available}
asetpts=PTS-STARTPTS
adelay={delay_ms}:all=1
```

Rồi:

```text
amix=inputs=N:duration=longest:dropout_transition=0:normalize=0,alimiter=limit=0.95
```

FFmpeg:

- nhiều `-i seg_*.mp3`
- `-filter_complex_script filter_complex.txt` (tránh argv quá dài)
- `-map [outa] -c:a libmp3lame -q:a 2` → `vietnamese_voiceover.mp3`

**Bắt buộc:** FFmpeg build có filter **`rubberband`**.

Missing bất kỳ `seg_*.mp3` → error rõ.

### 4.6 Build video (`build_video.py`) — PORT BẮT BUỘC

```text
ffmpeg -y
  -i <input_video>
  -i <voiceover.mp3>
  -map 0:v:0 -map 1:a:0
  -c:v copy
  -c:a aac -b:a 192k
  -af apad
  -shortest
  -metadata:s:a:0 language=vie
  -disposition:a:0 default
  <output.vi-dub.mp4>
```

Không re-encode video.

### 4.7 Translate (OPTIONAL sau MVP)

`translate_srt.py`: Google via deep-translator, protect tech terms placeholders `ZXQTERM###QXZ`, batch 20, source corrections.  
MVP: UI có thể có toggle disabled “Coming soon”, hoặc skip.

### 4.8 Edge / gTTS

Không bắt buộc MVP. Azure first.

---

## 5. Kiến trúc monorepo Rust (bắt buộc: core dùng chung)

```text
viet-dub/                          # repo gốc (giữ Python để parity đến khi archive)
  python_reference/                # reference parity; root *.py là compatibility shims
  crates/
    viet-dub-core/                 # library: TOÀN BỘ pipeline
      Cargo.toml
      src/
        lib.rs
        paths.rs                   # safe_work_*, find_srt, list_videos
        clean_rolling.rs
        srt.rs
        tts_azure.rs
        tts_cache.rs
        build_audio.rs
        build_video.rs
        ffmpeg.rs                  # resolve ffmpeg/ffprobe (PATH | env | bundled)
        orchestrator.rs            # create_dub_for_video / create_all
        config.rs                  # Azure, provider, root, ProgressSink trait
        progress.rs                # trait ProgressReporter { fn event(...) }
    viet-dub-cli/                  # binary Linux (và cross-build được)
      Cargo.toml                   # depends on viet-dub-core
      src/main.rs                  # clap → core
  apps/
    desktop/                       # Tauri 2 Windows
      package.json
      src/                         # frontend (React/Svelte + Tabler)
      src-tauri/
        Cargo.toml                 # depends on viet-dub-core
        tauri.conf.json
        resources/ffmpeg/          # Windows only: ffmpeg.exe + ffprobe + deps
        src/
          main.rs / lib.rs
          commands.rs              # thin wrappers → core
  Cargo.toml                       # workspace
  HANDOFF-TAURI-RUST.md
  README.md
```

**Quy tắc:** CLI và Tauri **không** chứa logic clean/TTS/filter. Chỉ:

- parse args / UI state  
- implement `ProgressReporter` (stdout vs Tauri emit)  
- truyền `FfmpegPaths` (Linux PATH vs Windows resource dir)

### Progress abstraction

```rust
// pseudocode
pub trait ProgressReporter: Send + Sync {
    fn event(&self, e: ProgressEvent);
}

pub struct ProgressEvent {
    pub stage: Stage, // Clean | Tts | Audio | Video | Done | Error | Skip
    pub video: String,
    pub current: Option<u32>,
    pub total: Option<u32>,
    pub message: String,
}
```

- CLI: print lines  
- Tauri: `app.emit("dub-progress", event)`

### Tauri commands (gợi ý)

- `get_config` / `save_config`
- `pick_project_root` (dialog)
- `scan_videos` → list relative paths + has_srt? + has_output?
- `start_dub { targets, force, force_tts, translate }`
- `cancel_dub`
- `open_path` (explorer)
- `get_ffmpeg_status` (version + rubberband present?)

### Events frontend

```json
{ "jobId": "...", "stage": "clean|tts|audio|video|done|error|skip",
  "video": "course/part/lesson.mp4",
  "current": 12, "total": 200,
  "message": "..." }
```

### Config storage

| | Linux CLI | Windows Desktop |
|--|-----------|-----------------|
| Project root | cwd hoặc `--root` | Settings / `%APPDATA%/viet-dub/` |
| Azure key | env + `<root>/.env` | Settings encrypted/plain local store |
| FFmpeg | PATH hoặc `VIET_DUB_FFMPEG` / `VIET_DUB_FFPROBE` | Bundled resources |

Key nhạy cảm: không log full key.

### Path & subprocess rules

- Mọi path qua `Path`/`std::path`; **không** ghép shell string.  
- `Command::new(ffmpeg).args([...])` list args.  
- Hỗ trợ path có `[ ]` và spaces (course Udemy-style).

### FFmpeg resolution (`ffmpeg.rs`)

```text
resolve order:
  1. env VIET_DUB_FFMPEG / VIET_DUB_FFPROBE nếu set
  2. nếu desktop Windows build: resource_dir/ffmpeg/ffmpeg.exe
  3. "ffmpeg" / "ffprobe" trên PATH
```

Startup / first job: chạy `ffmpeg -filters` (hoặc tương đương) assert **rubberband** có mặt; fail message rõ.

### Bundled FFmpeg (Windows desktop only)

1. Build hoặc tải **FFmpeg Windows** có `ffmpeg.exe`, `ffprobe.exe`, filter **rubberband**.  
2. Resources: `resource_dir/ffmpeg/...`  
3. Installer Tauri (NSIS) đóng gói.  
4. **Giấy phép:** THIRD_PARTY / About (GPL/LGPL).  

Nhiều build public **không** có rubberband — pin build có `--enable-librubberband`.

### FFmpeg Linux (VPS / CLI)

- Expect package đã cài (apt/static) với rubberband — **máy VPS hiện tại đã dùng Python pipeline → reuse**.  
- Document: `ffmpeg -filters | grep rubberband`  
- Optional sau MVP: script download static FFmpeg rubberband cạnh binary.

---

## 6. UI / UX (hiện đại + Tabler Icons)

### Màn hình tối thiểu

1. **Setup / Settings**
   - Project root
   - Azure key + region
   - Kiểm tra FFmpeg (OK / thiếu rubberband)
2. **Library**
   - Tree hoặc table: relative path, SRT ✓/✗, Output ✓/✗
   - Multi-select hoặc “Dub all”
3. **Job runner**
   - Progress bar + stage
   - Log scroll (TTS `12/200`, errors)
   - Cancel
4. **Actions**
   - Start dub
   - Force reprocess / Force TTS (checkbox)
   - Open `outputs/` / open file

### Design

- Dark/light gọn, spacing rộng, typography rõ  
- Icons: Tabler (`tabler-icons` npm hoặc SVG)  
- Không AI-slop generic: chọn 1 palette + 1 font, nhất quán  
- Tiếng Việt labels

### Tech frontend gợi ý

- React + Vite + TypeScript **hoặc** SvelteKit — agent chọn 1, không mix.  
- Tailwind OK.  
- State: job running global.

---

## 7. Phases & acceptance criteria

Làm **tuần tự**. Core+CLI (Linux) xong trước khi polish Windows installer.  
Không nhảy Phase 6 trước parity CLI.

### Phase 0 — Workspace scaffold

- [ ] Cargo workspace: `viet-dub-core`, `viet-dub-cli`
- [ ] (Song song hoặc sau Phase 4) Tauri app skeleton `apps/desktop`
- [ ] `viet-dub --help` chạy trên Linux
- [ ] CI local: `cargo test -p viet-dub-core`

**Done:** `cargo build -p viet-dub-cli` trên VPS Linux thành công.

### Phase 1 — Paths + scan (core + CLI)

- [ ] `list_videos`, `find_matching_srt`, `safe_work_*` parity
- [ ] CLI: `viet-dub` không target → list/process all (hoặc dry-run `viet-dub list` optional)

**Done:** scan course giống Python (kể cả `CC-` fallback) trên Linux VPS.

### Phase 2 — clean_rolling

- [ ] Port 1:1 + unit tests vs Python fixtures
- [ ] Ghi `work/.../clean_final.srt`

**Done:** diff với `python3 clean_rolling.py` trên ≥2 SRT = identical (hoặc whitespace-only).

### Phase 3 — TTS Azure + cache

- [ ] HTTP SSML, `seg_N.mp3` + `.sha256`
- [ ] Skip fresh; `--force-tts` xóa `tts/`
- [ ] concurrency 4, retry 3
- [ ] Progress stdout (CLI)

**Done:** SRT ngắn 10–30 seg; re-run không gọi Azure lại khi fresh. Chạy **trên VPS** với `.env` thật.

### Phase 4 — FFmpeg audio + video + orchestrator CLI

- [ ] PATH ffmpeg/ffprobe + rubberband check (Linux)
- [ ] `build_audio` / `build_video` parity
- [ ] Full `viet-dub <video>` = end-to-end
- [ ] Flags `--force`, `--force-tts`, skip up-to-date

**Done (CLI MVP):** trên VPS, `viet-dub "…/lesson.mp4"` ra `outputs/.../*.vi-dub.mp4` tương đương Python. **Đây là milestone quan trọng nhất.**

### Phase 5 — Desktop Tauri Windows

- [ ] Tauri 2 UI: Settings, library, job runner, Tabler icons
- [ ] Commands gọi **cùng** `viet-dub-core`
- [ ] Bundle FFmpeg+rubberband trong installer
- [ ] Progress events, cancel nếu làm được

**Done:** Windows máy sạch cài installer → dub 1 video, không Python.

### Phase 6 — Performance (CHỈ SAU PARITY CLI + desktop smoke)

- [ ] TTS concurrency configurable + 429 backoff  
- [ ] Parallel duration probe  
- [ ] Optional multi-video jobs  
- [ ] Áp dụng **cùng** trong core → CLI và Desktop đều hưởng  

**Không** làm Phase 6 trong MVP.

---

## 8. OUT OF SCOPE (MVP)

- macOS desktop / macOS CLI  
- Linux **desktop GUI** (CLI only trên Linux)  
- Windows CLI (optional nice-to-have; core compile được nhưng không ship installer CLI)  
- Python sidecar production  
- Google Drive (`drive_helper`)  
- `fuzzy_clean`  
- gTTS  
- Translate EN→VI (trừ khi còn bandwidth sau Phase 5)  
- Bundle FFmpeg vào binary Linux (dùng system FFmpeg)  
- Cloud sync, account system  
- Re-encode / burn subtitles  
- “Cải tiến” thuật toán merge caption  
- Nhân đôi logic pipeline giữa CLI và Tauri (cấm)

---

## 9. Gotchas (đọc kỹ)

1. Path Udemy có `[NEW]`, spaces — test sớm trên Windows.  
2. Hai parser SRT khác nhau.  
3. Azure voice **Đức** + `xml:lang="vi-VN"` là **cố ý** (Florian HD đọc mixed VI/EN tốt).  
4. Cache key phải gồm provider + voice + text; đổi voice → invalidate.  
5. `amix` nhiều trăm input: video dài có thể nặng RAM; parity trước, chunk sau (Phase 6+).  
6. FFmpeg **phải** có `rubberband`.  
7. Không commit `assets/video/*`, `work/`, `outputs/`, Azure key.  
8. Silent fallback path bug Python: port đúng `seg_{index}.mp3`.  
9. Windows argv length: dùng `filter_complex_script` file.  
10. Antivirus có thể quét FFmpeg.exe bundled — document nếu cần.

---

## 10. Verification (bắt buộc trước khi gọi xong)

### 10.1 Linux CLI vs Python (trên VPS hiện tại — ưu tiên)

Cùng project root, cùng 1 video mẫu nhỏ:

| Check | Cách |
|-------|------|
| Clean SRT | So text/timing với `python3 clean_rolling.py` |
| Full pipeline | `python3 run_dub.py "…"` vs `viet-dub "…"` (work dirs tách hoặc force sạch) |
| TTS cache | Re-run CLI không regenerate khi text giữ nguyên |
| Skip up-to-date | Không ghi đè output nếu mtime OK |
| `--force` / `--force-tts` | Hành vi đúng |
| ffprobe | video stream + audio track |
| Nghe thử | timing tương đối ổn |

### 10.2 Windows desktop

| Check | Cách |
|-------|------|
| Installer | Máy không có Python/FFmpeg system vẫn chạy (FFmpeg bundled) |
| Cùng Azure key | Dub 1 video short thành công |
| UI | Progress + error missing SRT / bad key rõ ràng |

Unit tests Rust: clean_rolling, cache key, path sanitize (bắt buộc).

---

## 11. Dependencies gợi ý (Rust)

**viet-dub-core:**

- `tokio` (async TTS)  
- `reqwest` (Azure, rustls)  
- `serde` / `serde_json`  
- `sha2`  
- `regex`  
- `thiserror` / `anyhow`  
- `walkdir`  
- `dotenvy` (load `.env` — CLI + optional core)

**viet-dub-cli:**

- `clap` (derive)  
- depends `viet-dub-core`

**desktop src-tauri:**

- `tauri` 2.x + `dialog`, `store`, …  
- depends `viet-dub-core`  
- Frontend: Tabler icons  

Không cần `ffmpeg-next` — **subprocess FFmpeg** đủ và gần parity hơn.

---

## 12. Thứ tự implement (code order)

1. Workspace + `viet-dub-core` empty + `viet-dub-cli --help`  
2. `paths.rs` ← `run_dub.py` helpers  
3. `clean_rolling.rs` + tests  
4. `srt.rs`  
5. `tts_cache.rs` + `tts_azure.rs`  
6. `ffmpeg.rs` (PATH) + `build_audio` + `build_video`  
7. `orchestrator.rs` + CLI flags full → **parity trên VPS**  
8. Tauri shell + commands → core  
9. Windows bundle FFmpeg + installer  
10. Phase 6 perf (shared core)

Giữ Python **nguyên** để so parity đến khi user archive.

---

## 13. Definition of Done (hai sản phẩm)

### A. Linux CLI (VPS)

1. `cargo build --release -p viet-dub-cli` (hoặc binary release) trên Linux.  
2. Đặt `AZURE_KEY` / `.env`, FFmpeg+rubberband trên PATH.  
3. Trong project root: `viet-dub` / `viet-dub "<rel>.mp4"` thay được `python3 run_dub.py`.  
4. Output `outputs/**/*.vi-dub.mp4`; cache TTS/work tương thích khái niệm (layout giống Python).  
5. Không cần Python để chạy production trên VPS.

### B. Windows Desktop

1. Cài installer `viet-dub`.  
2. Azure key + project root trong Settings.  
3. List bài → Dub → progress → output.  
4. Không cài Python; FFmpeg **bundle** trong app.  
5. UI tiếng Việt, Tabler icons, hiện đại.

Cả hai gọi **cùng** `viet-dub-core` — một bugfix, cả hai hết.

---

## 14. Prompt ngắn để dán cho AI đệ (copy-paste)

```text
Implement viet-dub theo HANDOFF-TAURI-RUST.md (phương án B, full Rust).

HAI SẢN PHẨM:
1) Linux CLI binary `viet-dub` (clap) — chạy trên VPS, thay python3 run_dub.py
2) Windows Tauri 2 desktop — .exe + installer, UI Tabler icons

KIẾN TRÚC BẮT BUỘC:
- Cargo workspace: viet-dub-core (pipeline) + viet-dub-cli + apps/desktop (Tauri)
- CLI và Tauri CHỈ thin UI; MỌI clean/TTS/ffmpeg/orchestrator nằm trong core
- Không bọc Python; không nhân đôi logic

LOCKED:
- Tên: viet-dub
- Desktop: Windows only; FFmpeg+rubberband bundle trong installer
- CLI: Linux; FFmpeg lấy từ PATH (VPS đã có rubberband)
- Azure credentials: CLI env/.env; Desktop Settings
- Port parity Python: run_dub.py, clean_rolling.py, generate_tts.py,
  build_audio.py, build_video.py
- TTS default Azure Florian Dragon HD; SRT default đã VI
- Translate / Edge / Drive = sau MVP
- Tối ưu perf = Phase 6 sau parity

THỨ TỰ: Phase 0→4 = core + CLI parity trên Linux TRƯỚC;
Phase 5 = Tauri Windows; Phase 6 = perf chung trong core.

Mỗi phase báo cáo checklist. Unit test clean_rolling + cache key + paths.
Đọc AGENTS.md, README.md, HANDOFF, source Python trước khi code.
Bắt đầu Phase 0 workspace scaffold.
```

---

## 15. Quyết định còn mở (default nếu im)

| Câu hỏi | Default |
|---------|---------|
| Frontend desktop | React + TS + Vite + Tailwind |
| Installer Windows | Tauri NSIS |
| Config Windows | `%APPDATA%/viet-dub/` |
| CLI subcommand `list` | Optional; default `viet-dub` không args = process all |
| Windows CLI binary | Không ship MVP |
| Edge TTS / Translate | Sau MVP |
| Multi-select desktop | Có, chạy tuần tự |
| Linux FFmpeg bundle | Không MVP (system PATH) |

Hết handoff.

---

## 16. Trạng thái triển khai (cập nhật sau Codex + tiếp nối)

| Phase | Trạng thái | Ghi chú |
|-------|------------|---------|
| 0 Workspace | **Done** | `viet-dub-core`, `viet-dub-cli`, `apps/desktop` |
| 1 Paths/scan | **Done** | unit tests + CLI |
| 2 clean_rolling | **Done** | parity Python `diff` empty trên fixture UML |
| 3 TTS Azure + cache | **Done** | cache key tương thích Python; E2E reuse 69 seg |
| 4 CLI orchestrator + FFmpeg | **Done** | E2E: `viet-dub --force "UML/UML 2 State Machine Diagrams.mp4"` → `outputs/...vi-dub.mp4` (av1+aac, lang `vie`) |
| 5 Desktop Windows | **Code ready / ship on Windows** | UI + commands + icons + cancel; `npm run build` OK trên Linux. Installer: chạy `prepare-ffmpeg-windows.ps1` + `npm run tauri build` **trên Windows** (VPS Linux thiếu gdk/WebView cho full Tauri link). |
| 6 Perf | **Not started** | đúng plan — sau parity |

Binary CLI: `cargo build --release -p viet-dub-cli` → `./target/release/viet-dub`
