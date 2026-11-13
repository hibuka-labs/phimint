#!/usr/bin/env bash
# The installer's contract, exercised in a sandbox.
#
#   bash tests/install_script_test.sh
#
# Every case runs the REAL install.sh against a local index and a local
# tarball — no network, and never the user's own HOME. The rules it pins
# (notes/install-onboarding-design.md, decisions 3/4/5/6/7/8):
#
#   1. the binary lands in a directory that is already on PATH, and the
#      shell rc is left alone;
#   2. a directory that is NOT on PATH gets an idempotent marker block in
#      the rc file (decision 5);
#   3. PHIMINT_NO_PATH_EDIT=1 turns that off;
#   4. python3 is NOT required (decision 8);
#   5. a checksum mismatch installs nothing;
#   6. an existing config is never touched (decision 6);
#   7. the App writes the config skeleton — the installer calls
#      `phimint init --if-missing` (decision 3);
#   8. the installer names the config the App will actually read — its
#      resolve_path rule (an existing LLM_CONFIG, else the first existing
#      default), never a parallel rule of our own.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTALLER="$ROOT/install.sh"

PASSED=0
FAILED=0

ok()  { printf '  ok   %s\n' "$1"; PASSED=$((PASSED + 1)); }
bad() { printf '  FAIL %s\n' "$1"; FAILED=$((FAILED + 1)); }

check() {
    # check <description> <condition-exit-code>
    if [ "$2" -eq 0 ]; then ok "$1"; else bad "$1"; fi
}

# The platform key the installer computes — the same names the manifest and
# the flat index use.
platform_key() {
    local os arch
    case "$(uname -s)" in
        Darwin) os=darwin ;;
        Linux)  os=linux ;;
        *)      os=other ;;
    esac
    case "$(uname -m)" in
        arm64|aarch64) arch=aarch64 ;;
        x86_64|amd64)  arch=x86_64 ;;
        *)             arch=other ;;
    esac
    printf '%s-%s\n' "$os" "$arch"
}

# A PATH with the tools the installer needs — and, in one case, WITHOUT
# python3.
sandbox_bin() {
    local dir="$1"
    local tool
    mkdir -p "$dir"
    for tool in bash sh tar awk grep sed mkdir mv chmod rm cp ln cat \
                dirname basename uname id shasum sha256sum mktemp tr head \
                cut sort wc env stty python3 python; do
        local found
        found="$(command -v "$tool" 2>/dev/null)"
        [ -n "$found" ] || continue
        [ -e "$dir/$tool" ] || ln -s "$found" "$dir/$tool"
    done
    # curl is a shim, not a symlink: the sandbox has no network, so a URL
    # that is not a local file fails at once instead of hanging.
    local real_curl
    real_curl="$(command -v curl)"
    cat > "$dir/curl" <<SHIM
#!/usr/bin/env bash
for arg in "\$@"; do
    case "\$arg" in
        http://*|https://*) echo "sandbox: no network: \$arg" >&2; exit 22 ;;
    esac
done
exec "$real_curl" "\$@"
SHIM
    chmod +x "$dir/curl"
}

# Build a sandbox: a fake HOME, a stub `phimint` binary that only records
# how it was called, and a flat index pointing at a local tarball.
make_sandbox() {
    SANDBOX="$(mktemp -d)"
    mkdir -p "$SANDBOX/home" "$SANDBOX/stub" "$SANDBOX/tools"
    cat > "$SANDBOX/stub/phimint" <<'STUB'
#!/usr/bin/env bash
echo "$@" >> "$PHIMINT_TEST_LOG"
exit 0
STUB
    chmod +x "$SANDBOX/stub/phimint"
    tar -czf "$SANDBOX/phimint.tar.gz" -C "$SANDBOX/stub" phimint
    local sha
    # The checksum helper must match what the installer uses, and both
    # spellings exist in the wild: sha256sum on Linux, shasum on macOS.
    # Hardcoding shasum broke the whole suite on the Ubuntu runner
    # (2026-11-13: 16 failures, every one downstream of an empty digest).
    if command -v sha256sum >/dev/null 2>&1; then
        sha="$(sha256sum "$SANDBOX/phimint.tar.gz" | awk '{print $1}')"
    else
        sha="$(shasum -a 256 "$SANDBOX/phimint.tar.gz" | awk '{print $1}')"
    fi
    printf 'version 9.9.9\n%s file://%s %s\n' \
        "$(platform_key)" "$SANDBOX/phimint.tar.gz" "$sha" \
        > "$SANDBOX/index.txt"
    : > "$SANDBOX/log"
}

# Run the installer in the sandbox. Extra env/args come from the caller.
run_installer() {
    ( cd "$ROOT" && env -i \
        HOME="$SANDBOX/home" \
        PATH="$TEST_PATH" \
        SHELL=/bin/zsh \
        PHIMINT_INDEX_URL="file://$SANDBOX/index.txt" \
        PHIMINT_TEST_LOG="$SANDBOX/log" \
        "$@" \
        bash "$INSTALLER" ) > "$SANDBOX/out.txt" 2>&1
    return $?
}

cleanup() { [ -n "${SANDBOX:-}" ] && rm -rf "$SANDBOX"; }
trap cleanup EXIT

# ── 1. A directory that is already on PATH ───────────────────────────────
make_sandbox
sandbox_bin "$SANDBOX/tools"
mkdir -p "$SANDBOX/home/.local/bin"
TEST_PATH="$SANDBOX/home/.local/bin:$SANDBOX/tools"
run_installer
status=$?
check "install succeeds" "$([ "$status" -eq 0 ] && echo 0 || echo 1)"
check "the binary lands in the PATH directory" \
    "$([ -x "$SANDBOX/home/.local/bin/phimint" ] && echo 0 || echo 1)"
check "the rc file is left alone" \
    "$([ ! -f "$SANDBOX/home/.zshrc" ] && echo 0 || echo 1)"
check "no PATH instruction is printed" \
    "$(grep -q 'export PATH=' "$SANDBOX/out.txt" && echo 1 || echo 0)"
check "the App is asked to write the config skeleton" \
    "$(grep -q 'init --if-missing' "$SANDBOX/log" && echo 0 || echo 1)"

# ── 2. A directory that is NOT on PATH → the rc marker block ─────────────
make_sandbox
sandbox_bin "$SANDBOX/tools"
TEST_PATH="$SANDBOX/tools"
run_installer
status=$?
check "install succeeds without a PATH directory" \
    "$([ "$status" -eq 0 ] && echo 0 || echo 1)"
check "the binary lands in ~/.local/bin" \
    "$([ -x "$SANDBOX/home/.local/bin/phimint" ] && echo 0 || echo 1)"
check "the rc file carries the marker block" \
    "$(grep -q '^# >>> phimint >>>$' "$SANDBOX/home/.zshrc" && echo 0 || echo 1)"
check "the rc block ends with the closing marker" \
    "$(grep -q '^# <<< phimint <<<$' "$SANDBOX/home/.zshrc" && echo 0 || echo 1)"
check "the rc block exports the install directory" \
    "$(grep -q 'export PATH=' "$SANDBOX/home/.zshrc" && echo 0 || echo 1)"
check "the only user action left is opening a terminal" \
    "$(grep -qi 'new terminal' "$SANDBOX/out.txt" && echo 0 || echo 1)"

# Idempotent: a second install does not add a second block.
run_installer
count="$(grep -c '^# >>> phimint >>>$' "$SANDBOX/home/.zshrc")"
check "the marker block is written once" "$([ "$count" -eq 1 ] && echo 0 || echo 1)"

# ── 3. PHIMINT_NO_PATH_EDIT=1 → the rc is not touched ────────────────────
make_sandbox
sandbox_bin "$SANDBOX/tools"
TEST_PATH="$SANDBOX/tools"
run_installer PHIMINT_NO_PATH_EDIT=1
status=$?
check "install succeeds with the opt-out" \
    "$([ "$status" -eq 0 ] && echo 0 || echo 1)"
check "no rc file is created" \
    "$([ ! -f "$SANDBOX/home/.zshrc" ] && echo 0 || echo 1)"
check "the PATH instruction is printed instead" \
    "$(grep -q 'export PATH=' "$SANDBOX/out.txt" && echo 0 || echo 1)"

# ── 4. python3 is not required ───────────────────────────────────────────
make_sandbox
sandbox_bin "$SANDBOX/tools"
rm -f "$SANDBOX/tools/python3" "$SANDBOX/tools/python"
mkdir -p "$SANDBOX/home/.local/bin"
TEST_PATH="$SANDBOX/home/.local/bin:$SANDBOX/tools"
run_installer
status=$?
check "install succeeds without python3" \
    "$([ "$status" -eq 0 ] && echo 0 || echo 1)"
check "the binary is installed without python3" \
    "$([ -x "$SANDBOX/home/.local/bin/phimint" ] && echo 0 || echo 1)"

# ── 5. A checksum mismatch installs nothing ──────────────────────────────
make_sandbox
sandbox_bin "$SANDBOX/tools"
mkdir -p "$SANDBOX/home/.local/bin"
sed -i '' 's/[0-9a-f]\{64\}/0000000000000000000000000000000000000000000000000000000000000000/' \
    "$SANDBOX/index.txt" 2>/dev/null \
    || sed -i 's/[0-9a-f]\{64\}/0000000000000000000000000000000000000000000000000000000000000000/' \
        "$SANDBOX/index.txt"
TEST_PATH="$SANDBOX/home/.local/bin:$SANDBOX/tools"
run_installer
status=$?
check "a checksum mismatch fails the install" \
    "$([ "$status" -ne 0 ] && echo 0 || echo 1)"
check "nothing is installed on a mismatch" \
    "$([ ! -f "$SANDBOX/home/.local/bin/phimint" ] && echo 0 || echo 1)"

# ── 6. An existing config is never touched ───────────────────────────────
make_sandbox
sandbox_bin "$SANDBOX/tools"
mkdir -p "$SANDBOX/home/.local/bin" "$SANDBOX/home/.phimint"
printf '{\n  "api_key": "sk-mine"\n}\n' > "$SANDBOX/home/.phimint/config.json"
TEST_PATH="$SANDBOX/home/.local/bin:$SANDBOX/tools"
run_installer
check "the existing config survives the install" \
    "$(grep -q 'sk-mine' "$SANDBOX/home/.phimint/config.json" && echo 0 || echo 1)"

# ── 7. The flat index is generated from the same source as the manifest ──
make_sandbox
sandbox_bin "$SANDBOX/tools"
mkdir -p "$SANDBOX/dist"
cp "$SANDBOX/phimint.tar.gz" "$SANDBOX/dist/phimint-9.9.9-$(platform_key).tar.gz"
python3 "$ROOT/scripts/gen-manifest.py" \
    --version 9.9.9 \
    --dist-dir "$SANDBOX/dist" \
    --github-base "https://github.com/example/phimint/releases/download/v9.9.9" \
    --gitee-base "https://gitee.com/example/phimint/releases/download/v9.9.9" \
    > "$SANDBOX/gen.txt" 2>&1
check "gen-manifest succeeds" \
    "$([ -f "$SANDBOX/dist/manifest.json" ] && echo 0 || echo 1)"
check "the flat index is produced" \
    "$([ -f "$SANDBOX/dist/install-index.txt" ] && echo 0 || echo 1)"
check "the flat index carries the version" \
    "$(grep -q '^version 9.9.9$' "$SANDBOX/dist/install-index.txt" && echo 0 || echo 1)"
check "the flat index carries the platform entry" \
    "$(grep -q "^$(platform_key) https://" "$SANDBOX/dist/install-index.txt" && echo 0 || echo 1)"
check "the flat index carries the checksum" \
    "$(grep -qE "^$(platform_key) https://.* [0-9a-f]{64}$" "$SANDBOX/dist/install-index.txt" && echo 0 || echo 1)"
check "the gitee index points at the gitee base" \
    "$(grep -q "^$(platform_key) https://gitee.com/" "$SANDBOX/dist/install-index-gitee.txt" && echo 0 || echo 1)"

# ── 8. The installer names the config the App will read ────────────────────
# The App's resolve_path rule: an existing LLM_CONFIG wins; a stale LLM_CONFIG
# falls through to the first existing default. Naming any other file sends the
# user to a config the App never reads.
make_sandbox
sandbox_bin "$SANDBOX/tools"
mkdir -p "$SANDBOX/home/.local/bin" "$SANDBOX/home/.config-alt"
printf '{\n  "api_key": "sk-alt"\n}\n' > "$SANDBOX/home/.config-alt/config.json"
TEST_PATH="$SANDBOX/home/.local/bin:$SANDBOX/tools"
run_installer LLM_CONFIG="$SANDBOX/home/.config-alt/config.json"
check "an existing LLM_CONFIG is the config the installer names" \
    "$(grep -q "Next: open $SANDBOX/home/.config-alt/config.json" "$SANDBOX/out.txt" && echo 0 || echo 1)"

make_sandbox
sandbox_bin "$SANDBOX/tools"
mkdir -p "$SANDBOX/home/.local/bin" "$SANDBOX/home/.phimint"
printf '{\n  "api_key": "sk-mine"\n}\n' > "$SANDBOX/home/.phimint/config.json"
TEST_PATH="$SANDBOX/home/.local/bin:$SANDBOX/tools"
run_installer LLM_CONFIG="$SANDBOX/home/.missing/config.json"
check "a stale LLM_CONFIG falls through to the first existing default" \
    "$(grep -q "Next: open $SANDBOX/home/.phimint/config.json" "$SANDBOX/out.txt" && echo 0 || echo 1)"

printf '\n%s passed, %s failed\n' "$PASSED" "$FAILED"
[ "$FAILED" -eq 0 ]
