#!/usr/bin/env python3
"""Lossless librsvg vendor layout conversion and a standalone offline restoration recipe."""
import argparse
import hashlib
import json
import os
import re
import shutil
import tarfile
import tomllib
from pathlib import Path, PurePosixPath

INDEX = 'rsvg-vendor-archive.json'
ARCHIVE = 'rsvg-vendor.tar.gz'
LOCK_SHA = '5ff0046d02d58e34e4f160ee4f255f2e32a7005484874774ea1deff1e044cda1'
SOURCE_SHA = '1bebdb27d6eede1bef529642cf3eb1de119f17b530dcf5d38981f5373fc9d671'


def digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def write_json(path, value):
    Path(path).write_text(json.dumps(value, sort_keys=True, indent=2) + '\n')


def safe_relative(value):
    if (not isinstance(value, str) or not value or value.startswith('/')
            or any(p in ('', '.', '..') for p in value.split('/'))
            or any(c in value for c in '\\\r\n\0') or PurePosixPath(value).is_absolute()):
        raise ValueError('unsafe vendor path')
    return value


def vendor_inventory(root):
    rows = []
    if root.is_symlink() or not root.is_dir():
        raise ValueError('vendor tree must be a regular directory')
    for path in sorted(root.rglob('*')):
        relative = safe_relative(path.relative_to(root).as_posix())
        if path.is_symlink():
            raise ValueError('vendor links are forbidden')
        if path.is_dir():
            continue
        if not path.is_file():
            raise ValueError('vendor special files are forbidden')
        rows.append({'path': relative, 'size': path.stat().st_size,
                     'mode': '0755' if path.stat().st_mode & 0o111 else '0644', 'sha256': digest(path)})
    if not rows:
        raise ValueError('vendor tree is empty')
    return rows


def archive_proof(archive, expected, expected_sha):
    if Path(archive).is_symlink() or digest(archive) != expected_sha:
        raise ValueError('vendor archive SHA differs')
    pending = {}
    for row in expected:
        name = safe_relative(row['path'])
        if (name in pending or type(row['size']) is not int or row['size'] < 0
                or row['mode'] not in ('0644', '0755') or not re.fullmatch('[0-9a-f]{64}', row['sha256'])):
            raise ValueError('invalid expected vendor inventory')
        pending[name] = row
    if not pending:
        raise ValueError('empty expected vendor inventory')
    seen = set()
    file_names = set()
    files = 0
    with tarfile.open(archive, 'r:*') as stream:
        for entry in stream:
            name = safe_relative(entry.name.rstrip('/'))
            if name in seen:
                raise ValueError('duplicate archive path')
            parts = name.split('/')
            if parts[0] != 'rsvg-vendor':
                raise ValueError('archive has unexpected root')
            if not (entry.isfile() or entry.isdir()):
                raise ValueError('archive links/special entries are forbidden')
            if any('/'.join(parts[:i]) in file_names for i in range(1, len(parts))):
                raise ValueError('archive file parent alias')
            if entry.isfile() and any(p.startswith(name + '/') for p in seen):
                raise ValueError('archive file aliases directory')
            seen.add(name)
            if entry.isdir():
                continue
            file_names.add(name)
            relative = '/'.join(parts[1:])
            row = pending.pop(relative, None)
            if row is None:
                raise ValueError('unexpected archive file')
            with stream.extractfile(entry) as body:
                actual_sha = hashlib.file_digest(body, 'sha256').hexdigest()
            mode = '0755' if entry.mode & 0o111 else '0644'
            if entry.size != row['size'] or actual_sha != row['sha256'] or mode != row['mode']:
                raise ValueError('vendor archive file bytes/size/mode differ')
            files += 1
    if pending:
        raise ValueError('archive is missing vendor files')
    return {'files': files, 'bytes': sum(row['size'] for row in expected),
            'archiveBytes': Path(archive).stat().st_size, 'archiveSha256': expected_sha}


def validate_vendor(source):
    lock = source / 'rsvg-Cargo.lock'
    if digest(lock) != LOCK_SHA:
        raise ValueError('fixed librsvg Cargo.lock differs')
    distribution = json.loads((source / 'source-distribution.json').read_text())
    upstream = distribution['sourceArchive']
    safe_relative(upstream['file'])
    if upstream['sha256'] != SOURCE_SHA or digest(source / upstream['file']) != SOURCE_SHA:
        raise ValueError('fixed libvips source archive differs')
    config = tomllib.loads((source / 'rsvg-vendor-config.toml').read_text())
    if (set(config) != {'source'} or set(config['source']) != {'crates-io', 'vendored-sources'}
            or config['source']['crates-io'] != {'replace-with': 'vendored-sources'}
            or set(config['source']['vendored-sources']) != {'directory'}):
        raise ValueError('unsupported original vendor source configuration')
    manifest = json.loads((source / 'vendor-manifest.json').read_text())
    locked = {p.get('checksum') for p in tomllib.loads(lock.read_text())['package'] if p.get('checksum')}
    vendor = source / 'rsvg-vendor'
    rows = vendor_inventory(vendor)
    crates = {path.name for path in vendor.iterdir() if path.is_dir()}
    if crates != set(manifest) or distribution['rsvgVendor']['crateCount'] != len(crates):
        raise ValueError('vendor crate inventory differs')
    for crate, identity in manifest.items():
        safe_relative(crate)
        checksum_file = vendor / crate / '.cargo-checksum.json'
        if digest(checksum_file) != identity['checksumFileSha256']:
            raise ValueError('vendor checksum manifest differs')
        checksum = json.loads(checksum_file.read_text())
        if checksum['package'] != identity['packageChecksum'] or checksum['package'] not in locked:
            raise ValueError('vendor package does not match exact lock')
        for relative, sha in checksum['files'].items():
            safe_relative(relative)
            if digest(vendor / crate / relative) != sha:
                raise ValueError('vendor crate file checksum differs')
    return rows


def fresh_output(output, source):
    output = Path(output).absolute()
    if os.path.lexists(output):
        raise ValueError('output already exists')
    output.parent.resolve(strict=True)
    if output.resolve().is_relative_to(Path(source).resolve()):
        raise ValueError('output must be outside input')
    return output


def compact(prefix, output, target, archive, archive_sha):
    # Existing common/component contract stays unchanged. No compiler or product runs here.
    from common import inventory, binary_target
    prefix = Path(prefix).resolve(strict=True)
    output = fresh_output(output, prefix)
    original_files = inventory(prefix)
    binary = prefix / 'bin/pilotdeck-image'
    if binary_target(binary) != target or not binary.stat().st_mode & 0o111:
        raise ValueError('image helper architecture/executable differs')
    if not any(p.is_file() for p in (prefix / 'share/licenses').rglob('*')):
        raise ValueError('image license distribution is missing')
    if not any(p.is_file() for p in (prefix / 'lib').glob('*.dylib')):
        raise ValueError('image runtime library is missing')
    source = prefix / 'share/source'
    for name in [INDEX, ARCHIVE, 'restore-rsvg-vendor.py']:
        if os.path.lexists(source / name):
            raise ValueError('vendor already compacted or reserved source path exists')
    rows = validate_vendor(source)
    proof = archive_proof(archive, rows, archive_sha)
    provenance_names = ['source-distribution.json', 'vendor-manifest.json', 'vendor-verification.json',
                        'rsvg-Cargo.lock', 'rsvg-vendor-config.toml', 'original-vendor-config.toml']
    provenance = {name: digest(source / name) for name in provenance_names}
    original_build = (source / 'BUILD-MACOS.md').read_bytes()
    provenance['provenance/original-BUILD-MACOS.md'] = hashlib.sha256(original_build).hexdigest()
    license_inventory = vendor_inventory(prefix / 'share/licenses')
    output.mkdir()
    try:
        def exclude(path, names):
            return ['rsvg-vendor'] if Path(path) == source else []
        shutil.copytree(prefix, output, symlinks=True, dirs_exist_ok=True, ignore=exclude)
        dest = output / 'share/source'
        shutil.copy2(archive, dest / ARCHIVE)
        shutil.copy2(Path(__file__), dest / 'restore-rsvg-vendor.py')
        index = {'schema': 1, 'archive': ARCHIVE, 'archiveSha256': archive_sha,
                 'inventory': rows, 'licenseInventory': license_inventory, 'originalProvenanceSha256': provenance,
                 'historicalOfflineMetadataReceipt': 'vendor-verification.json',
                 'scope': 'lossless source representation; historical build evidence retained, no new compilation'}
        write_json(dest / INDEX, index)
        history = dest / 'provenance'
        history.mkdir()
        (history / 'original-BUILD-MACOS.md').write_bytes(original_build)
        with (dest / 'BUILD-MACOS.md').open('a') as doc:
            doc.write('\n## Archived vendor layout\n\nThe original paragraphs and receipts above describe the original build. '
                      'The complete identical vendor is now in rsvg-vendor.tar.gz; restore before rebuilding. '
                      'Run `python3 restore-rsvg-vendor.py restore --source . --out /absolute/NEW-build-input`. '
                      'The recipe verifies the archive SHA and every file, then writes the same crate-source replacement '
                      'into NEW-build-input/cargo-home/config.toml with net.offline=true. '
                      'Use that isolated CARGO_HOME for the existing locked source build and local source-cache recipe. '
                      'No crates or licenses are dropped, and no new offline compilation is claimed.\n')
        if inventory(prefix) != original_files:
            raise ValueError('original prefix changed during copy')
        if any(digest(dest / name) != sha for name, sha in provenance.items()):
            raise ValueError('source provenance changed')
        if inventory(prefix / 'share/licenses') != inventory(output / 'share/licenses'):
            raise ValueError('licenses changed')
        if digest(binary) != digest(output / 'bin/pilotdeck-image'):
            raise ValueError('helper bytes changed')
        if inventory(prefix / 'lib') != inventory(output / 'lib'):
            raise ValueError('runtime libraries changed')
        verify(dest)
        return {'proof': proof, 'originalUnchanged': True, 'licensesAndRuntimeUnchanged': True,
                'originalBytes': sum(r.get('size', 0) for r in original_files),
                'compactedBytes': sum(r.get('size', 0) for r in inventory(output))}
    except BaseException:
        shutil.rmtree(output)
        raise


def verify(source):
    source = Path(source).resolve(strict=True)
    index = json.loads((source / INDEX).read_text())
    if index['schema'] != 1 or index['archive'] != ARCHIVE:
        raise ValueError('unexpected vendor archive identity')
    if vendor_inventory(source.parent / 'licenses') != index['licenseInventory']:
        raise ValueError('license distribution differs or is missing')
    for name, sha in index['originalProvenanceSha256'].items():
        safe_relative(name)
        if digest(source / name) != sha:
            raise ValueError('source provenance changed')
    return archive_proof(source / ARCHIVE, index['inventory'], index['archiveSha256'])


def restore(source, output):
    source = Path(source).resolve(strict=True)
    output = fresh_output(output, source)
    proof = verify(source)  # All bytes/paths checked before creating output.
    index = json.loads((source / INDEX).read_text())
    output.mkdir()
    try:
        with tarfile.open(source / ARCHIVE, 'r:*') as stream:
            for entry in stream:
                relative = safe_relative(entry.name.rstrip('/'))
                if relative.split('/')[0] != 'rsvg-vendor' or not (entry.isdir() or entry.isfile()):
                    raise ValueError('unsafe archive changed during restoration')
                path = output / relative
                if entry.isdir():
                    path.mkdir(parents=True, exist_ok=True)
                else:
                    path.parent.mkdir(parents=True, exist_ok=True)
                    with stream.extractfile(entry) as body, path.open('xb') as destination:
                        shutil.copyfileobj(body, destination)
                    path.chmod(0o755 if entry.mode & 0o111 else 0o644)
        if digest(source / ARCHIVE) != index['archiveSha256']:
            raise ValueError('archive changed during restoration')
        if vendor_inventory(output / 'rsvg-vendor') != index['inventory']:
            raise ValueError('restored vendor differs')
        config = ('[net]\noffline = true\n[source.crates-io]\nreplace-with = "vendored-sources"\n'
                  '[source.vendored-sources]\ndirectory = ' + json.dumps(str(output / 'rsvg-vendor')) + '\n')
        home = output / 'cargo-home'
        home.mkdir()
        (home / 'config.toml').write_text(config)
        return {'proof': proof, 'restoredInventoryEquivalent': True,
                'cargoConfig': str(home / 'config.toml'), 'offline': True, 'compilerStarted': False}
    except BaseException:
        shutil.rmtree(output)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    compact_parser = commands.add_parser('compact')
    compact_parser.add_argument('--input', type=Path, required=True)
    compact_parser.add_argument('--out', type=Path, required=True)
    compact_parser.add_argument('--target', required=True)
    compact_parser.add_argument('--vendor-archive', type=Path, required=True)
    compact_parser.add_argument('--archive-sha256', required=True)
    for name in ['verify', 'restore']:
        item = commands.add_parser(name)
        item.add_argument('--source', type=Path, required=True)
        if name == 'restore':
            item.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    if args.command == 'compact':
        result = compact(args.input, args.out, args.target, args.vendor_archive, args.archive_sha256)
    elif args.command == 'verify':
        result = verify(args.source)
    else:
        result = restore(args.source, args.out)
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
