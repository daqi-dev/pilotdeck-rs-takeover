import { readFileSync } from 'node:fs';

export function transformQueueRetry(source) {
    const anchor = `export async function resumeInputQueueViaGateway(sessionId, writer, provider = 'pilotdeck') {
    const state = sessionState.get(sessionId);
    if (!state) return { ok: false, error: 'Session queue was not found.' };
    return resumeInputQueueState(state, writer, provider);
}`;
    const helper = readFileSync(new URL('./queue-retry.mjs', import.meta.url), 'utf8')
        .replace("import { randomUUID } from 'node:crypto';\n", '')
        .replace('export async function resumeQueueWithSafeRetry(', 'async function resumeQueueWithSafeRetry(');
    if (source.split(anchor).length !== 2) throw Error('Queue resume source anchor mismatch');
    return `const resumeQueueWithSafeRetry = (() => {\n${helper}\nreturn resumeQueueWithSafeRetry;\n})();\n` + source.replace(anchor, `export async function resumeInputQueueViaGateway(sessionId, writer, provider = 'pilotdeck') {
    const state = sessionState.get(sessionId);
    if (!state) return { ok: false, error: 'Session queue was not found.' };
    let gateway;
    try { gateway = await ensureGateway(); } catch { return { ok: false, error: 'PilotDeck is disconnected.' }; }
    return resumeQueueWithSafeRetry({
        state, gateway,
        persist() {
            if (!queueSidecarPath(state)) throw Error('Queue persistence is unavailable.');
            state.queueRevision += 1;
            persistQueueState(state, true);
        },
        resume: () => resumeInputQueueState(state, writer, provider),
    });
}`);
}
