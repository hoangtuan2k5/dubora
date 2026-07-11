# Bundled FFmpeg

Run this from the repository root on Windows before building the installer:

```powershell
.\scripts\prepare-ffmpeg-windows.ps1
```

The script downloads the pinned Gyan FFmpeg 8.1.2 essentials x64 build
(from the `GyanD/codexffmpeg` mirror), verifies its SHA-256 checksum, copies
`ffmpeg.exe`, `ffprobe.exe`, `LICENSE`, and `README.txt` here, then asserts that
the `rubberband` filter is present.

Do not commit the downloaded binaries; they are gitignored.
