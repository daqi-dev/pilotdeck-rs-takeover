#!/usr/bin/env python3
"""Real independent engine checks; pass a new private work directory."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import zipfile
from openpyxl import Workbook, load_workbook

root = Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
helper = Path(__file__).resolve().parents[2] / '_tools' / 'spreadsheet_tools.py'
records = []

def call(name, *arguments, fail=False):
    result = subprocess.run([sys.executable, str(helper), *arguments], text=True, capture_output=True, timeout=180)
    (root / (name + '.stdout.json')).write_text(result.stdout)
    (root / (name + '.stderr.txt')).write_text(result.stderr)
    assert (result.returncode != 0) if fail else (result.returncode == 0), (name, result.returncode, result.stdout, result.stderr)
    records.append({'case': name, 'expectedFailure': fail, 'exit': result.returncode})
    return json.loads(result.stdout) if result.returncode == 0 else None

source = root / 'source.xlsx'
book = Workbook(); sheet = book.active; sheet.title = 'Data'
sheet.append(['Item', 'Amount']); sheet.append(['A', 4]); sheet.append(['B', 6])
sheet['B4'] = '=SUM(B2:B3)'; sheet['C2'] = '=B2*2'; sheet['C3'] = '=IF(B3>5,"YES","NO")'
sheet['A6'] = 'ORIGINAL'; sheet.column_dimensions['A'].width = 22
book.save(source)
source_hash = hashlib.sha256(source.read_bytes()).hexdigest()
validation = call('validate', 'validate', '--input', str(source)); assert len(validation['missingCaches']) == 3
calculated = root / 'recalculated.xlsx'
report = call('recalculate', 'recalculate', '--input', str(source), '--out', str(calculated))
assert report['formulaCount'] == 3
readback = load_workbook(calculated, data_only=True)
assert readback['Data']['B4'].value == 10 and readback['Data']['C2'].value == 8 and readback['Data']['C3'].value == 'YES'
assert load_workbook(calculated, data_only=False)['Data']['B4'].value == '=SUM(B2:B3)'
with zipfile.ZipFile(source) as a, zipfile.ZipFile(calculated) as b:
    assert set(a.namelist()) == set(b.namelist())
    assert [n for n in a.namelist() if a.read(n) != b.read(n)] == ['xl/worksheets/sheet1.xml']
assert call('compare-same', 'compare', '--before', str(source), '--after', str(source))['equal']
assert not call('compare-caches', 'compare', '--before', str(source), '--after', str(calculated))['equal']
call('missing-engine', 'recalculate', '--input', str(source), '--out', str(root/'missing.xlsx'), '--soffice', '/nonexistent/soffice', fail=True)
assert not (root/'missing.xlsx').exists()
# Real dependency change must update caches, not simply preserve the first result.
book['Data']['B2'] = 9; changed = root / 'changed.xlsx'; book.save(changed)
call('changed-input', 'recalculate', '--input', str(changed), '--out', str(root/'changed-cache.xlsx'))
assert load_workbook(root/'changed-cache.xlsx', data_only=True)['Data']['B4'].value == 15
book['Data']['D1'] = '=NO_SUCH_FUNCTION(1)'; invalid = root/'invalid-formula.xlsx'; book.save(invalid)
call('bad-formula', 'recalculate', '--input', str(invalid), '--out', str(root/'bad-cache.xlsx'), fail=True)
assert not (root/'bad-cache.xlsx').exists()
render = call('render', 'render', '--input', str(calculated), '--out-dir', str(root/'render'))
assert render['pages']
final = root/'delivered.xlsx'
call('deliver', 'deliver', '--input', str(calculated), '--out', str(final), '--source', str(source))
assert final.read_bytes() == calculated.read_bytes()
call('no-overwrite', 'deliver', '--input', str(source), '--out', str(final), fail=True)
call('protect-source', 'deliver', '--input', str(calculated), '--out', str(source), '--source', str(source), fail=True)
assert hashlib.sha256(source.read_bytes()).hexdigest() == source_hash
replace = call('replace-source', 'deliver', '--input', str(calculated), '--out', str(source), '--source', str(source), '--replace-source')
assert Path(replace['recovery']).read_bytes() != source.read_bytes()
assert hashlib.sha256(Path(replace['recovery']).read_bytes()).hexdigest() == source_hash
# Corrupt/unsafe package must never publish.
unsafe = root/'unsafe.xlsx'
with zipfile.ZipFile(unsafe, 'w') as archive: archive.writestr('../escape', 'bad')
call('unsafe-package', 'deliver', '--input', str(unsafe), '--out', str(root/'unsafe-delivery.xlsx'), fail=True)
assert not (root/'unsafe-delivery.xlsx').exists()
spec = root/'edit.json'; spec.write_text(json.dumps({'sheets':[{'name':'Data','cells':{'A6':{'value':'EDITED','font':{'bold':True}},'B2':{'value':20,'numberFormat':'0.00'}},'freeze':'B2','autoFilter':'A1:B3','columnWidths':{'B':18}}],'names':{'Amounts':"'Data'!$B$2:$B$3"}}))
call('edit', 'edit', '--input', str(calculated), '--out', str(root/'edited.xlsx'), '--spec', str(spec))
edited = load_workbook(root/'edited.xlsx'); assert edited['Data']['A6'].value == 'EDITED' and edited['Data']['B2'].value == 20
assert edited['Data']['B4'].value == '=SUM(B2:B3)' and edited['Data'].freeze_panes == 'B2'
(root/'results.json').write_text(json.dumps({'status':'passed','cases':records},indent=2)+'\n')
print(json.dumps({'status':'passed','cases':len(records),'render':str(root/'render')}))
