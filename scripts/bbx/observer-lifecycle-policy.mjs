import { isDeepStrictEqual } from 'node:util';
// User-approved narrow observer lifecycle windows; raw captures remain immutable.
// It never mutates captures, sorts/drops arrays, or changes the official judge.
import assert from 'node:assert/strict';
const SPECS = {
 '9f8c13a2e92a85869dfb5ee4ed602560fe1002b78888a111068c6a511c4563ca': 'agent03-sdk-fork',
 'd654c045d164826de415eb7581325471b5b0b32b57d664a8069a1feb504e101a': 'agent03-sdk-fork-lifecycle-probe',
};
const one = (items, message) => { assert.equal(items.length, 1, message); return items[0]; };
const text = value => typeof value === 'string' ? value : (value ?? []).filter(b => b.type === 'text').map(b => b.text).join('');
const date = value => { const result = Date.parse(value); assert.ok(Number.isFinite(result), 'valid timestamp'); return result; };
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const kinds = ['subagent_text_delta', 'subagent_assistant_block', 'subagent_completed'];
function indexRows(rows, session, turn) {
 const entries = new Map();
 rows.forEach((row, i) => {
  assert.equal(row.sequence, i + 1, 'durable continuous sequence');
  assert.ok(UUID.test(row.entryId), 'durable entry ID');
  assert.ok(!entries.has(row.entryId), 'duplicate durable entry ID');
  if (i) assert.equal(row.parentEntryId, rows[i - 1].entryId, 'durable parent chain'); else assert.ok(row.parentEntryId === null || row.parentEntryId === undefined, 'durable root has no predecessor');
  assert.equal(row.sessionId, session, 'durable session');
  if (turn) assert.equal(row.turnId, turn, 'durable turn');
  entries.set(row.entryId, row);
 });
 return entries;
}
function projection(value, entries) {
 assert.ok(value?.anchor, 'projection missing');
 assert.equal(value.asOfSequence, entries.size, 'projection latest checkpoint');
 function walk(v) {
  if (Array.isArray(v)) return v.forEach(walk);
  if (!v || typeof v !== 'object') return;
  if (v.entryId !== undefined && v.sequence !== undefined) {
   const original = entries.get(v.entryId); assert.ok(original, 'projection unknown entry');
   for (const key of ['sequence', 'parentEntryId', 'turnId', 'sessionId', 'type', 'createdAt']) if (Object.hasOwn(v, key)) assert.deepEqual(v[key], original[key], 'projection identity ' + key);
  }
  for (const child of Object.values(v)) walk(child);
 }
 walk(value);
 const last = [...entries.values()].at(-1);
 assert.equal(value.anchor.entryId, last.entryId, 'projection last anchor');
}
export function lifecycleContract(raw) {
 const sceneHash = raw.evidence?.scenario?.sha256;
 assert.equal(raw.scenario, SPECS[sceneHash], 'fixed scene identity');
 assert.ok(SPECS[sceneHash], 'unsupported scene');
 assert.deepEqual(raw.failures, []); assert.deepEqual(raw.shutdown, { code: 0, signal: null });
 assert.equal(raw.evidence?.side, raw.side);
 assert.equal(raw.evidence?.inputs?.ts?.commit, 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85');
 const [parentPath, parentFile] = one(Object.entries(raw.tree).filter(([p, f]) => p.endsWith('/chats/agent03-parent.jsonl') && f.jsonl), 'unique parent transcript');
 const rows = parentFile.jsonl, parentSession = 'agent03-parent';
 const entries = indexRows(rows, parentSession);
 projection(raw.tree[parentPath + '.projections.json']?.json, entries);
 const inputs = rows.filter(r => r.type === 'accepted_input');
 assert.equal(inputs.length, 2, 'two parent turns');
 assert.equal(text(inputs[0].messages[0].content), 'AG03-PARENT-BEGIN');
 assert.equal(text(inputs[1].messages[0].content), 'AG03-PARENT-CHECK');
 const runs = inputs.map(r => r.turnId); assert.notEqual(runs[0], runs[1]);
 const started = rows.filter(r => r.type === 'subagent_started');
 assert.equal(started.length, 2, 'exactly worker and reviewer');
 const worker = one(started.filter(r => r.subagentType === 'worker'), 'worker reference');
 const reviewer = one(started.filter(r => r.subagentType === 'reviewer'), 'reviewer reference');
 assert.notEqual(worker.subagentId, reviewer.subagentId);
 for (const child of [worker, reviewer]) { assert.ok(UUID.test(child.subagentId)); assert.equal(child.turnId, runs[0]); }
 const completed = rows.filter(r => r.type === 'subagent_completed');
 assert.equal(completed.length, 2, 'exactly two durable completions');
 const endFor = child => one(completed.filter(r => r.subagentId === child.subagentId && r.subagentType === child.subagentType), 'matching durable completion');
 const workerEnd = endFor(worker), reviewerEnd = endFor(reviewer);
 const referenceState = raw.tree[parentPath + '.projections.json'].json.projections?.['agent-transcript.subagent-references']?.state;
 assert.ok(referenceState, 'parent subagent projection present');
 for (const [name, source] of [['started', started], ['completed', completed]]) {
  assert.equal(referenceState[name]?.length, 2, 'complete subagent reference projection');
  for (const row of source) assert.deepEqual(one(referenceState[name].filter(r => r.entryId === row.entryId), 'exact projected subagent identity'), row, 'full subagent reference payload');
 }

 assert.ok(workerEnd.sequence < reviewer.sequence, 'worker completed before reviewer start');
 assert.equal(reviewerEnd.turnId, runs[0], 'durable source parent turn retained');
 assert.equal(reviewerEnd.errored, false); assert.equal(reviewerEnd.summaryPreview, 'AG03-OBSERVER-REPORT');
 assert.deepEqual(reviewerEnd.usage, { inputTokens: 12, outputTokens: 4, totalTokens: 16 });
 const childFiles = new Map();
 for (const child of [worker, reviewer]) {
  const [file, value] = one(Object.entries(raw.tree).filter(([p, v]) => p.endsWith('/' + child.transcriptRelativePath) && v.jsonl), 'actual child transcript');
  assert.ok(child.subagentSessionId.endsWith('::sub::' + child.subagentId), 'child session identity');
  const childEntries = indexRows(value.jsonl, child.subagentSessionId, child.subagentId + '-t0');
  projection(raw.tree[file + '.projections.json']?.json, childEntries);
  const terminal = one(value.jsonl.filter(r => r.type === 'turn_result'), 'one child terminal');
  assert.equal(terminal.result.type, 'success');
  const end = endFor(child);
  assert.ok(date(terminal.createdAt) <= date(end.createdAt), 'child terminal before durable completion');
  childFiles.set(child.subagentType, { file, rows: value.jsonl, entries: childEntries, terminal });
 }
 const child = childFiles.get('reviewer');
 const accepted = one(child.rows.filter(r => r.type === 'accepted_input'), 'reviewer accepted input');
 const prompt = text(accepted.messages[0].content);
 assert.equal(prompt.split('- assistant response emitted').length - 1, 4, 'four complete activity lines');
 assert.ok(prompt.includes('Outcome: completed\nObserved final report:\nScope: done.\nResult: AG03-CHILD-REPORT.'));
 assert.ok(prompt.endsWith('AG03-OBSERVER-POSTAMBLE'));
 const request = one(raw.provider.observer ?? [], 'actual observer provider request');
 assert.deepEqual(request.body.metadata, { subagentId: reviewer.subagentId, subagentType: 'reviewer' }, 'provider producer metadata');
 assert.equal(text(one(request.body.messages.filter(m => m.role === 'user' && text(m.content).startsWith('You are an observer.')), 'observer user').content), prompt);
 const storedRequest = one(child.rows.filter(r => r.type === 'model_request'), 'reviewer durable model request');
 assert.equal(text(storedRequest.request.messages[0].content), prompt);
 const final = one(child.rows.filter(r => r.type === 'assistant_message'), 'reviewer final message');
 assert.equal(text(final.message.content), 'AG03-OBSERVER-REPORT');
 const conversation = raw.tree[child.file + '.projections.json'].json.projections?.['agent-transcript.conversation']?.state?.entries;
 assert.ok(Array.isArray(conversation), 'child conversation projection present');
 for (const row of [accepted, final]) assert.deepEqual(one(conversation.filter(v => v.entry?.entryId === row.entryId), 'child durable projection witness').entry, row, 'child projected body');

 const result = one(rows.filter(r => r.type === 'tool_result' && r.result?.toolCallId === 'ag03-agent'), 'parent agent result');
 assert.equal(result.result.type, 'success');
 const stepEnd = one(rows.filter(r => r.type === 'step_completed' && r.turnId === runs[0] && r.step === result.step), 'agent step completion');
 const ri = rows.indexOf(reviewer), ti = rows.indexOf(result), si = rows.indexOf(stepEnd);
 // Only these two observed branch forms are proposed, not arbitrary crossings.
 const early = ri + 1 === ti && ti + 1 === si;
 const late = ti + 1 === si && si + 1 === ri;
 assert.ok(early || late, 'reviewer start crossed an unapproved event');
 assert.equal(result.turnId, runs[0]); assert.ok(workerEnd.sequence < result.sequence);
 const registryPath = one(Object.keys(raw.tree).filter(p => p.endsWith('/runs/registry.json')), 'registry');
 const registry = raw.tree[registryPath].json;
 const witnesses = [], reviewerEvents = [];
 for (const step of [0, 3]) {
  const frames = raw.steps[step].frames;
  assert.ok(frames.every(f => f.type === 'event' && f.id === (step === 0 ? 'q1' : 'q4')), 'actual request identity');
  frames.forEach((f, i) => assert.equal(f.seq, i, 'wire continuous seq'));
  const currentRun = runs[step === 0 ? 0 : 1];
  for (const f of frames) assert.equal(f.event.runId, currentRun, 'event current run');
  const expectedText = step === 0 ? 'AG03-PARENT-DONE' : 'AG03-PARENT-CHECK-DONE';
  const parentFinal = one(rows.filter(row => row.type === 'assistant_message' && row.turnId === currentRun && text(row.message.content)), 'parent final durable text');
  assert.equal(text(parentFinal.message.content), expectedText);
  for (const frame of frames) if (['assistant_text_delta', 'assistant_block'].includes(frame.event.type)) assert.equal(frame.event.text, expectedText, 'non-target parent final text');
  const events = frames.filter(f => !f.final);
  assert.equal(frames.filter(f => f.final).length, 1);
  const regKey = '\0' + parentSession + '\0' + currentRun;
  const reg = registry.events[regKey]; assert.ok(reg, 'run registry exists');
  assert.equal(reg.length, events.length, 'registry complete stream');
  reg.forEach((r, i) => { assert.equal(r.seq, i + 1, 'registry continuous seq'); assert.deepEqual(r.event, events[i].event, 'wire registry projection'); });
  for (const [i, frame] of frames.entries()) {
   if (!kinds.includes(frame.event.event)) continue;
   const detail = frame.event.detail;
   if (detail.subagentId !== reviewer.subagentId) { assert.equal(detail.subagentId, worker.subagentId, 'wrong child'); continue; }
   reviewerEvents.push({ step, index: i, frame, registryKey: regKey });
  }
  witnesses.push({ step, currentRun, requestId: step === 0 ? 'q1' : 'q4' });
 }
 assert.equal(reviewerEvents.length, 3, 'exactly three reviewer events');
 assert.deepEqual(reviewerEvents.map(e => e.frame.event.event), kinds, 'reviewer event order');
 assert.equal(new Set(reviewerEvents.map(e => e.step)).size, 1, 'one drain window');
 const window = reviewerEvents[0].step;
 for (const e of reviewerEvents) {
  const value = e.frame.event;
  if (value.event !== 'subagent_completed') {
   assert.equal(value.detail.text, 'AG03-OBSERVER-REPORT');
   assert.equal(value.timeline.turnId, reviewer.subagentId + '-t0', 'same producer child turn');
  } else { assert.equal(value.detail.subagentType, 'reviewer'); assert.equal(value.detail.success, true); }
 }
 const firstTerminal = one(rows.filter(r => r.type === 'turn_result' && r.turnId === runs[0]), 'parent first terminal');
 assert.ok(reviewerEnd.sequence > reviewer.sequence, 'reviewer completion after start');
 if (window === 0) assert.ok(reviewerEnd.sequence < firstTerminal.sequence, 'current-window completion before terminal');
 else {
  assert.ok(reviewerEnd.sequence > firstTerminal.sequence, 'next-window producer completed after first terminal');
  const nextStart = one(rows.filter(r => r.type === 'turn_started' && r.turnId === runs[1]), 'next parent run');
  assert.ok(reviewerEnd.sequence < nextStart.sequence, 'next-window durable producer completed before next run');
 }
 const history = raw.steps[6].frames[0].result.stream;
 assert.equal(history.sessionKey, parentSession); assert.equal(history.runId, runs[1]);
 assert.equal(history.active, false);
 assert.deepEqual(history.events, raw.steps[3].frames.filter(f => !f.final).map(f => f.event), 'actual history replays the same run events');
 const anchors = {
  parentRun0: runs[0], parentRun1: runs[1], secondSessionRun: raw.steps[4].frames[0].event.runId, worker: worker.subagentId, reviewer: reviewer.subagentId,
 };
 assert.ok(UUID.test(anchors.secondSessionRun));
 const secondRows = one(Object.entries(raw.tree).filter(([p, value]) => p.endsWith('/chats/agent03-second.jsonl') && value.jsonl), 'second session transcript')[1].jsonl;
 assert.equal(one(secondRows.filter(row => row.type === 'accepted_input'), 'second session input').turnId, anchors.secondSessionRun);
 // Stable producer block anchor; do not infer it from first-seen UUID order.
 const block = reviewerEvents.find(e => e.frame.event.event === 'subagent_assistant_block').frame.event;
 const persistedBlock = one(final.message.content.filter(b => b.type === 'text'), 'one observer text block');
 assert.equal(block.detail.blockId, persistedBlock.blockId); assert.equal(block.timeline.id, persistedBlock.timeline.id);
 const delta = reviewerEvents.find(e => e.frame.event.event === 'subagent_text_delta').frame.event;
 assert.equal(delta.timeline.id, block.timeline.id, 'delta and block share exact producer block');
 anchors.reviewerBlock = block.detail.blockId.split(':')[0]; assert.ok(UUID.test(anchors.reviewerBlock));
 return { sceneHash, parentPath, registryPath, anchors, digest: prompt, startWindow: early ? 'before-tool-result' : 'after-step-completed', completionWindow: window,
  reviewerEvents: reviewerEvents.map(e => ({ wire: `$.steps[${e.step}].frames[${e.index}]`, registry: `$.tree.${registryPath}.json.events.${e.registryKey}[${e.index}]`, history: e.step === 3 ? `$.steps[6].frames[0].result.stream.events[${e.index}]` : null, original: e.frame })),
  durableStart: `$.tree.${parentPath}.jsonl[${ri}]`, durableCompleted: `$.tree.${parentPath}.jsonl[${rows.indexOf(reviewerEnd)}]`, witnesses,
  limitation: 'Private causal witness only. No diff suppression; any unanchored UUID or unrelated ordered/value difference remains ordinary DIFF.' };
}
export function lifecyclePair(a, b) {
 const left = lifecycleContract(a), right = lifecycleContract(b);
 assert.equal(left.sceneHash, right.sceneHash, 'same exact scene');
 assert.equal(left.digest, right.digest, 'complete observer digest exact');
 const stable = (body, anchors) => JSON.stringify(body, (_key, value) => typeof value === 'string' ? value.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g, id => { const entry = Object.entries(anchors).find(([, raw]) => raw === id); return entry ? `<entity:${entry[0]}>` : id; }) : value);
 assert.equal(stable(a.provider.observer[0].body, left.anchors), stable(b.provider.observer[0].body, right.anchors), 'entire observer request unchanged except anchored entity ID');
 const targetPayload = (event, proof) => { const copy = structuredClone(event); copy.runId = '<validated-drain-run>'; if (copy.event === 'subagent_completed') { assert.ok(Number.isFinite(copy.detail.durationMs) && copy.detail.durationMs >= 0); copy.detail.durationMs = '<existing-duration>'; } return stable(copy, proof.anchors); };
 left.reviewerEvents.forEach((event, i) => assert.equal(targetPayload(event.original.event, left), targetPayload(right.reviewerEvents[i].original.event, right), 'complete target payload except approved identity/window/duration'));

 // All non-target frames keep their real array order; two cursors only pair
 // corresponding events after separately accounting for the exact three.
 const nonTargetComparisons = [];
 for (const step of [0, 3]) {
  const streams = [[a, left], [b, right]].map(([raw, proof]) => raw.steps[step].frames.map((frame, index) => ({ frame, index })).filter(({ frame }) => !(kinds.includes(frame.event.event) && frame.event.detail?.subagentId === proof.anchors.reviewer)));
  assert.equal(streams[0].length, streams[1].length, 'non-target stream count');
  const key = (frame, proof) => JSON.stringify([frame.final, frame.event.type, frame.event.event, frame.event.toolCallId, frame.event.detail?.subagentType, frame.event.detail?.subagentId === proof.anchors.worker ? 'worker' : frame.event.detail?.subagentId === proof.anchors.reviewer ? 'reviewer' : frame.event.detail?.subagentId]);
  streams[0].forEach((row, i) => {
   const other = streams[1][i]; assert.equal(key(row.frame, left), key(other.frame, right), 'non-target array order changed');
   nonTargetComparisons.push({ left: `$.steps[${step}].frames[${row.index}]`, right: `$.steps[${step}].frames[${other.index}]`, equalAfterKnownEntityOnly: stable(row.frame.event, left.anchors) === stable(other.frame.event, right.anchors) });
  });
 }
 const nonTargetDurableComparisons = [];
 // Honor only the pre-existing BBX title FLOAT_OVER rule for this order
 // guard; retain each row's actual path and do not mutate either raw array.
 const existingTitleOrder = rows => { const refs = rows.map((row, index) => ({ row, index })); const floating = ['step_started', 'context_snapshot', 'agent_instructions', 'compaction_started', 'compaction_completed', 'agent_status_message']; for (let i = 0; i < refs.length; i++) { if (refs[i].row.type !== 'session_metadata' || !refs[i].row.metadata?.titleSource) continue; let j = i; while (j > 0 && floating.includes(refs[j - 1].row.type) && refs[j - 1].row.turnId === refs[i].row.turnId) j--; if (i !== j) { const [ref] = refs.splice(i, 1); refs.splice(j, 0, ref); } } return refs; };
 const durableStreams = [[a, left], [b, right]].map(([raw, proof]) => existingTitleOrder(raw.tree[proof.parentPath].jsonl).filter(({ row }) => !(['subagent_started', 'subagent_completed'].includes(row.type) && row.subagentId === proof.anchors.reviewer)));
 assert.equal(durableStreams[0].length, durableStreams[1].length, 'non-target durable count');
 const durableKey = (row, proof) => JSON.stringify([row.type, row.turnId === proof.anchors.parentRun0 ? 'W0' : row.turnId === proof.anchors.parentRun1 ? 'W1' : row.turnId, row.step, row.event?.type ?? row.event, row.toolCallId ?? row.result?.toolCallId, row.subagentId === proof.anchors.worker ? 'worker' : row.subagentId]);
 durableStreams[0].forEach((row, i) => { const other = durableStreams[1][i]; assert.equal(durableKey(row.row, left), durableKey(other.row, right), 'non-target durable array order changed'); nonTargetDurableComparisons.push({ left: `$.tree.${left.parentPath}.jsonl[${row.index}]`, right: `$.tree.${right.parentPath}.jsonl[${other.index}]`, equalAfterKnownEntityOnly: stable(row.row, left.anchors) === stable(other.row, right.anchors) }); });
 // Body/arguments are exact business guards. The sole duration substitution
 // is an identity guard for the independently proposed byte-value group,
 // never permission for this lifecycle proposal to suppress those fields.
 const bodyGuard = (body, proof) => { const copy = structuredClone(body); for (const message of copy.messages ?? []) if (message.role === 'tool' && message.tool_call_id === 'ag03-agent' && typeof message.content === 'string') { assert.equal([...message.content.matchAll(/("durationMs":)\d+/g)].length, 1, 'single known agent report duration'); message.content = message.content.replace(/("durationMs":)\d+/g, '$1<runtime-duration>'); } return stable(copy, proof.anchors); };
 assert.equal(Object.keys(a.provider).length, Object.keys(b.provider).length, 'provider tag count'); assert.ok(Object.keys(a.provider).every(tag => Object.hasOwn(b.provider, tag)), 'same provider tags; actual key order remains DIFF');
 for (const tag of Object.keys(a.provider)) { assert.equal(a.provider[tag].length, b.provider[tag].length, 'provider request count'); for (let i = 0; i < a.provider[tag].length; i++) assert.equal(bodyGuard(a.provider[tag][i].body, left), bodyGuard(b.provider[tag][i].body, right), `business request/arguments ${tag}[${i}]`); }
 const names = Object.keys(left.anchors);
 assert.deepEqual(names, Object.keys(right.anchors));
 const lset = new Set(), rset = new Set(), ids = [];
 for (const name of names) { const l = left.anchors[name], r = right.anchors[name]; assert.ok(!lset.has(l) && !rset.has(r), 'entity IDs cannot merge'); lset.add(l); rset.add(r); ids.push({ entity: name, left: l, right: r }); }
 return { proposalOnly: true, approved: false, formalSame: false, left, right, stableEntityPairs: ids, nonTargetComparisons, nonTargetDurableComparisons, comparisons: {
  started: { left: left.durableStart, right: right.durableStart, windows: [left.startWindow, right.startWindow] },
  completed: left.reviewerEvents.map((e, i) => ({ left: e, right: right.reviewerEvents[i] })),
 }, unchanged: 'All original arrays/paths preserved. Non-target events remain in actual order. This report is not an allowed-difference Map.' };
}
const aliasShape = value => typeof value === 'string' ? value.replace(/<uuid:\d+>/g, '<uuid>') : Array.isArray(value) ? value.map(aliasShape) : value && typeof value === 'object' ? Object.fromEntries(Object.entries(value).map(([k, v]) => [k, aliasShape(v)])) : value;
function missingLeaves(value, at, left, out) {
 const put = v => out.push({ at, a: left ? v : undefined, b: left ? undefined : v });
 if (value && typeof value === 'object') {
  const type = Array.isArray(value) ? 'array' : 'object';
  out.push({ at: at + '.<type>', a: left ? type : undefined, b: left ? undefined : type });
  if (Array.isArray(value)) {
   out.push({ at: at + '.length', a: left ? value.length : undefined, b: left ? undefined : value.length });
   value.forEach((v, i) => missingLeaves(v, `${at}[${i}]`, left, out));
  } else {
   out.push({ at: at + '.<key order>', a: left ? Object.keys(value) : undefined, b: left ? undefined : Object.keys(value) });
   for (const [k, v] of Object.entries(value)) missingLeaves(v, at + '.' + k, left, out);
  }
 } else put(value);
}
/** Individually proven original diff paths only; no record/array exemption.
 * Extra array records are expanded to typed leaves for this named contract,
 * preserving the original record difference separately in wholeRecordDifferences.
 */
export function observerLifecyclePaths(rawA, rawB, a, b, aliasesA, aliasesB, differences) {
 if (!Object.values(SPECS).includes(rawA.scenario) && !Object.values(SPECS).includes(rawB.scenario) && !SPECS[rawA.evidence?.scenario?.sha256] && !SPECS[rawB.evidence?.scenario?.sha256]) return new Map();
 const proof = lifecyclePair(rawA, rawB);
 assert.ok(aliasesA instanceof Map && aliasesB instanceof Map, 'normalizer UUID maps required');
 const maps = [aliasesA, aliasesB], proofs = [proof.left, proof.right], values = [a, b], raws = [rawA, rawB];
 const token = (id, side) => { assert.ok(maps[side].has(id), 'known UUID alias'); return `<uuid:${maps[side].get(id)}>`; };
 const key = (v, side) => v.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g, id => token(id, side));
 const strong = proofs.map((p, side) => new Map(Object.entries(p.anchors).map(([name, id]) => [token(id, side), name])));
 const canon = (v, side) => typeof v === 'string' ? v.replace(/<uuid:\d+>/g, id => strong[side].has(id) ? `<entity:${strong[side].get(id)}>` : id) : v;
 const sideLeaves = [new Set(), new Set()], nodeRoots = [new Set(), new Set()], proposed = new Map();
 const paired = [];
 function pair(lv, rv, lp, rp, options = {}) {
  nodeRoots[0].add(lp); nodeRoots[1].add(rp); paired.push({ left: lp, right: rp });
  function walk(x, y, l, r, relative) {
   const tx = Array.isArray(x) ? 'array' : x === null ? 'null' : typeof x;
   const ty = Array.isArray(y) ? 'array' : y === null ? 'null' : typeof y;
   if (tx !== ty) return;
   const mark = suffix => { sideLeaves[0].add(l + suffix); sideLeaves[1].add(r + suffix); };
   if (tx === 'object' || tx === 'array') {
    mark('.<type>');
    if (tx === 'array') {
     if (x.length === y.length) mark('.length');
     for (let i = 0; i < Math.min(x.length, y.length); i++) walk(x[i], y[i], `${l}[${i}]`, `${r}[${i}]`, relative + `[${i}]`);
    } else {
     if (isDeepStrictEqual(Object.keys(x), Object.keys(y))) mark('.<key order>');
     for (const k of Object.keys(x)) if (Object.hasOwn(y, k)) walk(x[k], y[k], l + '.' + k, r + '.' + k, relative ? relative + '.' + k : k);
    }
   } else if (isDeepStrictEqual(x, y) || options.fields?.has(relative) || (options.target && canon(x, 0) === canon(y, 1))) mark('');
  }
  walk(lv, rv, lp, rp, '');
 }
 // Actual frame identity pairs: all non-target items keep their relative order.
 const links = [];
 for (const row of proof.nonTargetComparisons) {
  const parse = p => [...p.matchAll(/\[(\d+)\]/g)].map(m => Number(m[1]));
  const [ls, li] = parse(row.left), [rs, ri] = parse(row.right); links.push({ ls, li, rs, ri, target: false });
 }
 for (const row of proof.comparisons.completed) {
  const parse = p => [...p.matchAll(/\[(\d+)\]/g)].map(m => Number(m[1]));
  const [ls, li] = parse(row.left.wire), [rs, ri] = parse(row.right.wire); links.push({ ls, li, rs, ri, target: true });
 }
 const regPath = proofs.map(p => p.registryPath);
 const regKey = (side, step) => key('\0agent03-parent\0' + proofs[side].anchors[step === 0 ? 'parentRun0' : 'parentRun1'], side);
 for (const link of links) {
  const { ls, li, rs, ri, target } = link;
  const lf = a.steps[ls].frames[li], rf = b.steps[rs].frames[ri];
  pair(lf, rf, `$.steps[${ls}].frames[${li}]`, `$.steps[${rs}].frames[${ri}]`, { target, fields: new Set(target ? ['id', 'seq', 'event.runId'] : ['seq']) });
  if (!lf.final && !rf.final) {
   const lr = a.tree[regPath[0]].json.events[regKey(0, ls)][li], rr = b.tree[regPath[1]].json.events[regKey(1, rs)][ri];
   pair(lr, rr, `$.tree.${regPath[0]}.json.events.${regKey(0, ls)}[${li}]`, `$.tree.${regPath[1]}.json.events.${regKey(1, rs)}[${ri}]`, { target, fields: new Set(target ? ['seq', 'event.runId'] : ['seq']) });
   if (ls === 3 || rs === 3) {
    const lh = ls === 3 ? a.steps[6].frames[0].result.stream.events[li] : lf.event;
    const rh = rs === 3 ? b.steps[6].frames[0].result.stream.events[ri] : rf.event;
    const lp = ls === 3 ? `$.steps[6].frames[0].result.stream.events[${li}]` : `$.steps[${ls}].frames[${li}].event`;
    const rp = rs === 3 ? `$.steps[6].frames[0].result.stream.events[${ri}]` : `$.steps[${rs}].frames[${ri}].event`;
    pair(lh, rh, lp, rp, { target, fields: new Set(target ? ['runId'] : []) });
   }
  }
 }
 for (const step of [0, 3]) {
  for (const p of [`$.steps[${step}].frames.length`, `$.tree.${regPath[0]}.json.events.${regKey(0, step)}.length`, `$.tree.${regPath[1]}.json.events.${regKey(1, step)}.length`]) proposed.set(p, 'exact three reviewer events move between the two proven drain windows; original frames/registry remain intact');
 }
 proposed.set('$.steps[6].frames[0].result.stream.events.length', 'history exactly replays validated W1 registry/wire stream, including the proven reviewer window');
 const parentRows = values.map((v, side) => v.tree[proofs[side].parentPath].jsonl);
 const rowPairs = [];
 const parseRow = p => Number(/\.jsonl\[(\d+)\]$/.exec(p)[1]);
 for (const row of proof.nonTargetDurableComparisons) rowPairs.push([parseRow(row.left), parseRow(row.right), false]);
 rowPairs.push([parseRow(proof.left.durableStart), parseRow(proof.right.durableStart), true]);
 rowPairs.push([parseRow(proof.left.durableCompleted), parseRow(proof.right.durableCompleted), true]);
 // Use the already approved title normalization's slots; lifecycle references
 // are beyond the title FLOAT_OVER window in these fixed inputs.
 const normalizedIndex = (side, rawIndex) => {
  const original = raws[side].tree[proofs[side].parentPath].jsonl[rawIndex];
  const id = token(original.entryId, side);
  return one(parentRows[side].map((r, i) => ({ r, i })).filter(({ r }) => r.entryId === id), 'normalized durable slot').i;
 };
 const rowIdPairs = new Map();
 for (const [l, r, target] of rowPairs) {
  const li = normalizedIndex(0, l), ri = normalizedIndex(1, r);
  const lv = parentRows[0][li], rv = parentRows[1][ri];
  // Existing title FLOAT_OVER keeps slot IDs; only pair a non-title payload
  // here if its semantic kind still matches. Unproved slots remain DIFF.
  if (lv.type !== rv.type) continue;
  rowIdPairs.set(lv.entryId, rv.entryId);
  pair(lv, rv, `$.tree.${proof.left.parentPath}.jsonl[${li}]`, `$.tree.${proof.right.parentPath}.jsonl[${ri}]`, { target, fields: new Set(['sequence']) });
 }
 const project = values.map((v, side) => v.tree[proofs[side].parentPath + '.projections.json'].json);
 function projectionPairs(x, y, lp, rp) {
  if (!x || !y || typeof x !== 'object' || typeof y !== 'object') return;
  if (x.entryId && y.entryId && rowIdPairs.get(x.entryId) === y.entryId && Number.isInteger(x.sequence) && Number.isInteger(y.sequence)) {
   sideLeaves[0].add(lp + '.sequence'); sideLeaves[1].add(rp + '.sequence');
  }
  if (x.entry?.entryId && y.entry?.entryId && rowIdPairs.get(x.entry.entryId) === y.entry.entryId && x.index === x.entry.sequence - 1 && y.index === y.entry.sequence - 1) {
   sideLeaves[0].add(lp + '.index'); sideLeaves[1].add(rp + '.index');
  }
  if (Array.isArray(x) && Array.isArray(y)) { if (x.length !== y.length) return; x.forEach((v, i) => projectionPairs(v, y[i], `${lp}[${i}]`, `${rp}[${i}]`)); }
  else for (const k of Object.keys(x)) if (Object.hasOwn(y, k)) projectionPairs(x[k], y[k], lp + '.' + k, rp + '.' + k);
 }
 projectionPairs(project[0], project[1], `$.tree.${proof.left.parentPath}.projections.json.json`, `$.tree.${proof.right.parentPath}.projections.json.json`);
 const expanded = [], originals = [];
 for (const d of differences) {
  if ((d.a === undefined || d.b === undefined) && (d.a ?? d.b) && typeof (d.a ?? d.b) === 'object' && (nodeRoots[0].has(d.at) || nodeRoots[1].has(d.at))) {
   originals.push(d); missingLeaves(d.a ?? d.b, d.at, d.a !== undefined, expanded);
  } else expanded.push(d);
 }
 for (const d of expanded) {
  // First-seen alias cascades are expressly outside this new approval.
  if (isDeepStrictEqual(aliasShape(d.a), aliasShape(d.b))) continue;
  const leftProved = d.a === undefined || sideLeaves[0].has(d.at);
  const rightProved = d.b === undefined || sideLeaves[1].has(d.at);
  if (leftProved && rightProved && (d.a !== undefined || d.b !== undefined)) proposed.set(d.at, 'user-approved observer lifecycle; each present leaf matches its unique validated counterpart at the recorded original path; run/seq offsets are locally proven');
 }
 // A proposed length path may be absent from this comparison; no broad regex.
 const result = new Map([...proposed].filter(([at]) => expanded.some(d => d.at === at)));
 result.expandedDifferences = expanded;
 result.wholeRecordDifferences = originals;
 result.witness = proof;
 return result;
}
