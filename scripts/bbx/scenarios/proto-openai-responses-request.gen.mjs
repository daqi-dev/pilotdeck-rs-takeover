// 生成 proto-openai-responses-request.json。用法（仓库根目录）：
//   node scripts/bbx/scenarios/proto-openai-responses-request.gen.mjs scripts/bbx/scenarios/proto-openai-responses-request.json
// 改场景改这里，再重新生成；生成的 JSON 一并提交。环境变量 ONLY=q1,q5 只生成这几步（排查用）。
import fs from 'node:fs';
const principal = { pilotDeckUserId: 'u', tenantId: 't', actorUserId: 'a', agentId: 'g' };
const call = (id, modelId, request) => ({
  http: 'POST /api/module-host/call', headers: { authorization: 'Bearer {TOKEN}' },
  body: { principal, operation: 'model_stream', input: { requestId: id, modelId, request } },
});
const user = (text) => ({ role: 'user', content: [{ type: 'text', text }] });
const tool = (name) => ({ name, description: `Tool ${name}`, inputSchema: { type: 'object', properties: { q: { type: 'string' }, n: { type: 'integer' } }, required: ['q'] } });
const PNG = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==';
const PDF = 'JVBERi0xLjQKJcOkw7zDtsOfCjEgMCBvYmoKPDw+PgplbmRvYmoKdHJhaWxlcgo8PD4+CiUlRU9GCg==';
const think = (mode) => (mode ? { enabled: true, mode } : { enabled: true });
const steps = [
  // 结构化输出（text.format）、工具选择、metadata 的字符串化。
  call('q1', 'resp/gpt-bbx', { messages: [user('q1: structured')], outputSchema: { name: 'answer', description: 'The answer.', schema: { type: 'object', properties: { value: { type: 'number' }, tags: { type: 'array', items: { type: 'string' } } }, required: ['value'] } } }),
  call('q2', 'resp/gpt-bbx', { messages: [user('q2: tools')], tools: [tool('lookup'), tool('fetch')], toolChoice: 'required', metadata: { user_id: 'u7', n: 3, flag: true } }),
  call('q3', 'resp/gpt-bbx', { messages: [user('q3: none')], tools: [tool('lookup')], toolChoice: 'none', outputSchema: { name: 'loose', strict: false, schema: { type: 'object' } } }),
  call('q4', 'resp/gpt-bbx', { messages: [user('q4: named')], tools: [tool('lookup'), tool('fetch')], toolChoice: { type: 'tool', name: 'fetch' } }),
  // 历史：instructions、max_output_tokens、图片（base64 / url）、PDF→input_file、助手 thinking→input_text、
  // 工具调用→function_call、结果→function_call_output（结果里的图片另起一条用户消息）、出错的结果、只有 thinking 的助手消息。
  call('q5', 'resp/gpt-bbx', {
    systemPrompt: 'be brief', maxOutputTokens: 123,
    tools: [tool('lookup')],
    messages: [
      { role: 'user', content: [{ type: 'text', text: 'q5: look' }, { type: 'image', source: 'base64', mimeType: 'image/png', data: PNG }, { type: 'image', source: 'url', mimeType: 'image/png', data: 'https://example.invalid/a.png' }, { type: 'pdf', source: 'base64', mimeType: 'application/pdf', data: PDF }] },
      { role: 'assistant', content: [{ type: 'thinking', text: 'first thought' }, { type: 'text', text: 'calling' }, { type: 'tool_call', id: 'call_1', name: 'lookup', input: { q: 'x' } }, { type: 'tool_call', id: 'call_2', name: 'lookup', input: { q: 'y' } }] },
      { role: 'user', content: [
        { type: 'tool_result', toolCallId: 'call_1', isError: false, content: [{ type: 'text', text: 'found' }, { type: 'image', source: 'base64', mimeType: 'image/png', data: PNG }] },
        { type: 'tool_result', toolCallId: 'call_2', isError: true, content: [{ type: 'text', text: 'boom' }] },
      ] },
      { role: 'assistant', content: [{ type: 'thinking', text: 'only thinking' }] },
      user('q5: again'),
    ],
  }),
  // 推理字段（model/thinking/registry.ts：openai-responses 走 openai 格式 → reasoning.effort）。
  call('q6', 'resp/gpt-think', { messages: [user('q6: think high')], thinking: think('high') }),
  call('q7', 'resp/gpt-off', { messages: [user('q7: think off')], thinking: think('low') }),
  call('q8', 'resp/gpt-think', { messages: [user('q8: think default')], thinking: think() }),
  call('q9', 'resp/gpt-server', { messages: [user('q9: server default')], thinking: think() }),
  // 快速模式 → service_tier。
  call('q10', 'resp/gpt-fast', { messages: [user('q10: fast')], speed: 0.8 }),
  call('q11', 'resp/gpt-fast', { messages: [user('q11: slow')], speed: 0.1 }),
  // 流解析：推理摘要、分块文本加工具调用、用量细项、incomplete、failed、HTTP 错误、需要修复 / 无法修复的工具参数。
  call('q12', 'resp/gpt-bbx', { messages: [user('q12: reasoning and tool')], tools: [tool('lookup')] }),
  call('q13', 'resp/gpt-bbx', { messages: [user('q13: cached usage')] }),
  call('q14', 'resp/gpt-bbx', { messages: [user('q14: incomplete')] }),
  call('q15', 'resp/gpt-bbx', { messages: [user('q15: failed')] }),
  call('q16', 'resp/gpt-bbx', { messages: [user('q16: http 400')] }),
  call('q17', 'resp/gpt-bbx', { messages: [user('q17: repair args')], tools: [tool('lookup')] }),
  call('q18', 'resp/gpt-bbx', { messages: [user('q18: broken args')], tools: [tool('lookup')] }),
];
const model = (extra = '') => `          capabilities:
            supportsToolUse: true
            maxContextTokens: 65536
            maxOutputTokens: 8192${extra}`;
const thinking = (state, efforts, format) => `
          thinking:
            state: ${state}${efforts ? `\n            efforts: [${efforts}]` : ''}${format ? `\n            format: ${format}` : ''}`;
const config = `schemaVersion: 1
agent:
  model: resp/gpt-bbx
  maxContextTokens: 65536
  maxOutputTokens: 8192
model:
  providers:
    resp:
      protocol: openai-responses
      url: {MOCK}
      apiKey: bbx-responses-key
      speedMapping: openai_service_tier
      retry:
        baseDelayMs: 1
        maxDelayMs: 1
        jitter: 0
      models:
        gpt-bbx:
          multimodal:
            input: [text, image, pdf]
${model()}
        gpt-think:${thinking('enabled', 'low, high')}
${model()}
        gpt-off:${thinking('disabled')}
${model()}
        gpt-server:${thinking('enabled', '', 'server-default')}
${model()}
        gpt-fast:
${model('\n            supportsSpeed: true')}
telemetry:
  enabled: false
`;
const scenario = {
  name: 'proto-openai-responses-request',
  about: '用公共 host 的 model_stream 直接驱动 OpenAI Responses 请求组装（model/providers/openai-responses/request.ts）与流解析（stream.ts）。q1 结构化输出→text.format（strict 缺省 true，schema 经 normalizeOpenAISchema）；q2 tool_choice required、工具 strict:true、metadata 值转字符串；q3 none + strict:false；q4 指定函数→{type:function,name}；q5 instructions、max_output_tokens、input_image（base64 / url）、PDF→input_file、助手 thinking→input_text、function_call / function_call_output、结果里的图片另起一条用户消息、出错的结果、只有 thinking 的助手消息；q6 reasoning.effort high；q7 关闭→effort none；q8 enabled 但不选强度→不带 reasoning；q9 server-default；q10/q11 speedMapping openai_service_tier→service_tier priority；q12 流里的推理摘要、分块文本与工具调用；q13 用量带 cached_tokens / reasoning_tokens；q14 status incomplete；q15 response.failed；q16 HTTP 400（端点回退到不带 /v1 的路径）；q17 工具参数需要 jsonrepair；q18 工具参数无法修复。',
  config,
  provider: [
    { name: 'q1', when: ['q1: structured'], reply: { text: '{"value":42,"tags":["a"]}' } },
    { name: 'q12', when: ['q12: reasoning and tool'], reply: { reasoning: 'chain of thought', chunks: ['look', 'ing'], text: 'looking', tool: { id: 'call_q12', name: 'lookup', arguments: { q: 'z' } } } },
    { name: 'q13', when: ['q13: cached usage'], reply: { text: 'cached', usage: { prompt_tokens: 100, completion_tokens: 7, total_tokens: 107, prompt_tokens_details: { cached_tokens: 64 }, completion_tokens_details: { reasoning_tokens: 3 } } } },
    { name: 'q14', when: ['q14: incomplete'], reply: { text: 'partial', finish: 'incomplete' } },
    { name: 'q15', when: ['q15: failed'], reply: { text: 'oops', finish: 'failed', error: { code: 'rate_limit_exceeded', message: 'Failed inside the stream.' } } },
    { name: 'q16', when: ['q16: http 400'], reply: { status: 400, body: { error: { message: 'Unsupported parameter: reasoning.', type: 'invalid_request_error', param: 'reasoning', code: 'unsupported_parameter' } } } },
    { name: 'q17', when: ['q17: repair args'], reply: { tool: { id: 'call_q17', name: 'lookup', rawArguments: '{"q": "x",' } } },
    { name: 'q18', when: ['q18: broken args'], reply: { tool: { id: 'call_q18', name: 'lookup', rawArguments: '}}}not json' } } },
    { name: 'other', reply: { text: 'ok' } },
  ],
  steps: process.env.ONLY ? steps.filter((s) => process.env.ONLY.split(',').includes(s.body.input.requestId)) : steps,
  volatile: ['\\.http\\.headers\\.date$'],
};
fs.writeFileSync(process.argv[2], JSON.stringify(scenario, null, 1) + '\n');
