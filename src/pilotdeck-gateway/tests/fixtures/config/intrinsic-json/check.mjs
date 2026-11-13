import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
const [expectedPath,actualPath]=process.argv.slice(2);
assert(expectedPath&&actualPath,'expected and actual recordings required');
const expected=JSON.parse(readFileSync(expectedPath)),actual=JSON.parse(readFileSync(actualPath));
assert.equal(expected.cases.length,48);assert.equal(new Set(expected.cases.map(c=>`${c.kind}/${c.key}`)).size,48);
assert.deepStrictEqual(actual,expected);console.log('PASS: exact 48-case intrinsic recording');
