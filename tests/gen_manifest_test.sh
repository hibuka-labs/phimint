#!/usr/bin/env bash
# The release manifest generator's contract, exercised in a sandbox.
#
#   bash tests/gen_manifest_test.sh
#
# Every case runs the REAL scripts/gen-manifest.py against a temp dist dir —
# no network, and never the user's own files. The rules it pins:
#
#   1. the update manifest lists every archive with its sha256 and URL;
#   2. the flat index and the manifest agree (same sums, same URLs);
#   3. tips.json is written when --tips-file is given, stamped with the
#      release date as `version` (the client compares it for equality);
#   4. tips.json carries no URL, so one file serves both mirrors;
#   5. an invalid tips source fails the generator instead of shipping a
#      manifest the client would silently reject.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GEN="$ROOT/scripts/gen-manifest.py"

PASSED=0
FAILED=0

pass() {
    PASSED=$((PASSED + 1))
    echo "  ok: $1"
}

fail() {
    FAILED=$((FAILED + 1))
    echo "  FAIL: $1"
}

check() {
    local name="$1"
    shift
    if "$@"; then
        pass "$name"
    else
        fail "$name"
    fi
}

# A minimal archive whose bytes are irrelevant — only the name and the
# checksum matter to the generator.
make_dist() {
    local dir="$1"
    local version="$2"
    mkdir -p "$dir"
    for key in darwin-aarch64 linux-x86_64 windows-x86_64; do
        local ext="tar.gz"
        [ "$key" = "windows-x86_64" ] && ext="zip"
        printf 'fake-binary-%s' "$key" >"$dir/phimint-${version}-${key}.${ext}"
    done
}

run_gen() {
    # run_gen <dist-dir> <version> [extra args...]
    local dist="$1"
    local version="$2"
    shift 2
    python3 "$GEN" \
        --version "$version" \
        --dist-dir "$dist" \
        --github-base "https://github.com/example/phimint/releases/download/v${version}" \
        --gitee-base "https://gitee.com/example/phimint/releases/download/v${version}" \
        "$@"
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# ── 1. the manifest lists every archive ──────────────────────────────
echo "1. manifest lists every archive"
dist="$tmp/case1"
make_dist "$dist" "9.9.9"
if run_gen "$dist" "9.9.9" >/dev/null 2>&1; then
    for key in darwin-aarch64 linux-x86_64 windows-x86_64; do
        check "manifest has $key" \
            grep -q "\"$key\"" "$dist/manifest.json"
    done
    check "manifest carries the release version" \
        grep -q '"version": "9.9.9"' "$dist/manifest.json"
    check "manifest urls point at GitHub" \
        grep -q "github.com/example/phimint/releases/download/v9.9.9" "$dist/manifest.json"
    check "the Gitee manifest points at Gitee" \
        grep -q "gitee.com/example/phimint/releases/download/v9.9.9" "$dist/manifest-gitee.json"
    check "sha256sums lists every archive" \
        test "$(grep -c 'phimint-9.9.9-' "$dist/sha256sums.txt")" -eq 3
else
    fail "generator exits non-zero"
fi

# ── 2. the flat index and the manifest agree ──────────────────────────
echo "2. flat index agrees with the manifest"
if [ -f "$dist/install-index.txt" ]; then
    check "index carries the version" \
        grep -q '^version 9.9.9$' "$dist/install-index.txt"
    # Every sha256 in the manifest must appear in the index, and vice versa.
    manifest_sums=$(python3 -c "
import json
m = json.load(open('$dist/manifest.json'))
print(' '.join(sorted(p['sha256'] for p in m['platforms'].values())))
")
    index_sums=$(awk 'NF == 3 {print $3}' "$dist/install-index.txt" | sort | tr '\n' ' ' | sed 's/^ *//; s/ *$//')
    check "index sums match manifest sums" \
        test "$manifest_sums" = "$index_sums"
else
    fail "install-index.txt missing"
fi

# ── 3. tips.json is written and stamped ───────────────────────────────
echo "3. tips.json is written and stamped"
dist="$tmp/case3"
make_dist "$dist" "9.9.9"
cat >"$tmp/tips.json" <<'JSON'
{
  "items": [
    { "kind": "note", "text": "a promo line" },
    { "kind": "tip", "text": "a usage tip" }
  ]
}
JSON
if run_gen "$dist" "9.9.9" --tips-file "$tmp/tips.json" >/dev/null 2>&1; then
    check "tips.json exists" test -f "$dist/tips.json"
    check "tips.json carries a version" \
        python3 -c "
import json, sys
m = json.load(open('$dist/tips.json'))
sys.exit(0 if m.get('version') else 1)
"
    check "the version is a date stamp" \
        python3 -c "
import json, re, sys
m = json.load(open('$dist/tips.json'))
sys.exit(0 if re.fullmatch(r'\d{4}-\d{2}-\d{2}(\.\d+)?', m['version']) else 1)
"
    check "the items survive the round trip" \
        python3 -c "
import json, sys
m = json.load(open('$dist/tips.json'))
items = m['items']
ok = len(items) == 2 and items[0]['text'] == 'a promo line' and items[1]['kind'] == 'tip'
sys.exit(0 if ok else 1)
"
    check "the source file is not mutated" \
        python3 -c "
import json, sys
src = json.load(open('$tmp/tips.json'))
sys.exit(0 if 'version' not in src else 1)
"
else
    fail "generator exits non-zero with --tips-file"
fi

# ── 4. one tips file serves both mirrors ──────────────────────────────
echo "4. one tips file serves both mirrors"
check "no tips-gitee.json variant is produced" \
    test ! -f "$dist/tips-gitee.json"
check "the tips content carries no mirror URL" \
    python3 -c "
import json, re, sys
m = json.load(open('$dist/tips.json'))
for item in m['items']:
    if 'github.com' in item['text'] or 'gitee.com' in item['text']:
        sys.exit(1)
sys.exit(0)
"

# ── 5. an invalid tips source fails the generator ─────────────────────
echo "5. an invalid tips source fails the generator"
dist="$tmp/case5"
make_dist "$dist" "9.9.9"

echo '{"items": []}' >"$tmp/empty.json"
if run_gen "$dist" "9.9.9" --tips-file "$tmp/empty.json" >/dev/null 2>&1; then
    fail "an empty item list is rejected"
else
    pass "an empty item list is rejected"
fi

echo '{"items": [{"kind": "tip", "text": ""}]}' >"$tmp/blank.json"
if run_gen "$dist" "9.9.9" --tips-file "$tmp/blank.json" >/dev/null 2>&1; then
    fail "an empty item text is rejected"
else
    pass "an empty item text is rejected"
fi

if run_gen "$dist" "9.9.9" --tips-file "$tmp/does-not-exist.json" >/dev/null 2>&1; then
    fail "a missing tips file is rejected"
else
    pass "a missing tips file is rejected"
fi

# Without --tips-file the generator still works (the asset is optional).
if run_gen "$dist" "9.9.9" >/dev/null 2>&1; then
    pass "tips generation is optional"
    check "no tips.json is written without --tips-file" \
        test ! -f "$dist/tips.json"
else
    fail "the generator fails without --tips-file"
fi

echo
echo "passed: $PASSED  failed: $FAILED"
[ "$FAILED" -eq 0 ]
