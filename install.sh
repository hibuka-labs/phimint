#!/usr/bin/env bash
# install.sh — Build and install phimint to ~/.cargo/bin
# Usage: ./install.sh

set -euo pipefail

echo "=== phimint installer ==="

# 1. Check Rust toolchain
if ! command -v cargo &>/dev/null; then
    echo "❌ Rust toolchain not found. Install it first:"
    echo "   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    exit 1
fi
echo "✓ Rust $(rustc --version | awk '{print $2}')"

# 2. Build & install
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
echo "Building phimint from $SCRIPT_DIR ..."
CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse cargo install --path "$SCRIPT_DIR" --locked
echo "✓ phimint installed to ~/.cargo/bin/phimint"

# 3. Ensure config directory exists
CONFIG_DIR="$HOME/.phimint"
if [ ! -d "$CONFIG_DIR" ]; then
    mkdir -p "$CONFIG_DIR"
    echo "✓ Created $CONFIG_DIR"
fi

# 4. Offer example config if none exists
if [ ! -f "$CONFIG_DIR/config.json" ]; then
    EXAMPLE="$SCRIPT_DIR/config.json.example"
    if [ -f "$EXAMPLE" ]; then
        cp "$EXAMPLE" "$CONFIG_DIR/config.json"
        echo "✓ Copied example config → $CONFIG_DIR/config.json"
        echo "  ⚠ Edit it with your API key and model before running phimint."
    fi
else
    echo "✓ Config already exists at $CONFIG_DIR/config.json"
fi

# 5. Verify
if command -v phimint &>/dev/null; then
    echo "✓ phimint is on PATH ($(phimint --version 2>/dev/null || echo 'version unknown'))"
else
    echo "⚠ ~/.cargo/bin is not in your PATH. Add it:"
    echo '  export PATH="$HOME/.cargo/bin:$PATH"'
fi

echo ""
echo "=== Done ==="
echo "Run 'phimint' to start."
echo ""
echo "Logs are at:  ~/.phimint/sessions/<session-id>/session.log"
echo "Turn events:  ~/.phimint/sessions/<session-id>/turn_*.jsonl"
echo "Token stats:  ~/.phimint/sessions/<session-id>/session_metrics.json"
echo "Update state: ~/.phimint/state.json"
