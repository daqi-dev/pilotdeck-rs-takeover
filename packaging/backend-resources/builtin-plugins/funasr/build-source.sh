#!/usr/bin/env bash
# Run only in an allocated native build slot. No binary release is downloaded.
set -euo pipefail
case "${1:-}" in --prepare-only|--build) funasr_mode=$1 ;; *) printf 'usage: %s --prepare-only|--build\n' "$0" >&2; exit 2 ;; esac
: "${FUNASR_WORK_ROOT:?Set an isolated absolute source/build directory}"
: "${FUNASR_RUNTIME_ROOT:?Set the isolated funasr cache destination}"
: "${FUNASR_CMAKE:?Set the previously source-built cmake executable}"
case "$FUNASR_WORK_ROOT:$FUNASR_RUNTIME_ROOT:$FUNASR_CMAKE" in /*:/*:/*) ;; *) exit 2 ;; esac
if [[ $(uname -s) == Darwin && $(uname -m) == arm64 ]]; then
    funasr_platform=darwin-arm64
elif [[ $(uname -s) == Linux ]]; then
    case $(uname -m) in x86_64) funasr_platform=linux-x64 ;; aarch64) funasr_platform=linux-arm64 ;; *) exit 2 ;; esac
else
    printf '%s\n' 'Unsupported supply recipe platform; Windows ownership remains unavailable.' >&2
    exit 2
fi
mkdir -p "$FUNASR_WORK_ROOT" "$FUNASR_RUNTIME_ROOT/runtime/v0.2.0/$funasr_platform"
fetch_source() {
    local name=$1 url=$2 commit=$3
    local destination="$FUNASR_WORK_ROOT/$name"
    if [[ ! -d "$destination/.git" ]]; then
        git init "$destination"
        git -C "$destination" remote add origin "$url"
        git -C "$destination" fetch --depth 1 origin "$commit"
        git -C "$destination" checkout --detach FETCH_HEAD
    fi
    [[ $(git -C "$destination" rev-parse HEAD) == "$commit" ]]
    [[ -z $(git -C "$destination" status --porcelain) ]]
}
fetch_source funasr https://github.com/modelscope/FunASR.git 500956bc331bb7edbaac58d8f84a84f28bd3d29f
fetch_source llama https://github.com/ggml-org/llama.cpp.git 803b7fcae893e9caaee3921779628fef83ac0965
if [[ "$funasr_mode" == --prepare-only ]]; then
    printf 'Prepared pinned FunASR and llama source; no configure/build executed.\n'
    exit 0
fi
"$FUNASR_CMAKE" -S "$FUNASR_WORK_ROOT/funasr/runtime/llama.cpp" -B "$FUNASR_WORK_ROOT/build" \
    -DCMAKE_BUILD_TYPE=Release -DFETCHCONTENT_SOURCE_DIR_LLAMA="$FUNASR_WORK_ROOT/llama" \
    -DFETCHCONTENT_FULLY_DISCONNECTED=ON -DGGML_NATIVE=OFF -DGGML_CUDA=OFF -DGGML_VULKAN=OFF
"$FUNASR_CMAKE" --build "$FUNASR_WORK_ROOT/build" --parallel 4 --target llama-funasr-sensevoice
install -m 755 "$FUNASR_WORK_ROOT/build/bin/llama-funasr-sensevoice" "$FUNASR_RUNTIME_ROOT/runtime/v0.2.0/$funasr_platform/llama-funasr-sensevoice"
mkdir -p "$FUNASR_RUNTIME_ROOT/licenses/FunASR" "$FUNASR_RUNTIME_ROOT/licenses/llama.cpp"
install -m 644 "$FUNASR_WORK_ROOT/funasr/LICENSE" "$FUNASR_RUNTIME_ROOT/licenses/FunASR/LICENSE"
install -m 644 "$FUNASR_WORK_ROOT/llama/LICENSE" "$FUNASR_RUNTIME_ROOT/licenses/llama.cpp/LICENSE"
