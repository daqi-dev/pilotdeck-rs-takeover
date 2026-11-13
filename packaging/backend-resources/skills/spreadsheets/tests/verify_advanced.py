import importlib.util,json,os,sys
from pathlib import Path
from types import SimpleNamespace
from openpyxl import load_workbook
root=Path(sys.argv[1]).resolve()
p=Path(__file__).resolve().parents[2]/'_tools'/'spreadsheet_tools.py'
spec=importlib.util.spec_from_file_location('spreadsheet_tools',p);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
out=root/'extra';out.mkdir()
a=SimpleNamespace(soffice=os.environ['PILOTDECK_SOFFICE'],timeout=120)
source=root/'run-c/recalculated.xlsx'
# Produce a real legacy binary source with the same isolated external engine.
m.engine(['--convert-to','xls:MS Excel 97','--outdir',str(out),str(source)],a,out/'fixture-profile')
a.input=str(out/'recalculated.xls');a.out=str(out/'converted.xlsx')
r=m.convert_legacy(a);assert load_workbook(a.out,data_only=True)['Data']['B4'].value==10
# Make an actual chart through the independent editing routine, recalc it,
# and prove every chart/drawing part retained original bytes.
specfile=out/'chart-edit.json';specfile.write_text(json.dumps({'sheets':[{'name':'Data','charts':[{'kind':'bar','data':'B1:B3','categories':'A2:A3','anchor':'E3','title':'Amounts'}]}]}))
a.input=str(source);a.out=str(out/'chart.xlsx');a.spec=str(specfile);m.edit(a)
a.input=a.out;a.out=str(out/'chart-cached.xlsx');m.recalculate(a)
x=m.package(a.input);y=m.package(a.out)
assert any(k.startswith('xl/charts/') for k in x)
assert all(y[k]==v for k,v in x.items() if k.startswith(('xl/charts/','xl/drawings/')))
# Existing charts must not be silently round-tripped through openpyxl.
a.input=str(out/'chart-cached.xlsx');a.out=str(out/'forbidden-edit.xlsx')
try:m.edit(a)
except ValueError:pass
else:raise AssertionError('sensitive template edit should fail')
assert not Path(a.out).exists()
(out/'results.json').write_text(json.dumps({'status':'passed','cases':['real-xls-conversion','chart-edit','chart-cache-preserves-drawings','sensitive-edit-refused']},indent=2)+'\n')
print('extra passed')
