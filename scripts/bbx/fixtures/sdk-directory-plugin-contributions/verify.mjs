#!/usr/bin/env node
// Formal WS/plugin verifier: no product imports or capture rewriting.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';
const here = path.dirname(fileURLToPath(import.meta.url));
const defaultScenario = path.resolve(here, '../../scenarios/sdk-directory-plugin-contributions.json');
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const text = value => typeof value === 'string' ? value : Array.isArray(value) ? value.filter(b => b?.type === 'text').map(b => b.text ?? '').join('') : '';
const exact = (rows, count, name) => { assert.equal(rows?.length ?? 0, count, `${name}: count`); return rows; };
function command(v) {
  const upper = v.toUpperCase();
  return `<plugin-command name="/local-${v}:probe">\n<command-body>\n<<<PILOTDECK_COMMAND_BODY_0>>>\nSDK-DIR-COMMAND-${upper}\n<<<PILOTDECK_COMMAND_BODY_0>>>\n</command-body>\n\n<command-argument>\n<<<PILOTDECK_COMMAND_ARGUMENT_0>>>\nSDK-DIR-PHASE-${upper}\n<<<PILOTDECK_COMMAND_ARGUMENT_0>>>\n</command-argument>\n</plugin-command>`;
}
function inputs(a, session) {
  const files = Object.entries(a.tree ?? {}).filter(([name, value]) => name.endsWith('.jsonl') && Array.isArray(value?.jsonl) && value.jsonl.some(row => row.sessionId === session));
  return exact(files, 1, `actual transcript ${session}`)[0][1].jsonl.filter(row => row.type === 'accepted_input' && row.sessionId === session);
}
function lastUser(r) { assert.ok(Array.isArray(r.body?.messages)); return text(r.body.messages.filter(m => m.role === 'user').at(-1)?.content); }
function system(r) { return r.body.messages.filter(m => m.role === 'system').map(m => text(m.content)).join('\n'); }
function tool(r, id) { return text(exact(r.body.messages.filter(m => m.role === 'tool' && m.tool_call_id === id), 1, id)[0].content); }
function validate(a, scene, sceneHash) {
  assert.equal(scene.name, 'sdk-directory-plugin-contributions'); assert.equal(a.scenario, scene.name);
  assert.equal(a.evidence?.scenario?.sha256, sceneHash, 'captured input bytes');
  assert.equal(a.evidence.side, a.side); assert.ok(['ts', 'rust'].includes(a.side)); assert.deepEqual(a.failures, []);
  const ids = scene.assertions.map(row => row.id); assert.equal(new Set(ids).size, ids.length);
  exact(a.evidence.assertions, ids.length, 'executed assertions');
  for (const id of ids) { const row = exact(a.evidence.assertions.filter(row => row.id === id), 1, id)[0]; assert.equal(row.passed, true, id); assert.ok(!row.skipped, id); }
  exact(a.steps, 13, 'real WS steps');
  for (const i of [0, 3, 6, 9, 12]) { assert.equal(a.steps[i].step.ws, 'submit_turn'); assert.ok(Array.isArray(a.steps[i].frames)); }
  const names = ['a-initial', 'a-hidden', 'a-final', 'b-initial', 'b-final', 'isolated', 'cleared'];
  const records = Object.fromEntries(names.map(name => [name, exact(a.provider[name], 1, name)[0]]));
  assert.equal(a.provider.unexpected?.length ?? 0, 0); assert.equal(a.provider.default?.length ?? 0, 0);
  for (const r of Object.values(records)) { assert.equal(r.method, 'POST'); assert.equal(r.url, '/v1/chat/completions'); assert.equal(r.body.model, 'deterministic'); assert.equal(r.body.stream, true); }
  for (const name of ['a-initial', 'a-hidden', 'a-final']) {
    const r = records[name]; assert.equal(lastUser(r), command('a'), 'full SDK command actually expanded');
    assert.ok(system(r).includes('SDK-DIR-STYLE-A'), 'SDK outputStyle enters actual model prompt');
    assert.ok(system(r).includes('local-a:chosen'), 'selected local skill advertised');
    assert.ok(!system(r).includes('local-a:hidden')); assert.ok(!system(r).includes('SDK-DIR-STYLE-B'));
  }
  const recipe = tool(records['a-hidden'], 'call_sdk_dir_a');
  assert.ok(recipe.includes('SDK-DIR-SKILL-A'), 'real read_skill content');
  assert.ok(recipe.includes('/sdk-directory-plugins/local-a/skills/chosen/SKILL.md'), 'SDK-only recipe path');
  const hidden = tool(records['a-final'], 'call_sdk_dir_hidden');
  assert.ok(hidden.includes("Skill 'local-a:hidden' not found.")); assert.ok(!hidden.includes('SDK-DIR-HIDDEN-A'));
  for (const name of ['b-initial', 'b-final']) {
    const r = records[name]; assert.equal(lastUser(r), command('b'));
    assert.ok(system(r).includes('SDK-DIR-STYLE-B')); assert.ok(system(r).includes('local-b:chosen'));
    assert.ok(!system(r).includes('SDK-DIR-STYLE-A')); assert.ok(!system(r).includes('local-a:chosen'));
  }
  assert.ok(tool(records['b-final'], 'call_sdk_dir_b').includes('SDK-DIR-SKILL-B'));
  assert.ok(records['b-initial'].body.messages.some(m => m.role === 'assistant' && text(m.content) === 'SDK-DIR-A-DONE'), 'history survives SDK reconfigure');
  for (const [name, raw] of [['isolated', 'SDK-DIR-ISOLATED'], ['cleared', 'SDK-DIR-CLEARED']]) {
    assert.equal(lastUser(records[name]), raw);
    for (const marker of ['SDK-DIR-STYLE-A', 'SDK-DIR-STYLE-B', 'local-a:chosen', 'local-b:chosen']) assert.ok(!system(records[name]).includes(marker), `${name} leaked ${marker}`);
  }
  const shared = exact(inputs(a, 'sdk-dir-shared'), 3, 'durable shared inputs');
  assert.deepEqual(shared.map(row => text(row.messages?.[0]?.content)), [command('a'), command('b'), 'SDK-DIR-CLEARED'], 'durable input equals actual model input');
  assert.equal(text(exact(inputs(a, 'sdk-dir-isolated'), 1, 'durable isolated input')[0].messages?.[0]?.content), 'SDK-DIR-ISOLATED');
  for (const [i, marker] of [[2, 'SDK-DIR-A-DONE'], [5, 'SDK-DIR-ISOLATED-DONE'], [8, 'SDK-DIR-B-DONE'], [11, 'SDK-DIR-CLEARED-DONE']]) {
    assert.equal(a.steps[i].step.ws, 'read_session_messages');
    const r = exact(a.steps[i].frames.filter(f => f.type === 'response'), 1, `read ${i}`)[0];
    assert.ok(!r.error); assert.ok(JSON.stringify(r.result).includes(marker), 'actual WS persisted reply');
  }
  assert.ok(JSON.stringify(a.steps[12].frames).includes('SDK_PLUGIN_NOT_FOUND'), 'actual missing SDK directory error');
  assert.ok(!Object.values(a.provider).flat().some(r => r?.body?.stream === true && JSON.stringify(r.body.messages).includes('SDK-DIR-MISSING')));
  assert.ok(!Object.keys(a.tree).some(name => /\/plugins\/local-[ab]\//.test(name)), 'must not use auto-discovered user/project plugins');
  return { passed: true, side: a.side, streamedRequests: 7, sessionInputs: 4, snapshots: ['A', 'B', 'cleared'], mcp: 'not in this minimal scenario' };
}
const args = process.argv.slice(2);
if (args[0] === '--write-negative-scenario') {
  const [, kind, output] = args; assert.ok(output);
  const scene = JSON.parse(fs.readFileSync(defaultScenario));
  const relative = { 'wrong-command': 'commands/probe.md', 'wrong-skill': 'skills/chosen/SKILL.md', 'wrong-style': 'output-styles/blue.md' }[kind];
  assert.ok(relative, 'unknown negative'); const key = `home/.pilotdeck/sdk-directory-plugins/local-a/${relative}`;
  scene.files[key] = scene.files[key].replace(/SDK-DIR-(COMMAND|SKILL|STYLE)-A/g, 'SDK-DIR-ACTUAL-NEGATIVE');
  fs.writeFileSync(output, JSON.stringify(scene, null, 2) + '\n', { flag: 'wx' });
  console.log(JSON.stringify({ negativeScenario: output, kind, sha256: sha(fs.readFileSync(output)) }));
} else {
  const self = args[0] === '--self-test'; if (self) args.shift(); assert.ok(args[0], 'verify.mjs [--self-test] capture.json [captured-scenario.json]');
  const bytes = fs.readFileSync(args[0]), sceneBytes = fs.readFileSync(args[1] ?? defaultScenario);
  const artifact = JSON.parse(bytes), scene = JSON.parse(sceneBytes), sceneHash = sha(sceneBytes);
  const result = validate(artifact, scene, sceneHash);
  if (self) {
    const rules = [
      ['missing-provider', a => { a.provider['a-initial'] = []; }],
      ['raw-command', a => { a.provider['a-initial'][0].body.messages.filter(m => m.role === 'user').at(-1).content = '/local-a:probe SDK-DIR-PHASE-A'; }],
      ['lost-style', a => { for (const m of a.provider['a-initial'][0].body.messages.filter(m => m.role === 'system')) m.content = text(m.content).replaceAll('SDK-DIR-STYLE-A', 'MISSING-STYLE'); }],
      ['duplicate-model', a => { a.provider['b-initial'].push(structuredClone(a.provider['b-initial'][0])); }],
      ['lost-skill', a => { a.provider['a-hidden'][0].body.messages.find(m => m.tool_call_id === 'call_sdk_dir_a').content = 'missing'; }],
      ['lost-durable', a => { for (const v of Object.values(a.tree)) if (Array.isArray(v.jsonl)) v.jsonl = v.jsonl.filter(r => r.type !== 'accepted_input'); }],
      ['missing-config-error', a => { a.steps[12].frames = []; }],
    ];
    for (const [name, mutate] of rules) { const a = structuredClone(artifact); mutate(a); assert.throws(() => validate(a, scene, sceneHash), undefined, name); }
    result.verifierMutationChecks = rules.map(([name]) => name);
  }
  assert.equal(sha(fs.readFileSync(args[0])), sha(bytes), 'raw capture never rewritten');
  console.log(JSON.stringify({ ...result, artifactSha256: sha(bytes), scenarioSha256: sceneHash, verifierSha256: sha(fs.readFileSync(fileURLToPath(import.meta.url))) }));
}
