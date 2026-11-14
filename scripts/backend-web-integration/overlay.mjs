import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { createHash } from 'node:crypto';

export const sha256 = source => createHash('sha256').update(source).digest('hex');
const specs = JSON.parse(await readFile(new URL('./patches.json', import.meta.url), 'utf8'));
export function applyOverlay(source, spec) {
  if (sha256(source) !== spec.sourceSha256) throw new Error(`Baseline hash mismatch: ${spec.path}`);
  let code = source;
  for (const { before, after } of spec.replacements) {
    if (code.split(before).length !== 2) throw new Error(`Patch anchor mismatch: ${spec.path}`);
    code = code.replace(before, after);
  }
  if (sha256(code) !== spec.resultSha256) throw new Error(`Patched hash mismatch: ${spec.path}`);
  return code;
}
export async function verifyOverlay(baseline) {
  return Promise.all(specs.map(async spec => {
    applyOverlay(await readFile(resolve(baseline, spec.path), 'utf8'), spec);
    return { path: spec.path, sourceSha256: spec.sourceSha256, resultSha256: spec.resultSha256 };
  }));
}
export function integrationOverlay(baseline) {
  const targets = new Map(specs.map(spec => [resolve(baseline, spec.path), spec]));
  const applied = new Set();
  return {
    name: 'pilotdeck-backend-web-integration',
    enforce: 'pre',
    transform(source, id) {
      const spec = targets.get(id.split('?')[0]);
      if (!spec) return null;
      applied.add(spec.path);
      return { code: applyOverlay(source, spec), map: null };
    },
    buildEnd(error) {
      if (error) return;
      // types.ts is erased by the TS transpiler; its patch is verified above.
      for (const spec of specs.filter(spec => !spec.path.endsWith('/types.ts'))) {
        if (!applied.has(spec.path)) throw new Error(`Overlay not applied: ${spec.path}`);
      }
    },
  };
}
