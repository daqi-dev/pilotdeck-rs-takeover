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
const cases = [
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
fs.rmSync(dir, { recursive: true, force: true });
console.log(bad ? `比较器自测：${cases.length - bad}/${cases.length} 符合预期` : `比较器自测：${cases.length}/${cases.length} 符合预期`);
process.exit(bad ? 1 : 0);
