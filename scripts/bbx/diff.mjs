#!/usr/bin/env node
// 黑盒对拍 · 比较器。
// 用法：node scripts/bbx/diff.mjs <a.json> <b.json> [--max N] [--quiet] [--self]
// 退出码：0 一致，1 有差异，2 用法错误。
//
// 归一化只有下面四条，都是「同一侧连跑两次也会变」的值，别的一律按原样比：
//   1. UUID            → <uuid:N>，N 是它在该侧输出里第一次出现的序号（保留「同一个 id」的关系）
//      AO discovery 唯一例外：完整输入/run/provider/metadata/projection严格关联后，
//      将1200 UTF16边界内的UUID残片映射到该完整UUID同别名，保留残片长度。
//   2. ISO 时间戳      → <iso>
//   3. 毫秒时间戳      → <ms>（1.6e12 ~ 2.0e12 之间的整数；文本里独立出现的同范围 13 位整数同样处理（前后紧挨十六进制字符或小数点的不算，免得误伤哈希），
//                        例如 CLI 输出的 JSON 文本里的 lastModified）；带小数的记成 <ms.frac>，
//                        这样「一侧整数、一侧小数」仍然算差异
//   4. 进程号          → <pid>（键名为 pid / ownerPid 的整数；Node 警告行前缀 "(node:1234)"；文本里的 "pid=1234"）
//   5. requestFingerprint → <fingerprint>（键名为 requestFingerprint 的字符串；会话里已有历史时，
//                        基线两次发出的请求体逐字节相同，这个值仍然不同）
//   6. 耗时            → 键名为 duration_ms / duration_seconds / durationMs / waitedMs / delayMs（重试退避，带随机抖动）的数值，以及文本里
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
//      照常报差异。挪的是内容，位置上的 sequence / entryId / parentEntryId / createdAt 不动（基线里它们也只随位置变）；
//      同一会话的投影检查点里抄下来的这些位置字段跟着改
//  10. always-on 的 events.jsonl 不比记录先后（基线不等待地追加，先后随机），只比记录集合与内容
//  11. 服务进程 stdout 里的 "[gateway:turn-timing] {…}" 行整行去掉：它只在某一步耗时超过 200ms 时才打印，
//      打不打、数值多少都随机器快慢变
//  12. 秒级时间戳      → <sec>（只认键名 created：apiServer 渠道的 OpenAI 兼容响应里 Math.floor(Date.now()/1000)，
//      1.6e9 ~ 2.0e9 之间的整数；SSE 正文文本里的 "created":<10 位整数> 同样处理）。别的键名的 10 位整数照原样比
// 数组按下标比。唯一的例外：元素全是带名字的对象（function.name 或 name，且名字不重复）的数组
// 按名字对齐——缺的、多的各报一条，共有元素的相对顺序不同报一条，同名元素再逐字段比。
// 这只改变差异的报法（少一个工具不再表现为后面所有工具都错位），不会让任何差异消失。
// 已接受差异（ACCEPTED，同样每次打印）：只属于 Node 运行时、与 PilotDeck 行为无关的输出。
// 场景可在 "accepted" 登记已批准差异：[{id, why, paths:[正则]}]；新增项须有冻结账本依据与正向断言。
// T1 诊断使用独立逐值 policy，不允许路径正则。命中理由和原始值每次打印；未接受字段照常比较。
// 另有一张「不比较」的清单 IGNORED，每次运行都会打印出来，不藏。
// 场景自己可以在 "volatile" 里再声明不稳定的路径（正则，匹配差异路径），同样会打印。
// 过期检查：基线 vs Rust 比较时，场景登记的每条 accepted 正则一处都没命中，打印 STALE 并计 1 处差异；
// volatile 正则一处都没命中只打印 STALE? 提示（基线这次没抖时本来就是 0）。--self（基线自比）不做这个检查。
import fs from 'node:fs';
import {IGNORED,ACCEPTED,normalizer,load,compare,show} from './normalization.mjs';
import {observerSupplementPaths} from './observer-supplement-policy.mjs';
import {observerIdLinkPaths} from './observer-id-links-policy.mjs';
import {observerMemoryWitnessPaths} from './observer-memory-witness.mjs';
import {durationFileSizePaths} from './duration-file-size-policy.mjs';
import {feishuStopAcceptedPaths} from './feishu-stop-policy.mjs';
import {diagnosticsAcceptedPaths} from './diagnostics-policy.mjs';
import {memoryTreeOrderPaths} from './memory-tree-order-policy.mjs';
export {IGNORED,ACCEPTED} from './normalization.mjs';
import { observerUuidViews } from './observer-uuid-policy.mjs';
import { observerMemoryPaths } from './observer-memory-policy.mjs';
import { observerLifecyclePaths } from './observer-lifecycle-policy.mjs';
import { discoveryPrefixPair } from './ao-discovery-prefix-policy.mjs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { cliShutdownAcceptedPaths } from './cli-shutdown-policy.mjs';
import { syntaxAcceptedPaths } from './syntax-policy.mjs';
import { isDeepStrictEqual } from 'node:util';
import { channelPollVolatilePaths } from './channel-poll-policy.mjs';
import { sdkAgentDerivedPaths, sdkAgentOrderChildTree } from './sdk-agent-policy.mjs';
import { sdkSignalAcceptedPaths } from './sdk-signal-policy.mjs';
import { sdkSignalAcceptedEntry, validateSdkSignalAcceptedEvidence } from './evidence.mjs';

const args = process.argv.slice(2);
const files = args.filter((a) => !a.startsWith('--'));
const max = args.includes('--max') ? Number(args[args.indexOf('--max') + 1]) : 60;
const quiet = args.includes('--quiet');
const selfCompare = args.includes('--self');
if (files.length < 2) { console.error('usage: diff.mjs <a.json> <b.json> [--max N] [--quiet] [--self]'); process.exit(2); }

const [fa, fb] = files;
const inputs = [fa, fb].map(file => JSON.parse(fs.readFileSync(file, 'utf8')));
let prefixes;
try { prefixes = discoveryPrefixPair(inputs[0], inputs[1], value => normalizer()(value)); }
catch (error) { console.error(`INVALID AO discovery UUID prefix: ${error.message}`); process.exit(2); }
const uuidAliases = [new Map(), new Map()];
let a = load(inputs[0], prefixes[0], uuidAliases[0]), b = load(inputs[1], prefixes[1], uuidAliases[1]);
const raw = files.slice(0, 2).map((file) => JSON.parse(fs.readFileSync(file, 'utf8')));
let uuidEntities;
try {
  uuidEntities = observerUuidViews(raw[0], raw[1], a, b, uuidAliases[0], uuidAliases[1]);
  [a, b] = uuidEntities.views;
  [uuidAliases[0], uuidAliases[1]] = uuidEntities.aliases;
} catch (error) { console.error(`INVALID observer lifecycle evidence (UUID binding): ${error.message}`); process.exit(2); }
const scenarioVolatile = (JSON.parse(fs.readFileSync(fa, 'utf8')).volatile ?? []).map((p) => new RegExp(p));
const all = [];
compare(a, b, '$', all);

const compatibilityValues = new Map();
let observerLifecycle;
try {
  observerLifecycle = observerLifecyclePaths(raw[0], raw[1], a, b, uuidAliases[0], uuidAliases[1], all);
  if (observerLifecycle.expandedDifferences) all.splice(0, all.length, ...observerLifecycle.expandedDifferences);
  for (const [at, reason] of observerLifecycle) compatibilityValues.set(at, { policy: 'observerLifecycleWindows', reason });
  for (const [at, reason] of observerMemoryPaths(raw[0], raw[1], a, b)) compatibilityValues.set(at, { policy: 'observerMemoryApproved', reason });
  const baselineRoot = process.env.BBX_BASELINE || path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../resources/PilotDeck');
  for (const [policy, paths] of [
    ['observerSupplementApproved', observerSupplementPaths(raw[0], raw[1], baselineRoot)],
    ['observerSameIdLinks', observerIdLinkPaths(raw[0], raw[1], baselineRoot)],
    ['observerMemorySchemaApproved', observerMemoryWitnessPaths(raw[0], raw[1], baselineRoot)],
    ['durationFileSizeApproved', durationFileSizePaths(raw[0], raw[1])],
    ['sealedMemoryTreeOrderApproved', memoryTreeOrderPaths(fa, fb, {self: selfCompare, expectedRaw: raw})],
    ['fixedParserDiagnosticsApproved', selfCompare ? new Map() : diagnosticsAcceptedPaths(raw[0], raw[1], baselineRoot)],
  ]) for (const [at, reason] of paths) compatibilityValues.set(at, { policy, reason });
  if (!selfCompare && raw.some(x => x.scenario === 'backend-feishu-stream-stop')) {
    for (const [at, reason] of feishuStopAcceptedPaths(fa, fb, baselineRoot)) compatibilityValues.set(at, { policy: 'feishuOwnedStopApproved', reason });
  }
} catch (error) { console.error(`INVALID observer lifecycle evidence: ${error.message}`); process.exit(2); }

// A declared time policy accepts only individually validated values. The raw
// captures remain immutable, and every accepted URL is printed below.
if (!isDeepStrictEqual(raw[0].volatilePolicies ?? [], raw[1].volatilePolicies ?? [])) throw Error('volatile value policy mismatch');
const timeValues = new Map();
const timePolicies = new Map();
for (const entry of raw[0].volatilePolicies ?? []) {
  let paths;
  if (isDeepStrictEqual(entry, { policy: 'coreChannelPollAfter' })) paths = channelPollVolatilePaths(raw[0], raw[1], entry);
  else if (isDeepStrictEqual(entry, { policy: 'sdkAgent03DerivedValues' })) {
    const baseline = process.env.BBX_BASELINE || path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../resources/PilotDeck');
    paths = await sdkAgentDerivedPaths(raw[0], raw[1], a, b, entry, baseline);
  } else throw Error('unsupported volatile value policy');
  for (const [at, reason] of paths) { timeValues.set(at, reason); timePolicies.set(at, entry.policy); }
}
// The one named signal scenario cannot bypass its required proof by deleting
// accepted declarations from both captures. TS self remains the original diff.
let sdkSignalScenario;
if (!selfCompare && raw.some(artifact => artifact.scenario === 'c1-sdk-cancel-resume')) {
  try {
    sdkSignalScenario = JSON.parse(fs.readFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), 'scenarios/c1-sdk-cancel-resume.json'), 'utf8'));
    if (!raw.every(artifact => artifact.scenario === sdkSignalScenario.name
      && isDeepStrictEqual(artifact.accepted ?? [], sdkSignalScenario.accepted ?? []))) throw Error('required named scenario accepted declaration is missing or changed');
  } catch (error) { console.error(`INVALID SDK signal evidence: ${error.message}`); process.exit(2); }
}
// Named CLI policy is bound to the trusted scene and this comparator's own load.
// Old immutable raw captures predate the declaration; their exact scene hash is
// separately proven by the policy. Self comparisons never apply this exception.
let cliShutdownEntry;
if (!selfCompare && ['cli-entry-default','cli-survey-offline-nontui','cli-default-turns'].includes(raw[0].scenario)) {
  try {
    const scene = JSON.parse(fs.readFileSync(new URL(`./scenarios/${raw[0].scenario}.json`, import.meta.url), 'utf8'));
    const expected = [{id:'D1-CLI-SHUTDOWN-001',why:'固定七步CLI任务/EOF完成后的关闭挂起修复及129→130尾metadata checkpoint',policy:'cliShutdownCompleted',assertions:['cli-shutdown-completed-positive']}];
    if (!isDeepStrictEqual(scene.accepted, expected)) throw Error('required CLI shutdown scene declaration changed');
    for (const artifact of raw) if (!isDeepStrictEqual(artifact.accepted ?? [], []) && !isDeepStrictEqual(artifact.accepted, expected)) throw Error('invalid CLI shutdown artifact declaration');
    cliShutdownEntry = {...expected[0], res: [], hits: 0, checked: cliShutdownAcceptedPaths(raw[0], raw[1], value => load(value, new WeakMap(), new Map()))};
  } catch (error) { console.error(`INVALID CLI shutdown evidence: ${error.message}`); process.exit(2); }
}
const scenarioAccepted = (cliShutdownEntry ? [] : (raw[0].accepted ?? [])).map((x) => ({ ...x, res: (x.paths ?? []).map((p) => new RegExp(p)), hits: 0 }));
if (cliShutdownEntry) scenarioAccepted.push(cliShutdownEntry);
for (const entry of scenarioAccepted) if (entry.policy) {
  if (entry.policy === 'cliShutdownCompleted') {
    if (entry === cliShutdownEntry) continue;
    if (selfCompare) { entry.checked = new Map(); continue; }
    throw Error('unbound CLI shutdown policy');
  }
  if (['sdkSignalAbortOrder','sdkSignalTerminalObservation'].includes(entry.policy)) {
    if (!sdkSignalAcceptedEntry(entry) || !isDeepStrictEqual(raw[0].accepted ?? [], raw[1].accepted ?? [])) throw Error('invalid SDK signal accepted declaration');
    if (selfCompare) { entry.checked = new Map(); continue; }
    try {
      const scenario = sdkSignalScenario ?? JSON.parse(fs.readFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), 'scenarios/c1-sdk-cancel-resume.json'), 'utf8'));
      validateSdkSignalAcceptedEvidence(raw[0], scenario);
      const wire = validateSdkSignalAcceptedEvidence(raw[1], scenario);
      entry.checked = sdkSignalAcceptedPaths({ left: a, right: b, rawLeft: raw[0], rawRight: raw[1], scenario, wireReceipt: wire?.receipt, policy: entry.policy });
    } catch (error) { console.error(`INVALID SDK signal evidence: ${error.message}`); process.exit(2); }
    continue;
  }
  if (entry.policy !== 'syntaxDiagnostics' || entry.res.length) throw Error('unsupported value-checked accepted policy');
  const baseline = process.env.BBX_BASELINE || path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../resources/PilotDeck');
  const require = createRequire(path.join(baseline, 'package.json'));
  const tokenizer = new (require('js-tiktoken/lite').Tiktoken)(require('js-tiktoken/ranks/o200k_base'));
  entry.checked = syntaxAcceptedPaths(a, b, entry.config, (text) => tokenizer.encode(text).length, selfCompare ? ['ts', 'ts'] : ['ts', 'rust'], raw);
}
const regexStable = all.filter((d) => !scenarioVolatile.some((re) => re.test(d.at)));
const stable = regexStable.filter((d) => !timeValues.has(d.at) && !compatibilityValues.has(d.at));
const acceptedValues = [];
const diffs = stable.filter((d) => !scenarioAccepted.some((x) => {
  const reason = x.checked?.get(d.at);
  if (reason || x.res.some((re) => re.test(d.at))) {
    ++x.hits; if (reason) acceptedValues.push({ ...d, id: x.id, reason }); return true;
  }
  return false;
}));
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
  if (scenarioVolatile.length) console.log(`  场景声明的不稳定路径: ${scenarioVolatile.map(String).join(' ')}（因此略过 ${all.length - regexStable.length} 处）`);
  for (const d of regexStable.filter((d) => timeValues.has(d.at))) console.log(`  VOLATILE-VALUE ${timePolicies.get(d.at)} ${d.at}: ${timeValues.get(d.at)}\n      raw a: ${show(d.a)}\n      raw b: ${show(d.b)}`);
  for (const x of scenarioAccepted) console.log(`  场景登记的已接受差异: ${x.id}（${x.why}）略过 ${x.hits} 处`);
  for (const entity of uuidEntities?.entityTokens ?? []) console.log(`  UUID-ENTITY ${entity.entity}: ${entity.left} ↔ ${entity.right} → ${entity.token}`);
  for (const original of observerLifecycle?.wholeRecordDifferences ?? []) console.log(`  LIFECYCLE-LEAF-EXPANSION ${original.at}: original extra record retained in capture; typed leaves are independently compared`);
  for (const d of regexStable.filter(d => compatibilityValues.has(d.at))) { const value = compatibilityValues.get(d.at); console.log(`  COMPATIBILITY-VALUE ${value.policy} ${d.at}: ${value.reason}\n      raw a: ${show(d.a)}\n      raw b: ${show(d.b)}`); }
  for (const d of acceptedValues) console.log(`  ACCEPTED ${d.id} ${d.at}: ${d.reason}\n      raw a: ${show(d.a)}\n      raw b: ${show(d.b)}`);
  for (const d of diffs.slice(0, max)) console.log(`  ${d.at}\n      a: ${show(d.a)}\n      b: ${show(d.b)}`);
  if (diffs.length > max) console.log(`  … 另有 ${diffs.length - max} 处`);
  for (const x of stale) console.log(`  ${x.tag} ${x.what}：一处都没命中${x.tag === 'STALE' ? '，计 1 处差异' : '（只提示）'}`);
}
console.log(count ? `DIFF ${count}` : 'SAME');
// Let stdout/stderr pipes drain the complete per-leaf proof report.
process.exitCode = count ? 1 : 0;
