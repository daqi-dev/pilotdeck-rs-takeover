"""Locally generated OOXML and explicit fake-engine contracts; no accounts."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch
import zipfile

TOOL = Path(__file__).resolve().parents[2] / '_tools' / 'presentation_tools.py'
spec = importlib.util.spec_from_file_location('presentation_tools', TOOL)
pptx = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pptx)


def sample(path):
    from pptx import Presentation
    deck = Presentation()
    for title in ['Quarterly plan', 'Milestones']:
        slide = deck.slides.add_slide(deck.slide_layouts[1])
        slide.shapes.title.text = title
        slide.placeholders[1].text = 'Editable source text'
        slide.notes_slide.notes_text_frame.text = 'Presenter context'
    deck.save(path)
    return path


class Contracts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='pptx-native-tests-')
        self.root = Path(self.tmp.name)
        self.work = self.root / 'work'
        self.work.mkdir()
        self.source = sample(self.root / 'source.pptx')
        self.env = patch.dict(os.environ, {'HOME': str(self.root / 'home'), 'PILOT_HOME': str(self.root / 'pilot'), 'PILOTDECK_WORK_DIR': str(self.work)})
        self.env.start()

    def tearDown(self):
        self.env.stop()
        self.tmp.cleanup()

    def variant(self, name, edit):
        target = self.work / name
        with zipfile.ZipFile(self.source) as original:
            parts = {entry.filename: original.read(entry) for entry in original.infolist()}
        edit(parts)
        with zipfile.ZipFile(target, 'w') as output:
            for key, value in parts.items():
                output.writestr(key, value)
        return target

    def engine(self, name, body):
        path = self.root / name
        path.write_text('#!' + sys.executable + '\n' + body)
        path.chmod(0o700)
        return str(path)

    def test_inspects_actual_slide_order_and_notes(self):
        report, _, _ = pptx.read_package(self.source)
        self.assertEqual(report['presentation']['slideCount'], 2)
        self.assertIn('Quarterly plan', report['slides'][0]['text'])
        self.assertIn('Presenter context', report['slides'][1]['notes'])

    def test_duplicate_archive_member_is_rejected(self):
        path = self.work / 'duplicate.pptx'
        shutil.copyfile(self.source, path)
        with zipfile.ZipFile(path, 'a') as archive:
            import warnings
            with warnings.catch_warnings():
                warnings.simplefilter('ignore')
                archive.writestr('[Content_Types].xml', b'<Types/>')
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            pptx.read_package(path)

    def test_traversal_and_dtd_and_missing_relationship_are_rejected(self):
        for name, edit, message in [
            ('escape.pptx', lambda parts: parts.update({'../escape.xml': b'x'}), 'unsafe'),
            ('dtd.pptx', lambda parts: parts.update({'ppt/slides/slide1.xml': b'<!DOCTYPE x><x/>'}), 'DTD'),
            ('missing.pptx', lambda parts: parts.pop('ppt/slides/slide1.xml'), 'missing'),
        ]:
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, message):
                pptx.read_package(self.variant(name, edit))

    def test_localized_edit_preserves_other_parts_and_notes(self):
        output = self.work / 'edited.pptx'
        result = pptx.replace_text(self.source, output, 'Quarterly', 'Annual')
        self.assertEqual(result['replacements'], 1)
        before, parts_before, _ = pptx.read_package(self.source)
        after, parts_after, _ = pptx.read_package(output)
        changed = [name for name in parts_before if parts_before[name] != parts_after[name]]
        self.assertEqual(changed, ['ppt/slides/slide1.xml'])
        self.assertIn('Annual plan', after['slides'][0]['text'])
        self.assertEqual(before['slides'][0]['notes'], after['slides'][0]['notes'])
        comparison = pptx.compare(self.source, output)
        self.assertTrue(comparison['changed'])
        self.assertEqual(comparison['packageParts']['modified'], changed)

    def test_edit_requires_real_match_and_separate_internal_output(self):
        with self.assertRaisesRegex(ValueError, 'not found'):
            pptx.replace_text(self.source, self.work / 'absent.pptx', 'not in source', 'new')
        self.assertFalse((self.work / 'absent.pptx').exists())
        with self.assertRaisesRegex(ValueError, 'under'):
            pptx.replace_text(self.source, self.root / 'escape.pptx', 'Quarterly', 'new')

    def test_delivery_exclusive_bytes_and_preserved_source(self):
        candidate = self.work / 'candidate.pptx'
        shutil.copyfile(self.source, candidate)
        output = self.root / 'final.pptx'
        original = self.source.read_bytes()
        pptx.deliver(candidate, output, self.source)
        self.assertEqual(output.read_bytes(), candidate.read_bytes())
        self.assertEqual(self.source.read_bytes(), original)
        with self.assertRaises(FileExistsError):
            pptx.deliver(candidate, output, self.source)
        self.assertEqual(output.read_bytes(), original)

    def test_source_replace_requires_explicit_flag_and_recovery(self):
        candidate = self.work / 'candidate.pptx'
        pptx.replace_text(self.source, candidate, 'Quarterly', 'Annual')
        original = self.source.read_bytes()
        with self.assertRaisesRegex(ValueError, 'replace-source'):
            pptx.deliver(candidate, self.source, self.source)
        result = pptx.deliver(candidate, self.source, self.source, replace_source=True)
        self.assertTrue(result['sourceReplaced'])
        self.assertEqual(Path(result['recovery']['path']).read_bytes(), original)
        self.assertEqual(self.source.read_bytes(), candidate.read_bytes())

    def test_symlink_escape_is_rejected(self):
        outside = self.root / 'outside'
        outside.mkdir()
        (self.work / 'escape').symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'under'):
            pptx.work_path(self.work / 'escape' / 'candidate.pptx')

    def test_corrupt_candidate_cannot_publish(self):
        candidate = self.work / 'broken.pptx'
        candidate.write_bytes(b'not zip')
        with self.assertRaises(ValueError):
            pptx.deliver(candidate, self.root / 'final.pptx')
        self.assertFalse((self.root / 'final.pptx').exists())

    def test_missing_engine_errors_without_output(self):
        result = subprocess.run([sys.executable, '-B', str(TOOL), 'render', '--input', str(self.source), '--out-dir', str(self.work / 'slides'), '--soffice', str(self.root / 'missing')], env={**os.environ, 'PILOTDECK_SOFFICE': '', 'PPTX_SKILL_SOFFICE': ''}, capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn('explicit executable', result.stderr)
        self.assertFalse((self.work / 'slides').exists())

    def test_legacy_converter_must_produce_valid_package(self):
        legacy = self.root / 'source.ppt'
        legacy.write_bytes(pptx.OLE + b'generated conversion fixture')
        office = self.engine('fake-office', 'import sys\nsys.exit(0)\n')
        with self.assertRaisesRegex(ValueError, 'did not produce'):
            pptx.convert(legacy, self.work / 'converted.pptx', office)
        self.assertFalse((self.work / 'converted.pptx').exists())
        office = self.engine('fake-office-with-package', 'import pathlib,sys,shutil\na=sys.argv\no=pathlib.Path(a[a.index("--outdir")+1]);shutil.copyfile(' + repr(str(self.source)) + ',o/"converted.pptx")\n')
        before = legacy.read_bytes()
        result = pptx.convert(legacy, self.work / 'converted.pptx', office)
        self.assertEqual(result['status'], 'converted_with_warnings')
        self.assertEqual(legacy.read_bytes(), before)
        self.assertEqual(pptx.read_package(self.work / 'converted.pptx')[0]['presentation']['slideCount'], 2)

    def test_render_rejects_empty_or_wrong_page_count(self):
        office = self.engine('mock-office', 'import pathlib,sys\na=sys.argv\no=pathlib.Path(a[a.index("--outdir")+1]);(o/"source.pdf").write_bytes(b"%PDF-1.7\\nmock")\n')
        renderer = self.engine('mock-pdftoppm', 'import pathlib,sys\npathlib.Path(sys.argv[-1]+"-1.png").write_bytes(b"\\x89PNG\\r\\n\\x1a\\nfixture")\n')
        with self.assertRaisesRegex(ValueError, 'slide count'):
            pptx.render(self.source, self.work / 'slides', 144, office, renderer)
        self.assertFalse((self.work / 'slides').exists())

    def test_mock_engine_validates_real_pngs_and_never_uses_default_office_profile(self):
        office=self.engine('mock office with spaces', 'import pathlib,sys,urllib.parse\na=sys.argv\np=[v for v in a if v.startswith("-env:UserInstallation=")][0].split("=",1)[1]\nassert p.startswith("file://")\no=pathlib.Path(a[a.index("--outdir")+1]);assert pathlib.Path(urllib.parse.unquote(urllib.parse.urlsplit(p).path)).parent==o.parent\n(o/"source.pdf").write_bytes(bytes.fromhex("255044462d312e370a6d6f636b"))\n')
        renderer=self.engine('mock-pdftoppm-valid', 'import pathlib,sys\nfrom PIL import Image\nfor number in [1,2]: Image.new("RGB",(16,9),(number,0,0)).save(sys.argv[-1]+"-"+str(number)+".png")\n')
        output=self.work/'rendered'
        result=pptx.render(self.source,output,144,office,renderer)
        self.assertEqual(result['slideCount'],2)
        self.assertEqual([Path(name).name for name in result['slides']],['slide-1.png','slide-2.png'])
        with self.assertRaisesRegex(ValueError,'already exists'):
            pptx.render(self.source,output,144,office,renderer)

    def test_renderer_cannot_pass_with_png_headers_without_image_data(self):
        office=self.engine('mock-office', 'import pathlib,sys\na=sys.argv\no=pathlib.Path(a[a.index("--outdir")+1]);(o/"source.pdf").write_bytes(bytes.fromhex("255044462d312e370a6d6f636b"))\n')
        renderer=self.engine('mock-pdftoppm-bogus', 'import pathlib,sys\nfor n in [1,2]: pathlib.Path(sys.argv[-1]+"-"+str(n)+".png").write_bytes(bytes.fromhex("89504e470d0a1a0a")+b"fixture")\n')
        with self.assertRaisesRegex(ValueError,'invalid PNG'):
            pptx.render(self.source,self.work/'bogus',144,office,renderer)
        self.assertFalse((self.work/'bogus').exists())

    def test_concurrent_source_save_rejected_at_commit_and_recovery_retained(self):
        candidate=self.work/'replacement.pptx'
        shutil.copyfile(self.source,candidate)
        old=self.source.read_bytes()
        changed=old+b'CONCURRENT-NEW-SOURCE'
        real_publish=pptx.publish
        def racing_publish(data,output,*args,**kwargs):
            if Path(output).resolve()==self.source.resolve() and kwargs.get('overwrite'):
                self.source.write_bytes(changed)
            return real_publish(data,output,*args,**kwargs)
        with patch.object(pptx,'publish',racing_publish):
            with self.assertRaisesRegex(ValueError,'source changed before delivery commit'):
                pptx.deliver(candidate,self.source,self.source,replace_source=True)
        self.assertEqual(self.source.read_bytes(),changed)
        backups=list((self.work/'pptx'/'recovery').glob('*.pptx'))
        self.assertEqual(len(backups),1)
        self.assertEqual(backups[0].read_bytes(),old)

    @unittest.skipUnless(os.name=='posix','owned process groups are POSIX')
    def test_timeout_kills_pipe_descendant_after_leader_exits(self):
        started=time.monotonic()
        with self.assertRaises(subprocess.TimeoutExpired):
            pptx.run([sys.executable,'-c',"import subprocess,sys; subprocess.Popen([sys.executable,'-c','import time; time.sleep(2)'])"],timeout=.1)
        self.assertLess(time.monotonic()-started,1.5)


if __name__ == '__main__':
    unittest.main()
