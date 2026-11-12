"""Deterministic, source-only release assembly helpers (Python 3.11+)."""
from __future__ import annotations

import gzip
import hashlib
import json
import os
import pathlib
import subprocess
import tarfile

Path = pathlib.Path


def digest(path):
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n")


def run(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


def source_identity(root):
    scope = ["src", "components"]
    status = run("git", "status", "--porcelain=v1", "--untracked-files=all", "--", *scope, cwd=root)
    diff = subprocess.check_output(["git", "diff", "--binary", "HEAD", "--", *scope], cwd=root)
    untracked = run("git", "ls-files", "--others", "--exclude-standard", "--", *scope, cwd=root)
    return {
        "commit": run("git", "rev-parse", "HEAD", cwd=root),
        "dirty": bool(status),
        "status": status.splitlines(),
        "trackedDiffSha256": hashlib.sha256(diff).hexdigest(),
        "untracked": [{"path": p, "sha256": digest(root / p)} for p in untracked.splitlines()],
    }


def safe_name(name):
    p = pathlib.PurePosixPath(name)
    if p.is_absolute() or ".." in p.parts or "\n" in name or "\r" in name or "\\" in name:
        raise ValueError(f"unsafe package path: {name!r}")
    return p


def inventory(root):
    result = []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        safe_name(relative)
        if path.is_symlink():
            target = os.readlink(path)
            if os.path.isabs(target) or not path.resolve().is_relative_to(root.resolve()):
                raise ValueError(f"non-relocatable symlink: {relative} -> {target}")
            if not path.exists():
                raise ValueError(f"broken symlink: {relative}")
            result.append({"path": relative, "type": "symlink", "target": target})
        elif path.is_file():
            result.append({"path": relative, "type": "file", "size": path.stat().st_size,
                           "mode": "0755" if path.stat().st_mode & 0o111 else "0644", "sha256": digest(path)})
    return result


def archive(root, destination, epoch, prefix=""):
    """Stable gzip header, order, modes, owner and timestamps; no host metadata."""
    with destination.open("xb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch, compresslevel=6) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as tar:
                for path in sorted(root.rglob("*")):
                    relative = path.relative_to(root).as_posix()
                    safe_name(relative)
                    entry = tar.gettarinfo(str(path), arcname=f"{prefix}/{relative}" if prefix else relative)
                    entry.uid = entry.gid = 0
                    entry.uname = entry.gname = ""
                    entry.mtime = epoch
                    entry.pax_headers = {}
                    entry.mode = 0o755 if entry.isdir() or entry.issym() or path.stat().st_mode & 0o111 else 0o644
                    if entry.isfile():
                        with path.open("rb") as stream:
                            tar.addfile(entry, stream)
                    elif entry.isdir() or entry.issym():
                        tar.addfile(entry)
                    else:
                        raise ValueError(f"unsupported package entry: {relative}")


def binary_target(path):
    with path.open("rb") as stream:
        data = stream.read(64)
    if data[:4] == b"\x7fELF":
        machine = int.from_bytes(data[18:20], "little" if data[5] == 1 else "big")
        return {183: "aarch64-unknown-linux-gnu", 62: "x86_64-unknown-linux-gnu"}.get(machine)
    if data[:4] == b"\xcf\xfa\xed\xfe":
        cpu = int.from_bytes(data[4:8], "little")
        return {0x100000C: "aarch64-apple-darwin", 0x1000007: "x86_64-apple-darwin"}.get(cpu)
    return None
