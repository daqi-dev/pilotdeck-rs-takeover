#!/usr/bin/env python3
"""Synthetic local receipt append. A durable signal follows exactly one append."""
import argparse,json,os,signal
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--invoice',required=True);a=p.parse_args()
invoice=json.loads(Path(a.invoice).read_text())
root=Path.cwd();(root/'outputs').mkdir(exist_ok=True);(root/'signals').mkdir(exist_ok=True)
row={'operation_id':invoice['operation_id'],'invoice_id':invoice['invoice_id'],'amount_cents':invoice['amount_cents']}
with (root/'outputs/receipts.jsonl').open('a') as stream:
    stream.write(json.dumps(row,sort_keys=True)+'\n');stream.flush();os.fsync(stream.fileno())
with (root/'signals/owner.json').open('x') as stream:
    json.dump({'pid':os.getpid(),'pgid':os.getpgid(0)},stream);stream.flush();os.fsync(stream.fileno())
with (root/'signals/receipt-committed.json').open('x') as stream:
    json.dump({'operation_id':row['operation_id'],'committed':True},stream);stream.flush();os.fsync(stream.fileno())
print('Receipt committed; waiting for operator',flush=True)
while True:signal.pause()
