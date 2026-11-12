// 生成 compact-*.json 场景。用法（仓库根目录）：node scripts/bbx/scenarios/compaction.gen.mjs
// 改场景改这里，再重新生成；生成的 JSON 一并提交。
//
// 上下文压缩要在超过预算时才发生。场景把模型的上下文窗口配得很小（maxContextTokens 几千到一万多，
// maxOutputTokens 1000），每轮发一段固定的长文本，几轮之后越过阈值。文本由下面的 filler 生成，
// 内容固定、没有随机值，token 数两侧一样。
//
// 注意基线的预算估计会用上一轮 provider 报的 usage 校准（假 provider 固定报 prompt_tokens 12），
// 校准把估计值最多压低一半，所以第二轮起「用量」约是本地估计的一半。阈值按这个算。
import fs from 'node:fs';
import path from 'node:path';

const here = path.dirname(new URL(import.meta.url).pathname);

const config = ({ context = 14000, output = 1000 } = {}) => `schemaVersion: 1
agent:
  model: bbx/deterministic
  maxContextTokens: ${context}
  maxOutputTokens: ${output}
model:
  providers:
    bbx:
      protocol: openai
      url: {MOCK}/v1
      apiKey: bbx-mock-only
      models:
        deterministic:
          capabilities:
            supportsToolUse: true
            maxContextTokens: ${context}
            maxOutputTokens: ${output}
telemetry:
  enabled: false
`;

const WORDS = 'alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa';
// 一段固定文本：lines 行，每行约 20 个 token。
const filler = (tag, lines) => Array.from({ length: lines }, (_, j) => `${tag} line ${j + 1}: ${WORDS}.`).join('\n');

const TITLE = { name: 'title', when: ['Generate a concise title'], reply: { text: '{"title": "Compaction run"}' } };
const SUMMARY_TEXT = [
  '## Objective',
  'Keep the long notes conversation going.',
  '## Current State',
  'Several long notes were exchanged.',
  '## Remaining',
  'Answer the next note.',
  '## Files And Artifacts',
  'None',
].join('\n');
const SUMMARY = { name: 'summary', when: ['internal-compaction-control'], reply: { text: SUMMARY_TEXT } };
const MAIN = { name: 'main', reply: { text: 'Noted.' } };

const turn = (session, message, extra = {}) => ({
  ws: 'submit_turn',
  params: { sessionKey: session, channelKey: 'test', message },
  ...extra,
});

const scenarios = {};

// 自动全量压缩：六轮长文本把用量推过 0.9，第六轮开模型请求前先压缩（摘要请求 + 替换历史），
// 第七轮看压缩后的请求内容；然后读会话、分叉、导出。
{
  const steps = [];
  for (let i = 1; i <= 6; i += 1) {
    steps.push(turn('s-cmp', `note ${i}\n${filler(`n${i}`, 140)}`), { sleep: 300 });
  }
  steps.push(turn('s-cmp', 'note 7: short follow-up'), { sleep: 300 });
  steps.push(
    {
      ws: 'read_session_messages',
      params: { sessionKey: 's-cmp' },
      save: { note7: '"text":"note 7: short follow-up","entryId":"([0-9a-f-]{36})"' },
    },
    { ws: 'export_session_transcript', params: { sessionKey: 's-cmp' } },
    // 从第七轮的用户消息分叉：新会话带着压缩后的历史；在分叉上再问一轮。
    {
      ws: 'fork_session',
      params: { sessionKey: 's-cmp', fromEntryId: '{saved.note7}' },
      save: { forked: '"newSessionKey":"([^"]+)"' },
    },
    turn('{saved.forked}', 'note 8: on the fork'),
    { sleep: 300 },
    { ws: 'read_session_messages', params: { sessionKey: '{saved.forked}' } },
  );
  scenarios['compact-auto'] = {
    about: '自动全量压缩：上下文窗口配小，几轮长文本后用量越过 0.9，开模型请求前先发摘要请求（摘要提示词、裁剪后的输入），用摘要和保留的消息替换历史；之后一轮的请求带 compact-boundary 与摘要；读会话、导出会话都看到压缩后的结果。本文件由同目录的 compaction.gen.mjs 生成，不手改。',
    config: config({ context: 20000 }),
    provider: [TITLE, SUMMARY, MAIN],
    steps,
  };
}

const outDir = here;
for (const [name, body] of Object.entries(scenarios)) {
  fs.writeFileSync(path.join(outDir, `${name}.json`), JSON.stringify({ name, ...body }, null, 2) + '\n');
}
console.log(`wrote ${Object.keys(scenarios).length} scenarios`);
