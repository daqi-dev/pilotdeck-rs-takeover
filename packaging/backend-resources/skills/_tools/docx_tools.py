#!/usr/bin/env python3
"""Independent, standard-library DOCX package review and publication tools."""
import argparse
import copy
import datetime
import hashlib
import json
import io
import weakref
from xml.sax.saxutils import quoteattr
import os
import posixpath
import time
from urllib.parse import unquote
from pathlib import Path, PurePosixPath
import signal
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
import zipfile

W = 'http://schemas.openxmlformats.org/wordprocessingml/2006/main'
REL = 'http://schemas.openxmlformats.org/package/2006/relationships'
CT = 'http://schemas.openxmlformats.org/package/2006/content-types'
OFF = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships/'
ET.register_namespace('w', W)
NS = {'w': W}
MAX_BYTES = 128 * 1024 * 1024
NAMESPACES = weakref.WeakKeyDictionary()
REBOUND_ROOTS = weakref.WeakSet()

class DocxError(Exception):
    def __init__(self, message, code='invalid-docx', status='error'):
        super().__init__(message)
        self.code, self.status = code, status

def q(name):
    return '{' + W + '}' + name

def xml(data):
    if b'<!DOCTYPE' in data.upper() or b'<!ENTITY' in data.upper():
        raise DocxError('DTD/entity declarations are unsupported', 'unsafe-xml')
    try:
        namespaces = {}
        rebound = False
        for _, (prefix, uri) in ET.iterparse(io.BytesIO(data), events=('start-ns',)):
            if prefix in namespaces and namespaces[prefix] != uri:
                rebound = True
            namespaces[prefix] = uri
        parser = ET.XMLParser(target=ET.TreeBuilder(insert_comments=True, insert_pis=True))
        root = ET.fromstring(data, parser=parser)
        NAMESPACES[root] = namespaces
        if rebound:
            REBOUND_ROOTS.add(root)
        return root
    except ET.ParseError as error:
        raise DocxError(str(error), 'invalid-xml') from error

def encode(root):
    if root in REBOUND_ROOTS:
        raise DocxError('Editing rebound XML prefixes requires a namespace-preserving editor', 'namespace-rebinding', 'unsupported')
    namespaces = NAMESPACES.get(root, {})
    for prefix, uri in namespaces.items():
        if not (prefix.startswith('ns') and prefix[2:].isdigit()):
            ET.register_namespace(prefix, uri)
    namespace = root.tag[1:].split('}')[0] if root.tag.startswith('{') else ''
    if namespace in (REL, CT):
        ET.register_namespace('', namespace)
    data = ET.tostring(root, encoding='utf-8', xml_declaration=True)
    # Preserve declarations referenced only by QName-valued attributes, e.g. mc:Ignorable.
    start = data.find(b'<', data.find(b'?>') + 2)
    end = data.find(b'>', start)
    insert = end - 1 if data[end - 1:end] == b'/' else end
    missing = []
    for prefix, uri in namespaces.items():
        if not prefix or prefix == 'xml':
            continue
        declaration = ('xmlns:' + prefix + '=').encode()
        if declaration not in data[start:end]:
            missing.append(' xmlns:' + prefix + '=' + quoteattr(uri))
    return data[:insert] + ''.join(missing).encode() + data[insert:]

def load(path, data=None):
    if Path(path).suffix.lower() != '.docx':
        raise DocxError('Expected a .docx file', 'invalid-docx-path')
    try:
        with zipfile.ZipFile(io.BytesIO(data) if data is not None else path) as archive:
            infos = archive.infolist()
            names = [i.filename for i in infos]
            if len(names) != len(set(names)) or sum(i.file_size for i in infos) > MAX_BYTES:
                raise DocxError('Duplicate members or package exceeds 128 MiB', 'unsafe-package')
            for i in infos:
                p = PurePosixPath(i.filename)
                if p.is_absolute() or '..' in p.parts or '\\' in i.filename or (i.external_attr >> 16) & 0o170000 == 0o120000:
                    raise DocxError('Unsafe archive member', 'unsafe-package')
            parts = {i.filename: archive.read(i) for i in infos if not i.is_dir()}
    except (OSError, zipfile.BadZipFile, RuntimeError) as error:
        raise DocxError(str(error)) from error
    for required in ('[Content_Types].xml', '_rels/.rels', 'word/document.xml'):
        if required not in parts:
            raise DocxError('Missing ' + required)
    for name, data in parts.items():
        if name.endswith(('.xml', '.rels')):
            root = xml(data)
            if name.endswith('.rels'):
                base = '' if name == '_rels/.rels' else name.rsplit('/_rels/', 1)[0]
                ids = set()
                for rel in root:
                    rid = rel.get('Id')
                    if not rid or rid in ids:
                        raise DocxError('Duplicate/missing relationship ID', 'invalid-relationships')
                    ids.add(rid)
                    if rel.get('TargetMode') != 'External':
                        target = unquote(rel.get('Target', '').split('#')[0])
                        resolved = posixpath.normpath(posixpath.join(base, target)) if not target.startswith('/') else target.lstrip('/')
                        if resolved not in parts or resolved.startswith('../'):
                            raise DocxError('Missing relationship target: ' + resolved, 'invalid-relationships')
    if any(name.lower() == 'word/vbaproject.bin' for name in parts):
        raise DocxError('Macro payload detected; macro-enabled packages are unsupported', 'macro-package-unsupported')
    if xml(parts['word/document.xml']).tag != q('document'):
        raise DocxError('Not a WordprocessingML document')
    return parts

def mutate_guard(parts):
    if any('vbaproject' in n.lower() or n.lower().startswith(('word/activex/', '_xmlsignatures/')) or n.lower() in ('word/signatures.xml', 'word/signatureline.xml') for n in parts):
        raise DocxError('Signed/macro packages require a dedicated editor', 'unsafe-mutation', 'unsupported')
    if 'word/settings.xml' in parts:
        settings = xml(parts['word/settings.xml'])
        protection = settings.find(q('documentProtection'))
        if settings.find(q('writeProtection')) is not None or (protection is not None and protection.get(q('enforcement'), '').strip().lower() in ('1', 'true', 'on', 'yes')):
            raise DocxError('Document protection blocks mutation', 'document-protected', 'blocked')
    # AlternateContent needs branch-aware editing beyond namespace preservation.
    for name, data in parts.items():
        if name.endswith('.xml') and b'AlternateContent' in data:
            raise DocxError('Markup compatibility content requires a namespace-preserving editor', 'markup-compatibility', 'unsupported')

def internal_path(path, code):
    work = os.environ.get('PILOTDECK_WORK_DIR')
    if work and not Path(path).resolve().is_relative_to(Path(work).resolve()):
        raise DocxError('Internal artifacts must be under PILOTDECK_WORK_DIR', code, 'blocked')

def output_path(source, output, overwrite=False, internal=True):
    source, output = Path(source).resolve(), Path(output).absolute()
    if internal:
        internal_path(output, 'candidate-output-outside-work-dir' if output.suffix.lower() == '.docx' else 'control-artifact-outside-work-dir')
    if output.exists() and source.exists() and os.path.samefile(source, output):
        raise DocxError('Output aliases input', 'source-protected')
    if source == output.resolve():
        raise DocxError('Input and output must be distinct', 'source-protected')
    if output.is_symlink() or (output.exists() and not overwrite):
        raise DocxError('Output already exists', 'output-exists')
    output.parent.mkdir(parents=True, exist_ok=True)
    return output

def atomic_bytes(output, data, overwrite=False, precommit=None):
    fd, temp = tempfile.mkstemp(prefix='.docx-', dir=output.parent)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        if precommit is not None:
            precommit()
        if overwrite:
            if output.is_symlink():
                raise DocxError('Refusing output symlink', 'output-exists')
            os.replace(temp, output)
        else:
            os.link(temp, output)  # Atomic no-clobber publication.
    finally:
        if os.path.exists(temp):
            os.unlink(temp)

def save(parts, source, output, overwrite=False):
    import io
    target = output_path(source, output, overwrite)
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, 'w', zipfile.ZIP_DEFLATED) as archive:
        for name, value in parts.items():
            archive.writestr(name, value)
    atomic_bytes(target, buffer.getvalue(), overwrite)
    return {'status': 'ok', 'input': str(Path(source).resolve()), 'out': str(target), 'sha256': hashlib.sha256(buffer.getvalue()).hexdigest()}

def visible_text(element):
    # Word's final view excludes deletions, but retains insertions.
    def walk(node):
        if node.tag in (q('del'), q('moveFrom')):
            return ''
        if node.tag == q('t'):
            return node.text or ''
        if node.tag == q('tab'):
            return '\t'
        if node.tag in (q('br'), q('cr')):
            return '\n'
        return ''.join(walk(child) for child in node)
    return walk(element)

def text_content(node):
    result = []
    for child in node:
        if child.tag == q('r'):
            for value in child:
                if value.tag == q('t'):
                    result.append(value.text or '')
                elif value.tag in (q('tab'), q('ptab')):
                    result.append('\t')
                elif value.tag == q('noBreakHyphen'):
                    result.append('-')
                elif value.tag == q('cr') or (value.tag == q('br') and value.get(q('type'), 'textWrapping') == 'textWrapping'):
                    result.append('\n')
        elif child.tag == q('hyperlink'):
            result.append(text_content(child))
    return ''.join(result)

def validation(parts, source=None):
    types = xml(parts['[Content_Types].xml'])
    defaults = {n.get('Extension', '').lower() for n in types if n.tag == '{' + CT + '}Default'}
    overrides = {n.get('PartName', '').lstrip('/') for n in types if n.tag == '{' + CT + '}Override'}
    warnings = ['No content type declaration for part: ' + name for name in sorted(parts) if name != '[Content_Types].xml' and not name.endswith('.rels') and name not in overrides and name.rsplit('.', 1)[-1].lower() not in defaults]
    if 'docProps/custom.xml' in parts:
        warnings.append('Document contains custom properties')
    return {'status': 'ok', 'input': str(Path(source).resolve()) if source else None, 'errors': [], 'warnings': warnings, 'expanded_bytes': sum(map(len, parts.values())), 'xml_parts': sum(n.lower().endswith(('.xml', '.rels')) for n in parts)}

def inspect(parts, source=None):
    roots = {name: xml(data) for name, data in parts.items() if name.endswith(('.xml', '.rels'))}
    document = roots['word/document.xml']
    body = document.find(q('body'))
    if body is None:
        raise DocxError('Document body is missing')
    styles = {}
    style_types = {}
    defaults = {'paragraph': 'Normal', 'table': 'Normal Table'}
    if 'word/styles.xml' in roots:
        defaults = {'paragraph': None, 'table': None}
        for style in roots['word/styles.xml'].findall(q('style')):
            name = style.find(q('name'))
            label = name.get(q('val')) if name is not None else None
            if label and label.lower().startswith('heading '):
                label = 'Heading ' + label[8:]
            if label in ('header', 'footer', 'caption'):
                label = label.title()
            styles[style.get(q('styleId'))] = label
            style_types[style.get(q('styleId'))] = style.get(q('type'))
            if style.get(q('default')) == '1':
                defaults[style.get(q('type'))] = label
    def style_name(element, kind):
        properties = element.find(q('pPr' if kind == 'paragraph' else 'tblPr'))
        selected = properties.find(q('pStyle' if kind == 'paragraph' else 'tblStyle')) if properties is not None else None
        identifier = selected.get(q('val')) if selected is not None else None
        return styles.get(identifier, defaults.get(kind)) if style_types.get(identifier) == kind else defaults.get(kind)
    alignments = {'left': 'LEFT (0)', 'center': 'CENTER (1)', 'right': 'RIGHT (2)', 'both': 'JUSTIFY (3)', 'distribute': 'DISTRIBUTE (4)', 'mediumKashida': 'JUSTIFY_MED (5)', 'numTab': 'NUM_TAB (6)', 'highKashida': 'JUSTIFY_HI (7)', 'lowKashida': 'JUSTIFY_LOW (8)', 'thaiDistribute': 'THAI_JUSTIFY (9)'}
    underline_values = {'words': 2, 'double': 3, 'dotted': 4, 'thick': 6, 'dash': 7, 'dotDash': 9, 'dotDotDash': 10, 'wave': 11, 'dottedHeavy': 20, 'dashedHeavy': 23, 'dashLong': 39, 'dashLongHeavy': 55, 'dotDashHeavy': 25, 'dotDotDashHeavy': 26, 'wavyHeavy': 27, 'wavyDouble': 43}
    def run_record(run):
        props = run.find(q('rPr'))
        def prop(name):
            return props.find(q(name)) if props is not None else None
        def boolean(name):
            node = prop(name)
            return None if node is None else node.get(q('val'), '1') not in ('0', 'false', 'off')
        underline = prop('u')
        uv = underline.get(q('val'), 'single') if underline is not None else None
        font, size = prop('rFonts'), prop('sz')
        return {'text': text_content(wrap_run(run)), 'bold': boolean('b'), 'italic': boolean('i'), 'underline': None if uv is None else True if uv == 'single' else False if uv == 'none' else underline_values.get(uv), 'font': font.get(q('ascii')) if font is not None else None, 'size_pt': int(size.get(q('val'))) / 2 if size is not None and int(size.get(q('val'))) else None}
    def wrap_run(run):
        holder = ET.Element('holder')
        holder.append(run)
        return holder
    def paragraph_record(paragraph, location, index):
        alignment = paragraph.find('./w:pPr/w:jc', NS)
        val = alignment.get(q('val')) if alignment is not None else None
        return {'index': index, 'location': location, 'text': text_content(paragraph), 'style': style_name(paragraph, 'paragraph'), 'alignment': alignments.get(val, val), 'runs': [record for run in paragraph.findall(q('r')) if (record := run_record(run))['text']]}
    def cells(table):
        prior = {}
        for row in table.findall(q('tr')):
            result, current = [], {}
            before = row.find('./w:trPr/w:gridBefore', NS)
            column = int(before.get(q('val'), '0')) if before is not None else 0
            for cell in row.findall(q('tc')):
                span_node = cell.find('./w:tcPr/w:gridSpan', NS)
                span = int(span_node.get(q('val'), '1')) if span_node is not None else 1
                merge = cell.find('./w:tcPr/w:vMerge', NS)
                for offset in range(span):
                    if merge is not None and merge.get(q('val'), 'continue') == 'continue':
                        if column + offset not in prior:
                            raise DocxError('Vertical merge has no preceding cell', 'invalid-table')
                        value = prior[column + offset]
                    else:
                        value = cell
                    result.append(value)
                    current[column + offset] = value
                column += span
            yield result
            prior = current
    paragraph_nodes, table_nodes = [], []
    seen_tables = set()
    def table_visit(table, location):
        if id(table) not in seen_tables:
            table_nodes.append((table, location))
            seen_tables.add(id(table))
        for ri, row in enumerate(cells(table), 1):
            for ci, cell in enumerate(row, 1):
                prefix = location + '.r' + str(ri) + '.c' + str(ci)
                paragraph_nodes.extend((p, prefix) for p in cell.findall(q('p')))
                for ti, nested in enumerate(cell.findall(q('tbl')), 1):
                    table_visit(nested, prefix + '.table' + str(ti))
    paragraph_nodes.extend((p, 'body') for p in body.findall(q('p')))
    for ti, table in enumerate(body.findall(q('tbl')), 1):
        table_visit(table, 'table' + str(ti))
    sections_nodes = list(document.iter(q('sectPr')))
    rels = {r.get('Id'): r.get('Target') for r in roots.get('word/_rels/document.xml.rels', [])}
    seen_stories = set()
    inherited = {}
    for si, section in enumerate(sections_nodes, 1):
        for kind in ('header', 'footer'):
            refs = [r for r in section.findall(q(kind + 'Reference')) if r.get(q('type')) == 'default']
            if refs:
                rid = refs[-1].get('{' + OFF.rstrip('/') + '}id')
                target = rels.get(rid)
                if target:
                    inherited[kind] = target.lstrip('/') if target.startswith('/') else posixpath.normpath(posixpath.join('word', target))
            story = inherited.get(kind)
            if story and story not in seen_stories and story in roots:
                seen_stories.add(story)
                prefix = 'section' + str(si) + '.' + kind
                paragraph_nodes.extend((p, prefix) for p in roots[story].findall(q('p')))
                for ti, table in enumerate(roots[story].findall(q('tbl')), 1):
                    table_visit(table, prefix + '.table' + str(ti))
    paragraphs = []
    for index, (paragraph, location) in enumerate(paragraph_nodes, 1):
        if text_content(paragraph) or paragraph.findall(q('r')):
            paragraphs.append(paragraph_record(paragraph, location, index))
    tables = []
    for index, (table, location) in enumerate(table_nodes, 1):
        rows = list(cells(table))
        grid = table.find(q('tblGrid'))
        tables.append({'index': index, 'location': location, 'rows': len(rows), 'columns': len(grid.findall(q('gridCol'))) if grid is not None else max(map(len, rows), default=0), 'style': style_name(table, 'table'), 'cells': [['\n'.join(text_content(p) for p in c.findall(q('p'))) for c in row] for row in rows]})
    sections = []
    for index, section in enumerate(sections_nodes, 1):
        size, margins = section.find(q('pgSz')), section.find(q('pgMar'))
        def inches(node, attr):
            value = node.get(q(attr)) if node is not None else None
            return int(value) / 1440 if value is not None else None
        sections.append({'index': index, 'width_inches': inches(size, 'w'), 'height_inches': inches(size, 'h'), 'orientation': 'LANDSCAPE (1)' if size is not None and size.get(q('orient')) == 'landscape' else 'PORTRAIT (0)', 'margins_inches': {key: inches(margins, key) for key in ('top', 'right', 'bottom', 'left')}})
    core = roots.get('docProps/core.xml')
    metadata = {}
    for key, local in [('title', 'title'), ('subject', 'subject'), ('author', 'creator'), ('last_modified_by', 'lastModifiedBy'), ('keywords', 'keywords'), ('category', 'category'), ('comments', 'description')]:
        metadata[key] = next((n.text or '' for n in core if isinstance(n.tag, str) and n.tag.split('}')[-1] == local), '') if core is not None else ''
    counts = {'insertions': 'ins', 'deletions': 'del', 'moves_from': 'moveFrom', 'moves_to': 'moveTo'}
    revisions = {key: 0 for key in (*counts, 'property_changes')}
    by_part, fields, comments = {}, [], []
    features = {'macros': any(n.lower().endswith('vbaproject.bin') for n in parts), 'active_content': sorted(n for n in parts if n.lower() == 'word/vbaproject.bin' or n.lower().startswith('word/activex/')), 'digital_signatures': any(n.lower().startswith('_xmlsignatures/') or n.lower() in ('word/signatures.xml', 'word/signatureline.xml') for n in parts), 'embeddings': sorted(n for n in parts if n.startswith('word/embeddings/')), 'charts': sorted(n for n in parts if n.startswith('word/charts/') and n.endswith('.xml')), 'diagrams': sorted(n for n in parts if n.startswith('word/diagrams/')), 'custom_xml': sorted(n for n in parts if n.startswith('customXml/')), 'custom_xml_schemas': [], 'content_controls': 0, 'text_boxes': 0, 'footnotes': 0, 'endnotes': 0, 'alt_chunks': 0, 'office_math': 0, 'document_protection_settings': {}, 'document_protection': {}}
    property_names = {q(n) for n in ('pPrChange', 'rPrChange', 'tblPrChange', 'trPrChange', 'tcPrChange', 'sectPrChange')}
    for name in sorted(roots):
        root = roots[name]
        if name.startswith('word/') and name.endswith('.xml'):
            values = {key: len(list(root.iter(q(tag)))) for key, tag in counts.items()}
            values['property_changes'] = sum(n.tag in property_names for n in root.iter())
            if any(values.values()):
                by_part[name] = values
                for key, value in values.items():
                    revisions[key] += value
            for tag, form, attr in [('instrText', 'complex', None), ('fldSimple', 'simple', q('instr'))]:
                for node in root.iter(q(tag)):
                    value = ' '.join(((node.get(attr) if attr else node.text) or '').split())
                    if value:
                        fields.append({'part': name, 'instruction': value, 'form': form})
            for feature, tag in [('content_controls', 'sdt'), ('text_boxes', 'txbxContent'), ('alt_chunks', 'altChunk')]:
                features[feature] += len(list(root.iter(q(tag))))
            features['office_math'] += sum(isinstance(n.tag, str) and n.tag.split('}')[-1] == 'oMath' for n in root.iter())
        if name.startswith('customXml/itemProps'):
            for node in root.iter():
                if isinstance(node.tag, str) and node.tag.split('}')[-1] == 'schemaRef':
                    features['custom_xml_schemas'].extend(v for k, v in node.attrib.items() if k.split('}')[-1] == 'uri' and v)
    for kind in ('footnote', 'endnote'):
        root = roots.get('word/' + kind + 's.xml')
        if root is not None:
            for node in root.findall(q(kind)):
                note_id = node.get(q('id'), '').strip()
                if node.get(q('type'), '').strip() not in ('separator', 'continuationSeparator', 'continuationNotice') and not (note_id.lstrip('-').isdigit() and int(note_id) < 0):
                    features[kind + 's'] += 1
    settings = roots.get('word/settings.xml')
    if settings is not None:
        for key in ('documentProtection', 'writeProtection'):
            node = settings.find(q(key))
            if node is not None:
                values = {k.split('}')[-1]: v for k, v in sorted(node.attrib.items())}
                features['document_protection_settings'][key] = values
                if key == 'writeProtection' or values.get('enforcement', '').strip().lower() in ('1', 'true', 'on', 'yes'):
                    features['document_protection'][key] = values
    if 'word/comments.xml' in roots:
        comments = [{'id': c.get(q('id')), 'author': c.get(q('author')), 'date': c.get(q('date')), 'text': ''.join(n.text or '' for n in c.iter(q('t')))} for c in roots['word/comments.xml'].findall(q('comment'))]
    features['custom_xml_schemas'] = sorted(set(features['custom_xml_schemas']))
    features['custom_xml_nonstandard'] = [uri for uri in features['custom_xml_schemas'] if uri != 'http://schemas.openxmlformats.org/officeDocument/2006/bibliography']
    limitations = []
    for feature, message in [('text_boxes', 'Text boxes are inventoried but not included in normal reading order.'), ('charts', 'Chart parts are inventoried but chart semantics are not extracted.'), ('diagrams', 'Diagram and SmartArt parts are inventoried but their visual semantics are not extracted.'), ('content_controls', 'Content controls are inventoried but their behavior is not modeled.'), ('footnotes', 'Footnote content is inventoried but not included in normal reading order.'), ('endnotes', 'Endnote content is inventoried but not included in normal reading order.'), ('alt_chunks', 'Alternative-format imported content is inventoried but not interpreted.'), ('office_math', 'Office Math objects are inventoried but mathematical semantics are not extracted.'), ('custom_xml_nonstandard', 'Nonstandard custom XML is inventoried but its application behavior is not modeled.'), ('embeddings', 'Embedded object contents are not inspected.'), ('active_content', 'ActiveX or macro content is inventoried but never executed or interpreted.'), ('digital_signatures', 'Digital signature validity and signer identity are not verified.'), ('document_protection', 'Document protection settings are inventoried but credentials and enforcement are not verified.')]:
        if features[feature]:
            limitations.append(message)
    if comments:
        limitations.append('Comment text is inventoried but comment anchor ranges are not mapped to reading-order text.')
    if any(revisions.values()):
        limitations.append('Tracked revisions are inventoried but accepted/rejected reading states are not reconstructed.')
    revisions['by_part'] = by_part
    external = [{'part': name, 'type': r.get('Type'), 'target': r.get('Target')} for name in sorted(roots) if name.endswith('.rels') for r in roots[name] if r.get('TargetMode') == 'External']
    return {'status': 'partial' if limitations else 'ok', 'input': str(Path(source).resolve()) if source else None, 'metadata': metadata, 'paragraphs': paragraphs, 'headings': [p for p in paragraphs if p['style'] and p['style'].lower().startswith('heading')], 'tables': tables, 'sections': sections, 'paragraph_count': len(paragraphs), 'table_count': len(tables), 'validation': validation(parts, source), 'comments': comments, 'tracked_changes': revisions, 'fields': fields, 'image_parts': sum(n.startswith('word/media/') for n in parts), 'external_relationships': external, 'package_features': features, 'inspection_coverage': {'status': 'partial' if limitations else 'complete', 'limitations': limitations}}

def filter_inspection(result, args):
    if args.max_items < 1:
        raise DocxError('max_items must be positive', 'invalid-inspection-filter')
    filtered = dict(result)
    paragraphs = [p for p in result['paragraphs'] if (not args.search or args.search.casefold() in p['text'].casefold()) and (not args.location or p['location'].startswith(args.location))]
    selected = paragraphs[:args.max_items]
    filtered['query'] = {'search': args.search, 'location': args.location, 'total_matches': len(paragraphs), 'returned': len(selected), 'truncated': len(paragraphs) > len(selected)}
    if args.summary:
        filtered.pop('paragraphs')
        filtered.pop('tables')
    else:
        filtered['paragraphs'] = selected
        indexes = {p['index'] for p in selected}
        filtered['headings'] = [p for p in result['headings'] if p['index'] in indexes]
        if args.search or args.location:
            filtered.pop('tables')
    return filtered

def compare(before, after, before_path, after_path):
    import difflib
    from collections import Counter
    result = {'status': 'ok' if all(v['inspection_coverage']['status'] == 'complete' for v in (before, after)) else 'partial', 'before': str(Path(before_path).resolve()), 'after': str(Path(after_path).resolve()), 'inspection_coverage': {'before': before['inspection_coverage'], 'after': after['inspection_coverage']}}
    for label, field in [('paragraph', 'paragraphs'), ('table', 'tables'), ('heading', 'headings'), ('section', 'sections'), ('field', 'fields')]:
        for side, value in [('before', before), ('after', after)]:
            result[label + '_count_' + side] = len(value[field])
    result['image_count_before'], result['image_count_after'] = before['image_parts'], after['image_parts']
    def field_counter(value):
        return Counter((f['part'], f['instruction'], f['form']) for f in value['fields'])
    bc, ac = field_counter(before), field_counter(after)
    for key, delta in [('fields_removed', bc - ac), ('fields_added', ac - bc)]:
        result[key] = [{'part': part, 'instruction': instruction, 'form': form, 'count': count} for (part, instruction, form), count in sorted(delta.items())]
    for key, field in [('metadata_changes', 'metadata'), ('package_feature_changes', 'package_features')]:
        result[key] = {name: {'before': before[field].get(name), 'after': after[field].get(name)} for name in sorted(set(before[field]) | set(after[field])) if before[field].get(name) != after[field].get(name)}
    result['diff'] = list(difflib.unified_diff([p['text'] for p in before['paragraphs']], [p['text'] for p in after['paragraphs']], fromfile=Path(before_path).name, tofile=Path(after_path).name, lineterm=''))
    return result

def run_text(original, value, deleted=False):
    run = ET.Element(q('r'))
    properties = original.find(q('rPr'))
    if properties is not None:
        run.append(copy.deepcopy(properties))
    node = ET.SubElement(run, q('delText' if deleted else 't'))
    node.set('{http://www.w3.org/XML/1998/namespace}space', 'preserve')
    node.text = value
    return run

def choose(candidates, item):
    occurrence = item.get('occurrence')
    if occurrence is not None and (type(occurrence) is not int or occurrence < 1):
        raise DocxError('occurrence must be a positive integer', 'invalid-spec')
    if not candidates or (occurrence or 1) > len(candidates):
        raise DocxError('Review target was not found', 'review-target-not-found', 'partial')
    if occurrence is None and len(candidates) > 1:
        raise DocxError('Specify occurrence for an ambiguous review target', 'ambiguous-review-target', 'partial')
    return candidates[(occurrence or 1) - 1]

def annotate(parts, spec):
    mutate_guard(parts)
    if not isinstance(spec, dict) or set(spec) - {'comments', 'tracked_replacements'}:
        raise DocxError('Unknown annotation fields', 'invalid-spec')
    for key, required in [('comments', 'text'), ('tracked_replacements', 'replacement')]:
        items = spec.get(key, [])
        if not isinstance(items, list):
            raise DocxError(key + ' must be an array', 'invalid-spec')
        for item in items:
            if not isinstance(item, dict) or any(not isinstance(item.get(k), str) or not item[k] for k in ('match', required)):
                raise DocxError('Review text must be non-empty strings', 'invalid-spec')
            if item.get('location', 'body') not in ('', 'body'):
                raise DocxError('Only the main document story can be annotated', 'review-location-unsupported', 'unsupported')
    root = xml(parts['word/document.xml'])
    ids = [int(e.get(q('id'))) for e in root.iter() if (e.get(q('id')) or '').isdigit()]
    next_id = max(ids, default=0) + 1
    date = datetime.datetime.now(datetime.timezone.utc).isoformat()
    comments = xml(parts['word/comments.xml']) if 'word/comments.xml' in parts else ET.Element(q('comments'))
    for item in spec.get('comments', []):
        para = choose([p for p in root.iter(q('p')) if item['match'] in visible_text(p)], item)
        cid = str(max([int(c.get(q('id'))) for c in comments if (c.get(q('id')) or '').isdigit()], default=-1) + 1)
        nodes = [n for n in para if n.tag != q('pPr')]
        if not nodes:
            raise DocxError('No commentable content', 'review-target-not-found')
        start = ET.Element(q('commentRangeStart'), {q('id'): cid})
        para.insert(list(para).index(nodes[0]), start)
        para.append(ET.Element(q('commentRangeEnd'), {q('id'): cid}))
        ET.SubElement(ET.SubElement(para, q('r')), q('commentReference'), {q('id'): cid})
        c = ET.SubElement(comments, q('comment'), {q('id'): cid, q('author'): str(item.get('author', 'PilotDeck')), q('date'): str(item.get('date', date))})
        ET.SubElement(ET.SubElement(ET.SubElement(c, q('p')), q('r')), q('t')).text = item['text']
    if spec.get('comments'):
        parts['word/comments.xml'] = encode(comments)
        name = 'word/_rels/document.xml.rels'
        rels = xml(parts[name]) if name in parts else ET.Element('{' + REL + '}Relationships')
        if not any(r.get('Type') == OFF + 'comments' for r in rels):
            used = {r.get('Id') for r in rels}
            rid = 1
            while 'rId' + str(rid) in used:
                rid += 1
            ET.SubElement(rels, '{' + REL + '}Relationship', {'Id': 'rId' + str(rid), 'Type': OFF + 'comments', 'Target': 'comments.xml'})
        parts[name] = encode(rels)
        types = xml(parts['[Content_Types].xml'])
        if not any(t.get('PartName') == '/word/comments.xml' for t in types):
            ET.SubElement(types, '{' + CT + '}Override', {'PartName': '/word/comments.xml', 'ContentType': 'application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml'})
        parts['[Content_Types].xml'] = encode(types)
    for item in spec.get('tracked_replacements', []):
        parents = {child: parent for parent in root.iter() for child in parent}
        candidates = []
        for run in root.iter(q('r')):
            ancestor = parents.get(run)
            revision = False
            while ancestor is not None:
                revision |= ancestor.tag in (q('ins'), q('del'))
                ancestor = parents.get(ancestor)
            if revision:
                continue
            value = visible_text(run)
            offset = 0
            while True:
                offset = value.find(item['match'], offset)
                if offset < 0:
                    break
                candidates.append((run, value, offset))
                offset += len(item['match'])
        if not candidates and any(item['match'] in visible_text(p) for p in root.iter(q('p'))):
            raise DocxError('Cross-run replacement cannot safely preserve formatting', 'cross-run-redline', 'unsupported')
        run, value, offset = choose(candidates, item)
        if any(n.tag not in (q('rPr'), q('t')) for n in run):
            raise DocxError('Replacement run contains non-text content', 'complex-run-redline', 'unsupported')
        parent = parents[run]
        index = list(parent).index(run)
        parent.remove(run)
        nodes = []
        if offset:
            nodes.append(run_text(run, value[:offset]))
        for tag, content in [('del', item['match']), ('ins', item['replacement'])]:
            revision = ET.Element(q(tag), {q('id'): str(next_id), q('author'): str(item.get('author', 'PilotDeck')), q('date'): str(item.get('date', date))})
            next_id += 1
            revision.append(run_text(run, content, tag == 'del'))
            nodes.append(revision)
        if offset + len(item['match']) < len(value):
            nodes.append(run_text(run, value[offset + len(item['match']):]))
        for node in reversed(nodes):
            parent.insert(index, node)
    if spec.get('tracked_replacements'):
        if 'word/settings.xml' not in parts:
            raise DocxError('word/settings.xml required for tracked changes')
        settings = xml(parts['word/settings.xml'])
        if settings.find(q('trackRevisions')) is None:
            ET.SubElement(settings, q('trackRevisions'))
        parts['word/settings.xml'] = encode(settings)
    parts['word/document.xml'] = encode(root)

def remove_comments(parts):
    for name in list(parts):
        if name.startswith('word/comments') or name == 'word/people.xml':
            del parts[name]
            continue
        if not name.endswith(('.xml', '.rels')):
            continue
        root = xml(parts[name])
        for parent in root.iter():
            for node in list(parent):
                if node.tag in (q('commentRangeStart'), q('commentRangeEnd'), q('commentReference')) or ('comments' in node.get('Type', '') or node.get('Type', '').endswith('/people')) or node.get('PartName', '').startswith(('/word/comments', '/word/people.xml')):
                    parent.remove(node)
        parts[name] = encode(root)

def finalize(parts, accept=False, reject=False, comments=False):
    mutate_guard(parts)
    if not (accept or reject or comments):
        raise DocxError('Choose a finalize action', 'finalize-action-required')
    if accept or reject:
        for name in list(parts):
            if not name.startswith('word/') or not name.endswith('.xml') or name.startswith('word/comments'):
                continue
            root = xml(parts[name])
            # Do not silently discard property/table/move revisions that need richer semantics.
            if any((isinstance(n.tag, str) and n.tag.endswith('PrChange')) or n.tag in (q('moveFrom'), q('moveTo'), q('cellDel'), q('cellIns')) for n in root.iter()):
                raise DocxError('Complex revisions require a dedicated editor', 'complex-revisions', 'unsupported')
            for parent in reversed(list(root.iter())):
                for node in list(parent):
                    if node.tag == q('trackRevisions'):
                        parent.remove(node)
                    elif node.tag in (q('ins'), q('del')):
                        if not len(node) or any(child.tag != q('r') for child in node):
                            raise DocxError('Non-run revisions require a dedicated editor', 'complex-revisions', 'unsupported')
                        keep = (node.tag == q('ins')) == accept
                        index = list(parent).index(node)
                        parent.remove(node)
                        if keep:
                            for child in reversed(list(node)):
                                for text in child.iter(q('delText')):
                                    text.tag = q('t')
                                parent.insert(index, child)
            parts[name] = encode(root)
    if comments:
        remove_comments(parts)

def sanitize(parts, comments=False):
    mutate_guard(parts)
    parts.pop('docProps/custom.xml', None)
    for name in list(parts):
        if not name.endswith(('.xml', '.rels')):
            continue
        root = xml(parts[name])
        for parent in root.iter():
            for attr in list(parent.attrib):
                if attr.split('}')[-1].startswith('rsid'):
                    del parent.attrib[attr]
            for node in list(parent):
                if not isinstance(node.tag, str):
                    continue
                local = node.tag.split('}')[-1]
                if (name == 'docProps/core.xml' and local in ('creator', 'lastModifiedBy', 'subject', 'keywords')) or node.get('PartName') == '/docProps/custom.xml' or node.get('Target', '').lstrip('/') == 'docProps/custom.xml':
                    parent.remove(node)
        parts[name] = encode(root)
    if comments:
        remove_comments(parts)

def owned_command(argv, timeout, cwd, env=None):
    if os.name != 'posix':
        raise DocxError('Render process ownership currently requires POSIX', 'renderer-platform', 'unsupported')
    with tempfile.TemporaryFile() as log:
        child = subprocess.Popen(argv, cwd=cwd, stdout=log, stderr=log, start_new_session=True, stdin=subprocess.DEVNULL, env=env)
        try:
            deadline = time.monotonic() + timeout
            while child.poll() is None:
                if log.tell() > 8 * 1024 * 1024:
                    raise DocxError('Renderer output exceeded 8 MiB', 'renderer-output-limit')
                if time.monotonic() >= deadline:
                    raise subprocess.TimeoutExpired(argv, timeout)
                time.sleep(0.02)
            code = child.returncode
        except subprocess.TimeoutExpired as error:
            raise DocxError('Renderer exceeded timeout', 'renderer-timeout') from error
        finally:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
        if log.tell() > 8 * 1024 * 1024:
            raise DocxError('Renderer output exceeded 8 MiB', 'renderer-output-limit')
        log.seek(0)
        detail = log.read(8192).decode('utf-8', 'replace')
        if code:
            raise DocxError('Renderer failed (' + str(code) + '): ' + detail, 'renderer-failed')

def decode_png(path):
    import struct
    import zlib
    data = Path(path).read_bytes()
    if not data.startswith(b'\x89PNG\r\n\x1a\n'):
        raise DocxError('Invalid PNG signature', 'invalid-render-image')
    offset, payload, header = 8, bytearray(), None
    while offset + 12 <= len(data):
        length = struct.unpack('>I', data[offset:offset + 4])[0]
        kind = data[offset + 4:offset + 8]
        content = data[offset + 8:offset + 8 + length]
        if offset + 12 + length > len(data) or zlib.crc32(kind + content) & 0xffffffff != struct.unpack('>I', data[offset + 8 + length:offset + 12 + length])[0]:
            raise DocxError('PNG chunk checksum/length mismatch', 'invalid-render-image')
        if kind == b'IHDR':
            if header is not None or len(content) != 13:
                raise DocxError('Invalid PNG header', 'invalid-render-image')
            header = struct.unpack('>IIBBBBB', content)
        elif kind == b'IDAT':
            payload.extend(content)
        elif kind == b'IEND':
            break
        offset += length + 12
    if header is None:
        raise DocxError('PNG has no header', 'invalid-render-image')
    width, height, depth, color, compression, filtering, interlace = header
    channels = {0: 1, 2: 3, 4: 2, 6: 4}.get(color)
    if not channels or depth != 8 or compression or filtering or interlace or width < 1 or height < 1 or width * height > 50_000_000:
        raise DocxError('Unsupported PNG pixel format or dimensions', 'invalid-render-image')
    stride = width * channels
    expected = height * (stride + 1)
    inflater = zlib.decompressobj()
    raw = inflater.decompress(payload, expected + 1)
    if len(raw) != expected or not inflater.eof:
        raise DocxError('Invalid PNG raster length', 'invalid-render-image')
    pixels = bytearray(height * stride)
    for y in range(height):
        start = y * (stride + 1)
        mode = raw[start]
        if mode > 4:
            raise DocxError('Unknown PNG row filter', 'invalid-render-image')
        row = raw[start + 1:start + 1 + stride]
        for x, value in enumerate(row):
            index = y * stride + x
            left = pixels[index - channels] if x >= channels else 0
            up = pixels[index - stride] if y else 0
            diagonal = pixels[index - stride - channels] if y and x >= channels else 0
            if mode == 1:
                value += left
            elif mode == 2:
                value += up
            elif mode == 3:
                value += (left + up) // 2
            elif mode == 4:
                predictor = left + up - diagonal
                distances = [abs(predictor - v) for v in (left, up, diagonal)]
                value += (left, up, diagonal)[distances.index(min(distances))]
            pixels[index] = value & 255
    return width, height, channels, pixels

def is_cjk(value):
    return len(value) == 1 and any(lo <= ord(value) <= hi for lo, hi in ((0x3400, 0x4dbf), (0x4e00, 0x9fff), (0xf900, 0xfaff)))

def page_cjk_coverage(page, image, scale):
    width, height, channels, pixels = decode_png(image)
    total = painted = 0
    for line in page.iter('line'):
        line_box = [float(n) for n in line.get('bbox', '').split()]
        for character in line.iter('char'):
            if not is_cjk(character.get('c', '')):
                continue
            coordinates = [float(n) for n in character.get('quad', '').split()]
            if len(coordinates) != 8:
                raise DocxError('Structured text character lacks a quad', 'invalid-stext')
            xs, ys = coordinates[::2], coordinates[1::2]
            x0, x1, y0, y1 = min(xs), max(xs), min(ys), max(ys)
            # Missing glyphs can have zero-height quads. The line's vertical font
            # extent makes their empty paint observable instead of silently skipping them.
            if y1 <= y0 and len(line_box) == 4:
                y0, y1 = line_box[1], line_box[3]
            left, top = max(0, int(x0 * scale)), max(0, int(y0 * scale))
            right, bottom = min(width, int(x1 * scale + 1)), min(height, int(y1 * scale + 1))
            if right <= left or bottom <= top:
                continue
            total += 1
            ink, area = 0, (right - left) * (bottom - top)
            for y in range(top, bottom):
                for x in range(left, right):
                    offset = (y * width + x) * channels
                    values = pixels[offset:offset + (1 if channels in (1, 2) else 3)]
                    if any(value < 245 for value in values):
                        ink += 1
            if area and ink / area >= 0.03:
                painted += 1
    return {'characters': total, 'visible_characters': painted, 'ratio': round(painted / total, 4) if total else None}

def check_cjk_pages(structured, images, dpi):
    pages = list(xml(Path(structured).read_bytes()).iter('page'))
    if len(pages) != len(images):
        raise DocxError('Text/raster page count mismatch', 'invalid-stext')
    coverage, text = [], []
    for number, (page, image) in enumerate(zip(pages, images), 1):
        item = page_cjk_coverage(page, image, dpi / 72)
        item['page'] = number
        coverage.append(item)
        text.append({'page': number, 'characters': sum(len(''.join(c.get('c', '') for c in line.iter('char'))) + 1 for line in page.iter('line') if any(True for _ in line.iter('char')))})
    failed = [p for p in coverage if p['characters'] >= 8 and p['ratio'] is not None and p['ratio'] < 0.7]
    if failed:
        raise DocxError('Rendered PDF text exists but CJK glyphs are not visibly painted: ' + json.dumps(failed), 'render-cjk-glyphs-missing')
    return coverage, text

def render(args):
    source_bytes = Path(args.input).read_bytes()
    source_parts = load(args.input, source_bytes)
    for value in (args.soffice, args.pdftoppm, args.mutool):
        if not value or not Path(value).is_absolute() or not os.access(value, os.X_OK):
            raise DocxError('Supply executable absolute --soffice, --pdf-renderer and --mutool paths', 'renderer-unavailable', 'blocked')
    if not 72 <= args.dpi <= 300 or not 1 <= args.timeout <= 1800:
        raise DocxError('dpi/timeout out of range', 'invalid-render-options')
    render_root = Path(args.out_dir).resolve()
    internal_path(render_root, 'control-artifact-outside-work-dir')
    revision = hashlib.sha256(source_bytes).hexdigest()
    output = render_root / revision[:16]
    if output.is_symlink():
        raise DocxError('Refusing symlink render directory', 'unsafe-render-path')
    output.mkdir(parents=True, exist_ok=True)
    for stale in output.glob('page-*.png'):
        if stale.is_file() or stale.is_symlink():
            stale.unlink()
    with tempfile.TemporaryDirectory(prefix='.docx-render-', dir=output.parent) as work:
        work = Path(work)
        snapshot = work / 'source.docx'
        snapshot.write_bytes(source_bytes)
        snapshot.chmod(0o600)
        home = work / 'home'
        home.mkdir()
        render_env = dict(os.environ, HOME=str(home), PILOT_HOME=str(home / 'pilot'), TMPDIR=str(work))
        profile = work / 'profile'
        user = profile / 'user'
        for directory in ('autocorr', 'config', 'pack', 'temp'):
            (user / directory).mkdir(parents=True, exist_ok=True)
        oor = 'http://openoffice.org/2001/registry'
        registry = ET.Element('{' + oor + '}items')
        item = ET.SubElement(registry, 'item', {'{' + oor + '}path': '/org.openoffice.Office.Common/I18N/CJK'})
        for name in ('AsianTypography', 'CJKFont'):
            prop = ET.SubElement(item, 'prop', {'{' + oor + '}name': name, '{' + oor + '}op': 'fuse'})
            ET.SubElement(prop, 'value').text = 'true'
        (user / 'registrymodifications.xcu').write_bytes(encode(registry))
        if args.font_dir:
            config = ET.Element('fontconfig')
            for directory in args.font_dir:
                if not Path(directory).is_dir():
                    raise DocxError('Font directory does not exist: ' + directory, 'fonts-unavailable', 'blocked')
                ET.SubElement(config, 'dir').text = str(Path(directory).resolve())
            ET.SubElement(config, 'cachedir').text = str(work / 'font-cache')
            fontconfig = work / 'fonts.conf'
            fontconfig.write_bytes(encode(config))
            render_env['FONTCONFIG_FILE'] = str(fontconfig)
        owned_command([args.soffice, '-env:UserInstallation=' + (work / 'profile').as_uri(), '--headless', '--convert-to', 'pdf', '--outdir', str(work), str(snapshot)], args.timeout, work, render_env)
        pdf = work / 'source.pdf'
        if not pdf.is_file() or not pdf.read_bytes().startswith(b'%PDF-'):
            raise DocxError('Renderer did not produce a PDF', 'renderer-output-missing')
        pages = work / 'pages'
        pages.mkdir()
        owned_command([args.pdftoppm, '-png', '-r', str(args.dpi), str(pdf), str(pages / 'page')], args.timeout, work, render_env)
        images = sorted(pages.glob('page-*.png'), key=lambda p: int(p.stem.rsplit('-', 1)[-1]))
        if not images or any(not p.read_bytes().startswith(b'\x89PNG\r\n\x1a\n') for p in images):
            raise DocxError('Renderer did not produce PNG pages', 'renderer-output-missing')
        coverage, page_text = [], []
        if not args.mutool or not Path(args.mutool).is_absolute() or not os.access(args.mutool, os.X_OK):
            raise DocxError('Rendering text/glyph checks require an executable absolute --mutool path', 'glyph-checker-unavailable', 'blocked')
        structured = work / 'text.xml'
        owned_command([args.mutool, 'draw', '-F', 'stext', '-o', str(structured), str(pdf)], args.timeout, work, render_env)
        coverage, page_text = check_cjk_pages(structured, images, args.dpi)
        expected_cjk = sum(is_cjk(c) for node in xml(source_parts['word/document.xml']).iter(q('t')) for c in (node.text or ''))
        if expected_cjk >= 8 and sum(p['characters'] for p in coverage) == 0:
            raise DocxError('Source contains CJK but rendered PDF has no CJK text', 'render-cjk-text-missing')
        names = ['page-' + str(number) + '.png' for number in range(1, len(images) + 1)]
        for image, name in zip(images, names):
            os.replace(image, output / name)
    return {'status': 'ok', 'input': str(Path(args.input).resolve()), 'revision': revision, 'render_root': str(render_root), 'out_dir': str(output), 'pages': len(images), 'images': [str(output / name) for name in names], 'page_text': page_text, 'cjk_glyph_coverage': coverage, 'text_characters': sum(p['characters'] for p in page_text), 'dpi': args.dpi, 'libreoffice_profile_seed': 'minimal-cjk-registry'}

def parser():
    p = argparse.ArgumentParser(description=__doc__)
    commands = p.add_subparsers(dest='command', required=True)
    for name in ('inspect', 'validate', 'annotate', 'finalize', 'sanitize', 'deliver', 'render'):
        sub = commands.add_parser(name)
        sub.add_argument('--input', required=True)
        if name not in ('validate', 'render'):
            sub.add_argument('--out', required=name != 'inspect')
        if name in ('annotate', 'finalize', 'sanitize', 'deliver'):
            sub.add_argument('--overwrite', action='store_true')
        if name == 'inspect':
            sub.add_argument('--summary', action='store_true')
            sub.add_argument('--search')
            sub.add_argument('--location')
            sub.add_argument('--max-items', type=int, default=200)
        if name == 'deliver':
            sub.add_argument('--source')
            sub.add_argument('--replace-source', action='store_true')
        if name == 'annotate':
            sub.add_argument('--spec', required=True)
        if name == 'finalize':
            group = sub.add_mutually_exclusive_group()
            group.add_argument('--accept-changes', action='store_true')
            group.add_argument('--reject-changes', action='store_true')
        if name in ('sanitize', 'finalize'):
            sub.add_argument('--remove-comments', action='store_true')
        if name == 'render':
            sub.add_argument('--out-dir', required=True)
            sub.add_argument('--soffice', default=os.environ.get('PILOTDECK_SOFFICE') or os.environ.get('DOCX_SKILL_SOFFICE'))
            sub.add_argument('--mutool', default=os.environ.get('PILOTDECK_MUTOOL'))
            sub.add_argument('--font-dir', action='append', default=[])
            sub.add_argument('--pdf-renderer', '--pdftoppm', dest='pdftoppm', default=os.environ.get('PILOTDECK_PDF_RENDERER') or os.environ.get('PILOTDECK_PDFTOPPM'))
            sub.add_argument('--dpi', type=int, default=150)
            sub.add_argument('--timeout', type=int, default=180)
    commands.add_parser('self-test')
    compare = commands.add_parser('compare')
    compare.add_argument('--before', required=True)
    compare.add_argument('--after', required=True)
    compare.add_argument('--out', required=True)
    return p

def execute(args):
    if args.command == 'self-test':
        import importlib.util
        import unittest
        tests = Path(__file__).resolve().parents[1] / 'docx' / 'scripts' / 'test_docx_tools.py'
        spec = importlib.util.spec_from_file_location('docx_helper_tests', tests)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        result = unittest.TextTestRunner(stream=sys.stderr).run(unittest.defaultTestLoader.loadTestsFromModule(module))
        if not result.wasSuccessful():
            raise DocxError('DOCX regression tests failed', 'self-test-failed')
        return {'status': 'ok', 'tests': result.testsRun}
    if args.command == 'render':
        return render(args)
    if args.command == 'compare':
        before, after = load(args.before), load(args.after)
        result = compare(inspect(before, args.before), inspect(after, args.after), args.before, args.after)
        target = output_path(args.before, args.out)
        if target.resolve() == Path(args.after).resolve() or (target.exists() and os.path.samefile(target, args.after)):
            raise DocxError('Comparison output would replace input', 'source-protected')
        atomic_bytes(target, json.dumps(result, ensure_ascii=False, indent=2).encode())
        result['out'] = str(target)
        return result
    parts = load(args.input)
    if args.command == 'validate':
        return validation(parts, args.input)
    if args.command == 'inspect':
        result = filter_inspection(inspect(parts, args.input), args)
        if args.out:
            target = output_path(args.input, args.out)
            atomic_bytes(target, json.dumps(result, ensure_ascii=False, indent=2).encode())
            result['out'] = str(target)
        return result
    if args.command == 'annotate':
        internal_path(args.spec, 'control-artifact-outside-work-dir')
        if Path(args.spec).resolve() == Path(args.out).resolve() or (Path(args.out).exists() and os.path.samefile(args.spec, args.out)):
            raise DocxError('Spec and output must be distinct', 'source-protected')
        annotate(parts, json.loads(Path(args.spec).read_text()))
    elif args.command == 'finalize':
        finalize(parts, args.accept_changes, args.reject_changes, args.remove_comments)
    elif args.command == 'sanitize':
        sanitize(parts, args.remove_comments)
    elif args.command == 'deliver':
        source = Path(args.source).resolve() if args.source else None
        raw_output = Path(args.out).absolute()
        replacing = source is not None and (source == raw_output.resolve() or (source.exists() and raw_output.exists() and os.path.samefile(source, raw_output)))
        if replacing and not args.replace_source:
            raise DocxError('Source overwrite requires --replace-source', 'source-overwrite-requires-explicit-mode')
        if args.replace_source and not replacing:
            raise DocxError('--replace-source must target --source exactly', 'source-replacement-path-mismatch')
        backup = None
        work = os.environ.get('PILOTDECK_WORK_DIR')
        if work:
            work = Path(work).resolve()
            if not Path(args.input).resolve().is_relative_to(work):
                raise DocxError('Delivery candidate must be under PILOTDECK_WORK_DIR', 'candidate-not-internal')
            if raw_output.resolve().is_relative_to(work):
                raise DocxError('Delivery output must be outside PILOTDECK_WORK_DIR', 'delivery-output-is-internal')
        source_state = None
        if source is not None:
            def identity(path):
                value = path.stat()
                return (value.st_dev, value.st_ino, value.st_size, value.st_mtime_ns, value.st_ctime_ns)
            before_read = identity(source)
            original = source.read_bytes()
            source_state = identity(source)
            if before_read != source_state:
                raise DocxError('Source changed while taking delivery snapshot', 'source-changed-during-delivery')
        if replacing:
            if not work:
                raise DocxError('Source replacement requires PILOTDECK_WORK_DIR recovery storage', 'source-replacement-requires-work-dir')
            load(source, original)
            recovery = work / 'docx' / 'recovery'
            recovery.mkdir(parents=True, exist_ok=True)
            backup = recovery / (source.stem + '-' + hashlib.sha256(original).hexdigest()[:16] + '.docx')
            if not backup.exists():
                atomic_bytes(backup, original)
            if backup.read_bytes() != original:
                raise DocxError('Source backup mismatch', 'source-backup-mismatch')
        target = output_path(args.input, args.out, args.overwrite or replacing, internal=False)
        source_digest = hashlib.sha256(original).hexdigest() if source is not None else None
        candidate = Path(args.input).read_bytes()
        validated_parts = load(args.input, candidate)
        def verify_source_before_replacement():
            if identity(source) != source_state or source.read_bytes() != original or identity(source) != source_state:
                raise DocxError('Source changed before replacement; new source preserved', 'source-changed-during-delivery')
        if replacing:
            atomic_bytes(target, candidate, True, precommit=verify_source_before_replacement)
        else:
            atomic_bytes(target, candidate, args.overwrite)
        if source is not None and not replacing and hashlib.sha256(source.read_bytes()).hexdigest() != source_digest:
            raise DocxError('The source DOCX changed during delivery', 'source-changed-during-delivery')
        if target.read_bytes() != candidate:
            raise DocxError('Delivered copy differs from the validated candidate', 'delivery-copy-mismatch')
        return {'status': 'ok', 'input': str(Path(args.input).resolve()), 'out': str(target), 'sha256': hashlib.sha256(candidate).hexdigest(), 'source': str(source) if source is not None else None, 'source_replaced': replacing, 'source_backup': {'path': str(backup), 'sha256': hashlib.sha256(backup.read_bytes()).hexdigest()} if backup else None, 'validation': validation(validated_parts, args.input)}
    return save(parts, args.input, args.out, args.overwrite)

def main():
    args = parser().parse_args()
    if args.command == 'render' and os.name == 'posix':
        def interrupted(signum, _frame):
            raise DocxError('Render interrupted by signal ' + str(signum), 'renderer-cancelled')
        signal.signal(signal.SIGTERM, interrupted)
        signal.signal(signal.SIGINT, interrupted)
    try:
        print(json.dumps(execute(args), ensure_ascii=False, indent=2))
        return 0
    except (DocxError, OSError, ValueError) as error:
        print(json.dumps({'status': getattr(error, 'status', 'error'), 'code': getattr(error, 'code', 'io-error'), 'error': str(error), 'command': args.command}), file=sys.stderr)
        return 3

if __name__ == '__main__':
    sys.exit(main())
