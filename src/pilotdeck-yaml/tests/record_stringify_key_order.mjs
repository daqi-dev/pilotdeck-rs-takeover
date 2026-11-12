// Keep inputs as raw JSON strings. Recording an object-valued `value` through
// JSON.stringify would sort its index keys before Rust sees it, hiding this bug.
import { createRequire } from 'node:module';
import path from 'node:path';

if (!process.env.HOME || !process.env.PILOT_HOME) {
  throw new Error('Isolate HOME and PILOT_HOME before recording.');
}
const require = createRequire(import.meta.url);
const moduleRoot = path.resolve(process.argv[2]);
const { parseDocument, stringify } = require(moduleRoot);
const yamlVersion = require(path.join(moduleRoot, 'package.json')).version;
if (yamlVersion !== '2.9.0' || process.version !== 'v22.22.0') {
  throw new Error(`Expected Node 22.22.0 / yaml 2.9.0, got ${process.version} / ${yamlVersion}`);
}

const inputs = [
  ['descending-top', '{"10":"ten","2":"two","1":"one","tail":"tail"}'],
  ['nested-and-arrays', '{"outer":{"10":"ten","2":"two","1":"one"},"array":[{"3":"three","1":"one"},{"9":{"4":"four","0":"zero"},"2":"two"}]}'],
  ['index-boundaries', '{"4294967295":"ordinary-max","4294967294":"largest-index","01":"leading-zero","1":"one","0":"zero","4294967296":"ordinary-overflow","-0":"negative-zero","+1":"plus-one","1.0":"decimal","1e0":"exponent","tail":"tail"}'],
  ['duplicate-properties', '{"2":"old-two","1":"one","2":"new-two","x":"old-x","00":"zeroes","x":"new-x"}'],
  ['unicode-ordinary', '{"中文":"first","2":"two","１":"fullwidth","1":"one","é":"last"}'],
  ['setup-groups', '{"model":{"providers":{"2":{"models":{"10":{},"2":{},"01":{}}},"1":{"models":{"3":{},"0":{}}},"01":{"models":{}}}},"adapters":{"wecom":{"groups":{"10":{},"2":{},"01":{}}}}}'],
];
const formats = [
  ['default', {}],
  ['setup', { lineWidth: 0, singleQuote: false }],
];
const cases = [];
for (const [name, raw_json] of inputs) {
  for (const [format, options] of formats) {
    const value = JSON.parse(raw_json);
    cases.push({ name: `${name}/${format}`, raw_json, src: '', path: [], options, output: stringify(value, options) });
  }
}
const raw_json = '{"3":"three","1":"one","01":"ordinary"}';
const src = 'existing: keep\n"2": two\n"1": one\n';
const options = { lineWidth: 0, singleQuote: false };
const document = parseDocument(src);
document.setIn(['added'], JSON.parse(raw_json));
cases.push({ name: 'preserve-parsed-map/setup', raw_json, src, path: ['added'], options, output: document.toString(options) });
process.stdout.write(`${JSON.stringify({ baseline: 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85', node: process.version, yaml: yamlVersion, cases }, null, 2)}\n`);
