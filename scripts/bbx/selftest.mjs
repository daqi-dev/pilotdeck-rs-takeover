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
  ['毫秒时间戳整数对小数', frame({ t: 1794000000000 }), frame({ t: 1794000000000.5 }), 'DIFF'],
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
  ['别的锁文件只在一侧有', art([], { tree: { '/home/.pilotdeck/other.json.lock': { mode: '644', text: 'x' } } }), art([]), 'DIFF'],
  ['created 之外的 10 位整数', frame({ startedAt: 1794495673 }), frame({ startedAt: 1794495675 }), 'DIFF'],
  ['created 一侧不是时间戳', frame({ created: 1794495673 }), frame({ created: 12 }), 'DIFF'],
  ['警告之外的 stderr', frame({ stderr: '(node:1) ExperimentalWarning: x\n(Use `node --trace-warnings ...` to show where the warning was created)\nreal\n' }), frame({ stderr: 'other\n' }), 'DIFF'],

  ['带名字的数组少一个', frame({ tools: [{ name: 'a' }, { name: 'b' }] }), frame({ tools: [{ name: 'a' }] }), 'DIFF'],
  ['带名字的数组顺序不同', frame({ tools: [{ name: 'a' }, { name: 'b' }] }), frame({ tools: [{ name: 'b' }, { name: 'a' }] }), 'DIFF'],
  ['带名字的数组里同名元素内容不同', frame({ tools: [{ name: 'a', d: 1 }] }), frame({ tools: [{ name: 'a', d: 2 }] }), 'DIFF'],

  ['标题记录与循环开头的记录换位', jsonl([rec('accepted_input'), title, rec('step_started'), rec('model_request')]), jsonl([rec('accepted_input'), rec('step_started'), title, rec('model_request')]), 'SAME'],
  ['标题记录落到 model_request 之后', jsonl([rec('accepted_input'), title, rec('step_started'), rec('model_request')]), jsonl([rec('accepted_input'), rec('step_started'), rec('model_request'), title]), 'DIFF'],
  ['标题内容不同', jsonl([rec('accepted_input'), title]), jsonl([rec('accepted_input'), { ...title, metadata: { aiTitle: 'X', titleSource: 'provider' } }]), 'DIFF'],
  ['别的记录换位', jsonl([rec('accepted_input'), rec('step_started'), rec('model_request')]), jsonl([rec('accepted_input'), rec('model_request'), rec('step_started')]), 'DIFF'],
  ['少一条记录', jsonl([rec('accepted_input'), rec('step_started')]), jsonl([rec('accepted_input')]), 'DIFF'],

  ['always-on 事件文件里记录换位', art([], { tree: { '/home/.pilotdeck/always-on/projects/p/events.jsonl': { mode: '644', jsonl: [{ phase: 'a' }, { phase: 'b' }] } } }), art([], { tree: { '/home/.pilotdeck/always-on/projects/p/events.jsonl': { mode: '644', jsonl: [{ phase: 'b' }, { phase: 'a' }] } } }), 'SAME'],
  ['always-on 事件文件里少一条', art([], { tree: { '/home/.pilotdeck/always-on/projects/p/events.jsonl': { mode: '644', jsonl: [{ phase: 'a' }, { phase: 'b' }] } } }), art([], { tree: { '/home/.pilotdeck/always-on/projects/p/events.jsonl': { mode: '644', jsonl: [{ phase: 'a' }] } } }), 'DIFF'],
  ['always-on 事件文件里内容不同', art([], { tree: { '/home/.pilotdeck/always-on/projects/p/events.jsonl': { mode: '644', jsonl: [{ phase: 'a' }, { phase: 'b' }] } } }), art([], { tree: { '/home/.pilotdeck/always-on/projects/p/events.jsonl': { mode: '644', jsonl: [{ phase: 'a' }, { phase: 'c' }] } } }), 'DIFF'],
  ['别的 jsonl 文件里记录换位照报', art([], { tree: { '/home/.pilotdeck/other/events.jsonl': { mode: '644', jsonl: [{ phase: 'a' }, { phase: 'b' }] } } }), art([], { tree: { '/home/.pilotdeck/other/events.jsonl': { mode: '644', jsonl: [{ phase: 'b' }, { phase: 'a' }] } } }), 'DIFF'],
  ['计时行有无与数值', frame({ stdout: 'listening\n[gateway:turn-timing] {"totalMs":250}\nbye\n' }), frame({ stdout: 'listening\nbye\n' }), 'SAME'],
  ['计时行之外的输出', frame({ stdout: 'listening\n[gateway:turn-timing] {"totalMs":250}\nbye\n' }), frame({ stdout: 'listening\nBYE\n' }), 'DIFF'],
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
}
for (const [name, ok] of collectorCases) if (!ok) { bad++; console.log(`不符合预期  采集器：${name}`); }
fs.rmSync(dir, { recursive: true, force: true });
const total = cases.length + collectorCases.length;
console.log(`比较器与采集器自测：${total - bad}/${total} 符合预期`);
process.exit(bad ? 1 : 0);
