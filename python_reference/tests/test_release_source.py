"""Exercise the source-archive publication gate without network or credentials."""

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import textwrap
import unittest
import zipfile


@unittest.skipUnless(shutil.which("bash"), "requires Bash used by the release job")
class ReleaseSourceGateTest(unittest.TestCase):
    def test_release_stays_draft_until_source_checksum_and_zip_are_verified(self):
        root = Path(__file__).resolve().parents[2]
        workflow = (root / ".github/workflows/build-windows.yml").read_text()
        script = textwrap.dedent(workflow[workflow.index("          mapfile -d '' assets"):])
        for existing, configured, valid_checksum, valid_zip, publishes in [
            ("", False, False, True, False),
            ("true", False, False, True, False),
            ("false", False, False, True, False),
            ("", True, True, True, True),
            ("true", True, True, True, True),
            ("", True, False, True, False),
            ("", True, True, False, False),
        ]:
            with self.subTest(existing=existing, configured=configured, valid_checksum=valid_checksum):
                with tempfile.TemporaryDirectory() as directory:
                    work = Path(directory)
                    (work / "release-assets").mkdir()
                    (work / "release-assets/app.exe").write_bytes(b"build fixture")
                    (work / "release-notes.md").write_text("Test release")
                    archive = work / "fixture-source.zip"
                    with zipfile.ZipFile(archive, "w") as source:
                        source.writestr("README.txt", "Synthetic source-gate fixture")
                    if not valid_zip:
                        archive.write_bytes(b"invalid ZIP fixture")
                    checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
                    bin_dir = work / "bin"
                    bin_dir.mkdir()
                    gh = bin_dir / "gh"
                    gh.write_text(
                        '#!/bin/sh\nprintf "%s\\n" "$*" >> "$CALLS"\n'
                        'if [ "$1 $2" = "release view" ]; then\n'
                        '  [ -n "$EXISTING_RELEASE" ] || exit 1\n'
                        '  printf "%s\\n" "$EXISTING_RELEASE"\n'
                        'fi\n'
                    )
                    curl = bin_dir / "curl"
                    curl.write_text('#!/bin/sh\ncp "$SOURCE_ZIP" third-party-source.zip\n')
                    gh.chmod(0o755)
                    curl.chmod(0o755)
                    calls = work / "calls.txt"
                    result = subprocess.run(
                        ["bash", "-e", "-c", script], cwd=work, capture_output=True, text=True,
                        env={**os.environ, "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
                             "CALLS": str(calls), "EXISTING_RELEASE": existing,
                             "SOURCE_ZIP": str(archive), "RELEASE_TAG": "v0.2.2",
                             "RELEASE_TITLE": "Dubora 0.2.2", "GITHUB_SHA": "test-commit",
                             "RUNNER_TEMP": str(work), "GITHUB_STEP_SUMMARY": str(work / "summary.md"),
                             "THIRD_PARTY_SOURCE_URL": "https://example.invalid/source.zip" if configured else "",
                             "THIRD_PARTY_SOURCE_SHA256": (checksum if valid_checksum else "0" * 64) if configured else ""},
                    )
                    commands = calls.read_text().splitlines()
                    self.assertEqual(any("--draft=false" in call for call in commands), publishes)
                    self.assertEqual(result.returncode == 0, not configured or (valid_checksum and valid_zip), result.stderr)
                    if not existing:
                        self.assertTrue(any("release create" in call and "--draft" in call for call in commands))
                    elif existing == "false":
                        self.assertEqual(len(commands), 1, "published releases must remain unchanged")

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
