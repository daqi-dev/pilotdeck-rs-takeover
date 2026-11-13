// User-approved two sealed TS captures only; never a general tree sort.
import fs from 'node:fs';
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {brotliDecompressSync} from 'node:zlib';
import {lifecyclePair} from './observer-lifecycle-policy.mjs';
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const RAW_SHA = ['cd96411ddc477083a32947bf4d28cc999675b6fd3431f1f5b39efd5fe020c75e', '5ca79f49465369b96c9b1586018b6927661818bbaaf06f652d6ba9739fd609fd'];
const CLOSURE_SHA = 'a3b4aa0efe32f3edcdfdea5b6b2a47ea92c95746291487b137f565ed681ae358';
const SCENE_SHA = 'd654c045d164826de415eb7581325471b5b0b32b57d664a8069a1feb504e101a';
// The archive is the exact already-reviewed receipt, not a caller-authored proof.
function sealedClosure() {
  const compressed = fs.readFileSync(new URL('./fixtures/memory-tree-order/closure-receipt.json.br', import.meta.url));
  assert.equal(sha(compressed), 'e05fd368b6b0be32b03c63ba38662b0f70ddc43d60918e72508766fb3907fe77', 'sealed compressed closure');
  const bytes = brotliDecompressSync(compressed, {maxOutputLength: 471379});
  assert.equal(bytes.length, 471379);
  assert.equal(sha(bytes), CLOSURE_SHA, 'sealed 644 closure receipt');
  const receipt = JSON.parse(bytes);
  assert.equal(receipt.harness.head, '644a8d80a45916bbe7a0b0ee93ff6e331a9dbf98');
  assert.equal(sha(fs.readFileSync(new URL('./observer-lifecycle-policy.mjs', import.meta.url))), receipt.harness.files['scripts/bbx/observer-lifecycle-policy.mjs'], 'unchanged full lifecycle implementation');
  return receipt.rows.find(row => row.name === 'agent03-sdk-memory-lifecycle').inputs.slice(0, 2);
}
export function memoryTreeOrderPaths(leftFile, rightFile, {self = false, expectedRaw} = {}) {
  const out = new Map();
  if (!self) return out; // No pair permission, even for these exact files.
  const bytes = [leftFile, rightFile].map(file => fs.readFileSync(file));
  // Unknown/future captures obtain no permission, not an artifact-wide waiver.
  if (!bytes.every((value, index) => sha(value) === RAW_SHA[index])) return out;
  const raw = bytes.map(value => JSON.parse(value));
  assert.equal(JSON.stringify(expectedRaw), JSON.stringify(raw), 'trusted comparator raw must match these same sealed file bytes and key order');
  const captured = sealedClosure();
  for (const [i, r] of raw.entries()) {
    assert.equal(r.side, 'ts');
    assert.equal(r.scenario, 'agent03-sdk-fork-lifecycle-probe');
    assert.equal(r.evidence.scenario.name, 'agent03-sdk-memory-lifecycle');
    assert.equal(r.evidence.scenario.sha256, SCENE_SHA);
    assert.deepEqual(r.evidence.inputs, captured[i].capturedInputs);
    assert.equal(r.evidence.runId, captured[i].runId);
  }
  const life = lifecyclePair(...raw); // Entire existing child/parent payload chain.
  const roles = [life.left, life.right].map((side, i) => {
    const prefix = side.parentPath.replace(/\.jsonl$/, '') + '/subagents/';
    const group = id => [prefix + id + '.jsonl', prefix + id + '.jsonl.projections.json'];
    const worker = group(side.anchors.worker), reviewer = group(side.anchors.reviewer);
    const keys = Object.keys(raw[i].tree);
    assert.equal(keys.length, 27);
    assert.deepEqual(keys.slice(5, 9), i === 0 ? [...worker, ...reviewer] : [...reviewer, ...worker]);
    return {keys, remainder: [...keys.slice(0, 5), ...keys.slice(9)]};
  });
  assert.deepEqual(roles[0].remainder, roles[1].remainder, 'all other 23 raw keys and their exact order');
  // The two raw byte hashes fix every key, value, ownership and full source.
  // Only this typed key-order leaf is licensed; ordinary comparison retains all
  // other keys/records/arrays, and nothing here mutates either normalized view.
  out.set('$.tree.<key order>', 'User-approved sealed memory TS self: only indices5..8 worker/reviewer transcript+projection group exchange; every other key/order/content remains strict');
  return out;
}
