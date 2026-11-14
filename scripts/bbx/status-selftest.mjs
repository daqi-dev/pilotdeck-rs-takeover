#!/usr/bin/env node
// status.mjs 的自测：用一组很小的假产物（3 个场景、两个维度的表面项、两个功能域）检查
// 归属（规则 / 例外 / 归属不了）、计数、缺口清单、台账与审阅记录的解析、占位与透明代理的判据、
// 不计项的理由表、CSV，以及同样输入两次生成的结果逐字节相同。
// 用法：node scripts/bbx/status-selftest.mjs（退出码 0 表示全部符合预期）。
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-status-selftest-'));
const w = (rel, content) => { const f = path.join(dir, rel); fs.mkdirSync(path.dirname(f), { recursive: true }); fs.writeFileSync(f, typeof content === 'string' ? content : JSON.stringify(content, null, 2)); return f; };

// 场景：a-one 由规则归到 x.one；b-two 由例外归到 x.one 与 y.two；c-three 归属不了。
w('scenarios/a-one.json', { name: 'a-one', about: '', steps: [{ ws: 'foo_method', params: { alpha: 1 } }] });
w('scenarios/b-two.json', { name: 'b-two', about: '', accepted: [{ id: 'k', why: '', paths: ['x'] }], steps: [{ ws: 'bar_method' }] });
w('scenarios/c-three.json', { name: 'c-three', about: '', steps: [{ http: 'GET /zzz' }] });
// 产物：a-one SAME，b-two DIFF，c-three 没有产物（未跑）。
w('out/a-one.diff.txt', 'bbx diff: /home/u/x/a-one.ts.json vs b\nSAME\n');
w('out/a-one.ts.json', { failures: [], steps: [], marker: 'OBSERVED-A' });
w('out/b-two.diff.txt', 'bbx diff: /home/u/x/b-two.ts.json vs b\nDIFF 3\n');
w('out/b-two.ts.json', { failures: [] });
w('surface.json', { dimensions: {
  'ws-methods': { about: '', items: [
    { id: 'ws:foo_method', where: 'f.ts:1', note: '', probe: ['"foo_method"'] },
    { id: 'ws:bar_method', where: 'f.ts:2', note: '', probe: ['"bar_method"'] },
    { id: 'ws:baz_method', where: 'f.ts:3', note: '', probe: ['"baz_method"'] },
  ] },
  misc: { about: '', items: [
    { id: 'm:observed', where: 'g.ts:1', note: '', probe: [], observe: 'OBSERVED-A' },
    { id: 'm:skipped', where: 'g.ts:2', note: '', probe: [], skip: '只在不在范围的部分出现（g.ts:2）' },
    { id: 'm:noreason', where: 'g.ts:3', note: '', probe: [], skip: ' ' },
    { id: 'm:orphan', where: 'g.ts:4', note: '', probe: ['/zzz-never/'] },
    { id: 'm:special', where: 'g.ts:5', note: '', probe: ['"alpha"'] },
  ] },
} });
w('features.json', {
  domains: [
    { id: 'x', name: '域 X', about: '', caps: [{ id: 'x.one', name: '一', about: '', where: ['src/x/'] }, { id: 'x.empty', name: '空', about: '', where: ['src/e/'] }] },
    { id: 'y', name: '域 Y', about: '', caps: [{ id: 'y.two', name: '二', about: '', where: ['src/y/'] }] },
  ],
  methodCaps: { 'x.one': ['foo_method', 'baz_method'], 'y.two': ['bar_method'] },
  surfaceRules: [{ dims: ['misc'], match: '^m:(observed|skipped|noreason)$', cap: 'y.two' }],
  surfaceExceptions: [{ dim: 'misc', id: 'm:special', cap: 'x.one', why: '' }],
  scenarioRules: [{ match: '^a-', caps: ['x.one'] }],
  scenarioExceptions: [{ name: 'b-two', caps: ['x.one', 'y.two'], why: '' }],
  upstreamTests: { groups: [{ group: 'g1', caps: ['y.two'], source: '' }] },
});
w('issues.md', [
  '# 台账', '', '## 1 安全与数据', '', '| 问题 | 功能域 | 来源 | 状态 |', '|---|---|---|---|',
  '| 占位 | x | — | — |', '', '## 2 主路径', '', '| 问题 | 功能域 | 来源 | 状态 |', '|---|---|---|---|',
  '| 甲 | x, y | 审阅 | 待派 |', '| 乙 | nope | 审阅 | 待派 |', '', '## 尺子本身的问题', '', '| 问题 | 功能域 | 来源 | 状态 |', '|---|---|---|---|',
  '| 丙 | * | 审阅 | 在查 |', '',
].join('\n'));
w('audits.md', [
  '| 日期 | 类型 | 执行者 | 覆盖（功能域 / 子能力） | 发现（条数与级别） | 记录在哪 |', '|---|---|---|---|---|---|',
  '| 2026-01-02 | 盲审 | 甲 | `x.one` | 无 | 某处 |', '| 2026-01-05 | 代码审阅 | 乙 | `y` | 1 条第 3 级 | 某处 |', '',
].join('\n'));
w('checks.json', {});
w('delivery.md', '# x\n\n## 允许不一致的清单\n\n| 场景 | 原因 | 定于 |\n|---|---|---|\n| `zzz` | r | d |\n');
w('src/crate/src/lib.rs', 'fn a() { todo!() }\n// todo!() 注释里的不算\nconst X: &str = "dist/src/cli/pilotdeck.js";\n');
// 第 5 条的四类新判据：构造处算、匹配分支与定义不算、注释不算、顶层 #[cfg(test)] mod 块不算。
w('src/cli/src/main.rs', [
  'fn a() -> R { Err(Failure::Todo("pilotdeck tui")) }', // 1：构造
  '    Err(Failure::Todo(what)) => 2,', // 匹配分支，不算
  'fn b() { let _ = Failure::Todo(', // 3：跨行构造
  '    "x"); }',
  '// Failure::Todo("注释里的不算")',
  'const M: &str = "Rust candidate does not yet support the X profile.";', // 6：自报未实现文案
  'fn c() { e("JS_HOST_REQUIRED"); x.requires_host = true; }', // 7：宿主边界哨兵（一行算一处）
  'fn d() { y.requires_host = false; }', // 不算
  'fn profile_gap(p: &str) {}', // 定义不算
  'fn f() { profile_gap("open x"); gap("y"); }', // 10：原生档案缺口
  'fn g() { h("NODE22_DATE_LOCAL_LEAP_PROFILE_UNSUPPORTED: z"); i("CONFIG_X_PROFILE_INVALID"); }', // 11：前一个算
  '#[cfg(test)]',
  'mod tests {',
  '    fn t() { Err(Failure::Todo("测试里的不算")); requires_host: true; profile_gap("t"); }',
  '}',
  'fn after() { Err(Failure::Todo("测试块之后照常算")) }', // 16
  '',
].join('\n'));
w('src/crate/src/tests.rs', 'fn t() { todo!() }\n');
// 已定范围外的逐处清单：认到 1 处（main.rs:1）；其余 5 条各因一种原因失效。
w('oos-placeholders.json', { about: '', items: ['pilotdeck tui', '遥测上报'], entries: [
  { file: 'cli/src/main.rs', match: 'Failure::Todo("pilotdeck tui")', item: 'pilotdeck tui', reason: 'r' },
  { file: 'cli/src/main.rs', match: 'Failure::Todo("早就删掉了")', item: 'pilotdeck tui', reason: 'r' },
  { file: 'cli/src/main.rs', match: 'Failure::Todo(', item: 'pilotdeck tui', reason: 'r' },
  { file: 'cli/src/*.rs', match: 'does not yet support', item: '遥测上报', reason: 'r' },
  { file: 'cli/src/main.rs', match: 'does not yet support', item: '不在六项里', reason: 'r' },
  { file: 'cli/src/main.rs', match: 'JS_HOST_REQUIRED', item: '遥测上报', reason: ' ' },
] });
w('src/crate/tests/it.rs', 'fn t() { unimplemented!() }\n');

const run = (extra = []) => spawnSync(process.execPath, [path.join(here, 'status.mjs'),
  '--out', path.join(dir, 'out'), '--scenarios', path.join(dir, 'scenarios'), '--surface', path.join(dir, 'surface.json'),
  '--features', path.join(dir, 'features.json'), '--issues', path.join(dir, 'issues.md'), '--audits', path.join(dir, 'audits.md'),
  '--checks', path.join(dir, 'checks.json'), '--delivery', path.join(dir, 'delivery.md'), '--src', path.join(dir, 'src'),
  '--oos-placeholders', path.join(dir, 'oos-placeholders.json'),
  '--status', path.join(dir, 'status.md'), '--oos', path.join(dir, 'oos.md'), '--now', 'T0', '--commit', 'c0ffee', '--out-label', 'fake', ...extra], { encoding: 'utf8' });

let r = run(['--csv', path.join(dir, 'csv')]);
if (r.status !== 0) { console.error(r.stderr); process.exit(1); }
const status = fs.readFileSync(path.join(dir, 'status.md'), 'utf8');
const oos = fs.readFileSync(path.join(dir, 'oos.md'), 'utf8');
const first = status;
r = run();
const again = fs.readFileSync(path.join(dir, 'status.md'), 'utf8');

let fail = 0;
const check = (what, ok) => { console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`); if (!ok) fail++; };
const has = (s) => status.includes(s);
const row = (k, at, cls = '范围内的占位') => has(`| \`${k}\` | \`${at}\` | ${cls} |`);
check('文件头：时间与提交来自参数', has('生成时间：T0') && has('`c0ffee`'));
check('文件头：平台从 diff 文件头判断为 Linux', has('Linux 2 个'));
check('文件头：未跑的场景列出', /当前提交有、产物里没有的（记为「未跑」）1 个：`c-three`/.test(status));
check('第 1 条：3 个场景 SAME 1 DIFF 1 未跑 1', has('场景 3 个：SAME 1，DIFF 1，INVALID 0，未跑 1'));
check('第 1 条：b-two 归为登记了 accepted', /\| `b-two` \| DIFF \| 场景登记了 accepted/.test(status));
check('第 1 条：c-three 归属不了', /\| `c-three` \| 未跑 \| 其余 \| 归属不了 \|/.test(status));
check('第 2 条：8 项，范围内 6，碰到 3（foo、observed、special），只被差异碰到 1（bar），没碰到 2，不计 2', has('表面项 8 个，范围内 6 个：被 SAME 场景碰到 3，只被有差异的场景碰到 1，没碰到 2；标为不计 2'));
check('第 5 条：todo! 只数产品文件里非注释的 1 处，tests.rs 与 tests/ 不算', has('`unimplemented!` 0 处、`todo!` 1 处') && row('todo!', 'crate/src/lib.rs:1'));
check('第 5 条：Failure::Todo 构造处 3 处（跳过匹配分支、注释、测试块），其中 1 处已定范围外，范围内 2 处', has('`Failure::Todo` 2 处') && row('Failure::Todo', 'cli/src/main.rs:1', '已定范围外 | pilotdeck tui') && row('Failure::Todo', 'cli/src/main.rs:3') && row('Failure::Todo', 'cli/src/main.rs:16') && !has('`cli/src/main.rs:2`') && !has('`cli/src/main.rs:14`'));
check('第 5 条：自报未实现文案 1 处、宿主边界哨兵 1 处（false 不算）', has('自报未实现文案 1 处') && row('自报未实现文案', 'cli/src/main.rs:6') && has('宿主边界哨兵 1 处') && row('宿主边界哨兵', 'cli/src/main.rs:7'));
check('第 5 条：原生档案缺口 2 处（定义与 *_PROFILE_INVALID 不算）', has('原生档案缺口 2 处') && row('原生档案缺口', 'cli/src/main.rs:10') && row('原生档案缺口', 'cli/src/main.rs:11') && !has('`cli/src/main.rs:9`'));
check('第 5 条：范围内总数 7（todo! 1 + 新判据 7 − 已定范围外 1），已定范围外 1 处', has('（共 7，逐条见下）；已定范围外 1 处'));
check('第 5 条：清单失效 5 条（已删、不唯一、通配、不在六项、没写理由），各自报出', has('范围外清单里失效的 5 条') && has('代码里已没有这处命中') && has('命中 3 行，须唯一') && has('不许目录或通配') && has('不在已定范围外的六项里') && has('没写理由'));
check('第 6 条：透明代理判据命中 1 处', has('命中 1 处') && has('`crate/src/lib.rs:3`'));
check('第 5 条：没有检查结果时写没有数据', has('四条命令：没有数据'));
check('按功能域：x 的场景 2（a、b）一致 1，表面项 3（foo、baz、special）', /\| `x` 域 X \| 2 \| 2 \| 1 \| 3 \| 2 \| 0 \| 1 \| 0 \|/.test(status));
check('按功能域：y 的场景 1、表面项 4（bar、observed、skipped、noreason）', /\| `y` 域 Y \| 1 \| 1 \| 0 \| 4 \| 1 \| 1 \| 0 \| 2 \|/.test(status));
check('按功能域：问题计数（占位行不计、x 与 y 各 1 条第 2 级）', /\| `x` 域 X .*\| 0\/1\/0\/0\/0 \|/.test(status) && /\| `y` 域 Y .*\| 0\/1\/0\/0\/0 \|/.test(status));
check('按功能域：横跨的尺子问题单列', /\| 横跨（`\*`） .*\| 0\/0\/0\/0\/1 \|/.test(status));
check('台账里认不出的功能域被列出', has('的功能域 nope'));
check('审阅：x 的最近盲审取子能力 x.one 的记录，y 的最近代码审阅', /\| `x` 域 X .*\| 2026-01-02（盲审） \| — \|/.test(status) && /\| `y` 域 Y .*\| — \| 2026-01-05（代码审阅） \|/.test(status));
check('缺口：没碰到的 baz 与 orphan（归属不了）、只被差异碰到的 bar', has('没碰到：`ws-methods` `ws:baz_method`') && has('只被有差异的碰到（`b-two`）：`ws-methods` `ws:bar_method`') && /#### 归属不了的\n\n- `misc` `m:orphan`/.test(status));
check('缺口：没有场景的子能力 x.empty', has('- `x.empty` 空'));
check('缺口：没审过盲审的功能域 y', has('没有任何盲审或缺口审查记录的（1）：`y`'));
check('归属统计：表面项 规则 6 例外 1 归属不了 1；场景 规则 1 例外 1 归属不了 1', has('按规则归属 6，例外 1，归属不了 1') && has('按规则归属 1，例外 1，归属不了 1'));
check('上游验收集挂到子能力', /\| `y\.two` 二 \|[^\n]*\| g1 \|/.test(status));
check('对照表按维度', has('| misc | 5 | 2 | 0 | 1 | 2 |') && has('| ws-methods | 3 | 1 | 1 | 1 | 0 |'));
check('不计理由表：没有理由的单列在前，有理由的按功能域', oos.indexOf('m:noreason') < oos.indexOf('## `y`') && oos.includes('只在不在范围的部分出现'));
const csv = fs.readFileSync(path.join(dir, 'csv/caps.csv'), 'utf8');
check('CSV 带 BOM，三个文件都在', csv.startsWith('﻿') && fs.existsSync(path.join(dir, 'csv/domains.csv')) && fs.existsSync(path.join(dir, 'csv/gaps.csv')));
check('同样输入两次生成逐字节相同', first === again);
// 清单引用了不存在的子能力时要报出来。
const f = JSON.parse(fs.readFileSync(path.join(dir, 'features.json'), 'utf8'));
f.scenarioRules.push({ match: '^c-', caps: ['x.nope'] }); fs.writeFileSync(path.join(dir, 'features.json'), JSON.stringify(f));
run();
check('清单引用不存在的子能力时报出', fs.readFileSync(path.join(dir, 'status.md'), 'utf8').includes('引用了不存在的子能力 x.nope'));

fs.rmSync(dir, { recursive: true, force: true });
console.log(fail ? `\n${fail} 项不符合预期` : '\n全部符合预期');
process.exit(fail ? 1 : 0);
