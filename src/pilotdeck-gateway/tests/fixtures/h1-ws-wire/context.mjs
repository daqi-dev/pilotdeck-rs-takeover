#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {fileURLToPath} from 'node:url';
import {decodedMessages, verify, verifyWire} from './capture.mjs';

const [capture, output] = process.argv.slice(2);
assert.ok(capture && output && !fs.existsSync(output));
const source = fs.readFileSync(path.join(capture, 'results.json'));
const rows = JSON.parse(source);
assert.deepEqual(rows.map(row => row.channel), ['mattermost', 'homeassistant']);
const checks = [];
for (const row of rows) {
  assert.notEqual(row.fragmented, true, 'this audit removes one complete Binary frame');
  assert.ok(row.outboundFrames.every(frame => frame.fin && frame.opcode !== 0), 'raw stream has no fragmented message');
  await verifyWire(row);
  verify(row);
  const binary = row.outboundFrames.filter(frame => frame.opcode === 2);
  assert.equal(binary.length, 1);
  assert.equal(binary[0].fin, true);
  let failure;
  try {
    await decodedMessages(row.outboundFrames.filter(frame => frame.opcode !== 2));
  } catch (error) {
    failure = {code: error.code, message: error.message};
  }
  assert.deepEqual(failure, {code: 'Z_DATA_ERROR', message: 'invalid distance too far back'});
  checks.push({channel: row.channel, completeStreamValid: true, omittedBinaryDictionary: failure});
}
const fixturePath = fileURLToPath(new URL('./mattermost-deflate.json', import.meta.url));
const fixtureBytes = fs.readFileSync(fixturePath);
const fixture = JSON.parse(fixtureBytes);
const captured = rows[0].outboundFrames.filter(frame => frame.opcode === 1 || frame.opcode === 2);
assert.deepEqual(fixture.frames, captured.map(({opcode, fin, rsv1, payloadBase64}) => ({opcode, fin, rsv1, payloadBase64})));
const normal = rows[0].serverMessages.find(message => message.opcode === 1);
assert.equal(fixture.expectedMessage, JSON.parse(JSON.parse(normal.text).data.post).message.trim());
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
fs.writeFileSync(output, JSON.stringify({valid: true, capture: path.resolve(capture), sourceSha256: hash(source), checks, rustFixtureMatchesCapturedBytes: true, rustFixtureSha256: hash(fixtureBytes)}, null, 2) + '\n');
console.log('both formal streams require their ignored Binary dictionary; Rust fixture matches raw captured payloads');
