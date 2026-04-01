#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${APP_DIR}/../.." && pwd)"
DOCKERFILE="${SCRIPT_DIR}/Dockerfile.native-linux-smoke"
IMAGE_NAME="${IRIS_NATIVE_DOCKER_IMAGE:-iris-browser/iris-native-linux-smoke}"
SHM_SIZE="${IRIS_NATIVE_DOCKER_SHM_SIZE:-2g}"
DOCKER_ENV_ARGS=()
RUN_COMMAND="pnpm run test:native:linux"

case "${IRIS_NATIVE_DOCKER_PLATFORM:-}" in
  "")
    case "$(uname -m)" in
      arm64|aarch64)
        PLATFORM="linux/arm64"
        ;;
      x86_64|amd64)
        PLATFORM="linux/amd64"
        ;;
      *)
        PLATFORM="linux/amd64"
        ;;
    esac
    ;;
  *)
    PLATFORM="${IRIS_NATIVE_DOCKER_PLATFORM}"
    ;;
esac

while IFS='=' read -r name _; do
  case "${name}" in
    IRIS_*|TAURI_DRIVER_PORT|HTREE_*)
      DOCKER_ENV_ARGS+=(-e "${name}")
      ;;
  esac
done < <(env)

if [[ "$#" -gt 0 ]]; then
  printf -v RUN_COMMAND '%q ' "$@"
  RUN_COMMAND="${RUN_COMMAND% }"
fi

docker build \
  --platform "${PLATFORM}" \
  -f "${DOCKERFILE}" \
  -t "${IMAGE_NAME}" \
  "${SCRIPT_DIR}"

docker_run_args=(
  docker run --rm
  --platform "${PLATFORM}"
  --shm-size "${SHM_SIZE}"
)

if ((${#DOCKER_ENV_ARGS[@]})); then
  docker_run_args+=("${DOCKER_ENV_ARGS[@]}")
fi

docker_run_args+=(
  -e "IRIS_NATIVE_DOCKER_COMMAND=${RUN_COMMAND}"
  -v "${REPO_ROOT}:/workspace"
  -v iris-browser-iris-native-node-modules:/workspace/node_modules
  -v iris-browser-iris-native-pnpm-store:/pnpm/store
  -v iris-browser-iris-native-target:/workspace/apps/iris/src-tauri/target
  -v iris-browser-iris-native-cargo-registry:/root/.cargo/registry
  -v iris-browser-iris-native-cargo-git:/root/.cargo/git
  -w /workspace/apps/iris
  "${IMAGE_NAME}"
  bash -lc '
    set -euo pipefail
    pnpm config set store-dir /pnpm/store
    pnpm --dir /workspace install --frozen-lockfile
    DBUS_SESSION_BUS_ADDRESS= dbus-run-session -- xvfb-run -a bash -lc '"'"'
      set -euo pipefail
      export GDK_SCALE=1
      export GDK_DPI_SCALE=1
      openbox >/tmp/openbox.log 2>&1 &
      eval "${IRIS_NATIVE_DOCKER_COMMAND}"
    '"'"'
  '
)

"${docker_run_args[@]}"
