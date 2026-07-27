import asyncio
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import generate_tts
import run_dub
import translate_srt


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

    def test_silent_tts_fallback_uses_the_expected_filename_and_cache_key(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            srt = root / "input.srt"
            tts = root / "tts"
            srt.write_text("1\n00:00:00,000 --> 00:00:01,000\ntext\n", encoding="utf-8")

            async def fail_tts(*args, **kwargs):
                return False

            def create_fallback(command, **kwargs):
                Path(command[-1]).write_bytes(b"fallback")

            with (
                patch.object(generate_tts, "generate_speech_for_segment", side_effect=fail_tts),
                patch.object(generate_tts.subprocess, "run", side_effect=create_fallback),
            ):
                asyncio.run(generate_tts.generate_all_tts("edge", srt, tts))

            output = tts / "seg_1.mp3"
            self.assertTrue(output.exists())
            self.assertEqual(
                Path(f"{output}.sha256").read_text(encoding="utf-8"),
                generate_tts.tts_cache_key("edge", "text"),
            )

    def test_gemini_translation_rotates_keys_per_batch(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "input.srt"
            output = root / "output.srt"
            source.write_text(
                "\n".join(
                    f"{index}\n00:00:{index:02},000 --> 00:01:{index:02},000\nText {index}\n"
                    for index in range(1, 22)
                ),
                encoding="utf-8",
            )
            used_keys = []

            class Response:
                def raise_for_status(self):
                    pass

                def json(self):
                    return {"steps": [{"type": "model_output", "content": [{"type": "text", "text": self.text}]}]}

            def fake_post(*args, **kwargs):
                used_keys.append(kwargs["headers"]["x-goog-api-key"])
                segments = json.loads(kwargs["json"]["input"].split("Input JSON: ", 1)[1])
                response = Response()
                response.text = json.dumps({"segments": segments})
                return response

            with patch.object(translate_srt.requests, "post", side_effect=fake_post):
                translate_srt.translate_srt(source, output, api_keys=["key-1", "key-2"])

            self.assertEqual(used_keys, ["key-1", "key-2"])
            self.assertEqual(len(translate_srt.parse_srt(output)), 21)


if __name__ == "__main__":
    unittest.main()
