"""Private diagnostic contract; no change to frozen acceptance."""
import json, re
from pathlib import Path
SID = 'sidecar_parent_close_after_admission'
TEXT = 'MOCK_PARENT_AFTER_ADMISSION::Close the parent after the child starts'

def check(directory):
    p = Path(directory)
    ledger = json.loads((p / 'run.json').read_text())
    rows = [json.loads(x) for x in (p / 'artifacts' / f'{SID}.pilotdeck-current-sidecar.jsonl').read_text().splitlines()]
    if not (ledger['valid_round'] is True and ledger['identity_ok'] is True and (ledger['side'] in ('ts', 'rust'))):
        raise ValueError("parent-close business contract: ledger['valid_round'] is True and ledger['identity_ok'] is True and (ledger['side'] in ('ts', 'rust'))")
    if not all((x.get('scenarioId') == SID for x in rows)):
        raise ValueError("parent-close business contract: all((x.get('scenarioId') == SID for x in rows))")
    inv = {x.get('invocationId') for x in rows}
    if not (len(inv) == 1 and None not in inv):
        raise ValueError('parent-close business contract: len(inv) == 1 and None not in inv')
    seq = [x['sequence'] for x in rows]
    if not seq == list(range(len(rows))):
        raise ValueError('parent-close business contract: seq == list(range(len(rows)))')

    def one(pred):
        xs = [x for x in rows if pred(x)]
        if not len(xs) == 1:
            raise ValueError('parent-close business contract: len(xs) == 1')
        return xs[0]
    proof = one(lambda x: x['kind'] == 'harness.proof' and x.get('state') == 'sidecar_identity')
    if not (proof['side'] == ledger['side'] and proof['sha256'] == proof['expectedSha256'] == ledger['expected_sha256'] and (proof['spawnObserved'] is True)):
        raise ValueError("parent-close business contract: proof['side'] == ledger['side'] and proof['sha256'] == proof['expectedSha256'] == ledger['expected_sha256'] and (proof['spawnObserved'] is True)")
    start = one(lambda x: x['kind'] == 'tool.start' and x.get('name') == 'subagent')
    finish = one(lambda x: x['kind'] == 'tool.finish' and x.get('name') == 'subagent')
    result = one(lambda x: x['kind'] == 'tool.result' and x.get('toolCallId') == start['toolCallId'])
    if not (finish['toolCallId'] == start['toolCallId'] and finish['success'] is True and (result['result']['type'] == 'success') and re.search('accepted message .* for subagent ', result['result']['data']['preview'])):
        raise ValueError("parent-close business contract: finish['toolCallId'] == start['toolCallId'] and finish['success'] is True and (result['result']['type'] == 'success') and re.search('accepted message .* for subagent ', result['result']['data']['preview'])")
    child = one(lambda x: x['kind'] == 'model.request' and x.get('agentScope') == 'child')
    req = one(lambda x: x['kind'] == 'sidecar.lifecycle' and x.get('state') == 'parent_close_requested')
    closed = one(lambda x: x['kind'] == 'sidecar.lifecycle' and x.get('state') == 'parent_closed')
    if not (req['stage'] == 'child_model_started' and req['subagentModelRequests'] == 1 and (closed['stage'] == 'child_first_drain') and (closed['parentClosed'] is True) and (closed['subagentModelRequests'] == 1)):
        raise ValueError("parent-close business contract: req['stage'] == 'child_model_started' and req['subagentModelRequests'] == 1 and (closed['stage'] == 'child_first_drain') and (closed['parentClosed'] is True) and (closed['subagentModelRequests'] == 1)")
    effect = one(lambda x: x['kind'] == 'side_effect.state')
    op = one(lambda x: x['kind'] == 'operation.terminal')
    term = one(lambda x: x['kind'] == 'terminal')
    if not (effect['sideEffectCount'] == 0 and effect['counts'] == {} and (start['sequence'] < finish['sequence'] < result['sequence'] < child['sequence'] < req['sequence'] < closed['sequence'] < effect['sequence'] < op['sequence'] < term['sequence'])):
        raise ValueError("parent-close business contract: effect['sideEffectCount'] == 0 and effect['counts'] == {} and (start['sequence'] < finish['sequence'] < result['sequence'] < child['sequence'] < req['sequence'] < closed['sequence'] < effect['sequence'] < op['sequence'] < term['sequence'])")
    output = [x for x in rows if x['kind'] == 'user.output']
    parent_responses = [x for x in rows if x['kind'] == 'model.response' and x.get('agentScope') == 'parent' and (x.get('attempt') == 2)]
    if term['outcome'] == 'completed':
        if not (term['resultType'] == 'success' and term['stopReason'] == term['durableStopReason'] == 'completed' and (term['output'] == TEXT) and (op['outcome'] == 'completed')):
            raise ValueError("parent-close business contract: term['resultType'] == 'success' and term['stopReason'] == term['durableStopReason'] == 'completed' and (term['output'] == TEXT) and (op['outcome'] == 'completed')")
        if not (len(output) == len(parent_responses) == 1 and output[0]['text'] == TEXT and (parent_responses[0]['response']['content'] == TEXT) and (req['sequence'] < parent_responses[0]['sequence'] < output[0]['sequence'] < closed['sequence'])):
            raise ValueError("parent-close business contract: len(output) == len(parent_responses) == 1 and output[0]['text'] == TEXT and (parent_responses[0]['response']['content'] == TEXT) and (req['sequence'] < parent_responses[0]['sequence'] < output[0]['sequence'] < closed['sequence'])")
        return 'completed'
    if not (term['outcome'] == 'cancelled' and term['resultType'] == 'aborted' and (term['stopReason'] == term['durableStopReason'] == 'aborted_streaming') and (term['output'] == '') and (op['outcome'] == 'cancelled') and (not output) and (not parent_responses)):
        raise ValueError("parent-close business contract: term['outcome'] == 'cancelled' and term['resultType'] == 'aborted' and (term['stopReason'] == term['durableStopReason'] == 'aborted_streaming') and (term['output'] == '') and (op['outcome'] == 'cancelled') and (not output) and (not parent_responses)")
    return 'cancelled'
