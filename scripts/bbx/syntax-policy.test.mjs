import test from 'node:test';
import assert from 'node:assert/strict';
import { syntaxContract, syntaxAcceptedPaths } from './syntax-policy.mjs';

const config = { operation: 'write', extension: 'js', badContent: 'const value = ;\n', goodContent: 'const value = 2;\n', locations: { ts: [1, 15], rust: [1, 15] } };
function fixture(side) {
  const text = `Created bad.js.\n\nSyntax issues detected:\n- L1:15 error: ${side === 'ts' ? 'Expression expected.' : 'Unexpected token'}`;
  const total = side === 'ts' ? 120 : 119;
  const budget = { type: 'context_budget', runId: 'run', localEstimateTokens: total, source: 'calibrated', exact: false,
    estimateSource: 'usage', calibrationActualInputTokens: 12, calibrationEstimatedInputTokens: 100,
    effectiveTotal: 1000, used: 60, displayUsed: 60, usageTokens: 60, ratio: 0.06,
    breakdown: { total, system: 40, tools: 50, messages: total - 90, mcp: 0, memory: 0 } };
  const request = (id, content) => {
    const body = { model: 'baseline-model', messages: [{ role: 'tool', tool_call_id: id, content }] };
    return { headers: { 'content-length': String(Buffer.byteLength(JSON.stringify(body))) }, body };
  };
  return { scenario: 't1-diagnostics-write-js', provider: {
    'verified-bad': [request('call_bad', text)], 'verified-good': [request('call_good', 'Created good.js.')],
  }, tree: { '/proj/bad.js': { text: config.badContent }, '/proj/good.js': { text: config.goodContent } },
  steps: [{}, { frames: [
    { event: { type: 'tool_call_finished', toolCallId: 'call_bad', runId: 'run', ok: true, resultPreview: text, resultBytes: Buffer.byteLength(text) } },
    { event: { type: 'tool_result_detail_available', toolCallId: 'call_bad', runId: 'run', fullText: text } }, { event: budget },
  ] }] };
}
const tokens = (text) => text.includes('Expression expected.') ? 20 : 19;
test('records only verified text and arithmetic-derived paths', () => {
  const a = fixture('ts'), b = fixture('rust');
  const paths = syntaxAcceptedPaths(a, b, config, tokens);
  assert.equal(syntaxContract(b, config, 'rust').bad.endsWith('Unexpected token'), true);
  assert.ok(paths.has('$.steps[1].frames[0].event.resultBytes'));
  assert.ok(paths.has('$.steps[1].frames[2].event.breakdown.messages'));
  assert.ok(paths.has('$.provider.verified-bad[0].body.messages[0].content'));
  assert.equal(paths.has('$.provider.verified-bad[0].body.model'), false);
  assert.equal(paths.has('$.steps[1].frames[0].event.ok'), false);
});
for (const [name, mutate] of [
  ['severity removed', (x) => { x.provider['verified-bad'][0].body.messages[0].content = x.provider['verified-bad'][0].body.messages[0].content.replace('error: ', ''); }],
  ['severity changed', (x) => { x.provider['verified-bad'][0].body.messages[0].content = x.provider['verified-bad'][0].body.messages[0].content.replace('error:', 'warning:'); }],
  ['empty diagnostic', (x) => { x.provider['verified-bad'][0].body.messages[0].content = 'Created bad.js.\n\nSyntax issues detected:\n'; }],
  ['invalid diagnostic location', (x) => { x.provider['verified-bad'][0].body.messages[0].content = x.provider['verified-bad'][0].body.messages[0].content.replace('L1:15', 'L1:14'); }],
  ['wrong persisted bad content', (x) => { x.tree['/proj/bad.js'].text = 'wrong'; }],
  ['wrong persisted good content', (x) => { x.tree['/proj/good.js'].text = 'wrong'; }],
  ['good file emits diagnostics', (x) => { x.provider['verified-good'][0].body.messages[0].content += '\nSyntax issues detected:'; }],
  ['failed tool result', (x) => { x.steps[1].frames[0].event.ok = false; }],
  ['invalid result byte count', (x) => { x.steps[1].frames[0].event.resultBytes++; }],
  ['arbitrary provider model', (x) => {
    const req = x.provider['verified-bad'][0]; req.body.model = 'tampered-model';
    req.headers['content-length'] = String(Buffer.byteLength(JSON.stringify(req.body)));
  }],
  ['invalid provider length', (x) => { x.provider['verified-bad'][0].headers['content-length'] = '1'; }],
  ['arbitrary token delta', (x) => { x.steps[1].frames[2].event.localEstimateTokens++; }],
  ['invalid calibrated usage', (x) => { x.steps[1].frames[2].event.used++; }],
  ['invalid projected token ratio', (x) => { x.steps[1].frames[2].event.ratio = 0; }],
]) test(`rejects ${name}`, () => {
  const a = fixture('ts'), b = fixture('rust'); mutate(b);
  assert.throws(() => syntaxAcceptedPaths(a, b, config, tokens), /syntax policy:/);
});
test('unrelated header changes do not gain accepted paths', () => {
  const a = fixture('ts'), b = fixture('rust'); b.provider['verified-bad'][0].headers.authorization = 'unexpected';
  assert.equal(syntaxAcceptedPaths(a, b, config, tokens).has('$.provider.verified-bad[0].headers.authorization'), false);
});
