import assert from 'node:assert/strict';
import test from 'node:test';
import { readFile } from 'node:fs/promises';
import { assertAuthorizedSourceIncrement } from '../../products/pilotdeck-staffdeck-sop/conformance/authorized-source-increment.mjs';

test('approved source schema compares the full request without transforming baseline', async (t) => {
  const fixture = process.env.PILOTDECK_SOURCE_INCREMENT_FIXTURE;
  if (!fixture) return t.skip('set PILOTDECK_SOURCE_INCREMENT_FIXTURE to the original full request evidence');
  const { expected, actual } = JSON.parse(await readFile(fixture, 'utf8'));
  const tracePath = process.env.PILOTDECK_SOURCE_INCREMENT_TRACE;
  assert.ok(tracePath, 'source authority must come from the original pre-compaction trace');
  const trace = JSON.parse(await readFile(tracePath, 'utf8'));
  const automatic = trace.compaction.automatic;
  const attempt = automatic.attempts.find(row => row.summaryRequest.maxOutputTokens === expected.maxOutputTokens);
  function restore(value) {
    if (typeof value === 'string') return value.replaceAll('<workspace>', automatic.replayWorkspacePath);
    if (Array.isArray(value)) return value.map(restore);
    if (!value || typeof value !== 'object') return value;
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, restore(item)]));
  }
  const sourceMessages = restore(attempt.trigger.input.messages);
  const result = assertAuthorizedSourceIncrement(expected, actual, sourceMessages);
  assert.equal(result.schema, 'e2e01-approved-source-anchors/v2');
  assert.equal(result.strictB0Equality, true);
  assert.ok(result.relationships.some(row => row.toolName === 'knowledge_query'));
  for (const mutate of [
    (value) => { value.maxOutputTokens += 1; },
    (value) => { value.systemPrompt += ' extra'; },
    (value) => { value.messages[0].content[0].text += ' changed'; },
    (value) => { value.messages.at(-1).content[0].text = value.messages.at(-1).content[0].text.replace('knowledge_query', 'wrong_tool'); },
    (value) => { value.messages.at(-1).content[0].text = value.messages.at(-1).content[0].text.replace(/kdoc_[A-Za-z0-9]+/, 'kdoc_wrong'); },
    (value) => { value.tools = []; },
  ]) {
    const altered = structuredClone(expected);
    mutate(altered);
    assert.throws(() => assertAuthorizedSourceIncrement(altered, actual, sourceMessages));
  }
});
