// Shared read-only comparison for the two fixed-port C1 capture formats.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { verifyCase as verifyCron, resolveSpec } from './capture.mjs';
import { verifyCase as verifyOnline } from '../c1-online/capture.mjs';
import { BASELINE, sha, equal, reader, rootMapper, mapStrings, differences,
  verifyRaw, snapshotProjection, emitReport } from '../c1-entry/compare-common.mjs';
const here = path.dirname(fileURLToPath(import.meta.url));
const fixedSides = ['ts', 'rust'];
const without = (value, keys) => Object.fromEntries(Object.entries(value).filter(([key]) => !keys.includes(key)));

export function mapCronSidePath(value, fixture, side) {
  const root = `${fixture}/${side}`;
  assert.ok(value === root || value.startsWith(root + '/'), 'cron isolated side environment');
  return `${fixture}/SIDE${value.slice(root.length)}`;
}

export function frameMapper(requests, map) {
  const ids = new Map();
  for (const [i, request] of requests.entries()) {
    assert.match(request.value.id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    assert.equal(ids.has(request.value.id), false, 'request UUID reused'); ids.set(request.value.id, `<RPC_${i}>`);
  }
  return raw => {
    const value = JSON.parse(raw);
    equal(JSON.stringify(value), raw, 'recorded fixture frame is canonical JSON; no unrecorded formatting loss');
    if (['request', 'response', 'event'].includes(value.type)) {
      assert.ok(ids.has(value.id), 'response/event/request references recorded request UUID');
      value.id = ids.get(value.id);
    }
    return map(JSON.stringify(value));
  };
}

function validateIdentity(kind, evidence, side, options, specs) {
  const identity = evidence.json('identity.json');
  equal(identity.node, 'v22.22.0', 'fixed Node version'); equal(identity.port, 18789);
  equal(identity.publishedPorts, 'declared none by E0');
  equal(identity[kind === 'cron' ? 'isolationDeclaration' : 'isolation'], 'private-no-published-ports');
  assert.match(identity.nodeSha256, /^[a-f0-9]{64}$/); assert.match(identity.platform, /^linux-/);
  assert.ok(identity.networkNamespace && Number.isInteger(identity.driverPid));
  const dir = path.join(here, kind === 'cron' ? '.' : '../c1-online');
  if (kind === 'cron') {
    equal(identity.casesSha256, sha(fs.readFileSync(path.join(dir, 'cases.json'))), 'captured cases match comparator input');
    equal(identity.driverSha256, sha(fs.readFileSync(path.join(dir, 'capture.mjs'))), 'captured driver matches verifier');
  } else for (const [name, digest] of Object.entries(identity.files)) equal(digest, sha(fs.readFileSync(path.join(dir, name))), `captured ${name} matches verifier`);
  if (kind === 'online') equal(Object.keys(identity.files).sort(), ['../c1-cron/capture.mjs', 'capture.mjs', 'cases.json', 'mock.mjs'].sort());
  if (kind === 'cron') equal(new Set(identity.sources.map(source => source.side)).size, identity.sources.length, 'unique source identities');
  const sources = kind === 'cron' ? Object.fromEntries(identity.sources.map(source => [source.side, source])) : identity.sources;
  assert.ok(sources[side], `missing ${side} source identity`);
  const source = sources[side], commit = source.sourceCommit ?? source.commit;
  equal(commit, side === 'ts' ? BASELINE : options['rust-sha'], 'candidate source identity');
  const executable = kind === 'cron' ? source.files.executable : { path: source.executable, sha256: source.sha256 };
  assert.match(executable.sha256, /^[a-f0-9]{64}$/);
  if (side === 'rust') equal(executable.sha256, options['rust-binary-sha256'], 'binary matches independent build receipt');
  if (side === 'ts') {
    const expected = kind === 'cron' ? ['executable', 'src/cli/pilotdeck.ts', 'src/gateway/client/probeServer.ts', 'src/gateway/client/GatewayWsClient.ts', 'src/gateway/client/RemoteGateway.ts']
      : ['src/cli/pilotdeck.ts', 'src/adapters/channel/cli/CliChannel.ts', 'src/gateway/client/GatewayWsClient.ts', 'src/gateway/client/probeServer.ts', 'dist/src/adapters/channel/cli/CliChannel.js', 'dist/src/gateway/client/GatewayWsClient.js', 'dist/src/gateway/client/probeServer.js'];
    equal(Object.keys(source.files).sort(), expected.sort(), 'complete captured baseline source/build hash set');
    for (const value of Object.values(source.files)) assert.match(kind === 'cron' ? value.sha256 : value, /^[a-f0-9]{64}$/);
  }
  const summary = evidence.json('summary.json');
  const sides = Object.keys(sources).sort();
  assert.ok(sides.length && sides.every(item => fixedSides.includes(item)));
  equal(summary.expectedRows, specs.length * sides.length); equal(summary.observedRows, summary.expectedRows);
  equal(summary.collectionPassed, true, 'failed capture cannot become a comparison pass');
  equal(summary.rows.map(row => `${row.case}:${row.side}`).sort(), specs.flatMap(spec => sides.map(item => `${spec.id}:${item}`)).sort(), 'complete exact capture rows');
  equal(summary.tsObservedHangs, summary.rows.filter(row => row.side === 'ts' && row.lifecycle === 'observed-hang').length);
  equal(summary.rustPassed, sides.includes('rust') ? summary.rows.filter(row => row.side === 'rust').every(row => row.productPassed) : null);
  return { identity, source, executable, summary, side };
}

function processAudit(kind, result, events, command) {
  const starts = events.filter(event => event.kind === 'process-start'), closes = events.filter(event => event.kind === 'process-close');
  equal(starts.length, 1, 'one process start'); equal(closes.length, 1, 'one process close');
  equal(starts[0].command, command.command); equal(starts[0].args, command.args);
  equal(closes[0].exitCode, result.exitCode); equal(closes[0].signal, result.signal);
  const pid = kind === 'cron' ? result.pid : result.identity.pid;
  assert.ok(Number.isInteger(pid) && pid > 0, 'recorded PID');
  if (kind === 'cron') { equal(starts[0].pid, pid); equal(starts[0].processGroup, pid); equal(result.processGroup, pid); }
  else { equal(starts[0].identity, result.identity); equal(starts[0].cwd, command.cwd); }
  for (const stream of ['stdout', 'stderr']) equal(events.filter(event => event.kind === 'output' && event.stream === stream).reduce((n, event) => n + event.bytes, 0), Buffer.byteLength(result[stream]), 'output event byte accounting');
  const signalKind = kind === 'cron' ? 'owned-process-signal' : 'owned-pid-signal';
  equal(events.filter(event => event.kind === signalKind).map(event => without(event, ['kind'])), result.signals, 'signals match process journal');
  const boundaries = events.filter(event => event.kind === 'observation-boundary');
  equal(boundaries.map(event => without(event, ['kind'])), result.boundary ? [result.boundary] : [], 'observation boundary matches process journal');
  const logicalKey = kind === 'cron' ? 'terminalAtMs' : 'logicalAt', logicalKind = kind === 'cron' ? 'terminal-output-observed' : 'logical-output-observed';
  equal(events.filter(event => event.kind === logicalKind).length, result[logicalKey] === null ? 0 : 1, 'logical output observation');
  const allowed = ['process-start', 'output', signalKind, 'observation-boundary', logicalKind, 'process-close'];
  assert.ok(events.every(event => allowed.includes(event.kind)), 'unknown/problem process event cannot be discarded');
  const targets = kind === 'online' ? [result.identity, ...result.descendants] : [];
  const signals = result.signals.map(signal => {
    if (kind === 'cron') { equal(signal.targetProcessGroup, pid, 'signal belongs to this child group'); return without(signal, ['atMs', 'targetProcessGroup']); }
    const target = targets.find(item => item.pid === signal.target.pid && item.startTicks === signal.target.startTicks);
    assert.ok(target, 'signal belongs to recorded child/descendant');
    return { ...without(signal, ['atMs', 'target']), target: target.pid === pid ? 'CLI_PID' : 'OWNED_DESCENDANT' };
  });
  return {
    exitCode: result.exitCode, signal: result.signal, startupMs: result.startupMs, observeMs: result.observeMs,
    logicalOutputObserved: result[logicalKey] !== null,
    boundary: result.boundary && without(result.boundary, ['atMs', 'descendants']), signals,
    ...(kind === 'online' ? { sentUserSignal: result.sentUserSignal, leftoverCount: result.leftovers.length } : {}),
    problem: result.problem, verdict: result.verdict,
  };
}

function networkProjection(kind, state, journal, map) {
  if (kind === 'cron') equal(state.events, journal, 'network state equals raw event journal');
  const received = journal.filter(event => event.kind === 'ws-receive').map(event => JSON.parse(event.raw));
  equal(received.filter(value => value.type === 'hello'), state.hellos.map(row => row.value), 'hello raw journal consistency');
  equal(received.filter(value => value.type === 'request'), state.requests.map(row => row.value), 'RPC raw journal consistency');
  for (const row of [...state.hellos, ...state.requests]) equal(JSON.parse(row.raw), row.value, 'frame value copy consistency');
  const frame = frameMapper(state.requests, map);
  const events = journal.map(event => {
    if (event.kind === 'ws-send' || event.kind === 'ws-receive') {
      return { ...mapStrings(without(event, ['at', 'atMs', 'raw']), map), raw: frame(event.raw) };
    }
    return mapStrings(without(event, ['at', 'atMs']), map);
  });
  const copied = without(state, ['events', 'hellos', 'requests', 'modelRequests']);
  const result = { ...copied, hellos: state.hellos.map(row => frame(row.raw)), requests: state.requests.map(row => frame(row.raw)), events };
  if (kind === 'online') {
    equal(journal.filter(event => event.kind === 'model-request').map(event => event.raw), state.modelRequests.map(row => row.raw), 'all auxiliary and main model requests retained');
    result.modelRequests = state.modelRequests.map(request => {
      const raw = map(request.raw), headers = { ...request.headers };
      if (headers['content-length'] !== undefined) {
        equal(headers['content-length'], String(Buffer.byteLength(request.raw)), 'HTTP content length matches raw body');
        headers['content-length'] = String(Buffer.byteLength(raw));
      }
      return { ...request, headers: mapStrings(headers, map), raw };
    });
  }
  return result;
}

export function projectCase(kind, evidence, captured, spec) {
  const prefix = `${spec.id}/${captured.side}`, command = evidence.json(`${prefix}/command.json`), result = evidence.json(`${prefix}/result.json`);
  verifyRaw(evidence, prefix, result);
  const resultKeys = ['exitCode', 'signal', 'startupMs', 'observeMs', 'elapsedMs', 'boundary', 'signals', 'problem', 'stdout', 'stderr', 'stdoutSha256', 'stderrSha256', 'network', 'verdict',
    ...(kind === 'cron' ? ['pid', 'processGroup', 'startedAt', 'terminalAtMs'] : ['identity', 'logicalAt', 'sentUserSignal', 'descendants', 'leftovers'])];
  equal(Object.keys(result).sort(), resultKeys.sort(), 'unknown/missing result surface requires explicit comparator review');
  equal(Object.keys(result.network).sort(), (kind === 'cron'
    ? ['events', 'problems', 'health', 'hellos', 'requests', 'closeReceived', 'openSockets']
    : ['health', 'hellos', 'requests', 'modelRequests', 'problems', 'closeReceived', 'pongs', 'openSockets']).sort(), 'unknown/missing network surface');
  const project = command.cwd, fixture = kind === 'cron' ? path.dirname(project) : path.dirname(project);
  assert.ok(path.isAbsolute(project));
  assert.ok(command.env.HOME && command.env.PILOT_HOME && command.env.TMPDIR, 'isolated command environment recorded');
  if (kind === 'cron') {
    equal(project.endsWith(`/${spec.id}/project`), true);
    equal(evidence.json(`${spec.id}/input.json`), resolveSpec(spec, project));
  } else { equal(project.endsWith(`/${spec.id}/${captured.side}/fixture/project`), true); equal(command.spec, spec); }
  equal(command.args, [...(captured.side === 'ts' ? [captured.executable.path] : []), ...(kind === 'cron' ? ['cron'] : []), ...spec.argv]);
  equal(command.command, captured.side === 'ts' ? captured.identity.nodeExecutable : captured.executable.path);
  const map = rootMapper(fixture);
  const verified = kind === 'cron'
    ? verifyCron(resolveSpec(spec, project), captured.side, result, result.network)
    : verifyOnline(spec, captured.side, result, result.network, project);
  equal(result.verdict, verified, 'recomputed driver assertions agree');
  equal(captured.summary.rows.find(row => row.case === spec.id && row.side === captured.side), { case: spec.id, side: captured.side, ...verified });
  const lifecycle = processAudit(kind, result, evidence.lines(`${prefix}/process.jsonl`), command);
  const network = networkProjection(kind, result.network, evidence.lines(`${prefix}/network.jsonl`), map);
  const env = { ...command.env };
  if (kind === 'cron') {
    // Cron uses a shared project but side-specific HOME under that same case.
    for (const key of ['HOME', 'USERPROFILE', 'CFFIXED_USER_HOME', 'PILOT_HOME', 'TMPDIR']) {
      env[key] = mapCronSidePath(env[key], fixture, captured.side);
    }
  }
  const projection = { command: { argv: command.args.slice(captured.side === 'ts' ? 1 : 0), cwd: map(project), env: mapStrings(env, map) },
    stdout: map(result.stdout), stderr: map(result.stderr), lifecycle, network };
  if (kind === 'online') for (const suffix of ['before', 'after']) projection[`files-${suffix}`] = snapshotProjection(evidence.json(`${prefix}/files-${suffix}.json`), map);
  return projection;
}

export function compareCaptured(kind, options) {
  const expectedCases = kind === 'cron' ? 14 : 8, projections = [];
  const report = { kind, mode: options.mode, expectedCases, observedCases: 0, status: 'INVALID', productPassed: false,
    acceptedDifferences: [], normalization: ['recorded isolated fixture paths only', 'RPC UUIDs mapped only at frame envelope id, preserving all references', 'journal observation clocks excluded; configured deadlines/lifecycle retained', 'process ownership validated before PID/start-tick removal', 'stdout chunk boundaries audited by byte count; full output compared', 'content-length checked against original body then recomputed after root mapping'],
    limitations: ['No cron/default-CLI hang exception is registered here.', 'Provider body timestamps/UUIDs and file contents are not broadly normalized.', 'Binary/source build relationship still requires the independent E0 build receipt.'], rows: [] };
  let left, right;
  try {
    const dir = path.join(here, kind === 'cron' ? '.' : '../c1-online'), specs = JSON.parse(fs.readFileSync(path.join(dir, 'cases.json')));
    equal(specs.length, expectedCases); equal(new Set(specs.map(spec => spec.id)).size, expectedCases);
    left = reader(options.left); right = reader(options.right);
    const a = validateIdentity(kind, left, 'ts', options, specs), b = validateIdentity(kind, right, options.mode === 'self' ? 'ts' : 'rust', options, specs);
    equal(a.identity.nodeSha256, b.identity.nodeSha256, 'same Node executable'); equal(a.identity.platform, b.identity.platform, 'same platform');
    if (options.mode === 'self') {
      equal(a.executable.sha256, b.executable.sha256, 'same baseline executable');
      equal(without(a.source.files, ['executable']), without(b.source.files, ['executable']), 'same baseline source hashes');
    }
    for (const spec of specs) {
      try {
        const first = projectCase(kind, left, a, spec), second = projectCase(kind, right, b, spec);
        const delta = differences(first, second); projections.push({ id: spec.id, left: first, right: second });
        report.rows.push({ id: spec.id, status: delta.length ? 'DIFF' : 'SAME', differences: delta,
          leftProductPassed: first.lifecycle.verdict.productPassed, rightProductPassed: second.lifecycle.verdict.productPassed });
      } catch (error) { report.rows.push({ id: spec.id, status: 'INVALID', error: error.message }); }
    }
    report.observedCases = report.rows.length;
    report.status = report.rows.some(row => row.status === 'INVALID') ? 'INVALID' : report.rows.some(row => row.status === 'DIFF') ? 'DIFF' : 'SAME';
    report.productPassed = report.status === 'SAME' && report.rows.every(row => row.leftProductPassed && row.rightProductPassed);
    report.identities = { left: a.identity, right: b.identity };
  } catch (error) { report.error = error.message; }
  report.inputHashes = { left: left?.hashes, right: right?.hashes };
  return emitReport(options, report, projections);
}
