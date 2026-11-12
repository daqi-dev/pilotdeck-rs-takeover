// 用基线 dist 里的 buildOpenAIResponsesRequest 生成「规范请求 → 请求体」的期望值。不发网络请求。
// 用法：HOME=<隔离目录> node gen-responses-oracle.mjs <PilotDeck 基线目录> <输出.json>
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const [baseline, out] = process.argv.slice(2);
const mod = await import(pathToFileURL(path.join(baseline, 'dist/src/model/providers/openai-responses/request.js')).href);
const model = (extra = {}) => ({ id: 'gpt-r', capabilities: { maxOutputTokens: 4096, ...extra.capabilities }, ...(extra.thinking ? { thinking: extra.thinking } : {}) });
const provider = (extra = {}) => ({ id: 'oa', protocol: 'openai-responses', url: 'http://127.0.0.1/v1', apiKey: 'k', headers: {}, models: {}, ...extra });
const PNG = 'iVBORw0KGgo=';
const user = (text) => ({ role: 'user', content: [{ type: 'text', text }] });
const cases = [
  // 字段真缺省：无 tools / metadata / systemPrompt / maxOutputTokens / toolChoice / stream。
  { name: 'bare', request: { model: 'gpt-r', messages: [user('hi')] }, model: model(), provider: provider() },
  { name: 'tool-choice-none', request: { model: 'gpt-r', messages: [user('hi')], tools: [{ name: 't', description: 'd', inputSchema: { type: 'object', properties: {} } }], toolChoice: 'none' }, model: model(), provider: provider() },
  { name: 'tool-choice-required', request: { model: 'gpt-r', messages: [user('hi')], tools: [{ name: 't', inputSchema: { type: 'object' } }], toolChoice: 'required' }, model: model(), provider: provider() },
  { name: 'tool-choice-auto', request: { model: 'gpt-r', messages: [user('hi')], tools: [{ name: 't', inputSchema: { type: 'object' } }], toolChoice: 'auto' }, model: model(), provider: provider() },
  { name: 'history', request: {
    model: 'gpt-r', stream: true, systemPrompt: 'sys', maxOutputTokens: 7,
    messages: [
      { role: 'user', content: [{ type: 'text', text: 'look' }, { type: 'image', source: 'base64', mimeType: 'image/png', data: PNG }, { type: 'image', source: 'url', mimeType: 'image/png', data: 'https://x.invalid/a.png' }] },
      { role: 'assistant', content: [{ type: 'thinking', text: 'hmm' }, { type: 'text', text: 'calling' }, { type: 'tool_call', id: 'c1', name: 't', input: { q: 1 } }, { type: 'tool_call', id: 'c2', name: 't' }] },
      { role: 'user', content: [{ type: 'tool_result', toolCallId: 'c1', isError: true, content: [{ type: 'text', text: 'boom' }, { type: 'image', source: 'base64', mimeType: 'image/png', data: PNG }] }, { type: 'tool_result', toolCallId: 'c2', isError: false, content: [{ type: 'text', text: 'ok' }] }] },
      { role: 'assistant', content: [{ type: 'text', text: 'done' }] },
      user('again'),
    ] }, model: model(), provider: provider() },
  { name: 'reasoning-effort', request: { model: 'gpt-r', messages: [user('hi')], thinking: { enabled: true, mode: 'high' } }, model: model({ thinking: { state: 'enabled', efforts: ['high'], format: 'provider' } }), provider: provider() },
  { name: 'reasoning-default', request: { model: 'gpt-r', messages: [user('hi')], thinking: { enabled: true } }, model: model({ thinking: { state: 'enabled', efforts: [], format: 'provider' } }), provider: provider() },
  { name: 'service-tier', request: { model: 'gpt-r', messages: [user('hi')], speed: 0.7 }, model: model({ capabilities: { supportsSpeed: true } }), provider: provider({ speedMapping: 'openai_service_tier' }) },
  { name: 'structured', request: { model: 'gpt-r', messages: [user('hi')], outputSchema: { name: 'ans', schema: { type: 'object', properties: { a: { type: 'string' }, b: { type: 'number' } }, required: ['a'] } } }, model: model(), provider: provider() },
];
const results = cases.map((c) => {
  try {
    return { name: c.name, request: c.request, model: c.model, provider: c.provider, body: JSON.parse(JSON.stringify(mod.buildOpenAIResponsesRequest(c.request, c.model, c.provider))) };
  } catch (error) {
    return { name: c.name, request: c.request, model: c.model, provider: c.provider, error: { name: error.name, code: error.code, message: error.message } };
  }
});
fs.writeFileSync(out, JSON.stringify({
  generator: "src/pilotdeck-gateway/tests/fixtures/responses-request/generate.mjs（见 README.md）",
  baseline: 'OpenBMB/PilotDeck@cc7c4e6 dist/src/model/providers/openai-responses/request.js',
  cases: results,
}, null, 1) + '\n');
console.log(results.map((r) => `${r.name}: ${r.error ? 'ERROR ' + r.error.message : Object.keys(r.body).join(',')}`).join('\n'));
