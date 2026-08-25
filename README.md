# Dubora

Create Vietnamese dubbed videos from video files and matching SRT subtitles.

Author: Hoàng Chiều Nguyễn Tuấn  
Email: tuan@hoangtuan.dev  
GitHub: [hoangtuan2k5](https://github.com/hoangtuan2k5)  
Repository: [hoangtuan2k5/dubora](https://github.com/hoangtuan2k5/dubora)

The pipeline:

1. Read source video from `assets/video/`.
2. Find the matching `.srt` file in `assets/srt/`.
3. Clean rolling/duplicated subtitles.
4. Treat cleaned subtitles as Vietnamese by default.
5. Generate Vietnamese voiceover with Azure Florian Dragon HD Latest.
6. Build final MP4 with Vietnamese audio only.
7. Copy the cleaned Vietnamese SRT beside the MP4 with the same basename for player auto-loading.

The Rust implementation ships one shared pipeline through:

- `dubora`, a Linux CLI in `crates/dubora-cli`
- `Dubora`, a Windows Tauri 2 desktop app in `apps/desktop`
- `dubora-core`, the library used by both frontends

The Python parity reference lives under `python_reference/`. Root-level `.py`
files remain as compatibility entry points, so existing commands such as
`python3 run_dub.py` continue to work.

## Rust CLI (Linux)

Prerequisites:

- Rust 1.85+
- FFmpeg and FFprobe available in `PATH`
- FFmpeg must include the `rubberband` audio filter
- `yt-dlp` available in `PATH` to import a YouTube video and its subtitle

Build the release binary:

```bash
cargo build --release -p dubora-cli
```

Run it from a project root or pass `--root` explicitly:

```bash
export AZURE_KEY=...
export AZURE_REGION=southeastasia
export GEMINI_API_KEYS=key_1,key_2

./target/release/dubora
./target/release/dubora "course/part/lesson.mp4"
./target/release/dubora --force "course/part/lesson.mp4"
./target/release/dubora --force-tts "course/part/lesson.mp4"
./target/release/dubora --voice vi-VN-HoaiMyNeural --voice-style chat "course/part/lesson.mp4"
./target/release/dubora --root /data/courses/my-course
```

`<root>/.env` is also supported (`AZURE_KEY`, `AZURE_REGION`, `AZURE_VOICE`,
`AZURE_VOICE_STYLE`, `GEMINI_API_KEYS`). Use
`--translate` to translate and polish English SRT files with Gemini 3.5 Flash-Lite. Multiple
Gemini keys are used round-robin once per logical request.

### Natural Voice

Natural Voice is opt-in. Gemini creates structured Speech Plans and Dubora renders only the SSML
controls supported by the selected Azure voice. Timing-capable voices use source-aligned whole
utterances; timing-unsupported Dragon HD voices use one continuous REST utterance per source
narration block and do not claim semantic-anchor alignment. It requires `GEMINI_API_KEYS` even when
the input SRT is already Vietnamese.

```bash
# Balanced is the default: move an elastic block by at most 250 ms inside its sync island.
./target/release/dubora --natural-voice "course/part/lesson.mp4"

# Prefer exact starts, or allow up to 500 ms of local movement for elastic explanation.
./target/release/dubora --natural-voice --natural-mode sync "course/part/lesson.mp4"
./target/release/dubora --natural-voice --natural-mode natural "course/part/lesson.mp4"

# Translate English subtitles first, then build the Natural Voice Speech Plan from the indexed
# Vietnamese SRT.
./target/release/dubora --translate --natural-voice "course/part/lesson.mp4"

# Keep raw Natural Voice TTS files without TailOnly pause-fit recovery.
./target/release/dubora --natural-voice --no-pause-fit "course/part/lesson.mp4"

# Optional audio polish; every flag is off by default.
./target/release/dubora --duck-original --voiceover-eq --loudness-normalize "course/part/lesson.mp4"

# Optional source-word timing experiment (uploads a temporary mono track to Azure Speech).
# The default remains caption-derived and does not upload source audio.
DUBORA_SOURCE_LOCALE=en-US ./target/release/dubora --natural-voice \
  --source-timing azure-fast "course/part/lesson.mp4"
```

Natural Voice applies TailOnly encoder lead/tail recovery by default. Use
`--no-pause-fit` or `DUBORA_INTERNAL_PAUSE_FIT=0` to restore raw TTS timing.
Use `DUBORA_INTERNAL_JOIN_FADES=0` only to restore the pre-PR4 mixer joins.
`--duck-original` keeps the source audio bed at -22 dB, `--voiceover-eq` applies an 80 Hz
voiceover high-pass, and `--loudness-normalize` targets -17 LUFS for voiceover only. These
audio-polish options are opt-in and remain off by default.

For timing-capable voices, Dubora synthesizes each source-aligned utterance once and measures the
whole island before choosing a follow-up. A block may use at most one mutually exclusive follow-up:
a supported timing repair, or one batched concise revision followed by one whole-utterance
resynthesis. Accepted audio uses final time-stretch within `1.13x`. Unresolved anchors, structural
timing conflicts, or infeasible islands mark the render `reviewRequired`; the aligned path never
splits or compacts an utterance into segment files. Dubora never overlaps speech or silently
truncates content. Source-aligned diagnostics report `granular`, `captionDerived`, or `cueOnly`
timing evidence. Dragon HD uses punctuation and supported semantic breaks only; no fake prosody or
timing-event alignment is claimed. A continuous block that remains over the `1.13x` cap becomes
`reviewRequired` as one whole block, never a mixture of `utterance_*` and `seg_*` files.

The source-aligned path keeps one Rust-owned timeline: Gemini chooses wording and delivery only;
Rust owns source timing, semantic-anchor windows, allocation, and SSML rendering. Each run writes
`source_timeline.json`, `render_manifest.json`, `sync_report.json`, and secret-free
`provider_metrics.json` under the ignored `work/` directory. A `reviewRequired` manifest is never
sent to FFmpeg, so an uncertain island cannot overwrite a previous output.
The CLI progress event and `sync_report.json` include the affected entry and `mm:ss.mmm` review
window so the region can be checked in the existing player.
To calculate a local Gemini estimate, set `DUBORA_GEMINI_INPUT_USD_PER_MILLION` and
`DUBORA_GEMINI_OUTPUT_USD_PER_MILLION`; otherwise the report explicitly records that provider
rates are unavailable.

```bash
export DUBORA_FFMPEG=/usr/bin/ffmpeg
export DUBORA_FFPROBE=/usr/bin/ffprobe
```

Azure is the Rust provider; Edge TTS remains in the Python reference.

The `--source-timing azure-fast` CLI option and the Desktop **Azure Fast granular** selector enable
the same opt-in source-word timing path. Dubora extracts a temporary
16 kHz mono WAV under ignored work/, requests Azure Fast Transcription word timestamps, aligns
recognized words to the source SRT with bounded matching, and interpolates unmatched spans only
after the coverage gate passes. It keeps the original subtitle text/timestamps unchanged. The
transcript is cached by source-video metadata and locale; a provider error or low alignment
coverage falls back to the normal caption-derived or cue-only timeline and records the reason in
source_timeline.json.
For translated or otherwise low-coverage input, the fallback may remain cueOnly and project only a
unique exact required action anchor; Dubora never reports that projection as granular timing.
Azure receives the extracted audio only in this opt-in mode, so review the service's retention and
regional data-processing policy before enabling it. Set DUBORA_AZURE_STT_USD_PER_AUDIO_HOUR when
you want provider_metrics.json to include a local transcription cost estimate; otherwise it
reports provider_rate_unavailable and still records audio duration, bytes, latency, and cache hits.

Verify:

```bash
cargo test -p dubora-core
ffmpeg -filters | grep rubberband
```

## Windows Desktop

The desktop app includes a **Tải từ YouTube** field at the top of **Thư viện**.
Paste a `youtube.com` or `youtu.be` URL after choosing a project root, then choose a
quality from lowest through 1080p or highest. It saves an MP4 under `assets/video/`
and one matching SRT under `assets/srt/`. Subtitle language
fallback is English, European languages, Chinese, Portuguese, Japanese, Korean,
Indonesian, Hindi, Arabic, then any other available language.

### Dubora Hub and library organization

**Dubora Hub** is the first item in the desktop sidebar. It plays a source video
inside the app with its matching input SRT, native volume controls, 0.5x–2x speed
selection, seeking, and fullscreen. The Library also has a play button for each
video, so a video can be opened without leaving the file-management workflow.

The Library supports list and grid views, sorting by name or added date, and
grouping by name, added date, course, or no grouping. A course is the first
directory under `assets/video/`; for example, `assets/video/Rust Basics/lesson-01.mp4`
appears under the `Rust Basics` course group. Grid view uses the video frame directly
and does not generate or persist thumbnail files.

Create a playlist in the Library or Hub, select that playlist, then select videos in
the Library and use **Add**. Playlists are stored locally in the Dubora app config
and are scoped to their project root. Renaming or deleting a video also updates its
playlist entries.

### Build via GitHub Actions (recommended)

Workflow: `.github/workflows/build-windows.yml` (runs on `windows-latest`).
Each successful push to `main` builds both `x64` and `x86` targets, then creates
a GitHub Release tagged `v<app-version>-build.<run-number>` with the NSIS
installers and portable `.exe` files attached. Manual workflow runs upload
artifacts but do not publish a release.

```bash
# After the repo is on GitHub:
gh workflow run "Build Windows"
gh run watch
# Download artifacts when finished:
gh run download -n dubora-windows-x64
gh run download -n dubora-windows-x86
```

Release assets include architecture-specific NSIS installers
(`Dubora-x64-installer-*.exe`, `Dubora-x86-installer-*.exe`) and raw app
binaries when present.

### Build locally on Windows

```powershell
.\scripts\prepare-ffmpeg-windows.ps1
cd apps\desktop
npm install
npm run tauri build
```

The preparation script downloads the pinned Gyan FFmpeg 8.1.2 essentials build,
verifies its checksum, stages binaries under
`apps/desktop/src-tauri/resources/ffmpeg/`, and asserts the `rubberband` filter
exists. See `THIRD_PARTY_NOTICES.md` before distributing an installer.

The desktop app stores project root and Azure settings under the local Tauri
app config directory. UI copy is Vietnamese; icons use Tabler Icons.
After loading the Azure voice list, selecting a voice exposes its supported expressive
styles and Natural Voice capabilities (emphasis, prosody, semantic breaks, and style). The `vi-VN`
filter also includes multilingual voices that support Vietnamese. In **Thư viện**, enable
**Natural Voice** and choose Sync, Balanced, or Natural timing. Natural Voice needs at least one
Gemini key stored in **Thiết lập → Google AI Studio**. Desktop also exposes the source timing
selector next to the Natural Voice mode; Caption timing remains the default.

Dev UI only (no native shell) from `apps/desktop`:

```bash
npm install
npm run build
```

## Python Reference Setup

The reference implementation lives in `python_reference/dubora_reference/`;
the root-level Python files are compatibility shims for existing commands.
Its regression tests live in `python_reference/tests/` and remain separate from
the production Rust pipeline.

Prerequisites:

- Python 3.10+
- FFmpeg available in `PATH`
- FFmpeg must include the `rubberband` audio filter

Verify FFmpeg:

```bash
ffmpeg -version
ffmpeg -filters | grep rubberband
```

On Windows PowerShell, use:

```powershell
ffmpeg -version
ffmpeg -filters | findstr rubberband
```

Install Python dependencies from the project folder.

Ubuntu/macOS:

```bash
cd ~/dubora
python3 -m venv .venv
source .venv/bin/activate
python -m pip install -r requirements.txt
```

Windows PowerShell:

```powershell
cd dubora
py -3 -m venv .venv
.\.venv\Scripts\Activate.ps1
python -m pip install -r requirements.txt
```

If PowerShell blocks venv activation, run this in the same terminal first:

```powershell
Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass
```

Add Azure Speech credentials to `.env` in the project folder:

```text
AZURE_KEY=...
AZURE_REGION=southeastasia
GEMINI_API_KEYS=key_1,key_2
```

The default Azure region is `southeastasia`.

To create a Gemini key, open [Google AI Studio API Keys](https://aistudio.google.com/app/apikey),
select **Create API key**, then add it to `GEMINI_API_KEYS`. Keep keys out of source control.
The desktop app accepts the same list in **Thiết lập → Google AI Studio**.

## Folder Layout

```text
assets/
  video/
    [NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/
      part1/
        1. Introduction.mp4
  srt/
    [NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/
      part1/
        1. Introduction.srt
outputs/
work/
```

Put videos under `assets/video/`.

Put subtitles under `assets/srt/`.

The video and SRT should have the same relative path and basename:

```text
assets/video/[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.mp4
assets/srt/[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.srt
```

If the subtitle course folder has a `CC-` prefix, that is also supported:

```text
assets/video/[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.mp4
assets/srt/CC-[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.srt
```

## Run

Always run commands from `~/dubora`:

```bash
cd ~/dubora
```

Process every video recursively:

```bash
python3 run_dub.py
```

This scans:

```text
assets/video/**/*.mp4
assets/video/**/*.mov
assets/video/**/*.mkv
assets/video/**/*.webm
```

Process one video:

```bash
python3 run_dub.py "[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.mp4"
```

If the SRT is English and needs translation, opt in explicitly:

```bash
python3 run_dub.py --translate "[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.mp4"
```

Regenerate existing TTS audio for a video:

```bash
python3 run_dub.py --force-tts "[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.mp4"
```

Regenerate existing TTS audio for all videos:

```bash
python3 run_dub.py --force-tts
```

Output:

```text
outputs/[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.vi-dub.mp4
outputs/[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.vi-dub.srt
```

The default TTS provider is Azure Speech with:

```text
de-DE-Florian:DragonHDLatestNeural
```

Azure voices that expose expressive styles can use `AZURE_VOICE_STYLE` or the CLI
`--voice-style` option. The desktop Settings screen lists styles supported by the selected voice.
IP addresses are spoken character by character: `192.168.0.1` becomes
`một chín hai chấm một sáu tám chấm không chấm một`, and `2001:db8::1` uses
Vietnamese digit/hex names with `hai chấm` for `:`.

You can still run the old Edge provider if needed:

```bash
python3 run_dub.py --provider edge
```

## TTS Pronunciation

Subtitles keep technical terms in English. The default Azure Florian provider
reads mixed Vietnamese and English terms directly.

The legacy Edge provider still uses a pronunciation map so Vietnamese voice does
not spell acronyms awkwardly.

Examples:

```text
CRUD    -> cờ rút
REST    -> rét
RESTful -> rét phồ
API     -> ây pi ai
JPA     -> jay pi ây
SQL     -> ét quy eo
MVC     -> em vi xi
AOP     -> ây ô pi
```

This only changes the spoken text sent to TTS. It does not change the SRT text.

## Matching Rules

For a video:

```text
assets/video/<course>/<part>/<section>/<lesson>.mp4
```

the runner first tries:

```text
assets/srt/<course>/<part>/<section>/<lesson>.srt
```

then tries the `CC-` folder fallback:

```text
assets/srt/CC-<course>/<part>/<section>/<lesson>.srt
```

If no matching SRT exists, the script stops and prints the paths it tried.

## Notes

- `work/` stores intermediate cleaned/translated SRT, TTS segments, and voiceover audio.
- By default, input SRT files are assumed to already be Vietnamese.
- `outputs/` stores final dubbed videos and matching `*.vi-dub.srt` subtitle files.
- By default, the final video keeps only the original video stream and the Vietnamese audio track;
  opt-in audio polish can also retain a quiet source bed.
- Output keeps the same folder structure as `assets/video/`.
