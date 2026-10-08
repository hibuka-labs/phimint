#!/usr/bin/env bash
# install.sh — phimint installer (macOS / Linux)
#
# Installs a prebuilt binary; no Rust toolchain required.
#
#   curl -fsSL https://github.com/hibuka-labs/phimint/releases/latest/download/install.sh | bash
#   curl -fsSL https://gitee.com/chenkangzeng_admin/phimint/releases/download/latest/install.sh | PHIMINT_MIRROR=gitee bash   # mainland China
#
# Env:
#   PHIMINT_MIRROR=gitee|github   force a download mirror (default: try both)
#   PHIMINT_INSTALL_DIR=<dir>     install location (default: ~/.local/bin)
set -euo pipefail

GITHUB_MANIFEST="https://github.com/hibuka-labs/phimint/releases/latest/download/manifest.json"
GITEE_MANIFEST="https://gitee.com/chenkangzeng_admin/phimint/raw/release-metadata/manifest.json"

info()  { printf '  %s\n' "$*"; }
fail()  { printf '❌ %s\n' "$*" >&2; exit 1; }

# ── 1. Platform detection (keys match the update manifest) ──────────────────
case "$(uname -s)" in
    Darwin) OS="darwin" ;;
    Linux)  OS="linux" ;;
    *)      fail "unsupported OS '$(uname -s)'. Windows: see install.ps1 in the README." ;;
esac
case "$(uname -m)" in
    arm64|aarch64) ARCH="aarch64" ;;
    x86_64|amd64)  ARCH="x86_64" ;;
    *)             fail "unsupported architecture '$(uname -m)'" ;;
esac
KEY="${OS}-${ARCH}"

command -v python3 >/dev/null 2>&1 || fail "python3 is required (preinstalled on macOS; install via your package manager)."
command -v tar >/dev/null 2>&1 || fail "tar is required."

# ── 2. Manifest fetch: GitHub first, Gitee fallback (mainland China) ────────
case "${PHIMINT_MIRROR:-auto}" in
    gitee)  ENDPOINTS="$GITEE_MANIFEST $GITHUB_MANIFEST" ;;
    github) ENDPOINTS="$GITHUB_MANIFEST $GITEE_MANIFEST" ;;
    auto|*) ENDPOINTS="$GITHUB_MANIFEST $GITEE_MANIFEST" ;;
esac

MANIFEST=""
for endpoint in $ENDPOINTS; do
    info "fetching manifest: $endpoint"
    if MANIFEST=$(curl -fsSL --connect-timeout 5 --max-time 20 "$endpoint" 2>/dev/null); then
        break
    fi
    MANIFEST=""
done
[ -n "$MANIFEST" ] || fail "could not fetch the release manifest from any mirror."

# ── 3. Resolve version / URL / sha256 for this platform ─────────────────────
read -r VERSION ASSET_URL ASSET_SHA256 <<<"$(printf '%s' "$MANIFEST" | python3 -c '
import json, sys
m = json.load(sys.stdin)
key = sys.argv[1]
entry = m.get("platforms", {}).get(key)
if entry is None:
    sys.exit("no prebuilt binary for platform " + key + " in manifest")
print(m["version"], entry["url"], entry.get("sha256") or "-")
' "$KEY")"

info "phimint ${VERSION} for ${KEY}"

# ── 4. Download + checksum ──────────────────────────────────────────────────
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT
ARCHIVE="$TMP_DIR/phimint.tar.gz"
curl -fSL --connect-timeout 5 --max-time 300 -o "$ARCHIVE" "$ASSET_URL" \
    || fail "download failed: $ASSET_URL"

if [ "$ASSET_SHA256" != "-" ]; then
    if command -v shasum >/dev/null 2>&1; then
        ACTUAL=$(shasum -a 256 "$ARCHIVE" | awk '{print $1}')
    else
        ACTUAL=$(sha256sum "$ARCHIVE" | awk '{print $1}')
    fi
    [ "$ACTUAL" = "$ASSET_SHA256" ] || fail "checksum mismatch (expected ${ASSET_SHA256}, got ${ACTUAL})"
    info "checksum ✓"
else
    info "checksum: manifest has none — installing unverified"
fi

# ── 5. Install binary ───────────────────────────────────────────────────────
INSTALL_DIR="${PHIMINT_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$INSTALL_DIR"
tar -xzf "$ARCHIVE" -C "$TMP_DIR" phimint \
    || fail "unexpected archive layout (no phimint binary inside)"
chmod +x "$TMP_DIR/phimint"
mv "$TMP_DIR/phimint" "$INSTALL_DIR/phimint"
info "installed → $INSTALL_DIR/phimint"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) info "⚠ $INSTALL_DIR is not on PATH. Add it:  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac

# ── 6. Record install source (upgrade routing) + config bootstrap ───────────
mkdir -p "$HOME/.phimint"
python3 - "$HOME/.phimint/state.json" <<'PY'
import json, os, sys
path = sys.argv[1]
state = {}
if os.path.exists(path):
    try:
        with open(path) as f:
            state = json.load(f)
    except Exception:
        state = {}
state["install_source"] = "standalone"
with open(path, "w") as f:
    json.dump(state, f, indent=2)
PY
info "install source recorded (upgrade: phimint update)"

CONFIG="$HOME/.phimint/config.json"
if [ ! -f "$CONFIG" ]; then
    info "next: create ~/.phimint/config.json with your API key (see https://github.com/hibuka-labs/phimint#configure)"
fi

echo ""
echo "✅ phimint ${VERSION} ready. Run:  phimint"
