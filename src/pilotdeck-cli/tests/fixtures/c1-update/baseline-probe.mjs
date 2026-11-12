// Read-only probe of the fixed baseline; every subprocess is a local mock.
import { spawn, execFileSync } from 'node:child_process';
import assert from 'node:assert/strict';
import { pathToFileURL } from 'node:url';
import path from 'node:path';

const baseline = process.argv[2];
if (!baseline) throw new Error('Pass the absolute fixed-baseline directory.');
const sourceSha = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: baseline, encoding: 'utf8' }).trim();
assert.equal(sourceSha, 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85', 'fixed TS baseline');
assert.equal(process.version, 'v22.22.0', 'fixed Node runtime');
const { runUpdateProcess } = await import(pathToFileURL(path.join(baseline, 'src/runtime/updateCommand.ts')));
console.log(JSON.stringify({ baseline, sourceSha, node: process.version, platform: process.platform, arch: process.arch }));
for (const [name, script] of [
  ['complete', 'process.stdout.write("中文🙂\\n");process.stderr.write("错误信息\\n")'],
  ['large', 'process.stdout.write("中文🙂".repeat(10000))'],
  ['split', 'const b=Buffer.from("中文🙂\\n");process.stdout.write(b.subarray(0,2));setTimeout(()=>process.stdout.write(b.subarray(2)),50)'],
  ['invalid', 'process.stdout.write(Buffer.from([0x61,0xff,0xe4,0xb8]))'],
]) {
  const chunks = [];
  const result = await runUpdateProcess([], {
    projectRoot: baseline,
    output: chunk => chunks.push(chunk),
    spawnImpl: (_command, _args, options) => spawn(process.execPath, ['-e', script], options),
  });
  const output = chunks.join('');
  assert.equal(result.code, 0, name);
  if (name === 'complete') assert.equal([...output].sort().join(''), [...'中文🙂\n错误信息\n'].sort().join(''));
  if (name === 'large') assert.equal(output, '中文🙂'.repeat(10000));
  if (name === 'split') assert.equal(output, '��文🙂\n', 'baseline corrupts the split character');
  if (name === 'invalid') assert.equal(output, 'a��', 'invalid bytes and EOF suffix are replaced');
  console.log(JSON.stringify({ name, code: result.code, length: output.length, replacements: output.split('\ufffd').length - 1, preview: output.slice(0, 30), chunks: chunks.map(chunk => chunk.length) }));
}
for (const [name, script] of [
  ['success', 'printf ready; printf warning >&2; exit 0'],
  ['noUpdate', 'exit 2'],
  ['failure', 'printf failed >&2; exit 7'],
  ['signal', 'kill -TERM $$'],
  ['stdin', 'if read line; then printf unexpected; else printf eof; fi'],
]) {
  const chunks = [];
  const result = await runUpdateProcess(['--check'], {
    projectRoot: baseline,
    output: chunk => chunks.push(chunk),
    spawnImpl: (_command, _args, options) => spawn('/bin/sh', ['-c', script], options),
  });
  const expectedCode = { success: 0, noUpdate: 2, failure: 7, signal: 1, stdin: 0 }[name];
  assert.equal(result.code, expectedCode, name);
  if (name === 'stdin') assert.equal(chunks.join(''), 'eof');
  if (name === 'success') assert.ok(['readywarning', 'warningready'].includes(chunks.join('')));
  if (name === 'failure') assert.equal(chunks.join(''), 'failed');
  console.log(JSON.stringify({ name, result, forwarded: chunks.join(''), cliCode: [0, 2].includes(result.code) ? 0 : 1 }));
}
