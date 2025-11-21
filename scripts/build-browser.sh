#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

# Kill any existing server processes
echo "[build-browser] checking for existing server processes..."
pkill -f "target/debug/server" 2>/dev/null || pkill -f "cargo run -p server" 2>/dev/null || pkill -f "target/release/server" 2>/dev/null || true
sleep 0.5

echo "[build-browser] starting server..."
cargo run --release -p server &
SERVER_PID=$!

cleanup() {
  echo "[build-browser] stopping server (pid $SERVER_PID)..."
  kill $SERVER_PID 2>/dev/null || true
  # Also kill the HTTP server if it's running
  pkill -f "python.*http.server" 2>/dev/null || pkill -f "python3.*http.server" 2>/dev/null || true
}
trap cleanup EXIT

# Give the server a moment to start
echo "[build-browser] waiting for server to start..."
sleep 2

echo "[build-browser] building client in release mode..."
cd client
trunk build --release

echo "[build-browser] Build complete! Starting HTTP server to serve release build..."
echo "[build-browser] Open http://localhost:8080 in your browser"

# Serve the dist folder with a simple HTTP server
cd dist
# Use Python's built-in HTTP server (works on both Python 2 and 3)
python3 -m http.server 8080 2>/dev/null || python -m SimpleHTTPServer 8080 2>/dev/null || {
  echo "[build-browser] Error: Could not start HTTP server. Please install Python or use another HTTP server."
  exit 1
}

