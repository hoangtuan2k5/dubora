import argparse
import shutil
import subprocess
import sys
import time
from pathlib import Path

from .clean_rolling import clean_youtube_rolling_subtitles
from .translate_srt import translate_srt
from .generate_tts import is_fresh_audio_file, parse_srt, tts_cache_key


ROOT = Path(__file__).resolve().parents[2]
PYTHON_REFERENCE_DIR = Path(__file__).resolve().parent
VIDEO_DIR = ROOT / "assets" / "video"
SRT_DIR = ROOT / "assets" / "srt"
OUTPUT_DIR = ROOT / "outputs"
WORK_DIR = ROOT / "work"
VIDEO_SUFFIXES = {".mp4", ".mov", ".mkv", ".webm"}


def safe_work_name(stem):
    keep = []
    for char in stem:
        if char.isalnum() or char in ("-", "_", "."):
            keep.append(char)
        elif char.isspace():
            keep.append("_")
    return "".join(keep).strip("._") or "job"


def safe_work_path(relative_path):
    parts = [safe_work_name(part) for part in relative_path.with_suffix("").parts]
    return WORK_DIR.joinpath(*parts)


def video_relative_path(video_path):
    try:
        return video_path.resolve().relative_to(VIDEO_DIR.resolve())
    except ValueError:
        return Path(video_path.name)


def list_videos():
    return sorted(
        path.resolve()
        for path in VIDEO_DIR.rglob("*")
        if path.is_file() and path.suffix.lower() in VIDEO_SUFFIXES
    )


def find_targets(name):
    path = Path(name)
    if path.is_absolute():
        if path.is_file():
            return [path.resolve()]
        if path.is_dir():
            return sorted(p.resolve() for p in path.rglob("*") if p.is_file() and p.suffix.lower() in VIDEO_SUFFIXES)

    candidate = VIDEO_DIR / name
    if candidate.is_file():
        return [candidate.resolve()]
    if candidate.is_dir():
        return sorted(p.resolve() for p in candidate.rglob("*") if p.is_file() and p.suffix.lower() in VIDEO_SUFFIXES)

    matches = sorted(VIDEO_DIR.glob(f"{name}.*"))
    video_matches = [p for p in matches if p.suffix.lower() in VIDEO_SUFFIXES]
    if len(video_matches) == 1:
        return [video_matches[0].resolve()]
    if len(video_matches) > 1:
        raise SystemExit(f"Multiple video files match '{name}': {video_matches}")
    raise SystemExit(f"Target not found: {name}")


def find_matching_srt(video_path):
    relative_path = video_relative_path(video_path)
    candidates = [(SRT_DIR / relative_path).with_suffix(".srt")]

    if relative_path.parts:
        prefixed_parts = (f"CC-{relative_path.parts[0]}", *relative_path.parts[1:])
        candidates.append((SRT_DIR / Path(*prefixed_parts)).with_suffix(".srt"))

    for srt_path in candidates:
        if srt_path.is_file():
            return srt_path.resolve()

    candidate_list = "\n".join(f"- {path}" for path in candidates)
    raise SystemExit(
        f"Missing matching SRT for assets/video/{relative_path}. Tried:\n{candidate_list}"
    )


def run(cmd):
    print("+", " ".join(str(part) for part in cmd))
    subprocess.run([str(part) for part in cmd], check=True)


def missing_tts_segments(vi_srt, tts_dir, provider=None, freshness_source=None):
    freshness_source = freshness_source or vi_srt
    return [
        segment["index"]
        for segment in parse_srt(str(vi_srt))
        if not is_fresh_audio_file(
            str(tts_dir / f"seg_{segment['index']}.mp3"),
            str(freshness_source),
            tts_cache_key(provider, segment["text"]) if provider else None,
        )
    ]


def generate_tts_until_complete(provider, vi_srt, tts_dir, freshness_source=None, max_runs=8):
    for run_index in range(1, max_runs + 1):
        missing = missing_tts_segments(vi_srt, tts_dir, provider, freshness_source)
        if not missing:
            print("All TTS segments are ready.")
            return

        print(f"TTS pass {run_index}/{max_runs}: {len(missing)} missing segment(s): {missing}")
        cmd = [sys.executable, PYTHON_REFERENCE_DIR / "generate_tts.py", provider, vi_srt, tts_dir]
        if freshness_source:
            cmd.append(freshness_source)
        print("+", " ".join(str(part) for part in cmd))
        result = subprocess.run([str(part) for part in cmd])
        if result.returncode != 0:
            still_missing = missing_tts_segments(vi_srt, tts_dir, provider, freshness_source)
            if not still_missing:
                print("TTS command returned non-zero, but all segments were generated.")
                return
            print(f"TTS pass {run_index} ended with {len(still_missing)} segment(s) still missing.")

        time.sleep(2)

    missing = missing_tts_segments(vi_srt, tts_dir, provider, freshness_source)
    if missing:
        raise RuntimeError(f"Failed to generate {len(missing)} TTS segment(s) after {max_runs} pass(es): {missing}")


def create_dubs_for_target(target_name, provider, force=False, force_tts=False, translate=False):
    video_paths = find_targets(target_name)
    if not video_paths:
        raise SystemExit(f"No videos found for target: {target_name}")
    outputs = []
    for index, video_path in enumerate(video_paths, 1):
        relative_path = video_relative_path(video_path)
        print(f"\n[{index}/{len(video_paths)}] {relative_path}")
        outputs.append(create_dub_for_video(video_path, provider, force, force_tts, translate))
    return outputs


def create_dub_for_video(video_path, provider, force=False, force_tts=False, translate=False):
    srt_path = find_matching_srt(video_path)
    relative_path = video_relative_path(video_path)

    job_dir = safe_work_path(relative_path)
    tts_dir = job_dir / "tts"
    output_video = (OUTPUT_DIR / relative_path).with_name(f"{video_path.stem}.vi-dub.mp4")
    output_srt = output_video.with_suffix(".srt")

    if not force and not force_tts and output_video.exists():
        video_mtime = video_path.stat().st_mtime
        srt_mtime = srt_path.stat().st_mtime
        out_mtime = output_video.stat().st_mtime
        if (
            out_mtime >= video_mtime
            and out_mtime >= srt_mtime
            and output_srt.exists()
            and output_srt.stat().st_mtime >= srt_mtime
        ):
            print(f"Skipping {relative_path}, output video already exists and is up to date.")
            return output_video

    job_dir.mkdir(parents=True, exist_ok=True)
    output_video.parent.mkdir(parents=True, exist_ok=True)

    input_srt = job_dir / "input.srt"
    clean_srt = job_dir / "clean_final.srt"
    normalized_srt = job_dir / "clean_final_tech.srt"
    vi_srt = job_dir / "clean_final_vi.srt"
    voiceover = job_dir / "vietnamese_voiceover.mp3"

    if force_tts and tts_dir.exists():
        shutil.rmtree(tts_dir)

    shutil.copy2(srt_path, input_srt)
    clean_youtube_rolling_subtitles(str(input_srt), str(clean_srt))
    if translate:
        translate_srt(str(clean_srt), str(vi_srt), str(normalized_srt))
    else:
        shutil.copy2(clean_srt, vi_srt)
        shutil.copy2(clean_srt, normalized_srt)
        print(f"Using cleaned Vietnamese SRT without translation: {vi_srt}")

    generate_tts_until_complete(provider, vi_srt, tts_dir, srt_path)
    run([sys.executable, PYTHON_REFERENCE_DIR / "build_audio.py", vi_srt, tts_dir, voiceover, job_dir])
    run([sys.executable, PYTHON_REFERENCE_DIR / "build_video.py", video_path, voiceover, output_video])
    shutil.copy2(vi_srt, output_srt)

    print(f"Done: {output_video} (+ {output_srt.name})")
    return output_video


def create_all_dubs(provider, force=False, force_tts=False, translate=False):
    videos = list_videos()
    if not videos:
        raise SystemExit(f"No video files found in {VIDEO_DIR}")

    print(f"Found {len(videos)} video file(s).")
    outputs = []
    for index, video_path in enumerate(videos, 1):
        relative_path = video_relative_path(video_path)
        print(f"\n[{index}/{len(videos)}] {relative_path}")
        outputs.append(create_dub_for_video(video_path, provider, force, force_tts, translate))
    return outputs


def main():
    parser = argparse.ArgumentParser(
        description="Create Vietnamese dubbed videos from matching assets/video and assets/srt files."
    )
    parser.add_argument(
        "target",
        nargs="?",
        help="Optional video filename, path, or directory under assets/video. If omitted, all videos are processed.",
    )
    parser.add_argument("--provider", choices=["azure", "edge"], default="azure")
    parser.add_argument(
        "--force",
        action="store_true",
        help="Force reprocessing of the video even if output exists and is up to date.",
    )
    parser.add_argument(
        "--force-tts",
        action="store_true",
        help="Delete existing TTS segments for each job and regenerate audio.",
    )
    parser.add_argument(
        "--translate",
        action="store_true",
        help="Translate cleaned SRT from English to Vietnamese. By default, SRT is treated as already Vietnamese.",
    )
    args = parser.parse_args()
    if args.target:
        create_dubs_for_target(args.target, args.provider, args.force, args.force_tts, args.translate)
    else:
        create_all_dubs(args.provider, args.force, args.force_tts, args.translate)


if __name__ == "__main__":
    main()
