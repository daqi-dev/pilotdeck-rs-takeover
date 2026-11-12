#!/usr/bin/env python3
"""Isolated executable resource checks; these do not replace the Rust loader."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


artifact = module('artifact_inspect', ROOT / 'skills/_tools/artifact_inspect.py')
migration = module('migration', ROOT / 'skills/pilotdeck-skills-migration/scripts/migrate.py')
srt = module('srt', ROOT / 'skills/meeting-recorder-assistant/scripts/srt_transcript.py')


class Resources(unittest.TestCase):
    def test_catalog_is_not_empty_or_missing_names(self):
        expected = set(json.loads((ROOT / 'RESOURCE-COVERAGE.json').read_text())['skills'])
        actual = {path.parent.name for path in (ROOT / 'skills').glob('*/SKILL.md')}
        self.assertEqual(expected, actual)
        self.assertEqual(len(actual), 32)
        for path in (ROOT / 'skills').glob('*/SKILL.md'):
            self.assertIn(f'name: {path.parent.name}\n', path.read_text())
            self.assertGreater(len(path.read_text().split('---', 2)[-1].strip()), 250)

    def test_ooxml_text_formulas_and_no_visual_claim(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'sample.xlsx'
            with zipfile.ZipFile(path, 'w') as package:
                package.writestr('[Content_Types].xml', '<Types/>')
                package.writestr('xl/worksheets/sheet1.xml', '<worksheet><c><f>1+2</f><v>3</v></c></worksheet>')
            result = artifact.inspect(path)
            self.assertEqual(result['formulas']['xl/worksheets/sheet1.xml'], ['1+2'])
            self.assertEqual(result['text']['xl/worksheets/sheet1.xml'], ['3'])
            self.assertFalse(result['rendered'])
            self.assertFalse(result['recalculated'])

    def test_ooxml_entity_attack_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'unsafe.docx'
            with zipfile.ZipFile(path, 'w') as package:
                package.writestr('[Content_Types].xml', '<Types/>')
                package.writestr('word/document.xml', '<!DOCTYPE a [<!ENTITY a "x">]><a>&a;</a>')
            with self.assertRaisesRegex(ValueError, 'DTD/entity'):
                artifact.inspect(path)

    def test_csv_embedded_newline_not_extra_row(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'data.csv'
            path.write_text('name,value\n"two\nlines",3\n')
            result = artifact.inspect(path)
            self.assertEqual(result['rows'], 2)
            self.assertEqual(result['preview'][1], ['two\nlines', '3'])

    def test_migration_inventory_and_no_overwrite(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, destination = root / 'input', root / 'output'
            source.mkdir()
            (source / 'SKILL.md').write_text('---\nname: test\ndescription: fixture\n---\nbody\n')
            result = migration.migrate(source, destination)
            self.assertEqual((source / 'SKILL.md').read_bytes(), (destination / 'SKILL.md').read_bytes())
            self.assertEqual(set(result['files']), {'SKILL.md'})
            with self.assertRaisesRegex(ValueError, 'already exists'):
                migration.migrate(source, destination)

    def test_migration_rejects_external_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'input'
            source.mkdir()
            (source / 'SKILL.md').write_text('fixture')
            (source / 'external').symlink_to(root / 'outside')
            with self.assertRaisesRegex(ValueError, 'links'):
                migration.migrate(source, root / 'output')

    def test_srt_preserves_timestamp_words_and_rejects_bad_range(self):
        result = srt.parse('1\n00:00:01,250 --> 00:00:02,500\nhello\nworld\n')
        self.assertEqual(result, [{'start': 1.25, 'end': 2.5, 'text': 'hello\nworld'}])
        with self.assertRaisesRegex(ValueError, 'range'):
            srt.parse('1\n00:00:02,000 --> 00:00:01,000\ninvalid\n')

    def test_basic_artifact_builders_reopen_editable_formats(self):
        builder = module('artifact_build', ROOT / 'skills/_tools/artifact_build.py')
        from docx import Document
        from openpyxl import load_workbook
        from pptx import Presentation
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            word = root / 'report.docx'
            builder.build({'title': 'Report', 'paragraphs': ['Measured value'], 'tables': [[['Name', 'Value'], ['A', '3']]]}, word)
            reopened = Document(word)
            self.assertIn('Measured value', [p.text for p in reopened.paragraphs])
            self.assertEqual(reopened.tables[0].cell(1, 1).text, '3')
            sheet = root / 'numbers.xlsx'
            builder.build({'sheets': [{'name': 'Data', 'rows': [['Value'], [3], ['=SUM(A2:A2)']]}]}, sheet)
            reopened_sheet = load_workbook(sheet)
            self.assertEqual(reopened_sheet['Data']['A3'].value, '=SUM(A2:A2)')
            reopened_sheet.close()
            slides = root / 'deck.pptx'
            builder.build({'slides': [{'title': 'Outcome', 'body': 'Measured result'}]}, slides)
            reopened_slides = Presentation(slides)
            self.assertEqual(reopened_slides.slides[0].shapes.title.text, 'Outcome')
            with self.assertRaisesRegex(ValueError, 'already exists'):
                builder.build({'slides': [{'title': 'Overwrite'}]}, slides)

    def test_xlsx_case_insensitive_duplicates_fail_before_output(self):
        builder = module('artifact_build_case', ROOT / 'skills/_tools/artifact_build.py')
        from openpyxl import load_workbook
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'duplicate.xlsx'
            with self.assertRaisesRegex(ValueError, 'case-insensitive'):
                builder.build({'sheets': [{'name': 'Data', 'rows': [[1]]}, {'name': 'data', 'rows': [[2]]}]}, path)
            self.assertFalse(path.exists(), 'a rejected workbook must not be written')
            builder.build({'sheets': [{'name': 'Data', 'rows': [[1]]}, {'name': 'Other', 'rows': [[2]]}]}, path)
            workbook = load_workbook(path)
            self.assertEqual(workbook.sheetnames, ['Data', 'Other'])
            workbook.close()

    def test_pdf_actual_page_reorder_rotate_and_input_preservation(self):
        pages = module('pdf_pages', ROOT / 'skills/pdf/scripts/pdf_pages.py')
        from pypdf import PdfWriter, PdfReader
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'input.pdf'
            output = Path(directory) / 'output.pdf'
            writer = PdfWriter()
            writer.add_blank_page(width=100, height=200)
            writer.add_blank_page(width=300, height=400)
            with source.open('wb') as stream:
                writer.write(stream)
            before = source.read_bytes()
            result = pages.operate(source, output, [2, 1], 90)
            self.assertEqual(result['outputPages'], 2)
            reopened = PdfReader(output)
            self.assertEqual(float(reopened.pages[0].mediabox.width), 300)
            self.assertEqual(reopened.pages[0].rotation, 90)
            self.assertEqual(source.read_bytes(), before)
            with self.assertRaisesRegex(ValueError, 'new path'):
                pages.operate(source, output, [1], 0)

    @unittest.skipUnless(os.name == 'posix' and shutil.which('node'), 'POSIX and Node required')
    def test_funasr_supervisor_and_native_start_failures_have_receipts(self):
        import platform
        import select
        node = shutil.which('node')
        for mode in ['missing-python', 'native-start-failure']:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                home, project, runtime = root / 'home', root / 'project', root / 'runtime'
                home.mkdir(); project.mkdir()
                (project / 'audio.wav').write_bytes(b'fixture')
                key = {'Darwin': 'darwin', 'Linux': 'linux'}[platform.system()] + '-' + {'arm64': 'arm64', 'aarch64': 'arm64', 'x86_64': 'x64'}[platform.machine()]
                bin_dir = runtime / 'runtime/v0.2.0' / key
                bin_dir.mkdir(parents=True)
                binary = bin_dir / 'llama-funasr-sensevoice'
                binary.write_text('#!/nonexistent-pdrs-interpreter\n')
                binary.chmod(0o755)
                (runtime / 'models').mkdir()
                for name in ['sensevoice-small-q8.gguf', 'fsmn-vad.gguf']:
                    (runtime / 'models' / name).write_bytes(b'model fixture')
                env = dict(os.environ, HOME=str(home), PILOT_HOME=str(home / 'pilot'))
                if mode == 'missing-python':
                    env['PATH'] = str(root / 'empty-path')
                process = subprocess.Popen([node, str(ROOT / 'builtin-plugins/funasr/funasr-local-mcp.mjs'), '--project-root', str(project), '--runtime-root', str(runtime)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
                try:
                    process.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call', 'params': {'name': 'transcribe_audio', 'arguments': {'audio_path': 'audio.wav'}}}) + '\n')
                    process.stdin.flush()
                    ready, _, _ = select.select([process.stdout], [], [], 3)
                    self.assertTrue(ready, 'startup failure must not wait for generic timeout')
                    result = json.loads(process.stdout.readline())['result']
                    self.assertTrue(result['isError'])
                    self.assertIn('python3' if mode == 'missing-python' else 'supervisor failed', result['content'][0]['text'])
                    process.stdin.close()
                    self.assertEqual(process.wait(timeout=3), 0)
                finally:
                    if process.stdin and not process.stdin.closed:
                        process.stdin.close()
                    if process.poll() is None:
                        process.kill(); process.wait(timeout=3)
                    process.stdout.close(); process.stderr.close()

    @unittest.skipUnless(os.name == 'posix' and shutil.which('node'), 'POSIX Python supervisor and Node required')
    def test_funasr_owned_descendant_cancel_eof_and_normal_completion(self):
        import platform
        import select
        import signal
        import sys
        import time
        for mode in ['cancel', 'eof', 'normal']:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                project, home, runtime = root / 'project', root / 'home', root / 'runtime'
                project.mkdir(); home.mkdir()
                (project / 'audio.wav').write_bytes(b'fixture')
                key = {'Darwin': 'darwin', 'Linux': 'linux'}[platform.system()] + '-' + {'arm64': 'arm64', 'aarch64': 'arm64', 'x86_64': 'x64'}[platform.machine()]
                binary_dir = runtime / 'runtime/v0.2.0' / key
                binary_dir.mkdir(parents=True)
                binary = binary_dir / 'llama-funasr-sensevoice'
                binary.write_text('#!' + sys.executable + '\n' + """import os,subprocess,sys,time,pathlib
root=pathlib.Path(os.environ['PDRS_OWNED_FIXTURE'])
try:
 os.fstat(3)
 private_status_inherited=True
except OSError:
 private_status_inherited=False
(root/'native.json').write_text(__import__('json').dumps({'native':os.getpid(),'supervisor':os.getppid(),'privateStatusInherited':private_status_inherited}))
leaf=subprocess.Popen([sys.executable,'-c','import time;time.sleep(30)'],stdin=subprocess.DEVNULL)
(root/'descendant.pid').write_text(str(leaf.pid))
if os.environ['PDRS_OWNED_MODE']=='normal':
 print('1\\n00:00:01,000 --> 00:00:02,000\\nowned fixture',flush=True)
else:
 time.sleep(30)
""")
                binary.chmod(0o755)
                (runtime / 'models').mkdir()
                for name in ['sensevoice-small-q8.gguf', 'fsmn-vad.gguf']:
                    (runtime / 'models' / name).write_bytes(b'model fixture')
                env = dict(os.environ, HOME=str(home), PILOT_HOME=str(home / 'pilot'), PDRS_OWNED_FIXTURE=str(root), PDRS_OWNED_MODE=mode)
                process = subprocess.Popen([shutil.which('node'), str(ROOT / 'builtin-plugins/funasr/funasr-local-mcp.mjs'), '--project-root', str(project), '--runtime-root', str(runtime)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
                identities = None
                descendant = None
                def status(pid):
                    return subprocess.run(['/bin/ps', '-p', str(pid), '-o', 'pid=,pgid=,stat=,command='], capture_output=True, text=True, check=True).stdout.strip()
                try:
                    process.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call', 'params': {'name': 'transcribe_audio', 'arguments': {'audio_path': 'audio.wav'}}}) + '\n')
                    process.stdin.flush()
                    deadline = time.monotonic() + 5
                    while not (root / 'descendant.pid').exists():
                        self.assertLess(time.monotonic(), deadline, 'native readiness deadline')
                        time.sleep(0.01)
                    identities = json.loads((root / 'native.json').read_text())
                    descendant = int((root / 'descendant.pid').read_text())
                    self.assertFalse(identities['privateStatusInherited'], 'native child must not inherit supervisor status fd')
                    if mode == 'cancel':
                        process.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/cancelled', 'params': {'requestId': 1}}) + '\n')
                        process.stdin.flush()
                    if mode == 'eof':
                        process.stdin.close()
                    else:
                        readable, _, _ = select.select([process.stdout], [], [], 3)
                        self.assertTrue(readable, 'owned subtree must close before MCP receipt deadline')
                        reply = json.loads(process.stdout.readline())['result']
                        if mode == 'cancel':
                            self.assertTrue(reply['isError'])
                            self.assertIn('cancelled', reply['content'][0]['text'])
                        else:
                            self.assertEqual(reply['structuredContent']['text'], 'owned fixture')
                        process.stdin.close()
                    self.assertEqual(process.wait(timeout=3), 0, 'EOF must drain the owned group')
                    deadline = time.monotonic() + 3
                    remaining = {}
                    while time.monotonic() < deadline:
                        remaining = {}
                        for pid in [identities['supervisor'], identities['native'], descendant]:
                            # ps exits 1 when the exact PID no longer exists.
                            row = subprocess.run(['/bin/ps', '-p', str(pid), '-o', 'pid=,pgid=,stat=,command='], capture_output=True, text=True).stdout.strip()
                            if row:
                                remaining[pid] = row
                        if not remaining:
                            break
                        time.sleep(0.02)
                    self.assertEqual(remaining, {}, 'owned leader/native/descendant must all terminate')
                finally:
                    # Failure cleanup targets only the live, uniquely identified
                    # supervisor group. Never issue a name-wide kill or signal
                    # a group after its leader disappeared/reaped.
                    if identities:
                        row = subprocess.run(['/bin/ps', '-p', str(identities['supervisor']), '-o', 'pid=,pgid=,stat=,command='], capture_output=True, text=True).stdout.strip()
                        parts = row.split(None, 3)
                        if len(parts) == 4 and parts[0] == parts[1] == str(identities['supervisor']) and str(ROOT / 'builtin-plugins/funasr/supervise.py') in parts[3]:
                            os.killpg(identities['supervisor'], signal.SIGKILL)
                    if process.stdin and not process.stdin.closed:
                        process.stdin.close()
                    if process.poll() is None:
                        try:
                            process.wait(timeout=3)
                        except subprocess.TimeoutExpired:
                            process.kill(); process.wait(timeout=3)
                    process.stdout.close(); process.stderr.close()
                    evidence_dir = os.environ.get('PDRS_RESOURCE_TEST_EVIDENCE')
                    if evidence_dir:
                        evidence_path = Path(evidence_dir)
                        evidence_path.mkdir(parents=True, exist_ok=True)
                        remaining_after_cleanup = {}
                        deadline = time.monotonic() + 2
                        while time.monotonic() < deadline:
                            remaining_after_cleanup = {}
                            for pid in ([identities['supervisor'], identities['native'], descendant] if identities and descendant else []):
                                row = subprocess.run(['/bin/ps', '-p', str(pid), '-o', 'pid=,pgid=,stat=,command='], capture_output=True, text=True).stdout.strip()
                                if row:
                                    remaining_after_cleanup[pid] = row
                            if not remaining_after_cleanup:
                                break
                            time.sleep(0.02)
                        (evidence_path / f'{mode}.json').write_text(json.dumps({'mode': mode, 'mcpPid': process.pid, 'owned': identities, 'descendantPid': descendant, 'remainingAfterCleanup': remaining_after_cleanup}, indent=2) + '\n')

    @unittest.skipUnless(shutil.which('node'), 'Node is optional; external MCP test needs it')
    def test_funasr_real_stdio_mock_executable_and_path_failure(self):
        import platform
        import select
        import time
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project, home, runtime = root / 'project', root / 'home', root / 'runtime'
            project.mkdir(); home.mkdir()
            (project / 'recording.wav').write_bytes(b'local fixture')
            key = {'Darwin': 'darwin', 'Linux': 'linux'}[platform.system()] + '-' + {'arm64': 'arm64', 'aarch64': 'arm64', 'x86_64': 'x64'}[platform.machine()]
            binary_dir = runtime / 'runtime/v0.2.0' / key
            binary_dir.mkdir(parents=True)
            binary = binary_dir / 'llama-funasr-sensevoice'
            binary.write_text('#!/bin/sh\nprintf "1\\n00:00:01,000 --> 00:00:02,500\\nfixture transcript\\n"\n')
            binary.chmod(0o755)
            (runtime / 'models').mkdir()
            for name in ['sensevoice-small-q8.gguf', 'fsmn-vad.gguf']:
                (runtime / 'models' / name).write_bytes(b'model fixture')
            env = dict(os.environ, HOME=str(home), PILOT_HOME=str(home / 'pilot'))
            process = subprocess.Popen([shutil.which('node'), str(ROOT / 'builtin-plugins/funasr/funasr-local-mcp.mjs'), '--project-root', str(project), '--runtime-root', str(runtime)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
            def request(value):
                process.stdin.write(json.dumps(value) + '\n'); process.stdin.flush()
                readable, _, _ = select.select([process.stdout], [], [], 5)
                self.assertTrue(readable, 'MCP receipt deadline')
                return json.loads(process.stdout.readline())
            try:
                hello = request({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {'protocolVersion': '2024-11-05'}})
                self.assertEqual(hello['result']['protocolVersion'], '2024-11-05')
                listed = request({'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list'})
                self.assertEqual(listed['result']['tools'][0]['name'], 'transcribe_audio')
                reply = request({'jsonrpc': '2.0', 'id': 3, 'method': 'tools/call', 'params': {'name': 'transcribe_audio', 'arguments': {'audio_path': 'recording.wav'}}})
                self.assertEqual(reply['result']['structuredContent'], {'text': 'fixture transcript', 'segments': [{'start': 1, 'end': 2.5, 'text': 'fixture transcript'}]})
                rejected = request({'jsonrpc': '2.0', 'id': 4, 'method': 'tools/call', 'params': {'name': 'transcribe_audio', 'arguments': {'audio_path': '../home'}}})
                self.assertTrue(rejected['result']['isError'])
                process.stdin.close()
                self.assertEqual(process.wait(timeout=5), 0)
            finally:
                if process.poll() is None:
                    process.kill(); process.wait()
                process.stdout.close(); process.stderr.close()


if __name__ == '__main__':
    unittest.main()
