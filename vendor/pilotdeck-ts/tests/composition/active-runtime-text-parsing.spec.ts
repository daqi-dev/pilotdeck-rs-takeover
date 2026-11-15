import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { createActiveRuntimeTextParsingPort } from '../../src/composition/activeRuntimeTextParsing.js';

test('selected file port extracts the valid PDF that succeeds on the domain owner', async () => {
  const bytes = await readFile('tests/composition/fixtures/valid-knowledge.pdf');
  const parse = createActiveRuntimeTextParsingPort();
  const result = await parse({ filename: 'knowledge.PDF', bytes });
  assert.match(result.text, /Valid Knowledge PDF fact G4-PDF-HERON-8371/);
  assert.equal(result.mediaType, 'application/pdf');
  await assert.rejects(parse({ filename: 'broken.pdf', bytes: Buffer.from('not a PDF') }), { code: 'PUBLIC_FILE_PARSE_FAILED', status: 422 });
  await assert.rejects(parse({ filename: 'knowledge.pdf', bytes, maxBytes: 1 }), { code: 'PUBLIC_FILE_TOO_LARGE', status: 413 });
  await assert.rejects(parse({ filename: 'knowledge.pdf', bytes, signal: AbortSignal.abort() }), { code: 'PUBLIC_HOST_CANCELLED', status: 499 });
  assert.equal((await parse({ filename: 'fact.md', bytes: Buffer.from('original text') })).text, 'original text');
});
