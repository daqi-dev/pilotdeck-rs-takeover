import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { BASELINE, SOURCE_FILES, sha, parseCaptureArgs, captureIdentity, verifyIdentity, rawRequest, verifyRequestBytes, verifyBoundFiles, driveRaw, treeSnapshot } from './https-evidence.mjs';

function temporary(run) {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'c1-https-evidence-synthetic-')));
  try { return run(root); } finally { fs.rmSync(root, { recursive: true, force: true }); }
}
function sourceFixture(root) {
  for (const file of SOURCE_FILES) { fs.mkdirSync(path.dirname(path.join(root, file)), { recursive: true }); fs.writeFileSync(path.join(root, file), `synthetic ${file}`); }
  for (const file of ['fake-node', 'setup-https-capture.mjs', 'https-evidence.mjs', 'fake-rust']) fs.writeFileSync(path.join(root, file), `synthetic ${file}`);
  return { version: 'v22.22.0', platform: 'linux-synthetic', executable: path.join(root, 'fake-node'), driverFiles: ['setup-https-capture.mjs', 'https-evidence.mjs'].map(file => path.join(root, file)) };
}
test('complete mode requires explicit full Rust identity and legacy mode stays explicit', () => {
  assert.equal(parseCaptureArgs(['ts', '/baseline', '/new']).complete, false);
  assert.throws(() => parseCaptureArgs(['rust', '/bin', '/new', '--complete']), /rust-root/);
  assert.throws(() => parseCaptureArgs(['rust', '/bin', '/new', '--rust-root', '/source', '--rust-sha', 'a'.repeat(40)]), /require complete/);
  assert.throws(() => parseCaptureArgs(['rust', '/bin', '/new', '--complete', '--rust-root', '/source', '--rust-sha', 'short']), /full/);
  assert.equal(parseCaptureArgs(['rust', '/bin', '/new', '--complete', '--rust-root', '/source', '--rust-sha', 'a'.repeat(40)]).complete, true);
});
test('TS identity fixes complete source/dist and runtime/driver hashes; wrong SHA/dirty/version fail', () => temporary(root => {
  const runtime = sourceFixture(root), options = { side: 'ts', executable: root, complete: true };
  const fakeGit = (_root, args) => args[0] === 'rev-parse' ? BASELINE : '';
  const identity = captureIdentity(options, runtime, fakeGit);
  verifyIdentity(identity, { side: 'ts', driverHashes: identity.driverFiles });
  assert.equal(identity.source.files['dist/src/cli/pilotdeck.js'], identity.executableSha256);
  assert.equal(identity.nodeSha256, sha(fs.readFileSync(runtime.executable)));
  assert.throws(() => captureIdentity(options, runtime, (_root, args) => args[0] === 'rev-parse' ? 'a'.repeat(40) : ''), /fixed SHA/);
  assert.throws(() => captureIdentity(options, runtime, (_root, args) => args[0] === 'rev-parse' ? BASELINE : ' M src/cli/pilotdeck.ts'), /clean tracked/);
  assert.throws(() => captureIdentity(options, { ...runtime, version: 'v24.0.0' }, fakeGit), /fixed Node/);
  const changed = structuredClone(identity); changed.source.sourceCommit = 'b'.repeat(40); assert.throws(() => verifyIdentity(changed, { side: 'ts', driverHashes: identity.driverFiles }), /source SHA/);
  const missing = structuredClone(identity); delete missing.source.files[SOURCE_FILES[0]]; assert.throws(() => verifyIdentity(missing, { side: 'ts', driverHashes: identity.driverFiles }));
}));
test('Rust identity binds source SHA and exact executable hash to external build receipt', () => temporary(root => {
  const runtime = sourceFixture(root), sourceSha = 'a'.repeat(40), options = { side: 'rust', executable: path.join(root, 'fake-rust'), complete: true, 'rust-root': root, 'rust-sha': sourceSha };
  const identity = captureIdentity(options, runtime, (_root, args) => args[0] === 'rev-parse' ? sourceSha : '');
  verifyIdentity(identity, { side: 'rust', rustSha: sourceSha, binarySha256: identity.executableSha256, driverHashes: identity.driverFiles });
  assert.throws(() => verifyIdentity(identity, { side: 'rust', rustSha: 'b'.repeat(40), binarySha256: identity.executableSha256, driverHashes: identity.driverFiles }), /source SHA/);
  assert.throws(() => verifyIdentity(identity, { side: 'rust', rustSha: sourceSha, binarySha256: '0'.repeat(64), driverHashes: identity.driverFiles }), /binary/);
  assert.throws(() => verifyIdentity(identity, { side: 'rust', rustSha: sourceSha, binarySha256: identity.executableSha256, driverHashes: {} }), /driver/);
}));
test('raw request keeps header casing/order/duplicates and exact body bytes', () => {
  const bytes = Buffer.from('{ "value": "中文" }\n');
  const request = rawRequest({ method: 'POST', url: '/fixture', httpVersion: '1.1', headers: { host: 'fixture', 'x-trace': 'one, two' }, rawHeaders: ['Host', 'fixture', 'X-Trace', 'one', 'x-trace', 'two'] }, bytes, 'request-0.body.bin');
  verifyRequestBytes(request, bytes); assert.deepEqual(request.rawHeaders, ['Host', 'fixture', 'X-Trace', 'one', 'x-trace', 'two']);
  assert.throws(() => verifyRequestBytes(request, Buffer.from(JSON.stringify(request.body))), /equal|hash/);
  assert.throws(() => verifyRequestBytes({ ...request, bodySha256: '0'.repeat(64) }, bytes), /raw hash/);
  assert.throws(() => verifyRequestBytes({ ...request, body: { value: 'wrong' } }, bytes), /decoded body/);
  const invalid = Buffer.from('{not-json');
  const captured = rawRequest({ method: 'POST', url: '/fixture', httpVersion: '1.1', headers: {}, rawHeaders: [] }, invalid, 'request-1.body.bin');
  assert.equal(captured.bodySha256, sha(invalid)); assert.equal(typeof captured.bodyParseError, 'string');
});
test('identity-bound evidence rejects changed identity, byte hash and unsafe paths', () => {
  const files = { 'stdout.bin': Buffer.from('raw\0stdout'), 'files-after.json': Buffer.from('[]\n') }, id = 'a'.repeat(64);
  const binding = { identitySha256: id, files: Object.fromEntries(Object.entries(files).map(([file, bytes]) => [file, sha(bytes)])) };
  verifyBoundFiles(binding, id, file => files[file]);
  assert.throws(() => verifyBoundFiles(binding, 'b'.repeat(64), file => files[file]), /identity/);
  assert.throws(() => verifyBoundFiles(binding, id, file => file === 'stdout.bin' ? Buffer.from('changed') : files[file]), /hash/);
  assert.throws(() => verifyBoundFiles({ identitySha256: id, files: { '../outside': sha('x') } }, id, () => Buffer.from('x')), /stays in case/);
});
test('raw stdout/stderr preserve split UTF-8 and binary bytes from a fake child only', async () => {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'c1-https-raw-fake-child-')));
  try {
    const fake = path.join(root, 'fake.mjs');
    fs.writeFileSync(fake, "const bytes=Buffer.from('提示: ');process.stdout.write(bytes.subarray(0,1));setTimeout(()=>{process.stdout.write(bytes.subarray(1));},5);process.stdin.once('data',()=>{process.stdout.write('回答完毕\\n');process.stderr.write(Buffer.from([0,255]));process.exit(0);});");
    const value = await driveRaw(process.execPath, [fake], { PATH: process.env.PATH, HOME: root, PILOT_HOME: path.join(root, 'pilot') }, root, [['提示: ', 'yes']], 2000);
    assert.equal(value.exitCode, 0); assert.equal(value.answered, 1); assert.equal(value.problem, undefined);
    assert.deepEqual(value.stdoutBytes, Buffer.from('提示: 回答完毕\n')); assert.deepEqual(value.stderrBytes, Buffer.from([0, 255]));
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
test('disk manifest includes empty directories and exact file hashes', () => temporary(root => {
  for (const dir of ['home/empty', 'config', 'project']) fs.mkdirSync(path.join(root, dir), { recursive: true });
  fs.writeFileSync(path.join(root, 'config/pilotdeck.yaml'), 'enabled: true\n');
  const rows = treeSnapshot(root); assert.ok(rows.some(row => row.path === 'home/empty' && row.directory));
  assert.equal(rows.find(row => row.path === 'config/pilotdeck.yaml').sha256, sha('enabled: true\n'));
}));
