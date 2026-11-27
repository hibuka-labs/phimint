#!/usr/bin/env python3
"""sync-cn.py — mirror GitHub to the China mirrors with the Chinese README.

GitHub is the English face of the project; Gitee and AtomGit are the Chinese
face. The Chinese README and manual live in `deploy/cn/` — a local-only
directory that is never pushed — and this script writes them over `README.md`
and `docs/USAGE_CN.md` on each mirror. So:

  clone from GitHub  → English README
  clone from Gitee / AtomGit → Chinese README

The overlay is a single commit on top of the mirrored main, rebuilt from
scratch on every run, so a mirror never drifts: GitHub stays the source of
truth for code, the mirrors add the Chinese front page.

Usage:
    scripts/sync-cn.py                    # both mirrors
    scripts/sync-cn.py --target gitee
    scripts/sync-cn.py --check            # report only, change nothing

Env:
    CN_MASTER_DIR   where the Chinese masters live (default: deploy/cn)
"""

import argparse
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

PROJECT_DIR = Path(__file__).resolve().parent.parent
CN_DIR = Path(
    __import__("os").environ.get("CN_MASTER_DIR", PROJECT_DIR / "deploy" / "cn")
)

# remote name → what that mirror gets
TARGETS = {
    "gitee": {"label": "Gitee", "cn": True},
    "atomgit": {"label": "AtomGit", "cn": True},
}


def step(msg):
    print(f"\033[34m▸\033[0m {msg}")


def ok(msg):
    print(f"\033[32m✓\033[0m {msg}")


def warn(msg):
    print(f"\033[33m!\033[0m {msg}")


def fail(msg):
    print(f"\033[31m✗\033[0m {msg}", file=sys.stderr)


def git(*args, cwd=None, check=True):
    proc = subprocess.run(
        ["git", *args], cwd=cwd, capture_output=True, text=True
    )
    if check and proc.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed: {proc.stderr.strip()}")
    return proc.stdout.strip()


def ls_remote(url):
    refs = {}
    for line in git("ls-remote", url, check=False).splitlines():
        parts = line.split()
        if len(parts) == 2:
            refs[parts[1]] = parts[0]
    return refs


def repo_version():
    """The version the package is about to ship (Cargo.toml [package].version)."""
    text = (PROJECT_DIR / "Cargo.toml").read_text()
    m = re.search(r'(?m)^\[package\]\s*\nversion\s*=\s*"([^"]+)"', text)
    if not m:
        m = re.search(r'(?m)^version\s*=\s*"([^"]+)"', text)
    return m.group(1) if m else ""


def cn_readme(version):
    """The Chinese README, with the version pin rewritten to the shipping one."""
    text = (CN_DIR / "README.md").read_text()
    if version:
        text = re.sub(r'phimint = "=[^"]+"', f'phimint = "={version}"', text)
    return text


def sync(target, check_only):
    label = TARGETS[target]["label"]
    url = git("remote", "get-url", target, cwd=PROJECT_DIR, check=False)
    if not url:
        fail(f"remote '{target}' is not configured")
        return False
    src = git("remote", "get-url", "origin", cwd=PROJECT_DIR, check=False)
    if not src:
        fail("remote 'origin' (GitHub) is not configured")
        return False
    step(f"{label} ← {src} → {url}")

    version = repo_version()
    readme = cn_readme(version)
    usage = (CN_DIR / "USAGE_CN.md").read_text()

    if check_only:
        refs = ls_remote(url)
        head = refs.get("refs/heads/main")
        warn(f"would write Chinese README.md ({len(readme)} bytes) + docs/USAGE_CN.md")
        warn(f"current main: {head[:12] if head else '(none)'}")
        return True

    work = Path(tempfile.mkdtemp(prefix=f"sync-cn-{target}-"))
    try:
        clone = work / "repo"
        proc = subprocess.run(
            ["git", "clone", "--quiet", "--branch", "main", src, str(clone)],
            capture_output=True,
            text=True,
        )
        if proc.returncode != 0:
            # an empty mirror has no main yet
            proc = subprocess.run(
                ["git", "clone", "--quiet", src, str(clone)],
                capture_output=True,
                text=True,
            )
            if proc.returncode != 0:
                fail(f"clone failed: {proc.stderr.strip()[:200]}")
                return False
            git("checkout", "-B", "main", cwd=clone)

        # the overlay
        (clone / "README.md").write_text(readme)
        (clone / "docs").mkdir(exist_ok=True)
        (clone / "docs" / "USAGE_CN.md").write_text(usage)
        # The AtomGit org profile renders one file of one repo — the org index
        # (Chinese family listing) lives here so GitHub never carries it.
        org_index = CN_DIR.parent / "brand" / "ORG_README.md"
        add_paths = ["README.md", "docs/USAGE_CN.md"]
        if target == "atomgit" and org_index.is_file():
            (clone / "docs" / "ORG.md").write_text(org_index.read_text())
            add_paths.append("docs/ORG.md")
        git("add", *add_paths, cwd=clone)
        dirty = git("status", "--porcelain", cwd=clone)
        if dirty:
            git(
                "commit", "-q", "-m",
                "docs: 中文版 README 与使用手册（国内渠道）\n\n"
                "由 scripts/sync-cn.py 生成：GitHub 保留英文首页，"
                "国内镜像用中文首页。",
                cwd=clone,
            )
            ok("Chinese README committed")
        else:
            skip_msg = "Chinese README already in place"
            print(f"\033[2m-\033[0m {skip_msg}")

        pushes = ["refs/heads/main:refs/heads/main"]
        tags = git("tag", "--list", cwd=clone)
        if tags:
            pushes.append("refs/tags/*:refs/tags/*")
        proc = subprocess.run(
            ["git", "push", "--force", url, *pushes],
            cwd=clone,
            capture_output=True,
            text=True,
        )
        if proc.returncode != 0:
            fail(f"push failed: {proc.stderr.strip()[:300]}")
            return False
        ok(f"pushed main + {len(tags.splitlines())} tags")

        # `latest` is the moving tag the one-line installer points at
        # (releases/download/latest/install.sh). GitHub has no such tag, so
        # each mirror carries its own.
        release_tag = f"v{version}"
        if version and release_tag in tags.splitlines():
            proc = subprocess.run(
                ["git", "push", "--force", url, f"refs/tags/{release_tag}:refs/tags/latest"],
                cwd=clone,
                capture_output=True,
                text=True,
            )
            if proc.returncode == 0:
                ok(f"latest → {release_tag}")
            else:
                warn(f"could not move latest: {proc.stderr.strip()[:160]}")
        return True
    finally:
        shutil.rmtree(work, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser(description="mirror GitHub to the China mirrors")
    ap.add_argument("--target", choices=sorted(TARGETS), help="one mirror (default: both)")
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args()

    if not CN_DIR.is_dir():
        fail(f"Chinese masters not found in {CN_DIR}")
        return 1

    targets = [args.target] if args.target else list(TARGETS)
    print(f"Chinese masters : {CN_DIR}")
    print(f"Package version : {repo_version()}")
    print(f"Targets         : {', '.join(targets)}")
    print()

    failed = []
    for t in targets:
        try:
            if not sync(t, args.check):
                failed.append(t)
        except Exception as exc:
            fail(f"{t}: {exc}")
            failed.append(t)

    print()
    if failed:
        fail(f"failed: {', '.join(failed)}")
        return 1
    ok("sync finished")
    return 0


if __name__ == "__main__":
    sys.exit(main())
