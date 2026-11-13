#!/usr/bin/env python3
"""Reject missing, changed, extra or escaped files in an extracted release."""
import argparse
import json
from common import Path, digest, inventory
from ripgrep_component import validate_ripgrep


def verify(root):
    manifest = json.loads((root / "manifest.json").read_text())
    if manifest.get("schema") != 1:
        raise ValueError("unsupported package manifest")
    if manifest.get('variant') in {'core','full'}:
        if manifest.get('requiredComponents') != ['ripgrep']:
            raise ValueError('core/full requires source-built ripgrep component')
        checked = validate_ripgrep(root / 'components/ripgrep', manifest.get('target'))
        components = [entry for entry in manifest.get('components',[]) if entry.get('name')=='ripgrep']
        if components != [checked]:
            raise ValueError('manifest ripgrep identity differs from actual component')
    expected = manifest["files"]
    observed = [item for item in inventory(root) if item["path"] not in {"manifest.json", "SHA256SUMS"}]
    if observed != expected:
        raise ValueError("package inventory differs (missing, changed, extra file or mode)")
    sums = []
    for item in inventory(root):
        if item["type"] == "file" and item["path"] != "SHA256SUMS":
            sums.append(f'{item["sha256"]}  {item["path"]}\n')
    if (root / "SHA256SUMS").read_text() != "".join(sums):
        raise ValueError("checksum list is missing, stale or altered")
    return {"verified": True, "manifestSha256": digest(root / "manifest.json"), "files": len(expected)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package", type=Path)
    args = parser.parse_args()
    print(json.dumps(verify(args.package.resolve(strict=True))))
