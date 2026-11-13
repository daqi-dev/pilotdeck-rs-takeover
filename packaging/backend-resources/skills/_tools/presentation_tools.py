#!/usr/bin/env python3
"""Native PPTX package review and safe delivery; office conversion is explicit."""
import argparse
import hashlib
import json
import io
import math
import html
import os
from pathlib import Path, PurePosixPath
import posixpath
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
from urllib.parse import unquote, urlsplit
import xml.etree.ElementTree as ET
import zipfile

OLE = bytes.fromhex('d0cf11e0a1b11ae1')
PML = 'http://schemas.openxmlformats.org/presentationml/2006/main'
REL = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
MAIN_TYPE = 'application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml'
SLIDE_TYPE = 'application/vnd.openxmlformats-officedocument.presentationml.slide+xml'


def digest(data):
    return hashlib.sha256(data).hexdigest()


def local(tag):
    return tag.rsplit('}', 1)[-1]


def xml(data, part):
    if b'<!DOCTYPE' in data.upper() or b'<!ENTITY' in data.upper():
        raise ValueError('DTD/entity declarations are not allowed in ' + part)
    try:
        return ET.fromstring(data)
    except ET.ParseError as error:
        raise ValueError(f'invalid XML in {part}: {error}') from error


def safe_part(name):
    if not name or '\\' in name or name.startswith('/') or any(p in ('', '.', '..') for p in name.split('/')):
        raise ValueError('unsafe OOXML part name: ' + name)
    return name


def relationship_owner(part):
    if part == '_rels/.rels':
        return ''
    folder, name = posixpath.split(part)
    if not folder.endswith('/_rels') or not name.endswith('.rels'):
        raise ValueError('invalid relationship part: ' + part)
    return posixpath.join(folder[:-6], name[:-5])


def target_part(owner, target):
    parsed = urlsplit(target)
    if parsed.scheme or parsed.netloc or '\\' in target:
        raise ValueError('internal relationship target is not a package path: ' + target)
    decoded = unquote(parsed.path)
    joined = posixpath.normpath(posixpath.join(posixpath.dirname(owner), decoded.lstrip('/')) if not decoded.startswith('/') else decoded[1:])
    return safe_part(joined)


def read_package(source):
    source = Path(source).resolve(strict=True)
    raw = source.read_bytes()
    if raw.startswith(OLE):
        raise ValueError('binary PPT requires convert-legacy before OOXML review')
    try:
        with zipfile.ZipFile(io.BytesIO(raw)) as archive:
            parts = {}
            infos = []
            for info in archive.infolist():
                if info.is_dir():
                    continue
                name = safe_part(info.filename)
                if name in parts:
                    raise ValueError('duplicate OOXML part: ' + name)
                if stat.S_ISLNK(info.external_attr >> 16):
                    raise ValueError('symlink OOXML part: ' + name)
                parts[name] = archive.read(info)
                infos.append(info)
    except (zipfile.BadZipFile, RuntimeError) as error:
        raise ValueError('unreadable PPTX ZIP package: ' + str(error)) from error
    for part in ('[Content_Types].xml', '_rels/.rels'):
        if part not in parts:
            raise ValueError('missing required OOXML part: ' + part)
    parsed = {name: xml(data, name) for name, data in parts.items() if name.endswith(('.xml', '.rels'))}
    types = parsed['[Content_Types].xml']
    if types.tag != '{http://schemas.openxmlformats.org/package/2006/content-types}Types':
        raise ValueError('invalid content-types root')
    overrides = {}
    defaults = {}
    for item in types:
        if local(item.tag) == 'Override':
            name = safe_part(item.attrib.get('PartName', '').lstrip('/'))
            if name not in parts or name in overrides:
                raise ValueError('missing/duplicate content-type override: ' + name)
            if not item.attrib.get('ContentType'):
                raise ValueError('content-type override is empty: ' + name)
            overrides[name] = item.attrib.get('ContentType')
        elif local(item.tag) == 'Default':
            extension = item.attrib.get('Extension', '')
            if not extension or extension in defaults:
                raise ValueError('invalid/duplicate default content-type')
            if not item.attrib.get('ContentType'):
                raise ValueError('default content-type is empty')
            defaults[extension] = item.attrib.get('ContentType')
    for name in parts:
        if name != '[Content_Types].xml' and name not in overrides and name.rsplit('.', 1)[-1] not in defaults:
            raise ValueError('part has no content-type: ' + name)
    relationships = {}
    relation_count = 0
    for name, root in parsed.items():
        if not name.endswith('.rels'):
            continue
        if root.tag != '{http://schemas.openxmlformats.org/package/2006/relationships}Relationships':
            raise ValueError('invalid relationships root: ' + name)
        owner = relationship_owner(name)
        if owner and owner not in parts:
            raise ValueError('relationship owner is missing: ' + owner)
        by_id = {}
        for item in root:
            if local(item.tag) != 'Relationship':
                continue
            key, kind, target = (item.attrib.get(k, '') for k in ('Id', 'Type', 'Target'))
            if not key or key in by_id or not kind or not target:
                raise ValueError('invalid/duplicate relationship in ' + name)
            external = item.attrib.get('TargetMode') == 'External'
            resolved = target if external else target_part(owner, target)
            if not external and resolved not in parts:
                raise ValueError(f'missing relationship target {resolved} from {name}')
            by_id[key] = {'type': kind, 'target': resolved, 'external': external}
            relation_count += 1
        relationships[owner] = by_id
    mains = [r['target'] for r in relationships[''].values() if r['type'].endswith('/officeDocument') and not r['external']]
    if len(mains) != 1 or overrides.get(mains[0]) != MAIN_TYPE:
        raise ValueError('package must contain exactly one PPTX presentation main part')
    main = mains[0]
    namespaces=(PML,'http://purl.oclc.org/ooxml/presentationml/main')
    if main not in parsed or parsed[main].tag not in {f'{{{ns}}}presentation' for ns in namespaces}:
        raise ValueError('main XML root is not a presentation')
    slides = []
    used = set()
    for item in parsed[main].iter():
        if local(item.tag) != 'sldId':
            continue
        rid = next((value for key, value in item.attrib.items() if key.endswith('}id')), None)
        relationship = relationships.get(main, {}).get(rid)
        if not relationship or relationship['external'] or not relationship['type'].endswith('/slide'):
            raise ValueError('slide id does not reference an internal slide')
        part = relationship['target']
        if part in used or overrides.get(part) != SLIDE_TYPE or part not in parsed or parsed[part].tag not in {f'{{{ns}}}sld' for ns in namespaces}:
            raise ValueError('invalid or duplicate slide part: ' + part)
        used.add(part)
        notes = [r['target'] for r in relationships.get(part, {}).values() if r['type'].endswith('/notesSlide') and not r['external']]
        if any(note not in parsed or local(parsed[note].tag)!='notes' for note in notes):
            raise ValueError('invalid notes slide relationship')
        text = lambda root: '\n'.join(node.text or '' for node in root.iter() if local(node.tag) == 't')
        slides.append({'number': len(slides) + 1, 'part': part, 'text': text(parsed[part]), 'notes': '\n'.join(text(parsed[note]) for note in notes)})
    if not slides:
        raise ValueError('presentation must contain at least one referenced slide')
    report = {'status': 'ok', 'input': str(source), 'sha256': digest(raw), 'package': {'readable': True, 'partCount': len(parts), 'relationshipCount': relation_count}, 'presentation': {'slideCount': len(slides), 'notesSlideCount': sum('/notesSlides/notesSlide' in n and n.endswith('.xml') for n in parts), 'chartCount': sum(content_type=='application/vnd.openxmlformats-officedocument.drawingml.chart+xml' for content_type in overrides.values()), 'mediaCount': sum(n.startswith('ppt/media/') for n in parts)}, 'slides': slides}
    return report, parts, infos


def work_path(value):
    configured = os.environ.get('PILOTDECK_WORK_DIR')
    if not configured:
        raise ValueError('PILOTDECK_WORK_DIR is required for internal PPTX artifacts')
    work, target = Path(configured).resolve(), Path(value).resolve()
    if not target.is_relative_to(work):
        raise ValueError('internal artifact must be under PILOTDECK_WORK_DIR')
    return target


def publish(data, output, overwrite=False, mode=0o644, precommit=None):
    output = Path(output).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix='.' + output.name + '.', dir=output.parent)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, mode & 0o777)
        if precommit is not None:
            precommit()
        if overwrite:
            os.replace(temporary, output)
        else:
            os.link(temporary, output)
        if digest(output.read_bytes()) != digest(data):
            raise ValueError('published bytes differ from the validated candidate')
    finally:
        Path(temporary).unlink(missing_ok=True)
    return output


def replace_text(source, output, before, after):
    """Localized run-text edit for task scripts. Never flattens a presentation."""
    source, output = Path(source).resolve(), work_path(output)
    if source == output or not before:
        raise ValueError('text edit requires nonempty search and a separate candidate')
    _, parts, infos = read_package(source)
    if any(name.startswith('_xmlsignatures/') for name in parts):
        raise ValueError('editing a signed package requires an explicit signature strategy')
    changed = 0
    for name in list(parts):
        if not re.fullmatch(r'ppt/slides/slide\d+\.xml', name):
            continue
        # Rewrite only text-run payload bytes: do not reserialize slide XML,
        # which could remove namespace declarations referenced by mc:Ignorable.
        expression=re.compile(rb'(<(?P<tag>(?:[A-Za-z_][\w.-]*:)?t)(?:\s[^>]*)?>)(?P<body>.*?)(</(?P=tag)\s*>)',re.DOTALL)
        hits=[0]
        def edit(match):
            body=match.group('body').decode('utf-8')
            if body.startswith('<![CDATA[') and body.endswith(']]>'):
                text=body[9:-3]
            elif '<' not in body:
                text=html.unescape(body)
            else:
                return match.group(0)
            if before not in text:
                return match.group(0)
            hits[0]+=text.count(before)
            replaced=text.replace(before,after).replace('&','&amp;').replace('<','&lt;').replace('>','&gt;')
            return match.group(1)+replaced.encode('utf-8')+match.group(4)
        revised=expression.sub(edit,parts[name])
        if hits[0]:
            xml(revised,name)
            parts[name]=revised
            changed+=hits[0]
    if not changed:
        raise ValueError('search text was not found inside a slide text run')
    work=Path(os.environ['PILOTDECK_WORK_DIR']).resolve()
    work.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='pptx-edit-',dir=work) as temporary:
        candidate = Path(temporary) / 'candidate.pptx'
        with zipfile.ZipFile(candidate, 'w') as archive:
            for info in infos:
                archive.writestr(info, parts[info.filename])
        read_package(candidate)
        publish(candidate.read_bytes(), output)
    return {'status': 'ok', 'output': str(output), 'replacements': changed}


def compare(source, candidate):
    left, a, _ = read_package(source)
    right, b, _ = read_package(work_path(candidate))
    if Path(source).resolve() == Path(candidate).resolve():
        raise ValueError('source and candidate must be distinct')
    modified = [name for name in sorted(a.keys() & b.keys()) if a[name] != b[name]]
    slides = []
    for index in range(max(len(left['slides']), len(right['slides']))):
        before = left['slides'][index] if index < len(left['slides']) else None
        after = right['slides'][index] if index < len(right['slides']) else None
        if before != after:
            slides.append({'number': index + 1, 'source': before, 'candidate': after})
    return {'status': 'ok', 'changed': left['sha256'] != right['sha256'], 'source': {'path': left['input'], 'sha256': left['sha256']}, 'candidate': {'path': right['input'], 'sha256': right['sha256']}, 'summary': {'slideCountChanged': len(left['slides']) != len(right['slides']), 'changedSlidePositions': len(slides), 'modifiedPartCount': len(modified)}, 'slides': slides, 'packageParts': {'added': sorted(b.keys() - a.keys()), 'removed': sorted(a.keys() - b.keys()), 'modified': modified}, 'judgment': 'Package/text/notes differences are observations, not a visual pass/fail verdict.'}


def executable(explicit, *variables):
    value = explicit or next((os.environ[key] for key in variables if os.environ.get(key)), None)
    if not value or not Path(value).is_absolute() or not Path(value).is_file() or not os.access(value, os.X_OK):
        raise ValueError('configure an explicit executable path using ' + variables[0])
    return str(Path(value).resolve())


def run(command, timeout=None):
    process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=(os.name == 'posix'))
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except BaseException:
        # A reaped group leader can leave pipe-holding descendants alive.
        # The group belongs to this invocation even after its leader exits.
        try:
            if os.name == 'posix':
                os.killpg(process.pid, signal.SIGKILL)
            elif process.poll() is None:
                process.kill()
        except ProcessLookupError:
            pass
        try:
            process.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            # An engine that escaped our group must not hold cleanup hostage.
            process.stdout.close()
            process.stderr.close()
            process.wait(timeout=2)
        raise
    if process.returncode:
        raise ValueError(f'engine exited {process.returncode}: ' + (stderr or stdout).decode('utf-8', 'replace').strip())


def office_convert(source, directory, kind, office, timeout):
    profile = directory / 'profile'
    profile.mkdir()
    converted = directory / 'converted'
    converted.mkdir()
    run([office, '-env:UserInstallation=' + profile.as_uri(), '--headless', '--convert-to', kind, '--outdir', str(converted), str(source)], timeout)
    suffix = '.pdf' if kind == 'pdf' else '.pptx'
    matches = list(converted.glob('*' + suffix))
    if len(matches) != 1 or not matches[0].is_file() or matches[0].stat().st_size == 0:
        raise ValueError('LibreOffice did not produce exactly one nonempty ' + suffix)
    return matches[0]


def render(source, output, dpi, office, renderer, timeout=None):
    report, _, _ = read_package(source)
    source = Path(source).resolve()
    output = work_path(output)
    if output.exists():
        raise ValueError('render output directory already exists; choose a new directory')
    if not math.isfinite(dpi) or dpi <= 0:
        raise ValueError('dpi must be finite and greater than zero')
    try:
        from PIL import Image
    except ImportError as error:
        raise ValueError('Pillow is required to verify actual rendered PNG images') from error
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='pptx-render-', dir=output.parent) as scratch:
        scratch = Path(scratch)
        pdf = office_convert(source, scratch, 'pdf', office, timeout)
        if not pdf.read_bytes().startswith(b'%PDF-'):
            raise ValueError('LibreOffice produced a non-PDF file')
        images = scratch / 'slides'
        images.mkdir()
        stem = Path(renderer).name.lower()
        if 'pdftoppm' in stem:
            command = [renderer, '-png', '-r', str(dpi), str(pdf), str(images / 'slide')]
        elif 'mutool' in stem:
            command = [renderer, 'draw', '-r', str(dpi), '-o', str(images / 'slide-%d.png'), str(pdf)]
        else:
            raise ValueError('unsupported renderer; configure pdftoppm or mutool')
        run(command, timeout)
        slides = sorted(images.glob('slide-*.png'), key=lambda name: int(re.fullmatch(r'slide-(\d+)\.png', name.name)[1]))
        if len(slides) != report['presentation']['slideCount']:
            raise ValueError('rendered slide count differs from presentation order')
        for slide in slides:
            try:
                with Image.open(slide) as image:
                    if image.format!='PNG' or image.width<=0 or image.height<=0:
                        raise ValueError('renderer produced invalid PNG dimensions')
                    image.verify()
            except (OSError,ValueError) as error:
                raise ValueError('renderer produced invalid PNG: '+slide.name) from error
        if digest(source.read_bytes())!=report['sha256']:
            raise ValueError('source changed during rendering')
        output.mkdir()
        try:
            for slide in slides:
                shutil.copyfile(slide,output/slide.name)
        except BaseException:
            shutil.rmtree(output)
            raise
    return {'status': 'ready', 'input': str(source), 'output': str(output), 'dpi': dpi, 'slideCount': len(slides), 'slides': [str(output / slide.name) for slide in slides], 'engine': office, 'rasterizer': renderer, 'compatibilityNote': 'LibreOffice may substitute fonts; inspect every rendered slide.'}


def deliver(source, output, original=None, replace_source=False, overwrite=False):
    source = work_path(source)
    output = Path(output).resolve()
    work = Path(os.environ['PILOTDECK_WORK_DIR']).resolve()
    original = Path(original).resolve(strict=True) if original else None
    if any(item.suffix.lower()!='.pptx' for item in [source,output]+([original] if original else [])):
        raise ValueError('PPTX delivery input, output and source require .pptx extensions')
    if source == output or (original and source == original) or output.is_relative_to(work):
        raise ValueError('delivery requires a separate internal candidate and final path outside the work directory')
    replacing = original == output
    if bool(replace_source) != bool(replacing):
        raise ValueError('replacing the original requires --source and --replace-source together')
    report, _, _ = read_package(source)
    data = source.read_bytes()
    if digest(data)!=report['sha256']:
        raise ValueError('candidate changed after validation')
    def source_identity():
        stat = original.stat()
        return (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns)
    identity = source_identity() if original else None
    prior = original.read_bytes() if original else None
    def check_source():
        if original and (source_identity() != identity or original.read_bytes() != prior):
            raise ValueError('source changed before delivery commit; recovery retained')
    check_source()
    recovery = None
    if replacing:
        backup = work / 'pptx' / 'recovery' / (original.stem + '-' + digest(prior)[:16] + original.suffix)
        if backup.exists():
            if backup.read_bytes() != prior:
                raise ValueError('existing source recovery bytes do not match')
        else:
            publish(prior, backup, mode=original.stat().st_mode)
        recovery = {'path': str(backup), 'sha256': digest(prior)}
    publish(data, output, overwrite=(overwrite or replacing), mode=source.stat().st_mode, precommit=check_source)
    if original and not replacing and original.read_bytes() != prior:
        raise ValueError('source changed during delivery')
    return {'status': 'ok', 'input': str(source), 'output': str(output), 'sha256': digest(data), 'source': str(original) if original else None, 'sourceReplaced': replacing, 'recovery': recovery, 'structure': report['presentation']}


def convert(source, output, office, force=False, timeout=None):
    source = Path(source).resolve(strict=True)
    output = work_path(output)
    if source == output or output.suffix.lower() != '.pptx':
        raise ValueError('legacy conversion requires a separate .pptx output')
    if not source.read_bytes().startswith(OLE):
        raise ValueError('legacy conversion requires a binary PowerPoint .ppt input')
    before = source.read_bytes()
    work=Path(os.environ['PILOTDECK_WORK_DIR']).resolve()
    work.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='pptx-convert-', dir=work) as directory:
        staged=Path(directory)/source.name
        staged.write_bytes(before)
        converted = office_convert(staged, Path(directory), 'pptx:Impress MS PowerPoint 2007 XML', office, timeout)
        report, _, _ = read_package(converted)
        data = converted.read_bytes()
        if source.read_bytes() != before:
            raise ValueError('source changed during legacy conversion')
        publish(data, output, overwrite=force)
    return {'status': 'converted_with_warnings', 'source': {'path': str(source), 'sha256': digest(before), 'preserved': True}, 'output': {'path': str(output), 'sha256': digest(data), 'slideCount': report['presentation']['slideCount']}, 'warnings': ['Binary PPT conversion may change fonts, animation, layout, embedded objects and editable features; review every slide.']}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    for name in ('validate', 'render', 'compare', 'deliver', 'convert-legacy'):
        command = sub.add_parser(name)
        command.add_argument('--input', required=name not in ('compare',))
        if name in ('validate', 'compare'):
            command.add_argument('--out')
        if name == 'render':
            command.add_argument('--out-dir', required=True)
            command.add_argument('--dpi', type=float, default=144)
        if name == 'compare':
            command.add_argument('--source', required=True)
            command.add_argument('--candidate', required=True)
        if name == 'deliver':
            command.add_argument('--out', required=True)
            command.add_argument('--source')
            command.add_argument('--replace-source', action='store_true')
            command.add_argument('--overwrite', action='store_true')
        if name == 'convert-legacy':
            command.add_argument('--out', required=True)
            command.add_argument('--force', action='store_true')
        if name in ('render', 'convert-legacy'):
            command.add_argument('--soffice')
            command.add_argument('--timeout', type=float)
        if name == 'render':
            command.add_argument('--pdf-renderer')
    args = parser.parse_args(argv)
    timeout=getattr(args,'timeout',None)
    if timeout is not None and (not math.isfinite(timeout) or timeout<=0):
        raise ValueError('timeout must be finite and greater than zero')
    if args.command == 'validate':
        result = read_package(args.input)[0]
    elif args.command == 'compare':
        result = compare(args.source, args.candidate)
    elif args.command == 'deliver':
        result = deliver(args.input, args.out, args.source, args.replace_source, args.overwrite)
    else:
        office = executable(args.soffice, 'PILOTDECK_SOFFICE', 'PPTX_SKILL_SOFFICE')
        if args.command == 'render':
            renderer = executable(args.pdf_renderer, 'PILOTDECK_PDF_RENDERER', 'PPTX_SKILL_PDF_RENDERER')
            result = render(args.input, args.out_dir, args.dpi, office, renderer, args.timeout)
        else:
            result = convert(args.input, args.out, office, args.force, args.timeout)
    if args.command in ('validate', 'compare') and args.out:
        output = work_path(args.out)
        inputs = [Path(v).resolve() for v in [args.input, getattr(args, 'source', None), getattr(args, 'candidate', None)] if v]
        if output in inputs:
            raise ValueError('report must be distinct from presentation inputs')
        publish((json.dumps(result, ensure_ascii=False, indent=2) + '\n').encode(), output,overwrite=True)
        result['report'] = str(output)
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (ValueError, OSError, zipfile.BadZipFile, subprocess.SubprocessError) as error:
        print('pptx: ' + str(error), file=sys.stderr)
        raise SystemExit(1)
