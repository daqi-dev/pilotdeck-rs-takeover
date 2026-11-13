#!/usr/bin/env python3
"""Independent XLSX package review, editing, engine-backed caches and publication.

No formula evaluator is included. LibreOffice and PDF rendering are external,
explicitly selected executables. All commands fail nonzero on operational errors.
"""
import argparse
import copy
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import posixpath
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import zipfile
import xml.etree.ElementTree as ET

MAIN = 'http://schemas.openxmlformats.org/spreadsheetml/2006/main'
REL = 'http://schemas.openxmlformats.org/package/2006/relationships'
DOCREL = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
MAX_BYTES = 256 * 1024 * 1024
CELL = re.compile(r'^[A-Z]{1,3}[1-9][0-9]{0,6}$')


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def xml(data):
    require(b'<!DOCTYPE' not in data.upper() and b'<!ENTITY' not in data.upper(),
            'DTD/entity declarations are not supported')
    return ET.fromstring(data)


def package(path, raw=None):
    require(Path(path).suffix.lower() == '.xlsx', 'expected .xlsx package')
    raw = Path(path).read_bytes() if raw is None else raw
    require(len(raw) <= MAX_BYTES, 'compressed package exceeds limit')
    with zipfile.ZipFile(io.BytesIO(raw)) as archive:
        infos = archive.infolist()
        require(len(infos) <= 10000, 'package has too many entries')
        names = [i.filename for i in infos]
        require(len(names) == len(set(names)), 'duplicate ZIP entries')
        require(sum(i.file_size for i in infos) <= MAX_BYTES, 'expanded package exceeds limit')
        parts = {}
        for info in infos:
            name = info.filename
            require(not info.flag_bits & 1, 'encrypted ZIP unsupported')
            require('\\' not in name and not name.startswith('/') and
                    '..' not in PurePosixPath(name).parts, 'unsafe package path')
            if not info.is_dir():
                parts[name] = archive.read(name)
    for required in ['[Content_Types].xml', '_rels/.rels', 'xl/workbook.xml', 'xl/_rels/workbook.xml.rels']:
        require(required in parts, f'missing required part: {required}')
    for name, data in parts.items():
        if name.endswith(('.xml', '.rels')):
            xml(data)
    content_types = xml(parts['[Content_Types].xml'])
    require(any(e.get('PartName') == '/xl/workbook.xml' and e.get('ContentType') ==
                'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml'
                for e in content_types), 'not a standalone non-macro XLSX workbook')
    for name, data in parts.items():
        if not name.endswith('.rels'):
            continue
        owner = '' if name == '_rels/.rels' else posixpath.join(
            posixpath.dirname(posixpath.dirname(name)), posixpath.basename(name)[:-5])
        ids = set()
        for rel in xml(data):
            require(rel.get('Id') not in ids, f'duplicate relationship ID: {name}')
            ids.add(rel.get('Id'))
            if rel.get('TargetMode') == 'External':
                continue
            target = rel.get('Target', '')
            require(target and '\\' not in target, 'invalid relationship target')
            resolved = posixpath.normpath(posixpath.join(posixpath.dirname(owner), target)) if not target.startswith('/') else target[1:]
            require(not resolved.startswith('../') and resolved in parts,
                    f'missing/unsafe relationship target: {name}: {target}')
    sheets(parts)
    return parts


def sheets(parts):
    rels = {r.get('Id'): r for r in xml(parts['xl/_rels/workbook.xml.rels'])}
    result = {}
    for sheet in xml(parts['xl/workbook.xml']).findall(f'{{{MAIN}}}sheets/{{{MAIN}}}sheet'):
        name = sheet.get('name')
        rel = rels.get(sheet.get(f'{{{DOCREL}}}id'))
        require(name and name not in result and rel is not None, 'invalid/duplicate worksheet identity')
        require(rel.get('Type', '').endswith('/worksheet'), 'only worksheet sheets are supported')
        target = rel.get('Target', '')
        part = target[1:] if target.startswith('/') else posixpath.normpath(posixpath.join('xl', target))
        require(part in parts, f'missing worksheet: {name}')
        result[name] = part
    require(result, 'workbook has no worksheets')
    return result


def cells(parts):
    result = {}
    for sheet, part in sheets(parts).items():
        for cell in xml(parts[part]).iter(f'{{{MAIN}}}c'):
            address = cell.get('r')
            require(address and CELL.fullmatch(address), 'invalid cell address')
            key = sheet + '!' + address
            require(key not in result, f'duplicate cell: {key}')
            formula = cell.find(f'{{{MAIN}}}f')
            value = cell.find(f'{{{MAIN}}}v')
            inline = cell.find(f'{{{MAIN}}}is')
            result[key] = {'sheet': sheet, 'address': address, 'part': part,
                           'type': cell.get('t', 'n'), 'style': cell.get('s'),
                           'formula': None if formula is None else (formula.text or ''),
                           'formulaAttributes': {} if formula is None else dict(formula.attrib),
                           'cachePresent': value is not None,
                           'value': None if value is None else ((value.text or '') if cell.get('t') == 'str' else value.text),
                           'inline': None if inline is None else ''.join(inline.itertext())}
    return result


def validate(path, raw=None):
    raw = Path(path).read_bytes() if raw is None else raw
    parts = package(path, raw)
    records = cells(parts)
    formulas = {k: v for k, v in records.items() if v['formula'] is not None}
    return {'status': 'ok', 'sheets': sheets(parts), 'parts': len(parts),
            'cells': len(records), 'formulas': formulas,
            'missingCaches': [k for k, v in formulas.items() if not v['cachePresent'] or (v['value'] is None and v['type'] != 'str')],
            'errors': {k: v['value'] for k, v in records.items() if v['type'] == 'e'},
            'sha256': digest(raw)}


def file_identity(path):
    info = os.stat(path, follow_symlinks=False)
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_mode)


def publish(data, output, replace=False, expected_source=None):
    output = Path(output)
    require(not output.is_symlink(), 'refusing symlink output')
    output.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix='.spreadsheet-', dir=output.parent)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        if replace:
            if expected_source is not None:
                previous_bytes, previous_identity = expected_source
                require(not output.is_symlink() and file_identity(output) == previous_identity and
                        output.read_bytes() == previous_bytes and file_identity(output) == previous_identity,
                        'source changed before commit; refusing to overwrite the concurrent version')
            # POSIX replace is atomic; non-cooperating writers can still race the
            # final identity/hash check. This is not a cross-process CAS transaction.
            os.replace(temporary, output)
        else:
            os.link(temporary, output)  # atomic, exclusive: never overwrite a racing creator
        require(digest(output.read_bytes()) == digest(data), 'published bytes differ from candidate')
    finally:
        Path(temporary).unlink(missing_ok=True)
    return str(output.resolve())


def archive_bytes(parts):
    stream = io.BytesIO()
    with zipfile.ZipFile(stream, 'w', zipfile.ZIP_DEFLATED) as archive:
        for name, data in parts.items():
            archive.writestr(name, data)
    return stream.getvalue()


def executable(value, label):
    require(value, f'{label} executable path is required')
    path = Path(value)
    require(path.is_absolute() and path.is_file() and os.access(path, os.X_OK),
            f'{label} must name an existing executable absolute path: {value}')
    return str(path)


def run_owned(command, timeout, env=None):
    # Own a fresh process group and reap its direct child on every path. Logs are
    # bounded when read; neither a verbose engine nor stderr can fill memory.
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        child = subprocess.Popen(command, stdout=stdout, stderr=stderr,
                                 start_new_session=True, env=env)
        print(json.dumps({'ownedPid': child.pid, 'ownedPgid': child.pid,
                          'executable': command[0]}), file=sys.stderr, flush=True)
        try:
            child.wait(timeout=timeout)
        except BaseException:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
            raise
        stdout.seek(0); stderr.seek(0)
        log = (stdout.read(65536) + stderr.read(65536)).decode('utf-8', 'replace')
        require(child.returncode == 0, f'external engine failed ({child.returncode}): {log}')
        return {'path': command[0], 'pid': child.pid, 'returncode': child.returncode, 'log': log}


def engine(args, options, profile):
    path = executable(options.soffice, 'LibreOffice')
    command = [path, '-env:UserInstallation=' + profile.as_uri(), '--headless', '--nologo',
               '--nodefault', '--nolockcheck', '--norestore', *args]
    return run_owned(command, options.timeout,
                     {**os.environ, 'HOME': str(profile.parent), 'SAL_USE_VCLPLUGIN': 'svp'})


def patch_cache(data, updates):
    # Local substitution leaves formula text, namespaces, drawings and other XML byte-identical.
    pattern = re.compile(rb'<(?P<tag>(?:[A-Za-z_][\w.-]*:)?c)\b(?P<attrs>[^>]*)>(?P<body>.*?)</(?P=tag)>', re.S)
    seen = set()
    def replace(match):
        attr = match.group('attrs')
        address = re.search(rb'\br\s*=\s*[\"\']([^\"\']+)[\"\']', attr)
        if address is None or address[1].decode() not in updates:
            return match[0]
        key = address[1].decode()
        require(key not in seen, 'duplicate cache target')
        seen.add(key)
        value = updates[key]
        attr = re.sub(rb'\s+t\s*=\s*[\"\'][^\"\']*[\"\']', b'', attr)
        attr += b' t="' + value['type'].encode() + b'"'
        body = re.sub(rb'<(?:[\w.-]+:)?v\b[^>]*(?:/>|>.*?</(?:[\w.-]+:)?v>)', b'', match.group('body'), flags=re.S)
        prefix = match.group('tag')[:-1]
        cached = ET.Element('v'); cached.text = value['value']
        encoded = ET.tostring(cached, encoding='utf-8')
        if prefix:
            encoded = encoded.replace(b'<v', b'<' + prefix + b'v').replace(b'</v>', b'</' + prefix + b'v>')
        return b'<' + match.group('tag') + attr + b'>' + body + encoded + b'</' + match.group('tag') + b'>'
    result = pattern.sub(replace, data)
    require(seen == set(updates), 'could not locate every original formula cell for cache merge')
    return result


def expanded_formulas(formulas):
    """Expand shared references only; calculation remains the external engine's job."""
    groups = {}
    for key, cell in formulas.items():
        attributes = cell['formulaAttributes']
        if attributes.get('t') == 'shared' and cell['formula']:
            group = (cell['sheet'], attributes.get('si'))
            require(group[1] is not None and group not in groups and attributes.get('ref'),
                    f'invalid shared formula anchor: {key}')
            groups[group] = cell
    result = {}
    for key, cell in formulas.items():
        attributes = cell['formulaAttributes']
        if attributes.get('t') != 'shared':
            result[key] = cell['formula']
            continue
        anchor = groups.get((cell['sheet'], attributes.get('si')))
        require(anchor is not None, f'shared formula anchor missing: {key}')
        from openpyxl.formula.translate import Translator
        from openpyxl.utils.cell import range_boundaries, coordinate_to_tuple
        left, top, right, bottom = range_boundaries(anchor['formulaAttributes']['ref'])
        row, column = coordinate_to_tuple(cell['address'])
        require(left <= column <= right and top <= row <= bottom,
                f'shared formula follower lies outside declared range: {key}')
        result[key] = Translator('=' + anchor['formula'], origin=anchor['address']).translate_formula(cell['address'])[1:]
    return result


def recalculate(options):
    original = package(options.input)
    before = cells(original)
    formulas = {k: v for k, v in before.items() if v['formula'] is not None}
    expected_formulas = expanded_formulas(formulas)
    for key, cell in formulas.items():
        require(cell['formulaAttributes'].get('t', 'normal') in {'normal', 'shared'} and
                not re.search(r'_xlfn\.|_xlws\.|_xludf\.|\[[^]]+\][^!]*!|\bCUBE\w*\s*\(|[A-Z]{1,3}[0-9]+#', expected_formulas[key], re.I),
                f'unsupported formula feature, no output produced: {key}')
    require(Path(options.input).resolve() != Path(options.out).resolve(), 'recalculate requires distinct output')
    executable(options.soffice, 'LibreOffice')
    with tempfile.TemporaryDirectory(prefix='spreadsheet-recalc-') as directory:
        root = Path(directory)
        source = root / 'source.xlsx'
        # Force full calculation on the private engine copy; never trust input caches.
        private = copy.copy(original)
        workbook = xml(private['xl/workbook.xml'])
        calc = workbook.find(f'{{{MAIN}}}calcPr')
        if calc is None:
            calc = ET.SubElement(workbook, f'{{{MAIN}}}calcPr')
        calc.set('fullCalcOnLoad', '1'); calc.set('forceFullCalc', '1'); calc.set('calcMode', 'auto')
        private['xl/workbook.xml'] = ET.tostring(workbook, encoding='utf-8', xml_declaration=True)
        source.write_bytes(archive_bytes(private))
        generated = root / 'calculated'; generated.mkdir()
        evidence = engine(['--convert-to', 'xlsx:Calc MS Excel 2007 XML', '--outdir', str(generated), str(source)], options, root / 'profile')
        calculated = generated / 'source.xlsx'
        require(calculated.is_file() and calculated.stat().st_size > 0, 'engine produced no recalculated XLSX')
        after = cells(package(calculated))
        actual_formulas = expanded_formulas({k: v for k, v in after.items() if v['formula'] is not None})
        updates = {}
        for key, cell in formulas.items():
            result = after.get(key)
            require(result is not None and actual_formulas.get(key) == expected_formulas[key], f'engine changed/missed formula identity: {key}')
            require(result['cachePresent'] and result['type'] in {'n', 'b', 'str', 'e', 'd'} and (result['value'] is not None or result['type'] == 'str') and not (result['type'] == 'e' and (result['value'] or '').upper().startswith('#NAME')),
                    f'missing, error or unsafe formula cache: {key}: {result}')
            updates.setdefault(cell['part'], {})[cell['address']] = result
        merged = copy.copy(original)
        for part, part_updates in updates.items():
            merged[part] = patch_cache(original[part], part_updates)
        candidate = root / 'candidate.xlsx'; candidate.write_bytes(archive_bytes(merged))
        verified = cells(package(candidate))
        for key in formulas:
            require(verified[key]['formula'] == before[key]['formula'] and
                    verified[key]['value'] == after[key]['value'] and verified[key]['type'] == after[key]['type'],
                    f'cache readback failed: {key}')
        for key, cell in before.items():
            if key not in formulas:
                require(verified.get(key) == cell, f'non-formula cell changed: {key}')
        output = publish(candidate.read_bytes(), options.out)
    return {'status': 'ok', 'output': output, 'engine': evidence,
            'formulaCount': len(formulas), 'cachedResults': {k: verified[k] for k in formulas},
            'changedParts': [k for k in original if original[k] != merged[k]]}


def compare(before, after):
    left, right = package(before), package(after)
    a, b = cells(left), cells(right)
    return {'status': 'ok', 'equal': left == right,
            'addedParts': sorted(right.keys() - left.keys()), 'removedParts': sorted(left.keys() - right.keys()),
            'changedParts': {k: {'before': digest(left[k]), 'after': digest(right[k])}
                             for k in left.keys() & right.keys() if left[k] != right[k]},
            'cellDifferences': {k: {'before': a.get(k), 'after': b.get(k)}
                                for k in sorted(a.keys() | b.keys()) if a.get(k) != b.get(k)}}


def edit(options):
    parts = package(options.input)
    # openpyxl round trips can discard unsupported objects. Refuse these templates,
    # instead of claiming an advanced object was preserved.
    forbidden = ('xl/drawings/', 'xl/charts/', 'xl/pivot', 'xl/slicer', 'xl/externalLinks/',
                 'xl/activeX/', 'xl/embeddings/', '_xmlsignatures/', 'customXml/')
    require(not any(name.startswith(forbidden) for name in parts),
            'package-sensitive workbook: use a localized OOXML task script and compare; edit refuses lossy round trip')
    from openpyxl import load_workbook
    from openpyxl.styles import Font, PatternFill, Alignment
    from openpyxl.chart import BarChart, LineChart, Reference
    from openpyxl.workbook.defined_name import DefinedName
    spec = json.loads(Path(options.spec).read_text())
    require(isinstance(spec, dict) and set(spec) <= {'sheets', 'names'}, 'unknown edit specification field')
    require(not any(b'extLst' in data for name, data in parts.items() if name.endswith('.xml')),
            'extension objects require localized OOXML editing')
    book = load_workbook(options.input)
    for item in spec.get('sheets', []):
        require(set(item) <= {'name', 'cells', 'merges', 'columnWidths', 'rowHeights', 'freeze', 'autoFilter', 'charts'}, 'unknown sheet edit field')
        sheet = book[item['name']]
        for address, patch in item.get('cells', {}).items():
            require(CELL.fullmatch(address), 'invalid edit cell address')
            require(set(patch) <= {'value', 'formula', 'numberFormat', 'font', 'fill', 'alignment'}, 'unknown cell edit field')
            require(not ('value' in patch and 'formula' in patch), 'choose value or formula')
            cell = sheet[address]
            if 'value' in patch:
                require(not isinstance(patch['value'], (dict, list)), 'cell value must be scalar')
                cell.value = patch['value']
            if 'formula' in patch:
                require(isinstance(patch['formula'], str) and patch['formula'].startswith('='), 'formula must begin with =')
                cell.value = patch['formula']
            if 'numberFormat' in patch: cell.number_format = patch['numberFormat']
            if 'font' in patch: cell.font = Font(**patch['font'])
            if 'fill' in patch: cell.fill = PatternFill(fill_type='solid', fgColor=patch['fill'])
            if 'alignment' in patch: cell.alignment = Alignment(**patch['alignment'])
        for merge in item.get('merges', []): sheet.merge_cells(merge)
        for col, width in item.get('columnWidths', {}).items(): sheet.column_dimensions[col].width = width
        for row, height in item.get('rowHeights', {}).items(): sheet.row_dimensions[int(row)].height = height
        if 'freeze' in item: sheet.freeze_panes = item['freeze']
        if 'autoFilter' in item: sheet.auto_filter.ref = item['autoFilter']
        for spec_chart in item.get('charts', []):
            require(set(spec_chart) <= {'kind', 'data', 'categories', 'title', 'anchor'}, 'unknown chart field')
            require(spec_chart['kind'] in {'bar', 'line'}, 'supported charts: bar, line')
            chart = BarChart() if spec_chart['kind'] == 'bar' else LineChart()
            chart.title = spec_chart.get('title', '')
            chart.add_data(Reference(sheet, range_string=f"'{sheet.title.replace(chr(39), chr(39)*2)}'!{spec_chart['data']}"), titles_from_data=True)
            if 'categories' in spec_chart:
                chart.set_categories(Reference(sheet, range_string=f"'{sheet.title.replace(chr(39), chr(39)*2)}'!{spec_chart['categories']}"))
            sheet.add_chart(chart, spec_chart['anchor'])
    for name, reference in spec.get('names', {}).items():
        book.defined_names.add(DefinedName(name, attr_text=reference))
    with tempfile.TemporaryDirectory(prefix='spreadsheet-edit-') as directory:
        candidate = Path(directory) / 'candidate.xlsx'; book.save(candidate)
        saved = package(candidate)
        known = re.compile(r'(?:\[Content_Types\]\.xml|_rels/\.rels|docProps/(?:app|core)\.xml|xl/(?:workbook|styles|sharedStrings|calcChain)\.xml|xl/_rels/workbook\.xml\.rels|xl/theme/theme[0-9]+\.xml|xl/worksheets/sheet[0-9]+\.xml|xl/worksheets/_rels/sheet[0-9]+\.xml\.rels)')
        for part, payload in parts.items():
            if not known.fullmatch(part):
                require(saved.get(part) == payload, f'editing would discard/change an unknown package part: {part}')
        report = validate(candidate)
        output = publish(candidate.read_bytes(), options.out)
    return {'status': 'ok', 'output': output, 'recalculated': False, 'validation': report}


def render(options):
    package(options.input)
    renderer = executable(options.pdf_renderer, 'pdftoppm')
    require(Path(renderer).name.startswith('pdftoppm'), 'renderer must implement pdftoppm CLI')
    output = Path(options.out_dir)
    require(not output.exists(), 'render output directory must be new')
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='spreadsheet-render-') as directory:
        root = Path(directory); source = root / 'source.xlsx'; shutil.copyfile(options.input, source)
        evidence = engine(['--convert-to', 'pdf:calc_pdf_Export', '--outdir', str(root), str(source)], options, root / 'profile')
        pdf = root / 'source.pdf'
        require(pdf.is_file() and pdf.read_bytes().startswith(b'%PDF-'), 'engine produced no valid PDF')
        pages = root / 'pages'; pages.mkdir()
        renderer_evidence = run_owned([renderer, '-png', '-r', '144', str(pdf), str(pages / 'page')], options.timeout)
        from PIL import Image
        images = list(pages.glob('page-*.png'))
        require(images and all(re.fullmatch(r'page-[0-9]+\.png', p.name) for p in images),
                'renderer produced no valid page filenames')
        images.sort(key=lambda p: int(p.stem.split('-')[1]))
        require([int(p.stem.split('-')[1]) for p in images] == list(range(1, len(images) + 1)),
                'renderer pages must have consecutive unique indices')
        for page in images:
            with Image.open(page) as image:
                require(image.format == 'PNG' and image.width > 0 and image.height > 0,
                        f'renderer produced invalid PNG: {page.name}')
                image.verify()
            with Image.open(page) as image:
                image.load()

        shutil.copyfile(pdf, pages / 'workbook.pdf')
        staging = Path(tempfile.mkdtemp(prefix='.spreadsheet-render-', dir=output.parent))
        try:
            for child in pages.iterdir(): shutil.copyfile(child, staging / child.name)
            require(not output.exists(), 'render output appeared concurrently')
            os.rename(staging, output)
        finally:
            if staging.exists(): shutil.rmtree(staging)
    return {'status': 'ok', 'output': str(output.resolve()), 'pages': [p.name for p in images], 'engine': evidence, 'renderer': renderer_evidence}


def deliver(options):
    require(not Path(options.out).is_symlink(), 'refusing symlink deliverable')
    candidate = Path(options.input).resolve(); output = Path(options.out).resolve()
    require(candidate != output, 'candidate and delivery must be distinct')
    source = Path(options.source).resolve() if options.source else None
    replacing = source == output
    require(not options.replace_source or replacing, '--replace-source requires matching --source and --out')
    require(not replacing or options.replace_source, 'source replacement requires explicit --replace-source')
    data = candidate.read_bytes()
    validate(candidate, data)
    recovery = None
    expected_source = None
    if replacing:
        identity = file_identity(source)
        prior = source.read_bytes()
        require(file_identity(source) == identity, 'source changed while taking recovery snapshot')
        expected_source = (prior, identity)
        work = os.environ.get('PILOTDECK_WORK_DIR')
        require(work, 'source replacement requires PILOTDECK_WORK_DIR for recovery')
        recovery_dir = Path(work) / 'spreadsheets' / 'recovery'; recovery_dir.mkdir(parents=True, exist_ok=True)
        fd, recovery = tempfile.mkstemp(prefix=source.name + '.', suffix='.bak', dir=recovery_dir)
        with os.fdopen(fd, 'wb') as stream:
            stream.write(prior); stream.flush(); os.fsync(stream.fileno())
        require(Path(recovery).read_bytes() == prior, 'source recovery bytes failed verification')
    return {'status': 'ok', 'output': publish(data, output, replacing, expected_source), 'sha256': digest(data), 'recovery': recovery}


def convert_legacy(options):
    source = Path(options.input)
    require(source.suffix.lower() == '.xls', 'convert-legacy accepts .xls only')
    require(source.is_file() and source.stat().st_size > 0, 'legacy source is missing/empty')
    require(Path(options.out).suffix.lower() == '.xlsx', 'legacy output must be .xlsx')
    with tempfile.TemporaryDirectory(prefix='spreadsheet-legacy-') as directory:
        root = Path(directory)
        private = root / 'source.xls'; shutil.copyfile(source, private)
        evidence = engine(['--convert-to', 'xlsx:Calc MS Excel 2007 XML', '--outdir', str(root), str(private)], options, root / 'profile')
        candidate = root / 'source.xlsx'
        report = validate(candidate)
        output = publish(candidate.read_bytes(), options.out)
    return {'status': 'ok', 'output': output, 'engine': evidence, 'validation': report}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['check', 'validate', 'edit', 'recalculate', 'render', 'compare', 'deliver', 'convert-legacy'])
    for flag in ['input', 'out', 'spec', 'before', 'after', 'out-dir', 'source', 'soffice', 'pdf-renderer', 'report']:
        parser.add_argument('--' + flag)
    parser.add_argument('--replace-source', action='store_true')
    parser.add_argument('--timeout', type=int, default=120)
    options = parser.parse_args()
    options.soffice = options.soffice or os.environ.get('PILOTDECK_SOFFICE') or os.environ.get('SPREADSHEET_SKILL_SOFFICE')
    options.pdf_renderer = options.pdf_renderer or os.environ.get('PILOTDECK_PDF_RENDERER') or os.environ.get('SPREADSHEET_SKILL_PDF_RENDERER')
    require(1 <= options.timeout <= 300, 'timeout must be between 1 and 300 seconds')
    if options.command == 'check':
        import importlib.util
        report = {'status': 'ok', 'openpyxl': importlib.util.find_spec('openpyxl') is not None,
                  'soffice': options.soffice, 'pdfRenderer': options.pdf_renderer}
        if options.soffice: executable(options.soffice, 'LibreOffice')
        if options.pdf_renderer: executable(options.pdf_renderer, 'pdftoppm')
    elif options.command == 'validate': report = validate(options.input)
    elif options.command == 'edit': report = edit(options)
    elif options.command == 'recalculate': report = recalculate(options)
    elif options.command == 'render': report = render(options)
    elif options.command == 'convert-legacy': report = convert_legacy(options)
    elif options.command == 'compare': report = compare(options.before, options.after)
    else: report = deliver(options)
    if options.report: publish((json.dumps(report, ensure_ascii=False, indent=2) + '\n').encode(), options.report)
    print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(json.dumps({'status': 'error', 'error': str(error)}, ensure_ascii=False), file=sys.stderr)
        sys.exit(2)
