import assert from 'node:assert/strict';
import test from 'node:test';
import { observePreviousCitation } from './citation-lifecycle-observation.js';
const input = { tenantId: 'tenant', agentId: 'agent', chunkId: 'old-chunk' };
test('old citation observation records actual rejection or resolution under the original scope', async () => {
  let seen;
  const rejected = await observePreviousCitation(async request => {
    seen = request;
    throw Object.assign(new Error('Knowledge citation not found'), { code: 'STAFFDECK_HTTP_404' });
  }, input);
  assert.deepEqual(seen, input);
  assert.equal(rejected.rejected, true);
  assert.deepEqual(rejected.request, input);
  const resolved = await observePreviousCitation(async () => ({ id: input.chunkId, content: 'original' }), input);
  assert.equal(resolved.rejected, false);
  assert.deepEqual(resolved.result, { id: input.chunkId, content: 'original' });
});
test('auth, outage, other 404 and invalid responses cannot prove old citation rejection', async () => {
  for (const [code, message] of [['STAFFDECK_HTTP_401', 'Unauthorized'], ['STAFFDECK_HTTP_503', 'Unavailable'],
    ['STAFFDECK_HTTP_404', 'Knowledge base not found']]) {
    const failure = Object.assign(new Error(message), { code });
    await assert.rejects(() => observePreviousCitation(async () => { throw failure; }, input), error => error === failure);
  }
  await assert.rejects(() => observePreviousCitation(async () => ({}), input), /invalid owner response/);
});
