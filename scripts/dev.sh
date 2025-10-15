#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")"/.. && pwd)"

pushd "$ROOT_DIR" > /dev/null

: "${APP_ENV:=development}"
: "${DATABASE_URL:=sqlite://$ROOT_DIR/dev.db}"
: "${WEBHOOK_SECRET:=dev-secret-change-me}"
export APP_ENV DATABASE_URL WEBHOOK_SECRET

cleanup() {
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
  fi
  if [[ -n "${WEB_PID:-}" ]]; then
    kill "$WEB_PID" 2>/dev/null || true
  fi
}

trap cleanup INT TERM EXIT

cargo run -p twi-overlay-app &
SERVER_PID=$!

pushd web/overlay > /dev/null
if [[ ! -d node_modules ]]; then
  npm install >/dev/null
fi
npm run dev -- --host &
WEB_PID=$!
popd > /dev/null

wait "$SERVER_PID" "$WEB_PID"
