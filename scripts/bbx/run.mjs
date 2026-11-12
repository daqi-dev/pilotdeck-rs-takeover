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
import { DatabaseSync } from 'node:sqlite';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const BASELINE = process.env.BBX_BASELINE || path.join(REPO, 'resources/PilotDeck');
const TARGET = process.env.BBX_TARGET || path.join(REPO, 'src/target/debug');
// 并行槽位：BBX_SLOT=1..8 时换一套等长的目录和端口（pdrs-bbx → pdrs-bbN，3841x → 384(1+N)x），
// 场景文件里写死的同名路径和端口在读入时做同样的替换。两侧在同一个槽位里跑，结论不受槽位影响；
// 不同槽位的采集产物之间不要互相比。槽位 0（缺省）就是原来的固定目录和端口。
const SLOT = Number(process.env.BBX_SLOT || 0);
if (!Number.isInteger(SLOT) || SLOT < 0 || SLOT > 8) { console.error('bbx: BBX_SLOT must be 0..8'); process.exit(2); }
// 根目录的父目录：macOS 上是 /private/tmp（/tmp 的物理路径），其他平台是 /tmp。场景里写死的
// /private/tmp/pdrs-bbx 在读入时换成本平台的写法。两侧在同一台机器上跑，路径长度随平台变不影响结论；
// 不同平台的采集产物不要互相比。
const TMP_BASE = process.platform === 'darwin' ? '/private/tmp' : '/tmp';
const slotText = (t) => {
  const local = TMP_BASE === '/private/tmp' ? t : t.replaceAll('/private/tmp/pdrs-bbx', `${TMP_BASE}/pdrs-bbx`);
  return SLOT ? local.replaceAll('pdrs-bbx', `pdrs-bb${SLOT}`).replaceAll('3841', `384${1 + SLOT}`) : local;
};
export const ROOT = slotText('/private/tmp/pdrs-bbx/root');
const GATEWAY_PORT = 38411 + 10 * SLOT;
const MOCK_PORT = 38412 + 10 * SLOT;
const TOKEN = 'bbx-token';
const STEP_TIMEOUT_MS = 30000;

const [side, scenarioPath, outPath] = process.argv.slice(2);
if (!['ts', 'rust'].includes(side) || !scenarioPath || !outPath) {
  console.error('usage: run.mjs <ts|rust> <scenario.json> <out.json>');
  process.exit(2);
}
const scenario = JSON.parse(slotText(fs.readFileSync(scenarioPath, 'utf8')));
const home = path.join(ROOT, 'home');
const pilotHome = path.join(home, '.pilotdeck');
const project = path.join(ROOT, 'proj');
const tmp = path.join(ROOT, 'tmp');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const subst = (text) => text
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
const listening18789 = (() => {
  const lsof = spawnSync('lsof', ['-nP', '-iTCP:18789', '-sTCP:LISTEN'], { encoding: 'utf8' });
  if (!lsof.error) return lsof.stdout.trim() !== '';
  const ss = spawnSync('ss', ['-H', '-ltn', 'sport = :18789'], { encoding: 'utf8' });
  if (!ss.error && ss.status === 0) return ss.stdout.trim() !== '';
  console.error('bbx: neither lsof nor ss is usable; cannot check port 18789'); process.exit(5);
})();
if (listening18789) {
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
  fs.writeFileSync(target, typeof content === 'string' ? subst(content) : JSON.stringify(substDeep(content), null, 2) + '\n');
  fs.utimesSync(target, seedBase + index, seedBase + index);
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
  const raw = Buffer.concat(parts).toString('utf8');
  let body = null; try { body = JSON.parse(raw); } catch {}
  const rule = rules.find((r) => (r.when ?? []).every((s) => raw.includes(s)) && (r.unless ?? []).every((s) => !raw.includes(s)));
  (providerRequests[rule.name] ??= []).push({
    method: req.method, url: req.url, headers: req.headers, body: body ?? { raw },
  });
  capture(rule.save, raw);
  const reply = withSaved(rule.reply ?? {});
  if (reply.status) { res.writeHead(reply.status, { 'content-type': 'application/json' }); res.end(JSON.stringify(reply.body ?? {})); return; }
  // 应答的组成：文本（text 或分块的 chunks）、推理文本（reasoning）、零到多个工具调用（tool / tools；
  // rawArguments 原样当参数字符串，用来回非法 JSON）、结束原因（finish）。delayMs 先等一会儿再回；
  // breakAfter: n 流里发出 n 块后断开连接；hangAfter: n 发出 n 块后挂住不结束（hangMs 后断开，缺省 60 秒）。
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
  const google = req.url.match(/\/models\/([^/:?]+):(generateContent|streamGenerateContent)(\?|$)/);
  if (google) {
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
    if (await portOpen(GATEWAY_PORT)) break;
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
out.tree = Object.fromEntries([...walk(home), ...walk(project)].map(([p, v]) => [p.slice(ROOT.length), v]));
fs.mkdirSync(path.dirname(path.resolve(outPath)), { recursive: true });
fs.writeFileSync(outPath, JSON.stringify(out, null, 1) + '\n');
console.error(`bbx: ${side} ${scenario.name}: ${out.startFailure ? 'START FAILED' : `${out.steps.length} steps`} -> ${outPath}`);
process.exit(0);
