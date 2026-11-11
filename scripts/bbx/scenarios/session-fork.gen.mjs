// 生成 session-fork.json、session-fork-assistant.json、session-fork-resume-at.json。
// 用法（仓库根目录）：
//   BBX_SLOT=<n> node scripts/bbx/run.mjs ts scripts/bbx/scenarios/session-fork-src.json /tmp/fork-src.ts.json
//   node scripts/bbx/scenarios/session-fork.gen.mjs /tmp/fork-src.ts.json <n> scripts/bbx/scenarios
// session-fork-src 场景：会话 s-src 上两轮纯文本对话（runId run-src-1 / run-src-2）。
// 从基线录下的 s-src.jsonl 取出会话记录（model_request 里的系统提示词和工具声明裁掉），
// 槽位 n 的目录名和端口换回缺省槽位的写法，作为初始文件；fork_session 引用其中固定的 entryId。
// 每个场景只做一次成功的分叉：新会话名是随机 uuid，落盘树里有两个随机名时键的先后会变，基线自比不稳。
import fs from 'node:fs';
const [capturePath, slotArg, outDir] = process.argv.slice(2);
const slot = Number(slotArg);
const unslot = (t) => slot ? t.replaceAll(`pdrs-bb${slot}`, 'pdrs-bbx').replaceAll(`384${1 + slot}`, '3841') : t;
const capture = JSON.parse(unslot(fs.readFileSync(capturePath, 'utf8')));
const CHATS = 'home/.pilotdeck/projects/private-tmp-pdrs-bbx-root-home-.pilotdeck/chats';
const key = Object.keys(capture.tree).find((p) => p.endsWith('/s-src.jsonl'));
const entries = capture.tree[key].jsonl.map((entry) => entry.type !== 'model_request' ? entry
  : { ...entry, request: { ...entry.request, systemPrompt: 'fixture system prompt', tools: [] } });
const find = (type, turnId) => entries.find((e) => e.type === type && e.turnId === turnId).entryId;
const accepted2 = find('accepted_input', 'run-src-2');
const accepted1 = find('accepted_input', 'run-src-1');
const assistant1 = find('assistant_message', 'run-src-1');
const fork = (params) => ({ ws: 'fork_session', params: { sessionKey: 's-src', ...params } });
const base = (name, about, steps) => ({
  name,
  about: `${about} 对着一份由基线录下的两轮会话（s-src），之后列出会话并比较落盘。本文件由同目录的 session-fork.gen.mjs 生成，不手改。`,
  provider: [
    { name: 'title', when: ['Generate a concise title'], reply: { text: '{"title": "Fork"}' } },
    { name: 'main', reply: { text: 'Hello from the mock.' } },
  ],
  files: {
    [`${CHATS}/s-src.jsonl`]: entries.map((e) => JSON.stringify(e)).join('\n') + '\n',
  },
  steps: [...steps, { ws: 'list_sessions', params: {} }],
});
const scenarios = [
  base('session-fork', 'fork_session：各类错误（缺参数、未知 entryId、空会话、resumeDropsTurn 的三种校验），从第二轮用户消息分叉（新会话只带第一轮，回填第二轮文本）。', [
    { ws: 'fork_session', params: {} },
    fork({}),
    fork({ fromEntryId: 'no-such-entry' }),
    { ws: 'fork_session', params: { sessionKey: 's-empty', fromEntryId: accepted1 } },
    fork({ fromEntryId: assistant1, resumeDropsTurn: 'no-such-entry' }),
    fork({ fromEntryId: assistant1, resumeDropsTurn: accepted1 }),
    fork({ fromEntryId: assistant1, resumeDropsTurn: accepted2 }),
    fork({ fromEntryId: accepted1, resumeDropsTurn: accepted2 }),
    fork({ fromEntryId: accepted2 }),
  ]),
  base('session-fork-assistant', 'fork_session：从第一轮助手消息分叉（保留到该消息及该轮的 turn_result）。', [
    fork({ fromEntryId: assistant1 }),
  ]),
  base('session-fork-resume-at', 'fork_session：resumeAt 为真时从第二轮用户消息分叉并保留它。', [
    fork({ fromEntryId: accepted2, resumeAt: true }),
  ]),
];
for (const scenario of scenarios) fs.writeFileSync(`${outDir}/${scenario.name}.json`, JSON.stringify(scenario, null, 2) + '\n');
