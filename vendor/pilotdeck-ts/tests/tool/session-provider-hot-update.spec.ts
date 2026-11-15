import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { SessionExecutionProviderHost } from "../../src/sandbox/SessionExecutionProviderHost.js";
import { nextSessionExecutionBinding } from "../../src/sandbox/SessionExecutionStorage.js";
import { NsjailSessionExecutionProvider } from "../../src/sandbox/nsjail/NsjailProvider.js";
import type { SessionExecutionHandle, SessionExecutionProvider } from "../../src/sandbox/SessionExecutionProvider.js";

test("host releases a synchronous startup failure and snapshots the binding", async () => {
  let attempts = 0;
  let released = 0;
  const provider = {
    id: "fixture", contractVersion: 1, probe: async () => ({ ready: true }), dispose: async () => {},
    createSession(binding) {
      assert.equal(binding.sandboxKey, "YQ");
      assert.equal(binding.storage.workspace, "/sessions/YQ/workspace");
      if (++attempts === 1) throw new Error("startup failed");
      return Promise.resolve({ sandboxKey: "YQ", generation: 1, dispose: async () => { released++; } } as SessionExecutionHandle);
    },
  } as SessionExecutionProvider;
  const host = new SessionExecutionProviderHost(provider, { network: "deny" });
  const binding = { sessionKey: "a", sandboxKey: "YQ", generation: 1,
    storage: { workspace: "/sessions/YQ/workspace", home: "/sessions/YQ/home", temp: "/sessions/YQ/tmp" }, policy: { network: "deny" as const } };
  await assert.rejects(host.createSession(binding), /startup failed/);
  const pending = host.createSession(binding);
  binding.sandboxKey = "forged";
  binding.storage.workspace = "/sessions/forged/workspace";
  const handle = await pending;
  await Promise.all([handle.dispose(), handle.dispose()]);
  assert.equal(released, 1);
  await host.dispose();
});

test("host provider/profile update preserves active A and applies the new provider to B", {
  skip: process.platform !== "linux" || !process.env.PILOTDECK_NSJAIL_ROOTFS,
}, async () => {
  const root = await mkdtemp(join(tmpdir(), "pd-hot-update-"));
  const options = { sessionsRoot: root, rootfs: process.env.PILOTDECK_NSJAIL_ROOTFS!, executable: process.env.PILOTDECK_NSJAIL_BIN,
    cgroupV2Root: process.env.PILOTDECK_NSJAIL_CGROUP_ROOT, egressAllowlist: [{ hostname: "example.com", ports: [443] }] };
  const first = new NsjailSessionExecutionProvider(options);
  const second = new NsjailSessionExecutionProvider(options);
  const host = new SessionExecutionProviderHost(first, { network: "deny" });
  let failure: unknown;
  let observation: unknown;
  const startedAt = new Date().toISOString();
  try {
    const bindingA = await nextSessionExecutionBinding(root, "a", { network: "deny" });
    const a = await host.createSession(bindingA);
    host.replace(second, { network: "allow" });
    await assert.rejects(host.createSession(bindingA), /already active/);
    const bindingB = await nextSessionExecutionBinding(root, "b", { network: "deny" });
    const b = await host.createSession(bindingB);
    await assert.rejects(a.world.network!.fetch("https://example.com"), /denied/);
    const response = await b.world.network!.fetch("https://example.com");
    assert.equal(response.status, 200);
    await response.body?.cancel();
    const output = await a.world.shell.execute({ command: "printf A_STILL_DENY", cwd: bindingA.storage.workspace, env: {}, timeoutMs: 5_000 });
    assert.equal(output.stdout, "A_STILL_DENY");
    await Promise.all([a.dispose(), a.dispose(), b.dispose()]);
    await host.dispose();
    await host.dispose();
    observation = { a: "original provider, deny", b: "replacement provider, allow", aAlive: output.stdout };
  } catch (error) { failure = error; throw error; }
  finally {
    await host.dispose();
    const artifacts = process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR;
    if (artifacts) { await mkdir(artifacts, { recursive: true }); await writeFile(join(artifacts, "provider-hot-update.json"), JSON.stringify({ status: failure ? "FAIL" : "PASS", cases: [{ caseId: "LIFE-HOT-UPDATE", status: failure ? "FAIL" : "PASS", sessionKey: ["a", "b"], sandboxKey: ["YQ", "Yg"], generation: [1, 1],
      request: { aPolicy: "deny", replacementPolicy: "allow", target: "https://example.com" }, response: observation, hostObservation: observation,
      startedAt, finishedAt: new Date().toISOString(), exitCode: failure ? 1 : 0, failureReason: failure ? String(failure) : null }], failureReason: failure ? String(failure) : null }, null, 2)); }
    await rm(root, { recursive: true, force: true });
  }
});
