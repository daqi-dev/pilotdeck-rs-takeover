#!/usr/bin/env node
// ui-smoke · 复现：WS 客户端不发 close 帧就断开 TCP（ui/server 进程退出时就是这样），之后 gateway 收到 SIGTERM 能否退出。
// 用法：node scripts/ui-smoke/halfopen-shutdown.mjs <ts|rust> <end|closeframe> [等待毫秒=20000]
//   end        客户端 hello 之后直接 socket.end()（只发 FIN，不发 WS close 帧）
//   closeframe 客户端先发 WS close 帧（对照组）
// 在 UI_SMOKE_ROOT（默认 /private/tmp/pdrs-ui-smoke/halfopen）下建隔离的 HOME / PILOT_HOME，gateway 监听 127.0.0.1:39101（UI_SMOKE_PORT_BASE+1）。
// 输出一行「<侧> <方式> exit 0 <毫秒>」或「... timeout」；超时后按进程组 SIGKILL。
// 基线 cc7c4e6 上 end 方式在 90 秒内都不退出（src/gateway/server/websocket.ts 只在 socket 'close' 时移除连接，
// 对端只发 FIN 时 socket 停在半开状态，GatewayServer 的 server.close() 一直等它）。
import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, '../..');
const [side, how, waitArg] = process.argv.slice(2);
if (!['ts', 'rust'].includes(side) || !['end', 'closeframe'].includes(how)) { console.error('usage: halfopen-shutdown.mjs <ts|rust> <end|closeframe> [waitMs]'); process.exit(2); }
const WAIT = Number(waitArg ?? 20000);
const BASELINE = path.resolve(process.env.UI_SMOKE_BASELINE ?? path.join(REPO, 'resources/PilotDeck'));
const TARGET = path.resolve(process.env.UI_SMOKE_TARGET ?? path.join(REPO, 'src/target/debug'));
const ROOT = path.resolve(process.env.UI_SMOKE_ROOT ?? '/private/tmp/pdrs-ui-smoke/halfopen');
const PORT = Number(process.env.UI_SMOKE_PORT_BASE ?? 39100) + 1;
const TOKEN = 'ui-smoke-gateway-token-0000000000000000000';

fs.rmSync(ROOT, { recursive: true, force: true });
const home = path.join(ROOT, 'home'); const pilotHome = path.join(home, '.pilotdeck'); const proj = path.join(ROOT, 'proj'); const tmp = path.join(ROOT, 'tmp');
for (const d of [pilotHome, proj, tmp]) fs.mkdirSync(d, { recursive: true });
fs.writeFileSync(path.join(pilotHome, 'server-token'), `${TOKEN}\n`, { mode: 0o600 });
// 最小配置（模型不会被调用；没有配置文件时 gateway 不起）。
fs.writeFileSync(path.join(pilotHome, 'pilotdeck.yaml'), `schemaVersion: 1
agent:
  model: smoke/deterministic
model:
  providers:
    smoke:
      protocol: openai
      url: http://127.0.0.1:${PORT - 1}/v1
      apiKey: ui-smoke-mock-only
      models:
        deterministic: {}
telemetry:
  enabled: false
`);
const sb = path.join(tmp, 'sb.sb');
fs.writeFileSync(sb, `(version 1)\n(allow default)\n(deny network-outbound)\n(allow network-outbound (remote ip "localhost:*"))\n(allow network-outbound (remote unix-socket))\n(deny file-read* (subpath ${JSON.stringify(path.join(os.userInfo().homedir, '.pilotdeck'))}))\n`);
const env = { PATH: process.env.PATH, HOME: home, PILOT_HOME: pilotHome, TMPDIR: tmp, TZ: 'UTC', NO_COLOR: '1', PILOTDECK_SKIP_BROWSER_OPEN: '1' };
const cmd = side === 'ts' ? [process.execPath, path.join(BASELINE, 'dist/src/cli/pilotdeck.js'), 'server', '--port', String(PORT)] : [path.join(TARGET, 'pilotdeck'), 'server', '--port', String(PORT)];
const child = spawn('sandbox-exec', ['-f', sb, ...cmd], { cwd: proj, env, stdio: 'ignore', detached: true });
const up = () => new Promise((r) => { const s = net.connect(PORT, '127.0.0.1').once('connect', () => { s.destroy(); r(true); }).once('error', () => r(false)); });
for (let i = 0; !(await up()); i++) {
  if (child.exitCode !== null || child.signalCode) throw new Error(`gateway exited before listening (${child.exitCode ?? child.signalCode})`);
  if (i > 600) { try { process.kill(-child.pid, 'SIGKILL'); } catch {} throw new Error('gateway did not listen'); }
  await new Promise((r) => setTimeout(r, 100));
}

const sock = net.connect(PORT, '127.0.0.1');
await new Promise((r) => sock.once('connect', r));
sock.write(`GET /ws HTTP/1.1\r\nHost: 127.0.0.1:${PORT}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: ${crypto.randomBytes(16).toString('base64')}\r\nSec-WebSocket-Version: 13\r\n\r\n`);
const frame = (payload, opcode) => {
  const mask = crypto.randomBytes(4); const len = payload.length;
  const head = len < 126 ? Buffer.from([0x80 | opcode, 0x80 | len]) : Buffer.from([0x80 | opcode, 0x80 | 126, len >> 8, len & 255]);
  return Buffer.concat([head, mask, Buffer.from(payload.map((b, i) => b ^ mask[i & 3]))]);
};
await new Promise((r) => sock.once('data', r)); // 101
sock.write(frame(Buffer.from(JSON.stringify({ type: 'hello', protocolVersion: '1.2', clientName: 'web', clientVersion: '0.1.0', token: TOKEN })), 1));
await new Promise((r) => sock.once('data', r)); // hello_ok
if (how === 'end') sock.end(); else sock.write(frame(Buffer.from([0x03, 0xe8]), 8));
await new Promise((r) => setTimeout(r, 1000));
const t0 = Date.now();
process.kill(child.pid, 'SIGTERM');
const res = await Promise.race([new Promise((r) => child.once('exit', (c, s) => r(`exit ${c ?? s}`))), new Promise((r) => setTimeout(() => r('timeout'), WAIT))]);
console.log(`${side} ${how} ${res} ${Date.now() - t0}ms`);
if (res === 'timeout') { try { process.kill(-child.pid, 'SIGKILL'); } catch {} }
sock.destroy();
