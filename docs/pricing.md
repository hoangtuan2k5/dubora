# Pricing Model — Dubora

Mô hình giá dựa trên **benchmark chi phí thật** (50 file SRT, 625 phút video, 8/2026).
Dữ liệu thô + script tái chạy: `benchmark/` (xem cuối trang).

## Chi phí thành phần (giá hiện tại, 8/2026)

### Biến đổi (trả theo mỗi video xử lý)

| Thành phần | Đơn giá | Ghi chú |
| --- | --- | --- |
| Azure TTS — neural | $15 / 1M chars | Giọng chuẩn |
| Azure TTS — HD premium | $48 / 1M chars | Giọng `de-DE-Florian:DragonHD` (3.2x neural) |

### DeepSeek V4 — dịch SRT

| Model | Loại token | Cũ / 1M | Mới off-peak / 1M | Mới peak / 1M |
| --- | --- | ---: | ---: | ---: |
| V4 Flash | Cache hit | $0.0028 | $0.007 | $0.014 |
| V4 Flash | Input cache miss | $0.14 | $0.22 | $0.44 |
| V4 Flash | Output | $0.28 | $0.66 | $1.32 |
| V4 Pro | Cache hit | $0.003625 | $0.022 | $0.044 |
| V4 Pro | Input cache miss | $0.435 | $0.66 | $1.32 |
| V4 Pro | Output | $0.87 | $1.98 | $3.96 |

Các bảng lợi nhuận dưới dùng **V4 Flash input cache miss**: mỗi SRT có nội dung khác nhau,
nên không nên hứa hẹn cache hit. Peak đắt gấp 2x off-peak; batch dịch nên chạy off-peak.
V4 Pro chưa là mặc định và không được dùng để định giá gói hiện tại.

### Cố định (trả hàng tháng)

| Khoản | Giá | Ghi chú |
| --- | --- | --- |
| VPS | ~$72.7 / tháng | 730 giờ |
| Domain | ~$28 / năm ≈ $2.33 / tháng | ~700k VND/năm |
| R2 storage | $0.015 / GB / tháng | Output video; 50GB ≈ $0.75/tháng |
| **Tổng cố định** | **~$76 / tháng** | |

## Kết quả benchmark (50 file, 625 phút video)

Mật độ nói: **avg 1,167 | median 968 | min 156 | max 2,868 | p95 2,716** (chars VI/phút video)

Chi phí `$/phút video` (tách riêng, đã hiệu chỉnh 8 file Serial bị cắt):

| | avg | median | min | max | p95 |
| --- | --- | --- | --- | --- | --- |
| AI dịch Flash (giá cũ) | 0.0010 | 0.0008 | 0.0002 | 0.0029 | 0.0023 |
| AI dịch Flash (off-peak) | 0.0022 | 0.0017 | 0.0004 | 0.0067 | 0.0053 |
| AI dịch Flash (peak) | 0.0045 | 0.0034 | 0.0009 | 0.0134 | 0.0106 |
| TTS neural $15 | 0.0212 | 0.0148 | 0.0046 | 0.0483 | 0.0466 |
| TTS HD $48 | 0.0679 | 0.0473 | 0.0147 | 0.1545 | 0.1493 |
| **TỔNG rẻ** (Flash off-peak + neural) | 0.0235 | 0.0165 | 0.0053 | 0.0513 | 0.0485 |
| **TỔNG đắt** (Flash peak + HD) | 0.0725 | 0.0507 | 0.0162 | 0.1589 | 0.1531 |

Tổng 625 phút: dịch Flash cũ $0.53, mới $1.14 off-peak hoặc $2.28 peak; TTS giữ nguyên
$17.23 neural hoặc $55.15 HD. Tổng biến đổi mới là **$18.37** (off-peak + neural) đến
**$57.43** (peak + HD). TTS vẫn chiếm khoảng **94–96%** chi phí.

## Cách tính giá trong app

**Credit theo chars, tách riêng theo giọng:**
- 1 credit = 1 char tiếng Việt (giọng neural)
- Credit HD = credit neural ÷ 3.2 (vì $48/$15)

**Quy trình:**
1. User upload SRT + chọn giọng → ước lượng `chars_EN × 1.09 × rate` (giá dự kiến)
2. Sau dịch → đếm chars VI thật → trừ credit chính xác
3. Quote theo chars + margin (không quote theo phút)

**Nguyên tắc tránh lỗ:**
- Dùng **p95** làm mức tham chiếu an toàn ($0.05/min neural, $0.15/min HD)
- Video dài (>40 phút) phải **chunk trước khi dịch** (giới hạn output token ~65k)

## Gói bán (chốt 8/2026)

| Gói | Giá | Credit Neural | Credit HD |
| --- | --- | --- | --- |
| Starter | $5 | 100k | 30k |
| Creator | $15 | 350k | 110k |
| Pro | $30 | 800k | 250k |
| Pro x2 | $60 | 1.6M | 500k |
| Pro x5 | $150 | 4M | 1.25M |
| Pro x10 | $300 | 8M | 2.5M |

**Quy đổi ra video 10 phút** (mật độ thật: median 968 chars/min, p95 2,716):

| Gói | Giá | Lời NN (off / peak) | Video 10p NN (median) | Video 10p NN (p95) | Video 10p HD (median) | Video 10p HD (p95) |
| --- | --- | --- | --- | --- | --- | --- |
| Starter | $5 | $3.35 / $3.20 | ~10 | ~4 | ~3 | ~1 |
| Creator | $15 | $9.23 / $8.71 | ~36 | ~13 | ~11 | ~4 |
| Pro | $30 | $16.81 / $15.62 | ~83 | ~29 | ~26 | ~9 |
| Pro x2 | $60 | $33.62 / $31.24 | ~165 | ~59 | ~52 | ~18 |
| Pro x5 | $150 | $84.05 / $78.09 | ~413 | ~147 | ~129 | ~46 |
| Pro x10 | $300 | $168.09 / $156.19 | ~826 | ~295 | ~258 | ~92 |

**Chiến lược**: Starter lời mỏng = lures kéo khách; lời thật từ Creator/Pro.
**Break-even**: phụ thuộc gói, giờ chạy và chi phí phát sinh; bảng bên dưới dùng Flash Neural.

## Lời/lãi gộp từng gói

Cost = `chars × rate_TTS + AI dịch Flash`; giả định khách dùng hết credit, SRT cần dịch,
và input cache miss. Chi phí AI dùng token thực tế benchmark trả về, nên không giả định cache hit.
Đây là **lãi gộp**, chưa trừ VPS, R2/bandwidth, payment fee hay thuế.

| Gói | Lời NN cũ | NN off-peak | NN peak | Lời HD cũ | HD off-peak | HD peak |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Starter | $3.43 | $3.35 | $3.20 | $3.54 | $3.52 | $3.47 |
| Creator | $9.51 | $9.23 | $8.71 | $9.64 | $9.56 | $9.39 |
| Pro | $17.45 | $16.81 | $15.62 | $17.83 | $17.63 | $17.26 |
| Pro x2 | $34.90 | $33.62 | $31.24 | $35.66 | $35.26 | $34.51 |
| Pro x5 | $87.25 | $84.05 | $78.09 | $89.14 | $88.14 | $86.28 |
| Pro x10 | $174.50 | $168.09 | $156.19 | $178.28 | $176.28 | $172.56 |

**Break-even (bù fixed $76/tháng, Flash Neural) — số gói cần bán:**

| Gói | Off-peak | Peak |
| --- | ---: | ---: |
| Starter | 23 | 24 |
| Creator | 9 | 9 |
| Pro | 5 | 5 |
| Pro x2 | 3 | 3 |
| Pro x5 | 1 | 1 |
| Pro x10 | 1 | 1 |

Lãi ròng tháng = tổng lãi gộp − $76 fixed − R2/bandwidth phát sinh − payment fee − thuế.

---

## Dữ liệu thô & tái chạy benchmark

- `benchmark/results.jsonl` — 50 dòng thô (chars, tokens, duration từng file; 8 file `finish=length` đã hiệu chỉnh bằng ratio vi/en 1.13)
- `benchmark/bench_cost.py` — script chạy benchmark (cần `AIKEY` trong `.env`; cache theo file, chạy lại không tốn thêm API)

Tái chạy:
```bash
python3 benchmark/bench_cost.py <srt_dir> <path_to_.env>
# env: DS_IN, DS_OUT, DS_PEAK, AZURE_STD, AZURE_HD
```
