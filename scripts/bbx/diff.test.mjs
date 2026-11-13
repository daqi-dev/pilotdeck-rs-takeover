// Public comparator regression: ignored text cannot allocate observable UUID aliases.
// Exercises the real diff CLI, with no product process, network or user home.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {spawnSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';

const diff = fileURLToPath(new URL('./diff.mjs', import.meta.url));
const ids = prefix => [1, 2, 3].map(n => `${prefix}${n}-1111-4111-8111-111111111111`);
function capture(side, timing = false) {
  const [connection, entry, turn] = ids(side === 'ts' ? 'a000000' : 'b000000');
  return {
    scenario: 'timing-uuid-alias-regression', side,
    steps: [{serve: {stdout: `${timing ? `[gateway:turn-timing] {"runId":"${turn}","phase":"accepted"}\n` : ''}ready\n[adapters] startup task completed\n`}, then: [
      {hello: {interactionBinding: {connectionId: connection, generation: 1}}},
      {frames: [{result: {messages: [{entryId: entry, turnId: turn}], stream: {runId: turn, events: [
        {type: 'turn_started', runId: turn}, {type: 'input_accepted', runId: turn},
      ]}}}]},
    ]}],
    tree: {registry: {json: {events: {[`\0session\0${turn}`]: [{entryId: entry, runId: turn}]}}}},
  };
}
function compare(left, right) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-timing-uuid-'));
  try {
    const files = [left, right].map((value, i) => {
      const file = path.join(root, `${i}.json`); fs.writeFileSync(file, JSON.stringify(value)); return file;
    });
    const result = spawnSync(process.execPath, [diff, ...files], {encoding: 'utf8', timeout: 10000});
    assert.equal(result.signal, null); assert.equal(result.error, undefined);
    return {status: result.status, verdict: result.stdout.trim().split('\n').at(-1), stderr: result.stderr};
  } finally { fs.rmSync(root, {recursive: true, force: true}); }
}
const rejected = (left, right) => { const result = compare(left, right); assert.equal(result.status, 1, JSON.stringify(result)); assert.match(result.verdict, /^DIFF [1-9]\d*$/); };

test('existing ignored timing runId cannot claim a first-seen alias before hello', () => {
  const result = compare(capture('ts'), capture('rust', true));
  assert.equal(result.status, 0, JSON.stringify(result)); assert.equal(result.verdict, 'SAME');
});
test('additional UUIDs in an already ignored timing line have no alias side effects', () => {
  const right = capture('rust', true);
  right.steps[0].serve.stdout = '[gateway:turn-timing] {"runId":"cccccccc-1111-4111-8111-111111111111"}\n' + right.steps[0].serve.stdout;
  assert.equal(compare(capture('ts'), right).verdict, 'SAME');
});
test('connection and run identity conflation is still rejected', () => {
  const right = capture('rust', true);
  right.steps[0].then[0].hello.interactionBinding.connectionId = right.steps[0].then[1].frames[0].result.stream.runId;
  rejected(capture('ts'), right);
});
test('broken references inside a turn remain observable', () => {
  const right = capture('rust', true);
  right.steps[0].then[1].frames[0].result.stream.events[0].runId = 'dddddddd-1111-4111-8111-111111111111';
  rejected(capture('ts'), right);
});
test('missing events remain observable', () => {
  const right = capture('rust', true); right.steps[0].then[1].frames[0].result.stream.events.pop();
  rejected(capture('ts'), right);
});
test('event order remains observable', () => {
  const right = capture('rust', true); right.steps[0].then[1].frames[0].result.stream.events.reverse();
  rejected(capture('ts'), right);
});
test('non-timing log bytes are still compared', () => {
  const right = capture('rust', true); right.steps[0].serve.stdout = right.steps[0].serve.stdout.replace('startup task completed', 'startup task CORRUPTED');
  rejected(capture('ts'), right);
});
