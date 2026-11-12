import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { CASES, compareHttps, applyMissingHomeDifference, verifyRequests } from './compare-https.mjs';
import { BASELINE, SOURCE_FILES, treeSnapshot, sha, json } from './https-evidence.mjs';
const here = path.dirname(fileURLToPath(import.meta.url));
const initial = 'schemaVersion: 1\ntelemetry:\n  enabled: false\n';
const creds = JSON.stringify({ baseUrl: 'https://ilinkai.weixin.qq.com', botToken: 'c1-bot-token', accountId: 'c1-account' }, null, 2);
const successTail = '\n✅ 微信登录成功！\n   账号 ID: c1-account\n   凭据已保存到: <C1_FIXTURE>/home/.pilotdeck/weixin-credentials.json\n   重启 PilotDeck 服务后生效\n\n';
const failureTail = "\n❌ 微信登录失败: ENOENT: no such file or directory, open '<C1_FIXTURE>/home/custom-pilot/weixin-credentials.json'\n   请检查网络连接后重试。\n\n";
const baselineTree = [{ path: 'home', directory: true }, { path: 'home/.pilotdeck', directory: true }, { path: 'config', directory: true }, { path: 'config/pilotdeck.yaml', text: initial }, { path: 'project', directory: true }];
const successTree = [{ path: 'home', directory: true }, { path: 'home/.pilotdeck', directory: true }, { path: 'home/.pilotdeck/weixin-credentials.json', text: creds }, { path: 'config', directory: true }, { path: 'config/pilotdeck.yaml', text: 'enabled: true\n' }, { path: 'project', directory: true }];
function acceptedPair() {
  const success = { stdout: 'prompt-prefix\n' + successTail, yaml: 'enabled: true\n', credentials: creds, tree: successTree };
  const left = { id: 'weixin-missing-pilot-home', stdout: 'prompt-prefix\n' + failureTail, yaml: initial, credentials: null, tree: baselineTree };
  const right = { id: left.id, ...structuredClone(success), stdout: success.stdout.replace('/home/.pilotdeck/', '/home/custom-pilot/'), tree: JSON.parse(JSON.stringify(successTree).replaceAll('home/.pilotdeck', 'home/custom-pilot')) };
  return [structuredClone(left), right, structuredClone(success), structuredClone(success)];
}
test('registered missing-parent difference checks exact failure, success and full trees', () => {
  const values = acceptedPair(); applyMissingHomeDifference(...values);
  assert.deepEqual(values[0], values[1]);
  for (const mutation of [
    values => { values[0].stdout = values[0].stdout.replace('ENOENT', 'EACCES'); },
    values => { values[0].credentials = creds; },
    values => { values[1].yaml += 'unexpected: true\n'; },
    values => { values[1].tree.push({ path: 'home/extra', text: 'secret' }); },
    values => { values[1].stdout = values[1].stdout.replace('账号 ID: c1-account', '账号 ID: wrong'); },
  ]) { const pair = acceptedPair(); mutation(pair); assert.throws(() => applyMissingHomeDifference(...pair)); }
});
function requestFor(id) {
  const get = (host, url, headers = {}) => ({ method: 'GET', path: url, headers: { host, ...headers } });
  if (id.startsWith('feishu') || id.startsWith('lark')) return [{ method: 'POST', path: '/open-apis/auth/v3/tenant_access_token/internal', headers: { host: id.startsWith('lark') ? 'open.larksuite.com' : 'open.feishu.cn', 'content-type': 'application/json; charset=utf-8' }, body: { app_id: 'c1-fixture-app', app_secret: 'c1-fixture-secret' } }];
  if (id.startsWith('wecom')) return ['/ai/qc/generate?source=hermes', '/ai/qc/query_result?scode=c1-scode'].map(url => get('work.weixin.qq.com', url, { 'user-agent': 'PilotDeck/1.0' }));
  return [get('ilinkai.weixin.qq.com', '/ilink/bot/get_bot_qrcode?bot_type=3'), ...(id === 'weixin-http-failure' ? [] : [get('ilinkai.weixin.qq.com', '/ilink/bot/get_qrcode_status?qrcode=c1-qr', { 'ilink-app-clientversion': '1' })])];
}
function synthetic(root, pid, side = 'ts') {
  fs.mkdirSync(root); fs.mkdirSync(path.join(root, 'test-tls'));
  fs.writeFileSync(path.join(root, 'identity.json'), JSON.stringify({ side, node: 'v22.22.0', platform: 'linux-synthetic', executableSha256: '1'.repeat(64), casesSha256: '2'.repeat(64) }));
  for (const id of CASES) {
    const dir = path.join(root, id), pilot = id === 'weixin-missing-pilot-home' ? 'custom-pilot' : '.pilotdeck';
    for (const sub of [id === 'weixin-missing-pilot-home' && side === 'rust' ? 'home/custom-pilot' : 'home/.pilotdeck', 'config', 'project']) fs.mkdirSync(path.join(dir, sub), { recursive: true });
    const hasCreds = id.startsWith('weixin') && id !== 'weixin-http-failure' && !(id === 'weixin-missing-pilot-home' && side === 'ts');
    const yaml = id === 'weixin-config-write-failure' ? null : initial + (id === 'feishu-success' ? 'feishu:\n  enabled: true\n  appId: c1-fixture-app\n  domainName: feishu\n' : id === 'feishu-rejected-save' ? 'appSecret: c1-fixture-secret\n' : id === 'wecom-qr-success' ? 'token: c1-qr-bot\nsecret: c1-qr-secret\n' : id === 'weixin-success' || (id === 'weixin-missing-pilot-home' && side === 'rust') ? 'weixin:\n  enabled: true\n' : '');
    if (yaml === null) fs.mkdirSync(path.join(dir, 'config/pilotdeck.yaml')); else fs.writeFileSync(path.join(dir, 'config/pilotdeck.yaml'), yaml);
    if (hasCreds) fs.writeFileSync(path.join(dir, `home/${pilot}/weixin-credentials.json`), creds);
    const text = { 'feishu-success': '✅ 凭据验证通过！', 'lark-rejected-cancel': '已取消。', 'feishu-rejected-save': '飞书配置已写入 pilotdeck.yaml', 'wecom-qr-success': '企业微信配置已写入 pilotdeck.yaml', 'weixin-success': '微信登录成功！', 'weixin-missing-pilot-home': '微信登录失败:', 'weixin-config-write-failure': '微信登录失败:', 'weixin-http-failure': '微信登录失败: Failed to fetch QR code: 503' }[id];
    const output = id === 'weixin-success' ? 'prompt-prefix\n' + successTail : id === 'weixin-missing-pilot-home' ? 'prompt-prefix\n' + (side === 'ts' ? failureTail : successTail.replace('/home/.pilotdeck/', '/home/custom-pilot/')) : `${text} ${dir}/home\n`;
    const result = { id, exitCode: 0, signal: null, answered: id.startsWith('feishu') ? id === 'feishu-success' ? 3 : 4 : id.startsWith('lark') || id.startsWith('wecom') ? 4 : 1,
      stdout: output.replaceAll('<C1_FIXTURE>', dir), stderr: `(node:${pid}) ExperimentalWarning: SQLite is an experimental feature and might change at any time\n`, requests: requestFor(id), yaml, credentials: hasCreds ? creds : null };
    fs.writeFileSync(path.join(dir, 'result.json'), JSON.stringify(result));
  }
}
function withEvidence(mutate, status) {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'c1-https-compare-synthetic-')));
  try {
    const left = path.join(root, 'a'), right = path.join(root, 'b'), out = path.join(root, 'out'); synthetic(left, 123); synthetic(right, 456); mutate?.(right);
    const code = compareHttps({ mode: 'self', left, right, out }), report = JSON.parse(fs.readFileSync(path.join(out, 'comparison.json')));
    assert.equal(report.status, status, report.error ?? JSON.stringify(report.rows.filter(row => row.status !== 'SAME'))); assert.equal(code, status === 'SAME' ? 0 : 1);
    assert.equal(report.expectedCases, 8);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
}
test('eight-case HTTPS self comparison preserves warning text and compares actual disk', () => { withEvidence(null, 'SAME'); });
test('HTTPS altered request, missing case and forged saved credentials fail closed', () => {
  withEvidence(root => fs.rmSync(path.join(root, CASES[0]), { recursive: true }), 'INVALID');
  withEvidence(root => fs.writeFileSync(path.join(root, 'weixin-success/home/.pilotdeck/weixin-credentials.json'), '{}'), 'INVALID');
  const requests = requestFor('feishu-success'); requests[0].body.app_id = 'wrong'; assert.throws(() => verifyRequests('feishu-success', requests));
});
test('unregistered HTTP header and extra disk file differences remain visible', () => {
  withEvidence(root => { const file = path.join(root, 'feishu-success/result.json'), value = JSON.parse(fs.readFileSync(file)); value.requests[0].headers['user-agent'] = 'changed'; fs.writeFileSync(file, JSON.stringify(value)); }, 'DIFF');
  withEvidence(root => fs.writeFileSync(path.join(root, 'weixin-success/home/extra'), 'unexpected'), 'DIFF');
});
test('complete HTTPS parity accepts only the documented missing-parent cell and records ledger hash', () => {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'c1-https-parity-synthetic-')));
  try {
    const left = path.join(root, 'ts'), right = path.join(root, 'rust'), out = path.join(root, 'comparison');
    synthetic(left, 123); synthetic(right, 456, 'rust');
    assert.equal(compareHttps({ mode: 'parity', left, right, out, 'rust-sha': 'a'.repeat(40), 'rust-binary-sha256': '1'.repeat(64) }), 0);
    const report = JSON.parse(fs.readFileSync(path.join(out, 'comparison.json')));
    assert.equal(report.status, 'SAME_WITH_ACCEPTED_DIFFERENCE'); assert.equal(report.acceptedDifferences.length, 1);
    assert.equal(report.semanticPassed, true); assert.equal(report.provenanceComplete, false); assert.equal(report.productPassed, false);
    assert.equal(report.acceptedDifferences[0].case, 'weixin-missing-pilot-home'); assert.match(report.acceptedDifferences[0].ledgerSha256, /^[a-f0-9]{64}$/);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});

function completeSynthetic(root, side) {
  const specs = CASES.map(id => ({ id, platform: id.split('-')[0], ...(id === 'weixin-missing-pilot-home' ? { missingPilot: true } : {}), ...(id === 'weixin-config-write-failure' ? { configDirectory: true } : {}) }));
  fs.writeFileSync(path.join(root, 'cases.json'), JSON.stringify(specs)); fs.writeFileSync(path.join(root, 'test-tls/cert.pem'), 'synthetic certificate');
  const entry = side === 'ts' ? '/synthetic-baseline/dist/src/cli/pilotdeck.js' : '/synthetic-rust-bin';
  const identity = { schemaVersion: 2, captureMode: 'complete', provenanceComplete: true, side, node: 'v22.22.0', platform: 'linux-synthetic',
    nodeExecutable: '/synthetic-node', nodeSha256: '2'.repeat(64), entry, executableSha256: '1'.repeat(64),
    source: { root: side === 'ts' ? '/synthetic-baseline' : '/synthetic-source', sourceCommit: side === 'ts' ? BASELINE : 'a'.repeat(40), trackedClean: true,
      files: side === 'ts' ? Object.fromEntries(SOURCE_FILES.map(file => [file, '1'.repeat(64)])) : {} },
    driverFiles: Object.fromEntries(['setup-https-capture.mjs', 'https-evidence.mjs'].map(file => [file, sha(fs.readFileSync(path.join(here, file)))])),
    captureRoot: root, casesSha256: sha(fs.readFileSync(path.join(root, 'cases.json'))), certificateSha256: sha('synthetic certificate'), port: 443,
    isolation: { docker: true, addresses: [{ internal: true, address: '127.0.0.1' }], resolvedHosts: Object.fromEntries(['open.feishu.cn', 'open.larksuite.com', 'work.weixin.qq.com', 'ilinkai.weixin.qq.com'].map(host => [host, [{ address: '127.0.0.1', family: 4 }]])) } };
  fs.writeFileSync(path.join(root, 'identity.json'), json(identity)); const identitySha256 = sha(fs.readFileSync(path.join(root, 'identity.json'))), rows = [];
  for (const spec of specs) {
    const dir = path.join(root, spec.id), file = path.join(dir, 'result.json'), result = JSON.parse(fs.readFileSync(file));
    for (const stream of ['stdout', 'stderr']) { fs.writeFileSync(path.join(dir, `${stream}.bin`), result[stream]); result[`${stream}Sha256`] = sha(result[stream]); }
    result.requests = result.requests.map((request, index) => {
      const bytes = Buffer.from(request.body ? JSON.stringify(request.body) : ''), bodyFile = `request-${index}.body.bin`;
      fs.writeFileSync(path.join(dir, bodyFile), bytes);
      return { ...request, bodyFile, bodySize: bytes.length, bodySha256: sha(bytes), httpVersion: '1.1', rawHeaders: Object.entries(request.headers).flat() };
    });
    fs.writeFileSync(file, json(result)); fs.writeFileSync(path.join(dir, 'input.json'), json(spec));
    const command = { command: side === 'ts' ? identity.nodeExecutable : entry, args: [...(side === 'ts' ? [entry] : []), 'gateway', 'setup', spec.platform], cwd: path.join(dir, 'project'), env: {
      PATH: '/synthetic/tools/node/bin:/usr/bin:/bin', HOME: path.join(dir, 'home'), USERPROFILE: path.join(dir, 'home'), CFFIXED_USER_HOME: path.join(dir, 'home'),
      PILOT_HOME: path.join(dir, 'home', spec.missingPilot ? 'custom-pilot' : '.pilotdeck'), PILOTDECK_CONFIG_PATH: path.join(dir, 'config/pilotdeck.yaml'), NODE_EXTRA_CA_CERTS: path.join(root, 'test-tls/cert.pem'),
      TMPDIR: dir, LANG: 'C', LC_ALL: 'C', TZ: 'UTC', TERM: 'dumb', NO_COLOR: '1', DOCKER_CONTAINER: '1', COMMIT_HASH: 'c1-fixture' } };
    fs.writeFileSync(path.join(dir, 'command.json'), json(command));
    const before = [{ path: 'home', directory: true }, ...(!spec.missingPilot ? [{ path: 'home/.pilotdeck', directory: true }] : []), { path: 'config', directory: true },
      spec.configDirectory ? { path: 'config/pilotdeck.yaml', directory: true } : { path: 'config/pilotdeck.yaml', size: Buffer.byteLength(initial), sha256: sha(initial) }, { path: 'project', directory: true }];
    fs.writeFileSync(path.join(dir, 'files-before.json'), json(before)); fs.writeFileSync(path.join(dir, 'files-after.json'), json(treeSnapshot(dir)));
    const files = ['input.json', 'command.json', 'result.json', 'stdout.bin', 'stderr.bin', 'files-before.json', 'files-after.json', ...result.requests.map(request => request.bodyFile)];
    fs.writeFileSync(path.join(dir, 'case-evidence.json'), json({ id: spec.id, identitySha256, files: Object.fromEntries(files.map(name => [name, sha(fs.readFileSync(path.join(dir, name)))])) }));
    rows.push({ id: spec.id, assertionPassed: true, caseEvidenceSha256: sha(fs.readFileSync(path.join(dir, 'case-evidence.json'))) });
  }
  fs.writeFileSync(path.join(root, 'summary.json'), json({ identitySha256, expectedCases: 8, observedCases: 8, collectionPassed: true, rows }));
}
function withComplete(mutate, expected) {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'c1-https-complete-synthetic-')));
  try {
    const left = path.join(root, 'ts'), right = path.join(root, 'rust'), out = path.join(root, 'comparison');
    synthetic(left, 123); synthetic(right, 456, 'rust'); completeSynthetic(left, 'ts'); completeSynthetic(right, 'rust'); mutate?.(left, right);
    const code = compareHttps({ mode: 'parity', left, right, out, 'rust-sha': 'a'.repeat(40), 'rust-binary-sha256': '1'.repeat(64) });
    const report = JSON.parse(fs.readFileSync(path.join(out, 'comparison.json')));
    assert.equal(report.status, expected, report.error ?? JSON.stringify(report.rows.filter(row => row.status !== 'SAME')));
    assert.equal(code, expected === 'SAME_WITH_ACCEPTED_DIFFERENCE' ? 0 : 1); return report;
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
}
test('complete identity and bound raw evidence can produce a positive product verdict', () => {
  const report = withComplete(null, 'SAME_WITH_ACCEPTED_DIFFERENCE');
  assert.equal(report.provenanceComplete, true); assert.equal(report.semanticPassed, true); assert.equal(report.productPassed, true);
});
test('complete comparison rejects wrong SHA/binary and drift in raw output/request/files', () => {
  for (const field of ['source', 'binary']) withComplete((_left, right) => {
    const file = path.join(right, 'identity.json'), value = JSON.parse(fs.readFileSync(file));
    if (field === 'source') value.source.sourceCommit = 'b'.repeat(40); else value.executableSha256 = '0'.repeat(64);
    fs.writeFileSync(file, json(value));
  }, 'INVALID');
  for (const file of ['stdout.bin', 'request-0.body.bin', 'config/pilotdeck.yaml']) {
    const report = withComplete((_left, right) => fs.appendFileSync(path.join(right, 'feishu-success', file), 'deliberate change'), 'INVALID');
    assert.equal(report.productPassed, false); assert.equal(report.provenanceComplete, false);
  }
});
test('complete TS self uses real-driver command environment and rejects rebound setup dist drift', () => {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'c1-https-ts-self-synthetic-')));
  try {
    const left = path.join(root, 'a'), right = path.join(root, 'b'); synthetic(left, 123); synthetic(right, 456); completeSynthetic(left, 'ts'); completeSynthetic(right, 'ts');
    const command = JSON.parse(fs.readFileSync(path.join(right, 'weixin-success/command.json')));
    assert.deepEqual(Object.keys(command.env).sort(), ['PATH', 'HOME', 'USERPROFILE', 'CFFIXED_USER_HOME', 'PILOT_HOME', 'PILOTDECK_CONFIG_PATH', 'NODE_EXTRA_CA_CERTS', 'TMPDIR', 'LANG', 'LC_ALL', 'TZ', 'TERM', 'NO_COLOR', 'DOCKER_CONTAINER', 'COMMIT_HASH'].sort());
    assert.equal(command.env.TMPDIR, path.join(right, 'weixin-success'));
    const out = path.join(root, 'positive'); assert.equal(compareHttps({ mode: 'self', left, right, out }), 0);
    const positive = JSON.parse(fs.readFileSync(path.join(out, 'comparison.json'))); assert.equal(positive.productPassed, true);

    const identityFile = path.join(right, 'identity.json'), identity = JSON.parse(fs.readFileSync(identityFile));
    const originalEntry = identity.executableSha256, originalSource = identity.source.sourceCommit;
    identity.source.files['dist/src/cli/commands/gatewaySetup.js'] = 'b'.repeat(64);
    fs.writeFileSync(identityFile, json(identity)); const identitySha256 = sha(fs.readFileSync(identityFile));
    const summaryFile = path.join(right, 'summary.json'), summary = JSON.parse(fs.readFileSync(summaryFile)); summary.identitySha256 = identitySha256;
    for (const row of summary.rows) {
      const bindingFile = path.join(right, row.id, 'case-evidence.json'), binding = JSON.parse(fs.readFileSync(bindingFile));
      binding.identitySha256 = identitySha256; fs.writeFileSync(bindingFile, json(binding)); row.caseEvidenceSha256 = sha(fs.readFileSync(bindingFile));
    }
    fs.writeFileSync(summaryFile, json(summary));
    assert.equal(identity.executableSha256, originalEntry); assert.equal(identity.source.sourceCommit, originalSource);
    const failureOut = path.join(root, 'negative'); assert.equal(compareHttps({ mode: 'self', left, right, out: failureOut }), 1);
    const negative = JSON.parse(fs.readFileSync(path.join(failureOut, 'comparison.json')));
    assert.equal(negative.status, 'INVALID'); assert.equal(negative.productPassed, false); assert.match(negative.error, /same complete TS source\/dist\/dependency hashes/);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
