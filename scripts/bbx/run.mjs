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
import https from 'node:https';
import net from 'node:net';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';
import { DatabaseSync } from 'node:sqlite';
import { parseHttpHead, parseHttpResponses } from './lib/rawhttp.mjs';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const BASELINE = process.env.BBX_BASELINE || path.join(REPO, 'resources/PilotDeck');
const TARGET = process.env.BBX_TARGET || path.join(REPO, 'src/target/debug');
// 并行槽位：BBX_SLOT=1..8 时换一套等长的目录和端口（pdrs-bbx → pdrs-bbN，3841x → 384(1+N)x），
// 场景文件里写死的同名路径和端口在读入时做同样的替换。两侧在同一个槽位里跑，结论不受槽位影响；
// 不同槽位的采集产物之间不要互相比。槽位 0（缺省）就是原来的固定目录和端口。
const SLOT = Number(process.env.BBX_SLOT || 0);
if (!Number.isInteger(SLOT) || SLOT < 0 || SLOT > 8) { console.error('bbx: BBX_SLOT must be 0..8'); process.exit(2); }
// 根目录的父目录：macOS 上是 /private/tmp（/tmp 的物理路径），其他平台是 /tmp/private。后者与前者等长，
// 这样各平台上根目录之下的所有路径（以及由路径派生的项目目录名、项目键、百分号编码）都与 macOS 等长：
// 基线有按字符数截断的文本（例如会话元数据里 1200 字符的 firstPrompt），路径短 8 个字符会把截断点挪进
// 随机 UUID 的中间，基线自比就不稳了；token 估算、文件大小同理。场景里写死的三种写法在读入时换成
// 本平台的写法：/private/tmp/pdrs-bbx、项目目录名 private-tmp-pdrs-bbx、编码后的 %2Fprivate%2Ftmp%2Fpdrs-bbx。
// macOS 上不做任何替换。两侧在同一台机器上跑；不同平台的采集产物不要互相比。
const TMP_BASE = process.platform === 'darwin' ? '/private/tmp' : '/tmp/private';
const PLATFORM_FORMS = TMP_BASE === '/private/tmp' ? [] : [
  ['/private/tmp/pdrs-bbx', `${TMP_BASE}/pdrs-bbx`],
  ['%2Fprivate%2Ftmp%2Fpdrs-bbx', `${encodeURIComponent(TMP_BASE)}%2Fpdrs-bbx`],
  ['private-tmp-pdrs-bbx', `${TMP_BASE.slice(1).replaceAll('/', '-')}-pdrs-bbx`],
];
const slotText = (t) => {
  const local = PLATFORM_FORMS.reduce((s, [from, to]) => s.replaceAll(from, to), t);
  return SLOT ? local.replaceAll('pdrs-bbx', `pdrs-bb${SLOT}`).replaceAll('3841', `384${1 + SLOT}`) : local;
};
export const ROOT = slotText('/private/tmp/pdrs-bbx/root');
const TOKEN = 'bbx-token';
const STEP_TIMEOUT_MS = 30000;

const [side, scenarioPath, outPath] = process.argv.slice(2);
if (!['ts', 'rust'].includes(side) || !scenarioPath || !outPath) {
  console.error('usage: run.mjs <ts|rust> <scenario.json> <out.json>');
  process.exit(2);
}
const scenario = JSON.parse(slotText(fs.readFileSync(scenarioPath, 'utf8')));
// 基线 CLI 的 cron 子命令和默认对话的在线路径只探测写死的 127.0.0.1:18789（真实 PilotDeck 的默认端口），
// 要对拍它们只能把被测 server 起在 18789 上。这必须显式选择：场景写 "gatewayPort": 18789，并且运行时设
// BBX_ALLOW_18789=1。没设时这类场景什么都不做，退出码 6（check.mjs 记为 SKIPPED）。设了时仍然先确认 18789
// 没有监听者（下面对所有场景都做的检查），再用一把跨槽位的全局锁串行化（18789 只有一个）。
const PORT_18789 = scenario.gatewayPort !== undefined;
if (PORT_18789 && scenario.gatewayPort !== 18789) {
  console.error(`bbx: scenario gatewayPort may only be 18789, got ${JSON.stringify(scenario.gatewayPort)}`); process.exit(2);
}
if (PORT_18789 && process.env.BBX_ALLOW_18789 !== '1') {
  console.error(`bbx: SKIPPED ${scenario.name}: it runs the server on port 18789; set BBX_ALLOW_18789=1 to run it`); process.exit(6);
}
const GATEWAY_PORT = PORT_18789 ? 18789 : 38411 + 10 * SLOT;
const MOCK_PORT = 38412 + 10 * SLOT;
const home = path.join(ROOT, 'home');
const pilotHome = path.join(home, '.pilotdeck');
const project = path.join(ROOT, 'proj');
const tmp = path.join(ROOT, 'tmp');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// {SIDECAR}：本侧 agent loop sidecar 的启动命令行（shell 写法，与下面 sidecar 步骤起的是同一个程序）。
// 两侧的值不同，只能放进不落盘的地方（场景的 env），例如 env 里 "BBX_SIDECAR": "{SIDECAR}"，
// 再让 modules.agentLoop 的 module-stdio-v2 绑定用 /bin/sh -c 'exec $BBX_SIDECAR' 起它；写进配置文件会成为差异。
const shq = (s) => `'${s.replaceAll("'", "'\\''")}'`;
const SIDECAR_LINE = side === 'ts'
  ? `node ${shq(path.join(BASELINE, 'dist/src/cli/pilotdeck-agent-loop-sidecar.js'))}`
  : shq(path.join(TARGET, 'pilotdeck-agent-loop-sidecar'));
// {PATH}：采集器自己的 PATH（两侧相同；场景的 pathDir 不在其中），让场景能在 env 里拼出 "目录:{PATH}"。
const HTTPS_CA = path.join(ROOT, 'tls/ca.pem');
const subst = (text) => text.replaceAll('{SIDECAR}', SIDECAR_LINE).replaceAll('{PATH}', process.env.PATH ?? '')
  .replaceAll('{HTTPS_CA}', HTTPS_CA).replaceAll('{HTTPS}', `https://127.0.0.1:${scenario.httpsPort}`)
  .replaceAll('{ROOT}', ROOT).replaceAll('{HOME}', home).replaceAll('{PILOT_HOME}', pilotHome)
  .replaceAll('{PROJECT}', project).replaceAll('{MOCK}', `http://127.0.0.1:${MOCK_PORT}`)
  .replaceAll('{GATEWAY}', `http://127.0.0.1:${GATEWAY_PORT}`).replaceAll('{TOKEN}', TOKEN);
// 引用前面跑出来的值：步骤或 provider 规则上写 "save": {"名字": "正则"}，正则在该步骤结果（或该次模型请求）
// 的 JSON 文本上匹配，第一个捕获组存成这个名字；之后的步骤和 provider 应答里用 {saved.名字} 引用。
// 存下来的值只用来驱动后续输入，不进产物；没存到时 {saved.名字} 原样留着，后续步骤会因此明显失败。
const saved = {};
const capture = (rules, text) => {
  for (const [name, re] of Object.entries(rules ?? {})) { const m = new RegExp(re).exec(text); if (m) saved[name] = m[1] ?? m[0]; }
};
const withSaved = (v) => typeof v === 'string' ? v.replace(/\{saved\.([A-Za-z0-9_]+)\}/g, (all, k) => saved[k] ?? all)
  : Array.isArray(v) ? v.map(withSaved)
  : v && typeof v === 'object' ? Object.fromEntries(Object.entries(v).map(([k, x]) => [k, withSaved(x)]))
  : v;
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
// 锁是一个目录，里面的 pid 文件记持有者；持有者已经不在了的锁可以接过来。只删自己拿到的锁。
const heldLocks = [];
const takeLock = (lock) => {
  try { fs.mkdirSync(lock); } catch {
    let owner = 0; try { owner = Number(fs.readFileSync(path.join(lock, 'pid'), 'utf8').trim() || 0); } catch {}
    let alive = false; try { process.kill(owner, 0); alive = true; } catch {}
    if (alive) { console.error(`bbx: another run holds ${lock} (pid ${owner})`); process.exit(3); }
  }
  fs.writeFileSync(path.join(lock, 'pid'), String(process.pid));
  heldLocks.push(lock);
};
const children = [];
const cleanup = () => {
  for (const c of children) { try { process.kill(-c.pid, 'SIGKILL'); } catch {} }
  for (const lock of heldLocks) { try { fs.rmSync(lock, { recursive: true, force: true }); } catch {} }
};
process.on('exit', cleanup);
for (const s of ['SIGINT', 'SIGTERM', 'SIGHUP']) process.on(s, () => process.exit(130));
takeLock(path.join(path.dirname(ROOT), 'lock'));
// 18789 场景的全局锁：不随槽位变，所有槽位共用一把。
if (PORT_18789) takeLock(path.join(TMP_BASE, 'pdrs-bbx-18789.lock'));

const portFree = (port) => new Promise((resolve) => {
  const s = net.createServer().once('error', () => resolve(false))
    .once('listening', () => s.close(() => resolve(true))).listen(port, '127.0.0.1');
});
// 等服务起来时用「能不能连上」来探，不用 portFree：后者要自己绑一下端口，正好撞上服务在绑的那一刻时，
// 服务会因 EADDRINUSE 启动失败（采集器自己造成的偶发）。
const portOpen = (port) => new Promise((resolve) => {
  const s = net.connect(port, '127.0.0.1');
  s.once('connect', () => { s.destroy(); resolve(true); }).once('error', () => resolve(false));
});
// 基线 CLI 的若干路径会探测写死的默认端口 18789（真实 PilotDeck 的默认端口）。
// 那里有人监听时一律拒跑，免得碰到本机正在运行的实例。用 lsof 看，不去连它。
import { spawnSync } from 'node:child_process';
// 没有 lsof 的机器（常见于 Linux）改用 ss；两个都没有就拒跑，不当作「没人监听」。
const listening18789 = () => {
  const lsof = spawnSync('lsof', ['-nP', '-iTCP:18789', '-sTCP:LISTEN'], { encoding: 'utf8' });
  if (!lsof.error) return lsof.stdout.trim() !== '';
  const ss = spawnSync('ss', ['-H', '-ltn', 'sport = :18789'], { encoding: 'utf8' });
  if (!ss.error && ss.status === 0) return ss.stdout.trim() !== '';
  console.error('bbx: neither lsof nor ss is usable; cannot check port 18789'); process.exit(5);
};
if (listening18789()) {
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
// 种子文件的 mtime 按书写顺序每个相隔 1 秒、全部落在过去：几个文件在同一毫秒内写完时，
// 基线按 mtime 排序的结果（例如 list_sessions）取决于写入恰好跨没跨毫秒，自比都不稳。
const seedFiles = Object.entries(scenario.files ?? {});
const seedBase = Math.floor(Date.now() / 1000) - seedFiles.length;
for (const [index, [rel, content]] of seedFiles.entries()) {
  const target = path.join(ROOT, rel);
  if (!target.startsWith(ROOT + '/')) throw new Error(`file escapes root: ${rel}`);
  fs.mkdirSync(path.dirname(target), { recursive: true });
  // {"$file": {text, prefix, repeat, suffix, mode}}：按 raw 步骤 send 的写法拼大串（prefix + text×repeat + suffix，
  // 各段替换占位符），mode 是八进制权限（如 "755"，用来放可执行的假程序）。只有这一个键的对象才按此解释。
  const spec = content && typeof content === 'object' && !Array.isArray(content) && Object.keys(content).length === 1 && content.$file;
  if (spec) {
    const fd = fs.openSync(target, 'w');
    try {
      fs.writeSync(fd, subst(spec.prefix ?? ''));
      const unit = Buffer.from(subst(spec.text ?? ''));
      const times = spec.repeat ?? 1;
      if (unit.length) { const batch = Math.max(1, Math.floor((1 << 20) / unit.length)); const chunk = Buffer.concat(Array(Math.min(batch, times)).fill(unit));
        for (let left = times; left > 0; left -= batch) fs.writeSync(fd, left >= batch ? chunk : chunk.subarray(0, left * unit.length)); }
      fs.writeSync(fd, subst(spec.suffix ?? ''));
    } finally { fs.closeSync(fd); }
    if (spec.mode !== undefined) fs.chmodSync(target, parseInt(String(spec.mode), 8));
  } else fs.writeFileSync(target, typeof content === 'string' ? subst(content) : JSON.stringify(substDeep(content), null, 2) + '\n');
  fs.utimesSync(target, seedBase + index, seedBase + index);
}
// symlinks：{"相对根目录的链接路径": "链接内容"}，在种完文件之后建；链接内容同样替换占位符，只能放在根目录之下。
for (const [rel, link] of Object.entries(scenario.symlinks ?? {})) {
  const at = path.join(ROOT, rel);
  if (!at.startsWith(ROOT + '/')) throw new Error(`symlink escapes root: ${rel}`);
  fs.mkdirSync(path.dirname(at), { recursive: true });
  fs.symlinkSync(subst(link), at);
}

// ── 假 provider（按请求路径分流：OpenAI chat completions、Anthropic messages、OpenAI Responses、Google Gemini）──
// 规则按顺序匹配请求体的 JSON 文本：when（全部包含）/ unless（全部不包含）。
// 规则里的字符串（包括工具参数）同样做 {ROOT} 等占位符替换。
// 请求按命中的规则名分组记录，这样互不相干的两路请求（如标题生成与主对话）
// 谁先到不影响比较。没有规则命中时用 default，回一段固定文本。
const rules = [...substDeep(scenario.provider ?? []), { name: 'default', reply: { text: 'BBX_OK' } }];
const providerRequests = {};
const mock = http.createServer(async (req, res) => {
  const parts = []; for await (const p of req) parts.push(p);
  const bytes = Buffer.concat(parts);
  const raw = bytes.toString('utf8');
  let body = null; try { body = JSON.parse(raw); } catch {}
  // url：请求行里的路径（含查询串）要包含这段文字（外部模块的 manifest GET 没有请求体，几个模块同时配时只能按路径分）。
  const rule = rules.find((r) => (r.url === undefined || req.url.includes(r.url)) && (r.when ?? []).every((s) => raw.includes(s)) && (r.unless ?? []).every((s) => !raw.includes(s)));
  // 请求头 content-length 在这里就核对：等于实际收到的请求体字节数记 true，否则 false；没有这个头不记。
  // 比较器的规则 19 在两侧都是 true 时不比这个头的数值（数值里含着被归一化的变长值的位数）。
  const declared = req.headers['content-length'];
  (providerRequests[rule.name] ??= []).push({
    method: req.method, url: req.url, headers: req.headers,
    ...(declared === undefined ? {} : { 'content-length-ok': /^\d+$/.test(declared) && Number(declared) === bytes.length }),
    body: body ?? { raw },
  });
  capture(rule.save, raw);
  const reply = withSaved(rule.reply ?? {});
  // 在一轮进行中打断外部 sidecar（见下面的 sidecar / tcpProxy 步骤）：先做这件事，再照常应答。
  // killSidecar: 信号名，发给当前的 sidecar 进程；cutProxy: true，断开 TCP 代理上现有的全部连接（代理继续监听）。
  if (reply.killSidecar && sidecarProc) { try { sidecarProc.child.kill(reply.killSidecar); } catch {} }
  // killServer: 信号名，发给 "server" 起的被测服务（用来在一轮进行中让 server 消失；只发信号，不等它退出）。
  // killServerDelayMs：收到请求后过这么久再发（缺省立刻）。立刻发时正撞上服务端在写落盘（例如基线的运行登记锁），
  // 留下什么取决于两侧各自的写入时机；等服务端闲下来在等模型应答时再发，比的才是「server 消失」本身。
  if (reply.killServer) {
    const kill = () => { if (server && server.exitCode === null && server.signalCode === null) { try { server.kill(reply.killServer); } catch {} } };
    if (reply.killServerDelayMs) setTimeout(kill, reply.killServerDelayMs); else kill();
  }
  if (reply.cutProxy) { for (const s of proxySockets.splice(0)) s.destroy(); }
  // {status, body} 同样认 delayMs：先等一会儿再回（外部模块的应答要在一轮进行中被中止时用）。
  if (reply.status) { if (reply.delayMs) await sleep(reply.delayMs); if (res.destroyed) return; res.writeHead(reply.status, { 'content-type': 'application/json' }); res.end(JSON.stringify(reply.body ?? {})); return; }
  // 应答的组成：文本（text 或分块的 chunks）、推理文本（reasoning）、零到多个工具调用（tool / tools；
  // rawArguments 原样当参数字符串，用来回非法 JSON）、结束原因（finish）。delayMs 先等一会儿再回；
  // breakAfter: n 流里发出 n 块后断开连接；hangAfter: n 发出 n 块后挂住不结束（hangMs 后断开，缺省 60 秒）；
  // pauseAfter: n 发出 n 块后停 pauseMs 毫秒（缺省 1000）再照常发完，用来让客户端在流的中途、确定的位置做事（见 turn-steer-mid-stream）。
  const calls = [...(reply.tool ? [reply.tool] : []), ...(reply.tools ?? [])].map((t, i) => ({
    id: t.id ?? `call_bbx_${i + 1}`, type: 'function',
    function: { name: t.name, arguments: t.rawArguments ?? JSON.stringify(t.arguments ?? {}) },
  }));
  const text = reply.text ?? (calls.length ? null : reply.chunks ? reply.chunks.join('') : 'BBX_OK');
  const finish = reply.finish ?? (calls.length ? 'tool_calls' : 'stop');
  const usage = reply.usage ?? { prompt_tokens: 12, completion_tokens: 4, total_tokens: 16 };
  if (reply.delayMs) await sleep(reply.delayMs);
  // 流式输出的公共部分：按块写，支持中途断开和挂住。
  const stream = async (blocks) => {
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    let sent = 0;
    for (const block of blocks) {
      // 先等已写出的块真正发到对端，再断开或挂住；直接 destroy 会把还在缓冲里的块一起丢掉。
      if (reply.breakAfter !== undefined && sent >= reply.breakAfter) { await sleep(50); res.destroy(); return; }
      if (reply.hangAfter !== undefined && sent >= reply.hangAfter) { setTimeout(() => res.destroy(), reply.hangMs ?? 60000).unref(); return; }
      // endAfter: n 发出 n 块后正常结束响应（没有结束原因 / [DONE] / message_stop），不是断开连接。
      if (reply.endAfter !== undefined && sent >= reply.endAfter) { res.end(); return; }
      if (reply.pauseAfter !== undefined && sent === reply.pauseAfter) await sleep(reply.pauseMs ?? 1000);
      await new Promise((done) => res.write(block, done)); sent++;
    }
    res.end();
  };
  // Anthropic messages 形状（请求路径以 /messages 结尾）。
  if (/\/messages(\?|$)/.test(req.url)) {
    const stop = reply.finish ?? (calls.length ? 'tool_use' : 'end_turn');
    const content = [
      ...(reply.reasoning ? [{ type: 'thinking', thinking: reply.reasoning, signature: 'sig_bbx' }] : []),
      ...(text !== null ? [{ type: 'text', text }] : []),
      ...calls.map((c) => ({ type: 'tool_use', id: c.id, name: c.function.name, input: (() => { try { return JSON.parse(c.function.arguments); } catch { return {}; } })() })),
    ];
    const head = { id: 'msg_bbx', type: 'message', role: 'assistant', model: body?.model };
    if (!body?.stream) {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ ...head, content, stop_reason: stop, stop_sequence: null, usage: { input_tokens: usage.prompt_tokens, output_tokens: usage.completion_tokens } }));
      return;
    }
    const ev = (type, data) => `event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`;
    const blocks = [ev('message_start', { message: { ...head, content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: usage.prompt_tokens, output_tokens: 0 } } })];
    let index = 0;
    const block = (start, deltas) => {
      blocks.push(ev('content_block_start', { index, content_block: start }));
      for (const delta of deltas) blocks.push(ev('content_block_delta', { index, delta }));
      blocks.push(ev('content_block_stop', { index })); index++;
    };
    if (reply.reasoning) block({ type: 'thinking', thinking: '' }, [{ type: 'thinking_delta', thinking: reply.reasoning }, { type: 'signature_delta', signature: 'sig_bbx' }]);
    if (text !== null) block({ type: 'text', text: '' }, (reply.chunks ?? [text]).map((t) => ({ type: 'text_delta', text: t })));
    for (const c of calls) block({ type: 'tool_use', id: c.id, name: c.function.name, input: {} }, [{ type: 'input_json_delta', partial_json: c.function.arguments }]);
    blocks.push(ev('message_delta', { delta: { stop_reason: stop, stop_sequence: null }, usage: { output_tokens: usage.completion_tokens } }), ev('message_stop', {}));
    await stream(blocks); return;
  }
  // OpenAI Responses 形状（请求路径以 /responses 结尾）。finish 是响应的 status：缺省 completed，
  // incomplete 表示被 max_output_tokens 截断，failed 表示失败（错误对象取 reply.error）。
  // 用量从 chat 形状换算：prompt_tokens_details.cached_tokens / completion_tokens_details.reasoning_tokens 进 *_details。
  if (/\/responses(\?|$)/.test(req.url)) {
    const status = reply.finish ?? 'completed';
    const respUsage = {
      input_tokens: usage.prompt_tokens, input_tokens_details: { cached_tokens: usage.prompt_tokens_details?.cached_tokens ?? 0 },
      output_tokens: usage.completion_tokens, output_tokens_details: { reasoning_tokens: usage.completion_tokens_details?.reasoning_tokens ?? 0 },
      total_tokens: usage.total_tokens,
    };
    const items = [
      ...(reply.reasoning ? [{ id: 'rs_bbx', type: 'reasoning', summary: [{ type: 'summary_text', text: reply.reasoning }] }] : []),
      ...(text !== null ? [{ id: 'msg_bbx', type: 'message', status: 'completed', role: 'assistant', content: [{ type: 'output_text', text, annotations: [] }] }] : []),
      ...calls.map((c, i) => ({ id: `fc_bbx_${i + 1}`, type: 'function_call', status: 'completed', call_id: c.id, name: c.function.name, arguments: c.function.arguments })),
    ];
    const response = (st, output, u) => ({
      id: 'resp_bbx', object: 'response', created_at: 1700000000, status: st, model: body?.model, output,
      incomplete_details: st === 'incomplete' ? { reason: 'max_output_tokens' } : null,
      ...(st === 'failed' ? { error: reply.error ?? { code: 'server_error', message: 'The model failed to generate a response.' } } : { error: null }),
      usage: u,
    });
    if (!body?.stream) {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify(response(status, items, respUsage)));
      return;
    }
    let seq = 0;
    const ev = (type, data) => `event: ${type}\ndata: ${JSON.stringify({ type, ...data, sequence_number: seq++ })}\n\n`;
    const blocks = [ev('response.created', { response: response('in_progress', [], null) }), ev('response.in_progress', { response: response('in_progress', [], null) })];
    items.forEach((item, output_index) => {
      const at = { item_id: item.id, output_index };
      if (item.type === 'reasoning') {
        const part = item.summary[0];
        blocks.push(
          ev('response.output_item.added', { output_index, item: { ...item, summary: [] } }),
          ev('response.reasoning_summary_part.added', { ...at, summary_index: 0, part: { ...part, text: '' } }),
          ev('response.reasoning_summary_text.delta', { ...at, summary_index: 0, delta: part.text }),
          ev('response.reasoning_summary_text.done', { ...at, summary_index: 0, text: part.text }),
          ev('response.reasoning_summary_part.done', { ...at, summary_index: 0, part }),
          ev('response.output_item.done', { output_index, item }));
      } else if (item.type === 'message') {
        const part = item.content[0];
        blocks.push(
          ev('response.output_item.added', { output_index, item: { ...item, status: 'in_progress', content: [] } }),
          ev('response.content_part.added', { ...at, content_index: 0, part: { ...part, text: '' } }),
          ...(reply.chunks ?? [text]).map((delta) => ev('response.output_text.delta', { ...at, content_index: 0, delta })),
          ev('response.output_text.done', { ...at, content_index: 0, text: part.text }),
          ev('response.content_part.done', { ...at, content_index: 0, part }),
          ev('response.output_item.done', { output_index, item }));
      } else {
        blocks.push(
          ev('response.output_item.added', { output_index, item: { ...item, status: 'in_progress', arguments: '' } }),
          ev('response.function_call_arguments.delta', { ...at, delta: item.arguments }),
          ev('response.function_call_arguments.done', { ...at, arguments: item.arguments }),
          ev('response.output_item.done', { output_index, item }));
      }
    });
    blocks.push(ev(`response.${status}`, { response: response(status, items, respUsage) }));
    await stream(blocks); return;
  }
  // Google Gemini generateContent 形状（路径 …/models/<模型>:generateContent 或 :streamGenerateContent?alt=sse）。
  // finish 是 finishReason（缺省 STOP，工具调用也是 STOP）；推理文本是 thought: true 的部分，带 thoughtSignature。
  // 工具调用只在规则里显式写了 id 时才带 functionCall.id（真实 Gemini API 通常不带）。用量换算成 usageMetadata。
  // @google/genai 把整份应答头（含 date）挂在每个结果对象的 sdkHttpResponse 上，基线会把它带进事件和落盘，
  // 所以这里固定 date 头，免得两次运行只差一个时间。
  const google = req.url.match(/\/models\/([^/:?]+):(generateContent|streamGenerateContent)(\?|$)/);
  if (google) {
    res.setHeader('date', 'Thu, 01 Jan 2026 00:00:00 GMT');
    const finishReason = reply.finish ?? 'STOP';
    const given = [...(reply.tool ? [reply.tool] : []), ...(reply.tools ?? [])];
    const cached = usage.prompt_tokens_details?.cached_tokens;
    const thoughts = usage.completion_tokens_details?.reasoning_tokens;
    const usageMetadata = {
      promptTokenCount: usage.prompt_tokens, candidatesTokenCount: usage.completion_tokens, totalTokenCount: usage.total_tokens,
      ...(cached !== undefined ? { cachedContentTokenCount: cached } : {}),
      ...(thoughts !== undefined ? { thoughtsTokenCount: thoughts } : {}),
    };
    const thought = reply.reasoning ? [{ text: reply.reasoning, thought: true, thoughtSignature: 'sig_bbx' }] : [];
    const callParts = calls.map((c, i) => ({ functionCall: {
      ...(given[i]?.id !== undefined ? { id: given[i].id } : {}),
      name: c.function.name, args: (() => { try { return JSON.parse(c.function.arguments); } catch { return {}; } })(),
    } }));
    const chunk = (parts, last) => ({
      candidates: [{ content: { role: 'model', parts }, ...(last ? { finishReason } : {}), index: 0 }],
      ...(last ? { usageMetadata } : {}), modelVersion: decodeURIComponent(google[1]), responseId: 'resp_bbx',
    });
    if (google[2] === 'generateContent') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify(chunk([...thought, ...(text !== null ? [{ text }] : []), ...callParts], true)));
      return;
    }
    const pieces = [
      ...thought.map((p) => [p]),
      ...(text !== null ? (reply.chunks ?? [text]).map((t) => [{ text: t }]) : []),
      ...(callParts.length ? [callParts] : []),
    ];
    if (!pieces.length) pieces.push([{ text: '' }]);
    await stream(pieces.map((parts, i) => `data: ${JSON.stringify(chunk(parts, i === pieces.length - 1))}\r\n\r\n`));
    return;
  }
  // OpenAI chat completions 形状。
  const message = {
    role: 'assistant', content: text,
    ...(reply.reasoning ? { reasoning_content: reply.reasoning } : {}),
    ...(calls.length ? { tool_calls: calls } : {}),
  };
  if (!body?.stream) {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ id: 'chatcmpl-bbx', model: body?.model, choices: [{ index: 0, message, finish_reason: finish }], usage }));
    return;
  }
  const deltas = [
    ...(reply.reasoning ? [{ reasoning_content: reply.reasoning }] : []),
    ...(text !== null ? (reply.chunks ?? [text]).map((t) => ({ content: t })) : []),
    ...(calls.length ? [{ tool_calls: calls.map((t, i) => ({ index: i, ...t })) }] : []),
  ];
  if (deltas.length) deltas[0] = { role: 'assistant', ...deltas[0] };
  await stream([
    ...deltas.map((delta) => `data: ${JSON.stringify({ choices: [{ index: 0, delta, finish_reason: null }] })}\n\n`),
    `data: ${JSON.stringify({ choices: [{ index: 0, delta: {}, finish_reason: finish }], usage })}\n\n`,
    'data: [DONE]\n\n',
  ]);
});
await new Promise((r) => mock.listen(MOCK_PORT, '127.0.0.1', r));

// ── 假 HTTP 代理（场景字段 httpProxies）──
// {"httpProxies": {"A": 38415, "B": {"port": 38416, "status": 407}}}：在 127.0.0.1 上各起一个 HTTP 代理，
// 早于被测进程。收到的每个请求记一条 {method, target, headers}：CONNECT 的 target 是请求行里的 host:port，
// 绝对 URI 请求的 target 是整个 URL。只转发到本机假 provider：目标主机是本机写法（[::1]、0.0.0.0、
// 127.0.0.1、localhost）时不论端口，CONNECT 回 200 并把隧道接到 127.0.0.1 上的假 provider，绝对 URI 请求
// 同样转给它；其余目标回 502、不外连。端口不限是为了让不同种类的请求用不同的目标端口（各自一条隧道，
// 记录里分得清是谁走了代理）；这些端口上没有人监听 [::1]，没走代理的请求直连时立刻被拒。
// 写了 status 的代理对 CONNECT 一律回这个状态并关连接（模拟代理拒绝）。记录按 JSON 文本排序
// （并发请求到达代理的先后不稳定），条数照比，所以隧道是否复用（CONNECT 了几次）也在比较之内。
const proxyRecords = {};
const LOOPBACK_NAMES = new Set(['[::1]', '0.0.0.0', '127.0.0.1', 'localhost']);
// {"proxyLog": true} 步骤取出各代理自上一次取出以来新收到的记录（排序后），用来分段看「这一段的请求走没走代理」；
// 收尾时把剩下的记进产物的 proxies。
const takeProxyLog = () => Object.fromEntries(Object.entries(proxyRecords).map(([name, records]) => [name,
  records.splice(0).map((r) => JSON.stringify(r)).sort().map((t) => JSON.parse(t))]));
for (const [name, spec] of Object.entries(scenario.httpProxies ?? {})) {
  const { port, status } = typeof spec === 'number' ? { port: spec } : spec;
  const records = proxyRecords[name] = [];
  const toMock = (target) => { try { return LOOPBACK_NAMES.has(new URL(`http://${target}`).hostname); } catch { return false; } };
  const proxy = http.createServer((req, res) => {
    records.push({ method: req.method, target: req.url, headers: req.headers });
    let url = null; try { url = new URL(req.url); } catch {}
    if (status || !url || url.protocol !== 'http:' || !toMock(url.host)) { res.writeHead(status || 502); res.end(); return; }
    const upstream = http.request({ host: '127.0.0.1', port: MOCK_PORT, method: req.method, path: url.pathname + url.search, headers: req.headers }, (up) => {
      res.writeHead(up.statusCode, up.headers); up.pipe(res);
    });
    upstream.on('error', () => res.destroy());
    req.pipe(upstream);
  });
  proxy.on('connect', (req, client, head) => {
    records.push({ method: req.method, target: req.url, headers: req.headers });
    client.on('error', () => {});
    if (status || !toMock(req.url)) { client.end(`HTTP/1.1 ${status || 502} Proxy Refused\r\n\r\n`); return; }
    const upstream = net.connect(MOCK_PORT, '127.0.0.1', () => {
      client.write('HTTP/1.1 200 Connection Established\r\n\r\n');
      if (head?.length) upstream.write(head);
      client.pipe(upstream); upstream.pipe(client);
    });
    upstream.on('error', () => client.destroy());
    client.on('close', () => upstream.destroy()); upstream.on('close', () => client.destroy());
  });
  await new Promise((r) => proxy.listen(port, '127.0.0.1', r));
}

// ── 本机 HTTPS（场景字段 httpsPort）──
// {"httpsPort": 38419}：在 127.0.0.1 上起一个 TLS 前端，解密后原样转给假 provider（请求照常按规则应答、记录）。
// 证书是仓库里一次性的测试 CA 与 127.0.0.1 叶子（src/pilotdeck-gateway/tests/fixtures/model-stream/tls.json，
// 不是凭据）；CA 写到 {HTTPS_CA}（根目录下、两棵比较树之外），场景在 env 里写 "NODE_EXTRA_CA_CERTS": "{HTTPS_CA}"
// 让两侧都信任它（基线是 Node 的启动参数，Rust 照 Node 读同一个变量）。{HTTPS} 是 https://127.0.0.1:<端口>。
if (scenario.httpsPort !== undefined) {
  const tls = JSON.parse(fs.readFileSync(path.join(REPO, 'src/pilotdeck-gateway/tests/fixtures/model-stream/tls.json'), 'utf8'));
  fs.mkdirSync(path.dirname(HTTPS_CA), { recursive: true });
  fs.writeFileSync(HTTPS_CA, tls.ca);
  const front = https.createServer({ cert: tls.cert, key: tls.key }, (req, res) => {
    const upstream = http.request({ host: '127.0.0.1', port: MOCK_PORT, method: req.method, path: req.url, headers: req.headers }, (up) => {
      res.writeHead(up.statusCode, up.headers); up.pipe(res);
    });
    upstream.on('error', () => res.destroy());
    req.on('error', () => upstream.destroy());
    res.on('close', () => upstream.destroy());
    req.pipe(upstream);
  });
  front.on('tlsClientError', () => {});
  await new Promise((r) => front.listen(scenario.httpsPort, '127.0.0.1', r));
}

// ── 被测一侧 ──
// pathDir：场景自带的假可执行文件所在目录（相对根目录，放进 files 并给 mode），排在子进程 PATH 的最前面。
// 被测进程本身（基线的 node、Rust 的绝对路径）在此之前已按采集器自己的 PATH 解析，不受影响；
// 目录里不要放 node。场景 env 里显式写的 PATH 仍然盖过这里。
const pathDir = scenario.pathDir === undefined ? null : path.join(ROOT, scenario.pathDir);
if (pathDir && !pathDir.startsWith(ROOT + '/')) throw new Error(`pathDir escapes root: ${scenario.pathDir}`);
if (pathDir) fs.mkdirSync(pathDir, { recursive: true });
const env = {
  PATH: pathDir ? `${pathDir}:${process.env.PATH}` : process.env.PATH, HOME: home, PILOT_HOME: pilotHome, TMPDIR: tmp,
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
// 外部常驻的 agent loop sidecar（TCP 部署用）：基线是 dist 里的 sidecar 入口，Rust 是独立的 sidecar 可执行文件。
SIDES.ts.sidecar = ['node', path.join(BASELINE, 'dist/src/cli/pilotdeck-agent-loop-sidecar.js')];
SIDES.rust.sidecar = [path.join(TARGET, 'pilotdeck-agent-loop-sidecar')];
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
    if (await portOpen(GATEWAY_PORT)) break;
    await sleep(50);
  }
  out.startMs = Date.now() - t0;
}

// ── WS 连接（惰性建立；一个场景一条连接，除非步骤要求重连）──
// frames 是当前连接收到的帧，步骤从这里取自己的帧；allFrames 跨重连累积，收尾时从中取通知。
let ws; let frames = []; let wake = []; const allFrames = [];
// 不发关闭帧的断开（{disconnect: "destroy"} / {disconnect: "reset"}）要拿到 WS 底下的 TCP 套接字。
// Node 自带的 WebSocket（undici）经 net.connect 建连：只在建连期间、只对连向 gateway 端口的那次调用记下套接字。
let wsSocket = null; let captureWsSocket = false;
const netConnect = net.connect;
net.connect = function (...args) {
  const socket = netConnect.apply(this, args);
  if (captureWsSocket && Number(args[0]?.port) === GATEWAY_PORT) { wsSocket = socket; captureWsSocket = false; }
  return socket;
};
const connect = async (hello) => {
  wsSocket = null; captureWsSocket = true;
  ws = new WebSocket(`ws://127.0.0.1:${GATEWAY_PORT}/ws`);
  frames = [];
  const got = (f) => { frames.push(f); allFrames.push(f); wake.splice(0).forEach((w) => w()); };
  ws.onmessage = (e) => { let f; try { f = JSON.parse(e.data); } catch { f = { raw: String(e.data) }; } got(f); };
  let closed = null;
  ws.onclose = (e) => { closed = { code: e.code, reason: e.reason }; got({ type: '__closed', ...closed }); };
  await new Promise((resolve) => { ws.onopen = resolve; ws.onerror = resolve; });
  captureWsSocket = false;
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
// sidecar 步骤：{sidecar: "start", port, env} 起一个常驻的 agent loop sidecar 进程（环境是场景环境盖上步骤 env，
// 在 port 上等它开始监听）；{sidecar: "stop", signal} 给最近起的那个发信号（缺省 SIGTERM），等它退出，
// 记下退出码、信号、stdout、stderr（15 秒不退就 SIGKILL，如实记成 SIGKILL）。进程没等到监听就自己退出时，
// start 同样记下这些。场景结束时还在跑的由收尾清理按进程组杀掉，不记输出。
let sidecarProc = null;
// tcpProxy 步骤：{tcpProxy: "start", port, target} 在 127.0.0.1:port 上起一个透明的 TCP 转发到 target，
// 用来在 sidecar 进程不退出的情况下断开 gateway 与它之间的连接（假 provider 应答上的 cutProxy）；
// {tcpProxy: "cut"} 立刻断开现有的全部连接。代理只转发字节，不记录也不改内容。
const proxySockets = [];
let proxyServer = null;
const proxyStep = async (step) => {
  if (step.tcpProxy === 'start') {
    proxyServer = net.createServer((client) => {
      const upstream = net.connect(step.target, '127.0.0.1');
      proxySockets.push(client, upstream);
      const drop = () => {
        client.destroy(); upstream.destroy();
        for (const s of [client, upstream]) { const i = proxySockets.indexOf(s); if (i >= 0) proxySockets.splice(i, 1); }
      };
      client.on('error', drop).on('close', drop); upstream.on('error', drop).on('close', drop);
      client.pipe(upstream); upstream.pipe(client);
    });
    await new Promise((r) => proxyServer.listen(step.port, '127.0.0.1', r));
    return { action: 'start' };
  }
  // live：断开前代理上还连着的 gateway 连接数（每条连接在代理两端各占一个套接字）。
  if (step.tcpProxy === 'cut') { const live = proxySockets.length / 2; for (const s of proxySockets.splice(0)) s.destroy(); return { action: 'cut', live }; }
  throw new Error(`unknown tcpProxy action: ${step.tcpProxy}`);
};
const sidecarStep = async (step) => {
  if (step.sidecar === 'start') {
    const [cmd, ...prefix] = SIDES[side].sidecar;
    const child = spawn(cmd, prefix, { cwd: project, env: stepEnv(step), stdio: ['ignore', 'pipe', 'pipe'], detached: true });
    children.push(child);
    const record = { stdout: '', stderr: '', closed: new Promise((r) => child.on('close', (c, s) => r([c, s]))) };
    child.stdout.on('data', (d) => { record.stdout += d; }); child.stderr.on('data', (d) => { record.stderr += d; });
    sidecarProc = { child, record };
    const t0 = Date.now();
    let listening = false;
    while (child.exitCode === null && child.signalCode === null && Date.now() - t0 < (step.timeout ?? STEP_TIMEOUT_MS)) {
      if (await portOpen(step.port)) { listening = true; break; }
      await sleep(50);
    }
    if (listening) return { action: 'start', listening };
    if (child.exitCode === null && child.signalCode === null) return { action: 'start', listening, timedOut: true };
    const [exitCode, signal] = await record.closed;
    return { action: 'start', listening, exitCode, signal, stdout: record.stdout, stderr: record.stderr };
  }
  if (step.sidecar === 'stdio') {
    // stdio 部署：起一个 sidecar 子进程（与 start 同一个程序、同一套环境），按 send 往它的 stdin 写（见 sendScript；
    // 字节项上的 end 关闭 stdin，不写就一直开着），stdout 按行记录。写完后等到进程退出或 readMs（缺省 1000）到时；
    // 还没退出的记 exited: false，再发 signal（缺省 SIGTERM，15 秒不退就 SIGKILL），记下退出码与信号；stderr 原样记。
    const [cmd, ...prefix] = SIDES[side].sidecar;
    const child = spawn(cmd, prefix, { cwd: project, env: stepEnv(step), stdio: ['pipe', 'pipe', 'pipe'], detached: true });
    children.push(child);
    let stdout = Buffer.alloc(0), stderr = '';
    const wakers = [];
    const poke = () => wakers.splice(0).forEach((w) => w());
    let exited = false;
    const closed = new Promise((r) => child.on('close', (c, s) => { exited = true; poke(); r([c, s]); }));
    child.stdout.on('data', (d) => { stdout = Buffer.concat([stdout, d]); poke(); });
    child.stderr.on('data', (d) => { stderr += d; });
    // 进程先退出时 stdin 上的写会 EPIPE：吞掉，不让采集器崩溃（输出与退出码照记）。
    child.stdin.on('error', () => {});
    const wait = (pred, ms) => new Promise((resolve) => {
      const t0 = Date.now();
      const check = () => { if (pred() || exited || Date.now() - t0 >= ms) return resolve(); wakers.push(check); setTimeout(check, Math.min(50, Math.max(1, ms - (Date.now() - t0)))); };
      check();
    });
    const script = await sendScript(step.send ?? [], {
      closed: () => exited,
      write: (bytes) => child.stdin.write(bytes),
      end: () => child.stdin.end(),
      pause: () => {}, resume: () => {},
      received: () => stdout.toString('utf8'),
      wait,
    });
    await wait(() => false, step.readMs ?? 1000);
    const exitedByItself = exited;
    if (!exited) child.kill(step.signal ?? 'SIGTERM');
    const killer = setTimeout(() => { try { process.kill(-child.pid, 'SIGKILL'); } catch {} }, 15000);
    const [exitCode, signal] = await closed;
    clearTimeout(killer);
    return { action: 'stdio', ...splitLines(stdout.toString('utf8')), stderr, exited: exitedByItself, exitCode, signal,
      ...(script.untilMissed === undefined ? {} : { untilMissed: script.untilMissed }) };
  }
  if (step.sidecar === 'stop') {
    if (!sidecarProc) return { action: 'stop', running: false };
    const { child, record } = sidecarProc;
    sidecarProc = null;
    if (child.exitCode === null && child.signalCode === null) child.kill(step.signal ?? 'SIGTERM');
    const killer = setTimeout(() => { try { process.kill(-child.pid, 'SIGKILL'); } catch {} }, 15000);
    const [exitCode, signal] = await record.closed;
    clearTimeout(killer);
    return { action: 'stop', exitCode, signal, stdout: record.stdout, stderr: record.stderr };
  }
  throw new Error(`unknown sidecar action: ${step.sidecar}`);
};
// ── 原始字节与并发连接（raw / flood 步骤，见 README）──
// raw：自己开一条 TCP 连接，可选地做 WS 升级和 hello，然后按顺序写原始字节或手工拼的 WS 帧（可不加掩码、
// 改 FIN / RSV / 声明长度），读到服务端关闭或 readMs 到时为止，记下解析后的应答。浏览器式客户端做不到的
// 输入（非法帧、超大帧、半截请求行、不升级的握手）都走这里。
const wsClientFrame = (f) => {
  const payload = f.json !== undefined ? Buffer.from(JSON.stringify(substDeep(f.json)))
    : f.hex !== undefined ? Buffer.from(f.hex, 'hex')
    : Buffer.from(subst(f.prefix ?? '') + subst(f.text ?? '').repeat(f.repeat ?? 1) + subst(f.suffix ?? ''));
  const len = f.length ?? payload.length; const mask = f.mask !== false;
  const b0 = (f.fin === false ? 0 : 0x80) | ((f.rsv ?? 0) << 4) | (f.opcode ?? 1);
  let head;
  if (len < 126) head = Buffer.from([b0, (mask ? 0x80 : 0) | len]);
  else if (len <= 0xffff) { head = Buffer.alloc(4); head[0] = b0; head[1] = (mask ? 0x80 : 0) | 126; head.writeUInt16BE(len, 2); }
  else { head = Buffer.alloc(10); head[0] = b0; head[1] = (mask ? 0x80 : 0) | 127; head.writeBigUInt64BE(BigInt(len), 2); }
  if (!mask) return Buffer.concat([head, payload]);
  const key = Buffer.from([0x12, 0x34, 0x56, 0x78]); const body = Buffer.from(payload);
  for (let i = 0; i < body.length; i++) body[i] ^= key[i & 3];
  return Buffer.concat([head, key, body]);
};
// 服务端帧：文本能解析成 JSON 的记解析结果，长文本只记长度和 sha256，关闭帧记码和原因。
const readServerFrames = (buf) => {
  const frames = []; let off = 0;
  for (;;) {
    if (buf.length - off < 2) break;
    let len = buf[off + 1] & 0x7f, at = off + 2;
    if (len === 126) { if (buf.length - off < 4) break; len = buf.readUInt16BE(off + 2); at = off + 4; }
    else if (len === 127) { if (buf.length - off < 10) break; len = Number(buf.readBigUInt64BE(off + 2)); at = off + 10; }
    if (buf[off + 1] & 0x80) at += 4;
    if (buf.length < at + len) break;
    const p = buf.subarray(at, at + len); const op = buf[off] & 0x0f;
    const fr = { fin: !!(buf[off] & 0x80), rsv: (buf[off] >> 4) & 7, opcode: op, masked: !!(buf[off + 1] & 0x80) };
    if (op === 8) Object.assign(fr, p.length >= 2 ? { code: p.readUInt16BE(0), reason: p.subarray(2).toString() } : { code: null });
    else { const t = p.toString(); let j; try { j = JSON.parse(t); } catch {} if (j !== undefined) fr.json = j; else if (t.length <= 4096) fr.text = t; else fr.len = p.length, fr.sha256 = createHash('sha256').update(p).digest('hex'); }
    frames.push(fr); off = at + len;
  }
  return { frames, rest: buf.subarray(off) };
};
// HTTP 应答头：状态行和头（小写名，去掉 date），正文原样。
const openRaw = (port) => new Promise((resolve) => {
  const s = net.connect(port, '127.0.0.1');
  // 收到的块先攒着，用到 buf 时才拼（几十 MB 的应答逐块 concat 是平方级）。
  let joined = Buffer.alloc(0); let pending = [];
  const st = { s, ended: null, error: null, closed: false, wake: [],
    get buf() { if (pending.length) { joined = Buffer.concat([joined, ...pending]); pending = []; } return joined; },
    set buf(v) { joined = v; pending = []; } };
  const poke = () => st.wake.splice(0).forEach((w) => w());
  s.on('data', (d) => { pending.push(d); poke(); });
  s.on('end', () => { st.ended = st.ended ?? 'fin'; poke(); });
  s.on('error', (e) => { st.error = e.code ?? String(e); poke(); });
  s.on('close', () => { st.closed = true; if (!st.ended && st.error) st.ended = 'reset'; poke(); resolve(st); });
  s.once('connect', () => resolve(st));
});
const rawWait = (st, pred, ms) => new Promise((resolve) => {
  const t0 = Date.now();
  const check = () => { if (pred() || st.closed || Date.now() - t0 >= ms) return resolve(); st.wake.push(check); setTimeout(check, Math.min(50, Math.max(1, ms - (Date.now() - t0)))); };
  check();
});
const upgradeRequest = (spec) => {
  const o = spec === true ? {} : spec;
  const headers = { Host: `127.0.0.1:${GATEWAY_PORT}`, Upgrade: 'websocket', Connection: 'Upgrade', 'Sec-WebSocket-Key': 'YmJ4LXJhdy1rZXktMDAwMA==', 'Sec-WebSocket-Version': '13', ...substDeep(o.headers ?? {}) };
  return `${o.method ?? 'GET'} ${subst(o.path ?? '/ws')} HTTP/1.1\r\n${Object.entries(headers).filter(([, v]) => v !== null).map(([k, v]) => `${k}: ${v}`).join('\r\n')}\r\n\r\n`;
};
// 发送脚本：raw 步骤（TCP）与 sidecar stdio 步骤共用。依次处理 send 里的项：
//   {text} / {hex} / {frame}（拼字节，见 README）、{json: 值}（写 JSON.stringify(值) 加换行，一行一条按行协议的消息）；
//   字节项可带 times / splitAt / end（写完关闭写方向：TCP 半关闭，stdio 关闭 stdin）；
//   {wait: 毫秒}；{pauseRead} / {resumeRead}（只对 TCP）；
//   {until: {lines: N} | {text: "…"}, timeout, save}：等对方的输出里出现第 N 个完整行（从头数）/ 在上一次 until
//     停下的位置之后（写 fromStart: true 时从头找）出现这段文字且那一行已经完整，再往下走；save 的正则在那一整行上匹配（与步骤上的 save 同一套
//     {saved.名字}），用来应答对方发来的请求（例如 sidecar 发给宿主的 module_call）。timeout（缺省 10000 毫秒）
//     内没等到，后面的项不再执行，结果里记 untilMissed（这一项的下标）。
// 每一项在用到时才替换 {saved.名字}，所以前面 until 刚存下的值后面的项就能引用。对方关闭后不再写。
const sendScript = async (items, io) => {
  let cursor = 0;
  for (const [index, spec] of items.entries()) {
    if (io.closed()) break;
    const item = withSaved(spec);
    if (item.wait) { await io.wait(() => false, item.wait); continue; }
    if (item.pauseRead) { io.pause(); continue; }
    if (item.resumeRead) { io.resume(); continue; }
    if (item.until) {
      const find = () => {
        const text = io.received();
        if (item.until.lines !== undefined) {
          let start = 0;
          for (let n = 1; ; n++) {
            const end = text.indexOf('\n', start);
            if (end < 0) return null;
            if (n === item.until.lines) return { line: text.slice(start, end), next: end + 1 };
            start = end + 1;
          }
        }
        const at = text.indexOf(subst(item.until.text), item.until.fromStart ? 0 : cursor);
        const end = at < 0 ? -1 : text.indexOf('\n', at);
        if (end < 0) return null;
        return { line: text.slice(text.lastIndexOf('\n', at - 1) + 1, end), next: end + 1 };
      };
      let hit = find();
      if (!hit) await io.wait(() => (hit = find()) !== null, item.timeout ?? 10000);
      if (!hit) return { untilMissed: index };
      capture(item.save, hit.line);
      cursor = Math.max(cursor, hit.next);
      continue;
    }
    const one = item.json !== undefined ? Buffer.from(JSON.stringify(substDeep(item.json)) + '\n')
      : item.frame ? wsClientFrame(item.frame) : item.hex !== undefined ? Buffer.from(item.hex, 'hex')
      : Buffer.from(subst(item.prefix ?? '') + subst(item.text ?? '').repeat(item.repeat ?? 1) + subst(item.suffix ?? ''));
    // times：同样的字节连写 n 遍、一次写出（例如不等应答连发很多个请求帧）。
    const bytes = item.times ? Buffer.concat(Array(item.times).fill(one)) : one;
    // splitAt：先写前 n 个字节，停 200 毫秒再写其余（一帧跨两次读）。
    if (item.splitAt) { io.write(bytes.subarray(0, item.splitAt)); await io.wait(() => false, 200); io.write(bytes.subarray(item.splitAt)); }
    else if (bytes.length) io.write(bytes);
    if (item.end) io.end();
  }
  return {};
};
// 按行协议的输出：按换行切开，每行能解析成 JSON 的记解析结果，否则记 {text}；没有换行结尾的最后一段另记 partial。
const splitLines = (text) => {
  const parts = text.split('\n'); const tail = parts.pop();
  return { lines: parts.map((l) => { try { return JSON.parse(l); } catch { return { text: l }; } }), ...(tail ? { partial: tail } : {}) };
};
const rawStep = async (spec) => {
  const st = await openRaw(spec.port ?? GATEWAY_PORT);
  if (st.error && !st.s.writable) return { refused: st.error };
  const r = {};
  let upgraded = false;
  if (spec.ws) {
    st.s.write(upgradeRequest(spec.ws));
    await rawWait(st, () => st.buf.includes('\r\n\r\n'), 5000);
    const i = st.buf.indexOf('\r\n\r\n');
    if (i >= 0) { r.upgrade = parseHttpHead(st.buf.subarray(0, i).toString()); delete r.upgrade.body; st.buf = st.buf.subarray(i + 4); upgraded = r.upgrade.status.includes(' 101 '); }
    if (upgraded && spec.hello) {
      st.s.write(wsClientFrame({ json: { type: 'hello', protocolVersion: '1.2', clientName: 'test', clientVersion: '0.0.0', token: TOKEN } }));
      await rawWait(st, () => readServerFrames(st.buf).frames.length > 0, 5000);
    }
  }
  const script = await sendScript(spec.send ?? [], {
    closed: () => st.closed,
    write: (bytes) => st.s.write(bytes),
    end: () => st.s.end(),
    // pauseRead / resumeRead：停止从套接字读（字节留在内核缓冲里，服务端写不出去时就在它那边排队），之后恢复。
    pause: () => st.s.pause(), resume: () => st.s.resume(),
    received: () => st.buf.toString('utf8'),
    wait: (pred, ms) => rawWait(st, pred, ms),
  });
  if (script.untilMissed !== undefined) r.untilMissed = script.untilMissed;
  await rawWait(st, () => false, spec.readMs ?? 1000);
  // lines：对方说的是按行分隔的 JSON（agent loop sidecar 的原始协议），收到的字节按换行切开，
  // 每行能解析成 JSON 的记解析结果，否则记原文；最后一段没有换行结尾的另记 partial。
  if (spec.lines) {
    // 键序与原来相同：lines、partial 在 untilMissed 之前（untilMissed 只在没等到时出现）。
    const { untilMissed } = r; delete r.untilMissed;
    Object.assign(r, splitLines(st.buf.toString('utf8')), untilMissed === undefined ? {} : { untilMissed });
  } else if (upgraded || (spec.ws && st.buf.length && !st.buf.subarray(0, 5).toString().startsWith('HTTP/'))) {
    const { frames, rest } = readServerFrames(st.buf); r.frames = frames; if (rest.length) r.trailingBytes = rest.length;
    // countFrames：帧太多时只记按「类型:id」（关闭帧记码与原因，其余记 opcode）分组的计数和总数，次序不记。
    if (spec.countFrames) {
      const counts = {};
      for (const f of frames) { const k = f.opcode === 8 ? `close:${f.code}:${f.reason}` : f.json ? `${f.json.type}:${f.json.id ?? ''}` : `opcode:${f.opcode}`; counts[k] = (counts[k] ?? 0) + 1; }
      r.frames = { total: frames.length, counts };
    }
  } else if (st.buf.length) r.http = parseHttpResponses(st.buf.toString('latin1'));
  r.serverClosed = st.closed ? (st.ended ?? 'close') : st.ended === 'fin' ? 'fin-half' : null;
  await rawClose(st, !spec.lines && (upgraded || / 101 /.test(r.http?.status ?? '')));
  return r;
};
// 收尾：升级过、还开着的连接先发关闭帧等服务端关（最多 1 秒），再断开。基线对只收到 FIN 的 WS 连接
// 不做处理（websocket.ts 不听 end），直接断开会让它停机时一直等这条连接（见上面 disconnect 的说明）。
// 收尾这段的往来不记录。
const rawClose = async (st, upgraded) => {
  if (upgraded && !st.closed) {
    st.s.write(wsClientFrame({ opcode: 8, hex: '03e8' }));
    await rawWait(st, () => false, 1000);
  }
  st.s.destroy();
};
// flood：{flood: N} 同时另开 N 条 WS 连接（升级 + hello），等每条拿到第一帧或被关（5 秒），连接保持到
// {flood: "release"} 或场景结束；结果按结局计数。用来比连接很多时服务端是否照常接受、照常服务别的请求。
const floodSockets = [];
const floodStep = async (step) => {
  if (step.flood === 'release') { const all = floodSockets.splice(0); await Promise.all(all.map((st) => rawClose(st, true))); return { released: all.length }; }
  const all = await Promise.all(Array.from({ length: step.flood }, () => openRaw(GATEWAY_PORT)));
  // 先等 101 再发 hello：跟升级请求挤在同一段里的字节基线不读（GatewayServer.ts:67 丢掉 upgrade 事件的 head）。
  for (const st of all) if (!st.closed) st.s.write(upgradeRequest(true));
  await Promise.all(all.map((st) => rawWait(st, () => st.buf.includes('\r\n\r\n'), 5000)));
  for (const st of all) if (!st.closed && st.buf.subarray(0, st.buf.indexOf('\r\n')).toString().includes(' 101 ')) st.s.write(wsClientFrame({ json: { type: 'hello', protocolVersion: '1.2', clientName: 'test', clientVersion: '0.0.0', token: TOKEN } }));
  await Promise.all(all.map((st) => rawWait(st, () => { const i = st.buf.indexOf('\r\n\r\n'); return i >= 0 && readServerFrames(st.buf.subarray(i + 4)).frames.length > 0; }, 5000)));
  const tally = {};
  for (const st of all) {
    const i = st.buf.indexOf('\r\n\r\n'); const status = i >= 0 ? st.buf.subarray(0, st.buf.indexOf('\r\n')).toString() : null;
    const first = i >= 0 ? readServerFrames(st.buf.subarray(i + 4)).frames[0] : undefined;
    const k = !status ? (st.closed ? `closed-before-upgrade:${st.ended ?? 'close'}` : 'no-upgrade-response')
      : !status.includes(' 101 ') ? status : first ? (first.json?.type ?? `opcode-${first.opcode}`) : st.closed ? `closed-after-upgrade:${st.ended ?? 'close'}` : 'no-frame';
    tally[k] = (tally[k] ?? 0) + 1;
    if (!st.closed) floodSockets.push(st);
  }
  return { flood: step.flood, tally };
};
let requestSeq = 0;
const runStep = async (step) => {
  if (step.raw) return { raw: await rawStep(step.raw) };
  if (step.flood) return await floodStep(step);
  if (step.sleep) { await sleep(step.sleep); return { slept: true }; }
  if (step.proxyLog) return { proxyLog: takeProxyLog() };
  if (step.disconnect === 'end') {
    // 只关写方向（半关闭，发 FIN）：不发关闭帧、不再发请求，但照常接收服务端的帧。之后可以再
    // {disconnect: "destroy"} 把这条连接彻底关掉；在那之前不要 connect 新连接（两条连接的帧会混在一起）。
    if (!ws || ws.readyState > 1 || !wsSocket) return { disconnected: false, mode: step.disconnect };
    wsSocket.end();
    return { disconnected: true, mode: step.disconnect };
  }
  if (step.disconnect === 'destroy' || step.disconnect === 'reset') {
    // 不发 WS 关闭帧，直接关掉底层 TCP：destroy 是普通关闭（FIN），reset 是 RST（SO_LINGER 0）。
    // 客户端这边随后看到的关闭（undici 报 1006）照常记下。
    if (!ws || ws.readyState > 1 || !wsSocket) return { disconnected: false, mode: step.disconnect };
    const done = waitFrame((f) => f.type === '__closed', 5000);
    if (step.disconnect === 'reset') wsSocket.resetAndDestroy(); else wsSocket.destroy();
    const last = await done;
    return { disconnected: true, mode: step.disconnect, close: last.type === '__closed' ? { code: last.code, reason: last.reason } : last };
  }
  if (step.disconnect) {
    if (!ws || ws.readyState > 1) return { disconnected: false };
    const done = waitFrame((f) => f.type === '__closed', 5000);
    ws.close();
    const last = await done;
    return { disconnected: true, close: last.type === '__closed' ? { code: last.code, reason: last.reason } : last };
  }
  if (step.connect) return { hello: await connect(step.connect === true ? undefined : substDeep(step.connect)) };
  if (step.vanish) {
    // {vanish: N}：另建 N 条 WS 连接，各自 hello、等到第一帧非通知应答后不发关闭帧直接关掉 TCP（FIN），
    // 模拟 N 个客户端进程被杀。逐条依次建，与场景的主连接互不相干：这些连接上的帧不进步骤结果和通知。
    const keep = wsSocket; const replies = {}; let opened = 0;
    for (let i = 0; i < step.vanish; i++) {
      wsSocket = null; captureWsSocket = true;
      const c = new WebSocket(`ws://127.0.0.1:${GATEWAY_PORT}/ws`);
      await new Promise((resolve) => { c.onopen = resolve; c.onerror = resolve; });
      captureWsSocket = false;
      const socket = wsSocket;
      if (c.readyState !== 1 || !socket) { c.onclose = null; replies.connectFailed = (replies.connectFailed ?? 0) + 1; continue; }
      opened++;
      const reply = new Promise((resolve) => {
        const timer = setTimeout(() => resolve('__timeout'), 5000);
        c.onmessage = (e) => { let f; try { f = JSON.parse(e.data); } catch { f = {}; } if (f.type !== 'notification') { clearTimeout(timer); resolve(f.type); } };
        c.onclose = () => { clearTimeout(timer); resolve('__closed'); };
      });
      c.send(JSON.stringify({ type: 'hello', protocolVersion: '1.2', clientName: 'test', clientVersion: '0.0.0', token: TOKEN }));
      const type = await reply;
      replies[type] = (replies[type] ?? 0) + 1;
      c.onmessage = null; c.onclose = null;
      socket.destroy();
    }
    wsSocket = keep;
    return { vanish: { opened, replies } };
  }
  if (step.ws) {
    let hello; if (!ws) hello = await connect();
    if (hello?.connectFailed) return { hello };
    // 当前连接已经断了（例如前面的 connect 没连上）：如实记下，不往关掉的连接上发。
    if (ws.readyState !== 1) return { notConnected: true };
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
      // delayMs：等这么久再发（例如工具开始后过一会儿再中止，让中止落在工具执行的后半段）。
      const send = () => { if (ws.readyState === 1) ws.send(JSON.stringify({ type: 'request', id: answerId, method: hit.rule.ws, params })); };
      if (hit.rule.delayMs) setTimeout(send, hit.rule.delayMs); else send();
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
    let text;
    try { text = await response.text(); } catch (error) { return { http: { status: response.status, headers: Object.fromEntries(response.headers), bodyError: String(error.cause?.code ?? error.message ?? error) } }; }
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
      // then 里的步骤同样支持 save / {saved.名字}（外层替换时还没存下的占位原样留着，这里逐步再换一次）。
      for (const inner of step.then ?? []) {
        const result = await runStep(withSaved(inner));
        capture(inner.save, JSON.stringify(result));
        then.push({ step: inner, ...result });
      }
      child.kill(step.signal ?? 'SIGTERM');
    }
    const killer = setTimeout(() => { try { process.kill(-child.pid, 'SIGKILL'); } catch {} }, reached ? 15000 : 0);
    const [exitCode, signal] = await closed;
    clearTimeout(killer);
    return { serve: { reached, exitCode, signal, stdout, stderr }, then, ...(timedOut && { timedOut }) };
  }
  if (step.sidecar) return { sidecar: await sidecarStep(step) };
  if (step.stopServer) {
    // 给 "server" 起的被测服务发信号（stopServer 的值是信号名），等它退出，记下退出码和信号；15 秒不退就
    // SIGKILL 整个进程组，如实记成 SIGKILL。之后收尾不再停机。
    if (!server || server.exitCode !== null || server.signalCode !== null) {
      return { stopServer: { running: false, exitCode: server?.exitCode ?? null, signal: server?.signalCode ?? null } };
    }
    const exited = new Promise((r) => server.once('exit', (code, sig) => r({ exitCode: code, signal: sig })));
    server.kill(step.stopServer);
    const killer = setTimeout(() => { try { process.kill(-server.pid, 'SIGKILL'); } catch {} }, 15000);
    const result = await exited;
    clearTimeout(killer);
    return { stopServer: result };
  }
  if (step.tcpProxy) return { tcpProxy: await proxyStep(step) };
  if (step.cli) {
    const [cmd, ...prefix] = SIDES[side].cli;
    const child = spawn(cmd, [...prefix, ...substDeep(step.cli)], { cwd: step.cwd ? subst(step.cwd) : project, env: stepEnv(step), stdio: ['pipe', 'pipe', 'pipe'] });
    let stdout = '', stderr = '';
    child.stdout.on('data', (d) => { stdout += d; }); child.stderr.on('data', (d) => { stderr += d; });
    let timedOut = false;
    const timer = setTimeout(() => { timedOut = true; child.kill('SIGKILL'); }, step.timeout ?? STEP_TIMEOUT_MS);
    let ended = false;
    const closed = new Promise((r) => child.on('close', (c, s) => { ended = true; r([c, s]); }));
    if (Array.isArray(step.stdin)) {
      // 分块输入（逐个回答交互式提问）：每块写之前等被测进程打出新的 stdout（比上一块写入时变长，且 100 毫秒内不再变化），
      // 让每块单独到达、各自回答一个提问；全部写完后关闭 stdin。进程先退出时剩下的块不写（EPIPE 吞掉）。
      child.stdin.on('error', () => {});
      (async () => {
        let mark = 0;
        for (const chunk of step.stdin) {
          for (;;) {
            if (ended) return;
            const len = stdout.length;
            if (len > mark) { await sleep(100); if (stdout.length === len) break; } else await sleep(20);
          }
          mark = stdout.length;
          child.stdin.write(chunk);
        }
        child.stdin.end();
      })();
    } else child.stdin.end(step.stdin ?? '');
    // until / then：stdout 或 stderr 里出现 until 时（进程还在不在都行），依次执行 then 里的步骤，然后照常等进程退出。
    // 进程没输出 until 就结束了（或超时）时 then 不执行，记成空数组。
    let then;
    if (step.until !== undefined) {
      const until = subst(step.until);
      const seen = () => stdout.includes(until) || stderr.includes(until);
      while (!seen() && !ended) await sleep(20);
      then = [];
      if (seen()) for (const inner of step.then ?? []) then.push({ step: inner, ...(await runStep(inner)) });
    }
    const [exitCode, signal] = await closed;
    clearTimeout(timer);
    return { cli: { exitCode, signal, stdout, stderr }, ...(then && { then }), ...(timedOut && { timedOut }) };
  }
  throw new Error(`unknown step: ${JSON.stringify(step)}`);
};
// 失败：步骤超时（含被采集器杀掉的 cli / serve）、连不上。只有步骤上声明了 expectTimeout 的超时算预期；
// 其余记进 failures，check.mjs 见到非预期的就把场景判为 INVALID，不管两侧是否相同。
const failuresOf = (step, r, at) => {
  const t = (x) => x?.type === '__timeout';
  const httpTimeout = ['TimeoutError', 'AbortError'].includes(r.http?.error);
  const kinds = [];
  if (r.timedOut || r.sidecar?.timedOut || t(r.close) || t(r.hello) || (r.frames ?? []).some(t) || (r.answers ?? []).some((a) => a.frames.some(t)) || httpTimeout) kinds.push('timeout');
  if (r.vanish?.replies.__timeout) kinds.push('timeout');
  if (r.hello?.connectFailed || r.notConnected || r.vanish?.replies.connectFailed || (r.http?.error && !httpTimeout)) kinds.push('connect');
  return [...kinds.map((kind) => ({ at, kind, expected: kind === 'timeout' && step.expectTimeout === true })),
    ...(r.then ?? []).flatMap((x, i) => failuresOf(x.step, x, `${at}.then[${i}]`))];
};
if (!out.startFailure) {
  for (const step of scenario.steps) {
    const result = await runStep(withSaved(step));
    capture(step.save, JSON.stringify(result));
    out.steps.push({ step, ...result });
  }
  await sleep(scenario.settleMs ?? 300);
}
out.failures = [
  ...(out.startFailure ? [{ at: 'start', kind: 'start', expected: scenario.expectStartFailure === true }] : []),
  ...out.steps.flatMap((r, i) => failuresOf(r.step, r, `steps[${i}]`)),
];
out.notifications = allFrames.filter((f) => f.type === 'notification');
out.unsolicited = allFrames.filter((f) => !['notification', 'hello_ok', 'response', 'event', '__closed'].includes(f.type));
if (ws) ws.close();
await Promise.all(floodSockets.splice(0).map((st) => rawClose(st, true)));

// ── 停机：SIGTERM，记退出码 ──
if (server && server.exitCode === null && server.signalCode === null) {
  const exited = new Promise((r) => server.on('exit', (code, signal) => r({ code, signal })));
  server.kill('SIGTERM');
  out.shutdown = await Promise.race([exited, sleep(15000).then(() => ({ timeout: true }))]);
  if (out.shutdown.timeout) out.failures.push({ at: 'shutdown', kind: 'timeout', expected: false });
}
// 18789 场景：采集结束就把自己起的进程全部收掉，再确认 18789 上没有留下监听者；留下了记一处失败并大声说。
if (PORT_18789) {
  for (const c of children) { try { process.kill(-c.pid, 'SIGKILL'); } catch {} }
  for (let i = 0; i < 50 && listening18789(); i++) await sleep(100);
  if (listening18789()) {
    out.failures.push({ at: 'release18789', kind: 'listener', expected: false });
    console.error('bbx: WARNING: something is still listening on 18789 after the run');
  }
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
const LARGE_FILE_BYTES = 16 * 1024 * 1024;
const walk = (dir) => fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => (a.name < b.name ? -1 : 1)).flatMap((e) => {
  const p = path.join(dir, e.name);
  if (e.isDirectory()) { const inner = walk(p); return inner.length ? inner : [[p + '/', { emptyDir: true }]]; }
  if (e.isSymbolicLink()) return [[p, { symlink: fs.readlinkSync(p) }]];
  const stat = fs.statSync(p);
  const mode = (stat.mode & 0o777).toString(8);
  // 命名管道、套接字等不是普通文件的：只记种类，不去读（读命名管道会一直阻塞）。
  if (!stat.isFile()) return [[p, { mode, special: stat.isFIFO() ? 'fifo' : stat.isSocket() ? 'socket' : 'other' }]];
  // 超过 LARGE_FILE_BYTES 的文件（场景用 files 的 repeat 种的大文件）不把全文放进产物：比长度、全文 sha256，
  // 以及末尾 64 KiB 里完整的行（tail，能解析成 JSON 的记解析结果、否则记原文；被测进程追加在大文件末尾的记录在这里比）。
  if (stat.size > LARGE_FILE_BYTES) {
    const bytes = fs.readFileSync(p);
    const tail = bytes.subarray(Math.max(0, bytes.length - 65536)).toString('utf8').split('\n').slice(1).filter((l) => l !== '')
      .map((l) => { try { return { json: JSON.parse(l) }; } catch { return { text: l }; } });
    const head = { mode, largeBytes: stat.size, sha256: createHash('sha256').update(bytes).digest('hex') };
    // .jsonl 的末尾记录全能解析时放在 jsonl 下（tailOnly 标明只是末尾），比较器对会话记录的归一（标题记录的落点等）照样适用。
    if (p.endsWith('.jsonl') && tail.every((l) => 'json' in l)) return [[p, { ...head, tailOnly: true, jsonl: tail.map((l) => l.json) }]];
    return [[p, { ...head, tail }]];
  }
  const bytes = fs.readFileSync(p);
  const text = bytes.toString('utf8');
  // sqlite 数据库：文件字节逐次不同（页内空闲区、时间戳），比的是内容——每张表的建表语句和全部行
  // （行按其 JSON 文本排序）。-wal / -shm 伴随文件只记有没有。
  if (/-(wal|shm)$/.test(p)) return [[p, { mode, sqliteAux: true }]];
  if (bytes.subarray(0, 16).toString('latin1') === 'SQLite format 3\0') {
    try {
      const db = new DatabaseSync(p, { readOnly: true });
      const objects = db.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name").all();
      const tables = {};
      for (const o of objects) {
        if (o.type !== 'table') continue;
        tables[o.name] = db.prepare(`SELECT * FROM "${o.name.replaceAll('"', '""')}"`).all()
          .map((row) => Object.fromEntries(Object.entries(row).map(([k, v]) => [k, typeof v === 'bigint' ? Number(v) : v instanceof Uint8Array ? { blobSha256: createHash('sha256').update(v).digest('hex') } : v])))
          .sort((a, b) => (JSON.stringify(a) < JSON.stringify(b) ? -1 : 1));
      }
      db.close();
      return [[p, { mode, sqlite: { objects: objects.map((o) => ({ ...o })), tables } }]];
    } catch (error) { return [[p, { mode, sqliteError: String(error.message ?? error) }]]; }
  }
  // 其它非 UTF-8 文件：比长度和内容的 sha256。
  if (Buffer.from(text, 'utf8').compare(bytes) !== 0) return [[p, { mode, binaryBytes: bytes.length, sha256: createHash('sha256').update(bytes).digest('hex') }]];
  if (p.endsWith('.jsonl')) { try { return [[p, { mode, jsonl: text.split('\n').filter(Boolean).map((l) => JSON.parse(l)), raw: rawPrint(text) }]]; } catch {} }
  if (p.endsWith('.json')) { try { return [[p, { mode, json: JSON.parse(text), raw: rawPrint(text) }]]; } catch {} }
  return [[p, { mode, text }]];
});
if (scenario.httpProxies) out.proxies = takeProxyLog();
out.tree = Object.fromEntries([...walk(home), ...walk(project)].map(([p, v]) => [p.slice(ROOT.length), v]));
fs.mkdirSync(path.dirname(path.resolve(outPath)), { recursive: true });
fs.writeFileSync(outPath, JSON.stringify(out, null, 1) + '\n');
console.error(`bbx: ${side} ${scenario.name}: ${out.startFailure ? 'START FAILED' : `${out.steps.length} steps`} -> ${outPath}`);
process.exit(0);
