// 生成 host-failure-*.json 场景。用法（仓库根目录）：node scripts/bbx/scenarios/host-failure.gen.mjs
// 改场景改这里，再重新生成；生成的 JSON 一并提交。
//
// 宿主一侧的模块回调（工具执行、hook、权限、落盘）在轮次中途失败时，基线（进程内循环）把那个错误
// 原样抛到轮次：本轮以 agent_invalid_state 失败，消息是原始错误。这里用插件 command hook 在轮次
// 中途把会话记录文件改成只读，制造落盘失败。
import fs from 'node:fs';
import path from 'node:path';

const here = path.dirname(new URL(import.meta.url).pathname);

const TITLE = { name: 'title', when: ['Generate a concise title'], reply: { text: '{"title": "Host failure"}' } };
const turn = (session, message, extra = {}) => ({
  ws: 'submit_turn',
  params: { sessionKey: session, channelKey: 'test', message },
  ...extra,
});
// 插件 hook：把本会话的记录文件改成只读（之后的追加写入失败）。
const chmodHook = (session) => ({
  type: 'command',
  command: `chmod 444 "$HOME"/.pilotdeck/projects/*/chats/${session}.jsonl`,
});
const plugin = (event, matcher, session) => ({
  'home/.pilotdeck/plugins/ro/plugin.json': {
    name: 'ro',
    version: '0.1.0',
    description: 'bbx ro',
    hooks: { [event]: [{ ...(matcher ? { matcher } : {}), hooks: [chmodHook(session)] }] },
  },
});

const scenarios = {};

// 工具调用前（PreToolUse）把会话记录改成只读：工具结果落盘失败。
scenarios['host-failure-transcript-tool'] = {
  about: '宿主回调失败（落盘）：PreToolUse hook 在工具执行前把会话记录文件改成只读，之后工具调用 / 结果的落盘失败；看本轮的失败事件、错误码与文案（基线进程内循环把原始错误抛到轮次）。本文件由同目录的 host-failure.gen.mjs 生成，不手改。',
  files: {
    ...plugin('PreToolUse', 'read_file', 's-ro'),
    'home/.pilotdeck/notes/a.txt': 'alpha\n',
  },
  provider: [
    TITLE,
    { name: 'after', when: ['ro run:', '"tool_call_id":"call_ro"'], reply: { text: 'done' } },
    { name: 'tool', when: ['ro run:'], reply: { tool: { id: 'call_ro', name: 'read_file', arguments: { file_path: '{PILOT_HOME}/notes/a.txt' } } } },
  ],
  steps: [
    turn('s-ro', 'ro run: read the note'),
    { sleep: 500 },
    { ws: 'list_sessions' },
  ],
};

// 第二轮开始时（UserPromptSubmit）把会话记录改成只读：开模型请求前的上下文记录落盘失败。
scenarios['host-failure-transcript-prompt'] = {
  about: '宿主回调失败（落盘）：第二轮的 UserPromptSubmit hook 把会话记录文件改成只读，之后这一轮开模型请求前的上下文落盘失败；看失败事件的错误码与文案。本文件由同目录的 host-failure.gen.mjs 生成，不手改。',
  files: {
    ...plugin('UserPromptSubmit', '', 's-ro2'),
  },
  provider: [TITLE, { name: 'main', reply: { text: 'answer' } }],
  steps: [
    turn('s-ro2', 'first: hi'),
    { sleep: 500 },
    { ws: 'list_sessions' },
  ],
};

// SDK HTTP hook 回 async + 同一个 asyncHookId：第一个会话登记后挂起，第二个会话再登记同一个 id 时
// 网关拒绝（ASYNC_HOOK_INVOCATION_CONFLICT），这次 lifecycle 派发失败。
const sdkHooks = (event) => ({ hooks: { url: '{MOCK}/hooks', events: { [event]: [{}] } } });
const asyncHook = (event) => ({
  name: `hook-${event}`,
  when: ['"transcriptPath"', `"hookEventName":"${event}"`],
  reply: { status: 200, body: { async: true, asyncHookId: 'inv-shared', asyncTimeout: 60 } },
});
const sdkTurn = (session, message, event) => ({
  ws: 'submit_turn',
  params: { sessionKey: session, channelKey: 'test', message, sdkSessionConfig: sdkHooks(event) },
  timeout: 20000,
});

// UserPromptSubmit 派发失败：派发在网关提交轮次时（循环开始前），本轮以 gateway_submit_failed 结束，消息是冲突原文。
scenarios['host-failure-hook-prompt'] = {
  about: '宿主回调失败（hook）：UserPromptSubmit 的 SDK HTTP hook 回 async，两个会话登记同一个 asyncHookId，第二个会话的 lifecycle 派发失败；这一派发在循环开始前，基线以 gateway_submit_failed 结束本轮，消息是冲突原文。本文件由同目录的 host-failure.gen.mjs 生成，不手改。',
  provider: [TITLE, asyncHook('UserPromptSubmit'), { name: 'main', reply: { text: 'answer' } }],
  steps: [
    sdkTurn('s-first', 'first: hi', 'UserPromptSubmit'),
    { sleep: 300 },
    sdkTurn('s-second', 'second: hi', 'UserPromptSubmit'),
    { sleep: 300 },
    { ws: 'hook_async_result', params: { sessionKey: 's-first', invocationId: 'inv-shared', output: {} } },
  ],
};

// PreToolUse 派发失败：失败发生在宿主执行工具批次时；基线进程内循环的工具批次 catch 给每个调用
// 补一个缺失结果（tool_execution_failed + 恢复建议），模型拿到错误后照常收尾。
scenarios['host-failure-hook-tool'] = {
  about: '宿主回调失败（工具批次）：PreToolUse 的 SDK HTTP hook 回 async，两个会话登记同一个 asyncHookId，第二个会话的工具批次在宿主侧失败；基线把冲突原文作为该批次每个调用的缺失结果（TOOL_ERROR 格式与恢复建议），模型再回一句收尾。本文件由同目录的 host-failure.gen.mjs 生成，不手改。',
  files: { 'home/.pilotdeck/notes/a.txt': 'alpha\n' },
  provider: [
    TITLE,
    asyncHook('PreToolUse'),
    { name: 'after', when: ['"tool_call_id":"call_hk"'], reply: { text: 'done' } },
    { name: 'tool', when: ['read it'], reply: { tool: { id: 'call_hk', name: 'read_file', arguments: { file_path: '{PILOT_HOME}/notes/a.txt' } } } },
  ],
  steps: [
    sdkTurn('s-first', 'first: read it', 'PreToolUse'),
    { sleep: 300 },
    sdkTurn('s-second', 'second: read it', 'PreToolUse'),
    { sleep: 300 },
    { ws: 'hook_async_result', params: { sessionKey: 's-first', invocationId: 'inv-shared', output: {} } },
  ],
};

for (const [name, body] of Object.entries(scenarios)) {
  fs.writeFileSync(path.join(here, `${name}.json`), JSON.stringify({ name, ...body }, null, 2) + '\n');
}
console.log(`wrote ${Object.keys(scenarios).length} scenarios`);
