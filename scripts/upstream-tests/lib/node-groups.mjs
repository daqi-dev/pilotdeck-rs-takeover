// 跑一组生成的上游用例（node --test），返回逐例结果。
import { existsSync, mkdirSync, readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { HERE, isolatedEnv, runBounded } from './common.mjs';

const REPORTER = pathToFileURL(join(HERE, 'lib/reporter.mjs')).href;

function readJsonl(path) {
  if (!existsSync(path)) return [];
  return readFileSync(path, 'utf8').split('\n').filter(Boolean).map((line) => JSON.parse(line));
}

/**
 * 跑一次。target: baseline | rust | inprocess。
 * 返回 { rows: [{name,status,touched,excluded,error,durationMs}], exit, timedOut, stderr }。
 */
export async function runNodeGroup({ baseline, group, genFile, target, rustBin, runDir, cwdRoot, transcriptFile, timeoutMs = 20 * 60_000, verbose }) {
  rmSync(runDir, { recursive: true, force: true });
  mkdirSync(runDir, { recursive: true });
  const report = join(runDir, 'report.jsonl');
  const touches = join(runDir, 'touch.jsonl');
  const env = isolatedEnv(runDir, {
    PDRS_UT_BASELINE: baseline,
    PDRS_UT_TARGET: target,
    PDRS_UT_GROUP: group,
    PDRS_UT_CWD_ROOT: cwdRoot,
    PDRS_UT_REPORT: report,
    PDRS_UT_TOUCH_FILE: touches,
    PDRS_UT_TRANSCRIPT_FILE: transcriptFile,
    PDRS_SIDECAR_BIN: target === 'rust' ? rustBin : undefined,
  });
  const args = [
    '--test', '--test-force-exit', '--test-concurrency=1', '--test-timeout=300000',
    `--test-reporter=${REPORTER}`, '--test-reporter-destination=stdout', genFile,
  ];
  const result = await runBounded(process.execPath, args, {
    cwd: cwdRoot, env, timeoutMs,
    onStdout: verbose ? (s) => process.stdout.write(s) : undefined,
  });
  const touched = new Set(readJsonl(touches).map((t) => t.name));
  const rows = readJsonl(report).map((row) => ({
    ...row,
    excluded: typeof row.skip === 'string' && row.skip.startsWith('PDRS-EXCLUDED'),
    touched: touched.has(row.name),
  }));
  return { rows, exit: result.code, timedOut: result.timedOut, stdout: result.stdout, stderr: result.stderr };
}
