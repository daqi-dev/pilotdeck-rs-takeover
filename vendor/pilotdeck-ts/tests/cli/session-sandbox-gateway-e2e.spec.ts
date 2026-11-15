import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, readdir, rm, stat, symlink, writeFile } from "node:fs/promises";
import { Readable } from "node:stream";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { createLocalGateway } from "../../src/cli/createLocalGateway.js";
import { NsjailSessionExecutionProvider } from "../../src/sandbox/nsjail/NsjailProvider.js";
import { LspService, createNodeStdioLspProvider } from "../../src/lsp/index.js";
import type { PilotDeckToolDefinition } from "../../src/tool/protocol/types.js";
import type {
  CanonicalModelEvent,
  CanonicalModelRequest,
  CanonicalModelResponse,
} from "../../src/model/protocol/canonical.js";
import type { ModelRuntime } from "../../src/model/ModelRuntime.js";
import { DEFAULT_MODEL_CAPABILITIES } from "../../src/model/protocol/capabilities.js";
import type { MultimodalConstraints } from "../../src/model/protocol/multimodal.js";
import { attachCaseEvidence, observeExecution, type ExecutionObservation } from "../fixtures/session-execution-evidence.js";
import { UploadStore } from "../../src/gateway/dialog/UploadStore.js";
import type { ChannelAttachment, UploadedAttachmentRef } from "../../src/gateway/protocol/types.js";

const rootfs = process.env.PILOTDECK_NSJAIL_ROOTFS;
const executable = process.env.PILOTDECK_NSJAIL_BIN ?? "nsjail";
const skipReason = process.platform !== "linux"
  ? "Gateway nsjail E2E requires Linux"
  : !rootfs
    ? "set PILOTDECK_NSJAIL_ROOTFS to run the Gateway nsjail E2E"
    : undefined;

const TEST_CONFIG = `
schemaVersion: 1
agent:
  model: test/test
  maxContextTokens: 32768
  maxOutputTokens: 4096
extension:
  builtinPluginsEnabled:
    funasr: false
model:
  providers:
    test:
      protocol: openai
      url: http://127.0.0.1:1
      apiKey: test-only
      models:
        test:
          capabilities:
            supportsToolUse: true
            maxContextTokens: 32768
            maxOutputTokens: 4096
`;

class SessionMarkerModel implements ModelRuntime {
  private readonly stages = new Map<string, number>();
  private readonly childStages = new Map<string, number>();
  constructor(private readonly sessionsRoot: string, private readonly hostMarker: string, private readonly observeRequest: (request: CanonicalModelRequest) => void) {}

  async *stream(request: CanonicalModelRequest): AsyncIterable<CanonicalModelEvent> {
    this.observeRequest(request);
    const childRequest = request.messages.flatMap((message) => message.role === "user" ? message.content.filter((part) => part.type === "text").map((part) => part.text) : []).find((text) => text.includes("CHILD_SANDBOX_"));
    if (childRequest) {
      const marker = /CHILD_SANDBOX_([ABC])/.exec(childRequest)![1]!;
      const stage = this.childStages.get(marker) ?? 0;
      this.childStages.set(marker, stage + 1);
      yield { type: "request_started", provider: "test", model: "test" };
      yield { type: "message_start", role: "assistant" };
      if (stage === 0) {
        const other = join(this.sessionsRoot, Buffer.from(marker === "A" ? "session-b" : "session-a").toString("base64url"), "workspace", "victim.txt");
        for (const call of [
          { id: `child-write-${marker}`, name: "write_file", input: { file_path: "child.txt", content: `${marker}_CHILD` } },
          { id: `child-bash-${marker}`, name: "bash", input: { command: `test ! -e '${other}' && test ! -e '${this.hostMarker}' && cat same.txt` } },
          { id: `child-mcp-${marker}`, name: "mcp__child__probe", input: { otherPath: other, marker } },
        ]) { yield { type: "tool_call_start", id: call.id, name: call.name }; yield { type: "tool_call_end", toolCall: call }; }
        yield { type: "message_end", finishReason: "tool_call" };
      } else {
        yield { type: "text_delta", text: `Scope: session ${marker}\nResult: isolated\nKey files: child.txt\nFiles changed: child.txt\nIssues: none` };
        yield { type: "message_end", finishReason: "stop" };
      }
      return;
    }
    const session = request.messages.flatMap((message) => message.role === "user"
      ? message.content.flatMap((part) => part.type === "text" ? [/create the marker for session-([abc])/.exec(part.text)?.[1]] : []) : [])
      .find(Boolean)!.toUpperCase();
    yield { type: "request_started", provider: "test", model: "test" };
    yield { type: "message_start", role: "assistant" };
    const stage = this.stages.get(session) ?? 0;
    this.stages.set(session, stage + 1);
    if (stage === 0) {
      yield { type: "tool_call_start", id: `write-${session}`, name: "write_file" };
      yield {
        type: "tool_call_end",
        toolCall: {
          id: `write-${session}`,
          name: "write_file",
          input: { file_path: "same.txt", content: `${session}_MARKER\n` },
        },
      };
      yield { type: "message_end", finishReason: "tool_call" };
      return;
    }
    if (stage === 1) {
      const otherKey = session === "A" ? "session-b" : "session-a";
      const otherFile = join(this.sessionsRoot, Buffer.from(otherKey).toString("base64url"), "workspace", "victim.txt");
      const calls = [
        { id: `read-${session}`, name: "read_file", input: { file_path: "same.txt" } },
        { id: `read-edit-${session}`, name: "read_file", input: { file_path: "edit.txt" } },
        { id: `bash-${session}`, name: "bash", input: { command: `test ! -e '${this.hostMarker}' && test ! -e '${otherFile}' && printf '${session}_SHELL'` } },
        { id: `python-${session}`, name: "execute_code", input: { code: `import os\nassert not os.path.exists(${JSON.stringify(otherFile)})\nfrom pilotdeck_tools import read_file\nprint(read_file(file_path='same.txt'))\nprint(open('same.txt').read())` } },
        { id: `grep-${session}`, name: "grep", input: { pattern: `${session}_MARKER`, path: "." } },
        { id: `glob-${session}`, name: "glob", input: { pattern: "*.txt" } },
        { id: `mcp-${session}`, name: "mcp__isolation__probe", input: { otherPath: otherFile, marker: session } },
        { id: `lsp-${session}`, name: "lsp", input: { operation: "goToDefinition", file_path: "source.ts", line: 1, character: 1 } },
        { id: `lsp-foreign-${session}`, name: "lsp", input: { operation: "goToDefinition", file_path: "foreign.ts", line: 1, character: 1 } },
      ];
      for (const call of calls) {
        yield { type: "tool_call_start", id: call.id, name: call.name };
        yield { type: "tool_call_end", toolCall: call };
      }
      yield { type: "message_end", finishReason: "tool_call" };
      return;
    }
    if (stage === 2) {
      for (const call of [
        { id: `edit-${session}`, name: "edit_file", input: { file_path: "edit.txt", old_string: "BEFORE", new_string: `${session}_EDIT` } },
        { id: `agent-${session}`, name: "agent", input: { description: "probe inherited isolation", subagent_type: "isolation-child", prompt: `CHILD_SANDBOX_${session}` } },
        { id: `task-${session}`, name: "task_create", input: { command: `printf '${session}_TASK'; sleep 1` } },
        { id: `extension-${session}`, name: "session_extension_probe", input: {} },
        { id: `attachment-${session}`, name: "send_attachment", input: { file_path: "same.txt" } },
      ]) { yield { type: "tool_call_start", id: call.id, name: call.name }; yield { type: "tool_call_end", toolCall: call }; }
      yield { type: "message_end", finishReason: "tool_call" };
      return;
    }
    if (stage === 3) {
      const otherKey = session === "A" ? "session-b" : "session-a";
      const otherFile = join(this.sessionsRoot, Buffer.from(otherKey).toString("base64url"), "workspace", "victim.txt");
      for (const call of [
        { id: `cross-read-${session}`, name: "read_file", input: { file_path: otherFile } },
        { id: `cross-write-${session}`, name: "write_file", input: { file_path: otherFile, content: "BAD" } },
      ]) {
        yield { type: "tool_call_start", id: call.id, name: call.name };
        yield { type: "tool_call_end", toolCall: call };
      }
      yield { type: "message_end", finishReason: "tool_call" };
      return;
    }
    yield { type: "text_delta", text: `${session} complete` };
    yield { type: "message_end", finishReason: "stop" };
  }

  async complete(): Promise<CanonicalModelResponse> {
    return { role: "assistant", content: [{ type: "text", text: "complete" }], finishReason: "stop" };
  }

  getCapabilities() { return DEFAULT_MODEL_CAPABILITIES; }
  getMultimodal(): MultimodalConstraints { return { input: ["text"] }; }
  getProviderProtocol() { return "openai" as const; }
  getProviderBaseUrl() { return undefined; }
}

class ResumeModel implements ModelRuntime {
  async *stream(_request: CanonicalModelRequest): AsyncIterable<CanonicalModelEvent> {
    yield { type: "request_started", provider: "test", model: "test" };
    yield { type: "message_start", role: "assistant" };
    yield { type: "text_delta", text: "resumed" };
    yield { type: "message_end", finishReason: "stop" };
  }

  async complete(): Promise<CanonicalModelResponse> {
    return { role: "assistant", content: [{ type: "text", text: "resumed" }], finishReason: "stop" };
  }

  getCapabilities() { return DEFAULT_MODEL_CAPABILITIES; }
  getMultimodal(): MultimodalConstraints { return { input: ["text"] }; }
  getProviderProtocol() { return "openai" as const; }
  getProviderBaseUrl() { return undefined; }
}

test("one Gateway isolates concurrent A/B/C sessions and resumes A from its path", { skip: skipReason ?? false }, async () => {
  const root = await mkdtemp(join(tmpdir(), "pilotdeck-gateway-nsjail-e2e-"));
  const projectRoot = join(root, "project");
  const sessionsRoot = join(root, "sessions");
  await mkdir(projectRoot, { recursive: true });
  await mkdir(sessionsRoot, { recursive: true });
  await writeFile(join(projectRoot, "pilotdeck.yaml"), TEST_CONFIG, "utf8");
  await mkdir(join(projectRoot, ".pilotdeck"), { recursive: true });
  const hookPlugin = join(projectRoot, ".pilotdeck", "plugins", "isolation-hooks");
  await mkdir(hookPlugin, { recursive: true });
  await writeFile(join(hookPlugin, "plugin.json"), JSON.stringify({ name: "isolation-hooks", hooks: {
    PreToolUse: [{ matcher: "*", hooks: [{ type: "command", command: "printf '%s' \"$HOME\" > hook-observed.txt; printf '{\"continue\":true}'" }] }],
  } }));
  await writeFile(join(projectRoot, ".pilotdeck", "mcp.json"), JSON.stringify({ mcpServers: {
    isolation: { command: "node", args: ["/workspace/mcp-fixture.mjs"] },
  } }));
  const hostMarker = join(root, "host-secret.txt");
  await writeFile(hostMarker, "HOST_PRIVATE");
  for (const key of ["session-a", "session-b", "session-c"]) {
    const workspace = join(sessionsRoot, Buffer.from(key).toString("base64url"), "workspace");
    await mkdir(workspace, { recursive: true });
    await writeFile(join(workspace, "victim.txt"), `${key}_PRIVATE`);
    await writeFile(join(workspace, "edit.txt"), "BEFORE");
    await writeFile(join(workspace, "mcp-fixture.mjs"), MCP_FIXTURE);
    await writeFile(join(workspace, "lsp-fixture.mjs"), LSP_FIXTURE);
    await writeFile(join(workspace, "source.ts"), `const marker = ${JSON.stringify(key)};`);
    await writeFile(join(workspace, "foreign.ts"), "const foreign = 1;");
  }
  const cases: Array<Record<string, unknown>> = [];
  const executionObservations: ExecutionObservation[] = [];
  const hostObservations: unknown[] = [];
  attachCaseEvidence(cases, executionObservations);
  const observedEvents: Record<string, unknown[]> = {};
  const modelRequests: string[] = [];
  let failure: unknown;

  const createProvider = () => {
    const provider = new NsjailSessionExecutionProvider({
    executable,
    rootfs: rootfs!,
    sessionsRoot,
    sandboxMode: "danger-full-access",
    });
    const create = provider.createSession.bind(provider);
    provider.createSession = async (binding) => observeExecution(await create(binding), binding, executionObservations);
    return provider;
  };
  const firstProvider = createProvider();
  const uploads = new UploadStore({ resolveProject: async () => projectRoot, listProjects: async () => [projectRoot] });
  const first = createLocalGateway({
    projectRoot,
    pilotHome: projectRoot,
    fallbackProjectRoot: projectRoot,
    permissionMode: "bypassPermissions",
    sessionExecutionProvider: firstProvider,
    sessionExecutionStorageRoot: sessionsRoot,
    uploadLifecycle: uploads,
    extraTools: [{
      name: "session_extension_probe", description: "Write using the current execution world", kind: "custom",
      inputSchema: { type: "object", properties: {} }, isReadOnly: () => false, isConcurrencySafe: () => false,
      execute: async () => { throw new Error("Unbound extension executed on host"); },
      bindExecutionWorld: (world) => ({
        name: "session_extension_probe", description: "Bound extension", kind: "custom",
        inputSchema: { type: "object", properties: {} }, isReadOnly: () => false, isConcurrencySafe: () => false,
        execute: async (_input, context) => {
          await world.fs.writeText(join(context.cwd, "extension.txt"), "SESSION_EXTENSION");
          return { content: [{ type: "text", text: "SESSION_EXTENSION" }] };
        },
      }),
    } satisfies PilotDeckToolDefinition],
    lspServiceFactory: () => {
      const service = new LspService();
      service.registerProvider(createNodeStdioLspProvider({
        id: "isolation", command: "node", args: ["/workspace/lsp-fixture.mjs"],
        extensionToLanguage: { ".ts": "typescript" }, requestTimeoutMs: 5_000,
      }));
      return service;
    },
    __testModelFactory: () => new SessionMarkerModel(sessionsRoot, hostMarker, (request) => { modelRequests.push(JSON.stringify(request.messages)); }),
  });

  const submit = async (sessionKey: string, message = `create the marker for ${sessionKey}`, attachments?: ChannelAttachment[], uploadedAttachments?: UploadedAttachmentRef[]) => {
    const events = [];
    for await (const event of first.gateway.submitTurn({
      sessionKey,
      projectKey: projectRoot,
      channelKey: "test",
      workspaceCwd: projectRoot,
      message,
      attachments,
      uploadedAttachments,
      mode: "bypassPermissions",
      sdkSessionConfig: { agents: {
        "isolation-child": { description: "session isolation probe", prompt: "Perform the CHILD_SANDBOX directive.", tools: ["write_file", "bash", "mcp__child__probe"], mcpServers: { child: { type: "stdio", command: "node", args: ["/workspace/mcp-fixture.mjs"] } } },
      } },
    })) events.push(event);
    observedEvents[`${sessionKey}-${Object.keys(observedEvents).length}`] = events;
    return events;
  };

  try {
    const [eventsA, eventsB, eventsC] = await Promise.all([submit("session-a"), submit("session-b"), submit("session-c")]);
    assert.equal(eventsA.some((event) => event.type === "turn_completed" && event.finishReason === "completed"), true, JSON.stringify(eventsA));
    assert.equal(eventsB.some((event) => event.type === "turn_completed" && event.finishReason === "completed"), true, JSON.stringify(eventsB));
    assert.equal(eventsC.some((event) => event.type === "turn_completed" && event.finishReason === "completed"), true, JSON.stringify(eventsC));

    const aRoot = join(sessionsRoot, Buffer.from("session-a").toString("base64url"), "workspace");
    const bRoot = join(sessionsRoot, Buffer.from("session-b").toString("base64url"), "workspace");
    assert.equal(await readFile(join(aRoot, "same.txt"), "utf8"), "A_MARKER\n");
    assert.equal(await readFile(join(bRoot, "same.txt"), "utf8"), "B_MARKER\n");
    for (const [marker, events] of [["A", eventsA], ["B", eventsB], ["C", eventsC]] as const) {
      const ownRoot = join(sessionsRoot, Buffer.from(`session-${marker.toLowerCase()}`).toString("base64url"), "workspace");
      assert.equal(await readFile(join(ownRoot, "same.txt"), "utf8"), `${marker}_MARKER\n`);
      assert.equal(await readFile(join(ownRoot, "victim.txt"), "utf8"), `session-${marker.toLowerCase()}_PRIVATE`);
      assert.equal(await readFile(join(ownRoot, "hook-observed.txt"), "utf8"), "/home/agent");
      assert.equal(await readFile(join(ownRoot, "child.txt"), "utf8"), `${marker}_CHILD`);
      hostObservations.push({ marker, workspace: ownRoot, same: await readFile(join(ownRoot, "same.txt"), "utf8"),
        victim: await readFile(join(ownRoot, "victim.txt"), "utf8"), entries: await readdir(ownRoot) });
      for (const tool of ["read", "edit", "bash", "python", "grep", "glob", "mcp", "lsp", "agent", "task", "extension", "attachment"]) {
        const result = events.find((event) => event.type === "tool_call_finished" && event.toolCallId === `${tool}-${marker}`);
        assert.ok(result?.type === "tool_call_finished" && result.ok, JSON.stringify({ marker, tool, result, results: events.filter((event) => event.type === "tool_call_finished") }));
        if (tool === "python") {
          assert.match(JSON.stringify(result), /"status":"success"/, JSON.stringify(result));
          assert.match(JSON.stringify(result), /"tool_calls_made":1/, JSON.stringify(result));
          assert.match(JSON.stringify(result), new RegExp(`${marker}_MARKER`), JSON.stringify(result));
        }
      }
      const attachment = events.find((event) => event.type === "assistant_attachment");
      assert.ok(attachment?.type === "assistant_attachment" && attachment.attachment.path);
      assert.equal(await readFile(attachment.attachment.path, "utf8"), `${marker}_MARKER\n`);
      for (const tool of ["cross-read", "cross-write", "lsp-foreign"]) {
        const result = events.find((event) => event.type === "tool_call_finished" && event.toolCallId === `${tool}-${marker}`);
        assert.ok(result?.type === "tool_call_finished" && !result.ok, JSON.stringify(result));
        assert.doesNotMatch(JSON.stringify(result), /session-[ab]_PRIVATE|HOST_PRIVATE/);
      }
    }
    assert.equal(await readFile(join(aRoot, "victim.txt"), "utf8"), "session-a_PRIVATE");
    assert.equal(await readFile(join(bRoot, "victim.txt"), "utf8"), "session-b_PRIVATE");
    assert.equal(await readFile(join(aRoot, "hook-observed.txt"), "utf8"), "/home/agent");
    assert.equal(await readFile(join(bRoot, "hook-observed.txt"), "utf8"), "/home/agent");
    assert.equal(await readFile(join(aRoot, "child.txt"), "utf8"), "A_CHILD");
    assert.equal(await readFile(join(bRoot, "child.txt"), "utf8"), "B_CHILD");
    cases.push({ id: "GATEWAY-ISO-AB", status: "PASS", sessionKeys: ["session-a", "session-b"], detail: "one Gateway wrote identical paths into isolated workspaces" });
    cases.push({ id: "GATEWAY-ISO-ABC", status: "PASS", sessionKeys: ["session-a", "session-b", "session-c"], detail: "three concurrent sessions completed their own tools, MCP, LSP, hooks and child agent in one Gateway" });
    cases.push({ id: "EXT-03", status: "PASS", sessionKeys: ["session-a", "session-b"], detail: "real stdio MCP launched in each session jail" });
    cases.push({ id: "EXT-04", status: "PASS", sessionKeys: ["session-a", "session-b"], detail: "real stdio LSP used guest source URIs and returned session-owned locations" });
    cases.push({ id: "EXT-05-COMMAND", status: "PASS", detail: "Gateway command hooks wrote session-local files with guest HOME" });
    cases.push({ id: "EXT-05-EXTENSION", status: "PASS", detail: "Gateway extra tool used the injected session filesystem" });
    cases.push({ id: "EXT-06-DELIVERY", status: "PASS", detail: "Gateway delivered each session's own immutable attachment bytes" });
    cases.push({ id: "EXT-07", status: "PASS", detail: "real child agent inherited jailed file/shell ports and launched its custom MCP through the parent session provider" });
    const inputFile = join(aRoot, "input-attachment.txt");
    await writeFile(inputFile, "OWN_INPUT_ATTACHMENT_PAYLOAD");
    let requestCursor = modelRequests.length;
    const ownAttachmentEvents = await submit("session-a", "inspect own input attachment", [{ type: "file", path: inputFile, mimeType: "text/plain", metadata: { channelKey: "test" } }]);
    assert.ok(ownAttachmentEvents.some((event) => event.type === "turn_completed" && event.finishReason === "completed"));
    assert.ok(modelRequests.slice(requestCursor).some((request) => request.includes("OWN_INPUT_ATTACHMENT_PAYLOAD")));
    await symlink(join(bRoot, "victim.txt"), join(aRoot, "input-escape.txt"));
    requestCursor = modelRequests.length;
    const foreignAttachmentEvents = await submit("session-a", "inspect untrusted input attachment", [
      { type: "file", path: join(bRoot, "victim.txt"), mimeType: "text/plain", metadata: { channelKey: "test" } },
      { type: "file", path: join(aRoot, "input-escape.txt"), mimeType: "text/plain", metadata: { channelKey: "test" } },
    ]);
    assert.ok(foreignAttachmentEvents.some((event) => event.type === "turn_completed" && event.finishReason === "completed"));
    assert.ok(modelRequests.slice(requestCursor).every((request) => !request.includes("session-b_PRIVATE")));
    await rm(join(aRoot, "input-escape.txt"));
    const uploadPayload = "AUTHORIZED_UPLOAD_PAYLOAD";
    const upload = await uploads.create(projectRoot, [{ clientFileId: "file", name: "uploaded.txt", relativePath: "uploaded.txt", size: Buffer.byteLength(uploadPayload), mimeType: "text/plain" }]);
    await uploads.writePart(upload.uploadId, "file", Readable.from(uploadPayload));
    await uploads.complete(upload.uploadId);
    requestCursor = modelRequests.length;
    const importedEvents = await submit("session-a", "inspect authorized upload", undefined, [{ uploadId: upload.uploadId }]);
    assert.ok(importedEvents.some((event) => event.type === "turn_completed" && event.finishReason === "completed"), JSON.stringify(importedEvents));
    assert.ok(modelRequests.slice(requestCursor).some((request) => request.includes(uploadPayload)));
    const importRoot = join(sessionsRoot, Buffer.from("session-a").toString("base64url"), ".pilotdeck", "artifact", "imports");
    const importDirs = await readdir(importRoot);
    assert.equal(importDirs.length, 1);
    assert.equal(await readFile(join(importRoot, importDirs[0]!, "uploaded.txt"), "utf8"), uploadPayload);
    cases.push({ id: "EXT-06-INPUT-IMPORT", status: "PASS", sessionKey: "session-a", sandboxKey: Buffer.from("session-a").toString("base64url"), generation: 1,
      detail: "Gateway resolves raw input attachments in the session jail and copies authorized upload bytes into private artifact storage", hostObservation: { importedBytes: uploadPayload, foreignBytesVisible: false } });
    const history = await first.gateway.readSessionMessages({ sessionKey: "session-a", projectKey: projectRoot });
    const forkPoint = history.messages.find((message) => message.role === "assistant" && message.entryId);
    assert.ok(forkPoint?.entryId, JSON.stringify(history));
    const fork = await first.gateway.forkSession({ sessionKey: "session-a", projectKey: projectRoot, fromEntryId: forkPoint.entryId });
    const dRoot = join(sessionsRoot, Buffer.from(fork.newSessionKey).toString("base64url"), "workspace");
    assert.equal(await readFile(join(dRoot, "same.txt"), "utf8"), "A_MARKER\n");
    assert.notEqual((await stat(join(dRoot, "same.txt"))).ino, (await stat(join(aRoot, "same.txt"))).ino);
    await writeFile(join(dRoot, "same.txt"), "D_MARKER");
    assert.equal(await readFile(join(aRoot, "same.txt"), "utf8"), "A_MARKER\n");
    cases.push({ id: "EXT-10", status: "PASS", detail: "Gateway fork copies authorized session data to new private writable files" });

    await first.dispose();
    const secondProvider = createProvider();
    const resumed = createLocalGateway({
      projectRoot,
      pilotHome: projectRoot,
      fallbackProjectRoot: projectRoot,
      permissionMode: "bypassPermissions",
      sessionExecutionProvider: secondProvider,
      sessionExecutionStorageRoot: sessionsRoot,
      __testModelFactory: () => new ResumeModel(),
    });
    try {
      const resumedEvents = [];
      for await (const event of resumed.gateway.submitTurn({
        sessionKey: "session-a",
        projectKey: projectRoot,
        channelKey: "test",
        workspaceCwd: projectRoot,
        message: "resume",
        mode: "bypassPermissions",
      })) resumedEvents.push(event);
      assert.equal(resumedEvents.some((event) => event.type === "turn_completed" && event.finishReason === "completed"), true, JSON.stringify(resumedEvents));
      assert.equal(await readFile(join(aRoot, "same.txt"), "utf8"), "A_MARKER\n");
      assert.equal(Number.parseInt(await readFile(join(sessionsRoot, Buffer.from("session-a").toString("base64url"), ".pilotdeck", "control", "generation"), "utf8"), 10), 2);
      cases.push({ id: "GATEWAY-RESUME", status: "PASS", sessionKey: "session-a", generation: 2, detail: "Gateway restart resumed the original workspace" });
    } finally {
      await resumed.dispose();
    }
  } catch (error) {
    failure = error;
    throw error;
  } finally {
    await first.dispose();
    for (const entry of cases) entry.hostObservation ??= hostObservations;
    const artifactDir = process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR;
    if (artifactDir) {
      await mkdir(artifactDir, { recursive: true });
      await writeFile(join(artifactDir, "gateway-events.json"), JSON.stringify(observedEvents, null, 2));
      await writeFile(join(artifactDir, "gateway-e2e.json"), JSON.stringify({
        status: !failure && ["GATEWAY-ISO-AB", "GATEWAY-RESUME", "EXT-03", "EXT-04", "EXT-05-COMMAND", "EXT-07", "EXT-10"].every((id) => cases.some((entry) => entry.id === id && entry.status === "PASS")) ? "PASS" : "FAIL",
        failureReason: failure instanceof Error ? failure.message : failure ? String(failure) : undefined,
        cases,
        executionObservations,
        observations: hostObservations,
      }, null, 2));
    }
    await rm(root, { recursive: true, force: true });
  }
});

const LSP_FIXTURE = `
import { existsSync, writeFileSync } from 'node:fs';
let buffer = Buffer.alloc(0);
const send = (id, result) => {
  const body = Buffer.from(JSON.stringify({ jsonrpc: '2.0', id, result }));
  process.stdout.write('Content-Length: ' + body.length + '\\r\\n\\r\\n');
  process.stdout.write(body);
};
process.stdin.on('data', chunk => {
  buffer = Buffer.concat([buffer, chunk]);
  while (true) {
    const split = buffer.indexOf('\\r\\n\\r\\n');
    if (split < 0) break;
    const size = Number(/Content-Length: (\\d+)/i.exec(buffer.subarray(0, split).toString())[1]);
    const start = split + 4;
    if (buffer.length < start + size) break;
    const msg = JSON.parse(buffer.subarray(start, start + size));
    buffer = buffer.subarray(start + size);
    if (msg.method === 'exit') process.exit(0);
    if (msg.method === 'initialize') {
      if (msg.params.rootUri !== 'file:///workspace' || !existsSync('/workspace/source.ts')) process.exit(3);
      writeFileSync('/workspace/lsp-observed.txt', process.env.HOME);
      send(msg.id, { capabilities: { definitionProvider: true } });
    } else if (msg.method === 'textDocument/definition') {
      send(msg.id, [{ uri: msg.params.textDocument.uri.endsWith('/foreign.ts') ? 'file:///other-session/victim.ts' : msg.params.textDocument.uri, range: { start: { line: 0, character: 0 }, end: { line: 0, character: 1 } } }]);
    } else if (msg.id !== undefined) send(msg.id, null);
  }
});
`;

const MCP_FIXTURE = String.raw`
import { createInterface } from 'node:readline';
import { readFileSync, writeFileSync } from 'node:fs';
const lines = createInterface({ input: process.stdin });
lines.on('line', line => {
 const request = JSON.parse(line);
 if (request.id === undefined) return;
 let result = {};
 if (request.method === 'initialize') result = { protocolVersion: request.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: 'session-isolation-fixture', version: '1.0.0' } };
 if (request.method === 'tools/list') result = { tools: [{ name: 'probe', description: 'Session isolation probe', inputSchema: { type: 'object', required: ['otherPath','marker'], properties: { otherPath: { type: 'string' }, marker: { type: 'string' } } } }] };
 if (request.method === 'tools/call') {
  const { otherPath, marker } = request.params.arguments;
  let crossRead = false;
  try { readFileSync(otherPath); crossRead = true; } catch {}
  const own = readFileSync('same.txt','utf8');
  writeFileSync('.mcp-marker', marker);
  result = { content: [{ type: 'text', text: JSON.stringify({ own, crossRead, home: process.env.HOME }) }], isError: crossRead };
 }
 process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result })+'\n');
});
lines.on('close', () => process.exit(0));
`;
