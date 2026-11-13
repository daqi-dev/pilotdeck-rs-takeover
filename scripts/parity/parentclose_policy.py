"""Private candidate: supplemental parent-close reference policy, not registered."""
import sys
import hashlib,importlib.util,itertools,json
from pathlib import Path
p=Path(__file__).with_name('parentclose_oracle.py');spec=importlib.util.spec_from_file_location('parentclose_oracle_candidate',p);oracle=importlib.util.module_from_spec(spec);spec.loader.exec_module(oracle)
SID=oracle.SID

def calibrate(score, directories, inputs):
    dirs=[Path(x).resolve() for x in directories]
    if len(dirs)<20 or len(set(dirs))!=len(dirs):raise ValueError('requires >=20 independent parent-close reference directories')
    captures=[];records=[];modes=[];fingerprints=set()
    for path in dirs:
        root,output,checked=inputs.capture(path,'ts',(SID,))
        trace=score.G.artifact_dir(root)/f'{SID}.pilotdeck-current-sidecar.jsonl';fingerprint=inputs.hashes[str(trace.resolve())]
        if fingerprint in fingerprints:raise ValueError('copied parent-close reference')
        fingerprints.add(fingerprint)
        if checked['per_scenario'][SID]['verdict']!='PASS':raise ValueError('invalid reference self/health')
        try:mode=oracle.check(root)
        except (AssertionError,KeyError,TypeError,ValueError,FileNotFoundError,StopIteration) as e:raise ValueError('invalid parent-close business contract') from e
        captures.append((root,output));records.append(score.G.load_aligned(trace));modes.append(mode)
    if len({x[1] for x in captures})!=1:raise ValueError('reference output paths differ')
    score.require_ts_source([x[0] for x in captures],inputs)
    days={inputs.dates[str(root)][SID]['capture_utc_day'] for root,_ in captures}
    if len(days)!=1:raise ValueError('reference days differ')
    scenario=score.G.scenario_catalog()[SID];matrix={}
    for i,j in itertools.combinations(range(len(dirs)),2):
        result=score.compact(score.G.compare_records(SID,records[i],records[j],scenario));matrix[f'{i+1}-{j+1}']=result
        if (result['verdict']=='PASS') != (modes[i]==modes[j]):raise ValueError('parent-close class matrix not equivalent to validated outcomes')
    groups=[]
    for name in ('completed','cancelled'):
        indices=[i for i,mode in enumerate(modes) if mode==name]
        if len(indices)<2:raise ValueError('parent-close class has fewer than two references')
        groups.append({'class':name,'count':len(indices),'member_runs':[i+1 for i in indices],'members':[str(captures[i][0]) for i in indices]})
    registration={'classes':groups,'pairs':matrix,'capture_utc_day':next(iter(days)),'source_sha256':{key:inputs.ledgers[str(captures[0][0])][key] for key in score.SOURCE_KEYS},'trace_sha256':{str(root):inputs.hashes[str((score.G.artifact_dir(root)/f'{SID}.pilotdeck-current-sidecar.jsonl').resolve())] for root,_ in captures}}
    return captures,records,registration

def compare(score,root,records,registration,side):
    # Revalidate independently: callers cannot bypass identity, health or dates.
    verified=score.Inputs()
    try:
        actual_root,output,checked=verified.capture(root,side,(SID,))
        if checked['per_scenario'][SID]['verdict']!='PASS':raise ValueError('actual health failed')
        reference_inputs=score.Inputs()
        reference_roots=list(registration['trace_sha256'])
        if str(actual_root) in reference_roots:raise ValueError('actual overlaps reference pool')
        for reference in reference_roots:
            ref_root,ref_output,ref_checked=reference_inputs.capture(reference,'ts',(SID,))
            if ref_checked['per_scenario'][SID]['verdict']!='PASS':raise ValueError('reference health failed')
            if output!=ref_output:raise ValueError('capture output differs')
            reason=score.date_mismatch(reference_inputs.dates[str(ref_root)][SID],verified.dates[str(actual_root)][SID],'actual/reference')
            if reason:raise ValueError(reason)
        actual_hash=verified.hashes[str((score.G.artifact_dir(actual_root)/f'{SID}.pilotdeck-current-sidecar.jsonl').resolve())]
        if actual_hash in registration['trace_sha256'].values():raise ValueError('actual copied reference trace')
    except (OSError,ValueError,KeyError,TypeError,AttributeError,IndexError) as e:
        return {'verdict':'REFUSED','reason':str(e)}
    try: mode=oracle.check(root)
    except (AssertionError,KeyError,TypeError,ValueError,FileNotFoundError,StopIteration) as e:return {'verdict':'FAIL','reason':'business contract failed','detail':type(e).__name__}
    actual=score.G.load_aligned(score.G.artifact_dir(root)/f'{SID}.pilotdeck-current-sidecar.jsonl');scenario=score.G.scenario_catalog()[SID]
    comparisons=[score.compact(score.G.compare_records(SID,r,actual,scenario)) for r in records]
    classes=score.class_matches(registration['classes'],comparisons);matches=[x['class'] for x in classes if x['complete_match']]
    return {'verdict':'PASS' if matches==[mode] else 'FAIL','mode':mode,'class_matches':classes,'comparisons':comparisons}

def validate_ts(score, ts_dirs, parent_dirs, parent_registration, cancel_dirs, *, parent_lock):
    """Recompute TS-only evidence; never read Rust. Fail closed on missing lock."""
    if not parent_lock:raise ValueError('parent-close registration lock required')
    inputs=score.Inputs(); pool,records,registration=calibrate(score,parent_dirs,inputs)
    # The cancel pool remains independently calibrated by its original policy.
    cancel_captures,_,_=score.calibrate_pool(cancel_dirs,inputs)
    inputs.track(Path(parent_lock))
    pinned=json.loads(Path(parent_lock).read_text())
    if pinned.get('TS_calibration')!=registration:raise ValueError('parent-close registration mismatch')
    ts=[Path(d).resolve() for d in ts_dirs]; cancel=[Path(d).resolve() for d in cancel_dirs]
    if len(ts)!=5 or len(set(ts))!=5:raise ValueError('requires five distinct scoring TS runs')
    pool_roots={r for r,_ in pool};pool_hashes=set(registration['trace_sha256'].values())
    if pool_roots.intersection(ts) or pool_roots.intersection(cancel):raise ValueError('parent-close reference directory overlap')
    if set(ts).intersection(cancel):raise ValueError('cancel reference directory overlap')
    checks=[]
    for directory in ts:
        root,output,checked=inputs.capture(directory,'ts',(SID,))
        if checked['per_scenario'][SID]['verdict']!='PASS':raise ValueError('TS self/health failure')
        if output!=pool[0][1]:raise ValueError('TS capture-time output differs')
        if inputs.dates[str(root)][SID]['capture_utc_day']!=registration['capture_utc_day']:raise ValueError('TS capture UTC day differs')
        if inputs.hashes[str((score.G.artifact_dir(root)/f'{SID}.pilotdeck-current-sidecar.jsonl').resolve())] in pool_hashes:raise ValueError('parent-close TS trace overlap')
        result=compare(score,root,records,registration,'ts')
        if result['verdict']!='PASS':raise ValueError('TS run does not match a complete validated class')
        checks.append(result)
    if len({output for _,output in pool+cancel_captures})!=1:raise ValueError('independent pool output differs')
    all_days={context['capture_utc_day'] for contexts in inputs.dates.values() for context in contexts.values()}
    if len(all_days)!=1:raise ValueError('independent pools / scoring TS days differ')
    score.require_ts_source([r for r,_ in pool]+[r for r,_ in cancel_captures]+ts,inputs)
    return {'parentclose_registration':registration,'ts_checks':checks,'input_sha256':inputs.hashes,'TS_unchanged_pairwise_S':None}
