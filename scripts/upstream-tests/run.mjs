#!/usr/bin/env node
// 来自上游（基线自带）的 sidecar 验收集：一条命令跑完，打印结果表，退出码 0 表示计入的用例全部通过。
//
//   node scripts/upstream-tests/run.mjs                     # 全部组，指向 Rust sidecar
//   node scripts/upstream-tests/run.mjs --target baseline   # 指向基线自己的 sidecar（外部进程），校准用
//   node scripts/upstream-tests/run.mjs --group client,tcp --repeat 10 --target baseline
//
// 选项：
//   --target rust|baseline|inprocess  被测 sidecar（缺省 rust）；inprocess 只对 node 组有意义，是上游原样的进程内对照
//   --group  逗号分隔：client,tcp,deployment,stdio,parity,schema（缺省全部）
//   --repeat N   每组重复 N 轮（缺省 1）；表里的「通过」是每一轮都通过的用例数
//   --out DIR    运行产物目录（缺省 PDRS_UT_OUT，再缺省 <repo>/_build/upstream-tests）
//   --verbose    把 node --test 的失败输出直接打出来
// 环境：PDRS_BASELINE（基线目录）、PDRS_SIDECAR_BIN（Rust 二进制，缺省 src/target/debug/pilotdeck-agent-loop-sidecar）
import { mkdirSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { REPO, baselineRoot, rustSidecarBin, assertFreshBinary, prepareCwdRoot } from './lib/common.mjs';
import { GROUPS, EXCLUSIONS, SIDECAR_TESTS } from './lib/manifest.mjs';
import { generateGroup } from './lib/generate.mjs';
import { runNodeGroup } from './lib/node-groups.mjs';

const ALL_GROUPS = ['client', 'tcp', 'deployment', 'stdio', 'parity', 'schema'];

function parseArgs(argv) {
  const options = { target: 'rust', groups: ALL_GROUPS, repeat: 1, out: undefined, verbose: false };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const value = () => { const v = argv[++i]; if (v === undefined) throw new Error(`${arg} needs a value`); return v; };
    if (arg === '--target') options.target = value();
    else if (arg === '--group') options.groups = value().split(',').filter(Boolean);
    else if (arg === '--repeat') options.repeat = Number(value());
    else if (arg === '--out') options.out = value();
    else if (arg === '--verbose') options.verbose = true;
    else throw new Error(`unknown argument ${arg}`);
  }
  if (!['rust', 'baseline', 'inprocess'].includes(options.target)) throw new Error('--target must be rust, baseline or inprocess');
  for (const g of options.groups) if (!ALL_GROUPS.includes(g)) throw new Error(`unknown group ${g}`);
  if (!Number.isInteger(options.repeat) || options.repeat < 1) throw new Error('--repeat must be a positive integer');
  return options;
}

function summarizeNode(group, upstreamTests, runs) {
  const byName = new Map();
  for (const run of runs) {
    for (const row of run.rows) {
      const entry = byName.get(row.name) ?? { name: row.name, pass: 0, fail: 0, skip: 0, touched: false, excluded: row.excluded, skipReason: row.skip, errors: [] };
      entry[row.status] += 1;
      entry.touched ||= row.touched;
      if (row.status === 'fail') entry.errors.push(row.error);
      byName.set(row.name, entry);
    }
  }
  const sidecarNames = new Set(SIDECAR_TESTS[group]);
  // 计入的用例按 manifest 的静态清单算（不随某一轮是否崩溃而变）；缺席的也算没过。
  for (const name of sidecarNames) {
    if (!byName.has(name)) byName.set(name, { name, pass: 0, fail: 0, skip: 0, touched: false, excluded: false, errors: ['没有出现在报告里'] });
  }
  const entries = [...byName.values()];
  const excluded = entries.filter((e) => e.excluded);
  const hostOnly = entries.filter((e) => !e.excluded && !sidecarNames.has(e.name));
  const counted = entries.filter((e) => !e.excluded && sidecarNames.has(e.name));
  const passedAll = counted.filter((e) => e.pass === runs.length);
  const broken = runs.filter((r) => r.timedOut || r.rows.length === 0);
  // 通过了的用例，碰没碰 sidecar 必须与清单一致；否则说明改写或分类失效了。
  const misclassified = entries.filter((e) => e.pass > 0 && !e.excluded && e.touched !== sidecarNames.has(e.name)).map((e) => e.name);
  return {
    group, upstreamTests, seen: entries.length, rounds: runs.length,
    excluded: excluded.map((e) => ({ name: e.name, reason: e.skipReason })),
    hostOnly: hostOnly.map((e) => ({ name: e.name, pass: e.pass, fail: e.fail })),
    counted: counted.length, passed: passedAll.length, misclassified,
    failures: counted.filter((e) => e.pass !== runs.length).map((e) => ({ name: e.name, pass: e.pass, fail: e.fail, error: e.errors[0] })),
    brokenRuns: broken.map((r) => ({ exit: r.exit, timedOut: r.timedOut, stderr: r.stderr.slice(-2000) })),
    ok: broken.length === 0 && entries.length === upstreamTests && passedAll.length === counted.length
      && hostOnly.every((e) => e.fail === 0) && misclassified.length === 0,
  };
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const baseline = baselineRoot();
  const out = resolve(options.out ?? process.env.PDRS_UT_OUT ?? join(REPO, '_build/upstream-tests'));
  mkdirSync(out, { recursive: true });
  const rustBin = rustSidecarBin();
  if (options.target === 'rust') assertFreshBinary(rustBin);
  const cwdRoot = prepareCwdRoot(baseline, join(out, 'cwd-root'));
  console.log(`baseline: ${baseline}\ntarget:   ${options.target}${options.target === 'rust' ? ` (${rustBin})` : ''}\nout:      ${out}\n`);

  const summaries = [];
  for (const group of options.groups) {
    if (GROUPS[group]) {
      const gen = generateGroup(baseline, group, join(out, 'gen'));
      const runs = [];
      for (let round = 1; round <= options.repeat; round += 1) {
        const runDir = join(out, 'runs', options.target, group, String(round));
        const started = Date.now();
        const run = await runNodeGroup({
          baseline, group, genFile: gen.outPath, target: options.target, rustBin, runDir, cwdRoot,
          transcriptFile: process.env.PDRS_UT_TRANSCRIPT ? join(runDir, 'frames.jsonl') : undefined,
          verbose: options.verbose,
        });
        runs.push(run);
        const fails = run.rows.filter((r) => r.status === 'fail').map((r) => r.name);
        console.log(`[${group} ${options.target} ${round}/${options.repeat}] ${run.rows.length} tests, ${fails.length} failed, ${Math.round((Date.now() - started) / 1000)}s${run.timedOut ? ' TIMED OUT' : ''}${fails.length ? `\n    ${fails.join('\n    ')}` : ''}`);
      }
      summaries.push(summarizeNode(group, gen.upstreamTests, runs));
    } else {
      summaries.push({ group, skipped: true });
    }
  }
  writeFileSync(join(out, `summary.${options.target}.json`), JSON.stringify({ target: options.target, summaries }, null, 2));
  printTable(summaries);
  process.exitCode = summaries.every((s) => s.skipped || s.ok) ? 0 : 1;
}

function printTable(summaries) {
  console.log('\n| 组 | 上游总数 | 不经被测 sidecar | 剔除 | 计入 | 通过（每轮都过） | 轮数 | 结论 |');
  console.log('|---|---|---|---|---|---|---|---|');
  for (const s of summaries) {
    if (s.skipped) { console.log(`| ${s.group} | — | — | — | — | — | — | 未实现 |`); continue; }
    console.log(`| ${s.group} | ${s.upstreamTests} | ${s.hostOnly.length} | ${s.excluded.length} | ${s.counted} | ${s.passed} | ${s.rounds} | ${s.ok ? 'OK' : 'FAIL'} |`);
  }
  for (const s of summaries) {
    if (s.skipped) continue;
    if (s.seen !== s.upstreamTests) console.log(`\n${s.group}: 报告了 ${s.seen} 个用例，上游有 ${s.upstreamTests} 个（文件级错误？）`);
    for (const f of s.failures) console.log(`\n${s.group} 未过：${f.name}（过 ${f.pass} / 败 ${f.fail}）\n  ${String(f.error ?? '').split('\n').slice(0, 6).join('\n  ')}`);
    for (const h of s.hostOnly.filter((x) => x.fail)) console.log(`\n${s.group} 不经被测 sidecar 的用例失败：${h.name}`);
    for (const m of s.misclassified) console.log(`\n${s.group} 分类不符（碰没碰 sidecar 与清单不一致）：${m}`);
    for (const b of s.brokenRuns) console.log(`\n${s.group} 运行异常：exit=${b.exit} timedOut=${b.timedOut}\n${b.stderr}`);
  }
  void EXCLUSIONS;
}

main().catch((error) => { console.error(error.stack ?? error); process.exitCode = 2; });
