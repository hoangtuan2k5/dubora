"""Check release notes include vendor source references."""

from pathlib import Path
import unittest


class ReleaseSourceLinksTest(unittest.TestCase):
    def test_release_notes_include_upstream_links_and_limitations(self):
        root = Path(__file__).resolve().parents[2]
        workflow = (root / ".github/workflows/build-windows.yml").read_text()
        self.assertIn("https://github.com/FFmpeg/FFmpeg", workflow)
        self.assertIn("https://github.com/yt-dlp/yt-dlp", workflow)
        self.assertIn("chưa xác minh đầy đủ source", workflow)
        self.assertNotIn("THIRD_PARTY_SOURCE_URL", workflow)
        self.assertIn('gh release edit "$RELEASE_TAG" --draft=false --latest', workflow)


if __name__ == "__main__":
    unittest.main()
