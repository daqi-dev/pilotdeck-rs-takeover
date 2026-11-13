#!/usr/bin/env node
// 进度总表：从对拍产物、表面清单、功能清单、问题台账、审阅记录和检查结果算出 state/status.md 与 state/out-of-scope.md。
// 不跑对拍，只读产物；同样的输入生成逐字节相同的输出（生成时间可用 --now 固定）。
// 表面项是否被碰到的判定与 coverage.mjs --surface 共用 lib/surface.mjs，口径相同。
//
// 用法：node scripts/bbx/status.mjs [参数]
//   --out DIR          对拍产物目录（check.mjs 的 --out），缺省 _build/bbx
//   --out-label TEXT   总表里显示的产物目录名（缺省为 DIR 相对仓库的路径）
//   --round-note TEXT  写进文件头的产物来历说明（哪台机器、哪个检出；产物里读不到的由人给）
//   --now TEXT         生成时间（缺省当前 UTC 时间）
//   --commit TEXT      仓库提交号（缺省 git rev-parse HEAD 的前 9 位）
//   --csv DIR          另把「按功能域」「按子能力」「缺口清单」导出成 UTF-8 带 BOM 的 CSV
//   --status FILE / --oos FILE   输出位置，缺省 state/status.md、state/out-of-scope.md
//   --stdout           只把总表打印到标准输出，不写文件
//   输入位置（测试用）：--scenarios DIR --surface FILE --features FILE --issues FILE --audits FILE
//                      --checks FILE --delivery FILE --src DIR
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { scenarioNames, loadScenarioTexts, surfaceTable } from './lib/surface.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '../..');
const argv = process.argv.slice(2);
const opt = (name, dflt) => { const i = argv.indexOf(name); return i >= 0 ? argv[i + 1] : dflt; };
const flag = (name) => argv.includes(name);
const P = {
  out: path.resolve(opt('--out', path.join(repo, '_build/bbx'))),
  scenarios: path.resolve(opt('--scenarios', path.join(here, 'scenarios'))),
  surface: path.resolve(opt('--surface', path.join(here, 'surface.json'))),
  features: path.resolve(opt('--features', path.join(here, 'features.json'))),
  issues: path.resolve(opt('--issues', path.join(repo, 'state/issues.md'))),
  audits: path.resolve(opt('--audits', path.join(repo, 'state/audits.md'))),
  checks: path.resolve(opt('--checks', path.join(repo, 'state/check-results.json'))),
  delivery: path.resolve(opt('--delivery', path.join(repo, 'specs/delivery/README.md'))),
  src: path.resolve(opt('--src', path.join(repo, 'src'))),
  status: path.resolve(opt('--status', path.join(repo, 'state/status.md'))),
  oos: path.resolve(opt('--oos', path.join(repo, 'state/out-of-scope.md'))),
};
const rel = (p) => { const r = path.relative(repo, p); return r && !r.startsWith('..') ? r : p; };
const now = opt('--now', new Date().toISOString().replace(/\.\d+Z$/, 'Z'));
const commit = opt('--commit', (() => { try { return execFileSync('git', ['-C', repo, 'rev-parse', '--short=9', 'HEAD'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim(); } catch { return '未知'; } })());
const readJson = (f) => JSON.parse(fs.readFileSync(f, 'utf8'));
const cmp = (a, b) => (a < b ? -1 : a > b ? 1 : 0);
const md = (s) => String(s ?? '').replace(/\|/g, '\\|').replace(/\n/g, ' ');

// ── 功能清单与归属规则 ──
const features = readJson(P.features);
const domains = features.domains;
const domainOf = new Map(); const capInfo = new Map();
for (const d of domains) for (const c of d.caps) { domainOf.set(c.id, d.id); capInfo.set(c.id, c); }
const domainIds = new Set(domains.map((d) => d.id));
const problems = []; // 清单本身的问题：引用了不存在的子能力等
const checkCap = (cap, where) => { if (!capInfo.has(cap)) problems.push(`${where} 引用了不存在的子能力 ${cap}`); };

const surfaceRules = [];
for (const [cap, methods] of Object.entries(features.methodCaps ?? {})) {
  if (cap === 'about') continue;
  checkCap(cap, 'methodCaps');
  surfaceRules.push({ dims: ['ws-methods', 'ws-params'], re: new RegExp(`^(ws:)?(${methods.join('|')})(\\.|$)`), cap });
}
for (const r of features.surfaceRules ?? []) { checkCap(r.cap, `surfaceRules ${r.match}`); surfaceRules.push({ dims: r.dims ?? [], re: new RegExp(r.match), cap: r.cap }); }
const surfaceEx = new Map();
for (const e of features.surfaceExceptions ?? []) { checkCap(e.cap, `surfaceExceptions ${e.id}`); surfaceEx.set(`${e.dim}\u0000${e.id}`, e.cap); }
const assignSurface = (dim, id) => {
  const ex = surfaceEx.get(`${dim}\u0000${id}`);
  if (ex) return { cap: ex, how: 'exception' };
  const r = surfaceRules.find((x) => (!x.dims.length || x.dims.includes(dim)) && x.re.test(id));
  return r ? { cap: r.cap, how: 'rule' } : { cap: null, how: null };
};
const scenarioRules = (features.scenarioRules ?? []).map((r) => { r.caps.forEach((c) => checkCap(c, `scenarioRules ${r.match}`)); return { re: new RegExp(r.match), caps: r.caps }; });
const scenarioEx = new Map();
for (const e of features.scenarioExceptions ?? []) { e.caps.forEach((c) => checkCap(c, `scenarioExceptions ${e.name}`)); scenarioEx.set(e.name, e.caps); }
const assignScenario = (name) => {
  if (scenarioEx.has(name)) return { caps: [...new Set(scenarioEx.get(name))].sort(), how: 'exception' };
  const caps = new Set(); for (const r of scenarioRules) if (r.re.test(name)) r.caps.forEach((c) => caps.add(c));
  return caps.size ? { caps: [...caps].sort(), how: 'rule' } : { caps: [], how: null };
};
const upstreamGroups = features.upstreamTests?.groups ?? [];
for (const g of upstreamGroups) g.caps.forEach((c) => checkCap(c, `upstreamTests ${g.group}`));

// ── 场景与表面项 ──
const names = scenarioNames(P.scenarios).sort();
const texts = loadScenarioTexts(P.scenarios, P.out, names);
const verdict = Object.fromEntries(texts.map((t) => [t.name, t.verdict]));
const scenarioJson = Object.fromEntries(names.map((n) => [n, readJson(path.join(P.scenarios, `${n}.json`))]));
const surface = readJson(P.surface);
const table = surfaceTable(surface, texts);
const items = []; // {dim, id, where, note, skip, status, hits, cap, how}
for (const row of table) for (const j of row.judged) items.push({ dim: row.dim, id: j.item.id, where: j.item.where, note: j.item.note, skip: j.item.skip, status: j.status, hits: j.hits, ...assignSurface(row.dim, j.item.id) });
const scen = names.map((n) => ({ name: n, verdict: verdict[n], ...assignScenario(n) }));

// ── 产物这一轮的情况 ──
function roundInfo() {
  if (!fs.existsSync(P.out)) return { exists: false };
  const files = fs.readdirSync(P.out).filter((f) => f.endsWith('.diff.txt')).sort();
  const platforms = {}; const hours = {}; let min = null, max = null;
  for (const f of files) {
    const full = path.join(P.out, f);
    const t = fs.statSync(full).mtime.toISOString();
    if (min === null || t < min) min = t; if (max === null || t > max) max = t;
    const h = t.slice(0, 13) + ':00Z'; hours[h] = (hours[h] ?? 0) + 1;
    const head = fs.readFileSync(full, 'utf8').split('\n', 1)[0];
    const plat = /\/home\/|\/tmp\/private\//.test(head) ? 'Linux' : /\/Users\/|\/private\/tmp\//.test(head) ? 'macOS' : '认不出';
    platforms[plat] = (platforms[plat] ?? 0) + 1;
  }
  const inOut = new Set(files.map((f) => f.slice(0, -'.diff.txt'.length)));
  const stale = [...inOut].filter((n) => !verdict[n]).sort();
  const notRun = names.filter((n) => !inOut.has(n));
  return { exists: true, count: files.length, min, max, hours, platforms, stale, notRun };
}
const round = roundInfo();

// ── 交付标准里「允许不一致的清单」 ──
const allowed = new Set();
if (fs.existsSync(P.delivery)) {
  const text = fs.readFileSync(P.delivery, 'utf8');
  const sec = text.split(/^## /m).find((s) => s.startsWith('允许不一致的清单'));
  if (sec) for (const line of sec.split('\n')) { const m = /^\|\s*`([^`]+)`/.exec(line); if (m) allowed.add(m[1]); }
}

// ── 台账：每张表的「功能域」列 ──
function parseTables(file) {
  const out = []; if (!fs.existsSync(file)) return out;
  let section = ''; let header = null;
  for (const line of fs.readFileSync(file, 'utf8').split('\n')) {
    if (line.startsWith('## ')) { section = line.slice(3).trim(); header = null; continue; }
    if (!line.startsWith('|')) { header = null; continue; }
    const cells = line.replace(/^\||\|$/g, '').split(/(?<!\\)\|/).map((c) => c.trim());
    if (!header) { header = cells; continue; }
    if (cells.every((c) => /^:?-+:?$/.test(c))) continue;
    out.push({ section, row: Object.fromEntries(header.map((h, i) => [h, cells[i] ?? ''])) });
  }
  return out;
}
const LEVELS = ['1', '2', '3', '4', '尺子'];
const issues = []; const issueWarnings = [];
for (const { section, row } of parseTables(P.issues)) {
  const level = /^([1-4]) /.exec(section)?.[1] ?? (section.startsWith('尺子') ? '尺子' : null);
  if (!level || !('功能域' in row)) continue;
  if ((row['状态'] ?? '').trim() === '—') continue; // 占位行（「无开着的」）
  const toks = row['功能域'].replace(/`/g, '').split(/[\s,，、]+/).filter(Boolean);
  const doms = [];
  for (const t of toks) { if (t === '*' || domainIds.has(t)) doms.push(t); else issueWarnings.push(`「${row['问题'].slice(0, 30)}…」的功能域 ${t}`); }
  if (!doms.length) issueWarnings.push(`「${row['问题'].slice(0, 30)}…」没有填功能域`);
  issues.push({ level, domains: doms, title: row['问题'] });
}
const issueCount = (dom) => Object.fromEntries(LEVELS.map((l) => [l, issues.filter((x) => x.level === l && x.domains.includes(dom)).length]));

// ── 审阅记录 ──
const audits = [];
for (const { row } of parseTables(P.audits)) {
  if (!('日期' in row) || !('类型' in row)) continue;
  const date = /\d{4}-\d{2}-\d{2}/.exec(row['日期'])?.[0]; if (!date) continue;
  const covKey = Object.keys(row).find((k) => k.startsWith('覆盖'));
  const toks = [...(row[covKey] ?? '').matchAll(/`([^`]+)`/g)].map((m) => m[1]);
  const caps = new Set(), doms = new Set();
  for (const t of toks) { if (t === '*') domainIds.forEach((d) => doms.add(d)); else if (domainIds.has(t)) doms.add(t); else if (capInfo.has(t)) caps.add(t); }
  const findKey = Object.keys(row).find((k) => k.startsWith('发现'));
  audits.push({ date, type: row['类型'].replace(/`/g, ''), who: row['执行者'] ?? '', doms, caps, findings: row[findKey] ?? '' });
}
const isBlind = (a) => /盲审|缺口审查/.test(a.type);
const isCode = (a) => /代码审阅/.test(a.type);
const covers = (a, dom, cap) => a.doms.has(dom) || (cap ? a.caps.has(cap) : [...a.caps].some((c) => domainOf.get(c) === dom));
const latest = (pred, dom, cap) => audits.filter((a) => pred(a) && covers(a, dom, cap)).sort((x, y) => cmp(y.date, x.date))[0];
const auditCell = (a) => (a ? `${a.date}（${a.type}）` : '—');

// ── 检查结果、占位与透明代理（src/） ──
const checks = fs.existsSync(P.checks) ? readJson(P.checks) : null;
function walkRs(dir, acc = []) {
  if (!fs.existsSync(dir)) return acc;
  for (const e of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => cmp(a.name, b.name))) {
    if (e.name.startsWith('.')) continue;
    const full = path.join(dir, e.name);
    if (e.isDirectory()) { if (!['target', 'tests', 'benches', 'examples', 'fixtures'].includes(e.name)) walkRs(full, acc); }
    else if (e.name.endsWith('.rs') && !/^(tests?|.*_tests?|tests?_.*|test_.*)\.rs$/.test(e.name)) acc.push(full);
  }
  return acc;
}
const rsFiles = walkRs(P.src);
const PLACEHOLDERS = { 'unimplemented!': /\bunimplemented!\s*\(/, 'todo!': /\btodo!\s*\(/, NOT_IMPLEMENTED: /NOT_IMPLEMENTED/ };
const PROXY = /\b(?:var|var_os)\(\s*"PILOTDECK_(?:PROXY_UPSTREAM|PROXY_LISTEN|GATEWAY_MODE)"|dist\/src\/cli\/pilotdeck\.js|\bstruct\s+Proxy\b/;
const placeholderHits = Object.fromEntries(Object.keys(PLACEHOLDERS).map((k) => [k, []]));
const proxyHits = [];
for (const f of rsFiles) {
  const lines = fs.readFileSync(f, 'utf8').split('\n');
  lines.forEach((line, i) => {
    if (/^\s*\/\//.test(line)) return;
    const at = `${path.relative(P.src, f)}:${i + 1}`;
    for (const [k, re] of Object.entries(PLACEHOLDERS)) if (re.test(line)) placeholderHits[k].push(at);
    if (PROXY.test(line)) proxyHits.push(at);
  });
}

// ── 汇总 ──
const scenOf = (pred) => scen.filter(pred);
const capRow = (cap) => {
  const ss = scenOf((s) => s.caps.includes(cap)); const it = items.filter((x) => x.cap === cap);
  return {
    scenarios: ss.length, same: ss.filter((s) => s.verdict === 'SAME').length,
    items: it.length, touched: it.filter((x) => x.status === 'same').length, diffOnly: it.filter((x) => x.status === 'diff').length,
    missing: it.filter((x) => x.status === 'missing').length, skip: it.filter((x) => x.status === 'skip').length,
    upstream: upstreamGroups.filter((g) => g.caps.includes(cap)).map((g) => g.group),
  };
};
const domRow = (dom) => {
  const caps = domains.find((d) => d.id === dom).caps.map((c) => c.id);
  const ss = scenOf((s) => s.caps.some((c) => caps.includes(c))); const it = items.filter((x) => caps.includes(x.cap));
  return {
    caps: caps.length, scenarios: ss.length, same: ss.filter((s) => s.verdict === 'SAME').length,
    items: it.length, touched: it.filter((x) => x.status === 'same').length, diffOnly: it.filter((x) => x.status === 'diff').length,
    missing: it.filter((x) => x.status === 'missing').length, skip: it.filter((x) => x.status === 'skip').length,
    issues: issueCount(dom), blind: latest(isBlind, dom), code: latest(isCode, dom),
  };
};
const totals = { items: items.length, same: 0, diff: 0, missing: 0, skip: 0 };
for (const x of items) totals[x.status]++;
const vcount = (v) => scen.filter((s) => s.verdict === v).length;
const nonSame = scen.filter((s) => s.verdict !== 'SAME');
const tagOf = (s) => allowed.has(s.name) ? '允许不一致的清单' : (scenarioJson[s.name].accepted ?? []).length ? '场景登记了 accepted（登记以外仍有差异）' : '其余';

// ── 写 status.md ──
const L = [];
const p = (...xs) => L.push(...xs);
p('# 进度总表（`dev/takeover`）', '');
p('> 本文件由 `node scripts/bbx/status.mjs` 生成，不要手改。改功能清单改 `scripts/bbx/features.json`，改问题改 `state/issues.md`，改审阅记录改 `state/audits.md`，改检查结果改 `state/check-results.json`，然后重新生成。', '');
p(`- 生成时间：${now}`);
p(`- 仓库提交：\`${commit}\`（场景、表面清单、功能清单、台账都取自这个提交的工作区）`);
p(`- 对拍产物：\`${opt('--out-label', rel(P.out))}\``);
if (opt('--round-note')) p(`- 产物来历（人给的说明）：${opt('--round-note')}`);
if (!round.exists) p('- 产物目录不存在：场景结论全部是「未跑」。');
else {
  p(`- 产物里的场景结果：${round.count} 个 \`.diff.txt\`；修改时间 ${round.min} 至 ${round.max}（UTC）；按小时：${Object.entries(round.hours).sort().map(([h, n]) => `${h} ${n} 个`).join('，')}`);
  p(`- 采集平台（从 diff 文件头里的产物路径判断）：${Object.entries(round.platforms).sort().map(([k, n]) => `${k} ${n} 个`).join('，')}`);
  p(`- 产物里有、当前提交没有的场景 ${round.stale.length} 个${round.stale.length ? `：${round.stale.map((n) => `\`${n}\``).join('、')}` : ''}；当前提交有、产物里没有的（记为「未跑」）${round.notRun.length} 个${round.notRun.length ? `：${round.notRun.map((n) => `\`${n}\``).join('、')}` : ''}`);
  p('- 产物不记录被测提交号，也不记录是哪一轮；修改时间跨度大或分成几段时，可能混合了多轮。');
}
if (problems.length) { p('', '**功能清单本身的问题**：', ...problems.map((x) => `- ${x}`)); }
p('');

// 八条标准
const ck = checks?.fourCommands; const up = checks?.upstreamTests;
const fourText = ck ? `${ck.date} 在 ${ck.machine} 上、提交 \`${ck.commit}\`：fmt ${ck.fmt}，clippy ${ck.clippy}，nextest ${ck.nextest.result}（${ck.nextest.passed}/${ck.nextest.total}），文档测试 ${ck.doctest}` : '没有数据（`state/check-results.json` 缺 `fourCommands`）';
const phCount = Object.values(placeholderHits).reduce((a, b) => a + b.length, 0);
const neverBlind = domains.filter((d) => !latest(isBlind, d.id)).map((d) => d.id);
const strictBlind = audits.filter((a) => /盲审/.test(a.type)).length;
p('## 八条标准', '');
p('| 条 | 标准 | 现在（由数据算出） |', '|---|---|---|');
p(`| 1 | 全量对拍稳定一致 | 场景 ${names.length} 个：SAME ${vcount('SAME')}，DIFF ${vcount('DIFF')}，INVALID ${vcount('INVALID')}，未跑 ${vcount('未跑')}。不是 SAME 的里属于允许清单 ${nonSame.filter((s) => tagOf(s) === '允许不一致的清单').length} 个、登记了 accepted ${nonSame.filter((s) => tagOf(s).startsWith('场景登记')).length} 个、其余 ${nonSame.filter((s) => tagOf(s) === '其余').length} 个（逐个见下）。连续 3 轮稳定：需要连续 3 轮的产物，未提供 |`);
p(`| 2 | 表面覆盖完整 | 表面项 ${totals.items} 个，范围内 ${totals.items - totals.skip} 个：被 SAME 场景碰到 ${totals.same}，只被有差异的场景碰到 ${totals.diff}，没碰到 ${totals.missing}；标为不计 ${totals.skip}（理由表在 \`state/out-of-scope.md\`，待人过目） |`);
p(`| 3 | 盲审收敛 | 审阅记录 ${audits.length} 次，其中类型为「盲审」的 ${strictBlind} 次；${domains.length} 个功能域里从没有过盲审或缺口审查的 ${neverBlind.length} 个。各功能域最近一次见下表。「两次连续盲审覆盖全部区域且无 1、2 级新发现」：不满足 |`);
p('| 4 | 原版界面可用 | 没有数据：需要 `scripts/ui-smoke` 的当次结果和人亲手使用的记录 |');
p(`| 5 | 构建与测试全绿，没有占位 | 四条命令：${fourText}。\`src/\` 产品代码里 \`unimplemented!\` ${placeholderHits['unimplemented!'].length} 处、\`todo!\` ${placeholderHits['todo!'].length} 处、\`NOT_IMPLEMENTED\` ${placeholderHits.NOT_IMPLEMENTED.length} 处（共 ${phCount}，逐条见下；空 match 分支与转发给 TS 的兜底没有机械判据，未统计） |`);
p(`| 6 | TS gateway 退役 | 透明代理入口的机械判据命中 ${proxyHits.length} 处${proxyHits.length ? '（见下）' : ''}。进程树里没有 Node gateway 的证据：没有数据（需要全量对拍与界面冒烟期间的进程列表） |`);
p('| 7 | 性能报告 | 没有数据：需要发布构建下两侧的启动耗时、每轮开销、内存、并发会话的测量 |');
p('| 8 | 真实使用 | 没有数据：需要人在最终组合上完成的真实任务记录 |');
p('');
if (up) {
  p(`上游 sidecar 验收集（\`scripts/upstream-tests\`，挂在 ${[...new Set(upstreamGroups.flatMap((g) => g.caps))].sort().map((c) => `\`${c}\``).join('、')}）：${up.date} 在 ${up.machine} 上、提交 \`${up.commit}\`：${Object.entries(up.groups).map(([g, r]) => `${g} ${r.summary}`).join('；')}。来源：${up.source}`, '');
} else p('上游 sidecar 验收集：没有数据。', '');

p('### 第 1 条：不是 SAME 的场景', '');
if (!nonSame.length) p('（无）', '');
else { p('| 场景 | 结论 | 归类 | 子能力 |', '|---|---|---|---|'); for (const s of nonSame) p(`| \`${s.name}\` | ${s.verdict} | ${tagOf(s)} | ${s.caps.map((c) => `\`${c}\``).join(' ') || '归属不了'} |`); p(''); }
p('### 第 5 条：占位的逐处位置', '');
p('判据：`src/` 下的 `.rs`，跳过 `target`、`tests`、`benches`、`examples`、`fixtures` 目录和 `tests.rs`、`*_tests.rs`、`test_*.rs` 一类文件名，跳过整行注释；文件内的 `#[cfg(test)]` 模块没有排除。', '');
if (!phCount) p('（无）', '');
else for (const [k, hits] of Object.entries(placeholderHits)) for (const h of hits) p(`- \`${k}\`：\`${h}\``);
p('', '### 第 6 条：透明代理的判据', '');
p('判据（可机械检查，与第 5 条同样的文件范围）：读取透明代理的配置项（`var` / `var_os` 读 `PILOTDECK_PROXY_UPSTREAM`、`PILOTDECK_PROXY_LISTEN`、`PILOTDECK_GATEWAY_MODE`）、引用基线 gateway 入口脚本 `dist/src/cli/pilotdeck.js`、定义 `struct Proxy`。命中 0 处只说明旧入口不在，不说明运行时没有 Node 进程。', '');
p(proxyHits.length ? proxyHits.map((h) => `- \`${h}\``).join('\n') : '（命中 0 处）', '');

// 按功能域
p('## 按功能域', '');
p('「问题」按台账的级别 1 / 2 / 3 / 4 / 尺子分列；台账里功能域填 `*` 的横跨全部，单列在表末，不计入各域。', '');
p('| 功能域 | 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 问题 1/2/3/4/尺子 | 最近盲审 / 缺口审查 | 最近代码审阅 |', '|---|---|---|---|---|---|---|---|---|---|---|---|');
const domRows = domains.map((d) => ({ d, r: domRow(d.id) }));
for (const { d, r } of domRows) p(`| \`${d.id}\` ${d.name} | ${r.caps} | ${r.scenarios} | ${r.same} | ${r.items} | ${r.touched} | ${r.diffOnly} | ${r.missing} | ${r.skip} | ${LEVELS.map((l) => r.issues[l]).join('/')} | ${auditCell(r.blind)} | ${auditCell(r.code)} |`);
const star = issueCount('*');
p(`| 横跨（\`*\`） | | | | | | | | | ${LEVELS.map((l) => star[l]).join('/')} | | |`);
const unS = items.filter((x) => !x.cap); const unSc = scen.filter((s) => !s.caps.length);
p(`| 归属不了 | | ${unSc.length} | ${unSc.filter((s) => s.verdict === 'SAME').length} | ${unS.length} | ${unS.filter((x) => x.status === 'same').length} | ${unS.filter((x) => x.status === 'diff').length} | ${unS.filter((x) => x.status === 'missing').length} | ${unS.filter((x) => x.status === 'skip').length} | | | |`);
p('', '一个场景可以属于几个子能力，所以各域场景数之和大于场景总数；表面项每项只属于一个子能力。', '');
if (issueWarnings.length) p('台账里认不出的功能域：', ...issueWarnings.map((w) => `- ${w}`), '');

// 按子能力
p('## 按子能力', '');
const capRows = [];
for (const d of domains) {
  p(`### \`${d.id}\` ${d.name}`, '', d.about, '');
  p('| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |', '|---|---|---|---|---|---|---|---|---|---|---|---|');
  for (const c of d.caps) {
    const r = capRow(c.id); capRows.push({ d, c, r });
    p(`| \`${c.id}\` ${c.name} | ${r.scenarios} | ${r.same} | ${r.items} | ${r.touched} | ${r.diffOnly} | ${r.missing} | ${r.skip} | ${r.upstream.join(' ') || '—'} | ${auditCell(latest(isBlind, d.id, c.id))} | ${auditCell(latest(isCode, d.id, c.id))} | ${c.where.map((w) => `\`${w}\``).join(' ')} |`);
  }
  p('');
}

// 缺口清单
p('## 缺口清单', '', '这一节是「接下来派什么活」的来源。', '');
p('### 没被一致的场景碰到的表面项', '');
p('「没碰到」是没有场景碰到；「只被有差异的碰到」是碰到它的场景都不是 SAME（括号里是那些场景）。', '');
const gaps = [];
for (const d of domains) {
  const rows = items.filter((x) => (x.status === 'missing' || x.status === 'diff') && domainOf.get(x.cap) === d.id).sort((a, b) => cmp(a.cap, b.cap) || cmp(a.dim, b.dim) || cmp(a.id, b.id));
  if (!rows.length) continue;
  p(`#### \`${d.id}\` ${d.name}（${rows.length}）`, '');
  let cur = null;
  for (const x of rows) {
    if (x.cap !== cur) { cur = x.cap; p(`- \`${cur}\``); }
    p(`  - ${x.status === 'missing' ? '没碰到' : `只被有差异的碰到（${x.hits.map((h) => `\`${h}\``).join(' ')}）`}：\`${x.dim}\` \`${md(x.id)}\` — ${x.where}`);
    gaps.push({ kind: x.status === 'missing' ? '没碰到的表面项' : '只被有差异的场景碰到的表面项', domain: d.id, cap: x.cap, what: `${x.dim} ${x.id}`, detail: x.where });
  }
  p('');
}
const unGap = unS.filter((x) => x.status === 'missing' || x.status === 'diff');
if (unGap.length) { p('#### 归属不了的', ''); for (const x of unGap) { p(`- \`${x.dim}\` \`${md(x.id)}\` — ${x.where}`); gaps.push({ kind: '没碰到的表面项', domain: '', cap: '', what: `${x.dim} ${x.id}`, detail: x.where }); } p(''); }
p('### 没有任何场景的子能力', '');
const noScen = capRows.filter(({ r }) => !r.scenarios);
if (!noScen.length) p('（无）');
for (const { d, c } of noScen) { p(`- \`${c.id}\` ${c.name}`); gaps.push({ kind: '没有场景的子能力', domain: d.id, cap: c.id, what: c.name, detail: c.where.join(' ') }); }
const noSame = capRows.filter(({ r }) => r.scenarios && !r.same);
if (noSame.length) { p('', '有场景但没有一个一致的子能力：'); for (const { d, c } of noSame) { p(`- \`${c.id}\` ${c.name}`); gaps.push({ kind: '没有一致场景的子能力', domain: d.id, cap: c.id, what: c.name, detail: c.where.join(' ') }); } }
p('', '### 没审过的功能域', '');
p(`没有任何盲审或缺口审查记录的（${neverBlind.length}）：${neverBlind.map((x) => `\`${x}\``).join('、') || '无'}`);
for (const x of neverBlind) gaps.push({ kind: '没有盲审的功能域', domain: x, cap: '', what: domains.find((d) => d.id === x).name, detail: '' });
const neverCode = domains.filter((d) => !latest(isCode, d.id)).map((d) => d.id);
p('', `没有任何代码审阅记录的（${neverCode.length}）：${neverCode.map((x) => `\`${x}\``).join('、') || '无'}`);
for (const x of neverCode) gaps.push({ kind: '没有代码审阅的功能域', domain: x, cap: '', what: domains.find((d) => d.id === x).name, detail: '' });
p('', '### 归属不了的场景', '');
p(unSc.length ? unSc.map((s) => `- \`${s.name}\`（${s.verdict}）`).join('\n') : '（无）');
p('');

// 归属统计与对照
const howCount = (arr, key) => ({ rule: arr.filter((x) => x[key] === 'rule').length, ex: arr.filter((x) => x[key] === 'exception').length, none: arr.filter((x) => !x[key]).length });
const hs = howCount(items, 'how'); const hc = howCount(scen, 'how');
p('## 归属统计', '');
p(`- 表面项 ${items.length} 个：按规则归属 ${hs.rule}，例外 ${hs.ex}，归属不了 ${hs.none}。`);
p(`- 场景 ${scen.length} 个：按规则归属 ${hc.rule}，例外 ${hc.ex}，归属不了 ${hc.none}。`);
p('');
p('## 与 `coverage.mjs --surface` 的对照', '');
p(`两者用同一个判定（\`scripts/bbx/lib/surface.mjs\`）。对同一产物目录运行 \`node scripts/bbx/coverage.mjs --surface --out <产物目录>\` 应打印与下表相同的数字（场景 ${vcount('SAME')}/${names.length} SAME）。`, '');
p('| 维度 | 项数 | 被SAME场景碰到 | 只被有差异的场景碰到 | 没碰到 | 不计 |', '|---|---|---|---|---|---|');
for (const r of table) p(`| ${r.dim} | ${r.items} | ${r.same} | ${r.diff} | ${r.missing} | ${r.skip} |`);
p(`| 合计 | ${totals.items} | ${totals.same} | ${totals.diff} | ${totals.missing} | ${totals.skip} |`);
p('');
const statusText = L.join('\n');

// ── out-of-scope.md ──
const O = [];
O.push('# 不计入覆盖的表面项（交付标准第 2 条，待人过目）', '');
O.push('> 本文件由 `node scripts/bbx/status.mjs` 生成，不要手改。理由来自 `scripts/bbx/surface.json` 各项的 `skip` 字段；要改理由或改成计入，改那里再重新生成。', '');
O.push(`- 生成时间：${now}`, `- 仓库提交：\`${commit}\``, '');
const skipped = items.filter((x) => x.status === 'skip');
const noReason = skipped.filter((x) => typeof x.skip !== 'string' || !x.skip.trim());
O.push(`共 ${skipped.length} 项，其中没有理由的 ${noReason.length} 项。`, '');
O.push('## 没有理由的', '');
O.push(noReason.length ? noReason.map((x) => `- \`${x.dim}\` \`${md(x.id)}\`（${x.where}）`).join('\n') : '（无）', '');
for (const d of [...domains, { id: null, name: '归属不了' }]) {
  const rows = skipped.filter((x) => !noReason.includes(x) && (d.id ? domainOf.get(x.cap) === d.id : !x.cap));
  if (!rows.length) continue;
  O.push(`## ${d.id ? `\`${d.id}\` ` : ''}${d.name}（${rows.length}）`, '', '| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |', '|---|---|---|---|---|');
  for (const x of rows.sort((a, b) => cmp(a.cap ?? '', b.cap ?? '') || cmp(a.dim, b.dim) || cmp(a.id, b.id))) O.push(`| ${x.dim} | \`${md(x.id)}\` | ${x.cap ? `\`${x.cap}\`` : '—'} | ${md(x.where)} | ${md(x.skip)} |`);
  O.push('');
}
const oosText = O.join('\n');

if (flag('--stdout')) process.stdout.write(statusText);
else {
  fs.mkdirSync(path.dirname(P.status), { recursive: true });
  fs.writeFileSync(P.status, statusText);
  fs.writeFileSync(P.oos, oosText);
  console.log(`写入 ${rel(P.status)}、${rel(P.oos)}`);
}

// ── CSV ──
if (opt('--csv')) {
  const dir = path.resolve(opt('--csv')); fs.mkdirSync(dir, { recursive: true });
  const csv = (rows) => '﻿' + rows.map((r) => r.map((v) => { const s = String(v ?? ''); return /[",\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s; }).join(',')).join('\r\n') + '\r\n';
  fs.writeFileSync(path.join(dir, 'domains.csv'), csv([
    ['功能域', '名称', '子能力', '场景', '一致', '表面项', '被碰到', '只被有差异的碰到', '没碰到', '不计', '问题1', '问题2', '问题3', '问题4', '尺子', '最近盲审或缺口审查', '最近代码审阅'],
    ...domRows.map(({ d, r }) => [d.id, d.name, r.caps, r.scenarios, r.same, r.items, r.touched, r.diffOnly, r.missing, r.skip, ...LEVELS.map((l) => r.issues[l]), r.blind?.date ?? '', r.code?.date ?? '']),
  ]));
  fs.writeFileSync(path.join(dir, 'caps.csv'), csv([
    ['功能域', '子能力', '名称', '场景', '一致', '表面项', '被碰到', '只被有差异的碰到', '没碰到', '不计', '上游验收集', '基线位置'],
    ...capRows.map(({ d, c, r }) => [d.id, c.id, c.name, r.scenarios, r.same, r.items, r.touched, r.diffOnly, r.missing, r.skip, r.upstream.join(' '), c.where.join(' ')]),
  ]));
  fs.writeFileSync(path.join(dir, 'gaps.csv'), csv([['类别', '功能域', '子能力', '内容', '基线位置'], ...gaps.map((g) => [g.kind, g.domain, g.cap, g.what, g.detail])]));
  console.log(`CSV 写入 ${dir}`);
}
