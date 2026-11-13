#!/usr/bin/env python3
"""One offline ReportLab builder: edit content and layout here, then rerun."""
import argparse
from pathlib import Path
import os
import tempfile
from xml.sax.saxutils import escape
from reportlab.lib import colors
from reportlab.lib.pagesizes import A4
from reportlab.lib.styles import ParagraphStyle
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.ttfonts import TTFont
from reportlab.platypus import SimpleDocTemplate, Paragraph, Spacer, Table, TableStyle


def document_font():
    requested = os.environ.get('PILOTDECK_PDF_FONT')
    candidates = [requested] if requested else ['/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf', '/System/Library/Fonts/Supplemental/Arial.ttf', '/Library/Fonts/Arial.ttf', 'C:/Windows/Fonts/arial.ttf']
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            pdfmetrics.registerFont(TTFont('DocumentFont', candidate))
            return 'DocumentFont'
    if requested:
        raise ValueError('requested font does not exist')
    # An explicit PDF Base14 fallback, not an embedded Unicode font.
    return 'Helvetica'


def build(output):
    output = Path(output)
    if output.exists() or output.is_symlink():
        raise ValueError('output must be new')
    output.parent.mkdir(parents=True, exist_ok=True)
    font = document_font()
    body = ParagraphStyle('Body', fontName=font, fontSize=11, leading=16, spaceAfter=10)
    title = ParagraphStyle('Title', parent=body, fontSize=24, leading=29, spaceAfter=20, textColor=colors.HexColor('#164763'))
    heading = ParagraphStyle('Heading', parent=body, fontSize=15, leading=20, spaceBefore=14, keepWithNext=True)
    # Keep content separate from painting callbacks; paragraphs wrap and flow.
    sections = [('Repeatable generation', 'Edit this single Python builder to change your document. ReportLab flowables wrap paragraphs and paginate tables; keep the source content and calculations easy to inspect.'), ('Verification', 'After building, audit the PDF and render every final page. Inspect full-size images for clipping, missing glyphs and incorrect page breaks. Extraction alone does not prove appearance.')]
    story = [Paragraph('PilotDeck PDF', title)]
    for name, text in sections:
        story.extend([Paragraph(escape(name), heading), Paragraph(escape(text), body)])
    rows = [[Paragraph(escape(value), body) for value in row] for row in [('Stage', 'Evidence'), ('Build', 'One editable offline builder'), ('Audit', 'Readable structure and intentional geometry'), ('Render', 'Every output page checked visually')]]
    story.extend([Spacer(1, 10), Table(rows, colWidths=[105, 390], repeatRows=1, style=TableStyle([('VALIGN', (0,0), (-1,-1), 'TOP'), ('BACKGROUND', (0,0), (-1,0), colors.HexColor('#e8f1f5')), ('GRID', (0,0), (-1,-1), 0.5, colors.HexColor('#aabcc6')), ('LEFTPADDING', (0,0), (-1,-1), 8), ('RIGHTPADDING', (0,0), (-1,-1), 8), ('TOPPADDING', (0,0), (-1,-1), 8)]))])
    def footer(canvas, document):
        canvas.saveState()
        canvas.setFont(font, 9)
        canvas.setFillColor(colors.HexColor('#465963'))
        canvas.drawString(document.leftMargin, 28, 'PilotDeck PDF')
        canvas.drawRightString(A4[0] - document.rightMargin, 28, f'Page {document.page}')
        canvas.restoreState()
    with tempfile.TemporaryDirectory(prefix='pilotdeck-reportlab-') as work:
        temporary = Path(work) / 'document.pdf'
        document = SimpleDocTemplate(str(temporary), pagesize=A4, leftMargin=50, rightMargin=50, topMargin=45, bottomMargin=55, title='PilotDeck PDF', author='PilotDeck')
        document.build(story, onFirstPage=footer, onLaterPages=footer)
        with output.open('xb') as destination:
            destination.write(temporary.read_bytes())


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', required=True)
    build(parser.parse_args().out)
