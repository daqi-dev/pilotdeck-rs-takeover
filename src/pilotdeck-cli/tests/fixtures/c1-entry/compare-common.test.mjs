import test from 'node:test';
import assert from 'node:assert/strict';
import { rootMapper, mapStrings, differences, snapshotProjection, sha } from './compare-common.mjs';
import { frameMapper } from '../c1-cron/compare-common.mjs';
const a = '00000000-0000-4000-8000-000000000001', b = '00000000-0000-4000-8000-000000000002';
test('concrete roots map; business timestamps, arbitrary UUIDs and warning text remain', () => {
  const map = rootMapper('/fixture/a');
  assert.deepEqual(mapStrings({ text: `/fixture/a/project ${a}`, createdAt: '2030-01-01', warning: 'SQLite warning' }, map),
    { text: `<C1_FIXTURE>/project ${a}`, createdAt: '2030-01-01', warning: 'SQLite warning' });
  assert.equal(differences({ value: a }, { value: b }).length, 1);
  assert.throws(() => map('<C1_FIXTURE>'), /literal/);
});
test('RPC identity projection keeps envelope links and business strings separate', () => {
  const map = frameMapper([{ value: { id: a } }], value => value);
  assert.equal(map(JSON.stringify({ type: 'event', id: a, event: { text: a } })), JSON.stringify({ type: 'event', id: '<RPC_0>', event: { text: a } }));
  assert.throws(() => map(JSON.stringify({ type: 'response', id: b })), /references/);
  assert.throws(() => frameMapper([{ value: { id: a } }, { value: { id: a } }], value => value), /reused/);
});
test('snapshot validates bytes and retains all text, binary, symlink and extra files', () => {
  const row = text => { const bytes = Buffer.from(text); return { path: 'state.json', size: bytes.length, sha256: sha(bytes), base64: bytes.toString('base64') }; };
  const good = row('{"cwd":"/fixture/a/p","time":123}');
  assert.deepEqual(snapshotProjection([good], rootMapper('/fixture/a')), [{ path: 'state.json', text: '{"cwd":"<C1_FIXTURE>/p","time":123}' }]);
  assert.throws(() => snapshotProjection([{ ...good, sha256: '0'.repeat(64) }], value => value), /hash/);
  assert.throws(() => snapshotProjection([good, good], value => value), /duplicate/);
  assert.equal(differences(snapshotProjection([good], value => value), snapshotProjection([row('{"cwd":"/fixture/a/p","time":124}')], value => value)).length, 1);
  assert.deepEqual(snapshotProjection([{ path: 'pointer', symlink: '/fixture/a/p' }], rootMapper('/fixture/a')), [{ path: 'pointer', symlink: '<C1_FIXTURE>/p' }]);
  const binary = Buffer.from([0xff]);
  assert.deepEqual(snapshotProjection([{ path: 'data', size: 1, sha256: sha(binary), base64: binary.toString('base64') }], value => value), [{ path: 'data', base64: '/w==' }]);
});
test('structural comparator rejects missing keys, reordered lists and byte differences', () => {
  assert.equal(differences({ a: 1 }, {}).length, 1);
  assert.equal(differences([1, 2], [2, 1]).length, 2);
  assert.equal(differences({ file: '{"a":1}' }, { file: '{ "a": 1 }' }).length, 1);
});
