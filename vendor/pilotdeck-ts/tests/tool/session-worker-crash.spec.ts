import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { NsjailSessionExecutionProvider } from "../../src/sandbox/nsjail/NsjailProvider.js";
import { nextSessionExecutionBinding } from "../../src/sandbox/SessionExecutionStorage.js";
import { nsjailCgroupSessionKey } from "../../src/sandbox/nsjail/NsjailCgroupLease.js";

test("killing an nsjail command supervisor reaps its descendants and leaves B alive", {
  skip: process.platform !== "linux" || !process.env.PILOTDECK_NSJAIL_ROOTFS || !process.env.PILOTDECK_NSJAIL_CGROUP_ROOT,
  timeout: 30_000,
}, async () => {
  const root = await mkdtemp(join(tmpdir(), "pd-worker-crash-"));
  const provider = new NsjailSessionExecutionProvider({ sessionsRoot: root, rootfs: process.env.PILOTDECK_NSJAIL_ROOTFS!,
    executable: process.env.PILOTDECK_NSJAIL_BIN, cgroupV2Root: process.env.PILOTDECK_NSJAIL_CGROUP_ROOT });
  let failure: unknown;
  let observation: unknown;
  const startedAt = new Date().toISOString();
  try {
    const [bindingA, bindingB] = await Promise.all(["a", "b"].map((key) => nextSessionExecutionBinding(root, key, { network: "deny" })));
    const [a, b] = await Promise.all([provider.createSession(bindingA!), provider.createSession(bindingB!)]);
    const worker = await a.world.detachedShell.start({ command: "sleep 120 & printf READY > ready; wait", cwd: bindingA!.storage.workspace, env: {} });
    for (let attempt = 0; ; attempt++) {
      if (await readFile(join(bindingA!.storage.workspace, "ready"), "utf8").catch(() => "") === "READY") break;
      assert.ok(attempt < 200, "worker did not start");
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    const key = await nsjailCgroupSessionKey(dirname(bindingA!.storage.workspace), bindingA!.sandboxKey);
    const pids = await cgroupPids(join(process.env.PILOTDECK_NSJAIL_CGROUP_ROOT!, `session-${key}-1`));
    assert.ok(pids.length >= 2, JSON.stringify(pids));
    assert.ok(worker.pid);
    process.kill(worker.pid, "SIGKILL");
    await worker.exit;
    for (let attempt = 0; ; attempt++) {
      const remaining = await Promise.all(pids.map(async (pid) => {
        return readFile(`/proc/${pid}/stat`, "utf8").then(() => true, () => false);
      }));
      if (remaining.every((value) => !value)) break;
      assert.ok(attempt < 200, `worker descendants survived: ${JSON.stringify(pids)}`);
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    const output = await b.world.shell.execute({ command: "printf B_ALIVE", cwd: bindingB!.storage.workspace, env: {}, timeoutMs: 5_000 });
    assert.equal(output.stdout, "B_ALIVE");
    await a.dispose();
    await assert.rejects(a.world.shell.execute({ command: "printf OLD", cwd: bindingA!.storage.workspace, env: {}, timeoutMs: 5_000 }), /closed/);
    observation = { supervisorPid: worker.pid, descendants: pids, aliveDescendants: [], b: output };
  } catch (error) { failure = error; throw error; }
  finally {
    await provider.dispose();
    const artifacts = process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR;
    if (artifacts) { await mkdir(artifacts, { recursive: true }); await writeFile(join(artifacts, "worker-crash.json"), JSON.stringify({ status: failure ? "FAIL" : "PASS", cases: [{ caseId: "LIFE-WORKER-CRASH", status: failure ? "FAIL" : "PASS", sessionKey: "a", sandboxKey: "YQ", generation: 1,
      request: { operation: "SIGKILL command supervisor with sleep child" }, response: observation, hostObservation: observation, exitCode: failure ? 1 : 0,
      startedAt, finishedAt: new Date().toISOString(), failureReason: failure ? String(failure) : null }], failureReason: failure ? String(failure) : null }, null, 2)); }
    await rm(root, { recursive: true, force: true });
  }
});

async function cgroupPids(root: string): Promise<number[]> {
  const pids = (await readFile(join(root, "cgroup.procs"), "utf8")).trim().split(/\s+/).filter(Boolean).map(Number);
  for (const entry of await readdir(root, { withFileTypes: true })) if (entry.isDirectory()) pids.push(...await cgroupPids(join(root, entry.name)));
  return pids;
}
