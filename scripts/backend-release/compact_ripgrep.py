#!/usr/bin/env python3
"""Copy a validated rg prefix with one lossless vendor representation; never edit input."""
import argparse
import hashlib
import json
import os
import shutil
import tarfile
from pathlib import Path, PurePosixPath

from common import digest, inventory, write_json
from ripgrep_component import RECORD, distribution_identity, validate_ripgrep

PROVENANCE = 'share/compaction/original-ripgrep-build.json'
RECEIPT = 'share/compaction/vendor-compaction.json'


def prove_vendor_archive(directory, archive):
    """Read archive only: no extraction, links, escapes, aliases, or duplicate paths."""
    directory = Path(directory)
    if directory.is_symlink() or not directory.is_dir() or Path(archive).is_symlink():
        raise ValueError('vendor directory/archive must not be symlinks')
    expected = inventory(directory)
    if not expected or any(row['type'] != 'file' for row in expected):
        raise ValueError('compact vendor requires nonempty regular-file tree without symlinks')
    pending = {row['path']: row for row in expected}
    seen = set()
    regular_names = set()
    archive_files = 0
    with tarfile.open(archive, 'r:*') as stream:
        for member in stream:
            name = member.name.rstrip('/')
            parts = name.split('/')
            root_metadata = member.isfile() and name in {'Cargo.lock', 'original-vendor-config.toml'}
            if (not name or name.startswith('/') or any(p in ('', '.', '..') for p in parts)
                    or '\\' in name or any(c in name for c in '\r\n\0')
                    or (parts[0] != directory.name and not root_metadata) or PurePosixPath(name).is_absolute()):
                raise ValueError('unsafe vendor archive path')
            if name in seen:
                raise ValueError('duplicate vendor archive member')
            if any('/'.join(parts[:i]) in regular_names for i in range(1, len(parts))):
                raise ValueError('vendor archive member has a file parent')
            if member.isfile() and any(previous.startswith(name + '/') for previous in seen):
                raise ValueError('vendor archive file aliases a directory')
            seen.add(name)
            if not (member.isfile() or member.isdir()):
                raise ValueError('vendor archive links/special members are forbidden')
            if member.isdir():
                continue
            regular_names.add(name)
            archive_files += 1
            relative = '/'.join(parts[1:]) if parts[0] == directory.name else None
            if not relative and not root_metadata:
                raise ValueError('vendor archive root must be a directory')
            expected_row = pending.get(relative)
            # Extra regular files (the existing tar's lock/config) are retained in the tar.
            with stream.extractfile(member) as body:
                sha = hashlib.file_digest(body, 'sha256').hexdigest()
            if expected_row is not None:
                if member.size != expected_row['size'] or sha != expected_row['sha256']:
                    raise ValueError('vendor archive bytes differ from expanded tree')
                if bool(member.mode & 0o111) != (expected_row['mode'] == '0755'):
                    raise ValueError('vendor archive executable mode differs')
                del pending[relative]
    if pending:
        raise ValueError('vendor archive does not cover every expanded file')
    return {'expandedInventory': expected, 'expandedFiles': len(expected),
            'expandedBytes': sum(row['size'] for row in expected),
            'archiveFiles': archive_files, 'archiveSha256': digest(archive)}


def compact(prefix, output, target, vendor_archive='share/source/registry-vendor-source.tar.gz',
            expanded_vendor=None):
    prefix = Path(prefix).resolve(strict=True)
    output = Path(output).absolute()
    if os.path.lexists(output):
        raise ValueError('output already exists; no overwrite is permitted')
    output.parent.resolve(strict=True)
    if output.resolve().is_relative_to(prefix):
        raise ValueError('output must not be inside input prefix')
    original_checked = validate_ripgrep(prefix, target)
    original_record_bytes = (prefix / RECORD).read_bytes()
    record = json.loads(original_record_bytes)
    vendor = record['distribution']['vendor']
    expanded_relative = expanded_vendor or (vendor['path'] if vendor['type'] == 'directory'
                                            else 'share/source/registry-vendor')
    # Resolve safely before scanning; do not permit caller-selected paths outside prefix.
    from ripgrep_component import checked_path
    expanded = checked_path(prefix, expanded_relative)
    if vendor['type'] == 'directory' and expanded_relative != vendor['path']:
        raise ValueError('expanded path differs from recorded vendor directory')
    archive_identity = distribution_identity(prefix, vendor_archive)
    if vendor['type'] == 'file' and vendor != archive_identity:
        raise ValueError('retained archive differs from recorded vendor role')
    if archive_identity['type'] != 'file':
        raise ValueError('vendor archive must be a regular file')
    for reserved in [PROVENANCE, RECEIPT]:
        if os.path.lexists(prefix / reserved):
            raise ValueError('compaction provenance path already exists')
    # No retained source/license/lock/evidence/binary role may be removed or re-identified.
    roles = [entry for entries in record['distribution']['licenses'].values() for entry in entries]
    roles += list(record['distribution']['sources'].values())
    roles += [record['distribution']['cargoLock'], record['binary'], *record['buildEvidence']]
    roles += [{'path': RECORD}, {'path': vendor_archive}]
    for role in roles:
        path = prefix / role['path']
        if path == expanded or path.is_relative_to(expanded) or expanded.is_relative_to(path):
            raise ValueError('expanded vendor overlaps a retained distribution role')
    proof = prove_vendor_archive(expanded, prefix / vendor_archive)
    original_files = inventory(prefix)
    receipt = {'schema': 1, 'kind': 'lossless-vendor-layout-compaction',
               'originalBuildRecordSha256': digest(prefix / RECORD),
               'originalBuildRecord': PROVENANCE, 'originalComponent': original_checked,
               'removedExpandedPath': expanded_relative, 'retainedArchive': archive_identity,
               'coverage': proof,
               'scope': 'source layout only; binary, source pins, locks, licenses and build evidence unchanged'}
    output.mkdir()
    try:
        def exclude(path, names):
            return [name for name in names if Path(path) / name == expanded]
        shutil.copytree(prefix, output, symlinks=True, dirs_exist_ok=True, ignore=exclude)
        (output / PROVENANCE).parent.mkdir(parents=True)
        (output / PROVENANCE).write_bytes(original_record_bytes)
        write_json(output / RECEIPT, receipt)
        record['distribution']['vendor'] = archive_identity
        record['compaction'] = {'originalBuildRecord': PROVENANCE, 'receipt': RECEIPT,
                                'receiptSha256': digest(output / RECEIPT)}
        record['files'] = [row for row in inventory(output) if row['path'] != RECORD]
        write_json(output / RECORD, record)
        result = validate_ripgrep(output, target)
        if inventory(prefix) != original_files or (prefix / RECORD).read_bytes() != original_record_bytes:
            raise ValueError('input prefix changed during compaction')
        if any(digest(output / role['path']) != digest(prefix / role['path'])
               for role in roles if (prefix / role['path']).is_file() and role['path'] != RECORD):
            raise ValueError('retained role bytes changed')
        return {'component': result, 'proof': receipt, 'originalUnchanged': True,
                'removedExpandedBytes': proof['expandedBytes'],
                'originalFileBytes': sum(row.get('size', 0) for row in original_files),
                'compactedFileBytes': sum(row.get('size', 0) for row in inventory(output))}
    except BaseException:
        shutil.rmtree(output)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--target', required=True)
    args = parser.parse_args()
    print(json.dumps(compact(args.input, args.out, args.target), ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
