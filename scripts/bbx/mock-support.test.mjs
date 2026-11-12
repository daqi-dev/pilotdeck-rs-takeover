import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { matchesProviderRule, writeSeedFile } from './mock-support.mjs';

test('empty-body identity endpoints dispatch by exact method and pathname', () => {
  const rules = [
    { name: 'identity', method: 'GET', path: '/api/auth/me' },
    { name: 'credentials', method: 'GET', path: '/api/auth/me/api-credentials' },
    { name: 'agent', method: 'GET', path: '/api/v1/agents/a' },
    { name: 'fallback' },
  ];
  const match = (method, url) => rules.find((r) => matchesProviderRule(r, { method, url, body: '' })).name;
  assert.equal(match('GET', '/api/auth/me'), 'identity');
  assert.equal(match('GET', '/api/auth/me/api-credentials'), 'credentials');
  assert.equal(match('GET', '/api/v1/agents/a?query=1'), 'agent');
  assert.equal(match('POST', '/api/auth/me'), 'fallback');
  assert.equal(match('GET', '/api/auth/me-extra'), 'fallback');
});
test('path and method conditions do not bypass body matching', () => {
  const rule = { method: 'POST', path: '/m', when: ['needle'], unless: ['no'] };
  assert.equal(matchesProviderRule(rule, { method: 'POST', url: '/m', body: 'needle' }), true);
  assert.equal(matchesProviderRule(rule, { method: 'POST', url: '/m', body: '' }), false);
  assert.equal(matchesProviderRule(rule, { method: 'POST', url: '/m', body: 'needle no' }), false);
});
test('SQLite seed creates real rows, substitutions, and preserves ordinary JSON', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-sqlite-'));
  try {
    const file = path.join(dir, 'auth.db');
    writeSeedFile(file, { $sqlite: ['CREATE TABLE users (id TEXT, active INTEGER)', "INSERT INTO users VALUES ('{ID}',1)"] }, (s) => s.replaceAll('{ID}', 'owner'), (v) => v);
    const db = new DatabaseSync(file, { readOnly: true });
    try { assert.deepEqual({ ...db.prepare('SELECT * FROM users').get() }, { id: 'owner', active: 1 }); } finally { db.close(); }
    const json = path.join(dir, 'regular.json');
    writeSeedFile(json, { $sqlite: ['ordinary'], another: true }, (s) => s, (v) => v);
    assert.deepEqual(JSON.parse(fs.readFileSync(json)), { $sqlite: ['ordinary'], another: true });
    assert.throws(() => writeSeedFile(path.join(dir, 'bad.db'), { $sqlite: [42] }, (s) => s, (v) => v), /SQL strings/);
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
});
