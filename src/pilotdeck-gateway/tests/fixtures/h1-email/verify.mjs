import {readFileSync} from 'node:fs';
import assert from 'node:assert/strict';
if(process.argv.length!==4) throw new Error('usage: verify.mjs EXPECTED ACTUAL');
assert.deepStrictEqual(JSON.parse(readFileSync(process.argv[3],'utf8')),JSON.parse(readFileSync(process.argv[2],'utf8')));
