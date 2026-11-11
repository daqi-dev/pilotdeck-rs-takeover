#!/usr/bin/env node
// 黑盒对拍 · 采集器。
// 用法：node scripts/bbx/run.mjs <ts|rust> <scenario.json> <out.json>
//
// 做的事只有一件：在一个固定路径的干净隔离目录里，按场景写入同样的初始文件，
// 起一个假 provider 和被测一侧的进程，逐步发送同样的输入，把对外可观察的输出
// 原样记下来：WS 帧、HTTP 响应、CLI 的 stdout/stderr/退出码、发给 provider 的请求、
// 结束后的落盘文件。这里不做任何归一化和比较，那是 diff.mjs 的事。
//
// 两侧使用同一个根目录路径和同一组端口（先后跑，不并行），所以路径和端口在两侧
// 的输出里天然相同，不需要归一化。
import { spawn } from 'node:child_process';
import http from 'node:http';
import net from 'node:net';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const BASELINE = process.env.BBX_BASELINE || path.join(REPO, 'resources/PilotDeck');
const TARGET = process.env.BBX_TARGET || path.join(REPO, 'src/target/debug');
export const ROOT = '/private/tmp/pdrs-bbx/root';
const GATEWAY_PORT = 38411;
const MOCK_PORT = 38412;
const TOKEN = 'bbx-token';
const STEP_TIMEOUT_MS = 30000;

const [side, scenarioPath, outPath] = process.argv.slice(2);
if (!['ts', 'rust'].includes(side) || !scenarioPath || !outPath) {
  console.error('usage: run.mjs <ts|rust> <scenario.json> <out.json>');
  process.exit(2);
}
const scenario = JSON.parse(fs.readFileSync(scenarioPath, 'utf8'));
const home = path.join(ROOT, 'home');
const pilotHome = path.join(home, '.pilotdeck');
const project = path.join(ROOT, 'proj');
const tmp = path.join(ROOT, 'tmp');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const subst = (text) => text
  .replaceAll('{ROOT}', ROOT).replaceAll('{HOME}', home).replaceAll('{PILOT_HOME}', pilotHome)
  .replaceAll('{PROJECT}', project).replaceAll('{MOCK}', `http://127.0.0.1:${MOCK_PORT}`)
  .replaceAll('{GATEWAY}', `http://127.0.0.1:${GATEWAY_PORT}`).replaceAll('{TOKEN}', TOKEN);
const substDeep = (v) => typeof v === 'string' ? subst(v)
  : Array.isArray(v) ? v.map(substDeep)
  : v && typeof v === 'object' ? Object.fromEntries(Object.entries(v).map(([k, x]) => [k, substDeep(x)]))
  : v;

// answer 模板：字符串里的 {event.a.b} 换成触发事件的对应字段；整个字符串就是一个占位符时保留原类型。
const fillEvent = (v, event) => {
  const pick = (p) => p.split('.').reduce((o, k) => (o == null ? undefined : o[k]), event);
  if (typeof v === 'string') {
    const whole = /^\{event\.([\w.]+)\}$/.exec(v);
    if (whole) return pick(whole[1]);
    return v.replace(/\{event\.([\w.]+)\}/g, (_, p) => String(pick(p)));
  }
  if (Array.isArray(v)) return v.map((x) => fillEvent(x, event));
  if (v && typeof v === 'object') return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, fillEvent(x, event)]));
  return v;
};

// ── Rust 二进制不能比源码旧：src/ 下（不含 target/）的 .rs、Cargo.toml、Cargo.lock 有比它新的就拒跑 ──
// 有 cargo 的依赖清单 <二进制>.d 时，.rs 只看清单里列出的（改测试文件不会让二进制重链）。BBX_ALLOW_STALE=1 跳过。
if (side === 'rust' && process.env.BBX_ALLOW_STALE !== '1') {
  const src = process.env.BBX_SRC || path.resolve(TARGET, '../..');
  const sources = (dir) => fs.readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = path.join(dir, e.name);
    return e.isDirectory() ? (p === path.join(src, 'target') ? [] : sources(p)) : /\.rs$|^Cargo\.(toml|lock)$/.test(e.name) ? [p] : [];
  });
  const all = sources(src);
  for (const bin of ['pilotdeck', 'pilotdeck-agent-loop-sidecar'].map((b) => path.join(TARGET, b)).filter((b) => fs.existsSync(b))) {
    let deps = null; try { deps = new Set(fs.readFileSync(bin + '.d', 'utf8').split(/(?<!\\)\s+/)); } catch {}
    const built = fs.statSync(bin).mtimeMs;
    const newer = all.filter((f) => (!deps || !f.endsWith('.rs') || deps.has(f)) && fs.statSync(f).mtimeMs > built);
    if (newer.length) {
      console.error(`bbx: ${bin} is older than ${newer.length} source file(s), e.g. ${newer[0]}; build first (or BBX_ALLOW_STALE=1)`);
      process.exit(4);
    }
  }
}

// ── 独占：同一时刻只允许一次采集（固定路径和端口的代价） ──
fs.mkdirSync(path.dirname(ROOT), { recursive: true });
const lock = path.join(path.dirname(ROOT), 'lock');
try { fs.mkdirSync(lock); } catch {
  const owner = Number(fs.readFileSync(path.join(lock, 'pid'), 'utf8').trim() || 0);
  let alive = false; try { process.kill(owner, 0); alive = true; } catch {}
  if (alive) { console.error(`bbx: another run holds ${lock} (pid ${owner})`); process.exit(3); }
}
fs.writeFileSync(path.join(lock, 'pid'), String(process.pid));
const children = [];
const cleanup = () => {
  for (const c of children) { try { process.kill(-c.pid, 'SIGKILL'); } catch {} }
  try { fs.rmSync(lock, { recursive: true, force: true }); } catch {}
};
process.on('exit', cleanup);
for (const s of ['SIGINT', 'SIGTERM']) process.on(s, () => process.exit(130));

const portFree = (port) => new Promise((resolve) => {
  const s = net.createServer().once('error', () => resolve(false))
    .once('listening', () => s.close(() => resolve(true))).listen(port, '127.0.0.1');
});
// 基线 CLI 的若干路径会探测写死的默认端口 18789（真实 PilotDeck 的默认端口）。
// 那里有人监听时一律拒跑，免得碰到本机正在运行的实例。用 lsof 看，不去连它。
import { spawnSync } from 'node:child_process';
if (spawnSync('lsof', ['-nP', '-iTCP:18789', '-sTCP:LISTEN'], { encoding: 'utf8' }).stdout.trim()) {
  console.error('bbx: something is listening on 18789 (a real PilotDeck?); refusing to run'); process.exit(5);
}
for (const p of [GATEWAY_PORT, MOCK_PORT]) {
  if (!(await portFree(p))) { console.error(`bbx: port ${p} is in use`); process.exit(3); }
}

// ── 初始状态：两侧完全一样 ──
fs.rmSync(ROOT, { recursive: true, force: true });
for (const d of [pilotHome, project, tmp]) fs.mkdirSync(d, { recursive: true });
const DEFAULT_CONFIG = `schemaVersion: 1
agent:
  model: bbx/deterministic
  maxContextTokens: 65536
  maxOutputTokens: 8192
model:
  providers:
    bbx:
      protocol: openai
      url: {MOCK}/v1
      apiKey: bbx-mock-only
      models:
        deterministic:
          capabilities:
            supportsToolUse: true
            maxContextTokens: 65536
            maxOutputTokens: 8192
telemetry:
  enabled: false
`;
if (scenario.config !== null) {
  fs.writeFileSync(path.join(pilotHome, 'pilotdeck.yaml'), subst(scenario.config ?? DEFAULT_CONFIG));
}
for (const [rel, content] of Object.entries(scenario.files ?? {})) {
  const target = path.join(ROOT, rel);
  if (!target.startsWith(ROOT + '/')) throw new Error(`file escapes root: ${rel}`);
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.writeFileSync(target, typeof content === 'string' ? subst(content) : JSON.stringify(substDeep(content), null, 2) + '\n');
}

// ── 假 provider（OpenAI chat completions 形状）──
// 规则按顺序匹配请求体的 JSON 文本：when（全部包含）/ unless（全部不包含）。
// 规则里的字符串（包括工具参数）同样做 {ROOT} 等占位符替换。
// 请求按命中的规则名分组记录，这样互不相干的两路请求（如标题生成与主对话）
// 谁先到不影响比较。没有规则命中时用 default，回一段固定文本。
const rules = [...substDeep(scenario.provider ?? []), { name: 'default', reply: { text: 'BBX_OK' } }];
const providerRequests = {};
const mock = http.createServer(async (req, res) => {
  const parts = []; for await (const p of req) parts.push(p);
  const raw = Buffer.concat(parts).toString('utf8');
  let body = null; try { body = JSON.parse(raw); } catch {}
  const rule = rules.find((r) => (r.when ?? []).every((s) => raw.includes(s)) && (r.unless ?? []).every((s) => !raw.includes(s)));
  (providerRequests[rule.name] ??= []).push({
    method: req.method, url: req.url, headers: req.headers, body: body ?? { raw },
  });
  const reply = rule.reply ?? {};
  if (reply.status) { res.writeHead(reply.status, { 'content-type': 'application/json' }); res.end(JSON.stringify(reply.body ?? {})); return; }
  const message = reply.tool
    ? { role: 'assistant', content: null, tool_calls: [{ id: reply.tool.id ?? 'call_bbx_1', type: 'function', function: { name: reply.tool.name, arguments: JSON.stringify(reply.tool.arguments ?? {}) } }] }
    : { role: 'assistant', content: reply.text ?? 'BBX_OK' };
  const finish = reply.tool ? 'tool_calls' : 'stop';
  const usage = { prompt_tokens: 12, completion_tokens: 4, total_tokens: 16 };
  if (!body?.stream) {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ id: 'chatcmpl-bbx', model: body?.model, choices: [{ index: 0, message, finish_reason: finish }], usage }));
    return;
  }
  res.writeHead(200, { 'content-type': 'text/event-stream' });
  const delta = message.tool_calls
    ? { role: 'assistant', tool_calls: message.tool_calls.map((t) => ({ index: 0, ...t })) }
    : { role: 'assistant', content: message.content };
  res.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta, finish_reason: null }] })}\n\n`);
  res.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: {}, finish_reason: finish }], usage })}\n\n`);
  res.end('data: [DONE]\n\n');
});
await new Promise((r) => mock.listen(MOCK_PORT, '127.0.0.1', r));

// ── 被测一侧 ──
const env = {
  PATH: process.env.PATH, HOME: home, PILOT_HOME: pilotHome, TMPDIR: tmp,
  TZ: 'UTC', LANG: 'en_US.UTF-8', NO_COLOR: '1',
  PILOTDECK_GATEWAY_TOKEN: TOKEN, PILOTDECK_GATEWAY_PORT: String(GATEWAY_PORT), PILOTDECK_SKIP_BROWSER_OPEN: '1',
  // 内置 skills 目录两侧指向同一处（基线自带的那份），否则 Rust 的开发构建找不到它。
  PILOTDECK_BUNDLED_SKILLS_DIR: path.join(BASELINE, 'skills'),
  // 内置插件目录：基线固定用自己安装位置下的这一处（loadBuiltinPlugins.ts），不认这个变量；
  // Rust 没有随二进制带插件，靠它指到基线那份。
  PILOTDECK_BUNDLED_PLUGINS_DIR: path.join(BASELINE, 'dist/src/extension/plugins/builtin'),
  ...substDeep(scenario.env ?? {}),
};
const SIDES = {
  ts: {
    server: ['node', path.join(BASELINE, 'dist/src/cli/pilotdeck.js'), 'server', '--port', String(GATEWAY_PORT)],
    cli: ['node', path.join(BASELINE, 'dist/src/cli/pilotdeck.js')],
  },
  rust: {
    server: [path.join(TARGET, 'pilotdeck'), 'server', '--port', String(GATEWAY_PORT)],
    cli: [path.join(TARGET, 'pilotdeck')],
  },
};
// volatile 原样带进产物：diff.mjs 从产物里读它（两侧相同，不会成为差异）。
const out = { scenario: scenario.name, side, volatile: scenario.volatile ?? [], accepted: scenario.accepted ?? [], steps: [], notifications: [], provider: providerRequests };
let server; let serverLog = '';
const wantServer = scenario.server !== false;
if (wantServer) {
  const [cmd, ...args] = SIDES[side].server;
  server = spawn(cmd, args, { cwd: project, env, stdio: ['ignore', 'pipe', 'pipe'], detached: true });
  children.push(server);
  server.stdout.on('data', (d) => { serverLog += d; });
  server.stderr.on('data', (d) => { serverLog += d; });
  const t0 = Date.now();
  for (;;) {
    if (server.exitCode !== null) { out.startFailure = { exitCode: server.exitCode, log: serverLog }; break; }
    if (Date.now() - t0 > 60000) { out.startFailure = { timeout: true, log: serverLog }; break; }
    if (!(await portFree(GATEWAY_PORT))) break;
    await sleep(50);
  }
  out.startMs = Date.now() - t0;
}

// ── WS 连接（惰性建立；一个场景一条连接，除非步骤要求重连）──
// frames 是当前连接收到的帧，步骤从这里取自己的帧；allFrames 跨重连累积，收尾时从中取通知。
let ws; let frames = []; let wake = []; const allFrames = [];
const connect = async (hello) => {
  ws = new WebSocket(`ws://127.0.0.1:${GATEWAY_PORT}/ws`);
  frames = [];
  const got = (f) => { frames.push(f); allFrames.push(f); wake.splice(0).forEach((w) => w()); };
  ws.onmessage = (e) => { let f; try { f = JSON.parse(e.data); } catch { f = { raw: String(e.data) }; } got(f); };
  let closed = null;
  ws.onclose = (e) => { closed = { code: e.code, reason: e.reason }; got({ type: '__closed', ...closed }); };
  await new Promise((resolve) => { ws.onopen = resolve; ws.onerror = resolve; });
  if (ws.readyState !== 1) return { connectFailed: true };
  ws.send(JSON.stringify(hello ?? { type: 'hello', protocolVersion: '1.2', clientName: 'test', clientVersion: '0.0.0', token: TOKEN }));
  return waitFrame((f) => f.type !== 'notification');
};
const waitFrame = async (pred, timeout = STEP_TIMEOUT_MS) => {
  const t0 = Date.now(); let seen = 0;
  for (;;) {
    for (; seen < frames.length; seen++) if (pred(frames[seen])) return frames[seen];
    if (Date.now() - t0 > timeout) return { type: '__timeout' };
    await new Promise((r) => { wake.push(r); setTimeout(r, 200); });
  }
};
// 步骤级环境变量：盖在场景环境之上；值为 null 表示删掉该变量。
const stepEnv = (step) => {
  const merged = { ...env };
  for (const [k, v] of Object.entries(substDeep(step.env ?? {}))) { if (v === null) delete merged[k]; else merged[k] = v; }
  return merged;
};
let requestSeq = 0;
const runStep = async (step) => {
  if (step.sleep) { await sleep(step.sleep); return { slept: true }; }
  if (step.disconnect) {
    if (!ws || ws.readyState > 1) return { disconnected: false };
    const done = waitFrame((f) => f.type === '__closed', 5000);
    ws.close();
    const last = await done;
    return { disconnected: true, close: last.type === '__closed' ? { code: last.code, reason: last.reason } : last };
  }
  if (step.connect) return { hello: await connect(step.connect === true ? undefined : substDeep(step.connect)) };
  if (step.ws) {
    let hello; if (!ws) hello = await connect();
    if (hello?.connectFailed) return { hello };
    const id = `q${++requestSeq}`;
    ws.send(JSON.stringify({ type: 'request', id, method: step.ws, params: substDeep(step.params ?? {}) }));
    // answer：收集本步骤的帧期间，遇到匹配的事件就按模板发一个请求（见 README）。
    // 每条规则最多应答 count 次（缺省不限）；一个事件只由第一条还有余量的匹配规则应答。
    const rulesLeft = (step.answer ?? []).map((rule) => ({ rule, left: rule.count ?? Infinity }));
    const answers = [];
    const react = (f) => {
      if (f.id !== id || f.type !== 'event' || !f.event) return;
      const hit = rulesLeft.find((r) => r.left > 0 && Object.entries(r.rule.on ?? {}).every(([k, v]) => f.event[k] === v));
      if (!hit) return;
      hit.left--;
      const answerId = `q${++requestSeq}`;
      const params = fillEvent(substDeep(hit.rule.params ?? {}), f.event);
      answers.push({ id: answerId, ws: hit.rule.ws, params });
      ws.send(JSON.stringify({ type: 'request', id: answerId, method: hit.rule.ws, params }));
    };
    const last = await waitFrame((f) => { react(f); return f.type === '__closed' || f.type === '__timeout'
      || (f.id === id && (f.type === 'response' || (f.type === 'event' && f.final))); }, step.timeout);
    const mine = frames.filter((f) => f.id === id);
    if (last.type?.startsWith('__')) mine.push(last);
    const result = hello ? { hello, frames: mine } : { frames: mine };
    if (step.answer) {
      result.answers = [];
      for (const a of answers) {
        const done = await waitFrame((f) => f.type === '__closed' || f.type === '__timeout'
          || (f.id === a.id && (f.type === 'response' || (f.type === 'event' && f.final))), 5000);
        const got = frames.filter((f) => f.id === a.id);
        if (done.type?.startsWith('__')) got.push(done);
        result.answers.push({ ws: a.ws, params: a.params, frames: got });
      }
    }
    return result;
  }
  if (step.http) {
    const [method, url] = step.http.split(' ');
    const response = await fetch(`http://127.0.0.1:${GATEWAY_PORT}${subst(url)}`, {
      method, headers: substDeep(step.headers ?? {}), redirect: 'manual',
      body: step.body === undefined ? undefined : typeof step.body === 'string' ? subst(step.body) : JSON.stringify(substDeep(step.body)),
      signal: AbortSignal.timeout(step.timeout ?? STEP_TIMEOUT_MS),
    }).catch((error) => ({ error: String(error.cause?.code ?? error.name) }));
    if (response.error) return { http: response };
    const text = await response.text();
    let json; try { json = JSON.parse(text); } catch {}
    return { http: { status: response.status, headers: Object.fromEntries(response.headers), body: json ?? text } };
  }
  if (step.serve) {
    // 前台起一个会常驻的 CLI 进程（如 `server`）：等 stdout 出现 until，执行 then 里的步骤，
    // 发 signal，记下 stdout / stderr / 退出码。进程自己先退出（启动失败）也照样记。
    const [cmd, ...prefix] = SIDES[side].cli;
    const child = spawn(cmd, [...prefix, ...substDeep(step.serve)], {
      cwd: step.cwd ? subst(step.cwd) : project, env: stepEnv(step), stdio: ['ignore', 'pipe', 'pipe'], detached: true,
    });
    children.push(child);
    let stdout = '', stderr = '';
    child.stdout.on('data', (d) => { stdout += d; }); child.stderr.on('data', (d) => { stderr += d; });
    const closed = new Promise((r) => child.on('close', (c, s) => r([c, s])));
    const until = subst(step.until ?? 'WebSocket: ');
    const t0 = Date.now();
    while (!stdout.includes(until) && child.exitCode === null && child.signalCode === null
      && Date.now() - t0 < (step.timeout ?? STEP_TIMEOUT_MS)) await sleep(20);
    const reached = stdout.includes(until);
    const timedOut = !reached && child.exitCode === null && child.signalCode === null;
    const then = [];
    if (reached) {
      await sleep(step.settleMs ?? 100);
      for (const inner of step.then ?? []) then.push({ step: inner, ...(await runStep(inner)) });
      child.kill(step.signal ?? 'SIGTERM');
    }
    const killer = setTimeout(() => { try { process.kill(-child.pid, 'SIGKILL'); } catch {} }, reached ? 15000 : 0);
    const [exitCode, signal] = await closed;
    clearTimeout(killer);
    return { serve: { reached, exitCode, signal, stdout, stderr }, then, ...(timedOut && { timedOut }) };
  }
  if (step.cli) {
    const [cmd, ...prefix] = SIDES[side].cli;
    const child = spawn(cmd, [...prefix, ...substDeep(step.cli)], { cwd: step.cwd ? subst(step.cwd) : project, env: stepEnv(step), stdio: ['pipe', 'pipe', 'pipe'] });
    let stdout = '', stderr = '';
    child.stdout.on('data', (d) => { stdout += d; }); child.stderr.on('data', (d) => { stderr += d; });
    child.stdin.end(step.stdin ?? '');
    let timedOut = false;
    const timer = setTimeout(() => { timedOut = true; child.kill('SIGKILL'); }, step.timeout ?? STEP_TIMEOUT_MS);
    const [exitCode, signal] = await new Promise((r) => child.on('close', (c, s) => r([c, s])));
    clearTimeout(timer);
    return { cli: { exitCode, signal, stdout, stderr }, ...(timedOut && { timedOut }) };
  }
  throw new Error(`unknown step: ${JSON.stringify(step)}`);
};
// 失败：步骤超时（含被采集器杀掉的 cli / serve）、连不上。只有步骤上声明了 expectTimeout 的超时算预期；
// 其余记进 failures，check.mjs 见到非预期的就把场景判为 INVALID，不管两侧是否相同。
const failuresOf = (step, r, at) => {
  const t = (x) => x?.type === '__timeout';
  const httpTimeout = ['TimeoutError', 'AbortError'].includes(r.http?.error);
  const kinds = [];
  if (r.timedOut || t(r.close) || t(r.hello) || (r.frames ?? []).some(t) || (r.answers ?? []).some((a) => a.frames.some(t)) || httpTimeout) kinds.push('timeout');
  if (r.hello?.connectFailed || (r.http?.error && !httpTimeout)) kinds.push('connect');
  return [...kinds.map((kind) => ({ at, kind, expected: kind === 'timeout' && step.expectTimeout === true })),
    ...(r.then ?? []).flatMap((x, i) => failuresOf(x.step, x, `${at}.then[${i}]`))];
};
if (!out.startFailure) {
  for (const step of scenario.steps) out.steps.push({ step, ...(await runStep(step)) });
  await sleep(scenario.settleMs ?? 300);
}
out.failures = [
  ...(out.startFailure ? [{ at: 'start', kind: 'start', expected: scenario.expectStartFailure === true }] : []),
  ...out.steps.flatMap((r, i) => failuresOf(r.step, r, `steps[${i}]`)),
];
out.notifications = allFrames.filter((f) => f.type === 'notification');
out.unsolicited = allFrames.filter((f) => !['notification', 'hello_ok', 'response', 'event', '__closed'].includes(f.type));
if (ws) ws.close();

// ── 停机：SIGTERM，记退出码 ──
if (server && server.exitCode === null) {
  const exited = new Promise((r) => server.on('exit', (code, signal) => r({ code, signal })));
  server.kill('SIGTERM');
  out.shutdown = await Promise.race([exited, sleep(15000).then(() => ({ timeout: true }))]);
  if (out.shutdown.timeout) out.failures.push({ at: 'shutdown', kind: 'timeout', expected: false });
}
mock.close(); mock.closeAllConnections();
out.serverLog = serverLog;

// ── 落盘：home 与 project 下的全部文件 ──
// .json / .jsonl 解析后比内容；另记一份原文指纹抓排版差异（缩进、空格、换行、转义、数字写法）：
// 行数、是否以换行结尾，以及每行骨架的 sha256 计数（按哈希排序，行的先后由解析结果比）。
// 骨架：字符串只留转义序列，数字只留写法（0 / 0.0 / 0e0）；耗时键的值先换成占位（它在两次运行间
// 会在整数和小数之间变，见 diff.mjs 第 6 条）。diff.mjs 其余归一化对象（UUID、ISO 时间、请求指纹、
// 随机路径段）都在字符串里，毫秒时间戳和进程号的写法不变，骨架里已经没有它们。
const skeleton = (line) => line
  .replace(/("(?:duration_ms|duration_seconds|durationMs)"\s*:\s*)-?[\d.eE+-]+/g, '$1D')
  .replace(/"(?:[^"\\]|\\.)*"|-?\d+(\.\d+)?([eE][+-]?\d+)?/g, (tok, frac, exp) => tok[0] === '"'
    ? `"${(tok.match(/\\(?:u[0-9a-fA-F]{4}|.)/g) ?? []).join('')}"` : exp ? '0e0' : frac ? '0.0' : '0');
const rawPrint = (text) => {
  const lines = text.split('\n'); if (text.endsWith('\n')) lines.pop();
  const counts = {};
  for (const l of lines) { const h = createHash('sha256').update(skeleton(l)).digest('hex').slice(0, 16); counts[h] = (counts[h] ?? 0) + 1; }
  return { lines: lines.length, finalNewline: text.endsWith('\n'), skeletons: Object.fromEntries(Object.entries(counts).sort()) };
};
const walk = (dir) => fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => (a.name < b.name ? -1 : 1)).flatMap((e) => {
  const p = path.join(dir, e.name);
  if (e.isDirectory()) { const inner = walk(p); return inner.length ? inner : [[p + '/', { emptyDir: true }]]; }
  if (e.isSymbolicLink()) return [[p, { symlink: fs.readlinkSync(p) }]];
  const bytes = fs.readFileSync(p);
  const text = bytes.toString('utf8');
  const mode = (fs.statSync(p).mode & 0o777).toString(8);
  // 非 UTF-8 文件：比长度和内容的 sha256（目前各场景的落盘里没有这类文件）。
  if (Buffer.from(text, 'utf8').compare(bytes) !== 0) return [[p, { mode, binaryBytes: bytes.length, sha256: createHash('sha256').update(bytes).digest('hex') }]];
  if (p.endsWith('.jsonl')) { try { return [[p, { mode, jsonl: text.split('\n').filter(Boolean).map((l) => JSON.parse(l)), raw: rawPrint(text) }]]; } catch {} }
  if (p.endsWith('.json')) { try { return [[p, { mode, json: JSON.parse(text), raw: rawPrint(text) }]]; } catch {} }
  return [[p, { mode, text }]];
});
out.tree = Object.fromEntries([...walk(home), ...walk(project)].map(([p, v]) => [p.slice(ROOT.length), v]));
fs.mkdirSync(path.dirname(path.resolve(outPath)), { recursive: true });
fs.writeFileSync(outPath, JSON.stringify(out, null, 1) + '\n');
console.error(`bbx: ${side} ${scenario.name}: ${out.startFailure ? 'START FAILED' : `${out.steps.length} steps`} -> ${outPath}`);
process.exit(0);
