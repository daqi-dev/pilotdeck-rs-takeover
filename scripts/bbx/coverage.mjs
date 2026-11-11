#!/usr/bin/env node
// 黑盒对拍 · 覆盖清单。从场景文件和最近一次 check.mjs 的结果算出：
// 基线的每个 gateway method 有没有被「非普查」场景调用过，这些场景现在是不是 SAME。
// 进度以它的输出为准，不手写。
// 用法：node scripts/bbx/coverage.mjs [--out DIR] [--list]
//   --out DIR  check.mjs 的产物目录，默认 _build/bbx
//   --list     逐个 method 列出覆盖它的场景
// 一个 method 的状态：
//   SAME     至少一个调用它的场景是 SAME，且没有调用它的场景是 DIFF / INVALID
//   DIFF     有调用它的场景不是 SAME
//   未覆盖   只在普查场景（survey-*）里出现，或者完全没出现
//   未跑     有场景调用它，但产物目录里没有那个场景的结果
// 「调用过」只说明正常路径有场景，不说明它的全部分支都比过。
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '../..');
const args = process.argv.slice(2);
const outDir = args.includes('--out') ? args[args.indexOf('--out') + 1] : path.join(repo, '_build/bbx');
const frames = fs.readFileSync(path.join(repo, 'resources/PilotDeck/src/gateway/protocol/frames.ts'), 'utf8');
const methods = [...frames.match(/WsGatewayMethod\s*=([^;]+);/s)[1].matchAll(/"([a-z_]+)"/g)].map((m) => m[1]);

// 场景里所有 ws 调用（含 serve.then、answer 里的）。
const calls = (v, found = new Set()) => {
  if (Array.isArray(v)) v.forEach((x) => calls(x, found));
  else if (v && typeof v === 'object') {
    if (typeof v.ws === 'string') found.add(v.ws);
    Object.values(v).forEach((x) => calls(x, found));
  }
  return found;
};
const verdictOf = (name) => {
  const file = path.join(outDir, `${name}.diff.txt`);
  if (!fs.existsSync(file)) return '未跑';
  const raw = path.join(outDir, `${name}.ts.json`);
  const failures = fs.existsSync(raw) ? (JSON.parse(fs.readFileSync(raw, 'utf8')).failures ?? []).filter((x) => !x.expected) : [];
  if (failures.length) return 'INVALID';
  return fs.readFileSync(file, 'utf8').trim().split('\n').pop().startsWith('SAME') ? 'SAME' : 'DIFF';
};

const by = Object.fromEntries(methods.map((m) => [m, []]));
const scenarios = fs.readdirSync(path.join(here, 'scenarios')).filter((f) => f.endsWith('.json')).map((f) => f.slice(0, -5));
let same = 0;
for (const name of scenarios) {
  const verdict = verdictOf(name);
  if (verdict === 'SAME') same++;
  if (name.startsWith('survey-')) continue;
  const scenario = JSON.parse(fs.readFileSync(path.join(here, 'scenarios', `${name}.json`), 'utf8'));
  for (const m of calls(scenario.steps ?? [])) if (by[m]) by[m].push([name, verdict]);
}
const status = (list) => !list.length ? '未覆盖'
  : list.some(([, v]) => v === 'DIFF' || v === 'INVALID') ? 'DIFF'
  : list.some(([, v]) => v === 'SAME') ? 'SAME' : '未跑';
const groups = { SAME: [], DIFF: [], 未跑: [], 未覆盖: [] };
for (const m of methods) groups[status(by[m])].push(m);

console.log(`场景：${same}/${scenarios.length} SAME（产物目录 ${path.relative(process.cwd(), outDir) || '.'}）`);
console.log(`gateway method：${methods.length} 个；有场景且全部 SAME ${groups.SAME.length}，有场景但有差异 ${groups.DIFF.length}，有场景未跑 ${groups['未跑'].length}，未覆盖 ${groups['未覆盖'].length}`);
for (const k of ['DIFF', '未跑', '未覆盖']) if (groups[k].length) console.log(`\n${k}（${groups[k].length}）：${groups[k].join(' ')}`);
if (args.includes('--list')) {
  console.log('');
  for (const m of methods) console.log(`${m}\t${status(by[m])}\t${by[m].map(([n, v]) => `${n}:${v}`).join(' ')}`);
}
