#!/usr/bin/env python3
"""Generate release distribution files from built archives.

Scans a dist directory for phimint release archives
(``phimint-{version}-{os}-{arch}.tar.gz`` / ``.zip``), then writes:

- ``manifest.json``        — update manifest, URLs pointing at GitHub
- ``manifest-gitee.json``  — update manifest, URLs pointing at Gitee
- ``sha256sums.txt``       — checksums of every archive
- ``install-index.txt``    — flat index for install.sh, URLs pointing at GitHub
- ``install-index-gitee.txt`` — flat index for install.sh, URLs pointing at Gitee
- ``tips.json``            — cloud tips manifest (the tips band's content source)

The flat index is the same data as the manifest, flattened:

    version <v>
    <platform-key> <url> <sha256>

so the installer needs no JSON parser (and no python3). It is generated from
the same ``sums`` map as the manifest, so the two can never disagree.

The manifest shape is the family upgrade manifest v1 consumed by
``src/update/manifest.rs`` (unknown fields ignored by older clients).

``tips.json`` is the one asset whose content lives in the repo rather than
being derived from the build: ``--tips-file`` points at the source file, and
the generator stamps it with the release date as ``version``. The client
(``src/tips/manifest.rs``) compares ``version`` for equality, so a same-day
re-release needs a fresh stamp or the client keeps its cached items. The
content carries no URLs, so the same file serves the GitHub and the Gitee
mirror — there is no ``tips-gitee.json`` variant.
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


def write_tips(source: Path, target: Path) -> None:
    """Write the cloud tips manifest, stamped with the release date.

    The source file holds the items only; ``version`` is stamped here so a
    re-release on the same day still counts as new content for the client
    (it compares ``version`` for equality). The rules mirror
    ``src/tips/manifest.rs``: a non-empty version, a non-empty item list, and
    no empty item text — the client rejects the whole manifest otherwise and
    silently keeps its built-in tips.
    """
    if not source.exists():
        print(f"error: tips source file not found: {source}", file=sys.stderr)
        raise SystemExit(1)

    data = json.loads(source.read_text())
    items = data.get("items") or []
    if not items:
        print(f"error: tips source file has no items: {source}", file=sys.stderr)
        raise SystemExit(1)

    for index, item in enumerate(items):
        text = item.get("text") or ""
        if not text:
            print(
                f"error: tips item #{index} has empty text: {source}",
                file=sys.stderr,
            )
            raise SystemExit(1)

    manifest = {
        "version": datetime.now(timezone.utc).strftime("%Y-%m-%d"),
        "items": items,
    }
    target.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"tips: {len(items)} items -> {target.name}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="release version, e.g. 0.2.0")
    parser.add_argument("--dist-dir", required=True, type=Path)
    parser.add_argument("--github-base", required=True, help="GitHub asset base URL")
    parser.add_argument("--gitee-base", required=True, help="Gitee asset base URL")
    parser.add_argument("--notes", default=None, help="release notes for the manifest")
    parser.add_argument(
        "--tips-file",
        type=Path,
        default=None,
        help="tips source file (JSON with an 'items' array); skipped when absent",
    )
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

    def flat_index(base: str) -> str:
        lines = [f"version {args.version}"]
        for key, (filename, digest) in sums.items():
            lines.append(f"{key} {base}/{filename} {digest}")
        return "\n".join(lines) + "\n"

    (args.dist_dir / "install-index.txt").write_text(flat_index(args.github_base))
    (args.dist_dir / "install-index-gitee.txt").write_text(flat_index(args.gitee_base))

    if args.tips_file is not None:
        write_tips(args.tips_file, args.dist_dir / "tips.json")

    print(f"platforms: {', '.join(sorted(sums))}")
    for key, (filename, digest) in sorted(sums.items()):
        print(f"  {key}: {filename} sha256={digest[:12]}…")
    return 0


if __name__ == "__main__":
    sys.exit(main())
