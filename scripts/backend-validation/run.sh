#!/usr/bin/env bash
# Run an immutable Git checkout, logging every gate and preserving first failures.
set -euo pipefail
if (( $# < 2 )); then echo 'usage: run.sh CHECKOUT OUTPUT_DIRECTORY [cargo arguments ...]' >&2; exit 2; fi
checkout=$(cd "$1" && pwd)
output=$2
shift 2
mkdir -p "$(dirname "$output")"
mkdir "$output"
output=$(cd "$output" && pwd)
exec 9>"${PDRS_BUILD_LOCK:-$(dirname "$checkout")/.build.lock}"
flock -n 9 || { echo 'another task build holds the queue' >&2; exit 3; }
cd "$checkout"
git rev-parse HEAD > "$output/commit.txt"
git status --porcelain > "$output/status.txt"
if [[ -s "$output/status.txt" ]]; then echo 'build checkout must be clean' >&2; exit 2; fi
{ uname -a; rustc --version; cargo --version; node --version; } > "$output/platform.txt" 2>&1
export CARGO_BUILD_JOBS=6
cd src
if (( $# )); then
  set +e
  cargo "$@" > "$output/cargo.log" 2>&1
  result=$?
  set -e
  printf '%s\n' "$result" > "$output/cargo.exit"
  exit "$result"
fi
failed=0
for gate in fmt clippy nextest doctest; do
  case "$gate" in
    fmt) command=(cargo fmt --all --check);;
    clippy) command=(cargo clippy --all-targets -- -D warnings);;
    nextest) command=(cargo nextest run --workspace --no-fail-fast --profile ci --config-file ../.config/nextest.toml -j 6);;
    doctest) command=(cargo test --workspace --doc);;
  esac
  printf '%q ' "${command[@]}" > "$output/$gate.command"
  set +e
  "${command[@]}" > "$output/$gate.log" 2>&1
  result=$?
  set -e
  printf '%s\n' "$result" > "$output/$gate.exit"
  if (( result )); then failed=1; fi
done
exit "$failed"
