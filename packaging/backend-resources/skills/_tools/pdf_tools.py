#!/usr/bin/env python3
"""Native resource PDF workflow. Python libraries and renderer are external runtimes."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def fresh(path):
    path = Path(path)
    if path.exists() or path.is_symlink():
        raise ValueError(f'output already exists: {path}')
    path.parent.mkdir(parents=True, exist_ok=True)
    return path


def write_json(path, value):
    with fresh(path).open('x') as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write('\n')


def reader(path):
    from pypdf import PdfReader
    result = PdfReader(path, strict=True)
    if result.is_encrypted:
        raise ValueError('encrypted PDF requires a password workflow')
    return result


def save(writer, path):
    path = fresh(path)
    created = False
    try:
        with path.open('xb') as stream:
            created = True
            writer.write(stream)
    except BaseException:
        if created:
            path.unlink(missing_ok=True)
        raise


def pages(spec, count):
    if spec is None or spec.strip().lower() == 'all':
        return list(range(count))
    result = []
    for token in spec.split(','):
        ends = token.strip().split('-')
        if len(ends) > 2:
            raise ValueError('invalid page range')
        first, last = int(ends[0]), int(ends[-1])
        if first > last or first < 1 or last > count:
            raise ValueError(f'page selection outside 1-{count}')
        for number in range(first - 1, last):
            if number not in result:
                result.append(number)
    if not result:
        raise ValueError('empty page selection')
    return result


def forms(document):
    fields = document.get_fields() or {}
    widgets = []
    for index, page in enumerate(document.pages, 1):
        for reference in page.get('/Annots', []):
            widget = reference.get_object()
            if widget.get('/Subtype') == '/Widget':
                parent = widget.get('/Parent')
                owner = parent.get_object() if parent else widget
                widgets.append(dict(page=index, name=owner.get('/T'), value=owner.get('/V'), rectangle=list(widget.get('/Rect', [])), appearance='/AP' in widget))
    return {'count': len(fields), 'fields': [dict(name=name, type=field.get('/FT'), value=field.get('/V'), defaultValue=field.get('/DV'), options=field.get('/Opt'), flags=field.get('/Ff'), alternateName=field.get('/TU')) for name, field in fields.items()], 'widgets': widgets}


def inspect(path, extract=False, tables=False):
    import pdfplumber
    document = reader(path)
    rows, texts, table_rows = [], [], []
    with pdfplumber.open(path) as pdf:
        for i, page in enumerate(pdf.pages):
            geometry = document.pages[i]
            text = page.extract_text(layout=True) or ''
            detected = page.extract_tables() if tables else []
            rows.append(dict(page=i + 1, widthPoints=float(geometry.mediabox.width), heightPoints=float(geometry.mediabox.height), rotation=geometry.rotation, characters=len(page.chars), words=len(page.extract_words()), images=len(page.images), detectedTables=len(detected) if tables else None, textPreview=' '.join(text.split())[:280]))
            texts.append(dict(page=i + 1, text=text))
            table_rows.append(dict(page=i + 1, tables=detected))
    report = dict(status='ok', input=str(Path(path).resolve()), sizeBytes=Path(path).stat().st_size, encrypted=False, pageCount=len(rows), metadata=dict(document.metadata or {}), forms=forms(document), pages=rows)
    return report, texts, table_rows


def audit(path):
    try:
        report, _, _ = inspect(path)
        failures = [] if report['pageCount'] else ['PDF has no pages']
        warnings = []
        for row in report['pages']:
            if min(row['widthPoints'], row['heightPoints']) <= 0:
                failures.append(f"page {row['page']} has invalid dimensions")
            if not row['characters'] and not row['images']:
                warnings.append(f"page {row['page']} has no extracted text or images; vector-only content may exist")
        if len({(r['widthPoints'], r['heightPoints']) for r in report['pages']}) > 1:
            warnings.append('document contains multiple page sizes')
        report.update(status='failed' if failures else 'ok', hardFailures=failures, warnings=warnings)
        return report
    except Exception as error:
        return dict(status='failed', hardFailures=[str(error)], warnings=[])


def renderer():
    explicit = os.environ.get('PILOTDECK_PDF_RENDERER')
    executable = shutil.which(explicit or 'pdftoppm')
    if not executable:
        raise ValueError('renderer missing: install Poppler or set PILOTDECK_PDF_RENDERER to source-built mutool')
    kind = 'mutool' if Path(executable).name == 'mutool' else 'pdftoppm'
    if explicit and Path(executable).name not in ('mutool', 'pdftoppm'):
        raise ValueError('renderer must be mutool or pdftoppm')
    return executable, kind


def render(args):
    from PIL import Image, ImageDraw
    if not 72 <= args.dpi <= 600:
        raise ValueError('dpi must be between 72 and 600')
    document = reader(args.input)
    destination = fresh(args.out_dir)
    executable, kind = renderer()
    if args.montage:
        fresh(args.montage)
    with tempfile.TemporaryDirectory(prefix='pilotdeck-pdf-render-') as scratch:
        prefix = str(Path(scratch) / 'page')
        command = [executable, 'draw', '-r', str(args.dpi), '-o', prefix + '-%d.png', args.input] if kind == 'mutool' else [executable, '-png', '-r', str(args.dpi), args.input, prefix]
        run = subprocess.run(command, capture_output=True, text=True)
        if run.returncode:
            raise ValueError(f'renderer failed: {run.stderr}')
        images = sorted(Path(scratch).glob('page-*.png'), key=lambda p: int(p.stem.split('-')[-1]))
        if len(images) != len(document.pages) or not images:
            raise ValueError('renderer page count differs from PDF')
        destination.mkdir()
        outputs = []
        previews = []
        for i, image in enumerate(images, 1):
            with Image.open(image) as decoded:
                if decoded.format != 'PNG':
                    raise ValueError('renderer output is not PNG: ' + image.name)
                decoded.load()
                preview = decoded.convert('RGB')
                preview.thumbnail((360, 520))
                previews.append(preview)
            target = destination / f'page-{i}.png'
            shutil.copyfile(image, target)
            outputs.append(str(target))
        if args.montage:
            width, height = 390, 560
            canvas = Image.new('RGB', (width * min(3, len(previews)), height * ((len(previews) + 2) // 3)), 'white')
            draw = ImageDraw.Draw(canvas)
            for i, preview in enumerate(previews):
                x, y = i % 3 * width, i // 3 * height
                canvas.paste(preview, (x + 10, y + 10))
                draw.text((x + 10, y + 535), f'Page {i + 1}', fill='black')
            canvas.save(args.montage)
    return dict(status='ok', pages=outputs, renderer=dict(path=executable, kind=kind, external=True), montage=args.montage)


def fill(args):
    from pypdf import PdfWriter
    document = reader(args.input)
    form = document.trailer['/Root'].get('/AcroForm')
    if form and form.get_object().get('/XFA'):
        raise ValueError('XFA forms are unsupported')
    fields = document.get_fields() or {}
    if any(field.get('/FT') == '/Sig' for field in fields.values()):
        raise ValueError('signed/signature forms require a dedicated workflow')
    values = json.loads(Path(args.data).read_text()) if args.data else json.loads(args.values)
    if not isinstance(values, dict) or not fields:
        raise ValueError('form requires an existing field tree and JSON object values')
    if set(values) - set(fields):
        raise ValueError('unknown form fields: ' + ', '.join(sorted(set(values) - set(fields))))
    # Do not silently fill a canonical field while an unrelated same-name
    # widget remains stale. Parent/Kids membership identifies the real tree.
    canonical = set()
    def collect(node):
        ref = getattr(node, 'indirect_reference', None)
        if ref is not None:
            canonical.add((ref.idnum, ref.generation))
        for child in node.get('/Kids', []):
            collect(child.get_object())
    for node in form.get_object().get('/Fields', []):
        collect(node.get_object())
    for page in document.pages:
        for annotation in page.get('/Annots', []):
            widget = annotation.get_object()
            if widget.get('/Subtype') != '/Widget':
                continue
            ref = getattr(widget, 'indirect_reference', None)
            if ref is None or (ref.idnum, ref.generation) not in canonical:
                raise ValueError('orphaned or ambiguous widget requires a dedicated repair workflow')
    writer = PdfWriter()
    writer.clone_document_from_reader(document)
    for page in writer.pages:
        writer.update_page_form_field_values(page, values, auto_regenerate=False)
    save(writer, args.out)
    verified = reader(args.out).get_fields() or {}
    if any(str(verified[name].get('/V')) != str(value) for name, value in values.items()):
        Path(args.out).unlink()
        raise ValueError('canonical field value verification failed')
    return dict(status='ok', output=args.out, updatedFields=sorted(values), interactive=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    for name in ('inspect', 'audit', 'forms-inspect'):
        command = commands.add_parser(name)
        command.add_argument('--input', required=True)
        command.add_argument('--out', required=True)
        if name == 'inspect':
            command.add_argument('--text-out')
            command.add_argument('--tables-out')
    commands.add_parser('check')
    commands.add_parser('fix')
    command = commands.add_parser('self-test')
    command.add_argument('--out', required=True)
    command = commands.add_parser('render')
    command.add_argument('--input', required=True)
    command.add_argument('--out-dir', required=True)
    command.add_argument('--dpi', type=int, default=144)
    command.add_argument('--montage')
    for name in ('scaffold', 'build'):
        command = commands.add_parser(name)
        command.add_argument('--out', required=True)
        if name == 'build':
            command.add_argument('--builder', required=True)
        else:
            command.add_argument('--force', action='store_true')
    command = commands.add_parser('merge')
    command.add_argument('--inputs', nargs='+', required=True)
    command.add_argument('--out', required=True)
    for name in ('split', 'rotate', 'forms-fill'):
        command = commands.add_parser(name)
        command.add_argument('--input', required=True)
        command.add_argument('--out-dir' if name == 'split' else '--out', required=True)
        if name != 'forms-fill':
            command.add_argument('--pages')
        if name == 'rotate':
            command.add_argument('--degrees', type=int, required=True)
        if name == 'forms-fill':
            data = command.add_mutually_exclusive_group(required=True)
            data.add_argument('--data')
            data.add_argument('--values')
    args = parser.parse_args(argv)
    if args.command == 'fix':
        import venv
        cache = Path(os.environ.get('PDF_SKILL_CACHE', str(Path(os.environ.get('XDG_CACHE_HOME', str(Path.home() / '.cache'))) / 'pilotdeck-pdf')))
        runtime = cache / 'runtime'
        # Reuse already installed Pillow and parsing libraries without modifying
        # them. Missing requirements are installed only into this private venv.
        venv.EnvBuilder(with_pip=True, system_site_packages=True).create(runtime)
        python = runtime / ('Scripts/python.exe' if os.name == 'nt' else 'bin/python')
        requirement = Path(__file__).resolve().parents[1] / 'pdf/runtime/requirements.txt'
        command = [str(python), '-m', 'pip', 'install', '--disable-pip-version-check', '--no-input', '--no-binary=:all:', '--no-build-isolation', '-r', str(requirement)]
        if os.environ.get('PILOTDECK_PDF_SOURCE_DIR'):
            command.extend(['--no-index', '--find-links', os.environ['PILOTDECK_PDF_SOURCE_DIR']])
        environment = dict(os.environ, PIP_CACHE_DIR=str(cache / 'pip-cache'))
        run = subprocess.run(command, capture_output=True, text=True, env=environment)
        if run.returncode:
            raise ValueError('source-only runtime installation failed: ' + run.stderr + run.stdout)
        check = subprocess.run([str(python), str(Path(__file__).resolve()), 'check'], capture_output=True, text=True, env=environment)
        if check.returncode:
            raise ValueError('runtime installed but check failed: ' + check.stderr)
        return dict(status='ok', runtime=str(runtime), installation='source-only; existing external libraries reused', check=json.loads(check.stdout))
    if args.command == 'check':
        import importlib.metadata
        import pdfplumber, pypdf, reportlab, chardet
        from PIL import Image
        versions = {name: importlib.metadata.version(name) for name in ('pypdf', 'pdfplumber', 'Pillow', 'reportlab', 'chardet')}
        def release(value):
            import re
            return tuple(int(part) for part in re.match(r'^(\d+)\.(\d+)', value).groups())
        ranges = {'pypdf': ((5, 0), (7, 0)), 'pdfplumber': ((0, 11), (0, 12)), 'Pillow': ((10, 0), (13, 0)), 'reportlab': ((4, 2), (5, 0))}
        for name, (minimum, maximum) in ranges.items():
            if not minimum <= release(versions[name]) < maximum:
                raise ValueError(f'{name} version {versions[name]} outside supported range')
        executable, kind = renderer()
        return dict(status='ok', python=sys.executable, libraries=versions, renderer=dict(path=executable, kind=kind, external=True))
    if args.command == 'self-test':
        destination = fresh(args.out)
        destination.mkdir()
        test = Path(__file__).resolve().parents[1] / 'pdf/scripts/test_pdf_tools.py'
        executable, _ = renderer()
        run = subprocess.run([sys.executable, str(test)], capture_output=True, text=True, env=dict(os.environ, PILOTDECK_PDF_RENDERER=executable))
        write_json(destination / 'result.json', dict(status='ok' if run.returncode == 0 else 'failed', stdout=run.stdout, stderr=run.stderr, rendered=True, renderer=executable))
        if run.returncode:
            raise ValueError('self-test failed: ' + run.stderr)
        return dict(status='ok', report=str(destination / 'result.json'))
    if args.command == 'inspect':
        report, texts, tables = inspect(args.input, True, bool(args.tables_out))
        write_json(args.out, report)
        if args.text_out:
            write_json(args.text_out, dict(input=args.input, pages=texts))
        if args.tables_out:
            write_json(args.tables_out, dict(input=args.input, pages=tables))
        return dict(status='ok', report=args.out, pages=report['pageCount'])
    if args.command == 'audit':
        report = audit(args.input)
        write_json(args.out, report)
        if report['hardFailures']:
            raise ValueError('; '.join(report['hardFailures']))
        return dict(status='ok', report=args.out, warnings=report['warnings'])
    if args.command == 'forms-inspect':
        document = reader(args.input)
        report = dict(status='ok', input=args.input, pageCount=len(document.pages), forms=forms(document))
        write_json(args.out, report)
        return report
    if args.command == 'forms-fill':
        return fill(args)
    if args.command == 'render':
        return render(args)
    if args.command == 'scaffold':
        source = Path(__file__).resolve().parents[1] / 'pdf/assets/starter_pdf.py'
        target = Path(args.out)
        if args.force and target.is_file() and not target.is_symlink():
            target.write_text(source.read_text())
        else:
            with fresh(args.out).open('x') as stream:
                stream.write(source.read_text())
        return dict(status='ok', builder=args.out)
    if args.command == 'build':
        output = fresh(args.out)
        work = os.environ.get('PILOTDECK_WORK_DIR')
        if work:
            Path(work).mkdir(parents=True, exist_ok=True)
        # The executable task builder can use normal overwrite-oriented PDF
        # libraries, but can only write its private candidate. A competing final
        # file is never handed to it or overwritten by our exclusive publication.
        with tempfile.TemporaryDirectory(prefix='pilotdeck-pdf-build-', dir=work) as private:
            candidate = Path(private) / 'candidate.pdf'
            result = subprocess.run([sys.executable, str(Path(args.builder).resolve()), '--out', str(candidate.resolve())], capture_output=True, text=True)
            if result.returncode:
                raise ValueError('builder failed: ' + result.stderr)
            data = candidate.read_bytes()
            # Audit the same immutable snapshot that will be published.
            staged = Path(private) / 'audited.pdf'
            staged.write_bytes(data)
            report = audit(staged)
            if report['hardFailures']:
                raise ValueError('built PDF failed audit: ' + '; '.join(report['hardFailures']))
            descriptor, pending = tempfile.mkstemp(prefix='.pilotdeck-pdf-', dir=output.parent)
            try:
                with os.fdopen(descriptor, 'wb') as stream:
                    stream.write(data)
                    stream.flush()
                    os.fsync(stream.fileno())
                # Same-directory hard link publishes complete bytes atomically
                # and refuses even a final path created after fresh().
                os.link(pending, output)
            finally:
                Path(pending).unlink(missing_ok=True)
        return dict(status='ok', output=args.out, pages=report['pageCount'], warnings=report['warnings'])
    from pypdf import PdfWriter
    if args.command == 'merge':
        writer = PdfWriter()
        for source in args.inputs:
            writer.append(reader(source))
        save(writer, args.out)
        return dict(status='ok', output=args.out, pages=len(writer.pages))
    document = reader(args.input)
    selected = pages(args.pages, len(document.pages))
    if args.command == 'rotate':
        if args.degrees % 90:
            raise ValueError('degrees must be a multiple of 90')
        writer = PdfWriter()
        writer.clone_document_from_reader(document)
        for i in selected:
            writer.pages[i].rotate(args.degrees)
        save(writer, args.out)
        return dict(status='ok', output=args.out, pages=[i + 1 for i in selected], degrees=args.degrees)
    destination = fresh(args.out_dir)
    destination.mkdir()
    outputs = []
    for i in selected:
        writer = PdfWriter()
        writer.append(document, pages=[i])
        path = destination / f'page-{i + 1}.pdf'
        save(writer, path)
        outputs.append(str(path))
    return dict(status='ok', outputs=outputs)


if __name__ == '__main__':
    try:
        print(json.dumps(main(), ensure_ascii=False, default=str))
    except Exception as error:
        print(f'PDF operation failed: {error}', file=sys.stderr)
        raise SystemExit(1)
