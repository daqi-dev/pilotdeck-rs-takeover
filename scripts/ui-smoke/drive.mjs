#!/usr/bin/env node
// ui-smoke · 扮演浏览器前端，按典型顺序驱动 ui/server（HTTP API + /ws 聊天 WebSocket）。
// 用法：node scripts/ui-smoke/drive.mjs <ui/server 基址> <输出.json>
// 环境：UI_SMOKE_PROJECT（要登记的项目目录）、UI_SMOKE_PILOT_HOME。
// 输出 {steps:[...]}：每个 HTTP 步骤记状态与响应体；每个 WS 步骤记这一步期间 ui/server 推来的全部消息。
// 最后按 routes.mjs 的清单把其余路由各打一次（路由清单与跳过原因见那里）。
import fs from 'node:fs';
import { ROUTE_SWEEP } from './routes.mjs';

const [base, outPath] = process.argv.slice(2);
const PROJECT = process.env.UI_SMOKE_PROJECT;
const PILOT_HOME = process.env.UI_SMOKE_PILOT_HOME;
if (!base || !outPath || !PROJECT) { console.error('usage: drive.mjs <base> <out.json>  (UI_SMOKE_PROJECT required)'); process.exit(2); }
const steps = [];
const hit = new Set(); // 打到过的「方法 路由模板」
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const save = () => fs.writeFileSync(outPath, JSON.stringify({ steps, routesHit: [...hit].sort() }, null, 2));

// ── HTTP ──
const KEEP_HEADERS = ['content-type', 'x-projects-revision', 'location', 'content-disposition'];
async function http(name, method, url, { body, form, route, headers = {}, timeout = 30000, sse = false } = {}) {
  if (route) hit.add(route);
  const init = { method, headers: { ...headers }, signal: AbortSignal.timeout(timeout), redirect: 'manual' };
  if (form) init.body = form;
  else if (body !== undefined) { init.headers['content-type'] = 'application/json'; init.body = JSON.stringify(body); }
  let step; const t0 = Date.now();
  try {
    const res = await fetch(base + url, init);
    let text = '';
    if (sse) {
      // SSE：读到流结束或 3 秒为止。
      const reader = res.body.getReader(); const dec = new TextDecoder(); const t0 = Date.now();
      while (Date.now() - t0 < 3000) {
        const r = await Promise.race([reader.read(), sleep(3000 - (Date.now() - t0)).then(() => null)]);
        if (!r || r.done) break; text += dec.decode(r.value);
      }
      try { await reader.cancel(); } catch {}
    } else text = await res.text();
    let parsed = text; try { parsed = JSON.parse(text); } catch {}
    if (typeof parsed === 'string' && parsed.length > 2000) parsed = { truncatedText: parsed.slice(0, 300), length: parsed.length };
    const h = {}; for (const k of KEEP_HEADERS) if (res.headers.get(k)) h[k] = res.headers.get(k);
    step = { name, http: `${method} ${url}`, ...(body !== undefined ? { request: body } : {}), status: res.status, headers: h, body: parsed };
  } catch (e) {
    step = { name, http: `${method} ${url}`, error: String(e?.name === 'TimeoutError' ? 'timeout' : e?.message ?? e) };
  }
  step.t = [t0, Date.now()]; steps.push(step); save();
  return step;
}

// ── 聊天 WebSocket ──
let ws; const inbox = []; let wake = [];
async function connectWs() {
  hit.add('WS /ws');
  ws = new WebSocket(base.replace(/^http/, 'ws') + '/ws');
  ws.onmessage = (e) => { let f; try { f = JSON.parse(e.data); } catch { f = { raw: String(e.data) }; } inbox.push(f); wake.splice(0).forEach((w) => w()); };
  ws.onclose = (e) => { inbox.push({ __closed: e.code }); wake.splice(0).forEach((w) => w()); };
  await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = reject; });
}
// 发一帧，收集直到 until(帧) 为真（或超时），再静置 settle 毫秒把尾随帧收进来。
// answer(帧) 返回要回给 ui/server 的帧（权限批准等），也记进本步骤。
async function wsStep(name, frame, { until, timeout = 30000, settle = 800, answer } = {}) {
  const start = inbox.length; const st0 = Date.now(); const sent = []; let seen = start; let done = false; let timedOut = false;
  if (frame) { ws.send(JSON.stringify(frame)); sent.push(frame); }
  const t0 = Date.now();
  while (!done) {
    for (; seen < inbox.length; seen++) {
      const f = inbox[seen];
      const reply = answer?.(f);
      if (reply) { ws.send(JSON.stringify(reply)); sent.push(reply); }
      if (until?.(f)) { done = true; break; }
    }
    if (done || !until) break;
    if (Date.now() - t0 > timeout) { timedOut = true; break; }
    await new Promise((r) => { wake.push(r); setTimeout(r, 100); });
  }
  await sleep(settle);
  const frames = inbox.slice(start);
  const step = { name, ws: frame?.type ?? null, sent, frames, ...(timedOut ? { timedOut: true } : {}), t: [st0, Date.now()] };
  steps.push(step); save();
  return step;
}
const isComplete = (sid) => (f) => f.kind === 'complete' && (!sid || f.sessionId === sid);

// ── 典型前端流程 ──
const runId = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
const cmd = (command, extra = {}) => ({
  type: 'pilotdeck-command', command,
  options: { projectPath: PROJECT, cwd: PROJECT, permissionMode: 'default', toolsSettings: { allowedTools: [], disallowedTools: [], skipPermissions: false }, ...extra },
});

await http('health', 'GET', '/health', { route: 'GET /health' });
await http('auth-status', 'GET', '/api/auth/status', { route: 'GET /api/auth/status' });
await http('auth-user', 'GET', '/api/auth/user', { route: 'GET /api/auth/user' });
await http('onboarding-status', 'GET', '/api/user/onboarding-status', { route: 'GET /api/user/onboarding-status' });
await http('runtime-status', 'GET', '/api/user/runtime-status', { route: 'GET /api/user/runtime-status' });
await http('projects-initial', 'GET', '/api/projects', { route: 'GET /api/projects' });
await http('project-create', 'POST', '/api/projects/create', { body: { path: PROJECT }, route: 'POST /api/projects/create' });
const projects = await http('projects-after-create', 'GET', '/api/projects', { route: 'GET /api/projects' });
const proj = Array.isArray(projects.body) ? projects.body.find((p) => p.fullPath === PROJECT || p.path === PROJECT) : null;
const PN = encodeURIComponent(proj?.name ?? 'missing-project');
await http('sessions-empty', 'GET', `/api/projects/${PN}/sessions`, { route: 'GET /api/projects/:projectName/sessions' });

await connectWs();
await wsStep('active-sessions', { type: 'get-active-sessions' }, { until: (f) => f.type === 'active-sessions', timeout: 5000, settle: 200 });
await wsStep('session-activity', { type: 'get-session-activity' }, { until: (f) => f.type === 'session-activity-snapshot', timeout: 5000, settle: 200 });

// 1. 新会话，纯文本回复
const t1 = await wsStep('turn-text-new-session', cmd('hello from ui-smoke', { runId: runId(1), userVisibleInput: 'hello from ui-smoke' }), { until: isComplete() });
const SID = t1.frames.find((f) => f.kind === 'session_created')?.newSessionId ?? t1.frames.find((f) => f.sessionId)?.sessionId;
// 2. 工具调用：read_file（只读，不问权限）
await wsStep('turn-tool-read', cmd('UI_SMOKE_READ the note', { sessionId: SID, resume: true, runId: runId(2) }), { until: isComplete(SID) });
// 3. 权限询问：write_file，界面侧批准
await wsStep('turn-permission-allow', cmd('UI_SMOKE_WRITE a file', { sessionId: SID, resume: true, runId: runId(3) }), {
  until: isComplete(SID),
  answer: (f) => (f.kind === 'permission_request' ? { type: 'permission-response', requestId: f.requestId, sessionId: SID, allow: true, provider: 'pilotdeck' } : null),
});
// 4. 中止一轮：provider 挂住，看到第一段输出后发 abort-session
await wsStep('turn-abort', cmd('UI_SMOKE_SLOW answer', { sessionId: SID, resume: true, runId: runId(4) }), {
  until: (f) => f.kind === 'complete' && f.aborted === true,
  answer: (() => { let sent = false; return (f) => (!sent && f.sessionId === SID && (f.kind === 'stream_delta' || f.kind === 'text') && f.role !== 'user' ? (sent = true, { type: 'abort-session', sessionId: SID, provider: 'pilotdeck' }) : null); })(),
  settle: 1500,
});
// 5. 权限询问：界面侧拒绝
await wsStep('turn-permission-deny', cmd('UI_SMOKE_DENY a file', { sessionId: SID, resume: true, runId: runId(11) }), {
  until: isComplete(SID),
  answer: (f) => (f.kind === 'permission_request' ? { type: 'permission-response', requestId: f.requestId, sessionId: SID, allow: false, message: 'no thanks', provider: 'pilotdeck' } : null),
});
// 6. 提问（ask_user_question → elicitation），界面侧回答
await wsStep('turn-ask-user', cmd('UI_SMOKE_ASK me', { sessionId: SID, resume: true, runId: runId(12) }), {
  until: isComplete(SID),
  answer: (f) => (f.kind === 'permission_request' && f.isElicitation
    ? { type: 'elicitation-response', requestId: f.requestId, sessionId: SID, provider: 'pilotdeck', answer: { type: 'answered', answers: { 'Which color?': 'Blue' } } } : null),
});
// 7. 输入队列：回合进行中排一条、删掉，再中止
await wsStep('turn-queue-then-abort', cmd('UI_SMOKE_SLOW queue test', { sessionId: SID, resume: true, runId: runId(13) }), {
  until: (f) => f.kind === 'complete' && f.aborted === true,
  answer: (() => {
    let phase = 0;
    return (f) => {
      if (phase === 0 && f.sessionId === SID && (f.kind === 'stream_delta' || f.kind === 'text') && f.role !== 'user') {
        phase = 1;
        return { type: 'queue-input', sessionId: SID, requestId: 'queue-1', provider: 'pilotdeck', item: { id: 'queued-1', runId: runId(14), command: 'queued message', displayText: 'queued message', createdAt: '2026-01-01T00:00:00.000Z', options: { projectPath: PROJECT, cwd: PROJECT, permissionMode: 'default' } } };
      }
      if (phase === 1 && f.type === 'input-queue-operation-result' && f.operation === 'enqueue') { phase = 2; return { type: 'delete-queued-input', sessionId: SID, requestId: 'queue-2', itemId: 'queued-1' }; }
      if (phase === 2 && f.type === 'input-queue-operation-result' && f.operation === 'delete') { phase = 3; return { type: 'abort-session', sessionId: SID, provider: 'pilotdeck' }; }
      return null;
    };
  })(),
  settle: 1500,
});
await wsStep('resume-input-queue', { type: 'resume-input-queue', sessionId: SID, requestId: 'queue-3', provider: 'pilotdeck' }, { until: (f) => f.type === 'input-queue-operation-result', timeout: 10000, settle: 500 });
// 8. 计划模式（plan）下的一轮
await wsStep('turn-plan-mode', cmd('plan something', { sessionId: SID, resume: true, runId: runId(15), permissionMode: 'plan' }), { until: isComplete(SID) });
// 9. 重新生成最后一条回复
{
  const h = await http('session-messages-before-regenerate', 'GET', `/api/sessions/${SID}/messages?projectPath=${encodeURIComponent(PROJECT)}`, { route: 'GET /api/sessions/:sessionId/messages' });
  const turnId = [...(h.body?.messages ?? [])].reverse().find((m) => m.turnId)?.turnId ?? '';
  await wsStep('regenerate-last-message', { type: 'regenerate-last-message', requestId: 'regen-1', sessionId: SID, expectedTurnId: turnId, command: 'plan something again',
    options: { sessionId: SID, resume: true, projectPath: PROJECT, cwd: PROJECT, runId: runId(16), permissionMode: 'default', toolsSettings: { allowedTools: [], disallowedTools: [], skipPermissions: false } } },
  { until: isComplete(SID), timeout: 30000 });
}
await wsStep('check-session-status', { type: 'check-session-status', sessionId: SID, provider: 'pilotdeck', statusRequestId: 1 }, { until: (f) => f.type === 'session-status', timeout: 10000, settle: 300 });
await wsStep('watch-session', { type: 'watch-session', sessionId: SID }, { settle: 800 });
await wsStep('get-input-queue', { type: 'get-input-queue', sessionId: SID }, { until: (f) => f.type === 'session-input-queue' || f.type?.includes?.('queue'), timeout: 5000, settle: 300 });
await wsStep('get-pending-permissions', { type: 'get-pending-permissions', sessionId: SID }, { until: (f) => f.type === 'pending-permissions-response', timeout: 5000, settle: 200 });
await wsStep('session-permission-grant', { type: 'session-permission-grant', sessionId: SID, requestId: 'grant-1', entry: 'write_file' }, { until: (f) => f.type === 'session-permission-grant-result', timeout: 10000, settle: 200 });

// 改名、列会话、读历史
await http('session-rename', 'PUT', `/api/sessions/${SID}/rename`, { body: { summary: 'Renamed by ui-smoke', provider: 'pilotdeck' }, route: 'PUT /api/sessions/:sessionId/rename' });
await http('sessions-after-turns', 'GET', `/api/projects/${PN}/sessions?limit=5&offset=0`, { route: 'GET /api/projects/:projectName/sessions' });
const hist = await http('session-messages', 'GET', `/api/sessions/${SID}/messages?projectPath=${encodeURIComponent(PROJECT)}`, { route: 'GET /api/sessions/:sessionId/messages' });
const forkFrom = (hist.body?.messages ?? []).find((m) => m.entryId && m.role === 'assistant')?.entryId ?? (hist.body?.messages ?? []).find((m) => m.entryId)?.entryId;
await http('session-fork', 'POST', `/api/sessions/${SID}/fork`, { body: { projectPath: PROJECT, fromEntryId: forkFrom ?? 'missing-entry' }, route: 'POST /api/sessions/:sessionId/fork' });
await http('session-messages-page', 'GET', `/api/sessions/${SID}/messages?projectPath=${encodeURIComponent(PROJECT)}&limit=2&offset=1`, { route: 'GET /api/sessions/:sessionId/messages' });
await http('session-token-usage', 'GET', `/api/projects/${PN}/sessions/${SID}/token-usage`, { route: 'GET /api/projects/:projectName/sessions/:sessionId/token-usage' });

// 切换模型
const q = `projectKey=${encodeURIComponent(PROJECT)}&sessionKey=${SID}`;
await http('models', 'GET', '/api/models', { route: 'GET /api/models' });
await http('session-model-get', 'GET', `/api/sessions/model?${q}`, { route: 'GET /api/sessions/model' });
await http('session-model-set', 'PUT', '/api/sessions/model', { body: { projectKey: PROJECT, sessionKey: SID, selection: { mode: 'model', provider: 'smoke', model: 'alternate' } }, route: 'PUT /api/sessions/model' });
await http('session-model-get-after-set', 'GET', `/api/sessions/model?${q}`, { route: 'GET /api/sessions/model' });
await wsStep('turn-after-model-switch', cmd('after model switch', { sessionId: SID, resume: true, runId: runId(5), modelSelection: { mode: 'model', provider: 'smoke', model: 'alternate' } }), { until: isComplete(SID) });
await http('session-model-clear', 'DELETE', `/api/sessions/model?${q}`, { route: 'DELETE /api/sessions/model' });

// 设置与 skills
const cfg = await http('config', 'GET', '/api/config', { route: 'GET /api/config' });
// 设置页保存（原样 YAML 加一行注释）：ui/server 写盘后让 gateway 重载配置。
await http('config-save-raw', 'PUT', '/api/config', { body: { raw: `${cfg.body?.raw ?? ''}# saved by ui-smoke\n`, baseRevision: cfg.body?.revision }, route: 'PUT /api/config' });
await http('settings-permissions', 'GET', '/api/settings/permissions', { route: 'GET /api/settings/permissions' });
await http('skills-list', 'POST', '/api/skills/list', { body: { projectPath: PROJECT }, route: 'POST /api/skills/list' });
await http('commands-list', 'POST', '/api/commands/list', { body: { projectPath: PROJECT }, route: 'POST /api/commands/list' });

// 附件：上传任务 → 内容 → 在消息里引用
const att = 'attachment body from ui-smoke\n';
await http('upload-limits', 'GET', '/api/uploads/limits', { route: 'GET /api/uploads/limits' });
const up = await http('upload-create', 'POST', '/api/uploads', { body: { projectKey: PROJECT, files: [{ clientFileId: 'f1', name: 'att.txt', relativePath: 'att.txt', size: Buffer.byteLength(att) }] }, route: 'POST /api/uploads' });
const UID = up.body?.uploadId ?? 'missing-upload';
const fd = new FormData(); fd.append('files[f1]', new Blob([att], { type: 'text/plain' }), 'att.txt');
const upc = await http('upload-content', 'POST', `/api/uploads/${UID}/content`, { form: fd, route: 'POST /api/uploads/:uploadId/content' });
await http('upload-get', 'GET', `/api/uploads/${UID}`, { route: 'GET /api/uploads/:uploadId' });
await http('upload-events', 'GET', `/api/uploads/${UID}/events`, { sse: true, route: 'GET /api/uploads/:uploadId/events' });
const attIds = (upc.body?.attachments ?? []).map((a) => a.attachmentId);
await wsStep('turn-with-attachment', cmd('please look at the attachment', { sessionId: SID, resume: true, runId: runId(6), uploadedAttachments: [{ uploadId: UID, attachmentIds: attIds }] }), { until: isComplete(SID) });
await http('upload-preview-not-image', 'GET', `/api/uploads/${UID}/attachments/${attIds[0] ?? 'x'}/preview`, { route: 'GET /api/uploads/:uploadId/attachments/:attachmentId/preview' });
const legacy = new FormData(); legacy.append('attachments', new Blob(['legacy attachment\n'], { type: 'text/plain' }), 'legacy.txt');
await http('upload-attachments-legacy', 'POST', `/api/projects/${PN}/upload-attachments`, { form: legacy, route: 'POST /api/projects/:projectName/upload-attachments' });

// 第二个会话：在 General（不带项目）里开，并删除它
const g = await wsStep('turn-general', { type: 'pilotdeck-command', command: 'hello general', options: { permissionMode: 'default', runId: runId(7) } }, { until: isComplete() });
const GSID = g.frames.find((f) => f.kind === 'session_created')?.newSessionId;

// 删除会话
await http('session-delete', 'DELETE', `/api/projects/${PN}/sessions/${SID}`, { route: 'DELETE /api/projects/:projectName/sessions/:sessionId' });
await http('sessions-after-delete', 'GET', `/api/projects/${PN}/sessions`, { route: 'GET /api/projects/:projectName/sessions' });
await http('projects-final', 'GET', '/api/projects', { route: 'GET /api/projects' });

// ── 其余路由各打一次 ──
const ctx = { PN, SID: GSID ?? SID, UID, PROJECT, PILOT_HOME, PROJECT_Q: encodeURIComponent(PROJECT) };
for (const r of ROUTE_SWEEP) {
  if (r.skip) continue;
  if (hit.has(`${r.method} ${r.route}`) && !r.again) continue;
  const fill = (s) => s.replace(/\{(\w+)\}/g, (_, k) => ctx[k] ?? `{${k}}`);
  const body = r.body === undefined ? undefined : JSON.parse(fill(JSON.stringify(r.body)));
  await http(`sweep ${r.method} ${r.route}`, r.method, fill(r.url), { body, route: `${r.method} ${r.route}`, sse: r.sse, timeout: r.timeout ?? 20000 });
}
if (ws?.readyState === 1) {
  // 扫完路由后，收集期间 WS 上推来的消息（项目更新等广播），然后断开。
  await wsStep('ws-trailing', null, { settle: 500 });
  ws.close();
}
save();
console.log(`[drive] ${steps.length} steps, ${hit.size} routes hit`);
