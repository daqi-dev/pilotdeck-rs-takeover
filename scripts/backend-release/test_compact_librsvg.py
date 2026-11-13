"""Small genuine tar/checksum fixtures; no native image build or product-process claim."""
import io
import json
import os
import tarfile
import tempfile
import tomllib
import unittest
from pathlib import Path
from unittest.mock import patch

from common import archive, digest, inventory
import compact_librsvg as compact_module
from compact_librsvg import compact, restore, verify, ARCHIVE, INDEX


class CompactLibrsvgTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='pd-rsvg-compact-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.prefix = self.root / 'prefix'
        self.source = self.prefix / 'share/source'
        self.vendor = self.source / 'rsvg-vendor'
        crate = self.vendor / 'sample'
        (crate / 'src').mkdir(parents=True)
        (crate / 'Cargo.toml').write_text('[package]\nname="sample"\nversion="1.0.0"\n')
        (crate / 'src/lib.rs').write_bytes(b'fixed source\x00\xff\n')
        (crate / 'LICENSE').write_text('controlled vendor license')
        (crate / 'run.sh').write_text('#!/bin/sh\nexit 0\n')
        (crate / 'run.sh').chmod(0o755)
        package_sha = 'a' * 64
        checksums = {'package': package_sha, 'files': {p.relative_to(crate).as_posix(): digest(p) for p in crate.rglob('*') if p.is_file()}}
        (crate / '.cargo-checksum.json').write_text(json.dumps(checksums))
        (self.source / 'rsvg-Cargo.lock').write_text('[[package]]\nname="sample"\nversion="1.0.0"\nchecksum="' + package_sha + '"\n')
        (self.source / 'vendor-manifest.json').write_text(json.dumps({'sample': {'packageChecksum': package_sha, 'checksumFileSha256': digest(crate / '.cargo-checksum.json')}}))
        (self.source / 'libvips.tar.gz').write_bytes(b'controlled original source archive')
        (self.source / 'source-distribution.json').write_text(json.dumps({'sourceArchive': {'file': 'libvips.tar.gz', 'sha256': digest(self.source / 'libvips.tar.gz')}, 'rsvgVendor': {'crateCount': 1}}))
        (self.source / 'vendor-verification.json').write_text('{"historical":true}')
        (self.source / 'rsvg-vendor-config.toml').write_text('[source.crates-io]\nreplace-with="vendored-sources"\n[source.vendored-sources]\ndirectory="/original/vendor"\n')
        (self.source / 'original-vendor-config.toml').write_text('original retained config')
        (self.source / 'BUILD-MACOS.md').write_text('original source build recipe\n')
        (self.prefix / 'share/licenses').mkdir()
        (self.prefix / 'share/licenses/LICENSE').write_text('controlled image license')
        (self.prefix / 'bin').mkdir()
        (self.prefix / 'bin/pilotdeck-image').write_text('CONTROLLED FAKE helper')
        (self.prefix / 'bin/pilotdeck-image').chmod(0o755)
        (self.prefix / 'lib').mkdir()
        (self.prefix / 'lib/libvips-cpp.8.dylib').write_text('CONTROLLED FAKE library')
        os.symlink('libvips-cpp.8.dylib', self.prefix / 'lib/libvips-cpp.dylib')
        self.archive = self.root / 'vendor.tar.gz'
        archive(self.vendor, self.archive, 0, 'rsvg-vendor')
        self.archive_sha = digest(self.archive)
        for name, value in [('LOCK_SHA', digest(self.source / 'rsvg-Cargo.lock')), ('SOURCE_SHA', digest(self.source / 'libvips.tar.gz'))]:
            control = patch.object(compact_module, name, value)
            control.start()
            self.addCleanup(control.stop)
        control = patch('common.binary_target', return_value='aarch64-apple-darwin')
        control.start()
        self.addCleanup(control.stop)
        self.output = self.root / 'new-prefix'

    def convert(self):
        return compact(self.prefix, self.output, 'aarch64-apple-darwin', self.archive, self.archive_sha)

    def test_full_roundtrip_preserves_vendor_modes_provenance_licenses_and_runtime(self):
        before = inventory(self.prefix)
        result = self.convert()
        self.assertTrue(result['originalUnchanged'])
        self.assertFalse((self.output / 'share/source/rsvg-vendor').exists())
        self.assertEqual(before, inventory(self.prefix))
        for name in ['source-distribution.json', 'vendor-manifest.json', 'vendor-verification.json', 'rsvg-Cargo.lock', 'rsvg-vendor-config.toml']:
            self.assertEqual(digest(self.source / name), digest(self.output / 'share/source' / name))
        destination = self.root / 'restored'
        self.assertTrue(restore(self.output / 'share/source', destination)['restoredInventoryEquivalent'])
        self.assertEqual(inventory(self.vendor), inventory(destination / 'rsvg-vendor'))
        config = tomllib.loads((destination / 'cargo-home/config.toml').read_text())
        self.assertTrue(config['net']['offline'])
        self.assertEqual(config['source']['vendored-sources']['directory'], str(destination / 'rsvg-vendor'))
        self.assertEqual(config['source']['crates-io'], {'replace-with': 'vendored-sources'})
        # Existing public component() accepts this archived layout, unchanged.
        from assemble import component
        target = self.root / 'component-copy'
        with patch('assemble.binary_target', return_value='aarch64-apple-darwin'):
            self.assertEqual(component(self.output, 'image', target, 'aarch64-apple-darwin')['sourceIncluded'], True)
        self.assertEqual(inventory(self.output), inventory(target / 'components/image'))
        self.assertEqual((target / 'components/image/share/source/provenance/original-BUILD-MACOS.md').read_bytes(), (self.source / 'BUILD-MACOS.md').read_bytes())

    def test_standalone_restore_has_no_repository_python_import_dependency(self):
        import subprocess
        import sys
        self.convert()
        source = self.output / 'share/source'
        dest = self.root / 'standalone-restored'
        result = subprocess.run([sys.executable, '-I', str(source / 'restore-rsvg-vendor.py'),
                                 'restore', '--source', str(source), '--out', str(dest)],
                                text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(inventory(self.vendor), inventory(dest / 'rsvg-vendor'))

    def test_verify_and_restore_reject_missing_license_before_output(self):
        self.convert()
        (self.output / 'share/licenses/LICENSE').unlink()
        destination = self.root / 'restore-no-license'
        with self.assertRaises(ValueError):
            restore(self.output / 'share/source', destination)
        self.assertFalse(destination.exists())

    def test_fixed_lock_source_archive_and_config_are_checked(self):
        lock = self.source / 'rsvg-Cargo.lock'
        original = lock.read_bytes()
        lock.write_text('bad lock')
        with self.assertRaisesRegex(ValueError, 'Cargo.lock'):
            self.convert()
        lock.write_bytes(original)
        (self.source / 'libvips.tar.gz').write_text('wrong source')
        with self.assertRaisesRegex(ValueError, 'source archive'):
            self.convert()
        self.assertFalse(self.output.exists())

    def test_missing_license_is_rejected_before_output(self):
        (self.prefix / 'share/licenses/LICENSE').unlink()
        with self.assertRaisesRegex(ValueError, 'license'):
            self.convert()
        self.assertFalse(self.output.exists())

    def test_bad_archive_sha_is_rejected(self):
        self.archive.write_bytes(b'wrong archive')
        with self.assertRaisesRegex(ValueError, 'SHA'):
            self.convert()
        self.assertFalse(self.output.exists())

    def test_corrupt_archive_with_matching_sha_is_rejected(self):
        self.archive.write_bytes(b'not tar')
        self.archive_sha = digest(self.archive)
        with self.assertRaises(tarfile.ReadError):
            self.convert()
        self.assertFalse(self.output.exists())

    def test_missing_or_changed_member_and_exec_mode_are_rejected(self):
        for mutation in ['missing', 'bytes', 'mode']:
            with self.subTest(mutation=mutation):
                with tarfile.open(self.archive, 'w:gz') as stream:
                    for path in sorted(self.vendor.rglob('*')):
                        if not path.is_file():
                            continue
                        if mutation == 'missing' and path.name == 'run.sh':
                            continue
                        entry = stream.gettarinfo(str(path), arcname='rsvg-vendor/' + path.relative_to(self.vendor).as_posix())
                        if mutation == 'mode' and path.name == 'run.sh':
                            entry.mode = 0o644
                        content = path.read_bytes()
                        if mutation == 'bytes' and path.name == 'run.sh':
                            content = b'x' * len(content)
                        stream.addfile(entry, io.BytesIO(content))
                self.archive_sha = digest(self.archive)
                with self.assertRaises(ValueError):
                    self.convert()
                self.assertFalse(self.output.exists())

    def test_unsafe_paths_links_and_duplicate_members_are_rejected(self):
        for name, kind, link in [('../escape', tarfile.REGTYPE, ''), ('/absolute', tarfile.REGTYPE, ''),
                                 ('rsvg-vendor/a/../x', tarfile.REGTYPE, ''),
                                 ('rsvg-vendor/link', tarfile.SYMTYPE, '../../escape'),
                                 ('rsvg-vendor/hard', tarfile.LNKTYPE, 'rsvg-vendor/sample/run.sh'),
                                 ('rsvg-vendor/sample/run.sh', tarfile.REGTYPE, '')]:
            with self.subTest(name=name):
                with tarfile.open(self.archive, 'w:gz') as stream:
                    for path in sorted(self.vendor.rglob('*')):
                        stream.add(path, arcname='rsvg-vendor/' + path.relative_to(self.vendor).as_posix(), recursive=False)
                    entry = tarfile.TarInfo(name)
                    entry.type = kind
                    entry.linkname = link
                    stream.addfile(entry)
                self.archive_sha = digest(self.archive)
                with self.assertRaises(ValueError):
                    self.convert()
                self.assertFalse(self.output.exists())
        self.assertFalse((self.root / 'escape').exists())

    def test_existing_outputs_and_inside_input_are_rejected(self):
        self.output.mkdir()
        marker = self.output / 'marker'
        marker.write_text('retain')
        with self.assertRaisesRegex(ValueError, 'exists'):
            self.convert()
        self.assertEqual(marker.read_text(), 'retain')
        with self.assertRaisesRegex(ValueError, 'outside'):
            compact(self.prefix, self.prefix / 'nested', 'aarch64-apple-darwin', self.archive, self.archive_sha)

    def test_vendor_checksum_lock_and_provenance_tamper_are_rejected(self):
        (self.vendor / 'sample/run.sh').write_text('changed source')
        with self.assertRaisesRegex(ValueError, 'checksum'):
            self.convert()
        self.assertFalse(self.output.exists())

    def test_restore_existing_output_rejected_and_late_failure_cleans_only_new_output(self):
        self.convert()
        destination = self.root / 'restored'
        destination.mkdir()
        (destination / 'marker').write_text('retain')
        with self.assertRaisesRegex(ValueError, 'exists'):
            restore(self.output / 'share/source', destination)
        self.assertEqual((destination / 'marker').read_text(), 'retain')
        fresh = self.root / 'fresh-restore'
        with patch.object(compact_module, 'vendor_inventory', side_effect=ValueError('controlled late fault')):
            with self.assertRaisesRegex(ValueError, 'late fault'):
                restore(self.output / 'share/source', fresh)
        self.assertFalse(fresh.exists())

    def test_verify_rejects_modified_archive_and_modified_original_receipt(self):
        self.convert()
        source = self.output / 'share/source'
        (source / 'vendor-verification.json').write_text('changed receipt')
        with self.assertRaisesRegex(ValueError, 'provenance'):
            verify(source)
        (source / 'vendor-verification.json').write_bytes((self.source / 'vendor-verification.json').read_bytes())
        (source / ARCHIVE).write_text('changed tar')
        with self.assertRaisesRegex(ValueError, 'SHA'):
            verify(source)


if __name__ == '__main__':
    unittest.main()
