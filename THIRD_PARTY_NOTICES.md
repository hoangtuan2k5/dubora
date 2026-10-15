# Third-Party Notices

## FFmpeg

The Windows installer bundles the Gyan FFmpeg 8.1.2 essentials x64 build:

- Build: `ffmpeg-8.1.2-essentials_build.zip`
- SHA-256: `db580001caa24ac104c8cb856cd113a87b0a443f7bdf47d8c12b1d740584a2ec`
- Binary mirror: <https://github.com/GyanD/codexffmpeg/releases/tag/8.1.2>
- Catalog: <https://www.gyan.dev/ffmpeg/builds/>
- FFmpeg source commit recorded in the verified vendor archive's `README.txt`:
  <https://github.com/FFmpeg/FFmpeg/commit/38b88335f9>
- Source snapshot: <https://github.com/FFmpeg/FFmpeg/archive/38b88335f9.tar.gz>
- Includes `librubberband` (required for tempo adjustment)

The build is distributed under GNU GPL version 3. Its `LICENSE` and vendor
`README.txt` are included beside the executables in the application resources.
When publishing an installer, publish or otherwise provide the corresponding
source alongside it as required by the GPL. The upstream FFmpeg tree alone is
not a complete source package for this build: include the exact linked library
sources, patches, license texts, and build instructions/configuration as well.

## yt-dlp

The Windows installer bundles the pinned `2026.08.19` x64 `yt-dlp.exe` downloaded from the
[official yt-dlp release](https://github.com/yt-dlp/yt-dlp/releases). The build
script verifies it against the release's `SHA2-256SUMS` file. The standalone
executable includes GPLv3+ components; its corresponding source and notices
are available from the yt-dlp project. The installer includes the pinned
`LICENSE`, `THIRD_PARTY_LICENSES.txt`, and `BUILD-INFO.txt` alongside the binary.

## Release source archive

Before publishing a new binary release, prepare a reviewed `third-party-source.zip`
covering **both** pinned vendor binaries, including bundled dependencies and their
build material. Record binary versions and SHA-256 hashes in the archive so they
can be compared with the staging scripts and installed `BUILD-INFO.txt` files.
Keep the archive available for recipients of those binaries.

Set the repository Actions variables `THIRD_PARTY_SOURCE_URL` (HTTPS download)
and `THIRD_PARTY_SOURCE_SHA256` (64 hexadecimal characters) only after that review.
The release workflow verifies the checksum and ZIP integrity, then attaches the
archive, this notice, and Dubora's MIT license to the same GitHub Release.
These mechanical checks do not establish source completeness; the review is required.

Existing v0.2.0/v0.2.1 binary releases predate this gate. Audit and supplement their
exact corresponding source before exposing those release assets publicly. A source
package for newer vendor versions does not cover older binaries automatically.
