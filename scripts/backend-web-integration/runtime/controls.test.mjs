import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';
import { identity, transformBridge, transformIndex, transformModelProbe } from './overlay.mjs';
import { load } from './loader.mjs';
import { pathToFileURL } from 'node:url';
const { createGatewayConnectionCache } = await import(new URL('./services/gatewayConnectionCache.js', pathToFileURL(identity['ui/server/pilotdeck-bridge.js'].path)).href);

const bridge = readFileSync(identity['ui/server/pilotdeck-bridge.js'].path, 'utf8');
const index = readFileSync(identity['ui/server/index.js'].path, 'utf8');
const patchedBridge = transformBridge(bridge);
const patchedIndex = transformIndex(index);
const section = (source, start, end) => {
    const from = source.indexOf(start);
    assert.notEqual(from, -1);
    const to = source.indexOf(end, from);
    assert.notEqual(to, -1);
    return source.slice(from, to);
};
const tick = () => new Promise(resolve => setImmediate(resolve));
const binding = generation => Object.freeze({ connectionId: `server-connection-${generation}`, generation });

function harness(source, gateways, states = new Map()) {
    const variables = section(source, 'const gatewayInteractionBindings =', '/** @type {Set<');
    const reader = section(source, 'function interactionBindingFromGateway(', '/**\n * Convert a Gateway-owned');
    const cache = section(source, 'const gatewayConnections = createGatewayConnectionCache(', 'function ensureGateway()');
    const reconnect = section(source, 'export async function reconnectBridgeInteraction(', 'export function isGatewayUnavailableError(').replace('export async function', 'async function');
    return new Function('createGatewayConnectionCache', 'connectWithRetry', 'sessionState', `
        ${variables}
        const gatewayNotificationHandlers = new Set();
        const console = {warn() {}};
        const interactionReplayRequestToGatewayEvent = () => null;
        ${reader}
        ${reconnect}
        ${cache}
        return {
            get: () => gatewayConnections.get(),
            read: gateway => interactionBindingFromGateway(gateway),
            retired: () => disconnectedInteractionBinding,
            current: () => gatewayConnections.current(),
            reconcile: gateway => reconnectActiveInteractionsAfterGatewayReconnect(gateway),
        };
    `)(createGatewayConnectionCache, async () => {
        const next = gateways.shift();
        assert.ok(next, 'only prepared fake Gateway connections are used');
        return next;
    }, states);
}

class Gateway {
    constructor(issued, reconnect) { this.hello = issued; this.observers = []; this.reconnectInteraction = reconnect; }
    get interactionBinding() { return this.hello; }
    onDisconnect(callback) { this.observers.push(callback); }
    onNotification() {}
    close() {
        // The actual GatewayWsClient clears hello before notifying observers.
        this.hello = undefined;
        for (const observer of this.observers) observer(new Error('Gateway WebSocket closed.'));
    }
}

for (const kind of ['question', 'permission']) {
    test(`${kind}: exact retired snapshot rebinds after getter is cleared`, async () => {
        const first = binding(1), second = binding(2);
        const pending = { kind, owner: 'web:session', requestId: 'pending-request', current: first, last: first, binding: first };
        const calls = [];
        const old = new Gateway(first);
        const next = new Gateway(second, async input => {
            calls.push(input);
            assert.deepEqual(Object.keys(input).sort(), ['previousBinding', 'sessionKey']);
            assert.deepEqual(input.previousBinding, first);
            assert.equal(input.sessionKey, pending.owner);
            pending.current = second;
            pending.binding = second;
            return { outcome: 'reconnected', binding: second, requests: [] };
        });
        const states = new Map([[pending.owner, { sessionKey: pending.owner, active: true, runId: 'run' }]]);
        const app = harness(patchedBridge, [old, next], states);
        await app.get();
        old.close();
        pending.current = undefined; pending.binding = undefined;
        assert.equal(old.interactionBinding, undefined);
        assert.deepEqual(app.retired(), first);
        assert.deepEqual(app.read(old), first, 'stream catch retains exact retired ownership proof');
        assert.ok(Object.isFrozen(app.read(old)));
        assert.notDeepEqual(pending.binding, second, 'new socket alone cannot answer pending interaction');
        await app.get();
        await app.reconcile(next);
        assert.equal(calls.length, 1);
        assert.deepEqual(pending.current, second);
        assert.deepEqual(pending.binding, second);
        assert.equal(app.retired(), null);
        old.close();
        assert.equal(app.current(), next, 'late old close cannot invalidate replacement');
        assert.equal(app.retired(), null, 'late old callback cannot overwrite retired binding');
    });
}

test('empty or malformed hello never produces an ownership proof', async () => {
    const empty = new Gateway(undefined);
    const app = harness(patchedBridge, [empty]);
    await app.get(); empty.close();
    assert.equal(app.read(empty), null);
    assert.equal(app.retired(), null);
    assert.equal(app.read(new Gateway({ connectionId: '', generation: 1 })), null);
    assert.equal(app.read(new Gateway({ connectionId: 'bad', generation: 1.5 })), null);
});

test('retired object keeps its first issued identity while a new object is independent', async () => {
    const first = binding(1), second = binding(2);
    const old = new Gateway(first), next = new Gateway(second);
    const app = harness(patchedBridge, [old, next]);
    await app.get(); old.close(); await app.get();
    // The production factory does not reconnect the same wrapper. Even if a
    // getter changed, a delayed catch for that wrapper must keep its first ID.
    old.hello = binding(99);
    assert.deepEqual(app.read(old), first);
    assert.deepEqual(app.read(next), second);
});

test('old reconciliation cannot erase a newer retired connection', async () => {
    let release;
    const gate = new Promise(resolve => { release = resolve; });
    const first = new Gateway(binding(1));
    const second = new Gateway(binding(2), async () => { await gate; return { outcome: 'reconnected', requests: [] }; });
    const third = new Gateway(binding(3), async () => ({ outcome: 'reconnected', requests: [] }));
    const states = new Map([['s', { sessionKey: 's', active: true, runId: 'run' }]]);
    const app = harness(patchedBridge, [first, second, third], states);
    await app.get(); first.close(); await app.get();
    await tick(); second.close();
    assert.deepEqual(app.retired(), binding(2));
    await app.get();
    release(); await tick(); await tick();
    assert.deepEqual(app.retired(), binding(2), 'A→B completion preserves B→C retirement');
    assert.equal(app.current(), third);
});

function responseBody(source, type, nextType) {
    const branch = section(source, `} else if (data.type === '${type}') {`, `} else if (data.type === '${nextType}') {`);
    // The next branch's leading `}` closes this outer branch; it is excluded
    // by section(). Keep this branch's inner if-closing brace intact.
    return branch.slice(branch.indexOf('{') + 1);
}

for (const [type, nextType, helper] of [
    ['permission-response', 'session-permission-grant', 'decidePermissionViaGateway'],
    ['elicitation-response', 'check-session-status', 'elicitationRespondViaGateway'],
]) {
    test(`${type}: only delivered=true broadcasts the existing cancellation acknowledgement`, async () => {
        const body = responseBody(patchedIndex, type, nextType);
        const fn = new Function(helper, 'normalizeSessionId', 'broadcastToSessionWatchers', 'createNormalizedMessage', 'data', 'userId', `return (async () => {${body}})();`);
        for (const delivered of [false, true]) {
            const calls = [];
            const helperCalls = [];
            await fn(async (...args) => { helperCalls.push(args); return delivered; }, x => x, (...args) => calls.push(args), x => x,
                { type, requestId: 'request', sessionId: 'session', answer: { type: 'answered' }, allow: true }, 'user');
            assert.equal(helperCalls.length, 1);
            assert.equal(calls.length, delivered ? 1 : 0);
            if (delivered) assert.deepEqual(calls[0], ['session', { kind: 'permission_cancelled', requestId: 'request', sessionId: 'session', provider: 'pilotdeck' }, 'user']);
        }
    });
}

test('SHA drift is refused and hooks pass unrelated modules through', async () => {
    assert.throws(() => transformBridge(`${bridge}\n`), /SHA mismatch/);
    assert.throws(() => transformIndex(`${index}\n`), /SHA mismatch/);
    const probe = readFileSync(identity['ui/server/services/modelConnectionProbe.js'].path, 'utf8');
    assert.throws(() => transformModelProbe(`${probe}\n`), /SHA mismatch/);
    const fallback = {};
    assert.equal(await load('file:///unrelated.mjs', {}, async () => fallback), fallback);
    for (const name of ['ui/server/pilotdeck-bridge.js', 'ui/server/index.js', 'ui/server/services/modelConnectionProbe.js']) {
        const loaded = await load(pathToFileURL(identity[name].path).href, {}, async () => { throw new Error('target must use exact overlay'); });
        assert.equal(loaded.shortCircuit, true);
        assert.equal(loaded.format, 'module');
        assert.equal(loaded.source, name.includes('bridge') ? patchedBridge : name.endsWith('/index.js') ? patchedIndex : transformModelProbe(probe));
    }
});

test('negative control: removing snapshot fallback breaks the closed-getter witness', async () => {
    const mutant = patchedBridge.replace('gatewayInteractionBindings.get(gateway) ?? gateway?.interactionBinding', 'gateway?.interactionBinding');
    const old = new Gateway(binding(1));
    const app = harness(mutant, [old]);
    await app.get(); old.close();
    assert.equal(app.read(old), null);
    assert.equal(app.retired(), null);
});

for (const [type, nextType, helper] of [
    ['permission-response', 'session-permission-grant', 'decidePermissionViaGateway'],
    ['elicitation-response', 'check-session-status', 'elicitationRespondViaGateway'],
]) {
    test(`negative control: baseline ${type} falsely acknowledges delivered=false`, async () => {
        const body = responseBody(index, type, nextType);
        const fn = new Function(helper, 'normalizeSessionId', 'broadcastToSessionWatchers', 'createNormalizedMessage', 'data', 'userId', `return (async () => {${body}})();`);
        const calls = [];
        await fn(async () => false, x => x, (...args) => calls.push(args), x => x, { requestId: 'r', sessionId: 's', answer: {} }, 'user');
        assert.equal(calls.length, 1, 'the false-cancellation control reaches the real baseline branch');
    });
}

// Exercise the actual transformed helper and WS branch bodies.
function ownerHarness(source, name) {
    const start = source.indexOf('export async function ' + name);
    const end = name === 'decidePermissionViaGateway' ? source.indexOf('export async function grantSessionPermissionViaGateway', start) : source.length;
    assert.ok(start >= 0 && end > start);
    const calls = [];
    const gateway = {
        permissionDecide: async input => { calls.push(input); return { delivered: input.sessionKey === 'web:A' }; },
        respondElicitation: async input => { calls.push(input); return { delivered: input.sessionKey === 'web:A' }; },
    };
    const fn = new Function('sessionState', 'isPilotDeckSessionKey', 'ensureGateway', 'reconnectActiveInteractionsAfterGatewayReconnect', `${source.slice(start, end).replace('export ', '')}; return ${name};`)(
        new Map([['web:A', { sessionKey: 'web:A' }], ['web:B', { sessionKey: 'web:B' }]]),
        value => typeof value === 'string' && value.startsWith('web:'), async () => gateway, async () => {},
    );
    return { calls, invoke: owner => name === 'decidePermissionViaGateway' ? fn('request', 'allow', { sessionId: owner }) : fn('request', {}, owner) };
}
for (const name of ['decidePermissionViaGateway', 'elicitationRespondViaGateway']) {
    test(`${name}: missing and unknown owner cannot call Gateway; wrong owner never searches another session`, async () => {
        const h = ownerHarness(patchedBridge, name);
        assert.equal(await h.invoke(undefined), false);
        assert.equal(await h.invoke('web:unknown'), false);
        assert.equal(h.calls.length, 0);
        assert.equal(await h.invoke('web:B'), false);
        assert.deepEqual(h.calls.map(x => x.sessionKey), ['web:B']);
        assert.equal(await h.invoke('web:A'), true);
        assert.deepEqual(h.calls.map(x => x.sessionKey), ['web:B', 'web:A']);
    });
    test(`negative control: original ${name} accepts a request belonging to a different explicit owner`, async () => {
        const h = ownerHarness(bridge, name);
        assert.equal(await h.invoke('web:B'), true);
        assert.deepEqual(h.calls.map(x => x.sessionKey), ['web:A']);
    });
}
async function editBranch(source, data) {
    const body = responseBody(source, 'regenerate-last-message', 'abort-session');
    const calls = [], frames = [];
    const fn = new Function('data', 'normalizeSessionId', 'writer', 'streamWriter', 'sessionWatchRegistry', 'regenerateLastMessageTransaction', 'replaceLastTurnViaGateway', 'finalizeLastTurnReplacementViaGateway', 'runChatViaGateway', 'ws', `return (async () => {${body}})();`);
    await fn(data, x => typeof x === 'string' && x.trim() ? x.trim() : null, { send: x => frames.push(x) }, {},
        { watch: sessionId => calls.push(['watch', sessionId]) },
        async x => calls.push(['transaction', x.sessionId, x.data.options.sessionId, x.data.options.sessionKey]), () => {}, () => {}, () => {}, {});
    return { calls, frames };
}
for (const [name, data, conflict] of [
    ['top/options conflict', { sessionId: 'A', options: { sessionId: 'B' } }, true],
    ['missing top with conflicting options aliases', { options: { sessionId: 'A', sessionKey: 'B' } }, true],
    ['top/sessionKey conflict', { sessionId: 'A', options: { sessionKey: 'B' } }, true],
    ['same owner', { sessionId: 'A', options: { sessionId: 'A', sessionKey: 'A' } }, false],
    ['legacy top only', { sessionId: 'A' }, false],
    ['legacy options only', { options: { sessionId: 'A' } }, false],
]) {
    test(`regenerate owner: ${name}`, async () => {
        const result = await editBranch(patchedIndex, { ...data, expectedTurnId: 'old' });
        if (conflict) {
            assert.deepEqual(result.calls, []);
            assert.equal(result.frames.length, 1);
            assert.equal(result.frames[0].success, false);
        } else {
            assert.deepEqual(result.calls.slice(0, 1), [['watch', 'A']]);
            assert.deepEqual(result.calls[1].slice(0, 3), ['transaction', 'A', 'A']);
        }
    });
}
test('negative control: original edit branch prepares A but passes B to execution', async () => {
    const result = await editBranch(index, { sessionId: 'A', options: { sessionId: 'B' }, expectedTurnId: 'old' });
    assert.deepEqual(result.calls[1].slice(0, 3), ['transaction', 'A', 'B']);
});
