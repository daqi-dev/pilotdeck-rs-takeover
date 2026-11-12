#!/usr/bin/env node
// Drive the real executable through sequential setup prompts. Never import
// product code or replace its network implementation. These cases are offline.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const casesFile = path.join(here, 'cases.json');

export async function drive(command, args, env, cwd, answers, timeoutMs = 10000) {
  const child = spawn(command, args, { cwd, env, detached: true, stdio: ['pipe', 'pipe', 'pipe'] });
  child.stdout.setEncoding('utf8'); child.stderr.setEncoding('utf8');
  let stdout = '', stderr = '', answered = 0, cursor = 0, problem;
  const terminate = () => { try { process.kill(-child.pid, 'SIGKILL'); } catch {} };
  const timer = setTimeout(() => { problem = `deadline after ${timeoutMs}ms`; terminate(); }, timeoutMs);
  child.stdout.on('data', bytes => {
    stdout += bytes.toString();
    if (stdout.length > 1024 * 1024) { problem = 'stdout exceeded 1MiB'; terminate(); return; }
    if (answered >= answers.length) return;
    const [prompt, response] = answers[answered];
    const position = stdout.indexOf(prompt, cursor);
    if (position < 0) return;
    cursor = position + prompt.length;
    answered++;
    if (response === null) child.stdin.end();
    else child.stdin.write(`${response}\n`);
  });
  child.stderr.on('data', bytes => {
    stderr += bytes.toString();
    if (stderr.length > 1024 * 1024) { problem = 'stderr exceeded 1MiB'; terminate(); }
  });
  child.stdin.on('error', error => { problem ??= `stdin: ${error.message}`; terminate(); });
  const result = await new Promise(resolve => {
    child.once('error', error => { problem = `spawn: ${error.message}`; });
    child.once('close', (exitCode, signal) => resolve({ exitCode, signal }));
  });
  clearTimeout(timer);
  return { ...result, stdout, stderr, answered, ...(problem ? { problem } : {}) };
}

export function verifyCase(spec, result, tree) {
  assert.equal(result.problem, undefined, `${spec.id}: ${result.problem}`);
  assert.equal(result.answered, spec.answers.length, `${spec.id}: all prompts answered`);
  assert.equal(result.exitCode, 0, `${spec.id}: exit code; stderr=${result.stderr}`);
  assert.equal(result.signal, null, `${spec.id}: normal termination`);
  assert.ok(result.stdout.includes(spec.stdoutIncludes), `${spec.id}: success/abandonment output`);
  const yaml = tree['pilotdeck.yaml'];
  assert.equal(typeof yaml, 'string', `${spec.id}: config exists`);
  for (const value of spec.yamlIncludes ?? []) assert.ok(yaml.includes(value), `${spec.id}: config contains ${value}`);
  for (const value of spec.yamlExcludes ?? []) assert.ok(!yaml.includes(value), `${spec.id}: config excludes ${value}`);
  if (spec.yamlEquals !== undefined) assert.equal(yaml, spec.yamlEquals, `${spec.id}: untouched config`);
  for (const [name, expected] of Object.entries(spec.files ?? {})) {
    assert.deepEqual(JSON.parse(tree[name]), expected, `${spec.id}: existing file preserved ${name}`);
  }
}

function treeAt(root) {
  const result = {};
  const visit = dir => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      const absolute = path.join(dir, entry.name);
      if (entry.isDirectory()) visit(absolute);
      else result[path.relative(root, absolute)] = fs.readFileSync(absolute, 'utf8');
    }
  };
  visit(root);
  return result;
}

async function main() {
  const [side, executable, output] = process.argv.slice(2);
  assert.ok(side === 'ts' || side === 'rust', 'usage: capture.mjs <ts BASELINE|rust BINARY> NEW_OUTPUT_DIR');
  assert.ok(executable && output, 'executable/baseline and new output directory required');
  const location = fs.realpathSync(executable);
  const command = side === 'ts' ? process.execPath : location;
  const entry = side === 'ts' ? path.join(location, 'dist/src/cli/pilotdeck.js') : location;
  const prefix = side === 'ts' ? [entry] : [];
  const out = path.resolve(output);
  fs.mkdirSync(out); // Existing evidence is never overwritten.
  const specs = JSON.parse(fs.readFileSync(casesFile, 'utf8'));
  const identity = {
    side, node: process.version, platform: `${os.platform()}-${os.arch()}`,
    executable: entry, executableSha256: sha(fs.readFileSync(entry)),
    casesSha256: sha(fs.readFileSync(casesFile)), captureSha256: sha(fs.readFileSync(fileURLToPath(import.meta.url))),
    sourceCommit: side === 'ts' ? spawnSync('git', ['-C', location, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).stdout.trim() : undefined,
  };
  if (side === 'ts') assert.equal(identity.sourceCommit, 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85', 'fixed TS source');
  fs.writeFileSync(path.join(out, 'identity.json'), JSON.stringify(identity, null, 2) + '\n');
  let failed = false;
  for (const spec of specs) {
    const root = path.join(out, spec.id);
    const home = path.join(root, 'home');
    const pilot = path.join(home, '.pilotdeck');
    const project = path.join(root, 'project');
    fs.mkdirSync(pilot, { recursive: true }); fs.mkdirSync(project);
    fs.writeFileSync(path.join(pilot, 'pilotdeck.yaml'), spec.config);
    for (const [name, value] of Object.entries(spec.files ?? {})) {
      assert.equal(path.basename(name), name, 'fixture files stay in isolated pilot home');
      fs.writeFileSync(path.join(pilot, name), typeof value === 'string' ? value : JSON.stringify(value, null, 2));
    }
    const env = {
      PATH: process.env.PATH, HOME: home, USERPROFILE: home, CFFIXED_USER_HOME: home,
      PILOT_HOME: pilot, PILOTDECK_CONFIG_PATH: path.join(pilot, 'pilotdeck.yaml'),
      TMPDIR: root, LANG: 'C', LC_ALL: 'C', TZ: 'UTC', TERM: 'dumb', NO_COLOR: '1',
      DOCKER_CONTAINER: '1', COMMIT_HASH: 'c1-fixture',
    };
    const result = await drive(command, [...prefix, ...spec.argv], env, project, spec.answers);
    const tree = treeAt(pilot);
    let assertionFailure;
    try { verifyCase(spec, result, tree); } catch (error) { assertionFailure = error.message; failed = true; }
    fs.writeFileSync(path.join(root, 'result.json'), JSON.stringify({ ...result, tree, ...(assertionFailure ? { assertionFailure } : {}) }, null, 2) + '\n');
    console.log(`${spec.id}: ${assertionFailure ? `FAIL ${assertionFailure}` : 'PASS'}`);
  }
  process.exitCode = failed ? 1 : 0;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main();
