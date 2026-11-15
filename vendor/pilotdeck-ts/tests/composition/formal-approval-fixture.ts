import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import { randomBytes } from "node:crypto";
import fs from "node:fs";
import { mkdir, readFile } from "node:fs/promises";
import { createServer, type Server } from "node:http";
import { syncBuiltinESMExports } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";
import { parse } from "yaml";

import type { createLocalGateway } from "../../src/cli/createLocalGateway.js";
import { handlePublicHostHttpRequest } from "../../src/composition/publicHostHttpTransport.js";
import { readNativeSessionAdmission } from "../../src/composition/nativeApprovalSessionResolver.js";
import type { CanonicalModelRequest, ModelRuntime } from "../../src/model/index.js";
import { DEFAULT_MODEL_CAPABILITIES } from "../../src/model/protocol/capabilities.js";
import type { StaffDeckSopStatusSnapshot } from "../../src/sop/staffdeck/types.js";

type LocalGateway = ReturnType<typeof createLocalGateway>;
const testRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const pdRoot = fs.existsSync(join(testRoot, "package.json")) ? testRoot : dirname(testRoot);
const placeholder = "${STAFFDECK_SOP_MANAGEMENT_API_KEY}";
const profileKeys = new Map<string, string>();
const originalRead = fs.readFileSync;

// Launcher expansion is limited to registered test profiles, never an auth/resolver override.
fs.readFileSync = ((path: Parameters<typeof originalRead>[0], options: unknown) => {
  const value = originalRead(path, options as never) as string | Buffer;
  const key = typeof path === "string" ? profileKeys.get(resolve(path)) : undefined;
  if (!key) return value;
  return typeof value === "string" ? value.replaceAll(placeholder, key)
    : Buffer.from(value.toString().replaceAll(placeholder, key));
}) as typeof originalRead;
syncBuiltinESMExports();

async function listen(server: Server): Promise<number> {
  const range = process.env.PILOTDECK_APPROVAL_PORT_RANGE ?? "24630-24649";
  const match = /^(\d+)-(\d+)$/.exec(range);
  if (!match) throw new Error("Invalid PILOTDECK_APPROVAL_PORT_RANGE");
  for (let port = Number(match[1]); port <= Number(match[2]); port++) {
    try {
      await new Promise<void>((accept, reject) => {
        server.once("error", reject);
        server.listen(port, "127.0.0.1", () => { server.removeListener("error", reject); accept(); });
      });
      return port;
    } catch (error) { if ((error as NodeJS.ErrnoException).code !== "EADDRINUSE") throw error; }
  }
  throw new Error(`No free formal approval port in ${range}`);
}

async function stop(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise<void>((accept) => child.once("exit", () => accept()));
  child.kill("SIGTERM");
  await Promise.race([exited, delay(5000)]);
  if (child.exitCode === null && child.signalCode === null) { child.kill("SIGKILL"); await exited; }
}

async function close(server: Server): Promise<void> {
  server.closeAllConnections();
  if (server.listening) await new Promise<void>((accept, reject) => server.close(error => error ? reject(error) : accept()));
}

async function ready(origin: string, path: string, child: ChildProcess, logs: string[], secrets: string[]): Promise<void> {
  for (let attempt = 0; attempt < 240; attempt++) {
    if (child.exitCode !== null || child.signalCode !== null) {
      let detail = logs.join("").slice(-6000);
      for (const secret of secrets) detail = detail.replaceAll(secret, "<redacted>");
      throw new Error(`Formal bootstrap exited ${child.exitCode ?? child.signalCode}: ${detail}`);
    }
    try { if ((await fetch(origin + path, { signal: AbortSignal.timeout(1000) })).ok) return; } catch { /* starting */ }
    await delay(250);
  }
  throw new Error(`Formal bootstrap health timeout (${path})`);
}

export async function createFormalApprovalFixture(input: { root: string; projectRoot: string; staffDeckRoot?: string; python?: string }) {
  const sdRoot = input.staffDeckRoot ?? process.env.STAFFDECK_SOP_ROOT;
  if (!sdRoot) throw new Error("Formal approval runner requires STAFFDECK_SOP_ROOT (locked StaffDeck checkout)");
  const python = input.python ?? process.env.STAFFDECK_PYTHON ?? join(sdRoot, "backend/.venv/bin/python");
  const home = join(input.root, "formal-approval");
  await mkdir(home, { recursive: true });
  const secrets: string[] = [];
  const secret = () => { const value = randomBytes(32).toString("hex"); secrets.push(value); return value; };
  const bridgeToken = secret();
  const children: ChildProcess[] = [];
  let local: LocalGateway | undefined;
  const host = createServer(async (req, res) => {
    if (!await handlePublicHostHttpRequest(req, res, { token: bridgeToken, resolve: () => {
      if (!local) throw new Error("Formal host gateway not attached");
      return local.getPublicHostCapabilities();
    } })) { res.writeHead(404); res.end(); }
  });
  const reservations: Server[] = [];
  const reserve = async () => {
    const server = createServer(); reservations.push(server);
    return listen(server);
  };
  const profilePath = join(input.projectRoot, "pilotdeck.yaml");
  const cleanup = async () => {
    profileKeys.delete(resolve(profilePath));
    for (const child of children.reverse()) await stop(child);
    for (const server of reservations) await close(server);
    await close(host);
  };
  try {
    const hostPort = await listen(host);
    const pdPort = await reserve();
    const pdReservation = reservations.at(-1)!;
    const sdPort = await reserve();
    const sdReservation = reservations.at(-1)!;
    const databasePath = join(home, "pd-auth.sqlite");
    const spawnService = (executable: string, args: string[], cwd: string, env: NodeJS.ProcessEnv) => {
      const logs: string[] = [];
      const childEnv = { ...process.env, ...env }; delete childEnv.NODE_OPTIONS;
      const child = spawn(executable, args, { cwd, env: childEnv, stdio: ["ignore", "pipe", "pipe"] });
      child.on("error", error => logs.push(error.message));
      child.stdout?.on("data", data => logs.push(String(data)));
      child.stderr?.on("data", data => logs.push(String(data)));
      children.push(child);
      return { child, logs };
    };
    await close(pdReservation);
    const pd = spawnService(process.execPath, ["--import", "tsx", "--input-type=module", "-e", `
      const {default: express} = await import('./ui/node_modules/express/index.js');
      const {initializeDatabase} = await import('./ui/server/database/db.js');
      await initializeDatabase();
      const {default: auth} = await import('./ui/server/routes/auth.js');
      const app = express(); app.use(express.json()); app.use('/api/auth', auth);
      app.listen(Number(process.env.FORMAL_PD_PORT), '127.0.0.1');
    `], pdRoot, { DATABASE_PATH: databasePath, PILOT_HOME: home, PILOTDECK_CONFIG_PATH: join(home, "installation.yaml"), JWT_SECRET: secret(), PILOTDECK_DISABLE_LOCAL_AUTH: "0", FORMAL_PD_PORT: String(pdPort) });
    const pdOrigin = `http://127.0.0.1:${pdPort}`;
    await ready(pdOrigin, "/api/auth/status", pd.child, pd.logs, secrets);
    const call = async (origin: string, method: string, path: string, body?: unknown, token?: string): Promise<any> => {
      const response = await fetch(origin + path, {
        method, headers: { "content-type": "application/json", ...(token ? { authorization: `Bearer ${token}` } : {}) },
        ...(body !== undefined ? { body: JSON.stringify(body) } : {}), signal: AbortSignal.timeout(60000),
      });
      if (!response.ok) {
        const error = await response.json().catch(() => ({})) as { code?: string; detail?: string; error?: { code?: string } };
        let detail = typeof error.detail === "string" ? error.detail : "";
        for (const secret of secrets) detail = detail.replaceAll(secret, "<redacted>");
        throw new Error(`Formal bootstrap ${method} ${path.split("?")[0]} HTTP ${response.status} ${error.code ?? error.error?.code ?? ""} ${detail}`);
      }
      return response.json();
    };
    const owner = await call(pdOrigin, "POST", "/api/auth/register", { username: `owner_${randomBytes(6).toString("hex")}`, password: secret() });
    secrets.push(owner.token);
    owner.user.id = String(owner.user.id);
    assert.equal(typeof owner.user.id, "string");
    await close(sdReservation);
    const harnessRoot = process.env.STAFFDECK_HARNESS_ROOT;
    if (!harnessRoot) throw new Error("Formal approval runner requires STAFFDECK_HARNESS_ROOT (installed locked runtime)");
    const sdArgs = ["-c", `
import os, uvicorn
from app.public_api.pilotdeck_domain_host import PilotDeckDomainHostClient, bind_pilotdeck_domain_host
bind_pilotdeck_domain_host(PilotDeckDomainHostClient(os.environ['FORMAL_HOST_ORIGIN'], os.environ['FORMAL_BRIDGE_TOKEN'], os.environ['FORMAL_PD_OWNER']))
uvicorn.run('app.main:app', host='127.0.0.1', port=int(os.environ['FORMAL_SD_PORT']), log_level='warning')
`];
    const sdEnv = {
      PYTHONPATH: [join(sdRoot, "backend"), join(sdRoot, "backend/src"), join(sdRoot, "portable_sop/src")].join(":"),
      DATABASE_URL: `sqlite:///${join(home, "sd.sqlite")}`, APP_SECRET: secret(), DEMO_SEED_ENABLED: "true",
      PUBLIC_API_ENABLED: "true", STARTUP_ORPHAN_CLEANUP_ENABLED: "false",
      HARNESS_V3_ROOT: harnessRoot, HARNESS_V3_HOME: join(home, "harness"), HARNESS_V3_NODE_BIN: process.execPath,
      ULTRARAG_DATA_DIR: join(home, "knowledge-data"), FORMAL_SD_PORT: String(sdPort),
      FORMAL_HOST_ORIGIN: `http://127.0.0.1:${hostPort}`, FORMAL_BRIDGE_TOKEN: bridgeToken, FORMAL_PD_OWNER: owner.user.id,
    };
    let sd = spawnService(python, sdArgs, join(sdRoot, "backend"), sdEnv);
    const origin = `http://127.0.0.1:${sdPort}`;
    await ready(origin, "/api/health", sd.child, sd.logs, secrets);
    const admin = await call(origin, "POST", "/api/auth/login", { tenant_id: "tenant_demo", username: "admin", password: "admin" });
    secrets.push(admin.token);
    const identities: Record<string, { id: string; token: string }> = {};
    for (const [kind, role] of [["actor", "admin"], ["assignee", "member"], ["other", "member"]]) {
      const username = `composition_${kind}_${randomBytes(6).toString("hex")}`;
      const password = secret();
      const user = await call(origin, "POST", "/api/auth/users", { tenant_id: "tenant_demo", username, password, role }, admin.token);
      const login = await call(origin, "POST", "/api/auth/login", { tenant_id: "tenant_demo", username, password });
      secrets.push(login.token);
      identities[kind] = { id: user.id, token: login.token };
    }
    const actor = identities.actor!; const assignee = identities.assignee!;
    const target = await call(origin, "POST", "/api/enterprise/agents", { tenant_id: "tenant_demo", name: "Composition approval target", source_mode: "blank", is_overall: false }, actor.token);
    const credential = await call(origin, "POST", "/api/auth/me/api-credentials", { name: "composition-owned" }, actor.token);
    secrets.push(credential.api_key);
    profileKeys.set(resolve(profilePath), credential.api_key);
    const env: NodeJS.ProcessEnv = {
      ...process.env, DATABASE_PATH: databasePath, PILOT_HOME: input.projectRoot, PILOTDECK_CONFIG_PATH: profilePath, STAFFDECK_FORMAL_API_ORIGIN: origin,
      STAFFDECK_COPY_TENANT_ID: "tenant_demo", STAFFDECK_COPY_TARGET_AGENT_ID: target.id,
      STAFFDECK_COPY_ACTOR_USER_ID: actor.id, STAFFDECK_COPY_PILOTDECK_USER_ID: owner.user.id,
      STAFFDECK_SOP_MANAGEMENT_CREDENTIAL_ID: credential.id, STAFFDECK_COPY_USER_TOKEN: actor.token,
      STAFFDECK_APPROVAL_USER_ID: assignee.id,
    };
    delete env.NODE_OPTIONS;
    const routingRequests: CanonicalModelRequest[] = [];
    return {
      env, assigneeUserId: assignee.id, approver: { approverAuthorization: `Bearer ${assignee.token}` }, routingRequests,
      discoveryYaml: `    discoveryEndpoint: ${origin}/api/v1\n    discoveryAgentId: ${target.id}\n    discoveryApiKey: '${placeholder}'\n`,
      attach(gateway: LocalGateway) { local = gateway; },
      async restartStaffDeck() {
        await stop(sd.child);
        sd = spawnService(python, sdArgs, join(sdRoot, "backend"), sdEnv);
        await ready(origin, "/api/health", sd.child, sd.logs, secrets);
      },
      async checkDiscovery() {
        const result = await call(origin, "POST", `/api/v1/agents/${target.id}/sops:route`, { message: "Request approval", model_source: "pilotdeck_host" }, credential.api_key);
        assert.equal(result.selected_sop_id, "approval");
        assert.ok(result.candidate_sop_ids.includes("approval"));
      },
      async publishDefinition() {
        const bundle = parse(await readFile(join(input.projectRoot, "approval.yaml"), "utf8"));
        const definition = bundle.sops[0];
        const content = { ...definition.content, nodes: definition.content.nodes.map((node: Record<string, unknown>) => ({ ...node, name: node.name ?? node.node_id, assignee_user_id: assignee.id })) };
        const card = { ...content, skill_id: definition.id, name: definition.name, version: definition.version ?? "1", trigger_intents: ["approval"], user_utterance_examples: ["Request approval"], terminal_node_ids: content.terminal_node_ids ?? [content.start_node_id] };
        const draft = await call(origin, "POST", `/api/v1/agents/${target.id}/sops`, { content: card }, credential.api_key);
        const valid = await call(origin, "POST", `/api/v1/sops/${definition.id}:validate?agent_id=${target.id}&draft_id=${draft.id}`, undefined, credential.api_key);
        assert.equal(valid.valid, true, "Formal SOP definition must validate before publication");
        await call(origin, "POST", `/api/v1/sops/${definition.id}:publish?agent_id=${target.id}`, { draft_id: draft.id }, credential.api_key);
      },
      // Only deterministic planner output is supplied; API auth, discovery and host transport remain production paths.
      routingModel(delegate?: ModelRuntime, realRouting = false): ModelRuntime {
        if (realRouting) {
          if (!delegate) throw new Error("Real routing requires the configured real ModelRuntime");
          return delegate;
        }
        return {
          async *stream(request, options) {
            const isPlanner = request.messages.some(message => message.content.some(block => block.type === "text"
              && /当前阶段：\s*TurnPlanner/.test(block.text)));
            if (!isPlanner) {
              if (!delegate) throw new Error("Non-routing request must use the selected external model Port");
              yield* delegate.stream(request, options); return;
            }
            routingRequests.push(request);
            yield { type: "request_started", provider: request.provider, model: request.model };
            yield { type: "message_start", role: "assistant" };
            yield { type: "text_delta", text: JSON.stringify({ decision: "start_new_task", confidence: 1, task_frames: [{ kind: "sop", decision: "start_new_task", target_skill_id: "approval" }] }) };
            yield { type: "message_end", finishReason: "stop" };
          },
          complete: (request, options) => { if (!delegate) throw new Error("Unexpected routing complete"); return delegate.complete(request, options); },
          getCapabilities: (provider, model) => delegate?.getCapabilities(provider, model)
            ?? { ...DEFAULT_MODEL_CAPABILITIES, supportsToolUse: true, maxContextTokens: 65_536, maxOutputTokens: 8192 },
          getMultimodal: (provider, model) => delegate?.getMultimodal(provider, model) ?? { input: ["text"] },
          getProviderProtocol: provider => delegate?.getProviderProtocol(provider) ?? "openai",
          getProviderBaseUrl: provider => delegate?.getProviderBaseUrl(provider),
        };
      },
      async assertOtherSubjectDenied(gateway: LocalGateway, sessionKey: string, waiting: StaffDeckSopStatusSnapshot) {
        const storage = gateway.registry.createPersistentSessionStorage(input.projectRoot, sessionKey);
        try {
          const { entries, diagnostics } = await storage.persistence.load();
          assert.equal(diagnostics.length, 0);
          assert.deepEqual(readNativeSessionAdmission(entries, sessionKey), {
            tenantId: "tenant_demo", agentId: target.id, pilotDeckUserId: owner.user.id, projectKey: input.projectRoot,
            actorUserId: actor.id, credentialId: credential.id, staffDeckOrigin: origin,
          }, "Normal turn must persist authenticated host admission in its original transcript");
        } finally { await storage.dispose(); }
        assert.equal(waiting.approval?.assigneeUserId, assignee.id, "Original pinned definition must name the formal assignee");
        const path = join(input.projectRoot, "sop/sessions", `${Buffer.from(sessionKey).toString("base64url")}.json`);
        const before = await readFile(path, "utf8");
        await assert.rejects(() => gateway.gateway.resumeSop!({ sessionKey, projectKey: input.projectRoot,
          approverAuthorization: `Bearer ${identities.other!.token}`, source: "human", requestId: "other-subject-rejected",
          waitId: waiting.wait!.id, message: "Other subject must not approve.", expectedRevision: waiting.revision,
        }), { code: "APPROVAL_SUBJECT_MISMATCH" });
        assert.equal(await readFile(path, "utf8"), before, "Subject rejection must not advance wait/receipt");
        console.log(`[formal-approval] ${sessionKey}: original admission, pinned assignee, other-subject rejection/readback PASS`);
      },
      async assertRevokedAdmissionDenied(gateway: LocalGateway, sessionKey: string, waiting: StaffDeckSopStatusSnapshot) {
        const path = join(input.projectRoot, "sop/sessions", `${Buffer.from(sessionKey).toString("base64url")}.json`);
        const before = await readFile(path, "utf8");
        await call(origin, "POST", `/api/auth/me/api-credentials/${credential.id}/revoke`, undefined, actor.token);
        await assert.rejects(() => gateway.gateway.sopStatus!({ sessionKey, projectKey: input.projectRoot, approverAuthorization: `Bearer ${assignee.token}` }), { code: "SOP_APPROVAL_SESSION_FORBIDDEN" });
        await assert.rejects(() => gateway.gateway.resumeSop!({ sessionKey, projectKey: input.projectRoot,
          approverAuthorization: `Bearer ${assignee.token}`, source: "human", requestId: "revoked-admission-rejected",
          waitId: waiting.wait!.id, message: "Revoked admission must not approve.", expectedRevision: waiting.revision,
        }), { code: "SOP_APPROVAL_SESSION_FORBIDDEN" });
        assert.equal(await readFile(path, "utf8"), before, "Revoked original admission must not advance wait/receipt");
        console.log(`[formal-approval] ${sessionKey}: revoked owned credential rejects status/resume; original wait/receipt unchanged PASS`);
      },
      close: cleanup,
    };
  } catch (error) { await cleanup(); throw error; }
}

export type FormalApprovalFixture = Awaited<ReturnType<typeof createFormalApprovalFixture>>;
