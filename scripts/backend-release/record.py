#!/usr/bin/env python3
"""Record an existing build; does not compile or assert an unobserved build passed."""
import argparse
import json
from common import Path, binary_target, digest, source_identity, write_json


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--profile", choices=["release", "debug"], required=True)
    parser.add_argument("--binary", action="append", default=[], metavar="NAME=PATH")
    parser.add_argument("--evidence", type=Path, action="append", default=[])
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if args.out.exists():
        parser.error("output must be new")
    binaries = {}
    for item in args.binary:
        name, path = item.split("=", 1)
        path = Path(path).resolve(strict=True)
        if name not in {"pilotdeck", "pilotdeck-agent-loop-sidecar", "pilotdeck-gateway"} or name in binaries:
            parser.error(f"unknown or duplicate binary: {name}")
        if binary_target(path) != args.target:
            parser.error(f"binary architecture does not match target: {name}")
        binaries[name] = {"path": str(path), "sha256": digest(path)}
    value = {"schema": 1, "target": args.target, "profile": args.profile,
             "source": source_identity(args.source.resolve()), "binaries": binaries,
             "evidence": [{"path": str(p.resolve(strict=True)), "sha256": digest(p)} for p in args.evidence]}
    write_json(args.out, value)
    print(json.dumps({"record": str(args.out), "sha256": digest(args.out)}))


if __name__ == "__main__":
    main()
