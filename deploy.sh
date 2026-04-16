#!/usr/bin/env bash
# Quick build + deploy to NASty for iteration.
# Usage: ./deploy.sh [host]
set -euo pipefail

HOST="${1:-root@10.10.20.100}"
TARGET="x86_64-unknown-linux-musl"
BIN="target/${TARGET}/release/nasty-top"

echo "==> Building for ${TARGET}..."
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="x86_64-linux-musl-gcc" \
  cargo build --release --target "${TARGET}"

echo "==> Deploying to ${HOST}..."
scp "${BIN}" "${HOST}:/tmp/nasty-top"

echo "==> Done. Run on target:"
echo "    ssh ${HOST} /tmp/nasty-top"
