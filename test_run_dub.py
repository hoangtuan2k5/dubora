import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import run_dub


class OutputSubtitleTest(unittest.TestCase):
    def test_creates_subtitle_with_dubbed_video_basename(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            video = root / "assets/video/lesson.mp4"
            srt = root / "assets/srt/lesson.srt"
            video.parent.mkdir(parents=True)
            srt.parent.mkdir(parents=True)
            video.write_bytes(b"video")
            srt.write_text("1\n00:00:00,000 --> 00:00:01,000\nXin chao\n", encoding="utf-8")

            def copy_cleaned(source, destination):
                Path(destination).write_bytes(Path(source).read_bytes())

            def fake_run(command):
                if Path(command[1]).name == "build_video.py":
                    Path(command[-1]).write_bytes(b"dubbed")

            with (
                patch.object(run_dub, "ROOT", root),
                patch.object(run_dub, "OUTPUT_DIR", root / "outputs"),
                patch.object(run_dub, "WORK_DIR", root / "work"),
                patch.object(run_dub, "find_matching_srt", return_value=srt),
                patch.object(run_dub, "clean_youtube_rolling_subtitles", side_effect=copy_cleaned),
                patch.object(run_dub, "generate_tts_until_complete"),
                patch.object(run_dub, "run", side_effect=fake_run),
            ):
                output = run_dub.create_dub_for_video(video, "azure")

            output_srt = output.with_suffix(".srt")
            self.assertTrue(output.exists())
            self.assertEqual(output_srt.read_text(encoding="utf-8"), srt.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
