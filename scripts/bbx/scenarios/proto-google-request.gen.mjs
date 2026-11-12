// 生成 proto-google-request.json。用法（仓库根目录）：
//   node scripts/bbx/scenarios/proto-google-request.gen.mjs scripts/bbx/scenarios/proto-google-request.json
// 改场景改这里，再重新生成；生成的 JSON 一并提交。环境变量 ONLY=k1,k5 只生成这几步（排查用）。
// 不放进场景的：推理被模型关闭、非 gemini-3 模型选了推理强度（unsupported_thinking）。基线在 model_stream 的
// ndjson 已经回了 200 头之后才抛错，连接被直接断开（见 proto-anthropic-request.gen.mjs 开头的说明）。
import fs from 'node:fs';
const principal = { pilotDeckUserId: 'u', tenantId: 't', actorUserId: 'a', agentId: 'g' };
const call = (id, modelId, request) => ({
  http: 'POST /api/module-host/call', headers: { authorization: 'Bearer {TOKEN}' },
  body: { principal, operation: 'model_stream', input: { requestId: id, modelId, request } },
});
const user = (text) => ({ role: 'user', content: [{ type: 'text', text }] });
const tool = (name) => ({ name, description: `Tool ${name}`, inputSchema: { type: 'object', properties: { q: { type: 'string' }, n: { type: 'integer' }, mode: { anyOf: [{ const: 'a' }, { const: 'b' }] } }, required: ['q'], additionalProperties: false } });
const PNG = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==';
const PDF = 'JVBERi0xLjQKJcOkw7zDtsOfCjEgMCBvYmoKPDw+PgplbmRvYmoKdHJhaWxlcgo8PD4+CiUlRU9GCg==';
const think = (mode) => (mode ? { enabled: true, mode } : { enabled: true });
const steps = [
  // 结构化输出（responseMimeType / responseJsonSchema）、工具选择（functionCallingConfig）、工具 schema 清洗。
  call('k1', 'gem/gemini-bbx', { messages: [user('k1: structured')], outputSchema: { name: 'answer', description: 'The answer.', schema: { type: 'object', properties: { value: { type: 'number' }, tags: { type: 'array', items: { type: 'string' } } }, required: ['value'], additionalProperties: false } } }),
  call('k2', 'gem/gemini-bbx', { messages: [user('k2: tools')], tools: [tool('lookup'), tool('fetch')], toolChoice: 'required', metadata: { user_id: 'u7' } }),
  call('k3', 'gem/gemini-bbx', { messages: [user('k3: none')], tools: [tool('lookup')], toolChoice: 'none' }),
  call('k4', 'gem/gemini-bbx', { messages: [user('k4: named')], tools: [tool('lookup'), tool('fetch')], toolChoice: { type: 'tool', name: 'fetch' } }),
  call('k5', 'gem/gemini-bbx', { messages: [user('k5: auto')], tools: [tool('lookup')], toolChoice: 'auto' }),
  // 历史：systemInstruction、maxOutputTokens、图片（inlineData / fileData）、PDF、助手 thinking（thought + thoughtSignature）、
  // 工具调用（id 清洗、非对象参数包成 {value}）、结果（functionResponse 按 id 找名字、出错→error、结果里的图片→parts）、
  // 同角色合并。
  call('k6', 'gem/gemini-bbx', {
    systemPrompt: 'be brief', maxOutputTokens: 123,
    tools: [tool('lookup')],
    messages: [
      { role: 'user', content: [{ type: 'text', text: 'k6: look' }, { type: 'image', source: 'base64', mimeType: 'image/png', data: PNG }, { type: 'image', source: 'url', mimeType: 'image/png', data: 'https://example.invalid/a.png' }, { type: 'pdf', source: 'base64', mimeType: 'application/pdf', data: PDF }] },
      { role: 'assistant', content: [{ type: 'thinking', text: 'first thought', signature: 'sig-1' }, { type: 'text', text: 'calling' }, { type: 'tool_call', id: 'call 1/x', name: 'lookup', input: { q: 'x' } }, { type: 'tool_call', id: 'call_2', name: 'lookup', input: 'raw string' }] },
      { role: 'user', content: [
        { type: 'tool_result', toolCallId: 'call 1/x', isError: false, content: [{ type: 'text', text: 'found' }, { type: 'image', source: 'base64', mimeType: 'image/png', data: PNG }] },
        { type: 'tool_result', toolCallId: 'call_2', isError: true, content: [{ type: 'text', text: 'boom' }] },
      ] },
      user('k6: and'),
      { role: 'assistant', content: [{ type: 'thinking', text: 'only thinking' }] },
      user('k6: again'),
    ],
  }),
  // 第一条是助手消息时补一条用户消息；空文本被丢掉。
  call('k7', 'gem/gemini-bbx', { messages: [{ role: 'assistant', content: [{ type: 'text', text: 'k7: I spoke first' }] }, { role: 'user', content: [{ type: 'text', text: '' }, { type: 'text', text: 'k7: then you' }] }] }),
  // 推理（thinkingConfig）：gemini-3 模型 + 强度；只 enabled 不选强度；model id 别名（gemini-3-pro → gemini-3-pro-preview）。
  call('k8', 'gem/gemini-3-pro', { messages: [user('k8: think high')], thinking: think('high') }),
  call('k9', 'gem/gemini-3-pro', { messages: [user('k9: think default')], thinking: think() }),
  call('k10', 'gem/gemini-bbx-think', { messages: [user('k10: not gemini 3')], thinking: think() }),
  // 第二个 provider：url 带 v1beta、自定义头、extraBody（generationConfig 深合并，temperature 被剔除）、timeoutMs（流式不带）。
  call('k11', 'gem2/gemini-bbx', { messages: [user('k11: custom provider')] }),
  // 流解析：thought + 分块文本 + 不带 id 的工具调用、带 id 的工具调用（id 清洗、重复去重）、用量细项、各 finishReason、HTTP 错误。
  call('k12', 'gem/gemini-bbx', { messages: [user('k12: reasoning and tool')], tools: [tool('lookup')] }),
  call('k13', 'gem/gemini-bbx', { messages: [user('k13: ids')], tools: [tool('lookup')] }),
  call('k14', 'gem/gemini-bbx', { messages: [user('k14: cached usage')] }),
  call('k15', 'gem/gemini-bbx', { messages: [user('k15: max tokens')] }),
  call('k16', 'gem/gemini-bbx', { messages: [user('k16: recitation')] }),
  call('k17', 'gem/gemini-bbx', { messages: [user('k17: odd finish')] }),
  call('k18', 'gem/gemini-bbx', { messages: [user('k18: http 400')] }),
  call('k19', 'gem/gemini-bbx', { messages: [user('k19: http 503')] }),
];
const model = (extra = '') => `          capabilities:
            supportsToolUse: true
            maxContextTokens: 65536
            maxOutputTokens: 8192${extra}`;
const thinking = (state, efforts) => `
          thinking:
            state: ${state}${efforts ? `\n            efforts: [${efforts}]` : ''}`;
const config = `schemaVersion: 1
agent:
  model: gem/gemini-bbx
  maxContextTokens: 65536
  maxOutputTokens: 8192
model:
  providers:
    gem:
      protocol: google
      url: {MOCK}
      apiKey: bbx-google-key
      retry:
        baseDelayMs: 1
        maxDelayMs: 1
        jitter: 0
      models:
        gemini-bbx:
          multimodal:
            input: [text, image, pdf]
${model()}
        gemini-3-pro:${thinking('enabled', 'low, high')}
${model()}
        gemini-bbx-think:${thinking('enabled', 'low')}
${model()}
    gem2:
      protocol: google
      url: {MOCK}/v1beta
      apiKey: bbx-google-key-2
      timeoutMs: 45000
      headers:
        X-Bbx-Trace: trace-1
        Content-Type: application/json; charset=utf-8
      extraBody:
        temperature: 0.3
        generationConfig:
          temperature: 0.5
          topK: 7
        safetySettings:
          - category: HARM_CATEGORY_HARASSMENT
            threshold: BLOCK_NONE
      models:
        gemini-bbx:
${model()}
telemetry:
  enabled: false
`;
const scenario = {
  name: 'proto-google-request',
  about: '用公共 host 的 model_stream 直接驱动 Google Gemini 请求组装（model/providers/google/request.ts、schema.ts，经 @google/genai 2.10.0 的 generateContentParametersToMldev 落到线上）与流解析（stream.ts）。k1 结构化输出→generationConfig.responseMimeType / responseJsonSchema；k2–k5 toolConfig.functionCallingConfig（ANY / NONE / ANY+allowedFunctionNames / AUTO）与工具 schema 清洗（parametersJsonSchema）；k6 systemInstruction、maxOutputTokens、inlineData / fileData、PDF、thought + thoughtSignature、functionCall（id 清洗、非对象参数→{value}）、functionResponse（按 id 找名字、出错→error、结果里的图片→parts）、同角色合并；k7 第一条是助手消息时补用户消息、空文本丢弃；k8 gemini-3 thinkingLevel high（模型名别名 gemini-3-pro→gemini-3-pro-preview）；k9 只 includeThoughts；k10 非 gemini-3 不带 thinkingConfig；k11 url 带 v1beta、自定义头（与 SDK 头的合并顺序）、extraBody 深合并并剔除 temperature、流式不带 X-Server-Timeout；k12 流里的 thought、分块文本、不带 id 的工具调用（按 responseId 合成）；k13 带 id 的工具调用（清洗、重复去重）；k14 用量带 cachedContentTokenCount / thoughtsTokenCount；k15–k17 finishReason MAX_TOKENS / RECITATION / 未知；k18 HTTP 400；k19 HTTP 503（重试后出错）。',
  config,
  provider: [
    { name: 'k1', when: ['k1: structured'], reply: { text: '{"value":42,"tags":["a"]}' } },
    { name: 'k12', when: ['k12: reasoning and tool'], reply: { reasoning: 'chain of thought', chunks: ['look', 'ing'], text: 'looking', tool: { name: 'lookup', arguments: { q: 'z' } } } },
    { name: 'k13', when: ['k13: ids'], reply: { tools: [{ id: 'fc 1/a', name: 'lookup', arguments: { q: 'a' } }, { id: 'fc_1_a', name: 'lookup', arguments: { q: 'b' } }, { name: 'lookup', arguments: { q: 'c' } }] } },
    { name: 'k14', when: ['k14: cached usage'], reply: { text: 'cached', usage: { prompt_tokens: 100, completion_tokens: 7, total_tokens: 110, prompt_tokens_details: { cached_tokens: 64 }, completion_tokens_details: { reasoning_tokens: 3 } } } },
    { name: 'k15', when: ['k15: max tokens'], reply: { text: 'partial', finish: 'MAX_TOKENS' } },
    { name: 'k16', when: ['k16: recitation'], reply: { text: 'quoted', finish: 'RECITATION' } },
    { name: 'k17', when: ['k17: odd finish'], reply: { text: 'odd', finish: 'FINISH_REASON_UNSPECIFIED' } },
    { name: 'k18', when: ['k18: http 400'], reply: { status: 400, body: { error: { code: 400, message: 'Invalid value at \'generation_config\'.', status: 'INVALID_ARGUMENT', details: [{ '@type': 'type.googleapis.com/google.rpc.BadRequest', fieldViolations: [{ field: 'generation_config', description: 'bad' }] }] } } } },
    { name: 'k19', when: ['k19: http 503'], reply: { status: 503, body: { error: { code: 503, message: 'The model is overloaded. Please try again later.', status: 'UNAVAILABLE' } } } },
    { name: 'other', reply: { text: 'ok' } },
  ],
  steps: process.env.ONLY ? steps.filter((s) => process.env.ONLY.split(',').includes(s.body.input.requestId)) : steps,
  volatile: ['\\.http\\.headers\\.date$'],
};
fs.writeFileSync(process.argv[2], JSON.stringify(scenario, null, 1) + '\n');
