#!/usr/bin/env bash
# End-to-end verification of MintHub-downloaded `pilotdeck-rs/*` packages stitched together:
#   Stage 1 : Pure remote `mint add` (8 top-level roots, 0 path: args) -> 60 locked + `mint verify` + scaffold
#   Stage 2A: Pure-14 workspace build + `nextest` WITHOUT `deps/agent-kernel`
#   Stage 2B: Compatibility probe of `pilotdeck-model-iface` against Hub `agentcpm/iface@0.1.6`
#   Stage 3 : Full-20 downloaded crates + `pilotdeck-native-loop` binary + full workspace `nextest` + harnesses
#   Stage 4 : Cross-package `pilotdeck-minthub-stitched-e2e` over live `agentcpm-host` UDS & builtin tools
set -euo pipefail
umask 022

if [ -d "${HOME:-}/.cargo/bin" ]; then
  PATH="$HOME/.cargo/bin:$PATH"
  export PATH
fi

REGISTRY="https://mint-hub.ea.modelbest.co"
MINT=""
OUT_REPORT_DIR=""
WORK_DIR=""
JOBS=6
FULL_TEST_THREADS=4
KEEP_WORK_DIR=0

usage() {
  cat <<'EOF'
Usage: scripts/verify-minthub-stitched-assembly.sh [OPTIONS]

  --mint <abs-path>         Executable mint CLI path (auto-detected if on PATH)
  --registry <url>          MintHub registry URL (default: https://mint-hub.ea.modelbest.co)
  --out-report-dir <abs>    Directory to store all stage logs + stitched-summary.json (required)
  --work-dir <abs>          Temporary stitched build root outside repo (default: /tmp/pd-mhs-XXXXXX)
  --jobs <N>                Cargo parallel jobs (default: 6)
  --full-test-threads <N>   Max concurrent nextest threads for Stage 3 UDS tests (default: 4)
  --keep-work-dir           Keep temporary work-dir on exit for manual inspection
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --mint) MINT="${2:?}"; shift 2 ;;
    --registry) REGISTRY="${2:?}"; shift 2 ;;
    --out-report-dir) OUT_REPORT_DIR="${2:?}"; shift 2 ;;
    --work-dir) WORK_DIR="${2:?}"; shift 2 ;;
    --jobs) JOBS="${2:?}"; shift 2 ;;
    --full-test-threads) FULL_TEST_THREADS="${2:?}"; shift 2 ;;
    --keep-work-dir) KEEP_WORK_DIR=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

if [ "$JOBS" -lt "$FULL_TEST_THREADS" ]; then
  FULL_TEST_THREADS="$JOBS"
fi

[ -n "$OUT_REPORT_DIR" ] || { printf 'ERROR: --out-report-dir is required\n' >&2; exit 2; }
case "$OUT_REPORT_DIR" in
  /*) : ;;
  *) printf 'ERROR: --out-report-dir must be absolute: %s\n' "$OUT_REPORT_DIR" >&2; exit 2 ;;
esac

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCAFFOLD_PY="$REPO_ROOT/scripts/lib/pilotdeck_minthub_scaffold.py"
NEXTEST_CFG="$REPO_ROOT/.config/nextest.toml"

pick_python() {
  local cand
  for cand in "${PYTHON_BIN:-}" python3.13 python3.12 python3.11 python3; do
    [ -n "$cand" ] || continue
    if command -v "$cand" >/dev/null 2>&1 && "$cand" -I -c 'import tomllib' >/dev/null 2>&1; then
      command -v "$cand"
      return 0
    fi
  done
  return 1
}
PYTHON="$(pick_python)" || { printf 'ERROR: python >= 3.11 with tomllib required\n' >&2; exit 10; }

if [ -z "$MINT" ]; then
  MINT="$(command -v mint || true)"
fi
[ -n "$MINT" ] && [ -x "$MINT" ] || { printf 'ERROR: executable mint CLI not found\n' >&2; exit 2; }

MINT_EXEC="$("$PYTHON" -I -c '
from pathlib import Path
import os, re, sys
p = Path(sys.argv[1])
raw = p.read_bytes()[:512]
if raw.startswith(b"#!"):
    t = raw.decode("utf-8", errors="replace")
    m = re.search(r"^MINT_HOME=[\x27\"]([^\x27\"]+)[\x27\"]\s*$", t, re.M)
    if m and "exec \"$MINT_HOME/bin/mint\"" in t:
        inner = Path(m.group(1)) / "bin" / "mint"
        if inner.is_file() and os.access(inner, os.X_OK):
            print(str(inner))
            raise SystemExit(0)
print(str(p))
' "$MINT")"

if [ -z "$WORK_DIR" ]; then
  WORK_DIR="$(mktemp -d /tmp/pd-mhs-XXXXXX)"
else
  mkdir -p "$WORK_DIR"
fi
mkdir -p "$OUT_REPORT_DIR"

cleanup() {
  if [ "$KEEP_WORK_DIR" -eq 0 ] && [ -n "${WORK_DIR:-}" ] && [ -d "$WORK_DIR" ]; then
    rm -rf "$WORK_DIR"
  fi
}
trap cleanup EXIT

export LANG="${LANG:-en_US.UTF-8}"
export LC_ALL="${LC_ALL:-en_US.UTF-8}"
MINT_HOME_DIR="$WORK_DIR/mint-home"
PROJ_DIR="$WORK_DIR/proj-stitched"
STITCH_OUT="$WORK_DIR/stitched"
mkdir -p "$MINT_HOME_DIR" "$PROJ_DIR" "$STITCH_OUT"

printf 'repo=%s\nwork=%s\nreport=%s\nmint=%s\njobs=%s\n' \
  "$REPO_ROOT" "$WORK_DIR" "$OUT_REPORT_DIR" "$MINT_EXEC" "$JOBS"

# Compute the exact top-level published pilotdeck-rs/*@=<ver> roots
ROOTS_TXT="$WORK_DIR/online-roots.txt"
"$PYTHON" -I - "$REPO_ROOT/packages/minthub" "$ROOTS_TXT" <<'PY'
from pathlib import Path
import sys, tomllib

pkg_base = Path(sys.argv[1])
out_path = Path(sys.argv[2])
order = [
    "child-signals", "regex-proto", "tokenizer", "yaml", "protocol",
    "context-core", "http-transport", "model-core", "tool-core", "transport",
    "agent", "context-runtime", "model-runtime", "turn-preparation",
    "model-iface", "tool-iface", "trusted-http-host", "context-iface",
    "session-migration", "iface",
]
versions, incoming = {}, {}
for s in order:
    doc = tomllib.loads((pkg_base / s / "mint.toml").read_text(encoding="utf-8"))
    name = doc["package"]["name"]
    versions[name] = doc["package"]["version"]
    incoming[name] = 0
for s in order:
    doc = tomllib.loads((pkg_base / s / "mint.toml").read_text(encoding="utf-8"))
    for dep in (doc.get("dependencies") or {}):
        if dep.startswith("pilotdeck-rs/"):
            incoming[dep] += 1
roots = [f"{n}@={versions[n]}" for n in versions if incoming[n] == 0]
out_path.write_text("\n".join(roots) + "\n", encoding="utf-8")
PY

ROOT_ARGS=()
while IFS= read -r line; do
  [ -n "$line" ] && ROOT_ARGS+=("$line")
done < "$ROOTS_TXT"

# ── Stage 1: Warm index cache in $MINT_HOME_DIR, then pure 8-root remote install + verify + scaffold ──
WARM_DIR="$WORK_DIR/warm-cache"
mkdir -p "$WARM_DIR"
(
  cd "$WARM_DIR"
  set +e
  MINT_HOME="$MINT_HOME_DIR" "$MINT_EXEC" --registry "$REGISTRY" init "$WARM_DIR" -y --no-preset >"$OUT_REPORT_DIR/01-mint-warm.log" 2>&1
  MINT_HOME="$MINT_HOME_DIR" "$MINT_EXEC" --registry "$REGISTRY" add -y agentcpm/host iface/loop >>"$OUT_REPORT_DIR/01-mint-warm.log" 2>&1
  set -e
)
rm -rf "$WARM_DIR"

(
  cd "$PROJ_DIR"
  set +e
  MINT_HOME="$MINT_HOME_DIR" "$MINT_EXEC" --registry "$REGISTRY" init "$PROJ_DIR" -y --no-preset >"$OUT_REPORT_DIR/01-mint-init.log" 2>&1
  INIT_RC=$?
  set -e
  if [ "$INIT_RC" -ne 0 ]; then
    if [ -f "$PROJ_DIR/mint.toml" ] && [ -d "$PROJ_DIR/.mint" ]; then
      printf 'NOTE: mint init exited %d on %s (host core flavor absent); mint.toml and .mint/ initialized, continuing.\n' "$INIT_RC" "$PROJ_DIR" >>"$OUT_REPORT_DIR/01-mint-init.log"
    else
      cat "$OUT_REPORT_DIR/01-mint-init.log" >&2 || true
      exit "$INIT_RC"
    fi
  fi
  MINT_HOME="$MINT_HOME_DIR" "$MINT_EXEC" --registry "$REGISTRY" add -y "${ROOT_ARGS[@]}" >"$OUT_REPORT_DIR/01-mint-add.log" 2>&1
  MINT_HOME="$MINT_HOME_DIR" "$MINT_EXEC" list >"$OUT_REPORT_DIR/01-mint-list.log" 2>&1
  MINT_HOME="$MINT_HOME_DIR" "$MINT_EXEC" verify >"$OUT_REPORT_DIR/01-mint-verify.log" 2>&1
)
"$PYTHON" -I "$SCAFFOLD_PY" \
  --project "$PROJ_DIR" \
  --repo-root "$REPO_ROOT" \
  --out "$STITCH_OUT" \
  --summary "$OUT_REPORT_DIR/scaffold-plan.json" >"$OUT_REPORT_DIR/01-scaffold.log" 2>&1

cp "$STITCH_OUT/components/install.json" "$OUT_REPORT_DIR/install.json"
cp "$STITCH_OUT/install.excluded.json" "$OUT_REPORT_DIR/install.excluded.json"

# ── Stage 2A: Pure-14 workspace (ZERO deps/agent-kernel) ──────────────────────
PURE_MANIFEST="$STITCH_OUT/pure-build/src/Cargo.toml"
PURE_TARGET="$STITCH_OUT/pure-target"
CARGO_TARGET_DIR="$PURE_TARGET" cargo nextest run \
  --manifest-path "$PURE_MANIFEST" \
  --workspace --offline --build-jobs "$JOBS" --test-threads "$JOBS" \
  --no-fail-fast --profile ci --config-file "$NEXTEST_CFG" \
  >"$OUT_REPORT_DIR/02a-pure14-nextest.log" 2>&1

# ── Stage 2B: Adapter vs MintHub agentcpm/iface@0.1.6 compatibility probe ───────
PROBE_MANIFEST="$STITCH_OUT/hub016-probe/src/Cargo.toml"
PROBE_TARGET="$STITCH_OUT/probe-target"
set +e
CARGO_TARGET_DIR="$PROBE_TARGET" cargo check \
  --manifest-path "$PROBE_MANIFEST" \
  -p pilotdeck-model-iface --offline -j "$JOBS" \
  >"$OUT_REPORT_DIR/02b-hub016-adapter-probe.log" 2>&1
PROBE_RC=$?
set -e
if [ "$PROBE_RC" -eq 0 ]; then
  printf 'UNEXPECTED: pilotdeck-model-iface compiled cleanly against MintHub agentcpm/iface@0.1.6\n' >>"$OUT_REPORT_DIR/02b-hub016-adapter-probe.log"
fi

# ── Stage 3: Full 20 downloaded bundles + binary build + workspace tests ────────
FULL_MANIFEST="$STITCH_OUT/full-build/src/Cargo.toml"
FULL_TARGET="$STITCH_OUT/full-target"
CARGO_TARGET_DIR="$FULL_TARGET" cargo build \
  --manifest-path "$FULL_MANIFEST" \
  --release \
  -p pilotdeck-iface --features host-composition \
  --bin pilotdeck-native-loop --example managed_native --example resident_native \
  --offline -j "$JOBS" \
  >"$OUT_REPORT_DIR/03a-full20-build-bins.log" 2>&1

NATIVE_LOOP_BIN="$FULL_TARGET/release/pilotdeck-native-loop"
MANAGED_FIXTURE_BIN="$FULL_TARGET/release/examples/managed_native"
RESIDENT_FIXTURE_BIN="$FULL_TARGET/release/examples/resident_native"
[ -x "$NATIVE_LOOP_BIN" ] || { printf 'ERROR: missing stitched pilotdeck-native-loop at %s\n' "$NATIVE_LOOP_BIN" >&2; exit 1; }

PILOTDECK_IFACE_NATIVE_LOOP_BIN="$NATIVE_LOOP_BIN" \
PILOTDECK_IFACE_MANAGED_NATIVE_FIXTURE="$MANAGED_FIXTURE_BIN" \
PILOTDECK_IFACE_NATIVE_FIXTURE="$RESIDENT_FIXTURE_BIN" \
CARGO_TARGET_DIR="$FULL_TARGET" cargo nextest run \
  --manifest-path "$FULL_MANIFEST" \
  --workspace --exclude pilotdeck-minthub-stitched-e2e \
  --features pilotdeck-iface/host-composition \
  --offline --build-jobs "$JOBS" --test-threads "$FULL_TEST_THREADS" --no-fail-fast --profile ci --config-file "$NEXTEST_CFG" \
  >"$OUT_REPORT_DIR/03b-full20-nextest.log" 2>&1

PILOTDECK_IFACE_NATIVE_LOOP_BIN="$NATIVE_LOOP_BIN" \
PILOTDECK_IFACE_MANAGED_NATIVE_FIXTURE="$MANAGED_FIXTURE_BIN" \
PILOTDECK_IFACE_NATIVE_FIXTURE="$RESIDENT_FIXTURE_BIN" \
CARGO_TARGET_DIR="$FULL_TARGET" cargo test \
  --manifest-path "$FULL_MANIFEST" \
  -p pilotdeck-iface --features host-composition \
  --test resident_model --test managed_model \
  --offline -j "$JOBS" \
  >"$OUT_REPORT_DIR/03c-full20-harnesses.log" 2>&1

# ── Stage 4: Stitched cross-package E2E & clippy -D warnings ──────────────────
CARGO_TARGET_DIR="$FULL_TARGET" cargo clippy \
  --manifest-path "$FULL_MANIFEST" \
  -p pilotdeck-minthub-stitched-e2e --all-targets --offline -j "$JOBS" -- -D warnings \
  >"$OUT_REPORT_DIR/04a-stitched-clippy.log" 2>&1

STITCHED_COMPONENTS_DIR="$STITCH_OUT/components" \
STITCHED_NATIVE_LOOP_BIN="$NATIVE_LOOP_BIN" \
CARGO_TARGET_DIR="$FULL_TARGET" cargo nextest run \
  --manifest-path "$FULL_MANIFEST" \
  -p pilotdeck-minthub-stitched-e2e \
  --offline --build-jobs "$JOBS" --test-threads 1 --no-fail-fast --profile ci --config-file "$NEXTEST_CFG" \
  >"$OUT_REPORT_DIR/04b-stitched-e2e.log" 2>&1

# Summarize machine-readable metrics
"$PYTHON" -I - "$OUT_REPORT_DIR" "$NATIVE_LOOP_BIN" "$PROBE_RC" <<'PY'
import hashlib, json, re, sys
from pathlib import Path

rep = Path(sys.argv[1])
bin_path = Path(sys.argv[2])
probe_rc = int(sys.argv[3])
plan = json.loads((rep / "scaffold-plan.json").read_text(encoding="utf-8"))

def parse_nextest_summary(log_name: str) -> dict:
    txt = (rep / log_name).read_text(encoding="utf-8", errors="replace")
    txt = re.sub(r"\x1b\[[0-9;]*m", "", txt)
    m = re.findall(r"Summary \[[^\]]+\]\s+(\d+)\s+tests run:\s+(\d+)\s+passed(?:\s+\((\d+)\s+slow\))?(?:,\s+(\d+)\s+failed)?(?:,\s+(\d+)\s+skipped)?", txt)
    if not m:
        return {"raw_summary_found": False}
    last = m[-1]
    return {
        "raw_summary_found": True,
        "tests_run": int(last[0]),
        "passed": int(last[1]),
        "slow": int(last[2] or 0),
        "failed": int(last[3] or 0),
        "skipped": int(last[4] or 0),
    }

h = hashlib.sha256(bin_path.read_bytes()).hexdigest()
harness_txt = (rep / "03c-full20-harnesses.log").read_text(encoding="utf-8", errors="replace")
probe_txt = (rep / "02b-hub016-adapter-probe.log").read_text(encoding="utf-8", errors="replace")

summary = {
    "schema": "pilotdeck.minthub.stitched-summary/v1",
    "platform": f"{sys.platform}",
    "locked_total": plan["locked_total"],
    "install_declared_count": plan["install_declared_count"],
    "install_excluded_count": plan["install_excluded_count"],
    "install_json_sha256": plan["install_json_sha256"],
    "stitched_native_loop_sha256": h,
    "stage_2a_pure_14_nextest": parse_nextest_summary("02a-pure14-nextest.log"),
    "stage_2b_hub016_probe": {
        "exit_code": probe_rc,
        "expected_incompatible_with_hub_0_1_6": probe_rc != 0,
        "mentions_missing_018_items": ("http_pull" in probe_txt or "model_session" in probe_txt or "could not compile" in probe_txt),
    },
    "stage_3_full_20_nextest": parse_nextest_summary("03b-full20-nextest.log"),
    "stage_3_harnesses": {
        "resident_model_ok": "resident_model: 3 checks passed" in harness_txt,
        "managed_model_ok": "managed_model: 4 checks passed" in harness_txt,
        "demand_gate_checkpoints_ok": "gate-child: 7 checkpoint groups passed" in harness_txt,
    },
    "stage_4_stitched_e2e_nextest": parse_nextest_summary("04b-stitched-e2e.log"),
}
(rep / "stitched-summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
print(json.dumps(summary, ensure_ascii=False, indent=2))
assert all(summary["stage_3_harnesses"].values()), f"harness summary check failed: {summary['stage_3_harnesses']}"
PY
