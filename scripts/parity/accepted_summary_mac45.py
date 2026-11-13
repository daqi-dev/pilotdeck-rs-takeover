import sys
if not sys.flags.isolated:
    raise SystemExit('run with -I -B')
sys.dont_write_bytecode = True
import argparse
import importlib.util
import json
from pathlib import Path
HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('original53_mac45_exact_deadline', HERE / 'deadline_accepted_mac45.py')
M = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = M
spec.loader.exec_module(M)
parser = argparse.ArgumentParser()
parser.add_argument('--evidence-root', type=Path, required=True)
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()
result = M.summarize(args.evidence_root)
M.G.write_result_exclusive(args.output, json.dumps(result, ensure_ascii=False, indent=2) + '\n')
print(json.dumps({'strict': result['strict_original']['score'], 'strict_verdict': result['strict_original']['verdict'],
                  'accepted': result['accepted']['score'], 'accepted_verdict': result['accepted']['verdict'],
                  'failed': result['accepted']['failed_scenarios']}))
raise SystemExit(0 if result['accepted']['verdict'] == 'PASS' else 1)
