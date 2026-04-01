#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
BINARY="${IRIS_BINARY:-${APP_DIR}/src-tauri/target/debug/iris}"
AUTOMATION_PORT="${IRIS_AUTOMATION_PORT:-21977}"
DATA_DIR="${HTREE_DATA_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/iris-native-smoke-XXXXXX")}"

if [[ ! -x "${BINARY}" ]]; then
  echo "Iris binary not found or not executable: ${BINARY}" >&2
  echo "Build it first with: pnpm exec tauri build --debug --no-bundle" >&2
  exit 1
fi

export IRIS_AUTOMATION=1
export IRIS_AUTOMATION_PORT="${AUTOMATION_PORT}"
export HTREE_DATA_DIR="${DATA_DIR}"

exec "${BINARY}" "$@"
