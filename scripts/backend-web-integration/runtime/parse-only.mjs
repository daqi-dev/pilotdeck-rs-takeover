import { readFileSync } from 'node:fs';
import { SourceTextModule } from 'node:vm';
import { identity, sha256, transformBridge, transformIndex, transformModelProbe } from './overlay.mjs';

for (const [name, transform] of [
    ['ui/server/pilotdeck-bridge.js', transformBridge],
    ['ui/server/index.js', transformIndex],
    ['ui/server/services/modelConnectionProbe.js', transformModelProbe],
]) {
    const source = transform(readFileSync(identity[name].path, 'utf8'));
    // Compile the complete ESM grammar without linking imports or evaluating
    // the UI module. This cannot start its watcher, server, or proxy setup.
    new SourceTextModule(source, { identifier: identity[name].path });
    console.log(JSON.stringify({ file: name, parsed: true, evaluated: false, sha256: sha256(source) }));
}
