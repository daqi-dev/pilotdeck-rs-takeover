import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
const [oraclePath,capturePath]=process.argv.slice(2);
const oracle=JSON.parse(readFileSync(oraclePath,'utf8'));
const capture=JSON.parse(readFileSync(capturePath,'utf8'));
assert.equal(capture.node,oracle.node);
assert.deepEqual(capture.cases,oracle.cases);
assert.deepEqual(capture.bounded,oracle.bounded);
console.log('7 exact cases and bounded UTF-16 hash match');
