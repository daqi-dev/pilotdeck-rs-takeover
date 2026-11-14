import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { transformQueueRetry } from './queue-retry-overlay.mjs';

export const baseline = JSON.parse(readFileSync(new URL('./baseline-identity.json', import.meta.url), 'utf8'));
const baselineRoot = process.env.PILOTDECK_WEB_BASELINE_ROOT;
if (!baselineRoot || !path.isAbsolute(baselineRoot)) throw new Error('PILOTDECK_WEB_BASELINE_ROOT must be an absolute fixed-baseline checkout path.');
export const identity = Object.fromEntries(Object.entries(baseline.files).map(([name, file]) => [name, {
    path: path.join(baselineRoot, name), sha256: file.sourceSha256, resultSha256: file.resultSha256,
}]));
export const sha256 = source => createHash('sha256').update(source).digest('hex');

function replaceOnce(source, before, after) {
    if (source.split(before).length !== 2) throw new Error('Runtime overlay anchor is absent or ambiguous.');
    return source.replace(before, after);
}

export function transformBridge(source) {
    if (sha256(source) !== identity['ui/server/pilotdeck-bridge.js'].sha256) throw new Error('Bridge baseline SHA mismatch.');
    let next = replaceOnce(source,
        'let disconnectedInteractionBinding = null;',
        'const gatewayInteractionBindings = new WeakMap();\nlet disconnectedInteractionBinding = null;');
    next = replaceOnce(next,
        '    onConnected(gateway) {\n        for (const handler of gatewayNotificationHandlers) {',
        '    onConnected(gateway) {\n        const binding = interactionBindingFromGateway(gateway);\n        if (binding && !gatewayInteractionBindings.has(gateway)) gatewayInteractionBindings.set(gateway, binding);\n        for (const handler of gatewayNotificationHandlers) {');
    next = replaceOnce(next,
        '    const binding = gateway?.interactionBinding;',
        '    const binding = gatewayInteractionBindings.get(gateway) ?? gateway?.interactionBinding;');
    next = replaceOnce(next,
        "        if (results.every((result) => result.status === 'fulfilled')) {\n            disconnectedInteractionBinding = null;",
        "        if (results.every((result) => result.status === 'fulfilled')\n            && disconnectedInteractionBinding === previousBinding\n            && gatewayConnections.current() === gateway) {\n            disconnectedInteractionBinding = null;");
    if (sha256(next) !== baseline.queueRetry.previousBridgeResultSha256) throw new Error('Prior bridge overlay result SHA mismatch.');
    for (const [name, expected] of Object.entries(baseline.queueRetry.ownedFiles)) {
        if (sha256(readFileSync(new URL(name, import.meta.url))) !== expected) throw new Error('Queue retry implementation SHA mismatch.');
    }
    next = transformQueueRetry(next);
    if (sha256(next) !== baseline.ownerIsolation.previousBridgeResultSha256) throw new Error('Prior bridge owner overlay SHA mismatch.');
    next = ownerBridge(next);
    if (sha256(next) !== identity['ui/server/pilotdeck-bridge.js'].resultSha256) throw new Error('Bridge overlay result SHA mismatch.');
    return next;
}

export function transformIndex(source) {
    if (sha256(source) !== identity['ui/server/index.js'].sha256) throw new Error('UI server index baseline SHA mismatch.');
    let next = replaceOnce(source,
        '                    await decidePermissionViaGateway(\n',
        '                    const delivered = await decidePermissionViaGateway(\n');
    next = replaceOnce(next,
        "                            reason: data.message,\n                        },\n                    );\n                    const resolvedSessionId = normalizeSessionId(data.sessionId);\n                    if (resolvedSessionId) {",
        "                            reason: data.message,\n                        },\n                    );\n                    const resolvedSessionId = normalizeSessionId(data.sessionId);\n                    if (delivered && resolvedSessionId) {");
    next = replaceOnce(next,
        '                    await elicitationRespondViaGateway(data.requestId, data.answer);\n                    const resolvedSessionId = normalizeSessionId(data.sessionId);\n                    if (resolvedSessionId) {',
        '                    const delivered = await elicitationRespondViaGateway(data.requestId, data.answer);\n                    const resolvedSessionId = normalizeSessionId(data.sessionId);\n                    if (delivered && resolvedSessionId) {');
    if (sha256(next) !== baseline.ownerIsolation.previousIndexResultSha256) throw new Error('Prior index owner overlay SHA mismatch.');
    next = transformEditIndex(ownerIndex(next));
    if (sha256(next) !== identity['ui/server/index.js'].resultSha256) throw new Error('UI server index overlay result SHA mismatch.');
    return next;
}

export function transformModelProbe(source) {
    const file = identity['ui/server/services/modelConnectionProbe.js'];
    if (sha256(source) !== file.sha256) throw new Error('Model probe baseline SHA mismatch.');
    let next = replaceOnce(source,
        'function looksLikeImageUnsupported(detail) {\n  return ',
        String.raw`function looksLikeImageUnsupported(detail, status) {
  const textOnlyParameterError = status === 400
    && /^\s*messages\.content\.type\s+参数非法\s*[，,]\s*取值范围\s*[:：]?\s*\[\s*(['"])text\1\s*\]\s*$/.test(detail);
  return textOnlyParameterError || `);
    next = replaceOnce(next,
        'if (image && looksLikeImageUnsupported(detail)) {',
        'if (image && looksLikeImageUnsupported(detail, response.status)) {');
    if (sha256(next) !== file.resultSha256) throw new Error('Model probe overlay result SHA mismatch.');
    return next;
}

// 会话归属校验在既有 Web12 变换及其 SHA 校验之后执行。
function once(source,before,after){if(source.split(before).length!==2)throw Error('absent/ambiguous owner anchor');return source.replace(before,after);}
function ownerIndex(source){
 source=once(source,'                            remember: Boolean(data.rememberEntry),','                            sessionId: data.sessionId,\n                            remember: Boolean(data.rememberEntry),');
 return once(source,'elicitationRespondViaGateway(data.requestId, data.answer)','elicitationRespondViaGateway(data.requestId, data.answer, data.sessionId)');
}
function ownerBridge(source){
 for(const [start,end,selector] of [
  ['export async function decidePermissionViaGateway(', 'export async function grantSessionPermissionViaGateway(', 'options.sessionId'],
  ['export async function elicitationRespondViaGateway(', null, 'sessionId'],
 ]){
  const a=source.indexOf(start);const b=end?source.indexOf(end,a):source.length;
  if(a<0||b<a)throw Error('missing helper');let helper=source.slice(a,b);
  if(!end)helper=once(helper,'(requestId, answer) {','(requestId, answer, sessionId) {');
  helper=once(helper,'    const gw = await ensureGateway();',`    // Consume only the explicit originating session; never search other owners.\n    const owner = ${selector};\n    if (typeof owner !== 'string' || !isPilotDeckSessionKey(owner)) return false;\n    const state = sessionState.get(owner);\n    if (!state || state.sessionKey !== owner) return false;\n    const gw = await ensureGateway();`);
  helper=helper.replace('    // PermissionBus is keyed by sessionKey + requestId. We don\'t know\n    // which session owns the request, so try each known session.\n','');
  helper=once(helper,'    for (const state of sessionState.values()) {','    {');
  source=source.slice(0,a)+helper+source.slice(b);
 }
 return source;
}

function transformEditIndex(source) {
 const start=source.indexOf("            } else if (data.type === 'regenerate-last-message') {");
 const end=source.indexOf("            } else if (data.type === 'abort-session') {",start);
 if(start<0||end<start)throw Error('edit branch anchor mismatch');
 let branch=source.slice(start,end);
 const anchor="                const provider = data.options?.providerHint || 'pilotdeck';";
 if(branch.split(anchor).length!==2)throw Error('edit owner index anchor mismatch');
 branch=branch.replace(anchor,`                const suppliedOwners = [data.sessionId, data.options?.sessionId, data.options?.sessionKey]
                    .filter(value => value !== undefined && value !== null && value !== '');
                if (suppliedOwners.some(value => normalizeSessionId(value) !== sessionId)) {
                    writer.send({ type: 'regenerate-last-message-result', requestId, sessionId,
                        success: false, error: 'The edited message session does not match the requested session.' });
                    return;
                }
                const provider = data.options?.providerHint || 'pilotdeck';`);
 const callAnchor='                    data,\n                    sessionId,';
 if(branch.split(callAnchor).length!==2)throw Error('edit transaction call anchor mismatch');
 branch=branch.replace(callAnchor,'                    data: { ...data, options: { ...data.options, sessionId } },\n                    sessionId,');
 return source.slice(0,start)+branch+source.slice(end);
}
