// 仅从固定 TS 基线读取模块；不启动产品、不访问凭据、不调用模型。
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { resolve, join } from 'node:path';
import { pathToFileURL } from 'node:url';

const [baselineArg, outputArg] = process.argv.slice(2);
if (!baselineArg || !outputArg) throw new Error('usage: node --experimental-strip-types capture-baseline.mjs <baseline> <output>');
const baseline = resolve(baselineArg);
const commit = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: baseline, encoding: 'utf8' }).trim();
if (commit !== 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85') throw new Error(`unexpected baseline ${commit}`);
const paths = [
  'src/model/response/normalizeUsage.ts',
  'src/model/response/normalizeFinishReason.ts',
  'src/model/providers/google/response.ts',
  'src/model/request/sanitizeProviderBody.ts',
];
const [usage, finish, google, request] = await Promise.all(paths.map(p => import(pathToFileURL(join(baseline, p)))));
const usageInputs = [
  null, [], {}, { input_tokens: 0, output_tokens: 0 },
  { input_tokens: 100, output_tokens: 5, cache_read_input_tokens: 20, cache_creation_input_tokens: 7 },
  { prompt_tokens: 100, input_tokens: 999, completion_tokens: 8, output_tokens: 999, prompt_tokens_details: { cached_tokens: 20, cache_write_tokens: 7 }, total_cost: 0.01 },
  { input_tokens: 100, output_tokens: 8, input_tokens_details: { cached_tokens: 20, cache_write_tokens: 7 }, estimated_cost: 0.02 },
  { prompt_tokens: 1, cache_read_input_tokens: 3, cache_creation_input_tokens: 2, total_tokens: 0, cost: 0, total_cost: 999 },
  { input_tokens: '2', output_tokens: null, nativeCost: 7 },
  { promptTokenCount: 20, candidatesTokenCount: 3, responseTokenCount: 999, thoughtsTokenCount: 4, cachedContentTokenCount: 2 },
  { promptTokenCount: 1, responseTokenCount: 2, cachedContentTokenCount: 3, totalTokenCount: 0 },
  { thoughtsTokenCount: 0 }, { cachedContentTokenCount: 0 },
];
const functions = {
  anthropic: [usage.normalizeAnthropicUsage, finish.normalizeAnthropicFinishReason],
  openai: [usage.normalizeOpenAIUsage, finish.normalizeOpenAIFinishReason],
  'openai-responses': [usage.normalizeOpenAIUsage, finish.normalizeOpenAIFinishReason],
  google: [google.normalizeGoogleUsage, google.normalizeGoogleFinishReason],
};
const usageCases = Object.entries(functions).flatMap(([protocol, [fn]]) => usageInputs.map(raw => ({ protocol, raw, expected: fn(raw) ?? null })));
const reasons = [null, 1, 'end_turn', 'stop_sequence', 'max_tokens', 'tool_use', 'refusal', 'stop', 'length', 'tool_calls', 'function_call', 'content_filter_results', 'MAX_TOKENS', 'unexpected_tool_call', 'IMAGE_SAFETY', 'unknown'];
const finishCases = Object.entries(functions).flatMap(([protocol, [, fn]]) => reasons.map(raw => ({ protocol, raw, expected: fn(raw) })));
const bodyInputs = [
  { body: { model: 'm', temperature: 0.5 }, provider: {} },
  { body: { model: 'm', temperature: 0.5, generationConfig: { temperature: 1, maxOutputTokens: 7 } }, provider: { extraBody: { model: 'override', generationConfig: { temperature: 9, candidateCount: 2 } } } },
  { body: { generationConfig: ['untouched'], text: '\ud800' }, provider: { extraBody: { temperature: 0 } } },
  { body: {}, provider: { extraBody: { generationConfig: null, stop: [] } } },
];
const bodyCases = bodyInputs.map(({body, provider}) => ({ body, provider, expected: request.sanitizeProviderBody({ ...body, ...provider.extraBody }) }));
const output = {
  sourceCommit: commit,
  sourceSha256: Object.fromEntries(paths.map(p => [p, createHash('sha256').update(readFileSync(join(baseline,p))).digest('hex')])),
  node: process.version,
  usageCases, finishCases, bodyCases,
};
writeFileSync(resolve(outputArg), JSON.stringify(output, null, 2) + '\n');
process.stdout.write(JSON.stringify({ usage: usageCases.length, finish: finishCases.length, body: bodyCases.length, sourceCommit: commit }) + '\n');
