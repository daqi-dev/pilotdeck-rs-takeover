// 生成的上游用例在运行时 import 这个模块（见 generate.mjs 的改写规则）。
// 它只做三件事：
//   1. 按 PDRS_UT_TARGET 决定被测的 sidecar 是哪个外部进程；
//   2. 把 node:test 的 test() 包一层：按 manifest 跳过登记过的用例，并记下用例是否碰到了 sidecar；
//   3. 可选地把宿主与 sidecar 之间的每一帧转录下来（给 schema 校验用）。
// 用例本身的断言一条不动。
import nodeTest from 'node:test';
import { AsyncLocalStorage } from 'node:async_hooks';
import { appendFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { exclusionFor } from './manifest.mjs';

const env = process.env;
const baseline = required('PDRS_UT_BASELINE');
const target = required('PDRS_UT_TARGET');
const group = required('PDRS_UT_GROUP');
if (!['baseline', 'rust', 'inprocess'].includes(target)) {
  throw new Error(`PDRS_UT_TARGET must be baseline, rust or inprocess, got ${target}`);
}
const rustBin = target === 'rust' ? required('PDRS_SIDECAR_BIN') : undefined;
const baselineSidecarJs = join(baseline, 'dist/src/cli/pilotdeck-agent-loop-sidecar.js');
const touchFile = env.PDRS_UT_TOUCH_FILE;
const transcriptFile = env.PDRS_UT_TRANSCRIPT_FILE;

const { createStdioAgentLoopSidecarConnectionFactory } = await import(
  pathToFileURL(join(baseline, 'dist/src/agent/index.js')).href);

const current = new AsyncLocalStorage();
let lastStarted;
let connectionSeq = 0;

function required(name) {
  const value = env[name];
  if (!value) throw new Error(`${name} is required (set by scripts/upstream-tests/run.mjs)`);
  return value;
}

function touch(kind) {
  const name = current.getStore() ?? lastStarted;
  if (touchFile && name !== undefined) {
    appendFileSync(touchFile, JSON.stringify({ group, name, kind }) + '\n');
  }
}

/** node:test 的 test(name, fn)，外加 manifest 跳过与「碰没碰 sidecar」的记录。 */
export default function test(name, fn) {
  const excluded = exclusionFor(group, name);
  if (excluded) {
    return nodeTest(name, { skip: `PDRS-EXCLUDED[${excluded.kind}]: ${excluded.reason}` }, fn);
  }
  return nodeTest(name, (t) => {
    lastStarted = name;
    return current.run(name, () => fn(t));
  });
}

/** 被测 sidecar 的启动命令（替换上游的 process.execPath）。 */
export function sidecarCommand() {
  touch('command');
  return target === 'rust' ? rustBin : process.execPath;
}

/** 被测 sidecar 的启动参数（替换上游的 [builtSidecarPath()]）。 */
export function sidecarArgs() {
  return target === 'rust' ? [] : [baselineSidecarJs];
}

/**
 * 进程级部署（createLocalGateway + PILOTDECK_AGENT_LOOP_TRANSPORT=stdio）的附加环境：
 * 基线不加任何东西（上游原样就是拉起基线编译好的 sidecar），Rust 用生产配置的
 * PILOTDECK_AGENT_LOOP_SIDECAR_COMMAND 选中二进制。
 */
export function stdioDeploymentEnv() {
  touch('deployment');
  return target === 'rust' ? { PILOTDECK_AGENT_LOOP_SIDECAR_COMMAND: rustBin } : {};
}

/** 上游用 process.cwd() 指基线仓库根；这里换成 run.mjs 准备的隔离根目录。 */
export function cwdRoot() {
  return required('PDRS_UT_CWD_ROOT');
}

/**
 * 替换上游的 loopbackConnection()：原本是进程内的 TS sidecar server 接两条 PassThrough，
 * 这里换成一个外部 sidecar 子进程的 stdio 连接（基线的生产 stdio 客户端）。
 * PDRS_UT_TARGET=inprocess 时仍用上游原来的进程内实现（只用来对照）。
 */
export function externalConnection(upstreamLoopback) {
  touch('loopback');
  const connection = target === 'inprocess'
    ? upstreamLoopback()
    : createStdioAgentLoopSidecarConnectionFactory({
      command: target === 'rust' ? rustBin : process.execPath,
      args: target === 'rust' ? [] : [baselineSidecarJs],
      killTimeoutMs: 5_000,
    })();
  return plainConnection(connection);
}

// 上游用例常写 `{ ...loopbackConnection(), send(m) { … } }` 来截获发出的帧：上游的 loopback 连接是
// 普通对象，展开后 receive / close 还在；生产 stdio 连接是类实例，方法在原型上，展开会丢。
// 所以这里一律包成与 loopback 同形的普通对象（只有 send / receive / close），顺带做可选的转录。
function plainConnection(connection) {
  const id = ++connectionSeq;
  const test = current.getStore() ?? lastStarted;
  const record = (direction, frame) => {
    if (!transcriptFile) return;
    appendFileSync(transcriptFile, JSON.stringify({ group, target, test, connection: id, direction, frame }) + '\n');
  };
  return {
    send(message) {
      record('host_to_sidecar', message);
      return connection.send(message);
    },
    receive() {
      const source = connection.receive();
      return (async function* () {
        for await (const frame of source) {
          record('sidecar_to_host', frame);
          yield frame;
        }
      })();
    },
    close(reason) {
      return connection.close?.(reason);
    },
  };
}
