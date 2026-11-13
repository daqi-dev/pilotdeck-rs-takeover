"""Only oracle apparatus tests with synthetic evidence; NOT product/LLM passes."""
import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import hashlib

ROOT=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('journey_data_oracle',ROOT/'verify.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
manifest=json.loads((ROOT/'manifest.json').read_text())
manifest_sha=hashlib.sha256((ROOT/'manifest.json').read_bytes()).hexdigest()


def fixture(scenario,directory):
    project=directory/'project';project.mkdir(parents=True)
    for name,content in scenario['files'].items():
        p=project/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_text(content)
    for expected in scenario['expect']['files']:
        p=project/expected['path'];p.parent.mkdir(parents=True,exist_ok=True)
        if 'jsonSubset'in expected:p.write_text(json.dumps(expected['jsonSubset'],ensure_ascii=False))
        elif 'jsonlEquals'in expected:p.write_text(''.join(json.dumps(row)+'\n'for row in expected['jsonlEquals']))
        else:p.write_text(' '.join(expected['citations'])+'\n'+('这是装置专用的合成说明，不能当作模型实际完成。'*8))
    turns=[]
    for index,task in enumerate(scenario['turns']):
        frames=[];run=f'synthetic-run-{index}'
        for number,path in enumerate(scenario['trace'].get('sourcePaths',[])):
            call=f'read-{index}-{number}'
            frames.extend([{'event':{'type':'tool_call_started','runId':run,'toolCallId':call,'name':'read_file','argsPreview':json.dumps({'file_path':path})}},
                           {'event':{'type':'tool_call_finished','runId':run,'toolCallId':call,'ok':True,'toolName':'read_file','resultPreview':scenario['files'][path]}}])
        if index==len(scenario['turns'])-1 and scenario['expect']['files']:
            frames.extend([{'event':{'type':'tool_call_started','runId':run,'toolCallId':'write','name':'write_file','argsPreview':json.dumps({'file_path':scenario['expect']['files'][0]['path']})}},
                           {'event':{'type':'tool_call_finished','runId':run,'toolCallId':'write','ok':True,'toolName':'write_file'}}])
        final={'type':'turn_completed','finishReason':'completed'}
        frames.append({'type':'event','final':True,'event':final})
        turns.append({'final':final,'index':index,'message':task['message'],'sessionKey':'synthetic-session','runId':run,'frames':frames,'permissions':[]})
    artifact={'id':scenario['id'],'projectRoot':str(project),'turns':turns,'provider':[],
              'runnerPassed':True,'manifestSha256':manifest_sha,'cleanup':[{'code':0,'remaining':[],'socketReleased':True}for _ in turns]}
    if len(turns)>1:
        artifact['provider']=[{'turnIndex':1,'request':{'messages':[{'role':'user','content':turns[0]['message']},{'role':'assistant','content':'synthetic acknowledgment'},{'role':'user','content':turns[1]['message']}]}}]
    if scenario['trace'].get('requirePermissionDenied'):
        turn=turns[0];call='denied-write';run=turn['runId']
        turn['frames'].extend([{'event':{'type':'tool_call_started','runId':run,'toolCallId':call,'name':'write_file','argsPreview':'protected/approved-plan.json'}},
                              {'event':{'type':'tool_call_finished','runId':run,'toolCallId':call,'ok':False,'toolName':'write_file'}},
                              {'id':'permission-synthetic','type':'response','result':{}}])
        turn['permissions']=[{'request':{'type':'permission_request','requestId':'synthetic-id','toolName':'write_file'},'answer':{'id':'permission-synthetic','method':'permission_decide','params':{'requestId':'synthetic-id','decision':'deny'}}}]
    if scenario['trace'].get('requireCancelAccepted'):
        turn=turns[0];turn['frames'].extend([{'event':{'type':'tool_call_started','runId':turn['runId'],'toolCallId':'book','name':'bash','argsPreview':json.dumps({'command':'python3 scripts/book_receipt.py --invoice source/invoice.json'})}},
                                                   {'id':'abort-synthetic','type':'response','result':{}}])
        turn['abortRequest']={'id':'abort-synthetic','method':'abort_turn'}
        (project/'signals').mkdir();(project/'signals/owner.json').write_text('{"pid":1234}')
        (project/'signals/receipt-committed.json').write_text('{"operation_id":"OP-CAN-021","committed":true}')
        artifact['cancelEvidence']={'triggerFile':'signals/receipt-committed.json','triggerObservedBeforeAbort':True,'ownerPid':1234,'ownerExited':True,'observations':[{'pid':1234,'exists':False}]}
    (directory/'journey.json').write_text(json.dumps(artifact,ensure_ascii=False))
    return artifact,project


def main():
    positives=[];negative=[]
    def reject(name,scenario,mutate):
        with tempfile.TemporaryDirectory(prefix='journey-oracle-negative-')as directory:
            d=Path(directory);artifact,project=fixture(scenario,d);mutate(artifact,project)
            (d/'journey.json').write_text(json.dumps(artifact,ensure_ascii=False))
            try:m.verify(manifest,scenario['id'],d,manifest_sha)
            except AssertionError:negative.append(name);return
            raise AssertionError('oracle missed negative mutation: '+name)
    for scenario in manifest['scenarios']:
        if scenario['id']=='cancel-committed-receipt':continue
        with tempfile.TemporaryDirectory(prefix='journey-oracle-positive-')as directory:
            d=Path(directory);fixture(scenario,d);m.verify(manifest,scenario['id'],d,manifest_sha);positives.append(scenario['id'])
    sales,procurement,restart,denied,cancel=manifest['scenarios']
    # A representative actual Anthropic message shape, without any API call.
    with tempfile.TemporaryDirectory(prefix='journey-anthropic-oracle-')as directory:
        d=Path(directory);artifact,project=fixture(sales,d);uses=[];results=[]
        for frame in artifact['turns'][0]['frames']:
            event=frame.get('event',{})
            if event.get('type')=='tool_call_started' and event.get('name')=='read_file':
                args=json.loads(event['argsPreview']);event['argsPreview']='truncated-preview'
                uses.append({'type':'tool_use','id':event['toolCallId'],'name':'read_file','input':args})
                results.append({'type':'tool_result','tool_use_id':event['toolCallId'],'content':sales['files'][args['file_path']]})
        artifact['provider']=[{'turnIndex':0,'request':{'messages':[{'role':'assistant','content':uses},{'role':'user','content':results}]}}]
        (d/'journey.json').write_text(json.dumps(artifact,ensure_ascii=False))
        result=m.verify(manifest,sales['id'],d,manifest_sha)
        assert result['fullyVerified'] is True
        positives.append('anthropic-user-tool-result-and-full-tool-input')
    def wrong_math(a,p):
        path=p/'outputs/sales-analysis.json';value=json.loads(path.read_text());value['totals']['revenue_cents']+=1;path.write_text(json.dumps(value))
    reject('wrong-business-number',sales,wrong_math)
    reject('input-modified',sales,lambda a,p:(p/'source/transactions.csv').write_text('changed'))
    reject('no-real-tool-trace',sales,lambda a,p:a['turns'][0].update(frames=[]))
    reject('missing-source-reference',sales,lambda a,p:(p/'outputs/销售分析.md').write_text('无来源说明'*100))
    reject('missing-restored-history',restart,lambda a,p:a.update(provider=[]))
    reject('different-restarted-session',restart,lambda a,p:a['turns'][1].update(sessionKey='another-session'))
    reject('no-actual-deny',denied,lambda a,p:a['turns'][0].update(permissions=[]))
    reject('duplicate-side-effect',cancel,lambda a,p:(p/'outputs/receipts.jsonl').write_text((p/'outputs/receipts.jsonl').read_text()*2))
    reject('cancel-unsupported-is-not-green',cancel,lambda a,p:None)
    reject('runner-failed-is-not-green',sales,lambda a,p:a.update(runnerPassed=False))
    reject('manifest-identity-mismatch',sales,lambda a,p:a.update(manifestSha256='0'*64))
    reject('missing-real-terminal',sales,lambda a,p:a['turns'][0].update(final={}))
    def outside_path(a,p):
        a['turns'][0]['frames'][0]['event']['argsPreview']=json.dumps({'file_path':'../outside.txt'})
    reject('structured-path-outside-project',sales,outside_path)
    results={'apparatusOnly':True,'productOrLlmRuns':0,'positiveSyntheticArtifacts':positives,'rejectedMutations':negative}
    output=ROOT/'oracle-self-test.json';output.write_text(json.dumps(results,ensure_ascii=False,indent=2)+'\n')
    print(json.dumps(results,ensure_ascii=False))

if __name__=='__main__':main()
