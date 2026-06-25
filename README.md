# viet-dub

Create Vietnamese dubbed videos from video files and matching SRT subtitles.

Author: hoangtuan2k5

The pipeline:

1. Read source video from `assets/video/`.
2. Find the matching `.srt` file in `assets/srt/`.
3. Clean rolling/duplicated subtitles.
4. Treat cleaned subtitles as Vietnamese by default.
5. Generate Vietnamese voiceover with Azure Seraphina Dragon HD Latest.
6. Build final MP4 with Vietnamese audio only.

## Setup

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
cd ~/viet-dub
python3 -m venv .venv
source .venv/bin/activate
python -m pip install -r requirements.txt
```

Windows PowerShell:

```powershell
cd viet-dub
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
```

The default Azure region is `southeastasia`.

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

Always run commands from `~/viet-dub`:

```bash
cd ~/viet-dub
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
```

The default TTS provider is Azure Speech with:

```text
de-DE-Seraphina:DragonHDLatestNeural
```

You can still run the old Edge provider if needed:

```bash
python3 run_dub.py --provider edge
```

## TTS Pronunciation

Subtitles keep technical terms in English. The default Azure Seraphina provider
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
- `outputs/` stores final dubbed videos.
- The final video keeps only the original video stream and the Vietnamese audio track.
- Output keeps the same folder structure as `assets/video/`.
