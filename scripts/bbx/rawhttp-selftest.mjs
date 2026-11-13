#!/usr/bin/env node
// 采集器自测：raw 步骤记 HTTP 应答的规则（lib/rawhttp.mjs）。头名大小写、chunk 长度的写法这类 HTTP 语义等价的
// 传输细节记成同一个结果；状态行、头的值、正文、是否 chunked、管线化的后续应答这些照样分得开。
// 用法：node scripts/bbx/rawhttp-selftest.mjs（退出码 0 表示全部符合预期）。
import { parseHttpResponses } from './lib/rawhttp.mjs';
const same = (a, b) => JSON.stringify(parseHttpResponses(a)) === JSON.stringify(parseHttpResponses(b));
const lower = 'HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nDate: Thu, 01 Jan 2026 00:00:00 GMT\r\nTransfer-Encoding: chunked\r\n\r\nb\r\n{"ok":true}\r\n0\r\n\r\n';
const upper = 'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\ndate: Fri, 02 Jan 2026 00:00:00 GMT\r\ntransfer-encoding: chunked\r\n\r\nB\r\n{"ok":true}\r\n0\r\n\r\n';
const split = 'HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{"ok\r\n7\r\n":true}\r\n0\r\n\r\n';
const sized = 'HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{"ok":true}';
const pipe = (second) => lower + `HTTP/1.1 404 Not Found\r\n${second}: text/plain; charset=utf-8\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nnot found\r\n0\r\n\r\n`;
const utf = 'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n6\r\n' + Buffer.from('héllo').toString('latin1') + '\r\n0\r\n\r\n';
const cases = [
  ['头名大小写、chunk 长度大小写、date 不同', same(lower, upper), true],
  ['chunk 切法不同', same(lower, split), true],
  ['chunked 与 content-length 分得开', same(lower, sized), false],
  ['正文不同照报', same(lower, lower.replace('{"ok":true}', '{"ok":fals}')), false],
  ['头的值不同照报', same(lower, lower.replace('application/json', 'text/plain')), false],
  ['状态行不同照报', same(lower, lower.replace('HTTP/1.1 200 OK', 'HTTP/1.0 200 OK')), false],
  ['管线化的第二个应答头名大小写不同', same(pipe('content-type'), pipe('Content-Type')), true],
  ['管线化的第二个应答缺了照报', same(pipe('content-type'), lower), false],
  ['多字节正文按字节数切', parseHttpResponses(utf).body === 'héllo', true],
  ['没收完的 chunked 标出来', parseHttpResponses(lower.slice(0, -5)).chunked === 'incomplete', true],
];
let bad = 0;
for (const [name, got, expected] of cases) if (got !== expected) { bad++; console.log(`不符合预期  ${name}`); }
console.log(`采集器 raw HTTP 自测：${cases.length - bad}/${cases.length} 符合预期`);
process.exit(bad ? 1 : 0);
