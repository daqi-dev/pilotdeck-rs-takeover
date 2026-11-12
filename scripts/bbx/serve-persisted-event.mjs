// A read-only trigger for the SDK CLI, which deliberately hides partial text.
// It never opens a Gateway connection or executes a scenario-supplied hook.
import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { isDeepStrictEqual } from 'node:util';

const sha256 = value => createHash('sha256').update(value).digest('hex');
const recordObject = value => value && typeof value === 'object' && !Array.isArray(value);
export function validateTrigger(condition) {
  if (!recordObject(condition) || Object.keys(condition).sort().join(',') !== 'event,projectKey,providerRule,requestText,sessionKey'
    || ['sessionKey', 'projectKey', 'requestText', 'providerRule'].some(key => typeof condition[key] !== 'string' || !condition[key])
    || !recordObject(condition.event) || Object.keys(condition.event).sort().join(',') !== 'text,type'
    || condition.event.type !== 'assistant_text_delta' || typeof condition.event.text !== 'string' || !condition.event.text) {
    throw Error('untilPersistedRunEvent requires exact session/project/request/provider and assistant_text_delta text');
  }
}

export function trackProviderResponse(response, body, rule, index) {
  const state = { rule, index, body, open: true, flushedChunks: 0 };
  response.once('close', () => { state.open = false; });
  response.once('finish', () => { state.open = false; });
  return {
    flushed(error) { if (!error) state.flushedChunks++; },
    snapshot() { return { ...state, open: state.open && !response.destroyed && !response.writableEnded }; },
  };
}
function requestText(body) {
  const message = Array.isArray(body?.messages) ? body.messages.at(-1) : undefined;
  if (message?.role !== 'user') return undefined;
  if (typeof message.content === 'string') return message.content;
  if (Array.isArray(message.content) && message.content.every(part => part?.type === 'text' && typeof part.text === 'string')) {
    return message.content.map(part => part.text).join('');
  }
}
export function matchPersistedRunEvent(registry, condition, providers) {
  validateTrigger(condition);
  if (registry?.schemaVersion !== 1 || !Array.isArray(registry.records) || !recordObject(registry.events)) return null;
  const records = registry.records.filter(record => {
    if (record?.sessionKey !== condition.sessionKey || record.projectKey !== condition.projectKey) return false;
    try {
      const request = JSON.parse(record.requestMaterial);
      return request.sessionKey === condition.sessionKey && request.projectKey === condition.projectKey && request.message === condition.requestText;
    } catch { return false; }
  });
  if (records.length !== 1) return null;
  const record = records[0];
  if (record.state !== 'running' || typeof record.runId !== 'string' || !record.runId) return null;
  const events = registry.events[`${condition.projectKey}\0${condition.sessionKey}\0${record.runId}`];
  if (!Array.isArray(events)) return null;
  const event = events.find(frame => Number.isInteger(frame?.seq) && frame.seq > 0 && frame.event?.runId === record.runId
    && frame.event.type === condition.event.type && frame.event.text === condition.event.text);
  if (!event) return null;
  // A historical request with the same marker cannot stand in for the live
  // current provider response. The exact final user input must also match.
  const matching = providers.filter(provider => provider.rule === condition.providerRule && requestText(provider.body) === condition.requestText);
  if (matching.length !== 1) return null;
  const provider = matching[0];
  if (!provider.open || !Number.isInteger(provider.flushedChunks) || provider.flushedChunks < 1 || !Number.isInteger(provider.index) || provider.index < 0) return null;
  return structuredClone({ record, event, provider: { rule: provider.rule, index: provider.index, open: true, flushedChunks: provider.flushedChunks } });
}
export function observePersistedRunEvent(pilotHome, condition, providers) {
  const registryPath = path.join(pilotHome, 'runs', 'registry.json');
  let bytes, registry;
  try { bytes = fs.readFileSync(registryPath); registry = JSON.parse(bytes); }
  catch (error) { if (error.code === 'ENOENT' || error instanceof SyntaxError) return null; throw error; }
  const matched = matchPersistedRunEvent(registry, condition, providers);
  if (!matched) return null;
  return { matched, raw: { registryPath, base64: bytes.toString('base64'), sha256: sha256(bytes) } };
}

// Called only for scenarios using this trigger; the original evidence path
// for every other scenario remains unchanged.
export function validatePersistedRunTriggers(artifact, scenario) {
  const errors = [], proofs = artifact.evidence?.serveTriggers ?? [];
  const saved = {};
  let proofIndex = 0;
  for (const [index, step] of scenario.steps.entries()) {
    const observed = artifact.steps?.[index];
    if (step.untilPersistedRunEvent) {
      const proof = proofs[proofIndex++];
      try {
        if (!proof || proof.stepIndex !== index || !observed?.serve?.reached || !proof.signalSent) throw Error('missing successful trigger/signal evidence');
        const condition = proof.condition;
        validateTrigger(condition);
        const resolve = value => typeof value === 'string'
          ? value.replaceAll('{PROJECT}', proof.projectKey).replace(/\{saved\.([A-Za-z0-9_]+)\}/g, (all, key) => saved[key] ?? all)
          : Object.fromEntries(Object.entries(value).map(([key, item]) => [key, resolve(item)]));
        const expected = resolve(step.untilPersistedRunEvent);
        if (!isDeepStrictEqual(condition, expected) || condition.projectKey !== proof.projectKey) throw Error('trigger condition mismatch');
        const bytes = Buffer.from(proof.raw.base64, 'base64');
        if (bytes.toString('base64') !== proof.raw.base64 || sha256(bytes) !== proof.raw.sha256) throw Error('registry raw hash mismatch');
        if (!Array.isArray(proof.providers)) throw Error('missing provider snapshot');
        const seen = new Set();
        const providers = proof.providers.map(provider => {
          const key = `${provider.rule}\0${provider.index}`;
          if (seen.has(key)) throw Error('duplicate provider snapshot index');
          seen.add(key);
          const recorded = artifact.provider?.[provider.rule]?.[provider.index];
          if (!recorded) throw Error('missing recorded provider request');
          return { ...provider, body: recorded.body };
        });
        // Requests are recorded in index order. The snapshot must contain every
        // earlier request for each rule, including closed historical streams.
        for (const provider of providers) for (let i = 0; i < provider.index; i++) {
          if (!seen.has(`${provider.rule}\0${i}`)) throw Error('incomplete provider snapshot');
        }
        const matched = matchPersistedRunEvent(JSON.parse(bytes), condition, providers);
        if (!matched || !isDeepStrictEqual(matched, observed.serve.persistedRunEvent)) throw Error('raw registry does not reproduce trigger projection');
        if (!isDeepStrictEqual(proof.provider, matched.provider)) throw Error('provider projection mismatch');
        if (proof.signalSent !== (step.signal ?? 'SIGTERM')) throw Error('trigger signal mismatch');
        const process = proof.process;
        if (!Number.isSafeInteger(proof.pid) || proof.pid <= 0 || proof.pid !== process?.pid
          || proof.startedAt !== process?.startedAt || proof.signalSentAt !== process?.signalSentAt
          || !Number.isFinite(Date.parse(proof.startedAt)) || !Number.isFinite(Date.parse(proof.signalSentAt))
          || Date.parse(proof.signalSentAt) < Date.parse(proof.startedAt)) throw Error('trigger PID/time mismatch');
      } catch (error) { errors.push(`steps[${index}]:persisted trigger: ${error.message}`); }
    }
    if (step.save && observed) {
      const { step: _step, ...result } = observed;
      for (const [key, pattern] of Object.entries(step.save)) {
        const match = new RegExp(pattern).exec(JSON.stringify(result));
        if (match) saved[key] = match[1] ?? match[0];
      }
    }
  }
  if (proofIndex !== proofs.length) errors.push('unexpected persisted trigger evidence');
  return errors;
}
