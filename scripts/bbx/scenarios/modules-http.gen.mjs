// 生成 modules-model-* / modules-context-* 场景。用法（仓库根目录）：node scripts/bbx/scenarios/modules-http.gen.mjs
// 改场景改这里，再重新生成；生成的 JSON 一并提交。
//
// modules.modelProvider / modules.context 绑到外部 HTTP 模块（composition/runtimePorts.ts:81-122，
// HttpModuleClient.ts，agent/modules/llm/hostModelInvokerPort.ts，agent/modules/context/hostContextRuntime.ts）。
// 假 provider 兼当模块端点：每个模块的 manifestPath / callPath 放在自己的路径前缀下（/mp/、/cx/、/sk/），
// 规则按 url 分到各自的模块；应答信封的 inReplyTo 用 save 存下的请求 messageId。
import fs from 'node:fs';
import path from 'node:path';

const here = path.dirname(new URL(import.meta.url).pathname);

const BASE = [
  'schemaVersion: 1',
  'agent:',
  '  model: bbx/deterministic',
  '  maxContextTokens: 65536',
  '  maxOutputTokens: 8192',
  'model:',
  '  providers:',
  '    bbx:',
  '      protocol: openai',
  '      url: {MOCK}/v1',
  '      apiKey: bbx-mock-only',
  '      models:',
  '        deterministic:',
  '          capabilities:',
  '            supportsToolUse: true',
  '            maxContextTokens: 65536',
  '            maxOutputTokens: 8192',
  'telemetry:',
  '  enabled: false',
  'modules:',
].join('\n');
const binding = (slot, prefix, implementationId, contract, methods, endpoint = '{MOCK}') => [
  `  ${slot}:`,
  '    enabled: true',
  `    implementationId: ${implementationId}`,
  `    contract: ${contract}`,
  '    transport: module-http-v2',
  `    endpoint: ${endpoint}`,
  `    manifestPath: /${prefix}/module-manifest`,
  `    callPath: /${prefix}/call`,
  '    methods:',
  ...methods.map((m) => `      - ${m}`),
].join('\n');
const MODEL_METHODS = ['prepare', 'stream'];
const PULL_METHODS = ['prepare', 'stream', 'stream_next', 'close_stream'];
const model = (methods = MODEL_METHODS, endpoint) => binding('modelProvider', 'mp', 'bbx.model', 'pilotdeck.model/v1', methods, endpoint);
const config = (...bindings) => `${BASE}\n${bindings.join('\n')}\n`;

const TITLE = (title) => ({ name: 'title', when: ['Generate a concise title'], reply: { text: JSON.stringify({ title }) } });
const MID = { mid: '"messageId":"(module-http-\\d+)"' };
const manifest = (prefix, implementationId, contract, methods) => ({
  name: `${prefix}-manifest`, url: `/${prefix}/module-manifest`,
  reply: { status: 200, body: { protocolVersion: '2.0', implementationId, contract, transport: 'module-http-v2', methods } },
});
const ok = (payload) => ({ kind: 'response', messageId: 'bbx-module-reply', inReplyTo: '{saved.mid}', ok: true, payload });
const failed = (code, message, extra = {}) => ({
  kind: 'response', messageId: 'bbx-module-reply', inReplyTo: '{saved.mid}', ok: false, code,
  error: { code, message, retryability: 'unsafe', ...extra },
});
// 模块的一条应答规则：url 限定在本模块的调用路径，when 是请求体里要有的文字。
const call = (prefix, name, when, body, extra = {}) => ({
  name, url: `/${prefix}/call`, when, save: MID, reply: { status: 200, body, ...extra },
});
const mpManifest = (methods = MODEL_METHODS) => manifest('mp', 'bbx.model', 'pilotdeck.model/v1', methods);
// prepare 的应答：把请求换成带记号的固定请求，之后的 stream 按记号认出是哪一次准备。
const prepared = (marker, extra = {}) => ok({
  prepared: {
    provider: 'bbx', model: 'deterministic',
    request: { provider: 'bbx', model: 'deterministic', messages: [{ role: 'user', content: [{ type: 'text', text: marker }] }] },
    ...extra,
  },
});
const textEvents = (text, usage = { inputTokens: 10, outputTokens: 3 }) => [
  { type: 'request_started', provider: 'bbx', model: 'deterministic' },
  { type: 'message_start', role: 'assistant' },
  { type: 'text_delta', text },
  { type: 'message_end', finishReason: 'stop' },
  { type: 'usage', usage },
];
const turn = (session, message, extra = {}) => ({
  ws: 'submit_turn', params: { sessionKey: session, channelKey: 'test', message }, timeout: 20000, ...extra,
});
// 先给会话起名：自动标题请求与模块请求并发，谁先到不定（标题生成是另一路异步请求）；有了名字就不再生成标题。
const rename = (session) => ({ ws: 'rename_session', params: { sessionKey: session, value: 'Named first' } });
const read = (session) => ({ ws: 'read_session_messages', params: { sessionKey: session } });

const scenarios = {};

scenarios['modules-model-external'] = {
  about: 'modules.modelProvider 绑到外部 HTTP 模块（runtimePorts.ts:99-104，hostModelInvokerPort.ts:66-216，AgentSessionRuntimeBundle.ts:184-207）：server 照常启动；主循环的每次模型调用换成向模块 prepare（带 preparationId 与执行上下文）再 stream（一次拿回全部规范事件），请求与事件照常写进会话记录；prepare 回的 maxContextTokens / maxOutputTokens 改变本轮预算；标题生成仍走 router（假 provider）。同一会话两轮：模块客户端的 messageId 接着编号，manifest 只取一次。比较发给模块的每个请求、帧与落盘。',
  config: config(model()),
  provider: [
    TITLE('Model run'),
    mpManifest(),
    call('mp', 'mp-prepare-2', ['"operation":"prepare"', 'turn-2:'], prepared('marker-two')),
    call('mp', 'mp-prepare-1', ['"operation":"prepare"', 'turn-1:'], prepared('marker-one', { maxContextTokens: 32768, maxOutputTokens: 2048 })),
    call('mp', 'mp-stream-1', ['"operation":"stream"', 'marker-one'], ok({ events: textEvents('Hello from module.') })),
    call('mp', 'mp-stream-2', ['"operation":"stream"', 'marker-two'], ok({
      events: [
        { type: 'request_started', provider: 'bbx', model: 'deterministic' },
        { type: 'message_start', role: 'assistant' },
        { type: 'thinking_delta', text: 'pondering' },
        { type: 'text_delta', text: 'Second ' },
        { type: 'text_delta', text: 'answer.' },
        { type: 'message_end', finishReason: 'stop' },
        { type: 'usage', usage: { inputTokens: 20, outputTokens: 5, cacheReadTokens: 4 } },
      ],
    })),
    { name: 'main', reply: { text: 'provider-direct' } },
  ],
  steps: [
    turn('s-m1', 'turn-1: hello'),
    { sleep: 300 },
    turn('s-m1', 'turn-2: again'),
    { sleep: 300 },
    read('s-m1'),
  ],
};

scenarios['modules-model-external-tool'] = {
  about: 'modules.modelProvider 外部模块下带工具的一轮：模块的 stream 回 tool_call_start / tool_call_delta / tool_call_end 与 finishReason tool_call，宿主照常执行工具（read_file），工具结果进下一次 prepare 的请求，第二次 stream 回文本收尾。比较两次 prepare 的请求体、工具帧与落盘。',
  config: config(model()),
  files: { 'home/.pilotdeck/seed.txt': 'seed for module\n', 'proj/seed.txt': 'seed for module\n' },
  provider: [
    TITLE('Model tool run'),
    mpManifest(),
    call('mp', 'mp-prepare-after', ['"operation":"prepare"', 'call_mt1'], prepared('marker-after')),
    call('mp', 'mp-prepare-first', ['"operation":"prepare"', 'tool-1:'], prepared('marker-first')),
    call('mp', 'mp-stream-first', ['"operation":"stream"', 'marker-first'], ok({
      events: [
        { type: 'request_started', provider: 'bbx', model: 'deterministic' },
        { type: 'message_start', role: 'assistant' },
        { type: 'text_delta', text: 'Reading.' },
        { type: 'tool_call_start', id: 'call_mt1', name: 'read_file' },
        { type: 'tool_call_delta', id: 'call_mt1', delta: '{"path":"seed.txt"}' },
        { type: 'tool_call_end', toolCall: { id: 'call_mt1', name: 'read_file', input: { path: 'seed.txt' } } },
        { type: 'message_end', finishReason: 'tool_call' },
        { type: 'usage', usage: { inputTokens: 10, outputTokens: 6 } },
      ],
    })),
    call('mp', 'mp-stream-after', ['"operation":"stream"', 'marker-after'], ok({ events: textEvents('Seed read.') })),
    { name: 'main', reply: { text: 'provider-direct' } },
  ],
  steps: [turn('s-mt', 'tool-1: read the seed'), { sleep: 300 }, read('s-mt')],
};

scenarios['modules-model-external-pull'] = {
  about: 'modules.modelProvider 外部模块声明 stream_next / close_stream 时按拉取式流（hostModelInvokerPort.ts:153-192）：每次 stream_next 带同一个 preparationId 取一批事件，done:true 为止；中途某批回错时先发 close_stream（只带 operation 与 preparationId，不带中止信号）再把错误交给本轮。比较每次 stream_next / close_stream 的请求、帧与落盘。',
  config: config(model(PULL_METHODS)),
  provider: [
    TITLE('Model pull run'),
    mpManifest(PULL_METHODS),
    call('mp', 'mp-prepare-2', ['"operation":"prepare"', 'pull-2:'], prepared('marker-p2')),
    call('mp', 'mp-prepare-1', ['"operation":"prepare"', 'pull-1:'], prepared('marker-p1')),
    call('mp', 'mp-next-1a', ['"operation":"stream_next"', 'marker-p1', '"messageId":"module-http-2"'], ok({
      events: [
        { type: 'request_started', provider: 'bbx', model: 'deterministic' },
        { type: 'message_start', role: 'assistant' },
        { type: 'text_delta', text: 'Pulled ' },
      ],
      done: false,
    })),
    call('mp', 'mp-next-1b', ['"operation":"stream_next"', 'marker-p1'], ok({
      events: [
        { type: 'text_delta', text: 'twice.' },
        { type: 'message_end', finishReason: 'stop' },
        { type: 'usage', usage: { inputTokens: 10, outputTokens: 3 } },
      ],
      done: true,
    })),
    call('mp', 'mp-next-2a', ['"operation":"stream_next"', 'marker-p2', '"messageId":"module-http-2"'], ok({
      events: [
        { type: 'request_started', provider: 'bbx', model: 'deterministic' },
        { type: 'message_start', role: 'assistant' },
        { type: 'text_delta', text: 'Half ' },
      ],
      done: false,
    })),
    call('mp', 'mp-next-2b', ['"operation":"stream_next"', 'marker-p2'], failed('STREAM_BROKEN', 'Module stream broke.')),
    call('mp', 'mp-close', ['"operation":"close_stream"'], ok({ closed: true })),
    { name: 'main', reply: { text: 'provider-direct' } },
  ],
  steps: [
    turn('s-p1', 'pull-1: stream it'),
    { sleep: 300 },
    turn('s-p2', 'pull-2: break it'),
    { sleep: 500 },
    read('s-p1'),
    read('s-p2'),
  ],
};

scenarios['modules-model-external-error'] = {
  about: 'modules.modelProvider 外部模块回错与回异常形状（hostModelInvokerPort.ts:218-288，HttpModuleClient.ts:52-60、138-160）：prepare 回 ok:false（错误码与消息原样成为本轮错误）；stream 回带 canonical 的 ok:false（按 ModelProviderError 处理）；stream 回 ok:true 但没有 events（INVALID_MODEL_RESPONSE）；prepare 回的 prepared 缺 provider（INVALID_MODEL_PREPARATION）；应答信封 inReplyTo 对不上（MODULE_PROTOCOL_INCOMPATIBLE）；stream 的事件里带 error 事件；stream 回只有 code / message 的 canonical（仍按 ModelProviderError）。每个会话一种，比较帧、发给模块的请求与落盘。',
  config: config(model()),
  provider: [
    TITLE('Model error run'),
    mpManifest(),
    call('mp', 'mp-prepare-e1', ['"operation":"prepare"', 'err-1:'], failed('MODEL_DOWN', 'Model module is down.')),
    call('mp', 'mp-prepare-e4', ['"operation":"prepare"', 'err-4:'], ok({ prepared: { model: 'deterministic', request: { provider: 'bbx', model: 'deterministic', messages: [] } } })),
    { name: 'mp-prepare-e5', url: '/mp/call', when: ['"operation":"prepare"', 'err-5:'], reply: { status: 200, body: { kind: 'response', messageId: 'bbx-module-reply', inReplyTo: 'module-http-99', ok: true, payload: {} } } },
    call('mp', 'mp-prepare-e2', ['"operation":"prepare"', 'err-2:'], prepared('marker-e2')),
    call('mp', 'mp-prepare-e3', ['"operation":"prepare"', 'err-3:'], prepared('marker-e3')),
    call('mp', 'mp-prepare-e6', ['"operation":"prepare"', 'err-6:'], prepared('marker-e6')),
    call('mp', 'mp-prepare-e7', ['"operation":"prepare"', 'err-7:'], prepared('marker-e7')),
    call('mp', 'mp-stream-e7', ['"operation":"stream"', 'marker-e7'], failed('PROVIDER_FAILED', 'Bare canonical.', { canonical: { code: 'server_error', message: 'Bare canonical from module.' } })),
    call('mp', 'mp-stream-e2', ['"operation":"stream"', 'marker-e2'], failed('PROVIDER_FAILED', 'Upstream failed.', {
      canonical: { provider: 'bbx', model: 'deterministic', protocol: 'openai', code: 'invalid_request', status: 400, message: 'Upstream rejected the request.', retryable: false },
    })),
    call('mp', 'mp-stream-e3', ['"operation":"stream"', 'marker-e3'], ok({ notEvents: true })),
    call('mp', 'mp-stream-e6', ['"operation":"stream"', 'marker-e6'], ok({
      events: [
        { type: 'request_started', provider: 'bbx', model: 'deterministic' },
        { type: 'message_start', role: 'assistant' },
        { type: 'text_delta', text: 'Partial' },
        { type: 'error', error: { code: 'invalid_request', message: 'Event error from module.', retryable: false } },
      ],
    })),
    { name: 'main', reply: { text: 'provider-direct' } },
  ],
  steps: [
    turn('s-e1', 'err-1: prepare fails'), { sleep: 300 },
    turn('s-e2', 'err-2: canonical'), { sleep: 300 },
    turn('s-e3', 'err-3: no events'), { sleep: 300 },
    turn('s-e4', 'err-4: bad prepared'), { sleep: 300 },
    turn('s-e5', 'err-5: bad envelope'), { sleep: 300 },
    turn('s-e6', 'err-6: error event'), { sleep: 300 },
    turn('s-e7', 'err-7: bare canonical'), { sleep: 300 },
    read('s-e1'), read('s-e2'), read('s-e3'), read('s-e4'), read('s-e5'), read('s-e6'), read('s-e7'),
  ],
};

scenarios['modules-model-external-unreachable'] = {
  about: 'modules.modelProvider 绑到一个没有人监听的本机端点（127.0.0.1:9）：server 照常启动（基线启动时不探测模块）；轮次里取 manifest 失败（MODULE_TRANSPORT_UNAVAILABLE，HttpModuleClient.ts:122-130），本轮失败；下一轮重新取 manifest（失败的 manifest 不缓存）。比较帧与落盘。',
  config: config(model(MODEL_METHODS, 'http://127.0.0.1:9')),
  provider: [TITLE('Model unreachable'), { name: 'main', reply: { text: 'provider-direct' } }],
  steps: [
    rename('s-u'),
    turn('s-u', 'unreach-1: hello'), { sleep: 300 },
    turn('s-u', 'unreach-2: again'), { sleep: 300 },
    read('s-u'),
  ],
};

scenarios['modules-model-external-abort'] = {
  about: 'modules.modelProvider 外部模块的请求在途时中止本轮（HttpModuleClient.ts:102-134：中止信号掐断 fetch，报 MODULE_ABORTED）：s-as 在 stream 迟迟不回时 abort_turn；s-ap 在 prepare 迟迟不回时 abort_turn。模块声明了 stream_next / close_stream：被掐断的拉取在 finally 里发 close_stream。之后同一会话再来一轮。比较帧、发给模块的请求（含被掐断的那次）与落盘。',
  config: config(model(PULL_METHODS)),
  provider: [
    TITLE('Model abort run'),
    mpManifest(PULL_METHODS),
    call('mp', 'mp-prepare-next', ['"operation":"prepare"', 'zz-again-'], prepared('marker-next')),
    call('mp', 'mp-prepare-ap', ['"operation":"prepare"', 'ab-prep:'], prepared('marker-ap'), { delayMs: 3000 }),
    call('mp', 'mp-prepare-as', ['"operation":"prepare"', 'ab-stream:'], prepared('marker-as')),
    call('mp', 'mp-next-as', ['"operation":"stream_next"', 'marker-as'], ok({ events: textEvents('late'), done: true }), { delayMs: 3000 }),
    call('mp', 'mp-next-ap', ['"operation":"stream_next"', 'marker-ap'], ok({ events: textEvents('never'), done: true })),
    call('mp', 'mp-next-next', ['"operation":"stream_next"', 'marker-next'], ok({ events: textEvents('Next answer.'), done: true })),
    call('mp', 'mp-close', ['"operation":"close_stream"'], ok({ closed: true })),
    { name: 'main', reply: { text: 'provider-direct' } },
  ],
  steps: [
    turn('s-as', 'ab-stream: go', { answer: [{ on: { type: 'model_selection_changed' }, ws: 'abort_turn', params: { sessionKey: 's-as' }, count: 1, delayMs: 300 }] }),
    { sleep: 3500 },
    turn('s-as', 'zz-again-as: again'),
    { sleep: 300 },
    turn('s-ap', 'ab-prep: go', { answer: [{ on: { type: 'model_request_started' }, ws: 'abort_turn', params: { sessionKey: 's-ap' }, count: 1, delayMs: 300 }] }),
    { sleep: 3500 },
    turn('s-ap', 'zz-again-ap: again'),
    { sleep: 300 },
    read('s-as'),
    read('s-ap'),
  ],
};

const CX_ALL = ['prepare_for_model', 'apply_tool_results', 'recover_from_model_error', 'capture_turn', 'try_auto_compact'];
const context = (methods = ['prepare_for_model']) => binding('context', 'cx', 'bbx.context', 'pilotdeck.context/v1', methods);
const cxManifest = (methods = ['prepare_for_model']) => manifest('cx', 'bbx.context', 'pilotdeck.context/v1', methods);
const modelContext = (marker, extra = {}) => ok({
  result: {
    messages: [{ role: 'user', content: [{ type: 'text', text: marker }] }],
    systemPrompt: `System from context module (${marker}).`,
    systemPromptParts: [`System from context module (${marker}).`],
    tools: [],
    diagnostics: [],
    boundaries: [],
    ...extra,
  },
});

scenarios['modules-context-external'] = {
  about: 'modules.context 绑到外部 HTTP 模块，只声明 prepare_for_model（runtimePorts.ts:115-122，hostContextRuntime.ts:34-86）：server 照常启动；每次模型请求前向模块 prepare_for_model（runId 是会话 id，operationId 是 context-<会话 id>），模块回的 messages / systemPrompt / tools 原样成为发给 provider 的请求；没声明的操作不调用。比较发给模块的请求、发给 provider 的请求、帧与落盘。',
  config: config(context()),
  provider: [
    TITLE('Context run'),
    cxManifest(),
    call('cx', 'cx-prepare-2', ['"operation":"prepare_for_model"', 'turn-2:'], modelContext('ctx-two')),
    call('cx', 'cx-prepare-1', ['"operation":"prepare_for_model"', 'turn-1:'], modelContext('ctx-one')),
    { name: 'main-2', when: ['ctx-two'], reply: { text: 'Second from provider.' } },
    { name: 'main-1', when: ['ctx-one'], reply: { text: 'First from provider.' } },
    { name: 'main', reply: { text: 'provider-direct' } },
  ],
  steps: [
    turn('s-c1', 'turn-1: hello'),
    { sleep: 300 },
    turn('s-c1', 'turn-2: again'),
    { sleep: 300 },
    read('s-c1'),
  ],
};

scenarios['modules-context-external-all'] = {
  about: 'modules.context 外部模块声明全部五个操作（hostContextRuntime.ts:67-85）：带工具的一轮里 apply_tool_results 收到工具结果（模块回的 result 作为追加的消息），轮末 capture_turn，模型请求前 try_auto_compact（带 budgetProjection），模型回错时 recover_from_model_error。比较每个操作发给模块的请求体、发给 provider 的请求、帧与落盘。',
  config: config(context(CX_ALL)),
  files: { 'home/.pilotdeck/seed.txt': 'seed for context\n', 'proj/seed.txt': 'seed for context\n' },
  provider: [
    TITLE('Context all run'),
    cxManifest(CX_ALL),
    call('cx', 'cx-compact', ['"operation":"try_auto_compact"'], ok({ result: { compacted: false } })),
    call('cx', 'cx-capture', ['"operation":"capture_turn"'], ok({ result: null })),
    call('cx', 'cx-recover', ['"operation":"recover_from_model_error"'], ok({ result: { action: 'fail' } })),
    call('cx', 'cx-apply', ['"operation":"apply_tool_results"'], ok({
      result: { messages: [{ role: 'user', content: [{ type: 'tool_result', toolCallId: 'call_cx1', content: [{ type: 'text', text: 'seed via context' }] }] }] },
    })),
    call('cx', 'cx-prepare-after', ['"operation":"prepare_for_model"', 'call_cx1'], modelContext('ctx-after')),
    call('cx', 'cx-prepare-err', ['"operation":"prepare_for_model"', 'err-1:'], modelContext('ctx-err')),
    call('cx', 'cx-prepare-tool', ['"operation":"prepare_for_model"', 'tool-1:'], modelContext('ctx-tool', {
      tools: [{ name: 'read_file', description: 'Read a file.', inputSchema: { type: 'object', properties: { path: { type: 'string' } } } }],
    })),
    { name: 'main-after', when: ['seed via context'], reply: { text: 'Done with tool.' } },
    { name: 'main-tool', when: ['ctx-tool'], reply: { tool: { id: 'call_cx1', name: 'read_file', arguments: { path: 'seed.txt' } } } },
    { name: 'main-err', when: ['ctx-err'], reply: { status: 400, body: { error: { message: 'bad request from provider', type: 'invalid_request_error' } } } },
    { name: 'main', reply: { text: 'provider-direct' } },
  ],
  steps: [
    turn('s-ca', 'tool-1: read the seed'),
    { sleep: 300 },
    turn('s-ce', 'err-1: provider fails'),
    { sleep: 300 },
    read('s-ca'),
    read('s-ce'),
  ],
};

scenarios['modules-context-external-error'] = {
  about: 'modules.context 外部模块回错与回异常形状（hostContextRuntime.ts:54-64）：prepare_for_model 回 ok:false（错误消息与码成为本轮错误）；回 ok:true 但 payload 里没有 result（Context module prepare_for_model returned no result.）；应答信封 inReplyTo 对不上（MODULE_PROTOCOL_INCOMPATIBLE）。每个会话一种，比较帧与落盘。',
  config: config(context()),
  provider: [
    TITLE('Context error run'),
    cxManifest(),
    call('cx', 'cx-prepare-e1', ['"operation":"prepare_for_model"', 'cerr-1:'], failed('CONTEXT_DOWN', 'Context module is down.')),
    call('cx', 'cx-prepare-e2', ['"operation":"prepare_for_model"', 'cerr-2:'], ok({ notResult: true })),
    { name: 'cx-prepare-e3', url: '/cx/call', when: ['"operation":"prepare_for_model"', 'cerr-3:'], reply: { status: 200, body: { kind: 'response', messageId: 'bbx-module-reply', inReplyTo: 'module-http-99', ok: true, payload: { result: null } } } },
    { name: 'main', reply: { text: 'provider-direct' } },
  ],
  steps: [
    turn('s-x1', 'cerr-1: fails'), { sleep: 300 },
    turn('s-x2', 'cerr-2: no result'), { sleep: 300 },
    turn('s-x3', 'cerr-3: bad envelope'), { sleep: 300 },
    read('s-x1'), read('s-x2'), read('s-x3'),
  ],
};

scenarios['modules-context-external-unreachable'] = {
  about: 'modules.context 绑到没有人监听的本机端点（127.0.0.1:9）：server 照常启动；轮次里取 manifest 失败（MODULE_TRANSPORT_UNAVAILABLE），本轮失败，下一轮重新取。比较帧与落盘。',
  config: config(binding('context', 'cx', 'bbx.context', 'pilotdeck.context/v1', ['prepare_for_model'], 'http://127.0.0.1:9')),
  provider: [TITLE('Context unreachable'), { name: 'main', reply: { text: 'provider-direct' } }],
  steps: [rename('s-cu'), turn('s-cu', 'cunreach-1: hello'), { sleep: 300 }, turn('s-cu', 'cunreach-2: again'), { sleep: 300 }, read('s-cu')],
};

scenarios['modules-model-context-external'] = {
  about: 'modules.modelProvider 与 modules.context 同时绑到外部 HTTP 模块：每次模型调用先向 context 模块 prepare_for_model，模块回的请求再交给 model 模块 prepare / stream；两个模块各有自己的客户端（各自取 manifest、messageId 各自编号）。带工具的一轮加一轮纯文本。比较发给两个模块的每个请求、帧与落盘。',
  config: config(model(), context()),
  files: { 'home/.pilotdeck/seed.txt': 'seed for both\n', 'proj/seed.txt': 'seed for both\n' },
  provider: [
    TITLE('Both modules'),
    mpManifest(),
    cxManifest(),
    call('cx', 'cx-prepare-after', ['"operation":"prepare_for_model"', 'call_mc1'], modelContext('ctx-after')),
    call('cx', 'cx-prepare-tool', ['"operation":"prepare_for_model"', 'both-1:'], modelContext('ctx-tool', {
      tools: [{ name: 'read_file', description: 'Read a file.', inputSchema: { type: 'object', properties: { path: { type: 'string' } } } }],
    })),
    call('mp', 'mp-prepare-after', ['"operation":"prepare"', 'ctx-after'], prepared('marker-after')),
    call('mp', 'mp-prepare-tool', ['"operation":"prepare"', 'ctx-tool'], prepared('marker-tool')),
    call('mp', 'mp-stream-tool', ['"operation":"stream"', 'marker-tool'], ok({
      events: [
        { type: 'request_started', provider: 'bbx', model: 'deterministic' },
        { type: 'message_start', role: 'assistant' },
        { type: 'tool_call_start', id: 'call_mc1', name: 'read_file' },
        { type: 'tool_call_end', toolCall: { id: 'call_mc1', name: 'read_file', input: { path: 'seed.txt' } } },
        { type: 'message_end', finishReason: 'tool_call' },
        { type: 'usage', usage: { inputTokens: 10, outputTokens: 6 } },
      ],
    })),
    call('mp', 'mp-stream-after', ['"operation":"stream"', 'marker-after'], ok({ events: textEvents('Both done.') })),
    { name: 'main', reply: { text: 'provider-direct' } },
  ],
  steps: [turn('s-mc', 'both-1: read the seed'), { sleep: 300 }, read('s-mc')],
};

const SK_METHODS = ['list', 'read'];
scenarios['modules-model-context-skills-external'] = {
  about: 'modules.modelProvider、modules.context 与 modules.skills 三个槽同时绑到外部 HTTP 模块（manifest 各在自己的路径下，按路径分）：建会话时向 skills 模块 list，context 模块的 prepare_for_model 收到的系统提示词输入里带模块给的技能列表，模型由 model 模块应答，read_skill 走 skills 模块。比较三个模块收到的请求、帧与落盘。',
  config: config(model(), context(), binding('skills', 'sk', 'bbx.skills', 'pilotdeck.skills/v1', SK_METHODS)),
  provider: [
    TITLE('Three modules'),
    mpManifest(),
    cxManifest(),
    manifest('sk', 'bbx.skills', 'pilotdeck.skills/v1', SK_METHODS),
    call('sk', 'sk-list', ['"operation":"list"'], ok({ result: { items: [{ slug: 'ext-alpha', name: 'Ext Alpha', skillFile: '/ext/alpha/SKILL.md', description: 'External alpha skill.' }] } })),
    call('sk', 'sk-read', ['"operation":"read"'], ok({ result: { content: '# alpha\n\nReply with ALPHA.' } })),
    call('cx', 'cx-prepare-after', ['"operation":"prepare_for_model"', 'call_sk1'], modelContext('ctx-after')),
    call('cx', 'cx-prepare', ['"operation":"prepare_for_model"', 'three-1:'], modelContext('ctx-first', {
      tools: [{ name: 'read_skill', description: 'Read a skill.', inputSchema: { type: 'object', properties: { skillName: { type: 'string' } } } }],
    })),
    call('mp', 'mp-prepare-after', ['"operation":"prepare"', 'ctx-after'], prepared('marker-after')),
    call('mp', 'mp-prepare', ['"operation":"prepare"', 'ctx-first'], prepared('marker-first')),
    call('mp', 'mp-stream-first', ['"operation":"stream"', 'marker-first'], ok({
      events: [
        { type: 'request_started', provider: 'bbx', model: 'deterministic' },
        { type: 'message_start', role: 'assistant' },
        { type: 'tool_call_start', id: 'call_sk1', name: 'read_skill' },
        { type: 'tool_call_end', toolCall: { id: 'call_sk1', name: 'read_skill', input: { skillName: 'ext-alpha' } } },
        { type: 'message_end', finishReason: 'tool_call' },
      ],
    })),
    call('mp', 'mp-stream-after', ['"operation":"stream"', 'marker-after'], ok({ events: textEvents('ALPHA') })),
    { name: 'main', reply: { text: 'provider-direct' } },
  ],
  steps: [rename('s-three'), turn('s-three', 'three-1: use the skill'), { sleep: 300 }, read('s-three')],
};

for (const [name, body] of Object.entries(scenarios)) {
  fs.writeFileSync(path.join(here, `${name}.json`), JSON.stringify({ name, ...body }, null, 2) + '\n');
}
console.log(`wrote ${Object.keys(scenarios).length} scenarios`);
