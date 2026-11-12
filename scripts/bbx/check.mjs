#!/usr/bin/env node
// Every invocation owns a fresh directory and a complete, verifiable manifest.
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { randomUUID } from 'node:crypto';
import { isDeepStrictEqual } from 'node:util';
import { here, repo, fileHash, inputIdentity, scenarioIdentity, validateArtifact, checkWorker } from './evidence.mjs';

const args = process.argv.slice(2);
let outDir, self = false, slots, named = [];
try {
  for (let i = 0; i < args.length; i++) {
    if (args[i] === '--self') self = true;
    else if (args[i] === '--out') { if (!args[++i]) throw Error('--out needs a directory'); outDir = path.resolve(args[i]); }
    else if (args[i] === '--slots') { if (!args[++i]) throw Error('--slots needs a list'); slots = args[i].split(',').map(Number); }
    else if (args[i].startsWith('--')) throw Error(`unknown option ${args[i]}`);
    else named.push(args[i]);
  }
  if (slots && (!slots.length || new Set(slots).size !== slots.length || slots.some((s) => !Number.isInteger(s) || s < 0 || s > 8))) throw Error('slots must be unique integers 0..8');
  outDir ??= path.join(repo, '_build/bbx', new Date().toISOString().replaceAll(':', '-') + '-' + randomUUID().slice(0, 8));
  fs.mkdirSync(path.dirname(outDir), { recursive: true });
  fs.mkdirSync(outDir); // EEXIST is intentional: stale reports must never be reused.
  const files = (named.length ? named : fs.readdirSync(path.join(here, 'scenarios')).filter((f) => f.endsWith('.json')).sort())
    .map((n) => path.resolve(fs.existsSync(n) ? n : path.join(here, 'scenarios', n.endsWith('.json') ? n : n + '.json')));
  const scenarios = files.map(scenarioIdentity);
  if (!scenarios.length || new Set(scenarios.map((s) => s.name)).size !== scenarios.length) throw Error('empty or duplicate scenario selection');
  const manifest = { schema: 1, runId: process.env.BBX_RUN_ID || randomUUID(), mode: self ? 'self' : 'compare', started: new Date().toISOString(), complete: false, inputs: inputIdentity(), scenarios, results: [], errors: [] };
  const write = () => fs.writeFileSync(path.join(outDir, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
  write();
  if (slots) {
    const shares = slots.map(() => []); scenarios.forEach((s, i) => shares[i % slots.length].push(s));
    const workers = await Promise.all(slots.map((slot, i) => new Promise((resolve) => {
      if (!shares[i].length) return resolve({ rows: [], errors: [] });
      const workerDir = path.join(outDir, `slot-${slot}`);
      const child = spawn(process.execPath, [path.join(here, 'check.mjs'), '--out', workerDir, ...(self ? ['--self'] : []), ...shares[i].map((s) => s.path)],
        { env: { ...process.env, BBX_SLOT: String(slot), BBX_CHECK_ROWS: '1', BBX_RUN_ID: manifest.runId }, stdio: ['ignore', 'pipe', 'pipe'] });
      let stdout = '', stderr = '', error;
      child.stdout.on('data', (d) => { stdout += d; }); child.stderr.on('data', (d) => { stderr += d; });
      child.on('error', (e) => { error = String(e); });
      child.on('close', (code, signal) => {
        fs.writeFileSync(path.join(outDir, `worker-${slot}.log`), stderr);
        const result = checkWorker(shares[i], { code, signal, error, stdout });
        if (result.errors.length) result.rows = shares[i].map((s) => ({ name: s.name, status: 'RUN FAILED', note: result.errors.join('; ') }));
        for (const row of result.rows) {
          row.artifacts = row.artifacts?.map((f) => path.join(`slot-${slot}`, f));
          row.report = row.report && path.join(`slot-${slot}`, row.report);
          row.files = Object.fromEntries(Object.entries(row.files ?? {}).map(([f, h]) => [path.join(`slot-${slot}`, f), h]));
        }
        resolve(result);
      });
    })));
    manifest.results = workers.flatMap((r) => r.rows);
    manifest.errors.push(...workers.flatMap((r) => r.errors));
    for (const s of scenarios) if (!manifest.results.some((r) => r.name === s.name)) manifest.results.push({ name: s.name, status: 'RUN FAILED', note: 'worker did not return scenario' });
  } else {
    const node = (script, ...a) => spawnSync(process.execPath, [path.join(here, script), ...a], { encoding: 'utf8', timeout: Number(process.env.BBX_COLLECT_TIMEOUT_MS || 600000), killSignal: 'SIGTERM', maxBuffer: 16 * 1024 * 1024, env: { ...process.env, BBX_RUN_ID: manifest.runId } });
    for (const identity of scenarios) {
      const spec = JSON.parse(fs.readFileSync(identity.path, 'utf8'));
      const sides = self ? ['ts', 'ts'] : ['ts', 'rust'];
      const artifacts = sides.map((side, i) => `${identity.name}.${side}${self ? '-' + (i + 1) : ''}.json`);
      const row = { name: identity.name, status: 'RUN FAILED', artifacts, files: {}, note: '' };
      const errors = [];
      for (const [i, side] of sides.entries()) {
        let result;
        for (let waited = 0; ; waited += 15) {
          result = node('run.mjs', side, identity.path, path.join(outDir, artifacts[i]));
          if (result.status !== 3 || waited >= 1200) break;
          Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 15000);
        }
        fs.writeFileSync(path.join(outDir, `${artifacts[i]}.log`), result.stderr ?? String(result.error ?? ''));
        if (result.status !== 0 || result.signal || result.error) errors.push(`${side}: collector exit=${result.status} signal=${result.signal ?? '-'} ${result.error ?? ''}`);
        else {
          try {
            const artifact = JSON.parse(fs.readFileSync(path.join(outDir, artifacts[i]), 'utf8'));
            errors.push(...validateArtifact(artifact, spec, { runId: manifest.runId, side, scenario: identity, inputs: manifest.inputs }).map((e) => `${side}:${e}`));
          } catch (e) { errors.push(`${side}: ${e.message}`); }
        }
      }
      if (!errors.length) {
        const diff = node('diff.mjs', ...artifacts.map((f) => path.join(outDir, f)), '--max', '400', ...(self ? ['--self'] : []));
        row.report = `${identity.name}.${self ? 'self' : 'diff'}.txt`;
        fs.writeFileSync(path.join(outDir, row.report), (diff.stdout ?? '') + (diff.stderr ?? ''));
        const verdict = diff.stdout?.trim().split('\n').pop();
        row.status = diff.status === 0 && !diff.signal && verdict === 'SAME' ? 'SAME' : diff.status === 1 && /^DIFF \d+/.test(verdict ?? '') ? verdict : 'RUN FAILED';
        if (row.status === 'RUN FAILED') errors.push(`comparison exit=${diff.status} signal=${diff.signal ?? '-'}`);
      } else row.status = 'INVALID';
      row.note = errors.join('; ');
      for (const f of [...artifacts, ...(row.report ? [row.report] : [])]) if (fs.existsSync(path.join(outDir, f))) row.files[f] = fileHash(path.join(outDir, f));
      manifest.results.push(row); write();
    }
  }
  if (!isDeepStrictEqual(inputIdentity(), manifest.inputs) || scenarios.some((s) => fileHash(s.path) !== s.sha256)) manifest.errors.push('inputs changed during run');
  manifest.results.sort((a, b) => a.name.localeCompare(b.name));
  manifest.complete = manifest.results.length === scenarios.length && new Set(manifest.results.map((r) => r.name)).size === scenarios.length;
  manifest.finished = new Date().toISOString();
  manifest.counts = { passed: manifest.results.filter((r) => r.status === 'SAME').length, failed: manifest.results.filter((r) => r.status !== 'SAME').length, skipped: 0, blocked: 0 };
  write();
  if (process.env.BBX_CHECK_ROWS) for (const row of manifest.results) console.log(JSON.stringify(row));
  else {
    for (const row of manifest.results) console.log(`${row.name}\t${row.status}\t${row.note || row.report || ''}`);
    console.log(`${self ? '基线自比' : '基线 vs Rust'}：${manifest.counts.passed}/${scenarios.length} SAME；失败 ${manifest.counts.failed}，跳过 0，阻塞 0`);
    console.log(`产物：${outDir}`);
  }
  for (const error of manifest.errors) console.error(`bbx: ${error}`);
  process.exit(manifest.errors.length || manifest.counts.failed || !manifest.complete ? 1 : 0);
} catch (error) { console.error(`bbx: ${error.message}`); process.exit(2); }
