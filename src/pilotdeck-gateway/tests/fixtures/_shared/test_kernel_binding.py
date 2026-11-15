# anchor-selfcheck: unsupported shared source-binding tests, temporary Git repositories only
"""Exercise actual pinned source bindings without Cargo or a private Kernel copy."""
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

from _shared import negative_control as control


class KernelBinding(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='pdrs-kernel-binding-')
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.product = self.base / 'product'
        self.kernel = self.base / 'kernel'
        self.destination = self.base / 'scratch'
        self.destination.mkdir()
        self.init_repo(self.product, {'src/a.rs': 'fn f() {}\n', 'LICENSE': 'license\n',
                                     'rust-toolchain.toml': '[toolchain]\n',
                                     '.gitignore': '/deps/agent-kernel\n'})
        self.init_repo(self.kernel, {**{f'kernel/{name}/Cargo.toml': '[package]\n'
                                      for name in ('iface', 'scheduler-protocol', 'host')},
                                    'kernel/iface/src/lib.rs': 'pub struct Iface;\n'})
        self.commit = self.git(self.kernel, 'rev-parse', 'HEAD').strip()
        self.addCleanup(mock.patch.stopall)
        mock.patch.object(control, 'REPO', self.product).start()
        mock.patch.object(control, 'SRC', self.product / 'src').start()
        mock.patch.dict(os.environ).start()
        os.environ.pop(control.KERNEL_PIN_ENV, None)

    def git(self, root, *args):
        return subprocess.check_output(['git', '-C', str(root), *args], text=True)

    def init_repo(self, root, files):
        root.mkdir()
        for name, data in files.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(data)
        self.git(root, 'init', '-q')
        self.commit_repo(root)

    def commit_repo(self, root):
        self.git(root, 'add', '-A')
        self.git(root, '-c', 'user.email=t@t', '-c', 'user.name=t', 'commit', '-qm', 'fixture')

    def bind(self):
        (self.product / 'deps').mkdir(exist_ok=True)
        (self.product / control.KERNEL_BINDING).symlink_to(self.kernel, target_is_directory=True)
        os.environ[control.KERNEL_PIN_ENV] = self.commit

    def copy(self):
        receipt = {}
        result = control.source_copy(self.destination, binding_receipt=receipt)
        self.assertEqual(result, self.destination / 'src')
        return receipt

    def test_legacy_workspace_has_no_synthetic_dependency(self):
        receipt = self.copy()
        self.assertIsNone(receipt['before'])
        self.assertIsNone(receipt['after'])
        self.assertFalse((self.destination / 'deps').exists())

    def test_clean_explicit_binding_is_linked_and_original_inputs_unchanged(self):
        self.bind()
        before = {str(path): path.read_bytes() for root in (self.product, self.kernel)
                  for path in root.rglob('*')
                  if path.is_file() and '.git' not in path.parts and not path.is_symlink()}
        receipt = self.copy()
        link = self.destination / control.KERNEL_BINDING
        self.assertTrue(link.is_symlink())
        self.assertEqual(link.resolve(), self.kernel.resolve())
        self.assertEqual(receipt['before'], receipt['after'])
        self.assertEqual(receipt['before']['expectedCommit'], self.commit)
        self.assertEqual(receipt['before']['inputs']['kernel/iface/src/lib.rs'],
                         hashlib.sha256(b'pub struct Iface;\n').hexdigest())
        self.assertEqual(before, {name: Path(name).read_bytes() for name in before})
        self.assertEqual(self.git(self.product, 'status', '--porcelain'), '')
        self.assertEqual(self.git(self.kernel, 'status', '--porcelain'), '')

    def test_binding_never_infers_pin_from_head(self):
        self.bind()
        del os.environ[control.KERNEL_PIN_ENV]
        with self.assertRaisesRegex(RuntimeError, 'requires explicit'):
            self.copy()

    def test_full_expected_sha_is_required(self):
        self.bind()
        os.environ[control.KERNEL_PIN_ENV] = self.commit[:8]
        with self.assertRaisesRegex(RuntimeError, 'complete Git commit'):
            self.copy()

    def test_wrong_commit_and_new_clean_commit_are_refused(self):
        self.bind()
        (self.kernel / 'kernel/iface/src/lib.rs').write_text('pub struct Changed;\n')
        self.commit_repo(self.kernel)
        with self.assertRaisesRegex(RuntimeError, 'HEAD differs'):
            self.copy()

    def test_dirty_content_is_refused(self):
        self.bind()
        (self.kernel / 'kernel/iface/src/lib.rs').write_text('pub struct Changed;\n')
        with self.assertRaisesRegex(RuntimeError, 'checkout is dirty'):
            self.copy()

    def test_assume_unchanged_cannot_hide_content_drift(self):
        self.bind()
        self.git(self.kernel, 'update-index', '--assume-unchanged', 'kernel/iface/src/lib.rs')
        (self.kernel / 'kernel/iface/src/lib.rs').write_text('pub struct Hidden;\n')
        self.assertEqual(self.git(self.kernel, 'status', '--porcelain'), '')
        with self.assertRaisesRegex(RuntimeError, 'input differs from the pinned commit'):
            self.copy()

    def test_ignored_untracked_build_input_is_refused(self):
        self.bind()
        (self.kernel / '.git/info/exclude').write_text('hidden.rs\n')
        (self.kernel / 'kernel/iface/src/hidden.rs').write_text('pub struct Hidden;\n')
        self.assertEqual(self.git(self.kernel, 'status', '--porcelain'), '')
        with self.assertRaisesRegex(RuntimeError, 'untracked Kernel build input'):
            self.copy()

    def test_kernel_tracked_external_symlink_is_refused(self):
        self.bind()
        (self.kernel / 'kernel/iface/src/external.rs').symlink_to('/etc/hosts')
        self.commit_repo(self.kernel)
        os.environ[control.KERNEL_PIN_ENV] = self.git(self.kernel, 'rev-parse', 'HEAD').strip()
        with self.assertRaisesRegex(RuntimeError, 'not a plain tracked file'):
            self.copy()

    def test_arbitrary_product_external_link_stays_refused_with_binding(self):
        self.bind()
        (self.product / 'src/external.rs').symlink_to(self.kernel / 'kernel/iface/src/lib.rs')
        self.git(self.product, 'add', 'src/external.rs')
        with self.assertRaisesRegex(RuntimeError, 'changed while|symlink'):
            self.copy()
        self.assertFalse((self.destination / 'src/external.rs').exists())

    def test_dependency_content_change_during_source_copy_is_refused(self):
        self.bind()
        original = control._copy_input

        def copying(*args, **kwargs):
            original(*args, **kwargs)
            (self.kernel / 'kernel/iface/src/lib.rs').write_text('pub struct Moved;\n')

        with mock.patch.object(control, '_copy_input', side_effect=copying):
            with self.assertRaisesRegex(RuntimeError, 'checkout is dirty'):
                self.copy()

    def test_link_replacement_is_refused_even_for_same_source(self):
        self.bind()
        baseline = control._kernel_binding_snapshot()
        link = self.product / control.KERNEL_BINDING
        replacement = link.with_name('replacement')
        replacement.symlink_to(self.kernel, target_is_directory=True)
        replacement.replace(link)
        with self.assertRaisesRegex(RuntimeError, 'binding or its inputs changed'):
            control._verify_kernel_binding(baseline)

    def test_phase_content_drift_is_invalid_and_receipted(self):
        self.bind()
        receipt = self.copy()
        output = self.base / 'evidence'
        output.mkdir()
        runner = control.CargoTests(self.destination / 'src', output,
                                    {'CARGO_TARGET_DIR': str(self.base / 'target')},
                                    binding_receipt=receipt)

        def child(*args, **kwargs):
            (self.kernel / 'kernel/iface/src/lib.rs').write_text('pub struct Moved;\n')
            return subprocess.CompletedProcess(args[0], 0, 'success', '')

        with mock.patch.object(control, 'run_owned', side_effect=child):
            with self.assertRaisesRegex(RuntimeError, 'checkout is dirty'):
                runner.phase('drift', ['test-child'], timeout=1)
        import json
        phase = json.loads((output / 'drift.json').read_text())
        self.assertEqual(phase['kernelBindingBefore'], receipt['before'])
        self.assertIn('checkout is dirty', phase['kernelBindingError'])
        self.assertEqual(phase['returncode'], 0)

    def test_phase_refuses_changed_scratch_link_before_child(self):
        self.bind()
        receipt = self.copy()
        output = self.base / 'evidence'
        output.mkdir()
        link = self.destination / control.KERNEL_BINDING
        link.unlink()
        link.symlink_to(self.product, target_is_directory=True)
        runner = control.CargoTests(self.destination / 'src', output,
                                    {'CARGO_TARGET_DIR': str(self.base / 'target')},
                                    binding_receipt=receipt)
        with mock.patch.object(control, 'run_owned') as child:
            with self.assertRaisesRegex(RuntimeError, 'scratch Kernel binding changed'):
                runner.phase('redirect', ['test-child'], timeout=1)
            child.assert_not_called()


if __name__ == '__main__':
    unittest.main()
