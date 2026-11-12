// Fixed TS production consumers -> real ModelRuntime -> Google SDK -> loopback HTTP.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { bindBaseline } from '../_shared/generator_guard.mjs';
import { sourceClosureSha256 } from '../../baseline_source_guard.mjs';

const baseline = resolve(process.env.PDRS_BASELINE!);
const output = resolve(process.argv[2]);
assert.ok(process.env.PDRS_BASELINE && process.env.HOME && process.env.PILOT_HOME);
assert.ok(process.env.HOME.startsWith(resolve('_tmp')), 'isolate HOME under this checkout _tmp');
mkdirSync(output, { recursive: true });
const driverBytes = readFileSync(fileURLToPath(import.meta.url));
writeFileSync(join(output, 'driver.mts'), driverBytes);
const execution = { command: process.execPath, args: [...process.execArgv, ...process.argv.slice(1)], cwd: process.cwd(), env: { HOME: process.env.HOME, PILOT_HOME: process.env.PILOT_HOME, PDRS_BASELINE: baseline, LANG: process.env.LANG, M1_NEGATIVE_RESPONSE: process.env.M1_NEGATIVE_RESPONSE ?? null } };
writeFileSync(join(output, 'execution.json'), JSON.stringify(execution, null, 2) + '\n');
const sourceCommit = 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
const { gitExecutable } = bindBaseline({ baseline, commit: sourceCommit });
const entries = ['src/model/ModelRuntime.ts', 'src/session/title/SessionTitleGenerator.ts',
  'src/session/prompt/PromptSuggestionGenerator.ts', 'src/router/tokenSaver/classifyAndRoute.ts'];
const sourceSha256 = sourceClosureSha256(baseline, sourceCommit, gitExecutable, entries);
const load = (name: string) => import(pathToFileURL(join(baseline, name)).href);
const { createModelRuntime } = await load(entries[0]);
const { createNativeSessionTitleProvider } = await load(entries[1]);
const { createPromptSuggestionGenerator } = await load(entries[2]);
const { classifyAndRoute } = await load(entries[3]);
const transportHeaders = ['host', 'connection', 'content-length', 'user-agent', 'accept',
  'accept-language', 'sec-fetch-mode', 'accept-encoding', 'transfer-encoding', 'keep-alive'];
const jsonReply = (value: unknown, status = 200) => ({ status, headers: { 'content-type': 'application/json' }, chunks: [{ text: JSON.stringify(value) }] });
const response = (text: string) => ({ candidates: [{ content: { role: 'model', parts: [{ text }] }, finishReason: 'STOP' }] });
const ok = (text = '你好 🌍') => jsonReply(response(text));
const errorReply = (status: number) => jsonReply({ error: { code: status, message: 'fixture provider failure', status: status === 429 ? 'RESOURCE_EXHAUSTED' : 'INVALID_ARGUMENT' } }, status);
const provider = { id: 'p', protocol: 'google', url: '$MOCK/v1beta', apiKey: 'fixture-key', headers: {},
  models: { m: { id: 'm', capabilities: { maxContextTokens: 16000, maxOutputTokens: 4096, supportsToolUse: true, supportsSystemPrompt: true }, multimodal: { input: ['text', 'image'] } } },
  retry: { requestMaxRetries: 0, baseDelayMs: 1, maxDelayMs: 1, jitter: 0 } };
const request = { provider: 'p', model: 'm', messages: [{ role: 'user', content: [{ type: 'text', text: 'hi 中文' }] }], maxOutputTokens: 32 };
const rich = { responseId: ' resp/中文 ', ignored: 'drop', candidates: [{ ignored: true,
  content: { role: 'model', parts: [{ text: '思考', thought: true, thoughtSignature: 'sig==' }, { text: 'answer' }, { text: '' },
    { functionCall: { id: 'same id', name: 'lookup', args: { q: '中文' } } },
    { functionCall: { id: 'same id', name: 'lookup2', args: [] } },
    { functionCall: { name: 'lookup3', args: null } }, { inlineData: { mimeType: 'image/png', data: 'QUJD' } }] },
  finishReason: 'MALFORMED_FUNCTION_CALL', citationMetadata: { citationSources: [{ uri: 'fixture' }], ignored: true } }],
  modelVersion: 'model-version', usageMetadata: { promptTokenCount: 10, candidatesTokenCount: 3, thoughtsTokenCount: 2, cachedContentTokenCount: 4, totalTokenCount: 15 } };
const specs: any[] = [
  { name: 'text', replies: [ok()] },
  { name: 'thinking-tools-usage-sdk-projection', replies: [jsonReply(rich)] },
  { name: 'first-candidate-only', replies: [jsonReply({ candidates: [...response('first').candidates, ...response('second').candidates] })] },
  { name: 'empty-object', replies: [jsonReply({})] },
  { name: 'null-json', replies: [jsonReply(null)] },
  { name: 'array-json', replies: [jsonReply([])] },
  { name: 'string-json', replies: [jsonReply('scalar')] },
  { name: 'empty-body', replies: [{ chunks: [] }] },
  { name: 'invalid-json', replies: [{ chunks: [{ text: 'not json' }] }] },
  { name: 'null-body-204', replies: [{ status: 204, chunks: [] }] },
  ...['MAX_TOKENS', 'SAFETY', 'UNEXPECTED_TOOL_CALL', 'strange'].map(reason => ({ name: `finish-${reason}`, replies: [jsonReply({ candidates: [{ finishReason: reason }] })] })),
  { name: 'v1-custom-header-extra-body', provider: { url: '$MOCK/proxy/v1', headers: { 'x-goog-api-key': 'override', 'x-probe': 'complete' }, extraBody: { temperature: 0.1, generationConfig: { temperature: 0.2, topK: 7 }, custom: { yes: true } } }, replies: [ok()] },
  { name: 'request-system-tools-media', request: { systemPrompt: 'system 中文', temperature: 0.4, tools: [{ name: 'lookup', description: 'query', inputSchema: { type: 'object', properties: { q: { type: 'string' } } } }], messages: [{ role: 'user', content: [{ type: 'text', text: 'look' }, { type: 'image', source: 'base64', data: 'QUJD', mimeType: 'image/png' }] }] }, replies: [ok()] },
  { name: 'http-400', replies: [errorReply(400)] },
  { name: 'http-429-retry', provider: { retry: { requestMaxRetries: 1 } }, replies: [errorReply(429), ok('retried')] },
  { name: 'http-503-retry-exhausted', provider: { retry: { requestMaxRetries: 1 } }, replies: [errorReply(503), errorReply(503)] },
  { name: 'http-500-html', replies: [{ status: 500, headers: { 'content-type': 'text/html' }, chunks: [{ text: '<h1>failure</h1>' }] }] },
  { name: 'http-400-malformed-json', replies: [{ status: 400, headers: { 'content-type': 'application/json' }, chunks: [{ text: 'oops' }] }] },
  { name: 'body-destroy-retry', provider: { retry: { requestMaxRetries: 1 } }, replies: [{ chunks: [{ text: '{' }], end: 'destroy' }, ok('after body failure')] },
  { name: 'aborted-before-send', abortBefore: true, replies: [] },
  { name: 'abort-body-read', abortAfterMs: 60, replies: [{ chunks: [{ text: '{' }], end: 'hold' }] },
  { name: 'provider-timeout-body-read', provider: { timeoutMs: 60 }, replies: [{ chunks: [{ text: '{' }], end: 'hold' }] },
  { name: 'provider-timeout-header-ceil', provider: { timeoutMs: 1501 }, replies: [ok()] },
  { name: 'provider-timeout-header-append', provider: { timeoutMs: 1501, headers: { 'X-Server-Timeout': 'custom', 'x-server-timeout': 'second' } }, replies: [ok()] },
  { name: 'stream-idle-budget-does-not-apply', streamTimeoutMs: 1, provider: { retry: { streamIdleTimeoutMs: 1 } }, replies: [{ chunks: [...Array.from({ length: 10 }, () => ({ text: ' ' })), ...ok('after delayed body').chunks] }] },
  { name: 'title-success', consumer: 'title', replies: [ok(process.env.M1_NEGATIVE_RESPONSE ? '{"title":"WRONG"}' : '{"title":"修复登录按钮"}')] },
  { name: 'title-provider-error', consumer: 'title', replies: [errorReply(400)] },
  { name: 'suggestion-success', consumer: 'suggestion', replies: [ok('Suggestion: "补充边界测试"')] },
  { name: 'suggestion-provider-error', consumer: 'suggestion', replies: [errorReply(400)] },
  { name: 'judge-success', consumer: 'judge', replies: [ok('<tier>simple</tier>')] },
  { name: 'judge-provider-error', consumer: 'judge', replies: [errorReply(400)] },
  { name: 'judge-empty-retries', consumer: 'judge', replies: [ok(''), ok(''), ok('')] },
];
let active: any;
const server = createServer(async (req, res) => {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  const index = active.requests.length;
  const record: any = { method: req.method, path: req.url, headers: Object.entries(req.headers).filter(([key]) => !transportHeaders.includes(key)).sort(([a], [b]) => a.localeCompare(b)), body: Buffer.concat(chunks).toString() };
  active.requests.push(record);
  const reply = active.replies[index];
  if (!reply) { res.destroy(); return; }
  if (reply.end === 'hold') res.once('close', () => { record.clientClosed = true; });
  res.writeHead(reply.status ?? 200, reply.headers ?? { 'content-type': 'application/json' });
  for (const chunk of reply.chunks ?? []) { res.write(chunk.text); await new Promise(done => setTimeout(done, 4)); }
  if (reply.end === 'destroy') { res.flushHeaders(); setTimeout(() => res.destroy(), 20); }
  else if (reply.end === 'hold') res.flushHeaders();
  else res.end();
});
await new Promise<void>(done => server.listen(0, '127.0.0.1', done));
const origin = `http://127.0.0.1:${(server.address() as any).port}`;
function stripSdk(value: any): any {
  if (Array.isArray(value)) return value.map(stripSdk);
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).filter(([key]) => key !== 'sdkHttpResponse').map(([key, value]) => [key, stripSdk(value)]));
  return value;
}
const thrown = (error: any) => ({ name: error.name, message: error.message, ...(error.error ? { error: error.error } : {}), ...(error.name === 'ModelRequestError' ? { code: error.code } : {}) });
const cases = [];
try {
  for (const spec of specs) {
    active = { replies: spec.replies, requests: [] };
    const configured = { ...provider, ...spec.provider, retry: { ...provider.retry, ...spec.provider?.retry } };
    configured.url = configured.url.replace('$MOCK', origin);
    const runtime = createModelRuntime({ providers: { p: configured } });
    const calls: any[] = [];
    const actualComplete = runtime.complete;
    runtime.complete = async (input: any, options: any) => {
      const call: any = { request: input, wireStart: active.requests.length };
      calls.push(call);
      try { const result = await actualComplete(input, options); call.response = stripSdk(result); return result; }
      catch (error) { call.thrown = thrown(error); throw error; }
      finally { call.wireEnd = active.requests.length; }
    };
    const controller = new AbortController();
    if (spec.abortBefore) controller.abort();
    const timer = spec.abortAfterMs ? setTimeout(() => controller.abort(), spec.abortAfterMs) : undefined;
    const agentModel = { provider: 'p', model: 'm' };
    let result;
    if (spec.consumer === 'title') result = await createNativeSessionTitleProvider({ modelRuntime: runtime, agentModel, systemLanguage: 'zh-CN' }).generate({ text: '修复登录按钮', sessionId: 's', turnId: 't', signal: controller.signal });
    else if (spec.consumer === 'suggestion') result = await createPromptSuggestionGenerator({ modelRuntime: runtime, agentModel })({ userPrompt: '修复登录按钮', assistantResponse: '已完成修复', sessionId: 's', turnId: 't', signal: controller.signal });
    else if (spec.consumer === 'judge') result = await classifyAndRoute({ judgeRuntime: runtime, messages: request.messages, config: { enabled: true, judge: { id: 'p/m', ...agentModel }, defaultTier: 'medium', judgeTimeoutMs: 1000, tiers: { simple: { model: { id: 'p/simple', provider: 'p', model: 'simple' } }, medium: { model: { id: 'p/m', ...agentModel } } } } });
    else { try { await runtime.complete({ ...request, ...spec.request }, { signal: controller.signal, streamTimeoutMs: spec.streamTimeoutMs }); } catch {} }
    clearTimeout(timer);
    if (spec.replies.some((reply: any) => reply.end === 'hold')) {
      const deadline = Date.now() + 2000;
      while (active.requests.some((request: any) => !request.clientClosed) && Date.now() < deadline) await new Promise(done => setTimeout(done, 5));
      assert.ok(active.requests.every((request: any) => request.clientClosed === true), `${spec.name}: cancellation closes held provider connection`);
    }
    assert.equal(active.requests.length, spec.replies.length, `${spec.name}: exact HTTP requests; ${JSON.stringify(calls)}`);
    if (spec.name === 'title-success') assert.equal(result, '修复登录按钮');
    if (spec.name === 'suggestion-success') assert.equal(result, '补充边界测试');
    if (spec.name === 'judge-success') assert.equal(result?.resolvedFrom, 'judge');
    if (spec.name === 'judge-provider-error') assert.equal(result?.failure?.attempts, 1);
    if (spec.name === 'judge-empty-retries') assert.equal(result?.failure?.attempts, 3);
    cases.push({ name: spec.name, provider: spec.provider, replies: spec.replies, abortBefore: spec.abortBefore, abortAfterMs: spec.abortAfterMs, streamTimeoutMs: spec.streamTimeoutMs, consumer: spec.consumer, result, calls, requests: active.requests });
  }
} finally { server.closeAllConnections(); await new Promise<void>(done => server.close(() => done())); }
const oracle = { sourceCommit, sourceSha256, node: process.version, locale: new Intl.Collator().resolvedOptions().locale, transportHeaders, provider, cases };
writeFileSync(join(output, 'oracle.json'), JSON.stringify(oracle, null, 1) + '\n');
writeFileSync(join(output, 'manifest.json'), JSON.stringify({ entries, sourceCommit, execution, driverSha256: createHash('sha256').update(driverBytes).digest('hex'), sdkSha256: createHash('sha256').update(readFileSync(join(baseline, 'node_modules/@google/genai/dist/node/index.mjs'))).digest('hex'), cases: cases.length, requests: cases.reduce((sum, value) => sum + value.requests.length, 0) }, null, 2) + '\n');
console.log(JSON.stringify({ output, cases: cases.length }));
