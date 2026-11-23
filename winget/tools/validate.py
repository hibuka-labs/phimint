#!/usr/bin/env python3
"""Validate the phimint winget manifests against Microsoft's published schemas.

Run from the repo root:
    python3 winget/tools/validate.py

Schemas are cached under winget/.schema-cache so a flaky network does not
turn a manifest check into a coin flip.
"""
import json
import os
import sys
import time
import urllib.request

import jsonschema
import yaml

BASE = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                    "..", "manifests", "h", "hibuka-labs", "phimint")
CACHE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".schema-cache")

# (filename, schema kind, ManifestVersion)
FILES = [
    ("hibuka-labs.phimint.yaml", "version"),
    ("hibuka-labs.phimint.locale.en-US.yaml", "defaultLocale"),
    ("hibuka-labs.phimint.installer.yaml", "installer"),
]


def fetch_schema(kind, manifest_version):
    os.makedirs(CACHE, exist_ok=True)
    cached = os.path.join(CACHE, f"{kind}.{manifest_version}.json")
    if os.path.exists(cached):
        with open(cached, encoding="utf-8") as fh:
            return json.load(fh)

    url = f"https://aka.ms/winget-manifest.{kind}.{manifest_version}.schema.json"
    last = None
    for attempt in range(3):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "phimint-manifest-check"})
            with urllib.request.urlopen(req, timeout=45) as r:
                schema = json.load(r)
            with open(cached, "w", encoding="utf-8") as fh:
                json.dump(schema, fh)
            return schema
        except Exception as exc:
            last = exc
            time.sleep(2 * (attempt + 1))
    raise last


def main():
    version_dirs = sorted(
        d for d in os.listdir(BASE) if os.path.isdir(os.path.join(BASE, d))
    )
    if not version_dirs:
        print("no manifest versions found", file=sys.stderr)
        return 1

    failures = 0
    for version in version_dirs:
        vdir = os.path.join(BASE, version)
        print(f"--- version {version}")
        for fname, kind in FILES:
            path = os.path.join(vdir, fname)
            if not os.path.exists(path):
                print(f"  [MISS] {fname}")
                failures += 1
                continue
            with open(path, encoding="utf-8") as fh:
                doc = yaml.safe_load(fh)

            mv = doc.get("ManifestVersion", "1.6.0")
            try:
                schema = fetch_schema(kind, mv)
            except Exception as exc:  # network hiccup: report, keep going
                print(f"  [ERR ] {fname}: cannot fetch schema: {exc}")
                failures += 1
                continue

            try:
                jsonschema.validate(doc, schema)
            except jsonschema.ValidationError as exc:
                print(f"  [FAIL] {fname}: {exc.message} @ {list(exc.absolute_path)}")
                failures += 1
            else:
                print(f"  [PASS] {fname} ({kind}, schema {mv})")

    if failures:
        print(f"\n{failures} problem(s) found")
        return 1
    print("\nall manifests valid")
    return 0


if __name__ == "__main__":
    sys.exit(main())