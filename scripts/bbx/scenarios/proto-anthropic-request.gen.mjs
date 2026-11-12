// 生成 proto-anthropic-request.json。用法（仓库根目录）：
//   node scripts/bbx/scenarios/proto-anthropic-request.gen.mjs scripts/bbx/scenarios/proto-anthropic-request.json
// 改场景改这里，再重新生成；生成的 JSON 一并提交。环境变量 ONLY=r1,r5 只生成这几步（排查用）。
// 不放进场景的一步：老的只支持预算推理的模型选了推理强度（unsupported_thinking）。基线在 model_stream 的
// ndjson 已经回了 200 头之后才抛错，连接被直接断开，采集器读响应体时抛 TypeError: terminated 整个退出，
// 记不下来（需要采集器把 http 步骤读体失败记成结果）。
import fs from 'node:fs';
const principal = { pilotDeckUserId: 'u', tenantId: 't', actorUserId: 'a', agentId: 'g' };
const call = (id, modelId, request) => ({
  http: 'POST /api/module-host/call', headers: { authorization: 'Bearer {TOKEN}' },
  body: { principal, operation: 'model_stream', input: { requestId: id, modelId, request } },
});
const user = (text) => ({ role: 'user', content: [{ type: 'text', text }] });
const tool = (name) => ({ name, description: `Tool ${name}`, inputSchema: { type: 'object', properties: { q: { type: 'string' } }, required: ['q'] } });
const PNG = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==';
const PDF = 'JVBERi0xLjQKJcOkw7zDtsOfCjEgMCBvYmoKPDw+PgplbmRvYmoKdHJhaWxlcgo8PD4+CiUlRU9GCg==';
const steps = [
  call('r1', 'anth/claude-bbx', { messages: [user('r1: structured')], outputSchema: { name: 'answer', schema: { type: 'object', properties: { value: { type: 'number' } }, required: ['value'] } } }),
  call('r2', 'anth/claude-bbx', { messages: [user('r2: loose schema')], tools: [tool('lookup'), tool('fetch')], toolChoice: 'required', outputSchema: { name: 'answer', description: 'Give the answer.', strict: false, schema: { type: 'object' } } }),
  call('r3', 'anth/claude-bbx', { messages: [user('r3: none')], tools: [tool('lookup')], toolChoice: 'none' }),
  call('r4', 'anth/claude-bbx', { messages: [user('r4: named')], tools: [tool('lookup'), tool('fetch')], toolChoice: { type: 'tool', name: 'fetch' } }),
  call('r5', 'anth/claude-bbx', {
    systemPrompt: 'be brief', maxOutputTokens: 321, metadata: { user_id: 'user-7', other: 1 },
    cacheBreakpoints: [0, 1, 2, 3],
    tools: [tool('lookup')],
    messages: [
      { role: 'user', content: [{ type: 'text', text: 'r5: look' }, { type: 'image', source: 'base64', mimeType: 'image/png', data: PNG }, { type: 'image', source: 'url', mimeType: 'image/png', data: 'https://example.invalid/a.png' }, { type: 'pdf', source: 'base64', mimeType: 'application/pdf', data: PDF }] },
      { role: 'assistant', content: [{ type: 'thinking', text: 'hmm', signature: 'sig-1' }, { type: 'text', text: 'calling' }, { type: 'tool_call', id: 'toolu_1', name: 'lookup', input: { q: 'x' } }] },
      { role: 'user', content: [{ type: 'tool_result', toolCallId: 'toolu_1', isError: true, content: [{ type: 'text', text: 'boom' }] }] },
      { role: 'assistant', content: [{ type: 'thinking', text: 'only thinking' }] },
      user('r5: again'),
    ],
  }),
  call('r6', 'anth/claude-think', { messages: [user('r6: think high')], thinking: { enabled: true, mode: 'high' } }),
  call('r7', 'anth/claude-think', { messages: [user('r7: think default')], thinking: { enabled: true } }),
  call('r8', 'anth/claude-off', { messages: [user('r8: think off')], thinking: { enabled: true, mode: 'low' } }),
  call('r10', 'anth/claude-fast', { messages: [user('r10: fast')], speed: 0.9 }),
  call('r13', 'anth/claude-fast', { messages: [user('r13: slow')], speed: 0.2 }),
  call('r11', 'anth/claude-bbx', { messages: [user('r11: stop reasons')] }),
  call('r12', 'anth/claude-bbx', { messages: [user('r12: stream error')] }),
];
const config = `schemaVersion: 1
agent:
  model: anth/claude-bbx
  maxContextTokens: 65536
  maxOutputTokens: 8192
model:
  providers:
    anth:
      protocol: anthropic
      url: {MOCK}
      apiKey: bbx-anthropic-key
      speedMapping: anthropic_speed
      retry:
        baseDelayMs: 1
        maxDelayMs: 1
        jitter: 0
      models:
        claude-bbx:
          multimodal:
            input: [text, image, pdf]
          capabilities:
            supportsToolUse: true
            maxContextTokens: 65536
            maxOutputTokens: 8192
        claude-think:
          thinking:
            state: enabled
            efforts: [low, high]
          capabilities:
            maxContextTokens: 65536
            maxOutputTokens: 8192
        claude-off:
          thinking:
            state: disabled
          capabilities:
            maxContextTokens: 65536
            maxOutputTokens: 8192
        claude-sonnet-4-20250514:
          thinking:
            state: enabled
            efforts: [low]
          capabilities:
            maxContextTokens: 65536
            maxOutputTokens: 8192
        claude-fast:
          capabilities:
            supportsSpeed: true
            maxContextTokens: 65536
            maxOutputTokens: 8192
telemetry:
  enabled: false
`;
const scenario = {
  name: 'proto-anthropic-request',
  about: '用公共 host 的 model_stream 直接驱动 Anthropic messages 请求组装（model/providers/anthropic/request.ts）与流解析（stream.ts）：r1 结构化输出合成 __output__ 工具并强制 tool_choice；r2 strict:false 时不强制、tool_choice required→any；r3 none；r4 指定工具；r5 系统提示、max_tokens、metadata.user_id、图片（base64 / url）、PDF→document、历史里的 thinking+signature、tool_use / tool_result（is_error）、只有 thinking 的助手消息、cacheBreakpoints 超过 3 个时截取最后 3 个并打 cache_control（不打在 thinking 上）；r6 adaptive 推理 + output_config.effort；r7 adaptive 不带 effort；r8 模型关闭推理→thinking disabled；r10 speedMapping anthropic_speed 且 speed≥0.5→speed fast 与 anthropic-beta 头，r13 speed<0.5 不带；r11 stop_reason max_tokens；r12 回 529（流式重试后以错误事件结束）。已知不稳：响应头 date（与 module-host-model 相同，需要 volatile \\.http\\.headers\\.date$，本场景未加）。',
  config,
  provider: [
    { name: 'r1', when: ['r1: structured'], reply: { tool: { id: 'toolu_out', name: '__output__', arguments: { value: 42 } } } },
    { name: 'r11', when: ['r11: stop reasons'], reply: { text: 'cut', finish: 'max_tokens' } },
    { name: 'r12', when: ['r12: stream error'], reply: { status: 529, body: { type: 'error', error: { type: 'overloaded_error', message: 'Overloaded' } } } },
    { name: 'r6', when: ['r6: think high'], reply: { reasoning: 'deep thought', text: 'r6 done' } },
    { name: 'other', reply: { text: 'ok' } },
  ],
  steps: process.env.ONLY ? steps.filter((s) => process.env.ONLY.split(",").includes(s.body.input.requestId)) : steps,
};
fs.writeFileSync(process.argv[2], JSON.stringify(scenario, null, 1) + '\n');
