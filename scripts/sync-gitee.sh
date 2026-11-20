#!/usr/bin/env bash
# sync-gitee.sh — mirror the phi family's crates from GitHub to Gitee.
#
# The family is one product (phimint) split across many crates. GitHub is the
# source of truth; Gitee is the China-side mirror, so a user behind the GFW
# can read the code and clone the dependency chain. This script creates the
# missing Gitee repos and re-syncs the ones that already exist.
#
# Usage:
#   scripts/sync-gitee.sh                        # sync every repo in the table
#   scripts/sync-gitee.sh phi-tui code-intel     # sync only these repos
#   scripts/sync-gitee.sh --dry-run              # report what would happen, push nothing
#   scripts/sync-gitee.sh --check                # verify Gitee matches GitHub, change nothing
#
# Env:
#   GITEE_TOKEN   required to create repos. Read from deploy/.env when unset.
#   GITEE_NS      target Gitee namespace (default: chenkangzeng)
#   GITHUB_ORG    source GitHub org (default: hibuka-labs)
#   WORKSPACE_DIR where the sibling checkouts live (default: phimint's parent)
#
# The token is only needed for repo creation. Pushing uses the same token when
# it is available, and falls back to the git credential helper otherwise.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_DIR="${WORKSPACE_DIR:-$(cd "$PROJECT_DIR/.." && pwd)}"
GITEE_NS="${GITEE_NS:-chenkangzeng}"
GITHUB_ORG="${GITHUB_ORG:-hibuka-labs}"

# The crates phimint depends on, minus the ones already mirrored. The Gitee
# repo name always equals the GitHub repo name and the local checkout dir.
REPOS=(
  code-intel
  codexec
  llm-providers
  phi-agent-tui
  phi-canvas
  phi-cap
  phi-kernel-tools
  phi-ns
  phi-ns-desktop
  phi-telemetry
  phi-tui
  phi-wire
)

DRY_RUN=0
CHECK_ONLY=0
SELECTED=()

# ── output helpers ──────────────────────────────────────────────────────────
if [ -t 1 ]; then
  R=$'\033[31m'; G=$'\033[32m'; Y=$'\033[33m'; B=$'\033[34m'; D=$'\033[2m'; N=$'\033[0m'
else
  R=''; G=''; Y=''; B=''; D=''; N=''
fi
log()  { printf '%s\n' "$*"; }
step() { printf '%s▸%s %s\n' "$B" "$N" "$*"; }
ok()   { printf '%s✓%s %s\n' "$G" "$N" "$*"; }
warn() { printf '%s!%s %s\n' "$Y" "$N" "$*"; }
die()  { printf '%s✗%s %s\n' "$R" "$N" "$*" >&2; exit 1; }
skip() { printf '%s-%s %s\n' "$D" "$N" "$*"; }

# ── args ───────────────────────────────────────────────────────────────────
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    --check)   CHECK_ONLY=1 ;;
    -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) die "unknown flag: $arg" ;;
    *)  SELECTED+=("$arg") ;;
  esac
done

# ── token ───────────────────────────────────────────────────────────────────
# deploy/.env is gitignored; the token never enters the repo.
if [ -z "${GITEE_TOKEN:-}" ]; then
  for candidate in "$PROJECT_DIR/deploy/.env" "$WORKSPACE_DIR/phimint/deploy/.env"; do
    if [ -f "$candidate" ]; then
      GITEE_TOKEN="$(grep -E '^GITEE_TOKEN=' "$candidate" | head -1 | cut -d= -f2- | tr -d '"'"'"'')"
      break
    fi
  done
fi
export GITEE_TOKEN="${GITEE_TOKEN:-}"

if [ -z "$GITEE_TOKEN" ] && [ "$CHECK_ONLY" -eq 0 ] && [ "$DRY_RUN" -eq 0 ]; then
  die "GITEE_TOKEN is unset and no deploy/.env was found — cannot create Gitee repos."
fi

gitee_api() {
  local url="$1"; shift
  curl -sS -H "Content-Type: application/json" ${GITEE_TOKEN:+-H "Authorization: token $GITEE_TOKEN"} "$url" "$@"
}

# The push URL. With a token it is deterministic; without one we fall back to
# the credential helper (osxkeychain) that already holds gitee.com.
push_url() {
  local repo="$1"
  if [ -n "$GITEE_TOKEN" ]; then
    printf 'https://oauth2:%s@gitee.com/%s/%s.git' "$GITEE_TOKEN" "$GITEE_NS" "$repo"
  else
    printf 'https://gitee.com/%s/%s.git' "$GITEE_NS" "$repo"
  fi
}

# ── repo helpers ───────────────────────────────────────────────────────────
gitee_exists() {
  local repo="$1"
  local code
  code=$(curl -sS -o /dev/null -w '%{http_code}' \
    -H "Content-Type: application/json" ${GITEE_TOKEN:+-H "Authorization: token $GITEE_TOKEN"} \
    "https://gitee.com/api/v5/repos/$GITEE_NS/$repo")
  [ "$code" = "200" ]
}

# The branch the product ships from. main is the family's convention; master
# is only a fallback for a checkout that has not been migrated yet.
default_branch() {
  local dir="$1"
  local b
  b="$(git -C "$dir" symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null | sed 's|^origin/||')"
  [ -z "$b" ] && b="$(git -C "$dir" rev-parse --abbrev-ref HEAD)"
  printf '%s' "$b"
}

github_description() {
  local repo="$1"
  command -v gh >/dev/null 2>&1 || return 0
  gh api "repos/$GITHUB_ORG/$repo" --jq '.description // empty' 2>/dev/null || true
}

github_topics() {
  local repo="$1"
  command -v gh >/dev/null 2>&1 || return 0
  gh api "repos/$GITHUB_ORG/$repo/topics?per_page=100" --jq '.names // [] | join(" ")' 2>/dev/null || true
}

# ── per-repo steps ─────────────────────────────────────────────────────────
sync_repo() {
  local repo="$1"
  local dir="$WORKSPACE_DIR/$repo"
  local branch desc topics
  local full="$GITEE_NS/$repo"

  if [ ! -d "$dir/.git" ]; then
    warn "$repo — no checkout at $dir, skipped"
    return 0
  fi

  branch="$(default_branch "$dir")"
  desc="$(github_description "$repo")"
  topics="$(github_topics "$repo")"

  step "$full (branch: $branch)"

  # 1. create the repo when it is missing
  if gitee_exists "$repo"; then
    skip "$full already exists"
  elif [ "$CHECK_ONLY" -eq 1 ]; then
    warn "$full is MISSING on Gitee"
  elif [ "$DRY_RUN" -eq 1 ]; then
    skip "[dry-run] would create $full"
  else
    local payload
    payload="$(python3 - "$repo" "$desc" <<'PY'
import json, sys
repo, desc = sys.argv[1], sys.argv[2]
print(json.dumps({
    "name": repo,
    "path": repo,
    "description": desc or f"{repo} — part of the phi family, the crates behind phimint.",
    "private": False,
    "auto_init": False,
    "has_issues": True,
    "has_wiki": False,
    "has_projects": False,
}))
PY
)"
    local out code
    out="$(curl -sS -o /tmp/gitee-create.json -w '%{http_code}' -X POST \
      -H "Content-Type: application/json" \
      ${GITEE_TOKEN:+-H "Authorization: token $GITEE_TOKEN"} \
      "https://gitee.com/api/v5/orgs/$GITEE_NS/repos" -d "$payload")"
    code="$out"
    if [ "$code" = "201" ]; then
      ok "created $full"
    else
      die "could not create $full (HTTP $code): $(head -c 300 /tmp/gitee-create.json)"
    fi
  fi

  # 2. make the shipped branch the default one so the Gitee landing page shows it
  if [ "$CHECK_ONLY" -eq 0 ] && [ "$DRY_RUN" -eq 0 ] && gitee_exists "$repo"; then
    curl -sS -o /dev/null -X PATCH \
      -H "Content-Type: application/json" \
      ${GITEE_TOKEN:+-H "Authorization: token $GITEE_TOKEN"} \
      "https://gitee.com/api/v5/repos/$full" \
      -d "{\"default_branch\":\"$branch\",\"description\":$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$desc")}" >/dev/null || true
  fi

  # 3. push the branch and every tag
  if [ "$CHECK_ONLY" -eq 1 ]; then
    local gbranch gtag gtags
    gbranch="$(git ls-remote --heads "$(push_url "$repo")" "refs/heads/$branch" 2>/dev/null | awk '{print $1}' | head -1)"
    gtags="$(git ls-remote --tags "$(push_url "$repo")" 2>/dev/null | grep -c 'refs/tags/[^/]*$' || true)"
    local ltags
    ltags="$(git -C "$dir" tag | wc -l | tr -d ' ')"
    if [ "$gbranch" = "$(git -C "$dir" rev-parse "$branch")" ]; then
      ok "$full $branch is current"
    else
      warn "$full $branch differs from GitHub"
    fi
    if [ "$gtags" = "$ltags" ]; then
      ok "$full has all $ltags tags"
    else
      warn "$full has $gtags/$ltags tags"
    fi
    return 0
  fi

  if [ "$DRY_RUN" -eq 1 ]; then
    skip "[dry-run] would push $branch + tags to $full"
    return 0
  fi

  local url
  url="$(push_url "$repo")"

  if git -C "$dir" push --quiet --force "$url" "$branch:refs/heads/$branch"; then
    ok "pushed $branch"
  else
    die "failed to push $branch to $full"
  fi

  if git -C "$dir" push --quiet --tags "$url"; then
    ok "pushed tags"
  else
    die "failed to push tags to $full"
  fi

  # 4. record the mirror as a remote so later pushes are one command
  if git -C "$dir" remote get-url gitee >/dev/null 2>&1; then
    git -C "$dir" remote set-url gitee "$(push_url "$repo")"
  else
    git -C "$dir" remote add gitee "$(push_url "$repo")"
  fi
}

# ── main ───────────────────────────────────────────────────────────────────
if [ "${#SELECTED[@]}" -gt 0 ]; then
  TARGETS=("${SELECTED[@]}")
else
  TARGETS=("${REPOS[@]}")
fi

log "Gitee namespace : $GITEE_NS"
log "GitHub source   : $GITHUB_ORG"
log "Workspace       : $WORKSPACE_DIR"
log "Repos           : ${#TARGETS[@]}"
log ""

for repo in "${TARGETS[@]}"; do
  sync_repo "$repo"
done

log ""
if [ "$CHECK_ONLY" -eq 1 ]; then
  log "check finished."
elif [ "$DRY_RUN" -eq 1 ]; then
  log "dry run finished — nothing was created or pushed."
else
  log "sync finished."
fi
