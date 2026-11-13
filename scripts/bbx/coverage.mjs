#!/usr/bin/env node
// 黑盒对拍 · 覆盖清单。从场景文件和最近一次 check.mjs 的结果算出：
// 基线的每个 gateway method 有没有被「非普查」场景调用过，这些场景现在是不是 SAME。
// 进度以它的输出为准，不手写。
// 用法：node scripts/bbx/coverage.mjs [--out DIR] [--list] [--surface [维度]]
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
import { baselineDir, verdictOf as verdictIn, scenarioNames, loadScenarioTexts, judgeItem } from './lib/surface.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '../..');
const args = process.argv.slice(2);
const outDir = args.includes('--out') ? args[args.indexOf('--out') + 1] : path.join(repo, '_build/bbx');
// 场景结论与表面项的判定在 lib/surface.mjs，与 status.mjs 共用。
const frames = fs.readFileSync(path.join(baselineDir(repo), 'src/gateway/protocol/frames.ts'), 'utf8');
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
const verdictOf = (name) => verdictIn(outDir, name);

const by = Object.fromEntries(methods.map((m) => [m, []]));
const scenarios = scenarioNames(path.join(here, 'scenarios'));
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

// ── 基线对外表面清单（surface.json）的覆盖 ──
// --surface：按维度统计清单里每一项是否被场景碰到（probe 在场景文件原文里出现，或 observe 在基线采集产物里出现），
// 以及碰到它的场景里有没有 SAME 的。--surface <维度> 逐项列出该维度没被 SAME 场景碰到的项。
// 「被 SAME 场景碰到」只说明它出现在一个两侧一致的场景里，不说明它的每种取值都比过。
if (args.includes('--surface')) {
  const want = args[args.indexOf('--surface') + 1];
  const surface = JSON.parse(fs.readFileSync(path.join(here, 'surface.json'), 'utf8'));
  const texts = loadScenarioTexts(path.join(here, 'scenarios'), outDir, scenarios);
  console.log(`基线对外表面：${Object.keys(surface.dimensions).length} 个维度（场景 ${same}/${scenarios.length} SAME）`);
  // 项上的 skip 写明不计入的理由（基线里没有产生它的代码路径、只属于不在范围的部分等），
  // 这类项单列为「不计」，不算进没碰到；--surface <维度> 同样逐项列出理由。
  console.log('维度\t项数\t被SAME场景碰到\t只被有差异的场景碰到\t没碰到\t不计');
  for (const [dim, { items }] of Object.entries(surface.dimensions)) {
    let ok = 0, diff = 0, skipped = 0; const missing = []; const skips = [];
    for (const item of items) {
      const j = judgeItem(item, texts);
      if (j.status === 'skip') { skipped++; skips.push(`不计  \t${item.id}\t${item.where}\t${item.skip}`); continue; }
      if (j.status === 'same') ok++;
      else { if (j.status === 'diff') diff++; missing.push(`${j.status === 'diff' ? 'DIFF  ' : '未覆盖'}\t${item.id}\t${item.where}\t${item.note}`); }
    }
    console.log(`${dim}\t${items.length}\t${ok}\t${diff}\t${items.length - ok - diff - skipped}\t${skipped}`);
    if (want === dim) for (const line of [...missing, ...skips]) console.log('  ' + line);
  }
  process.exit(0);
}
console.log(`场景：${same}/${scenarios.length} SAME（产物目录 ${path.relative(process.cwd(), outDir) || '.'}）`);
console.log(`gateway method：${methods.length} 个；有场景且全部 SAME ${groups.SAME.length}，有场景但有差异 ${groups.DIFF.length}，有场景未跑 ${groups['未跑'].length}，未覆盖 ${groups['未覆盖'].length}`);
for (const k of ['DIFF', '未跑', '未覆盖']) if (groups[k].length) console.log(`\n${k}（${groups[k].length}）：${groups[k].join(' ')}`);
if (args.includes('--list')) {
  console.log('');
  for (const m of methods) console.log(`${m}\t${status(by[m])}\t${by[m].map(([n, v]) => `${n}:${v}`).join(' ')}`);
}
