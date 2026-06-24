import os
import subprocess


INPUT_VIDEO = "/tmp/dubbing/input.mp4"
VOICEOVER_AUDIO = "/tmp/dubbing/vietnamese_voiceover.mp3"
OUTPUT_VIDEO = "/tmp/dubbing/output_dubbed.mp4"


def build_dubbed_video(
    input_video=INPUT_VIDEO,
    voiceover_audio=VOICEOVER_AUDIO,
    output_video=OUTPUT_VIDEO,
):
    for path in (input_video, voiceover_audio):
        if not os.path.exists(path):
            raise FileNotFoundError(path)

    cmd = [
        "ffmpeg", "-y",
        "-i", input_video,
        "-i", voiceover_audio,
        "-map", "0:v:0",
        "-map", "1:a:0",
        "-c:v", "copy",
        "-c:a", "aac",
        "-b:a", "192k",
        "-af", "apad",
        "-shortest",
        "-metadata:s:a:0", "language=vie",
        "-disposition:a:0", "default",
        output_video,
    ]
    subprocess.run(cmd, check=True)
    print(f"Created {output_video} with Vietnamese audio only.")


if __name__ == "__main__":
    import sys
    if len(sys.argv) not in {1, 4}:
        raise SystemExit("Usage: python3 build_video.py INPUT_VIDEO VOICEOVER_AUDIO OUTPUT_VIDEO")
    if len(sys.argv) == 4:
        build_dubbed_video(sys.argv[1], sys.argv[2], sys.argv[3])
    else:
        build_dubbed_video()
