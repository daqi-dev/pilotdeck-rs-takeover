// Extracted without rule changes from the frozen BBX comparator.
import {isDeepStrictEqual} from 'node:util';
import {sdkAgentOrderChildTree} from './sdk-agent-policy.mjs';
// 不比较的顶层字段，以及原因。
export const IGNORED = {
  side: '采集侧标记',
  evidence: '运行身份和必需断言，由 check.mjs 逐侧严格核验，不作行为归一化',
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
const DURATION_KEYS = ['duration_ms', 'duration_seconds', 'durationMs', 'waitedMs', 'delayMs'];
const dur = () => '<dur>';
const UUID = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi;
const ISO = /\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})/g;

export function normalizer(prefixes = new WeakMap(), ids = new Map()) {
  const snaps = new Map();
  const snap = (id) => { if (!snaps.has(id)) snaps.set(id, snaps.size + 1); return `<snap:${snaps.get(id)}>`; };
  // Remove the already ignored timing line before it can allocate UUID aliases.
  // Its runId must not change later observable connection/entry references.
  const text = (s) => s.replace(/^\[gateway:turn-timing\] \{.*\}\n/gm, '').replace(UUID, (u) => {
    const k = u.toLowerCase();
    if (!ids.has(k)) ids.set(k, ids.size + 1);
    return `<uuid:${ids.get(k)}>`;
  }).replace(ISO, '<iso>').replace(/\(node:\d+\)/g, '(node:<pid>)')
    .replace(/(?<![\da-fA-F.])1[6-9]\d{11}(?![\da-fA-F.])/g, '<ms>')
    .replace(/pilotdeck_execute_code_[A-Za-z0-9]{6}/g, 'pilotdeck_execute_code_<tmp>')
    .replace(/\.pilotdeck-restore-[A-Za-z0-9]{6}/g, '.pilotdeck-restore-<tmp>')
    .replace(/(\/snapshots\/)([0-9a-f]{64})/g, (_, pre, id) => pre + snap(id))
    .replace(/(pilotdeck-tool-output\/)\d+\//g, '$1<pid>/')
    .replace(/("created":\s?)1[6-9]\d{8}(?!\d)/g, '$1<sec>')
    .replace(/("durationMs":\s?)\d+/g, '$1<dur>')
    .replace(/(duration_ms: )\d+/g, '$1<dur>')
    .replace(/\bpid=\d+/g, 'pid=<pid>')
    .replace(/(duration_seconds: )(\d+(?:\.\d+)?)/g, (_, k, n) => k + dur(n));
  const walk = (v) => {
    if (typeof v === 'string') return Object.values(ACCEPTED).reduce((s, [, apply]) => apply(s), text(v));
    if (typeof v === 'number') return v > 1.6e12 && v < 2.0e12 ? (Number.isInteger(v) ? '<ms>' : '<ms.frac>') : v;
    if (Array.isArray(v)) return v.map(walk);
    if (v && typeof v === 'object') {
      return Object.fromEntries(Object.entries(v).map(([k, x]) => [text(k), PID_KEYS.includes(k) && Number.isInteger(x) ? '<pid>' : DURATION_KEYS.includes(k) && typeof x === 'number' ? dur(x) : k === 'snapshotId' && /^[0-9a-f]{64}$/.test(x) ? snap(x) : k === 'requestFingerprint' && typeof x === 'string' ? '<fingerprint>' : k === 'created' && Number.isInteger(x) && x > 1.6e9 && x < 2.0e9 ? '<sec>' : prefixes.get(v)?.has(k) ? (() => { const d = prefixes.get(v).get(k); return text(d.prefix) + text(d.run) + `<prefix:${d.length}/36>`; })() : walk(x)]));
    }
    return v;
  };
  return walk;
}

const FLOAT_OVER = ['step_started', 'context_snapshot', 'agent_instructions', 'compaction_started', 'compaction_completed', 'agent_status_message'];
const SLOT_KEYS = ['sequence', 'entryId', 'parentEntryId', 'createdAt'];
const isTitle = (r) => r?.type === 'session_metadata' && r.metadata && typeof r.metadata === 'object' && 'titleSource' in r.metadata;
export function settleTitleRecords(recs) {
  // 返回被挪动的记录：原来所在位置的 entryId → 挪动后所在位置的位置字段。投影检查点里抄了这些记录的
  // 位置字段（sequence / entryId / parentEntryId，以及等于 sequence - 1 的 index），要跟着改。
  const moved = new Map();
  for (let i = 0; i < recs.length; i++) {
    if (!isTitle(recs[i])) continue;
    let j = i;
    while (j > 0 && FLOAT_OVER.includes(recs[j - 1]?.type) && recs[j - 1].turnId === recs[i].turnId) j--;
    if (j === i) continue;
    // 内容按 [title, j..i-1] 重排，位置字段留在原位。
    const payloads = [recs[i], ...recs.slice(j, i)];
    const slots = recs.slice(j, i + 1);
    for (let k = 0; k <= i - j; k++) {
      if (typeof payloads[k].entryId === 'string') moved.set(payloads[k].entryId, slots[k]);
      recs[j + k] = Object.fromEntries(Object.keys(payloads[k]).map((key) => [key, SLOT_KEYS.includes(key) ? slots[k][key] : payloads[k][key]]));
    }
  }
  return moved;
}
function resettleProjections(v, moved) {
  if (Array.isArray(v)) { v.forEach((x) => resettleProjections(x, moved)); return; }
  if (!v || typeof v !== 'object') return;
  const slot = typeof v.entryId === 'string' && 'sequence' in v ? moved.get(v.entryId) : undefined;
  const inner = v.entry && typeof v.entry === 'object' && typeof v.entry.entryId === 'string' ? moved.get(v.entry.entryId) : undefined;
  if (inner && typeof v.index === 'number' && typeof inner.sequence === 'number') v.index = inner.sequence - 1;
  for (const x of Object.values(v)) resettleProjections(x, moved);
  if (slot) for (const key of SLOT_KEYS) if (key in v && key in slot && key !== 'createdAt') v[key] = slot[key];
}
// 规则 10：always-on 的 events.jsonl 不比记录先后。基线追加这个文件时不等上一条写完（DiscoveryFire 的
// appendEvent 是 fire-and-forget），连续发出的两条事件谁先落盘是随机的。比较前按内容排序（先抹掉 id 和时间），
// 记录的集合和每条内容照比。
const UNORDERED_JSONL = /\/always-on\/projects\/[^/]+\/events\.jsonl$/;
const orderKey = (r) => JSON.stringify(r).replace(UUID, 'U').replace(ISO, 'T').replace(/\d{13}(\.\d+)?/g, 'M');
export function load(input, prefixes, aliases) {
  let raw = input;
  for (const entry of raw.volatilePolicies ?? []) {
    if (isDeepStrictEqual(entry, { policy: 'sdkAgent03DerivedValues' })) {
      raw = sdkAgentOrderChildTree(raw, entry);
    }
  }
  for (const [name, entry] of Object.entries(raw.tree ?? {})) {
    if (name.endsWith('.jsonl') && Array.isArray(entry?.jsonl)) {
      const moved = settleTitleRecords(entry.jsonl);
      const projections = raw.tree[`${name}.projections.json`];
      if (moved.size && projections?.json) resettleProjections(projections.json, moved);
    }
    if (UNORDERED_JSONL.test(name) && Array.isArray(entry?.jsonl)) entry.jsonl.sort((x, y) => (orderKey(x) < orderKey(y) ? -1 : orderKey(x) > orderKey(y) ? 1 : 0));
  }
  for (const k of Object.keys(IGNORED)) delete raw[k];
  return normalizer(prefixes, aliases)(raw);
}

export const show = (v) => { const s = JSON.stringify(v); return s === undefined ? '(absent)' : s.length > 200 ? s.slice(0, 200) + `… (${s.length} chars)` : s; };
// 数组元素的名字表；不是「全部元素都有不重复的名字」就返回 null。
function names(arr) {
  const out = arr.map((x) => (x && typeof x === 'object' && !Array.isArray(x)) ? (typeof x.function?.name === 'string' ? x.function.name : typeof x.name === 'string' ? x.name : null) : null);
  return out.every((n) => n !== null) && new Set(out).size === out.length ? out : null;
}
export function compare(a, b, at, outList) {
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

