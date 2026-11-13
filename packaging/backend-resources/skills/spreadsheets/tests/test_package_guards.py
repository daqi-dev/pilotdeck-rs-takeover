"""Pure package/race/error probes; fake render validates guards, not Office output."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace as NS
import unittest
from unittest.mock import patch
from openpyxl import Workbook

p = Path(__file__).resolve().parents[2] / '_tools' / 'spreadsheet_tools.py'
s = importlib.util.spec_from_file_location('spreadsheet_guards', p)
m = importlib.util.module_from_spec(s); s.loader.exec_module(m)

class Guards(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='spreadsheet-guards-')
        self.root = Path(self.tmp.name)
        self.source = self.root/'source.xlsx'
        book = Workbook(); book.active['A1'] = '=1+1'; book.save(self.source)
    def tearDown(self):
        self.tmp.cleanup()
    def test_candidate_bytes_parsed_are_exactly_the_bytes_delivered(self):
        expected = self.source.read_bytes()
        original = m.package
        def swap(path, raw=None):
            result = original(path, raw)
            Path(path).write_bytes(b'NOT A ZIP FILE')
            return result
        out = self.root/'delivered.xlsx'
        with patch.object(m,'package',swap):
            m.deliver(NS(input=self.source,out=out,source=None,replace_source=False))
        self.assertEqual(out.read_bytes(), expected)
        m.package(out)
    def test_unknown_part_cannot_be_lost_in_unrelated_edit(self):
        parts = m.package(self.source)
        parts['vendor/preserved.bin'] = b'USER-OWNED-OPAQUE-DATA'
        parts['[Content_Types].xml'] = parts['[Content_Types].xml'].replace(b'</Types>',b'<Default Extension="bin" ContentType="application/octet-stream"/></Types>')
        self.source.write_bytes(m.archive_bytes(parts))
        spec = self.root/'edit.json'; spec.write_text(json.dumps({'sheets':[{'name':'Sheet','cells':{'B1':{'value':'change'}}}]}))
        out = self.root/'edited.xlsx'
        with self.assertRaisesRegex(ValueError,'unknown package part'):
            m.edit(NS(input=self.source,out=out,spec=spec))
        self.assertFalse(out.exists())
        self.assertEqual(m.package(self.source)['vendor/preserved.bin'],b'USER-OWNED-OPAQUE-DATA')
    def test_detected_source_save_is_not_overwritten(self):
        candidate = self.root/'candidate.xlsx'; candidate.write_bytes(self.source.read_bytes())
        original = self.source.read_bytes(); changed = original + b'CONCURRENT-SAVE'
        publish = m.publish
        def racing_publish(data, output, *args, **kwargs):
            if Path(output).resolve() == self.source.resolve():
                self.source.write_bytes(changed)
            return publish(data, output, *args, **kwargs)
        with patch.dict(os.environ, {'PILOTDECK_WORK_DIR':str(self.root/'work')}), patch.object(m,'publish',racing_publish):
            with self.assertRaisesRegex(ValueError,'source changed before commit'):
                m.deliver(NS(input=candidate,out=self.source,source=self.source,replace_source=True))
        self.assertEqual(self.source.read_bytes(),changed)
        self.assertEqual(next((self.root/'work/spreadsheets/recovery').glob('*.bak')).read_bytes(),original)
    def fake_office(self, args, options, profile):
        directory = Path(args[args.index('--outdir')+1]); (directory/'source.pdf').write_bytes(b'%PDF-FAKE-ONLY-GUARD-TEST')
        return {'fake':True}
    def test_png_signature_without_real_image_is_rejected(self):
        def renderer(command, timeout):
            Path(command[-1]+'-1.png').write_bytes(b'\x89PNG\r\n\x1a\nX')
            return {'fake':True}
        out = self.root/'render'
        with patch.object(m,'engine',self.fake_office), patch.object(m,'run_owned',renderer), patch.object(m,'executable',lambda *_:'/fake/pdftoppm'):
            with self.assertRaises(OSError):
                m.render(NS(input=self.source,out_dir=out,pdf_renderer='/usr/bin/true',timeout=1))
        self.assertFalse(out.exists())
    def test_real_png_pages_are_naturally_ordered(self):
        from PIL import Image
        def renderer(command, timeout):
            for index in range(1,12):
                Image.new('RGB',(2,2),'white').save(command[-1]+f'-{index}.png')
            return {'fake':True}
        out = self.root/'render'
        # Only executable resolution is replaced: actual PIL verify/load runs.
        with patch.object(m,'engine',self.fake_office), patch.object(m,'run_owned',renderer), patch.object(m,'executable',lambda *_:'/fake/pdftoppm'):
            report=m.render(NS(input=self.source,out_dir=out,pdf_renderer='/fake/pdftoppm',timeout=1))
        self.assertEqual(report['pages'],[f'page-{i}.png' for i in range(1,12)])

if __name__ == '__main__': unittest.main()
