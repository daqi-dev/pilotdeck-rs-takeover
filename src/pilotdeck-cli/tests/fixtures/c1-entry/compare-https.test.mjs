import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { CASES, compareHttps, applyMissingHomeDifference, verifyRequests } from './compare-https.mjs';
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
