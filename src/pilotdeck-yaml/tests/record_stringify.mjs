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

// Directives and yaml-1.1 shapes a hand-written pilotdeck.yaml may carry
// (G8), each under a smaller set of edits.
const base = 'schemaVersion: 1\nagent:\n  model: bbx/deterministic\n';
const v11 = '%YAML 1.1\n---\n';
const rareSources = {
  yaml12: '%YAML 1.2\n---\n' + base,
  yaml11: v11 + base,
  yaml11Comment: '# head\n%YAML 1.1\n---\n# before\n' + base + '# tail\n',
  tags2: '%TAG !a! tag:a.com,2000:\n%TAG !b! tag:b.com,2000:\n---\n' + base + 'x: !a!foo bar\n',
  tags2Both: '%TAG !a! tag:a.com,2000:\n%TAG !b! tag:b.com,2000:\n---\n' + base + 'x: !a!foo bar\ny: !b!baz [1]\n',
  tagOverride: '%TAG !! tag:other.org,2000:\n---\n' + base + 'x: !!foo bar\n',
  localTag: base + 'x: !local bar\n',
  merge12: 'defaults: &d\n  maxOutputTokens: 100\n' + base + '  <<: *d\n',
  merge11: v11 + 'defaults: &d\n  maxOutputTokens: 100\n' + base + '  <<: *d\n',
  merge11Seq: v11 + 'a: &a {x: 1}\nb: &b {y: 2}\n' + base + '  <<: [*a, *b]\n',
  set11: v11 + base + 's: !!set\n  ? a\n  ? b\n',
  set11Flow: v11 + base + 's: !!set {a, b}\n',
  set11NullValues: v11 + base + 's: !!set\n  a: null\n  b: ~\n',
  omap11: v11 + base + 'o: !!omap\n  - a: 1\n  - b: 2\n',
  omap11Flow: v11 + base + 'o: !!omap [a: 1, b: 2]\n',
  pairs11: v11 + base + 'p: !!pairs\n  - a: 1\n  - a: 2\n',
  binary11: v11 + base + 'b: !!binary aGVsbG8=\n',
  binary11Long: v11 + base + 'b: !!binary |\n  ' + 'Bwc'.repeat(53) + 'H\n',
  binary11Quoted: v11 + base + 'b: !!binary "aGVsbG8="\n',
  timestamp11: v11 + base + 't: 2001-12-14t21:59:43.10-05:00\nd: 2002-12-14\nu: 2002-12-14 01:02:03\n',
  set12: base + 's: !!set\n  ? a\n',
  binary12: base + 'b: !!binary aGVsbG8=\n',
  timestamp12: base + 't: !!timestamp 2001-12-14\n',
  omap12: base + 'o: !!omap\n  - a: 1\n',
  bool11: v11 + base + 'k1: yes\nk2: off\nk3: Y\nk4: true\nk5: FALSE\nk6: On\n',
  int11: v11 + base + 'oct: 010\nbin: 0b101\nhex: 0x1F\nnhex: -0x1F\nnoct: -010\nsex: 190:20:30\nfsex: 20:30.15\nund: 1_000\nfl: 1.50\nexp: 1e3\n',
  str11: v11 + base + "s1: 'yes'\ns2: \"on\"\ns3: '010'\n",
};
const rareEdits = [
  [],
  [['set', ['agent', 'maxOutputTokens'], 4096]],
  [['set', ['agent', 'note'], 'yes'], ['set', ['agent', 'subagents', 'timeoutMs'], 60000]],
  // A created wrapper is an omap where the schema has that tag; later edits
  // go through it like a map.
  [
    ['set', ['agent', 'subagents', 'timeoutMs'], 60000],
    ['set', ['agent', 'subagents', 'default'], 'provider/some-model-name'],
    ['delete', ['agent', 'subagents', 'timeoutMs']],
    ['set', ['tools', 'webSearch', 'enabled'], false],
  ],
];

const cases = [];
const all = [
  ...Object.entries(sources).map(([name, src]) => [name, src, edits]),
  ...Object.entries(rareSources).map(([name, src]) => [name, src, rareEdits]),
];
for (const [name, src, sourceEdits] of all) {
  for (const ops of sourceEdits) {
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
