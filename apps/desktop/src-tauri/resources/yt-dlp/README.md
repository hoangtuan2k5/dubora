# yt-dlp resource

`prepare-ytdlp-windows.ps1` downloads the pinned official Windows x64 `yt-dlp.exe`
here during the Windows build. The binary is ignored by Git and bundled by
Tauri under `yt-dlp/`, together with its license texts and build identity.
See `THIRD_PARTY_NOTICES.md` at the repository root for release source requirements.
