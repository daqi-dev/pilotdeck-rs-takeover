#!/usr/bin/env python3
"""Accepted deadline record for one immutable registered Original53 batch.
No alternate registration or test flag is accepted by the command-line entry.
"""
import sys
if not sys.flags.isolated: raise SystemExit('run with python3 -I')
import json,hashlib,importlib.util,copy
from pathlib import Path,PurePosixPath
PARITY=Path(__file__).resolve().parent
REGISTRATION_SHA='bfba3dac717f009fc94ac0726e59bc74c30a11481991f2f40d20a7914a34b074'
RULE='C-DEADLINE-HOST-OPTIONAL-001'
def digest(data):return hashlib.sha256(data).hexdigest()
def canon(x):return json.dumps(x,sort_keys=True,separators=(',',':'),ensure_ascii=False,allow_nan=False)
def require(ok,message):
 if not ok:raise ValueError(message)
def readpin(path,expected):
 data=Path(path).read_bytes();require(digest(data)==expected,'sealed bytes changed: '+str(path));return data
REGISTRATION=json.loads(readpin(PARITY/'registrations/original53-deadline-20261114.json',REGISTRATION_SHA))
G_PATH=PARITY/'g03_compare.py'
readpin(G_PATH,'51ac99f443343032ca1833381f07a96f950ff016e8734a10c67556715ca6e8b8')
spec=importlib.util.spec_from_file_location('deadline_accepted_frozen_g03',G_PATH);G=importlib.util.module_from_spec(spec);sys.modules[spec.name]=G;spec.loader.exec_module(G)
for relative,expected in REGISTRATION['baseline_sources'].items():readpin(G.BASELINE/relative,expected)
for relative,expected in REGISTRATION['code_sources'].items():readpin(PARITY/relative,expected)
ENVELOPE={'scenarioId','q','invocationId','sequence'}
EXTRA={'kind':'durable.status','agentScope':'parent','event':'gateway_submit_failed','statusKind':'error','text':'AgentLoop terminal outcome is result_unknown and host reconciliation found no final result.'}
def semantic(r):return {k:v for k,v in r.items() if k not in ENVELOPE}
def canonical(records):
 generated={};return [G.T.canonicalize(copy.deepcopy(r),generated_ids=generated) for r in records]
def unique(rs,p,label):
 found=[(i,r) for i,r in enumerate(rs) if p(r)];require(len(found)==1,'unique '+label);return found[0]
def subset(rs,kinds):return [semantic(r) for r in rs if r.get('kind') in kinds]
def usages(rs):
 result=[];counts={}
 def visit(v,p):
  if isinstance(v,dict):
   for k,value in v.items():
    if 'usage' in k.lower():result.append((p+[k],value))
    visit(value,p+[k])
  elif isinstance(v,list):
   for i,value in enumerate(v):visit(value,p+[i])
 for r in rs:
  kind=r['kind'];i=counts.get(kind,0);counts[kind]=i+1;visit(semantic(r),[kind,i])
 return result


class DeadlineAccepted:
 def __init__(self,evidence_root):
  self.evidence_root=Path(evidence_root).resolve()
  self.CASES=copy.deepcopy(REGISTRATION['cases'])
  require(len(self.CASES)==20,'only fixed20')
  self.allowed_paths=set(REGISTRATION['strict_input_closure'])|set(REGISTRATION['strict_code_closure'])|{REGISTRATION['strict_score']['path']}
  self.allowed_paths.update(c[k] for c in self.CASES for k in ('path','ledger_path'))
  self.SEEDS={sid:self.rows(next(c for c in self.CASES if c['scenario']==sid and c['side']=='ts' and c['round']==1)) for sid in ('deadline','deadline_during_tool')}
  self.CATALOG=G.scenario_catalog()
 def resolve(self,relative):
  require(isinstance(relative,str) and relative in self.allowed_paths,'path not in immutable registration')
  parsed=PurePosixPath(relative)
  require(not parsed.is_absolute() and '..' not in parsed.parts and str(parsed)==relative,'unsafe registered relative path')
  return self.evidence_root.joinpath(*parsed.parts)
 def rows(self,case):return [json.loads(line) for line in G.jsonl_lines(readpin(self.resolve(case['path']),case['sha256']).decode('utf-8'))]
 def ledger(self,case):return json.loads(readpin(self.resolve(case['ledger_path']),case['ledger_sha256']))
 def original_result(self,sid,left,right):return G.compare_records(sid,canonical(left),canonical(right),self.CATALOG.get(sid))
 def _side_guard(self,case,rs,ledger):
  sid=case['scenario'];seed=self.SEEDS[sid]
  require(sid in self.SEEDS,'scene scope');require(ledger.get('valid_round') is True and ledger.get('identity_ok') is True,'ledger validity')
  require(ledger.get('side')==case['side'],'ledger side');require(ledger['upstream_summary']['provenance']['current']==REGISTRATION['baseline_sha'],'fixed cc7 TS host')
  require(ledger['cleanup']=={'allStopped':True,'remaining':[],'errors':[]},'owned round cleanup')
  require(ledger['identity'].get(sid) is True,'original identity wrapper')
  for name,sha in ledger['inputs_sha256'].items():readpin(G.TOOLS/name,sha)
  for name,sha in ledger['driver_sha256'].items():readpin(PARITY/name,sha)
  require(all(ENVELOPE <= set(r) for r in rs),'every raw record requires full original invocation envelope')
  require(all(r.get('scenarioId')==sid for r in rs),'record scene identity')
  require(all(r.get('q')==seed[0]['q'] for r in rs),'record query')
  inv=[r['invocationId'] for r in rs if 'invocationId' in r];require(bool(inv) and len(set(inv))==1 and set(inv)=={r['invocationId'] for r in self.rows(case) if 'invocationId' in r},'invocation ownership')
  require(all(type(r.get('sequence')) is int for r in rs) and [r['sequence'] for r in rs]==list(range(len(rs))),'raw contiguous sequence (never renumber projection)')
  _,identity=unique(rs,lambda r:r.get('kind')=='harness.proof' and r.get('state')=='sidecar_identity','sidecar identity')
  require(identity.get('side')==case['side'] and identity.get('sha256')==identity.get('expectedSha256')==ledger['expected_sha256'] and identity.get('spawnObserved') is True,'sidecar actual identity')
  require(canon(semantic(identity))==canon(semantic(self.rows(case)[-1])),'complete original identity observations')
  ti,timeout=unique(rs,lambda r:r.get('kind')=='durable.status' and r.get('event')=='turn_timeout','timeout')
  require(canon(semantic(timeout))==canon(next(semantic(r) for r in seed if r.get('event')=='turn_timeout')),'exact parent timeout payload')
  fi,proof=unique(rs,lambda r:r.get('kind')=='harness.proof' and (r.get('state')=='result_unknown_fail_closed' or r.get('type')=='result_unknown_fail_closed'),'failclosed')
  require(canon(semantic(proof))==canon({'kind':'harness.proof','state':'result_unknown_fail_closed','type':'result_unknown_fail_closed','source':'sidecar_final'}),'sidecar-final unknown proof')
  extras=[(i,r) for i,r in enumerate(rs) if r.get('event')=='gateway_submit_failed']
  require(len(extras)<=1,'max one gateway_submit_failed across entire trace')
  for i,r in extras:
   require(set(r)==ENVELOPE|set(EXTRA),'exact extra record keys');require(canon(semantic(r))==canon(EXTRA),'exact extra parent/B payload');require(G.channel_of(r)=='B','candidate B only')
  expected=['durable.status','harness.proof']+(['durable.status'] if extras else [])+['side_effect.state','operation.terminal','durable.state','terminal','harness.proof']
  require([r['kind'] for r in rs[ti:]]==expected and fi==ti+1,'exact adjacent tail position')
  if extras:require(extras[0][0]==fi+1,'extra directly after failclosed')
  for kind in ('side_effect.state','operation.terminal','durable.state','terminal'):
   _,record=unique(rs,lambda r:r.get('kind')==kind,kind);expectedRecord=next(semantic(r) for r in seed if r.get('kind')==kind)
   require(canon(semantic(record))==canon(expectedRecord),'exact original '+kind)
  se=next(r for r in rs if r['kind']=='side_effect.state');require(se.get('counts')=={} and type(se.get('sideEffectCount')) is int and se['sideEffectCount']==0,'zero side effects')
  op=next(r for r in rs if r['kind']=='operation.terminal');require(op['outcome']=='result_unknown' and op['code']=='DEADLINE_EXCEEDED' and op['lastAppliedSequence']==(3 if sid=='deadline' else 9),'full unknown terminal')
  require(canon(subset(rs,{'tool.call','tool.start','tool.finish','tool.result'}))==canon(subset(seed,{'tool.call','tool.start','tool.finish','tool.result'})),'exact observed tool start, never completion')
  require(not any(r.get('kind') in {'model.request','tool.call','tool.start','tool.finish','tool.result','user.output','gateway.error','agent.status'} for r in rs[ti+1:]),'no post-timeout model/tool/C records')
  for kind in ('model.request','model.response'):
   require(canon(subset(rs,{kind}))==canon(subset(seed,{kind})),'original unnormalized '+kind)
  require(canon(usages(rs))==canon(usages(seed)),'all original typed usage values')
  return {'extra':extras[0][0] if extras else None,'timeout':ti,'failclosed':fi,'lastAppliedSequence':op['lastAppliedSequence'],'health_checked_on':'unmodified original raw'}

 def _evaluate_registered_records(self,left_case,right_case,left,right,left_ledger,right_ledger):
  """Internal control layer: mutations remain untrusted, not a public file bypass."""
  sid=left_case['scenario'];strict=self.original_result(sid,left,right)
  result={'rule':RULE,'strict_verdict':strict['verdict'],'strict':strict,'accepted_verdict':strict['verdict'],'adaptation_applied':False,'admitted_records':[]}
  try:
   require(sid in self.SEEDS and right_case['scenario']==sid,'two-scene scope')
   # Identity, oracle, health and ALL single-side E checks use unchanged raw.
   for side in ('left','right'):
    require(not strict['health'][side],'original '+side+' trace health')
    require(not any(v.get('status')=='FAIL' for v in strict['edges'][side].values()),'original '+side+' single-side E/partial-order failure')
   guards=[self._side_guard(left_case,left,left_ledger),self._side_guard(right_case,right,right_ledger)]
   require(set(strict['triggered_rules'])<={'E10','E11','G03_channels'},'other original failure')
   counts=[int(g['extra'] is not None) for g in guards]
   if counts not in ([0,1],[1,0]):
    require(strict['verdict']=='PASS','matched counts must pass original comparison');result['reason']='no optional difference; original strict comparison retained';return result
   require(strict['triggered_rules']==['E10','E11','G03_channels'],'exact first failures required')
   require({k for k,v in strict['channel']['per_stream_diffs'].items() if v}=={'parent/B'},'only parent/B differs')
   originals=[left,right];views=[]
   for side,rs in enumerate(originals):
    i=guards[side]['extra'];views.append([copy.deepcopy(r) for j,r in enumerate(rs) if j!=i])
    if i is not None:result['admitted_records'].append({'side':['left','right'][side],'raw_index':i,'sequence':rs[i]['sequence'],'record':rs[i],'semantic':semantic(rs[i]),'semantic_row_sha256':digest(canon(rs[i]).encode())})
   # No renumbering or trace_health(view). E10/E11 are recomputed explicitly,
   # while original single-side E/health remain authoritative above.
   a,b=map(canonical,views);channel=G.rule_verdict(sid,a,b);multiplicity=G.identity_multiplicity_diffs(a,b)
   result['residual']={'channel':channel,'E10':'PASS' if channel['verdict']=='PASS' else 'FAIL','E11':'PASS' if not multiplicity else 'FAIL','multiplicity':multiplicity}
   require(channel['verdict']=='PASS' and not multiplicity,'all residual actor/channels and pair E10/E11')
   result.update(accepted_verdict='PASS',adaptation_applied=True,witnesses=guards)
  except (ValueError,KeyError,TypeError,IndexError,AttributeError) as e:
   result['accepted_verdict']='FAIL';result['rejection']=str(e)
  return result

 def evaluate_files(self,left_file,right_file):
  # Only these20 registered paths/bytes and exact ledgers; no test mode or flags.
  selected=[];ledgers=[]
  for file in (left_file,right_file):
   case=next((c for c in self.CASES if self.resolve(c['path'])==Path(file)),None);require(case is not None,'unregistered artifact')
   rs=self.rows(case);ledger=self.ledger(case);selected.append((case,rs));ledgers.append(ledger)
  result=self._evaluate_registered_records(selected[0][0],selected[1][0],selected[0][1],selected[1][1],*ledgers)
  for admitted in result['admitted_records']:
   case=selected[0 if admitted['side']=='left' else 1][0];line=G.jsonl_lines(readpin(self.resolve(case['path']),case['sha256']).decode('utf-8'))[admitted['raw_index']];require(json.loads(line)==admitted['record'],'admitted record must be the exact original raw line');admitted['raw_line_sha256_without_newline']=digest(line.encode('utf-8'))
  result['inputs']=[{'path':c['path'],'sha256':c['sha256'],'ledgerSHA256':c['ledger_sha256']} for c,_ in selected]
  return result
