import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { transformBridge } from './overlay.mjs';

const root = path.dirname(fileURLToPath(import.meta.url));
const baselineRoot = process.env.PILOTDECK_WEB_BASELINE_ROOT;
const cacheDir = process.env.QUEUE_RETRY_CACHE_DIR;
if (!baselineRoot || !path.isAbsolute(baselineRoot) || !cacheDir || !path.isAbsolute(cacheDir)) throw Error('Explicit isolated baseline/cache paths required');
export default {
    root, envDir: false, cacheDir,
    plugins: [{ name: 'fixed-baseline-queue-retry', enforce: 'pre', transform(code, id) {
        if (id.split('?')[0] === path.join(baselineRoot, 'ui/server/pilotdeck-bridge.js')) return transformBridge(code);
    } }],
    resolve: { alias: { vitest: path.join(baselineRoot, 'ui/node_modules/vitest/dist/index.js') } },
    test: { include: ['queue-retry.controls.test.mjs'], environment: 'node', watch: false, fileParallelism: false, maxWorkers: 1 },
};
