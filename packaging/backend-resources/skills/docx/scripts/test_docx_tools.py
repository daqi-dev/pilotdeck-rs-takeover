#!/usr/bin/env python3
"""Generated documents only; no account, downloads, or user documents."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import signal
import subprocess
import sys
import time
import unittest
import zipfile

MODULE = Path(os.environ.get('DOCX_TEST_MODULE', str(Path(__file__).resolve().parents[2] / '_tools' / 'docx_tools.py')))
spec = importlib.util.spec_from_file_location('docx_tools', MODULE)
docx = importlib.util.module_from_spec(spec)
spec.loader.exec_module(docx)

def fixture(path):
    parts = {
        '[Content_Types].xml': ('<Types xmlns="' + docx.CT + '"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="bin" ContentType="application/octet-stream"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>').encode(),
        '_rels/.rels': ('<Relationships xmlns="' + docx.REL + '"><Relationship Id="rId1" Type="' + docx.OFF + 'officeDocument" Target="word/document.xml"/></Relationships>').encode(),
        'word/document.xml': ('<w:document xmlns:w="' + docx.W + '"><w:body><w:p w:rsidR="123"><w:r><w:rPr><w:b/></w:rPr><w:t>Hello old world</w:t></w:r></w:p><w:sectPr/></w:body></w:document>').encode(),
        'word/settings.xml': ('<w:settings xmlns:w="' + docx.W + '"/>').encode(),
        'docProps/core.xml': b'<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:creator>Private Author</dc:creator></cp:coreProperties>',
        'word/media/opaque.bin': b'untouched-binary\x00\xff',
    }
    with zipfile.ZipFile(path, 'w') as archive:
        for name, value in parts.items():
            archive.writestr(name, value)
    return parts

class DocxToolsTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.source = self.root / 'source.docx'
        self.original = fixture(self.source)
        self.source_bytes = self.source.read_bytes()

    def annotated(self):
        parts = docx.load(self.source)
        docx.annotate(parts, {'comments': [{'match': 'old', 'text': 'Review it'}], 'tracked_replacements': [{'match': 'old', 'replacement': 'new'}]})
        return parts

    def test_annotate_accept_reject_roundtrip(self):
        parts = self.annotated()
        view = docx.inspect(parts)
        self.assertEqual(view['tracked_changes']['insertions'], 1)
        self.assertEqual(view['tracked_changes']['deletions'], 1)
        self.assertEqual(view['comments'][0]['text'], 'Review it')
        self.assertEqual(docx.visible_text(docx.xml(parts['word/document.xml'])), 'Hello new world')
        self.assertEqual(parts['word/media/opaque.bin'], self.original['word/media/opaque.bin'])
        output = self.root / 'review.docx'
        docx.save(parts, self.source, output)
        self.assertEqual(docx.load(output)['word/media/opaque.bin'], self.original['word/media/opaque.bin'])
        accepted = docx.load(output)
        docx.finalize(accepted, accept=True, comments=True)
        self.assertEqual(docx.inspect(accepted)['paragraphs'][0]['text'], 'Hello new world')
        self.assertEqual(docx.inspect(accepted)['tracked_changes']['insertions'], 0)
        self.assertNotIn('word/comments.xml', accepted)
        self.assertNotIn(b'comments', accepted['word/_rels/document.xml.rels'])
        self.assertIn(b'<w:b', accepted['word/document.xml'])
        rejected = docx.load(output)
        docx.finalize(rejected, reject=True)
        self.assertEqual(docx.inspect(rejected)['paragraphs'][0]['text'], 'Hello old world')
        self.assertEqual(docx.inspect(rejected)['tracked_changes']['deletions'], 0)
        self.assertEqual(self.source.read_bytes(), self.source_bytes)

    def test_sanitize_removes_metadata_without_accepting_revisions(self):
        parts = self.annotated()
        docx.sanitize(parts)
        self.assertNotIn(b'Private Author', parts['docProps/core.xml'])
        self.assertNotIn(b'rsid', parts['word/document.xml'])
        self.assertEqual(docx.inspect(parts)['tracked_changes']['insertions'], 1)
        self.assertEqual(docx.inspect(parts)['comments'][0]['author'], 'PilotDeck')

    def test_no_clobber_and_source_protected(self):
        with self.assertRaises(docx.DocxError):
            docx.save(self.original, self.source, self.source, True)
        dest = self.root / 'keep.docx'
        dest.write_bytes(b'keep')
        with self.assertRaises(docx.DocxError):
            docx.save(self.original, self.source, dest)
        self.assertEqual(dest.read_bytes(), b'keep')

    def test_bad_zip_duplicate_traversal_entities(self):
        for name, value in [('../escape', b'x'), ('word/evil.xml', b'<!DOCTYPE x><x/>')]:
            dest = self.root / 'bad.docx'
            with zipfile.ZipFile(dest, 'w') as archive:
                for key, data in self.original.items():
                    archive.writestr(key, data)
                archive.writestr(name, value)
            with self.assertRaises(docx.DocxError):
                docx.load(dest)

    def test_ambiguous_not_found_cross_run_and_invalid_spec(self):
        for spec in [{'comments': [{'match': 'missing', 'text': 'x'}]}, {'tracked_replacements': [{'match': 'old', 'replacement': 'x', 'occurrence': 0}]}, {'unknown': []}]:
            with self.assertRaises(docx.DocxError):
                docx.annotate(dict(self.original), spec)
        parts = dict(self.original)
        parts['word/document.xml'] = parts['word/document.xml'].replace(b'Hello old world', b'old old')
        with self.assertRaises(docx.DocxError):
            docx.annotate(parts, {'tracked_replacements': [{'match': 'old', 'replacement': 'x'}]})
        parts = dict(self.original)
        parts['word/document.xml'] = parts['word/document.xml'].replace(b'Hello old world', b'Hello ol').replace(b'</w:r></w:p>', b'</w:r><w:r><w:t>d world</w:t></w:r></w:p>')
        with self.assertRaisesRegex(docx.DocxError, 'Cross-run'):
            docx.annotate(parts, {'tracked_replacements': [{'match': 'old', 'replacement': 'x'}]})

    def test_complex_revisions_rejected(self):
        parts = dict(self.original)
        parts['word/document.xml'] = parts['word/document.xml'].replace(b'<w:sectPr/>', b'<w:pPrChange/>')
        with self.assertRaises(docx.DocxError):
            docx.finalize(parts, accept=True)

    def test_custom_metadata_relationships_are_removed(self):
        parts = dict(self.original)
        parts['docProps/custom.xml'] = b'<Properties><private>secret</private></Properties>'
        root = docx.xml(parts['_rels/.rels'])
        docx.ET.SubElement(root, '{' + docx.REL + '}Relationship', {'Id': 'rId9', 'Type': docx.OFF + 'custom-properties', 'Target': 'docProps/custom.xml'})
        parts['_rels/.rels'] = docx.encode(root)
        types = docx.xml(parts['[Content_Types].xml'])
        docx.ET.SubElement(types, '{' + docx.CT + '}Override', {'PartName': '/docProps/custom.xml', 'ContentType': 'application/xml'})
        parts['[Content_Types].xml'] = docx.encode(types)
        docx.sanitize(parts)
        self.assertNotIn('docProps/custom.xml', parts)
        self.assertNotIn(b'custom.xml', parts['_rels/.rels'])
        self.assertNotIn(b'custom.xml', parts['[Content_Types].xml'])

    def test_compare_and_deliver_preserve_candidate_bytes(self):
        candidate = self.root / 'candidate.docx'
        docx.save(self.annotated(), self.source, candidate)
        target = self.root / 'delivery.docx'
        args = docx.parser().parse_args(['deliver', '--input', str(candidate), '--out', str(target)])
        docx.execute(args)
        self.assertEqual(target.read_bytes(), candidate.read_bytes())
        report = self.root / 'compare.json'
        args = docx.parser().parse_args(['compare', '--before', str(self.source), '--after', str(candidate), '--out', str(report)])
        result = docx.execute(args)
        self.assertEqual(result['paragraph_count_before'], 1)
        self.assertTrue(result['diff'])
        self.assertTrue(report.exists())

    def test_namespace_qname_declarations_and_xml_comments_survive(self):
        parts = dict(self.original)
        parts['word/document.xml'] = parts['word/document.xml'].replace(b'<w:document ', b'<w:document xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml" mc:Ignorable="w14" ').replace(b'<w:body>', b'<w:body><!--keep-comment-->')
        docx.annotate(parts, {'tracked_replacements': [{'match': 'old', 'replacement': 'new'}]})
        self.assertIn(b'xmlns:w14=', parts['word/document.xml'])
        self.assertIn(b'Ignorable="w14"', parts['word/document.xml'])
        self.assertIn(b'<!--keep-comment-->', parts['word/document.xml'])
        target = self.root / 'namespaced.docx'
        docx.save(parts, self.source, target)
        docx.load(target)

    def test_source_replacement_requires_explicit_mode_and_keeps_backup(self):
        candidate = self.root / 'work' / 'candidate.docx'
        candidate.parent.mkdir()
        docx.save(self.annotated(), self.source, candidate)
        prior = os.environ.get('PILOTDECK_WORK_DIR')
        os.environ['PILOTDECK_WORK_DIR'] = str(candidate.parent)
        try:
            argv = ['deliver', '--input', str(candidate), '--source', str(self.source), '--out', str(self.source)]
            with self.assertRaises(docx.DocxError):
                docx.execute(docx.parser().parse_args(argv))
            result = docx.execute(docx.parser().parse_args(argv + ['--replace-source']))
            self.assertEqual(Path(result['source_backup']['path']).read_bytes(), self.source_bytes)
            self.assertEqual(self.source.read_bytes(), candidate.read_bytes())
        finally:
            if prior is None:
                os.environ.pop('PILOTDECK_WORK_DIR', None)
            else:
                os.environ['PILOTDECK_WORK_DIR'] = prior

    def test_delivery_detects_source_change_during_atomic_publication(self):
        candidate = self.root / 'candidate.docx'
        docx.save(self.annotated(), self.source, candidate)
        target = self.root / 'delivery.docx'
        args = docx.parser().parse_args(['deliver', '--input', str(candidate), '--source', str(self.source), '--out', str(target)])
        real = docx.atomic_bytes
        def concurrent_change(output, data, overwrite=False):
            real(output, data, overwrite)
            if output == target:
                self.source.write_bytes(b'concurrent-source-change')
        docx.atomic_bytes = concurrent_change
        try:
            with self.assertRaisesRegex(docx.DocxError, 'changed during delivery') as caught:
                docx.execute(args)
            self.assertEqual(caught.exception.code, 'source-changed-during-delivery')
            self.assertEqual(target.read_bytes(), candidate.read_bytes())
        finally:
            docx.atomic_bytes = real

    def test_cjk_paint_threshold_rejects_blank_text_but_accepts_painted(self):
        import struct
        import zlib
        def image(path, pixel):
            def chunk(kind, value):
                return struct.pack('>I', len(value)) + kind + value + struct.pack('>I', zlib.crc32(kind + value) & 0xffffffff)
            raster = b''.join(b'\x00' + bytes(pixel) * 20 for _ in range(20))
            path.write_bytes(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 20, 20, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(raster)) + chunk(b'IEND', b''))
        structured = self.root / 'text.xml'
        page = docx.ET.Element('page')
        line = docx.ET.SubElement(page, 'line', {'bbox': '0 0 20 20'})
        for index in range(10):
            x = index * 2
            docx.ET.SubElement(line, 'char', {'c': '文', 'quad': f'{x} 0 {x+2} 0 {x} 20 {x+2} 20'})
        document = docx.ET.Element('document'); document.append(page)
        structured.write_bytes(docx.encode(document))
        blank, painted = self.root / 'blank.png', self.root / 'painted.png'
        image(blank, (255, 255, 255)); image(painted, (0, 0, 0))
        with self.assertRaises(docx.DocxError) as caught:
            docx.check_cjk_pages(structured, [blank], 72)
        self.assertEqual(caught.exception.code, 'render-cjk-glyphs-missing')
        coverage, text = docx.check_cjk_pages(structured, [painted], 72)
        self.assertEqual(coverage, [{'characters': 10, 'visible_characters': 10, 'ratio': 1.0, 'page': 1}])
        self.assertEqual(text, [{'page': 1, 'characters': 11}])
        # A zero-height missing glyph must not disappear from the denominator.
        for char in line:
            char.set('quad', '0 10 2 10 0 10 2 10')
        self.assertEqual(docx.page_cjk_coverage(page, blank, 1)['characters'], 10)

    def test_complete_inspection_fields_and_filter_comparison_contract(self):
        parts = dict(self.original)
        document = docx.xml(parts['word/document.xml'])
        body = document.find(docx.q('body'))
        p = body.find(docx.q('p'))
        docx.ET.SubElement(p.find(docx.q('r')), docx.q('noBreakHyphen'))
        docx.ET.SubElement(p.find(docx.q('r')), docx.q('ptab'))
        properties = docx.ET.SubElement(p, docx.q('pPr'))
        docx.ET.SubElement(properties, docx.q('pStyle'), {docx.q('val'): 'Heading1'})
        docx.ET.SubElement(properties, docx.q('jc'), {docx.q('val'): 'center'})
        docx.ET.SubElement(p, docx.q('fldSimple'), {docx.q('instr'): ' PAGE  '})
        parts['word/document.xml'] = docx.encode(document)
        parts['word/styles.xml'] = ('<w:styles xmlns:w="' + docx.W + '"><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style></w:styles>').encode()
        result = docx.inspect(parts, self.source)
        self.assertEqual(set(result), {'status', 'input', 'metadata', 'paragraphs', 'headings', 'tables', 'sections', 'paragraph_count', 'table_count', 'validation', 'comments', 'tracked_changes', 'fields', 'image_parts', 'external_relationships', 'package_features', 'inspection_coverage'})
        self.assertEqual(result['headings'][0]['style'], 'Heading 1')
        self.assertEqual(result['paragraphs'][0]['alignment'], 'CENTER (1)')
        self.assertTrue(result['paragraphs'][0]['text'].endswith('-\t'))
        self.assertEqual(result['paragraphs'][0]['runs'][0]['bold'], True)
        self.assertEqual(result['fields'], [{'part': 'word/document.xml', 'instruction': 'PAGE', 'form': 'simple'}])
        args = docx.parser().parse_args(['inspect', '--input', str(self.source), '--search', 'hello', '--location', 'body', '--max-items', '1'])
        selected = docx.filter_inspection(result, args)
        self.assertEqual(selected['query'], {'search': 'hello', 'location': 'body', 'total_matches': 1, 'returned': 1, 'truncated': False})
        self.assertNotIn('tables', selected)
        comparison = docx.compare(docx.inspect(self.original), result, 'before.docx', 'after.docx')
        self.assertEqual(comparison['fields_added'], [{'part': 'word/document.xml', 'instruction': 'PAGE', 'form': 'simple', 'count': 1}])
        self.assertEqual(comparison['heading_count_after'], 1)

    def test_mutation_respects_active_content_and_effective_protection(self):
        for data in [('word/activeX/control.xml', b'<control/>'), ('word/signatures.xml', b'<signature/>')]:
            parts = dict(self.original); parts[data[0]] = data[1]
            with self.assertRaises(docx.DocxError):
                docx.sanitize(parts)
        parts = dict(self.original)
        parts['word/settings.xml'] = ('<w:settings xmlns:w="' + docx.W + '"><w:documentProtection w:enforcement="1"/></w:settings>').encode()
        with self.assertRaises(docx.DocxError):
            docx.annotate(parts, {})
        parts['word/settings.xml'] = parts['word/settings.xml'].replace(b'="1"', b'="0"')
        docx.annotate(parts, {})
        self.assertEqual(docx.inspect(parts)['package_features']['document_protection'], {})

    def test_render_report_revision_layout_and_line_count_with_injected_engine(self):
        import hashlib
        import struct
        import zlib
        args = docx.parser().parse_args(['render', '--input', str(self.source), '--out-dir', str(self.root / 'render-root'), '--soffice', sys.executable, '--pdf-renderer', sys.executable, '--mutool', sys.executable])
        def chunk(kind, value):
            return struct.pack('>I', len(value)) + kind + value + struct.pack('>I', zlib.crc32(kind + value) & 0xffffffff)
        png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 1, 1, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(b'\x00\xff\xff\xff')) + chunk(b'IEND', b'')
        commands = []
        def engine(argv, timeout, cwd, env=None):
            commands.append(argv)
            self.assertNotEqual(env['HOME'], os.environ.get('HOME'))
            if '--convert-to' in argv:
                self.source.write_bytes(b'CONCURRENT-SOURCE-SAVE')
                self.assertNotEqual(Path(argv[-1]).resolve(), self.source.resolve())
                self.assertEqual(Path(argv[-1]).read_bytes(), self.source_bytes)
                (Path(cwd) / 'source.pdf').write_bytes(b'%PDF-FAKE-UNIT-TRANSPORT')
            elif '-png' in argv:
                Path(argv[-1] + '-01.png').write_bytes(png)
            else:
                Path(argv[argv.index('-o') + 1]).write_text('<document><page><line bbox="0 0 1 1"><font><char c="A"/></font></line><line bbox="0 0 1 1"><font><char c="B"/></font></line></page></document>')
        real = docx.owned_command; docx.owned_command = engine
        try:
            result = docx.render(args)
        finally:
            docx.owned_command = real
        revision = hashlib.sha256(self.source_bytes).hexdigest()
        expected_dir = (self.root / 'render-root' / revision[:16]).resolve()
        self.assertEqual(set(result), {'status', 'input', 'revision', 'render_root', 'out_dir', 'pages', 'images', 'page_text', 'cjk_glyph_coverage', 'text_characters', 'dpi', 'libreoffice_profile_seed'})
        self.assertEqual(result['out_dir'], str(expected_dir))
        self.assertEqual(result['images'], [str(expected_dir / 'page-1.png')])
        self.assertEqual(result['page_text'], [{'page': 1, 'characters': 4}])
        self.assertEqual(result['text_characters'], 4)
        self.assertEqual(len(commands), 3)
        self.assertEqual(self.source.read_bytes(), b'CONCURRENT-SOURCE-SAVE')

    def test_source_replacement_rejects_concurrent_save_after_backup(self):
        from unittest.mock import patch
        work = self.root / 'work'; work.mkdir()
        candidate = work / 'candidate.docx'
        docx.save(self.annotated(), self.source, candidate)
        args = docx.parser().parse_args(['deliver', '--input', str(candidate), '--source', str(self.source), '--out', str(self.source), '--replace-source'])
        actual_write = docx.atomic_bytes
        new_source = b'NEW-SOURCE-THAT-MUST-SURVIVE'
        def concurrent_save(output, data, overwrite=False, precommit=None):
            if output.resolve() == self.source.resolve():
                self.assertEqual(len(list((work / 'docx' / 'recovery').glob('*.docx'))), 1)
                self.source.write_bytes(new_source)
            actual_write(output, data, overwrite, precommit)
        with patch.dict(os.environ, {'PILOTDECK_WORK_DIR': str(work)}), patch.object(docx, 'atomic_bytes', concurrent_save):
            with self.assertRaisesRegex(docx.DocxError, 'new source preserved'):
                docx.execute(args)
        self.assertEqual(self.source.read_bytes(), new_source)
        backups = list((work / 'docx' / 'recovery').glob('*.docx'))
        self.assertEqual(backups[0].read_bytes(), self.source_bytes)

    def test_render_missing_engine_is_not_success(self):
        args = docx.parser().parse_args(['render', '--input', str(self.source), '--out-dir', str(self.root / 'pages'), '--soffice', '/nonexistent', '--pdf-renderer', '/nonexistent', '--mutool', '/nonexistent'])
        with self.assertRaisesRegex(docx.DocxError, 'Supply executable'):
            docx.execute(args)
        self.assertEqual(list((self.root / 'pages').rglob('page-*.png')), [])

    @unittest.skipUnless(os.name == 'posix', 'POSIX process-group ownership')
    def test_cancel_reaps_renderer_before_receipt(self):
        script = self.root / 'held-renderer'
        script.write_text('#!/bin/sh\necho $$ > "$DOCX_TEST_PID"\nexec /bin/sleep 300\n')
        script.chmod(0o700)
        pidfile = self.root / 'pid'
        env = dict(os.environ, DOCX_TEST_PID=str(pidfile), HOME=str(self.root), PILOT_HOME=str(self.root / 'pilot'))
        process = subprocess.Popen([sys.executable, str(MODULE), 'render', '--input', str(self.source), '--out-dir', str(self.root / 'pages'), '--soffice', str(script), '--pdf-renderer', str(script), '--mutool', str(script)], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        renderer = None
        try:
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                if pidfile.exists() and pidfile.read_text().strip().isdigit():
                    renderer = int(pidfile.read_text().strip())
                    break
                time.sleep(0.01)
            self.assertIsNotNone(renderer)
            process.send_signal(signal.SIGTERM)
            _, stderr = process.communicate(timeout=5)
            self.assertEqual(process.returncode, 3, stderr)
            self.assertIn(b'renderer-cancelled', stderr)
            with self.assertRaises(ProcessLookupError):
                os.kill(renderer, 0)
            self.assertEqual(list((self.root / 'pages').rglob('page-*.png')), [])
        finally:
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=5)
            if renderer is not None:
                try:
                    os.kill(renderer, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_real_failed_renderer_does_not_publish_pages(self):
        script = self.root / 'renderer'
        script.write_text('#!/bin/sh\necho deliberate-renderer-failure >&2\nexit 19\n')
        script.chmod(0o700)
        args = docx.parser().parse_args(['render', '--input', str(self.source), '--out-dir', str(self.root / 'pages'), '--soffice', str(script), '--pdf-renderer', str(script), '--mutool', str(script)])
        with self.assertRaisesRegex(docx.DocxError, 'deliberate-renderer-failure'):
            docx.execute(args)
        self.assertEqual(list((self.root / 'pages').rglob('page-*.png')), [])

def reference_oracle(reference_root):
    """Explicit opt-in oracle; baseline files remain read-only, including bytecode."""
    sys.dont_write_bytecode = True
    from unittest.mock import patch
    from docx import Document
    from docx.shared import Pt
    from docx.enum.section import WD_SECTION_START
    from docx.enum.text import WD_ALIGN_PARAGRAPH, WD_UNDERLINE
    from docx.oxml import OxmlElement
    from docx.oxml.ns import qn
    sys.path.insert(0, str(Path(reference_root).resolve() / 'skills' / 'docx' / 'scripts'))
    from docxlib.core import inspect_docx, filter_inspection, compare_docx
    from docxlib.delivery import deliver_docx
    with tempfile.TemporaryDirectory(prefix='docx-reference-') as temporary, patch.dict(os.environ, {'HOME': temporary, 'PILOT_HOME': str(Path(temporary) / 'pilot'), 'PILOTDECK_WORK_DIR': temporary}):
        root = Path(temporary).resolve()
        source = root / 'rich.docx'
        document = Document()
        document.core_properties.title = 'Independent report'
        document.core_properties.author = 'Generated author'
        paragraph = document.add_heading('Heading', 1)
        paragraph.alignment = WD_ALIGN_PARAGRAPH.CENTER
        paragraph = document.add_paragraph('Body ')
        run = paragraph.add_run('styled')
        run.bold, run.italic, run.underline = True, False, WD_UNDERLINE.DOUBLE
        run.font.name, run.font.size = 'Arial', Pt(13)
        special = paragraph.add_run()
        special._r.append(OxmlElement('w:noBreakHyphen'))
        special._r.append(OxmlElement('w:ptab'))
        table = document.add_table(rows=2, cols=2)
        table.style = 'Table Grid'
        for row in range(2):
            for column in range(2):
                table.cell(row, column).text = f'Cell {row}/{column}'
        table.cell(1, 0).merge(table.cell(1, 1))
        table.cell(0, 0).add_table(rows=1, cols=1).cell(0, 0).text = 'Nested'
        document.sections[0].header.paragraphs[0].text = 'Header'
        document.sections[0].footer.paragraphs[0].text = 'Footer'
        document.sections[0].header.add_table(rows=1, cols=1, width=Pt(200)).cell(0, 0).text = 'Header table'
        document.add_section(WD_SECTION_START.NEW_PAGE)
        field = OxmlElement('w:fldSimple')
        field.set(qn('w:instr'), ' DATE  ')
        document.paragraphs[0]._p.append(field)
        document.save(source)
        baseline = inspect_docx(source)
        actual = docx.inspect(docx.load(source), source)
        assert json.loads(json.dumps(baseline)) == actual, 'rich inspection differs'
        count = 1
        for summary, search, location, limit in [(True, None, None, 200), (False, 'Nested', None, 1), (False, None, 'section1.header', 200), (False, None, None, 2)]:
            args = docx.parser().parse_args(['inspect', '--input', str(source), '--max-items', str(limit)] + (['--summary'] if summary else []) + (['--search', search] if search else []) + (['--location', location] if location else []))
            expected = filter_inspection(baseline, summary=summary, search=search, location=location, max_items=limit)
            assert json.loads(json.dumps(expected)) == docx.execute(args), 'inspection filter differs'
            count += 1
        parts = docx.load(source)
        docx.annotate(parts, {'comments': [{'match': 'Body', 'text': 'Review', 'date': '2026-11-13T00:00:00Z'}], 'tracked_replacements': [{'match': 'styled', 'replacement': 'changed', 'date': '2026-11-13T00:00:00Z'}]})
        review = root / 'review.docx'
        docx.save(parts, source, review)
        expected = inspect_docx(review)
        actual = docx.inspect(docx.load(review), review)
        assert json.loads(json.dumps(expected)) == actual, 'revision inspection differs'
        count += 1
        comparison = compare_docx(source, review, root / 'comparison.json')
        comparison.pop('out')
        assert comparison == docx.compare(baseline, actual, source, review), 'comparison differs'
        count += 1
        features = docx.load(source)
        features.update({
            'word/footnotes.xml': ('<w:footnotes xmlns:w="' + docx.W + '"><w:footnote w:id="-1"/><w:footnote w:id="0" w:type="separator"/><w:footnote w:id="1"/></w:footnotes>').encode(),
            'word/endnotes.xml': ('<w:endnotes xmlns:w="' + docx.W + '"><w:endnote w:id="1"/></w:endnotes>').encode(),
            'customXml/item1.xml': b'<record/>',
            'customXml/itemProps1.xml': b'<properties xmlns:ds="http://schemas.openxmlformats.org/officeDocument/2006/customXml"><ds:schemaRefs><ds:schemaRef ds:uri="urn:generated-test"/></ds:schemaRefs></properties>',
            'word/charts/chart1.xml': b'<chart/>',
            'word/diagrams/data1.xml': b'<diagram/>',
            'word/embeddings/file.bin': b'opaque',
            'word/activeX/control.xml': b'<control/>',
        })
        tree = docx.xml(features['word/document.xml'])
        paragraph = docx.ET.SubElement(tree.find(docx.q('body')), docx.q('p'))
        for tag in ('sdt', 'txbxContent', 'altChunk', 'oMath'):
            docx.ET.SubElement(paragraph, docx.q(tag))
        docx.ET.SubElement(docx.ET.SubElement(paragraph, docx.q('r')), docx.q('instrText')).text = ' PAGE '
        features['word/document.xml'] = docx.encode(tree)
        settings = docx.xml(features['word/settings.xml'])
        docx.ET.SubElement(settings, docx.q('documentProtection'), {docx.q('enforcement'): '0', docx.q('edit'): 'readOnly'})
        features['word/settings.xml'] = docx.encode(settings)
        inventory = root / 'inventory.docx'
        docx.save(features, source, inventory)
        assert inspect_docx(inventory) == docx.inspect(docx.load(inventory), inventory), 'complete package feature inventory differs'
        count += 1
        with tempfile.TemporaryDirectory(prefix='docx-delivered-') as published:
            target = Path(published).resolve() / 'final.docx'
            expected_delivery = deliver_docx(review, target, source_path=source, overwrite=True)
            args = docx.parser().parse_args(['deliver', '--input', str(review), '--source', str(source), '--out', str(target), '--overwrite'])
            assert expected_delivery == docx.execute(args), 'ordinary delivery differs'
        count += 1
        print(json.dumps({'status': 'ok', 'baseline': str(Path(reference_root).resolve()), 'rows': count, 'normal_paragraphs': baseline['paragraph_count'], 'normal_tables': baseline['table_count']}))

if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--baseline-root':
        reference_oracle(sys.argv[2])
    else:
        unittest.main()
