import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { nsjailCgroupSessionKey } from "../../src/sandbox/nsjail/NsjailCgroupLease.js";

test("a killed Gateway reclaims its old session processes before resuming the same storage", {
  skip: process.platform !== "linux" || !process.env.PILOTDECK_NSJAIL_ROOTFS || !process.env.PILOTDECK_NSJAIL_CGROUP_ROOT,
  timeout: 90_000,
}, async () => {
  const root = await mkdtemp(join(tmpdir(), "pd-gateway-crash-"));
  const project = join(root, "project");
  const sessions = join(root, "sessions");
  await mkdir(project); await mkdir(sessions);
  await writeFile(join(project, "pilotdeck.yaml"), "schemaVersion: 1\nagent:\n  model: test/test\n  maxContextTokens: 32768\n  maxOutputTokens: 4096\nextension:\n  builtinPluginsEnabled:\n    funasr: false\nmodel:\n  providers:\n    test:\n      protocol: openai\n      url: http://127.0.0.1:1\n      apiKey: test\n      models:\n        test:\n          capabilities:\n            supportsToolUse: true\n            maxContextTokens: 32768\n            maxOutputTokens: 4096\n");
  const sandboxKey = Buffer.from("crash-a").toString("base64url");
  const sessionRoot = join(sessions, sandboxKey);
  const cgroupRoot = process.env.PILOTDECK_NSJAIL_CGROUP_ROOT!;
  const worker = fileURLToPath(new URL(`../fixtures/session-gateway-crash-worker.${import.meta.url.endsWith(".ts") ? "ts" : "js"}`, import.meta.url));
  const children: ChildProcess[] = [];
  const logs: string[] = [];
  const start = (phase: string) => {
    const child = spawn(process.execPath, [...process.execArgv.filter((arg) => !arg.startsWith("--test")), worker, project, sessions, phase], { env: process.env, stdio: ["ignore", "pipe", "pipe", "ipc"] });
    children.push(child);
    child.stdout?.on("data", (chunk) => logs.push(String(chunk)));
    child.stderr?.on("data", (chunk) => logs.push(String(chunk)));
    const messages: unknown[] = [];
    child.on("message", (message) => messages.push(message));
    const exit = new Promise<number | null>((resolve, reject) => { child.once("error", reject); child.once("exit", resolve); });
    return { child, exit, messages };
  };
  let failure: unknown;
  let observation: unknown;
  const startedAt = new Date().toISOString();
  try {
    const first = start("crash");
    await waitUntil(async () => {
      if (first.child.exitCode !== null || first.child.signalCode !== null) throw new Error(`Gateway exited before starting the crash task: ${logs.join("")}`);
      return (await readFile(join(sessionRoot, "workspace", "crash-marker.txt"), "utf8").catch(() => "")) === "BEFORE_CRASH";
    }, 30_000);
    const cgroupKey = await nsjailCgroupSessionKey(sessionRoot, sandboxKey);
    const before = await cgroupPids(join(cgroupRoot, `session-${cgroupKey}-1`));
    assert.ok(before.length > 0);
    first.child.kill("SIGKILL");
    await first.exit;
    const second = start("resume");
    assert.equal(await second.exit, 0, logs.join(""));
    const events = JSON.stringify(second.messages);
    assert.match(events, /RECOVERED/);
    assert.match(events, /"finishReason":"completed"/);
    assert.equal(await readFile(join(sessionRoot, ".pilotdeck", "control", "generation"), "utf8"), "2\n");
    assert.equal(await readFile(join(sessionRoot, "workspace", "crash-marker.txt"), "utf8"), "BEFORE_CRASH");
    await waitUntil(async () => (await Promise.all(before.map(async (pid) => readFile(`/proc/${pid}/stat`, "utf8").then(() => false, () => true)))).every(Boolean), 5_000);
    assert.equal((await readdir(cgroupRoot)).some((name) => name.startsWith(`session-${cgroupKey}-`)), false);
    observation = { gatewayPids: [first.child.pid, second.child.pid], oldProcessPids: before, generation: 2, remainingCgroups: [] };
  } catch (error) { failure = error; throw error; }
  finally {
    for (const child of children) if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
    const artifacts = process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR;
    if (artifacts) { await mkdir(artifacts, { recursive: true }); await writeFile(join(artifacts, "gateway-crash.json"), JSON.stringify({ status: failure ? "FAIL" : "PASS", cases: [{ caseId: "LIFE-GATEWAY-CRASH", status: failure ? "FAIL" : "PASS", sessionKey: "crash-a", sandboxKey, generation: 2, hostObservation: observation,
      request: { operation: "SIGKILL Gateway; resume original storage", sessionRoot }, response: { observation, logs },
      startedAt, finishedAt: new Date().toISOString(), exitCode: failure ? 1 : 0, failureReason: failure ? String(failure) : null }], failure: failure ? String(failure) : undefined, logs }, null, 2)); }
    await rm(root, { recursive: true, force: true });
  }
});

async function cgroupPids(root: string): Promise<number[]> {
  const result = (await readFile(join(root, "cgroup.procs"), "utf8")).trim().split(/\s+/).filter(Boolean).map(Number);
  for (const entry of await readdir(root, { withFileTypes: true })) if (entry.isDirectory()) result.push(...await cgroupPids(join(root, entry.name)));
  return result;
}
async function waitUntil(check: () => Promise<boolean>, timeoutMs: number) {
  const deadline = Date.now() + timeoutMs;
  while (!await check()) {
    if (Date.now() >= deadline) throw new Error("Crash fixture observation timed out");
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}
