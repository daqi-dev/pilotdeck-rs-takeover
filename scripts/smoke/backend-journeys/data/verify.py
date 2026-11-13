#!/usr/bin/env python3
"""Dataset-specific file/trace assertions for the existing journey runner.

No server/model runner is implemented here. Missing raw lifecycle evidence fails.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shlex


def need(condition,message):
    if not condition: raise AssertionError(message)


def walk(value):
    if isinstance(value,dict):
        yield value
        for child in value.values(): yield from walk(child)
    elif isinstance(value,list):
        for child in value: yield from walk(child)


def subset(actual,expected,path='$'):
    if isinstance(expected,dict):
        need(isinstance(actual,dict),f'{path}: expected object')
        for key,value in expected.items():
            need(key in actual,f'{path}: missing key {key}')
            subset(actual[key],value,path+'.'+key)
    elif isinstance(expected,list):
        need(isinstance(actual,list),f'{path}: expected array')
        if not path.endswith('.sources'):need(len(actual)==len(expected),f'{path}: wrong cardinality')
        used=set()
        for value in expected:
            for index,candidate in enumerate(actual):
                if index in used:continue
                try:subset(candidate,value,path+'[]')
                except AssertionError:continue
                used.add(index);break
            else:raise AssertionError(f'{path}: missing required item {value!r}')
    else:
        need(type(actual) is type(expected) and actual==expected,f'{path}: expected {expected!r}, actual {actual!r}')


def event_key(event):
    return (event.get('runId') or event.get('timeline',{}).get('turnId'),event.get('toolCallId'))


def tool_events(turn):
    starts={};finishes={}
    for frame in turn.get('frames',[]):
        for event in walk(frame):
            kind=event.get('type')
            if kind not in ('tool_call_started','tool_call_finished'):continue
            need(event.get('toolCallId'),'tool event has no actual call identity')
            key=event_key(event)
            (starts if kind=='tool_call_started' else finishes)[key]=event
    for key in finishes:need(key in starts,f'tool finished without actual started event: {key}')
    return [(event,finishes.get(key)) for key,event in starts.items()]


def tool_name(event):return event.get('name') or event.get('toolName')
def serialize(value):return json.dumps(value,ensure_ascii=False,sort_keys=True)


def verify(manifest,scenario_id,run,manifest_sha):
    scenario=next(s for s in manifest['scenarios'] if s['id']==scenario_id)
    artifact=json.loads((run/'journey.json').read_text())
    need(artifact['id']==scenario_id,'journey belongs to a different scenario')
    need(artifact.get('runnerPassed') is True, 'runner did not complete successfully')
    need(artifact.get('manifestSha256') == manifest_sha, 'journey manifest identity differs from fixed data')
    project=Path(artifact['projectRoot']).resolve(strict=True)
    def inside(relative):
        path=project/relative
        need(not path.is_symlink(),f'output/input must not be symlink: {relative}')
        need(path.resolve().is_relative_to(project),f'escaped project: {relative}')
        return path
    for path,expected_hash in scenario['inputSha256'].items():
        actual=inside(path)
        need(actual.is_file(),f'missing source: {path}')
        need(hashlib.sha256(actual.read_bytes()).hexdigest()==expected_hash,f'source modified: {path}')
    for relative in scenario.get('forbiddenPaths',[]):need(not inside(relative).exists(),f'forbidden business output appeared: {relative}')
    for spec in scenario['expect']['files']:
        path=inside(spec['path']);need(path.is_file(),f'missing requested deliverable: {spec["path"]}')
        if 'jsonSubset' in spec:
            actual=json.loads(path.read_text());subset(actual,spec['jsonSubset'],spec['path'])
            sources=actual.get('sources',[])
            allowed=set(scenario['files'])|{'conversation:previous-user','outputs/receipts.jsonl'}
            need(all(source in allowed for source in sources),'citation to an undeclared source')
        elif 'jsonlEquals' in spec:
            actual=[json.loads(line)for line in path.read_text().splitlines()if line.strip()]
            subset(actual,spec['jsonlEquals'],spec['path'])
        if 'minCharacters' in spec:
            text=path.read_text();need(len(text.strip())>=spec['minCharacters'],'empty/insubstantial requested explanation')
            for citation in spec.get('citations',[]):need(citation in text,f'missing source citation: {citation}')
    turns=artifact.get('turns',[]);need(len(turns)==len(scenario['turns']),'missing journey phase')
    calls=[(index,start,end)for index,turn in enumerate(turns)for start,end in tool_events(turn)]
    successful=[(i,s,e)for i,s,e in calls if e and e.get('ok') is True]
    need(calls,'no real tool trace; prose is not execution evidence')
    trace=scenario['trace']
    for field in ['successfulReadAny','successfulWriteAny']:
        if field in trace:need(any(tool_name(s)in trace[field]for _,s,_ in successful),f'missing actual {field} capability')
    # Paths must be tied to successful read-capable calls, not merely echoed in
    # a user prompt or report-writing arguments. Full provider tool results are
    # also retained by the runner for manual review of shell wildcard reads.
    read_calls=[(s,e)for _,s,e in successful if tool_name(s)in trace.get('successfulReadAny',[])]
    def decoded_preview(start):
        preview=start.get('argsPreview','')
        try:return json.loads(preview)
        except (ValueError,TypeError):return preview
    read_observation='\n'.join(serialize({'args':decoded_preview(s),'result':e})for s,e in read_calls)
    tool_results=[]
    for record in artifact.get('provider',[]):
        request=record.get('request',{});request=request.get('body',request)
        for message in request.get('messages',[]):
            if message.get('role')=='tool':tool_results.append(message)
            # Actual Anthropic tool results arrive as user.content blocks.
            if message.get('role')=='user' and isinstance(message.get('content'),list):
                tool_results.extend(block for block in message['content'] if isinstance(block,dict) and block.get('type')=='tool_result')
    actual_result_text=re.sub(r'\s+',' ',serialize(tool_results).replace('\\n',' '))
    for relative in trace.get('sourcePaths',[]):
        content=scenario['files'][relative]
        probe=re.sub(r'\s+',' ',content.strip())[:70]
        need(relative in read_observation or probe in actual_result_text,f'no actual read evidence for {relative}')
    for index,expected_turn in enumerate(scenario['turns']):
        turn=turns[index]
        need(turn.get('message')==expected_turn['message'],'runner changed the supplied user task')
        need(not turn.get('timedOut'),'journey turn timed out')
        need(not expected_turn.get('cancelWhen'), 'cancel final semantics are not yet wired/pinned by the current journey runner')
        final=turn.get('final',{})
        expected_final=expected_turn.get('expectedFinal',{'type':'turn_completed','finishReason':'completed'})
        subset(final,expected_final,'actual turn final')
        need(any(frame.get('final') is True and frame.get('event')==final for frame in turn.get('frames',[])), 'terminal event is not present in the raw actual frames')
        if expected_turn.get('restartBefore'):
            need(turn.get('sessionKey')==turns[index-1].get('sessionKey'),'restart resumed a different session')
            need(len(artifact.get('cleanup',[]))>=2,'missing actual server lifecycle records for restart')
        if expected_turn.get('noBusinessWrites'):
            need(not any(i==index and tool_name(s)in ['write_file','edit_file','bash']for i,s,_ in successful),
                 'first phase wrote/used uncontrolled shell instead of conversation-only memory')
    if trace.get('secondTurnHistoryMustContain'):
        second=[r for r in artifact.get('provider',[])if r.get('turnIndex')==1]
        need(second,'no actual second-turn provider request')
        for token in trace['secondTurnHistoryMustContain']:
            need(any(token in serialize(r.get('request',{}))for r in second),f'previous conversation absent from actual resumed model request: {token}')
            need(token not in turns[1]['message'],'recovery fact was simply repeated in the second user prompt')
    if trace.get('requirePermissionDenied'):
        denied=[]
        for turn in turns:
            for record in turn.get('permissions',[]):
                answer=record.get('answer',{});params=answer.get('params',{})
                if answer.get('method')=='permission_decide' and params.get('decision')=='deny':
                    request=record.get('request',{})
                    need(params.get('requestId')==request.get('requestId'),'deny not tied to actual permission request')
                    need(any(frame.get('id')==answer.get('id')for frame in turn.get('frames',[])),'missing actual permission_decide response')
                    denied.append(record)
        need(denied,'no actual denied permission roundtrip')
        need(any(e and e.get('ok')is False for _,_,e in calls),'denied write never produced actual failed tool result')
    if trace.get('forbidSuccessfulWrites'):
        need(not any(tool_name(s)in ['write_file','edit_file','bash']for _,s,_ in successful),'write-capable tool succeeded despite deny')
    if trace.get('forbidAfterDeniedToolCalls'):
        need(len([1 for _,s,_ in calls if tool_name(s)in ['write_file','edit_file','bash']])==1,'model retried/bypassed denied write')
    if trace.get('scriptInvocationExactlyOnce'):
        script=trace['scriptInvocationExactlyOnce']
        def invokes(start):
            if tool_name(start)!='bash':return False
            preview=start.get('argsPreview','')
            try:
                args=json.loads(preview);command=args.get('command',preview)
            except (ValueError,AttributeError):command=preview
            tokens=shlex.split(command)
            for index,token in enumerate(tokens[:-1]):
                if Path(token).name not in ('python','python3'):continue
                following=tokens[index+1:]
                if following and following[0]=='-u':following=following[1:]
                if following and following[0].endswith(script):return True
            return False
        executions=[s for _,s,_ in calls if invokes(s)]
        need(len(executions)==1,'receipt script was not actually invoked exactly once')
    if trace.get('requireCancelAccepted'):
        first=turns[0];abort=first.get('abortRequest',{})
        need(abort.get('method')=='abort_turn','no actual cancel request recorded')
        need(any(f.get('id')==abort.get('id')for f in first.get('frames',[])),'cancel request has no actual response')
        evidence=artifact.get('cancelEvidence',{})
        need(evidence.get('triggerFile')=='signals/receipt-committed.json' and evidence.get('triggerObservedBeforeAbort')is True,
             'missing durable trigger-before-cancel evidence')
    if trace.get('requireOwnerExited'):
        evidence=artifact.get('cancelEvidence',{})
        owner=json.loads(inside('signals/owner.json').read_text())
        need(evidence.get('ownerPid')==owner['pid'] and evidence.get('ownerExited')is True and evidence.get('observations'),
             'missing actual owned process exit observations after cancel')
    # Check actual structured file arguments; do not treat a source-name
    # substring inside arbitrary shell code as proof of filesystem confinement.
    full_arguments={}
    for record in artifact.get('provider',[]):
        request=record.get('request',{});request=request.get('body',request)
        for node in walk(request.get('messages',[])):
            if node.get('type')=='tool_use' and node.get('id') and isinstance(node.get('input'),dict):
                full_arguments[node['id']]=node['input']
            if node.get('type')=='function' and node.get('id') and isinstance(node.get('function'),dict):
                try:full_arguments[node['id']]=json.loads(node['function'].get('arguments',''))
                except (ValueError,TypeError):pass
    unknown=[];checked_paths=[]
    for _,start,_ in calls:
        name=tool_name(start)
        if name in ('read_file','write_file','edit_file'):
            args=full_arguments.get(start['toolCallId'])
            if args is None:
                try:args=json.loads(start.get('argsPreview',''))
                except (ValueError,TypeError):args=None
            if not isinstance(args,dict):
                unknown.append({'toolCallId':start['toolCallId'],'tool':name,'reason':'complete structured args unavailable'});continue
            path_value=args.get('file_path',args.get('filePath',args.get('path')))
            if not isinstance(path_value,str):
                unknown.append({'toolCallId':start['toolCallId'],'tool':name,'reason':'file path unavailable'});continue
            need(not path_value.startswith('~'), 'file operation used home alias outside the declared project')
            path=Path(path_value);path=path if path.is_absolute() else project/path
            need(path.resolve().is_relative_to(project),f'actual file operation escaped project: {name}')
            checked_paths.append({'toolCallId':start['toolCallId'],'path':str(path)})
        elif name=='bash':
            unknown.append({'toolCallId':start['toolCallId'],'tool':name,'reason':'shell filesystem effects require manual review; string matching is not confinement proof'})
        else:
            unknown.append({'toolCallId':start['toolCallId'],'tool':name,'reason':'filesystem access semantics not checked by this dataset oracle'})
    cleanup=artifact.get('cleanup',[])
    need(cleanup and all(r.get('remaining')==[] and r.get('socketReleased')is True for r in cleanup),'owned process/socket cleanup not demonstrated')
    return {'status':'passed_with_manual_review' if unknown else 'passed','machineAssertionsPassed':True,
            'fullyVerified':not unknown,'scenario':scenario_id,'checkedInputFiles':len(scenario['files']),
            'checkedOutputFiles':len(scenario['expect']['files']),'actualToolCalls':len(calls),
            'constraints':{'externalProjectAccess':{'status':'manual_required' if unknown else 'checked_structured_paths','checked':checked_paths,'unknown':unknown}},
            'note':'Narrative quality/semantic consistency still receives human review; this is not a prose string matcher.'}

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--manifest',type=Path,default=Path(__file__).with_name('manifest.json'))
    parser.add_argument('--scenario',required=True);parser.add_argument('--run',type=Path,required=True)
    args=parser.parse_args()
    try:
        raw=args.manifest.read_bytes()
        print(json.dumps(verify(json.loads(raw),args.scenario,args.run,hashlib.sha256(raw).hexdigest()),ensure_ascii=False,indent=2))
    except Exception as error:print(json.dumps({'status':'failed','error':str(error)},ensure_ascii=False));raise SystemExit(1)
