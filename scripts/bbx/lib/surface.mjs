// coverage.mjs 与 status.mjs 共用的判定口径：场景结论、表面项的 probe / observe 匹配、表面项是否被一致的场景碰到。
// 两个命令都从这里取，所以 coverage.mjs --surface 与 status.mjs 的数字按构造一致；改口径只改这一处。
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';

// 基线目录：BBX_BASELINE > 本检出的 resources/PilotDeck > 主检出（git common dir 的上一级）的 resources/PilotDeck。
// worktree 里不建软链接也能读到基线。
export function baselineDir(repo) {
  if (process.env.BBX_BASELINE) return process.env.BBX_BASELINE;
  const local = path.join(repo, 'resources/PilotDeck');
  if (fs.existsSync(path.join(local, 'src'))) return local;
  try {
    const common = execFileSync('git', ['-C', repo, 'rev-parse', '--path-format=absolute', '--git-common-dir'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim();
    const main = path.join(path.dirname(common), 'resources/PilotDeck');
    if (fs.existsSync(path.join(main, 'src'))) return main;
  } catch { /* 不是 git 检出 */ }
  return local;
}

// 一个场景在产物目录里的结论：SAME / DIFF / INVALID（基线侧采集有非预期的失败）/ 未跑（没有 diff 文件）。
export function verdictOf(outDir, name) {
  const file = path.join(outDir, `${name}.diff.txt`);
  if (!fs.existsSync(file)) return '未跑';
  const raw = path.join(outDir, `${name}.ts.json`);
  const failures = fs.existsSync(raw) ? (JSON.parse(fs.readFileSync(raw, 'utf8')).failures ?? []).filter((x) => !x.expected) : [];
  if (failures.length) return 'INVALID';
  return fs.readFileSync(file, 'utf8').trim().split('\n').pop().startsWith('SAME') ? 'SAME' : 'DIFF';
}

export function scenarioNames(scenariosDir) {
  return fs.readdirSync(scenariosDir).filter((f) => f.endsWith('.json')).map((f) => f.slice(0, -5)); // 目录顺序，与 coverage.mjs 原来的输出一致；要确定次序的调用方自己排序
}

// probe：整串形如 /re/flags 的按正则，其余按子串（见 surface.json 的 probeSyntax）。
export function matcher(p) {
  const m = /^\/(.+)\/([gimsuy]*)$/.exec(p);
  if (m) { const re = new RegExp(m[1], m[2].replace('g', '')); return (t) => re.test(t); }
  return (t) => t.includes(p);
}

// 每个场景的原文、基线侧采集产物原文与结论。
export function loadScenarioTexts(scenariosDir, outDir, names = scenarioNames(scenariosDir)) {
  return names.map((name) => {
    const art = path.join(outDir, `${name}.ts.json`);
    return { name, verdict: verdictOf(outDir, name), scenario: fs.readFileSync(path.join(scenariosDir, `${name}.json`), 'utf8'), artifact: fs.existsSync(art) ? fs.readFileSync(art, 'utf8') : '' };
  });
}

// 一个表面项的判定：
//   skip     项上写了 skip 理由，不计
//   same     至少一个 SAME 的场景碰到它
//   diff     碰到它的场景都不是 SAME（只被有差异 / INVALID / 未跑的场景碰到）
//   missing  没有场景碰到它
// hits 是碰到它的场景名（按名字排序）。
export function judgeItem(item, texts) {
  if (item.skip) return { status: 'skip', hits: [] };
  const probes = (item.probe ?? []).map(matcher);
  const observe = item.observe ? new RegExp(item.observe) : null;
  const hits = texts.filter((t) => probes.some((f) => f(t.scenario)) || (observe && observe.test(t.artifact)));
  const status = hits.some((t) => t.verdict === 'SAME') ? 'same' : hits.length ? 'diff' : 'missing';
  return { status, hits: hits.map((t) => t.name) };
}

// 按维度汇总，与 coverage.mjs --surface 打印的表同一口径。
export function surfaceTable(surface, texts) {
  const rows = [];
  for (const [dim, { items }] of Object.entries(surface.dimensions)) {
    const row = { dim, items: items.length, same: 0, diff: 0, missing: 0, skip: 0, judged: [] };
    for (const item of items) {
      const j = judgeItem(item, texts);
      row[j.status]++;
      row.judged.push({ item, ...j });
    }
    rows.push(row);
  }
  return rows;
}
