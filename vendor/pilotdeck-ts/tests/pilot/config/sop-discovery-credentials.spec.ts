import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { loadPilotConfig } from '../../../src/pilot/config/loadPilotConfig.js';
import { redactConfig } from '../../../src/pilot/config/redact.js';
import { PilotConfigError } from '../../../src/pilot/config/types.js';

test('stock loader resolves process-only SOP discovery credentials and fails closed', () => {
  const root = mkdtempSync(join(tmpdir(), 'sop-reference-'));
  const configPath = join(root, 'pilotdeck.yaml');
  const profile = `schemaVersion: 1
agent: { model: custom/model }
model:
  providers:
    custom: { protocol: openai, url: 'https://example.com/v1', apiKey: '\${MODEL_KEY}', models: { model: {} } }
modules:
  sop:
    enabled: true
    provider: staffdeck
    endpoint: http://127.0.0.1:19991
    definitionsPath: definitions.yaml
    defaultSopId: publication
    discoveryEndpoint: http://127.0.0.1:19992/api/v1
    discoveryAgentId: target
    discoveryApiKey: '\${DISCOVERY_KEY}'
`;
  try {
    writeFileSync(configPath, profile);
    const env = { PILOT_HOME: root, MODEL_KEY: 'fixture-model', DISCOVERY_KEY: '  fixture-owned-key  ' };
    const loaded = loadPilotConfig({ configPath, env });
    assert.equal(loaded.config.modules?.sop?.discoveryApiKey, 'fixture-owned-key');
    assert.equal(readFileSync(configPath, 'utf8'), profile);
    assert.equal(JSON.stringify(redactConfig(loaded.config)).includes('fixture-owned-key'), false);
    assert.throws(() => loadPilotConfig({ configPath, env: { ...env, DISCOVERY_KEY: '' } }),
      (error) => error instanceof PilotConfigError && error.diagnostics.some((item) => item.code === 'SOP_DISCOVERY_API_KEY_INVALID'));
    assert.equal(loadPilotConfig({ configPath, env: { ...env, DISCOVERY_KEY: 'rotated-owned-key' } }).config.modules?.sop?.discoveryApiKey, 'rotated-owned-key');
  } finally { rmSync(root, { recursive: true, force: true }); }
});
