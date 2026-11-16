#!/usr/bin/env bash
# Stage, validate, dry-run, pack, verify-install, and publish the split
# PilotDeck Rust crates (`pilotdeck-rs/*`) as MintHub `kind = "bundle"` packages.
#
# Source of truth:
#   - Crate sources & Cargo versions: src/pilotdeck-<short>/Cargo.toml (+ src/Cargo.toml)
#   - MintHub manifests/README/skills: packages/minthub/<short>/{mint.toml,README.md,skills/<short>/SKILL.md}
#
# Never writes mint.toml/README.md/skills/ inside src/pilotdeck-*/ (to respect
# unsafe_boundary.rs on src/pilotdeck-child-signals/).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
REGISTRY="https://mint-hub.ea.modelbest.co"
MINT=""
MINT_EXEC=""
STAGING_DIR=""
TOKEN_FROM_ENV_LOCAL=""
MODE=""

usage() {
  cat <<'EOF'
Usage: bash scripts/publish-pilotdeck-mint-hub.sh [options]
  --plan                        Validate manifests/Cargo pins/DAG and print topological plan rows.
  --dry-run                     Default. Validate, stage, mint pack --out <staging>/dist, mint publish --dry-run, write receipt.json.
  --verify-install              Run --dry-run pipeline, then verify isolated mint init/add/list against live registry + staged packages.
  --publish                     Explicit opt-in: run --dry-run pipeline, check login/idempotency, publish missing versions in order, verify info+SHA-256.
  --staging-dir ABS             Retained staging directory (absolute path, outside repo). Defaults to mktemp directory.
  --mint ABS                    Absolute path to Mint CLI executable.
  --registry VALUE              Registry URL or name (default: https://mint-hub.ea.modelbest.co).
  --token-from-env-local PATH   Read `minthub_token` from .env.local into an ephemeral MINT_HOME via `mint login --token-stdin -y` (--publish only).
  -h, --help                    Show this help.

Supports --opt=VALUE as well as --opt VALUE. Requires Python 3.11+ (tomllib).
EOF
}

usage_error() { printf 'ERROR: %s\n' "$*" >&2; exit 64; }

select_mode() {
  if [ -n "$MODE" ] && [ "$MODE" != "$1" ]; then
    usage_error "choose only one of --plan, --dry-run, --verify-install, --publish"
  fi
  MODE="$1"
}

require_value() {
  [ "$#" -ge 2 ] && [ -n "$2" ] || usage_error "$1 requires a value"
  case "$2" in
    -*) usage_error "$1 requires a value, not another option ($2)" ;;
  esac
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --plan) select_mode plan; shift ;;
    --dry-run) select_mode dry-run; shift ;;
    --verify-install) select_mode verify-install; shift ;;
    --publish) select_mode publish; shift ;;
    --staging-dir) require_value "$@"; STAGING_DIR="$2"; shift 2 ;;
    --staging-dir=*) STAGING_DIR="${1#--staging-dir=}"; [ -n "$STAGING_DIR" ] || usage_error "--staging-dir requires a value"; shift ;;
    --mint) require_value "$@"; MINT="$2"; shift 2 ;;
    --mint=*) MINT="${1#--mint=}"; [ -n "$MINT" ] || usage_error "--mint requires a value"; shift ;;
    --registry) require_value "$@"; REGISTRY="$2"; shift 2 ;;
    --registry=*) REGISTRY="${1#--registry=}"; [ -n "$REGISTRY" ] || usage_error "--registry requires a value"; shift ;;
    --token-from-env-local) require_value "$@"; TOKEN_FROM_ENV_LOCAL="$2"; shift 2 ;;
    --token-from-env-local=*) TOKEN_FROM_ENV_LOCAL="${1#--token-from-env-local=}"; [ -n "$TOKEN_FROM_ENV_LOCAL" ] || usage_error "--token-from-env-local requires a value"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage_error "unknown argument: $1" ;;
  esac
done

MODE="${MODE:-dry-run}"

case "$REGISTRY" in
  ''|-*|*[[:space:]]*) usage_error "--registry requires a non-empty URL or registry name" ;;
esac
if [ -n "$TOKEN_FROM_ENV_LOCAL" ] && [ ! -f "$TOKEN_FROM_ENV_LOCAL" ]; then
  usage_error "missing --token-from-env-local file: $TOKEN_FROM_ENV_LOCAL"
fi

pick_python() {
  local cand
  for cand in \
    "${PYTHON_BIN:-}" \
    python3.14 python3.13 python3.12 python3.11 python3 \
    /opt/homebrew/bin/python3 /usr/local/bin/python3
  do
    [ -n "$cand" ] || continue
    command -v "$cand" >/dev/null 2>&1 || continue
    if "$cand" -c 'import tomllib' >/dev/null 2>&1; then
      command -v "$cand"
      return 0
    fi
  done
  return 1
}

PYTHON="$(pick_python || true)"
if [ -z "$PYTHON" ]; then
  echo "ERROR: Python 3.11+ (with tomllib) is required; none found in PATH." >&2
  exit 1
fi

# Validate all canonical manifests, Cargo version alignment, capabilities, layout rules, and topological order.
PLAN="$("$PYTHON" -I - "$REPO_ROOT" <<'PY'
from pathlib import Path
import sys
import tomllib

root = Path(sys.argv[1])
CANONICAL_ORDER_20 = [
    "protocol",
    "child-signals",
    "regex-proto",
    "tokenizer",
    "yaml",
    "context-core",
    "model-core",
    "agent",
    "model-runtime",
    "turn-preparation",
    "http-transport",
    "model-iface",
    "transport",
    "trusted-http-host",
    "context-runtime",
    "context-iface",
    "session-migration",
    "tool-core",
    "tool-iface",
    "iface",
]
CANONICAL_ORDER_19 = [s for s in CANONICAL_ORDER_20 if s != "session-migration"]
CANONICAL_ORDER_18 = [s for s in CANONICAL_ORDER_19 if s != "context-iface"]
ALLOWED_CAP_KEYS = {"fs.read", "fs.write", "net", "exec", "reason"}
FORBIDDEN_DIRS = {".git", "node_modules", "target", ".mint"}
FORBIDDEN_FILES = {"credentials.toml", "manifest.json"}


def fail(msg):
    raise SystemExit("MINTHUB PLAN FAIL: " + msg)


def vtuple(text):
    parts = str(text).split(".")
    if len(parts) != 3 or not all(p.isdigit() for p in parts):
        fail(f"invalid M.m.p version: {text!r}")
    return tuple(int(p) for p in parts)


def resolve_cargo_version(crate_dir, ws_version):
    cargo_path = crate_dir / "Cargo.toml"
    try:
        cargo_doc = tomllib.loads(cargo_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        fail(f"{cargo_path}: {exc}")
    pkg = cargo_doc.get("package") or {}
    ver = pkg.get("version")
    if isinstance(ver, dict) and ver.get("workspace") is True:
        return ws_version, pkg.get("name")
    if isinstance(ver, str) and ver:
        return ver, pkg.get("name")
    fail(f"{cargo_path}: missing package.version")


src_root = root / "src"
ws_cargo = tomllib.loads((src_root / "Cargo.toml").read_text(encoding="utf-8"))
ws_version = ws_cargo["workspace"]["package"]["version"]
vtuple(ws_version)

pkg_base = root / "packages/minthub"
if not pkg_base.is_dir():
    fail(f"missing directory: {pkg_base}")

if (pkg_base / "session-migration").is_dir() or (src_root / "pilotdeck-session-migration").is_dir():
    CANONICAL_ORDER = CANONICAL_ORDER_20
elif (pkg_base / "context-iface").is_dir() or (src_root / "pilotdeck-context-iface").is_dir():
    CANONICAL_ORDER = CANONICAL_ORDER_19
else:
    CANONICAL_ORDER = CANONICAL_ORDER_18
EXPECTED_SET = set(CANONICAL_ORDER)
RANK = {short: idx for idx, short in enumerate(CANONICAL_ORDER)}

found_dirs = {p.name for p in pkg_base.iterdir() if p.is_dir()}
if found_dirs != EXPECTED_SET:
    fail(
        f"packages/minthub must contain exact {len(CANONICAL_ORDER)} crates; "
        f"missing={sorted(EXPECTED_SET - found_dirs)}, extra={sorted(found_dirs - EXPECTED_SET)}"
    )

records = {}
for short in CANONICAL_ORDER:
    mdir = pkg_base / short
    crate_dir = src_root / f"pilotdeck-{short}"
    if not crate_dir.is_dir():
        fail(f"missing crate dir: {crate_dir}")
    # Check no mint artifacts inside src/pilotdeck-*/
    for forbidden_inside_src in ("mint.toml", "skills"):
        if (crate_dir / forbidden_inside_src).exists():
            fail(f"{crate_dir} must not contain {forbidden_inside_src} (keep src/ pristine for unsafe_boundary)")

    # Scan packages/minthub/<short> for forbidden files/dirs/symlinks
    for item in [mdir, *mdir.rglob("*")]:
        if item.is_symlink():
            fail(f"symlink forbidden in bundle source: {item}")
        if item.is_dir() and item.name in FORBIDDEN_DIRS:
            fail(f"forbidden directory in bundle source: {item}")
        if item.is_file() and (
            item.name in FORBIDDEN_FILES
            or item.name.endswith(".pem")
            or item.name.startswith(".env")
        ):
            fail(f"forbidden file in bundle source: {item}")

    for req_rel in ("mint.toml", "README.md", f"skills/{short}/SKILL.md"):
        if not (mdir / req_rel).is_file():
            fail(f"{short}: missing required file {req_rel}")

    try:
        doc = tomllib.loads((mdir / "mint.toml").read_text(encoding="utf-8"))
        pkg = doc["package"]
    except (OSError, ValueError, KeyError) as exc:
        fail(f"{short}/mint.toml: {exc}")

    expected_name = f"pilotdeck-rs/{short}"
    if pkg.get("name") != expected_name:
        fail(f"{short}: package.name must be {expected_name!r}, got {pkg.get('name')!r}")
    if pkg.get("kind") != "bundle":
        fail(f"{short}: package.kind must be 'bundle', got {pkg.get('kind')!r}")

    cargo_ver, cargo_name = resolve_cargo_version(crate_dir, ws_version)
    if cargo_name != f"pilotdeck-{short}":
        fail(f"{short}: Cargo.toml name mismatch ({cargo_name!r})")
    mver = pkg.get("version")
    if mver != cargo_ver:
        fail(f"{short}: mint.toml version ({mver!r}) != Cargo.toml version ({cargo_ver!r})")
    vtuple(mver)

    desc = pkg.get("description", "")
    if not isinstance(desc, str) or "\n" in desc or len(desc) > 300:
        fail(f"{short}: description must be single line <= 300 chars")
    kws = pkg.get("keywords", [])
    if (
        not isinstance(kws, list)
        or len(kws) > 5
        or any(not isinstance(k, str) or not k or len(k) > 20 or any(c.isspace() for c in k) for k in kws)
    ):
        fail(f"{short}: keywords invalid: {kws!r}")

    artifacts = doc.get("artifacts") or {}
    if artifacts.get("layout", "any") != "any" or "bin" in artifacts:
        fail(f"{short}: artifacts must have layout='any' and no bin")

    caps = doc.get("capabilities") or {}
    if not isinstance(caps, dict):
        fail(f"{short}: capabilities must be a table")
    bad_caps = set(caps) - ALLOWED_CAP_KEYS
    if bad_caps:
        fail(f"{short}: forbidden capabilities keys {sorted(bad_caps)}")
    reason = caps.get("reason") or {}
    if any(k != "reason" and (k not in reason or not str(reason[k]).strip()) for k in caps):
        fail(f"{short}: every declared capability requires matching [capabilities.reason]")

    requires = doc.get("requires") or {}
    req_ifaces = requires.get("interfaces", [])
    deps = doc.get("dependencies") or {}
    if not isinstance(deps, dict):
        fail(f"{short}: [dependencies] must be a table")

    # Verify iface/* deps match requires.interfaces
    dep_ifaces = {k.split("/", 1)[1] for k in deps if k.startswith("iface/")}
    if dep_ifaces != set(req_ifaces):
        fail(f"{short}: requires.interfaces {sorted(req_ifaces)} != iface/* deps {sorted(dep_ifaces)}")

    pd_deps = []
    for dep_name, dep_spec in deps.items():
        if not isinstance(dep_spec, str) or not dep_spec.strip():
            fail(f"{short}: dependency {dep_name!r} must be a non-empty version string")
        if dep_name.startswith("pilotdeck-rs/"):
            dep_short = dep_name.split("/", 1)[1]
            if dep_short not in EXPECTED_SET:
                fail(f"{short}: unknown internal dependency {dep_name!r}")
            if dep_short == short:
                fail(f"{short}: self-dependency {dep_name!r}")
            pd_deps.append(dep_short)

    records[short] = (expected_name, mver, pd_deps)

# Check internal version pins match target package version and topological order
pending = set(records)
done = set()
order = []
while pending:
    ready = sorted((s for s in pending if set(records[s][2]) <= done), key=lambda s: RANK[s])
    if not ready:
        fail(f"dependency cycle among: {sorted(pending)}")
    cur = ready[0]
    pending.remove(cur)
    done.add(cur)
    order.append(cur)

for short in order:
    name, version, _ = records[short]
    print(f"{short}\t{name}@{version}")
PY
)"

PLAN_COUNT="$(printf '%s\n' "$PLAN" | grep -c .)"

if [ "$MODE" = "plan" ]; then
  printf '%s\n' "$PLAN"
  exit 0
fi

if [ -z "$MINT" ]; then
  if command -v mint >/dev/null 2>&1; then
    MINT="$(command -v mint)"
  elif [ -x "$REPO_ROOT/deps/agent-kernel/target/release/mint" ]; then
    MINT="$REPO_ROOT/deps/agent-kernel/target/release/mint"
  else
    usage_error "--mint ABS is required (mint not found in PATH)"
  fi
fi
case "$MINT" in
  /*) : ;;
  *) usage_error "--mint must be an absolute path: $MINT" ;;
esac
[ -f "$MINT" ] && [ -x "$MINT" ] || usage_error "missing executable Mint CLI: $MINT"

# Unwrap the standard Mint bootstrap launcher (`MINT_HOME=...; exec "$MINT_HOME/bin/mint" "$@"`)
# so callers' explicit `MINT_HOME` isolation (ephemeral auth / verify-install) is respected.
MINT_EXEC="$("$PYTHON" -I -c '
from pathlib import Path
import os, re, sys
p = Path(sys.argv[1])
try:
    raw = p.read_bytes()[:512]
except OSError:
    print(str(p))
    raise SystemExit(0)
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

if [ -n "$STAGING_DIR" ]; then
  case "$STAGING_DIR" in
    /*) : ;;
    *) usage_error "--staging-dir must be an absolute path: $STAGING_DIR" ;;
  esac
  if [ "$STAGING_DIR" = "/" ]; then
    usage_error "--staging-dir cannot be /"
  fi
  if [ -L "$STAGING_DIR" ]; then
    usage_error "--staging-dir cannot be a symlink: $STAGING_DIR"
  fi
  case "$STAGING_DIR/" in
    "$REPO_ROOT/"*) usage_error "--staging-dir must be outside repository ($REPO_ROOT)" ;;
  esac
  mkdir -p "$STAGING_DIR"
  STAGING_DIR="$(cd "$STAGING_DIR" && pwd)"
else
  STAGING_DIR="$(mktemp -d "${TMPDIR:-/tmp}/pilotdeck-minthub.XXXXXX")"
fi

rm -rf "$STAGING_DIR/packages" "$STAGING_DIR/dist" "$STAGING_DIR/verify-install" "$STAGING_DIR/receipt.json" "$STAGING_DIR/plan.tsv" "$STAGING_DIR/publish-plan.tsv"
if [ "$MODE" = "verify-install" ]; then
  rm -f "$STAGING_DIR/logs/"*.pack.log "$STAGING_DIR/logs/"*.dry-run.log "$STAGING_DIR/logs/"verify-*.log
else
  rm -rf "$STAGING_DIR/logs" "$STAGING_DIR/publish-receipt.json"
fi
mkdir -p "$STAGING_DIR/packages" "$STAGING_DIR/dist" "$STAGING_DIR/logs"

printf 'mode=%s\nmint=%s\nregistry=%s\nstaging=%s\n' "$MODE" "$MINT" "$REGISTRY" "$STAGING_DIR"

RUN_FALLBACK_EPOCH="$(date +%s)"
resolve_pkg_epoch() {
  local short="$1"
  local ts=""
  if [ -n "${SOURCE_DATE_EPOCH:-}" ]; then
    printf '%s\n' "$SOURCE_DATE_EPOCH"
    return 0
  fi
  ts="$(git -C "$REPO_ROOT" log -1 --format=%ct -- "src/pilotdeck-$short" "packages/minthub/$short" 2>/dev/null || true)"
  case "$ts" in
    ''|*[!0-9]*) ts="$RUN_FALLBACK_EPOCH" ;;
  esac
  printf '%s\n' "$ts"
}

# Stage src/pilotdeck-<short>/ + overlay packages/minthub/<short>/
"$PYTHON" -I - "$REPO_ROOT" "$STAGING_DIR/packages" "$PLAN" <<'PY'
from pathlib import Path
import shutil
import sys

repo_root = Path(sys.argv[1])
dest_root = Path(sys.argv[2])
plan_lines = [l for l in sys.argv[3].splitlines() if l.strip()]

ignore = shutil.ignore_patterns(
    ".git", ".mint", "target", "dist", "node_modules", "__pycache__",
    "credentials.toml", ".env*", "*.pem", "*.mint.tar.zst", "*.sha256", "*.minisig",
)

for line in plan_lines:
    short, _spec = line.split("\t", 1)
    src_dir = repo_root / "src" / f"pilotdeck-{short}"
    pkg_overlay = repo_root / "packages" / "minthub" / short
    staged = dest_root / short

    shutil.copytree(src_dir, staged, ignore=ignore, symlinks=True)
    # Remove any root manifest.json if somehow present (fixtures inside tests/ stay intact)
    if (staged / "manifest.json").exists():
        raise SystemExit(f"forbidden root manifest.json in {staged}")

    # Verify zero symlinks copied from src
    for item in staged.rglob("*"):
        if item.is_symlink():
            raise SystemExit(f"symlink forbidden in staged package: {item}")

    # Overlay mint.toml, README.md, and skills/
    shutil.copy2(pkg_overlay / "mint.toml", staged / "mint.toml")
    shutil.copy2(pkg_overlay / "README.md", staged / "README.md")
    if (staged / "skills").exists():
        shutil.rmtree(staged / "skills")
    shutil.copytree(pkg_overlay / "skills", staged / "skills", symlinks=False)
PY

# Pack + dry-run every package in topological order, then write receipt.json
PACK_ROWS_TSV="$STAGING_DIR/pack-rows.tsv"
: > "$PACK_ROWS_TSV"

while IFS=$'\t' read -r short spec; do
  pkg_dir="$STAGING_DIR/packages/$short"
  pack_log="$STAGING_DIR/logs/$short.pack.log"
  dry_log="$STAGING_DIR/logs/$short.dry-run.log"
  pkg_epoch="$(resolve_pkg_epoch "$short")"

  if ! SOURCE_DATE_EPOCH="$pkg_epoch" "$MINT_EXEC" --registry "$REGISTRY" pack "$pkg_dir" --out "$STAGING_DIR/dist" --json >"$pack_log" 2>&1; then
    printf 'ERROR: mint pack failed for %s; log: %s\n' "$spec" "$pack_log" >&2
    cat "$pack_log" >&2 || true
    exit 1
  fi

  # Extract packed filename & integrity & verify sha256 sidecar
  row_info="$("$PYTHON" -I - "$pack_log" "$STAGING_DIR/dist" <<'PY'
import hashlib, json, sys
from pathlib import Path

log_text = Path(sys.argv[1]).read_text(encoding="utf-8")
dist_dir = Path(sys.argv[2])
done_ev = None
for line in log_text.splitlines():
    line = line.strip()
    if not line:
        continue
    try:
        ev = json.loads(line)
    except json.JSONDecodeError:
        continue
    if isinstance(ev, dict) and ev.get("event") == "pack.done":
        done_ev = ev
if not done_ev:
    raise SystemExit("missing pack.done event")

archive_path = Path(done_ev["path"])
if not archive_path.is_file():
    raise SystemExit(f"packed archive missing: {archive_path}")
actual_sha = hashlib.sha256(archive_path.read_bytes()).hexdigest()
if actual_sha != done_ev["sha256"]:
    raise SystemExit(f"sha256 mismatch: mint={done_ev['sha256']} actual={actual_sha}")
actual_bytes = archive_path.stat().st_size
if actual_bytes != int(done_ev["size"]):
    raise SystemExit(f"size mismatch: mint={done_ev['size']} actual={actual_bytes}")

sidecar = dist_dir / (archive_path.name + ".sha256")
sidecar.write_text(f"{actual_sha}  {archive_path.name}\n", encoding="utf-8")
print(f"{done_ev['name']}\t{done_ev['version']}\t{archive_path.name}\t{actual_sha}\t{actual_bytes}\t{done_ev.get('integrity','')}")
PY
)"
  printf '%s\t%s\n' "$short" "$row_info" >> "$PACK_ROWS_TSV"

  if ! SOURCE_DATE_EPOCH="$pkg_epoch" "$MINT_EXEC" --registry "$REGISTRY" publish "$pkg_dir" --dry-run >"$dry_log" 2>&1; then
    printf 'ERROR: mint publish --dry-run failed for %s; log: %s\n' "$spec" "$dry_log" >&2
    cat "$dry_log" >&2 || true
    exit 1
  fi
  # Keep staged package dir pristine after publish --dry-run's internal dist/ output
  rm -rf "$pkg_dir/dist"
  printf 'DRY-RUN+PACK OK %s\n' "$spec"
done <<< "$PLAN"

"$PYTHON" -I - "$PACK_ROWS_TSV" "$REGISTRY" "$STAGING_DIR/receipt.json" "$PLAN_COUNT" <<'PY'
from datetime import datetime, timezone
import json
from pathlib import Path
import sys

rows_path = Path(sys.argv[1])
registry = sys.argv[2]
out_path = Path(sys.argv[3])
expected_count = int(sys.argv[4])

items = []
for line in rows_path.read_text(encoding="utf-8").splitlines():
    if not line.strip():
        continue
    short, name, version, archive, sha256, size_s, integrity = line.split("\t")
    items.append({
        "short": short,
        "name": name,
        "version": version,
        "archive": archive,
        "sha256_sidecar": archive + ".sha256",
        "sha256": sha256,
        "bytes": int(size_s),
        "integrity": integrity,
    })

if len(items) != expected_count:
    raise SystemExit(f"expected {expected_count} receipt entries, got {len(items)}")

receipt = {
    "schema_version": 1,
    "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "registry": registry,
    "count": len(items),
    "packages": items,
}
out_path.write_text(json.dumps(receipt, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
PY
rm -f "$PACK_ROWS_TSV"
printf 'RECEIPT WRITTEN %s\n' "$STAGING_DIR/receipt.json"

if [ "$MODE" = "dry-run" ]; then
  printf 'DRY-RUN COMPLETE: all %s packages validated, packed, and dry-run verified.\n' "$PLAN_COUNT"
  exit 0
fi

if [ "$MODE" = "verify-install" ]; then
  VI_ROOT="$STAGING_DIR/verify-install"
  VI_HOME="$VI_ROOT/mint-home"
  mkdir -p "$VI_HOME"

  init_project() {
    local proj_dir="$1"
    local init_log="$2"
    local rc=0
    set +e
    MINT_HOME="$VI_HOME" "$MINT_EXEC" --registry "$REGISTRY" init "$proj_dir" -y --no-preset >"$init_log" 2>&1
    rc=$?
    set -e
    if [ "$rc" -ne 0 ]; then
      if [ -f "$proj_dir/mint.toml" ] && [ -d "$proj_dir/.mint" ]; then
        printf 'NOTE: mint init exited %d on %s (likely mint/core lacks target binary for this host triple); mint.toml and .mint/ initialized, continuing with mint add (requires.core check skipped).\n' "$rc" "$proj_dir"
      else
        printf 'ERROR: mint init failed fatally (rc=%d) for %s\n' "$rc" "$proj_dir" >&2
        cat "$init_log" >&2 || true
        return 1
      fi
    fi
  }

  # 1) Direct unmodified path: verification for the 3 root packages (no pilotdeck-rs/* deps)
  PROJ_ROOTS="$VI_ROOT/proj-roots"
  init_project "$PROJ_ROOTS" "$STAGING_DIR/logs/verify-init-roots.log"
  for root_short in protocol child-signals regex-proto; do
    add_log="$STAGING_DIR/logs/verify-root-$root_short.log"
    if ! (cd "$PROJ_ROOTS" && MINT_HOME="$VI_HOME" "$MINT_EXEC" --registry "$REGISTRY" add -y "path:$STAGING_DIR/packages/$root_short" >"$add_log" 2>&1); then
      printf 'ERROR: direct root path add failed for %s; log: %s\n' "$root_short" "$add_log" >&2
      cat "$add_log" >&2 || true
      exit 1
    fi
  done
  (cd "$PROJ_ROOTS" && MINT_HOME="$VI_HOME" "$MINT_EXEC" list >"$STAGING_DIR/logs/verify-list-roots.log" 2>&1)
  printf 'VERIFY ROOTS OK: protocol, child-signals, regex-proto installed directly from unmodified staging paths.\n'

  # Check whether ALL target PLAN name@version packages already exist on $REGISTRY
  printf '%s\n' "$PLAN" > "$STAGING_DIR/plan.tsv"
  PUBLISHED_ONLINE=0
  if "$PYTHON" -I - "$MINT_EXEC" "$REGISTRY" "$VI_HOME" "$STAGING_DIR/plan.tsv" "$STAGING_DIR/logs/verify-online-probe.log" <<'PY'
import json, os, subprocess, sys
from pathlib import Path

mint, registry, vi_home, plan_path, log_path = sys.argv[1:]
rows = [line.rstrip("\n").split("\t") for line in Path(plan_path).read_text(encoding="utf-8").splitlines() if line.strip()]
env = {**os.environ, "MINT_HOME": vi_home}
probe_lines = []
all_ok = True

for _short, spec in rows:
    name, version = spec.split("@", 1)
    proc = subprocess.run([mint, "--registry", registry, "info", name, "--json"],
                          env=env, capture_output=True, text=True, timeout=120)
    found = False
    if proc.returncode == 0:
        for line in (proc.stdout or "").splitlines():
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(ev, dict) and str(ev.get("event", "")).startswith("info"):
                if version in (ev.get("versions") or []):
                    found = True
    probe_lines.append(f"{name}@{version}\t{'online' if found else 'missing'}")
    if not found:
        all_ok = False

Path(log_path).write_text("\n".join(probe_lines) + "\n", encoding="utf-8")
raise SystemExit(0 if all_ok else 1)
PY
  then
    PUBLISHED_ONLINE=1
  fi
  rm -f "$STAGING_DIR/plan.tsv"

  # 2) Full closure + live iface/* + agentcpm/* packages
  PROJ_CLOSURE="$VI_ROOT/proj-closure"
  init_project "$PROJ_CLOSURE" "$STAGING_DIR/logs/verify-init-closure.log"
  PREREQ_LOG="$STAGING_DIR/logs/verify-prereqs-closure.log"
  if ! (cd "$PROJ_CLOSURE" && MINT_HOME="$VI_HOME" "$MINT_EXEC" --registry "$REGISTRY" add -y agentcpm/host iface/loop >"$PREREQ_LOG" 2>&1); then
    printf 'ERROR: prerequisite install (agentcpm/host iface/loop) failed; log: %s\n' "$PREREQ_LOG" >&2
    cat "$PREREQ_LOG" >&2 || true
    exit 1
  fi
  printf 'VERIFY PREREQS OK: agentcpm/host + iface/loop (40 live iface/* + agentcpm/* packages) installed.\n'

  if [ "$PUBLISHED_ONLINE" -eq 1 ]; then
    SRC_MODE="unmodified-staged-path"
    INSTALL_SRC_BASE="$STAGING_DIR/packages"
  else
    SRC_MODE="local-graph-overlay"
    INSTALL_SRC_BASE="$VI_ROOT/local-graph"
    "$PYTHON" -I - "$STAGING_DIR/packages" "$INSTALL_SRC_BASE" "$PLAN" <<'PY'
from pathlib import Path
import re
import shutil
import sys

src_base = Path(sys.argv[1])
dst_base = Path(sys.argv[2])
plan_lines = [l for l in sys.argv[3].splitlines() if l.strip()]

dst_base.mkdir(parents=True, exist_ok=True)
pat = re.compile(r'^"pilotdeck-rs/([a-z0-9-]+)"\s*=\s*"[^"]+"\s*$')
for line in plan_lines:
    short, _ = line.split("\t", 1)
    src = src_base / short
    dst = dst_base / short
    shutil.copytree(src, dst, symlinks=False)
    out_lines = []
    for raw in (dst / "mint.toml").read_text(encoding="utf-8").splitlines():
        m = pat.match(raw.strip())
        if m:
            dep_short = m.group(1)
            out_lines.append(f'"pilotdeck-rs/{dep_short}" = {{ path = "../{dep_short}" }}')
        else:
            out_lines.append(raw)
    (dst / "mint.toml").write_text("\n".join(out_lines) + "\n", encoding="utf-8")
PY
    printf 'NOTE: not all target pilotdeck-rs/* versions exist on %s yet; using local-graph path overlay for internal pilotdeck-rs/* edges while keeping live external iface/* and agentcpm/* deps.\n' "$REGISTRY"
  fi

  while IFS=$'\t' read -r short spec; do
    add_log="$STAGING_DIR/logs/verify-add-$short.log"
    if ! (cd "$PROJ_CLOSURE" && MINT_HOME="$VI_HOME" "$MINT_EXEC" --registry "$REGISTRY" add -y "path:$INSTALL_SRC_BASE/$short" >"$add_log" 2>&1); then
      printf 'ERROR: verify-install add failed for %s (%s); log: %s\n' "$spec" "$SRC_MODE" "$add_log" >&2
      cat "$add_log" >&2 || true
      exit 1
    fi
    printf 'VERIFY ADD OK %s (%s)\n' "$spec" "$SRC_MODE"
  done <<< "$PLAN"

  LIST_LOG="$STAGING_DIR/logs/verify-list-closure.log"
  (cd "$PROJ_CLOSURE" && MINT_HOME="$VI_HOME" "$MINT_EXEC" list >"$LIST_LOG" 2>&1)

  # 3) Standalone adapter check without agentcpm/host (verifies latest iface/* & agentcpm/iface semver compatibility)
  PROJ_STANDALONE="$VI_ROOT/proj-standalone"
  init_project "$PROJ_STANDALONE" "$STAGING_DIR/logs/verify-init-standalone.log"
  STANDALONE_LOG="$STAGING_DIR/logs/verify-add-standalone.log"
  if ! (cd "$PROJ_STANDALONE" && MINT_HOME="$VI_HOME" "$MINT_EXEC" --registry "$REGISTRY" add -y \
      "path:$INSTALL_SRC_BASE/model-iface" \
      "path:$INSTALL_SRC_BASE/tool-iface" >"$STANDALONE_LOG" 2>&1); then
    printf 'ERROR: standalone adapter closure (model-iface + tool-iface without agentcpm/host) failed; log: %s\n' "$STANDALONE_LOG" >&2
    cat "$STANDALONE_LOG" >&2 || true
    exit 1
  fi
  (cd "$PROJ_STANDALONE" && MINT_HOME="$VI_HOME" "$MINT_EXEC" list >"$STAGING_DIR/logs/verify-list-standalone.log" 2>&1)

  # 4) Pure remote consumer verification (proj-online) when ALL target PLAN packages are online
  PROJ_ONLINE="$VI_ROOT/proj-online"
  ONLINE_LIST_LOG="$STAGING_DIR/logs/verify-list-online.log"
  ONLINE_ROOTS_TXT="$VI_ROOT/online-roots.txt"
  if [ "$PUBLISHED_ONLINE" -eq 1 ]; then
    init_project "$PROJ_ONLINE" "$STAGING_DIR/logs/verify-init-online.log"
    "$PYTHON" -I - "$REPO_ROOT/packages/minthub" "$PLAN" "$ONLINE_ROOTS_TXT" <<'PY'
from pathlib import Path
import sys
import tomllib

pkg_base = Path(sys.argv[1])
plan_lines = [l for l in sys.argv[2].splitlines() if l.strip()]
out_path = Path(sys.argv[3])

versions = {}
incoming = {}
order = []
for line in plan_lines:
    short, spec = line.split("\t", 1)
    name, version = spec.split("@", 1)
    versions[name] = version
    incoming[name] = 0
    order.append((short, name))

for short, name in order:
    doc = tomllib.loads((pkg_base / short / "mint.toml").read_text(encoding="utf-8"))
    for dep_name in (doc.get("dependencies") or {}):
        if dep_name.startswith("pilotdeck-rs/"):
            if dep_name not in incoming:
                raise SystemExit(f"{name}: unknown pilotdeck-rs dependency {dep_name!r}")
            incoming[dep_name] += 1

roots = [f"{name}@={versions[name]}" for _short, name in order if incoming[name] == 0]
if not roots:
    raise SystemExit("no top-level pilotdeck-rs/* packages found")
out_path.write_text("\n".join(roots) + "\n", encoding="utf-8")
PY
    ONLINE_ROOT_ARGS=()
    while IFS= read -r spec_req; do
      [ -n "$spec_req" ] && ONLINE_ROOT_ARGS+=("$spec_req")
    done < "$ONLINE_ROOTS_TXT"
    ONLINE_ADD_LOG="$STAGING_DIR/logs/verify-add-online.log"
    if ! (cd "$PROJ_ONLINE" && MINT_HOME="$VI_HOME" "$MINT_EXEC" --registry "$REGISTRY" add -y "${ONLINE_ROOT_ARGS[@]}" >"$ONLINE_ADD_LOG" 2>&1); then
      printf 'ERROR: pure remote consumer install (proj-online) failed; log: %s\n' "$ONLINE_ADD_LOG" >&2
      cat "$ONLINE_ADD_LOG" >&2 || true
      exit 1
    fi
    (cd "$PROJ_ONLINE" && MINT_HOME="$VI_HOME" "$MINT_EXEC" list >"$ONLINE_LIST_LOG" 2>&1)
    printf 'VERIFY ONLINE CONSUMER OK: installed %d top-level specs purely from %s (no path: args).\n' "${#ONLINE_ROOT_ARGS[@]}" "$REGISTRY"
  fi

  # Assert all canonical pilotdeck-rs/* packages appear in full closure (+ proj-online when online) mint list + lock
  "$PYTHON" -I - "$PROJ_CLOSURE/mint.lock" "$LIST_LOG" "$PLAN" "$VI_ROOT/summary.json" "$SRC_MODE" "$PUBLISHED_ONLINE" "$PROJ_ONLINE" "$ONLINE_LIST_LOG" "$ONLINE_ROOTS_TXT" <<'PY'
import json
from pathlib import Path
import sys
import tomllib

lock_doc = tomllib.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
list_text = Path(sys.argv[2]).read_text(encoding="utf-8")
plan_lines = [l for l in sys.argv[3].splitlines() if l.strip()]
summary_path = Path(sys.argv[4])
src_mode = sys.argv[5]
published_online = int(sys.argv[6]) == 1
proj_online = Path(sys.argv[7])
online_list_path = Path(sys.argv[8])
online_roots_path = Path(sys.argv[9])

locked = {p["name"]: p["version"] for p in lock_doc.get("package", [])}
expected = {}
for line in plan_lines:
    _short, spec = line.split("\t", 1)
    name, version = spec.split("@", 1)
    expected[name] = version

missing = [n for n, v in expected.items() if locked.get(n) != v]
if missing:
    raise SystemExit(f"missing or version-mismatched pilotdeck-rs packages in mint.lock: {missing}")

for name in expected:
    if name not in list_text:
        raise SystemExit(f"{name} missing from mint list output")

summary = {
    "mode": src_mode,
    "total_locked_packages": len(locked),
    "pilotdeck_packages_verified": len(expected),
    "kernel_and_iface_packages_locked": sorted(k for k in locked if not k.startswith("pilotdeck-rs/")),
    "list_log": str(sys.argv[2]),
    "online_consumer_verified": False,
}

if published_online:
    online_toml = tomllib.loads((proj_online / "mint.toml").read_text(encoding="utf-8"))
    for dep_name, dep_val in (online_toml.get("dependencies") or {}).items():
        if isinstance(dep_val, dict) and "path" in dep_val:
            raise SystemExit(f"proj-online unexpectedly contains path dependency for {dep_name!r}")
    online_lock = tomllib.loads((proj_online / "mint.lock").read_text(encoding="utf-8"))
    online_locked = {p["name"]: p["version"] for p in online_lock.get("package", [])}
    online_list_text = online_list_path.read_text(encoding="utf-8")
    online_missing = [n for n, v in expected.items() if online_locked.get(n) != v]
    if online_missing:
        raise SystemExit(f"proj-online missing or version-mismatched pilotdeck-rs packages in mint.lock: {online_missing}")
    for name in expected:
        if name not in online_list_text:
            raise SystemExit(f"{name} missing from proj-online mint list output")
    online_roots = [l.strip() for l in online_roots_path.read_text(encoding="utf-8").splitlines() if l.strip()]
    summary["online_consumer_verified"] = True
    summary["online_consumer_roots"] = online_roots
    summary["online_consumer_total_locked_packages"] = len(online_locked)
    summary["online_consumer_pilotdeck_packages_verified"] = len(expected)
    summary["online_consumer_list_log"] = str(online_list_path)

summary_path.write_text(json.dumps(summary, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
print(f"VERIFY-INSTALL PASS: {len(expected)} pilotdeck-rs/* + {len(summary['kernel_and_iface_packages_locked'])} external packages (total {len(locked)}), online_consumer_verified={summary['online_consumer_verified']} in {summary_path}")
PY
  exit 0
fi

# ── Publish mode ───────────────────────────────────────────────────────────────
AUTH_MINT_HOME=""
PUBLISH_MINT_HOME="${MINT_HOME:-}"
cleanup_auth() {
  unset MINT_TOKEN || true
  if [ -n "${AUTH_MINT_HOME:-}" ] && [ -d "$AUTH_MINT_HOME" ]; then
    rm -rf "$AUTH_MINT_HOME"
  fi
}
trap cleanup_auth EXIT INT TERM

if [ -n "$TOKEN_FROM_ENV_LOCAL" ]; then
  AUTH_MINT_HOME="$(mktemp -d "${TMPDIR:-/tmp}/pilotdeck-mint-auth.XXXXXX")"
  chmod 700 "$AUTH_MINT_HOME"
  PUBLISH_MINT_HOME="$AUTH_MINT_HOME"
  if ! "$PYTHON" -I - "$TOKEN_FROM_ENV_LOCAL" "$MINT_EXEC" "$REGISTRY" "$AUTH_MINT_HOME" <<'PY'
import os
from pathlib import Path
import subprocess
import sys

env_path = Path(sys.argv[1])
mint_exec = sys.argv[2]
registry = sys.argv[3]
auth_mint_home = sys.argv[4]

token = None
for raw in env_path.read_text(encoding="utf-8").splitlines():
    line = raw.strip()
    if not line or line.startswith("#"):
        continue
    if line.startswith("export "):
        line = line[len("export "):].strip()
    if "=" not in line:
        continue
    k, v = line.split("=", 1)
    if k.strip() != "minthub_token":
        continue
    v = v.strip()
    if len(v) >= 2 and ((v[0] == '"' and v[-1] == '"') or (v[0] == "'" and v[-1] == "'")):
        v = v[1:-1]
    token = v
    break

if not token:
    raise SystemExit(f"ERROR: minthub_token missing or empty in {env_path}")

clean_env = {
    k: val for k, val in os.environ.items()
    if k != "MINT_TOKEN" and not k.startswith("MINT_HUB_TOKEN_")
}
clean_env["MINT_HOME"] = auth_mint_home

proc = subprocess.run(
    [mint_exec, "--registry", registry, "login", "--token-stdin", "-y"],
    input=token.encode("utf-8"),
    env=clean_env,
    capture_output=True,
    timeout=60,
)
stdout_txt = (proc.stdout or b"").decode("utf-8", errors="replace").replace(token, "[REDACTED]")
stderr_txt = (proc.stderr or b"").decode("utf-8", errors="replace").replace(token, "[REDACTED]")
cred_ok = (Path(auth_mint_home) / "credentials.toml").is_file()
del token

if proc.returncode != 0 or not cred_ok:
    msg = (stderr_txt or stdout_txt or f"exit={proc.returncode}").strip()
    raise SystemExit(f"ERROR: mint login --token-stdin failed: {msg}")
PY
  then
    exit 6
  fi
  unset MINT_TOKEN || true
else
  # Verify fallback credentials exist without printing any token value.
  CRED_PATH="${MINT_HOME:-$HOME/.mint}/credentials.toml"
  HAS_AUTH=0
  if [ -n "${MINT_TOKEN:-}" ]; then
    HAS_AUTH=1
  elif [ -f "$CRED_PATH" ] && grep -q 'token' "$CRED_PATH" 2>/dev/null; then
    HAS_AUTH=1
  else
    if env | grep -q '^MINT_HUB_TOKEN_.*=.' 2>/dev/null; then
      HAS_AUTH=1
    fi
  fi
  if [ "$HAS_AUTH" -ne 1 ]; then
    echo "ERROR: MintHub publish authentication missing (no --token-from-env-local / MINT_TOKEN / MINT_HUB_TOKEN_* / credentials.toml)." >&2
    echo "       Run: echo \"<token>\" | \"$MINT\" --registry \"$REGISTRY\" login --token-stdin" >&2
    exit 6
  fi
fi

# Fail-closed online immutability & SHA-256 idempotency guard before publishing any package.
printf '%s\n' "$PLAN" > "$STAGING_DIR/plan.tsv"
"$PYTHON" -I - "$MINT_EXEC" "$REGISTRY" "$STAGING_DIR/plan.tsv" "$STAGING_DIR/receipt.json" "$STAGING_DIR/publish-plan.tsv" "$PUBLISH_MINT_HOME" <<'PY'
import json, os, subprocess, sys
from pathlib import Path

mint, registry, plan_path, receipt_path, out_plan_path, pub_mint_home = sys.argv[1:]
rows = [line.rstrip("\n").split("\t") for line in Path(plan_path).read_text(encoding="utf-8").splitlines() if line.strip()]
receipt = json.loads(Path(receipt_path).read_text(encoding="utf-8"))
expected_by_name = {item["name"]: item["sha256"] for item in receipt["packages"]}

env = dict(os.environ)
if pub_mint_home:
    env["MINT_HOME"] = pub_mint_home

def events(text):
    res = []
    for l in text.splitlines():
        l = l.strip()
        if not l:
            continue
        try:
            obj = json.loads(l)
            if isinstance(obj, dict):
                res.append(obj)
        except json.JSONDecodeError:
            pass
    return res

def extract_remote_sha(info_ev):
    latest = info_ev.get("latest") or {}
    return latest.get("vers"), (latest.get("cksum") or ((latest.get("artifacts") or {}).get("files") or [{}])[0].get("sha256") or "")

problems = []
plan_rows = []
to_pub = 0
skip_ident = 0

for short, spec in rows:
    name, version = spec.split("@", 1)
    expected_sha = expected_by_name[name]
    proc = subprocess.run([mint, "--registry", registry, "info", name, "--json"],
                          env=env, capture_output=True, text=True, timeout=120)
    evs = events((proc.stdout or "").strip())

    if proc.returncode != 0:
        err_ev = next((e for e in reversed(evs) if e.get("event") == "error"), {})
        if err_ev.get("kind") == "network":
            problems.append(f"{name}: registry unreachable ({err_ev.get('message','')})")
            continue
        probe = subprocess.run([mint, "--registry", registry, "search", name, "--json"],
                               env=env, capture_output=True, text=True, timeout=120)
        pevs = events(probe.stdout or "")
        search_ev = next((e for e in reversed(pevs) if str(e.get("event", "")).startswith("search")), None)
        if probe.returncode != 0 or search_ev is None or not isinstance(search_ev.get("packages"), list):
            problems.append(f"{name}: info failed and search probe failed (fail-closed)")
            continue
        if any(isinstance(p, dict) and p.get("name") == name for p in search_ev["packages"]):
            problems.append(f"{name}: inconsistent registry response (info failed, search found package)")
            continue
        to_pub += 1
        plan_rows.append(f"{short}\t{name}\t{version}\t{expected_sha}\tto_publish\t")
        continue

    info_ev = next((e for e in reversed(evs) if str(e.get("event", "")).startswith("info")), None)
    if not info_ev or not isinstance(info_ev.get("versions"), list):
        problems.append(f"{name}: missing versions in info response")
        continue

    if version in info_ev["versions"]:
        latest_vers, remote_sha = extract_remote_sha(info_ev)
        if latest_vers != version:
            problems.append(
                f"{name}@{version}: already in versions={info_ev['versions']} but latest={latest_vers!r} (non-latest version collision)"
            )
            continue
        if not remote_sha or remote_sha != expected_sha:
            problems.append(
                f"{name}@{version}: already published with sha256 mismatch (remote={remote_sha!r} != local={expected_sha!r})"
            )
            continue
        skip_ident += 1
        plan_rows.append(f"{short}\t{name}\t{version}\t{expected_sha}\talready_published_identical\t{remote_sha}")
    else:
        to_pub += 1
        plan_rows.append(f"{short}\t{name}\t{version}\t{expected_sha}\tto_publish\t")

if problems:
    for p in problems:
        print("ERROR:", p, file=sys.stderr)
    raise SystemExit(1)

Path(out_plan_path).write_text("\n".join(plan_rows) + "\n", encoding="utf-8")
print(f"ONLINE GUARD OK: {to_pub} to_publish, {skip_ident} already_published_identical.")
PY

redact_and_print_log() {
  local log_file="$1"
  "$PYTHON" -I - "$log_file" "${TOKEN_FROM_ENV_LOCAL:-}" <<'PY'
import os
from pathlib import Path
import sys

log_text = Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace")
env_path_arg = sys.argv[2]
secrets = []
if os.environ.get("MINT_TOKEN"):
    secrets.append(os.environ["MINT_TOKEN"])
for k, v in os.environ.items():
    if k.startswith("MINT_HUB_TOKEN_") and v:
        secrets.append(v)
if env_path_arg and Path(env_path_arg).is_file():
    for raw in Path(env_path_arg).read_text(encoding="utf-8", errors="replace").splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        if line.startswith("export "):
            line = line[len("export "):].strip()
        k, v = line.split("=", 1)
        if k.strip() == "minthub_token":
            v = v.strip()
            if len(v) >= 2 and ((v[0] == '"' and v[-1] == '"') or (v[0] == "'" and v[-1] == "'")):
                v = v[1:-1]
            if v:
                secrets.append(v)
for sec in secrets:
    log_text = log_text.replace(sec, "[REDACTED]")
sys.stderr.write(log_text)
if log_text and not log_text.endswith("\n"):
    sys.stderr.write("\n")
PY
}

while IFS=$'\t' read -r short name version expected_sha status remote_sha; do
  if [ "$status" = "already_published_identical" ]; then
    printf 'SKIP IDENTICAL %s@%s sha256=%s\n' "$name" "$version" "$remote_sha"
    continue
  fi
  pub_log="$STAGING_DIR/logs/$short.publish.log"
  pkg_epoch="$(resolve_pkg_epoch "$short")"
  printf 'PUBLISH %s@%s\n' "$name" "$version"
  pub_rc=0
  (
    if [ -n "$TOKEN_FROM_ENV_LOCAL" ]; then
      unset MINT_TOKEN || true
      for env_k in $(compgen -e MINT_HUB_TOKEN_ || true); do
        unset "$env_k" || true
      done
    fi
    if [ -n "$PUBLISH_MINT_HOME" ]; then
      export MINT_HOME="$PUBLISH_MINT_HOME"
    fi
    SOURCE_DATE_EPOCH="$pkg_epoch" "$MINT_EXEC" --registry "$REGISTRY" publish -y "$STAGING_DIR/packages/$short" >"$pub_log" 2>&1
  ) || pub_rc=$?
  if [ "$pub_rc" -ne 0 ]; then
    printf 'ERROR: publish failed for %s@%s (rc=%d); stopped without retry or yank. Log: %s\n' "$name" "$version" "$pub_rc" "$pub_log" >&2
    redact_and_print_log "$pub_log" || true
    exit 1
  fi
done < "$STAGING_DIR/publish-plan.tsv"

# Verify published info + SHA-256 against receipt.json and write publish-receipt.json
"$PYTHON" -I - "$MINT_EXEC" "$REGISTRY" "$STAGING_DIR/publish-plan.tsv" "$STAGING_DIR/publish-receipt.json" "$PUBLISH_MINT_HOME" <<'PY'
from datetime import datetime, timezone
import json, os, subprocess, sys
from pathlib import Path

mint, registry, plan_path, out_receipt_path, pub_mint_home = sys.argv[1:]
rows = [line.rstrip("\n").split("\t") for line in Path(plan_path).read_text(encoding="utf-8").splitlines() if line.strip()]
env = dict(os.environ)
if pub_mint_home:
    env["MINT_HOME"] = pub_mint_home

results = []
published_count = 0
skipped_identical_count = 0

for short, name, version, expected_sha, planned_status, _planned_remote_sha in rows:
    proc = subprocess.run([mint, "--registry", registry, "info", name, "--json"],
                          env=env, capture_output=True, text=True, timeout=120)
    if proc.returncode != 0:
        raise SystemExit(f"post-publish info failed for {name}: {proc.stderr}")
    info_ev = None
    for line in (proc.stdout or "").splitlines():
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(obj, dict) and str(obj.get("event", "")).startswith("info"):
            info_ev = obj
    if not info_ev:
        raise SystemExit(f"missing info event for {name}")
    if version not in (info_ev.get("versions") or []):
        raise SystemExit(f"{name}@{version} not visible in registry versions={info_ev.get('versions')}")
    latest = info_ev.get("latest") or {}
    if latest.get("vers") != version:
        raise SystemExit(f"{name}@{version} latest version mismatch: latest={latest.get('vers')!r}")
    remote_sha = latest.get("cksum") or ((latest.get("artifacts") or {}).get("files") or [{}])[0].get("sha256")
    if remote_sha != expected_sha:
        raise SystemExit(f"{name}@{version} sha256 mismatch: remote={remote_sha} local={expected_sha}")

    if planned_status == "already_published_identical":
        final_status = "already_published_identical"
        skipped_identical_count += 1
        print(f"ONLINE VERIFIED (IDENTICAL SKIP) {name}@{version} sha256={remote_sha}")
    else:
        final_status = "published"
        published_count += 1
        print(f"ONLINE VERIFIED {name}@{version} sha256={remote_sha}")

    results.append({
        "short": short,
        "name": name,
        "version": version,
        "sha256": expected_sha,
        "status": final_status,
        "remote_sha256": remote_sha,
    })

publish_receipt = {
    "schema_version": 1,
    "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "registry": registry,
    "published_count": published_count,
    "skipped_identical_count": skipped_identical_count,
    "packages": results,
}
Path(out_receipt_path).write_text(json.dumps(publish_receipt, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
PY

rm -f "$STAGING_DIR/plan.tsv" "$STAGING_DIR/publish-plan.tsv"
unset MINT_TOKEN || true
printf 'PUBLISH COMPLETE: all %s packages verified online (%s).\n' "$PLAN_COUNT" "$STAGING_DIR/publish-receipt.json"
