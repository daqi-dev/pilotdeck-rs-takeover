#!/usr/bin/env python3
"""Independent accepted summary for the one registered deadline batch.
Original strict score and ruler are never changed or relabeled PASS.
"""
import sys
if not sys.flags.isolated:raise SystemExit('run with python3 -I')
import argparse,copy,importlib.util,json
from pathlib import Path
HERE=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('original53_deadline_accepted',HERE/'deadline_accepted.py');D=importlib.util.module_from_spec(spec);sys.modules[spec.name]=D;spec.loader.exec_module(D)
G=D.G

def compact(result):
 return {k:result[k] for k in ('verdict','triggered_rules')}|{'difference_paths':[d['path'] for d in result.get('channel',{}).get('differences',[])],'health':result.get('health',{}),'error':result.get('error')}

def summarize(evidence_root):
 engine=D.DeadlineAccepted(evidence_root);reg=D.REGISTRATION
 strict=json.loads(D.readpin(engine.resolve(reg['strict_score']['path']),reg['strict_score']['sha256']))
 prefix=reg['original_root']+'/'
 # The old absolute spellings remain identity values. Only IO paths relocate.
 for label,registered,observed in [('input',reg['strict_input_closure'],strict['input_sha256']),('code',reg['strict_code_closure'],strict['code_sha256'])]:
  D.require({prefix+p:sha for p,sha in registered.items()}==observed,'strict '+label+' manifest unchanged')
  for relative,expected in registered.items():
   D.readpin(engine.resolve(relative),expected)
   if label=='code':D.readpin(G.WORKTREE/relative,expected)
 D.require(strict['valid_score'] is True and strict['verdict']=='FAIL' and strict['k']==5 and strict['score']=={'numerator':50,'denominator':53,'fraction':'50/53'},'exact original strict50/53')
 D.require(set(strict['per_scenario'])==set(engine.CATALOG) and len(engine.CATALOG)==53,'unchanged53 catalog')
 D.require({s for s,r in strict['per_scenario'].items() if r['verdict']=='FAIL'}=={'deadline','deadline_during_tool','sidecar_budget_limit'},'exact original failure set')
 accepted=copy.deepcopy(strict['per_scenario']);affected={}
 for sid in ('deadline','deadline_during_tool'):
  runs=[]
  for round in range(1,6):
   cases=[next(c for c in engine.CASES if c['scenario']==sid and c['side']==side and c['round']==round) for side in ('ts','rust')]
   result=engine.evaluate_files(*(engine.resolve(c['path']) for c in cases))
   old=strict['per_scenario'][sid]['runs'][round-1]['paired']
   D.require(compact(result['strict'])==old,'original strict paired result preserved')
   runs.append({'run':round,**result})
  affected[sid]={'strict_verdict':strict['per_scenario'][sid]['verdict'],'accepted_verdict':'PASS' if all(r['accepted_verdict']=='PASS' for r in runs) else 'FAIL','runs':runs}
  accepted[sid]={'verdict':affected[sid]['accepted_verdict'],'counted':True,'policy':D.RULE,'strict_original':strict['per_scenario'][sid],'accepted_runs':runs}
 D.require(all(accepted[sid]==strict['per_scenario'][sid] for sid in accepted if sid not in affected),'all other51 remain identical')
 passed=sum(r['verdict']=='PASS' for r in accepted.values())
 failed=[sid for sid,r in accepted.items() if r['verdict']!='PASS']
 return {'schema':'original53-accepted-summary/v1','registration':{'id':reg['id'],'sha256':D.REGISTRATION_SHA,'base':reg['base'],'rule':D.RULE},'strict_original_score_sha256':reg['strict_score']['sha256'],'strict_original':strict,'accepted':{'verdict':'PASS' if passed==53 else 'FAIL','score':{'numerator':passed,'denominator':53,'fraction':f'{passed}/53'},'k':5,'failed_scenarios':failed,'per_scenario':accepted},'affected':affected,'revalidated':{'strict_input_files':len(reg['strict_input_closure']),'strict_code_files':len(reg['strict_code_closure']),'fixed_raw_files':20,'other51_unchanged':True,'no_new_TS_pool':True},'implementation':{p.name:D.digest(p.read_bytes()) for p in (Path(__file__),HERE/'deadline_accepted.py')},'limitations':reg['id']+' only; no future registration, full transport wire or post-sampling unlimited side-effect proof'}

def main():
 parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--evidence-root',type=Path,required=True);parser.add_argument('--output',type=Path,required=True);args=parser.parse_args()
 result=summarize(args.evidence_root)
 G.write_result_exclusive(args.output,json.dumps(result,ensure_ascii=False,indent=2)+'\n')
 print(json.dumps({'strict':result['strict_original']['score'],'strict_verdict':result['strict_original']['verdict'],'accepted':result['accepted']['score'],'accepted_verdict':result['accepted']['verdict'],'failed':result['accepted']['failed_scenarios']}))
 return 0 if result['accepted']['verdict']=='PASS' else 1
if __name__=='__main__':sys.exit(main())
