import assert from 'node:assert/strict';

// Evidence counterpart of session/transcript/CompactSnapshot. Present corrupt
// snapshots must fail comparison, even if a legacy replacement also exists.
export function compactReplacementMessages(boundary) {
  assert.ok(record(boundary) && boundary.kind === 'compact' && boundary.subtype === 'compact_boundary', 'Not a compact boundary');
  let messages;
  if (Object.hasOwn(boundary, 'snapshot')) {
    assert.ok(record(boundary.snapshot) && boundary.snapshot.version === 1, 'Invalid compact snapshot version');
    messages = boundary.snapshot.messages;
  } else {
    messages = boundary.replacementMessages;
  }
  assert.ok(Array.isArray(messages) && messages.length > 0 && messages.every(message), 'Invalid compact replacement messages');
  return messages;
}

function record(value) { return value !== null && typeof value === 'object' && !Array.isArray(value); }
function message(value) {
  return record(value) && ['user', 'assistant'].includes(value.role)
    && (value.metadata === undefined || record(value.metadata))
    && Array.isArray(value.content) && value.content.every(block);
}
function block(value) {
  if (!record(value)) return false;
  switch (value.type) {
    case 'text': case 'thinking': return typeof value.text === 'string';
    case 'image': case 'audio': return ['base64', 'url'].includes(value.source)
      && typeof value.data === 'string' && typeof value.mimeType === 'string';
    case 'pdf': return value.source === 'base64' && typeof value.data === 'string'
      && value.mimeType === 'application/pdf' && typeof value.bytes === 'number';
    case 'tool_call': return typeof value.id === 'string' && typeof value.name === 'string';
    case 'tool_result': return typeof value.toolCallId === 'string' && Array.isArray(value.content)
      && value.content.every(item => record(item) && ['text', 'image', 'pdf'].includes(item.type) && block(item));
    case 'tool_result_reference': case 'media_reference': return typeof value.path === 'string'
      && typeof value.originalBytes === 'number' && typeof value.preview === 'string' && typeof value.hasMore === 'boolean'
      && (value.type === 'tool_result_reference' ? typeof value.toolCallId === 'string'
        : typeof value.mimeType === 'string' && ['image', 'pdf', 'audio'].includes(value.mediaType));
    default: return false;
  }
}
