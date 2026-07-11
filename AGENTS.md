# AGENTS.md

Hướng dẫn cho agent (AI) khi làm việc trong kho `viet-dub`.

Người dùng giao tiếp bằng tiếng Việt; README chính thức bằng tiếng Anh. Khi viết
commit/documentation mới, giữ phong cách của README (tiếng Anh, present tense).
Khi trả lời trực tiếp cho người dùng, dùng tiếng Việt.

## Mục đích

Tạo video lồng tiếng Việt (Vietnamese dub) từ file video và file SRT khớp nhau.
Pipeline dòng chảy:

```
assets/video/<rel>.mp4  +  assets/srt/<rel>.srt
  -> clean rolling subtitles
  -> [tùy chọn] translate en -> vi
  -> TTS từng segment (azure mặc định)
  -> ghép voiceover (ffmpeg + rubberband + adelay + amix)
  -> mux video gốc + voiceover aac
  -> outputs/<rel>.vi-dub.mp4
```

Entry point duy nhất: `run_dub.py`. Không gọi trực tiếp `build_audio.py` /
`build_video.py` trong pipeline bình thường — `run_dub.py` đã điều phối.

## Kiến trúc module

| File | Trách nhiệm | Ghi chú |
| --- | --- | --- |
| `run_dub.py` | Orchestrator chính. Tìm video↔srt, quản lý `work/`, gọi lần lượt các bước. | Mọi arg flag ở đây. |
| `clean_rolling.py` | Linearize rolling captions (suffix-prefix overlap), gộp câu liền nhau (gap<2s hoặc dur<4.5s, tối đa 22 từ / 8s). | Coi subtitle là rolling pattern YouTube. |
| `translate_srt.py` | Dịch en→vi qua `deep-translator` (Google). Bảo vệ thuật ngữ kỹ thuật bằng placeholder `ZXQTERM###QXZ`, dịch batch 20, có fallback per-segment và các sửa sai nhận dạng (YouTube nghe nhầm "vapor classes" -> "wrapper classes"). | Chỉ chạy khi `--translate`. |
| `generate_tts.py` | Sinh MP3 từng segment, cache sha256 + sidecar `.sha256`, retry 3, concurrency semaphore (azure/edge=4, gtts=2). Providers: azure, edge, gtts. | `is_fresh_audio_file` quyết định regenerate. |
| `build_audio.py` | Xây voiceover: `rubberband=tempo` time-stretch khi audio dài quá slot, `atrim`, `adelay`, `amix=normalize=0`, `alimiter`. Ghi `filter_complex.txt` + runner script. | **Yêu cầu FFmpeg có filter `rubberband`.** |
| `build_video.py` | Mux: `0:v:0` copy + `1:a:0` aac 192k, `apad` + `-shortest`, metadata lang `vie`, default disposition. | Không re-encode video. |
| `drive_helper.py` | Upload/download Google Drive qua service account (`~/.hermes/google_service_account.json`). | File độc lập, không nằm trong pipeline chính. |
| `scratch/` | Script phụ/test (extract_srt, merge_srt, process_srt) + file text tạm. | Không production, không import. |

## Quy ước & gotchas quan trọng

- **Provider mặc định = `azure`**, voice `de-DE-Florian:DragonHDLatestNeural`, song
  SSML `xml:lang="vi-VN"`. Voice này đọc tốt cả tiếng Việt lẫn thuật ngữ Anh.
  Provider `edge` (giọng `vi-VN-NamMinh`) còn dùng `TTS_PRONUNCIATIONS` để sửa
  phát âm acronym; `azure` thì gửi text thẳng. Không đổi voice mặc định trừ khi
  được yêu cầu.
- **`--translate` là opt-in.** Mặc định SRT được coi là đã là tiếng Việt → copy thẳng.
  Chỉ bật dịch khi SRT thực sự là tiếng Anh.
- **Cache freshness** (`is_fresh_audio_file`): kiểm tra file tồn tại + duration > 0.05s,
  rồi so khớp sidecar `<path>.sha256` với `tts_cache_key(provider,text)`. Khi sửa
  logic provider/voice, phải đảm bảo cache key thay đổi để invalidate — commit gần
  đây (83f2f84) đã sửa lỗi cache cũ.
- **`--force-tts`** xóa thư mục `tts/` rồi sinh lại; dùng khi thay đổi giọng/provider.
- **Đường dẫn có dấu ngoặc vuông / khoảng trắng** (vd `[NEW] Spring Boot...`).
  Khi gọi subprocess hoặc glob, dùng `Path` và list arg, không拼接 chuỗi shell.
  `safe_work_name` sanitizer thay ký tự lạ thành `_` cho thư mục `work/`.
- **Matching SRT**: thử `assets/srt/<rel>.srt` trước, rồi fallback `CC-<course>/...`.
  Không khớp ⇒ `SystemExit` in ra các path đã thử.
- **Region Azure** mặc định `southeastasia` (env `AZURE_REGION`).
  Azure key đọc từ `.env` (và fallback `/home/ubuntu/AI_news/.env`). Không hard-code key.
- **`work/` và `outputs/` đã gitignore**; `assets/video/*` và `assets/srt/*` cũng vậy
  (chỉ keep `.gitkeep`). Không commit media/intermediate.
- **`parse_srt`/`parse_original_youtube_srt`** có hai bản khác nhau (mỗi file định nghĩa riêng).
  Không gộp trừ phi có lý do — chúng phục vụ định dạng đầu vào khác nhau.

## Lệnh chạy & verify

Luôn chạy từ thư mục gốc `~/viet-dub`:

```bash
cd ~/viet-dub
# Toàn bộ video (đệ quy assets/video)
python3 run_dub.py
# Một video
python3 run_dub.py "<course>/<part>/<lesson>.mp4"
# SRT tiếng Anh cần dịch
python3 run_dub.py --translate "<rel>.mp4"
# Sinh lại TTS cho một / tất cả video
python3 run_dub.py --force-tts "<rel>.mp4"
# Provider legacy edge
python3 run_dub.py --provider edge
```

Kiểm tra FFmpeg có `rubberband` (bắt buộc cho `build_audio.py`):

```bash
ffmpeg -filters | grep rubberband
```

Kiểm tra một module độc lập (thường không cần — gốc qua `run_dub.py`):

```bash
python3 clean_rolling.py <in.srt> <out.srt>
python3 generate_tts.py azure <vi.srt> <tts_dir>
python3 build_audio.py <vi.srt> <tts_dir> <out.mp3> <work_dir>
python3 build_video.py <in.mp4> <voiceover.mp3> <out.mp4>
```

Không có test tự động / linter / test-suite trong dự án. Verify bằng cách chạy
pipeline trên ít nhất một video nhỏ và kiểm tra `outputs/.../<stem>.vi-dub.mp4`
hợp lệ (`ffprobe`/ phát được, có track audio `vie`).

## Khi chỉnh sửa

- **Surgical**: chỉ chạm vào phần phục vụ yêu cầu. Không refactor `parse_srt` trùng lặp
  hay "dọn dẹp" code lân cận.
- Thay provider/voice: cập nhật `tts_cache_key`/note để cache cũ bị invalidate.
- Thay logic clean/translate: kiểm tra pipeline end-to-end vì nó ảnh hưởng số
  đoạn → số file TTS → voiceover.
- Giữ arg parser và các path mặc định trong `run_dub.py` đồng bộ với README
  (README là nguồn user-facing; update cả hai khi đổi CLI).
- Mọi subprocess trong pipeline dùng list-arg + `check=True`; giữ pattern này.
- Tuân thủ commit convention Conventional Commits (`feat:`, `fix:`, `chore:`, ...).

## Phụ thuộc

`requirements.txt`: `deep-translator`, `edge-tts`, `gTTS`,
`google-api-python-client`, `google-auth`, `requests`. Python 3.10+, FFmpeg + `rubberband`.