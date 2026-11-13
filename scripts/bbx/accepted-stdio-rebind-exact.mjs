// Approved independent exact-three-raw entry. Original strict judge remains unchanged.
// No argv input override, source capture, product execution, or network calls.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createHash } from 'node:crypto';
import { isDeepStrictEqual as equal } from 'node:util';
import { spawnSync } from 'node:child_process';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../..');
const manifestFile = path.join(here, 'registrations/stdio-rebind-exact-20261114.json');
const MANIFEST_SHA = '89e3c4b1eaf445002bb715f40ac9a4482dd0e93216c09f7e998a9719565d2098';
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const assert = (ok, why) => { if (!ok) throw Error(why); };
const clone = value => structuredClone(value);
const json = file => JSON.parse(fs.readFileSync(file, 'utf8'));
const resolve = file => path.resolve(root, file);
const manifestBytes = fs.readFileSync(manifestFile);
const checkManifest = bytes => assert(sha(bytes) === MANIFEST_SHA, 'fixed manifest hash');
checkManifest(manifestBytes);
const m = JSON.parse(manifestBytes);
assert(process.version === m.offlineNode.version, 'offline node version');
assert(sha(fs.readFileSync(process.execPath)) === m.offlineNode.sha256, 'offline node executable hash');
for (const [file, hash] of Object.entries({ ...m.referenceSHA256, ...m.frozenJudgeFiles })) {
  assert(sha(fs.readFileSync(resolve(file))) === hash, `provenance hash: ${file}`);
}
assert(sha(fs.readFileSync(resolve(m.scenarioFile))) === m.identities.sceneSHA256, 'fixed scene hash');
assert(sha(fs.readFileSync(resolve(m.judgeFile))) === m.identities.judgeSHA256, 'fixed comparator hash');
const scenario = json(resolve(m.scenarioFile));
const { validateArtifact } = await import(pathToFileURL(resolve(m.evidenceModule)));
const rows = raw => raw.tree[m.treePath].jsonl;
const projection = raw => raw.tree[m.treePath + '.projections.json'].json;
const SLOT = ['sequence', 'entryId', 'parentEntryId', 'createdAt'];
const journalTypes = ['agent_loop_operation_started', 'agent_loop_operation_accepted', 'agent_loop_operation_terminal'];
const tupleKeys = ['runId', 'operationId', 'requestId', 'sessionId', 'turnId', 'binding'];
const tuple = row => Object.fromEntries(tupleKeys.map(key => [key, row[key]]));
const fresh = row => row.type === 'session_metadata' && row.metadata?.titleSource && !row.metadata.isSnapshot;

// Inner semantic guards are exercised independently by the controls below.
// Production entrance additionally requires every original byte hash, above/below.
function guard(raw, label) {
  const expected = m.raws[label];
  const health = validateArtifact(raw, scenario, { runId: m.captureRunId, side: expected.side,
    scenario: m.expectedScenario, inputs: m.expectedInputs });
  assert(health.length === 0, `original health/identity gate: ${health.join('; ')}`);
  assert(raw.scenario === m.scenario && raw.side === expected.side, 'top-level identity');
  assert(equal(raw.failures, []) && equal(raw.shutdown, { code: 0, signal: null }), 'health/shutdown');
  assert(equal(raw.volatile, []) && equal(raw.accepted, []) && !raw.volatilePolicies, 'no additional policy');
  assert(Object.keys(raw.provider).length === 2 && raw.provider.title?.length === 1 && raw.provider.main?.length === 1,
    'exact title/main provider counts');
  const r = rows(raw), p = projection(raw);
  assert(r.length === 23 && new Set(r.map(v => v.entryId)).size === 23, '23 unique durable rows');
  r.forEach((v, i) => {
    assert(v.sequence === i + 1, 'durable slot sequence');
    assert(typeof v.entryId === 'string' && v.entryId.length > 0, 'durable entry identity');
    assert(v.parentEntryId === (i ? r[i - 1].entryId : null), 'durable parent chain');
    assert(v.sessionId === 's-bbx-sidecar' && v.turnId === r[0].turnId, 'single session/turn');
  });
  const j = journalTypes.map(type => {
    const matches = r.filter(v => v.type === type);
    assert(matches.length === 1, 'exact three unique journal boundaries');
    return matches[0];
  });
  const [s, a, t] = j;
  assert(r.indexOf(s) === expected.startedIndex && r.indexOf(a) === 5 && r.indexOf(t) === 19, 'fixed journal positions');
  assert(equal(tuple(s), tuple(a)) && equal(tuple(a), tuple(t)), 'complete journal tuple association');
  assert(s.runId === s.operationId && s.runId === s.turnId, 'run/operation/turn relationship');
  assert(a.streamId === t.streamId, 'accepted/terminal stream association');
  const atoms = [s.binding.moduleInstanceId, s.binding.connectionGeneration, a.streamId];
  assert(atoms.every(v => typeof v === 'string' && v.length > 0) && new Set(atoms).size === 3, 'three distinct typed atoms');
  assert(equal(tuple(s), expected.tuple) && a.streamId === expected.streamId, 'fixed full operation identity');
  assert(t.outcome === 'completed' && t.lastAppliedSequence === 10, 'terminal outcome/durable sequence');
  assert(equal(t.seedState, { allowedReadFiles: [], readFileState: {}, writeSnapshots: {} }), 'exact seed state');
  const only = type => { const found = r.filter(v => v.type === type); assert(found.length === 1, `single ${type}`); return found[0]; };
  const input = only('accepted_input'), assistant = only('assistant_message'), result = only('turn_result');
  only('model_request'); only('compaction_started');
  assert(only('compaction_completed').status === 'skipped', 'skipped compaction');
  assert(equal(t.result, result.result), 'terminal and actual turn result');
  assert(equal(t.messages, [...input.messages, assistant.message]), 'terminal and actual conversation');
  assert(t.result.sessionId === s.sessionId && t.result.turnId === s.turnId, 'result identity');
  const titles = r.filter(fresh);
  assert(titles.length === 1 && r.indexOf(titles[0]) === expected.freshTitleIndex, 'fixed fresh title position');
  const title = titles[0];
  assert(Math.min(r.indexOf(s), r.indexOf(title)) === 3 && Math.max(r.indexOf(s), r.indexOf(title)) === 4, 'only slots3/4 adjacency');
  assert(title.turnId === s.turnId && title.metadata.titleSourceTurnId === s.turnId && title.metadata.titleSource === 'provider'
    && equal(title.metadata.titleMessageSequences, [2]), 'same-turn provider title provenance');
  assert(r[22].type === 'session_metadata' && r[22].metadata.isSnapshot === true, 'final title snapshot');
  for (const [key, value] of Object.entries(title.metadata)) if (key !== 'updatedAt') {
    assert(equal(r[22].metadata[key], value), `final title snapshot: ${key}`);
  }
  assert(p.asOfSequence === 23 && p.sessionId === s.sessionId, 'projection checkpoint');
  for (const [key, value] of Object.entries(p.anchor)) assert(equal(r[22][key], value), `projection anchor ${key}`);
  const serialized = JSON.stringify(p);
  assert(!serialized.includes(r[3].entryId) && !serialized.includes(r[4].entryId), 'no references to either exchanged entry');
  // All checkpoint copies that identify a durable entry must preserve its fields.
  function checkCopies(value) {
    if (!value || typeof value !== 'object') return;
    if (typeof value.entryId === 'string') {
      const source = r.find(row => row.entryId === value.entryId);
      assert(source, 'projection entry exists');
      for (const [key, child] of Object.entries(value)) assert(equal(source[key], child), `projection copy ${key}`);
    }
    for (const child of Object.values(value)) checkCopies(child);
  }
  checkCopies(p);
  const finals = raw.steps[0].frames.filter(v => v.final === true);
  assert(finals.length === 1, 'one actual final WS frame');
  assert(finals[0].event.type === 'turn_completed' && finals[0].event.runId === s.runId
    && equal(finals[0].event.usage, t.result.usage) && finals[0].event.finishReason === t.result.stopReason,
    'actual WS completion linked to durable terminal');
  return { records: r.length, tuple: tuple(s), streamId: a.streamId, startedIndex: r.indexOf(s),
    freshTitleIndex: r.indexOf(title), acceptedIndex: 5, terminalIndex: 19, projectionUnchanged: true };
}

function derive(raw, label) {
  guard(raw, label);
  const view = clone(raw), r = rows(view), e = m.raws[label];
  const edits = [];
  for (const index of [e.startedIndex, 5, 19]) {
    for (const [key, token] of [['moduleInstanceId', '<module-instance:1>'], ['connectionGeneration', '<connection-generation:1>']]) {
      edits.push({ path: ['tree', m.treePath, 'jsonl', index, 'binding', key], from: r[index].binding[key], to: token });
      r[index].binding[key] = token;
    }
  }
  for (const index of [5, 19]) {
    edits.push({ path: ['tree', m.treePath, 'jsonl', index, 'streamId'], from: r[index].streamId, to: '<stream:1>' });
    r[index].streamId = '<stream:1>';
  }
  assert(edits.length === 8, 'exact eight typed atom positions');
  const beforeSwap = clone(r);
  if (label === 'rust') {
    // ONLY this fixed order and these slots. Preserve all slot identities/times.
    assert(r[3].type === journalTypes[0] && fresh(r[4]), 'fixed Rust start/title orientation');
    r[3] = Object.fromEntries(Object.keys(beforeSwap[4]).map(k => [k, SLOT.includes(k) ? beforeSwap[3][k] : beforeSwap[4][k]]));
    r[4] = Object.fromEntries(Object.keys(beforeSwap[3]).map(k => [k, SLOT.includes(k) ? beforeSwap[4][k] : beforeSwap[3][k]]));
  }
  assert(equal(projection(raw), projection(view)), 'checkpoint entirely unchanged');
  // Reverse exactly the declared operations; require byte-order-preserving object equality.
  const reverse = clone(view), rr = rows(reverse);
  if (label === 'rust') { rr[3] = clone(beforeSwap[3]); rr[4] = clone(beforeSwap[4]); }
  for (const edit of edits) {
    let target = reverse; for (const key of edit.path.slice(0, -1)) target = target[key];
    target[edit.path.at(-1)] = edit.from;
  }
  assert(JSON.stringify(reverse) === JSON.stringify(raw), 'no other view edits');
  return { view, audit: { typedPositions: edits, payloadSwap: label === 'rust' ? [3, 4] : [],
    retainedSlotKeys: SLOT, checkpointEdits: 0, reversibleWithoutOtherChanges: true } };
}

const actual = {}, validation = {}, derived = {}, audits = {};
const checkRawHash = (bytes, label) => assert(sha(bytes) === m.raws[label].sha256, `fixed raw hash: ${label}`);
for (const label of ['ts-1', 'ts-2', 'rust']) {
  const bytes = fs.readFileSync(resolve(m.raws[label].file)); checkRawHash(bytes, label);
  actual[label] = JSON.parse(bytes); validation[label] = guard(actual[label], label);
  const result = derive(actual[label], label); derived[label] = result.view; audits[label] = result.audit;
}
const args = process.argv.slice(2);
assert(args.length === 2 && args[0] === '--output' && path.isAbsolute(args[1]), 'usage: accepted-stdio-rebind-exact.mjs --output /absolute/new-directory');
const out = path.resolve(args[1]);
assert(!fs.existsSync(out), 'output must not exist; preserve earlier result');
fs.mkdirSync(out); fs.mkdirSync(path.join(out, 'home')); fs.mkdirSync(path.join(out, 'tmp'));
const write = (name, value) => { const file = path.join(out, name); fs.writeFileSync(file, JSON.stringify(value, null, 2) + '\n'); return file; };
const viewFiles = Object.fromEntries(Object.entries(derived).map(([label, v]) => [label, write(`${label}.view.json`, v)]));
function compare(name, first, second, self = false) {
  const argv = [resolve(m.judgeFile), first, second, '--max', '1000', ...(self ? ['--self'] : [])];
  const result = spawnSync(m.offlineNode.path, argv, { timeout: 30000, encoding: 'utf8', maxBuffer: 4 * 1024 * 1024,
    env: { HOME: path.join(out, 'home'), TMPDIR: path.join(out, 'tmp'), PATH: path.dirname(m.offlineNode.path), LANG: 'C.UTF-8' } });
  assert(!result.error && result.signal === null, `pure offline comparator completed: ${name}`);
  fs.writeFileSync(path.join(out, name + '.stdout'), result.stdout);
  fs.writeFileSync(path.join(out, name + '.stderr'), result.stderr);
  return { name, argv: [m.offlineNode.path, ...argv], exit: result.status, lastLine: result.stdout.trim().split('\n').at(-1) };
}
const comparisons = [compare('original-self', resolve(m.raws['ts-1'].file), resolve(m.raws['ts-2'].file), true),
  compare('original-pair', resolve(m.raws['ts-1'].file), resolve(m.raws.rust.file)),
  compare('private-self', viewFiles['ts-1'], viewFiles['ts-2'], true),
  compare('private-pair', viewFiles['ts-1'], viewFiles.rust)];
assert(equal(comparisons.map(v => [v.exit, v.lastLine]), [[1, 'DIFF 8'], [1, 'DIFF 69'], [0, 'SAME'], [0, 'SAME']]), 'complete expected original/private comparisons');

// Internal controls are intentionally limited to clones of these three originals.
// They bypass ONLY the outer immutable-byte check to exercise rejection causes;
// there is no command-line mode or API that admits a changed artifact.
const controls = [];
function reject(name, action) {
  let error; try { action(); } catch (e) { error = e.message; }
  assert(error, `negative control unexpectedly accepted: ${name}`);
  controls.push({ name, rejected: true, reason: error });
}
reject('changed-manifest-byte', () => checkManifest(Buffer.concat([manifestBytes, Buffer.from(' ')])));
reject('changed-raw-byte', () => checkRawHash(Buffer.from(JSON.stringify(actual.rust)), 'rust'));
function negative(name, mutate) { const raw = clone(actual.rust); mutate(raw, rows(raw), projection(raw)); reject(name, () => guard(raw, 'rust')); }
negative('wrong-source-identity', raw => { raw.evidence.inputs.rust.commit = '0'.repeat(40); });
negative('wrong-binary-identity', raw => { raw.evidence.inputs.binaries.pilotdeck = '0'.repeat(64); });
negative('wrong-harness-identity', raw => { raw.evidence.inputs.harness['diff.mjs'] = '0'.repeat(64); });
negative('wrong-scenario-identity', raw => { raw.evidence.scenario.sha256 = '0'.repeat(64); });
negative('missing-terminal', (_, r) => { r.splice(19, 1); });
negative('extra-journal', (_, r) => { r.push(clone(r[19])); });
negative('mismatched-journal-tuple', (_, r) => { r[19].requestId += '-wrong'; });
negative('mismatched-stream', (_, r) => { r[19].streamId += '-wrong'; });
negative('collapsed-typed-atoms', (_, r) => { for (const i of [3, 5, 19]) r[i].binding.connectionGeneration = r[i].binding.moduleInstanceId; });
negative('broken-parent-chain', (_, r) => { r[10].parentEntryId = 'wrong'; });
negative('broken-projection-anchor', (_, r, p) => { p.anchor.entryId = r[21].entryId; });
negative('unexpected-projection-reference-to-slot3', (_, r, p) => { p.extra = r[3].entryId; });
negative('terminal-sequence-9', (_, r) => { r[19].lastAppliedSequence = 9; });
negative('terminal-outcome-failed', (_, r) => { r[19].outcome = 'failed'; });
negative('seed-state-change', (_, r) => { r[19].seedState.allowedReadFiles = ['/unexpected']; });
negative('terminal-result-change', (_, r) => { r[19].result.turns = 2; });
negative('terminal-messages-change', (_, r) => { r[19].messages = []; });
negative('title-wrong-turn', (_, r) => { r[4].metadata.titleSourceTurnId = 'wrong'; });
negative('title-content-change-without-final-snapshot', (_, r) => { r[4].metadata.aiTitle = 'wrong'; });
negative('title-crosses-accepted', (_, r) => { [r[4], r[5]] = [r[5], r[4]]; });
negative('title-not-adjacent-even-with-preserved-slots', (_, r) => {
  const a = clone(r[4]), b = clone(r[6]);
  r[4] = Object.fromEntries(Object.keys(b).map(k => [k, SLOT.includes(k) ? a[k] : b[k]]));
  r[6] = Object.fromEntries(Object.keys(a).map(k => [k, SLOT.includes(k) ? b[k] : a[k]]));
});
negative('extra-provider-dispatch', raw => { raw.provider.main.push(clone(raw.provider.main[0])); });
const providerChanged = clone(derived.rust);
providerChanged.provider.main[0].body.model = 'deliberately-wrong-model';
const providerControl = compare('negative-provider-business-payload', viewFiles['ts-1'], write('negative-provider.view.json', providerChanged));
assert(providerControl.exit === 1 && providerControl.lastLine === 'DIFF 1', 'provider content still fully compared');
const titleChanged = clone(derived.rust), titleRows = rows(titleChanged);
for (const row of titleRows) if (row.type === 'session_metadata' && row.metadata?.aiTitle) row.metadata.aiTitle = 'Deliberately wrong title';
projection(titleChanged).projections['agent-transcript.metadata'].state.aiTitle = 'Deliberately wrong title';
const titleControl = compare('negative-title-business-content', viewFiles['ts-1'], write('negative-title.view.json', titleChanged));
assert(titleControl.exit === 1 && titleControl.lastLine === 'DIFF 3', 'consistent title content still fully compared at all three copies');
const report = { status: 'ACCEPTED_EXACT_THREE_RAW_ONLY_STRICT_VERDICTS_PRESERVED', approval: m.approval,
  manifestSHA256: MANIFEST_SHA, candidateSHA256: sha(fs.readFileSync(fileURLToPath(import.meta.url))),
  originalVerdictsPreserved: { historical: 'self8/pair157', thisCapture: 'self8/pair69 BASELINE_UNSTABLE' },
  validation, audits, comparisons, negativeControls: controls, contentNegativeControls: [providerControl, titleControl],
  noPublicChange: true, noGlobalFloatOverChange: true, productExecutions: 0, newCaptures: 0,
  limitations: m.limits, outputs: Object.fromEntries(fs.readdirSync(out).filter(f => fs.statSync(path.join(out, f)).isFile()).map(f => [f, sha(fs.readFileSync(path.join(out, f)))])) };
write('ready.json', report);
for (const [label, spec] of Object.entries(m.raws)) checkRawHash(fs.readFileSync(resolve(spec.file)), label);
console.log(JSON.stringify({ output: path.join(out, 'ready.json'), comparisons: comparisons.map(v => v.lastLine), controlsRejected: controls.length, contentControls: [providerControl.lastLine, titleControl.lastLine] }));
