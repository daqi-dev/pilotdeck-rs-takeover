#!/usr/bin/env python3
"""Inspect PDF structure or write selected/rotated pages to a distinct file.

Requires separately installed pypdf. Not a renderer or AcroForm appearance editor.
"""
import argparse
import json
from pathlib import Path
import sys


def operate(source, output=None, pages=None, rotate=0):
    from pypdf import PdfReader, PdfWriter
    source = Path(source)
    reader = PdfReader(source)
    if reader.is_encrypted:
        raise ValueError('encrypted PDF requires a separately authorized password workflow')
    result = {'pages': len(reader.pages), 'pageBoxes': [list(map(float, page.mediabox)) for page in reader.pages], 'fields': sorted((reader.get_fields() or {}).keys()), 'rendered': False}
    if output is None:
        return result
    output = Path(output)
    if output.exists() or output.is_symlink() or output.absolute() == source.absolute():
        raise ValueError('output must be a new path distinct from input')
    chosen = list(range(len(reader.pages))) if pages is None else pages
    if any(index < 1 or index > len(reader.pages) for index in chosen) and pages is not None:
        raise ValueError('page numbers are 1-based and must exist')
    if pages is not None:
        chosen = [index - 1 for index in chosen]
    if rotate % 90:
        raise ValueError('rotation must be a multiple of 90')
    writer = PdfWriter()
    for index in chosen:
        page = reader.pages[index]
        if rotate:
            page.rotate(rotate)
        writer.add_page(page)
    # No in-place truncation; exclusive destination creation is required.
    created = False
    try:
        with output.open('xb') as stream:
            created = True
            writer.write(stream)
    except BaseException:
        if created:
            output.unlink(missing_ok=True)
        raise
    result['output'] = str(output)
    result['outputPages'] = len(chosen)
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('input', type=Path)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--pages', help='comma-separated 1-based pages in desired order')
    parser.add_argument('--rotate', type=int, default=0)
    args = parser.parse_args()
    try:
        pages = [int(value) for value in args.pages.split(',')] if args.pages else None
        print(json.dumps(operate(args.input, args.output, pages, args.rotate), indent=2))
    except (OSError, ValueError, ImportError) as error:
        print(f'PDF operation failed: {error}', file=sys.stderr)
        raise SystemExit(1)
