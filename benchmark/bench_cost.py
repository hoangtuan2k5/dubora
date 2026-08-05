#!/usr/bin/env python3
"""Benchmark dubbing cost over SRTs: DeepSeek translate + Azure TTS.

Usage: bench_cost.py <srt_dir> [<env_path>]
Env vars: DS_IN, DS_OUT (USD/1M tokens), AZURE_RATE (USD/1M chars).
Results cached in results.jsonl — re-runs skip done files (no double spend).
"""
import json, os, re, sys, time, glob, statistics, argparse
import requests
from pathlib import Path

MODEL = os.environ.get("DS_MODEL", "deepseek-v4-flash")
DS_IN = float(os.environ.get("DS_IN", "0.14"))     # USD/1M tokens, giá không cao điểm
DS_OUT = float(os.environ.get("DS_OUT", "0.28"))   # USD/1M tokens, giá không cao điểm
DS_PEAK = float(os.environ.get("DS_PEAK", "2"))    # hệ số giờ cao điểm (Bắc Kinh 09-12, 14-18) = x2
AZURE_STD = float(os.environ.get("AZURE_STD", "15"))  # USD/1M chars — neural chuẩn
AZURE_HD = float(os.environ.get("AZURE_HD", "48"))    # USD/1M chars — premium HD (DragonHD)

TIME_RE = re.compile(r"(\d{2}):(\d{2}):(\d{2}),(\d{3})\s*-->")
TAG_RE = re.compile(r"<[^>]+>")

def load_env(path):
    env = {}
    if Path(path).exists():
        for line in Path(path).read_text().splitlines():
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                k, v = line.split("=", 1)
                env[k.strip()] = v.strip()
    return env

def parse_srt(path):
    """Return (chars_en, duration_sec). Duration = end of last cue."""
    chars, dur = 0, 0.0
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            m = TIME_RE.search(line)
            if m:
                h, mm, s, ms = map(int, m.groups())
                dur = max(dur, h * 3600 + mm * 60 + s + ms / 1000)
                continue
            t = TAG_RE.sub("", line).strip()
            if t and not t.isdigit():
                chars += len(t) + 1  # +1 cho khoảng trắng phân cách
    return chars, dur

def count_text_chars(srt_text):
    n = 0
    for line in srt_text.splitlines():
        line = line.strip()
        if line and not line.isdigit() and not TIME_RE.search(line):
            n += len(line) + 1
    return n

def translate(api_key, text):
    last_err = None
    for attempt in range(3):
        try:
            r = requests.post(
                "https://api.deepseek.com/chat/completions",
                headers={"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"},
                json={
                    "model": MODEL,
                    "temperature": 0.3,
                    "messages": [
                        {"role": "system",
                         "content": "Translate the SRT subtitle from English to Vietnamese. "
                                    "Keep cue numbers, timestamps and blank lines exactly; "
                                    "translate only the text lines. No explanations."},
                        {"role": "user", "content": text},
                    ],
                    "max_tokens": 65536,
                },
                timeout=300,
            )
            if r.status_code != 200:
                raise RuntimeError(f"HTTP {r.status_code}: {r.text[:200]}")
            data = r.json()
            usage = data.get("usage", {})
            content = data["choices"][0]["message"]["content"] or ""
            return content, usage.get("prompt_tokens", 0), usage.get("completion_tokens", 0), \
                   data["choices"][0].get("finish_reason")
        except Exception as e:
            last_err = e
            time.sleep(2 ** attempt)
    raise RuntimeError(f"translate failed: {last_err}")

def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(len(xs) * p))]

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("srt_dir")
    ap.add_argument("env_path", nargs="?", default=".env")
    ap.add_argument("--dry", action="store_true", help="chỉ tính chars/duration, không gọi API")
    args = ap.parse_args()

    env = load_env(args.env_path)
    api_key = env.get("AIKEY")
    if not api_key:
        sys.exit("AIKEY not found in " + args.env_path)

    cache_file = Path("results.jsonl")
    done = {}
    if cache_file.exists():
        for line in cache_file.read_text().splitlines():
            r = json.loads(line)
            done[r["file"]] = r

    files = sorted(glob.glob(os.path.join(args.srt_dir, "*.srt")))
    print(f"{len(files)} files | model {MODEL} | DS ${DS_IN}/1M in, ${DS_OUT}/1M out (peak x{DS_PEAK:.0f}) | "
          f"Azure ${AZURE_STD}/1M std, ${AZURE_HD}/1M HD\n", flush=True)

    rows = []
    for path in files:
        name = os.path.basename(path)
        if name in done:
            rows.append(done[name])
            continue
        chars_en, dur = parse_srt(path)
        if args.dry:
            row = {"file": name, "dur_min": dur / 60, "chars_en": chars_en}
            rows.append(row)
            continue
        t0 = time.time()
        text_vi, pt, ct, finish = translate(api_key, Path(path).read_text(encoding="utf-8", errors="replace"))
        chars_vi = count_text_chars(text_vi)
        row = {
            "file": name, "dur_min": round(dur / 60, 2), "chars_en": chars_en,
            "chars_vi": chars_vi, "prompt_tokens": pt, "completion_tokens": ct,
            "finish": finish, "secs": round(time.time() - t0, 1),
        }
        with open(cache_file, "a") as f:
            f.write(json.dumps(row, ensure_ascii=False) + "\n")
        rows.append(row)
        print(f"  {name[:55]:<55} {dur/60:5.1f}min {chars_en:>7}ch→{chars_vi:>7}ch "
              f"{pt:>6}in {ct:>6}out [{finish}] {time.time()-t0:4.0f}s", flush=True)
        time.sleep(0.3)

    if args.dry:
        for r in rows:
            print(f"{r['file']:<58} {r['dur_min']:6.1f}min {r['chars_en']:>8}ch {r['chars_en']/r['dur_min']/60:>7.0f}ch/min")
        return

    def c_translate(r, peak):
        k = DS_PEAK if peak else 1.0
        return (r["prompt_tokens"] * DS_IN * k + r["completion_tokens"] * DS_OUT * k) / 1e6
    def c_tts(r, hd):
        return r["chars_vi"] * (AZURE_HD if hd else AZURE_STD) / 1e6
    def c_total(r, peak, hd):
        return c_translate(r, peak) + c_tts(r, hd)

    print(f"\n{'file':<50} {'min':>5} {'ch_vi':>7} {'$dịch b':>8} {'$dịch pk':>8} {'$tts st':>8} {'$tts hd':>8} {'$tổng 1':>8} {'$tổng 2':>8}")
    for r in rows:
        if r["dur_min"] <= 0:
            continue
        print(f"{r['file'][:48]:<50} {r['dur_min']:5.1f} {r['chars_vi']:>7,} "
              f"{c_translate(r,False):>8.4f} {c_translate(r,True):>8.4f} "
              f"{c_tts(r,False):>8.4f} {c_tts(r,True):>8.4f} "
              f"{c_total(r,False,False):>8.4f} {c_total(r,True,True):>8.4f}")

    valid = [r for r in rows if r["dur_min"] > 0]
    cpm = [r["chars_vi"] / r["dur_min"] for r in valid]
    short = lambda xs: (statistics.mean(xs), statistics.median(xs), min(xs), max(xs), pct(xs, .95))

    print(f"\n--- chars_vi / phút video ---")
    m, med, mn, mx, p = short(cpm)
    print(f"avg {m:.0f} | median {med:.0f} | min {mn:.0f} | max {mx:.0f} | p95 {p:.0f}")

    for label, peak, hd in (
        ("RẺ: DS base + Azure neural chuẩn $15", False, False),
        ("ĐẮT: DS cao điểm x2 + Azure HD premium $48", True, True),
    ):
        cpm_min = [c_total(r, peak, hd) / r["dur_min"] for r in valid]
        m, med, mn, mx, p = short(cpm_min)
        tot = sum(c_total(r, peak, hd) for r in rows)
        t_tr = sum(c_translate(r, peak) for r in rows)
        t_tts = sum(c_tts(r, hd) for r in rows)
        tot_min = sum(r["dur_min"] for r in rows)
        print(f"\n--- $/phút video — {label} ---")
        print(f"avg {m:.4f} | median {med:.4f} | min {mn:.4f} | max {mx:.4f} | p95 {p:.4f}")
        print(f"tổng {tot_min:.0f} min | ${t_tr:.3f} dịch + ${t_tts:.3f} tts = ${tot:.3f} | trung bình ${tot/tot_min:.4f}/min")

if __name__ == "__main__":
    main()
