import json
import re
import sys
from pathlib import Path

def verify(value):
    assert len(value['sources']) == 2
    assert len({row['path'] for row in value['sources']}) == 2
    assert all(re.fullmatch('[0-9a-f]{64}', row['sha256']) for row in value['sources'])
    assert value['sessions'] == {'a':{'totalUsd':10,'spentUsd':0.5},'b':{'totalUsd':10,'spentUsd':0},'settledRows':1}
    assert value['projects'] == {'a':{'totalUsd':10,'spentUsd':0.75},'b':{'totalUsd':10,'spentUsd':0.75},'other':{'totalUsd':20,'spentUsd':0}}
    assert value['restart'] == {'loaded':{'totalUsd':10,'spentUsd':0.75},'beforeReconfigure':{'totalUsd':10,'spentUsd':0.75},'expired':{'totalUsd':20,'spentUsd':0}}
    failure = value['failedWrite']
    assert failure['firstError']['name'] == 'Error' and failure['firstError']['code'] == 'EISDIR'
    assert '<ROOT>/failed/gateway/sdk-task-budget-ledger.jsonl' in failure['firstError']['message']
    assert failure['repeated'] == 'resolved', 'preserve the observed baseline false-success defect'
    assert failure['snapshot'] == {'totalUsd':10,'spentUsd':0.5}
    assert value['cancel']['error']['name'] == 'AbortError'
    assert value['cancel']['ledgerExists'] is False and value['cancel']['configPublished'] is False

if __name__ == '__main__':
    verify(json.loads(Path(sys.argv[1]).read_text()))
    print('PASS: complete fixed-TS budget contract, including its observed IO defect')
