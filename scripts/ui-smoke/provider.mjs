#!/usr/bin/env node
// ui-smoke · 假模型 provider（OpenAI chat completions 形状，流式与非流式都支持）。
// 用法：node scripts/ui-smoke/provider.mjs <端口> <请求日志.jsonl>
// 只监听 127.0.0.1。应答由「最后一条消息」决定，与请求先后无关：
//   - 最后一条是 tool 结果         → 文本 "UI_SMOKE_TOOL_DONE"
//   - 最后一条 user 含 UI_SMOKE_READ  → read_file(note.txt)
//   - 最后一条 user 含 UI_SMOKE_WRITE → write_file(written.txt)
//   - 最后一条 user 含 UI_SMOKE_DENY  → write_file(denied.txt)（界面侧拒绝）
//   - 最后一条 user 含 UI_SMOKE_ASK   → ask_user_question（界面侧回答）
//   - 最后一条 user 含 UI_SMOKE_SLOW  → 先发一段文本，然后挂住直到客户端断开（用于中止、排队）
//   - 其他（含标题生成等非流式请求） → 文本 "UI_SMOKE_OK"
import fs from 'node:fs';
import http from 'node:http';

const [port, logPath] = process.argv.slice(2);
if (!port || !logPath) { console.error('usage: provider.mjs <port> <log.jsonl>'); process.exit(2); }

const textOf = (content) => (typeof content === 'string' ? content
  : Array.isArray(content) ? content.map((p) => (typeof p === 'string' ? p : p?.text ?? '')).join('\n') : '');

function decide(body) {
  const messages = Array.isArray(body?.messages) ? body.messages : [];
  const last = messages[messages.length - 1];
  if (last?.role === 'tool') return { rule: 'after-tool', text: 'UI_SMOKE_TOOL_DONE' };
  const lastUser = [...messages].reverse().find((m) => m.role === 'user');
  const text = textOf(lastUser?.content);
  if (body?.stream && text.includes('UI_SMOKE_READ')) return { rule: 'read', tool: { name: 'read_file', arguments: { file_path: 'note.txt' } } };
  if (body?.stream && text.includes('UI_SMOKE_WRITE')) return { rule: 'write', tool: { name: 'write_file', arguments: { file_path: 'written.txt', content: 'written by ui-smoke\n' } } };
  if (body?.stream && text.includes('UI_SMOKE_DENY')) return { rule: 'deny', tool: { name: 'write_file', arguments: { file_path: 'denied.txt', content: 'should not be written\n' } } };
  if (body?.stream && text.includes('UI_SMOKE_ASK')) return { rule: 'ask', tool: { name: 'ask_user_question', arguments: { questions: [{ question: 'Which color?', header: 'Color', options: [{ label: 'Red', description: 'warm' }, { label: 'Blue', description: 'cool' }], multiSelect: false }] } } };
  if (body?.stream && text.includes('UI_SMOKE_SLOW')) return { rule: 'slow', slow: true };
  return { rule: body?.stream ? 'default' : 'non-stream', text: 'UI_SMOKE_OK' };
}

const server = http.createServer(async (req, res) => {
  const parts = []; for await (const p of req) parts.push(p);
  const raw = Buffer.concat(parts).toString('utf8');
  let body = null; try { body = JSON.parse(raw); } catch {}
  const d = decide(body);
  fs.appendFileSync(logPath, JSON.stringify({ at: Date.now(), method: req.method, url: req.url, rule: d.rule, headers: req.headers, body: body ?? { raw } }) + '\n');
  const usage = { prompt_tokens: 12, completion_tokens: 4, total_tokens: 16 };
  const message = d.tool
    ? { role: 'assistant', content: null, tool_calls: [{ id: `call_ui_smoke_${d.rule}`, type: 'function', function: { name: d.tool.name, arguments: JSON.stringify(d.tool.arguments) } }] }
    : { role: 'assistant', content: d.text ?? 'UI_SMOKE_SLOW_PARTIAL' };
  const finish = d.tool ? 'tool_calls' : 'stop';
  if (!body?.stream) {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ id: 'chatcmpl-ui-smoke', model: body?.model, choices: [{ index: 0, message, finish_reason: finish }], usage }));
    return;
  }
  res.writeHead(200, { 'content-type': 'text/event-stream' });
  const delta = message.tool_calls
    ? { role: 'assistant', tool_calls: message.tool_calls.map((t) => ({ index: 0, ...t })) }
    : { role: 'assistant', content: message.content };
  res.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta, finish_reason: null }] })}\n\n`);
  if (d.slow) {
    // 挂住，直到对方断开或 120 秒。
    // 记下对方何时断开（中止是否真的取消了发往模型的请求）。
    const t0 = Date.now();
    const timer = setTimeout(() => res.end('data: [DONE]\n\n'), 120000);
    res.on('close', () => {
      clearTimeout(timer);
      fs.appendFileSync(logPath, JSON.stringify({ at: Date.now(), rule: 'slow-closed', afterMs: Date.now() - t0, byClient: !res.writableFinished }) + '\n');
    });
    return;
  }
  res.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: {}, finish_reason: finish }], usage })}\n\n`);
  res.end('data: [DONE]\n\n');
});
server.listen(Number(port), '127.0.0.1', () => console.log(`provider listening 127.0.0.1:${port}`));
