#!/usr/bin/env node
// ui-smoke · ui/server 与 gateway 之间的透明转发器，逐帧记账。
// 用法：node scripts/ui-smoke/proxy.mjs <监听端口> <目标端口> <日志.jsonl>
// 只监听 127.0.0.1。普通 HTTP 请求原样转发并记录请求与响应；WebSocket 升级后双向原样转发字节，
// 同时被动解析帧（文本帧记 JSON，其余记 opcode）。转发时去掉 Sec-WebSocket-Extensions，
// 免得协商出 permessage-deflate 后看不到帧内容——这是唯一一处改动，对两组相同。
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';

const [listenPort, targetPort, logPath] = process.argv.slice(2);
if (!listenPort || !targetPort || !logPath) { console.error('usage: proxy.mjs <listen> <target> <log.jsonl>'); process.exit(2); }
let seq = 0; let connSeq = 0;
const log = (entry) => fs.appendFileSync(logPath, JSON.stringify({ seq: ++seq, at: Date.now(), ...entry }) + '\n');
const parseMaybe = (s) => { try { return JSON.parse(s); } catch { return s; } };

// 被动 WS 帧解析：喂入字节，吐出完整消息（处理分片与掩码）。
function frameParser(onMessage) {
  let buf = Buffer.alloc(0); let fragments = []; let fragOpcode = 0;
  return (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    for (;;) {
      if (buf.length < 2) return;
      const fin = (buf[0] & 0x80) !== 0; const opcode = buf[0] & 0x0f;
      const masked = (buf[1] & 0x80) !== 0; let len = buf[1] & 0x7f; let off = 2;
      if (len === 126) { if (buf.length < 4) return; len = buf.readUInt16BE(2); off = 4; }
      else if (len === 127) { if (buf.length < 10) return; len = Number(buf.readBigUInt64BE(2)); off = 10; }
      const maskOff = off; if (masked) off += 4;
      if (buf.length < off + len) return;
      let payload = Buffer.from(buf.subarray(off, off + len));
      if (masked) for (let i = 0; i < payload.length; i++) payload[i] ^= buf[maskOff + (i & 3)];
      buf = buf.subarray(off + len);
      if (opcode === 0) { fragments.push(payload); if (fin) { onMessage(fragOpcode, Buffer.concat(fragments)); fragments = []; } continue; }
      if (!fin && opcode < 8) { fragOpcode = opcode; fragments = [payload]; continue; }
      onMessage(opcode, payload);
    }
  };
}

const server = http.createServer((req, res) => {
  const conn = ++connSeq; const parts = [];
  req.on('data', (p) => parts.push(p));
  req.on('end', () => {
    const body = Buffer.concat(parts);
    log({ conn, kind: 'http-req', method: req.method, url: req.url, headers: req.headers, body: parseMaybe(body.toString('utf8')) });
    const up = http.request({ host: '127.0.0.1', port: Number(targetPort), method: req.method, path: req.url, headers: req.headers }, (ur) => {
      const rparts = [];
      res.writeHead(ur.statusCode, ur.headers);
      ur.on('data', (p) => { rparts.push(p); res.write(p); });
      ur.on('end', () => {
        res.end();
        log({ conn, kind: 'http-res', status: ur.statusCode, headers: ur.headers, body: parseMaybe(Buffer.concat(rparts).toString('utf8')) });
      });
    });
    up.on('error', (e) => { log({ conn, kind: 'http-error', error: e.message }); res.writeHead(502); res.end(); });
    up.end(body);
  });
});

server.on('upgrade', (req, client, head) => {
  const conn = ++connSeq;
  const headers = { ...req.headers }; delete headers['sec-websocket-extensions'];
  log({ conn, kind: 'ws-open', url: req.url, headers });
  const upstream = net.connect(Number(targetPort), '127.0.0.1', () => {
    const lines = [`${req.method} ${req.url} HTTP/1.1`, ...Object.entries(headers).map(([k, v]) => `${k}: ${v}`), '', ''];
    upstream.write(lines.join('\r\n'));
    if (head?.length) upstream.write(head);
  });
  const onMsg = (dir) => (opcode, payload) => {
    if (opcode === 1) log({ conn, kind: 'ws', dir, frame: parseMaybe(payload.toString('utf8')) });
    else if (opcode === 8) log({ conn, kind: 'ws-close', dir, code: payload.length >= 2 ? payload.readUInt16BE(0) : null, reason: payload.subarray(2).toString('utf8') });
    else if (opcode !== 9 && opcode !== 10) log({ conn, kind: 'ws-binary', dir, opcode, length: payload.length });
  };
  const upParser = frameParser(onMsg('up'));
  let downHeadDone = false; let downPending = Buffer.alloc(0);
  const downParser = frameParser(onMsg('down'));
  client.on('data', (d) => { upstream.write(d); upParser(d); });
  upstream.on('data', (d) => {
    client.write(d);
    if (downHeadDone) { downParser(d); return; }
    downPending = Buffer.concat([downPending, d]);
    const end = downPending.indexOf('\r\n\r\n');
    if (end < 0) return;
    const status = downPending.subarray(0, end).toString('utf8').split('\r\n')[0];
    log({ conn, kind: 'ws-handshake', status });
    downHeadDone = true; const rest = downPending.subarray(end + 4); downPending = null;
    if (rest.length) downParser(rest);
  });
  let ended = false;
  const close = (who) => () => { if (ended) return; ended = true; log({ conn, kind: 'ws-end', who }); client.destroy(); upstream.destroy(); };
  client.on('close', close('client')); upstream.on('close', close('upstream'));
  client.on('error', () => {}); upstream.on('error', () => {});
});

server.listen(Number(listenPort), '127.0.0.1', () => console.log(`proxy 127.0.0.1:${listenPort} -> 127.0.0.1:${targetPort}`));
