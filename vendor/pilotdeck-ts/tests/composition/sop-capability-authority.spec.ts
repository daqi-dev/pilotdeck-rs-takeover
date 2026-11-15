import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createServer } from "node:http";
import test from "node:test";
import { createNativeSopAuthorityReader } from "../../src/composition/nativeSopAuthority.js";
import { createSopCapabilityAuthorityPort, currentKnowledgeAuthority, validateSopAuthorityProjection,
  withKnowledgeAuthority, type SopAuthorityContext, type SopAuthorityProjection } from "../../src/composition/sopCapabilityAuthority.js";
import { createKnowledgeModulePort } from "../../src/composition/domainPorts.js";
import { SopStateStore } from "../../src/sop/staffdeck/SopStateStore.js";
import { createRuntimeHostCapabilityProvider } from "../../src/composition/publicHostRuntimeAdapter.js";
import type { ExternalModuleBinding } from "../../src/composition/types.js";

const bundle = { sops: [{ id: "sop", version: "1", content: { nodes: [{ node_id: "response", type: "response" }] } }] };
const context: SopAuthorityContext = { sessionKey: "session", projectKey: "/project", expectedRevision: 2, requestId: "request" };
const principal = { pilotDeckUserId: "owner", tenantId: "tenant", actorUserId: "actor", agentId: "agent" };
const admission = { ...principal, projectKey: "/project", credentialId: "credential", staffDeckOrigin: "http://formal" };
const projection = (input = context): SopAuthorityProjection => ({ context: input, sopId: "sop", sopVersion: "1", nodeId: "response",
  snapshotId: "snapshot", registryGeneration: 3, optionalCapabilities: [{ operation: "knowledge.search/v1", resourceType: "knowledge_base",
    resourceId: "base", required: false, providerModuleId: "knowledge.local", providerVersion: "1", selectionMode: "current" }] });

test("owner callback maps only stale and inactive state refusals, preserving ordinary errors and state", async () => {
  const root = await mkdtemp(join(tmpdir(), "r160-owner-status-"));
  console.log(`owner-status-state-evidence=${root}`);
  const store = new SopStateStore(root);
  const reader = createNativeSopAuthorityReader({ readOwner: () => "owner", listProjects: async () => ["/project"],
    listSessions: async () => [{ sessionId: "session" }], sessionAdmission: async () => admission,
    readState: input => store.authority(input.sessionKey, input.expectedRevision) });
  const provider = createRuntimeHostCapabilityProvider({ profile: { id: "test" }, model: {}, tools: {}, skills: {},
    context: { forTool: () => ({}) }, sopAuthority: { read: reader },
    file: { parse: () => { throw Object.assign(new Error("ordinary"), { code: "SOP_REVISION_CONFLICT" }); } } });
  const call = (revision: number) => provider.call("read_sop_authority", { ...context, expectedRevision: revision,
    admissionCredentialId: "credential" }, { principal });
  assert.equal((await call(1)).status, 502);
  assert.equal(((await call(1)).body as { code: string }).code, "SOP_AUTHORITY_STATE_MISSING");
  assert.equal(await store.status("session"), undefined);
  const initial = await store.loadOrCreate("session", bundle, "sop");
  const saved = await store.replace("session", bundle, { status: "active", active_skill_id: "sop", active_step_id: "response" });
  assert.equal((await call(saved.revision)).status, 200);
  const stale = await call(initial.revision);
  assert.equal(stale.status, 409); assert.equal((stale.body as { code: string }).code, "SOP_REVISION_CONFLICT");
  assert.equal((await store.status("session"))?.revision, saved.revision);
  const inactive = await store.replace("session", bundle, { status: "completed", active_skill_id: "sop", active_step_id: "response" });
  const refused = await call(inactive.revision);
  assert.equal(refused.status, 403); assert.equal((refused.body as { code: string }).code, "SOP_AUTHORITY_STATE_INACTIVE");
  const invalid = await store.replace("session", bundle, { status: "active", active_skill_id: "wrong", active_step_id: "response" });
  assert.equal((await call(invalid.revision)).status, 502);
  assert.equal(((await call(invalid.revision)).body as { code: string }).code, "SOP_AUTHORITY_PIN_INVALID");
  assert.equal((await provider.call("file_parse", { filename: "x", content_base64: "eA==" }, { principal })).status, 502);
});

test("owner attestation rejects absent/foreign/ambiguous admission without reading or creating state", async () => {
  let reads = 0;
  const options = { readOwner: () => "owner", listProjects: async () => ["/project"],
    listSessions: async () => [{ sessionId: "session" }], sessionAdmission: async () => admission,
    readState: async () => { reads++; return { sopId: "sop", sopVersion: "1", nodeId: "response", content: {} }; } };
  const input = { ...context, admissionCredentialId: "credential" };
  const valid = await createNativeSopAuthorityReader(options)(input, principal);
  assert.equal(valid.admission.credentialId, "credential");
  for (const field of ["pilotDeckUserId", "tenantId", "actorUserId", "agentId"] as const) {
    await assert.rejects(createNativeSopAuthorityReader(options)(input, { ...principal, [field]: "foreign" }));
  }
  await assert.rejects(createNativeSopAuthorityReader(options)({ ...input, admissionCredentialId: "foreign" }, principal));
  await assert.rejects(createNativeSopAuthorityReader({ ...options, sessionAdmission: async () => undefined })(input, principal));
  await assert.rejects(createNativeSopAuthorityReader(options)({ ...input, sessionKey: "missing" }, principal));
  await assert.rejects(createNativeSopAuthorityReader(options)({ ...input, expectedRevision: 0 }, principal));
  await assert.rejects(createNativeSopAuthorityReader(options)({ ...input, grants: ["base"] } as never, principal));
  assert.equal(reads, 1);
});

test("prepared revision CAS rejects an overlapping mutation and pin survives two cold reads", async () => {
  const root = await mkdtemp(join(tmpdir(), "r150-sop-authority-"));
  console.log(`authority-state-evidence=${root}`);
  const store = new SopStateStore(root);
  const initial = await store.loadOrCreate("session", bundle, "sop");
  const active = { status: "active", active_skill_id: "sop", active_step_id: "response" };
  const saved = await store.replace("session", bundle, active, initial.revision);
  assert.equal(saved.revision, initial.revision + 1);
  const before = await store.authority("session", saved.revision);
  await assert.rejects(store.replace("session", bundle, { ...active, active_step_id: "other" }, initial.revision), /changed during preparation/);
  assert.deepEqual(await store.authority("session", saved.revision), before);
  for (let i = 0; i < 2; i++) {
    const script = `import {SopStateStore} from ${JSON.stringify(new URL("../../src/sop/staffdeck/SopStateStore.js", import.meta.url).href)};console.log(JSON.stringify(await new SopStateStore(${JSON.stringify(root)}).authority('session',${saved.revision})));`;
    const env = { ...process.env }; delete env.NODE_OPTIONS;
    const readback = spawnSync(process.execPath, [...process.execArgv, "--input-type=module", "-e", script], { env, encoding: "utf8" });
    assert.equal(readback.status, 0, readback.stderr);
    assert.deepEqual(JSON.parse(readback.stdout), before);
  }
  await store.replace("session", bundle, { ...active, status: "completed" });
  await assert.rejects(store.authority("session", saved.revision), /stale/);
  await assert.rejects(store.authority("missing", 1), /No admitted/);
  assert.equal(await store.status("missing"), undefined);
});

test("projection cannot infer authority from count/refs or incompatible provider/pin domains", () => {
  assert.deepEqual(validateSopAuthorityProjection(projection(), context), projection());
  for (const change of [ { context: { ...context, expectedRevision: 1 } }, { optionalCapabilities: [{ ...projection().optionalCapabilities[0], required: true }] },
    { optionalCapabilities: [{ ...projection().optionalCapabilities[0], providerModuleId: "other" }] },
    { optionalCapabilities: [{ ...projection().optionalCapabilities[0], selectionMode: "pinned" }] },
    { optionalCapabilities: [{ authorized_knowledge_base_count: 1 }] } ]) {
    assert.throws(() => validateSopAuthorityProjection({ ...projection(), ...change }, context));
  }
});

test("typed authority module and installed Knowledge Port carry host context outside model arguments", async t => {
  const bodies: Record<string, any>[] = [];
  const server = createServer(async (req, res) => {
    res.setHeader("content-type", "application/json");
    if (req.method === "GET") {
      const authority = req.url?.includes("sop-capability-authority");
      res.end(JSON.stringify({ protocolVersion: "2.0", implementationId: authority ? "staffdeck.sop-capability-authority" : "staffdeck.knowledge",
        contract: authority ? "staffdeck.sop-capability-authority/v1" : "staffdeck.knowledge/v1", transport: "module-http-v2", methods: [authority ? "resolve" : "query"] })); return;
    }
    const chunks: Buffer[] = []; for await (const chunk of req) chunks.push(Buffer.from(chunk));
    const body = JSON.parse(Buffer.concat(chunks).toString()); bodies.push(body);
    assert.equal(req.headers.authorization, "Bearer synthetic-account-key");
    res.end(JSON.stringify({ kind: "response", messageId: "reply", inReplyTo: body.messageId, requestId: body.requestId, ok: true,
      payload: { result: body.payload.operation === "resolve" ? projection(body.payload.input) : { chunks: [] } } }));
  });
  await new Promise<void>(resolve => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise<void>(resolve => server.close(() => resolve())));
  const address = server.address(); assert.ok(address && typeof address !== "string");
  const binding: ExternalModuleBinding = { enabled: true, implementationId: "staffdeck.knowledge", contract: "staffdeck.knowledge/v1",
    transport: "module-http-v2", endpoint: `http://127.0.0.1:${address.port}`, manifestPath: "/api/v1/knowledge-module/module-manifest",
    callPath: "/api/v1/agents/agent/knowledge-module/v2/module/call", methods: ["query"], agentId: "agent", credentialEnv: "R150_SYNTHETIC_KEY" };
  process.env.R150_SYNTHETIC_KEY = "synthetic-account-key"; t.after(() => { delete process.env.R150_SYNTHETIC_KEY; });
  assert.deepEqual(await createSopCapabilityAuthorityPort(binding).resolve(context), projection());
  const port = createKnowledgeModulePort(binding);
  const executionContext = { ...context, snapshotId: "snapshot", registryGeneration: 3 };
  await withKnowledgeAuthority(executionContext, () => port.call("query", { query: "fact" }));
  assert.deepEqual(bodies[1].payload.authorityContext, executionContext);
  assert.deepEqual(bodies[1].payload.input, { query: "fact" });
  await port.call("query", { query: "ordinary" });
  assert.equal(bodies[2].payload.authorityContext, undefined);
  assert.equal(currentKnowledgeAuthority(), undefined);
});
