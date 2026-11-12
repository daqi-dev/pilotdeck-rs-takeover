#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { capture, compare } from './capture.mjs';
import { captureStartup } from './startup.mjs';

const [baselineArg, binaryArg, expectedArg, outArg, mode = 'basic', acceptedStopDiff] = process.argv.slice(2);
assert.ok(baselineArg && binaryArg && expectedArg && outArg, 'compare.mjs BASELINE RUST_BINARY TS_OUTPUT NEW_OUTPUT [basic|interactions|cron|startup|stop|reconnect-rejected] [ACCEPTED_STOP_DIFF_PATH]');
const baseline = path.resolve(baselineArg), command = fs.realpathSync(binaryArg), out = path.resolve(outArg);
const expected = JSON.parse(fs.readFileSync(path.join(expectedArg, 'contract.json')));
assert.ok(['basic', 'interactions', 'cron', 'startup', 'stop', 'reconnect-rejected'].includes(mode));
// The stop expectation is a narrowly identified approved repair, not a blanket
// normalizer. Require its registered document and record the exact applied delta.
if (mode === 'stop') assert.ok(acceptedStopDiff && fs.statSync(acceptedStopDiff).isFile(), 'stop comparison requires the registered accepted-difference file');
const options = { baseline, out, command, args: ['server', '--host', '127.0.0.1', '--port', '0'] };
const actual = mode === 'startup' ? await captureStartup(options) : await capture({ ...options, interactions: mode === 'interactions', cron: mode === 'cron', stopProbe: mode === 'stop', stopExpectation: 'native-cancels', reconnectRejected: mode === 'reconnect-rejected' });
if (mode === 'stop') {
  assert.equal(expected.stopProbe.providerStillOpen, true, 'fixed TS evidence must expose the retained provider');
  assert.equal(expected.stopProbe.oldReplyRejected, true);
  expected.stopProbe.expectation = 'native-cancels'; expected.stopProbe.providerStillOpen = false; expected.stopProbe.oldReplyRejected = false;
}
compare(expected, actual);
fs.writeFileSync(path.join(out, 'comparison.json'), JSON.stringify({ passed: true, expected: path.resolve(expectedArg), mode, acceptedStopDiff: mode === 'stop' ? path.resolve(acceptedStopDiff) : null }, null, 2) + '\n');
console.log(JSON.stringify({ passed: true, mode, out }));
