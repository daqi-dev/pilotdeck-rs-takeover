// Records yaml@2.9.0 `parseDocument` + setIn/deleteIn + `toString()` outputs
// for tests/stringify.rs. Usage (repository root):
//   node src/pilotdeck-yaml/tests/record_stringify.mjs resources/PilotDeck/node_modules/yaml \
//     > src/pilotdeck-yaml/tests/fixtures/stringify-cases.json
import { createRequire } from 'node:module';
import path from 'node:path';

const require = createRequire(import.meta.url);
const { parseDocument } = require(path.resolve(process.argv[2]));

const long = 'word '.repeat(30).trim();
const sources = {
  config: 'schemaVersion: 1\nagent:\n  model: bbx/deterministic\n  maxContextTokens: 65536\n  maxOutputTokens: 8192\nmodel:\n  providers:\n    bbx:\n      protocol: openai\n      url: http://127.0.0.1:1/v1\n      apiKey: bbx-mock-only\n      models:\n        deterministic:\n          thinking:\n            state: enabled\n            efforts: [low, medium, high]\n          capabilities:\n            supportsToolUse: true\ntelemetry:\n  enabled: false\n',
  comments: '# head comment\n\n# second\nagent: # trailing on agent\n  # before model\n  model: a/b # after value\n\n  maxOutputTokens: 100\nlist:\n  - one # c1\n  # before two\n  - two\n# tail comment\n',
  scalars: 'a: "double quoted"\nb: \'single quoted\'\nc: |\n  literal\n  block\nd: >\n  folded\n  block\n  text\ne: ~\nf:\ng: null\nh: TRUE\ni: 0x1F\nj: 0o17\nk: 1.50\nl: 1e3\nm: .inf\nn: -.NaN\no: "123"\np: \'true\'\nq: "with: colon"\nr: "#hash"\ns: "line1\\nline2"\nt: "tab\\there"\nu: 2.0\nv: -0\n',
  flow: 'a: {x: 1, y: [1, 2, {z: 3}]}\nb: []\nc: {}\nd: [ "q", \'s\', plain ]\n',
  anchors: 'base: &b\n  x: 1\nderived: *b\nlist: &l [1, 2]\nother: *l\ns: &s scalar\nt: *s\n',
  tags: 'a: !!str 123\nb: !!int "7"\nc: !custom value\nd: !!float 3\n',
  longs: `a: ${long}\nb: "${long}"\nc: >\n  ${long}\n  ${long}\nlist:\n  - ${long}\n`,
  doc: '---\na: 1\n...\n',
  docComment: 'a: 1\n\n# final\n',
  nested: 'agent:\n  subagents:\n    default: inherit\n    maxDepth: 2\n  thinking:\n    enabled: false\nextension:\n  builtinPluginsEnabled:\n    one: true\n',
  emptyMap: 'agent: {}\nextension:\n',
  keep: 'a: |+\n  kept\n\nb: 1\n',
  multiDoc: '%YAML 1.2\n---\na: 1\n',
};

const edits = [
  [],
  [['set', ['agent', 'maxOutputTokens'], 4096]],
  [['set', ['agent', 'maxContextTokens'], 1000], ['set', ['agent', 'thinking'], { enabled: true, budgetTokens: 2048 }]],
  [['set', ['agent', 'subagents', 'default'], 'provider/some-model-name']],
  [['set', ['agent', 'subagents', 'timeoutMs'], 60000], ['set', ['extension', 'includeHookEvents'], true]],
  [['delete', ['agent', 'maxOutputTokens']]],
  [['delete', ['agent', 'subagents']], ['delete', ['extension', 'includeHookEvents']]],
  [['set', ['extension', 'builtinPluginsEnabled'], { alpha: true, 'beta gamma': false }]],
  [['set', ['tools', 'webSearch', 'enabled'], false]],
  [['set', ['agent', 'thinking'], null]],
  [['set', ['k'], 2.5], ['set', ['h'], false], ['set', ['i'], 255], ['set', ['e'], 'now a string'], ['set', ['p'], true]],
  [['set', ['a'], `${long} more`]],
  [['delete', ['agent', 'subagents', 'default']]],
];

const cases = [];
for (const [name, src] of Object.entries(sources)) {
  for (const ops of edits) {
    const doc = parseDocument(src, { prettyErrors: false });
    let error = null;
    try {
      for (const [op, path, value] of ops) {
        if (op === 'set') {
          if (value === null) doc.deleteIn(path);
          else doc.setIn(path, value);
        } else doc.deleteIn(path);
      }
    } catch (e) {
      error = e.message;
    }
    let output = null;
    if (!error) {
      try {
        output = doc.toString();
      } catch (e) {
        error = e.message;
      }
    }
    cases.push({ name, src, ops, output, error });
  }
}
process.stdout.write(JSON.stringify(cases, null, 1) + '\n');
