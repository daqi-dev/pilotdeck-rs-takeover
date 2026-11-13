#!/usr/bin/env node
// 诊断用的 stdio 转录垫片：把自己当成 sidecar 命令交给宿主，实际拉起 PDRS_TEE_TARGET（逗号分隔的
// 命令与参数），原样转发 stdin / stdout / stderr，并把每一行 NDJSON 连同方向写到 PDRS_TEE_LOG。
// 只用于 schema 采集与排查差异；验收集本身不经过它（对拍的身份核对要求命令就是被测二进制）。
import { spawn } from 'node:child_process';
import { appendFileSync } from 'node:fs';
import { createInterface } from 'node:readline';

const [command, ...args] = String(process.env.PDRS_TEE_TARGET ?? '').split(',').filter(Boolean);
const log = process.env.PDRS_TEE_LOG;
if (!command || !log) {
  process.stderr.write('tee-sidecar: PDRS_TEE_TARGET and PDRS_TEE_LOG are required\n');
  process.exit(2);
}
const connection = `${process.pid}`;
const env = { ...process.env };
delete env.PDRS_TEE_TARGET;
delete env.PDRS_TEE_LOG;
const child = spawn(command, args, { stdio: ['pipe', 'pipe', 'inherit'], env });
const record = (direction, line) => {
  let frame;
  try { frame = JSON.parse(line); } catch { frame = { unparsed: line }; }
  appendFileSync(log, JSON.stringify({ connection, t: Date.now(), direction, frame }) + '\n');
};
createInterface({ input: process.stdin, crlfDelay: Infinity }).on('line', (line) => {
  record('host_to_sidecar', line);
  child.stdin.write(`${line}\n`);
}).on('close', () => child.stdin.end());
createInterface({ input: child.stdout, crlfDelay: Infinity }).on('line', (line) => {
  record('sidecar_to_host', line);
  process.stdout.write(`${line}\n`);
});
for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => child.kill(signal));
child.on('close', (code, signal) => {
  process.stdin.destroy();
  if (signal) process.kill(process.pid, signal);
  else process.exitCode = code ?? 1;
});
