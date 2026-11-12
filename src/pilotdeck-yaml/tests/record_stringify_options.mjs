// Record fixed-baseline yaml@2.9.0 output for stringify_options.rs.
// Run with isolated HOME/PILOT_HOME; first argument is the baseline's yaml module.
import { createRequire } from 'node:module';
import path from 'node:path';
const require = createRequire(import.meta.url);
const moduleRoot = path.resolve(process.argv[2]);
const { parseDocument, stringify } = require(moduleRoot);
const yamlVersion = require(path.join(moduleRoot, 'package.json')).version;
if (yamlVersion !== '2.9.0') throw new Error(`Unexpected baseline yaml ${yamlVersion}`);
const words = '中文😀 long word '.repeat(14).trim();
const values = [
  ['quotes', { plain: '中文', quoted: 'say: "hello"', apostrophe: "it's: true", both: "'say': \"hello\"", boolean: 'true', empty: '' }],
  ['long-plain', { nested: { description: words } }],
  ['long-double', { nested: { description: `"${words}"` } }],
  ['new-multiline', { message: `${words}\n第二行\n`, leading: ' first\n second', trailing: 'line\n  ' }],
  ['control', { message: 'nul\0 bell\u0007 tab\t newline\nnext' }],
  ['sequences', { adapters: [{ type: 'wecom', botId: 'mock-only', secret: `"${words}"`, lines: ['first: "quoted"', '第二行'] }] }],
  ['narrow', { text: 'one two three four five six seven eight nine ten', quoted: '# one two three four five six seven eight nine ten' }],
];
const sources = [
  ['parsed-single', "first: 'say: \"hello\"'\nsecond: 'it''s true'\n"],
  ['parsed-long-single', `text: '${words}'\n`],
  ['parsed-long-double', `text: "${words}"\n`],
  ['parsed-flow', `list: [${Array.from({ length: 12 }, (_, i) => `\"中文 ${i} value\"`).join(', ')}]\n`],
  ['parsed-literal', `message: |\n  ${words}\n  last\n`],
  ['parsed-folded', `message: >\n  ${words}\n  last\n`],
  ['parsed-comments', `# heading\nmessage: '${words}' # keep\nlist: [one, two, three] # tail\n`],
];
const formats = [
  ['default', {}],
  ['setup', { lineWidth: 0, singleQuote: false }],
  ['no-fold', { lineWidth: 0 }],
  ['prefer-single', { lineWidth: 22, singleQuote: true }],
  ['very-narrow', { lineWidth: 8, singleQuote: false }],
];
const cases = [];
for (const [name, value] of values) {
  for (const [format, options] of formats) {
    cases.push({ name: `${name}/${format}`, value, options, output: stringify(value, options) });
  }
}
for (const [name, src] of sources) {
  for (const [format, options] of formats) {
    cases.push({ name: `${name}/${format}`, src, options, output: parseDocument(src).toString(options) });
  }
}
process.stdout.write(`${JSON.stringify({ baseline: 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85', node: process.version, yaml: yamlVersion, cases }, null, 2)}\n`);
