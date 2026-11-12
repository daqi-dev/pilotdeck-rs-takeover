// Run identity and validation are separate from behavioral normalization.
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { isDeepStrictEqual } from 'node:util';
import { fileURLToPath } from 'node:url';

export const here = path.dirname(fileURLToPath(import.meta.url));
export const repo = path.resolve(here, '../..');
export const sha256 = (value) => createHash('sha256').update(value).digest('hex');
export const fileHash = (file) => sha256(fs.readFileSync(file));
const command = (cmd, args, cwd) => {
  const r = spawnSync(cmd, args, { cwd, encoding: 'utf8' });
  return r.status === 0 && !r.signal ? r.stdout.trim() : null;
};
const untrackedIdentity = (cwd) => {
  const names = command('git', ['ls-files', '--others', '--exclude-standard', '-z'], cwd);
  return sha256((names ?? '').split('\0').filter((name) => /\.(rs|ts|mts|js|mjs)$|(^|\/)Cargo\.(toml|lock)$/.test(name)).sort().map((name) => `${name}:${fileHash(path.join(cwd, name))}`).join('\n'));
};
const gitIdentity = (cwd) => ({ untracked: untrackedIdentity(cwd), commit: command('git', ['rev-parse', 'HEAD'], cwd),
  // Include tracked changes in the identity; test runs may legitimately precede a commit.
  diff: sha256(command('git', ['diff', 'HEAD', '--', '.', ':!state', ':!docs'], cwd) ?? '') });
const treeHash = (root) => {
  if (!fs.existsSync(root)) return null;
  const hash = createHash('sha256');
  const walk = (dir) => {
    for (const e of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      const file = path.join(dir, e.name);
      if (e.isDirectory()) walk(file);
      else { hash.update(path.relative(root, file)); hash.update(e.isSymbolicLink() ? fs.readlinkSync(file) : fs.readFileSync(file)); }
    }
  };
  walk(root); return hash.digest('hex');
};
export function inputIdentity() {
  const baseline = process.env.BBX_BASELINE || path.join(repo, 'resources/PilotDeck');
  const target = process.env.BBX_TARGET || path.join(repo, 'src/target/debug');
  return {
    rust: gitIdentity(process.env.BBX_SRC || path.resolve(target, '../..')),
    ts: gitIdentity(baseline),
    binaries: Object.fromEntries(['pilotdeck', 'pilotdeck-agent-loop-sidecar'].map((name) => {
      const file = path.join(target, name); return [name, fs.existsSync(file) ? fileHash(file) : null];
    })),
    components: { pdf: { path: process.env.BBX_PDF_TOOL ?? null,
      sha256: process.env.BBX_PDF_TOOL && fs.existsSync(process.env.BBX_PDF_TOOL) ? fileHash(process.env.BBX_PDF_TOOL) : null } },
    baselineDist: treeHash(path.join(baseline, 'dist')),
    baselineLock: fs.existsSync(path.join(baseline, 'pnpm-lock.yaml')) ? fileHash(path.join(baseline, 'pnpm-lock.yaml')) : null,
    baselineEntry: fs.existsSync(path.join(baseline, 'dist/src/cli/pilotdeck.js')) ? fileHash(path.join(baseline, 'dist/src/cli/pilotdeck.js')) : null,
    harness: Object.fromEntries(['check.mjs', 'run.mjs', 'diff.mjs', 'evidence.mjs', 'mock-support.mjs'].map((name) => [name, fileHash(path.join(here, name))])),
    tools: { node: process.version, executable: fileHash(process.execPath), rust: command('rustc', ['--version'], repo), pnpm: command('pnpm', ['--version'], baseline) },
    platform: { os: process.platform, arch: process.arch, release: os.release() },
  };
}
export function scenarioIdentity(file) {
  return { name: path.basename(file, '.json'), path: path.resolve(file), sha256: fileHash(file) };
}
export function checkWorker(expected, result) {
  const errors = [];
  let rows = [];
  for (const line of result.stdout.split('\n').filter(Boolean)) {
    try { const row = JSON.parse(line); if (!row || typeof row.name !== 'string') throw Error('invalid row'); rows.push(row); }
    catch { errors.push('malformed worker result'); }
  }
  const names = new Set(expected.map((s) => s.name));
  for (const name of names) if (rows.filter((r) => r.name === name).length !== 1) errors.push(`missing or duplicate scenario: ${name}`);
  for (const row of rows) if (!names.has(row.name)) errors.push(`unexpected scenario: ${row.name}`);
  // Exit 1 with complete failed rows is the worker reporting observed failures.
  // A nonzero exit without those rows (or even with all SAME rows) is a harness failure.
  const reportedFailure = result.code === 1 && !errors.length && rows.some((r) => r.status !== 'SAME');
  if ((result.code !== 0 && !reportedFailure) || result.signal || result.error) errors.push(`worker exit=${result.code} signal=${result.signal ?? '-'} ${result.error ?? ''}`);
  return { rows, errors };
}
export function assertionResults(assertions, artifact, side) {
  const ids = new Set();
  return (assertions ?? []).map((a) => {
    if (!a.id || ids.has(a.id) || typeof a.path !== 'string' || !a.path.startsWith('/')) throw Error('assertions require unique id and JSON pointer path');
    ids.add(a.id);
    if (a.sides !== undefined && (!Array.isArray(a.sides) || !a.sides.length || a.sides.some((s) => !['ts', 'rust'].includes(s)))) throw Error(`assertion ${a.id} has invalid sides`);
    if (side && a.sides && !a.sides.includes(side)) return { id: a.id, skipped: true };
    const actual = a.path.slice(1).split('/').map((p) => p.replaceAll('~1', '/').replaceAll('~0', '~')).reduce((v, p) => v?.[p], artifact);
    const checks = [];
    if ('equals' in a) checks.push(isDeepStrictEqual(actual, a.equals));
    if ('exists' in a) checks.push((actual !== undefined) === a.exists);
    if ('includes' in a) checks.push(typeof actual === 'string' && actual.includes(a.includes));
    if ('matches' in a) checks.push(typeof actual === 'string' && new RegExp(a.matches).test(actual));
    if (!checks.length) throw Error(`assertion ${a.id} has no predicate`);
    return { id: a.id, passed: checks.every(Boolean) };
  });
}
export function validateArtifact(artifact, scenario, expected) {
  const errors = [];
  const evidence = artifact.evidence;
  if (!evidence || evidence.schema !== 1 || evidence.runId !== expected.runId || evidence.side !== expected.side
    || !isDeepStrictEqual(evidence.scenario, expected.scenario) || !isDeepStrictEqual(evidence.inputs, expected.inputs)) errors.push('artifact identity mismatch');
  if (!Array.isArray(artifact.failures)) errors.push('missing failures collection');
  else errors.push(...artifact.failures.filter((f) => !f.expected).map((f) => `${f.at}:${f.kind}`));
  if (!Array.isArray(artifact.steps) || (!artifact.startFailure && artifact.steps.length !== scenario.steps.length)) errors.push('missing or duplicate steps');
  if (artifact.startFailure && scenario.expectStartFailure !== true) errors.push('unexpected start failure');
  const assertions = assertionResults(scenario.assertions, artifact, expected.side);
  if (!isDeepStrictEqual(evidence?.assertions, assertions)) errors.push('required assertions were not executed');
  if (assertions.some((a) => !a.skipped && !a.passed)) errors.push('required assertion failed');
  if (expected.side === 'rust') for (const accepted of scenario.accepted ?? []) {
    if (!Array.isArray(accepted.assertions) || !accepted.assertions.length || accepted.assertions.some((id) => !assertions.some((a) => a.id === id && a.passed))) errors.push(`accepted difference ${accepted.id} lacks executed positive assertions`);
  }
  return errors;
}
export function loadRun(outDir, { verifyCurrent = true } = {}) {
  const manifest = JSON.parse(fs.readFileSync(path.join(outDir, 'manifest.json'), 'utf8'));
  if (manifest.errors?.length) throw Error('run has validation errors');
  if (manifest.schema !== 1 || !manifest.complete || manifest.mode !== 'compare') throw Error('not a completed comparison run');
  const rows = manifest.results;
  if (!Array.isArray(rows) || rows.length !== manifest.scenarios.length || new Set(rows.map((r) => r.name)).size !== rows.length) throw Error('incomplete or duplicate run results');
  for (const scenario of manifest.scenarios) {
    const row = rows.find((r) => r.name === scenario.name);
    if (!row) throw Error(`missing scenario ${scenario.name}`);
    if (verifyCurrent && fileHash(scenario.path) !== scenario.sha256) throw Error(`scenario changed: ${scenario.name}`);
    for (const [file, hash] of Object.entries(row.files ?? {})) if (fileHash(path.resolve(outDir, file)) !== hash) throw Error(`artifact changed: ${file}`);
    if (row.status === 'SAME') {
      if (!row.artifacts?.length || row.artifacts.length !== 2 || !row.report || [...row.artifacts, row.report].some((f) => !row.files?.[f])) throw Error(`missing evidence: ${row.name}`);
      const spec = JSON.parse(fs.readFileSync(scenario.path, 'utf8'));
      for (const [i, file] of row.artifacts.entries()) {
        const errors = validateArtifact(JSON.parse(fs.readFileSync(path.resolve(outDir, file), 'utf8')), spec,
          { runId: manifest.runId, side: i ? 'rust' : 'ts', scenario, inputs: manifest.inputs });
        if (errors.length) throw Error(`${row.name}: ${errors.join('; ')}`);
      }
    }
  }
  const passed = rows.filter((r) => r.status === 'SAME').length;
  if (manifest.counts?.passed !== passed || manifest.counts?.failed !== rows.length - passed) throw Error('result counts mismatch');
  if (verifyCurrent && !isDeepStrictEqual(inputIdentity(), manifest.inputs)) throw Error('source, binary, tools or harness changed since run');
  return manifest;
}
