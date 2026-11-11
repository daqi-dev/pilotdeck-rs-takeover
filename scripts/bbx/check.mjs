#!/usr/bin/env node
// 黑盒对拍 · 一键入口。
// 用法：node scripts/bbx/check.mjs [--out DIR] [--self] [场景名或路径 ...]
//   默认对 scenarios/ 下每个场景：基线跑一次、Rust 跑一次、比较。
//   --self：基线跑两次互相比较。它必须全部 SAME，否则说明场景或归一化本身不稳定，
//           这个场景的对拍结论不能用。
//   采集产物里有非预期的失败（启动失败、步骤超时、连不上；见 run.mjs 的 failuresOf）时判 INVALID，
//   不管两侧是否相同。预期的超时由场景在步骤上声明 expectTimeout，预期的启动失败声明 expectStartFailure。
// 退出码：0 全部 SAME，1 有 DIFF、INVALID 或采集失败。
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
  // 采集是独占的：锁被别的运行占着（run.mjs 退出码 3）就等，最多等 20 分钟。
  const collect = (side, out) => {
    for (let waited = 0; ; waited += 15) {
      const r = node('run.mjs', side, file, out);
      if (r.status !== 3 || waited >= 1200) return r;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 15000);
    }
  };
  const runs = [collect(sideA, a), collect(sideB, b)];
  const failed = runs.find((r) => r.status !== 0);
  if (failed) { bad++; rows.push([name, 'RUN FAILED', failed.stderr.trim().split('\n').pop()]); continue; }
  const d = node('diff.mjs', a, b, '--max', '400', ...(self ? ['--self'] : []));
  const report = path.join(outDir, `${name}.${self ? 'self' : 'diff'}.txt`);
  fs.writeFileSync(report, d.stdout);
  const verdict = d.stdout.trim().split('\n').pop();
  // 非预期的失败（启动失败、超时、连不上）：结论不能用，判 INVALID，两侧相同也一样。
  const failures = [a, b].flatMap((f) => (JSON.parse(fs.readFileSync(f, 'utf8')).failures ?? [])
    .filter((x) => !x.expected).map((x) => `${path.basename(f, '.json').split('.').pop()}:${x.at}:${x.kind}`));
  if (d.status !== 0 || failures.length) bad++;
  rows.push([name, failures.length ? 'INVALID' : verdict, `${path.relative(process.cwd(), report)}${failures.length ? `  (${verdict}; ${failures.join(' ')})` : ''}`]);
}
const width = Math.max(...rows.map((r) => r[0].length));
for (const [name, verdict, note] of rows) console.log(`${name.padEnd(width)}  ${verdict.padEnd(10)}  ${note}`);
console.log(`${self ? '基线自比' : '基线 vs Rust'}：${rows.length - bad}/${rows.length} SAME`);
process.exit(bad ? 1 : 0);
