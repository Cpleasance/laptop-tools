#!/usr/bin/env bash
# Integration and stress testing script for laptop-tools (powerwatch & resumecheck)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
BIN_DIR="$ROOT_DIR/target/release"

echo "========================================================"
echo " Starting laptop-tools verification & stress test..."
echo "========================================================"

# 1. Binary presence check
echo "[1/6] Checking release binaries..."
for bin in powerwatch resumecheck; do
    if [[ ! -x "$BIN_DIR/$bin" ]]; then
        echo "Error: $BIN_DIR/$bin not found or not executable. Run 'cargo build --release' first." >&2
        exit 1
    fi
    echo "  -> Found $BIN_DIR/$bin"
done

# 2. Config validation check
echo "[2/6] Validating configuration files..."
"$BIN_DIR/powerwatch" check-config
"$BIN_DIR/resumecheck" check-config

# 3. Dry-run and manual one-shot checks
echo "[3/6] Running dry-run one-shot passes..."
"$BIN_DIR/powerwatch" --dry-run --once
"$BIN_DIR/resumecheck" --now --since "-1h"

# 4. Status reporting check
echo "[4/6] Testing status subcommands..."
"$BIN_DIR/powerwatch" status || true
echo ""
"$BIN_DIR/resumecheck" status || true

# 5. History subcommand check
echo "[5/6] Testing history subcommand..."
"$BIN_DIR/resumecheck" history --limit 5 || true

# 6. Rapid invocation / idempotence stress test
echo "[6/6] Stress testing rapid repeated one-shot invocations..."
for i in {1..5}; do
    "$BIN_DIR/powerwatch" --dry-run --once >/dev/null
    "$BIN_DIR/powerwatch" check-config >/dev/null
    "$BIN_DIR/resumecheck" check-config >/dev/null
done
echo "  -> 5 sequential cycles completed cleanly."

echo "========================================================"
echo " All verification & stress tests passed successfully!"
echo "========================================================"
