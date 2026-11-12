#!/usr/bin/env bash
# Build the baseline-aligned MuPDF component exclusively from pinned source.
set -euo pipefail
if (( $# != 2 )); then echo 'usage: build-mupdf.sh TASK_ROOT NEW_OUTPUT_DIRECTORY' >&2; exit 2; fi
task_root=$(cd "$1" && pwd)
output=$2
mkdir -p "$(dirname "$output")"
mkdir "$output"
output=$(cd "$output" && pwd)
source_root="$task_root/tools/src/mupdf-1.27.0"
component_root="$task_root/checkout/components/mupdf"
[[ -f "$component_root/pilotdeck-pdf.c" && -f "$component_root/Makefile" ]]
git -C "$task_root/checkout" ls-files --error-unmatch components/mupdf/pilotdeck-pdf.c components/mupdf/Makefile > "$output/helper-files.txt"
git -C "$task_root/checkout" diff --exit-code HEAD -- components/mupdf > "$output/helper-diff.txt"
[[ $(git -C "$source_root" rev-parse HEAD) == d3b7556577b790e9761868c314ad6fd9b6dd86a9 ]]
[[ -z $(git -C "$source_root" status --porcelain --untracked-files=no) ]]
git -C "$source_root" submodule status --recursive > "$output/submodules.txt"
if LC_ALL=C grep -Eq '^[-+U]' "$output/submodules.txt"; then echo 'submodules missing or not pinned' >&2; exit 2; fi
exec 9>"$task_root/.build.lock"
flock -n 9 || { echo 'another task build holds the queue' >&2; exit 3; }
{ git -C "$source_root" rev-parse HEAD; git -C "$task_root/checkout" rev-parse HEAD; uname -a; cc --version; } > "$output/identity.txt"
set +e
make -C "$source_root" -f Makefile -f "$component_root/Makefile" \
  PILOTDECK_PDF_SOURCE="$component_root/pilotdeck-pdf.c" \
  -j6 build=release HAVE_X11=no HAVE_GLUT=no tools build/release/pilotdeck-pdf > "$output/build.log" 2>&1
status=$?
set -e
printf '%s\n' "$status" > "$output/build.exit"
(( status == 0 )) || exit "$status"
mkdir -p "$task_root/tools/mupdf/bin" "$task_root/tools/mupdf/share/licenses"
cp "$source_root/build/release/mutool" "$task_root/tools/mupdf/bin/mutool"
cp "$source_root/build/release/pilotdeck-pdf" "$task_root/tools/mupdf/bin/pilotdeck-pdf"
mkdir -p "$task_root/tools/mupdf/share/source/pilotdeck-pdf"
cp "$component_root/pilotdeck-pdf.c" "$component_root/Makefile" "$task_root/checkout/LICENSE" "$task_root/tools/mupdf/share/source/pilotdeck-pdf/"
"$task_root/tools/mupdf/bin/mutool" -v > "$output/version.txt" 2>&1
cp "$source_root/COPYING" "$task_root/tools/mupdf/share/licenses/MuPDF-COPYING"
if [[ -f "$source_root/README" ]]; then cp "$source_root/README" "$task_root/tools/mupdf/share/licenses/MuPDF-README"; fi
sha256sum "$task_root/tools/mupdf/bin/mutool" "$task_root/tools/mupdf/bin/pilotdeck-pdf" > "$output/binary.sha256"
# Ship the exact source and dependency notices together; omit only Git metadata
# and generated build products. No opaque precompiled dependency is fetched.
mkdir -p "$task_root/tools/mupdf/share/source"
tar -C "$task_root/tools/src" --exclude='.git' --exclude='mupdf-1.27.0/build' \
  -czf "$task_root/tools/mupdf/share/source/mupdf-1.27.0.tar.gz" mupdf-1.27.0
cp "$output/submodules.txt" "$output/identity.txt" "$output/binary.sha256" "$task_root/tools/mupdf/share/source/"
# The bundled source archive retains each thirdparty license in its original
# source location; release packaging must preserve this archive and manifest.
