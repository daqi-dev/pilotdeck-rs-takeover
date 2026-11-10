#!/usr/bin/env node
// 黑盒对拍 · 一键入口。
// 用法：node scripts/bbx/check.mjs [--out DIR] [--self] [场景名或路径 ...]
//   默认对 scenarios/ 下每个场景：基线跑一次、Rust 跑一次、比较。
//   --self：基线跑两次互相比较。它必须全部 SAME，否则说明场景或归一化本身不稳定，
//           这个场景的对拍结论不能用。
// 退出码：0 全部 SAME，1 有 DIFF 或采集失败。
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const here = path.dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const self = args.includes('--self');
const outDir = path.resolve(args.includes('--out') ? args[args.indexOf('--out') + 1] : path.join(here, '../../_build/bbx'));
const named = args.filter((a, i) => !a.startsWith('--') && args[i - 1] !== '--out');
const scenarios = (named.length ? named : fs.readdirSync(path.join(here, 'scenarios')).filter((f) => f.endsWith('.json')).sort())
  .map((n) => fs.existsSync(n) ? n : path.join(here, 'scenarios', n.endsWith('.json') ? n : n + '.json'));
const node = (script, ...a) => spawnSync(process.execPath, [path.join(here, script), ...a], { encoding: 'utf8' });
const [sideA, sideB] = self ? ['ts', 'ts'] : ['ts', 'rust'];
let bad = 0; const rows = [];
for (const file of scenarios) {
  const name = path.basename(file, '.json');
  const a = path.join(outDir, `${name}.${sideA}${self ? '-1' : ''}.json`);
  const b = path.join(outDir, `${name}.${sideB}${self ? '-2' : ''}.json`);
  const runs = [node('run.mjs', sideA, file, a), node('run.mjs', sideB, file, b)];
  const failed = runs.find((r) => r.status !== 0);
  if (failed) { bad++; rows.push([name, 'RUN FAILED', failed.stderr.trim().split('\n').pop()]); continue; }
  const d = node('diff.mjs', a, b, '--max', '400');
  fs.writeFileSync(path.join(outDir, `${name}.${self ? 'self' : 'diff'}.txt`), d.stdout);
  const verdict = d.stdout.trim().split('\n').pop();
  if (d.status !== 0) bad++;
  rows.push([name, verdict, path.relative(process.cwd(), path.join(outDir, `${name}.${self ? 'self' : 'diff'}.txt`))]);
}
const width = Math.max(...rows.map((r) => r[0].length));
for (const [name, verdict, note] of rows) console.log(`${name.padEnd(width)}  ${verdict.padEnd(10)}  ${note}`);
console.log(`${self ? '基线自比' : '基线 vs Rust'}：${rows.length - bad}/${rows.length} SAME`);
process.exit(bad ? 1 : 0);
