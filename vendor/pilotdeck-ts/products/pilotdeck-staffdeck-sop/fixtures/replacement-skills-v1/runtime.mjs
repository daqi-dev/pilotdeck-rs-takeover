import { readFile, appendFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(fileURLToPath(import.meta.url));
const manifest = JSON.parse(await readFile(join(root, 'manifest.json'), 'utf8'));
const content = await readFile(join(root, 'skills/replacement-guide/SKILL.md'), 'utf8');
const skill = { slug: 'replacement-guide', name: 'Replacement guide', description: 'Read-only Skills replacement acceptance fixture.',
  version: '1.0.0', skillFile: join(root, 'skills/replacement-guide/SKILL.md'), skillDir: join(root, 'skills/replacement-guide'),
  scope: 'builtin', readonly: true, mtime: 0, command: '/replacement-guide' };
const send = (res, status, body) => { res.writeHead(status, { 'content-type': 'application/json' }); res.end(JSON.stringify(body)); };
const server = createServer(async (req, res) => {
  if (req.method === 'GET' && req.url === '/module-manifest') return send(res, 200, manifest);
  if (req.method !== 'POST' || req.url !== '/v2/module/call') return send(res, 404, { code: 'NOT_FOUND' });
  const chunks = []; for await (const chunk of req) chunks.push(chunk);
  let call;
  try { call = JSON.parse(Buffer.concat(chunks).toString()); }
  catch { return send(res, 400, { code: 'MODULE_INPUT_INVALID' }); }
  const response = { kind: 'response', messageId: `reply-${call.messageId}`, inReplyTo: call.messageId, requestId: call.requestId };
  const fail = (status, code, message) => send(res, status, { ...response, ok: false, code, error: { code, message, retryability: 'safe' } });
  if (call.kind !== 'request' || call.method !== 'module_call' || call.module !== 'skills') return fail(400, 'MODULE_PROTOCOL_INCOMPATIBLE', 'Only the Skills module is supported.');
  const { operation, input = {} } = call.payload ?? {};
  if (process.env.REPLACEMENT_SKILLS_EVIDENCE_PATH) await appendFile(process.env.REPLACEMENT_SKILLS_EVIDENCE_PATH,
    JSON.stringify({ fixtureVersion: manifest.fixtureVersion, module: call.module, operation, input }) + '\n');
  if (!manifest.methods.includes(operation)) return fail(409, 'MODULE_CAPABILITY_UNAVAILABLE', 'This read-only fixture does not declare that operation.');
  if (operation === 'read') {
    const slug = input.slug ?? input.name;
    if (slug !== skill.slug || (input.scope !== undefined && input.scope !== 'builtin')) return fail(404, 'SKILL_NOT_FOUND', 'The requested fixture skill does not exist.');
    return send(res, 200, { ...response, ok: true, payload: { result: { scope: 'builtin', slug, skill, content } } });
  }
  const selected = (!input.scope || ['all', 'builtin'].includes(input.scope)) && (!input.query || `${skill.name} ${skill.description}`.toLowerCase().includes(String(input.query).toLowerCase()));
  return send(res, 200, { ...response, ok: true, payload: { result: { builtin: selected ? [skill] : [], user: [], project: [],
    items: selected ? [skill] : [], projectPath: input.projectKey ?? null } } });
});
server.listen(Number(process.env.REPLACEMENT_SKILLS_PORT ?? 19643), '127.0.0.1', () => {
  console.log(JSON.stringify({ ready: true, port: server.address().port, implementationId: manifest.implementationId }));
});
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => { server.close(() => process.exit(0)); server.closeAllConnections(); });
