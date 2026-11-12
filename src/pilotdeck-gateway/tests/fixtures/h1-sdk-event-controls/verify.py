"""Validate the complete scoped TS contract; a partial trace is not success."""
import json
import re
import sys
from pathlib import Path

def verify(value):
    sources = value['sources']
    assert len(sources) == 6 and len({row['path'] for row in sources}) == 6
    assert all(re.fullmatch(r'[0-9a-f]{64}', row['sha256']) for row in sources)
    rows = value['rows']
    assert len(rows) == 5
    assert [(row['includeToolProgress'], row['stopOnStructuredOutput']) for row in rows] == [(None, None), (None, None), (True, None), (None, True), (True, True)]
    assert all(row['providerOutputFormat'] is False for row in rows)
    assert len(value['events']) == 4
    assert [(row['forward'],row['kind']) for row in value['events']] == [(False,'text_delta'),(False,'thinking_delta'),(True,'text_delta'),(True,'thinking_delta')]
    for row in value['events']:
        assert len(row['events']) == 1
        event = row['events'][0]
        assert event['runId'] == 'run'
        promoted = row['forward'] and row['kind'] == 'text_delta'
        assert event['type'] == ('subagent_text_delta' if promoted else 'agent_status')
        detail = event if promoted else event['detail']
        assert (detail['subagentId'],detail['subagentType'],detail['text']) == ('child-1','general','child fragment')
        if not promoted:
            assert event['event'] == ('subagent_text_delta' if row['kind'] == 'text_delta' else 'subagent_thinking_delta')
    assert value['structured'] == [{'type':'structured_output','payload':{'status':'ok'},'runId':'run'},{'type':'turn_completed','usage':{},'finishReason':'completed','runId':'run'}]
    assert value['progress'] == [{'type':'tool_progress','toolCallId':'call','toolName':'progress_probe','message':'working','createdAt':'2026-11-13T00:00:00.000Z','runId':'run'}]

if __name__ == '__main__':
    verify(json.loads(Path(sys.argv[1]).read_text()))
    print('PASS: SDK output controls complete scoped contract')
