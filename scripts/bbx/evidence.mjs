// Run identity and validation are separate from behavioral normalization.
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { isDeepStrictEqual } from 'node:util';
import { fileURLToPath } from 'node:url';
import { syntaxContract } from './syntax-policy.mjs';
import { acceptedPositiveContract } from './accepted-positive-policy.mjs';
import { cliShutdownEvidence } from './cli-shutdown-policy.mjs';
import { verifyConfigWatchInput } from './fixtures/config-watch-input.mjs';
import { validateConfigWatchExactArtifact } from './config-watch-exact-policy.mjs';
import { nativeDependencyIdentity } from './native-identity.mjs';
import { validatePersistedRunTriggers } from './serve-persisted-event.mjs';
import { channelPollContract } from './channel-poll-policy.mjs';
import { sdkAgentContract } from './sdk-agent-policy.mjs';
import { validateSdkSignalArtifact, SDK_SIGNAL_POLICY_ID, SDK_SIGNAL_TERMINAL_POLICY_ID } from './sdk-signal-policy.mjs';

export const here = path.dirname(fileURLToPath(import.meta.url));
export const repo = path.resolve(here, '../..');
export const sha256 = (value) => createHash('sha256').update(value).digest('hex');
export const fileHash = (file) => sha256(fs.readFileSync(file));
const command = (cmd, args, cwd) => {
  const r = spawnSync(cmd, args, { cwd, encoding: 'utf8' });
  return r.status === 0 && !r.signal ? r.stdout.trim() : null;
};
const untrackedIdentity = (cwd) => {
  const names = command('git', ['ls-files', '--others', '--exclude-standard', '-z'], cwd);
  return sha256((names ?? '').split('\0').filter((name) => /\.(rs|ts|mts|js|mjs)$|(^|\/)Cargo\.(toml|lock)$/.test(name)).sort().map((name) => `${name}:${fileHash(path.join(cwd, name))}`).join('\n'));
};
const gitIdentity = (cwd) => ({ untracked: untrackedIdentity(cwd), commit: command('git', ['rev-parse', 'HEAD'], cwd),
  // Include tracked changes in the identity; test runs may legitimately precede a commit.
  diff: sha256(command('git', ['diff', 'HEAD', '--', '.', ':!state', ':!docs'], cwd) ?? '') });
const treeHash = (root) => {
  if (!fs.existsSync(root)) return null;
  const hash = createHash('sha256');
  const walk = (dir) => {
    for (const e of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      const file = path.join(dir, e.name);
      if (e.isDirectory()) walk(file);
      else { hash.update(path.relative(root, file)); hash.update(e.isSymbolicLink() ? fs.readlinkSync(file) : fs.readFileSync(file)); }
    }
  };
  walk(root); return hash.digest('hex');
};
const SDK_WIRE_DRIVER_SHA = '6b29ef61cf58a1403dbfc15e55f057c65ee8b1ef0afa3272f48a7a41d2d8926a';
const SDK_WIRE_VERIFY_SHA = 'e4e2d422e7b96d2b67219fec81510edb329e0566b308752d95add440ef1edc87';
const SDK_WIRE_CASES = ['final-and-abort-batch','final-and-abort-next-turn','final-and-abort-separated','wrong-run-response','malformed-response','abort-rpc-error','connection-drop','abort-timeout'];
const insistWire = (condition, message) => { if (!condition) throw Error(`sdk signal wire evidence: ${message}`); };
export function loadSdkSignalWireEvidence({ binarySha256, expectedIdentity } = {}) {
  insistWire(/^[a-f0-9]{64}$/.test(binarySha256 ?? ''), 'formal binary SHA required');
  const directory = process.env.BBX_SDK_SIGNAL_WIRE_DIR;
  const receiptFile = process.env.BBX_SDK_SIGNAL_WIRE_RECEIPT;
  insistWire(directory && receiptFile && path.isAbsolute(directory) && path.isAbsolute(receiptFile), 'absolute wire directory and original receipt file required');
  const root = fs.realpathSync(directory), receipt = fs.realpathSync(receiptFile);
  const fixture = path.join(repo, 'src/pilotdeck-cli/tests/fixtures/sdk-signal-abort-order');
  const driver = path.join(fixture, 'driver.mjs'), verifier = path.join(fixture, 'verify.mjs');
  insistWire(fileHash(driver) === SDK_WIRE_DRIVER_SHA && fileHash(verifier) === SDK_WIRE_VERIFY_SHA, 'trusted driver/verifier source changed');
  const names = ['identity.json','summary.json', ...SDK_WIRE_CASES.flatMap(name => ['summary.json','timeline.jsonl','stdout.txt','stderr.txt'].map(file => `${name}/${file}`))].sort();
  const snapshot = () => Object.fromEntries(names.map(name => {
    const file = path.join(root, name), stat = fs.lstatSync(file);
    insistWire(stat.isFile() && !stat.isSymbolicLink(), `raw evidence must be a regular file: ${name}`);
    return [name, fileHash(file)];
  }));
  const before = snapshot(), receiptHash = fileHash(receipt);
  const capturedIdentity = JSON.parse(fs.readFileSync(path.join(root, 'identity.json'), 'utf8'));
  insistWire(capturedIdentity.cliSha256 === binarySha256 && capturedIdentity.driverSha256 === SDK_WIRE_DRIVER_SHA, 'raw wire binary/producer identity mismatch');
  const result = spawnSync(process.execPath, [verifier, root, binarySha256], { encoding: 'utf8', timeout: 60_000, maxBuffer: 1_048_576 });
  insistWire(result.status === 0 && !result.signal && !result.error && result.stderr === '', `strict raw verifier failed: ${result.stderr || result.error?.message || result.signal || result.status}`);
  const checked = JSON.parse(result.stdout), original = JSON.parse(fs.readFileSync(receipt, 'utf8'));
  insistWire(isDeepStrictEqual(Object.keys(checked).sort(), ['binary','cases','status']) && checked.status === 'PASS'
    && checked.binary === binarySha256 && isDeepStrictEqual(checked.cases, SDK_WIRE_CASES), 'strict verifier receipt shape/cases mismatch');
  insistWire(isDeepStrictEqual(original, checked), 'original receipt does not match strict raw replay');
  insistWire(isDeepStrictEqual(before, snapshot()) && receiptHash === fileHash(receipt)
    && fileHash(driver) === SDK_WIRE_DRIVER_SHA && fileHash(verifier) === SDK_WIRE_VERIFY_SHA, 'wire evidence changed while being reverified');
  const identity = { schema: 1, directory: root, binarySha256,
    driver: { path: driver, sha256: SDK_WIRE_DRIVER_SHA }, verifier: { path: verifier, sha256: SDK_WIRE_VERIFY_SHA },
    receipt: { path: receipt, sha256: receiptHash }, files: before };
  if (expectedIdentity !== undefined) insistWire(isDeepStrictEqual(identity, expectedIdentity), 'wire files/source/receipt identity changed since formal capture');
  return { receipt: checked, identity };
}
export function sdkSignalAcceptedEntry(entry) {
  return ((entry?.policy === 'sdkSignalAbortOrder' && entry.id === SDK_SIGNAL_POLICY_ID)
    || (entry?.policy === 'sdkSignalTerminalObservation' && entry.id === SDK_SIGNAL_TERMINAL_POLICY_ID))
    && (entry.paths === undefined || isDeepStrictEqual(entry.paths, [])) && entry.config === undefined;
}
function signalWireForArtifact(artifact, inputs = artifact.evidence?.inputs) {
  const binary = inputs?.binaries?.pilotdeck;
  const binarySha256 = typeof binary === 'string' ? binary : binary?.sha256;
  insistWire(inputs?.sdkSignalWire, 'formal input identity is missing reverified wire evidence');
  return loadSdkSignalWireEvidence({ binarySha256, expectedIdentity: inputs.sdkSignalWire });
}
export function validateSdkSignalAcceptedEvidence(artifact, scenario, inputs = artifact.evidence?.inputs) {
  validateSdkSignalArtifact(artifact, scenario, artifact.side);
  const executed = assertionResults(scenario.assertions, artifact, artifact.side);
  insistWire(isDeepStrictEqual(artifact.evidence?.assertions, executed)
    && executed.every(result => result.skipped || result.passed), 'required named scenario assertions were not executed or failed');
  if (artifact.side === 'rust') return signalWireForArtifact(artifact, inputs);
  return null;
}
export function inputIdentity() {
  const baseline = process.env.BBX_BASELINE || path.join(repo, 'resources/PilotDeck');
  const target = process.env.BBX_TARGET || path.join(repo, 'src/target/debug');
  const binaries = Object.fromEntries(['pilotdeck', 'pilotdeck-agent-loop-sidecar'].map((name) => {
    const file = path.join(target, name); return [name, fs.existsSync(file) ? fileHash(file) : null];
  }));
  const wireConfigured = process.env.BBX_SDK_SIGNAL_WIRE_DIR || process.env.BBX_SDK_SIGNAL_WIRE_RECEIPT;
  const sdkSignalWire = wireConfigured ? loadSdkSignalWireEvidence({ binarySha256: binaries.pilotdeck }).identity : undefined;
  return {
    rust: gitIdentity(process.env.BBX_SRC || path.resolve(target, '../..')),
    ts: gitIdentity(baseline),
    binaries,
    ...(sdkSignalWire ? { sdkSignalWire } : {}),
    components: { pdf: { path: process.env.BBX_PDF_TOOL ?? null,
      sha256: process.env.BBX_PDF_TOOL && fs.existsSync(process.env.BBX_PDF_TOOL) ? fileHash(process.env.BBX_PDF_TOOL) : null } },
    dependencies: nativeDependencyIdentity(baseline, process.env.BBX_IMAGE_TOOL),
    baselineDist: treeHash(path.join(baseline, 'dist')),
    baselineLock: fs.existsSync(path.join(baseline, 'pnpm-lock.yaml')) ? fileHash(path.join(baseline, 'pnpm-lock.yaml')) : null,
    baselineEntry: fs.existsSync(path.join(baseline, 'dist/src/cli/pilotdeck.js')) ? fileHash(path.join(baseline, 'dist/src/cli/pilotdeck.js')) : null,
    harness: Object.fromEntries(['check.mjs', 'run.mjs', 'diff.mjs', 'evidence.mjs', 'mock-support.mjs', 'syntax-policy.mjs', 'native-identity.mjs', 'serve-persisted-event.mjs', 'channel-poll-policy.mjs', 'sdk-agent-policy.mjs', 'sdk-signal-policy.mjs', 'ao-discovery-prefix-policy.mjs', 'observer-lifecycle-policy.mjs', 'observer-memory-policy.mjs', 'observer-uuid-policy.mjs', 'component-supply.mjs', 'accepted-positive-policy.mjs', 'cli-shutdown-policy.mjs', 'fixtures/config-watch-input.mjs', 'config-watch-exact-policy.mjs', 'normalization.mjs', 'observer-provenance.mjs', 'observer-supplement-policy.mjs', 'observer-id-links-policy.mjs', 'observer-memory-witness.mjs', 'duration-file-size-policy.mjs', 'feishu-stop-policy.mjs', 'feishu-stop-policy-constants.mjs', 'diagnostics-policy.mjs', 'diagnostics-policy-contract.json', 'memory-tree-order-policy.mjs', 'fixtures/memory-tree-order/closure-receipt.json.br'].map((name) => [name, fileHash(path.join(here, name))])),
    tools: { node: process.version, executable: fileHash(process.execPath), rust: command('rustc', ['--version'], repo), pnpm: command('pnpm', ['--version'], baseline) },
    platform: { os: process.platform, arch: process.arch, release: os.release() },
  };
}
export function scenarioIdentity(file) {
  return { name: path.basename(file, '.json'), path: path.resolve(file), sha256: fileHash(file) };
}
export function checkWorker(expected, result) {
  const errors = [];
  let rows = [];
  for (const line of result.stdout.split('\n').filter(Boolean)) {
    try { const row = JSON.parse(line); if (!row || typeof row.name !== 'string') throw Error('invalid row'); rows.push(row); }
    catch { errors.push('malformed worker result'); }
  }
  const names = new Set(expected.map((s) => s.name));
  for (const name of names) if (rows.filter((r) => r.name === name).length !== 1) errors.push(`missing or duplicate scenario: ${name}`);
  for (const row of rows) if (!names.has(row.name)) errors.push(`unexpected scenario: ${row.name}`);
  // Exit 1 with complete failed rows is the worker reporting observed failures.
  // A nonzero exit without those rows (or even with all SAME rows) is a harness failure.
  const reportedFailure = result.code === 1 && !errors.length && rows.some((r) => r.status !== 'SAME');
  if ((result.code !== 0 && !reportedFailure) || result.signal || result.error) errors.push(`worker exit=${result.code} signal=${result.signal ?? '-'} ${result.error ?? ''}`);
  return { rows, errors };
}
export function assertionResults(assertions, artifact, side) {
  const ids = new Set();
  return (assertions ?? []).map((a) => {
    if (!a.id || ids.has(a.id) || typeof a.path !== 'string' || !a.path.startsWith('/')) throw Error('assertions require unique id and JSON pointer path');
    ids.add(a.id);
    if (a.sides !== undefined && (!Array.isArray(a.sides) || !a.sides.length || a.sides.some((s) => !['ts', 'rust'].includes(s)))) throw Error(`assertion ${a.id} has invalid sides`);
    if (side && a.sides && !a.sides.includes(side)) return { id: a.id, skipped: true };
    const actual = a.path.slice(1).split('/').map((p) => p.replaceAll('~1', '/').replaceAll('~0', '~')).reduce((v, p) => v?.[p], artifact);
    const checks = [];
    if ('equals' in a) checks.push(isDeepStrictEqual(actual, a.equals));
    if ('exists' in a) checks.push((actual !== undefined) === a.exists);
    if ('includes' in a) checks.push(typeof actual === 'string' && actual.includes(a.includes));
    if ('matches' in a) checks.push(typeof actual === 'string' && new RegExp(a.matches).test(actual));
    if ('imageAttachments' in a) checks.push(imageAttachmentsMatch(actual, a.imageAttachments));
    if ('acceptedPositive' in a) {
      try {
        if (a.path !== '/steps') throw Error('accepted positive contracts require /steps');
        acceptedPositiveContract(artifact, a.acceptedPositive); checks.push(true);
      } catch { checks.push(false); }
    }
    if ('configWatchInput' in a) {
      try {
        if (a.path !== '/steps' || a.configWatchInput !== true) throw Error('invalid config watch contract');
        verifyConfigWatchInput(artifact); checks.push(true);
      } catch { checks.push(false); }
    }
    if ('cliShutdownCompleted' in a) {
      try {
        if (a.path !== '/steps' || a.cliShutdownCompleted !== true) throw Error('invalid CLI shutdown contract');
        cliShutdownEvidence(artifact); checks.push(true);
      } catch { checks.push(false); }
    }
    if ('syntaxDiagnostics' in a) {
      try { syntaxContract(artifact, a.syntaxDiagnostics, side); checks.push(true); } catch { checks.push(false); }
    }
    if (!checks.length) throw Error(`assertion ${a.id} has no predicate`);
    return { id: a.id, passed: checks.every(Boolean) };
  });
}
export function imageAttachmentsMatch(actual, expected) {
  if (!actual || typeof actual !== 'object' || !Number.isInteger(expected.count) || expected.count < 0) return false;
  const attachments = [];
  const walk = (value) => {
    if (!value || typeof value !== 'object') return;
    if (value.type === 'image_url') attachments.push(value.image_url?.url);
    for (const child of Object.values(value)) walk(child);
  };
  walk(actual);
  if (attachments.length !== expected.count) return false;
  return attachments.every((url) => {
    const match = typeof url === 'string' && /^data:([^;,]+);base64,([A-Za-z0-9+/]*={0,2})$/.exec(url);
    if (!match || match[1] !== expected.mime || !Number.isInteger(expected.maxBytes) || expected.maxBytes <= 0) return false;
    const bytes = Buffer.from(match[2], 'base64');
    return bytes.length > 0 && bytes.length <= expected.maxBytes && bytes.toString('base64') === match[2]
      && (expected.bytes === undefined || bytes.length === expected.bytes)
      && (expected.sha256 === undefined || sha256(bytes) === expected.sha256);
  });
}
export function validateArtifact(artifact, scenario, expected) {
  const errors = [];
  const evidence = artifact.evidence;
  try { validateConfigWatchExactArtifact(artifact); } catch (error) { errors.push(`fixed settings exact evidence: ${error.message}`); }
  if (!evidence || evidence.schema !== 1 || evidence.runId !== expected.runId || evidence.side !== expected.side
    || !isDeepStrictEqual(evidence.scenario, expected.scenario) || !isDeepStrictEqual(evidence.inputs, expected.inputs)) errors.push('artifact identity mismatch');
  if (!Array.isArray(artifact.failures)) errors.push('missing failures collection');
  else errors.push(...artifact.failures.filter((f) => !f.expected).map((f) => `${f.at}:${f.kind}`));
  if (!Array.isArray(artifact.steps) || (!artifact.startFailure && artifact.steps.length !== scenario.steps.length)) errors.push('missing or duplicate steps');
  if (artifact.startFailure && scenario.expectStartFailure !== true) errors.push('unexpected start failure');
  if (scenario.steps.some(step => step.untilPersistedRunEvent)) errors.push(...validatePersistedRunTriggers(artifact, scenario));
  const assertions = assertionResults(scenario.assertions, artifact, expected.side);
  if (!isDeepStrictEqual(evidence?.assertions, assertions)) errors.push('required assertions were not executed');
  if (assertions.some((a) => !a.skipped && !a.passed)) errors.push('required assertion failed');
  if (!isDeepStrictEqual(artifact.accepted ?? [], scenario.accepted ?? [])) errors.push('accepted policy mismatch');
  if (!isDeepStrictEqual(artifact.volatilePolicies ?? [], scenario.volatilePolicies ?? [])) errors.push('volatile value policy mismatch');
  for (const policy of scenario.volatilePolicies ?? []) {
    try {
      if (isDeepStrictEqual(policy, { policy: 'coreChannelPollAfter' })) channelPollContract(artifact, policy);
      else if (isDeepStrictEqual(policy, { policy: 'sdkAgent03DerivedValues' })) sdkAgentContract(artifact, policy,
        process.env.BBX_BASELINE || path.resolve(here, '../../resources/PilotDeck'));
      else errors.push('unsupported volatile value policy');
    } catch (error) { errors.push(error.message); }
  }
  for (const accepted of scenario.accepted ?? []) if (['sdkSignalAbortOrder','sdkSignalTerminalObservation'].includes(accepted.policy)) {
    try {
      if (!sdkSignalAcceptedEntry(accepted)) throw Error('invalid SDK signal accepted declaration');
      validateSdkSignalAcceptedEvidence(artifact, scenario, expected.inputs);
    } catch (error) { errors.push(error.message); }
  }
  if (expected.side === 'rust') for (const accepted of scenario.accepted ?? []) {
    if (!Array.isArray(accepted.assertions) || !accepted.assertions.length || accepted.assertions.some((id) => !assertions.some((a) => a.id === id && a.passed))) errors.push(`accepted difference ${accepted.id} lacks executed positive assertions`);
    if (accepted.policy === 'syntaxDiagnostics' && !scenario.assertions?.some((a) => accepted.assertions.includes(a.id)
      && isDeepStrictEqual(a.syntaxDiagnostics, accepted.config))) errors.push('syntax policy lacks matching contract assertion');
  }
  return errors;
}
export function loadRun(outDir, { verifyCurrent = true } = {}) {
  const manifest = JSON.parse(fs.readFileSync(path.join(outDir, 'manifest.json'), 'utf8'));
  if (manifest.errors?.length) throw Error('run has validation errors');
  if (manifest.schema !== 1 || !manifest.complete || manifest.mode !== 'compare') throw Error('not a completed comparison run');
  const rows = manifest.results;
  if (!Array.isArray(rows) || rows.length !== manifest.scenarios.length || new Set(rows.map((r) => r.name)).size !== rows.length) throw Error('incomplete or duplicate run results');
  for (const scenario of manifest.scenarios) {
    const row = rows.find((r) => r.name === scenario.name);
    if (!row) throw Error(`missing scenario ${scenario.name}`);
    if (verifyCurrent && fileHash(scenario.path) !== scenario.sha256) throw Error(`scenario changed: ${scenario.name}`);
    for (const [file, hash] of Object.entries(row.files ?? {})) if (fileHash(path.resolve(outDir, file)) !== hash) throw Error(`artifact changed: ${file}`);
    if (row.status === 'SAME') {
      if (!row.artifacts?.length || row.artifacts.length !== 2 || !row.report || [...row.artifacts, row.report].some((f) => !row.files?.[f])) throw Error(`missing evidence: ${row.name}`);
      const spec = JSON.parse(fs.readFileSync(scenario.path, 'utf8'));
      for (const [i, file] of row.artifacts.entries()) {
        const errors = validateArtifact(JSON.parse(fs.readFileSync(path.resolve(outDir, file), 'utf8')), spec,
          { runId: manifest.runId, side: i ? 'rust' : 'ts', scenario, inputs: manifest.inputs });
        if (errors.length) throw Error(`${row.name}: ${errors.join('; ')}`);
      }
    }
  }
  const passed = rows.filter((r) => r.status === 'SAME').length;
  if (manifest.counts?.passed !== passed || manifest.counts?.failed !== rows.length - passed) throw Error('result counts mismatch');
  if (verifyCurrent && !isDeepStrictEqual(inputIdentity(), manifest.inputs)) throw Error('source, binary, tools or harness changed since run');
  return manifest;
}
