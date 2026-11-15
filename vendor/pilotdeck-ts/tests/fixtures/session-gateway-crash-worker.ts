import { createLocalGateway } from "../../src/cli/createLocalGateway.js";
import { NsjailSessionExecutionProvider } from "../../src/sandbox/nsjail/NsjailProvider.js";
import { DEFAULT_MODEL_CAPABILITIES } from "../../src/model/protocol/capabilities.js";
import type { ModelRuntime } from "../../src/model/ModelRuntime.js";
import type { CanonicalModelRequest, CanonicalModelEvent } from "../../src/model/protocol/canonical.js";

const [projectRoot, sessionsRoot, phase] = process.argv.slice(2);
if (!projectRoot || !sessionsRoot) throw new Error("Missing crash fixture paths");
class CrashModel implements ModelRuntime {
  private stage = 0;
  async *stream(_request: CanonicalModelRequest): AsyncIterable<CanonicalModelEvent> {
    yield { type: "request_started", provider: "test", model: "test" };
    yield { type: "message_start", role: "assistant" };
    if (this.stage++ === 0) {
      yield { type: "tool_call_start", id: "crash-command", name: "bash" };
      yield { type: "tool_call_end", toolCall: { id: "crash-command", name: "bash", input: {
        command: phase === "crash" ? "printf BEFORE_CRASH > crash-marker.txt; sleep 120 & wait" : "test \"$(cat crash-marker.txt)\" = BEFORE_CRASH; printf RECOVERED",
      } } };
      yield { type: "message_end", finishReason: "tool_call" };
    } else { yield { type: "text_delta", text: "recovered" }; yield { type: "message_end", finishReason: "stop" }; }
  }
  async complete() { return { role: "assistant" as const, content: [{ type: "text" as const, text: "recovered" }], finishReason: "stop" as const }; }
  getCapabilities() { return DEFAULT_MODEL_CAPABILITIES; }
  getMultimodal() { return { input: ["text" as const] }; }
  getProviderProtocol() { return "openai" as const; }
  getProviderBaseUrl() { return undefined; }
}
const provider = new NsjailSessionExecutionProvider({ rootfs: process.env.PILOTDECK_NSJAIL_ROOTFS!, executable: process.env.PILOTDECK_NSJAIL_BIN, sessionsRoot,
  cgroupV2Root: process.env.PILOTDECK_NSJAIL_CGROUP_ROOT });
const local = createLocalGateway({ projectRoot, pilotHome: projectRoot, fallbackProjectRoot: projectRoot,
  permissionMode: "bypassPermissions", sessionExecutionProvider: provider, sessionExecutionStorageRoot: sessionsRoot, __testModelFactory: () => new CrashModel() });
try {
  const events = [];
  for await (const event of local.gateway.submitTurn({ sessionKey: "crash-a", projectKey: projectRoot, channelKey: "test", workspaceCwd: projectRoot,
    message: "run crash fixture", mode: "bypassPermissions" })) {
      events.push(event);
      process.stdout.write(JSON.stringify(event) + "\n");
    }
  process.send?.({ events });
} finally { await local.dispose(); process.disconnect?.(); }
