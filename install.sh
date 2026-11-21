#!/usr/bin/env bash
# install.sh — phimint installer (macOS / Linux)
#
#   curl -fsSL https://github.com/hibuka-labs/phimint/releases/latest/download/install.sh | bash
#   curl -fsSL https://gitee.com/chenkangzeng_admin/phimint/releases/download/latest/install.sh | PHIMINT_MIRROR=gitee bash   # mainland China
#
# Env:
#   PHIMINT_MIRROR=gitee|github   force a download mirror (default: try both)
#   PHIMINT_INSTALL_DIR=<dir>     install location (default: the first writable
#                                  directory on PATH, else ~/.local/bin)
#   PHIMINT_NO_PATH_EDIT=1        do not touch the shell rc file
#   PHIMINT_INDEX_URL=<url>       fetch the flat index from here (self-hosted)
#
# The flat index (install-index.txt) is generated from the same source as
# manifest.json, so this script needs no JSON parser — and no python3.
#
# What it does (notes/install-onboarding-design.md):
#   * picks a directory that is on PATH and writable (decision 4);
#   * falls back to ~/.local/bin and writes a marked block into the shell rc
#     file, so the user does not have to (decision 5);
#   * writes the binary through a same-directory temp file and renames it
#     into place, so a running binary is never overwritten in place
#     (decision 7);
#   * asks the App to write the config skeleton — one source of truth for the
#     config shape (decision 3).

set -euo pipefail

GITHUB_INDEX="https://github.com/hibuka-labs/phimint/releases/latest/download/install-index.txt"
# Both mirrors serve the SAME asset name here. The generator
# (scripts/gen-manifest.py) emits two variants - install-index.txt (GitHub
# URLs) and install-index-gitee.txt (Gitee URLs) - and the release run
# (deploy/release.sh step 7.2b) uploads the Gitee variant renamed to
# install-index.txt on the Gitee release. The rename is the contract; do not
# "fix" this URL to install-index-gitee.txt.
GITEE_INDEX="https://gitee.com/chenkangzeng_admin/phimint/releases/download/latest/install-index.txt"

info() { printf '  %s\n' "$*"; }
fail() { printf '❌ %s\n' "$*" >&2; exit 1; }

# ── 1. Platform detection (keys match the update manifest) ──────────────────
case "$(uname -s)" in
    Darwin) OS="darwin" ;;
    Linux)  OS="linux" ;;
    *)      fail "unsupported OS '$(uname -s)'. Windows: see install.ps1 in the README." ;;
esac
case "$(uname -m)" in
    arm64|aarch64) ARCH="aarch64" ;;
    x86_64|amd64)  ARCH="x86_64" ;;
    *)             fail "unsupported architecture $(uname -m)" ;;
esac
KEY="${OS}-${ARCH}"

command -v tar >/dev/null 2>&1 || fail "tar is required."

# ── 2. Index fetch: both mirrors are probed at once ────────────────────────
case "${PHIMINT_MIRROR:-auto}" in
    gitee)  INDEX_URLS="$GITEE_INDEX $GITHUB_INDEX" ;;
    github) INDEX_URLS="$GITHUB_INDEX $GITEE_INDEX" ;;
    *)      INDEX_URLS="$GITHUB_INDEX $GITEE_INDEX" ;;
esac
[ -n "${PHIMINT_INDEX_URL:-}" ] && INDEX_URLS="$PHIMINT_INDEX_URL"

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

# ── 3. Resolve + download (one round per mirror) ───────────────────────────
VERSION=""
ASSET_URL=""
ASSET_SHA256=""

# Every index is fetched at the same time. An unreachable mirror must not
# delay the reachable one by a full connect timeout — that is the whole
# difference between a 12-second install and a 30-second one on a
# mainland-China network.
#
# Each fetch writes its index into a file and touches a `.ok` marker when it
# succeeds. Waiting for all of them costs max(their deadlines), not the sum:
# an unreachable mirror is bounded by curl's 5s connect timeout.
INDEX_DIR="$TMP_DIR/index"
mkdir -p "$INDEX_DIR"
n=0
for index_url in $INDEX_URLS; do
    n=$((n + 1))
    (
        if curl -fsSL --connect-timeout 5 --max-time 20 "$index_url" \
            > "$INDEX_DIR/$n.txt" 2>/dev/null; then
            : > "$INDEX_DIR/$n.ok"
        fi
    ) &
done
wait

# Parse every index that answered, in the order the URLs were given, so a
# pinned mirror (PHIMINT_MIRROR) is still preferred.
#
# The three lists are parallel: entry N of each belongs to the same mirror.
# They are built as positional parameters, so the Nth value is read with
# `eval` rather than by splitting a space-joined string (an empty field
# would shift every later index).
set --
n=0
for index_url in $INDEX_URLS; do
    n=$((n + 1))
    [ -f "$INDEX_DIR/$n.ok" ] || continue
    INDEX="$(cat "$INDEX_DIR/$n.txt")"
    [ -n "$INDEX" ] || continue
    index_version="$(printf '%s\n' "$INDEX" | awk '$1 == "version" { print $2; exit }')"
    index_url_asset="$(printf '%s\n' "$INDEX" | awk -v k="$KEY" '$1 == k { print $2; exit }')"
    index_sha="$(printf '%s\n' "$INDEX" | awk -v k="$KEY" '$1 == k { print $3; exit }')"
    [ -n "$index_url_asset" ] || { info "no entry for ${KEY} in ${index_url}"; continue; }
    info "fetching index: $index_url"
    set -- "$@" "$index_version" "$index_url_asset" "$index_sha"
done
[ "$#" -gt 0 ] || fail "could not fetch a prebuilt binary for ${KEY}."

# Download the asset. Each mirror's index carries its own asset URL, so a
# mirror that answers the probe but then stalls on the download falls back to
# the other mirror's URL — the same failure the updater hit.
attempt=0
while [ "$attempt" -lt "$#" ]; do
    attempt=$((attempt + 1))
    eval "index_version=\${$((attempt * 3 - 2))}"
    eval "index_url_asset=\${$((attempt * 3 - 1))}"
    eval "index_sha=\${$((attempt * 3))}"
    info "phimint ${index_version} for ${KEY}"
    if curl -fSL --connect-timeout 5 --max-time 300 -o "$TMP_DIR/phimint.tar.gz" "$index_url_asset" 2>/dev/null; then
        VERSION="$index_version"
        ASSET_URL="$index_url_asset"
        ASSET_SHA256="$index_sha"
        break
    fi
    info "download failed: $index_url_asset"
done
[ -n "$ASSET_URL" ] || fail "could not fetch a prebuilt binary for ${KEY}."

# ── 4. Checksum ────────────────────────────────────────────────────────────
if [ -n "$ASSET_SHA256" ]; then
    if command -v shasum >/dev/null 2>&1; then
        ACTUAL="$(shasum -a 256 "$TMP_DIR/phimint.tar.gz" | awk '{ print $1 }')"
    else
        ACTUAL="$(sha256sum "$TMP_DIR/phimint.tar.gz" | awk '{ print $1 }')"
    fi
    [ "$ACTUAL" = "$ASSET_SHA256" ] || fail "checksum mismatch (expected ${ASSET_SHA256}, got ${ACTUAL})"
    info "checksum ✓"
else
    info "checksum: the index has none — installing unverified"
fi

tar -xzf "$TMP_DIR/phimint.tar.gz" -C "$TMP_DIR" phimint \
    || fail "unexpected archive layout (no phimint binary inside)"
chmod +x "$TMP_DIR/phimint"

# ── 5. Install directory: on PATH and writable (decision 4) ─────────────────
on_path() { case ":$PATH:" in *":$1:"*) return 0 ;; *) return 1 ;; esac; }

INSTALL_DIR=""
NEEDS_PATH_EDIT=0
SUDO_INSTALLED=0

if [ -n "${PHIMINT_INSTALL_DIR:-}" ]; then
    INSTALL_DIR="$PHIMINT_INSTALL_DIR"
    mkdir -p "$INSTALL_DIR" 2>/dev/null || fail "cannot create ${INSTALL_DIR}"
    [ -w "$INSTALL_DIR" ] || fail "${INSTALL_DIR} is not writable"
    on_path "$INSTALL_DIR" || NEEDS_PATH_EDIT=1
else
    for candidate in "$HOME/.local/bin" /opt/homebrew/bin /usr/local/bin; do
        on_path "$candidate" || continue
        [ -d "$candidate" ] || mkdir -p "$candidate" 2>/dev/null || continue
        [ -w "$candidate" ] || continue
        INSTALL_DIR="$candidate"
        break
    done

    # No whitelist hit: /usr/local/bin with sudo when it is on PATH.
    if [ -z "$INSTALL_DIR" ] && on_path /usr/local/bin \
        && command -v sudo >/dev/null 2>&1 && [ "$(id -u)" != "0" ]; then
        info "installing into /usr/local/bin (this asks for your password)"
        if sudo -v 2>/dev/null \
            && sudo cp "$TMP_DIR/phimint" /usr/local/bin/phimint.tmp 2>/dev/null \
            && sudo chmod 755 /usr/local/bin/phimint.tmp \
            && sudo mv -f /usr/local/bin/phimint.tmp /usr/local/bin/phimint; then
            INSTALL_DIR="/usr/local/bin"
            SUDO_INSTALLED=1
        fi
    fi

    # Still nothing: ~/.local/bin, and the rc file carries the PATH (decision 5).
    if [ -z "$INSTALL_DIR" ]; then
        INSTALL_DIR="$HOME/.local/bin"
        NEEDS_PATH_EDIT=1
        mkdir -p "$INSTALL_DIR"
    fi
fi

# ── 6. Install the binary: same-directory temp file, then rename (decision 7)
if [ "$SUDO_INSTALLED" -eq 0 ]; then
    mkdir -p "$INSTALL_DIR"
    cp "$TMP_DIR/phimint" "$INSTALL_DIR/phimint.tmp"
    chmod 755 "$INSTALL_DIR/phimint.tmp"
    mv -f "$INSTALL_DIR/phimint.tmp" "$INSTALL_DIR/phimint"
fi
info "installed → $INSTALL_DIR/phimint"

# ── 7. PATH: automatic, idempotent, marked (decision 5) ────────────────────
write_rc() {
    local dir="$1"
    local shell_name rc RCS
    shell_name="$(basename "${SHELL:-}")"
    case "$shell_name" in
        zsh)  RCS="$HOME/.zshrc" ;;
        bash) RCS="$HOME/.bashrc $HOME/.profile" ;;
        fish) RCS="$HOME/.config/fish/config.fish" ;;
        *)    RCS="$HOME/.profile" ;;
    esac
    for rc in $RCS; do
        if grep -q '^# >>> phimint >>>$' "$rc" 2>/dev/null; then
            info "PATH already set in $rc"
            continue
        fi
        mkdir -p "$(dirname "$rc")"
        {
            printf '\n# >>> phimint >>>\n'
            printf '# added by phimint installer - delete this block to undo\n'
            if [ "$shell_name" = "fish" ]; then
                printf 'fish_add_path "%s"\n' "$dir"
            else
                printf 'export PATH="%s:$PATH"\n' "$dir"
            fi
            printf '# <<< phimint <<<\n'
        } >> "$rc"
        info "PATH: added $dir to $rc"
    done
}

if [ "$NEEDS_PATH_EDIT" -eq 1 ]; then
    if [ "${PHIMINT_NO_PATH_EDIT:-0}" = "1" ]; then
        info "PATH: add this yourself:  export PATH=\"${INSTALL_DIR}:\$PATH\""
    else
        write_rc "$INSTALL_DIR"
        info "open a new terminal (or: source your rc file) and phimint is on PATH"
    fi
fi

# ── 8. Record the install source + let the App write the config ────────────
mkdir -p "$HOME/.phimint"
STATE="$HOME/.phimint/state.json"
if [ ! -f "$STATE" ]; then
    # Only when it does not exist yet: the App owns this file afterwards and
    # keeps every other field.
    printf '{\n  "install_source": "standalone"\n}\n' > "$STATE"
    info "install source recorded (upgrade: phimint update)"
fi

# Name the file the App will actually read - its resolve_path rule in shell
# (an existing LLM_CONFIG, else the first existing default, else the
# skeleton fallback), not a parallel rule of our own. Naming anything else
# sends the user to a file the App never reads.
if [ -n "${LLM_CONFIG:-}" ] && [ -f "$LLM_CONFIG" ]; then
    CONFIG_FILE="$LLM_CONFIG"
elif [ -f "$HOME/.phimint/config.json" ]; then
    CONFIG_FILE="$HOME/.phimint/config.json"
elif [ -f "$HOME/.config/phimint/config.json" ]; then
    CONFIG_FILE="$HOME/.config/phimint/config.json"
else
    CONFIG_FILE="${LLM_CONFIG:-$HOME/.phimint/config.json}"
fi
if [ -x "$INSTALL_DIR/phimint" ]; then
    if "$INSTALL_DIR/phimint" init --if-missing >/dev/null 2>&1; then
        info "config file: $CONFIG_FILE"
    else
        info "could not write $CONFIG_FILE — create it and fill in api_key"
    fi
fi

echo ""
echo "✅ phimint ${VERSION} ready."
echo ""
echo "Next: open ${CONFIG_FILE}, fill in api_key, then run:  phimint"
echo ""
echo "  Quick start — one provider for all three tiers:"
echo "  {"
echo "    \"base_url\": \"https://api.openai.com/v1\","
echo "    \"api_key\": \"sk-...\","
echo "    \"main\": \"gpt-5.4-mini\",      // chat & coding"
echo "    \"lite\": \"gpt-4o-mini\",       // sub-agents & summaries"
echo "    \"advanced\": \"o1-preview\"     // architecture & hard problems"
echo "  }"
echo "  (lite/advanced are optional and fall back to main)"
echo ""
echo "  Advanced — a different provider per tier; api_key can be \"\${ENV:VAR}\""
echo "  to keep the key out of the file (set the var in your shell: export"
echo "  NAME=sk-...; a literal \"sk-...\" works too):"
echo "  {"
echo "    \"main\": {"
echo "      \"model\": \"gpt-5.4-mini\","
echo "      \"base_url\": \"https://api.openai.com/v1\","
echo "      \"api_key\": \"\${ENV:OPENAI_API_KEY}\""
echo "    },"
echo "    \"lite\": {"
echo "      \"model\": \"deepseek-chat\","
echo "      \"base_url\": \"https://api.deepseek.com/v1\","
echo "      \"api_key\": \"\${ENV:DEEPSEEK_API_KEY}\""
echo "    },"
echo "    \"advanced\": {"
echo "      \"model\": \"claude-opus-4\","
echo "      \"base_url\": \"https://api.anthropic.com\","
echo "      \"api_key\": \"\${ENV:ANTHROPIC_API_KEY}\""
echo "    }"
echo "  }"
