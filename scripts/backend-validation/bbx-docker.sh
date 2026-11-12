#!/usr/bin/env bash
# Run fixed-port CLI fixtures without exposing a host listener or user home.
set -euo pipefail
if (( $# < 2 )); then echo 'usage: bbx-docker.sh TASK_ROOT NEW_OUTPUT_DIRECTORY [check.mjs arguments ...]' >&2; exit 2; fi
task_root=$(cd "$1" && pwd)
harness_root="${PDRS_BBX_HARNESS_ROOT:-$task_root/checkout}"
[[ "$harness_root" == "$task_root/"* && -f "$harness_root/scripts/bbx/check.mjs" ]] || { echo 'harness must be inside task root' >&2; exit 2; }
output=$2
shift 2
[[ "$output" == "$task_root/"* ]] || { echo 'output must be inside task root' >&2; exit 2; }
[[ ! -e "$output" ]] || { echo 'output must be new' >&2; exit 2; }
[[ $("$task_root/tools/node/bin/node" --version) == v22.22.0 ]]
mkdir -p "$(dirname "$output")"
container_name="pdrs-bbx-$$-$(date +%s)"
printf '%s\n' "$container_name" > "$output.container-name"
if [[ -f "$harness_root/source-commit.txt" ]]; then
  cp "$harness_root/source-commit.txt" "$output.harness-commit.txt"
else
  git -C "$harness_root" rev-parse HEAD > "$output.harness-commit.txt"
fi
arch=$(uname -m)
libs="/lib/$arch-linux-gnu"
[[ -d "$libs" ]]
rust_toolchain="${PDRS_RUST_TOOLCHAIN:-/home/clawbox-08/.rustup/toolchains/1.99-aarch64-unknown-linux-gnu}"
[[ -x "$rust_toolchain/bin/rustc" ]]
# Existing system executables and their libraries are mounted read-only; no
# user home, Cargo credentials, host config or host network is exposed.
exec docker run --rm --name "$container_name" --network none --user "$(id -u):$(id -g)" --read-only \
  --cap-drop ALL --security-opt no-new-privileges \
  --tmpfs /tmp:rw,nosuid,nodev,size=2g \
  --mount "type=bind,src=$task_root,dst=$task_root" \
  --mount "type=bind,src=$libs,dst=$libs,readonly" \
  --mount "type=bind,src=$(command -v git),dst=/usr/local/bin/git,readonly" \
  --mount "type=bind,src=$(command -v lsof),dst=/usr/local/bin/lsof,readonly" \
  --mount "type=bind,src=$rust_toolchain,dst=/tools/rustc,readonly" \
  --env HOME=/tmp/bbx-home --env PILOT_HOME=/tmp/bbx-home/.pilotdeck \
  --env "PATH=$task_root/tools/node/bin:$task_root/tools/pnpm/node_modules/.bin:$task_root/tools/rust/bin:/tools/rustc/bin:/usr/local/bin:/usr/bin:/bin" \
  --env "BBX_BASELINE=$task_root/baseline/PilotDeck" \
  --env "BBX_TARGET=$task_root/checkout/src/target/debug" \
  --env "BBX_SRC=$task_root/checkout/src" \
  --env BBX_TMP_ROOT=/tmp \
  --env "BBX_PDF_TOOL=$task_root/tools/mupdf/bin/pilotdeck-pdf" \
  --workdir "$harness_root" \
  "${PDRS_TEST_IMAGE:-debian:bookworm-slim}" \
  node scripts/bbx/check.mjs --out "$output" "$@"
