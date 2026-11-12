// 生成 proto-openai-request.json。用法（仓库根目录）：
//   node scripts/bbx/scenarios/proto-openai-request.gen.mjs scripts/bbx/scenarios/proto-openai-request.json
// 改场景改这里，再重新生成；生成的 JSON 一并提交。环境变量 ONLY=o1,o5 只生成这几步（排查用）。
import fs from 'node:fs';
const principal = { pilotDeckUserId: 'u', tenantId: 't', actorUserId: 'a', agentId: 'g' };
const call = (id, modelId, request) => ({
  http: 'POST /api/module-host/call', headers: { authorization: 'Bearer {TOKEN}' },
  body: { principal, operation: 'model_stream', input: { requestId: id, modelId, request } },
});
const user = (text) => ({ role: 'user', content: [{ type: 'text', text }] });
const tool = (name) => ({ name, description: `Tool ${name}`, inputSchema: { type: 'object', properties: { q: { type: 'string' }, n: { type: 'integer' } }, required: ['q'] } });
const PNG = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==';
const think = (mode) => (mode ? { enabled: true, mode } : { enabled: true });
const steps = [
  // 结构化输出、工具选择、metadata 的字符串化。
  call('o1', 'oa/gpt-bbx', { messages: [user('o1: structured')], outputSchema: { name: 'answer', description: 'The answer.', schema: { type: 'object', properties: { value: { type: 'number' }, tags: { type: 'array', items: { type: 'string' } } }, required: ['value'] } } }),
  call('o2', 'oa/gpt-bbx', { messages: [user('o2: tools')], tools: [tool('lookup'), tool('fetch')], toolChoice: 'required', metadata: { user_id: 'u7', n: 3, flag: true } }),
  call('o3', 'oa/gpt-bbx', { messages: [user('o3: none')], tools: [tool('lookup')], toolChoice: 'none', outputSchema: { name: 'loose', strict: false, schema: { type: 'object' } } }),
  call('o4', 'oa/gpt-bbx', { messages: [user('o4: named')], tools: [tool('lookup'), tool('fetch')], toolChoice: { type: 'tool', name: 'fetch' } }),
  // 历史：系统提示、图片、助手的 thinking（→ reasoning_content）、工具调用与结果、结果里的图片。
  call('o5', 'oa/gpt-bbx', {
    systemPrompt: 'be brief', maxOutputTokens: 123,
    tools: [tool('lookup')],
    messages: [
      { role: 'user', content: [{ type: 'text', text: 'o5: look' }, { type: 'image', source: 'base64', mimeType: 'image/png', data: PNG }] },
      { role: 'assistant', content: [{ type: 'thinking', text: 'first thought' }, { type: 'thinking', text: 'second thought' }, { type: 'tool_call', id: 'call_1', name: 'lookup', input: { q: 'x' } }] },
      { role: 'user', content: [{ type: 'tool_result', toolCallId: 'call_1', isError: false, content: [{ type: 'text', text: 'found' }, { type: 'image', source: 'base64', mimeType: 'image/png', data: PNG }] }] },
      { role: 'assistant', content: [{ type: 'thinking', text: 'only thinking' }] },
      user('o5: again'),
    ],
  }),
  // 推理字段：各格式（model/thinking/registry.ts）。
  call('o6', 'oa/gpt-think', { messages: [user('o6: openai high')], thinking: think('high') }),
  call('o7', 'oa/gpt-off', { messages: [user('o7: openai off')], thinking: think('low') }),
  call('o8', 'dashscope/qwen-think', { messages: [user('o8: qwen cloud')], thinking: think('low') }),
  call('o9', 'oa/qwen-local', { messages: [user('o9: qwen local')], thinking: think('high') }),
  call('o10', 'deepseek/ds-think', { messages: [user('o10: thinking type')], thinking: think() }),
  call('o11', 'openrouter/or-think', { messages: [user('o11: openrouter')], thinking: think('low') }),
  call('o12', 'moonshot/kimi-k3-bbx', { messages: [user('o12: always on')], thinking: think('high') }),
  call('o13', 'ollama/llama-bbx', { messages: [user('o13: server default')], thinking: think() }),
  // 快速模式 → service_tier。
  call('o14', 'oa/gpt-fast', { messages: [user('o14: fast')], speed: 0.8 }),
  call('o15', 'oa/gpt-fast', { messages: [user('o15: slow')], speed: 0.1 }),
  // 流解析：reasoning_content、usage 的各种形状、finish_reason。
  call('o16', 'oa/gpt-bbx', { messages: [user('o16: reasoning stream')] }),
  call('o17', 'oa/gpt-bbx', { messages: [user('o17: cached usage')] }),
  call('o18', 'oa/gpt-bbx', { messages: [user('o18: content filter')] }),
  call('o19', 'oa/gpt-bbx', { messages: [user('o19: odd finish')] }),
];
const model = (extra = '') => `          capabilities:
            supportsToolUse: true
            maxContextTokens: 65536
            maxOutputTokens: 8192${extra}`;
const thinking = (state, efforts, format) => `
          thinking:
            state: ${state}${efforts ? `\n            efforts: [${efforts}]` : ''}${format ? `\n            format: ${format}` : ''}`;
const provider = (id, models) => `    ${id}:
      protocol: openai
      url: {MOCK}/v1
      apiKey: bbx-${id}-key
      models:
${Object.entries(models).map(([name, body]) => `        ${name}:${body}\n${model()}`).join('\n')}`;
const config = `schemaVersion: 1
agent:
  model: oa/gpt-bbx
  maxContextTokens: 65536
  maxOutputTokens: 8192
model:
  providers:
    oa:
      protocol: openai
      url: {MOCK}/v1
      apiKey: bbx-oa-key
      speedMapping: openai_service_tier
      models:
        gpt-bbx:
          multimodal:
            input: [text, image, pdf]
${model()}
        gpt-think:${thinking('enabled', 'low, high')}
${model()}
        gpt-off:${thinking('disabled')}
${model()}
        qwen-local:${thinking('enabled', 'high', 'qwen-local')}
${model()}
        gpt-fast:
${model('\n            supportsSpeed: true')}
${provider('dashscope', { 'qwen-think': thinking('enabled', 'low') })}
${provider('deepseek', { 'ds-think': thinking('enabled') })}
${provider('openrouter', { 'or-think': thinking('enabled', 'low') })}
${provider('moonshot', { 'kimi-k3-bbx': thinking('enabled', 'high') })}
${provider('ollama', { 'llama-bbx': thinking('enabled') })}
telemetry:
  enabled: false
`;
const scenario = {
  name: 'proto-openai-request',
  about: '用公共 host 的 model_stream 直接驱动 OpenAI chat completions 请求组装（model/providers/openai/request.ts）与流解析（stream.ts）。o1 结构化输出→response_format（strict 缺省 true，schema 经 normalizeOpenAISchema）；o2 tool_choice required、metadata 值转字符串；o3 none + strict:false；o4 指定函数；o5 系统提示、max_tokens、用户图片、助手 thinking→reasoning_content、工具调用与结果、结果里的图片另起一条用户消息、只有 thinking 的助手消息；o6–o13 推理字段的各格式（model/thinking/registry.ts：openai reasoning_effort、关闭→none、qwen-cloud enable_thinking、qwen-local chat_template_kwargs、thinking-type、openrouter reasoning、moonshot kimi-k3 只发 reasoning_effort、ollama server-default）；o14/o15 speedMapping openai_service_tier→service_tier priority；o16 流里的 reasoning_content；o17 usage 带缓存命中；o18 finish_reason content_filter；o19 未知 finish_reason。已知不稳：响应头 date（与 module-host-model 相同，需要 volatile \\.http\\.headers\\.date$，本场景未加）。',
  config,
  provider: [
    { name: 'o1', when: ['o1: structured'], reply: { text: '{"value":42,"tags":["a"]}' } },
    { name: 'o16', when: ['o16: reasoning stream'], reply: { reasoning: 'chain of thought', chunks: ['an', 'swer'] } },
    { name: 'o17', when: ['o17: cached usage'], reply: { text: 'cached', usage: { prompt_tokens: 100, completion_tokens: 7, total_tokens: 107, prompt_tokens_details: { cached_tokens: 64 }, completion_tokens_details: { reasoning_tokens: 3 } } } },
    { name: 'o18', when: ['o18: content filter'], reply: { text: 'partial', finish: 'content_filter' } },
    { name: 'o19', when: ['o19: odd finish'], reply: { text: 'odd', finish: 'weird_reason' } },
    { name: 'other', reply: { text: 'ok' } },
  ],
  steps: process.env.ONLY ? steps.filter((s) => process.env.ONLY.split(',').includes(s.body.input.requestId)) : steps,
};
fs.writeFileSync(process.argv[2], JSON.stringify(scenario, null, 1) + '\n');
