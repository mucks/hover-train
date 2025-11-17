#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

# Kill any existing server processes
echo "[dev-browser] checking for existing server processes..."
pkill -f "target/debug/server" 2>/dev/null || pkill -f "cargo run -p server" 2>/dev/null || true
sleep 0.5

echo "[dev-browser] starting server..."
cargo run -p server &
SERVER_PID=$!

cleanup() {
  echo "[dev-browser] stopping server (pid $SERVER_PID)..."
  kill $SERVER_PID 2>/dev/null || true
}
trap cleanup EXIT

# Give the server a moment to start
echo "[dev-browser] waiting for server to start..."
sleep 2

echo "[dev-browser] starting Trunk dev server..."
echo "[dev-browser] Open http://localhost:8080 in your browser"
cd client
exec trunk serve --open

