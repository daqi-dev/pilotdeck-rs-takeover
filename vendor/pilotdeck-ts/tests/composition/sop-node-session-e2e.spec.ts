import assert from "node:assert/strict";
import { createServer, type Server } from "node:http";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { createLocalGateway } from "../../src/cli/createLocalGateway.js";
import { AgentLoopSidecarServer, AgentLoopSidecarTcpServer } from "../../src/agent/index.js";
import { createSidecarExecution } from "../../src/cli/pilotdeck-agent-loop-default-factory.js";
import { readAgentProjectSessionPersistence, readSubagentProjectSessionPersistence } from "../../src/session/storage/ProjectSessionStorage.js";
import { replayTranscriptEntries } from "../../src/session/transcript/TranscriptReplay.js";
import { sopNodeSessionId, sopNodeSidechainId } from "../../src/sop/staffdeck/SopNodeSessions.js";
import { SopStateStore } from "../../src/sop/staffdeck/SopStateStore.js";

for (const sidecarMode of [false, true])
for (const waitStatus of ["awaiting_user", "handoff"] as const) test(`Gateway restores real ${sidecarMode ? "sidecar" : "native"} node sessions after ${waitStatus} and restart`, async () => {
  const root = await mkdtemp(join(tmpdir(), "sop-node-session-e2e-"));
  const main = "sop-real-main";
  const store = new SopStateStore(join(root, "sop", "sessions"));
  const requests: Record<string, unknown>[] = [];
  const ownerSessions: string[] = [];
  const submittedNodes: string[] = [];
  let agentCalls = 0;
  const owner = createServer(async (req, res) => {
    if (req.method === "GET") {
      res.end(JSON.stringify({ status: "ok", protocolVersion: "2.0", moduleId: "sop.runtime",
        contract: "sop.lifecycle/v2", operations: ["prepare", "submit"] }));
      return;
    }
    const chunks: Buffer[] = [];
    for await (const chunk of req) chunks.push(Buffer.from(chunk));
    const envelope = JSON.parse(Buffer.concat(chunks).toString());
    ownerSessions.push(envelope.sessionId);
    const { state, proposal } = envelope.payload;
    const nodeId = state.active_step_id ?? "A";
    let payload;
    if (req.url?.endsWith("prepare")) {
      payload = { state: { ...state, status: "active", active_step_id: nodeId }, step: {
        skillId: "example", skillName: "Example", version: "1", nodeId, node: { type: "response" },
        contextMode: "new_session", instruction: `Execute node-${nodeId}.`, knownSlots: state.slots_json ?? {},
        expectedUserInfo: nodeId === "B" && waitStatus === "awaiting_user" ? ["name"] : [], allowedNextStepIds: nodeId === "A" ? ["B"] : [],
        requiredToolNames: [], allowedActions: ["answer_user", "ask_user"], isTerminal: nodeId === "B",
        declaresHandoff: nodeId === "B" && waitStatus === "handoff" } };
    } else {
      submittedNodes.push(nodeId);
      payload = { state: { ...state, active_step_id: nodeId === "A" ? "B" : "B",
        status: nodeId === "A" ? "active" : proposal.status,
        slots_json: { ...state.slots_json, ...proposal.slotUpdates } },
        result: { ...proposal, events: [] } };
    }
    res.setHeader("content-type", "application/json");
    res.end(JSON.stringify({ protocolVersion: "2.0", requestId: envelope.requestId,
      ok: true, outcome: "completed", payload }));
  });
  const provider = createServer(async (req, res) => {
    const chunks: Buffer[] = [];
    for await (const chunk of req) chunks.push(Buffer.from(chunk));
    const body = JSON.parse(Buffer.concat(chunks).toString());
    if (!body.stream || !body.tools) {
      res.end(JSON.stringify({ choices: [{ message: { role: "assistant", content: '{"title":"Node sessions"}' }, finish_reason: "stop" }] }));
      return;
    }
    requests.push(body);
    const call = agentCalls++;
    const proposal = call === 0 ? { status: "completed", replyFragment: "A-internal-evidence", nextStepId: "B", slotUpdates: {} }
      : call === 1 ? { status: waitStatus, replyFragment: "Supply name", slotUpdates: {} }
      : { status: "completed", replyFragment: "Final Ada reply", slotUpdates: { name: "Ada" } };
    res.writeHead(200, { "content-type": "text/event-stream" });
    res.write(`data: ${JSON.stringify({ choices: [{ delta: { tool_calls: [{ index: 0, id: `node-call-${call}`,
      type: "function", function: { name: "submit_step_result", arguments: JSON.stringify(proposal) } }] }, finish_reason: null }] })}\n\n`);
    res.write(`data: ${JSON.stringify({ choices: [{ delta: {}, finish_reason: "tool_calls" }],
      usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } })}\n\n`);
    res.end("data: [DONE]\n\n");
  });
  let local: ReturnType<typeof createLocalGateway> | undefined;
  let sidecar: AgentLoopSidecarTcpServer | undefined;
  try {
    const ownerUrl = await listen(owner);
    const modelUrl = await listen(provider);
    const startSidecar = () => new AgentLoopSidecarTcpServer(new AgentLoopSidecarServer(
      async input => createSidecarExecution(input), { moduleId: "pilotdeck-agent-loop" }));
    sidecar = sidecarMode ? startSidecar() : undefined;
    const sidecarAddress = await sidecar?.listen({ host: "127.0.0.1", port: 0 });
    await writeFile(join(root, "nodes.yaml"), `sops:
  - id: example
    version: "1"
    content:
      start_node_id: A
      nodes:
        - { node_id: A, contextMode: new_session, model: test/node-a }
        - { node_id: B, contextMode: new_session, model: test/node-b, type: ${waitStatus === "handoff" ? "handoff" : "collect_info"}, assignee_user_id: approver }
`);
    await writeFile(join(root, "pilotdeck.yaml"), `schemaVersion: 1
agent: { model: test/test }
model:
  providers:
    test:
      protocol: openai
      url: ${modelUrl}
      apiKey: test-only
      models:
        test:
          capabilities: { supportsToolUse: true, maxContextTokens: 65536, maxOutputTokens: 8192 }
        node-a:
          capabilities: { supportsToolUse: true, maxContextTokens: 65536, maxOutputTokens: 8192 }
        node-b:
          capabilities: { supportsToolUse: true, maxContextTokens: 65536, maxOutputTokens: 8192 }
modules:
${sidecarAddress ? `  agentLoop:
    enabled: true
    implementationId: pilotdeck-agent-loop
    contract: pilotdeck.agent-loop/v1
    transport: module-tcp-v2
    host: ${sidecarAddress.host}
    port: ${sidecarAddress.port}
    methods: [execute, status, resume, ack]
` : ""}
  sop:
    enabled: true
    provider: staffdeck
    endpoint: ${ownerUrl}
    definitionsPath: nodes.yaml
    defaultSopId: example
`);
    const start = () => createLocalGateway({ projectRoot: root, pilotHome: root, fallbackProjectRoot: root,
      permissionMode: "bypassPermissions" });
    local = start();
    const turn = async (message: string) => {
      const events = [];
      for await (const event of local!.gateway.submitTurn({ sessionKey: main, workspaceCwd: root,
        channelKey: "test", message, mode: "bypassPermissions" })) events.push(event);
      assert.ok(events.some(e => e.type === "turn_completed"), JSON.stringify(events));
      const persisted = await readAgentProjectSessionPersistence({ projectRoot: root, pilotHome: root, sessionId: main });
      assert.ok(persisted.entries.every(e => e.sessionId === main));
      return events;
    };
    const initialEvents = await turn("Start workflow");
    assert.equal(initialEvents.filter(e => e.type === "turn_completed").length, 1);
    assert.equal(agentCalls, 2);
    assert.equal(requests[0]?.model, "node-a");
    assert.equal(requests[1]?.model, "node-b");
    assert.ok(!JSON.stringify(requests[1]).includes("A-internal-evidence"));
    assert.ok(!JSON.stringify(requests[1]).includes("node-call-0"), "a new node must exclude previous assistant/tool history");
    assert.ok(JSON.stringify(requests[1]).includes("Start workflow"));
    const readNode = async (nodeId: string) => {
      const sessionId = sopNodeSessionId(main, "example", nodeId);
      const persisted = await readSubagentProjectSessionPersistence({ projectRoot: root, pilotHome: root,
        parentSessionId: main, sessionId, sidechainId: sopNodeSidechainId("example", nodeId) });
      assert.ok(persisted.entries.length > 0, "node must have real persistent events");
      assert.ok(persisted.entries.every(e => e.sessionId === sessionId));
      const replay = replayTranscriptEntries(persisted.entries);
      assert.equal(replay.metadata.parentSessionId, main);
      return { persisted, replay };
    };
    const a = await readNode("A");
    const b = await readNode("B");
    for (const [node, model] of [[a, "node-a"], [b, "node-b"]] as const) {
      const requests = node.persisted.entries.filter(e => e.type === "model_request");
      assert.ok(requests.length > 0);
      assert.ok(requests.every(e => e.type === "model_request" && e.request.provider === "test" && e.request.model === model));
    }
    assert.notEqual(a.persisted.entries[0].sessionId, b.persisted.entries[0].sessionId);
    assert.ok(b.replay.messages.some(m => m.role === "assistant"));
    const waiting = await store.status(main);
    assert.equal(waiting?.state.status, waitStatus);
    const listed = await local.gateway.listSessions({ projectKey: root });
    assert.deepEqual(listed.sessions.map(s => s.sessionId), [main]);
    await local.dispose();
    await writeFile(join(root, "nodes.yaml"), (await readFile(join(root, "nodes.yaml"), "utf8"))
      .replace("model: test/node-b", "model: test/node-a"));
    if (sidecar && sidecarAddress) {
      await sidecar.close();
      sidecar = startSidecar();
      await sidecar.listen({ host: sidecarAddress.host, port: sidecarAddress.port });
    }
    local = start();
    const restored = await new SopStateStore(join(root, "sop", "sessions")).status(main);
    assert.equal(restored?.revision, waiting?.revision);
    assert.equal(restored?.wait?.id, waiting?.wait?.id);
    if (waitStatus === "handoff") {
      assert.ok(waiting?.wait?.id);
      const resume = { sessionId: main, requestId: "resume-real-node", waitId: waiting!.wait!.id,
        source: "human" as const, expectedRevision: waiting!.revision, message: "name: Ada",
        authority: { tenantId: "test", sessionId: main, subject: { tenantId: "test", userId: "approver",
          role: "member" as const, disabled: false, source: "web" as const } }, slotUpdates: { name: "Ada" } };
      await assert.rejects(() => store.resume({ ...resume, waitId: "expired-wait" }));
      await assert.rejects(() => store.resume({ ...resume, authority: { ...resume.authority, sessionId: "wrong-parent" } }));
      assert.equal((await store.status(main))?.revision, waiting!.revision);
      const accepted = await store.resume(resume);
      assert.equal((await store.resume(resume)).duplicate, true);
      assert.equal((await store.status(main))?.revision, accepted.revision);
    }
    await turn("name: Ada");
    assert.deepEqual(submittedNodes, ["A", "B", "B"]);
    assert.ok(JSON.stringify(requests[2]).includes("node-call-1"), "current node history survives Gateway restart");
    assert.equal(requests[2]?.model, "node-b");
    assert.ok(!JSON.stringify(requests[2]).includes("node-call-0"), "prior node history stays isolated");
    const resumedB = await readNode("B");
    assert.ok(resumedB.persisted.entries.filter(e => e.type === "model_request")
      .every(e => e.type === "model_request" && e.request.model === "node-b"), "resume must use the pinned node model");
    assert.equal(resumedB.persisted.entries[0].sessionId, b.persisted.entries[0].sessionId);
    assert.equal(resumedB.replay.messages.filter(m => m.role === "user"
      && m.content.some(part => part.type === "text")).length, 2);
    assert.equal((await readNode("A")).persisted.entries.length, a.persisted.entries.length);
    assert.ok(ownerSessions.every(id => id === main));
    assert.equal((await store.status(main))?.state.status, "completed");
    const history = await local.gateway.readSessionMessages({ sessionKey: main, projectKey: root });
    assert.equal(history.messages.filter(m => m.role === "assistant" && m.text === "Final Ada reply").length, 1);
    assert.ok(!JSON.stringify(history).includes("A-internal-evidence"));
    assert.ok(!JSON.stringify(history).includes("node-call-0"));
  } finally {
    await local?.dispose();
    await sidecar?.close();
    await Promise.all([close(owner), close(provider)]);
    await rm(root, { recursive: true, force: true });
  }
});

async function listen(server: Server): Promise<string> {
  await new Promise<void>(resolve => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("Server did not bind");
  return `http://127.0.0.1:${address.port}`;
}

async function close(server: Server): Promise<void> {
  await new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
}
