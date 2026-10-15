# Dubora

**Tiếng Việt** | [English](README.en.md)

Tạo video lồng tiếng Việt từ video và phụ đề SRT tương ứng.

Tác giả: Hoàng Chiều Nguyễn Tuấn\
Email: tuan@hoangtuan.dev\
GitHub: [hoangtuan2k5](https://github.com/hoangtuan2k5)\
Kho mã nguồn: [hoangtuan2k5/dubora](https://github.com/hoangtuan2k5/dubora)

Quy trình xử lý:

1. Đọc video nguồn trong `assets/video/`.
2. Tìm file `.srt` tương ứng trong `assets/srt/`.
3. Làm sạch phụ đề cuộn và nội dung trùng lặp.
4. Mặc định coi phụ đề đã làm sạch là tiếng Việt.
5. Sinh giọng lồng tiếng bằng Azure Florian Dragon HD Latest.
6. Tạo MP4 với hình ảnh gốc và âm thanh tiếng Việt.
7. Ghi phụ đề tiếng Việt cạnh MP4, cùng tên gốc để trình phát tự nhận diện.
   Natural Voice gộp câu hoàn chỉnh và đặt thời gian theo lời nói đã render;
   pipeline cũ sao chép phụ đề tiếng Việt đã làm sạch.

Bản Rust dùng chung một pipeline cho:

- `dubora`: CLI Linux trong `crates/dubora-cli`.
- `Dubora`: ứng dụng desktop Windows dùng Tauri 2 trong `apps/desktop`.
- `dubora-core`: thư viện xử lý chung của CLI và desktop.

Bản Python tham chiếu nằm trong `python_reference/`. Các file `.py` ở thư mục
gốc vẫn giữ vai trò entry point tương thích, nên các lệnh như
`python3 run_dub.py` vẫn hoạt động.

## CLI Rust (Linux)

Yêu cầu:

- Rust 1.85 trở lên.
- FFmpeg và FFprobe có trong `PATH`.
- FFmpeg có filter âm thanh `rubberband`.
- `yt-dlp` có trong `PATH` nếu cần nhập video và phụ đề từ YouTube.

Build binary release:

```bash
cargo build --release -p dubora-cli
```

Chạy từ thư mục gốc dự án hoặc chỉ định bằng `--root`:

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

Cũng có thể cấu hình trong `<root>/.env` với các biến `AZURE_KEY`, `AZURE_REGION`,
`AZURE_VOICE`, `AZURE_VOICE_STYLE` và `GEMINI_API_KEYS`. Bật `--translate` để dịch
và trau chuốt phụ đề tiếng Anh sang tiếng Việt bằng Gemini 3.5 Flash-Lite.
Nếu có nhiều Gemini key, ứng dụng luân phiên key theo từng yêu cầu logic.

### Natural Voice

Natural Voice là tính năng tùy chọn. Gemini tạo Speech Plan có cấu trúc, sau đó
Dubora chỉ render các điều khiển SSML mà giọng Azure đã chọn hỗ trợ. Giọng có
timing event dùng các đoạn nói hoàn chỉnh theo timeline nguồn. Dragon HD không
hỗ trợ timing event dùng một lượt tổng hợp REST liên tục cho mỗi khối lời dẫn
nguồn, không khẳng định đã căn chỉnh semantic anchor. Tính năng này cần
`GEMINI_API_KEYS` ngay cả khi SRT đầu vào đã là tiếng Việt.

```bash
# Balanced mặc định: cho phép dịch chuyển khối linh hoạt tối đa 250 ms trong sync island.
./target/release/dubora --natural-voice "course/part/lesson.mp4"

# Giữ thời điểm bắt đầu chính xác, hoặc cho phép dịch chuyển phần giải thích tối đa 500 ms.
./target/release/dubora --natural-voice --natural-mode sync "course/part/lesson.mp4"
./target/release/dubora --natural-voice --natural-mode natural "course/part/lesson.mp4"

# Dịch phụ đề tiếng Anh trước, rồi tạo Speech Plan từ SRT tiếng Việt đã đánh chỉ mục.
./target/release/dubora --translate --natural-voice "course/part/lesson.mp4"

# Giữ file TTS gốc, không thu hồi khoảng lặng đầu/cuối bằng TailOnly pause-fit.
./target/release/dubora --natural-voice --no-pause-fit "course/part/lesson.mp4"

# Các tùy chọn xử lý âm thanh đều mặc định tắt.
./target/release/dubora --duck-original --voiceover-eq --loudness-normalize "course/part/lesson.mp4"

# Thử nghiệm timing từng từ nguồn: gửi một track mono tạm thời lên Azure Speech.
# Mặc định lấy timing từ phụ đề và không gửi âm thanh nguồn.
DUBORA_SOURCE_LOCALE=en-US ./target/release/dubora --natural-voice \
  --source-timing azure-fast "course/part/lesson.mp4"
```

Natural Voice mặc định dùng TailOnly để thu hồi khoảng đệm đầu/cuối của file TTS.
Dùng `--no-pause-fit` hoặc `DUBORA_INTERNAL_PAUSE_FIT=0` để dùng timing TTS gốc.
Chỉ dùng `DUBORA_INTERNAL_JOIN_FADES=0` nếu cần khôi phục cách nối clip trước PR4.
`--duck-original` giữ âm thanh nguồn ở mức -22 dB; `--voiceover-eq` áp dụng
high-pass 80 Hz; `--loudness-normalize` nhắm mức -17 LUFS cho riêng voiceover.
Tất cả tùy chọn xử lý âm thanh này đều mặc định tắt.

Với giọng có timing event, Dubora tổng hợp từng đoạn nói theo nguồn rồi đo cả
sync island trước khi chọn bước xử lý tiếp. Mỗi khối chỉ có tối đa một bước:
sửa timing bằng điều khiển được hỗ trợ, hoặc viết gọn theo batch rồi tổng hợp
lại toàn bộ đoạn nói. Audio được chấp nhận có time-stretch tối đa `1.13x`.
Xung đột cấu trúc timing, island không thể phân bổ hoặc ràng buộc căn chỉnh bắt
buộc chưa giải quyết sẽ đặt bản render thành `reviewRequired`. Độ lệch sớm của
thao tác ở cuối câu có thể được chấp nhận nếu duration an toàn; độ lệch đo được
vẫn được ghi trong diagnostics.

Dragon HD ghi nhận semantic-anchor mapping còn thiếu trong diagnostics và có
thể xuất kết quả khi duration đo được phù hợp, không khẳng định semantic
alignment. Nhánh aligned không chia hoặc ép đoạn nói thành các file segment.
Dubora không chồng lời nói hoặc âm thầm cắt nội dung. Diagnostics của nhánh
source-aligned ghi rõ loại bằng chứng timing: `granular`, `captionDerived`
hoặc `cueOnly`. Dragon HD dùng dấu câu, semantic break và điều khiển phát âm
được hỗ trợ; không giả lập prosody hoặc căn chỉnh bằng timing event. Khối nói
liên tục còn vượt `1.13x` sẽ thành `reviewRequired` nguyên khối, không trộn
file `utterance_*` với `seg_*`.

Nhánh source-aligned giữ một timeline do Rust quản lý. Gemini chỉ chọn cách
diễn đạt và thể hiện; Rust quản lý timing nguồn, cửa sổ semantic anchor, phân
bổ và render SSML. Mỗi lần chạy ghi `source_timeline.json`,
`render_manifest.json`, `sync_report.json` và `provider_metrics.json` không
chứa secret trong thư mục `work/` đã được Git bỏ qua. Manifest
`reviewRequired` không được chuyển tới FFmpeg, nên không ghi đè output trước đó.
Sự kiện tiến độ CLI và `sync_report.json` ghi đoạn bị ảnh hưởng cùng khoảng
thời gian `mm:ss.mmm` để kiểm tra bằng trình phát hiện có.

Để ước tính chi phí Gemini cục bộ, đặt `DUBORA_GEMINI_INPUT_USD_PER_MILLION`
và `DUBORA_GEMINI_OUTPUT_USD_PER_MILLION`. Nếu không có, báo cáo ghi rõ chưa
có đơn giá nhà cung cấp.

```bash
export DUBORA_FFMPEG=/usr/bin/ffmpeg
export DUBORA_FFPROBE=/usr/bin/ffprobe
```

Bản Rust dùng Azure; Edge TTS vẫn có trong bản Python tham chiếu.

Option CLI `--source-timing azure-fast` và lựa chọn desktop **Azure Fast granular**
cùng bật chế độ timing từng từ nguồn. Dubora trích WAV mono 16 kHz tạm thời
trong `work/`, yêu cầu Azure Fast Transcription trả timestamp từng từ, đối
chiếu từ nhận dạng với SRT nguồn theo phạm vi giới hạn, rồi chỉ nội suy phần
thiếu sau khi đạt ngưỡng độ phủ. Source timing không sửa nội dung hay timestamp
của phụ đề đầu vào. Natural Voice tạo companion SRT riêng từ các câu hoàn
chỉnh đã render, giữ cách viết kỹ thuật hiển thị như IP, URL, đường dẫn và số
phiên bản model.

Transcript được cache theo metadata video và locale. Khi provider lỗi hoặc
độ phủ căn chỉnh thấp, ứng dụng dùng lại timeline từ phụ đề hoặc cue và ghi
lý do trong `source_timeline.json`. Với đầu vào đã dịch hoặc độ phủ thấp,
fallback có thể vẫn là `cueOnly` và chỉ ánh xạ một required action anchor
khớp chính xác, duy nhất; Dubora không báo ánh xạ đó là granular timing.

Azure chỉ nhận audio trích xuất khi bật chế độ này. Hãy xem chính sách lưu
trữ và xử lý dữ liệu theo vùng của dịch vụ trước khi bật. Đặt
`DUBORA_AZURE_STT_USD_PER_AUDIO_HOUR` nếu muốn `provider_metrics.json` có
ước tính chi phí transcription cục bộ; nếu không, báo cáo ghi
`provider_rate_unavailable` và vẫn thống kê thời lượng audio, số byte,
latency và cache hit.

Kiểm tra:

```bash
cargo test -p dubora-core
ffmpeg -filters | grep rubberband
```

## Desktop Windows

Ứng dụng có ô **Tải từ YouTube** ở đầu **Thư viện**. Sau khi chọn thư mục gốc
dự án, dán URL `youtube.com` hoặc `youtu.be` rồi chọn chất lượng từ thấp nhất,
1080p đến cao nhất. Ứng dụng lưu MP4 vào `assets/video/` và SRT tương ứng vào
`assets/srt/`. Thứ tự chọn ngôn ngữ phụ đề dự phòng là tiếng Anh, các ngôn ngữ
châu Âu, tiếng Trung, Bồ Đào Nha, Nhật, Hàn, Indonesia, Hindi, Ả Rập, rồi các
ngôn ngữ còn lại có sẵn.

### Dubora Hub và tổ chức thư viện

**Dubora Hub** là mục đầu tiên ở sidebar. Hub phát video nguồn trong ứng dụng
với SRT đầu vào tương ứng, điều khiển âm lượng, tốc độ 0.5x–2x, tua và toàn
màn hình. Thư viện cũng có nút phát cho từng video để mở ngay trong quá trình
quản lý file.

Thư viện hỗ trợ chế độ danh sách hoặc lưới, sắp xếp theo tên hoặc ngày thêm,
và nhóm theo tên, ngày thêm, khóa học hoặc không nhóm. Khóa học là thư mục
đầu tiên dưới `assets/video/`; ví dụ `assets/video/Rust Basics/lesson-01.mp4`
thuộc nhóm `Rust Basics`. Chế độ lưới dùng trực tiếp khung hình video, không
sinh hoặc lưu file thumbnail.

Tạo playlist trong Thư viện hoặc Hub, chọn playlist đó, rồi chọn video trong
Thư viện và bấm **Add**. Playlist được lưu cục bộ trong config Dubora, riêng
theo từng thư mục gốc dự án. Đổi tên hoặc xóa video cũng cập nhật mục tương
ứng trong playlist.

### Build bằng GitHub Actions (khuyến nghị)

Workflow `.github/workflows/build-windows.yml` chạy trên `windows-latest`.
Push vào `main` khớp bộ lọc đường dẫn sẽ build cả `x64` và `x86`. Job release
đọc mục phiên bản có số mới nhất trong `CHANGELOG.md`, tạo release
`v<version>` với NSIS installer và file `.exe` portable nếu release đó chưa
tồn tại. Việc phát hành cần source archive bên thứ ba đã được duyệt và
checksum như mô tả trong [Third-Party Notices](THIRD_PARTY_NOTICES.md).
Chạy workflow thủ công chỉ upload artifact, không tạo release.

```bash
# Sau khi repo đã có trên GitHub:
gh workflow run "Build Windows"
gh run watch
# Tải artifact khi hoàn tất:
gh run download -n dubora-windows-x64
gh run download -n dubora-windows-x86
```

Release asset gồm NSIS installer riêng cho từng kiến trúc
(`Dubora-x64-installer-*.exe`, `Dubora-x86-installer-*.exe`) và binary ứng
dụng riêng nếu có.

### Build cục bộ trên Windows

```powershell
.\scripts\prepare-ffmpeg-windows.ps1
.\scripts\prepare-ytdlp-windows.ps1
cd apps\desktop
npm install
npm run tauri build
```

Script chuẩn bị tải Gyan FFmpeg 8.1.2 essentials đã pin, xác minh checksum,
đặt binary vào `apps/desktop/src-tauri/resources/ffmpeg/` và kiểm tra filter
`rubberband`. Xem `THIRD_PARTY_NOTICES.md` trước khi phân phối installer.

Desktop lưu thư mục gốc dự án và thiết lập Azure trong thư mục config Tauri
cục bộ. Giao diện dùng tiếng Việt, icon dùng Tabler Icons. Sau khi tải danh
sách giọng Azure, chọn giọng để xem expressive style và các khả năng Natural
Voice được hỗ trợ: emphasis, prosody, semantic break và style. Bộ lọc `vi-VN`
cũng gồm các giọng đa ngôn ngữ hỗ trợ tiếng Việt.

Trong **Thư viện**, bật **Natural Voice** và chọn Sync, Balanced hoặc Natural.
Natural Voice cần ít nhất một Gemini key trong **Thiết lập → Google AI Studio**.
Desktop cũng có lựa chọn source timing cạnh chế độ Natural Voice;
Caption timing là mặc định.

Chỉ build giao diện, không chạy native shell, từ `apps/desktop`:

```bash
npm install
npm run build
```

## Thiết lập bản Python tham chiếu

Bản tham chiếu nằm trong `python_reference/dubora_reference/`. Các file
Python ở thư mục gốc là shim tương thích cho lệnh hiện có. Regression test
nằm trong `python_reference/tests/`, tách biệt với pipeline Rust chính.

Yêu cầu:

- Python 3.10 trở lên.
- FFmpeg có trong `PATH`.
- FFmpeg có filter âm thanh `rubberband`.

Kiểm tra FFmpeg:

```bash
ffmpeg -version
ffmpeg -filters | grep rubberband
```

Trên Windows PowerShell:

```powershell
ffmpeg -version
ffmpeg -filters | findstr rubberband
```

Cài dependency Python từ thư mục dự án.

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

Nếu PowerShell chặn kích hoạt venv, chạy lệnh sau trong cùng terminal:

```powershell
Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass
```

Thêm thông tin Azure Speech vào `.env` trong thư mục dự án:

```text
AZURE_KEY=...
AZURE_REGION=southeastasia
GEMINI_API_KEYS=key_1,key_2
```

Region Azure mặc định là `southeastasia`.

Để tạo Gemini key, mở [Google AI Studio API Keys](https://aistudio.google.com/app/apikey),
chọn **Create API key** rồi thêm vào `GEMINI_API_KEYS`. Không đưa key vào Git.
Desktop nhận cùng danh sách key trong **Thiết lập → Google AI Studio**.

## Cấu trúc thư mục

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

Đặt video trong `assets/video/`, phụ đề trong `assets/srt/`. Video và SRT phải
có cùng đường dẫn tương đối và tên gốc:

```text
assets/video/[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.mp4
assets/srt/[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.srt
```

Cũng hỗ trợ thư mục phụ đề khóa học có tiền tố `CC-`:

```text
assets/video/[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.mp4
assets/srt/CC-[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.srt
```

## Chạy bản Python tham chiếu

Luôn chạy từ `~/dubora`:

```bash
cd ~/dubora
```

Xử lý đệ quy tất cả video:

```bash
python3 run_dub.py
```

Lệnh này tìm các file:

```text
assets/video/**/*.mp4
assets/video/**/*.mov
assets/video/**/*.mkv
assets/video/**/*.webm
```

Xử lý một video:

```bash
python3 run_dub.py "[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.mp4"
```

Nếu SRT là tiếng Anh và cần dịch, bật tùy chọn:

```bash
python3 run_dub.py --translate "[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.mp4"
```

Sinh lại TTS cho một video:

```bash
python3 run_dub.py --force-tts "[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.mp4"
```

Sinh lại TTS cho tất cả video:

```bash
python3 run_dub.py --force-tts
```

Output:

```text
outputs/[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.vi-dub.mp4
outputs/[NEW] Spring Boot 3, Spring 6 & Hibernate for Beginners/part1/1. Introduction.vi-dub.srt
```

Provider TTS mặc định là Azure Speech với giọng:

```text
de-DE-Florian:DragonHDLatestNeural
```

Giọng Azure có expressive style dùng được `AZURE_VOICE_STYLE` hoặc option CLI
`--voice-style`. Màn hình thiết lập desktop liệt kê style mà giọng đã chọn
hỗ trợ. Địa chỉ IP được đọc từng ký tự: `192.168.0.1` thành
`một chín hai chấm một sáu tám chấm không chấm một`; `2001:db8::1` dùng tên
chữ số/hex tiếng Việt và `hai chấm` cho `:`.

Vẫn có thể dùng provider Edge cũ:

```bash
python3 run_dub.py --provider edge
```

## Phát âm TTS

Phụ đề giữ thuật ngữ kỹ thuật bằng tiếng Anh. Provider Azure Florian mặc định
đọc trực tiếp văn bản trộn tiếng Việt và tiếng Anh.

Provider Edge cũ dùng bảng thay thế phát âm để tránh đọc acronym không tự nhiên:

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

Việc thay thế chỉ tác động văn bản gửi tới TTS, không đổi nội dung SRT.

## Quy tắc ghép video với phụ đề

Với video:

```text
assets/video/<course>/<part>/<section>/<lesson>.mp4
```

Runner tìm trước ở:

```text
assets/srt/<course>/<part>/<section>/<lesson>.srt
```

Sau đó thử thư mục dự phòng `CC-`:

```text
assets/srt/CC-<course>/<part>/<section>/<lesson>.srt
```

Nếu không có SRT tương ứng, script dừng và in các đường dẫn đã thử.

## Ghi chú

- `work/` lưu SRT làm sạch/đã dịch, các đoạn TTS và audio voiceover trung gian.
- Mặc định coi SRT đầu vào đã là tiếng Việt.
- `outputs/` lưu video lồng tiếng và phụ đề `*.vi-dub.srt` tương ứng.
- Mặc định video cuối chỉ giữ hình ảnh gốc và audio tiếng Việt; các tùy chọn
  xử lý âm thanh cho phép giữ thêm âm thanh nguồn ở mức nhỏ.
- Output giữ cấu trúc thư mục tương ứng với `assets/video/`.
