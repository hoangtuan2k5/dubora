import json
import os
import re
import shlex
import subprocess
import sys

def parse_srt(path):
    with open(path, 'r', encoding='utf-8') as f:
        content = f.read()
    
    blocks = content.strip().split('\n\n')
    segments = []
    
    for block in blocks:
        lines = [l.strip() for l in block.split('\n') if l.strip()]
        if len(lines) < 3:
            continue
            
        index = lines[0]
        time_range = lines[1]
        text = " ".join(lines[2:]).strip()
        
        match = re.match(r"(\d{2}:\d{2}:\d{2},\d{3})\s*-->\s*(\d{2}:\d{2}:\d{2},\d{3})", time_range)
        if not match:
            continue
        start_time, end_time = match.groups()
        
        segments.append({
            'index': int(index),
            'start': start_time,
            'end': end_time,
            'text': text
        })
    return segments

def time_to_seconds(t_str):
    h, m, s_ms = t_str.split(':')
    s, ms = s_ms.split(',')
    return int(h)*3600 + int(m)*60 + int(s) + int(ms)/1000.0

def get_audio_duration(path):
    result = subprocess.run(
        [
            "ffprobe", "-v", "error",
            "-show_entries", "format=duration",
            "-of", "json",
            path,
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    return float(json.loads(result.stdout)["format"]["duration"])

def build_voiceover_track(
    input_srt="/tmp/dubbing/clean_final_vi.srt",
    tts_dir="/tmp/dubbing/tts",
    output_audio="/tmp/dubbing/vietnamese_voiceover.mp3",
    work_dir="/tmp/dubbing",
):
    segs = parse_srt(input_srt)
    print(f"Loaded {len(segs)} segments.")

    if not segs:
        raise RuntimeError("No Vietnamese subtitle segments were found.")

    missing_files = [
        os.path.join(tts_dir, f"seg_{s['index']}.mp3")
        for s in segs
        if not os.path.exists(os.path.join(tts_dir, f"seg_{s['index']}.mp3"))
    ]
    if missing_files:
        raise RuntimeError(
            f"Missing {len(missing_files)} TTS segments. Run generate_tts.py until all "
            f"segments exist. First missing file: {missing_files[0]}"
        )
    
    # We will generate an FFmpeg filter_complex input to compile the audio segments.
    # To do this cleanly:
    # 1. We create a list of inputs (each segment's mp3).
    # 2. We position each segment in time using the 'adelay' filter.
    #    adelay=delay_ms|delay_ms:all=1 (both channels delayed)
    # 3. We mix all delayed tracks using 'amix'.
    # However, FFmpeg has a limit on the number of inputs in a single command (usually ~1024, but 176 is fine).
    # To be extremely robust and avoid command line length limits, let's write a file-based concat/delay list,
    # or generate a temporary Python script that runs FFmpeg with an input list.
    # Let's write the FFmpeg command dynamically.
    
    filter_parts = []
    
    adjusted_count = 0
    max_tempo = 1.0

    for idx, s in enumerate(segs):
        mp3_path = os.path.join(tts_dir, f"seg_{s['index']}.mp3")
        start_sec = time_to_seconds(s['start'])
        if idx + 1 < len(segs):
            slot_end_sec = time_to_seconds(segs[idx + 1]['start'])
        else:
            slot_end_sec = time_to_seconds(s['end'])

        available_duration = max(0.1, slot_end_sec - start_sec - 0.08)
        audio_duration = get_audio_duration(mp3_path)
        tempo = max(1.0, audio_duration / available_duration)
        delay_ms = int(start_sec * 1000)

        filters = []
        if tempo > 1.0:
            filters.append(f"rubberband=tempo={tempo:.6f}")
            adjusted_count += 1
            max_tempo = max(max_tempo, tempo)
        filters.extend([
            f"atrim=duration={available_duration:.3f}",
            "asetpts=PTS-STARTPTS",
            f"adelay={delay_ms}:all=1",
        ])
        filter_parts.append(f"[{idx}:a]{','.join(filters)}[a{idx}]")

    print(
        f"Adjusted {adjusted_count}/{len(segs)} segments to prevent overlap "
        f"(maximum tempo {max_tempo:.2f}x)."
    )
        
    # amix filter
    amix_inputs = "".join(f"[a{idx}]" for idx in range(len(segs)))
    amix_filter = (
        f"{amix_inputs}amix=inputs={len(segs)}:duration=longest:"
        "dropout_transition=0:normalize=0,alimiter=limit=0.95[outa]"
    )
    
    filter_complex = ";".join(filter_parts) + ";" + amix_filter
    
    # Write FFmpeg filter complex to a file to avoid command argument limit
    os.makedirs(work_dir, exist_ok=True)
    filter_complex_path = os.path.join(work_dir, "filter_complex.txt")
    runner_path = os.path.join(work_dir, "run_ffmpeg_audio.sh")

    with open(filter_complex_path, "w", encoding="utf-8") as f:
        f.write(filter_complex)
        
    cmd = ["ffmpeg", "-y"]
    for s in segs:
        cmd.extend(["-i", os.path.join(tts_dir, f"seg_{s['index']}.mp3")])
    cmd.extend([
        "-filter_complex_script", filter_complex_path,
        "-map", "[outa]",
        "-c:a", "libmp3lame",
        "-q:a", "2",
        output_audio,
    ])
    
    print("Writing runner script...")
    with open(runner_path, "w") as f:
        f.write("#!/bin/bash\n")
        f.write(shlex.join(cmd) + "\n")
        
    os.chmod(runner_path, 0o755)
    subprocess.run(cmd, check=True)
    print(f"Created {output_audio}")

if __name__ == "__main__":
    if len(sys.argv) not in {1, 5}:
        raise SystemExit("Usage: python3 build_audio.py INPUT_VI.srt TTS_DIR OUTPUT.mp3 WORK_DIR")
    if len(sys.argv) == 5:
        build_voiceover_track(sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4])
    else:
        build_voiceover_track()
