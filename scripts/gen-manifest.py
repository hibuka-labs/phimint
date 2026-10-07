#!/usr/bin/env python3
"""Generate release distribution files from built archives.

Scans a dist directory for phimint release archives
(``phimint-{version}-{os}-{arch}.tar.gz`` / ``.zip``), then writes:

- ``manifest.json``        — update manifest, URLs pointing at GitHub
- ``manifest-gitee.json``  — update manifest, URLs pointing at Gitee
- ``sha256sums.txt``       — checksums of every archive

The manifest shape is the family upgrade manifest v1 consumed by
``src/update/manifest.rs`` (unknown fields ignored by older clients).
"""

import argparse
import hashlib
import json
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

ASSET_RE = re.compile(
    r"^phimint-(?P<version>[0-9][^-]*)-(?P<key>[a-z0-9_]+-[a-z0-9_]+)\.(?P<ext>tar\.gz|zip)$"
)


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="release version, e.g. 0.2.0")
    parser.add_argument("--dist-dir", required=True, type=Path)
    parser.add_argument("--github-base", required=True, help="GitHub asset base URL")
    parser.add_argument("--gitee-base", required=True, help="Gitee asset base URL")
    parser.add_argument("--notes", default=None, help="release notes for the manifest")
    args = parser.parse_args()

    archives = {}
    for path in sorted(args.dist_dir.iterdir()):
        match = ASSET_RE.match(path.name)
        if not match:
            continue
        if match.group("version") != args.version:
            print(
                f"error: {path.name} version does not match --version {args.version}",
                file=sys.stderr,
            )
            return 1
        archives[match.group("key")] = path

    if not archives:
        print(f"error: no release archives found in {args.dist_dir}", file=sys.stderr)
        return 1

    sums = {}
    for key, path in sorted(archives.items()):
        sums[key] = (path.name, sha256_of(path))

    def manifest(base: str) -> dict:
        return {
            "version": args.version,
            "notes": args.notes,
            "pub_date": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "channel": "stable",
            "mandatory": False,
            "platforms": {
                # signature: minisign signing is a planned follow-up; empty until
                # keys exist. sha256 is the v1 extension the updater verifies.
                key: {
                    "signature": "",
                    "url": f"{base}/{filename}",
                    "sha256": digest,
                }
                for key, (filename, digest) in sums.items()
            },
        }

    (args.dist_dir / "manifest.json").write_text(
        json.dumps(manifest(args.github_base), indent=2) + "\n"
    )
    (args.dist_dir / "manifest-gitee.json").write_text(
        json.dumps(manifest(args.gitee_base), indent=2) + "\n"
    )
    (args.dist_dir / "sha256sums.txt").write_text(
        "".join(f"{digest}  {filename}\n" for filename, digest in sums.values())
    )

    print(f"platforms: {', '.join(sorted(sums))}")
    for key, (filename, digest) in sorted(sums.items()):
        print(f"  {key}: {filename} sha256={digest[:12]}…")
    return 0


if __name__ == "__main__":
    sys.exit(main())
