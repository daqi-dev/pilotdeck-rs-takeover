import { readFile, appendFile } from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { baseline, identity, sha256, transformBridge, transformIndex } from './overlay.mjs';

const targets = new Map([
    [pathToFileURL(identity['ui/server/pilotdeck-bridge.js'].path).href, transformBridge],
    [pathToFileURL(identity['ui/server/index.js'].path).href, transformIndex],
]);

export async function load(url, context, nextLoad) {
    const transform = targets.get(url);
    if (!transform) return nextLoad(url, context);
    const original = await readFile(new URL(url), 'utf8');
    const source = transform(original);
    const receipt = { baselineCommit: baseline.baselineCommit, url, originalSha256: sha256(original), patchedSha256: sha256(source) };
    const output = process.env.PILOTDECK_WEB_RUNTIME_RECEIPT;
    if (output) {
        if (!path.isAbsolute(output)) throw new Error('PILOTDECK_WEB_RUNTIME_RECEIPT must be absolute.');
        await appendFile(output, JSON.stringify(receipt) + '\n', { mode: 0o600 });
    }
    process.stderr.write(`[web-reconnect-overlay] ${JSON.stringify(receipt)}\n`);
    return { format: 'module', source, shortCircuit: true };
}
