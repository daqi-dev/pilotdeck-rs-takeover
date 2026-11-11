#!/usr/bin/env node
// 黑盒对拍 · 比较器。
// 用法：node scripts/bbx/diff.mjs <a.json> <b.json> [--max N] [--quiet] [--self]
// 退出码：0 一致，1 有差异，2 用法错误。
//
// 归一化只有下面四条，都是「同一侧连跑两次也会变」的值，别的一律按原样比：
//   1. UUID            → <uuid:N>，N 是它在该侧输出里第一次出现的序号（保留「同一个 id」的关系）
//   2. ISO 时间戳      → <iso>
//   3. 毫秒时间戳      → <ms>（1.6e12 ~ 2.0e12 之间的整数；文本里独立出现的同范围 13 位整数同样处理，
//                        例如 CLI 输出的 JSON 文本里的 lastModified）；带小数的记成 <ms.frac>，
//                        这样「一侧整数、一侧小数」仍然算差异
//   4. 进程号          → <pid>（键名为 pid / ownerPid 的整数；Node 警告行前缀 "(node:1234)"；文本里的 "pid=1234"）
//   5. requestFingerprint → <fingerprint>（键名为 requestFingerprint 的字符串；会话里已有历史时，
//                        基线两次发出的请求体逐字节相同，这个值仍然不同）
//   6. 耗时            → 键名为 duration_ms / duration_seconds / durationMs 的数值，以及文本里
//                        "duration_seconds: 0.02"、"duration_ms: 12"、"durationMs":12 的数值，
//                        记成 <dur>。小数位数不比：基线把秒数取整到两位后按 JS 数字打印，0.10 会印成 0.1，
//                        位数本身逐次不同。取整和打印的写法由 Rust 侧单元测试负责
//   7. 基线 execute_code 的两处随机路径段：mkdtemp 目录 pilotdeck_execute_code_XXXXXX 的随机后缀 → <tmp>，
//      工具输出溢出目录 pilotdeck-tool-output/<进程号>/ → <pid>。只认这两个固定前缀
//   9. 工作区快照：快照 id 由随机的轮次 id 散列得来（键 snapshotId，以及路径段 /snapshots/<64 位十六进制>）
//      → <snap:N>，N 是它在该侧第一次出现的序号；快照恢复用的 mkdtemp 目录 .pilotdeck-restore-XXXXXX → <tmp>
//   8. 标题记录的落点：会话 .jsonl 里由 provider 生成标题的那条 session_metadata 记录（metadata.titleSource
//      存在）与循环开头的几条记录并发写入，基线两次运行的先后就不同。比较前把它的内容挪到同一轮里
//      accepted_input / 首条 session_metadata 之后；只允许越过 FLOAT_OVER 里列的记录类型，越过别的类型不挪，
//      照常报差异。挪的是内容，位置上的 sequence / entryId / parentEntryId / createdAt 不动（基线里它们也只随位置变）
// 数组按下标比。唯一的例外：元素全是带名字的对象（function.name 或 name，且名字不重复）的数组
// 按名字对齐——缺的、多的各报一条，共有元素的相对顺序不同报一条，同名元素再逐字段比。
// 这只改变差异的报法（少一个工具不再表现为后面所有工具都错位），不会让任何差异消失。
// 已接受差异（ACCEPTED，同样每次打印）：只属于 Node 运行时、与 PilotDeck 行为无关的输出。
// 场景自己可以在 "accepted" 里登记已接受差异：[{id, why, paths:[正则]}]，只用于基线自身的 bug
// （Rust 有意不照做）。命中的差异路径被略过，id、原因和略过的条数每次打印；依据记在 state/takeover.md。
// 另有一张「不比较」的清单 IGNORED，每次运行都会打印出来，不藏。
// 场景自己可以在 "volatile" 里再声明不稳定的路径（正则，匹配差异路径），同样会打印。
// 过期检查：基线 vs Rust 比较时，场景登记的每条 accepted 正则一处都没命中，打印 STALE 并计 1 处差异；
// volatile 正则一处都没命中只打印 STALE? 提示（基线这次没抖时本来就是 0）。--self（基线自比）不做这个检查。
import fs from 'node:fs';

const args = process.argv.slice(2);
const files = args.filter((a) => !a.startsWith('--'));
const max = args.includes('--max') ? Number(args[args.indexOf('--max') + 1]) : 60;
const quiet = args.includes('--quiet');
const selfCompare = args.includes('--self');
if (files.length < 2) { console.error('usage: diff.mjs <a.json> <b.json> [--max N] [--quiet] [--self]'); process.exit(2); }

// 不比较的顶层字段，以及原因。
export const IGNORED = {
  side: '采集侧标记',
  startMs: '启动耗时，性能指标，单独报告',
  serverLog: '服务进程自己的日志输出；CLI 子命令的 stdout/stderr 在 steps 里照常比较',
  failures: '超时 / 连不上的汇总，由 check.mjs 判 INVALID；对应的输出在 steps 里照常比较',
};

// 已接受差异：id → [说明, 对字符串做的处理]。新增条目要在 state/takeover.md 记依据。
export const ACCEPTED = {
  'node-runtime-warning': ['Node 运行时自己打到 stderr 的 ExperimentalWarning 及其提示行（基线用了 node:sqlite）；Rust 进程没有这个运行时',
    (s) => s.replace(/^\(node:<pid>\) ExperimentalWarning: .*\n\(Use `node --trace-warnings \.\.\.` to show where the warning was created\)\n/gm, '')],
};
const PID_KEYS = ['pid', 'ownerPid'];
const DURATION_KEYS = ['duration_ms', 'duration_seconds', 'durationMs'];
const dur = () => '<dur>';
const UUID = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi;
const ISO = /\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})/g;

function normalizer() {
  const ids = new Map();
  const snaps = new Map();
  const snap = (id) => { if (!snaps.has(id)) snaps.set(id, snaps.size + 1); return `<snap:${snaps.get(id)}>`; };
  const text = (s) => s.replace(UUID, (u) => {
    const k = u.toLowerCase();
    if (!ids.has(k)) ids.set(k, ids.size + 1);
    return `<uuid:${ids.get(k)}>`;
  }).replace(ISO, '<iso>').replace(/\(node:\d+\)/g, '(node:<pid>)')
    .replace(/(?<![\d.])1[6-9]\d{11}(?![\d.])/g, '<ms>')
    .replace(/pilotdeck_execute_code_[A-Za-z0-9]{6}/g, 'pilotdeck_execute_code_<tmp>')
    .replace(/\.pilotdeck-restore-[A-Za-z0-9]{6}/g, '.pilotdeck-restore-<tmp>')
    .replace(/(\/snapshots\/)([0-9a-f]{64})/g, (_, pre, id) => pre + snap(id))
    .replace(/(pilotdeck-tool-output\/)\d+\//g, '$1<pid>/')
    .replace(/("durationMs":\s?)\d+/g, '$1<dur>')
    .replace(/(duration_ms: )\d+/g, '$1<dur>')
    .replace(/\bpid=\d+/g, 'pid=<pid>')
    .replace(/(duration_seconds: )(\d+(?:\.\d+)?)/g, (_, k, n) => k + dur(n));
  const walk = (v) => {
    if (typeof v === 'string') return Object.values(ACCEPTED).reduce((s, [, apply]) => apply(s), text(v));
    if (typeof v === 'number') return v > 1.6e12 && v < 2.0e12 ? (Number.isInteger(v) ? '<ms>' : '<ms.frac>') : v;
    if (Array.isArray(v)) return v.map(walk);
    if (v && typeof v === 'object') {
      return Object.fromEntries(Object.entries(v).map(([k, x]) => [text(k), PID_KEYS.includes(k) && Number.isInteger(x) ? '<pid>' : DURATION_KEYS.includes(k) && typeof x === 'number' ? dur(x) : k === 'snapshotId' && /^[0-9a-f]{64}$/.test(x) ? snap(x) : k === 'requestFingerprint' && typeof x === 'string' ? '<fingerprint>' : walk(x)]));
    }
    return v;
  };
  return walk;
}

const FLOAT_OVER = ['step_started', 'context_snapshot', 'agent_instructions', 'compaction_started', 'compaction_completed', 'agent_status_message'];
const SLOT_KEYS = ['sequence', 'entryId', 'parentEntryId', 'createdAt'];
const isTitle = (r) => r?.type === 'session_metadata' && r.metadata && typeof r.metadata === 'object' && 'titleSource' in r.metadata;
function settleTitleRecords(recs) {
  for (let i = 0; i < recs.length; i++) {
    if (!isTitle(recs[i])) continue;
    let j = i;
    while (j > 0 && FLOAT_OVER.includes(recs[j - 1]?.type) && recs[j - 1].turnId === recs[i].turnId) j--;
    if (j === i) continue;
    // 内容按 [title, j..i-1] 重排，位置字段留在原位。
    const payloads = [recs[i], ...recs.slice(j, i)];
    const slots = recs.slice(j, i + 1);
    for (let k = 0; k <= i - j; k++) {
      recs[j + k] = Object.fromEntries(Object.keys(payloads[k]).map((key) => [key, SLOT_KEYS.includes(key) ? slots[k][key] : payloads[k][key]]));
    }
  }
}
function load(file) {
  const raw = JSON.parse(fs.readFileSync(file, 'utf8'));
  for (const [name, entry] of Object.entries(raw.tree ?? {})) {
    if (name.endsWith('.jsonl') && Array.isArray(entry?.jsonl)) settleTitleRecords(entry.jsonl);
  }
  for (const k of Object.keys(IGNORED)) delete raw[k];
  return normalizer()(raw);
}

const show = (v) => { const s = JSON.stringify(v); return s === undefined ? '(absent)' : s.length > 200 ? s.slice(0, 200) + `… (${s.length} chars)` : s; };
// 数组元素的名字表；不是「全部元素都有不重复的名字」就返回 null。
function names(arr) {
  const out = arr.map((x) => (x && typeof x === 'object' && !Array.isArray(x)) ? (typeof x.function?.name === 'string' ? x.function.name : typeof x.name === 'string' ? x.name : null) : null);
  return out.every((n) => n !== null) && new Set(out).size === out.length ? out : null;
}
function compare(a, b, at, outList) {
  if (a === b) return;
  const ta = Array.isArray(a) ? 'array' : a === null ? 'null' : typeof a;
  const tb = Array.isArray(b) ? 'array' : b === null ? 'null' : typeof b;
  if (ta !== tb || (ta !== 'object' && ta !== 'array')) { outList.push({ at, a, b }); return; }
  if (ta === 'array') {
    const na = names(a), nb = names(b);
    if (na && nb && (a.length || b.length) && JSON.stringify(na) !== JSON.stringify(nb)) {
      for (const n of na) if (!nb.includes(n)) outList.push({ at: `${at}[name=${n}]`, a: a[na.indexOf(n)], b: undefined });
      for (const n of nb) if (!na.includes(n)) outList.push({ at: `${at}[name=${n}]`, a: undefined, b: b[nb.indexOf(n)] });
      const sa = na.filter((n) => nb.includes(n)), sb = nb.filter((n) => na.includes(n));
      if (JSON.stringify(sa) !== JSON.stringify(sb)) outList.push({ at: at + '.<name order>', a: sa, b: sb });
      for (const n of sa) compare(a[na.indexOf(n)], b[nb.indexOf(n)], `${at}[name=${n}]`, outList);
      return;
    }
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
const scenarioAccepted = (JSON.parse(fs.readFileSync(fa, 'utf8')).accepted ?? []).map((x) => ({ ...x, res: x.paths.map((p) => new RegExp(p)), hits: 0 }));
const stable = all.filter((d) => !scenarioVolatile.some((re) => re.test(d.at)));
const diffs = stable.filter((d) => !scenarioAccepted.some((x) => x.res.some((re) => re.test(d.at)) && ++x.hits));
const unused = (res, list) => res.filter((re) => !list.some((d) => re.test(d.at)));
const stale = selfCompare ? [] : [
  ...unused(scenarioVolatile, all).map((re) => ({ tag: 'STALE?', what: `volatile ${re}` })),
  ...scenarioAccepted.flatMap((x) => unused(x.res, stable).map((re) => ({ tag: 'STALE', what: `accepted ${x.id} ${re}` }))),
];
const count = diffs.length + stale.filter((x) => x.tag === 'STALE').length;
if (!quiet) {
  console.log(`bbx diff: ${fa}  vs  ${fb}`);
  console.log(`  不比较的字段: ${Object.entries(IGNORED).map(([k, why]) => `${k}（${why}）`).join('；')}`);
  console.log(`  已接受差异: ${Object.entries(ACCEPTED).map(([k, [why]]) => `${k}（${why}）`).join('；')}`);
  if (scenarioVolatile.length) console.log(`  场景声明的不稳定路径: ${scenarioVolatile.map(String).join(' ')}（因此略过 ${all.length - stable.length} 处）`);
  for (const x of scenarioAccepted) console.log(`  场景登记的已接受差异: ${x.id}（${x.why}）略过 ${x.hits} 处`);
  for (const d of diffs.slice(0, max)) console.log(`  ${d.at}\n      a: ${show(d.a)}\n      b: ${show(d.b)}`);
  if (diffs.length > max) console.log(`  … 另有 ${diffs.length - max} 处`);
  for (const x of stale) console.log(`  ${x.tag} ${x.what}：一处都没命中${x.tag === 'STALE' ? '，计 1 处差异' : '（只提示）'}`);
}
console.log(count ? `DIFF ${count}` : 'SAME');
process.exit(count ? 1 : 0);
