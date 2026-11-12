#!/usr/bin/env bash
# Build the pinned baseline addon from its installed source, never @img binaries.
set -euo pipefail
if (( $# != 2 )); then echo 'usage: build-sharp.sh TASK_ROOT NEW_OUTPUT_DIRECTORY' >&2; exit 2; fi
task_root=$(cd "$1" && pwd)
output=$2
mkdir -p "$(dirname "$output")"
mkdir "$output"
output=$(cd "$output" && pwd)
trap 'status=$?; printf "%s\n" "$status" > "$output/script.exit"' EXIT
baseline="$task_root/baseline/PilotDeck"
prefix="$task_root/tools/vips"
[[ $(git -C "$baseline" rev-parse HEAD) == cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85 ]]
[[ -z $(git -C "$baseline" status --porcelain --untracked-files=no) ]]
mkdir -p "$output/home/.pilotdeck"
export HOME="$output/home" PILOT_HOME="$output/home/.pilotdeck"
export PATH="$task_root/tools/node/bin:$task_root/tools/sharp-build/node_modules/.bin:$PATH"
export NODE_PATH="$task_root/tools/sharp-build/node_modules"
export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
unset PKG_CONFIG_PATH
[[ $(node --version) == v22.22.0 && $(pkg-config --modversion vips-cpp) == 8.17.3 ]]
[[ -f "$task_root/tools/node/include/node/node_api.h" ]]
export npm_config_nodedir="$task_root/tools/node" npm_config_jobs=6
export npm_config_cache="$task_root/tools/npm-cache"
export SHARP_FORCE_GLOBAL_LIBVIPS=1
export LD_LIBRARY_PATH="$prefix/lib"
export LDFLAGS="-Wl,-rpath,$prefix/lib"
exec 9>"$task_root/.build.lock"
flock -n 9 || { echo 'another task build holds the queue' >&2; exit 3; }
cd "$baseline/node_modules/sharp"
node -e 'const a=require("./package.json"); if(a.version!=="0.34.5") throw Error("wrong sharp source"); for(const [p,v] of [["node-gyp","11.5.0"],["node-addon-api","8.5.0"]]) if(require(p+"/package.json").version!==v) throw Error("wrong build tool");'
{ git -C "$baseline" rev-parse HEAD; node --version; cc --version; pkg-config --modversion vips-cpp; sha256sum "$task_root/tools/sharp-build/package-lock.json"; } > "$output/identity.txt"
set +e
node install/build.js > "$output/build.log" 2>&1
status=$?
set -e
printf '%s\n' "$status" > "$output/build.exit"
(( status == 0 )) || exit "$status"
node - "$output" <<'JS'
const fs = require('node:fs'), path = require('node:path'), crypto = require('node:crypto');
const sharp = require('./');
if (sharp.versions.sharp !== '0.34.5' || sharp.versions.vips !== '8.17.3') throw Error('wrong native library versions');
const addons = Object.keys(require.cache).filter(file => file.endsWith('.node') && file.includes('sharp'));
if (addons.length !== 1 || !addons[0].includes('/sharp/src/build/Release/')) throw Error('source addon was not loaded');
const hash = file => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
fs.writeFileSync(path.join(process.argv[2], 'addon.json'), JSON.stringify({path:addons[0], sha256:hash(addons[0]), versions:sharp.versions}, null, 2) + '\n');
JS
[[ -z $(git -C "$baseline" status --porcelain --untracked-files=no) ]]
cp package.json LICENSE "$output/"
cp "$task_root/tools/sharp-build/package-lock.json" "$output/build-tools-package-lock.json"
tar --exclude=build --exclude=node_modules -czf "$output/sharp-source-0.34.5.tar.gz" src lib install package.json LICENSE
