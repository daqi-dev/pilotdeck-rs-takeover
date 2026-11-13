#!/usr/bin/env node
// 黑盒对拍 · 一键入口。
// 用法：node scripts/bbx/check.mjs [--out DIR] [--self] [--slots 5,6,7,8] [场景名或路径 ...]
//   --slots：把场景分给这几个并行槽位同时跑（见 run.mjs 的 BBX_SLOT），结果合在一起报。
//   默认对 scenarios/ 下每个场景：基线跑一次、Rust 跑一次、比较。
//   --self：基线跑两次互相比较。它必须全部 SAME，否则说明场景或归一化本身不稳定，
//           这个场景的对拍结论不能用。
//   采集产物里有非预期的失败（启动失败、步骤超时、连不上；见 run.mjs 的 failuresOf）时判 INVALID，
//   不管两侧是否相同。预期的超时由场景在步骤上声明 expectTimeout，预期的启动失败声明 expectStartFailure。
// 退出码：0 全部 SAME，1 有 DIFF、INVALID 或采集失败。
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const here = path.dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const self = args.includes('--self');
const outDir = path.resolve(args.includes('--out') ? args[args.indexOf('--out') + 1] : path.join(here, '../../_build/bbx'));
const slots = args.includes('--slots') ? args[args.indexOf('--slots') + 1].split(',').map(Number) : null;
const named = args.filter((a, i) => !a.startsWith('--') && args[i - 1] !== '--out' && args[i - 1] !== '--slots');
const scenarios = (named.length ? named : fs.readdirSync(path.join(here, 'scenarios')).filter((f) => f.endsWith('.json')).sort())
  .map((n) => fs.existsSync(n) ? n : path.join(here, 'scenarios', n.endsWith('.json') ? n : n + '.json'));
const node = (script, ...a) => spawnSync(process.execPath, [path.join(here, script), ...a], { encoding: 'utf8' });
const [sideA, sideB] = self ? ['ts', 'ts'] : ['ts', 'rust'];
let bad = 0; let rows = [];
if (slots) {
  // 并行：每个槽位一个子进程跑自己那一份场景，子进程把结果行按 JSON 打到 stdout。
  const share = slots.map(() => []);
  scenarios.forEach((file, i) => share[i % slots.length].push(file));
  const results = await Promise.all(slots.map((slot, i) => new Promise((resolve) => {
    if (!share[i].length) return resolve('');
    const child = spawn(process.execPath, [fileURLToPath(import.meta.url), '--out', outDir, ...(self ? ['--self'] : []), ...share[i]],
      { env: { ...process.env, BBX_SLOT: String(slot), BBX_CHECK_ROWS: '1' }, stdio: ['ignore', 'pipe', 'inherit'] });
    let out = ''; child.stdout.on('data', (d) => { out += d; }); child.on('close', () => resolve(out));
  })));
  rows = results.flatMap((out) => out.split('\n').filter(Boolean).map((l) => JSON.parse(l))).sort((x, y) => (x[0] < y[0] ? -1 : 1));
  bad = rows.filter((r) => r[1] !== 'SAME').length;
}
for (const file of slots ? [] : scenarios) {
  const name = path.basename(file, '.json');
  const a = path.join(outDir, `${name}.${sideA}${self ? '-1' : ''}.json`);
  const b = path.join(outDir, `${name}.${sideB}${self ? '-2' : ''}.json`);
  // 采集是独占的：锁被别的运行占着（run.mjs 退出码 3）就等，最多等 20 分钟。
  // BBX_WAIT_18789=1 时，18789 上有监听者（退出码 5）也等：只用在确知那个监听者是另一轮对拍
  // 短暂起的被测 server 的机器上（cb8）。缺省不等，直接算采集失败——本机那里可能是真实的 PilotDeck。
  const collect = (side, out) => {
    for (let waited = 0; ; ) {
      const r = node('run.mjs', side, file, out);
      const busy18789 = r.status === 5 && process.env.BBX_WAIT_18789 === '1';
      if ((r.status !== 3 && !busy18789) || waited >= 1200) return r;
      const pause = busy18789 ? 3 : 15; waited += pause;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, pause * 1000);
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
if (process.env.BBX_CHECK_ROWS) { for (const r of rows) console.log(JSON.stringify(r)); process.exit(bad ? 1 : 0); }
const width = Math.max(...rows.map((r) => r[0].length));
for (const [name, verdict, note] of rows) console.log(`${name.padEnd(width)}  ${verdict.padEnd(10)}  ${note}`);
console.log(`${self ? '基线自比' : '基线 vs Rust'}：${rows.length - bad}/${rows.length} SAME`);
process.exit(bad ? 1 : 0);
