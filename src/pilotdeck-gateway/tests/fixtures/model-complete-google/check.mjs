import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

export function compare(expected, actual) {
  assert.equal(expected.sourceCommit, 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85');
  assert.equal(expected.cases.length, 35);
  assert.equal(expected.cases.reduce((n, value) => n + value.requests.length, 0), 39);
  assert.deepEqual(actual, expected);
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  compare(...process.argv.slice(2, 4).map(file => JSON.parse(readFileSync(file, 'utf8'))));
  console.log('Google complete: source, all calls, outcomes and full wire requests match');
}
