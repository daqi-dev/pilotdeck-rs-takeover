import assert from 'node:assert/strict';

export const SOURCE_INCREMENT_SCHEMA = 'e2e01-approved-source-anchors/v2';

function sourceRelationships(messages) {
  const blocks = messages.flatMap(message => message.content);
  const relationships = [];
  for (const call of blocks) {
    if (call.type !== 'tool_call' || !/skill|file|knowledge/i.test(call.name)) continue;
    const input = JSON.stringify(call.input);
    const bounded = input.length <= 2000 ? input : input.slice(0, 1976) + '\n...[truncated]';
    const values = [bounded];
    for (const result of blocks) {
      if (result.toolCallId !== call.id) continue;
      if (result.type === 'tool_result_reference') {
        values.push(result.path, result.readFilePath ?? '', result.preview);
      } else if (result.type === 'tool_result') {
        // The E2E-01 fixture uses textual owner responses, not media summaries.
        assert.ok(result.content.every(block => block.type === 'text'), 'source increment needs original textual tool results');
        values.push(result.content.map(block => block.text).join('\n'));
      }
    }
    const identifiers = [...new Set(values.join('\n').match(/\b(?:r\d{2}[-_][A-Za-z0-9_.-]+|kdoc[_-][A-Za-z0-9_.-]+|kchunk[_-][A-Za-z0-9_.-]+|(?:skill|file|knowledge|document)[-_][A-Za-z0-9_.:/-]+)\b/gi) ?? [])];
    if (identifiers.length) relationships.push({ toolName: call.name, identifiers });
  }
  return relationships.slice(0, 12);
}

/** Compare the complete canonical request without transforming either side. */
export function assertAuthorizedSourceIncrement(candidate, baseline, sourceMessages) {
  const relationships = sourceRelationships(sourceMessages);
  assert.deepEqual(candidate, baseline, 'complete canonical request differs');
  return { schema: SOURCE_INCREMENT_SCHEMA, relationships, legacyFieldsCompared: 'complete canonical request', strictB0Equality: true };
}
