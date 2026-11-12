// HTTPS fixture provenance and byte capture only; no product imports.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
export const BASELINE = 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
export const SOURCE_FILES = ['package.json', 'pnpm-lock.yaml', 'src/cli/pilotdeck.ts', 'src/cli/commands/gatewaySetup.ts', 'dist/src/cli/pilotdeck.js', 'dist/src/cli/commands/gatewaySetup.js'];
export const sha = bytes => createHash('sha256').update(bytes).digest('hex');
export const json = value => JSON.stringify(value, null, 2) + '\n';
export function parseCaptureArgs(argv) {
  const [side, executable, output, ...rest] = argv;
  assert.ok(['ts', 'rust'].includes(side) && executable && output,
    'usage: setup-https-capture.mjs <ts BASELINE|rust BINARY> NEW_OUTPUT_DIR [--complete [--rust-root ROOT --rust-sha SHA]]');
  const options = { side, executable, output, complete: false };
  for (let i = 0; i < rest.length; i++) {
    const flag = rest[i];
    if (flag === '--complete') { assert.equal(options.complete, false, 'duplicate --complete'); options.complete = true; }
    else {
      assert.ok(['--rust-root', '--rust-sha'].includes(flag) && rest[i + 1], `unknown/incomplete ${flag}`);
      assert.equal(options[flag.slice(2)], undefined, `duplicate ${flag}`); options[flag.slice(2)] = rest[++i];
    }
  }
  if (options['rust-root'] || options['rust-sha']) assert.ok(options.complete && side === 'rust', 'Rust source identity flags require complete Rust mode');
  if (options.complete && side === 'rust') {
    assert.ok(options['rust-root'], '--rust-root required in complete Rust mode');
    assert.match(options['rust-sha'] ?? '', /^[a-f0-9]{40}$/, 'full --rust-sha required in complete Rust mode');
  }
  return options;
}
export function gitValue(root, args) {
  const result = spawnSync('git', ['-C', root, ...args], { encoding: 'utf8' });
  assert.equal(result.status, 0, `git ${args.join(' ')}: ${result.stderr}`); return result.stdout.trim();
}
export function captureIdentity(options, runtime, git = gitValue) {
  const location = fs.realpathSync(options.executable), entry = options.side === 'ts' ? path.join(location, 'dist/src/cli/pilotdeck.js') : location;
  const identity = { schemaVersion: 2, captureMode: options.complete ? 'complete' : 'legacy', provenanceComplete: options.complete,
    side: options.side, node: runtime.version, platform: runtime.platform, nodeExecutable: fs.realpathSync(runtime.executable),
    nodeSha256: sha(fs.readFileSync(runtime.executable)), entry, executableSha256: sha(fs.readFileSync(entry)),
    driverFiles: Object.fromEntries(runtime.driverFiles.map(file => [path.basename(file), sha(fs.readFileSync(file))])) };
  assert.equal(runtime.version, 'v22.22.0', 'fixed Node 22.22.0 required');
  if (options.side === 'ts' || options.complete) {
    const root = options.side === 'ts' ? location : fs.realpathSync(options['rust-root']);
    const sourceCommit = git(root, ['rev-parse', 'HEAD']);
    assert.equal(sourceCommit, options.side === 'ts' ? BASELINE : options['rust-sha'], 'source checkout must match explicit fixed SHA');
    const status = git(root, ['status', '--porcelain', '--untracked-files=no']);
    if (options.complete) assert.equal(status, '', 'complete evidence requires clean tracked source');
    identity.source = { root, sourceCommit, trackedClean: status === '', files: options.side === 'ts'
      ? Object.fromEntries(SOURCE_FILES.map(file => [file, sha(fs.readFileSync(path.join(root, file)))])) : {} };
  }
  return identity;
}
export function verifyIdentity(identity, { side, rustSha, binarySha256, driverHashes } = {}) {
  assert.equal(identity.schemaVersion, 2); assert.equal(identity.captureMode, 'complete'); assert.equal(identity.provenanceComplete, true);
  assert.equal(identity.side, side); assert.equal(identity.node, 'v22.22.0'); assert.match(identity.nodeSha256, /^[a-f0-9]{64}$/);
  assert.ok(path.isAbsolute(identity.nodeExecutable)); assert.match(identity.platform, /^linux-/);
  assert.equal(identity.source.trackedClean, true);
  assert.ok(path.isAbsolute(identity.source.root)); assert.ok(path.isAbsolute(identity.entry));
  assert.equal(identity.source.sourceCommit, side === 'ts' ? BASELINE : rustSha, 'recorded source SHA');
  assert.match(identity.executableSha256, /^[a-f0-9]{64}$/);
  if (side === 'rust') assert.equal(identity.executableSha256, binarySha256, 'recorded binary must match independent build hash');
  else {
    assert.equal(identity.entry, path.join(identity.source.root, 'dist/src/cli/pilotdeck.js'));
    assert.deepEqual(Object.keys(identity.source.files).sort(), [...SOURCE_FILES].sort());
    for (const digest of Object.values(identity.source.files)) assert.match(digest, /^[a-f0-9]{64}$/);
    assert.equal(identity.source.files['dist/src/cli/pilotdeck.js'], identity.executableSha256);
  }
  assert.deepEqual(identity.driverFiles, driverHashes, 'recorded driver/helper hashes match comparator-supported capture');
}
export function verifyBoundFiles(binding, identitySha256, read) {
  assert.equal(binding.identitySha256, identitySha256, 'case evidence belongs to captured identity');
  assert.ok(binding.files && Object.keys(binding.files).length > 0, 'bound evidence file list');
  for (const [file, digest] of Object.entries(binding.files)) {
    assert.ok(!path.isAbsolute(file) && !file.split('/').includes('..'), 'bound file stays in case');
    assert.match(digest, /^[a-f0-9]{64}$/); assert.equal(sha(read(file)), digest, `bound raw evidence hash ${file}`);
  }
}
export function treeSnapshot(root) {
  const rows = [];
  const visit = relative => {
    const absolute = path.join(root, relative), info = fs.lstatSync(absolute);
    if (info.isSymbolicLink()) rows.push({ path: relative, symlink: fs.readlinkSync(absolute) });
    else if (info.isDirectory()) { rows.push({ path: relative, directory: true }); for (const name of fs.readdirSync(absolute).sort()) visit(`${relative}/${name}`); }
    else { assert.ok(info.isFile()); const bytes = fs.readFileSync(absolute); rows.push({ path: relative, size: bytes.length, sha256: sha(bytes) }); }
  };
  for (const name of ['home', 'config', 'project']) visit(name);
  return rows;
}
export function rawRequest(request, bytes, bodyFile) {
  assert.ok(Buffer.isBuffer(bytes));
  let decoded = {};
  if (bytes.length) {
    try { decoded = { body: JSON.parse(bytes.toString('utf8')) }; }
    catch (error) { decoded = { bodyParseError: error.message }; }
  }
  return { method: request.method, path: request.url, headers: request.headers,
    httpVersion: request.httpVersion, rawHeaders: [...request.rawHeaders], bodyFile,
    bodySize: bytes.length, bodySha256: sha(bytes), ...decoded };
}
export function verifyRequestBytes(request, bytes) {
  assert.equal(bytes.length, request.bodySize); assert.equal(sha(bytes), request.bodySha256, 'request body raw hash');
  assert.deepEqual(bytes.length ? JSON.parse(bytes.toString('utf8')) : undefined, request.body, 'request decoded body matches raw bytes');
  assert.ok(Array.isArray(request.rawHeaders) && request.rawHeaders.length % 2 === 0 && request.rawHeaders.every(value => typeof value === 'string'));
  assert.match(request.httpVersion, /^1\.[01]$/);
  const raw = request.rawHeaders;
  for (const [name, value] of Object.entries(request.headers)) {
    const values = [];
    for (let i = 0; i < raw.length; i += 2) if (raw[i].toLowerCase() === name) values.push(raw[i + 1]);
    assert.ok(values.length, `decoded header ${name} has raw source`);
    // Node's duplicate-header policy is field-specific. Preserve all raw
    // duplicates in comparison; verify exact values for the ordinary single
    // headers these fixtures send, without inventing a general HTTP parser.
    if (values.length === 1) assert.deepEqual(value, values[0], `decoded header ${name}`);
  }
  for (let i = 0; i < raw.length; i += 2) assert.ok(Object.hasOwn(request.headers, raw[i].toLowerCase()), 'raw header retained in decoded view');
}
export async function driveRaw(command, args, env, cwd, answers, timeoutMs = 15000) {
  const child = spawn(command, args, { cwd, env, detached: true, stdio: ['pipe', 'pipe', 'pipe'] });
  const chunks = { stdout: [], stderr: [] }; let stdout = '', answered = 0, cursor = 0, problem;
  const terminate = () => { try { process.kill(-child.pid, 'SIGKILL'); } catch {} };
  const timer = setTimeout(() => { problem = `deadline after ${timeoutMs}ms`; terminate(); }, timeoutMs);
  for (const stream of ['stdout', 'stderr']) child[stream].on('data', bytes => {
    chunks[stream].push(Buffer.from(bytes));
    const all = Buffer.concat(chunks[stream]);
    if (all.length > 1024 * 1024) { problem = `${stream} exceeded 1MiB`; terminate(); return; }
    if (stream !== 'stdout') return;
    stdout = all.toString('utf8');
    if (answered >= answers.length) return;
    const [prompt, response] = answers[answered], position = stdout.indexOf(prompt, cursor);
    if (position < 0) return;
    cursor = position + prompt.length; answered++;
    if (response === null) child.stdin.end(); else child.stdin.write(`${response}\n`);
  });
  child.stdin.on('error', error => { problem ??= `stdin: ${error.message}`; terminate(); });
  const status = await new Promise(resolve => {
    child.once('error', error => { problem = `spawn: ${error.message}`; });
    child.once('close', (exitCode, signal) => resolve({ exitCode, signal }));
  });
  clearTimeout(timer);
  const stdoutBytes = Buffer.concat(chunks.stdout), stderrBytes = Buffer.concat(chunks.stderr);
  return { ...status, stdout: stdoutBytes.toString('utf8'), stderr: stderrBytes.toString('utf8'), stdoutBytes, stderrBytes,
    answered, ...(problem ? { problem } : {}) };
}
