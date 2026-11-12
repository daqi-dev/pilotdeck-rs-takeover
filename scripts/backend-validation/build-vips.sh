#!/usr/bin/env bash
# Adapt the fixed Sharp libvips source recipe to a private, bounded Linux build.
set -euo pipefail
if (( $# != 2 )); then echo 'usage: build-vips.sh TASK_ROOT NEW_OUTPUT_DIRECTORY' >&2; exit 2; fi
task_root=$(cd "$1" && pwd)
output=$2
mkdir -p "$(dirname "$output")"
mkdir "$output"
output=$(cd "$output" && pwd)
trap 'script_status=$?; printf "%s\n" "$script_status" > "$output/script.exit"' EXIT
source_root="$task_root/tools/src/sharp-libvips-1.2.4"
prefix="$task_root/tools/vips"
[[ $(uname -m) == aarch64 && $(uname -s) == Linux ]]
[[ $(git -C "$source_root" rev-parse HEAD) == 20b5e899954907a3039d6e3d4c200aaa0ec52c4c ]]
[[ -z $(git -C "$source_root" status --porcelain --untracked-files=no) ]]
[[ ! -e "$prefix" ]] || { echo 'vips prefix must be fresh; preserve any prior failed build' >&2; exit 2; }
export PATH="$task_root/tools/build-tools/bin:$task_root/tools/rust/bin:$PATH"
for command in meson ninja cargo-cbuild gperf cmake pkg-config autoreconf gettext patch; do
  command -v "$command" >> "$output/build-tools.txt"
done
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
mkdir -p "$output/home" "$output/platform" "$output/downloads"
export HOME="$output/home"
export CARGO_BUILD_JOBS=6
export PDRS_VIPS_SOURCE="$source_root"
export PDRS_VIPS_DEPS="$output/deps"
export PDRS_VIPS_PREFIX="$prefix"
export PDRS_VIPS_PLATFORM="$output/platform"
export PDRS_VIPS_DOWNLOADS="$output/downloads"
export PDRS_VIPS_DOWNLOADER="$output/download-source.sh"
if [[ -n "${PDRS_VIPS_RSVG_LOCK:-}" ]]; then
  PDRS_VIPS_RSVG_LOCK=$(readlink -f "$PDRS_VIPS_RSVG_LOCK")
  [[ "$PDRS_VIPS_RSVG_LOCK" == "$task_root/"* && -f "$PDRS_VIPS_RSVG_LOCK" ]]
  export PDRS_VIPS_RSVG_LOCK
  sha256sum "$PDRS_VIPS_RSVG_LOCK" > "$output/requested-rsvg-lock.sha256"
fi
exec 9>"$task_root/.build.lock"
flock -n 9 || { echo 'another task build holds the queue' >&2; exit 3; }
{ git -C "$source_root" rev-parse HEAD; uname -a; cc --version; rustc --version; meson --version; ninja --version; } > "$output/identity.txt"
cp "$source_root/platforms/linux-arm64v8/Toolchain.cmake" "$output/platform/Toolchain.cmake"
# Relative install locations are essential: the upstream packaging cross-file
# contains /etc, /var and /usr/share, which are outside this task's authority.
cat > "$output/platform/meson.ini" <<'INI'
[binaries]
strip = 'strip'
[built-in options]
libdir = 'lib'
datadir = 'share'
localedir = 'share/locale'
sysconfdir = 'etc'
localstatedir = 'var'
wrap_mode = 'nofallback'
INI
cat > "$output/download-source.sh" <<'DOWNLOAD'
#!/usr/bin/env bash
set -euo pipefail
(( $# == 1 )) && [[ "$1" == https://* ]] || exit 2
key=$(printf '%s' "$1" | sha256sum | cut -d' ' -f1)
file="$PDRS_VIPS_DOWNLOADS/$key.source"
if [[ ! -f "$file" ]]; then
  curl --fail --show-error --silent --location --retry 3 --retry-max-time 60 "$1" -o "$file.partial"
  mv "$file.partial" "$file"
fi
hash=$(sha256sum "$file" | cut -d' ' -f1)
printf '%s\t%s\t%s\n' "$key" "$hash" "$1" >> "$PDRS_VIPS_DOWNLOADS/manifest.tsv"
cat "$file"
DOWNLOAD
chmod +x "$output/download-source.sh"
python3 - "$source_root/build/posix.sh" "$output/build-source.sh" <<'PY'
from pathlib import Path
import sys
source = Path(sys.argv[1]).read_text()
replacements = {
    'set -e\n': 'set -e -o pipefail\n',
    '/packaging/versions.properties': '${PDRS_VIPS_SOURCE}/versions.properties',
    'DEPS=/deps': 'DEPS="${PDRS_VIPS_DEPS}"',
    'TARGET=/target': 'TARGET="${PDRS_VIPS_PREFIX}"',
    'PACKAGE=/packaging': 'PACKAGE="${PDRS_VIPS_SOURCE}"',
    'ROOT=/root': 'ROOT="${PDRS_VIPS_PLATFORM}"',
    'export MAKEFLAGS="-j$(nproc)"': 'export MAKEFLAGS="-j6"',
    'CURL="curl --silent --location --retry 3 --retry-max-time 30"': 'CURL="${PDRS_VIPS_DOWNLOADER}"',
    'cargo update --workspace': '''if [ -n "${PDRS_VIPS_RSVG_LOCK:-}" ]; then
  cp "$PDRS_VIPS_RSVG_LOCK" Cargo.lock
  cargo metadata --locked --no-deps --format-version 1 > /dev/null
else
  cargo update --workspace
fi''',
}
for before, after in replacements.items():
    expected = 2 if before == '/packaging/versions.properties' else 1
    if source.count(before) != expected:
        raise SystemExit(f'upstream recipe changed: {before}')
    source = source.replace(before, after)
# Keep headers, pkg-config files and static dependency libraries so the native
# component and the baseline Sharp addon can compile against the same prefix.
marker = '\n# Cleanup\n'
if source.count(marker) != 1:
    raise SystemExit('upstream cleanup boundary changed')
source = source.split(marker)[0] + '\n'
Path(sys.argv[2]).write_text(source)
PY
bash -n "$output/build-source.sh"
diff -u "$source_root/build/posix.sh" "$output/build-source.sh" > "$output/task-adaptation.patch" || [[ $? == 1 ]]
export PLATFORM=linux-arm64v8
export CHOST="$(cc -dumpmachine)"
export RUST_TARGET=aarch64-unknown-linux-gnu
export FLAGS='-march=armv8-a'
export PKG_CONFIG='pkg-config --static'
export MESON="--native-file=$output/platform/meson.ini"
set +e
bash "$output/build-source.sh" > "$output/build.log" 2>&1
status=$?
set -e
printf '%s\n' "$status" > "$output/build.exit"
(( status == 0 )) || exit "$status"
if [[ -n "${PDRS_VIPS_RSVG_LOCK:-}" ]]; then
  cmp "$PDRS_VIPS_RSVG_LOCK" "$output/deps/rsvg/Cargo.lock"
fi
PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig" pkg-config --modversion vips-cpp > "$output/version.txt"
[[ $(cat "$output/version.txt") == 8.17.3 ]]
mkdir -p "$prefix/share/source" "$prefix/share/licenses"
cp "$source_root/LICENSE" "$source_root/THIRD-PARTY-NOTICES.md" "$prefix/share/licenses/"
cp "$source_root/versions.properties" "$output/identity.txt" "$output/task-adaptation.patch" "$output/build-source.sh" "$prefix/share/source/"
cp "$output/downloads/manifest.tsv" "$prefix/share/source/downloads.tsv"
# Original archives plus the actual patched source retain dependency notices.
# Rust dependencies must also be vendored for the source distribution.
cargo vendor --locked --manifest-path "$output/deps/rsvg/Cargo.toml" "$prefix/share/source/rsvg-vendor" > "$prefix/share/source/rsvg-vendor-config.toml" 2> "$output/vendor.log"
cp "$output/deps/rsvg/Cargo.lock" "$prefix/share/source/rsvg-Cargo.lock"
tar -C "$output" --exclude='_build' --exclude='target' --exclude='*.o' --exclude='*.lo' --exclude='.git' \
  -czf "$prefix/share/source/libvips-source-8.17.3.tar.gz" deps downloads platform
find "$prefix/lib" -maxdepth 1 -type f -name '*.so*' -exec sha256sum '{}' + > "$output/binary.sha256"
cp "$output/binary.sha256" "$prefix/share/source/"
