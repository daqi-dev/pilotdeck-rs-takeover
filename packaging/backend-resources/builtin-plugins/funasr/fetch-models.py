#!/usr/bin/env python3
"""Fetch only the two locked model data files; never executable release assets."""
import hashlib
import json
from pathlib import Path
import sys
import urllib.request


def main():
    if len(sys.argv) != 2:
        raise SystemExit('usage: fetch-models.py /absolute/isolated/funasr-root')
    destination = Path(sys.argv[1])
    if not destination.is_absolute():
        raise SystemExit('runtime root must be absolute')
    lock = json.loads(Path(__file__).with_name('source-lock.json').read_text())
    models = destination / 'models'
    models.mkdir(parents=True, exist_ok=True)
    receipt = []
    for model in lock['models']:
        target = models / model['file']
        part = target.with_suffix(target.suffix + '.part')
        if not target.exists():
            digest = hashlib.sha256()
            size = 0
            with urllib.request.urlopen(model['url'], timeout=60) as source, part.open('wb') as output:
                while chunk := source.read(1024 * 1024):
                    output.write(chunk)
                    digest.update(chunk)
                    size += len(chunk)
            if size != model['bytes'] or digest.hexdigest() != model['sha256']:
                raise RuntimeError(f'model identity mismatch: {part}')
            part.replace(target)
        digest = hashlib.sha256()
        with target.open('rb') as source:
            while chunk := source.read(1024 * 1024):
                digest.update(chunk)
        if target.stat().st_size != model['bytes'] or digest.hexdigest() != model['sha256']:
            raise RuntimeError(f'existing model identity mismatch: {target}')
        receipt.append({**model, 'verifiedPath': str(target)})
    (destination / 'model-receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')


if __name__ == '__main__':
    main()
