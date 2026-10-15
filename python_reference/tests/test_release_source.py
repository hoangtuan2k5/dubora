"""Exercise the source-archive publication gate without network or credentials."""

import os
from pathlib import Path
import shutil
import subprocess
import unittest


@unittest.skipUnless(shutil.which("bash"), "requires Bash used by the release job")
class ReleaseSourceGateTest(unittest.TestCase):
    def test_missing_or_invalid_source_identity_blocks_publication(self):
        root = Path(__file__).resolve().parents[2]
        workflow = (root / ".github/workflows/build-windows.yml").read_text()
        start = workflow.index('test -n "$THIRD_PARTY_SOURCE_URL"')
        gate = workflow[start:workflow.index("curl --fail", start)]
        for url, checksum, accepted in [
            ("", "", False),
            ("https://example.invalid/source.zip", "", False),
            ("https://example.invalid/source.zip", "invalid", False),
            ("https://example.invalid/source.zip", "a" * 64, True),
        ]:
            with self.subTest(url=url, checksum=checksum):
                result = subprocess.run(
                    ["bash", "-e", "-c", gate],
                    env={**os.environ, "THIRD_PARTY_SOURCE_URL": url,
                         "THIRD_PARTY_SOURCE_SHA256": checksum},
                    capture_output=True,
                )
                self.assertEqual(result.returncode == 0, accepted)


if __name__ == "__main__":
    unittest.main()
