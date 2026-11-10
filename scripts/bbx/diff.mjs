#!/usr/bin/env node
// 黑盒对拍 · 比较器。
// 用法：node scripts/bbx/diff.mjs <a.json> <b.json> [--max N] [--quiet]
// 退出码：0 一致，1 有差异，2 用法错误。
//
// 归一化只有下面四条，都是「同一侧连跑两次也会变」的值，别的一律按原样比：
//   1. UUID            → <uuid:N>，N 是它在该侧输出里第一次出现的序号（保留「同一个 id」的关系）
//   2. ISO 时间戳      → <iso>
//   3. 毫秒时间戳      → <ms>（1.6e12 ~ 2.0e12 之间的整数）；带小数的记成 <ms.frac>，
//                        这样「一侧整数、一侧小数」仍然算差异
//   4. 进程号          → <pid>（键名为 pid / ownerPid 的整数；以及 Node 警告行前缀 "(node:1234)"）
// 已接受差异（ACCEPTED，同样每次打印）：只属于 Node 运行时、与 PilotDeck 行为无关的输出。
// 另有一张「不比较」的清单 IGNORED，每次运行都会打印出来，不藏。
// 场景自己可以在 "volatile" 里再声明不稳定的路径（正则，匹配差异路径），同样会打印。
import fs from 'node:fs';

const args = process.argv.slice(2);
const files = args.filter((a) => !a.startsWith('--'));
const max = args.includes('--max') ? Number(args[args.indexOf('--max') + 1]) : 60;
const quiet = args.includes('--quiet');
if (files.length < 2) { console.error('usage: diff.mjs <a.json> <b.json> [--max N] [--quiet]'); process.exit(2); }

// 不比较的顶层字段，以及原因。
export const IGNORED = {
  side: '采集侧标记',
  startMs: '启动耗时，性能指标，单独报告',
  serverLog: '服务进程自己的日志输出；CLI 子命令的 stdout/stderr 在 steps 里照常比较',
};

// 已接受差异：id → [说明, 对字符串做的处理]。新增条目要在 state/takeover.md 记依据。
export const ACCEPTED = {
  'node-runtime-warning': ['Node 运行时自己打到 stderr 的 ExperimentalWarning 及其提示行（基线用了 node:sqlite）；Rust 进程没有这个运行时',
    (s) => s.replace(/^\(node:<pid>\) ExperimentalWarning: .*\n\(Use `node --trace-warnings \.\.\.` to show where the warning was created\)\n/gm, '')],
};
const PID_KEYS = ['pid', 'ownerPid'];
const UUID = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi;
const ISO = /\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})/g;

function normalizer() {
  const ids = new Map();
  const text = (s) => s.replace(UUID, (u) => {
    const k = u.toLowerCase();
    if (!ids.has(k)) ids.set(k, ids.size + 1);
    return `<uuid:${ids.get(k)}>`;
  }).replace(ISO, '<iso>').replace(/\(node:\d+\)/g, '(node:<pid>)');
  const walk = (v) => {
    if (typeof v === 'string') return Object.values(ACCEPTED).reduce((s, [, apply]) => apply(s), text(v));
    if (typeof v === 'number') return v > 1.6e12 && v < 2.0e12 ? (Number.isInteger(v) ? '<ms>' : '<ms.frac>') : v;
    if (Array.isArray(v)) return v.map(walk);
    if (v && typeof v === 'object') {
      return Object.fromEntries(Object.entries(v).map(([k, x]) => [text(k), PID_KEYS.includes(k) && Number.isInteger(x) ? '<pid>' : walk(x)]));
    }
    return v;
  };
  return walk;
}

function load(file) {
  const raw = JSON.parse(fs.readFileSync(file, 'utf8'));
  for (const k of Object.keys(IGNORED)) delete raw[k];
  return normalizer()(raw);
}

const show = (v) => { const s = JSON.stringify(v); return s === undefined ? '(absent)' : s.length > 200 ? s.slice(0, 200) + `… (${s.length} chars)` : s; };
function compare(a, b, at, outList) {
  if (a === b) return;
  const ta = Array.isArray(a) ? 'array' : a === null ? 'null' : typeof a;
  const tb = Array.isArray(b) ? 'array' : b === null ? 'null' : typeof b;
  if (ta !== tb || (ta !== 'object' && ta !== 'array')) { outList.push({ at, a, b }); return; }
  if (ta === 'array') {
    if (a.length !== b.length) outList.push({ at: at + '.length', a: a.length, b: b.length });
    for (let i = 0; i < Math.max(a.length, b.length); i++) {
      if (i >= a.length) outList.push({ at: `${at}[${i}]`, a: undefined, b: b[i] });
      else if (i >= b.length) outList.push({ at: `${at}[${i}]`, a: a[i], b: undefined });
      else compare(a[i], b[i], `${at}[${i}]`, outList);
    }
    return;
  }
  // 键的顺序也是可观察的（JSON 序列化顺序），单独报一条，再按键比值。
  const ka = Object.keys(a), kb = Object.keys(b);
  const shared = ka.filter((k) => k in b);
  if (JSON.stringify(shared) !== JSON.stringify(kb.filter((k) => k in a))) outList.push({ at: at + '.<key order>', a: shared, b: kb.filter((k) => k in a) });
  for (const k of ka) if (!(k in b)) outList.push({ at: `${at}.${k}`, a: a[k], b: undefined });
  for (const k of kb) if (!(k in a)) outList.push({ at: `${at}.${k}`, a: undefined, b: b[k] });
  for (const k of shared) compare(a[k], b[k], `${at}.${k}`, outList);
}

const [fa, fb] = files;
const a = load(fa), b = load(fb);
const scenarioVolatile = (JSON.parse(fs.readFileSync(fa, 'utf8')).volatile ?? []).map((p) => new RegExp(p));
const all = [];
compare(a, b, '$', all);
const diffs = all.filter((d) => !scenarioVolatile.some((re) => re.test(d.at)));
if (!quiet) {
  console.log(`bbx diff: ${fa}  vs  ${fb}`);
  console.log(`  不比较的字段: ${Object.entries(IGNORED).map(([k, why]) => `${k}（${why}）`).join('；')}`);
  console.log(`  已接受差异: ${Object.entries(ACCEPTED).map(([k, [why]]) => `${k}（${why}）`).join('；')}`);
  if (scenarioVolatile.length) console.log(`  场景声明的不稳定路径: ${scenarioVolatile.map(String).join(' ')}（因此略过 ${all.length - diffs.length} 处）`);
  for (const d of diffs.slice(0, max)) console.log(`  ${d.at}\n      a: ${show(d.a)}\n      b: ${show(d.b)}`);
  if (diffs.length > max) console.log(`  … 另有 ${diffs.length - max} 处`);
}
console.log(diffs.length ? `DIFF ${diffs.length}` : 'SAME');
process.exit(diffs.length ? 1 : 0);
