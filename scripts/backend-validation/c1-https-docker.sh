#!/usr/bin/env bash
# Exercise real provider URLs against loopback-only TLS fixtures, never the host.
set -euo pipefail
if (( $# != 3 )); then echo 'usage: c1-https-docker.sh TASK_ROOT NEW_OUTPUT_DIRECTORY ts|rust' >&2; exit 2; fi
task_root=$(cd "$1" && pwd)
output=$2
side=$3
[[ "$side" == ts || "$side" == rust ]]
harness_root="${PDRS_C1_HARNESS_ROOT:-$task_root/checkout}"
[[ "$harness_root" == "$task_root/"* && "$output" == "$task_root/"* && ! -e "$output" ]]
[[ -f "$harness_root/src/pilotdeck-cli/tests/fixtures/c1-entry/setup-https-capture.mjs" ]]
mkdir -p "$(dirname "$output")"
component="$task_root/baseline/PilotDeck"
extra=()
if [[ "$side" == rust ]]; then
  component="$task_root/checkout/src/target/debug/pilotdeck"
  extra=(--rust-root "$task_root/checkout" --rust-sha "$(git -C "$task_root/checkout" rev-parse HEAD)")
fi
container_name="pdrs-c1-https-$$-$(date +%s)"
printf '%s\n' "$container_name" > "$output.container-name"
git -C "$harness_root" rev-parse HEAD > "$output.harness-commit.txt"
arch=$(uname -m)
libs="/lib/$arch-linux-gnu"
[[ -d "$libs" ]]
exec docker run --rm --name "$container_name" --network none --user "$(id -u):$(id -g)" --read-only \
  --cap-drop ALL --security-opt no-new-privileges --sysctl net.ipv4.ip_unprivileged_port_start=0 \
  --add-host open.feishu.cn:127.0.0.1 --add-host open.larksuite.com:127.0.0.1 \
  --add-host work.weixin.qq.com:127.0.0.1 --add-host ilinkai.weixin.qq.com:127.0.0.1 \
  --tmpfs /tmp:rw,nosuid,nodev,size=2g \
  --mount "type=bind,src=$task_root,dst=$task_root" \
  --mount "type=bind,src=$libs,dst=$libs,readonly" \
  --mount "type=bind,src=$(command -v git),dst=/usr/local/bin/git,readonly" \
  --mount "type=bind,src=$(command -v openssl),dst=/usr/local/bin/openssl,readonly" \
  --env HOME=/tmp/c1-home --env PILOT_HOME=/tmp/c1-home/.pilotdeck --env OPENSSL_CONF=/dev/null \
  --env "PATH=$task_root/tools/node/bin:/usr/local/bin:/usr/bin:/bin" \
  --workdir "$harness_root" "${PDRS_TEST_IMAGE:-debian:bookworm-slim}" \
  node src/pilotdeck-cli/tests/fixtures/c1-entry/setup-https-capture.mjs "$side" "$component" "$output" --complete "${extra[@]}"
