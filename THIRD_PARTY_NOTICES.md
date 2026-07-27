# Third-Party Notices

## FFmpeg

The Windows installer bundles the Gyan FFmpeg 8.1.2 essentials x64 build:

- Build: `ffmpeg-8.1.2-essentials_build.zip`
- SHA-256: `db580001caa24ac104c8cb856cd113a87b0a443f7bdf47d8c12b1d740584a2ec`
- Binary mirror: <https://github.com/GyanD/codexffmpeg/releases/tag/8.1.2>
- Catalog: <https://www.gyan.dev/ffmpeg/builds/>
- Corresponding source: <https://github.com/FFmpeg/FFmpeg/tree/n8.1.2>
- Includes `librubberband` (required for tempo adjustment)

The build is distributed under GNU GPL version 3. Its `LICENSE` and vendor
`README.txt` are included beside the executables in the application resources.
When publishing an installer, publish or otherwise provide the corresponding
source alongside it as required by the GPL.

## yt-dlp

The Windows installer bundles the current x64 `yt-dlp.exe` downloaded from the
[official yt-dlp release](https://github.com/yt-dlp/yt-dlp/releases). The build
script verifies it against the release's `SHA2-256SUMS` file. The standalone
executable includes GPLv3+ components; its corresponding source and notices
are available from the yt-dlp project.
