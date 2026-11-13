import sys
if not sys.flags.isolated:raise SystemExit('run with python3 -I')
sys.dont_write_bytecode = True
import tempfile
sys.pycache_prefix = tempfile.mkdtemp(prefix='pdrs-deadline-controls-pyc-')
import importlib.util,copy,json,itertools,hashlib
from pathlib import Path
HERE=Path(__file__).resolve().parent
if len(sys.argv)!=3:raise SystemExit('usage: python3 -I controls/deadline_accepted.py EVIDENCE_ROOT NEW_OUTPUT_DIRECTORY')
ROOT=Path(sys.argv[1]).resolve();OUTPUT=Path(sys.argv[2]);OUTPUT.mkdir(parents=True,exist_ok=False)
s=importlib.util.spec_from_file_location('deadline_accepted_controls',HERE.parent/'deadline_accepted.py');D=importlib.util.module_from_spec(s);sys.modules[s.name]=D;s.loader.exec_module(D)
M=D.DeadlineAccepted(ROOT)
def case(sid,side,round):return next(c for c in M.CASES if c['scenario']==sid and c['side']==side and c['round']==round)
def pair(sid='deadline_during_tool',round=5):
 cs=[copy.deepcopy(case(sid,side,round)) for side in ('ts','rust')];return cs,[M.rows(c) for c in cs],[M.ledger(c) for c in cs]
def run(cs,rs,ls):return M._evaluate_registered_records(*cs,*rs,*ls)
def find(rs,kind):return next(r for r in rs if r.get('kind')==kind)
def extra(rs):return next(r for r in rs if r.get('event')=='gateway_submit_failed')
def renumber(rs):
 for i,r in enumerate(rs):r['sequence']=i
results=[];actual=[]
# Full fixed original matrix: 5TSx5Rust for each scene, and reverse; no new captures.
for sid in ('deadline','deadline_during_tool'):
 for ti,ri in itertools.product(range(1,6),repeat=2):
  cs=[case(sid,'ts',ti),case(sid,'rust',ri)]
  for order in (cs,cs[::-1]):
   r=M.evaluate_files(*(M.resolve(c['path']) for c in order));assert r['accepted_verdict']=='PASS',r
   assert r['adaptation_applied']==bool(order[0]['extra_count']!=order[1]['extra_count'])
   if r['adaptation_applied']:
    assert r['strict_verdict']=='FAIL' and r['strict']['triggered_rules']==['E10','E11','G03_channels']
    assert len(r['admitted_records'])==1
    admitted=r['admitted_records'][0];owner=order[0 if admitted['side']=='left' else 1];line=D.G.jsonl_lines(M.resolve(owner['path']).read_text())[admitted['raw_index']];assert json.loads(line)==admitted['record'] and D.digest(line.encode())==admitted['raw_line_sha256_without_newline']
   actual.append({'scene':sid,'sides':[(c['side'],c['round']) for c in order],**r})
# Original registered self comparisons unchanged and require no optional admission.
for c in M.CASES:
 r=M.evaluate_files(M.resolve(c['path']),M.resolve(c['path']));assert r['strict_verdict']==r['accepted_verdict']=='PASS' and not r['adaptation_applied'];actual.append({'self':c['path'],**r})

def check(name,mutator,sid='deadline_during_tool'):
 cs,rs,ls=pair(sid);mutator(cs,rs,ls)
 try:r=run(cs,rs,ls)
 except (ValueError,KeyError,TypeError,IndexError,AttributeError) as e:r={'accepted_verdict':'FAIL','rejection':'input: '+str(e)}
 assert r['accepted_verdict']=='FAIL' and not r.get('adaptation_applied',False),(name,r)
 results.append({'name':name,'strict':r.get('strict_verdict'),'reason':r.get('rejection'),'rules':r.get('strict',{}).get('triggered_rules')})

def insert(rs,record,index=None):
 r=copy.deepcopy(record);rs.insert(index if index is not None else len(rs)-1,r);renumber(rs)

def duplicate(cs,rs,ls):insert(rs[1],extra(rs[1]),next(i for i,r in enumerate(rs[1]) if r.get('event')=='gateway_submit_failed'))
def move(cs,rs,ls,where):
 e=extra(rs[1]);rs[1].remove(e);rs[1].insert(where(rs[1]),e);renumber(rs[1])
def deletekind(cs,rs,ls,kind):rs[1].remove(find(rs[1],kind));renumber(rs[1])
check('duplicate extra',duplicate)
check('second extra different text',lambda c,r,l:(insert(r[1],{**extra(r[1]),'text':'other'})))
check('extra wrong text',lambda c,r,l:extra(r[1]).update(text='wrong'))
check('extra trailing space',lambda c,r,l:extra(r[1]).update(text=extra(r[1])['text']+' '))
check('extra child ownership',lambda c,r,l:extra(r[1]).update(agentScope='child'))
check('extra statusKind',lambda c,r,l:extra(r[1]).update(statusKind='warning'))
check('extra unknown code',lambda c,r,l:extra(r[1]).update(code='NEW'))
check('missing model request invocation',lambda c,r,l:find(r[1],'model.request').pop('invocationId'))
check('missing failclosed invocation',lambda c,r,l:next(a for a in r[1] if a.get('state')=='result_unknown_fail_closed').pop('invocationId'))
check('extra wrong invocation',lambda c,r,l:extra(r[1]).update(invocationId='wrong-run'))
check('extra wrong query',lambda c,r,l:extra(r[1]).update(q='wrong-q'))
check('extra sequence gap',lambda c,r,l:extra(r[1]).update(sequence=99))
check('before timeout',lambda c,r,l:move(c,r,l,lambda x:next(i for i,a in enumerate(x) if a.get('event')=='turn_timeout')))
check('after terminal',lambda c,r,l:move(c,r,l,lambda x:len(x)-1))
check('intervening normal state',lambda c,r,l:insert(r[1],{**extra(r[1]),'event':'other'},next(i for i,a in enumerate(r[1]) if a.get('kind')=='side_effect.state')))
check('missing timeout',lambda c,r,l:(r[1].remove(next(a for a in r[1] if a.get('event')=='turn_timeout')),renumber(r[1])))
check('duplicate timeout',lambda c,r,l:insert(r[1],next(a for a in r[1] if a.get('event')=='turn_timeout')))
check('missing unknown',lambda c,r,l:(r[1].remove(next(a for a in r[1] if a.get('state')=='result_unknown_fail_closed')),renumber(r[1])))
check('unknown source wrong',lambda c,r,l:next(a for a in r[1] if a.get('state')=='result_unknown_fail_closed').update(source='transport_interruption'))
check('unknown source extra field',lambda c,r,l:next(a for a in r[1] if a.get('state')=='result_unknown_fail_closed').update(detail='NEW'))
check('missing operation terminal',lambda c,r,l:deletekind(c,r,l,'operation.terminal'))
check('duplicate operation terminal',lambda c,r,l:insert(r[1],find(r[1],'operation.terminal')))
check('operation cancelled',lambda c,r,l:find(r[1],'operation.terminal').update(outcome='cancelled'))
check('operation code',lambda c,r,l:find(r[1],'operation.terminal').update(code='OTHER'))
check('operation lastAppliedSequence',lambda c,r,l:find(r[1],'operation.terminal').update(lastAppliedSequence=10))
check('operation unknown field',lambda c,r,l:find(r[1],'operation.terminal').update(detail='NEW'))
check('terminal nonempty output',lambda c,r,l:find(r[1],'terminal').update(output='succeeded'))
check('terminal completed',lambda c,r,l:find(r[1],'terminal').update(outcome='completed'))
check('side effect count',lambda c,r,l:find(r[1],'side_effect.state').update(sideEffectCount=1))
check('side effect bool zero',lambda c,r,l:find(r[1],'side_effect.state').update(sideEffectCount=False))
check('side effect actual counts',lambda c,r,l:find(r[1],'side_effect.state').update(counts={'file':1}))
check('durable state changed',lambda c,r,l:find(r[1],'durable.state').update(durableStatusCount=1))
check('tool finish success',lambda c,r,l:insert(r[1],{**find(r[1],'tool.start'),'kind':'tool.finish','success':True}))
check('tool call input',lambda c,r,l:find(r[1],'tool.call').update(arguments={'q':'different'}))
check('new model request after timeout',lambda c,r,l:insert(r[1],find(r[1],'model.request')))
check('original request field',lambda c,r,l:find(r[1],'model.request').update(newField='different'))
check('response usage changed',lambda c,r,l:find(r[1],'model.response').update(usage={'inputTokens':1}))
check('same changed usage both sides',lambda c,r,l:[find(a,'model.response').update(usage={'inputTokens':1}) for a in r])
check('C gateway error',lambda c,r,l:insert(r[1],{'kind':'gateway.error','scenarioId':r[1][0]['scenarioId'],'q':r[1][0]['q'],'invocationId':r[1][0]['invocationId'],'agentScope':'parent','message':'unexpected wire error'}))
check('C agent status',lambda c,r,l:insert(r[1],{'kind':'agent.status','scenarioId':r[1][0]['scenarioId'],'q':r[1][0]['q'],'invocationId':r[1][0]['invocationId'],'agentScope':'parent','event':'unexpected_event','detail':{'message':'extra'}}))
check('C error before timeout not optional B',lambda c,r,l:insert(r[1],{'kind':'gateway.error','scenarioId':r[1][0]['scenarioId'],'q':r[1][0]['q'],'invocationId':r[1][0]['invocationId'],'agentScope':'parent','message':'unexpected'},next(i for i,a in enumerate(r[1]) if a.get('event')=='turn_timeout')))
check('driver source hash mismatch',lambda c,r,l:l[1]['driver_sha256'].update({'bind.mjs':'0'*64}))
check('trace source hash mismatch',lambda c,r,l:l[1]['inputs_sha256'].update({'trace.py':'0'*64}))
check('outside scene',lambda c,r,l:c[1].update(scenario='cancel'))
check('identity wrong hash',lambda c,r,l:r[1][-1].update(sha256='0'*64))
check('identity missing',lambda c,r,l:r[1].pop())
check('source ledger wrong baseline',lambda c,r,l:l[1]['upstream_summary']['provenance'].update(current='wrong-source'))
check('ledger invalid',lambda c,r,l:l[1].update(valid_round=False))
check('single-side health contiguous fail',lambda c,r,l:r[1][0].update(sequence=8))
# Controls cannot enter public registered-files entry via temporary paths.
try:M.evaluate_files(HERE/'unregistered.json',case('deadline','ts',1)['path']);raise AssertionError('outside path accepted')
except ValueError:results.append({'name':'public unknown file path rejects','reason':'sealed path scope'})
import subprocess
existing=OUTPUT/'existing-output.json';existing.write_bytes(b'FIRST-FAIL-DO-NOT-OVERWRITE\n');before=existing.read_bytes()
cli=subprocess.run([sys.executable,'-I',str(HERE.parent/'accepted_summary.py'),'--evidence-root',str(ROOT),'--output',str(existing)],capture_output=True,text=True);assert cli.returncode!=0 and existing.read_bytes()==before;results.append({'name':'CLI existing output preserved','reason':'exclusive publication rejects'})
reuse=subprocess.run([sys.executable,'-I',str(HERE/'deadline_accepted.py'),str(ROOT),str(OUTPUT)],capture_output=True,text=True);assert reuse.returncode!=0 and existing.read_bytes()==before;results.append({'name':'controls existing directory rejected','reason':'new directory required'})
summaryFile=OUTPUT/'summary.json'
cli=subprocess.run([sys.executable,'-I',str(HERE.parent/'accepted_summary.py'),'--evidence-root',str(ROOT),'--output',str(summaryFile)],capture_output=True,text=True);assert cli.returncode==1 and summaryFile.exists();summary=json.loads(summaryFile.read_text());assert summary['strict_original']['score']['fraction']=='50/53' and summary['accepted']['score']['fraction']=='52/53' and summary['accepted']['failed_scenarios']==['sidecar_budget_limit'];results.append({'name':'public accepted summary retains strict50 and budget fail52','reason':'actual command and full673input/4code closure'})
for invalid in ('../escape','not-registered.json','/absolute/path'):
 try:M.resolve(invalid);raise AssertionError('unregistered relocation accepted')
 except ValueError:results.append({'name':'registered path rejects '+invalid,'reason':'fixed registration membership'})
cli=subprocess.run([sys.executable,'-I',str(HERE.parent/'accepted_summary.py'),'--evidence-root',str(ROOT),'--output',str(OUTPUT/'must-not-exist.json'),'--manifest',str(summaryFile)],capture_output=True,text=True);assert cli.returncode!=0 and not (OUTPUT/'must-not-exist.json').exists();results.append({'name':'CLI external registration rejected','reason':'no manifest argument'})
# A caller cannot add a different path spelling/symlink as registered input.
alias=OUTPUT/'alias.jsonl';alias.symlink_to(M.resolve(case('deadline','ts',1)['path']))
try:M.evaluate_files(alias,M.resolve(case('deadline','rust',1)['path']));raise AssertionError('unregistered alias accepted')
except ValueError:results.append({'name':'unregistered symlink input rejected','reason':'lexical registered path required'})
# Registration bytes are pinned in code, not trusted because a manifest says so.
regdir=OUTPUT/'tampered-registration';(regdir/'registrations').mkdir(parents=True)
(regdir/'deadline_accepted.py').write_bytes((HERE.parent/'deadline_accepted.py').read_bytes())
reg=copy.deepcopy(D.REGISTRATION);reg['cases'][0]['sha256']='0'*64
(regdir/'registrations/original53-deadline-20261114.json').write_text(json.dumps(reg))
cli=subprocess.run([sys.executable,'-I',str(regdir/'deadline_accepted.py')],capture_output=True,text=True);assert cli.returncode!=0 and 'sealed bytes changed' in cli.stderr;results.append({'name':'self-authorized modified registration rejected','reason':'embedded registration SHA'})
# Actual full entry must reject a modified non-deadline strict input as well.
shadow=OUTPUT/'shadow-evidence';shadow.mkdir()
for relative in M.allowed_paths:
 source=ROOT/relative
 if source.exists():
  target=shadow/relative;target.parent.mkdir(parents=True,exist_ok=True);target.symlink_to(source)
relative=next(p for p in D.REGISTRATION['strict_input_closure'] if '/archive/ref/' in p and p.endswith('run.json'))
target=shadow/relative;target.unlink();target.write_bytes((ROOT/relative).read_bytes()+b'\n')
cli=subprocess.run([sys.executable,'-I',str(HERE.parent/'accepted_summary.py'),'--evidence-root',str(shadow),'--output',str(OUTPUT/'invalid-closure-must-not-exist.json')],capture_output=True,text=True);assert cli.returncode!=0 and 'sealed bytes changed' in cli.stderr and not (OUTPUT/'invalid-closure-must-not-exist.json').exists();results.append({'name':'modified other51 calibration input rejected','reason':'full strict input closure before acceptance'})
# Even an internally valid JSON replacement cannot re-label the original score.
target=shadow/D.REGISTRATION['strict_score']['path'];target.unlink();forged=copy.deepcopy(summary['strict_original']);forged['verdict']='PASS';target.write_text(json.dumps(forged))
cli=subprocess.run([sys.executable,'-I',str(HERE.parent/'accepted_summary.py'),'--evidence-root',str(shadow),'--output',str(OUTPUT/'invalid-score-must-not-exist.json')],capture_output=True,text=True);assert cli.returncode!=0 and 'sealed bytes changed' in cli.stderr and not (OUTPUT/'invalid-score-must-not-exist.json').exists();results.append({'name':'relabelled strict score rejected','reason':'immutable strict score byte hash'})
# Disabling admission retains the three genuine same-round failures.
firstfails=[]
for sid,round in [('deadline',5),('deadline_during_tool',3),('deadline_during_tool',5)]:
 cs,rs,ls=pair(sid,round);r=M.original_result(sid,*rs);assert r['verdict']=='FAIL' and r['triggered_rules']==['E10','E11','G03_channels'];firstfails.append({'scene':sid,'round':round,'strict':r})
D.G.write_result_exclusive(OUTPUT/'actual-results.json',json.dumps(actual,indent=2)+'\n');D.G.write_result_exclusive(OUTPUT/'controls-results.json',json.dumps({'actualOriginalComparisons':len(actual),'totalControls':len(results),'negativeControls':len(results)-1,'auxiliaryPositiveControls':1,'controls':results,'strictFirstFailuresPreserved':firstfails},indent=2)+'\n');print(json.dumps({'actual':len(actual),'totalControls':len(results),'negative':len(results)-1,'auxiliaryPositive':1,'pass':True}))
