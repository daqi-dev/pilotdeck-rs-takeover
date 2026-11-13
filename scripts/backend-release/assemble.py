#!/usr/bin/env python3
"""Assemble a relocatable core/full backend from already built, identified inputs."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import tempfile
import tomllib
from common import Path, archive, binary_target, digest, inventory, run, source_identity, write_json
from linkage import inspect
from resource_layout import resource_contract
from ripgrep_component import copy_ripgrep, validate_ripgrep


def copy_tree(source, destination):
    inventory(source)  # reject escaped/broken symlinks before copying
    shutil.copytree(source, destination, symlinks=True)


def supplement(package, root):
    directory = Path(__file__).resolve().parent / "license-supplements"
    index = directory / "index.json"
    if not index.exists():
        return None
    for source in json.loads(index.read_text())["sources"]:
        for expected in source["packages"]:
            if (package["name"], package["version"]) != (expected["name"], expected["version"]):
                continue
            vcs = root / ".cargo_vcs_info.json"
            if not vcs.exists() or json.loads(vcs.read_text())["git"]["sha1"] != expected["sourceCommit"]:
                raise ValueError(f"license supplement source identity mismatch: {package['name']}")
            path = directory / source["file"]
            if digest(path) != source["sha256"]:
                raise ValueError(f"license supplement changed: {path.name}")
            return path, {k: v for k, v in source.items() if k != "packages"}
    return None


def source_notices(source, output, directory=None):
    """Collect identified notices for adapted source outside Cargo packages."""
    directory = directory or Path(__file__).resolve().parent / "license-supplements"
    index = json.loads((directory / "index.json").read_text())
    collected = []
    for notice in index.get("sourceNotices", []):
        identifier = notice["id"]
        if Path(identifier).name != identifier or identifier in {"", ".", ".."}:
            raise ValueError("invalid source notice identifier")
        for relative in notice["applicablePaths"]:
            path = (source / relative).resolve(strict=True)
            if not path.is_relative_to(source.resolve()):
                raise ValueError("source notice applicability escapes source")
        files = []
        for entry in notice["files"]:
            if Path(entry["file"]).name != entry["file"] or entry["file"] in {"", ".", ".."}:
                raise ValueError("source notice escapes supplement directory")
            path = (directory / entry["file"]).resolve(strict=True)
            if not path.is_relative_to(directory.resolve()):
                raise ValueError("source notice escapes supplement directory")
            if digest(path) != entry["sha256"]:
                raise ValueError(f"source notice changed: {entry['file']}")
            relative = Path("licenses/source-notices") / identifier / entry["file"]
            target = output / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, target)
            files.append({**entry, "path": relative.as_posix()})
        collected.append({**notice, "files": files})
    write_json(output / "source-notices.json", {"schema": 1, "notices": collected})


def dependencies(metadata_path, source, output):
    metadata = json.loads(metadata_path.read_text())
    lock = tomllib.loads((source / "src/Cargo.lock").read_text())
    locked = {(p["name"], p["version"], p.get("source")): p for p in lock["package"]}
    packages = []
    for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"], p.get("source") or "")):
        identity = (package["name"], package["version"], package.get("source"))
        if identity not in locked:
            raise ValueError(f"metadata package absent from Cargo.lock: {identity}")
        root = Path(package["manifest_path"]).parent
        key = f'{package["name"]}-{package["version"]}'
        if package.get("source"):
            key += "-" + hashlib.sha256(package["source"].encode()).hexdigest()[:8]
        candidates = set()
        if package.get("license_file"):
            candidates.add((root / package["license_file"]).resolve(strict=True))
        # Native sys crates can embed separately licensed codec/C sources.
        # Preserve those notices too instead of reporting only the Rust wrapper.
        for current, directories, files in os.walk(root):
            directories[:] = sorted(d for d in directories if d not in {".git", "target", "node_modules"})
            for name in files:
                path = Path(current) / name
                if path.is_file() and name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT")):
                    candidates.add(path)
                elif name == "AUTHORS" and path.is_file():
                    content = path.read_text(errors="replace")
                    if "Permission is hereby granted" in content and "COPYRIGHT:" in content:
                        candidates.add(path)  # r-efi keeps its license text here.
        if not package.get("source") and not candidates and package["name"].startswith("pilotdeck-"):
            candidates.add(source / "LICENSE")
        supplemental = supplement(package, root)
        if supplemental:
            candidates.add(supplemental[0])
        if not candidates:
            raise ValueError(f"dependency has no collected license/notice file: {key}")
        notices = []
        for index, path in enumerate(sorted(candidates)):
            relative = path.relative_to(root) if path.is_relative_to(root) else Path(f"external-{index}-{path.name}")
            target = output / "licenses/dependencies" / key / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, target)
            notices.append(target.relative_to(output).as_posix())
        packages.append({"name": package["name"], "version": package["version"], "source": package.get("source"),
                         "checksum": locked[identity].get("checksum"), "licenseExpression": package.get("license"),
                         "noticeFiles": notices, **({"supplement": supplemental[1]} if supplemental else {})})
    # A --no-deps document cannot be substituted for the complete inventory.
    external_locked = {key for key in locked if key[2]}
    observed = {(p["name"], p["version"], p["source"]) for p in packages}
    if external_locked - observed:
        raise ValueError("dependency metadata is incomplete for the locked workspace")
    write_json(output / "dependencies.json", {"schema": 1, "cargoLockSha256": digest(source / "src/Cargo.lock"), "packages": packages})


def component(prefix, name, output, target):
    executable = "pilotdeck-pdf" if name == "mupdf" else "pilotdeck-image"
    binary = prefix / "bin" / executable
    if binary_target(binary) != target:
        raise ValueError(f"{name} architecture differs from release target")
    destination = output / "components" / name
    (destination / "bin").mkdir(parents=True)
    shutil.copy2(binary, destination / "bin" / executable)
    for section in ["source", "licenses"]:
        directory = prefix / "share" / section
        if not directory.is_dir() or not any(directory.iterdir()):
            raise ValueError(f"{name} is missing its {section} distribution")
        copy_tree(directory, destination / "share" / section)
    if name == "image":
        library = destination / "lib"
        library.mkdir()
        candidates = [p for p in (prefix / "lib").iterdir() if ".so" in p.name or p.name.endswith(".dylib")]
        if not candidates:
            raise ValueError("image component has no runtime shared libraries")
        for path in candidates:
            if path.is_symlink():
                path.resolve(strict=True).relative_to((prefix / "lib").resolve())
            shutil.copy2(path, library / path.name, follow_symlinks=False)
    return {"name": name, "binary": f"components/{name}/bin/{executable}", "sha256": digest(binary),
            "sourceIncluded": True, "licensesIncluded": True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--build-record", type=Path, required=True)
    parser.add_argument("--cargo-metadata", type=Path, required=True)
    parser.add_argument("--variant", choices=["core", "full"], required=True)
    parser.add_argument("--ripgrep", type=Path, required=True)
    parser.add_argument("--mupdf", type=Path)
    parser.add_argument("--image", type=Path)
    parser.add_argument("--skills", type=Path)
    parser.add_argument("--plugins", type=Path)
    parser.add_argument("--development", action="store_true")
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    source = args.source.resolve(strict=True)
    record = json.loads(args.build_record.read_text())
    if record.get("schema") != 1 or record["source"] != source_identity(source):
        parser.error("build record does not match the current product source snapshot")
    if not args.development and (record["profile"] != "release" or record["source"]["dirty"]):
        parser.error("production assembly requires a clean source snapshot and release profile")
    if not args.development and not record["evidence"]:
        parser.error("production assembly requires retained build evidence")
    if not {"pilotdeck", "pilotdeck-agent-loop-sidecar"} <= record["binaries"].keys():
        parser.error("CLI and independent sidecar are both required")
    if args.variant == "full" and (not args.mupdf or not args.image):
        parser.error("full package requires both source-built components")
    if args.variant == "core" and (args.mupdf or args.image):
        parser.error("core package must not silently include optional components")
    validate_ripgrep(args.ripgrep, record["target"])
    contract = resource_contract(args.skills, args.plugins)
    if args.out.exists():
        parser.error("output directory must be new; preserve failed attempts")
    args.out.mkdir(parents=True)
    target = record["target"]
    epoch = int(run("git", "show", "-s", "--format=%ct", "HEAD", cwd=source))
    package = args.out / f"pilotdeck-{args.variant}-{target}"
    package.mkdir()
    (package / "bin").mkdir()
    provenance = {**record, "binaries": {}, "evidence": []}
    for name, binary in sorted(record["binaries"].items()):
        path = Path(binary["path"])
        if digest(path) != binary["sha256"] or binary_target(path) != target:
            raise ValueError(f"binary changed since build recording: {name}")
        shutil.copy2(path, package / "bin" / name)
        provenance["binaries"][name] = {"sha256": binary["sha256"]}
    for index, evidence in enumerate(record["evidence"]):
        path = Path(evidence["path"])
        if digest(path) != evidence["sha256"]:
            raise ValueError("build evidence changed")
        name = f"build-evidence/{index}-{path.name}"
        (package / name).parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, package / name)
        provenance["evidence"].append({"path": name, "sha256": evidence["sha256"]})
    write_json(package / "build.json", provenance)
    shutil.copyfile(source / "LICENSE", package / "LICENSE")
    dependencies(args.cargo_metadata, source, package)
    source_notices(source, package)
    components = [copy_ripgrep(args.ripgrep.resolve(), package, target)]
    if args.variant == "full":
        components.extend([component(args.mupdf.resolve(), "mupdf", package, target), component(args.image.resolve(), "image", package, target)])
    write_json(package / "native-dependencies.json", inspect(package, target))
    resources = {}
    for directory, key in [(args.skills, "skills"), (args.plugins, "builtin-plugins")]:
        resources[key] = bool(directory)
        if directory:
            directory = directory.resolve(strict=True)
            if directory.is_relative_to((source / "resources").resolve()):
                raise ValueError("reference resources cannot become release input")
            copy_tree(directory, package / key)
    # Explicit allowlist excludes reference repositories, private state and tests' outputs.
    with tempfile.TemporaryDirectory(prefix="pdrs-release-source-") as temporary:
        selected = Path(temporary)
        files = run("git", "ls-files", "--", "src", "components", "scripts/backend-release", "scripts/backend-validation", "LICENSE", ".config/nextest.toml", cwd=source).splitlines()
        for name in files:
            original = source / name
            if original.is_file() and not original.is_symlink():
                destination = selected / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(original, destination)
        if args.development:
            for item in record["source"]["untracked"]:
                destination = selected / item["path"]
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source / item["path"], destination)
        archive(selected, package / "source.tar.gz", epoch)
    write_json(package / "manifest.json", {
        "schema": 1, "variant": args.variant, "target": target, "sourceDateEpoch": epoch,
        "classification": "development" if args.development else "release-candidate",
        "defaultEntry": "bin/pilotdeck", "defaultRuntime": "native", "components": components,
        "requiredComponents": ["ripgrep"],
        "resources": resources, "resourceContract": contract,
        "variantMeaning": "native core" if args.variant == "core" else "native core plus source-built MuPDF and image components",
        "scopeExclusions": ["StaffDeck", "Web frontend", "ui/server", "desktop", "TUI"],
        "pendingDelivery": ["actual resource workflows and moved-package behavior require separate verification", "Node MCP plugins and their runtimes/models are explicit separate addons"],
        "files": inventory(package),
    })
    rows = inventory(package)
    (package / "SHA256SUMS").write_text("".join(f'{item["sha256"]}  {item["path"]}\n' for item in rows if item["type"] == "file"))
    archive(package, args.out / f"{package.name}.tar.gz", epoch, package.name)
    write_json(args.out / "archive.json", {"path": f"{package.name}.tar.gz", "sha256": digest(args.out / f"{package.name}.tar.gz")})
    print(args.out / "archive.json")


if __name__ == "__main__":
    main()
