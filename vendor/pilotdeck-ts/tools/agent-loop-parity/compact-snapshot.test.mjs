import assert from 'node:assert/strict';
import test from 'node:test';
import { compactReplacementMessages } from './compact-snapshot.mjs';
const base = { kind: 'compact', subtype: 'compact_boundary' };
const messages = [{ role: 'assistant', content: [{ type: 'text', text: 'Skill: skill-1; File: /input.txt; Knowledge: doc-1' }] }];
test('reads canonical version1 and supported legacy replacement without dropping content or anchors', () => {
  assert.equal(compactReplacementMessages({ ...base, snapshot: { version: 1, messages } }), messages);
  assert.equal(compactReplacementMessages({ ...base, replacementMessages: messages }), messages);
  const changed = structuredClone(messages);
  changed[0].content[0].text = 'mutated source';
  assert.notDeepEqual(compactReplacementMessages({ ...base, snapshot: { version: 1, messages: changed } }), messages);
});
test('rejects missing, empty, malformed or unknown snapshots rather than hiding corruption with legacy fallback', () => {
  for (const snapshot of [null, {}, { version: 2, messages }, { version: 1, messages: [] },
    { version: 1, messages: [{ role: 'system', content: [] }] }, { version: 1, messages: [{ role: 'user', content: [{ type: 'text', text: 4 }] }] }]) {
    assert.throws(() => compactReplacementMessages({ ...base, snapshot, replacementMessages: messages }));
  }
  assert.throws(() => compactReplacementMessages(base));
  assert.throws(() => compactReplacementMessages({ ...base, replacementMessages: [] }));
  assert.throws(() => compactReplacementMessages({ ...base, replacementMessages: [{ role: 'assistant', content: 'wrong' }] }));
});
