// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { staffDeckCopyClient } from './clients';
import { resolveKnowledgeCitation } from './KnowledgeCitationLink';

afterEach(() => vi.restoreAllMocks());
it('reads the exact selected source and refuses a shortened model locator', async () => {
  vi.spyOn(staffDeckCopyClient, 'call').mockResolvedValue([{ id: 'owner', tenant_id: 'tenant', copy_target: true, is_overall: false }] as never);
  const operations: string[] = [];
  vi.spyOn(globalThis, 'fetch').mockImplementation(async (_url, init) => {
    const { operation, input, scope } = JSON.parse(String(init?.body));
    expect(scope).toEqual({ kind: 'agent', agentId: 'owner' });
    operations.push(operation);
    const data = ({ list_knowledge_bases: [{ id: 'base' }], list_knowledge_documents: [{ id: 'document-exact' }],
      list_document_buckets: [{ id: 'bucket' }], list_bucket_chunks: [{ id: 'chunk-exact', content: 'Original source正文', source_ref: 'section 1' }],
    } as Record<string, unknown>)[operation];
    if (operation === 'get_knowledge_document') {
      expect(input).toEqual({ knowledgeBaseId: 'base', documentId: 'document-exact' });
      return new Response(JSON.stringify({ id: 'document-exact', title: 'Original title' }));
    }
    return new Response(JSON.stringify({ data }));
  });
  const source = await resolveKnowledgeCitation('ultrarag://knowledge/documents/document-exact');
  expect(source.document.title).toBe('Original title');
  expect(source.chunks[0].content).toBe('Original source正文');
  expect((await resolveKnowledgeCitation('ultrarag://knowledge/chunks/chunk-exact')).chunks).toHaveLength(1);
  operations.length = 0;
  await expect(resolveKnowledgeCitation('ultrarag://knowledge/documents/document')).rejects.toThrow('KNOWLEDGE_CITATION_NOT_FOUND');
  expect(operations).not.toContain('get_knowledge_document');
  await expect(resolveKnowledgeCitation('ultrarag://other/documents/document-exact')).rejects.toThrow('LOCATOR_INVALID');
});
