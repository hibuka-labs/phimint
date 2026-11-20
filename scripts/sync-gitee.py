#!/usr/bin/env python3
"""sync-gitee.py — mirror the phi family's crates from GitHub to Gitee.

The family is one product (phimint) split across many crates. GitHub is the
source of truth; Gitee is the China-side mirror, so a user behind the GFW can
read the code and clone the whole dependency chain.

Everything lands in ONE Gitee namespace (chenkangzeng_admin by default), next
to phimint itself. The org namespace (chenkangzeng / 不卡科技) is left behind
when --migrate is passed.

Usage:
    scripts/sync-gitee.py                     # sync every repo in the table
    scripts/sync-gitee.py phi-tui code-intel  # sync only these repos
    scripts/sync-gitee.py --dry-run           # report, change nothing
    scripts/sync-gitee.py --check             # verify Gitee matches GitHub
    scripts/sync-gitee.py --migrate           # also delete the org copies

Env:
    GITEE_TOKEN   needed to create/delete repos. Read from deploy/.env if unset.
    GITEE_NS      target Gitee namespace (default: chenkangzeng_admin)
    GITHUB_ORG    source GitHub org (default: hibuka-labs)
    WORKSPACE_DIR where the sibling checkouts live (default: phimint's parent)

Pushing uses the git credential helper (no token in .git/config). The token is
only used for the REST calls that create or delete a repo.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

PROJECT_DIR = Path(__file__).resolve().parent.parent
WORKSPACE_DIR = Path(os.environ.get("WORKSPACE_DIR", PROJECT_DIR.parent)).resolve()
GITEE_NS = os.environ.get("GITEE_NS", "chenkangzeng_admin")
GITHUB_ORG = os.environ.get("GITHUB_ORG", "hibuka-labs")
GITEE_API = "https://gitee.com/api/v5"

# The crates phimint depends on, plus the two standalone family repos.
# The Gitee repo name always equals the GitHub repo name.
REPOS = [
    "agent-base",
    "agent-works",
    "code-intel",
    "codexec",
    "llm-providers",
    "log-core",
    "phi-agent",
    "phi-agent-tui",
    "phi-canvas",
    "phi-cap",
    "phi-kernel-tools",
    "phi-ns",
    "phi-ns-desktop",
    "phi-telemetry",
    "phi-tools",
    "phi-tui",
    "phi-wire",
    "phimint",
    "sshr",
]

# Repos that also live in the org namespace. --migrate deletes them there.
ORG_NS = "chenkangzeng"


# ── output ──────────────────────────────────────────────────────────────────
def _c(code):
    return f"\033[{code}m" if sys.stdout.isatty() else ""


R, G, Y, B, D, N = _c(31), _c(32), _c(33), _c(34), _c(2), _c(0)


def step(msg):
    print(f"{B}▸{N} {msg}")


def ok(msg):
    print(f"{G}✓{N} {msg}")


def warn(msg):
    print(f"{Y}!{N} {msg}")


def fail(msg):
    print(f"{R}✗{N} {msg}", file=sys.stderr)


def skip(msg):
    print(f"{D}-{N} {msg}")


# ── token ───────────────────────────────────────────────────────────────────
def load_token():
    token = os.environ.get("GITEE_TOKEN")
    if token:
        return token.strip()
    for candidate in (PROJECT_DIR / "deploy" / ".env", WORKSPACE_DIR / "phimint" / "deploy" / ".env"):
        if candidate.is_file():
            for line in candidate.read_text().splitlines():
                if line.startswith("GITEE_TOKEN="):
                    return line.split("=", 1)[1].strip().strip('"').strip("'")
    return ""


TOKEN = load_token()


def api(path, method="GET", payload=None):
    """Call the Gitee REST API. Returns (status_code, decoded_body)."""
    url = f"{GITEE_API}/{path}"
    data = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Content-Type", "application/json")
    req.add_header("Accept", "application/json")
    if TOKEN:
        req.add_header("Authorization", f"token {TOKEN}")
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            body = resp.read().decode("utf-8", "replace")
            try:
                return resp.status, json.loads(body)
            except json.JSONDecodeError:
                return resp.status, body
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")
        try:
            return e.code, json.loads(body)
        except json.JSONDecodeError:
            return e.code, body


# ── git helpers ─────────────────────────────────────────────────────────────
def git(*args, cwd=None, check=True):
    proc = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
    if check and proc.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed: {proc.stderr.strip()}")
    return proc.stdout.strip()


def ls_remote(url, pattern=None):
    args = ["ls-remote", url]
    if pattern:
        args.append(pattern)
    out = git(*args, check=False)
    refs = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 2:
            refs[parts[1]] = parts[0]
    return refs


def gitee_url(repo):
    return f"https://gitee.com/{GITEE_NS}/{repo}.git"


def github_url(repo):
    return f"git@github.com:{GITHUB_ORG}/{repo}.git"


def github_meta(repo):
    out = subprocess.run(
        ["gh", "api", f"repos/{GITHUB_ORG}/{repo}"], capture_output=True, text=True
    )
    if out.returncode != 0:
        return {}
    try:
        return json.loads(out.stdout)
    except json.JSONDecodeError:
        return {}


# ── per-repo sync ───────────────────────────────────────────────────────────
def sync_repo(repo, dry_run, check_only, migrate):
    meta = github_meta(repo)
    default_branch = meta.get("default_branch") or "main"
    description = meta.get("description") or ""
    full = f"{GITEE_NS}/{repo}"

    step(f"{GITHUB_ORG}/{repo} → {full} (branch: {default_branch})")

    # 1. make sure the target repo exists
    status, _ = api(f"repos/{GITEE_NS}/{repo}")
    exists = status == 200
    if not exists:
        if check_only:
            warn(f"{full} is MISSING on Gitee")
        elif dry_run:
            skip(f"[dry-run] would create {full}")
        else:
            status, body = api(
                f"user/repos",
                method="POST",
                payload={
                    "name": repo,
                    "path": repo,
                    "description": description,
                    "private": False,
                    "auto_init": False,
                    "has_issues": True,
                    "has_wiki": False,
                    "has_projects": False,
                },
            )
            if status != 201:
                fail(f"could not create {full} (HTTP {status}): {str(body)[:200]}")
                return False
            ok(f"created {full}")
            exists = True
    else:
        skip(f"{full} exists")

    # 2. fetch a fresh mirror of GitHub — the local checkout may carry
    #    unreleased commits or lag behind, and a mirror must reflect GitHub.
    mirror_dir = Path(tempfile.mkdtemp(prefix=f"gitee-sync-{repo}-"))
    try:
        mirror = mirror_dir / "repo.git"
        proc = subprocess.run(
            ["git", "clone", "--quiet", "--mirror", github_url(repo), str(mirror)],
            capture_output=True,
            text=True,
        )
        if proc.returncode != 0:
            fail(f"could not clone {GITHUB_ORG}/{repo}: {proc.stderr.strip()[:200]}")
            return False

        mirror_refs = ls_remote(str(mirror))
        target_refs = ls_remote(gitee_url(repo)) if exists else {}

        # refs that only Gitee has: release pointers like phimint's `latest`
        # tag and its `release-metadata` branch. Keep them unless they are
        # just a stale ancestor of the branch we are about to publish.
        gitee_only = {
            ref: sha
            for ref, sha in target_refs.items()
            if ref not in mirror_refs and not ref.endswith("^{}")
        }

        if check_only:
            report_check(repo, mirror, mirror_refs, target_refs, default_branch)
            return True

        if dry_run:
            skip(f"[dry-run] would publish {default_branch} + {len(mirror_refs)} refs to {full}")
            return True

        # 3. publish the branch and every tag GitHub has
        url = gitee_url(repo)
        pushes = [f"refs/heads/{default_branch}:refs/heads/{default_branch}"]
        tag_refs = [r for r in mirror_refs if r.startswith("refs/tags/") and not r.endswith("^{}")]
        if tag_refs:
            pushes.append("refs/tags/*:refs/tags/*")
        proc = subprocess.run(
            ["git", "--git-dir", str(mirror), "push", "--force", url, *pushes],
            capture_output=True,
            text=True,
        )
        if proc.returncode != 0:
            fail(f"push failed for {full}: {proc.stderr.strip()[:300]}")
            return False
        ok(f"published {default_branch} + {len(tag_refs)} tags")

        # 4. restore the Gitee-only refs the push did not carry
        restored = []
        for ref, sha in gitee_only.items():
            if ref.startswith("refs/heads/"):
                # a stale branch that the new default branch already contains
                if git(
                    "merge-base", "--is-ancestor", sha, default_branch,
                    cwd=mirror, check=False,
                ) is not None and subprocess.run(
                    ["git", "merge-base", "--is-ancestor", sha, default_branch],
                    cwd=mirror, capture_output=True,
                ).returncode == 0:
                    subprocess.run(
                        ["git", "--git-dir", str(mirror), "push", url, f":{ref}"],
                        capture_output=True, text=True,
                    )
                    restored.append(f"deleted {ref.split('/')[-1]} (stale)")
                    continue
            proc = subprocess.run(
                ["git", "--git-dir", str(mirror), "push", url, f"{sha}:{ref}"],
                capture_output=True, text=True,
            )
            if proc.returncode == 0:
                restored.append(f"kept {ref.split('/')[-1]}")
        for note in restored:
            skip(note)

        # 5. point the Gitee landing page at the shipped branch
        api(
            f"repos/{full}",
            method="PATCH",
            payload={
                "default_branch": default_branch,
                "description": description,
                "has_wiki": False,
            },
        )

        # 6. record the mirror as a remote for later pushes
        local = WORKSPACE_DIR / repo
        if (local / ".git").is_dir():
            if git("remote", "get-url", "gitee", cwd=local, check=False):
                git("remote", "set-url", "gitee", gitee_url(repo), cwd=local)
            else:
                git("remote", "add", "gitee", gitee_url(repo), cwd=local)

        # 7. --migrate: remove the copy from the org namespace
        if migrate:
            org_status, _ = api(f"repos/{ORG_NS}/{repo}")
            if org_status == 200:
                if dry_run:
                    skip(f"[dry-run] would delete {ORG_NS}/{repo}")
                else:
                    status, body = api(f"repos/{ORG_NS}/{repo}", method="DELETE")
                    if status in (204, 200):
                        ok(f"deleted {ORG_NS}/{repo}")
                    else:
                        warn(f"could not delete {ORG_NS}/{repo} (HTTP {status}) {str(body)[:120]}")
            else:
                skip(f"{ORG_NS}/{repo} not present")

        return True
    finally:
        shutil.rmtree(mirror_dir, ignore_errors=True)


def report_check(repo, mirror, mirror_refs, target_refs, default_branch):
    full = f"{GITEE_NS}/{repo}"
    want_branch = mirror_refs.get(f"refs/heads/{default_branch}")
    have_branch = target_refs.get(f"refs/heads/{default_branch}")
    if want_branch and want_branch == have_branch:
        ok(f"{full} {default_branch} is current")
    else:
        warn(f"{full} {default_branch} differs from GitHub")

    want_tags = {r for r in mirror_refs if r.startswith("refs/tags/") and not r.endswith("^{}")}
    have_tags = {r for r in target_refs if r.startswith("refs/tags/") and not r.endswith("^{}")}
    missing = sorted(r.split("/")[-1] for r in want_tags - have_tags)
    if missing:
        warn(f"{full} missing tags: {missing}")
    else:
        ok(f"{full} has all {len(want_tags)} tags")


# ── main ────────────────────────────────────────────────────────────────────
def main():
    ap = argparse.ArgumentParser(description="mirror the phi family from GitHub to Gitee")
    ap.add_argument("repos", nargs="*", help="limit to these repos")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--migrate", action="store_true", help="also delete the org-namespace copies")
    args = ap.parse_args()

    targets = args.repos or REPOS
    unknown = [r for r in targets if r not in REPOS]
    if unknown:
        fail(f"unknown repo(s): {', '.join(unknown)}")

    if not TOKEN and not (args.check or args.dry_run):
        fail("GITEE_TOKEN is unset and no deploy/.env was found — cannot create repos.")
        return 1

    print(f"Gitee namespace : {GITEE_NS}")
    print(f"GitHub source   : {GITHUB_ORG}")
    print(f"Repos           : {len(targets)}")
    if args.migrate:
        print(f"Migrating from  : {ORG_NS} (copies deleted after the mirror lands)")
    print()

    failed = []
    for repo in targets:
        try:
            if not sync_repo(repo, args.dry_run, args.check, args.migrate):
                failed.append(repo)
        except Exception as exc:  # one bad repo must not stop the rest
            fail(f"{repo}: {exc}")
            failed.append(repo)

    print()
    if failed:
        fail(f"{len(failed)} repo(s) failed: {', '.join(failed)}")
        return 1
    if args.check:
        print("check finished.")
    elif args.dry_run:
        print("dry run finished — nothing was created, changed, or deleted.")
    else:
        print("sync finished.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
