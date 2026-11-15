import assert from "node:assert/strict";
import { mkdir, mkdtemp, readdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { NsjailSessionExecutionProvider } from "../../src/sandbox/nsjail/NsjailProvider.js";
import { nextSessionExecutionBinding } from "../../src/sandbox/SessionExecutionStorage.js";

test("startup rejects invalid configuration and never follows session-owned initialization symlinks", {
  skip: process.platform !== "linux" || !process.env.PILOTDECK_NSJAIL_ROOTFS,
}, async () => {
  const root = await mkdtemp(join(tmpdir(), "pd-startup-"));
  const provider = new NsjailSessionExecutionProvider({ sessionsRoot: root, rootfs: process.env.PILOTDECK_NSJAIL_ROOTFS!, executable: process.env.PILOTDECK_NSJAIL_BIN });
  const startedAt = new Date().toISOString();
  const requests: unknown[] = [];
  let failure: unknown;
  let observation: unknown;
  try {
    const a = await nextSessionExecutionBinding(root, "a", { network: "deny" });
    const b = await nextSessionExecutionBinding(root, "b", { network: "deny" });
    await mkdir(a.storage.home, { recursive: true });
    await mkdir(b.storage.home, { recursive: true });
    await symlink(b.storage.home, join(a.storage.home, ".local"));
    const before = await readdir(b.storage.home);
    requests.push(a);
    await assert.rejects(provider.createSession(a), /symlink|non-directory/);
    assert.deepEqual(await readdir(b.storage.home), before);
    await rm(join(a.storage.home, ".local"));
    for (const invalid of [
      { ...a, generation: 0 }, { ...a, generation: Number.MAX_SAFE_INTEGER + 1 },
      { ...a, sandboxKey: "../b" }, { ...a, storage: { ...a.storage, home: b.storage.home } },
      { ...a, storage: { ...a.storage, temp: a.storage.workspace } },
      { ...a, policy: { network: "deny" as const, maxMemoryBytes: -1 } },
      { ...a, policy: { network: "deny" as const, workspaceBytes: 1024 } },
    ]) { requests.push(invalid); await assert.rejects(provider.createSession(invalid)); }
    for (const invalid of [
      { sessionsRoot: root, rootfs: join(root, "missing") },
      { sessionsRoot: root, rootfs: process.env.PILOTDECK_NSJAIL_ROOTFS!, executable: join(root, "missing-nsjail") },
      { sessionsRoot: join(root, "missing-storage"), rootfs: process.env.PILOTDECK_NSJAIL_ROOTFS! },
    ]) {
      const unavailable = new NsjailSessionExecutionProvider(invalid);
      requests.push(invalid);
      assert.equal((await unavailable.probe()).ready, false);
      await unavailable.dispose();
    }
    observation = { bBefore: before, bAfter: await readdir(b.storage.home), unchanged: true };
  } catch (error) { failure = error; throw error; }
  finally {
    await provider.dispose();
    const artifacts = process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR;
    if (artifacts) { await mkdir(artifacts, { recursive: true }); await writeFile(join(artifacts, "startup-validation.json"), JSON.stringify({ status: failure ? "FAIL" : "PASS", cases: [{ caseId: "CORE-STARTUP-VALIDATION", status: failure ? "FAIL" : "PASS", sessionKey: "a", sandboxKey: "YQ", generation: 1,
      request: requests, response: { allRejected: !failure }, hostObservation: observation, exitCode: failure ? 1 : 0, startedAt, finishedAt: new Date().toISOString(), failureReason: failure ? String(failure) : null }] }, null, 2)); }
    await rm(root, { recursive: true, force: true });
  }
});
