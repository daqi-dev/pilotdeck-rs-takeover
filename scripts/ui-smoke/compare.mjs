#!/usr/bin/env node
// ui-smoke · 把两侧的产物整理成可比较的形状，用 scripts/bbx/diff.mjs 比较。
// 用法：node scripts/ui-smoke/compare.mjs <输出目录> [a侧=ts] [b侧=rust]
//
// 整理规则（每条都是「基线自己连跑两次就会变」的东西，用 ts vs ts2 自比确认过）：
//   ui 侧（<侧>.ui.json → <侧>.ui.cmp.json）
//     - 去掉步骤的时间戳 t。
//     - WS 步骤里去掉广播帧 loading_progress / projects_updated / session-activity：它们由 ui/server 的
//       项目目录监视器和活动登记异步推送，落在哪一步、推几次随时序变。项目快照的内容由 GET /api/projects 步骤比较。
//   gateway 侧（<侧>.gateway.jsonl → <侧>.calls.json）
//     - 请求 id 去掉（两侧各自随机），应答与事件帧挂到所属请求下（同一请求的事件保持原顺序）。
//     - 按 method 分组，组内按发出顺序比较：ui/server 内部并发发请求，不同 method 之间的交错不稳，
//       同一 method 的先后由驱动顺序决定（ts vs ts2 自比稳定）。
//     - list_projects / list_sessions / describe_project 是 ui/server 刷新项目快照的轮询，次数和时机随目录监视器变
//       （Rust 一轮更慢时会多拍到回合中途的快照），单列为 refreshSet，只比去重后的 (method, params) 集合，
//       不比次数和应答；快照内容由 ui 侧 GET /api/projects 与 sessions 步骤比较。
//     - 通知帧（notification）按 name 分组、组内按到达顺序比较 payload。
//     - ui/server→gateway 的 HTTP 请求（/api/module-host/*）与响应配对，同样按「method」（HTTP 方法+路径）分组。
//   provider 侧（<侧>.provider.jsonl → <侧>.provider.cmp.json）：见 buildProvider。
// 另生成 <侧>.inventory.json：method → 次数、出现过的参数键、触发它的驱动步骤名（按请求发出时刻归步）。
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const [outDir, a = 'ts', b = 'rust'] = process.argv.slice(2);
if (!outDir) { console.error('usage: compare.mjs <outdir> [a] [b]'); process.exit(2); }

const BROADCAST = new Set(['loading_progress', 'projects_updated', 'session-activity']);
const REFRESH = new Set(['list_projects', 'list_sessions', 'describe_project']);
// ui/server 自己生成的随机值（与 gateway 无关）：memory dream 追踪 id 与临时目录、旧式附件目录的随机后缀、
// API key、Web Push VAPID 公钥、导出文件名里的时间戳。下载的 zip（含文件时间）整体记成 <zip>。
const UI_LOCAL_RANDOM = [
  [/dream_trace_[0-9a-f]{10}/g, 'dream_trace_<id>'],
  [/\.memory\.dream-[a-z0-9]+-[a-z0-9]+/g, '.memory.dream-<tmp>'],
  [/chat-attachments\/(\d{13})-[0-9a-f]+\//g, 'chat-attachments/$1-<rand>/'],
  [/ck_[0-9a-f]{64}/g, 'ck_<key>'],
  [/"publicKey": "[A-Za-z0-9_-]{80,}"/g, '"publicKey": "<vapid>"'],
  [/\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}-\d{3}Z\.json/g, '<iso>.json'],
];
const key = (c) => JSON.stringify([c.method, c.params ?? null]);

function stepOf(steps, at) {
  for (const s of steps) if (s.t && at >= s.t[0] && at <= s.t[1]) return s.name;
  const first = steps.find((s) => s.t);
  if (!first || at < first.t[0]) return '(startup)';
  const last = [...steps].reverse().find((s) => s.t);
  if (at > last.t[1]) return '(shutdown)';
  // 落在两步之间：归到前一步。
  let prev = '(startup)'; for (const s of steps) { if (s.t && s.t[1] < at) prev = s.name; }
  return prev;
}

function build(side) {
  const ui = JSON.parse(fs.readFileSync(path.join(outDir, `${side}.ui.json`), 'utf8'));
  const entries = fs.readFileSync(path.join(outDir, `${side}.gateway.jsonl`), 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l));
  // ui 侧
  const uiCmp = {
    routesHit: ui.routesHit,
    steps: ui.steps.map(({ t, ...s }) => (s.frames ? { ...s, frames: s.frames.filter((f) => !BROADCAST.has(f.type)) } : s)),
  };
  for (const s of uiCmp.steps) if (typeof s.body?.truncatedText === 'string' && s.body.truncatedText.startsWith('PK')) s.body = '<zip>';
  let text = JSON.stringify(uiCmp, null, 2);
  for (const [re, to] of UI_LOCAL_RANDOM) text = text.replace(re, to);
  fs.writeFileSync(path.join(outDir, `${side}.ui.cmp.json`), text);
  // gateway 侧
  const byId = new Map(); const all = []; const notifications = {}; const httpOpen = new Map(); const other = [];
  for (const e of entries) {
    if (e.kind === 'ws' && e.dir === 'up' && e.frame?.type === 'request') {
      const c = { step: stepOf(ui.steps, e.at), method: e.frame.method, params: e.frame.params, response: null, events: [] };
      all.push(c); byId.set(e.frame.id, c);
    } else if (e.kind === 'ws' && e.dir === 'down') {
      const f = e.frame; const c = f && byId.get(f.id);
      if (c) { const { id, ...rest } = f; if (f.type === 'response') c.response = rest; else c.events.push(rest); }
      else if (f?.type === 'notification') (notifications[f.name ?? '?'] ??= []).push(f.payload);
      else other.push({ dir: 'down', frame: f });
    } else if (e.kind === 'ws' && e.dir === 'up') {
      other.push({ dir: 'up', frame: e.frame });
    } else if (e.kind === 'http-req') {
      const h = { step: stepOf(ui.steps, e.at), method: `HTTP ${e.method} ${e.url}`, params: e.body, response: null, events: [] };
      all.push(h); httpOpen.set(e.conn, h);
    } else if (e.kind === 'http-res') {
      const h = httpOpen.get(e.conn); if (h) h.response = { status: e.status, contentType: e.headers?.['content-type'], body: e.body };
    } else if (e.kind === 'ws-handshake' || e.kind === 'ws-close') {
      other.push({ kind: e.kind, ...(e.status ? { status: e.status } : {}), ...(e.code ? { code: e.code, reason: e.reason } : {}) });
    }
  }
  // 按 method 分组，组内按发出顺序：不同 method 之间的交错不稳，同一 method 的先后由驱动顺序决定。
  const byMethod = {};
  for (const c of all) if (!REFRESH.has(c.method)) (byMethod[c.method] ??= []).push((({ step, method, ...rest }) => rest)(c));
  const sortedByMethod = Object.fromEntries(Object.keys(byMethod).sort().map((m) => [m, byMethod[m]]));
  const calls = { steps: sortedByMethod, refreshSet: [...new Set(all.filter((c) => REFRESH.has(c.method)).map(key))].sort(), notifications, other };
  fs.writeFileSync(path.join(outDir, `${side}.calls.json`), JSON.stringify(calls, null, 2));
  // 清单
  const inv = {};
  for (const c of all) {
    const m = (inv[c.method] ??= { count: 0, paramKeys: {}, steps: [] });
    m.count++;
    for (const k of Object.keys(c.params ?? {})) m.paramKeys[k] = (m.paramKeys[k] ?? 0) + 1;
    if (!m.steps.includes(c.step)) m.steps.push(c.step);
  }
  fs.writeFileSync(path.join(outDir, `${side}.inventory.json`), JSON.stringify(inv, null, 2));
}

// 假 provider 收到的请求：按命中规则分组、组内按到达顺序；去掉到达时刻与「中止后多久断开」的毫秒数（只留是否由对方断开）。
function buildProvider(side) {
  const p = path.join(outDir, `${side}.provider.jsonl`);
  const byRule = {};
  for (const l of fs.readFileSync(p, 'utf8').split('\n').filter(Boolean)) {
    const { at, afterMs, rule, ...rest } = JSON.parse(l);
    (byRule[rule] ??= []).push(rest);
  }
  fs.writeFileSync(path.join(outDir, `${side}.provider.cmp.json`), JSON.stringify({ steps: byRule }, null, 2));
}
for (const side of [a, b]) { build(side); buildProvider(side); }
let rc = 0;
for (const kind of ['ui.cmp', 'calls', 'provider.cmp']) {
  const fa = path.join(outDir, `${a}.${kind}.json`); const fb = path.join(outDir, `${b}.${kind}.json`);
  const r = spawnSync(process.execPath, [path.join(HERE, '../bbx/diff.mjs'), fa, fb, '--max', '600', '--self'], { encoding: 'utf8' });
  const out = path.join(outDir, `${a}-vs-${b}.${kind}.diff.txt`);
  fs.writeFileSync(out, r.stdout + r.stderr);
  const n = (r.stdout.match(/^ {2}\$/gm) ?? []).length;
  console.log(`[compare] ${kind}: ${r.status === 0 ? 'SAME' : `DIFF ${n}`} → ${out}`);
  if (r.status !== 0) rc = 1;
}
process.exit(rc);
