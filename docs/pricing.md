# Pricing Model — Dubora

Mô hình giá dựa trên **benchmark chi phí thật** (50 file SRT, 625 phút video, 8/2026).
Dữ liệu thô + script tái chạy: `benchmark/` (xem cuối trang).

## Chi phí thành phần (giá hiện tại, 8/2026)

### Biến đổi (trả theo mỗi video xử lý)

| Thành phần | Đơn giá | Ghi chú |
| --- | --- | --- |
| Azure TTS — neural | $15 / 1M chars | Giọng chuẩn |
| Azure TTS — HD premium | $48 / 1M chars | Giọng `de-DE-Florian:DragonHD` (3.2x neural) |
| DeepSeek dịch — input | $0.14 / 1M tokens | `deepseek-v4-flash` |
| DeepSeek dịch — output | $0.28 / 1M tokens | Giờ cao điểm Bắc Kinh x2: $0.28 / $0.56 |

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
| AI dịch (base) | 0.0010 | 0.0008 | 0.0002 | 0.0029 | 0.0023 |
| AI dịch (peak x2) | 0.0020 | 0.0016 | 0.0004 | 0.0058 | 0.0046 |
| TTS neural $15 | 0.0212 | 0.0148 | 0.0046 | 0.0483 | 0.0466 |
| TTS HD $48 | 0.0679 | 0.0473 | 0.0147 | 0.1545 | 0.1493 |
| **TỔNG rẻ** (base+neural) | 0.0222 | 0.0156 | 0.0049 | 0.0490 | 0.0475 |
| **TỔNG đắt** (peak+HD) | 0.0699 | 0.0488 | 0.0154 | 0.1559 | 0.1509 |

Tổng 625 phút: dịch $0.53–1.05 + TTS $17.2–55 = $12–37.7. **TTS chiếm 97–98%** chi phí.

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

| Gói | Giá | Lời/gói | Video 10p NN (median) | Video 10p NN (p95) | Video 10p HD (median) | Video 10p HD (p95) |
| --- | --- | --- | --- | --- | --- | --- |
| Starter | $5 | $3.48 | ~10 | ~4 | ~3 | ~1 |
| Creator | $15 | $9.68 | ~36 | ~13 | ~11 | ~4 |
| Pro | $30 | $17.84 | ~83 | ~29 | ~26 | ~9 |
| Pro x2 | $60 | $35.68 | ~165 | ~59 | ~52 | ~18 |
| Pro x5 | $150 | $89.20 | ~413 | ~147 | ~129 | ~46 |
| Pro x10 | $300 | $178.40 | ~826 | ~295 | ~258 | ~92 |

Kiểm chứng lời (chi phí biến đổi khi dùng hết credit):
- Starter $5: neural $1.5 → **lời $3.5** | HD $1.44 → lời $3.56
- Creator $15: neural $5.25 → **lời $9.75** | HD $5.28 → lời $9.72
- Pro $30: neural $12 → **lời $18** | HD $12 → lời $18
- Pro x2/x5/x10: đều lời ~2–3x chi phí

**Chiến lược**: Starter lời mỏng = lures kéo khách; lời thật từ Creator/Pro.
**Break-even**: ~6–8 khách trả phí/tháng mới bù nổi fixed $76/tháng.

## Lời/lãi từng gói (đã gộp AI dịch ~1.5%)

Cost = `chars × rate_TTS + AI dịch (~1.5%)`; dùng credit dùng hết.

| Gói | Giá | Cost NN | Cost HD | Lời NN | Lời HD | Biên NN | Biên HD |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Starter | $5 | $1.52 | $1.45 | **$3.48** | $3.55 | 70% | 71% |
| Creator | $15 | $5.32 | $5.30 | **$9.68** | $9.70 | 65% | 65% |
| Pro | $30 | $12.16 | $12.05 | **$17.84** | $17.95 | 59% | 60% |
| Pro x2 | $60 | $24.32 | $24.10 | **$35.68** | $35.90 | 59% | 60% |
| Pro x5 | $150 | $60.80 | $60.25 | **$89.20** | $89.75 | 59% | 60% |
| Pro x10 | $300 | $121.60 | $120.50 | **$178.40** | $179.50 | 59% | 60% |

**Break-even (bù fixed $76/tháng) — số gói cần bán:**

| Gói | Gói cần bán/tháng |
| --- | --- |
| Starter | ~22 |
| Creator | ~8 |
| Pro | ~4 |
| Pro x2 | ~2 |
| Pro x5 | ~1 |
| Pro x10 | ~0.4 (tức 5 khách/tháng) |

Ví dụ hỗn hợp: 2 Creator + 1 Pro + 1 Pro x2 = 2×9.68 + 17.84 + 35.68 = **$72.9** ≈ hòa vốn với **4 khách/tháng**.

---

## Dữ liệu thô & tái chạy benchmark

- `benchmark/results.jsonl` — 50 dòng thô (chars, tokens, duration từng file; 8 file `finish=length` đã hiệu chỉnh bằng ratio vi/en 1.13)
- `benchmark/bench_cost.py` — script chạy benchmark (cần `AIKEY` trong `.env`; cache theo file, chạy lại không tốn thêm API)

Tái chạy:
```bash
python3 benchmark/bench_cost.py <srt_dir> <path_to_.env>
# env: DS_IN, DS_OUT, DS_PEAK, AZURE_STD, AZURE_HD
```
