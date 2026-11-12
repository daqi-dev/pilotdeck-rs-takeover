#!/usr/bin/env bash
# Compile the pinned TS baseline in the task directory, without touching reference sources.
set -euo pipefail
if (( $# != 2 )); then echo 'usage: build-baseline.sh TASK_ROOT NEW_OUTPUT_DIRECTORY' >&2; exit 2; fi
task_root=$(cd "$1" && pwd)
output=$2
mkdir -p "$(dirname "$output")"
mkdir "$output"
output=$(cd "$output" && pwd)
mkdir -p "$output/home/.pilotdeck"
export HOME="$output/home"
export PILOT_HOME="$output/home/.pilotdeck"
export PATH="$task_root/tools/node/bin:$task_root/tools/pnpm/node_modules/.bin:$task_root/tools/rust/bin:$PATH"
[[ $(node --version) == v22.22.0 ]]
[[ $(pnpm --version) == 10.32.1 ]]
cd "$task_root/baseline/PilotDeck"
[[ $(git rev-parse HEAD) == cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85 ]]
[[ -z $(git status --porcelain --untracked-files=no) ]]
exec 9>"$task_root/.build.lock"
flock -n 9 || { echo 'another task build holds the queue' >&2; exit 3; }
{ git rev-parse HEAD; node --version; pnpm --version; uname -a; } > "$output/identity.txt"
set +e
{
  pnpm --filter @pilotdeck/sdk run build &&
  pnpm --dir src/context/memory/edgeclaw-memory-core run build &&
  pnpm run build
} > "$output/build.log" 2>&1
status=$?
set -e
printf '%s\n' "$status" > "$output/build.exit"
(( status == 0 )) || exit "$status"
# The baseline resolves @vscode/ripgrep's declared binary path. Supply the
# task's source-built, pinned rg there instead of downloading its binary.
node --input-type=module - "$task_root/tools/rust/bin/rg" <<'JS'
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const { rgPath } = require('@vscode/ripgrep');
fs.mkdirSync(path.dirname(rgPath), { recursive: true });
if (!fs.existsSync(rgPath)) fs.symlinkSync(process.argv[2], rgPath);
JS
[[ -z $(git status --porcelain --untracked-files=no) ]]
