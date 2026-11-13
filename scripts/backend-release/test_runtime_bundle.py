import json
import runpy
import struct
import tempfile
import unittest
from pathlib import Path

runtime = runpy.run_path(str(Path(__file__).with_name('runtime-bundle.py')))


class RuntimeBundleTests(unittest.TestCase):
    def test_actual_browser_sources_and_relocatable_inventory(self):
        root = Path(__file__).resolve().parents[2] / '_build/browser-use-runtime-20261113-first'
        if not root.exists():
            self.skipTest('actual optional runtime input unavailable')
        sources = runtime['validate_sources'](root.resolve(), 'browser')
        self.assertEqual([p['version'] for p in sources['packages']], ['0.0.75', '1.61.0-alpha-1778188671000', '1.61.0-alpha-1778188671000'])
        rows = runtime['payload_inventory'](root.resolve(), 'browser')
        self.assertTrue(any(p['type'] == 'symlink' for p in rows))
        self.assertFalse(any('home/' in p['path'] for p in rows))

    def test_native_platform_is_read_from_binary(self):
        with tempfile.TemporaryDirectory() as temp:
            file = Path(temp) / 'engine'
            header = bytearray(32); header[:4] = b'\xcf\xfa\xed\xfe'
            struct.pack_into('<I', header, 4, 0x100000c)
            struct.pack_into('<I', header, 16, 1)
            file.write_bytes(header + struct.pack('<IIIIII', 0x32, 24, 1, (26 << 16) | (6 << 8), 0, 0))
            self.assertEqual(runtime['native_platform'](file), {'os':'darwin', 'arch':'arm64', 'minimumOS':'26.6.0'})
            file.write_bytes(b'fake native executable')
            with self.assertRaisesRegex(ValueError, 'Mach-O'):
                runtime['native_platform'](file)

    def test_missing_runtime_and_escape_are_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            with self.assertRaisesRegex(ValueError, 'missing runtime'):
                runtime['payload_inventory'](root, 'browser')
            (root / 'outside').symlink_to('/etc')
            with self.assertRaises(ValueError):
                runtime['payload_inventory'](root, 'browser')

    def test_tampered_browser_lock_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root/'sources.json').write_text('{}')
            (root/'pnpm-lock.yaml').write_text('tampered')
            (root/'runtime-manifest.json').write_text(json.dumps({'browserIncluded':False,'lifecycleScriptsExecuted':False,'network':'offline','sourceManifestSha256':'wrong','lockSha256':'wrong'}))
            with self.assertRaisesRegex(ValueError, 'source or dependency lock changed'):
                runtime['validate_sources'](root,'browser')


if __name__ == '__main__':
    unittest.main()
