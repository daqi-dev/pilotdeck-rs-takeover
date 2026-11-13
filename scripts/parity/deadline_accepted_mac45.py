"""Accepted comparison for one fixed45 batch; no external registration authority."""
import sys
sys.dont_write_bytecode = True
import copy
import hashlib
import importlib.util
import json
from pathlib import Path, PurePosixPath

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
REG_SHA = '21bdf4fa82738a8165a6a29c0593787d113facdf139b2b1ad53c420f49bf7517'
LEGACY_SHA = 'df8f756a75e3b90f27e6ca9bea6e37481b230f1c2eb9e6b4a74965b6e6f84c92'


def pin(path, expected):
    data = Path(path).read_bytes()
    if hashlib.sha256(data).hexdigest() != expected:
        raise ValueError('sealed bytes changed: ' + str(path))
    return data


def registration():
    return json.loads(pin(HERE / 'registrations/original53-mac45-deadline-20261114.json', REG_SHA))


legacy = HERE / 'deadline_accepted.py'
pin(legacy, LEGACY_SHA)
spec = importlib.util.spec_from_file_location('mac45_unchanged_legacy_deadline', legacy)
D = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = D
spec.loader.exec_module(D)
G = D.G


class Mac45Deadline(D.DeadlineAccepted):
    def __init__(self, evidence_root):
        self.evidence_root = Path(evidence_root).resolve()
        self._reset()

    def _reset(self):
        # Restore from code-pinned bytes at every public boundary. A caller's
        # mutated CASES/SEEDS/allowed_paths is never registration authority.
        reg = registration()
        self.CASES = copy.deepcopy(reg['cases'])
        self.allowed_paths = {binding['relative'] for binding in reg['io_bindings'].values()}
        self.allowed_paths.add(reg['strict_score']['path'])
        self.SEEDS = {'deadline': self.rows(next(case for case in self.CASES
                                               if case['side'] == 'ts' and case['round'] == 1))}
        self.CATALOG = G.scenario_catalog()

    def _side_guard(self, case, rows, ledger):
        sealed = registration()['cases']
        D.require(any(D.canon(case) == D.canon(original) for original in sealed),
                  'case is not exact immutable45 deadline case')
        D.require(case['scenario'] == 'deadline', '45 admission only deadline')
        extras = [i for i, row in enumerate(rows) if row.get('event') == 'gateway_submit_failed']
        D.require(extras == case['extraIndices'] and len(extras) == case['extraCount'],
                  'exact case extra ownership/count/index')
        for original in case['exact_optional_records']:
            D.require(D.canon(rows[original['index']]) == D.canon(original['record']),
                      'exact original optional record including envelope')
        return super()._side_guard(case, rows, ledger)

    def evaluate_files(self, left_file, right_file):
        self._reset()
        result = super().evaluate_files(left_file, right_file)
        result['rule'] = 'C-DEADLINE-MAC45-EXACT-001'
        return result

    def evaluate_control_files(self, left_file, right_file):
        self._reset()
        cases = registration()['strict_control_cases']
        selected = []
        for path in (left_file, right_file):
            case = next((case for case in cases if self.resolve(case['path']) == Path(path)), None)
            D.require(case is not None and case['scenario'] == 'deadline_during_tool',
                      'fixed during_tool strict control only')
            raw = self.rows(case)
            ledger = self.ledger(case)
            D.require(ledger['valid_round'] is True and ledger['identity_ok'] is True,
                      'control ledger identity')
            D.require(not any(row.get('event') == 'gateway_submit_failed' for row in raw),
                      'during_tool zero optional record')
            selected.append(raw)
        strict = self.original_result('deadline_during_tool', *selected)
        return {'strict_verdict': strict['verdict'], 'accepted_verdict': strict['verdict'],
                'adaptation_applied': False, 'strict': strict}

    def verify_closure(self):
        self._reset()
        reg = registration()
        strict = json.loads(pin(self.resolve(reg['strict_score']['path']),
                               reg['strict_score']['sha256']))
        expected_bindings = {}
        for kind in ('input', 'code'):
            observed = strict[kind + '_sha256']
            D.require(observed == reg['strict_' + kind + '_closure'],
                      'unchanged original absolute ' + kind + ' keys and hashes')
            for original, expected in observed.items():
                if original.startswith(reg['original_input_root'] + '/'):
                    relative = original[len(reg['original_input_root']) + 1:]
                    origin = 'capture-root'
                    D.require(kind == 'input', 'code origin must remain original managed')
                else:
                    prefix = reg['original_code_root'] + '/'
                    D.require(original.startswith(prefix), 'exact original managed prefix')
                    relative = original[len(prefix):]
                    origin = 'judge-root'
                    D.require(kind == 'code' or relative == 'specs/parity/accepted-diffs.md',
                              'only one known managed ledger input mapping')
                parsed = PurePosixPath(relative)
                D.require(not parsed.is_absolute() and '..' not in parsed.parts and str(parsed) == relative,
                          'safe exact relative IO binding')
                expected_bindings[original] = {'relative': relative, 'sha256': expected,
                                               'origin': origin, 'kind': kind}
                pin(self.resolve(relative), expected)
                if kind == 'code':
                    pin(G.WORKTREE / relative, expected)
        D.require(expected_bindings == reg['io_bindings'], 'exact closed677 IO bindings')
        for relative, expected in reg['baseline_sources'].items():
            pin(G.BASELINE / relative, expected)
        D.require(strict['valid_score'] is True and strict['verdict'] == 'FAIL' and strict['k'] == 5,
                  'original valid strict FAIL k5')
        D.require(strict['score'] == {'numerator': 52, 'denominator': 53, 'fraction': '52/53'},
                  'exact new strict52/53')
        D.require(set(strict['per_scenario']) == set(self.CATALOG) and len(self.CATALOG) == 53,
                  'all53 unchanged')
        D.require([sid for sid, value in strict['per_scenario'].items() if value['verdict'] != 'PASS'] == ['deadline'],
                  'other52 including during_tool strictly PASS')
        D.require([row['run'] for row in strict['per_scenario']['deadline']['runs'] if row['verdict'] != 'PASS'] == [2, 5],
                  'exact first paired failures2/5 retained')
        return reg, strict


def compact(result):
    return {key: result[key] for key in ('verdict', 'triggered_rules')} | {
        'difference_paths': [row['path'] for row in result.get('channel', {}).get('differences', [])],
        'health': result.get('health', {}), 'error': result.get('error')}


def summarize(root):
    engine = Mac45Deadline(root)
    reg, strict = engine.verify_closure()
    accepted = copy.deepcopy(strict['per_scenario'])
    runs = []
    for index in range(1, 6):
        cases = [next(case for case in registration()['cases']
                      if case['side'] == side and case['round'] == index) for side in ('ts', 'rust')]
        result = engine.evaluate_files(*(engine.resolve(case['path']) for case in cases))
        D.require(compact(result['strict']) == strict['per_scenario']['deadline']['runs'][index - 1]['paired'],
                  'original paired verdict and full errors unchanged')
        runs.append({'run': index, **result})
    accepted['deadline'] = {'verdict': 'PASS' if all(row['accepted_verdict'] == 'PASS' for row in runs) else 'FAIL',
                            'counted': True, 'policy': 'C-DEADLINE-MAC45-EXACT-001',
                            'strict_original': strict['per_scenario']['deadline'], 'accepted_runs': runs}
    D.require(all(accepted[sid] == strict['per_scenario'][sid] for sid in accepted if sid != 'deadline'),
              'all other52 exact objects unchanged')
    passed = sum(row['verdict'] == 'PASS' for row in accepted.values())
    return {'schema': 'original53-accepted-summary/v1',
            'registration': {'id': reg['id'], 'sha256': REG_SHA, 'rule': reg['rule']},
            'strict_original_score_sha256': reg['strict_score']['sha256'], 'strict_original': strict,
            'accepted': {'verdict': 'PASS' if passed == 53 else 'FAIL',
                         'score': {'numerator': passed, 'denominator': 53, 'fraction': f'{passed}/53'},
                         'k': 5, 'failed_scenarios': [sid for sid, row in accepted.items() if row['verdict'] != 'PASS'],
                         'per_scenario': accepted},
            'revalidated': {'strict_input_files': 673, 'strict_code_files': 4,
                            'adaptable_fixed_raw_files': 10, 'strict_control_raw_files': 10,
                            'other52_unchanged': True, 'futureCapturesAllowed': False},
            'implementation': {name: hashlib.sha256((HERE / name).read_bytes()).hexdigest()
                               for name in ('deadline_accepted_mac45.py', 'accepted_summary_mac45.py')},
            'limitations': 'fixed45 only; no future capture/binary, no full wire/fence or unlimited future side-effect proof'}
