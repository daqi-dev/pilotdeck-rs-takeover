import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { NsjailSessionExecutionProvider } from "../../src/sandbox/nsjail/NsjailProvider.js";
import { nextSessionExecutionBinding } from "../../src/sandbox/SessionExecutionStorage.js";

test("session performance freezes a 20-sample baseline before applying fixed gates", {
  skip: process.platform !== "linux" || !process.env.PILOTDECK_NSJAIL_ROOTFS, timeout: 180_000,
}, async () => {
  const root = await mkdtemp(join(tmpdir(), "pd-performance-"));
  const provider = new NsjailSessionExecutionProvider({ sessionsRoot: root,
    rootfs: process.env.PILOTDECK_NSJAIL_ROOTFS!, executable: process.env.PILOTDECK_NSJAIL_BIN,
    cgroupV2Root: process.env.PILOTDECK_NSJAIL_CGROUP_ROOT,
  });
  const artifacts = process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR;
  const startedAt = new Date().toISOString();
  if (artifacts) await mkdir(artifacts, { recursive: true });
  let sequence = 0;
  const sample = async () => {
    const binding = await nextSessionExecutionBinding(root, `perf-${sequence++}`, { network: "deny" });
    const coldStart = performance.now();
    const handle = await provider.createSession(binding);
    const invoke = () => handle.world.shell.execute({ command: "printf PERF_OK", cwd: binding.storage.workspace, env: {}, timeoutMs: 10_000 });
    try {
      assert.equal((await invoke()).stdout, "PERF_OK");
      const coldMs = performance.now() - coldStart;
      const idleRss = process.memoryUsage().rss;
      const hotStart = performance.now();
      assert.equal((await invoke()).stdout, "PERF_OK");
      const hotMs = performance.now() - hotStart;
      const executionRss = process.memoryUsage().rss;
      const stopStart = performance.now();
      await handle.dispose();
      return { sessionKey: binding.sessionKey, sandboxKey: binding.sandboxKey, generation: binding.generation,
        coldMs, hotMs, idleRss, executionRss, stopMs: performance.now() - stopStart };
    } finally { await handle.dispose(); }
  };
  const batch = async () => { const samples = []; for (let i = 0; i < 20; i++) samples.push(await sample()); return samples; };
  let report: Record<string, unknown> = { status: "FAIL" };
  try {
    await batch();
    const baselineSamples = await batch();
    const baseline = summarize(baselineSamples);
    const frozenAt = new Date().toISOString();
    if (artifacts) await writeFile(join(artifacts, "performance-baseline.json"), JSON.stringify({ frozenAt, baseline, samples: baselineSamples }, null, 2), { flag: "wx" });
    const samples = await batch();
    const measured = summarize(samples);
    const gates = {
      coldP95: measured.coldP95Ms <= baseline.coldP95Ms * 1.25,
      hotP95: measured.hotP95Ms <= baseline.hotP95Ms * 1.25,
      throughput: measured.throughput >= baseline.throughput * 0.8,
      idleRss: measured.idleRss <= baseline.idleRss * 1.1,
    };
    report = { status: Object.values(gates).every(Boolean) ? "PASS" : "FAIL", frozenAt,
      warmupCount: 20, baselineCount: 20, sampleCount: 20, baseline, measured, gates, samples,
      measurement: "Serial provider create + first jailed shell; second jailed shell; supervisor RSS while one session is idle/after execution; dispose drains that generation. The provider has no persistent idle worker.",
      cases: [{ caseId: "LIMIT-PERFORMANCE", status: Object.values(gates).every(Boolean) ? "PASS" : "FAIL",
        sessionKey: samples.map((sample) => sample.sessionKey), sandboxKey: samples.map((sample) => sample.sandboxKey), generation: samples.map((sample) => sample.generation),
        request: { warmupCount: 20, baselineCount: 20, sampleCount: 20, coldHotP95Ratio: 1.25, throughputRatio: .8, idleRssRatio: 1.1 },
        response: { baseline, measured, gates }, hostObservation: { samples, measurement: "supervisor RSS; no persistent idle worker" },
        startedAt, finishedAt: new Date().toISOString(), exitCode: Object.values(gates).every(Boolean) ? 0 : 1,
        failureReason: Object.values(gates).every(Boolean) ? null : "Fixed performance gates failed" }],
    };
    assert.ok(Object.values(gates).every(Boolean), JSON.stringify(report));
  } catch (error) { report.failureReason = String(error); throw error; }
  finally {
    await provider.dispose();
    if (artifacts) await writeFile(join(artifacts, "performance.json"), JSON.stringify(report, null, 2));
    await rm(root, { recursive: true, force: true });
  }
});

function summarize(samples: Array<{ coldMs: number; hotMs: number; idleRss: number; executionRss: number; stopMs: number }>) {
  const percentile = (key: keyof typeof samples[number], p: number) => samples.map((sample) => sample[key]).sort((a, b) => a - b)[Math.ceil(samples.length * p) - 1]!;
  return { coldP50Ms: percentile("coldMs", .5), coldP95Ms: percentile("coldMs", .95), hotP50Ms: percentile("hotMs", .5), hotP95Ms: percentile("hotMs", .95),
    idleRss: percentile("idleRss", .95), executionRss: percentile("executionRss", .95), stopP95Ms: percentile("stopMs", .95),
    throughput: samples.length * 1000 / samples.reduce((sum, sample) => sum + sample.hotMs, 0),
  };
}
