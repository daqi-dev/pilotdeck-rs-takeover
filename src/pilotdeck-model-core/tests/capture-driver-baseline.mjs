// Read-only direct execution of three pure modules from the fixed TS baseline.
// No product runtime, credentials, network, or copied/reimplemented TS functions.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { resolve, join } from 'node:path';
import { pathToFileURL } from 'node:url';

const [baselineArg, outputArg] = process.argv.slice(2);
if (!baselineArg || !outputArg) throw new Error('usage: node --experimental-strip-types capture-driver-baseline.mjs <baseline> <output>');
const baseline = resolve(baselineArg);
const git = args => execFileSync('git', ['--no-replace-objects', '-C', baseline, ...args]);
const commit = git(['rev-parse', 'HEAD']).toString().trim();
if (commit !== 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85') throw new Error(`unexpected baseline ${commit}`);
if (git(['status', '--porcelain', '--untracked-files=no']).length) throw new Error('baseline tracked files must be clean');
const paths = [
  'src/model/policy/RetryPolicy.ts',
  'src/model/streaming/StreamingCheckpoint.ts',
  'src/model/streaming/continuationRequest.ts',
];
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const sourceSha256 = Object.fromEntries(paths.map(path => {
  const bytes = readFileSync(join(baseline, path));
  if (hash(bytes) !== hash(git(['show', `${commit}:${path}`]))) throw new Error(`dirty source ${path}`);
  return [path, hash(bytes)];
}));
const [retry, checkpoint, continuation] = await Promise.all(paths.map(path => import(pathToFileURL(join(baseline, path)))));

// Tagged only at the fixture boundary: preserve values that ordinary JSON loses.
function encode(value) {
  if (value === undefined) return { $undefined: true };
  if (typeof value === 'number') {
    if (Number.isNaN(value)) return { $number: 'NaN' };
    if (value === Infinity) return { $number: '+Infinity' };
    if (value === -Infinity) return { $number: '-Infinity' };
    if (Object.is(value, -0)) return { $number: '-0' };
  }
  if (Array.isArray(value)) return value.map(encode);
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([k,v]) => [k, encode(v)]));
  return value;
}
const configs = [
  {}, { retry: {} }, { retry: null },
  { retry: { streamMaxRetries: 0, requestMaxRetries: 1.5 } },
  { retry: { baseDelayMs: 40, maxDelayMs: 60, jitter: 0.25 } },
  { retry: { baseDelayMs: null, jitter: null, maxDelayMs: null } },
  { retry: { baseDelayMs: -0, maxDelayMs: 0 } },
  { retry: { baseDelayMs: 0, maxDelayMs: -0 } },
  { retry: { maxDelayMs: NaN } },
  { retry: { baseDelayMs: Infinity, jitter: 0 } },
  { retry: { baseDelayMs: -10, jitter: 2 } },
  { retry: { maxDelayMs: -Infinity } },
];
const retryCases = [];
for (const kind of ['stream', 'request']) {
  for (const [i, provider] of configs.entries()) {
    const input = { provider, kind, attempt: i % 3 };
    const random = i % 2 ? 0.25 : 1;
    const policy = retry.createNativeRetryPolicy({ defaultMaxRetries: 2, defaultJitter: kind === 'request' ? 0 : 0.75, random: () => random });
    retryCases.push({ input, random, expected: policy.decide(input) });
  }
}
for (const retryAfterMs of [0, -0, 60, 9000, NaN, Infinity]) {
  const input = { provider: {}, kind: 'stream', attempt: 7, retryAfterMs };
  const random = 0.5;
  retryCases.push({ input, random, expected: retry.createNativeRetryPolicy({ random: () => random }).decide(input) });
}
const checkpointInputs = [
  [],
  [{ type: 'message_start', id: 'ignored' }],
  [{ type: 'text_delta', text: ' \t\u00a0\ufeff' }],
  [{ type: 'text_delta', text: '\ud800' }, { type: 'text_delta', text: '😀x' }],
  [{ type: 'thinking_delta', text: 'reason' }, { type: 'text_delta', text: 'answer' }],
  [{ type: 'text_delta', text: 'answer' }, { type: 'thinking_delta', text: 'reason' }],
  [{ type: 'tool_call_delta', id: 'a', delta: '😀' }, { type: 'tool_call_start', id: 'a', name: 'read' }, { type: 'tool_call_delta', id: 'a', delta: '\ud800{}' }],
  [{ type: 'tool_call_start', id: 'a', name: 'one' }, { type: 'tool_call_start', id: 'b', name: 'two' }, { type: 'tool_call_start', id: 'a', name: 'replaced' }, { type: 'tool_call_delta', id: 'b', delta: '😀' }, { type: 'tool_call_end', toolCall: { id: 'a' } }],
  [{ type: 'tool_call_start', id: '\ud800', name: '\udfff' }, { type: 'tool_call_end', toolCall: { id: '\ud800' } }],
];
const snapshot = manager => ({ partialText: manager.get().partialText, hasText: manager.hasSubstantialContent(), canContinueText: manager.canContinueText(), interruption: manager.interruption() });
const checkpointCases = checkpointInputs.map(events => {
  const manager = new checkpoint.StreamingCheckpointManager();
  const snapshots = [snapshot(manager)];
  for (const event of events) { manager.onEvent(event); snapshots.push(snapshot(manager)); }
  return { events, snapshots };
});
const text = (role, value) => ({ role, content: [{ type: 'text', text: value }] });
const instruction = continuation.LITELLM_CONTINUATION_INSTRUCTION;
const original = { model: 'm', messages: [text('user', 'question')], cachePlan: { index: 1 }, cacheBreakpoints: [0], metadata: { value: '\ud800' } };
const continuationInputs = [
  { original, partial: '' },
  { original, partial: '\ud800😀' },
  { original: { ...original, messages: [...original.messages, text('assistant', 'old'), text('user', instruction)] }, partial: 'replacement' },
  { original: { ...original, messages: [...original.messages, text('assistant', 'old'), text('user', `${instruction} `)] }, partial: 'keep pair' },
  { original: { messages: [text('user', instruction)] }, partial: 'single' },
  { original: { messages: [text('tool', 'old'), text('user', instruction)] }, partial: 'wrong role' },
];
const continuationCases = continuationInputs.map(({ original, partial }) => ({ original, partial, expected: continuation.buildLiteLLMContinuationRequest(original, partial) }));
const output = { sourceCommit: commit, sourceSha256, node: process.version, retryCases, checkpointCases, continuationCases };
writeFileSync(resolve(outputArg), JSON.stringify(encode(output), null, 2) + '\n');
process.stdout.write(JSON.stringify({ sourceCommit: commit, retry: retryCases.length, checkpoint: checkpointCases.length, continuation: continuationCases.length }) + '\n');
