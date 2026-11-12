#!/usr/bin/env node
// Fixed vendor URLs are tested only in a Docker network namespace with no
// external interfaces. DNS must point every vendor hostname to this mock.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import https from 'node:https';
import { lookup } from 'node:dns/promises';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { drive } from './capture.mjs';

const [side, executableArg, outputArg] = process.argv.slice(2);
assert.ok((side === 'ts' || side === 'rust') && executableArg && outputArg,
  'usage: setup-https-capture.mjs <ts BASELINE|rust BINARY> NEW_OUTPUT_DIR');
assert.equal(os.platform(), 'linux', 'HTTPS fixture requires Linux Docker');
assert.ok(fs.existsSync('/.dockerenv'), 'HTTPS fixture requires Docker');
assert.ok(Object.values(os.networkInterfaces()).flat().every(address => address.internal),
  'HTTPS fixture requires --network none (only loopback)');
const hosts = ['open.feishu.cn', 'open.larksuite.com', 'work.weixin.qq.com', 'ilinkai.weixin.qq.com'];
for (const host of hosts) {
  const addresses = await lookup(host, { all: true });
  assert.ok(addresses.length && addresses.every(item => item.address === '127.0.0.1'),
    `${host} must map exclusively to 127.0.0.1 inside the container`);
}
const executable = fs.realpathSync(executableArg);
const entry = side === 'ts' ? path.join(executable, 'dist/src/cli/pilotdeck.js') : executable;
const command = side === 'ts' ? process.execPath : executable;
const prefix = side === 'ts' ? [entry] : [];
if (side === 'ts') assert.equal(spawnSync('git', ['-C', executable, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).stdout.trim(),
  'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85', 'fixed TS source');
const output = path.resolve(outputArg); fs.mkdirSync(output);
const tls = path.join(output, 'test-tls'); fs.mkdirSync(tls, { mode: 0o700 });
const key = path.join(tls, 'key.pem'); const certificate = path.join(tls, 'cert.pem');
const certResult = spawnSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes',
  '-keyout', key, '-out', certificate, '-days', '1', '-subj', '/CN=C1 local setup fixture',
  '-addext', `subjectAltName=${hosts.map(host => `DNS:${host}`).join(',')}`], {
  encoding: 'utf8', env: { PATH: process.env.PATH, OPENSSL_CONF: '/dev/null' },
});
assert.equal(certResult.status, 0, `test certificate generation: ${certResult.stderr}`);
fs.chmodSync(key, 0o600);
const tokenPath = '/open-apis/auth/v3/tenant_access_token/internal';
const qrPath = '/ilink/bot/get_bot_qrcode?bot_type=3';
const statusPath = '/ilink/bot/get_qrcode_status?qrcode=c1-qr';
const feishuAnswers = domain => [
  ['使用哪个平台？ [feishu/lark] (当前: feishu): ', domain],
  ['App ID: ', 'c1-fixture-app'], ['App Secret: ', 'c1-fixture-secret'],
];
const wecomTail = [
  ['WebSocket URL (默认: wss://openws.work.weixin.qq.com): ', ''],
  ['私聊访问策略 [1=open 推荐, 2=allowlist, 3=disabled] (默认 1): ', '1'],
  ['群聊访问策略 [1=disabled 推荐, 2=open, 3=allowlist] (默认 1): ', '1'],
];
const specs = [
  { id: 'feishu-success', platform: 'feishu', answers: feishuAnswers('feishu'),
    text: '✅ 凭据验证通过！', yaml: ['feishu:', 'enabled: true', 'appId: c1-fixture-app', 'domainName: feishu'],
    routes: [{ host: hosts[0], path: tokenPath, method: 'POST', body: { app_id: 'c1-fixture-app', app_secret: 'c1-fixture-secret' }, reply: { code: 0, tenant_access_token: 'c1-local-token' } }] },
  { id: 'lark-rejected-cancel', platform: 'lark', answers: [...feishuAnswers('lark'), ['是否仍然保存配置？ [y/N]: ', 'n']],
    text: '已取消。', untouched: true,
    routes: [{ host: hosts[1], path: tokenPath, method: 'POST', reply: { code: 999, msg: 'C1 rejected' } }] },
  { id: 'feishu-rejected-save', platform: 'feishu', answers: [...feishuAnswers('feishu'), ['是否仍然保存配置？ [y/N]: ', 'yes']],
    text: '飞书配置已写入 pilotdeck.yaml', yaml: ['appSecret: c1-fixture-secret'],
    routes: [{ host: hosts[0], path: tokenPath, method: 'POST', reply: { code: 999, msg: 'C1 rejected' } }] },
  { id: 'wecom-qr-success', platform: 'wecom', answers: [['是否扫码自动创建/获取 Bot ID 和 Secret？ [Y/n]: ', 'y'], ...wecomTail],
    text: '企业微信配置已写入 pilotdeck.yaml', yaml: ['token: c1-qr-bot', 'secret: c1-qr-secret'],
    routes: [
      { host: hosts[2], path: '/ai/qc/generate?source=hermes', method: 'GET', reply: { data: { scode: 'c1-scode', auth_url: 'https://fixture.invalid/qr' } } },
      { host: hosts[2], path: '/ai/qc/query_result?scode=c1-scode', method: 'GET', reply: { data: { status: 'success', bot_info: { botid: 'c1-qr-bot', secret: 'c1-qr-secret' } } } },
    ] },
  ...[
    { id: 'weixin-success', text: '微信登录成功！' },
    { id: 'weixin-missing-pilot-home', missingPilot: true, text: side === 'ts' ? '微信登录失败:' : '微信登录成功！' },
    { id: 'weixin-config-write-failure', configDirectory: true, text: '微信登录失败:' },
  ].map(spec => ({ ...spec, platform: 'weixin', answers: [['继续扫码登录？ [Y/n]: ', 'y']],
    routes: [
      { host: hosts[3], path: qrPath, method: 'GET', reply: { qrcode: 'c1-qr', qrcode_img_content: 'https://fixture.invalid/qr' } },
      { host: hosts[3], path: statusPath, method: 'GET', reply: { status: 'confirmed', ilink_bot_id: 'c1-account', bot_token: 'c1-bot-token', baseurl: `https://${hosts[3]}` } },
    ], creds: spec.missingPilot ? side === 'rust' : true,
    ...(spec.configDirectory || (spec.missingPilot && side === 'ts') ? {} : { yaml: ['weixin:', 'enabled: true'] }),
  })),
  { id: 'weixin-http-failure', platform: 'weixin', answers: [['继续扫码登录？ [Y/n]: ', 'y']],
    text: '微信登录失败: Failed to fetch QR code: 503', untouched: true, creds: false,
    routes: [{ host: hosts[3], path: qrPath, method: 'GET', status: 503, reply: {} }] },
];
for (const spec of specs) for (const route of spec.routes) {
  if (route.path === tokenPath) {
    route.body = { app_id: 'c1-fixture-app', app_secret: 'c1-fixture-secret' };
    route.headers = { 'content-type': 'application/json; charset=utf-8' };
  } else if (route.host === hosts[2]) route.headers = { 'user-agent': 'PilotDeck/1.0' };
  else if (route.path === statusPath) route.headers = { 'ilink-app-clientversion': '1' };
}
fs.writeFileSync(path.join(output, 'identity.json'), JSON.stringify({
  side, node: process.version, platform: `${os.platform()}-${os.arch()}`, entry,
  executableSha256: createHash('sha256').update(fs.readFileSync(entry)).digest('hex'),
  casesSha256: createHash('sha256').update(JSON.stringify(specs)).digest('hex'),
  note: 'missing PILOT_HOME is a recorded baseline failure and an explicit Rust bug-fix expectation; requires coordinator difference registration',
}, null, 2) + '\n');
let failed = false;
for (const spec of specs) {
  const root = path.join(output, spec.id); const home = path.join(root, 'home');
  const pilot = path.join(home, spec.missingPilot ? 'custom-pilot' : '.pilotdeck');
  const config = path.join(root, 'config', 'pilotdeck.yaml');
  const project = path.join(root, 'project');
  fs.mkdirSync(home, { recursive: true }); fs.mkdirSync(path.dirname(config)); fs.mkdirSync(project);
  if (!spec.missingPilot) fs.mkdirSync(pilot);
  const initial = 'schemaVersion: 1\ntelemetry:\n  enabled: false\n';
  if (spec.configDirectory) fs.mkdirSync(config); else fs.writeFileSync(config, initial);
  const requests = []; let mockFailure;
  const server = https.createServer({ key: fs.readFileSync(key), cert: fs.readFileSync(certificate) }, async (req, res) => {
    try {
      const chunks = []; for await (const chunk of req) chunks.push(chunk);
      const raw = Buffer.concat(chunks).toString();
      const body = raw ? JSON.parse(raw) : undefined;
      const route = spec.routes[requests.length];
      requests.push({ method: req.method, path: req.url, headers: req.headers, ...(body ? { body } : {}) });
      assert.ok(route, 'no unexpected extra request');
      assert.equal(req.headers.host, route.host); assert.equal(req.url, route.path); assert.equal(req.method, route.method);
      if (route.body) assert.deepEqual(body, route.body);
      for (const [name, value] of Object.entries(route.headers ?? {})) assert.equal(req.headers[name], value, `required request header ${name}`);
      res.writeHead(route.status ?? 200, { 'content-type': 'application/json' }); res.end(JSON.stringify(route.reply));
    } catch (error) { mockFailure = error.message; res.writeHead(500); res.end('{}'); }
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(443, '127.0.0.1', resolve); });
  const result = await drive(command, [...prefix, 'gateway', 'setup', spec.platform], {
    PATH: process.env.PATH, HOME: home, USERPROFILE: home, CFFIXED_USER_HOME: home,
    PILOT_HOME: pilot, PILOTDECK_CONFIG_PATH: config, NODE_EXTRA_CA_CERTS: certificate,
    TMPDIR: root, LANG: 'C', LC_ALL: 'C', TZ: 'UTC', TERM: 'dumb', NO_COLOR: '1',
    DOCKER_CONTAINER: '1', COMMIT_HASH: 'c1-fixture',
  }, project, spec.answers, 15000);
  server.closeAllConnections(); await new Promise(resolve => server.close(resolve));
  const yaml = spec.configDirectory ? null : fs.readFileSync(config, 'utf8');
  const credsPath = path.join(pilot, 'weixin-credentials.json');
  const credentials = fs.existsSync(credsPath) ? fs.readFileSync(credsPath, 'utf8') : null;
  let assertionFailure;
  try {
    assert.equal(result.problem, undefined); assert.equal(mockFailure, undefined);
    assert.equal(result.exitCode, 0); assert.equal(result.signal, null);
    assert.equal(result.answered, spec.answers.length); assert.equal(requests.length, spec.routes.length);
    assert.ok(result.stdout.includes(spec.text), spec.text);
    for (const value of spec.yaml ?? []) assert.ok(yaml.includes(value), value);
    if (spec.untouched) assert.equal(yaml, initial);
    if (spec.creds !== undefined) assert.equal(credentials !== null, spec.creds, 'credentials existence');
    if (spec.creds) assert.deepEqual(JSON.parse(credentials), {
      baseUrl: `https://${hosts[3]}`, botToken: 'c1-bot-token', accountId: 'c1-account',
    });
    if (spec.configDirectory) assert.ok(fs.statSync(config).isDirectory(), 'failed destination unchanged');
  } catch (error) { assertionFailure = error.message; failed = true; }
  fs.writeFileSync(path.join(root, 'result.json'), JSON.stringify({ id: spec.id, ...result, requests,
    yaml, credentials, ...(mockFailure ? { mockFailure } : {}), ...(assertionFailure ? { assertionFailure } : {}),
  }, null, 2) + '\n');
  console.log(`${spec.id}: ${assertionFailure ? `FAIL ${assertionFailure}` : 'PASS'}`);
}
fs.rmSync(key); // Ephemeral test key is not a delivery artifact.
process.exitCode = failed ? 1 : 0;
