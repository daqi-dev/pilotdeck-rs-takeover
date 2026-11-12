#!/usr/bin/env python3
"""Read-only OOXML/CSV structural inventory; no archive extraction or rendering."""
import argparse
import csv
import io
import json
from pathlib import Path
import sys
import zipfile
import xml.etree.ElementTree as ET

MAX_BYTES = 32 * 1024 * 1024
MAX_MEMBERS = 20000


def inspect(path):
    path = Path(path)
    if path.suffix.lower() in {'.csv', '.tsv'}:
        if path.stat().st_size > MAX_BYTES:
            raise ValueError('delimited input exceeds 32 MiB')
        text = path.read_text(encoding='utf-8-sig')
        rows = list(csv.reader(io.StringIO(text), delimiter='\t' if path.suffix.lower() == '.tsv' else ','))
        widths = sorted({len(row) for row in rows})
        return {'format': path.suffix.lower()[1:], 'rows': len(rows), 'columnCounts': widths, 'preview': rows[:10]}
    kind = path.suffix.lower()[1:]
    roots = {'docx': 'word/', 'pptx': 'ppt/', 'xlsx': 'xl/'}
    if kind not in roots:
        raise ValueError('supported inputs: .docx .pptx .xlsx .csv .tsv')
    with zipfile.ZipFile(path) as archive:
        entries = archive.infolist()
        if len(entries) > MAX_MEMBERS or sum(item.file_size for item in entries) > MAX_BYTES:
            raise ValueError('expanded OOXML inventory exceeds limits')
        names = [item.filename for item in entries]
        if len(names) != len(set(names)):
            raise ValueError('duplicate OOXML member')
        if '[Content_Types].xml' not in names:
            raise ValueError('OOXML content types missing')
        texts, formulas = {}, {}
        for entry in entries:
            name = entry.filename
            if not name.startswith(roots[kind]) or not name.endswith('.xml'):
                continue
            raw = archive.read(entry)
            # Entity declarations are not needed in OOXML and must not cause
            # expansion of attacker-supplied internal DTDs.
            if b'<!DOCTYPE' in raw.upper() or b'<!ENTITY' in raw.upper():
                raise ValueError('DTD/entity declarations are not admitted')
            tree = ET.fromstring(raw)
            strings = [el.text for el in tree.iter() if el.tag.rsplit('}', 1)[-1] in {'t', 'v'} and el.text]
            if strings:
                texts[name] = strings
            fs = [el.text or '' for el in tree.iter() if el.tag.rsplit('}', 1)[-1] == 'f']
            if fs:
                formulas[name] = fs
        return {'format': kind, 'members': names, 'text': texts, 'formulas': formulas, 'rendered': False, 'recalculated': False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('input', type=Path)
    args = parser.parse_args()
    try:
        print(json.dumps(inspect(args.input), ensure_ascii=False, indent=2))
    except (OSError, ValueError, zipfile.BadZipFile, ET.ParseError, RuntimeError) as error:
        print(f'artifact inspection failed: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
