import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { createServer } from 'node:http';
import { authenticateNativeStaffDeckAdmission, createNativeApprovalSessionResolver, readNativeInstallationOwner, readNativeSessionAdmission } from '../../src/composition/nativeApprovalSessionResolver.js';
import { createLocalGateway } from '../../src/cli/createLocalGateway.js';
import { createAgentProjectSessionStorage } from '../../src/session/index.js';

const binding = { tenantId: 'tenant', agentId: 'target', pilotDeckUserId: '1' };
const authority = { actorUserId: 'actor', credentialId: 'credential', staffDeckOrigin: 'http://127.0.0.1' };
const key = 'controlled-key-prefix-api-key';
const subject = { tenantId: 'tenant', userId: 'approver', source: 'web' as const, role: 'member' as const, disabled: false };

test('production resolver joins persisted native owner and original transcript admission; refuses old, foreign and ambiguous sessions', async () => {
  const root = await mkdtemp(join(tmpdir(), 'native-approval-authority-'));
  const path = join(root, 'auth.db');
  const db = new DatabaseSync(path);
  db.exec('CREATE TABLE users (id INTEGER, is_active INTEGER); INSERT INTO users VALUES (1, 1)');
  const storage = createAgentProjectSessionStorage({ projectRoot: root, pilotHome: root, sessionId: 'session' });
  try {
    await storage.restore();
    await storage.transcript.recordSessionMetadata('session', 'host-admission', { staffDeckAdmission: { ...binding, ...authority, projectKey: root } });
    const readAdmission = async () => readNativeSessionAdmission((await storage.persistence.load()).entries, 'session');
    let projects = [root];
    const resolver = createNativeApprovalSessionResolver({
      readOwner: () => readNativeInstallationOwner(path), listProjects: async () => projects,
      listSessions: async () => [{ sessionId: 'session' }], sessionAdmission: readAdmission,
    });
    const input = { sessionKey: 'session', binding, subject };
    assert.deepEqual(await resolver(input), { ...binding, sessionKey: 'session', projectKey: root });
    projects = [root, root];
    assert.equal(await resolver(input), undefined);
    projects = [root];
    await storage.transcript.recordSessionMetadata('session', 'metadata-reappend', { isSnapshot: true, staffDeckAdmission: { ...binding, ...authority, projectKey: root } });
    assert.deepEqual(await resolver(input), { ...binding, sessionKey: 'session', projectKey: root });
    assert.equal(await resolver({ ...input, projectKey: '/foreign' }), undefined);
    assert.equal(await resolver({ ...input, sessionKey: 'unknown' }), undefined);
    assert.equal(await resolver({ ...input, binding: { ...binding, agentId: 'foreign' } }), undefined);
    db.exec('INSERT INTO users VALUES (2, 1)');
    assert.equal(await resolver(input), undefined);
    db.exec('DELETE FROM users WHERE id = 2');
    assert.equal(readNativeSessionAdmission([], 'session'), undefined);
    const entries = (await storage.persistence.load()).entries;
    assert.equal(readNativeSessionAdmission(entries, 'different-session'), undefined);
    await storage.transcript.recordSessionMetadata('session', 'host-admission', { staffDeckAdmission: { ...binding, ...authority, agentId: 'changed', projectKey: root } });
    assert.equal(await resolver(input), undefined);
  } finally { db.close(); await storage.dispose(); await rm(root, { recursive: true, force: true }); }
});


test('normal production gateway admission persists the join once and does not backfill old sessions', async () => {
  const root = await mkdtemp(join(tmpdir(), 'production-approval-admission-'));
  const db = new DatabaseSync(join(root, 'auth.db'));
  db.exec('CREATE TABLE users (id INTEGER, is_active INTEGER); INSERT INTO users VALUES (1, 1)');
  let revoked = false;
  const server = createServer((req, res) => {
    res.setHeader('content-type', 'application/json');
    if (req.url === '/api/auth/me' && req.headers.authorization === 'Bearer actor-token') {
      res.end(JSON.stringify({ id: 'actor', tenant_id: 'tenant', source: 'web', disabled: false }));
    } else if (req.url === '/api/auth/me/api-credentials' && req.headers.authorization === 'Bearer actor-token') {
      res.end(JSON.stringify([{ id: 'credential', user_id: 'actor', access: 'user_full_access',
        status: revoked ? 'revoked' : 'active', key_prefix: key.slice(0, 20) + '…' }]));
    } else if (req.url === '/api/v1/agents/target' && req.headers.authorization === `Bearer ${key}` && !revoked) {
      res.end(JSON.stringify({ id: 'target', status: 'active', is_overall: false }));
    } else { res.statusCode = 401; res.end('{}'); }
  });
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${(server.address() as import('node:net').AddressInfo).port}`;
  await writeFile(join(root, 'pilotdeck.yaml'), `schemaVersion: 1
agent: { model: test/test }
model:
  providers:
    test:
      protocol: openai
      url: http://127.0.0.1:1
      apiKey: controlled-test
      models:
        test:
          capabilities: { supportsToolUse: true, maxContextTokens: 8192, maxOutputTokens: 1024 }
modules:
  sop:
    enabled: true
    provider: staffdeck
    endpoint: http://127.0.0.1:1
    definitionsPath: bundle.yaml
    defaultSopId: approval
    discoveryEndpoint: ${origin}/api/v1
    discoveryAgentId: target
    discoveryApiKey: ${key}
`);
  await writeFile(join(root, 'bundle.yaml'), 'sops: [{ id: approval, version: "1", content: { nodes: [{ node_id: start }] } }]');
  const old = createAgentProjectSessionStorage({ projectRoot: root, pilotHome: root, sessionId: 'old' });
  await old.restore();
  await old.transcript.recordSessionMetadata('old', 'original-metadata', { title: 'Existing history' });
  await old.dispose();
  const broken = createAgentProjectSessionStorage({ projectRoot: root, pilotHome: root, sessionId: 'broken' });
  await writeFile(broken.transcriptPath, 'invalid original transcript');
  await broken.dispose();
  const local = createLocalGateway({ projectRoot: root, pilotHome: root, env: { ...process.env,
    STAFFDECK_COPY_TENANT_ID: 'tenant', STAFFDECK_COPY_TARGET_AGENT_ID: 'target', STAFFDECK_COPY_PILOTDECK_USER_ID: '1',
    STAFFDECK_COPY_ACTOR_USER_ID: 'actor', STAFFDECK_SOP_MANAGEMENT_CREDENTIAL_ID: 'credential',
    STAFFDECK_COPY_USER_TOKEN: 'actor-token', STAFFDECK_FORMAL_API_ORIGIN: origin } });
  try {
    const fresh = await local.registry.createSession({ sessionKey: 'fresh', projectKey: root, channelKey: 'test' });
    await fresh.dispose?.();
    const previous = await local.registry.createSession({ sessionKey: 'old', projectKey: root, channelKey: 'test' });
    await previous.dispose?.();
    const corrupt = await local.registry.createSession({ sessionKey: 'broken', projectKey: root, channelKey: 'test' });
    await corrupt.dispose?.();
    const read = async (sessionKey: string) => {
      const storage = local.registry.createPersistentSessionStorage(root, sessionKey);
      try { return readNativeSessionAdmission((await storage.persistence.load()).entries, sessionKey); }
      finally { await storage.dispose(); }
    };
    assert.deepEqual(await read('fresh'), { ...binding, ...authority, staffDeckOrigin: origin, projectKey: root });
    revoked = true;
    const refused = await local.registry.createSession({ sessionKey: 'revoked', projectKey: root, channelKey: 'test' });
    await refused.dispose?.();
    assert.equal(await read('revoked'), undefined);
    assert.equal(await read('old'), undefined);
    assert.equal(await read('broken'), undefined);
  } finally { await local.dispose(); await new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve())); db.close(); await rm(root, { recursive: true, force: true }); }
});


test('formal admission verifies actor, owned current credential and full-key target access without leaking credentials', async () => {
  const env = { STAFFDECK_COPY_TENANT_ID: 'tenant', STAFFDECK_COPY_TARGET_AGENT_ID: 'target',
    STAFFDECK_COPY_PILOTDECK_USER_ID: '1', STAFFDECK_COPY_ACTOR_USER_ID: 'actor',
    STAFFDECK_SOP_MANAGEMENT_CREDENTIAL_ID: 'credential', STAFFDECK_COPY_USER_TOKEN: 'actor-token',
    STAFFDECK_FORMAL_API_ORIGIN: 'https://formal.example' };
  let actorTenant = 'tenant', credentialOwner = 'actor', status = 'active', targetStatus = 'active';
  let keyAccepted = true;
  const request: typeof fetch = async (url) => {
    const path = new URL(String(url)).pathname;
    const value = path === '/api/auth/me' ? { id: 'actor', tenant_id: actorTenant, source: 'web' }
      : path === '/api/auth/me/api-credentials' ? [{ id: 'credential', user_id: credentialOwner,
        status, access: 'user_full_access', key_prefix: key.slice(0, 20) + '…' }]
      : { id: 'target', status: targetStatus, is_overall: false };
    return new Response(JSON.stringify(value), { status: path.includes('/api/v1/') && !keyAccepted ? 401 : 200 });
  };
  const input = { env, projectKey: '/project', discoveryEndpoint: 'https://formal.example/api/v1',
    discoveryAgentId: 'target', discoveryApiKey: key, pilotDeckUserId: '1', request };
  const original = await authenticateNativeStaffDeckAdmission(input);
  assert.deepEqual(original, { ...binding, ...authority, staffDeckOrigin: 'https://formal.example', projectKey: '/project' });
  assert.equal(JSON.stringify(original).includes(key), false);
  actorTenant = 'foreign'; assert.equal(await authenticateNativeStaffDeckAdmission(input), undefined); actorTenant = 'tenant';
  credentialOwner = 'foreign'; assert.equal(await authenticateNativeStaffDeckAdmission(input), undefined); credentialOwner = 'actor';
  status = 'revoked'; assert.equal(await authenticateNativeStaffDeckAdmission(input), undefined); status = 'active';
  keyAccepted = false; assert.equal(await authenticateNativeStaffDeckAdmission(input), undefined); keyAccepted = true;
  targetStatus = 'archived'; assert.equal(await authenticateNativeStaffDeckAdmission(input), undefined); targetStatus = 'active';
  assert.equal(await authenticateNativeStaffDeckAdmission({ ...input, discoveryEndpoint: 'https://foreign.example/api/v1' }), undefined);
});
