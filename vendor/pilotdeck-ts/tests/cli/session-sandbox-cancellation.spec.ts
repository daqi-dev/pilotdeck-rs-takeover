import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { createLocalGateway } from "../../src/cli/createLocalGateway.js";
import { NsjailSessionExecutionProvider } from "../../src/sandbox/nsjail/NsjailProvider.js";
import { DEFAULT_MODEL_CAPABILITIES } from "../../src/model/protocol/capabilities.js";
import type { ModelRuntime } from "../../src/model/ModelRuntime.js";
import type { CanonicalModelEvent, CanonicalModelRequest } from "../../src/model/protocol/canonical.js";
import type { GatewayEvent } from "../../src/gateway/protocol/types.js";

class CancellationModel implements ModelRuntime {
  private readonly stages = new Map<string, number>();
  async *stream(request: CanonicalModelRequest): AsyncIterable<CanonicalModelEvent> {
    const messages = JSON.stringify(request.messages);
    const recovering = messages.includes("RECOVER");
    const long = messages.includes("LONG");
    const key = recovering ? "RECOVER" : long ? "LONG" : "SHORT";
    const stage = this.stages.get(key) ?? 0;
    this.stages.set(key, stage + 1);
    yield { type: "request_started", provider: "test", model: "test" };
    yield { type: "message_start", role: "assistant" };
    if (stage === 0) {
      yield { type: "tool_call_start", id: "command", name: "bash" };
      yield { type: "tool_call_end", toolCall: { id: "command", name: "bash", input: {
        command: recovering ? "test \"$(cat counter)\" = 1; printf RECOVERED_ONCE"
          : long ? "n=$(cat counter 2>/dev/null || printf 0); printf '%s' $((n+1)) > counter; sleep 120 & wait"
          : "printf B_UNAFFECTED > result; cat result",
      } } };
      yield { type: "message_end", finishReason: "tool_call" };
    } else { yield { type: "text_delta", text: "done" }; yield { type: "message_end", finishReason: "stop" }; }
  }
  async complete() { return { role: "assistant" as const, content: [{ type: "text" as const, text: "done" }], finishReason: "stop" as const }; }
  getCapabilities() { return DEFAULT_MODEL_CAPABILITIES; }
  getMultimodal() { return { input: ["text" as const] }; }
  getProviderProtocol() { return "openai" as const; }
  getProviderBaseUrl() { return undefined; }
}

test("Gateway cancellation preserves B and resumes A without replaying an uncertain side effect", {
  skip: process.platform !== "linux" || !process.env.PILOTDECK_NSJAIL_ROOTFS, timeout: 90_000,
}, async () => {
  const root = await mkdtemp(join(tmpdir(), "pd-cancel-"));
  const project = join(root, "project");
  const sessions = join(root, "sessions");
  await mkdir(project); await mkdir(sessions);
  await writeFile(join(project, "pilotdeck.yaml"), "schemaVersion: 1\nagent:\n  model: test/test\n  maxContextTokens: 32768\n  maxOutputTokens: 4096\nextension:\n  builtinPluginsEnabled:\n    funasr: false\nmodel:\n  providers:\n    test:\n      protocol: openai\n      url: http://127.0.0.1:1\n      apiKey: test\n      models:\n        test:\n          capabilities:\n            supportsToolUse: true\n            maxContextTokens: 32768\n            maxOutputTokens: 4096\n");
  const provider = new NsjailSessionExecutionProvider({ sessionsRoot: sessions,
    rootfs: process.env.PILOTDECK_NSJAIL_ROOTFS!, executable: process.env.PILOTDECK_NSJAIL_BIN,
    cgroupV2Root: process.env.PILOTDECK_NSJAIL_CGROUP_ROOT });
  const local = createLocalGateway({ projectRoot: project, pilotHome: project, fallbackProjectRoot: project,
    permissionMode: "bypassPermissions", sessionExecutionProvider: provider, sessionExecutionStorageRoot: sessions,
    __testModelFactory: () => new CancellationModel() });
  const events: Record<string, GatewayEvent[]> = {};
  const submit = async (sessionKey: string, message: string) => {
    const result: GatewayEvent[] = [];
    events[`${sessionKey}-${message}`] = result;
    for await (const event of local.gateway.submitTurn({ sessionKey, projectKey: project, channelKey: "test", workspaceCwd: project, message, mode: "bypassPermissions" })) result.push(event);
    return result;
  };
  let failure: unknown;
  const startedAt = new Date().toISOString();
  const sandboxKey = Buffer.from("a").toString("base64url");
  const counter = join(sessions, sandboxKey, "workspace", "counter");
  try {
    const a = submit("a", "LONG");
    const b = submit("b", "SHORT");
    for (let attempt = 0; ; attempt++) {
      if (await readFile(counter, "utf8").catch(() => "") === "1") break;
      assert.ok(attempt < 600, "A side effect did not start");
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    await local.gateway.abortTurn({ sessionKey: "a", reason: "fixture response lost after observed write" });
    const [aEvents, bEvents] = await Promise.all([a, b]);
    assert.ok(bEvents.some((event) => event.type === "turn_completed" && event.finishReason === "completed"), JSON.stringify(bEvents));
    assert.ok(aEvents.some((event) => event.type === "turn_completed" && event.finishReason !== "completed"), JSON.stringify(aEvents));
    await local.gateway.closeSession({ sessionKey: "a", reason: "resume uncertain result" });
    const resumed = await submit("a", "RECOVER");
    assert.ok(resumed.some((event) => event.type === "turn_completed" && event.finishReason === "completed"), JSON.stringify(resumed));
    assert.match(JSON.stringify(resumed), /RECOVERED_ONCE/);
    assert.equal(await readFile(counter, "utf8"), "1");
    assert.equal(await readFile(join(sessions, Buffer.from("b").toString("base64url"), "workspace", "result"), "utf8"), "B_UNAFFECTED");
  } catch (error) { failure = error; throw error; }
  finally {
    await local.dispose();
    const artifacts = process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR;
    if (artifacts) {
      await mkdir(artifacts, { recursive: true });
      const observedCounter = await readFile(counter, "utf8").catch(() => null);
      await writeFile(join(artifacts, "gateway-cancellation.json"), JSON.stringify({ status: failure ? "FAIL" : "PASS", events,
        cases: ["LIFE-GATEWAY-CANCEL", "LIFE-UNKNOWN-EFFECT"].map((caseId) => ({ caseId, status: failure ? "FAIL" : "PASS", sessionKey: "a", sandboxKey, generation: 2,
          request: { operation: "abort after observed write; close; resume without replay" }, response: events, exitCode: failure ? 1 : 0, startedAt, finishedAt: new Date().toISOString(),
          hostObservation: { counter: observedCounter }, failureReason: failure ? String(failure) : null })),
      }, null, 2));
    }
    await rm(root, { recursive: true, force: true });
  }
});
