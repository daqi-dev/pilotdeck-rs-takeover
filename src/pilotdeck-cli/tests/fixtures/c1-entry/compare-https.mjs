#!/usr/bin/env node
// Compare already-captured HTTPS setup evidence. Does not execute the driver.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { argumentsFor, reader, sha, equal, rootMapper, mapStrings, differences, emitReport } from './compare-common.mjs';
export const CASES = ['feishu-success', 'lark-rejected-cancel', 'feishu-rejected-save', 'wecom-qr-success', 'weixin-success', 'weixin-missing-pilot-home', 'weixin-config-write-failure', 'weixin-http-failure'];
const INITIAL = 'schemaVersion: 1\ntelemetry:\n  enabled: false\n';
const CREDS = { baseUrl: 'https://ilinkai.weixin.qq.com', botToken: 'c1-bot-token', accountId: 'c1-account' };
const ACCEPTED = 'D1-SETUP-WEIXIN-HOME-001';
const here = path.dirname(fileURLToPath(import.meta.url));

function diskTree(evidence, id) {
  const result = [];
  const walk = relative => {
    const full = path.join(evidence.root, id, relative), stat = fs.lstatSync(full);
    assert.equal(stat.isSymbolicLink(), false, 'setup evidence must not redirect tree reads');
    if (stat.isDirectory()) {
      result.push({ path: relative, directory: true });
      for (const name of fs.readdirSync(full).sort()) walk(`${relative}/${name}`);
    } else { assert.ok(stat.isFile()); result.push({ path: relative, base64: evidence.bytes(`${id}/${relative}`).toString('base64') }); }
  };
  for (const root of ['home', 'config', 'project']) walk(root);
  return result;
}

export function verifyRequests(id, requests) {
  const token = '/open-apis/auth/v3/tenant_access_token/internal';
  let expected;
  if (id.startsWith('feishu') || id.startsWith('lark')) expected = [{ method: 'POST', host: id.startsWith('lark') ? 'open.larksuite.com' : 'open.feishu.cn', path: token }];
  else if (id.startsWith('wecom')) expected = ['/ai/qc/generate?source=hermes', '/ai/qc/query_result?scode=c1-scode'].map(url => ({ method: 'GET', host: 'work.weixin.qq.com', path: url }));
  else expected = ['/ilink/bot/get_bot_qrcode?bot_type=3', ...(id === 'weixin-http-failure' ? [] : ['/ilink/bot/get_qrcode_status?qrcode=c1-qr'])].map(url => ({ method: 'GET', host: 'ilinkai.weixin.qq.com', path: url }));
  equal(requests.length, expected.length, 'exact vendor request count');
  for (const [i, request] of requests.entries()) {
    equal(request.method, expected[i].method); equal(request.path, expected[i].path); equal(request.headers.host, expected[i].host);
    equal(Object.keys(request).sort(), ['method', 'path', 'headers', ...(request.method === 'POST' ? ['body'] : [])].sort());
    if (request.method === 'POST') {
      equal(request.body, { app_id: 'c1-fixture-app', app_secret: 'c1-fixture-secret' });
      equal(request.headers['content-type'], 'application/json; charset=utf-8');
    } else if (id.startsWith('wecom')) equal(request.headers['user-agent'], 'PilotDeck/1.0');
    else if (i === 1) equal(request.headers['ilink-app-clientversion'], '1');
  }
}

function identityOf(evidence, side, options) {
  const identity = evidence.json('identity.json');
  equal(identity.side, side); equal(identity.node, 'v22.22.0'); assert.match(identity.platform, /^linux-/);
  assert.match(identity.executableSha256, /^[a-f0-9]{64}$/); assert.match(identity.casesSha256, /^[a-f0-9]{64}$/);
  if (side === 'rust') equal(identity.executableSha256, options['rust-binary-sha256']);
  equal(fs.readdirSync(evidence.root).filter(name => fs.statSync(path.join(evidence.root, name)).isDirectory()).sort(), [...CASES, 'test-tls'].sort(), 'exact eight scenario directories');
  return identity;
}

export function projectHttpsCase(evidence, id, side) {
  const result = evidence.json(`${id}/result.json`);
  equal(Object.keys(result).sort(), ['id', 'exitCode', 'signal', 'stdout', 'stderr', 'answered', 'requests', 'yaml', 'credentials'].sort(), 'missing/unknown/failure result field');
  equal(result.id, id); equal(result.exitCode, 0); equal(result.signal, null);
  equal(result.answered, id.startsWith('feishu') ? (id === 'feishu-success' ? 3 : 4) : id.startsWith('lark') || id.startsWith('wecom') ? 4 : 1);
  verifyRequests(id, result.requests);
  const root = path.join(evidence.root, id), map = rootMapper(root), tree = diskTree(evidence, id);
  const yamlRow = tree.find(row => row.path === 'config/pilotdeck.yaml');
  assert.ok(yamlRow, 'config destination retained');
  equal(yamlRow.directory ? null : Buffer.from(yamlRow.base64, 'base64').toString('utf8'), result.yaml, 'actual YAML matches saved result');
  equal(Boolean(yamlRow.directory), id === 'weixin-config-write-failure', 'only declared directory failure');
  const pilot = id === 'weixin-missing-pilot-home' ? 'custom-pilot' : '.pilotdeck';
  const credentialsRow = tree.find(row => row.path === `home/${pilot}/weixin-credentials.json`);
  equal(credentialsRow ? Buffer.from(credentialsRow.base64, 'base64').toString('utf8') : null, result.credentials, 'actual credentials match saved result');
  const wantsCredentials = id.startsWith('weixin') && id !== 'weixin-http-failure' && !(id === 'weixin-missing-pilot-home' && side === 'ts');
  equal(result.credentials !== null, wantsCredentials);
  if (wantsCredentials) equal(JSON.parse(result.credentials), CREDS, 'complete credentials fields');
  if (['lark-rejected-cancel', 'weixin-http-failure'].includes(id)) equal(result.yaml, INITIAL);
  const yamlParts = id === 'feishu-success' ? ['feishu:', 'enabled: true', 'appId: c1-fixture-app', 'domainName: feishu']
    : id === 'feishu-rejected-save' ? ['appSecret: c1-fixture-secret']
      : id === 'wecom-qr-success' ? ['token: c1-qr-bot', 'secret: c1-qr-secret']
        : id === 'weixin-success' || (id === 'weixin-missing-pilot-home' && side === 'rust') ? ['weixin:', 'enabled: true'] : [];
  for (const part of yamlParts) assert.ok(result.yaml?.includes(part), `required saved YAML ${part}`);
  const text = {
    'feishu-success': '✅ 凭据验证通过！', 'lark-rejected-cancel': '已取消。',
    'feishu-rejected-save': '飞书配置已写入 pilotdeck.yaml', 'wecom-qr-success': '企业微信配置已写入 pilotdeck.yaml',
    'weixin-success': '微信登录成功！', 'weixin-missing-pilot-home': side === 'ts' ? '微信登录失败:' : '微信登录成功！',
    'weixin-config-write-failure': '微信登录失败:', 'weixin-http-failure': '微信登录失败: Failed to fetch QR code: 503',
  }[id];
  assert.ok(result.stdout.includes(text), 'required terminal outcome');
  const projection = mapStrings(result, map);
  // Node warning PID is runtime identity. The complete warning text and its
  // presence remain observable, including TS/Rust warning asymmetry.
  projection.stderr = projection.stderr.replace(/(?<=^|\n)\(node:[1-9][0-9]*\)(?= ExperimentalWarning: SQLite is an experimental feature and might change at any time\n)/g, '(node:<PID>)');
  projection.tree = tree.map(row => {
    if (row.directory) return row;
    const bytes = Buffer.from(row.base64, 'base64'), string = bytes.toString('utf8');
    return Buffer.from(string).equals(bytes) ? { path: row.path, text: map(string) } : row;
  });
  return projection;
}

// This exception is conditional on the complete, concrete behavior described
// by the registered difference. It cannot accept an arbitrary failed setup.
export function applyMissingHomeDifference(left, right, leftSuccess, rightSuccess) {
  equal(left.id, 'weixin-missing-pilot-home'); equal(right.id, left.id);
  const successTail = '\n✅ 微信登录成功！\n   账号 ID: c1-account\n   凭据已保存到: <C1_FIXTURE>/home/.pilotdeck/weixin-credentials.json\n   重启 PilotDeck 服务后生效\n\n';
  assert.ok(leftSuccess.stdout.endsWith(successTail), 'baseline normal-success suffix is exact');
  const prefix = leftSuccess.stdout.slice(0, -successTail.length);
  const failureTail = "\n❌ 微信登录失败: ENOENT: no such file or directory, open '<C1_FIXTURE>/home/custom-pilot/weixin-credentials.json'\n   请检查网络连接后重试。\n\n";
  equal(left.stdout, prefix + failureTail, 'only recorded missing-parent ENOENT is accepted');
  equal(left.yaml, INITIAL); equal(left.credentials, null);
  const baselineTree = [
    { path: 'home', directory: true }, { path: 'home/.pilotdeck', directory: true },
    { path: 'config', directory: true }, { path: 'config/pilotdeck.yaml', text: INITIAL }, { path: 'project', directory: true },
  ];
  equal(left.tree, baselineTree, 'baseline keeps only its known default-home directory and initial YAML');
  const replaceCustom = string => string.replaceAll('/home/custom-pilot/', '/home/.pilotdeck/').replaceAll('home/custom-pilot', 'home/.pilotdeck');
  equal(replaceCustom(right.stdout), rightSuccess.stdout, 'Rust missing-home stdout equals ordinary successful login');
  equal(right.yaml, rightSuccess.yaml); equal(right.credentials, rightSuccess.credentials);
  equal(mapStrings(right.tree, replaceCustom), rightSuccess.tree, 'Rust saves exactly normal successful files in custom parent');
  const successIndex = rightSuccess.stdout.lastIndexOf('\n✅ 微信登录成功！'); assert.ok(successIndex >= 0);
  left.stdout = prefix; right.stdout = rightSuccess.stdout.slice(0, successIndex);
  for (const value of [left, right]) {
    value.yaml = `<${ACCEPTED}:checked>`; value.credentials = `<${ACCEPTED}:checked>`;
    value.tree = `<${ACCEPTED}:checked>`;
  }
}

export function compareHttps(options) {
  const report = { kind: 'setup-https', mode: options.mode, expectedCases: 8, observedCases: 0, status: 'INVALID', productPassed: false, semanticPassed: false, provenanceComplete: false, acceptedDifferences: [], rows: [],
    normalization: ['only actual capture case root paths', 'only SQLite ExperimentalWarning PID, retaining warning text', 'D1-SETUP-WEIXIN-HOME-001 only after exact ENOENT/initial-file and normal-success checks'],
    limitations: ['HTTPS capture identity does not record sourceCommit, Node binary hash, or capture source hash; E0 invocation/build receipts must supply that provenance.', 'HTTPS driver saved decoded stdout/stderr and parsed JSON request bodies, not independent raw byte files; this comparator cannot invent the missing byte provenance.', 'All HTTP headers remain compared; no transport-default header exemption is assumed.'] };
  const projections = []; let left, right;
  try {
    left = reader(options.left); right = reader(options.right);
    const a = identityOf(left, 'ts', options), b = identityOf(right, options.mode === 'self' ? 'ts' : 'rust', options);
    equal(a.platform, b.platform);
    if (options.mode === 'self') { equal(a.executableSha256, b.executableSha256); equal(a.casesSha256, b.casesSha256); }
    const first = {}, second = {}, errors = {};
    for (const id of CASES) {
      try { first[id] = projectHttpsCase(left, id, 'ts'); second[id] = projectHttpsCase(right, id, options.mode === 'self' ? 'ts' : 'rust'); }
      catch (error) { errors[id] = error.message; }
    }
    for (const id of CASES) {
      if (errors[id]) { report.rows.push({ id, status: 'INVALID', error: errors[id] }); continue; }
      const one = structuredClone(first[id]), two = structuredClone(second[id]);
      try {
        if (options.mode === 'parity' && id === 'weixin-missing-pilot-home') {
          assert.ok(!errors['weixin-success'], 'ordinary-success reference must be valid');
          applyMissingHomeDifference(one, two, first['weixin-success'], second['weixin-success']);
          const ledger = fs.readFileSync(path.resolve(here, '../../../../..', 'specs/gateway-parity/diffs/D1-SETUP-WEIXIN-HOME-001.md'));
          report.acceptedDifferences.push({ id: ACCEPTED, case: id, ledgerSha256: sha(ledger) });
        }
        const delta = differences(one, two); projections.push({ id, left: one, right: two });
        report.rows.push({ id, status: delta.length ? 'DIFF' : 'SAME', differences: delta });
      } catch (error) { report.rows.push({ id, status: 'INVALID', error: error.message }); }
    }
    report.observedCases = report.rows.length;
    report.status = report.rows.some(row => row.status === 'INVALID') ? 'INVALID' : report.rows.some(row => row.status === 'DIFF') ? 'DIFF' : report.acceptedDifferences.length ? 'SAME_WITH_ACCEPTED_DIFFERENCE' : 'SAME';
    report.semanticPassed = ['SAME', 'SAME_WITH_ACCEPTED_DIFFERENCE'].includes(report.status);
    // The capture's missing source/driver/runtime identity cannot be recovered
    // by comparing its decoded output. A SAME semantic projection is therefore
    // deliberately not a fixed-baseline product acceptance verdict.
    report.productPassed = false;
    report.identities = { left: a, right: b, rustSourceShaFromExternalReceipt: options['rust-sha'] };
  } catch (error) { report.error = error.message; }
  report.inputHashes = { left: left?.hashes, right: right?.hashes };
  return emitReport(options, report, projections);
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { process.exitCode = compareHttps(argumentsFor(process.argv.slice(2))); }
  catch (error) { console.error(error.message); process.exitCode = 2; }
}
