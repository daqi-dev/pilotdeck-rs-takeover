// 生成 permission-subagent-*.json：子代理里的工具需要权限询问时。用法（仓库根目录）：
//   node scripts/bbx/scenarios/subagent-permission.gen.mjs
// 基线子代理的工具运行时用父会话的权限运行时（SubagentRuntimeComposition.ts:57-81 的
// inherited.permission），询问经父会话的应答器发到父会话的流上。
import fs from 'node:fs';
import path from 'node:path';

const here = path.dirname(new URL(import.meta.url).pathname);
const TITLE = { name: 'title', when: ['Generate a concise title'], reply: { text: '{"title": "Sub ask"}' } };
const REPORT = 'Scope: ran.\nResult: done.\nKey files: none\nFiles changed: none\nIssues: none';
const decide = (session, decision, extra = {}) => ({
  on: { type: 'permission_request' }, ws: 'permission_decide',
  params: { sessionKey: session, requestId: '{event.requestId}', decision, ...extra }, count: 1,
});
const submit = (session, message, answer, extra = {}) => ({
  ws: 'submit_turn', params: { sessionKey: session, channelKey: 'test', message, ...extra }, timeout: 20000, ...(answer ? { answer } : {}),
});
const read = (session) => ({ ws: 'read_session_messages', params: { sessionKey: session } });
const pending = (session) => ({ ws: 'permission_list', params: { sessionKey: session } });
// 子代理（标记 marker）调 bash 写一个文件，拿到结果后回报告；父轮调 agent 派它，拿到结果后收尾。
// 父轮的消息带 PARENT-<marker>，子代理的指令带 CHILD-<marker>：父轮的请求里有子代理指令（agent 调用的参数），
// 子代理的请求里没有父轮的消息，所以父轮的规则排在前面。
const oneShot = (marker, file) => [
  { name: `${marker}-parent-after`, when: [`PARENT-${marker}`, '"tool_call_id":"call_parent"'], reply: { text: `parent done ${marker}` } },
  { name: `${marker}-parent`, when: [`PARENT-${marker}`], reply: { tool: { id: 'call_parent', name: 'agent', arguments: { description: `Run ${marker}`, prompt: `CHILD-${marker}: write the file.`, subagent_type: 'general-purpose' } } } },
  { name: `${marker}-child-after`, when: [`CHILD-${marker}`, '"tool_call_id":"call_child"'], reply: { text: REPORT } },
  { name: `${marker}-child`, when: [`CHILD-${marker}`], reply: { tool: { id: 'call_child', name: 'bash', arguments: { command: `printf ${marker} > ${file}`, description: `child ${marker}` } } } },
];

const scenarios = {};

scenarios['permission-subagent-oneshot'] = {
  about: '一次性子代理（agent 工具，general-purpose）在父会话默认权限模式下调 bash：询问经父会话的应答器发到父会话的流上（permission_request 的字段、与子代理事件的先后），批准则执行（文件落盘），拒绝则工具结果为拒绝、子代理照常回报告；两个会话分别看。',
  provider: [TITLE, ...oneShot('allow', 'child-allow.txt'), ...oneShot('deny', 'child-deny.txt')],
  steps: [
    submit('s-allow', 'PARENT-allow delegate', [decide('s-allow', 'allow')]),
    { sleep: 300 },
    read('s-allow'),
    submit('s-deny', 'PARENT-deny delegate', [decide('s-deny', 'deny', { reason: 'bbx says no' })]),
    { sleep: 300 },
    read('s-deny'),
  ],
};

scenarios['permission-subagent-abort'] = {
  about: '一次性子代理的工具询问还没回答时用户中止父轮：父轮收尾、子代理以 aborted 结束，询问被撤回（之后的 permission_list 为空），子代理的工具不执行（文件不落盘）；同一会话下一轮照常。',
  provider: [
    TITLE,
    // 下一轮的请求里带着上一轮的历史，所以排在前面。
    { name: 'next', when: ['next:'], reply: { text: 'next done' } },
    ...oneShot('abort', 'child-abort.txt'),
  ],
  steps: [
    submit('s-abort', 'PARENT-abort delegate', [{ on: { type: 'permission_request' }, ws: 'abort_turn', params: { sessionKey: 's-abort' }, count: 1 }]),
    { sleep: 500 },
    pending('s-abort'),
    submit('s-abort', 'next: go on'),
    { sleep: 300 },
    read('s-abort'),
  ],
};

// 后台子代理：sdkSessionConfig.agents 里 background: true 的定义（OneShotSubagentPort.ts launchBackground）。
// 父轮同时调 agent 和 bash sleep 2：子代理的询问落在父轮还在跑的时候。
const BG_AGENTS = { agents: { worker: { description: 'Writes in the background.', prompt: 'WORKER-SUFFIX: do as told.', tools: ['bash'], background: true } } };
const background = (marker, file) => [
  { name: `${marker}-parent-after`, when: [`PARENT-${marker}`, '"tool_call_id":"call_sleep"'], reply: { text: `parent done ${marker}` } },
  { name: `${marker}-parent`, when: [`PARENT-${marker}`], reply: { tools: [
    { id: 'call_parent', name: 'agent', arguments: { description: `Run ${marker}`, prompt: `CHILD-${marker}: write the file.`, subagent_type: 'worker' } },
    { id: 'call_sleep', name: 'bash', arguments: { command: 'sleep 2', description: 'parent waits' } },
  ] } },
  { name: `${marker}-child-after`, when: [`CHILD-${marker}`, '"tool_call_id":"call_child"'], reply: { text: REPORT } },
  { name: `${marker}-child`, when: [`CHILD-${marker}`], reply: { tool: { id: 'call_child', name: 'bash', arguments: { command: `printf ${marker} > ${file}`, description: `child ${marker}` } } } },
];
scenarios['permission-subagent-background'] = {
  about: '后台子代理（sdkSessionConfig.agents 的 background: true）在父轮还在跑时调 bash：父轮自己的 bash（agent 不询问）与子代理的 bash 都要询问，全部经父会话的流发出，父轮的在前；s-bg-allow 两次都批准（子代理的文件落盘），s-bg-deny 批准父轮的、拒绝子代理的（文件不落盘、子代理照常回报告）。之后 background_tasks 与下一轮。',
  provider: [
    TITLE,
    { name: 'next', when: ['next:'], reply: { text: 'next done' } },
    ...background('bgallow', 'bg-allow.txt'),
    ...background('bgdeny', 'bg-deny.txt'),
  ],
  steps: [
    submit('s-bg-allow', 'PARENT-bgallow launch', [{ ...decide('s-bg-allow', 'allow'), count: 2 }], { sdkSessionConfig: BG_AGENTS }),
    { sleep: 1500 },
    { ws: 'background_tasks', params: { sessionKey: 's-bg-allow' } },
    submit('s-bg-allow', 'next: after', null, { sdkSessionConfig: BG_AGENTS }),
    { sleep: 300 },
    read('s-bg-allow'),
    // 询问的先后是确定的：父轮的 bash 立刻询问，子代理的 bash 要等它自己的一次模型请求之后。
    submit('s-bg-deny', 'PARENT-bgdeny launch', [
      decide('s-bg-deny', 'allow'),
      decide('s-bg-deny', 'deny', { reason: 'not the child' }),
    ], { sdkSessionConfig: BG_AGENTS }),
    { sleep: 1500 },
    { ws: 'background_tasks', params: { sessionKey: 's-bg-deny' } },
    submit('s-bg-deny', 'next: after', null, { sdkSessionConfig: BG_AGENTS }),
    { sleep: 300 },
    read('s-bg-deny'),
  ],
};

// 子代理结果文本里带位数不定的耗时（durationMs），父轮之后的 token 估算随之变；与 hook-subagent、tool-agent 等同一组字段。
const TOKEN_VOLATILE = ['\\.(used|ratio|displayUsed|usageTokens|localEstimateTokens|calibrationEstimatedInputTokens|fileSize|breakdown\\.total|breakdown\\.messages|resultBytes|content-length)$'];
for (const name of ['permission-subagent-oneshot', 'permission-subagent-background']) {
  scenarios[name].volatile = TOKEN_VOLATILE;
  scenarios[name].volatileWhy = '子代理结果文本里带位数不定的耗时，token 估算随之变；基线自比已证明（三次自比都只差在这些字段上）';
}

for (const [name, body] of Object.entries(scenarios)) {
  fs.writeFileSync(path.join(here, `${name}.json`), JSON.stringify({ name, ...body }, null, 2) + '\n');
  console.log('wrote', name);
}
