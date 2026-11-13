#!/usr/bin/env node
// Read-only exact captured-evidence overlay. Never executes a product or capture.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { isDeepStrictEqual } from 'node:util';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { REGISTRATION } from './https-presentation-registration.mjs';
export const WARNING = '(node:<PID>) ExperimentalWarning: SQLite is an experimental feature and might change at any time\n(Use `node --trace-warnings ...` to show where the warning was created)\n';
const headerSlots = {
  'feishu-success': [[0, 'Content-Type']], 'lark-rejected-cancel': [[0, 'Content-Type']],
  'feishu-rejected-save': [[0, 'Content-Type']],
  'wecom-qr-success': [[0, 'User-Agent'], [1, 'User-Agent']],
  'weixin-success': [[1, 'iLink-App-ClientVersion']],
  'weixin-missing-pilot-home': [[1, 'iLink-App-ClientVersion']],
  'weixin-config-write-failure': [[1, 'iLink-App-ClientVersion']], 'weixin-http-failure': [],
};
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
export function presentationView(projection, side) {
  assert.ok(['ts', 'rust'].includes(side));
  assert.ok(Object.hasOwn(headerSlots, projection.id), 'fixed eight-case roster');
  const out = structuredClone(projection);
  if (side === 'ts') {
    assert.ok(out.stderr.startsWith(WARNING), 'exact recorded SQLite prefix required');
    out.stderr = out.stderr.slice(WARNING.length);
  }
  for (const [index, name] of headerSlots[out.id]) {
    const raw = out.requests[index].rawHeaders;
    assert.equal(raw[4], side === 'ts' ? name : name.toLowerCase(), 'only exact observed header-name slot');
    raw[4] = name.toLowerCase();
  }
  return out;
}
export function compareProjected(left, right, rightSide = 'rust') {
  const a = presentationView(left, 'ts'), b = presentationView(right, rightSide);
  return isDeepStrictEqual(a, b) ? [] : [{ left: a, right: b }];
}
function exactFile(root, relative, digest) {
  assert.ok(relative && !path.isAbsolute(relative) && !relative.split('/').includes('..'));
  const file = path.join(root, relative);
  let cursor = root;
  for (const part of relative.split('/')) {
    cursor = path.join(cursor, part);
    assert.equal(fs.lstatSync(cursor).isSymbolicLink(), false, 'pinned input has no symlink');
  }
  assert.ok(fs.lstatSync(file).isFile(), 'pinned regular file');
  const bytes = fs.readFileSync(file);
  assert.equal(sha(bytes), digest, `pinned bytes ${relative}`);
  return bytes;
}
export function auditReadset(observed) {
  assert.deepEqual(observed, REGISTRATION.raw, 'exact complete 268-key readset, no missing or extra reads');
}
function verifyPinned(source, evidence) {
  for (const [file, digest] of Object.entries(REGISTRATION.judge)) exactFile(source, file, digest);
  for (const [file, digest] of Object.entries({ ...REGISTRATION.raw, ...REGISTRATION.build })) exactFile(evidence, file, digest);
  const record = JSON.parse(exactFile(evidence, 'c88650f6-sealed-release/release-record.json', REGISTRATION.build['c88650f6-sealed-release/release-record.json']));
  assert.equal(record.source.commit, REGISTRATION.productCandidate);
  assert.equal(record.source.dirty, false); assert.deepEqual(record.source.status, []);
  assert.equal(record.profile, 'release'); assert.equal(record.target, 'aarch64-unknown-linux-gnu');
  assert.equal(record.binaries.pilotdeck.sha256, REGISTRATION.binarySHA256);
  assert.deepEqual(record.evidence.map(row => row.sha256).sort(), Object.entries(REGISTRATION.build)
    .filter(([name]) => !name.endsWith('/release-record.json')).map(([, digest]) => digest).sort());
  assert.equal(fs.readFileSync(path.join(evidence, 'c88650f6-release/release.exit'), 'utf8').trim(), '0');
}
export async function runExact(argv) {
  assert.equal(argv.length, 4, 'usage: --evidence-root EXACT_CAPTURE_PARENT --out NEW_ABSOLUTE_DIRECTORY');
  assert.equal(argv[0], '--evidence-root'); assert.equal(argv[2], '--out');
  assert.ok(path.isAbsolute(argv[1]) && path.isAbsolute(argv[3]), 'absolute locators');
  const source = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../../../..');
  const evidence = fs.realpathSync(argv[1]);
  const out = path.join(fs.realpathSync(path.dirname(argv[3])), path.basename(argv[3]));
  assert.ok(out !== evidence && !out.startsWith(evidence + path.sep), 'output cannot mutate original inputs');
  assert.ok(!fs.existsSync(out), 'new output only');
  verifyPinned(source, evidence); // Before loading any pre-existing judge module.
  const { compareHttps, CASES } = await import(pathToFileURL(path.join(source, 'src/pilotdeck-cli/tests/fixtures/c1-entry/compare-https.mjs')));
  assert.deepEqual(CASES, Object.keys(headerSlots));
  fs.mkdirSync(out);
  const readset = {}, rows = [], strict = [];
  for (const [mode, right] of [['self', 'ts-b'], ['parity', 'rust']]) {
    const target = path.join(out, `strict-${mode}`);
    compareHttps({ mode, left: path.join(evidence, REGISTRATION.capturePrefix, 'ts-a'),
      right: path.join(evidence, REGISTRATION.capturePrefix, right), out: target,
      'rust-sha': REGISTRATION.productCandidate, 'rust-binary-sha256': REGISTRATION.binarySHA256 });
    const report = JSON.parse(fs.readFileSync(path.join(target, 'comparison.json')));
    assert.equal(report.provenanceComplete, true); assert.equal(report.observedCases, 8);
    assert.equal(report.rows.some(row => row.status === 'INVALID'), false);
    assert.equal(report.status, mode === 'self' ? 'SAME' : 'DIFF', 'retain exact original strict verdict');
    strict.push({ mode, status: report.status, path: `strict-${mode}/comparison.json`, sha256: sha(fs.readFileSync(path.join(target, 'comparison.json'))) });
    for (const [side, files] of Object.entries(report.inputHashes)) {
      const root = side === 'left' ? 'ts-a' : right;
      for (const [relative, digest] of Object.entries(files)) {
        const key = `${REGISTRATION.capturePrefix}/${root}/${relative}`;
        assert.ok(Object.hasOwn(REGISTRATION.raw, key), 'no unregistered read key');
        if (Object.hasOwn(readset, key)) assert.equal(readset[key], digest);
        readset[key] = digest;
      }
    }
    for (const id of CASES) {
      const read = side => JSON.parse(fs.readFileSync(path.join(target, `${id}.${side}.json`)));
      const delta = compareProjected(read('left'), read('right'), mode === 'self' ? 'ts' : 'rust');
      assert.deepEqual(delta, [], `no other difference in ${mode}:${id}`);
      rows.push({ mode, id, status: 'ACCEPTED_EXACT_PRESENTATION', registration: REGISTRATION.id });
    }
  }
  auditReadset(readset); verifyPinned(source, evidence);
  const result = { registration: REGISTRATION.id, productCandidate: REGISTRATION.productCandidate,
    binarySHA256: REGISTRATION.binarySHA256, actualRawCases: 24, comparisonRows: 16, rows, strict,
    fullReadset: readset, buildReceipt: REGISTRATION.build, judgeClosure: REGISTRATION.judge,
    entrySHA256: sha(fs.readFileSync(fileURLToPath(import.meta.url))),
    registrationSHA256: sha(fs.readFileSync(new URL('./https-presentation-registration.mjs', import.meta.url))),
    limits: ['only fixed three captures; no future-capture authorization', 'raw strict parity DIFF8 preserved',
      '268 read-key/hash locks before and after are not a hostile concurrent-writer CAS guarantee',
      'HTTP application headers/body only, not TLS records or HTTP chunk framing',
      'no product execution; binary/source association uses independently sealed Linux release receipt'] };
  fs.writeFileSync(path.join(out, 'accepted.json'), JSON.stringify(result, null, 2)+'\n');
  console.log('ACCEPTED_EXACT_PRESENTATION: 16 comparison rows from 24 fixed raw cases');
}
if (process.argv[1] && fs.realpathSync(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { await runExact(process.argv.slice(2)); }
  catch (error) { console.error(error.message); process.exitCode = 2; }
}
