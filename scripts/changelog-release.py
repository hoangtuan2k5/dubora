#!/usr/bin/env python3
"""Read the latest numbered version and notes from CHANGELOG.md."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

VERSION_HEADING = re.compile(
    r"^## \[((?:Unreleased)|(\d+\.\d+\.\d+))\](?:\s+-\s+(\d{4}-\d{2}-\d{2}))?\s*$"
)


def parse_changelog(text: str) -> list[dict[str, str | None]]:
    sections: list[dict[str, str | None]] = []
    current: dict[str, str | None] | None = None
    body: list[str] = []

    def flush() -> None:
        if current is None:
            return
        current["notes"] = "\n".join(body).strip()
        sections.append(current)

    for line in text.splitlines():
        match = VERSION_HEADING.match(line)
        if match:
            flush()
            current = {
                "version": match.group(1),
                "date": match.group(3),
                "notes": "",
            }
            body = []
            continue
        if current is not None:
            body.append(line)
    flush()
    return sections


def latest_release(sections: list[dict[str, str | None]]) -> dict[str, str | None]:
    for section in sections:
        version = section["version"]
        if version and version != "Unreleased" and re.fullmatch(r"\d+\.\d+\.\d+", version):
            if not (section.get("notes") or "").strip():
                raise SystemExit(f"CHANGELOG section [{version}] has no notes")
            if not section.get("date"):
                raise SystemExit(f"CHANGELOG section [{version}] is missing a date")
            return section
    raise SystemExit("CHANGELOG.md has no numbered version section")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("field", choices=("version", "date", "notes", "tag", "title"))
    parser.add_argument(
        "changelog",
        nargs="?",
        default="CHANGELOG.md",
        type=Path,
        help="Path to CHANGELOG.md",
    )
    args = parser.parse_args()
    text = args.changelog.read_text(encoding="utf-8")
    release = latest_release(parse_changelog(text))
    version = release["version"] or ""
    values = {
        "version": version,
        "date": release["date"] or "",
        "notes": release["notes"] or "",
        "tag": f"v{version}",
        "title": f"Dubora {version}",
    }
    sys.stdout.write(values[args.field])
    if args.field != "notes":
        sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
