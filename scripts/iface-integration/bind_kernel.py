#!/usr/bin/env python3
"""Bind an explicitly pinned Kernel source checkout without copying its code."""
import argparse
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[2]


def git(path, *arguments):
    return subprocess.check_output(["git", "-C", str(path), *arguments], text=True).strip()


def validate_source(kernel_root, expected_commit):
    """Validate the explicitly selected checkout; shared by binding and controls."""
    source = Path(kernel_root).resolve(strict=True)
    if not re.fullmatch(r"[0-9a-f]{40}", expected_commit):
        raise ValueError("--expected-commit requires a complete Git commit")
    if git(source, "rev-parse", "HEAD") != expected_commit:
        raise ValueError("Kernel HEAD differs from the explicitly selected source version")
    if git(source, "status", "--porcelain", "--untracked-files=normal"):
        raise ValueError("Kernel checkout is dirty; freeze a candidate before binding it")
    for package in ("iface", "scheduler-protocol", "host"):
        if not (source / "kernel" / package / "Cargo.toml").is_file():
            raise ValueError("missing Kernel source package: " + package)
    return source


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-root", required=True, type=Path)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--check", action="store_true", help="validate without creating the binding")
    args = parser.parse_args()
    try:
        source = validate_source(args.kernel_root, args.expected_commit)
    except ValueError as error:
        parser.error(str(error))
    target = ROOT / "deps" / "agent-kernel"
    if target.exists() or target.is_symlink():
        if not target.is_symlink() or target.resolve() != source:
            parser.error("existing binding differs; inspect it before replacing it")
    elif not args.check:
        target.parent.mkdir(parents=True, exist_ok=True)
        target.symlink_to(source, target_is_directory=True)
    print(json.dumps({"kernelCommit": args.expected_commit, "kernelRoot": str(source),
                      "binding": str(target), "checkOnly": args.check,
                      "productValidationPerformed": False}, ensure_ascii=False))


if __name__ == "__main__":
    main()
