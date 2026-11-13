import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from resource_layout import resource_contract
from verify import verify
import tarfile


class ResourceLayoutTests(unittest.TestCase):
    def fixture(self, root, name, **values):
        directory = root / name
        directory.mkdir()
        (directory / 'plugin.json').write_text(json.dumps({'name': name, 'version': '1', **values}))

    def test_actual_resource_inputs_and_archive_closure(self):
        source = Path(__file__).resolve().parents[2] / 'packaging/backend-resources'
        with self.assertRaisesRegex(ValueError, 'explicit addon'):
            resource_contract(source / 'skills', source / 'builtin-plugins')
        full = resource_contract(source / 'skills', source / 'builtin-plugins', addon=True)
        self.assertEqual(full['optionalAddonsIncluded'], ['browser-use', 'funasr'])
        self.assertEqual(len(full['skillNames']), 32)
        self.assertEqual([p['name'] for p in full['plugins'] if not p['requiresNodeOnDiscovery']], ['windows-skills'])
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / 'addon'
            run = subprocess.run([sys.executable, str(Path(__file__).with_name('addon.py')), '--plugins', str(source / 'builtin-plugins'), '--plugin', 'browser-use', '--plugin', 'funasr', '--out', str(output), '--source-date-epoch', '0'], capture_output=True)
            self.assertEqual(run.returncode, 0, run.stderr)
            extracted = Path(temp) / 'moved'; extracted.mkdir()
            with tarfile.open(output / 'pilotdeck-explicit-plugin-addon.tar.gz') as archive:
                archive.extractall(extracted, filter='data')
            package = extracted / 'pilotdeck-explicit-plugin-addon'
            self.assertTrue(verify(package)['verified'])
            changed = package / 'builtin-plugins/browser-use/plugin.json'
            changed.write_text(changed.read_text() + ' ')
            with self.assertRaisesRegex(ValueError, 'inventory differs'):
                verify(package)

    def test_empty_core_has_no_node(self):
        self.assertFalse(resource_contract(None, None)['requiresNodeOnDiscovery'])

    def test_skill_plugin_is_allowed(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.fixture(root, 'windows-skills', skills='skills')
            result = resource_contract(None, root)
            self.assertEqual(result['plugins'][0]['name'], 'windows-skills')
            self.assertFalse(result['requiresNodeOnDiscovery'])

    def test_core_rejects_node_and_non_node_services(self):
        for command in ['node', '/usr/bin/python3']:
            with self.subTest(command=command), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                self.fixture(root, 'external', mcpServers={'external': {'command': command}})
                with self.assertRaisesRegex(ValueError, 'explicit addon'):
                    resource_contract(None, root)

    def test_addon_records_default_node_dependency(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.fixture(root, 'browser-use', mcpServers={'browser': {'command': '__PILOTDECK_NODE_EXECUTABLE__'}})
            result = resource_contract(None, root, addon=True)
            self.assertTrue(result['requiresNodeOnDiscovery'])
            self.assertEqual(result['optionalAddonsIncluded'], ['browser-use'])
            self.assertFalse(result['externalDependenciesIncluded'])

    def test_escaping_symlink_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'escape').symlink_to('/etc')
            with self.assertRaises(Exception):
                resource_contract(None, root)

    def test_addon_rejects_hidden_native_runtime(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); plugins = root / 'plugins'; plugins.mkdir()
            (plugins / 'LICENSE').write_text('test license')
            self.fixture(plugins, 'browser-use', mcpServers={'browser': {'command': 'node'}})
            (plugins / 'browser-use/node').write_bytes(b'\x7fELFhidden-runtime')
            run = subprocess.run([sys.executable, str(Path(__file__).with_name('addon.py')), '--plugins', str(plugins), '--plugin', 'browser-use', '--out', str(root / 'out'), '--source-date-epoch', '0'], capture_output=True)
            self.assertNotEqual(run.returncode, 0)
            self.assertIn(b'native executables', run.stderr)
            self.assertFalse((root / 'out/archive.json').exists())

    def test_addon_archive_has_no_binary_and_rejects_traversal(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); plugins = root / 'plugins'; plugins.mkdir()
            (plugins / 'LICENSE').write_text('test license')
            self.fixture(plugins, 'browser-use', mcpServers={'browser': {'command': 'node'}})
            script = Path(__file__).with_name('addon.py')
            args = [sys.executable, str(script), '--plugins', str(plugins), '--out', str(root / 'out'), '--source-date-epoch', '0', '--plugin']
            bad = subprocess.run(args + ['../escape'], capture_output=True)
            self.assertNotEqual(bad.returncode, 0)
            self.assertFalse((root / 'out').exists())
            good = subprocess.run(args + ['browser-use'], capture_output=True)
            self.assertEqual(good.returncode, 0, good.stderr)
            package = root / 'out/pilotdeck-explicit-plugin-addon'
            value = json.loads((package / 'manifest.json').read_text())
            self.assertFalse(value['containsCoreBinaries'])
            self.assertFalse(value['containsExternalRuntimes'])
            self.assertFalse((package / 'bin').exists())


if __name__ == '__main__':
    unittest.main()
