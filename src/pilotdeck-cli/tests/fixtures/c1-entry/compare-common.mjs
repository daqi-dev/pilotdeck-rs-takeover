// Evidence-only helpers. No product execution, network, or oracle rewriting.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
export const BASELINE = 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
export const sha = bytes => createHash('sha256').update(bytes).digest('hex');
export const equal = (a, b, message) => assert.deepEqual(a, b, message);
export function reader(root) {
  const absolute = fs.realpathSync(root), hashes = {};
  const bytes = relative => {
    const target = path.resolve(absolute, relative);
    assert.ok(target.startsWith(absolute + path.sep), 'evidence path stays inside root');
    assert.ok(fs.realpathSync(target).startsWith(absolute + path.sep), 'evidence symlink must not escape root');
    const value = fs.readFileSync(target); hashes[relative] = sha(value); return value;
  };
  return { root: absolute, hashes, bytes, json: file => JSON.parse(bytes(file)),
    lines: file => bytes(file).toString('utf8').trimEnd().split('\n').filter(Boolean).map(line => JSON.parse(line)) };
}
export function argumentsFor(argv) {
  const [mode, left, right, ...rest] = argv;
  assert.ok(['--self', '--parity'].includes(mode) && left && right,
    'usage: compare.mjs <--self|--parity> LEFT_ROOT RIGHT_ROOT --out NEW_DIRECTORY [--rust-sha SHA --rust-binary-sha256 SHA256]');
  const options = { mode: mode.slice(2), left, right };
  for (let i = 0; i < rest.length; i += 2) {
    const key = rest[i];
    assert.ok(['--out', '--rust-sha', '--rust-binary-sha256'].includes(key) && rest[i + 1], `unknown/incomplete argument ${key}`);
    assert.equal(options[key.slice(2)], undefined, `duplicate ${key}`); options[key.slice(2)] = rest[i + 1];
  }
  assert.ok(options.out, '--out NEW_DIRECTORY is required');
  if (options.mode === 'self') assert.notEqual(fs.realpathSync(left), fs.realpathSync(right), 'self comparison needs two independent capture roots');
  else {
    assert.match(options['rust-sha'] ?? '', /^[a-f0-9]{40}$/, 'full Rust source SHA required');
    assert.match(options['rust-binary-sha256'] ?? '', /^[a-f0-9]{64}$/, 'independently recorded Rust executable SHA256 required');
  }
  assert.ok(!fs.existsSync(options.out), 'comparison output must be new');
  const requestedOutput = path.resolve(options.out);
  // mkdir below is intentionally non-recursive, so its parent must exist.
  // Resolve that parent before checking: a symlink alias must not turn a
  // seemingly separate output path into a write inside original evidence.
  const output = path.join(fs.realpathSync(path.dirname(requestedOutput)), path.basename(requestedOutput));
  for (const input of [left, right]) {
    const root = fs.realpathSync(input);
    assert.ok(output !== root && !output.startsWith(root + path.sep), 'comparison output must not mutate an input evidence tree');
  }
  options.out = output;
  return options;
}
// Only concrete, recorded fixture roots are rewritten. This never removes a
// field, matches arbitrary UUIDs, strips warning lines, or parses file contents.
export function rootMapper(root) {
  assert.ok(path.isAbsolute(root));
  return value => {
    assert.equal(value.includes('<C1_FIXTURE>'), false, 'literal normalization marker in evidence');
    return value.replaceAll(root, '<C1_FIXTURE>');
  };
}
export function mapStrings(value, map) {
  if (typeof value === 'string') return map(value);
  if (Array.isArray(value)) return value.map(item => mapStrings(item, map));
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, mapStrings(item, map)]));
  return value;
}
export function differences(left, right, pointer = '') {
  if (Object.is(left, right)) return [];
  if (Array.isArray(left) && Array.isArray(right)) {
    const result = []; for (let i = 0; i < Math.max(left.length, right.length); i++) result.push(...differences(left[i], right[i], `${pointer}/${i}`)); return result;
  }
  if (left && right && typeof left === 'object' && typeof right === 'object' && !Array.isArray(left) && !Array.isArray(right)) {
    const keys = [...new Set([...Object.keys(left), ...Object.keys(right)])].sort();
    return keys.flatMap(key => differences(left[key], right[key], `${pointer}/${key.replaceAll('~', '~0').replaceAll('/', '~1')}`));
  }
  return [{ path: pointer || '/', left: left === undefined ? { absent: true } : left, right: right === undefined ? { absent: true } : right }];
}
export function verifyRaw(reader, prefix, result) {
  for (const name of ['stdout', 'stderr']) {
    const bytes = reader.bytes(`${prefix}/${name}.bin`);
    equal(sha(bytes), result[`${name}Sha256`], `${prefix}: ${name} hash`);
    equal(bytes.toString('utf8'), result[name], `${prefix}: ${name} readable copy`);
    equal(Buffer.from(result[name]), bytes, `${prefix}: ${name} roundtrips as UTF-8`);
  }
}
export function snapshotProjection(rows, map) {
  const paths = new Set();
  return rows.map(row => {
    assert.equal(paths.has(row.path), false, 'duplicate snapshot path'); paths.add(row.path);
    assert.ok(row.path && !path.isAbsolute(row.path) && !row.path.split('/').includes('..'), 'safe snapshot path');
    if (row.symlink !== undefined) { equal(Object.keys(row).sort(), ['path', 'symlink']); return { path: map(row.path), symlink: map(row.symlink) }; }
    equal(Object.keys(row).sort(), ['base64', 'path', 'sha256', 'size']);
    const bytes = Buffer.from(row.base64, 'base64'); equal(bytes.toString('base64'), row.base64, 'canonical snapshot bytes');
    equal(bytes.length, row.size, 'snapshot byte count'); equal(sha(bytes), row.sha256, 'snapshot content hash');
    const text = bytes.toString('utf8');
    // Binary files stay byte-for-byte. Text files retain every byte except the
    // concrete isolated root; JSON/YAML key ordering and timestamps stay visible.
    return { path: map(row.path), ...(Buffer.from(text).equals(bytes) ? { text: map(text) } : { base64: row.base64 }) };
  }).sort((a, b) => a.path < b.path ? -1 : a.path > b.path ? 1 : 0);
}
export function emitReport(options, report, projections = []) {
  fs.mkdirSync(options.out);
  for (const row of projections) {
    assert.match(row.id, /^[a-z0-9-]+$/);
    for (const side of ['left', 'right']) fs.writeFileSync(path.join(options.out, `${row.id}.${side}.json`), JSON.stringify(row[side], null, 2) + '\n');
  }
  fs.writeFileSync(path.join(options.out, 'comparison.json'), JSON.stringify(report, null, 2) + '\n');
  console.log(`${report.status}: ${report.observedCases ?? 0}/${report.expectedCases} compared; productPassed=${report.productPassed}`);
  return report.status === 'SAME' || report.status === 'SAME_WITH_ACCEPTED_DIFFERENCE' ? 0 : 1;
}
