import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { createLocalGateway } from "../../src/cli/createLocalGateway.js";
import { NsjailSessionExecutionProvider } from "../../src/sandbox/nsjail/NsjailProvider.js";
import type { GatewayEvent } from "../../src/gateway/protocol/types.js";

const rootfs = process.env.PILOTDECK_NSJAIL_ROOTFS;
const modelUrl = process.env.PILOTDECK_ACCEPTANCE_MODEL_URL;
const model = process.env.PILOTDECK_ACCEPTANCE_MODEL ?? "qwen3.5-27b";

test("real model runs concurrent projects, delivers private artifacts and resumes A", {
  skip: process.platform !== "linux" || !rootfs || !modelUrl ? "Linux rootfs and PILOTDECK_ACCEPTANCE_MODEL_URL required" : false,
  timeout: 240_000,
}, async () => {
  const root = await mkdtemp(join(tmpdir(), "pd-model-"));
  const projectRoot = join(root, "project");
  const sessionsRoot = join(root, "sessions");
  await mkdir(projectRoot);
  await mkdir(sessionsRoot);
  await writeFile(join(projectRoot, "pilotdeck.yaml"), `schemaVersion: 1
agent:
  model: smoke/${model}
  maxContextTokens: 32768
  maxOutputTokens: 4096
extension:
  builtinPluginsEnabled:
    funasr: false
model:
  providers:
    smoke:
      protocol: openai
      url: ${JSON.stringify(modelUrl)}
      apiKey: acceptance-local
      extraBody:
        chat_template_kwargs:
          enable_thinking: false
      models:
        ${model}:
          capabilities:
            supportsToolUse: true
            maxContextTokens: 32768
            maxOutputTokens: 4096
`);
  const provider = new NsjailSessionExecutionProvider({
    executable: process.env.PILOTDECK_NSJAIL_BIN, rootfs: rootfs!, sessionsRoot, maxActiveSessions: 3,
    cgroupV2Root: process.env.PILOTDECK_NSJAIL_CGROUP_ROOT,
  });
  const local = createLocalGateway({ projectRoot, pilotHome: projectRoot, fallbackProjectRoot: projectRoot,
    permissionMode: "bypassPermissions", sessionExecutionProvider: provider, sessionExecutionStorageRoot: sessionsRoot });
  const cases: Record<string, unknown>[] = [];
  const events: Record<string, GatewayEvent[]> = {};
  let failure: unknown;
  const submit = async (sessionKey: string, message: string) => {
    const turnEvents: GatewayEvent[] = [];
    for await (const event of local.gateway.submitTurn({
      sessionKey, projectKey: projectRoot, channelKey: "acceptance", workspaceCwd: projectRoot,
      mode: "bypassPermissions", message, timeoutMs: 180_000,
    })) turnEvents.push(event);
    events[`${sessionKey}-${Object.keys(events).length}`] = turnEvents;
    assert.ok(turnEvents.some((event) => event.type === "turn_completed" && event.finishReason === "completed"), JSON.stringify(turnEvents));
    return turnEvents;
  };
  try {
    const outcomes = await Promise.allSettled(["A", "B"].map(async (marker) => {
      const sessionKey = `model-${marker}`;
      const startedAt = new Date().toISOString();
      const files = { "app.py": `MARKER='${marker}_MODEL'\n`, "test_app.py": `import unittest\nfrom app import MARKER\nclass TestMarker(unittest.TestCase):\n def test_marker(self): self.assertEqual(MARKER,'${marker}_MODEL')\n`, "result.txt": `${marker}_MODEL\n` };
      const script = `from pathlib import Path; files=${JSON.stringify(files)}; [Path(name).write_text(content) for name,content in files.items()]`;
      const command = `python3 -c '${script.replaceAll("'", "'\\''")}' && python3 -m unittest -v test_app.py`;
      const projectEvents = await submit(sessionKey,
        `Run this exact command using the bash tool to create the small project and test it: ${command}\nDo not rewrite the command or use other tools for this step. The test must exit 0.`);
      assert.ok(projectEvents.some((event) => event.type === "tool_call_finished" && event.toolName === "bash" && event.ok), "Project tests must run successfully through bash");
      const turnEvents = await submit(sessionKey,
        "Call send_attachment now with file_path=result.txt. The file already exists. Do not edit it, describe a download, or paste its contents. After the tool succeeds, reply DONE and finish this turn.");
      const workspace = join(sessionsRoot, Buffer.from(sessionKey).toString("base64url"), "workspace");
      assert.equal((await readFile(join(workspace, "result.txt"), "utf8")).trim(), `${marker}_MODEL`);
      const attachments = turnEvents.filter((event) => event.type === "assistant_attachment");
      assert.ok(attachments.length > 0, "Model must deliver the artifact through send_attachment");
      const attachment = attachments.find((event) => event.attachment.path?.endsWith("result.txt"));
      assert.ok(attachment?.attachment.path, "No result.txt attachment delivered");
      assert.equal((await readFile(attachment.attachment.path, "utf8")).trim(), `${marker}_MODEL`);
      cases.push({ caseId: `MODEL-${marker}`, status: "PASS", sessionKey, sandboxKey: Buffer.from(sessionKey).toString("base64url"), generation: 1,
        request: { command, artifact: "result.txt" }, response: { artifact: attachment.attachment, projectEvents, turnEvents }, hostObservation: { marker: `${marker}_MODEL` },
        startedAt, finishedAt: new Date().toISOString(), exitCode: 0, failureReason: null });
      return workspace;
    }));
    const failed = outcomes.filter((outcome): outcome is PromiseRejectedResult => outcome.status === "rejected");
    if (failed.length) throw new AggregateError(failed.map((outcome) => outcome.reason), "Concurrent model project smoke failed");
    const runs = outcomes.map((outcome) => (outcome as PromiseFulfilledResult<string>).value);
    const resumeStartedAt = new Date().toISOString();
    await local.gateway.closeSession({ sessionKey: "model-A", reason: "acceptance resume" });
    const resumed = await submit("model-A", "Use read_file to read result.txt, then run the existing Python unittest with bash: python3 -m unittest -v test_app.py. Preserve existing files.");
    assert.equal((await readFile(join(runs[0], "result.txt"), "utf8")).trim(), "A_MODEL");
    assert.ok(resumed.some((event) => event.type === "tool_call_finished" && event.toolName === "read_file" && event.ok), JSON.stringify(resumed));
    assert.ok(resumed.some((event) => event.type === "tool_call_finished" && event.toolName === "bash" && event.ok), JSON.stringify(resumed));
    const resumedArtifact = await submit("model-A", "Call send_attachment now with file_path=result.txt. The file already exists. Do not edit it, describe a download, or paste its contents. After the tool succeeds, reply DONE and finish this turn.");
    const delivered = resumedArtifact.find((event) => event.type === "assistant_attachment");
    assert.ok(delivered?.attachment.path, JSON.stringify(resumedArtifact));
    assert.equal((await readFile(delivered.attachment.path, "utf8")).trim(), "A_MODEL");
    assert.equal(Number(await readFile(join(sessionsRoot, Buffer.from("model-A").toString("base64url"), ".pilotdeck", "control", "generation"), "utf8")), 2);
    cases.push({ caseId: "MODEL-RESUME", status: "PASS", sessionKey: "model-A", sandboxKey: Buffer.from("model-A").toString("base64url"), generation: 2,
      request: { operation: "close/resume/read/test/deliver", artifact: "result.txt" }, response: { resumed, resumedArtifact }, hostObservation: { generation: 2, artifact: "A_MODEL" },
      startedAt: resumeStartedAt, finishedAt: new Date().toISOString(), exitCode: 0, failureReason: null });
  } catch (error) {
    failure = error;
    throw error;
  } finally {
    await local.dispose();
    const artifactDir = process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR;
    if (artifactDir) {
      await mkdir(artifactDir, { recursive: true });
      await writeFile(join(artifactDir, "model-smoke.json"), JSON.stringify({
        status: !failure && cases.length === 3 ? "PASS" : "FAIL", model, modelUrl, cases, events,
        failureReason: failure instanceof Error ? failure.message : failure ? String(failure) : undefined,
      }, null, 2));
    }
    await rm(root, { recursive: true, force: true });
  }
});
