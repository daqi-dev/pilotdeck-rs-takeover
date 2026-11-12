#!/usr/bin/env python3
"""Build basic editable DOCX/XLSX/PPTX from JSON using installed format libraries.

The input schema is deliberately small: title plus paragraphs/tables for DOCX;
sheets[{name,rows}] for XLSX; slides[{title,body}] for PPTX. Does not render.
"""
import argparse
import json
import os
from pathlib import Path
import sys
import tempfile


def build(spec, output):
    output = Path(output)
    suffix = output.suffix.lower()
    if output.exists() or output.is_symlink():
        raise ValueError('output already exists; select a new path')
    if suffix == '.docx':
        from docx import Document
        document = Document()
        if spec.get('title'):
            document.add_heading(str(spec['title']), 0)
        for paragraph in spec.get('paragraphs', []):
            if isinstance(paragraph, str):
                document.add_paragraph(paragraph)
            else:
                text = str(paragraph.get('text', ''))
                if paragraph.get('heading'):
                    document.add_heading(text, level=int(paragraph['heading']))
                else:
                    document.add_paragraph(text)
        for rows in spec.get('tables', []):
            width = max(map(len, rows), default=0)
            if not width:
                continue
            table = document.add_table(rows=len(rows), cols=width)
            for r, row in enumerate(rows):
                for c, value in enumerate(row):
                    table.cell(r, c).text = str(value)
        save = document.save
    elif suffix == '.xlsx':
        from openpyxl import Workbook
        document = Workbook()
        document.remove(document.active)
        sheets = spec.get('sheets', [])
        if not sheets:
            raise ValueError('XLSX requires at least one sheet')
        names = []
        folded = set()
        for item in sheets:
            name = item['name']
            if not isinstance(name, str) or not name or len(name) > 31 or any(char in name for char in '[]:*?/\\') or name.casefold() in folded:
                raise ValueError('sheet name is invalid or duplicated (case-insensitive)')
            folded.add(name.casefold())
            names.append(name)
        for item, name in zip(sheets, names):
            sheet = document.create_sheet(name)
            if sheet.title != name:
                raise ValueError('spreadsheet library renamed the requested sheet')
            for row in item.get('rows', []):
                sheet.append(row)
        save = document.save
    elif suffix == '.pptx':
        from pptx import Presentation
        document = Presentation()
        for item in spec.get('slides', []):
            slide = document.slides.add_slide(document.slide_layouts[1])
            slide.shapes.title.text = str(item.get('title', ''))
            slide.placeholders[1].text = str(item.get('body', ''))
        if not document.slides:
            raise ValueError('PPTX requires at least one slide')
        save = document.save
    else:
        raise ValueError('supported output formats: .docx .xlsx .pptx')
    output.parent.mkdir(parents=True, exist_ok=True)
    # Build privately, then exclusively create destination. No existing output
    # is replaced if another process wins the path while the library saves.
    with tempfile.TemporaryDirectory(prefix='pdrs-artifact-build-') as directory:
        temporary = Path(directory) / ('artifact' + suffix)
        save(temporary)
        created = False
        try:
            with output.open('xb') as target, temporary.open('rb') as source:
                created = True
                import shutil
                shutil.copyfileobj(source, target)
        except BaseException:
            if created:
                output.unlink(missing_ok=True)
            raise
    return {'output': str(output), 'format': suffix[1:], 'rendered': False, 'recalculated': False}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('spec', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    try:
        print(json.dumps(build(json.loads(args.spec.read_text()), args.output), indent=2))
    except (OSError, ValueError, KeyError, ImportError) as error:
        print(f'artifact build failed: {error}', file=sys.stderr)
        raise SystemExit(1)
