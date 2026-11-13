"""Shared references and actual engine result types; use a new output directory."""
import importlib.util
import json
from pathlib import Path
import os
import sys
from types import SimpleNamespace
import xml.etree.ElementTree as ET
from openpyxl import Workbook, load_workbook

p = Path(__file__).resolve().parents[2] / '_tools' / 'spreadsheet_tools.py'
s = importlib.util.spec_from_file_location('spreadsheet_types', p)
m = importlib.util.module_from_spec(s); s.loader.exec_module(m)
root = Path(sys.argv[1]).resolve(); root.mkdir(parents=True)
book = Workbook(); sheet = book.active; sheet.title = 'Types'
sheet['B2'] = 2; sheet['B3'] = 3
for address, value in {'D2':'=B2*2','D3':'=B3*2','E2':'=DATE(2025,1,1)','E3':'=1/0','E4':'=""','E5':'=TRUE()','E6':'=0'}.items():
    sheet[address] = value
source = root/'source.xlsx'; book.save(source)
parts = m.package(source)
part = 'xl/worksheets/sheet1.xml'
xml = ET.fromstring(parts[part])
for cell in xml.iter('{'+m.MAIN+'}c'):
    if cell.get('r') in {'D2','D3'}:
        formula = cell.find('{'+m.MAIN+'}f'); formula.set('t','shared'); formula.set('si','0')
        if cell.get('r') == 'D2': formula.set('ref','D2:D3')
        else: formula.text = None
parts[part] = ET.tostring(xml,encoding='utf-8'); source.write_bytes(m.archive_bytes(parts))
a = SimpleNamespace(input=str(source),out=str(root/'cached.xlsx'),soffice=os.environ['PILOTDECK_SOFFICE'],timeout=120)
report = m.recalculate(a)
result = m.cells(m.package(a.out))
assert result['Types!D2']['value'] == '4' and result['Types!D3']['value'] == '6'
assert result['Types!D3']['formula'] == '' and result['Types!D3']['formulaAttributes'] == {'t':'shared','si':'0'}
assert result['Types!E3']['type'] == 'e' and result['Types!E3']['value'].startswith('#DIV/0!')
assert result['Types!E4']['type'] == 'str' and result['Types!E4']['value'] == ''
assert result['Types!E5']['type'] == 'b' and result['Types!E5']['value'] == '1'
assert result['Types!E6']['value'] == '0'
assert float(result['Types!E2']['value']) > 45000
(root/'results.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps({'status':'passed','formulas':len(report['cachedResults'])}))
