// 按基线的 Module Protocol v2 schema（docs/pilotdeck-module-protocol-v2.schema.json）逐帧校验转录下来的帧。
// 校验器用基线 node_modules 里现成的 ajv（2020-12 方言）与 ajv-formats，不新增依赖。
//
// 输入是 JSONL，每行 { direction, frame, ... }（runtime.mjs 的转录或 tee-sidecar.mjs 的日志都是这个形状）。
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

export function loadValidator(baseline) {
  const require = createRequire(join(baseline, 'package.json'));
  const Ajv2020 = require('ajv/dist/2020').default;
  const addFormats = require('ajv-formats').default ?? require('ajv-formats');
  const schema = JSON.parse(readFileSync(join(baseline, 'docs/pilotdeck-module-protocol-v2.schema.json'), 'utf8'));
  // strict:true 让 ajv 遇到不认识的关键字直接报错，这样「schema 里有校验器没执行的关键字」不会被悄悄忽略。
  const ajv = new Ajv2020({ allErrors: true, strict: true, strictRequired: false, allowUnionTypes: true });
  addFormats(ajv);
  const validate = ajv.compile(schema);
  // 判定只用整份 schema（顶层 oneOf）；按 kind / method 选出的分支只用来给出能读的错误。
  const branch = Object.fromEntries(['executeRequest', 'moduleCallRequest', 'controlRequest', 'handshakeRequest', 'response', 'event', 'error']
    .map((name) => [name, ajv.compile({ $ref: `${schema.$id}#/$defs/${name}` })]));
  return { validate, branch, version: require('ajv/package.json').version, schemaId: schema.$id };
}

function branchFor(frame) {
  if (frame?.kind === 'request') {
    if (frame.method === 'execute') return 'executeRequest';
    if (frame.method === 'module_call') return 'moduleCallRequest';
    if (['hello', 'capabilities'].includes(frame.method)) return 'handshakeRequest';
    return 'controlRequest';
  }
  if (frame?.kind === 'response') return 'response';
  if (frame?.kind === 'event') return 'event';
  return 'error';
}

/** 校验若干 JSONL 文件里指定方向的帧，返回统计与按错误归类的失败样本。 */
export function validateFrames(validator, files, { direction = 'sidecar_to_host' } = {}) {
  const stats = { frames: 0, valid: 0, invalid: 0, byKind: {}, failures: new Map() };
  for (const file of files) {
    const lines = readFileSync(file, 'utf8').split('\n').filter(Boolean);
    for (const line of lines) {
      const row = JSON.parse(line);
      if (direction !== 'all' && row.direction !== direction) continue;
      const frame = row.frame;
      const kind = `${row.direction === 'host_to_sidecar' ? '>>' : '<<'} ${frame?.kind}:${frame?.method ?? frame?.eventType ?? (frame?.ok === false ? 'error' : 'ok')}`;
      stats.frames += 1;
      stats.byKind[kind] ??= { total: 0, invalid: 0 };
      stats.byKind[kind].total += 1;
      if (validator.validate(frame)) {
        stats.valid += 1;
        continue;
      }
      stats.invalid += 1;
      stats.byKind[kind].invalid += 1;
      const branchName = branchFor(frame);
      const check = validator.branch[branchName];
      const errors = check(frame) ? validator.validate.errors : check.errors;
      const key = `${kind} | ${branchName}: ${summarizeErrors(errors)}`;
      const entry = stats.failures.get(key) ?? { count: 0, sample: frame, test: row.test, errors };
      entry.count += 1;
      stats.failures.set(key, entry);
    }
  }
  return stats;
}

// oneOf 的失败会把每个分支的错误都列出来；只留最能说明问题的几条（非 oneOf / const / $ref 的叶子错误）。
function summarizeErrors(errors = []) {
  const leaves = errors.filter((e) => !['oneOf', 'if'].includes(e.keyword));
  return [...new Set(leaves.map((e) => `${e.instancePath || '/'} ${e.keyword} ${JSON.stringify(e.params)}`))].slice(0, 6).join('; ');
}

/**
 * 校验器自检：从已通过的帧里取样本，各做一处已知违规（删必填字段、加协议外字段、改协议版本、
 * 改字段名、改取值），每一处都必须被判为不合法；否则说明校验器没有在起作用。
 */
export function selfCheck(validator, files) {
  const samples = {};
  for (const file of files) {
    for (const line of readFileSync(file, 'utf8').split('\n').filter(Boolean)) {
      const { direction, frame } = JSON.parse(line);
      if (direction !== 'sidecar_to_host' || !validator.validate(frame)) continue;
      const key = frame.kind === 'request' ? `request:${frame.method}`
        : frame.kind === 'response' && frame.protocolVersion ? 'response:handshake' : frame.kind;
      samples[key] ??= frame;
    }
  }
  const mutations = [
    ['module_call 删 payload', samples['request:module_call'], (f) => { delete f.payload; }],
    ['module_call 加顶层 operation', samples['request:module_call'], (f) => { f.operation = 'x'; }],
    ['module_call 未知 module', samples['request:module_call'], (f) => { f.module = 'nope'; }],
    ['握手回应协议版本 2.1', samples['response:handshake'], (f) => { f.protocolVersion = '2.1'; }],
    ['事件 streamId 改名 stream_id', samples.event, (f) => { f.stream_id = f.streamId; delete f.streamId; }],
    ['事件 final 为真却没有 outcome', samples.event, (f) => { f.final = true; delete f.outcome; }],
    ['事件 sequence 为负', samples.event, (f) => { f.sequence = -1; }],
  ];
  return mutations.map(([name, sample, mutate]) => {
    if (!sample) return { name, ok: false, why: '没有可用的样本帧' };
    const frame = structuredClone(sample);
    mutate(frame);
    const rejected = !validator.validate(frame);
    return { name, ok: rejected, why: rejected ? '' : '注入的违规没有被判为不合法' };
  });
}

// 单独使用：node scripts/upstream-tests/lib/schema.mjs <sidecar_to_host|host_to_sidecar|all> <frames.jsonl>...
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const { baselineRoot } = await import('./common.mjs');
  const [direction, ...files] = process.argv.slice(2);
  const validator = loadValidator(baselineRoot());
  const stats = validateFrames(validator, files, { direction });
  console.log(`ajv ${validator.version}: ${stats.frames} frames, ${stats.valid} valid, ${stats.invalid} invalid`);
  for (const [key, entry] of stats.failures) console.log(`${entry.count}x ${key}\n   e.g. ${JSON.stringify(entry.sample).slice(0, 300)}`);
  for (const [kind, count] of Object.entries(stats.byKind)) console.log(`  ${kind}: ${count.total}${count.invalid ? ` (${count.invalid} invalid)` : ''}`);
  process.exitCode = stats.invalid ? 1 : 0;
}
