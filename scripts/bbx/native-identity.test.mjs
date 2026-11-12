import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { nativeDependencyIdentity, nativeFileIdentity } from './native-identity.mjs';

test('records missing files and changes when native target or symlink changes', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-native-file-'));
  try {
    const first = path.join(root, 'first'), second = path.join(root, 'second'), link = path.join(root, 'libvips.so');
    fs.writeFileSync(first, 'native-one'); fs.writeFileSync(second, 'native-two');
    assert.equal(nativeFileIdentity(link).sha256, null);
    fs.symlinkSync(first, link); const a = nativeFileIdentity(link);
    fs.unlinkSync(link); fs.symlinkSync(second, link); const b = nativeFileIdentity(link);
    assert.notEqual(a.sha256, b.sha256); assert.notEqual(a.realpath, b.realpath);
    fs.writeFileSync(second, 'replaced-native'); assert.notEqual(nativeFileIdentity(link).sha256, b.sha256);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});

test('same lockfile cannot hide replaced WASM, tokenizer code/ranks, or image libraries', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-native-deps-'));
  try {
    const baseline = path.join(root, 'baseline'), modules = path.join(baseline, 'node_modules');
    const mupdf = path.join(modules, 'mupdf'), tokenizer = path.join(modules, 'js-tiktoken');
    fs.mkdirSync(mupdf, { recursive: true }); fs.mkdirSync(tokenizer, { recursive: true });
    fs.writeFileSync(path.join(baseline, 'package.json'), '{}'); fs.writeFileSync(path.join(baseline, 'pnpm-lock.yaml'), 'fixed lock');
    fs.writeFileSync(path.join(mupdf, 'package.json'), '{"main":"mupdf.js"}');
    fs.writeFileSync(path.join(tokenizer, 'package.json'), '{"exports":{"./lite":"./lite.cjs","./ranks/o200k_base":"./o200k.cjs"}}');
    const files = [path.join(mupdf, 'mupdf.js'), path.join(mupdf, 'mupdf-wasm.js'), path.join(mupdf, 'mupdf-wasm.wasm'),
      path.join(tokenizer, 'lite.cjs'), path.join(tokenizer, 'o200k.cjs'), path.join(root, 'prefix/lib/libvips-cpp.so.8.17.3')];
    for (const file of files) { fs.mkdirSync(path.dirname(file), { recursive: true }); fs.writeFileSync(file, 'initial'); }
    const executable = path.join(root, 'prefix/bin/pilotdeck-image');
    const original = nativeDependencyIdentity(baseline, executable);
    for (const file of files) {
      fs.writeFileSync(file, 'changed'); assert.notDeepEqual(nativeDependencyIdentity(baseline, executable), original, file);
      fs.writeFileSync(file, 'initial');
    }
    assert.equal(fs.readFileSync(path.join(baseline, 'pnpm-lock.yaml'), 'utf8'), 'fixed lock');
    assert.deepEqual(nativeDependencyIdentity(baseline, executable), original);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
