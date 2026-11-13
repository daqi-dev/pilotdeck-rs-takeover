import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { componentSupplyEnvironment, componentSupplyIdentity } from './component-supply.mjs';

const sha = value => createHash('sha256').update(value).digest('hex');
test('only three explicit native overrides are passed, with existing BBX aliases', () => {
  assert.deepEqual(componentSupplyEnvironment({ PILOTDECK_RIPGREP_PATH: '/rg', BBX_PDF_TOOL: '/pdf', BBX_IMAGE_TOOL: '/image', PRIVATE_TOKEN: 'never copied', PATH: '/not-copied' }), { PILOTDECK_RIPGREP_PATH: '/rg', PILOTDECK_PDF_TOOL: '/pdf', PILOTDECK_IMAGE_TOOL: '/image' });
  assert.deepEqual(componentSupplyEnvironment({ PATH: '/usr/bin' }), {});
});
test('explicit product values precede aliases and empty values stay empty', () => {
  assert.deepEqual(componentSupplyEnvironment({ PILOTDECK_PDF_TOOL: '/explicit', BBX_PDF_TOOL: '/alias', PILOTDECK_IMAGE_TOOL: '', BBX_IMAGE_TOOL: '/must-not-restore' }), { PILOTDECK_PDF_TOOL: '/explicit', PILOTDECK_IMAGE_TOOL: '' });
  assert.throws(() => componentSupplyEnvironment({ PILOTDECK_RIPGREP_PATH: 3 }), TypeError);
});
test('configured path receipt hashes actual symlink target without executing it', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-supply-'));
  try {
    const file = path.join(root, 'real'), link = path.join(root, 'declared');
    fs.writeFileSync(file, 'not an executable; identity must only read bytes'); fs.symlinkSync(file, link);
    const first = componentSupplyIdentity({ PILOTDECK_RIPGREP_PATH: link });
    assert.deepEqual(first, { PILOTDECK_RIPGREP_PATH: { value: link, realpath: fs.realpathSync(file), sha256: sha(fs.readFileSync(file)), error: null } });
    fs.writeFileSync(file, 'changed engine bytes');
    assert.notEqual(componentSupplyIdentity({ PILOTDECK_RIPGREP_PATH: link }).PILOTDECK_RIPGREP_PATH.sha256, first.PILOTDECK_RIPGREP_PATH.sha256);
    fs.unlinkSync(file);
    const missing = componentSupplyIdentity({ PILOTDECK_RIPGREP_PATH: link }).PILOTDECK_RIPGREP_PATH;
    assert.equal(missing.sha256, null); assert.equal(missing.error, 'ENOENT');
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
test('missing, directory, relative and explicitly empty supplies cannot masquerade as hashed files', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-supply-'));
  try {
    for (const [value, error] of [[path.join(root, 'missing'), 'ENOENT'], [root, 'not_a_file'], ['relative/rg', 'relative_path_not_resolved'], ['', 'explicitly_unset']]) {
      const entry = componentSupplyIdentity({ PILOTDECK_IMAGE_TOOL: value }).PILOTDECK_IMAGE_TOOL;
      assert.equal(entry.error, error); assert.equal(entry.sha256, null);
    }
    assert.deepEqual(componentSupplyIdentity({ BBX_IMAGE_TOOL: '/not-product-env', PRIVATE_TOKEN: 'not recorded' }), {});
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
test('two valid deployment paths may differ without requiring cross-side path equality', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-supply-'));
  try {
    const a = path.join(root, 'ts-rg'), b = path.join(root, 'rust-rg'); fs.writeFileSync(a, 'same sourcebuilt bytes'); fs.writeFileSync(b, 'same sourcebuilt bytes');
    const left = componentSupplyIdentity({ PILOTDECK_RIPGREP_PATH: a }).PILOTDECK_RIPGREP_PATH;
    const right = componentSupplyIdentity({ PILOTDECK_RIPGREP_PATH: b }).PILOTDECK_RIPGREP_PATH;
    assert.notEqual(left.realpath, right.realpath); assert.equal(left.sha256, right.sha256);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
