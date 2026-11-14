#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';

const args = process.argv.slice(2);
assert.deepEqual([args[0], args[2]], ['--baseline-root', '--output']);
assert.equal(args.length, 4);
assert.equal(process.versions.node, '22.22.0', 'this integration candidate is verified on Node 22.22.0');
const root = path.resolve(args[1]), output = path.resolve(args[3]);
assert.ok(!fs.existsSync(output), 'retain previous evidence; output must be new');
fs.mkdirSync(output, { recursive: true });
for (const name of ['home', 'pilot', 'tmp']) fs.mkdirSync(path.join(output, name));
const directory = path.dirname(fileURLToPath(import.meta.url));
const baseline = JSON.parse(fs.readFileSync(path.join(directory, 'baseline-identity.json'), 'utf8'));
const env = {
    PATH: process.env.PATH,
    HOME: path.join(output, 'home'), PILOT_HOME: path.join(output, 'pilot'), TMPDIR: path.join(output, 'tmp'),
    PILOTDECK_WEB_BASELINE_ROOT: root, NO_COLOR: '1', NODE_NO_WARNINGS: '1',
};
const git = spawnSync('/usr/bin/git', ['-C', root, 'rev-parse', 'HEAD'], { env, encoding: 'utf8' });
assert.equal(git.status, 0, git.stderr);
assert.equal(git.stdout.trim(), baseline.baselineCommit);
const commands = {
    controls: [process.execPath, '--test', path.join(directory, 'controls.test.mjs')],
    parseWithTsxHooks: [process.execPath, '--experimental-vm-modules', '--import', 'tsx', '--import', path.join(directory, 'register.mjs'), path.join(directory, 'parse-only.mjs')],
};
const results = {};
for (const [name, command] of Object.entries(commands)) {
    const child = spawnSync(command[0], command.slice(1), { cwd: path.join(root, 'ui'), env, encoding: 'utf8', timeout: 30_000 });
    fs.writeFileSync(path.join(output, `${name}.stdout`), child.stdout || '');
    fs.writeFileSync(path.join(output, `${name}.stderr`), child.stderr || '');
    results[name] = { command, exit: child.status, signal: child.signal, error: child.error?.message, passed: child.status === 0 };
}
const sha = filename => createHash('sha256').update(fs.readFileSync(filename)).digest('hex');
const receipt = {
    node: process.version, baselineCommit: git.stdout.trim(), baselineRoot: root,
    baselineSources: Object.fromEntries(Object.keys(baseline.files).map(name => [name, sha(path.join(root, name))])),
    implementation: Object.fromEntries(['baseline-identity.json', 'overlay.mjs', 'loader.mjs', 'register.mjs', 'controls.test.mjs', 'parse-only.mjs', 'verify.mjs'].map(name => [name, sha(path.join(directory, name))])),
    results, passed: Object.values(results).every(result => result.passed),
    limitations: ['The parser does not link or evaluate either UI module. Root must observe both loader receipts on the actual UI launch.', 'Mock Gateway controls do not replace the real pending interaction journey.'],
};
fs.writeFileSync(path.join(output, 'receipt.json'), `${JSON.stringify(receipt, null, 2)}\n`);
console.log(JSON.stringify({ passed: receipt.passed, output }));
process.exitCode = receipt.passed ? 0 : 1;
