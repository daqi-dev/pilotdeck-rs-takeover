import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { bindNsjailBackgroundTasks } from "../../src/sandbox/nsjail/NsjailBackgroundTasks.js";
import type { BackgroundTaskRuntime } from "../../src/task/runtime/BackgroundTaskRuntime.js";
import type { BackgroundTaskCompletionEvent, BackgroundTaskCompletionHandler } from "../../src/task/runtime/BackgroundTaskCompletionEvents.js";
import { fenceNsjailSessionPort } from "../../src/sandbox/nsjail/NsjailSessionPorts.js";

test("session ports reject a successful response that arrives after generation closure", async () => {
  let active = true;
  let finish!: (value: string) => void;
  const pending = new Promise<string>((resolve) => { finish = resolve; });
  let disposed = false;
  const port = fenceNsjailSessionPort({ read: () => pending, sync: () => "value", dispose: () => { disposed = true; } }, () => {
    if (!active) throw new Error("generation closed");
  });
  assert.equal(port.sync(), "value");
  const result = port.read();
  const rejected = assert.rejects(result, /generation closed/);
  active = false;
  finish("OLD_FILE_OR_PROCESS_RESPONSE");
  await rejected;
  await assert.rejects(Promise.resolve(port.sync()), /generation closed/);
  port.dispose();
  assert.equal(disposed, true);
});

test("old generation responses and completion events are fenced after lease closure", async () => {
  const startedAt = new Date().toISOString();
  let active = true;
  let emit: BackgroundTaskCompletionHandler | undefined;
  let resolveWait!: (value: unknown) => void;
  const waiting = new Promise((resolve) => { resolveWait = resolve; });
  const runtime = {
    start: async () => ({ taskId: "generation-1-task", sessionId: "a" }),
    wait: () => waiting,
    subscribeCompletionEvents: (handler: BackgroundTaskCompletionHandler) => { emit = handler; return () => {}; },
  } as unknown as BackgroundTaskRuntime;
  const binding = { sessionKey: "a", sandboxKey: "YQ", generation: 1, storage: { workspace: "/sessions/YQ/workspace", home: "/sessions/YQ/home", temp: "/sessions/YQ/tmp" }, policy: { network: "deny" as const } };
  const assertActive = () => { if (!active) throw new Error("generation closed"); };
  const tasks = bindNsjailBackgroundTasks(runtime, binding, assertActive);
  const delivered: unknown[] = [];
  tasks.subscribeCompletionEvents((event) => { delivered.push(event); });
  await tasks.start({ command: "true", cwd: binding.storage.workspace, sessionId: "forged-b" });
  const event = { taskId: "generation-1-task", sessionId: "a" } as BackgroundTaskCompletionEvent;
  emit!(event);
  assert.equal(delivered.length, 1);
  const oldResponse = tasks.wait("generation-1-task");
  const rejected = assert.rejects(oldResponse, /generation closed/);
  active = false;
  resolveWait({ taskId: "generation-1-task", output: "OLD_RESPONSE" });
  await rejected;
  emit!(event);
  assert.equal(delivered.length, 1);
  const next = bindNsjailBackgroundTasks(runtime, { ...binding, generation: 2 }, () => {});
  next.subscribeCompletionEvents((value) => { delivered.push(value); });
  emit!(event);
  assert.equal(delivered.length, 1);
  const artifacts = process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR;
  if (artifacts) { await mkdir(artifacts, { recursive: true }); await writeFile(join(artifacts, "generation-fencing.json"), JSON.stringify({ status: "PASS", cases: [{ caseId: "STATE-STALE-GENERATION", status: "PASS", sessionKey: "a", sandboxKey: "YQ", generation: 2,
    request: { staleResponse: "OLD_RESPONSE", staleEvent: event, oldTaskId: event.taskId }, response: { deliveredBeforeClose: 1, deliveredAfterClose: 0, deliveredToNewGeneration: 0 }, hostObservation: { deterministicRuntimeFixture: true },
    startedAt, finishedAt: new Date().toISOString(), exitCode: 0, failureReason: null }] }, null, 2)); }
});
