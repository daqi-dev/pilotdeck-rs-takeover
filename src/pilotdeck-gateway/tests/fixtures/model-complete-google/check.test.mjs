import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { compare } from './check.mjs';

const oracle = JSON.parse(readFileSync(new URL('./oracle.json', import.meta.url), 'utf8'));
test('complete checker accepts unchanged capture', () => compare(oracle, structuredClone(oracle)));
for (const [name, mutate] of [
  ['stream endpoint substituted', value => { value.cases[0].requests[0].path += '?alt=sse'; }],
  ['canonical output emptied', value => { value.cases[0].calls[0].response.content = []; }],
  ['retry request removed', value => { value.cases.find(row => row.name === 'http-429-retry').requests.pop(); }],
  ['timeout header removed', value => { value.cases.find(row => row.name === 'provider-timeout-body-read').requests[0].headers = []; }],
  ['cancelled connection retained', value => { value.cases.find(row => row.name === 'abort-body-read').requests[0].clientClosed = false; }],
]) {
  test(`complete checker rejects ${name}`, () => {
    const changed = structuredClone(oracle); mutate(changed);
    assert.throws(() => compare(oracle, changed), { name: 'AssertionError' });
  });
}
