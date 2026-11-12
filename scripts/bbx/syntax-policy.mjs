// T1-SYNTAX-NATIVE-001: an explicit, value-checked exception for eight fixtures.
// No path-only exemption: every accepted byte/count is derived from validated
// diagnostic text. Raw differences remain in the comparator report.
import { isDeepStrictEqual } from 'node:util';

const insist = (condition, message) => { if (!condition) throw Error(`syntax policy: ${message}`); };
const toolText = (artifact, rule, id) => {
  const requests = artifact.provider?.[rule];
  insist(requests?.length === 1, `${rule} must have one real request`);
  const messages = requests[0].body?.messages?.filter((m) => m.role === 'tool' && m.tool_call_id === id);
  insist(messages?.length === 1 && typeof messages[0].content === 'string', `${id} result missing`);
  return messages[0].content;
};

export function syntaxContract(artifact, config, side) {
  insist(['write', 'edit'].includes(config.operation) && ['js', 'ts', 'html', 'csv'].includes(config.extension), 'unsupported fixture');
  const { extension: ext, operation } = config;
  insist(artifact.scenario === `t1-diagnostics-${operation}-${ext}`, 'fixture identity mismatch');
  const verb = operation === 'write' ? 'Created' : 'Updated';
  const suffix = operation === 'edit' ? ' (1 replacement)' : '';
  const bad = toolText(artifact, 'verified-bad', 'call_bad');
  const good = toolText(artifact, 'verified-good', 'call_good');
  const prefix = `${verb} bad.${ext}${suffix}.\n\nSyntax issues detected:\n`;
  insist(bad.startsWith(prefix), 'bad result has no diagnostic block');
  const diagnostic = bad.slice(prefix.length);
  const match = /^- L([1-9]\d*):([1-9]\d*) error: (\S[^\r\n]*)$/.exec(diagnostic);
  insist(match && match[3].length <= 500, 'diagnostic severity/location/message malformed');
  insist(isDeepStrictEqual([Number(match[1]), Number(match[2])], config.locations[side]), 'diagnostic fixture position changed');
  insist(good === `${verb} good.${ext}${suffix}.`, 'good result must have no diagnostic or extra output');
  insist(artifact.tree?.[`/proj/bad.${ext}`]?.text === config.badContent, 'bad file bytes changed');
  insist(artifact.tree?.[`/proj/good.${ext}`]?.text === config.goodContent, 'good file bytes changed');
  const events = artifact.steps?.[1]?.frames?.map((f) => f.event).filter(Boolean) ?? [];
  const finished = events.filter((e) => e.type === 'tool_call_finished' && e.toolCallId === 'call_bad');
  insist(finished.length === 1 && finished[0].ok === true && finished[0].resultPreview === bad
    && finished[0].resultBytes === Buffer.byteLength(bad), 'successful tool event or byte count invalid');
  const detail = events.filter((e) => e.type === 'tool_result_detail_available' && e.toolCallId === 'call_bad');
  insist(detail.length === 1 && detail[0].fullText === bad, 'full diagnostic detail missing');
  const budget = events.filter((e) => e.type === 'context_budget').at(-1);
  insist(budget && budget.runId === finished[0].runId, 'post-tool context budget missing');
  return { bad, good, budget, runId: finished[0].runId };
}

const calibrated = (budget) => {
  const local = budget.localEstimateTokens;
  insist(Number.isInteger(local) && local > 0 && budget.source === 'calibrated' && budget.exact === false
    && budget.estimateSource === 'usage', 'unexpected context estimator');
  const cap = Math.max(1, Math.floor(local * 0.5));
  const correction = budget.calibrationActualInputTokens - budget.calibrationEstimatedInputTokens;
  insist(Number.isFinite(correction), 'calibration inputs missing');
  return Math.max(1, Math.round(local + Math.max(-cap, Math.min(cap, correction))));
};

export function syntaxAcceptedPaths(a, b, config, countTokens, sides = ['ts', 'rust'], raw = [a, b]) {
  const left = syntaxContract(a, config, sides[0]), right = syntaxContract(b, config, sides[1]);
  const accepted = new Map();
  const add = (at, reason) => accepted.set(at, reason);
  const delta = countTokens(right.bad) - countTokens(left.bad);
  const contexts = [left.budget, right.budget];
  for (const budget of contexts) {
    const used = calibrated(budget), breakdown = budget.breakdown;
    insist(budget.used === used && budget.displayUsed === used && budget.usageTokens === used
      && budget.ratio === used / budget.effectiveTotal, 'context usage is not derived from calibration');
    insist(breakdown?.total === budget.localEstimateTokens
      && ['system', 'tools', 'messages', 'mcp', 'memory'].reduce((sum, k) => sum + breakdown[k], 0) === breakdown.total,
    'token breakdown does not add up');
  }
  insist(right.budget.localEstimateTokens - left.budget.localEstimateTokens === delta
    && right.budget.breakdown.messages - left.budget.breakdown.messages === delta, 'token change is not diagnostic-derived');
  for (const key of ['system', 'tools', 'mcp', 'memory']) insist(left.budget.breakdown[key] === right.budget.breakdown[key], `unrelated ${key} tokens changed`);
  for (const key of ['calibrationActualInputTokens', 'calibrationEstimatedInputTokens', 'effectiveTotal']) insist(left.budget[key] === right.budget[key], `unrelated ${key} changed`);

  // JSON request length is verified against each raw serialized body before
  // matching its diagnostic substitution. All other headers/body values compare.
  for (const artifact of raw) {
    const request = artifact.provider['verified-bad'][0];
    insist(request.headers['content-length'] === String(Buffer.byteLength(JSON.stringify(request.body))), 'provider content-length invalid');
  }
  const requestA = a.provider['verified-bad'][0], requestB = b.provider['verified-bad'][0];
  const replaceText = (v, from, to) => typeof v === 'string' ? (v === from ? to : v)
    : Array.isArray(v) ? v.map((x) => replaceText(x, from, to))
      : v && typeof v === 'object' ? Object.fromEntries(Object.entries(v).map(([k, x]) => [k, replaceText(x, from, to)])) : v;
  insist(isDeepStrictEqual(replaceText(requestB.body, right.bad, left.bad), requestA.body), 'unrelated provider payload changed');
  add('$.provider.verified-bad[0].headers.content-length', 'recomputed JSON request byte length');

  const walk = (x, y, at) => {
    if (x === y) return;
    if (typeof x === 'string' && x === left.bad && y === right.bad) {
      add(at, 'validated diagnostic wording/registered fixture position'); return;
    }
    if (!x || !y || typeof x !== 'object' || typeof y !== 'object' || Array.isArray(x) !== Array.isArray(y)) return;
    if (x.type === 'tool_call_finished' && y.type === x.type && x.toolCallId === 'call_bad' && y.toolCallId === x.toolCallId
      && x.resultPreview === left.bad && y.resultPreview === right.bad
      && x.resultBytes === Buffer.byteLength(left.bad) && y.resultBytes === Buffer.byteLength(right.bad)) {
      add(`${at}.resultBytes`, 'recomputed UTF-8 diagnostic result bytes');
    }
    if (x.type === 'context_budget' && y.type === x.type && x.runId === left.runId && y.runId === right.runId
      && isDeepStrictEqual(x, left.budget) && isDeepStrictEqual(y, right.budget)) {
      for (const key of ['used', 'displayUsed', 'localEstimateTokens', 'usageTokens', 'ratio']) add(`${at}.${key}`, 'recomputed o200k token change and bounded calibration');
      for (const key of ['total', 'messages']) add(`${at}.breakdown.${key}`, 'recomputed o200k diagnostic token delta');
    }
    const projection = '.json.projections.web-history.token-usage.state.latestContextBudget.usage';
    if (at.startsWith('$.tree.') && at.includes('/chats/s-t1-bad.jsonl.projections.json') && at.endsWith(projection)) {
      for (const [value, ctx] of [[x, left.budget], [y, right.budget]]) insist(value.used === ctx.used && value.displayUsed === ctx.displayUsed
        && value.ratio === ctx.ratio && value.effectiveTotal === ctx.effectiveTotal, 'projected context budget mismatch');
      for (const key of ['used', 'displayUsed', 'ratio']) add(`${at}.${key}`, 'verified projection of recalculated context usage');
    }
    for (const key of Object.keys(x)) if (key in y) walk(x[key], y[key], Array.isArray(x) ? `${at}[${key}]` : `${at}.${key}`);
  };
  walk(a, b, '$');
  return accepted;
}
