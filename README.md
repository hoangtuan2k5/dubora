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

The Python files remain in the repository as the parity reference.

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
./target/release/dubora --root /data/courses/my-course
```

`<root>/.env` is also supported (`AZURE_KEY`, `AZURE_REGION`, `GEMINI_API_KEYS`). Use
`--translate` to translate and polish English SRT files with Gemini 3.5 Flash-Lite. Multiple
Gemini keys are used round-robin once per subtitle batch.

```bash
export DUBORA_FFMPEG=/usr/bin/ffmpeg
export DUBORA_FFPROBE=/usr/bin/ffprobe
```

Azure is the Rust provider; Edge TTS remains in the Python reference.

Verify:

```bash
cargo test -p dubora-core
ffmpeg -filters | grep rubberband
```

## Windows Desktop

The desktop app includes a **Tải từ YouTube** field at the top of **Thư viện**.
Paste a `youtube.com` or `youtu.be` URL after choosing a project root. It saves the
video under `assets/video/` and one matching SRT under `assets/srt/`. Subtitle language
fallback is English, European languages, Chinese, Portuguese, Japanese, Korean,
Indonesian, Hindi, Arabic, then any other available language.

### Build via GitHub Actions (recommended)

Workflow: `.github/workflows/build-windows.yml` (runs on `windows-latest`).

```bash
# After the repo is on GitHub:
gh workflow run "Build Windows"
gh run watch
# Download artifacts when finished:
gh run download -n dubora-windows
```

Artifacts include the NSIS installer (`Dubora_*.exe`) and the raw app binary
when present.

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

Dev UI only (no native shell) from `apps/desktop`:

```bash
npm install
npm run build
```

## Python Reference Setup

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
- The final video keeps only the original video stream and the Vietnamese audio track.
- Output keeps the same folder structure as `assets/video/`.
