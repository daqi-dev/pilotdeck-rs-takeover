import { randomUUID } from 'node:crypto';

const blocked = () => ({ ok: false, error: 'The queue remains paused because the previous delivery could not be safely verified. Refresh and try again.' });
const integer = value => Number.isSafeInteger(value) && value >= 0;
const recordIdentity = record => JSON.stringify([record.sessionKey, record.projectKey ?? '', record.runId, record.state, record.revision, record.lastSeq, record.acceptedAt, record.updatedAt]);
const manifestIdentity = manifest => JSON.stringify([manifest.schemaVersion, manifest.format, manifest.sessionKey, manifest.entryCount, manifest.firstSequence ?? null, manifest.lastSequence ?? null, manifest.subagentCount, manifest.toolResultReferenceCount]);

// Called only by the existing explicit resume-input-queue action. It does not
// change generic Gateway run deduplication or edit any durable run record.
export async function resumeQueueWithSafeRetry({ state, gateway, persist, resume, uuid = randomUUID }) {
    if (!state?.inputQueue?.length) return resume();
    if (state.queueRetryPreparation || state.queuePaused !== true || state.active || state.queueDispatching || state.deleted || state.deleting) return blocked();
    const item = state.inputQueue[0];
    // Unknown delivery retains its original idempotency key and existing
    // reconciliation path. It never qualifies for a fresh attempt here.
    if (item.status === 'delivery_uncertain' || item.deliveryRunId) return blocked();
    const runId = item.runId || item.id;
    const fingerprint = JSON.stringify(item);
    const revision = state.queueRevision;
    const activityRevision = state.activityRevision;
    const projectKey = state.projectKey;
    const sessionKey = state.sessionKey;
    const current = () => state.inputQueue[0] === item && JSON.stringify(item) === fingerprint
        && state.queueRevision === revision && state.activityRevision === activityRevision
        && state.projectKey === projectKey && state.sessionKey === sessionKey && state.queuePaused === true
        && !state.active && !state.queueDispatching && !state.deleted && !state.deleting;
    state.queueRetryPreparation = true;
    try {
        if (!gateway?.runGet || !gateway.getActiveTurnSnapshot) return blocked();
        const reference = { sessionKey: state.sessionKey, projectKey, runId };
        const inactive = async () => {
            const snapshot = await gateway.getActiveTurnSnapshot({ sessionKey: state.sessionKey, includeEvents: false });
            return snapshot?.active === false && current();
        };
        if (!await inactive()) return blocked();
        const record = await gateway.runGet(reference);
        if (!current()) return blocked();
        // Authoritative absence is an ordinary, never-registered queued send.
        // Keep its original runId: any racing acceptance remains idempotent.
        if (record === undefined || record === null) {
            if (item.status !== 'queued' || !await inactive()) return blocked();
            return resume();
        }
        if (record.sessionKey !== reference.sessionKey || record.runId !== runId
            || (record.projectKey ?? '') !== (projectKey ?? '') || record.state !== 'failed'
            || !integer(record.revision) || record.revision < 1 || !integer(record.lastSeq) || record.lastSeq < 1
            || record.result?.type !== 'error' || record.result.code !== 'gateway_submit_failed'
            || !gateway.runEvents || !gateway.nativeArchiveManifest || !gateway.nativeArchiveEntries) return blocked();

        let afterSeq = 0;
        // Public run_events defaults to 500 and has no complete flag. Only a
        // contiguous read through the stable record.lastSeq proves absence.
        while (afterSeq < record.lastSeq) {
            const page = await gateway.runEvents({ ...reference, afterSeq, limit: 500 });
            if (!current() || page?.gap === true || !Array.isArray(page?.events) || !page.events.length || page.events.length > 500) return blocked();
            for (const row of page.events) {
                if (row?.seq !== afterSeq + 1 || row.seq > record.lastSeq) return blocked();
                const event = row.event;
                if (!event || !['error', 'agent_status'].includes(event.type)
                    || (event.runId !== undefined && event.runId !== runId)
                    || (event.sessionKey !== undefined && event.sessionKey !== state.sessionKey)) return blocked();
                afterSeq = row.seq;
            }
            if (page.nextSeq !== undefined && page.nextSeq !== afterSeq) return blocked();
        }

        const archiveRef = { sessionKey: state.sessionKey, projectKey };
        const manifest = await gateway.nativeArchiveManifest(archiveRef);
        if (!current() || manifest?.schemaVersion !== 1 || manifest.format !== 'native_transcript_entries'
            || manifest.sessionKey !== state.sessionKey || !integer(manifest.entryCount)
            || (manifest.entryCount > 0 && (!integer(manifest.firstSequence) || manifest.firstSequence < 1
                || !integer(manifest.lastSequence) || manifest.lastSequence < manifest.firstSequence))) return blocked();
        let afterSequence = 0, count = 0, firstSequence;
        for (;;) {
            const page = await gateway.nativeArchiveEntries({ ...archiveRef, afterSequence, limit: 500 });
            if (!current() || !Array.isArray(page?.entries) || page.entries.length > 500 || typeof page.complete !== 'boolean') return blocked();
            for (const entry of page.entries) {
                // Native fork logs legally have sequence gaps. Require strict
                // progress and manifest coverage, never consecutive sequences.
                if (!entry || entry.sessionId !== state.sessionKey || !integer(entry.sequence) || entry.sequence <= afterSequence
                    || (entry.turnId === runId && entry.type !== 'agent_status_message')) return blocked();
                firstSequence ??= entry.sequence;
                afterSequence = entry.sequence;
                count++;
                if (count > manifest.entryCount) return blocked();
            }
            if (page.complete) {
                if (page.nextSequence !== undefined || count !== manifest.entryCount
                    || (count > 0 && (firstSequence !== manifest.firstSequence || afterSequence !== manifest.lastSequence))) return blocked();
                break;
            }
            if (!page.entries.length || page.nextSequence !== afterSequence || count >= manifest.entryCount) return blocked();
        }
        const manifestAfter = await gateway.nativeArchiveManifest(archiveRef);
        const recordAfter = await gateway.runGet(reference);
        if (!current() || !recordAfter || manifestIdentity(manifestAfter) !== manifestIdentity(manifest)
            || recordIdentity(recordAfter) !== recordIdentity(record) || !await inactive()) return blocked();

        const nextRunId = uuid();
        if (typeof nextRunId !== 'string' || !/^[A-Za-z0-9._:-]{1,128}$/.test(nextRunId) || nextRunId === runId) return blocked();
        const hadRunId = Object.hasOwn(item, 'runId');
        const oldRunId = item.runId;
        item.runId = nextRunId;
        try {
            // Synchronous, strict disk commit while still paused. The caller
            // must not emit or dispatch before this atomic sidecar write.
            persist();
        } catch {
            if (hadRunId) item.runId = oldRunId;
            else delete item.runId;
            state.queueRevision = revision;
            state.queuePaused = true;
            return blocked();
        }
        // No await between durable identity commit and the existing resume.
        return resume();
    } catch {
        return blocked();
    } finally {
        state.queueRetryPreparation = false;
    }
}
