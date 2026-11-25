# Third-Party Notices

## FFmpeg

The Windows installer bundles the Gyan FFmpeg 9.1 essentials x64 build:

- Build: `ffmpeg-9.1-essentials_build.zip`
- SHA-256: `b9bbbb3acece000cbeec994f856649354e52c773807841e78908374b24d658fd`
- Binary mirror: <https://github.com/GyanD/codexffmpeg/releases/tag/9.1>
- Catalog: <https://www.gyan.dev/ffmpeg/builds/>
- FFmpeg source commit recorded in the verified vendor archive's `README.txt`:
  <https://github.com/FFmpeg/FFmpeg/commit/74e4393595>
- Source snapshot: <https://github.com/FFmpeg/FFmpeg/archive/74e4393595.tar.gz>
- Includes `librubberband` (required for tempo adjustment)

The build is distributed under GNU GPL version 3. Its `LICENSE` and vendor
`README.txt` are included beside the executables in the application resources.
When publishing an installer, publish or otherwise provide the corresponding
source alongside it as required by the GPL. The upstream FFmpeg tree alone is
not a complete source package for this build: include the exact linked library
sources, patches, license texts, and build instructions/configuration as well.

## yt-dlp

The Windows installer bundles the pinned `2026.11.23` x64 `yt-dlp.exe` downloaded from the
[official yt-dlp release](https://github.com/yt-dlp/yt-dlp/releases). The build
script verifies it against the release's `SHA2-256SUMS` file. The standalone
executable includes GPLv3+ components; its corresponding source and notices
are available from the yt-dlp project. The installer includes the pinned
`LICENSE`, `THIRD_PARTY_LICENSES.txt`, and `BUILD-INFO.txt` alongside the binary.

## Upstream source links

- FFmpeg: <https://github.com/FFmpeg/FFmpeg>
- yt-dlp: <https://github.com/yt-dlp/yt-dlp>

These are upstream project links. Complete corresponding source for the exact
bundled binaries and all dependencies has not been verified. Existing releases
use different binary versions and must be assessed separately.
