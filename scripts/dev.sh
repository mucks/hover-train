#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

# Kill any existing server processes
echo "[dev] checking for existing server processes..."
pkill -f "target/debug/server" 2>/dev/null || pkill -f "cargo run -p server" 2>/dev/null || true
sleep 0.5

echo "[dev] starting server..."
cargo run -p server &
SERVER_PID=$!

cleanup() {
  echo "[dev] stopping server (pid $SERVER_PID)..."
  kill $SERVER_PID 2>/dev/null || true
}
trap cleanup EXIT

# give the server a moment to start
sleep 1

echo "[dev] starting client..."
exec cargo run -p client


