// 上游对拍工具 tools/agent-loop-parity 的包装（只用 --comparison same-version）。
//
// 一轮 = 基线的 run.py 跑全部 53 个 PilotDeck 场景，每个场景两个 adapter：
//   pilotdeck-current-native   TS 原生循环（进程内）
//   pilotdeck-current-sidecar  经 stdio 部署拉起的 sidecar，再与 native 的 trace 做语义比较
// sidecar 一侧用仓库里冻结的 scripts/parity/bind.mjs 做 adapter（不改它）：side=ts 时被拉起的是基线
// sidecar，side=rust 时通过 modules.agentLoop 绑定换成 Rust 二进制，并核对实际 spawn 的身份与 sha256。
//
// 不在 resources/ 写任何东西：工具目录拷到输出目录里再跑（python 的字节码也只会落在拷贝里，且加了 -B），
// 产物与运行时目录都在输出目录。run.py 以 --pilotdeck-root 指向基线，只读 dist。
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { join } from 'node:path';
import { REPO, isolatedEnv, runBounded } from './common.mjs';

const BIND = join(REPO, 'scripts/parity/bind.mjs');

function sha256(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex');
}

export function parityScenarioIds(baseline) {
  const doc = JSON.parse(readFileSync(join(baseline, 'tools/agent-loop-parity/scenarios.json'), 'utf8'));
  return doc.scenarios.filter((s) => (s.pairs ?? []).includes('pilotdeck')).map((s) => String(s.scenarioId));
}

function copyTool(baseline, dest) {
  if (existsSync(join(dest, 'run.py'))) return dest;
  cpSync(join(baseline, 'tools/agent-loop-parity'), dest, {
    recursive: true,
    filter: (src) => !src.includes('__pycache__') && !src.endsWith('.pyc'),
  });
  return dest;
}

/** 跑一轮。side: ts | rust。返回 { perScenario: {sid: {status, details[]}}, summary, exit }。 */
export async function runParityRound({ baseline, side, rustBin, runDir, toolCopy, scenario = 'all', timeoutMs = 60 * 60_000 }) {
  rmSync(runDir, { recursive: true, force: true });
  mkdirSync(runDir, { recursive: true });
  const tool = copyTool(baseline, toolCopy);
  const candidate = side === 'rust' ? rustBin : join(baseline, 'dist/src/cli/pilotdeck-agent-loop-sidecar.js');
  const env = isolatedEnv(runDir, {
    PYTHONDONTWRITEBYTECODE: '1',
    PDRS_BASELINE: baseline,
    PDRS_SIDE: side,
    PDRS_EXPECTED_SHA256: sha256(candidate),
    PILOTDECK_AGENT_LOOP_TRANSPORT: 'stdio',
    PILOTDECK_AGENT_LOOP_SIDECAR_COMMAND: side === 'rust' ? rustBin : undefined,
  });
  const artifacts = join(runDir, 'artifacts');
  const args = ['-B', join(tool, 'run.py'),
    '--pilotdeck-root', baseline,
    '--comparison', 'same-version', '--surface', 'gateway', '--scenario', scenario,
    '--output', artifacts, '--adapter-timeout-seconds', '30',
    '--pilotdeck-sidecar-cmd', `${JSON.stringify(process.execPath)} ${JSON.stringify(BIND)}`];
  const python = process.env.PDRS_PYTHON || 'python3';
  const result = await runBounded(python, args, { cwd: runDir, env, timeoutMs });
  writeFileSync(join(runDir, 'run.py.stdout'), result.stdout);
  writeFileSync(join(runDir, 'run.py.stderr'), result.stderr);
  const summaryPath = join(artifacts, 'summary.json');
  const summary = existsSync(summaryPath) ? JSON.parse(readFileSync(summaryPath, 'utf8')) : undefined;
  const ids = scenario === 'all' ? parityScenarioIds(baseline) : [scenario];
  const perScenario = Object.fromEntries(ids.map((id) => [id, { status: summary ? 'PASS' : 'NO_SUMMARY', details: [] }]));
  if (summary) {
    const mark = (list, status) => {
      for (const line of list ?? []) {
        const id = String(line).split('/')[0];
        const entry = perScenario[id];
        if (!entry) continue;
        if (entry.status === 'PASS' || (entry.status === 'BLOCKED' && status !== 'BLOCKED')) entry.status = status;
        entry.details.push(line);
      }
    };
    mark(summary.blocked, 'BLOCKED');
    mark(summary.failed, 'FAIL');
    mark(summary.oracleFailures, 'FAIL');
  }
  return { perScenario, summary, exit: result.code, timedOut: result.timedOut, stderr: result.stderr.slice(-3000) };
}
