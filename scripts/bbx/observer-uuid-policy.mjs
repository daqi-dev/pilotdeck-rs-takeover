// Eight witnessed complete-UUID entities; same-ID normalization only.
// No new compatibility allowance, array reorder, or unknown-ID renumbering.
import assert from 'node:assert/strict';
import { lifecyclePair } from './observer-lifecycle-policy.mjs';
import { createHash } from 'node:crypto';
const checkedEvidence = new WeakSet();
const freeze = value => { if (value && typeof value === 'object') { Object.values(value).forEach(freeze); Object.freeze(value); } return value; };
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const one = (xs, why) => { assert.equal(xs.length, 1, why); return xs[0]; };
const text = v => typeof v === 'string' ? v : (v ?? []).filter(b => b.type === 'text').map(b => b.text).join('');
function parentBlock(raw, proof, step, entity) {
 const run = proof.anchors[step === 0 ? 'parentRun0' : 'parentRun1'];
 const expected = step === 0 ? 'AG03-PARENT-DONE' : 'AG03-PARENT-CHECK-DONE';
 const frames = raw.steps[step].frames;
 const block = one(frames.filter(f => f.event?.type === 'assistant_block'), entity + ' unique actual block').event;
 const delta = one(frames.filter(f => f.event?.type === 'assistant_text_delta'), entity + ' unique actual delta').event;
 assert.equal(block.kind, 'text'); assert.equal(block.streamState, 'closed');
 assert.equal(block.runId, run); assert.equal(delta.runId, run);
 assert.equal(block.text, expected); assert.equal(delta.text, expected);
 const match = /^([0-9a-f-]{36}):text:0$/.exec(block.blockId); assert.ok(match && uuid.test(match[1]), 'full block UUID, not fragment');
 const id = match[1];
 assert.equal(block.timeline.id, block.blockId); assert.equal(delta.timeline.id, block.blockId);
 assert.equal(block.timeline.turnId, run); assert.equal(delta.timeline.turnId, run);
 assert.equal(block.timeline.order, step === 0 ? 2 : 0); assert.equal(delta.timeline.order, block.timeline.order);
 if (step === 0) assert.equal(block.timeline.previousId, 'result:ag03-agent');
 const rows = raw.tree[proof.parentPath].jsonl;
 const message = one(rows.filter(r => r.type === 'assistant_message' && r.turnId === run && text(r.message.content) === expected), 'unique durable parent final');
 const persisted = one(message.message.content.filter(b => b.type === 'text'), 'durable final text block');
 assert.equal(persisted.blockId, block.blockId); assert.deepEqual(persisted.timeline, block.timeline);
 const terminal = one(rows.filter(r => r.type === 'turn_result' && r.turnId === run), 'parent terminal');
 assert.equal(terminal.result.type, 'success');
 const final = one(terminal.result.finalMessage.content.filter(b => b.type === 'text'), 'terminal exact block');
 assert.equal(final.text, expected); assert.equal(final.blockId, block.blockId); assert.deepEqual(final.timeline, block.timeline);
 // lifecyclePair independently checked complete WS-registry and W1 history,
 // and parent durable/projection identities. No index alone selects the ID.
 return { entity, id, source: { wireStep: step, run, blockId: block.blockId, durableEntryId: message.entryId, durableSequence: message.sequence, terminalEntryId: terminal.entryId } };
}

function uniquePairs(pairs) {
 const names = new Set(), sides = [new Set(), new Set()];
 assert.equal(pairs.length, 8, 'only the eight proved entities');
 for (const row of pairs) { assert.ok(!names.has(row.entity), 'duplicate entity'); names.add(row.entity); for (const [side, id] of [row.left, row.right].entries()) { assert.ok(uuid.test(id)); assert.ok(!sides[side].has(id), 'duplicate or many-to-one UUID relation'); sides[side].add(id); } }
}

function strongIdentityPairs(left, right) {
 const proof = lifecyclePair(left, right);
 const pair = [];
 for (const name of Object.keys(proof.left.anchors)) pair.push({ entity: name, left: proof.left.anchors[name], right: proof.right.anchors[name], proof: 'existing lifecycle strong-entity contract' });
 for (const [step, entity] of [[0, 'parentFinalBlockW0'], [3, 'parentFinalBlockW1']]) {
  const a = parentBlock(left, proof.left, step, entity), b = parentBlock(right, proof.right, step, entity);
  pair.push({ entity, left: a.id, right: b.id, proof: { left: a.source, right: b.source } });
 }
 const names = new Set(), ids = [new Map(), new Map()];
 for (const row of pair) {
  assert.ok(!names.has(row.entity), 'duplicate entity'); names.add(row.entity);
  for (const [side, id] of [row.left, row.right].entries()) {
   assert.ok(uuid.test(id), 'full entity UUID'); assert.ok(!ids[side].has(id), 'duplicate/many-to-one entity UUID'); ids[side].set(id, row.entity);
  }
 }
 const evidence = freeze({ pairs: pair, rawProof: structuredClone(proof), unknownPolicy: 'Every ID not in these eight independently witnessed entities remains exactly as the original normalized view; no structural or textual guessing.' }); checkedEvidence.add(evidence); return evidence;
}
function relabelProvedAliases(leftView, rightView, leftAliases, rightAliases, evidence) {
 assert.ok(checkedEvidence.has(evidence), 'unproved entity evidence');
 uniquePairs(evidence.pairs);
 const before = [leftView, rightView].map(v => JSON.stringify(v));
 const maps = [new Map(leftAliases), new Map(rightAliases)];
 const start = Math.max(0, ...leftAliases.values(), ...rightAliases.values()) + 1;
 for (let i = 0; i < evidence.pairs.length; i++) for (const source of before) assert.ok(!source.includes(`<uuid:${start + i}>`), 'reserved token collision with an unknown literal');
 const tokenMaps = [new Map(), new Map()];
 evidence.pairs.forEach((row, index) => {
  for (const side of [0, 1]) {
   const id = side === 0 ? row.left : row.right;
   assert.ok(maps[side].has(id), 'proved entity must exist in actual normalizer map');
   tokenMaps[side].set(`<uuid:${maps[side].get(id)}>`, `<uuid:${start + index}>`);
   maps[side].set(id, start + index);
  }
 });
 function walk(value, tokens) {
  if (typeof value === 'string') return value.replace(/<uuid:\d+>/g, token => tokens.get(token) ?? token);
  if (Array.isArray(value)) return value.map(v => walk(v, tokens)); // same length and order
  if (value && typeof value === 'object') {
   const entries = Object.entries(value).map(([key, child]) => [walk(key, tokens), walk(child, tokens)]);
   assert.equal(new Set(entries.map(([key]) => key)).size, entries.length, 'normalized key collision');
   return Object.fromEntries(entries); // same insertion order; no sorting
  }
  return value;
 }
 const views = [walk(leftView, tokenMaps[0]), walk(rightView, tokenMaps[1])];
 assert.deepEqual([leftView, rightView].map(v => JSON.stringify(v)), before, 'input views immutable');
 return { views, aliases: maps, entityTokens: evidence.pairs.map((row, i) => ({ entity: row.entity, token: `<uuid:${start + i}>`, left: row.left, right: row.right })) };
}
const sceneHashes = new Set(['9f8c13a2e92a85869dfb5ee4ed602560fe1002b78888a111068c6a511c4563ca', 'd654c045d164826de415eb7581325471b5b0b32b57d664a8069a1feb504e101a']);
const sceneNames = new Set(['agent03-sdk-fork', 'agent03-sdk-fork-lifecycle-probe']);
const omitted = new Set(['side', 'evidence', 'startMs', 'serverLog', 'failures']);
const fingerprint = value => createHash('sha256').update(JSON.stringify(value)).digest('hex');
const allUuids = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi;
function bindInputs(raw, view, aliases, evidence, side) {
 assert.ok(aliases instanceof Map, 'actual normalizer aliases required');
 const compared = Object.fromEntries(Object.entries(raw).filter(([key]) => !omitted.has(key)));
 const rawText = JSON.stringify(compared), viewText = JSON.stringify(view);
 const occurrences = new Map();
 for (const id of rawText.match(allUuids) ?? []) occurrences.set(id.toLowerCase(), (occurrences.get(id.toLowerCase()) ?? 0) + 1);
 const numbers = new Set();
 for (const [id, number] of aliases) {
  assert.ok(uuid.test(id) && occurrences.has(id), 'alias must originate in compared raw input');
  assert.ok(Number.isSafeInteger(number) && number > 0 && !numbers.has(number), 'normalizer aliases must be unique positive integers'); numbers.add(number);
 }
 for (let i = 1; i <= aliases.size; i++) assert.ok(numbers.has(i), 'actual first-seen alias table must be dense');
 const proof = side === 0 ? evidence.rawProof.left : evidence.rawProof.right;
 const parent = view.tree[proof.parentPath].jsonl;
 const alias = id => { assert.ok(aliases.has(id), 'strong UUID exists in actual aliases'); return `<uuid:${aliases.get(id)}>`; };
 const accepted = parent.filter(r => r.type === 'accepted_input');
 assert.equal(accepted[0].turnId, alias(proof.anchors.parentRun0), 'normalized W0 raw binding');
 assert.equal(accepted[1].turnId, alias(proof.anchors.parentRun1), 'normalized W1 raw binding');
 for (const role of ['worker', 'reviewer']) assert.equal(one(parent.filter(r => r.type === 'subagent_started' && r.subagentType === role), 'normalized child role').subagentId, alias(proof.anchors[role]), 'normalized raw child binding');
 const rawReviewer = one(raw.tree[proof.parentPath].jsonl.filter(r => r.type === 'subagent_started' && r.subagentType === 'reviewer'), 'raw reviewer reference');
 const reviewerSuffix = '/' + rawReviewer.transcriptRelativePath.replace(allUuids, id => alias(id.toLowerCase()));
 const reviewerRows = one(Object.entries(view.tree).filter(([p, file]) => p.endsWith(reviewerSuffix) && file.jsonl), 'normalized reviewer transcript')[1].jsonl;
 const reviewerFinal = one(reviewerRows.filter(r => r.type === 'assistant_message'), 'normalized reviewer final');
 assert.equal(one(reviewerFinal.message.content.filter(b => b.type === 'text'), 'normalized reviewer text block').blockId, alias(proof.anchors.reviewerBlock) + ':text:0', 'normalized reviewer block raw binding');
 const second = one(Object.entries(view.tree).filter(([p, f]) => p.endsWith('/chats/agent03-second.jsonl') && f.jsonl), 'normalized second session')[1].jsonl;
 assert.equal(one(second.filter(r => r.type === 'accepted_input'), 'normalized second input').turnId, alias(proof.anchors.secondSessionRun));
 for (const row of evidence.pairs) {
  const id = side === 0 ? row.left : row.right, token = alias(id);
  // The entire compared graph must contain precisely the raw references to
  // this entity: catches transplanted views/tables and literal-token spoofing.
  assert.equal(viewText.split(token).length - 1, occurrences.get(id), 'raw/view/alias reference binding for ' + row.entity);
 }
 for (const [step, entity] of [[0, 'parentFinalBlockW0'], [3, 'parentFinalBlockW1']]) {
  const normalized = one(view.steps[step].frames.filter(f => f.event?.type === 'assistant_block'), 'normalized final block').event;
  assert.equal(normalized.blockId, alias(evidence.pairs.find(p => p.entity === entity)[side === 0 ? 'left' : 'right']) + ':text:0');
 }
 return { raw: fingerprint(raw), normalized: fingerprint(view), aliases: fingerprint([...aliases]) };
}
/** Internal validation and immediate application: caller cannot supply a proof.
 * Only known entity tokens change; unknown aliases and all array positions stay.
 */
export function observerUuidViews(rawA, rawB, viewA, viewB, aliasesA, aliasesB) {
 if (![rawA, rawB].some(raw => sceneNames.has(raw.scenario) || sceneHashes.has(raw.evidence?.scenario?.sha256))) return { views: [viewA, viewB], aliases: [aliasesA, aliasesB], entityTokens: [], bindings: null };
 const evidence = strongIdentityPairs(rawA, rawB);
 const bindings = [bindInputs(rawA, viewA, aliasesA, evidence, 0), bindInputs(rawB, viewB, aliasesB, evidence, 1)];
 const result = relabelProvedAliases(viewA, viewB, aliasesA, aliasesB, evidence);
 for (const [side, values] of [[rawA, viewA, aliasesA], [rawB, viewB, aliasesB]].entries()) assert.deepEqual({ raw: fingerprint(values[0]), normalized: fingerprint(values[1]), aliases: fingerprint([...values[2]]) }, bindings[side], 'proof inputs changed during application');
 return { ...result, bindings, witness: evidence.pairs };
}
