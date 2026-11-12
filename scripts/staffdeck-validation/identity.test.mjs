import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import assert from 'node:assert/strict';
import { artifact, assertArtifactsUnchanged } from './identity.mjs';

test('validation identity rejects changed or missing script/binary artifacts', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'pilotdeck-identity-'));
  try {
    const binary = path.join(dir, 'binary'), script = path.join(dir, 'script.mjs');
    fs.writeFileSync(binary, 'binary-original');fs.writeFileSync(script, 'script-original');
    const expected = [artifact(binary), artifact(script)];
    assertArtifactsUnchanged(expected);
    fs.writeFileSync(binary, 'binary-modified');
    assert.throws(() => assertArtifactsUnchanged(expected), /artifact changed/);
    fs.writeFileSync(binary, 'binary-original');fs.writeFileSync(script, 'script-modified');
    assert.throws(() => assertArtifactsUnchanged(expected), /artifact changed/);
    fs.unlinkSync(script);
    assert.throws(() => assertArtifactsUnchanged(expected), /ENOENT/);
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
});
