#!/usr/bin/env python3
"""Copy one explicitly selected skill to a new destination without replacing files."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import sys


def migrate(source, destination):
    source, destination = Path(source).resolve(), Path(destination).absolute()
    if not (source / 'SKILL.md').is_file():
        raise ValueError('source requires SKILL.md')
    if destination.exists() or destination.is_symlink():
        raise ValueError('destination already exists')
    if destination.is_relative_to(source):
        raise ValueError('destination must be outside source')
    entries = list(source.rglob('*'))
    if any(path.is_symlink() or not (path.is_file() or path.is_dir()) for path in entries):
        raise ValueError('links and special files require manual review')
    inventory = {str(p.relative_to(source)): hashlib.sha256(p.read_bytes()).hexdigest() for p in entries if p.is_file()}
    # mkdir is exclusive; a concurrent destination creator is never merged.
    destination.mkdir(parents=True, exist_ok=False)
    try:
        for path in entries:
            target = destination / path.relative_to(source)
            if path.is_dir():
                target.mkdir(exist_ok=True)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                with path.open('rb') as src, target.open('xb') as dst:
                    shutil.copyfileobj(src, dst)
                shutil.copymode(path, target)
        actual = {str(p.relative_to(destination)): hashlib.sha256(p.read_bytes()).hexdigest() for p in destination.rglob('*') if p.is_file()}
        if actual != inventory:
            raise ValueError('copied inventory changed during migration')
    except BaseException:
        # Remove only the exclusively created destination from this invocation.
        shutil.rmtree(destination)
        raise
    return {'destination': str(destination), 'files': inventory}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path)
    parser.add_argument('destination', type=Path)
    args = parser.parse_args()
    try:
        print(json.dumps(migrate(args.source, args.destination), indent=2))
    except (OSError, ValueError) as error:
        print(f'migration failed: {error}', file=sys.stderr)
        raise SystemExit(1)
