import { test, expect, vi } from 'vitest';
import { readFile, writeFile, mkdir, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { resumeQueueWithSafeRetry } from './queue-retry.mjs';
const mock = { gateway: null };
const baselineRoot = process.env.PILOTDECK_WEB_BASELINE_ROOT;
if (!baselineRoot?.startsWith('/')) throw Error('Explicit fixed baseline root required');
vi.doMock(`${baselineRoot}/ui/server/services/gatewayConnectionCache.js`, () => ({ createGatewayConnectionCache: () => ({ get: async () => mock.gateway, invalidate() {} }) }));
const sid = 'web:s_proof', projectKey = '/synthetic-project';
const failedRecord = { sessionKey: sid, projectKey, runId: 'old-attempt', state: 'failed', revision: 3, lastSeq: 2, acceptedAt: 'synthetic-start', updatedAt: 'synthetic-end', result: { type: 'error', runId: 'old-attempt', code: 'gateway_submit_failed' } };
const rows = [{ seq: 1, event: failedRecord.result }, { seq: 2, event: { type: 'agent_status', runId: 'old-attempt' } }];
function fixture() {
  const state = { sessionKey: sid, projectKey, activityRevision: 0, queueRevision: 4, queuePaused: true, queuePauseReason: 'previous_turn_failed', active: false, inputQueue: [{ id: 'item-stable', runId: 'old-attempt', command: 'keep body', displayText: 'keep body', status: 'failed', options: {} }, { id: 'second', runId: 'second', command: 'later', status: 'queued', options: {} }], acceptedInputIds: new Set(['item-stable', 'second']) };
  const entries = [{ sessionId: sid, turnId: 'source-turn', sequence: 1, type: 'accepted_input' }, { sessionId: sid, turnId: 'source-turn', sequence: 19, type: 'turn_result' }];
  const manifest = { schemaVersion: 1, format: 'native_transcript_entries', sessionKey: sid, entryCount: 2, firstSequence: 1, lastSequence: 19, subagentCount: 0, toolResultReferenceCount: 0 };
  const gateway = { getActiveTurnSnapshot: vi.fn(async () => ({ active: false })), runGet: vi.fn(async () => structuredClone(failedRecord)), runEvents: vi.fn(async () => ({ events: structuredClone(rows), nextSeq: 2 })), nativeArchiveManifest: vi.fn(async () => structuredClone(manifest)), nativeArchiveEntries: vi.fn(async () => ({ entries: structuredClone(entries), complete: true })) };
  const receipts = [];
  return { state, entries, manifest, gateway, receipts, uuid: vi.fn(() => 'fresh-attempt'), persist: vi.fn(() => { expect(state.queuePaused).toBe(true); receipts.push({ runId: state.inputQueue[0].runId, ids: state.inputQueue.map(i => i.id), commands: state.inputQueue.map(i => i.command) }); state.queueRevision++; }), resume: vi.fn(() => { expect(receipts).toHaveLength(1); expect(receipts[0].runId).toBe('fresh-attempt'); return { ok: true }; }) };
}
const run = f => resumeQueueWithSafeRetry(f);
test('qualified explicit Continue persists new attempt before resume and preserves queue identity/order/body', async () => { const f = fixture(); expect(await run(f)).toEqual({ ok: true }); expect(f.state.inputQueue.map(i => i.id)).toEqual(['item-stable', 'second']); expect(f.state.inputQueue.map(i => i.command)).toEqual(['keep body', 'later']); expect([...f.state.acceptedInputIds]).toEqual(['item-stable', 'second']); expect(f.state.inputQueue[0].runId).toBe('fresh-attempt'); expect(f.persist).toHaveBeenCalledOnce(); expect(f.resume).toHaveBeenCalledOnce(); });
const negatives = [
 ['active authority', f => { f.gateway.getActiveTurnSnapshot.mockResolvedValue({ active: true, runId: 'other' }); }],
 ['unknown authority', f => { f.gateway.getActiveTurnSnapshot.mockResolvedValue({}); }],
 ['accepted run', f => { f.gateway.runGet.mockResolvedValue({ ...failedRecord, state: 'running' }); }],
 ['registry input accepted', f => { f.gateway.runEvents.mockResolvedValue({ events: [{ seq: 1, event: { type: 'input_accepted', runId: 'old-attempt' } }, rows[1]] }); }],
 ['registry sequence gap', f => { f.gateway.runEvents.mockResolvedValue({ events: [rows[1]] }); }],
 ['registry page gap flag', f => { f.gateway.runEvents.mockResolvedValue({ events: rows, gap: true }); }],
 ['native accepted input for attempt', f => { f.entries[0].turnId = 'old-attempt'; }],
 ['native incomplete empty page', f => { f.gateway.nativeArchiveEntries.mockResolvedValue({ entries: [], complete: false, nextSequence: 0 }); }],
 ['registry changes after proof', f => { f.gateway.runGet.mockResolvedValueOnce(failedRecord).mockResolvedValue({ ...failedRecord, revision: 4 }); }],
 ['native manifest changes', f => { f.gateway.nativeArchiveManifest.mockResolvedValueOnce(f.manifest).mockResolvedValue({ ...f.manifest, entryCount: 3 }); }],
 ['local activity changed', f => { f.gateway.nativeArchiveEntries.mockImplementation(async () => { f.state.activityRevision++; return { entries: f.entries, complete: true }; }); }],
 ['queue reordered during proof', f => { f.gateway.nativeArchiveEntries.mockImplementation(async () => { f.state.inputQueue.reverse(); return { entries: f.entries, complete: true }; }); }],
 ['unknown delivery', f => { f.state.inputQueue[0].status = 'delivery_uncertain'; }],
 ['missing evidence API', f => { delete f.gateway.nativeArchiveEntries; }],
 ['failed registry query', f => { f.gateway.runGet.mockRejectedValue(Error('synthetic')); }],
];
for (const [name, mutate] of negatives) test(`${name} keeps queue paused without fresh identity or dispatch`, async () => { const f = fixture(); mutate(f); expect((await run(f)).ok).toBe(false); expect(f.state.queuePaused).toBe(true); expect(f.state.inputQueue.find(i => i.id === 'item-stable').runId).toBe('old-attempt'); expect(f.uuid).not.toHaveBeenCalled(); expect(f.persist).not.toHaveBeenCalled(); expect(f.resume).not.toHaveBeenCalled(); });
test('strict persistence failure restores in-memory old identity and never resumes', async () => { const f = fixture(); f.persist.mockImplementation(() => { f.state.queueRevision++; throw Error('disk denied'); }); expect((await run(f)).ok).toBe(false); expect(f.state.inputQueue[0].runId).toBe('old-attempt'); expect(f.state.queueRevision).toBe(4); expect(f.state.queuePaused).toBe(true); expect(f.resume).not.toHaveBeenCalled(); });
test('concurrent Continue allocates exactly one fresh attempt', async () => { const f = fixture(); let release; const wait = new Promise(r => release = r); f.gateway.runGet.mockImplementationOnce(async () => { await wait; return failedRecord; }); const first = run(f); await vi.waitFor(() => expect(f.gateway.runGet).toHaveBeenCalledOnce()); expect((await run(f)).ok).toBe(false); release(); expect((await first).ok).toBe(true); expect(f.uuid).toHaveBeenCalledOnce(); expect(f.persist).toHaveBeenCalledOnce(); expect(f.resume).toHaveBeenCalledOnce(); });
test('full registry paging and native sequence gaps qualify', async () => { const f = fixture(); const record = { ...failedRecord, lastSeq: 502 }; f.gateway.runGet.mockResolvedValue(record); f.gateway.runEvents.mockImplementation(async ({ afterSeq }) => ({ events: Array.from({ length: Math.min(500, 502 - afterSeq) }, (_, n) => ({ seq: afterSeq + n + 1, event: { type: 'agent_status', runId: 'old-attempt' } })) })); expect((await run(f)).ok).toBe(true); expect(f.gateway.runEvents).toHaveBeenCalledTimes(2); expect(f.gateway.runEvents.mock.calls[1][0].afterSeq).toBe(500); });
test('authoritatively unregistered ordinary queued item resumes with same runId', async () => { const f = fixture(); f.state.inputQueue[0].status = 'queued'; f.gateway.runGet.mockResolvedValue(undefined); f.resume.mockImplementation(() => ({ ok: true })); expect((await run(f)).ok).toBe(true); expect(f.state.inputQueue[0].runId).toBe('old-attempt'); expect(f.uuid).not.toHaveBeenCalled(); expect(f.persist).not.toHaveBeenCalled(); });

test('candidate actual baseline queue drains repaired failed first item then second, with identity durable before dispatch', async () => {
  const output = process.env.ORACLE_OUTPUT;
  const { enqueueInputViaGateway, getInputQueueStateViaGateway, resumeInputQueueViaGateway } = await import(`${baselineRoot}/ui/server/pilotdeck-bridge.js`);
  const sessionId = 'web:s_actual_candidate', projectPath = join(output, 'project');
  await mkdir(projectPath, { recursive: true });
  const calls = [], frames = [], registry = new Map(); let repaired = false;
  let sidecar;
  async function findSidecar(directory) { for (const entry of await readdir(directory, { withFileTypes: true })) { const next = join(directory, entry.name); if (entry.isDirectory()) { const found = await findSidecar(next); if (found) return found; } else if (entry.name === sessionId + '.json') return next; } }
  const record = { ...failedRecord, sessionKey: sessionId, projectKey: projectPath, runId: 'original-item' };
  record.result = { ...record.result, runId: record.runId };
  const native = [{ sessionId, turnId: 'source', sequence: 1, type: 'accepted_input' }, { sessionId, turnId: 'source', sequence: 19, type: 'turn_result' }];
  const gateway = {
    getActiveTurnSnapshot: async () => ({ active: false, events: [] }),
    readSessionMessages: async () => ({ messages: [], total: 0 }),
    runGet: async ({ runId }) => runId === record.runId ? structuredClone(record) : undefined,
    runEvents: async () => ({ events: [{ seq: 1, event: record.result }, { seq: 2, event: { type: 'agent_status', runId: record.runId } }] }),
    nativeArchiveManifest: async () => ({ ...fixture().manifest, sessionKey: sessionId }),
    nativeArchiveEntries: async () => ({ entries: native, complete: true }),
    async *submitTurn(input) {
      calls.push(input.runId);
      if (registry.has(input.runId)) { yield* structuredClone(registry.get(input.runId)); return; }
      if (!repaired) { registry.set(input.runId, [record.result]); yield record.result; return; }
      if (input.message === 'QUEUE_ONE') {
        sidecar = await findSidecar(process.env.PILOT_HOME);
        const saved = JSON.parse(await readFile(sidecar, 'utf8'));
        expect(saved.items[0].id).toBe('original-item');
        expect(saved.items[0].runId).toBe(input.runId);
        expect(saved.items[0].runId).not.toBe('original-item');
        expect(saved.items.map(i => i.command)).toEqual(['QUEUE_ONE', 'QUEUE_TWO']);
      }
      yield { type: 'input_accepted', runId: input.runId };
      yield { type: 'turn_completed', runId: input.runId, finishReason: 'completed', usage: {} };
    },
  };
  mock.gateway = gateway;
  const writer = { send: frame => frames.push(structuredClone(frame)) };
  const item = (id, command) => ({ id, runId: id, command, displayText: command, createdAt: '2026-11-14T00:00:00.000Z', options: { projectPath } });
  await enqueueInputViaGateway(sessionId, item('original-item', 'QUEUE_ONE'), writer);
  await vi.waitFor(async () => expect(await getInputQueueStateViaGateway(sessionId)).toMatchObject({ paused: true, items: [{ id: 'original-item', status: 'failed' }] }));
  await enqueueInputViaGateway(sessionId, item('second-item', 'QUEUE_TWO'), writer);
  const originalFailed = structuredClone(registry.get('original-item'));
  repaired = true;
  expect((await resumeInputQueueViaGateway(sessionId, writer)).ok).toBe(true);
  await vi.waitFor(async () => expect((await getInputQueueStateViaGateway(sessionId)).items).toHaveLength(0));
  expect(calls).toHaveLength(3);
  expect(calls[0]).toBe('original-item'); expect(calls[1]).not.toBe('original-item'); expect(calls[2]).toBe('second-item');
  expect(registry.get('original-item')).toEqual(originalFailed);
  expect(frames.filter(f => f.queueItemId).map(f => f.queueItemId)).toEqual(['original-item', 'second-item']);
  const saved = JSON.parse(await readFile(sidecar, 'utf8'));
  expect(saved.acceptedInputIds).toEqual(['original-item', 'second-item']);
  await writeFile(join(output, 'actual-queue-result.json'), JSON.stringify({ passed: true, layer: 'Actual fixed-TS queue manager/runtime functions with private narrow overlay and real sidecar; mocked Gateway durable registry/history/activity, no network.', calls, originalFailedUnchanged: true, acceptedQueueItemIds: saved.acceptedInputIds, queueItemIds: frames.filter(f => f.queueItemId).map(f => f.queueItemId), finalQueue: await getInputQueueStateViaGateway(sessionId) }, null, 2) + '\n');
});
