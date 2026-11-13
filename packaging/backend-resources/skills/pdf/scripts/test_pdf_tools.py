#!/usr/bin/env python3
"""Real PDF structural contracts; render smoke requires explicit local renderer."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from pypdf import PdfReader, PdfWriter
from pypdf.generic import (ArrayObject, DictionaryObject, FloatObject, NameObject, NumberObject, TextStringObject)

HELPER = Path(__file__).resolve().parents[2] / '_tools/pdf_tools.py'
spec = importlib.util.spec_from_file_location('pdf_tools', HELPER)
tools = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tools)


def make_form(path):
    writer = PdfWriter()
    page = writer.add_blank_page(width=400, height=300)
    font = writer._add_object(DictionaryObject({NameObject('/Type'): NameObject('/Font'), NameObject('/Subtype'): NameObject('/Type1'), NameObject('/BaseFont'): NameObject('/Helvetica')}))
    resources = DictionaryObject({NameObject('/Font'): DictionaryObject({NameObject('/Helv'): font})})
    widget = DictionaryObject({NameObject('/Type'): NameObject('/Annot'), NameObject('/Subtype'): NameObject('/Widget'), NameObject('/FT'): NameObject('/Tx'), NameObject('/T'): TextStringObject('full_name'), NameObject('/V'): TextStringObject('Before'), NameObject('/Rect'): ArrayObject([FloatObject(v) for v in (40, 160, 330, 195)]), NameObject('/DA'): TextStringObject('/Helv 16 Tf 0 g'), NameObject('/F'): NumberObject(4)})
    ref = writer._add_object(widget)
    page[NameObject('/Annots')] = ArrayObject([ref])
    writer._root_object[NameObject('/AcroForm')] = writer._add_object(DictionaryObject({NameObject('/Fields'): ArrayObject([ref]), NameObject('/DR'): resources, NameObject('/DA'): TextStringObject('/Helv 16 Tf 0 g')}))
    writer.update_page_form_field_values(page, {'full_name': 'Before'}, auto_regenerate=False)
    tools.save(writer, path)


class Workflow(unittest.TestCase):
    def test_atomic_build_and_actual_png_format_guards(self):
        from PIL import Image
        with tempfile.TemporaryDirectory(prefix='pdf-publication-guards-') as directory:
            root = Path(directory)
            source = root/'source.pdf'
            writer = PdfWriter(); writer.add_blank_page(width=200,height=200)
            tools.save(writer,source)
            builder = root/'builder.py'
            builder.write_text('import argparse,shutil\np=argparse.ArgumentParser();p.add_argument("--out");a=p.parse_args();shutil.copyfile('+repr(str(source))+',a.out)\n')
            final = root/'final.pdf'
            real_run = tools.subprocess.run
            def competing_save(command,*args,**kwargs):
                if str(builder.resolve()) in command:
                    self.assertNotEqual(Path(command[-1]).resolve(),final.resolve())
                    final.write_bytes(b'CONCURRENT-USER-SAVE')
                return real_run(command,*args,**kwargs)
            with patch.object(tools.subprocess,'run',competing_save):
                with self.assertRaises(FileExistsError):
                    tools.main(['build','--builder',str(builder),'--out',str(final)])
            self.assertEqual(final.read_bytes(),b'CONCURRENT-USER-SAVE')
            bad = root/'bad.py'
            bad.write_text('import argparse\nfrom pathlib import Path\np=argparse.ArgumentParser();p.add_argument("--out");a=p.parse_args();Path(a.out).write_bytes(b"PARTIAL");raise SystemExit(1)\n')
            bad_final = root/'partial.pdf'
            with self.assertRaisesRegex(ValueError,'builder failed'):
                tools.main(['build','--builder',str(bad),'--out',str(bad_final)])
            self.assertFalse(bad_final.exists())
            def fake_renderer(command,*args,**kwargs):
                target = command[command.index('-o')+1].replace('%d','1')
                Image.new('RGB',(5,5),'white').save(target,format='JPEG')
                return subprocess.CompletedProcess(command,0,'','')
            with patch.object(tools,'renderer',lambda:('/fake/mutool','mutool')), patch.object(tools.subprocess,'run',fake_renderer):
                with self.assertRaisesRegex(ValueError,'not PNG'):
                    tools.main(['render','--input',str(source),'--out-dir',str(root/'bad-render'),'--dpi','72'])

    def test_real_pdf_contracts(self):
        with tempfile.TemporaryDirectory(prefix='pilotdeck-pdf-test-') as work:
            root = Path(work)
            def run(*args):
                return tools.main(list(map(str, args)))
            builder, sample = root/'builder.py', root/'sample.pdf'
            run('scaffold', '--out', builder)
            run('build', '--builder', builder, '--out', sample)
            self.assertIn('PilotDeck PDF', PdfReader(sample).pages[0].extract_text())
            # Exercise flowable pagination through the same editable builder.
            long_builder = root/'long_builder.py'
            long_builder.write_text(builder.read_text().replace("    story = [Paragraph('PilotDeck PDF', title)]", "    sections = sections * 10\n    story = [Paragraph('PilotDeck PDF', title)]"))
            long_pdf = root/'long.pdf'
            run('build', '--builder', long_builder, '--out', long_pdf)
            long_document = PdfReader(long_pdf)
            self.assertGreater(len(long_document.pages), 1)
            for number, page in enumerate(long_document.pages, 1):
                self.assertIn(f'Page {number}', page.extract_text())
                self.assertNotIn(page.extract_text().strip().splitlines()[-1], ('Repeatable generation', 'Verification'))
            run('inspect', '--input', sample, '--out', root/'inspect.json', '--text-out', root/'text.json', '--tables-out', root/'tables.json')
            self.assertEqual(json.loads((root/'inspect.json').read_text())['pageCount'], 1)
            merged = root/'merged.pdf'
            run('merge', '--inputs', sample, sample, '--out', merged)
            self.assertEqual(len(PdfReader(merged).pages), 2)
            run('split', '--input', merged, '--pages', '2,1-2', '--out-dir', root/'pages')
            self.assertEqual(len(list((root/'pages').glob('*.pdf'))), 2)
            rotated = root/'rotated.pdf'
            run('rotate', '--input', merged, '--pages', '2', '--degrees', '90', '--out', rotated)
            self.assertEqual([p.rotation for p in PdfReader(rotated).pages], [0, 90])
            run('audit', '--input', rotated, '--out', root/'audit.json')
            form, filled = root/'form.pdf', root/'filled.pdf'
            make_form(form)
            run('forms-inspect', '--input', form, '--out', root/'fields.json')
            run('forms-fill', '--input', form, '--values', '{"full_name":"Ada Lovelace"}', '--out', filled)
            document = PdfReader(filled)
            self.assertEqual(document.get_fields()['full_name']['/V'], 'Ada Lovelace')
            self.assertEqual(document.pages[0]['/Annots'][0].get_object()['/V'], 'Ada Lovelace')
            self.assertIn('/AP', document.pages[0]['/Annots'][0].get_object())
            self.assertEqual(PdfReader(form).get_fields()['full_name']['/V'], 'Before')
            with self.assertRaises(ValueError):
                run('forms-fill', '--input', form, '--values', '{"missing":"X"}', '--out', root/'bad.pdf')
            self.assertFalse((root/'bad.pdf').exists())
            with self.assertRaises(ValueError):
                run('rotate', '--input', sample, '--degrees', '45', '--out', root/'bad.pdf')
            with self.assertRaises(ValueError):
                run('merge', '--inputs', sample, '--out', sample)
            if os.environ.get('PILOTDECK_PDF_RENDERER'):
                for name, pdf in [('rotated', rotated), ('filled', filled)]:
                    result = run('render', '--input', pdf, '--out-dir', root/name, '--dpi', '72', '--montage', root/(name+'.png'))
                    self.assertEqual(len(result['pages']), len(PdfReader(pdf).pages))


if __name__ == '__main__':
    unittest.main()
