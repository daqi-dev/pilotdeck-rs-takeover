#!/usr/bin/env node
// 黑盒对拍 · 比较器自测。给 diff.mjs 喂成对的小产物，检查两件事：
//   该抹掉的（基线自己两次也不同的值）确实判 SAME；
//   抹掉之外的任何改动仍然判 DIFF——每条归一化规则旁边都放一个「差一点就该报」的例子。
// 改 diff.mjs 之后先跑它：node scripts/bbx/selftest.mjs（退出码 0 表示全部符合预期）。
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-selftest-'));
const verdict = (a, b, extra = []) => {
  fs.writeFileSync(path.join(dir, 'a.json'), JSON.stringify(a));
  fs.writeFileSync(path.join(dir, 'b.json'), JSON.stringify(b));
  const r = spawnSync(process.execPath, [path.join(here, 'diff.mjs'), path.join(dir, 'a.json'), path.join(dir, 'b.json'), '--quiet', ...extra], { encoding: 'utf8' });
  return r.stdout.trim().split('\n').pop().split(' ')[0];
};
const art = (steps, more = {}) => ({ scenario: 't', side: 'x', steps, notifications: [], provider: {}, tree: {}, ...more });
const frame = (v) => art([{ step: { ws: 'm' }, frames: [v] }]);
const U1 = '11111111-1111-4111-8111-111111111111', U2 = '22222222-2222-4222-8222-222222222222', U3 = '33333333-3333-4333-8333-333333333333';
const H1 = 'a'.repeat(64), H2 = 'b'.repeat(64);
const rec = (type, extra = {}) => ({ type, turnId: 't1', sequence: 0, entryId: 'e', parentEntryId: null, createdAt: '2026-01-01T00:00:00.000Z', ...extra });
const title = rec('session_metadata', { metadata: { aiTitle: 'T', titleSource: 'provider' } });
const sql = (row, volatile = []) => art([], { volatile, tree: { '/home/x/m.sqlite': { mode: '644', sqlite: { objects: [], tables: { t: [row] } } } } });
const jsonl = (records) => art([], { tree: { '/home/x/chats/s.jsonl': { mode: '644', jsonl: records.map((r, i) => ({ ...r, sequence: i + 1, entryId: `e${i}` })) } } });

// 会话文件加上一份投影检查点，投影里抄了 step_started 那条记录的位置字段。
const withProj = (records, kind = 'k') => {
  const list = records.map((r, i) => ({ ...r, sequence: i + 1, entryId: `e${i}` }));
  const e = list.find((r) => r.type === 'step_started');
  return art([], { tree: {
    '/home/x/chats/s.jsonl': { mode: '644', jsonl: list },
    '/home/x/chats/s.jsonl.projections.json': { mode: '644', json: { entries: [{ index: e.sequence - 1, kind, entry: { sequence: e.sequence, entryId: e.entryId, parentEntryId: null } }] } },
  } });
};
// 规则 17：会话文件 s.jsonl（第一条记录最短）加一帧 read_session_messages 的 session.fileSize。
// dur 是每条工具记录里 durationMs 的值，size 缺省为整份文件的字节数，sizeAt 给出时取前 sizeAt 条记录的字节数。
const sessRecs = (dur, n = 11) => [{ type: 'accepted_input', turnId: 't1' },
  ...Array.from({ length: n }, (_, i) => ({ type: 'tool_result', turnId: 't1', toolUseId: `call_${i}`, output: 'some tool output text here', durationMs: dur }))];
const bytes = (recs) => recs.reduce((n, r) => n + Buffer.byteLength(JSON.stringify(r)) + 1, 0);
const sized = (recs, { size = bytes(recs), sizeAt, file = true, rewritten = false } = {}) => art(
  [{ step: { ws: 'read_session_messages' }, frames: [{ type: 'response', ok: true, result: { session: { sessionId: 's', fileSize: sizeAt === undefined ? size : bytes(recs.slice(0, sizeAt)) } } }] }],
  { tree: file ? { '/home/.pilotdeck/projects/p/chats/s.jsonl': { mode: '644', jsonl: recs, raw: { lines: rewritten ? recs.length + 1 : recs.length, finalNewline: true, skeletons: {} } } } : {} });
const LOCK = '/home/.pilotdeck/runs/registry.json.lock';
const withLock = (text) => art([], { tree: text === undefined ? {} : { [LOCK]: { mode: '644', text } } });
const memState = (caseId, traceId, extra = 'x') => sql({ state_key: 'recentCaseTraces', state_json: JSON.stringify([{ caseId, note: extra, retrieval: { trace: { traceId, steps: [{ stepId: `${traceId}:step:1` }, { stepId: `${traceId}:step:2` }] } } }]) });
// 规则 19：工具结果原文（会话文件里的 tool_result 记录）加一帧带 resultBytes 的 tool_call_finished。
const toolRec = (id, content, turnId = 't1') => ({ type: 'tool_result', turnId, result: { type: 'success', toolCallId: id, toolName: 'bash', content } });
const finished = (id, bytes, turnId = 't1') => ({ type: 'event', event: { type: 'tool_call_finished', toolCallId: id, toolName: 'bash', resultBytes: bytes, timeline: { turnId } } });
const withResult = (text, { bytes = Buffer.byteLength(text), recs = [toolRec('c1', [{ type: 'text', text }])], turnId = 't1' } = {}) =>
  art([{ step: { ws: 'submit_turn' }, frames: [finished('c1', bytes, turnId)] }], { tree: { '/home/x/chats/s.jsonl': { mode: '644', jsonl: recs } } });
const DUR6 = 'exit_code: 1\n- duration_ms: 6', DUR12 = 'exit_code: 1\n- duration_ms: 12';
const toProvider = (len, ok, extra = {}) => art([], { provider: { m: [{ method: 'POST', url: '/v1/chat/completions', headers: { 'content-type': 'application/json', 'content-length': String(len), ...extra },
  ...(ok === undefined ? {} : { 'content-length-ok': ok }), body: { messages: [{ role: 'tool', content: `- duration_ms: ${len}` }] } }] } });
const cases = [
  ['resultBytes：两侧各自等于本侧原文字节数（耗时位数不同）', withResult(DUR6), withResult(DUR12), 'SAME'],
  ['resultBytes：一侧不等于本侧原文字节数', withResult(DUR6), withResult(DUR12, { bytes: Buffer.byteLength(DUR12) + 1 }), 'DIFF'],
  ['resultBytes：一侧找不到原文', withResult(DUR6), withResult(DUR12, { recs: [] }), 'DIFF'],
  ['resultBytes：两侧核对通过但原文内容不同', withResult(DUR6), withResult('exit_code: 2\n- duration_ms: 12'), 'DIFF'],
  ['resultBytes：同一个调用 id 在两轮里各有原文，按轮次挑出', withResult(DUR6, { recs: [toolRec('c1', [{ type: 'text', text: 'other' }], 't0'), toolRec('c1', [{ type: 'text', text: DUR6 }])] }),
    withResult(DUR12, { recs: [toolRec('c1', [{ type: 'text', text: 'other' }], 't0'), toolRec('c1', [{ type: 'text', text: DUR12 }])] }), 'SAME'],
  ['resultBytes：同一个调用 id 有两份大小不同的原文、分不出轮次', withResult(DUR6, { recs: [toolRec('c1', [{ type: 'text', text: 'other' }], 't0'), toolRec('c1', [{ type: 'text', text: DUR6 }], 't2')] }),
    withResult(DUR12, { recs: [toolRec('c1', [{ type: 'text', text: 'other' }], 't0'), toolRec('c1', [{ type: 'text', text: DUR12 }], 't2')] }), 'DIFF'],
  ['resultBytes：多段内容按换行拼接、json 段按 JSON.stringify', withResult('', { bytes: Buffer.byteLength('a\n{"durationMs":6}'), recs: [toolRec('c1', [{ type: 'text', text: 'a' }, { type: 'json', value: { durationMs: 6 } }])] }),
    withResult('', { bytes: Buffer.byteLength('a\n{"durationMs":12}'), recs: [toolRec('c1', [{ type: 'text', text: 'a' }, { type: 'json', value: { durationMs: 12 } }])] }), 'SAME'],
  ['resultBytes：多段内容拼接时少算了换行', withResult('', { bytes: Buffer.byteLength('a\n{"durationMs":6}'), recs: [toolRec('c1', [{ type: 'text', text: 'a' }, { type: 'json', value: { durationMs: 6 } }])] }),
    withResult('', { bytes: Buffer.byteLength('a{"durationMs":123}'), recs: [toolRec('c1', [{ type: 'text', text: 'a' }, { type: 'json', value: { durationMs: 123 } }])] }), 'DIFF'],
  ['resultBytes：一侧变成了字符串', withResult(DUR6), withResult(DUR12, { bytes: String(Buffer.byteLength(DUR12)) }), 'DIFF'],
  ['content-length：两侧都核对通过、数值不同', toProvider(9, true), toProvider(10, true), 'SAME'],
  ['content-length：一侧核对不通过', toProvider(9, true), toProvider(10, false), 'DIFF'],
  ['content-length：两侧都没有核对记录（旧产物）照比', toProvider(9), toProvider(10), 'DIFF'],
  ['content-length：核对通过，旁边的头不同照报', toProvider(9, true), toProvider(10, true, { 'x-extra': '1' }), 'DIFF'],

  // [说明, a, b, 期望, diff.mjs 额外参数]
  ['相同', frame({ a: 1 }), frame({ a: 1 }), 'SAME'],
  ['值不同', frame({ a: 1 }), frame({ a: 2 }), 'DIFF'],
  ['多一个键', frame({ a: 1 }), frame({ a: 1, b: 2 }), 'DIFF'],
  ['键顺序不同', frame({ a: 1, b: 2 }), frame({ b: 2, a: 1 }), 'DIFF'],
  ['类型不同', frame({ a: 1 }), frame({ a: '1' }), 'DIFF'],
  ['数组顺序不同', frame({ a: [1, 2] }), frame({ a: [2, 1] }), 'DIFF'],
  ['不比较 side / startMs / serverLog', art([], { side: 'ts', startMs: 1, serverLog: 'x' }), art([], { side: 'rust', startMs: 9, serverLog: 'y' }), 'SAME'],

  ['UUID 不同', frame({ id: U1 }), frame({ id: U2 }), 'SAME'],
  ['UUID 的相等关系变了', frame({ a: U1, b: U1 }), frame({ a: U2, b: U3 }), 'DIFF'],
  ['UUID 对非 UUID', frame({ id: U1 }), frame({ id: 'fixed' }), 'DIFF'],
  ['ISO 时间不同', frame({ t: '2026-01-01T00:00:00.000Z' }), frame({ t: '2026-02-02T11:22:33.444Z' }), 'SAME'],
  ['ISO 时间对空值', frame({ t: '2026-01-01T00:00:00.000Z' }), frame({ t: null }), 'DIFF'],
  ['毫秒时间戳不同', frame({ t: 1794000000000 }), frame({ t: 1794000009999 }), 'SAME'],
  ['毫秒时间戳整数对小数（mtimeMs 的纳秒部分恰好是整毫秒）', frame({ t: 1794000000000 }), frame({ t: 1794000000000.5 }), 'SAME'],
  ['毫秒时间戳：一侧 3 处带小数、另一侧对应位置全是整数（截成整数）', frame({ items: [1794630271916.82, 1794630271917.5, 1794630271918.25] }), frame({ items: [1794630271916, 1794630271917, 1794630271918] }), 'DIFF'],
  ['毫秒时间戳：反过来（b 侧带小数）也报', frame({ items: [1794630271916, 1794630271917, 1794630271918, 1794630271919] }), frame({ items: [1794630271916.82, 1794630271917.5, 1794630271918.25, 1794630271919.1] }), 'DIFF'],
  ['毫秒时间戳：只有 2 处带小数对整数不报', frame({ items: [1794630271916.82, 1794630271917.5] }), frame({ items: [1794630271916, 1794630271917] }), 'SAME'],
  ['毫秒时间戳：一侧 3 处带小数、另一侧有一处也带小数（巧合为 0 的不算截断）', frame({ items: [1794630271916.82, 1794630271917.5, 1794630271918.25] }), frame({ items: [1794630271916, 1794630271917.5, 1794630271918] }), 'SAME'],
  ['毫秒时间戳：两侧都带小数', frame({ items: [1794630271916.82, 1794630271917.5, 1794630271918.25] }), frame({ items: [1794630271916.1, 1794630271917.2, 1794630271918.3] }), 'SAME'],
  ['目录 mtimeMs 一侧小数为 0',frame({ items: [{ kind: 'directory', mtimeMs: 1794630273305 }] }), frame({ items: [{ kind: 'directory', mtimeMs: 1794630271916.8252 }] }), 'SAME'],
  ['内存诊断行的内存数值', frame({ s: 'x\n[pilotdeck:memory] {"event":"turn_completed","rss":81234567,"heapUsed":12345678,"heapTotal":23456789,"external":3456789,"arrayBuffers":45678,"sessionCount":1}\n' }), frame({ s: 'x\n[pilotdeck:memory] {"event":"turn_completed","rss":99999999,"heapUsed":1,"heapTotal":2,"external":3,"arrayBuffers":4,"sessionCount":1}\n' }), 'SAME'],
  ['内存诊断行的其余部分照比', frame({ s: '[pilotdeck:memory] {"event":"turn_completed","rss":1,"sessionCount":1}' }), frame({ s: '[pilotdeck:memory] {"event":"turn_completed","rss":2,"sessionCount":2}' }), 'DIFF'],
  ['别的行里的 rss 不抹', frame({ s: '[other] {"rss":1}' }), frame({ s: '[other] {"rss":2}' }), 'DIFF'],
  ['内存诊断行少一行', frame({ s: '[pilotdeck:memory] {"event":"turn_completed","rss":1}\n' }), frame({ s: '' }), 'DIFF'],
  ['毫秒时间戳对范围外的数', frame({ t: 1794000000000.5 }), frame({ t: 1594000000000.5 }), 'DIFF'],
  ['毫秒时间戳对字符串', frame({ t: 1794000000000.5 }), frame({ t: '1794000000000.5' }), 'DIFF'],
  ['范围外的大数不抹', frame({ t: 1294000000000 }), frame({ t: 1294000000001 }), 'DIFF'],
  ['文本里的毫秒时间戳', frame({ s: '"lastModified": 1794000000000,' }), frame({ s: '"lastModified": 1794000009999,' }), 'SAME'],
  ['十六进制串里的 13 位数字不抹', frame({ s: 'ab1794000000000cd' }), frame({ s: 'ab1794000009999cd' }), 'DIFF'],
  ['pid 键', frame({ pid: 100, ownerPid: 5 }), frame({ pid: 200, ownerPid: 6 }), 'SAME'],
  ['别的键里的小整数不抹', frame({ count: 100 }), frame({ count: 200 }), 'DIFF'],
  ['文本里的 pid=', frame({ s: 'task pid=123 running' }), frame({ s: 'task pid=45678 running' }), 'SAME'],
  ['requestFingerprint', frame({ requestFingerprint: 'aaa' }), frame({ requestFingerprint: 'bbb' }), 'SAME'],
  ['别的指纹类字段不抹', frame({ contentHash: 'aaa' }), frame({ contentHash: 'bbb' }), 'DIFF'],
  ['耗时键', frame({ durationMs: 5, duration_ms: 1, duration_seconds: 0.04, waitedMs: 0, delayMs: 300 }), frame({ durationMs: 77, duration_ms: 9, duration_seconds: 0.1, waitedMs: 1, delayMs: 412 }), 'SAME'],
  ['耗时键缺失', frame({ durationMs: 5 }), frame({}), 'DIFF'],
  ['别的毫秒字段不抹', frame({ timeoutMs: 5 }), frame({ timeoutMs: 6 }), 'DIFF'],
  ['文本里的耗时', frame({ s: 'status: ok\nduration_seconds: 0.04\n- duration_ms: 3' }), frame({ s: 'status: ok\nduration_seconds: 0.1\n- duration_ms: 12' }), 'SAME'],
  ['文本里耗时旁边的字', frame({ s: 'status: ok\nduration_seconds: 0.04' }), frame({ s: 'status: fail\nduration_seconds: 0.04' }), 'DIFF'],
  ['execute_code 临时目录后缀', frame({ s: '/tmp/pilotdeck_execute_code_AbC123/script.py' }), frame({ s: '/tmp/pilotdeck_execute_code_Zz9Yx8/script.py' }), 'SAME'],
  ['execute_code 临时目录的父路径', frame({ s: '/tmp/pilotdeck_execute_code_AbC123/script.py' }), frame({ s: '/var/pilotdeck_execute_code_AbC123/script.py' }), 'DIFF'],
  ['请求转储文件名：Rust 带进程号与序号的写法与基线对上', frame({ s: 'dumped to /t/pilotdeck_request_1794000000000.json then /t/pilotdeck_request_1794000000050.json' }), frame({ s: 'dumped to /t/pilotdeck_request_1794000001111_4242_0.json then /t/pilotdeck_request_1794000001111_4242_1.json' }), 'SAME'],
  ['请求转储文件名：两处引用是否同一个文件仍然在比', frame({ s: 'a /t/pilotdeck_request_1794000000000.json b /t/pilotdeck_request_1794000000000.json' }), frame({ s: 'a /t/pilotdeck_request_1794000001111_4242_0.json b /t/pilotdeck_request_1794000001111_4242_1.json' }), 'DIFF'],
  ['请求转储文件名里的毫秒', art([], { tree: { '/home/tmp/pilotdeck_request_1794537984648.json': { mode: '644', json: { a: 1 } } } }), art([], { tree: { '/home/tmp/pilotdeck_request_1794537985855.json': { mode: '644', json: { a: 1 } } } }), 'SAME'],
  ['请求转储换了前缀', frame({ s: '/tmp/pilotdeck_request_1794537984648.json' }), frame({ s: '/tmp/pilotdeck_reqvest_1794537984648.json' }), 'DIFF'],
  ['请求转储的内容', art([], { tree: { '/home/tmp/pilotdeck_request_1794537984648.json': { mode: '644', json: { a: 1 } } } }), art([], { tree: { '/home/tmp/pilotdeck_request_1794537985855.json': { mode: '644', json: { a: 2 } } } }), 'DIFF'],
  ['溢出目录里的进程号', frame({ s: '/tmp/pilotdeck-tool-output/123/s/x.txt' }), frame({ s: '/tmp/pilotdeck-tool-output/4567/s/x.txt' }), 'SAME'],
  ['溢出目录换了位置', frame({ s: '/tmp/pilotdeck-tool-output/123/s/x.txt' }), frame({ s: '/home/tool-results/s/x.txt' }), 'DIFF'],
  ['快照 id', frame({ snapshotId: H1, p: `/x/snapshots/${H1}/m.json` }), frame({ snapshotId: H2, p: `/x/snapshots/${H2}/m.json` }), 'SAME'],
  ['快照 id 与路径不再对应', frame({ snapshotId: H1, p: `/x/snapshots/${H1}/m.json` }), frame({ snapshotId: H1, p: `/x/snapshots/${H2}/m.json` }), 'DIFF'],
  ['别处的 64 位哈希不抹', frame({ sha256: H1 }), frame({ sha256: H2 }), 'DIFF'],
  ['快照恢复目录后缀', frame({ s: '/w/.pilotdeck-restore-AbC123' }), frame({ s: '/w/.pilotdeck-restore-Zz9Yx8' }), 'SAME'],
  ['Node 运行时警告', frame({ stderr: '(node:1) ExperimentalWarning: SQLite is an experimental feature\n(Use `node --trace-warnings ...` to show where the warning was created)\nreal\n' }), frame({ stderr: 'real\n' }), 'SAME'],
  ['一侧多一行 turn-timing（带 UUID）', frame({ stdout: `[gateway:turn-timing] {"runId":"${U3}","ms":301}\nrun ${U1} then ${U2}\n` }), frame({ stdout: `run ${U2} then ${U1}\n` }), 'SAME'],
  ['秒级时间戳 created', frame({ created: 1794495673, body: 'data: {"created":1794495673,"x":1}' }), frame({ created: 1794495675, body: 'data: {"created":1794495675,"x":1}' }), 'SAME'],
  ['请求转储文件名：Rust 带进程号与序号的写法与基线对上', frame({ s: 'dumped to /t/pilotdeck_request_1794000000000.json then /t/pilotdeck_request_1794000000050.json' }), frame({ s: 'dumped to /t/pilotdeck_request_1794000001111_4242_0.json then /t/pilotdeck_request_1794000001111_4242_1.json' }), 'SAME'],
  ['请求转储文件名：两处引用是否同一个文件仍然在比', frame({ s: 'a /t/pilotdeck_request_1794000000000.json b /t/pilotdeck_request_1794000000000.json' }), frame({ s: 'a /t/pilotdeck_request_1794000001111_4242_0.json b /t/pilotdeck_request_1794000001111_4242_1.json' }), 'DIFF'],
  ['请求转储文件名里的毫秒', frame({ s: 'dumped to /t/pilotdeck_request_1794000000000.json then /t/pilotdeck_request_1794000000050.json' }), frame({ s: 'dumped to /t/pilotdeck_request_1794000001111.json then /t/pilotdeck_request_1794000001170.json' }), 'SAME'],
  ['请求转储文件名的相等关系变了', frame({ a: '/t/pilotdeck_request_1794000000000.json', b: '/t/pilotdeck_request_1794000000000.json' }), frame({ a: '/t/pilotdeck_request_1794000001111.json', b: '/t/pilotdeck_request_1794000001170.json' }), 'DIFF'],
  ['别的文件名里的毫秒不抹', frame({ s: '/t/other_1794000000000.json' }), frame({ s: '/t/other_1794000001111.json' }), 'DIFF'],
  ['sidecar 身份的随机段', frame({ b: { moduleInstanceId: 'pilotdeck-agent-loop-instance-hloasjsoc6i', connectionGeneration: 'pilotdeck-agent-loop-instance-hloasjsoc6i-to3pp0vuy6o-g0ui61bw94w' }, streamId: 'stream-a9io7r7a4wu' }),
    frame({ b: { moduleInstanceId: 'pilotdeck-agent-loop-instance-14j56wlvxfz', connectionGeneration: 'pilotdeck-agent-loop-instance-14j56wlvxfz-g5281tr9mk-7mtdrsl9hx' }, streamId: 'stream-5i0fhci4pq' }), 'SAME'],
  ['重启后一侧是新实例、一侧还是旧实例', frame({ a: { moduleInstanceId: 'm-instance-aaa' }, b: { moduleInstanceId: 'm-instance-bbb' } }), frame({ a: { moduleInstanceId: 'm-instance-ccc' }, b: { moduleInstanceId: 'm-instance-ccc' } }), 'DIFF'],
  ['连接代与实例不再对应', frame({ moduleInstanceId: 'm-instance-aaa', connectionGeneration: 'm-instance-aaa-g1-c1' }), frame({ moduleInstanceId: 'm-instance-ccc', connectionGeneration: 'm-instance-ddd-g1-c1' }), 'DIFF'],
  ['连接代的段数不同', frame({ connectionGeneration: 'm-instance-aaa-g1-c1' }), frame({ connectionGeneration: 'm-instance-aaa-g1' }), 'DIFF'],
  ['实例 id 的模块名不同', frame({ moduleInstanceId: 'm-instance-aaa' }), frame({ moduleInstanceId: 'n-instance-aaa' }), 'DIFF'],
  ['同一条流对两条流', frame({ a: { streamId: 'stream-x1' }, b: { streamId: 'stream-x1' } }), frame({ a: { streamId: 'stream-y1' }, b: { streamId: 'stream-y2' } }), 'DIFF'],
  ['别的键里的同形字符串不抹', frame({ id: 'stream-x1' }), frame({ id: 'stream-y1' }), 'DIFF'],
  ['sidecar 应答的 messageId 随机段', frame({ lines: [{ kind: 'response', messageId: 'hello-k3j2h1g0f9', inReplyTo: 'c1' }, { kind: 'error', messageId: 'parse-0abcz12' }] }),
    frame({ lines: [{ kind: 'response', messageId: 'hello-p9o8i7u6y5t', inReplyTo: 'c1' }, { kind: 'error', messageId: 'parse-zz91' }] }), 'SAME'],
  ['sidecar messageId 的前缀不同', frame({ messageId: 'cancel-k3j2h1g0f9' }), frame({ messageId: 'status-k3j2h1g0f9' }), 'DIFF'],
  ['sidecar messageId 两条同 id 对两条不同 id', frame({ a: { messageId: 'ack-aaa1' }, b: { messageId: 'ack-aaa1' } }), frame({ a: { messageId: 'ack-bbb1' }, b: { messageId: 'ack-bbb2' } }), 'DIFF'],
  ['sidecar messageId 多段写法不抹', frame({ messageId: 'cancel-m-instance-aaa-g1-1' }), frame({ messageId: 'cancel-m-instance-bbb-g1-1' }), 'DIFF'],
  ['sidecar messageId 段数不同', frame({ messageId: 'cancel-k3j2h1g0f9' }), frame({ messageId: 'cancel-k3j2-h1g0f9' }), 'DIFF'],
  ['名单外前缀的 messageId 不抹', frame({ messageId: 'msg-k3j2h1g0f9' }), frame({ messageId: 'msg-p9o8i7u6y5t' }), 'DIFF'],
  ['inReplyTo 不抹', frame({ inReplyTo: 'hello-k3j2h1g0f9' }), frame({ inReplyTo: 'hello-p9o8i7u6y5t' }), 'DIFF'],
  // 规则 21：module_call 的 requestId / preparationId。
  ['module_call 标识的随机段（各端口前缀、准备序号、plan-todo 序号）', frame({ lines: [
    { method: 'module_call', requestId: 'model-prepare-k3j2h1g0f9' }, { method: 'module_call', requestId: 'model-sbkjwlh96f', payload: { preparationId: 'prepared-furx5a156x-1' } },
    { method: 'module_call', requestId: 'context-prepare_for_model-a1b2' }, { requestId: 'tool-batch-zz9' }, { requestId: 'tool-q1' }, { requestId: 'permission-decide-0p' },
    { requestId: 'lifecycle-dispatch-x1' }, { requestId: 'turn-drain_steer-y1' }, { requestId: 'budget-estimate_usage_cost-z1' }, { requestId: 'agent-event-w1' },
    { requestId: 'tool-catalog-v1' }, { requestId: 'model-stream-next-u1', payload: { preparationId: 'prepared-furx5a156x-1' } }, { requestId: 'plan-todo-t1-2' }] }),
  frame({ lines: [
    { method: 'module_call', requestId: 'model-prepare-p9o8i7u6y5t' }, { method: 'module_call', requestId: 'model-w3k0cw1l8hn', payload: { preparationId: 'prepared-9y9iqgabqdj-1' } },
    { method: 'module_call', requestId: 'context-prepare_for_model-c3d4e5' }, { requestId: 'tool-batch-aa1' }, { requestId: 'tool-q2' }, { requestId: 'permission-decide-1q' },
    { requestId: 'lifecycle-dispatch-x2' }, { requestId: 'turn-drain_steer-y2' }, { requestId: 'budget-estimate_usage_cost-z2' }, { requestId: 'agent-event-w2' },
    { requestId: 'tool-catalog-v2' }, { requestId: 'model-stream-next-u2', payload: { preparationId: 'prepared-9y9iqgabqdj-1' } }, { requestId: 'plan-todo-t2-2' }] }), 'SAME'],
  ['应答回带的 requestId 与请求对得上', frame({ a: { requestId: 'tool-abc1' }, b: { acceptedCall: { requestId: 'tool-abc1' } } }), frame({ a: { requestId: 'tool-xyz9' }, b: { acceptedCall: { requestId: 'tool-xyz9' } } }), 'SAME'],
  ['应答回带的 requestId 指向别的请求', frame({ a: { requestId: 'tool-abc1' }, b: { acceptedCall: { requestId: 'tool-abc1' } } }), frame({ a: { requestId: 'tool-xyz9' }, b: { acceptedCall: { requestId: 'tool-xyz8' } } }), 'DIFF'],
  ['续取用了另一个准备', frame({ a: { preparationId: 'prepared-aa-1' }, b: { preparationId: 'prepared-aa-1' } }), frame({ a: { preparationId: 'prepared-bb-1' }, b: { preparationId: 'prepared-cc-1' } }), 'DIFF'],
  ['module_call requestId 的端口前缀不同', frame({ requestId: 'model-k3j2h1g0f9' }), frame({ requestId: 'model-prepare-k3j2h1g0f9' }), 'DIFF'],
  ['module_call requestId 的操作名不同', frame({ requestId: 'context-prepare_for_model-a1' }), frame({ requestId: 'context-capture_turn-a1' }), 'DIFF'],
  ['preparationId 的序号不同', frame({ preparationId: 'prepared-aa-1' }), frame({ preparationId: 'prepared-bb-2' }), 'DIFF'],
  ['plan-todo 的序号不同', frame({ requestId: 'plan-todo-aa-1' }), frame({ requestId: 'plan-todo-bb-2' }), 'DIFF'],
  ['preparationId 一侧是 UUID 形状', frame({ preparationId: 'prepared-aa-1' }), frame({ preparationId: U1 }), 'DIFF'],
  ['model-metadata 的 requestId 没有随机段，不抹', frame({ requestId: 'model-metadata-p-m' }), frame({ requestId: 'model-metadata-p-n' }), 'DIFF'],
  ['名单外前缀的 requestId 不抹', frame({ requestId: 'rq-k3j2h1g0f9' }), frame({ requestId: 'rq-p9o8i7u6y5t' }), 'DIFF'],
  ['别的键里的端口前缀形状不抹', frame({ id: 'model-k3j2h1g0f9' }), frame({ id: 'model-p9o8i7u6y5t' }), 'DIFF'],
  ['requestId 的随机段多一段', frame({ requestId: 'tool-abc' }), frame({ requestId: 'tool-ab-c' }), 'DIFF'],
  ['sqlite 的 _json 单元格按内部字段放过', sql({ k: 'a', state_json: '{"x":1,"caseId":"r1"}' }, ['\\.state_json\\.<json>\\.caseId$']), sql({ k: 'a', state_json: '{"x":1,"caseId":"r2"}' }, ['\\.state_json\\.<json>\\.caseId$']), 'SAME'],
  ['sqlite 的 _json 单元格里别的字段照比', sql({ k: 'a', state_json: '{"x":1,"caseId":"r1"}' }, ['\\.state_json\\.<json>\\.caseId$']), sql({ k: 'a', state_json: '{"x":2,"caseId":"r2"}' }, ['\\.state_json\\.<json>\\.caseId$']), 'DIFF'],
  ['sqlite 的 _json 单元格写法不同', sql({ k: 'a', state_json: '{"x":1}' }), sql({ k: 'a', state_json: '{ "x": 1 }' }), 'DIFF'],
  ['sqlite 的 _json 单元格键顺序不同', sql({ k: 'a', state_json: '{"x":1,"y":2}' }), sql({ k: 'a', state_json: '{"y":2,"x":1}' }), 'DIFF'],
  ['sqlite 的 _json 单元格一侧不是 JSON', sql({ k: 'a', state_json: '{"x":1}' }), sql({ k: 'a', state_json: 'x=1' }), 'DIFF'],
  ['别的列里的 JSON 文本照整串比', sql({ k: 'a', note: '{"caseId":"r1"}' }, ['\\.note\\.<json>\\.caseId$']), sql({ k: 'a', note: '{"caseId":"r2"}' }, ['\\.note\\.<json>\\.caseId$']), 'DIFF'],
  ['记忆轨迹 id 的随机段', memState('case_trace_c8152c51eb', 'trace_32ee88c4c6'), memState('case_trace_df3f4efaa4', 'trace_2dcd40dfd6'), 'SAME'],
  ['记忆轨迹：步骤 id 不再指向同一条轨迹', memState('case_trace_c8152c51eb', 'trace_32ee88c4c6'),
    sql({ state_key: 'recentCaseTraces', state_json: JSON.stringify([{ caseId: 'case_trace_df3f4efaa4', note: 'x', retrieval: { trace: { traceId: 'trace_2dcd40dfd6', steps: [{ stepId: 'trace_2dcd40dfd6:step:1' }, { stepId: 'trace_aaaaaaaaaa:step:2' }] } } }]) }), 'DIFF'],
  ['记忆轨迹：case id 与 trace id 的相等关系', frame({ a: 'case_trace_1111111111', b: 'trace_2222222222' }), frame({ a: 'case_trace_3333333333', b: 'case_trace_3333333333' }), 'DIFF'],
  ['记忆轨迹旁边的字段照比', memState('case_trace_c8152c51eb', 'trace_32ee88c4c6', 'x'), memState('case_trace_df3f4efaa4', 'trace_2dcd40dfd6', 'y'), 'DIFF'],
  ['index_trace_ 不抹', frame({ id: 'index_trace_c8152c51eb' }), frame({ id: 'index_trace_df3f4efaa4' }), 'DIFF'],
  ['trace_ 后面位数不对不抹', frame({ id: 'trace_c8152c51eb0' }), frame({ id: 'trace_df3f4efaa41' }), 'DIFF'],
  ['fileSize：差 11 字节（11 个耗时 8 → 13），两侧都写到同一条', sized(sessRecs(8)), sized(sessRecs(13)), 'SAME'],
  ['fileSize：一侧是少写最后一条时的旧值', sized(sessRecs(8)), sized(sessRecs(8), { sizeAt: 11 }), 'DIFF'],
  ['fileSize：差一整条最短记录', sized(sessRecs(8)), sized(sessRecs(8), { size: bytes(sessRecs(8)) - bytes(sessRecs(8).slice(0, 1)) }), 'DIFF'],
  ['fileSize：一侧为 0', sized(sessRecs(8)), sized(sessRecs(8), { size: 0 }), 'DIFF'],
  ['fileSize 退到固定容差：差 11 字节', sized(sessRecs(8), { rewritten: true }), sized(sessRecs(13), { rewritten: true }), 'SAME'],
  ['fileSize 退到固定容差：差一整条最短记录', sized(sessRecs(8), { rewritten: true }), sized(sessRecs(8), { rewritten: true, size: bytes(sessRecs(8)) - bytes(sessRecs(8).slice(0, 1)) }), 'DIFF'],
  ['fileSize 退到固定容差：差值超过最短记录的一半', sized(sessRecs(8), { rewritten: true }), sized(sessRecs(8), { rewritten: true, size: bytes(sessRecs(8)) + 21 }), 'DIFF'],
  ['fileSize 退到固定容差：超过 64 字节', sized([{ type: 'accepted_input', text: 'x'.repeat(300) }], { rewritten: true }), sized([{ type: 'accepted_input', text: 'x'.repeat(300) }], { rewritten: true, size: bytes([{ type: 'accepted_input', text: 'x'.repeat(300) }]) + 65 }), 'DIFF'],
  ['fileSize 退到固定容差：一侧为 0', sized(sessRecs(8), { rewritten: true }), sized(sessRecs(8), { rewritten: true, size: 0 }), 'DIFF'],
  ['fileSize：树里没有这个会话文件', sized(sessRecs(8), { file: false }), sized(sessRecs(13), { file: false, size: bytes(sessRecs(13)) }), 'DIFF'],
  ['运行记录锁只在一侧有', withLock('{"pid":1,"acquiredAt":"2026-01-01T00:00:00.000Z"}'), withLock(undefined), 'SAME'],
  ['运行记录锁两侧都有、内容不同', withLock('{"pid":1,"acquiredAt":"2026-01-01T00:00:00.000Z"}'), withLock('{"pid":2,"acquiredAt":"2026-01-01T00:00:00.000Z","x":1}'), 'DIFF'],
  ['运行记录锁两侧都有、只有进程号和时间不同', withLock('{"pid":2590424,"acquiredAt":"2026-11-13T14:34:03.564Z"}'), withLock('{"pid":2602906,"acquiredAt":"2026-11-13T14:36:58.195Z"}'), 'SAME'],
  ['运行记录锁两侧都有、一侧还没写进内容', withLock('{"pid":2590424,"acquiredAt":"2026-11-13T14:34:03.564Z"}'), withLock(''), 'DIFF'],
  ['运行记录锁两侧都有、一侧进程号不是数字', withLock('{"pid":2590424,"acquiredAt":"2026-11-13T14:34:03.564Z"}'), withLock('{"pid":"2602906","acquiredAt":"2026-11-13T14:36:58.195Z"}'), 'DIFF'],
  ['别的锁文件里的进程号照比', art([], { tree: { '/home/.pilotdeck/other.json.lock': { mode: '644', text: '{"pid":1}' } } }), art([], { tree: { '/home/.pilotdeck/other.json.lock': { mode: '644', text: '{"pid":2}' } } }), 'DIFF'],
  ['别的锁文件只在一侧有',art([], { tree: { '/home/.pilotdeck/other.json.lock': { mode: '644', text: 'x' } } }), art([]), 'DIFF'],
  ['created 之外的 10 位整数', frame({ startedAt: 1794495673 }), frame({ startedAt: 1794495675 }), 'DIFF'],
  ['created 一侧不是时间戳', frame({ created: 1794495673 }), frame({ created: 12 }), 'DIFF'],
  ['警告之外的 stderr', frame({ stderr: '(node:1) ExperimentalWarning: x\n(Use `node --trace-warnings ...` to show where the warning was created)\nreal\n' }), frame({ stderr: 'other\n' }), 'DIFF'],

  ['带名字的数组少一个', frame({ tools: [{ name: 'a' }, { name: 'b' }] }), frame({ tools: [{ name: 'a' }] }), 'DIFF'],
  ['带名字的数组顺序不同', frame({ tools: [{ name: 'a' }, { name: 'b' }] }), frame({ tools: [{ name: 'b' }, { name: 'a' }] }), 'DIFF'],
  ['带名字的数组里同名元素内容不同', frame({ tools: [{ name: 'a', d: 1 }] }), frame({ tools: [{ name: 'a', d: 2 }] }), 'DIFF'],

  ['标题记录与循环开头的记录换位', jsonl([rec('accepted_input'), title, rec('step_started'), rec('model_request')]), jsonl([rec('accepted_input'), rec('step_started'), title, rec('model_request')]), 'SAME'],
  ['标题记录越过 agent_loop_operation_started / accepted（规则 8，sidecar 起得多快）', jsonl([rec('accepted_input'), title, rec('agent_loop_operation_started'), rec('agent_loop_operation_accepted'), rec('step_started')]), jsonl([rec('accepted_input'), rec('agent_loop_operation_started'), rec('agent_loop_operation_accepted'), rec('step_started'), title]), 'SAME'],
  ['标题记录越过 agent_loop_operation_* 之外的类型仍报', jsonl([rec('accepted_input'), title, rec('agent_loop_operation_started'), rec('agent_loop_operation_finished')]), jsonl([rec('accepted_input'), rec('agent_loop_operation_started'), rec('agent_loop_operation_finished'), title]), 'DIFF'],
  ['agent_loop_operation_started 自己换位仍报', jsonl([rec('accepted_input'), rec('agent_loop_operation_started'), rec('step_started')]), jsonl([rec('accepted_input'), rec('step_started'), rec('agent_loop_operation_started')]), 'DIFF'],
  ['标题记录落到 model_request 之后',jsonl([rec('accepted_input'), title, rec('step_started'), rec('model_request')]), jsonl([rec('accepted_input'), rec('step_started'), rec('model_request'), title]), 'DIFF'],
  ['标题内容不同', jsonl([rec('accepted_input'), title]), jsonl([rec('accepted_input'), { ...title, metadata: { aiTitle: 'X', titleSource: 'provider' } }]), 'DIFF'],
  ['别的记录换位', jsonl([rec('accepted_input'), rec('step_started'), rec('model_request')]), jsonl([rec('accepted_input'), rec('model_request'), rec('step_started')]), 'DIFF'],
  ['少一条记录', jsonl([rec('accepted_input'), rec('step_started')]), jsonl([rec('accepted_input')]), 'DIFF'],

  ['always-on 事件文件里记录换位', art([], { tree: { '/home/.pilotdeck/always-on/projects/p/events.jsonl': { mode: '644', jsonl: [{ phase: 'a' }, { phase: 'b' }] } } }), art([], { tree: { '/home/.pilotdeck/always-on/projects/p/events.jsonl': { mode: '644', jsonl: [{ phase: 'b' }, { phase: 'a' }] } } }), 'SAME'],
  ['always-on 事件文件里少一条', art([], { tree: { '/home/.pilotdeck/always-on/projects/p/events.jsonl': { mode: '644', jsonl: [{ phase: 'a' }, { phase: 'b' }] } } }), art([], { tree: { '/home/.pilotdeck/always-on/projects/p/events.jsonl': { mode: '644', jsonl: [{ phase: 'a' }] } } }), 'DIFF'],
  ['always-on 事件文件里内容不同', art([], { tree: { '/home/.pilotdeck/always-on/projects/p/events.jsonl': { mode: '644', jsonl: [{ phase: 'a' }, { phase: 'b' }] } } }), art([], { tree: { '/home/.pilotdeck/always-on/projects/p/events.jsonl': { mode: '644', jsonl: [{ phase: 'a' }, { phase: 'c' }] } } }), 'DIFF'],
  ['别的 jsonl 文件里记录换位照报', art([], { tree: { '/home/.pilotdeck/other/events.jsonl': { mode: '644', jsonl: [{ phase: 'a' }, { phase: 'b' }] } } }), art([], { tree: { '/home/.pilotdeck/other/events.jsonl': { mode: '644', jsonl: [{ phase: 'b' }, { phase: 'a' }] } } }), 'DIFF'],
  ['计时行有无与数值', frame({ stdout: 'listening\n[gateway:turn-timing] {"totalMs":250}\nbye\n' }), frame({ stdout: 'listening\nbye\n' }), 'SAME'],
  ['计时行之外的输出', frame({ stdout: 'listening\n[gateway:turn-timing] {"totalMs":250}\nbye\n' }), frame({ stdout: 'listening\nBYE\n' }), 'DIFF'],
  ['CLI stdout 末尾没有换行的计时行', art([{ step: { cli: [] }, cli: { stdout: 'hi\n[gateway:turn-timing] {"sessionKey":"s","phase":"released","releaseMs":231}', stderr: '' } }]), art([{ step: { cli: [] }, cli: { stdout: 'hi\n', stderr: '' } }]), 'SAME'],
  ['stderr 里两侧各多一行不同阶段的计时行', art([{ step: { cli: [] }, cli: { stdout: '', stderr: 'a\n[gateway:turn-timing] {"phase":"accepted","totalMs":201}\nb\n' } }]), art([{ step: { cli: [] }, cli: { stdout: '', stderr: 'a\nb\n[gateway:turn-timing] not json at all\n' } }]), 'SAME'],
  ['计时行前缀不在行首不摘', frame({ stdout: 'x [gateway:turn-timing] {"totalMs":250}\n' }), frame({ stdout: 'x\n' }), 'DIFF'],
  ['别的 [gateway: 行照比', frame({ stdout: '[gateway:turn-timings] {"totalMs":250}\n' }), frame({ stdout: '' }), 'DIFF'],
  ['计时行前缀少了空格不摘', frame({ stdout: '[gateway:turn-timing]{"totalMs":250}\n' }), frame({ stdout: '' }), 'DIFF'],
  ['标题换位连带的投影位置字段', withProj([rec('accepted_input'), title, rec('step_started'), rec('model_request')]), withProj([rec('accepted_input'), rec('step_started'), title, rec('model_request')]), 'SAME'],
  ['投影里别的字段不同照报', withProj([rec('accepted_input'), title, rec('step_started'), rec('model_request')]), withProj([rec('accepted_input'), rec('step_started'), title, rec('model_request')], 'other'), 'DIFF'],
  ['volatile 路径被略过', art([{ step: {}, frames: [{ used: 1 }] }], { volatile: ['\\.used$'] }), art([{ step: {}, frames: [{ used: 2 }] }], { volatile: ['\\.used$'] }), 'SAME'],
  ['volatile 之外照比', art([{ step: {}, frames: [{ used: 1, x: 1 }] }], { volatile: ['\\.used$'] }), art([{ step: {}, frames: [{ used: 2, x: 2 }] }], { volatile: ['\\.used$'] }), 'DIFF'],
  ['accepted 路径被略过', art([{ step: {}, frames: [{ v: 1 }] }], { accepted: [{ id: 'i', why: 'w', paths: ['\\.v$'] }] }), art([{ step: {}, frames: [{ v: 2 }] }], { accepted: [{ id: 'i', why: 'w', paths: ['\\.v$'] }] }), 'SAME'],
  ['accepted 一处没命中算差异', art([{ step: {}, frames: [{ v: 1 }] }], { accepted: [{ id: 'i', why: 'w', paths: ['\\.v$'] }] }), art([{ step: {}, frames: [{ v: 1 }] }], { accepted: [{ id: 'i', why: 'w', paths: ['\\.v$'] }] }), 'DIFF'],
  ['自比时不查 accepted 是否命中', art([{ step: {}, frames: [{ v: 1 }] }], { accepted: [{ id: 'i', why: 'w', paths: ['\\.v$'] }] }), art([{ step: {}, frames: [{ v: 1 }] }], { accepted: [{ id: 'i', why: 'w', paths: ['\\.v$'] }] }), 'SAME', ['--self']],
];

let bad = 0;
for (const [name, a, b, expected, extra] of cases) {
  const got = verdict(a, b, extra);
  if (got !== expected) { bad++; console.log(`不符合预期  ${name}：期望 ${expected}，实际 ${got}`); }
}

// 采集器的 18789 开关：没设 BBX_ALLOW_18789=1 时，写了 "gatewayPort": 18789 的场景在拿锁、查端口、
// 起任何进程之前就退出（退出码 6，不写产物），check.mjs 把它记成 SKIPPED、退出码 0、汇总行报出跳过个数；
// gatewayPort 写别的值是场景错误（退出码 2）。这几项都不碰 18789，也不需要基线或 Rust 二进制。
const collectorCases = [];
{
  const env = { ...process.env, BBX_SLOT: '0', BBX_BASELINE: path.join(dir, 'no-baseline'), BBX_TARGET: path.join(dir, 'no-target') };
  delete env.BBX_ALLOW_18789;
  const scenarioFile = (name, port) => {
    const f = path.join(dir, `${name}.json`);
    fs.writeFileSync(f, JSON.stringify({ name, gatewayPort: port, server: false, steps: [{ cli: ['--version'] }] }));
    return f;
  };
  const out = path.join(dir, 'out');
  const run = (script, args, extraEnv = {}) => spawnSync(process.execPath, [path.join(here, script), ...args], { encoding: 'utf8', env: { ...env, ...extraEnv } });
  const skip = run('run.mjs', ['ts', scenarioFile('needs-18789', 18789), path.join(out, 'a.json')]);
  collectorCases.push(['没设变量时 run.mjs 跳过（退出码 6）', skip.status === 6 && /SKIPPED needs-18789/.test(skip.stderr)]);
  collectorCases.push(['跳过时不写产物', !fs.existsSync(path.join(out, 'a.json'))]);
  const empty = run('run.mjs', ['rust', scenarioFile('needs-18789-b', 18789), path.join(out, 'b.json')], { BBX_ALLOW_18789: '' });
  collectorCases.push(['变量不是 1 也跳过', empty.status === 6]);
  const wrong = run('run.mjs', ['ts', scenarioFile('wrong-port', 18790), path.join(out, 'c.json')], { BBX_ALLOW_18789: '1' });
  collectorCases.push(['gatewayPort 只能是 18789', wrong.status === 2 && /may only be 18789/.test(wrong.stderr)]);
  const check = run('check.mjs', ['--out', out, scenarioFile('needs-18789-c', 18789)]);
  collectorCases.push(['check.mjs 记 SKIPPED、退出码 0', check.status === 0 && /needs-18789-c\s+SKIPPED/.test(check.stdout)]);
  collectorCases.push(['汇总行报出跳过个数', /0\/0 SAME，另有 1 个 SKIPPED/.test(check.stdout)]);
  // 假 HTTP 代理（httpProxies）：真跑一次采集（不起被测进程，只用 raw 步骤当客户端），所以用调用方的槽位。
  const proxyFile = path.join(dir, 'proxy-selftest.json');
  fs.writeFileSync(proxyFile, JSON.stringify({ name: 'proxy-selftest', server: false, settleMs: 0,
    httpProxies: { P: 38415, R: { port: 38416, status: 407 } },
    steps: [
      { raw: { port: 38415, readMs: 1500, send: [{ text: 'CONNECT [::1]:38412 HTTP/1.1\r\nhost: [::1]:38412\r\n\r\n' }, { wait: 200 },
        { text: 'POST /v1/chat/completions HTTP/1.1\r\nhost: [::1]:38412\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}' }] } },
      { proxyLog: true },
      { raw: { port: 38415, readMs: 500, send: [{ text: 'CONNECT example.com:443 HTTP/1.1\r\nhost: example.com:443\r\n\r\n' }] } },
      { raw: { port: 38416, readMs: 500, send: [{ text: 'CONNECT [::1]:38412 HTTP/1.1\r\n\r\n' }] } },
    ] }));
  const slot = process.env.BBX_SLOT || '0';
  const proxyRun = run('run.mjs', ['ts', proxyFile, path.join(out, 'proxy.json')], { BBX_SLOT: slot });
  let got = null; try { got = JSON.parse(fs.readFileSync(path.join(out, 'proxy.json'), 'utf8')); } catch {}
  const mockPort = String(38412 + 10 * Number(slot));
  collectorCases.push(['假代理：采集跑完', proxyRun.status === 0 && got !== null]);
  collectorCases.push(['假代理：CONNECT 记下目标与头，按段取出', got?.steps[1]?.proxyLog?.P?.length === 1 && got.steps[1].proxyLog.P[0].method === 'CONNECT'
    && got.steps[1].proxyLog.P[0].target === `[::1]:${mockPort}` && got.steps[1].proxyLog.P[0].headers.host === `[::1]:${mockPort}`]);
  collectorCases.push(['假代理：隧道接到假 provider', got?.steps[0]?.raw?.http?.status === 'HTTP/1.1 200 Connection Established' && (got?.provider?.default ?? []).length === 1]);
  collectorCases.push(['假代理：非本机目标回 502、不外连', / 502 /.test(got?.steps[2]?.raw?.http?.status ?? '') && got?.proxies?.P?.[0]?.target === 'example.com:443']);
  collectorCases.push(['假代理：写了 status 的代理拒绝 CONNECT', / 407 /.test(got?.steps[3]?.raw?.http?.status ?? '') && got?.proxies?.R?.length === 1]);
  // files 的 $file（repeat 拼大串、mode 设权限）、pathDir（假可执行文件排在 PATH 最前）、{PATH} 占位符、
  // 落盘里超过 16 MiB 的文件只记长度和 sha256：用一个假基线（只有 dist/src/cli/pilotdeck.js）真跑一次 cli 步骤。
  const fakeBase = path.join(dir, 'fake-base');
  fs.mkdirSync(path.join(fakeBase, 'dist/src/cli'), { recursive: true });
  fs.writeFileSync(path.join(fakeBase, 'dist/src/cli/pilotdeck.js'), [
    "const { execFileSync } = require('node:child_process');",
    "process.stdout.write(execFileSync('bbx-fake', ['one', 'two'], { encoding: 'utf8' }));",
    "process.stdout.write('orig:' + process.env.BBX_ORIG_PATH + '\\n');",
    "execFileSync('mkfifo', ['pipe']);",
  ].join('\n'));
  const toolsFile = path.join(dir, 'tools-selftest.json');
  fs.writeFileSync(toolsFile, JSON.stringify({ name: 'tools-selftest', server: false, settleMs: 0, pathDir: 'bin',
    env: { BBX_ORIG_PATH: '{PATH}', BBX_MARK: 'm1' },
    files: {
      'bin/bbx-fake': { $file: { text: '#!/bin/sh\nprintf "argv:%s\\n" "$*" > {PROJECT}/fake-argv.txt\nprintf "mark:%s\\n" "$BBX_MARK" >> {PROJECT}/fake-argv.txt\necho "head:${PATH%%:*}"\n', mode: '755' } },
      'proj/small.txt': { $file: { prefix: '<', text: 'ab', repeat: 3, suffix: '>', mode: '600' } },
      'proj/big.txt': { $file: { text: 'x', repeat: 17 * 1024 * 1024 } },
      'proj/big-lines.jsonl': { $file: { text: '{"n":1}\n', repeat: 2306868, suffix: '{"n":2}\nlast\n' } },
      'proj/plain.json': { $file: 'not-a-spec', other: 1 },
    },
    steps: [{ cli: [] }] }));
  const toolsRun = run('run.mjs', ['ts', toolsFile, path.join(out, 'tools.json')], { BBX_SLOT: slot, BBX_BASELINE: fakeBase });
  let tools = null; try { tools = JSON.parse(fs.readFileSync(path.join(out, 'tools.json'), 'utf8')); } catch {}
  const toolsRoot = `/private/tmp/pdrs-bb${Number(slot) ? slot : 'x'}/root`;
  const stdout = tools?.steps?.[0]?.cli?.stdout ?? '';
  collectorCases.push(['$file / pathDir：采集跑完', toolsRun.status === 0 && tools !== null]);
  collectorCases.push(['pathDir：假程序按 PATH 找到、排在最前', stdout.startsWith(`head:${process.platform === 'darwin' ? '/private/tmp' : '/tmp/private'}/${Number(slot) ? `pdrs-bb${slot}` : 'pdrs-bbx'}/root/bin\n`)]);
  collectorCases.push(['pathDir：假程序写下的 argv 与环境变量进落盘', tools?.tree?.['/proj/fake-argv.txt']?.text === 'argv:one two\nmark:m1\n']);
  collectorCases.push(['$file：mode 设权限', tools?.tree?.['/proj/small.txt']?.mode === '600']);
  collectorCases.push(['{PATH}：换成采集器自己的 PATH（不含 pathDir）', stdout.includes(`orig:${process.env.PATH}\n`)]);
  collectorCases.push(['$file：prefix + text×repeat + suffix', tools?.tree?.['/proj/small.txt']?.text === '<ababab>']);
  collectorCases.push(['落盘：超过 16 MiB 的文件只记长度和 sha256', tools?.tree?.['/proj/big.txt']?.largeBytes === 17 * 1024 * 1024 && /^[0-9a-f]{64}$/.test(tools?.tree?.['/proj/big.txt']?.sha256 ?? '') && tools?.tree?.['/proj/big.txt']?.text === undefined]);
  collectorCases.push(['不止一个键的对象仍按 JSON 内容写', tools?.tree?.['/proj/plain.json']?.json?.other === 1]);
  collectorCases.push(['落盘：命名管道只记种类、不去读', tools?.tree?.['/proj/pipe']?.special === 'fifo']);
  collectorCases.push(['落盘：大文件记末尾 64 KiB 的完整行', JSON.stringify(tools?.tree?.['/proj/big-lines.jsonl']?.tail?.slice(-2)) === '[{"json":{"n":2}},{"text":"last"}]' && tools.tree['/proj/big-lines.jsonl'].tail.length < 9000]);
  // httpsPort：本机 HTTPS 前端转给假 provider；假基线用 NODE_EXTRA_CA_CERTS={HTTPS_CA} 信任测试 CA 后 fetch 它。
  const httpsBase = path.join(dir, 'https-base');
  fs.mkdirSync(path.join(httpsBase, 'dist/src/cli'), { recursive: true });
  fs.writeFileSync(path.join(httpsBase, 'dist/src/cli/pilotdeck.js'),
    "fetch(process.env.BBX_URL).then(async (r) => console.log('status:' + r.status + ' body:' + (await r.text()).length), (e) => console.log('error:' + (e.cause?.code ?? e.message)));");
  const httpsFile = path.join(dir, 'https-selftest.json');
  fs.writeFileSync(httpsFile, JSON.stringify({ name: 'https-selftest', server: false, settleMs: 0, httpsPort: 38419,
    env: { NODE_EXTRA_CA_CERTS: '{HTTPS_CA}', BBX_URL: '{HTTPS}/page' }, steps: [{ cli: [] }, { cli: [], env: { NODE_EXTRA_CA_CERTS: null } }] }));
  const httpsRun = run('run.mjs', ['ts', httpsFile, path.join(out, 'https.json')], { BBX_SLOT: slot, BBX_BASELINE: httpsBase });
  let httpsGot = null; try { httpsGot = JSON.parse(fs.readFileSync(path.join(out, 'https.json'), 'utf8')); } catch {}
  collectorCases.push(['httpsPort：信任测试 CA 时经 TLS 到达假 provider', httpsRun.status === 0 && /^status:200 /.test(httpsGot?.steps?.[0]?.cli?.stdout ?? '') && (httpsGot?.provider?.default ?? []).length === 1]);
  collectorCases.push(['httpsPort：不信任时证书校验失败', /^error:/.test(httpsGot?.steps?.[1]?.cli?.stdout ?? '')]);
  // 规则的 url（按请求路径分）与 {status, body} 的 delayMs：两个没有请求体的 GET 只能按路径分到各自的规则。
  const urlBase = path.join(dir, 'url-base');
  fs.mkdirSync(path.join(urlBase, 'dist/src/cli'), { recursive: true });
  fs.writeFileSync(path.join(urlBase, 'dist/src/cli/pilotdeck.js'), [
    "(async () => { for (const p of ['/mod-a/m', '/mod-b/m', '/other']) {",
    "  const t = Date.now(); const r = await fetch(process.env.BBX_MOCK + p);",
    "  console.log(p + ':' + r.status + ':' + (await r.text()) + ':' + (Date.now() - t >= 280 ? 'slow' : 'fast')); } })();",
  ].join('\n'));
  const urlFile = path.join(dir, 'url-selftest.json');
  fs.writeFileSync(urlFile, JSON.stringify({ name: 'url-selftest', server: false, settleMs: 0, env: { BBX_MOCK: '{MOCK}' },
    provider: [
      { name: 'ra', url: '/mod-a/', reply: { status: 200, body: { a: 1 } } },
      { name: 'rb', url: '/mod-b/', reply: { status: 201, body: { b: 1 }, delayMs: 300 } },
    ],
    steps: [{ cli: [] }] }));
  const urlRun = run('run.mjs', ['ts', urlFile, path.join(out, 'url.json')], { BBX_SLOT: slot, BBX_BASELINE: urlBase });
  let urlGot = null; try { urlGot = JSON.parse(fs.readFileSync(path.join(out, 'url.json'), 'utf8')); } catch {}
  const urlOut = urlGot?.steps?.[0]?.cli?.stdout ?? '';
  collectorCases.push(['规则 url：按请求路径分到各自的规则', urlRun.status === 0 && urlOut.includes('/mod-a/m:200:{"a":1}:fast')
    && (urlGot?.provider?.ra ?? []).length === 1 && (urlGot?.provider?.rb ?? []).length === 1 && (urlGot?.provider?.default ?? []).length === 1]);
  collectorCases.push(['{status, body} 的 delayMs：等够了再回', urlOut.includes('/mod-b/m:201:{"b":1}:slow')]);
}
// 规则 11 的计数：差异报告头部写出每侧摘掉的行数。
{
  fs.writeFileSync(path.join(dir, 'a.json'), JSON.stringify(frame({ stdout: '[gateway:turn-timing] {"totalMs":250}\nok\n[gateway:turn-timing] {"releaseMs":300}' })));
  fs.writeFileSync(path.join(dir, 'b.json'), JSON.stringify(frame({ stdout: 'ok\n' })));
  const r = spawnSync(process.execPath, [path.join(here, 'diff.mjs'), path.join(dir, 'a.json'), path.join(dir, 'b.json')], { encoding: 'utf8' });
  collectorCases.push(['比较器：报告头部写出两侧摘掉的计时行数', /摘掉 a 侧 2 行、b 侧 0 行/.test(r.stdout) && /SAME$/.test(r.stdout.trim())]);
}
for (const [name, ok] of collectorCases) if (!ok) { bad++; console.log(`不符合预期  采集器：${name}`); }
if (!process.env.KEEP) fs.rmSync(dir, { recursive: true, force: true });
const total = cases.length + collectorCases.length;
console.log(`比较器与采集器自测：${total - bad}/${total} 符合预期`);
process.exit(bad ? 1 : 0);
