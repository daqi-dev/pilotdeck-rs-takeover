"""Exercise the real wrapper with a fake executable, including Ninja tool mode."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

WRAPPER = Path(__file__).with_name('ninja-bounded.sh')

class NinjaBounded(unittest.TestCase):
    def test_actual_process_arguments(self):
        with tempfile.TemporaryDirectory(prefix='pdrs-ninja-') as directory:
            executable = Path(directory) / 'record-argv'
            executable.write_text('#!/usr/bin/env python3\nimport json,sys\nprint(json.dumps(sys.argv[1:]))\n')
            executable.chmod(0o755)
            cases = [
                (['-j200', '-t', 'clean', '-g'], ['-j6', '-t', 'clean', '-g']),
                (['--jobs=20', '-k0', 'all'], ['-j6', '-k0', 'all']),
                (['-j', '0', '--jobs', '19', '--version'], ['-j6', '--version']),
                (['-j2', '-tcommands', 'target'], ['-j6', '-tcommands', 'target']),
                (['--', '-j9'], ['-j6', '--', '-j9']),
            ]
            env = dict(os.environ, PDRS_NINJA_REAL=str(executable))
            for supplied, expected in cases:
                with self.subTest(supplied=supplied):
                    result = subprocess.run(['bash', str(WRAPPER), *supplied], env=env, capture_output=True, text=True, check=True)
                    self.assertEqual(json.loads(result.stdout), expected)
            missing = subprocess.run(['bash', str(WRAPPER), '--jobs'], env=env, capture_output=True, text=True)
            self.assertEqual(missing.returncode, 2)
            self.assertEqual(missing.stdout, '')

if __name__ == '__main__':
    unittest.main()
