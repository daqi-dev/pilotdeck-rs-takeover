#!/usr/bin/env bash
# Keep Ninja's global concurrency bounded without changing tool-mode arguments.
set -euo pipefail
arguments=()
while (( $# )); do
  case "$1" in
    -t|--tool|-t?*|--tool=*|--)
      arguments+=("$@")
      break
      ;;
    -j|--jobs)
      (( $# >= 2 )) || { echo 'ninja: jobs option needs a value' >&2; exit 2; }
      shift 2
      ;;
    -j[0-9]*|--jobs=*) shift ;;
    *) arguments+=("$1"); shift ;;
  esac
done
real="${PDRS_NINJA_REAL:-$(dirname "$0")/../libexec/ninja}"
exec "$real" -j6 "${arguments[@]}"
