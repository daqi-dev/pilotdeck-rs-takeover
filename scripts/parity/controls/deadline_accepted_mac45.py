import sys
if not sys.flags.isolated:
    raise SystemExit('run with -I -B')
sys.dont_write_bytecode = True
import copy
import importlib.util
import itertools
import json
from pathlib import Path
import subprocess

HERE = Path(__file__).resolve().parent
if len(sys.argv) != 3:
    raise SystemExit('usage: python3 -I -B controls/deadline_accepted_mac45.py EVIDENCE_ROOT NEW_OUTPUT_DIRECTORY')
ROOT = Path(sys.argv[1]).resolve()
OUT = Path(sys.argv[2])
OUT.mkdir(exist_ok=False)
spec = importlib.util.spec_from_file_location('private_mac45_control_subject', HERE.parent / 'deadline_accepted_mac45.py')
M = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = M
spec.loader.exec_module(M)
engine = M.Mac45Deadline(ROOT)
engine.verify_closure()
reg = M.registration()
actual, semantic_controls, identity_controls, auxiliary = [], [], [], []


def get(side, index, controls=False):
    return next(case for case in reg['strict_control_cases' if controls else 'cases']
                if case['side'] == side and case['round'] == index)


for control in [False, True]:
    fn = engine.evaluate_control_files if control else engine.evaluate_files
    for ti, ri in itertools.product(range(1, 6), repeat=2):
        pair = [get('ts', ti, control), get('rust', ri, control)]
        for order in [pair, pair[::-1]]:
            result = fn(*(engine.resolve(case['path']) for case in order))
            assert result['accepted_verdict'] == 'PASS', result
            expected = not control and order[0]['extraCount'] != order[1]['extraCount']
            assert result['adaptation_applied'] is expected
            actual.append({'strictControl': control, 'sides': [(case['side'], case['round']) for case in order], **result})
    for case in reg['strict_control_cases' if control else 'cases']:
        result = fn(engine.resolve(case['path']), engine.resolve(case['path']))
        assert result['strict_verdict'] == result['accepted_verdict'] == 'PASS' and not result['adaptation_applied']
        actual.append({'strictControl': control, 'self': case['path'], **result})


def negative(name, mutate):
    subject = M.Mac45Deadline(ROOT)
    cases = copy.deepcopy([get('ts', 2), get('rust', 2)])
    rows = [subject.rows(case) for case in cases]
    ledgers = [subject.ledger(case) for case in cases]
    mutate(cases, rows, ledgers)
    try:
        result = subject._evaluate_registered_records(*cases, *rows, *ledgers)
    except (ValueError, KeyError, TypeError, IndexError, AttributeError) as error:
        result = {'accepted_verdict': 'FAIL', 'rejection': str(error)}
    assert result['accepted_verdict'] == 'FAIL' and not result.get('adaptation_applied', False), (name, result)
    semantic_controls.append({'name': name, 'reason': result.get('rejection'), 'strict': result.get('strict_verdict')})


def find(rows, kind):
    return next(row for row in rows if row['kind'] == kind)


def extra(rows):
    return next(row for row in rows if row.get('event') == 'gateway_submit_failed')


def renumber(rows):
    for index, row in enumerate(rows):
        row['sequence'] = index


def insert(rows, record, where=None):
    rows.insert(len(rows) - 1 if where is None else where, copy.deepcopy(record))
    renumber(rows)


for field, value in [('text', 'wrong'), ('text', M.D.EXTRA['text'] + ' '),
                     ('kind', 'agent.status'), ('agentScope', 'child'), ('statusKind', 'warning'),
                     ('event', 'other'), ('invocationId', 'wrong'), ('q', 'wrong'),
                     ('sequence', 99), ('sequence', True)]:
    negative('extra field ' + field + ' ' + repr(value)[:25],
             lambda c, r, l, field=field, value=value: extra(r[1]).update({field: value}))
negative('extra unknown field', lambda c, r, l: extra(r[1]).update(code='NEW'))
negative('duplicate extra', lambda c, r, l: insert(r[1], extra(r[1]), 24))
negative('remove actual extra', lambda c, r, l: (r[1].remove(extra(r[1])), renumber(r[1])))


def move_extra(cases, rows, ledgers, index):
    row = extra(rows[1]); rows[1].remove(row); rows[1].insert(index, row); renumber(rows[1])


negative('extra before timeout', lambda c, r, l: move_extra(c, r, l, 22))
negative('extra after terminal', lambda c, r, l: move_extra(c, r, l, len(r[1]) - 1))
negative('extra borrowed into rust1', lambda c, r, l: c.__setitem__(1, copy.deepcopy(get('rust', 1))))
negative('extra wrong scene', lambda c, r, l: c[1].update(scenario='deadline_during_tool'))
negative('extra forged round metadata', lambda c, r, l: c[1].update(round=3))
negative('missing timeout', lambda c, r, l: (r[1].pop(22), renumber(r[1])))
negative('duplicate timeout', lambda c, r, l: insert(r[1], r[1][22], 22))
negative('missing failclosed', lambda c, r, l: (r[1].pop(23), renumber(r[1])))
negative('wrong failclosed source', lambda c, r, l: r[1][23].update(source='transport_interruption'))
negative('failclosed unknown field', lambda c, r, l: r[1][23].update(detail='NEW'))
negative('missing invocation', lambda c, r, l: find(r[1], 'model.request').pop('invocationId'))
negative('operation completed', lambda c, r, l: find(r[1], 'operation.terminal').update(outcome='completed'))
negative('operation wrong code', lambda c, r, l: find(r[1], 'operation.terminal').update(code='OTHER'))
negative('operation sequence value', lambda c, r, l: find(r[1], 'operation.terminal').update(lastAppliedSequence=4))
negative('operation sequence type', lambda c, r, l: find(r[1], 'operation.terminal').update(lastAppliedSequence='3'))
negative('operation unknown field', lambda c, r, l: find(r[1], 'operation.terminal').update(detail='NEW'))
negative('terminal completed', lambda c, r, l: find(r[1], 'terminal').update(outcome='completed'))
negative('terminal nonempty', lambda c, r, l: find(r[1], 'terminal').update(output='done'))
negative('sideeffect count', lambda c, r, l: find(r[1], 'side_effect.state').update(sideEffectCount=1))
negative('sideeffect bool zero', lambda c, r, l: find(r[1], 'side_effect.state').update(sideEffectCount=False))
negative('sideeffect actual map', lambda c, r, l: find(r[1], 'side_effect.state').update(counts={'file': 1}))
negative('durable count', lambda c, r, l: find(r[1], 'durable.state').update(durableStatusCount=1))
negative('request new field', lambda c, r, l: find(r[1], 'model.request').update(newField='changed'))
negative('usage added', lambda c, r, l: find(r[1], 'model.request').update(usage={'inputTokens': 1}))
negative('same usage both sides', lambda c, r, l: [find(side, 'model.request').update(usage={'inputTokens': 1}) for side in r])
negative('posttimeout model', lambda c, r, l: insert(r[1], find(r[1], 'model.request')))
negative('posttimeout successful tool', lambda c, r, l: insert(r[1], {**r[1][24], 'kind': 'tool.finish', 'success': True, 'name': 'write_file'}))
negative('posttimeout gateway error', lambda c, r, l: insert(r[1], {**r[1][24], 'kind': 'gateway.error', 'message': 'bad'}))
negative('ledger source wrong', lambda c, r, l: l[1]['upstream_summary']['provenance'].update(current='wrong'))
negative('ledger invalid', lambda c, r, l: l[1].update(valid_round=False))
negative('identity wrong hash', lambda c, r, l: r[1][-1].update(sha256='0' * 64))
negative('identity missing', lambda c, r, l: r[1].pop())
negative('driver SHA changed', lambda c, r, l: l[1]['driver_sha256'].update({'round.py': '0' * 64}))


def during_extra():
    subject = M.Mac45Deadline(ROOT)
    cases = copy.deepcopy([get('ts', 2, True), get('rust', 2, True)])
    rows = [subject.rows(case) for case in cases]
    ledgers = [subject.ledger(case) for case in cases]
    insert(rows[1], {**{key: rows[1][34][key] for key in M.D.ENVELOPE}, **M.D.EXTRA}, 35)
    result = subject._evaluate_registered_records(*cases, *rows, *ledgers)
    assert result['accepted_verdict'] == 'FAIL' and not result['adaptation_applied']
    semantic_controls.append({'name': 'during_tool same exact extra is outside admission scope', 'reason': result.get('rejection')})


during_extra()


def refused(name, fn):
    try:
        fn()
    except (ValueError, KeyError, TypeError, IndexError, AttributeError):
        identity_controls.append({'name': name, 'rejected': True})
    else:
        raise AssertionError(name + ' accepted')


refused('public unknown path', lambda: engine.evaluate_files(OUT / 'unknown.jsonl', engine.resolve(get('ts', 1)['path'])))
alias = OUT / 'alias.jsonl'; alias.symlink_to(engine.resolve(get('rust', 2)['path']))
refused('public unregistered alias', lambda: engine.evaluate_files(alias, engine.resolve(get('ts', 2)['path'])))
engine.CASES = [{'path': str(alias), 'sha256': '0' * 64}]
engine.allowed_paths.add(str(alias))
refused('caller Map cannot authorize unknown path', lambda: engine.evaluate_files(alias, engine.resolve(get('ts', 2)['path'])))
engine.SEEDS = {'deadline': []}
assert engine.evaluate_files(engine.resolve(get('ts', 2)['path']), engine.resolve(get('rust', 2)['path']))['accepted_verdict'] == 'PASS'
auxiliary.append({'name': 'caller seed Map ignored at public boundary', 'pass': True})

shadow = OUT / 'shadow'; shadow.mkdir()
relative_paths = {binding['relative'] for binding in reg['io_bindings'].values()} | {reg['strict_score']['path']}
for relative in relative_paths:
    target = shadow / relative; target.parent.mkdir(parents=True, exist_ok=True); target.symlink_to(ROOT / relative)
for label, relative in [('nondeadline full closure', next(value for value in relative_paths if '/archive/ref/' in value and value.endswith('run.json'))),
                        ('mixed-prefix ledger input', 'specs/parity/accepted-diffs.md'),
                        ('strict original score', reg['strict_score']['path']),
                        ('four code mapping', 'scripts/parity/score.py')]:
    target = shadow / relative; target.unlink(); target.write_bytes((ROOT / relative).read_bytes() + b'\n')
    refused(label + ' byte mutation', lambda: M.Mac45Deadline(shadow).verify_closure())
    target.unlink(); target.symlink_to(ROOT / relative)


def cli(name, argv, expected, absent=None):
    result = subprocess.run(argv, capture_output=True, text=True)
    (OUT / (name + '.stdout.log')).write_text(result.stdout)
    (OUT / (name + '.stderr.log')).write_text(result.stderr)
    assert (result.returncode == 0) is expected, (name, result)
    if absent is not None:
        assert not absent.exists()
    return {'name': name, 'argv': argv, 'exit': result.returncode}


base = [sys.executable, '-I', '-B', str(HERE.parent / 'accepted_summary_mac45.py'), '--evidence-root', str(ROOT)]
summary = OUT / 'summary.json'
auxiliary.append(cli('actual new summary', base + ['--output', str(summary)], True))
result = json.loads(summary.read_bytes())
assert result['strict_original']['score']['fraction'] == '52/53' and result['accepted']['score']['fraction'] == '53/53'
assert M.D.canon(result['strict_original']) == M.D.canon(json.loads((ROOT / reg['strict_score']['path']).read_bytes()))
assert all(result['accepted']['per_scenario'][sid] == result['strict_original']['per_scenario'][sid] for sid in result['accepted']['per_scenario'] if sid != 'deadline')
before = summary.read_bytes(); identity_controls.append(cli('existing output rejects', base + ['--output', str(summary)], False)); assert summary.read_bytes() == before
absent = OUT / 'manifest-must-not-exist.json'
identity_controls.append(cli('external manifest rejects', base + ['--output', str(absent), '--manifest', str(HERE.parent / 'registrations/original53-mac45-deadline-20261114.json')], False, absent))
tamper = OUT / 'tampered-registration'; tamper.mkdir()
for filename in ['deadline_accepted_mac45.py', 'accepted_summary_mac45.py', 'deadline_accepted.py', 'g03_compare.py', 'score.py']:
    (tamper / filename).write_bytes((HERE.parent / filename).read_bytes())
forged = copy.deepcopy(reg); forged['cases'][0]['sha256'] = '0' * 64
(tamper / 'registrations').mkdir()
(tamper / 'registrations/original53-deadline-20261114.json').write_bytes((HERE.parent / 'registrations/original53-deadline-20261114.json').read_bytes())
(tamper / 'registrations/original53-mac45-deadline-20261114.json').write_text(json.dumps(forged))
absent = OUT / 'forged-must-not-exist.json'
identity_controls.append(cli('self authorized registration rejects', [sys.executable, '-I', '-B', str(tamper / 'accepted_summary_mac45.py'), '--evidence-root', str(ROOT), '--output', str(absent)], False, absent))
assert 'sealed bytes changed: ' + str(tamper / 'registrations/original53-mac45-deadline-20261114.json') in (OUT / 'self authorized registration rejects.stderr.log').read_text()
relative = next(value for value in relative_paths if '/archive/ref/' in value and value.endswith('run.json'))
target = shadow / relative; target.unlink(); target.write_bytes((ROOT / relative).read_bytes() + b'\n')
absent = OUT / 'changed-closure-must-not-exist.json'
identity_controls.append(cli('actual CLI nondeadline closure rejects', [sys.executable, '-I', '-B', str(HERE.parent / 'accepted_summary_mac45.py'), '--evidence-root', str(shadow), '--output', str(absent)], False, absent))
target.unlink(); target.symlink_to(ROOT / relative)
assert len(actual) == 120 and sum(bool(row['adaptation_applied']) for row in actual) == 20
output = {'status': 'REGISTERED_MAC45_CONTROLS', 'actualOriginalComparisons': len(actual), 'adaptedOriginalComparisons': 20,
          'semanticNegativeControls': len(semantic_controls), 'identityNegativeControls': len(identity_controls), 'auxiliaryPositiveControls': len(auxiliary),
          'semantic': semantic_controls, 'identity': identity_controls, 'auxiliary': auxiliary, 'allPass': True}
M.G.write_result_exclusive(OUT / 'original-comparisons.json', json.dumps(actual, ensure_ascii=False, indent=2) + '\n')
M.G.write_result_exclusive(OUT / 'controls.json', json.dumps(output, ensure_ascii=False, indent=2) + '\n')
print(json.dumps({key: value for key, value in output.items() if key not in {'semantic', 'identity', 'auxiliary'}}))
