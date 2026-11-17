#!/usr/bin/env python3
"""Scaffold MintHub-downloaded `.mint/packages/` bundles into isolated stitched workspaces.

Strictly sources ALL 20 `pilotdeck-rs/*` crate directories from `<project>/.mint/packages/`
(verifying every package file digest against its Mint `manifest.json` and asserting ZERO
files are copied from `<repo>/src/pilotdeck-*`), generates `<out>/components/install.json`
(`agentcpm.kernel/install@1`) + `install.excluded.json`, and materializes:
  1. `<out>/pure-build/`   — Stage 2A: 14 pure `pilotdeck-rs/*` crates, zero `deps/agent-kernel`
  2. `<out>/hub016-probe/` — Stage 2B: `pilotdeck-model-iface` vs MintHub `agentcpm/iface@0.1.6`
  3. `<out>/full-build/`   — Stage 3/4: all 20 MintHub crates + `minthub-stitched-e2e` + pinned `deps/agent-kernel`

Every generated workspace Cargo.toml forwards `[profile.dev.package.*]` overrides from
`<repo>/src/Cargo.toml` so dev/test builds keep the optimized hashing/serde/tokenizer
dependency graph (see `extract_profile_dev_package_sections`).
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import sys
import tomllib
from pathlib import Path

PURE_14 = [
    "child-signals",
    "regex-proto",
    "tokenizer",
    "yaml",
    "protocol",
    "context-core",
    "http-transport",
    "model-core",
    "tool-core",
    "transport",
    "agent",
    "context-runtime",
    "model-runtime",
    "turn-preparation",
]

ADAPTER_6 = [
    "model-iface",
    "tool-iface",
    "trusted-http-host",
    "context-iface",
    "session-migration",
    "iface",
]

ALL_20 = PURE_14 + ADAPTER_6

# Minimal workspace foundations & cross-crate read-only test oracle/anchor fixtures.
# None of these 6 files are executable library code from the 20 published bundles.
FOUNDATION_FILES = [
    ("rust-toolchain.toml", "rust-toolchain.toml"),
    ("LICENSE", "LICENSE"),
    ("src/Cargo.lock", "src/Cargo.lock"),
    (
        "src/pilotdeck-gateway/tests/fixtures/output-schema/regress-oracle.json",
        "src/pilotdeck-gateway/tests/fixtures/output-schema/regress-oracle.json",
    ),
    (
        "src/pilotdeck-gateway/tests/fixtures/sidecar-payload/oracle.json",
        "src/pilotdeck-gateway/tests/fixtures/sidecar-payload/oracle.json",
    ),
    (
        "src/pilotdeck-gateway/tests/fixtures/compaction-orchestrator/oracle.json",
        "src/pilotdeck-gateway/tests/fixtures/compaction-orchestrator/oracle.json",
    ),
    (
        "src/pilotdeck-gateway/tests/fixtures/edgeclaw-capture-turn/oracle.json",
        "src/pilotdeck-gateway/tests/fixtures/edgeclaw-capture-turn/oracle.json",
    ),
    (
        "src/pilotdeck-gateway/tests/fixtures/compaction-engine/oracle.json",
        "src/pilotdeck-gateway/tests/fixtures/compaction-engine/oracle.json",
    ),
    (
        "src/pilotdeck-cli/src/main.rs",
        "src/pilotdeck-cli/src/main.rs",
    ),
]

FOUNDATION_DIRS = [
    ("src/vendor/tiktoken-rs", "src/vendor/tiktoken-rs"),
    ("src/vendor/icu_collator", "src/vendor/icu_collator"),
    ("src/data/icu_collator_data", "src/data/icu_collator_data"),
]

RESERVED_INTERFACES = {"policy", "audit", "egress", "lease", "review", "assembly"}
COMPILED_IN_TOOLS = {"text.sha256", "web.fetch"}
ALLOWED_KINDS = {
    "core",
    "host",
    "plugin",
    "overlay",
    "bundle",
    "skill",
    "mcp",
    "preset",
    "template",
    "app",
    "iface",
    "agent",
}


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        while True:
            chunk = f.read(65536)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def verify_mint_manifest_files(pkg_dir: Path, name: str) -> dict:
    manifest_path = pkg_dir / "manifest.json"
    if not manifest_path.is_file():
        raise SystemExit(f"{name}: missing manifest.json under {pkg_dir}")
    doc = json.loads(manifest_path.read_text(encoding="utf-8"))
    files_field = doc.get("files")
    checked = 0
    if isinstance(files_field, dict):
        for rel, expected in files_field.items():
            fp = pkg_dir / rel
            if not fp.is_file():
                raise SystemExit(f"{name}: manifest file missing on disk: {fp}")
            actual = sha256_file(fp)
            exp_hex = expected.get("sha256") if isinstance(expected, dict) else str(expected)
            if exp_hex.startswith("sha256:"):
                exp_hex = exp_hex.split(":", 1)[1]
            if actual != exp_hex:
                raise SystemExit(f"{name}: sha256 mismatch for {rel}: {actual} != {exp_hex}")
            checked += 1
    elif isinstance(files_field, list):
        for entry in files_field:
            if isinstance(entry, dict) and "path" in entry and "sha256" in entry:
                fp = pkg_dir / entry["path"]
                if not fp.is_file():
                    raise SystemExit(f"{name}: manifest file missing on disk: {fp}")
                actual = sha256_file(fp)
                exp_hex = str(entry["sha256"])
                if exp_hex.startswith("sha256:"):
                    exp_hex = exp_hex.split(":", 1)[1]
                if actual != exp_hex:
                    raise SystemExit(f"{name}: sha256 mismatch for {entry['path']}: {actual} != {exp_hex}")
                checked += 1
    return {"manifest_checked_files": checked}


def discover_installed(project: Path) -> tuple[dict[str, Path], dict[str, dict], dict]:
    lock_path = project / "mint.lock"
    if not lock_path.is_file():
        raise SystemExit(f"missing mint.lock under {project}")
    lock_doc = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    locked = {p["name"]: p for p in lock_doc.get("package", [])}

    pkgs_root = project / ".mint" / "packages"
    if not pkgs_root.is_dir():
        raise SystemExit(f"missing .mint/packages under {project}")

    discovered: dict[str, dict] = {}
    for scope_dir in sorted(pkgs_root.iterdir()):
        if scope_dir.name.startswith(".") or not scope_dir.is_dir():
            continue
        for ver_dir in sorted(scope_dir.iterdir()):
            if ver_dir.name.startswith(".") or not ver_dir.is_dir():
                continue
            mint_path = ver_dir / "mint.toml"
            if not mint_path.is_file():
                raise SystemExit(f"missing mint.toml in installed package {ver_dir}")
            doc = tomllib.loads(mint_path.read_text(encoding="utf-8"))
            pkg = doc["package"]
            name = pkg["name"]
            version = pkg["version"]
            if name not in locked or locked[name]["version"] != version:
                raise SystemExit(
                    f"lock mismatch for {name}: disk={version}, lock={locked.get(name, {}).get('version')}"
                )
            info = verify_mint_manifest_files(ver_dir, name)
            discovered[name] = {
                "dir": ver_dir,
                "name": name,
                "version": version,
                "scope": scope_dir.name,
                "toml": doc,
                "mint_sha256": sha256_file(mint_path),
                "verified_files": info["manifest_checked_files"],
            }

    pd_dirs: dict[str, Path] = {}
    for short in ALL_20:
        mint_name = f"pilotdeck-rs/{short}"
        if mint_name not in discovered:
            raise SystemExit(f"missing expected MintHub package {mint_name} in {pkgs_root}")
        pd_dirs[short] = discovered[mint_name]["dir"]
    return pd_dirs, discovered, locked


def copy_pkg_crate_only(src_pkg_dir: Path, dst_crate_dir: Path) -> list[str]:
    """Copy a downloaded MintHub bundle without reusing any monorepo product sources."""
    if dst_crate_dir.exists():
        shutil.rmtree(dst_crate_dir)
    ignore = shutil.ignore_patterns(".git", ".mint", "target", "dist", "__pycache__")
    shutil.copytree(src_pkg_dir, dst_crate_dir, ignore=ignore, symlinks=False)
    copied = []
    for p in sorted(dst_crate_dir.rglob("*")):
        if p.is_file():
            copied.append(str(p.relative_to(dst_crate_dir)))
    return copied


PROFILE_DEV_PACKAGE_HEADER = re.compile(r"^\[profile\.dev\.package\.[A-Za-z0-9_.-]+\]$")


def extract_profile_dev_package_sections(text: str) -> list[str]:
    """Forward every `[profile.dev.package.<crate>]` block (header + body, in file order).

    The stitched workspaces build in dev/test profile via `nextest`; without these
    forwarded overrides the unoptimized `sha2`/`serde`/tokenizer dependency graphs make
    FullLate Native child SHA-256 verification and journal CAS stall debug execution.
    Other profile tables (`[profile.dev]`, `[profile.release]`, ...) stay unforwarded.
    """
    lines: list[str] = []
    inside = False
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("["):
            inside = PROFILE_DEV_PACKAGE_HEADER.match(s) is not None
            if inside:
                lines.append(line.rstrip())
            continue
        if inside and not s.startswith("#") and s:
            lines.append(line.rstrip())
    return lines


def parse_root_cargo_sections(repo_root: Path) -> tuple[list[str], dict[str, str], list[str], list[str]]:
    text = (repo_root / "src" / "Cargo.toml").read_text(encoding="utf-8")

    def extract_section(header: str) -> list[str]:
        out, inside = [], False
        for line in text.splitlines():
            s = line.strip()
            if s.startswith("["):
                if inside:
                    break
                if s == header:
                    inside = True
                continue
            if inside and not s.startswith("#") and s:
                out.append(line.rstrip())
        return out

    pkg_lines = extract_section("[workspace.package]")
    dep_lines = extract_section("[workspace.dependencies]")
    lint_rust = extract_section("[workspace.lints.rust]")
    lint_clippy = extract_section("[workspace.lints.clippy]")
    patch_lines = extract_section("[patch.crates-io]")

    dep_map: dict[str, str] = {}
    for line in dep_lines:
        m = re.match(r'^([A-Za-z0-9_.-]+)\s*=', line.strip())
        if m:
            dep_map[m.group(1)] = line
    return pkg_lines, dep_map, lint_rust, lint_clippy + [""] + patch_lines


def render_workspace_cargo(
    repo_root: Path,
    members: list[str],
    exclude: list[str],
    include_kernel_deps: bool,
    iface_override_path: str | None = None,
) -> str:
    pkg_lines, dep_map, lint_rust, tail_lines = parse_root_cargo_sections(repo_root)
    lines = [
        "# Generated by pilotdeck_minthub_scaffold.py for MintHub stitched verification.",
        "[workspace]",
        'resolver = "3"',
    ]
    if exclude:
        lines.append("exclude = [")
        for item in exclude:
            lines.append(f'  "{item}",')
        lines.append("]")
    lines.append("members = [")
    for member in members:
        lines.append(f'  "{member}",')
    lines.append("]")
    lines += ["", "[workspace.package]"] + pkg_lines
    lines += ["", "[workspace.dependencies]"]
    for key, line in dep_map.items():
        if key in ("agentcpm-iface", "agentcpm-scheduler-protocol"):
            if iface_override_path and key == "agentcpm-iface":
                lines.append(f'agentcpm-iface = {{ path = "{iface_override_path}" }}')
            elif include_kernel_deps:
                lines.append(line)
            continue
        lines.append(line)
    lines += ["", "[workspace.lints.rust]"] + lint_rust
    # Extract lints.clippy and patch.crates-io cleanly
    src_text = (repo_root / "src" / "Cargo.toml").read_text(encoding="utf-8")
    clippy_lines, patch_lines = [], []
    mode = None
    for raw in src_text.splitlines():
        s = raw.strip()
        if s.startswith("["):
            mode = s
            continue
        if s and not s.startswith("#"):
            if mode == "[workspace.lints.clippy]":
                clippy_lines.append(raw.rstrip())
            elif mode == "[patch.crates-io]":
                patch_lines.append(raw.rstrip())
    lines += ["", "[workspace.lints.clippy]"] + clippy_lines
    lines += ["", "[patch.crates-io]"] + patch_lines + [""]
    profile_lines = extract_profile_dev_package_sections(
        (repo_root / "src" / "Cargo.toml").read_text(encoding="utf-8")
    )
    if profile_lines:
        lines += ["", *profile_lines, ""]
    return "\n".join(lines)


def copy_foundation(repo_root: Path, ws_root: Path) -> list[dict]:
    records = []
    for src_rel, dst_rel in FOUNDATION_FILES:
        src = repo_root / src_rel
        if not src.is_file():
            raise SystemExit(f"missing foundation file: {src}")
        dst = ws_root / dst_rel
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(src, dst)
        records.append({"path": dst_rel, "source": src_rel, "sha256": sha256_file(dst)})
    for src_rel, dst_rel in FOUNDATION_DIRS:
        src = repo_root / src_rel
        dst = ws_root / dst_rel
        if dst.exists():
            shutil.rmtree(dst)
        shutil.copytree(src, dst, symlinks=False)
        records.append({"path": dst_rel, "source": src_rel, "type": "vendor_or_data_dir"})
    return records


def build_install_ledger(discovered: dict[str, dict], out_dir: Path) -> tuple[Path, list[dict], list[dict]]:
    comp_dir = out_dir / "components"
    if comp_dir.exists():
        shutil.rmtree(comp_dir)
    comp_dir.mkdir(parents=True, exist_ok=True)
    os.chmod(comp_dir, 0o755)

    declared: list[dict] = []
    excluded: list[dict] = []

    for name in sorted(discovered):
        info = discovered[name]
        doc = info["toml"]
        pkg = doc["package"]
        version = info["version"]
        kind = pkg.get("kind", "")
        subkind = pkg.get("subkind")
        scope = info["scope"]
        bare = name.split("/", 1)[1] if "/" in name else name
        safe_dir = f"{scope}__{bare}@{version}"

        provides = doc.get("provides") or {}
        prov_ifaces = list(provides.get("interfaces") or [])
        prov_loops = list(provides.get("loops") or [])
        prov_tools = list(provides.get("tools") or [])
        artifacts_raw = doc.get("artifacts") or {}
        artifacts = {k: v for k, v in artifacts_raw.items() if k in ("wasm", "bin") and isinstance(v, str)}
        # Align with kernel-scaffold-layout.py: table [capabilities] is not flat list; preserve list form only
        caps_raw = doc.get("capabilities")
        caps = [c for c in caps_raw if isinstance(c, str)] if isinstance(caps_raw, list) else []

        # Apply kernel component exclusion rules cleanly
        excl_reason = None
        if scope == "iface" or kind == "iface":
            excl_reason = ("SCAFFOLD_CONTRACT_IDENTITY", "iface contract identity package")
        elif kind not in ALLOWED_KINDS:
            excl_reason = ("COMPONENT_KIND_UNKNOWN", f"unknown kind {kind!r}")
        elif kind in ("core", "host", "app"):
            excl_reason = ("COMPONENT_KIND_KERNEL_BODY", f"kernel body kind={kind!r}")
        elif set(prov_ifaces) & RESERVED_INTERFACES:
            hits = sorted(set(prov_ifaces) & RESERVED_INTERFACES)
            excl_reason = ("COMPONENT_CLAIMS_RESERVED_INTERFACE", f"reserved interfaces {hits}")
        elif kind == "plugin" and not artifacts:
            excl_reason = ("COMPONENT_PLUGIN_NO_ARTIFACTS", f"plugin/{subkind} without artifacts")
        elif not artifacts and prov_loops:
            excl_reason = ("COMPONENT_LOCAL_TOOL_NOT_LOADABLE", "declares loops without artifact")
        elif not artifacts and any(t not in COMPILED_IN_TOOLS for t in prov_tools):
            excl_reason = ("COMPONENT_LOCAL_TOOL_NOT_LOADABLE", f"unserved source-level tools {prov_tools}")
        elif artifacts:
            missing = [rel for rel in artifacts.values() if not (info["dir"] / rel).is_file()]
            if missing:
                excl_reason = ("MISSING_DECLARED_ARTIFACT", f"declared artifact not present in source bundle: {missing}")

        if excl_reason:
            excluded.append({
                "name": name,
                "version": version,
                "kind": kind,
                "dir": safe_dir,
                "code": excl_reason[0],
                "reason": excl_reason[1],
            })
            continue

        target_pkg_dir = comp_dir / safe_dir
        target_pkg_dir.mkdir(parents=True, exist_ok=True)
        os.chmod(target_pkg_dir, 0o755)
        shutil.copyfile(info["dir"] / "mint.toml", target_pkg_dir / "mint.toml")
        os.chmod(target_pkg_dir / "mint.toml", 0o644)
        pins = {"mint.toml": sha256_file(target_pkg_dir / "mint.toml")}
        for slot, rel in sorted(artifacts.items()):
            src_art = info["dir"] / rel
            dst_art = target_pkg_dir / rel
            dst_art.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(src_art, dst_art)
            os.chmod(dst_art, (src_art.stat().st_mode & 0o777) & ~0o022)
            pins[rel] = sha256_file(dst_art)

        rec: dict = {
            "name": name,
            "version": version,
            "kind": kind,
            "dir": safe_dir,
            "provides": {
                "interfaces": prov_ifaces,
                "loops": prov_loops,
                "tools": prov_tools,
            },
            "sha256": dict(sorted(pins.items())),
        }
        if subkind:
            rec["subkind"] = subkind
        if artifacts:
            rec["artifacts"] = dict(sorted(artifacts.items()))
        if caps:
            rec["capabilities"] = caps
        if pkg.get("description"):
            rec["description"] = pkg["description"]
        declared.append(rec)

    install_doc = {"schema": "agentcpm.kernel/install@1", "packages": declared}
    install_path = comp_dir / "install.json"
    install_path.write_text(json.dumps(install_doc, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    os.chmod(install_path, 0o644)

    excl_doc = {
        "schema": "agentcpm.kernel/install-excluded@1",
        "excluded": excluded,
    }
    excl_path = out_dir / "install.excluded.json"
    excl_path.write_text(json.dumps(excl_doc, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    os.chmod(excl_path, 0o644)
    return install_path, declared, excluded


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--project", required=True, help="Mint consumer project dir with .mint/packages/")
    ap.add_argument("--repo-root", required=True, help="PilotDeck repo root (for vendor/data/deps)")
    ap.add_argument("--out", required=True, help="Output directory outside repo for stitched workspaces")
    ap.add_argument("--summary", help="Optional JSON summary output path")
    args = ap.parse_args()

    project = Path(args.project).resolve()
    repo_root = Path(args.repo_root).resolve()
    out_dir = Path(args.out).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    pd_dirs, discovered, locked = discover_installed(project)
    install_path, declared, excluded = build_install_ledger(discovered, out_dir)

    # 1) Stage 2A: Pure 14 workspace (ZERO deps/agent-kernel)
    pure_root = out_dir / "pure-build"
    if pure_root.exists():
        shutil.rmtree(pure_root)
    (pure_root / "src").mkdir(parents=True)
    pure_foundation = copy_foundation(repo_root, pure_root)
    for short in PURE_14:
        copy_pkg_crate_only(pd_dirs[short], pure_root / "src" / f"pilotdeck-{short}")
    pure_cargo = render_workspace_cargo(
        repo_root,
        [f"pilotdeck-{s}" for s in PURE_14],
        ["vendor/tiktoken-rs", "vendor/icu_collator", "data/icu_collator_data"],
        include_kernel_deps=False,
    )
    (pure_root / "src" / "Cargo.toml").write_text(pure_cargo, encoding="utf-8")

    # 2) Stage 2B: Compatibility probe for MintHub agentcpm/iface@0.1.6 vs pilotdeck-model-iface
    probe_root = out_dir / "hub016-probe"
    if probe_root.exists():
        shutil.rmtree(probe_root)
    (probe_root / "src").mkdir(parents=True)
    copy_foundation(repo_root, probe_root)
    probe_pd = PURE_14 + ["model-iface"]
    for short in probe_pd:
        copy_pkg_crate_only(pd_dirs[short], probe_root / "src" / f"pilotdeck-{short}")
    if "agentcpm/iface" not in discovered:
        raise SystemExit("agentcpm/iface missing from downloaded .mint/packages")
    copy_pkg_crate_only(discovered["agentcpm/iface"]["dir"], probe_root / "src" / "hub-agentcpm-iface")
    probe_cargo = render_workspace_cargo(
        repo_root,
        [f"pilotdeck-{s}" for s in probe_pd],
        ["vendor/tiktoken-rs", "vendor/icu_collator", "data/icu_collator_data", "hub-agentcpm-iface"],
        include_kernel_deps=False,
        iface_override_path="hub-agentcpm-iface",
    )
    (probe_root / "src" / "Cargo.toml").write_text(probe_cargo, encoding="utf-8")

    # 3) Stage 3/4: Full 20 MintHub pilotdeck-rs/* crates + minthub-stitched-e2e + pinned deps/agent-kernel
    full_root = out_dir / "full-build"
    if full_root.exists():
        shutil.rmtree(full_root)
    (full_root / "src").mkdir(parents=True)
    (full_root / "deps").mkdir(parents=True)
    full_foundation = copy_foundation(repo_root, full_root)
    for short in ALL_20:
        copy_pkg_crate_only(pd_dirs[short], full_root / "src" / f"pilotdeck-{short}")
    e2e_src = repo_root / "scripts" / "minthub-stitched-e2e"
    if not e2e_src.is_dir():
        raise SystemExit(f"missing {e2e_src}")
    copy_pkg_crate_only(e2e_src, full_root / "src" / "minthub-stitched-e2e")
    kernel_src = (repo_root / "deps" / "agent-kernel").resolve()
    os.symlink(kernel_src, full_root / "deps" / "agent-kernel")

    # Sanity check: ZERO pilotdeck-* sources in full_root overlap monorepo targets unintentionally
    for short in ALL_20:
        assert not (full_root / "src" / f"pilotdeck-{short}" / "target").exists()

    full_members = [f"pilotdeck-{s}" for s in ALL_20] + ["minthub-stitched-e2e"]
    full_cargo = render_workspace_cargo(
        repo_root,
        full_members,
        ["vendor/tiktoken-rs", "vendor/icu_collator", "data/icu_collator_data"],
        include_kernel_deps=True,
    )
    (full_root / "src" / "Cargo.toml").write_text(full_cargo, encoding="utf-8")

    plan = {
        "schema": "pilotdeck.minthub.stitched-scaffold/v1",
        "project": str(project),
        "out": str(out_dir),
        "locked_total": len(locked),
        "discovered_total": len(discovered),
        "pilotdeck_packages": {
            f"pilotdeck-rs/{short}": {
                "version": discovered[f"pilotdeck-rs/{short}"]["version"],
                "mint_toml_sha256": discovered[f"pilotdeck-rs/{short}"]["mint_sha256"],
                "manifest_verified_files": discovered[f"pilotdeck-rs/{short}"]["verified_files"],
                "source_dir": str(pd_dirs[short]),
            }
            for short in ALL_20
        },
        "install_json_path": str(install_path),
        "install_json_sha256": sha256_file(install_path),
        "install_declared_count": len(declared),
        "install_excluded_count": len(excluded),
        "pure_14_workspace": str(pure_root / "src" / "Cargo.toml"),
        "hub016_probe_workspace": str(probe_root / "src" / "Cargo.toml"),
        "hub016_version": discovered["agentcpm/iface"]["version"],
        "full_20_workspace": str(full_root / "src" / "Cargo.toml"),
        "foundation_files": pure_foundation,
        "full_foundation_files": full_foundation,
        "all_pilotdeck_sources_origin": ".mint/packages/pilotdeck-rs",
    }
    plan_path = out_dir / "scaffold-plan.json"
    plan_path.write_text(json.dumps(plan, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    if args.summary:
        Path(args.summary).write_text(
            json.dumps(plan, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
    print(
        f"SCAFFOLD OK: 20 pilotdeck-rs/* + {len(discovered)-20} ext packages, "
        f"install.json declared={len(declared)} excluded={len(excluded)}"
    )


if __name__ == "__main__":
    main()
