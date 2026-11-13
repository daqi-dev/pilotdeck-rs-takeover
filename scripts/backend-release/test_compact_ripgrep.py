"""Real tar byte controls over synthetic component records; no product-engine claim."""
import io
import json
import os
import tarfile
import unittest
from unittest.mock import patch

from common import digest, inventory
from compact_ripgrep import compact, prove_vendor_archive, PROVENANCE, RECEIPT
import test_release as release_fixture


class CompactRipgrepTests(unittest.TestCase):
    def setUp(self):
        self.fixture = release_fixture.RipgrepPackagingTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.tearDown)
        self.prefix = self.fixture.prefix
        self.vendor = self.prefix / 'share/source/vendor'
        (self.vendor / 'src').mkdir()
        (self.vendor / 'src/lib.rs').write_bytes(b'controlled source\x00\xff\n')
        self.archive = self.prefix / 'share/source/registry-vendor-source.tar.gz'
        self.make_archive()
        self.refresh()
        self.output = self.fixture.root / 'compacted'
        self.arch = patch('ripgrep_component.binary_target', return_value=self.fixture.target)
        self.arch.start()
        self.addCleanup(self.arch.stop)

    def make_archive(self, mutate=None):
        with tarfile.open(self.archive, 'w:gz') as stream:
            for path in sorted(self.vendor.rglob('*')):
                stream.add(path, arcname='vendor/' + path.relative_to(self.vendor).as_posix(), recursive=False)
            extra = tarfile.TarInfo('vendor/original-vendor-config.toml')
            extra.size = 4
            stream.addfile(extra, io.BytesIO(b'kept'))
            if mutate:
                mutate(stream)

    def refresh(self):
        from ripgrep_component import distribution_identity
        self.fixture.record['distribution']['vendor'] = distribution_identity(self.prefix, 'share/source/vendor')
        self.fixture.record['files'] = [r for r in inventory(self.prefix) if r['path'] != 'share/ripgrep-build.json']
        self.fixture.write_record()

    def test_real_tar_covers_every_source_and_preserves_all_original_roles(self):
        before = inventory(self.prefix)
        result = compact(self.prefix, self.output, self.fixture.target)
        self.assertEqual(before, inventory(self.prefix))
        self.assertTrue(result['originalUnchanged'])
        self.assertFalse((self.output / 'share/source/vendor').exists())
        self.assertEqual(digest(self.archive), digest(self.output / self.archive.relative_to(self.prefix)))
        original = json.loads((self.output / PROVENANCE).read_text())
        revised = json.loads((self.output / 'share/ripgrep-build.json').read_text())
        self.assertEqual(original['sourcePins'], revised['sourcePins'])
        self.assertEqual(original['buildEvidence'], revised['buildEvidence'])
        for role in ['sources', 'licenses', 'cargoLock']:
            self.assertEqual(original['distribution'][role], revised['distribution'][role])
        self.assertEqual(revised['distribution']['vendor']['type'], 'file')
        self.assertEqual(result['proof']['coverage']['expandedFiles'], 2)
        self.assertEqual(result['proof']['coverage']['archiveFiles'], 3)
        self.assertTrue((self.output / RECEIPT).is_file())
        # A small synthetic record can grow from its proof; do not promise a size reduction here.
        self.assertIn('compactedFileBytes', result)

    def test_already_archive_bound_record_loses_only_unreferenced_duplicate_tree(self):
        from ripgrep_component import distribution_identity
        self.fixture.record['distribution']['vendor'] = distribution_identity(
            self.prefix, self.archive.relative_to(self.prefix).as_posix())
        self.fixture.record['files'] = [r for r in inventory(self.prefix) if r['path'] != 'share/ripgrep-build.json']
        self.fixture.write_record()
        before = inventory(self.prefix)
        original_role = self.fixture.record['distribution']['vendor']
        result = compact(self.prefix, self.output, self.fixture.target,
                         expanded_vendor='share/source/vendor')
        revised = json.loads((self.output / 'share/ripgrep-build.json').read_text())
        self.assertEqual(original_role, revised['distribution']['vendor'])
        self.assertEqual(inventory(self.prefix), before)
        self.assertTrue(result['originalUnchanged'])

    def test_existing_output_is_never_overwritten(self):
        self.output.mkdir()
        marker = self.output / 'marker'
        marker.write_text('keep')
        with self.assertRaisesRegex(ValueError, 'already exists'):
            compact(self.prefix, self.output, self.fixture.target)
        self.assertEqual(marker.read_text(), 'keep')

    def test_output_inside_input_is_rejected(self):
        with self.assertRaisesRegex(ValueError, 'inside input'):
            compact(self.prefix, self.prefix / 'new', self.fixture.target)

    def test_missing_source_in_archive_is_rejected(self):
        with tarfile.open(self.archive, 'w:gz') as stream:
            stream.add(self.vendor / '.cargo-checksum.json', arcname='vendor/.cargo-checksum.json')
        self.refresh()
        with self.assertRaisesRegex(ValueError, 'every expanded file'):
            compact(self.prefix, self.output, self.fixture.target)
        self.assertFalse(self.output.exists())

    def test_wrong_source_bytes_are_rejected(self):
        (self.vendor / 'src/lib.rs').write_text('modified source')
        self.refresh()
        with self.assertRaisesRegex(ValueError, 'bytes differ'):
            compact(self.prefix, self.output, self.fixture.target)

    def test_changed_executable_mode_is_rejected(self):
        (self.vendor / 'src/lib.rs').chmod(0o755)
        self.refresh()
        with self.assertRaisesRegex(ValueError, 'mode differs'):
            compact(self.prefix, self.output, self.fixture.target)

    def test_archive_unsafe_members_rejected_without_extraction(self):
        for name, kind, link in [('../escape', tarfile.REGTYPE, ''),
                                 ('/absolute', tarfile.REGTYPE, ''),
                                 ('vendor/a/../escape', tarfile.REGTYPE, ''),
                                 ('vendor/link', tarfile.SYMTYPE, '../../escape'),
                                 ('vendor/hard', tarfile.LNKTYPE, 'vendor/src/lib.rs'),
                                 ('vendor/fifo', tarfile.FIFOTYPE, '')]:
            with self.subTest(name=name):
                def add(stream):
                    entry = tarfile.TarInfo(name)
                    entry.type = kind
                    entry.linkname = link
                    stream.addfile(entry)
                self.make_archive(add)
                with self.assertRaises(ValueError):
                    prove_vendor_archive(self.vendor, self.archive)
        self.assertFalse((self.fixture.root / 'escape').exists())

    def test_fixed_root_lock_and_original_config_are_retained_in_archive(self):
        def add(stream):
            for name in ['Cargo.lock', 'original-vendor-config.toml']:
                entry = tarfile.TarInfo(name)
                entry.size = 4
                stream.addfile(entry, io.BytesIO(b'kept'))
        self.make_archive(add)
        result = prove_vendor_archive(self.vendor, self.archive)
        self.assertEqual(result['expandedFiles'], 2)
        self.assertEqual(result['archiveFiles'], 5)
        self.make_archive(lambda stream: stream.addfile(tarfile.TarInfo('unexpected-root')))
        with self.assertRaisesRegex(ValueError, 'unsafe'):
            prove_vendor_archive(self.vendor, self.archive)

    def test_duplicate_tar_path_is_rejected(self):
        self.make_archive(lambda stream: stream.add(self.vendor / '.cargo-checksum.json', arcname='vendor/.cargo-checksum.json'))
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            prove_vendor_archive(self.vendor, self.archive)

    def test_tar_file_and_directory_alias_is_rejected(self):
        def add(stream):
            stream.addfile(tarfile.TarInfo('vendor/alias/child'), io.BytesIO(b''))
            entry = tarfile.TarInfo('vendor/alias')
            stream.addfile(entry, io.BytesIO(b''))
        self.make_archive(add)
        with self.assertRaisesRegex(ValueError, 'aliases a directory'):
            prove_vendor_archive(self.vendor, self.archive)

    def test_late_validation_failure_removes_only_new_output(self):
        from ripgrep_component import validate_ripgrep
        calls = 0
        def reject_new(prefix, target):
            nonlocal calls
            calls += 1
            if calls == 2:
                raise ValueError('controlled late failure')
            return validate_ripgrep(prefix, target)
        before = inventory(self.prefix)
        with patch('compact_ripgrep.validate_ripgrep', side_effect=reject_new):
            with self.assertRaisesRegex(ValueError, 'controlled late failure'):
                compact(self.prefix, self.output, self.fixture.target)
        self.assertFalse(self.output.exists())
        self.assertEqual(before, inventory(self.prefix))

    def test_tree_symlink_is_rejected(self):
        os.symlink('src/lib.rs', self.vendor / 'alias')
        with self.assertRaisesRegex(ValueError, 'without symlinks'):
            prove_vendor_archive(self.vendor, self.archive)

    def test_input_inventory_or_evidence_tamper_still_rejected(self):
        (self.prefix / 'share/evidence/rg-exit.txt').write_text('1')
        with self.assertRaisesRegex(ValueError, 'inventory differs'):
            compact(self.prefix, self.output, self.fixture.target)
        self.refresh()
        with self.assertRaisesRegex(ValueError, 'evidence changed'):
            compact(self.prefix, self.output, self.fixture.target)

    def test_retained_source_role_cannot_be_removed_as_vendor(self):
        from ripgrep_component import distribution_identity
        self.fixture.record['distribution']['sources']['ripgrep'] = distribution_identity(self.prefix, 'share/source/vendor')
        self.refresh()
        with self.assertRaisesRegex(ValueError, 'overlaps'):
            compact(self.prefix, self.output, self.fixture.target)


if __name__ == '__main__':
    unittest.main()
