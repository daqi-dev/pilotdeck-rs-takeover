#!/usr/bin/env bash
# Source-only prerequisites for the fixed libvips recipe; shares the build queue.
set -euo pipefail
if (( $# != 2 )); then echo 'usage: prepare-vips-tools.sh TASK_ROOT NEW_OUTPUT_DIRECTORY' >&2; exit 2; fi
task_root=$(cd "$1" && pwd)
script_root=$(cd "$(dirname "$0")" && pwd)
output=$2
mkdir -p "$(dirname "$output")"
mkdir "$output"
output=$(cd "$output" && pwd)
trap 'script_status=$?; printf "%s\n" "$script_status" > "$output/script.exit"' EXIT
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
mkdir -p "$output/home" "$task_root/tools/src" "$task_root/tools/build-tools/bin" "$task_root/tools/build-tools/libexec"
export HOME="$output/home"
export PATH="$task_root/tools/build-tools/bin:$task_root/tools/rust/bin:$PATH"
export CARGO_BUILD_JOBS=6
exec 9>"$task_root/.build.lock"
flock -n 9 || { echo 'another task build holds the queue' >&2; exit 3; }
clone_fixed() {
  local url=$1 tag=$2 expected=$3 dest=$4
  if [[ ! -d "$dest" ]]; then git clone --depth 1 --branch "$tag" "$url" "$dest"; fi
  [[ $(git -C "$dest" rev-parse HEAD) == "$expected" ]]
  [[ -z $(git -C "$dest" status --porcelain --untracked-files=no) ]]
  printf '%s\t%s\t%s\n' "$url" "$tag" "$expected" >> "$output/sources.tsv"
}
set +e
(
  set -e
  clone_fixed https://github.com/mesonbuild/meson.git 1.9.1 751b09390996163348612f0b106114256305ee40 "$task_root/tools/src/meson-1.9.1"
  clone_fixed https://github.com/ninja-build/ninja.git v1.13.1 79feac0f3e3bc9da9effc586cd5fea41e7550051 "$task_root/tools/src/ninja-1.13.1"
  clone_fixed https://github.com/lu-zero/cargo-c.git v0.10.18 78a62e7acbe4994040f9c44694b798ac57fd470a "$task_root/tools/src/cargo-c-0.10.18"
  printf '#!/usr/bin/env bash\nexec python3 %q "$@"\n' "$task_root/tools/src/meson-1.9.1/meson.py" > "$task_root/tools/build-tools/bin/meson"
  chmod +x "$task_root/tools/build-tools/bin/meson"
  cmake -S "$task_root/tools/src/ninja-1.13.1" -B "$output/ninja" -DCMAKE_BUILD_TYPE=Release -DBUILD_TESTING=OFF
  cmake --build "$output/ninja" --parallel 6
  cp "$output/ninja/ninja" "$task_root/tools/build-tools/libexec/ninja"
  # Meson's implicit rebuild on install otherwise lets Ninja select CPU count.
  cp "$script_root/ninja-bounded.sh" "$task_root/tools/build-tools/bin/ninja"
  chmod +x "$task_root/tools/build-tools/bin/ninja"
  curl --fail --show-error --location --retry 3 https://ftp.gnu.org/gnu/gperf/gperf-3.3.tar.gz -o "$output/gperf-3.3.tar.gz"
  sha256sum "$output/gperf-3.3.tar.gz" > "$output/gperf.sha256"
  mkdir "$output/gperf"
  tar --no-same-owner -xzf "$output/gperf-3.3.tar.gz" -C "$output/gperf" --strip-components=1
  cd "$output/gperf"
  ./configure --prefix="$task_root/tools/build-tools"
  make -j6
  make install
  cargo install --locked --path "$task_root/tools/src/cargo-c-0.10.18" --root "$task_root/tools/rust"
  meson --version
  ninja --version
  gperf --version
) > "$output/build.log" 2>&1
status=$?
set -e
printf '%s\n' "$status" > "$output/build.exit"
exit "$status"
