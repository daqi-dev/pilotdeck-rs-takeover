/** Record an actual owner lookup; unrelated failures must not become stale-citation evidence. */
export async function observePreviousCitation(
  resolve: (input: { tenantId: string; agentId: string; chunkId: string }) => Promise<unknown>,
  input: { tenantId: string; agentId: string; chunkId: string },
) {
  try {
    const result = await resolve(input);
    if (!result || typeof result !== 'object' || (result as { id?: unknown }).id !== input.chunkId) {
      throw new Error('Old citation lookup returned an invalid owner response.');
    }
    return { rejected: false, request: input, result: structuredClone(result) as Record<string, unknown> };
  } catch (error) {
    if ((error as { code?: unknown }).code !== 'STAFFDECK_HTTP_404'
      || !(error instanceof Error) || error.message !== 'Knowledge citation not found') throw error;
    return { rejected: true, request: input, error: { code: 'STAFFDECK_HTTP_404', message: error.message } };
  }
}
