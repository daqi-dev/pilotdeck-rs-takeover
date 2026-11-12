#!/usr/bin/env bash
# Compile the Node-free image helper against this task's fixed source-built codecs.
set -euo pipefail
if (( $# != 2 )); then echo 'usage: build-image.sh TASK_ROOT NEW_OUTPUT_DIRECTORY' >&2; exit 2; fi
task_root=$(cd "$1" && pwd)
output=$2
mkdir -p "$(dirname "$output")"
mkdir "$output"
output=$(cd "$output" && pwd)
trap 'script_status=$?; printf "%s\n" "$script_status" > "$output/script.exit"' EXIT
prefix="$task_root/tools/vips"
source_root="$task_root/checkout/components/image"
export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
unset PKG_CONFIG_PATH
[[ $(pkg-config --modversion vips-cpp) == 8.17.3 ]]
git -C "$task_root/checkout" ls-files --error-unmatch components/image/pilotdeck-image.cpp components/image/Makefile > "$output/helper-files.txt"
git -C "$task_root/checkout" diff --exit-code HEAD -- components/image > "$output/helper-diff.txt"
exec 9>"$task_root/.build.lock"
flock -n 9 || { echo 'another task build holds the queue' >&2; exit 3; }
{ git -C "$task_root/checkout" rev-parse HEAD; uname -a; c++ --version; pkg-config --modversion vips-cpp; pkg-config --static --libs vips-cpp; } > "$output/identity.txt"
set +e
make -C "$source_root" OUT="$output/pilotdeck-image" PKG_CONFIG='pkg-config --static' > "$output/build.log" 2>&1
status=$?
set -e
printf '%s\n' "$status" > "$output/build.exit"
(( status == 0 )) || exit "$status"
mkdir -p "$prefix/bin" "$prefix/share/source/pilotdeck-image"
cp "$output/pilotdeck-image" "$prefix/bin/pilotdeck-image"
cp "$source_root/pilotdeck-image.cpp" "$source_root/Makefile" "$task_root/checkout/LICENSE" "$output/identity.txt" "$prefix/share/source/pilotdeck-image/"
sha256sum "$prefix/bin/pilotdeck-image" > "$output/binary.sha256"
cp "$output/binary.sha256" "$prefix/share/source/pilotdeck-image/"
readelf -d "$prefix/bin/pilotdeck-image" > "$output/dynamic.txt"
