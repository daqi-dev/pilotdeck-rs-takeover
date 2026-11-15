import { copyFile, mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { createWriteStream } from "node:fs";
import { join, resolve } from "node:path";
import { spawn, spawnSync } from "node:child_process";

const runId = new Date().toISOString().replaceAll(/[:.]/g, "-");
const artifactDir = resolve(process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR ?? join("artifacts", "session-sandbox", runId));
await mkdir(resolve(artifactDir, ".."), { recursive: true });
// A run must never consume evidence left by an earlier invocation.
await mkdir(artifactDir);
const testEnv = { ...process.env, PILOTDECK_ACCEPTANCE_ARTIFACT_DIR: artifactDir };
delete testEnv.NODE_OPTIONS;
const nsjailBin = process.env.PILOTDECK_NSJAIL_BIN ?? "nsjail";
const environment = {
  runId, cwd: process.cwd(), commit: process.env.PILOTDECK_ACCEPTANCE_COMMIT ?? command("git", ["rev-parse", "HEAD"]).stdout.trim(),
  node: process.version, platform: process.platform, arch: process.arch, nsjail: command(nsjailBin, ["--version"]),
  uname: command("uname", ["-a"]), release: await readText("/etc/os-release"),
  rootfs: process.env.PILOTDECK_NSJAIL_ROOTFS ?? null, nsjailBin,
  rootfsManifest: await readJson(join(process.env.PILOTDECK_NSJAIL_ROOTFS ?? "/nonexistent", "pilotdeck-rootfs.json")),
  uid: process.getuid?.(), gid: process.getgid?.(), status: await readText("/proc/self/status"),
  cgroup: await readText("/proc/self/cgroup"), delegatedRoot: process.env.PILOTDECK_NSJAIL_CGROUP_ROOT ?? null,
  sourceSnapshot: process.env.PILOTDECK_ACCEPTANCE_SOURCE_SNAPSHOT ? "source-snapshot.tgz" : null,
};
if (process.env.PILOTDECK_ACCEPTANCE_SOURCE_SNAPSHOT) await copyFile(process.env.PILOTDECK_ACCEPTANCE_SOURCE_SNAPSHOT, join(artifactDir, "source-snapshot.tgz"));
await json("environment.json", environment);
await json("config.json", {
  provider: "nsjail", rootfs: environment.rootfs, capacityFixture: 2,
  storage: "Each fixture creates an owned sessions root and records bindings in its evidence.",
  network: "deny by default; example.com, httpbin.org and mcp.deepwiki.com allow profiles tested separately; owned HTTP/SSE mocks use per-session .invalid origins mapped to Unix sockets, never private TCP exceptions",
  capabilities: ["filesystem", "shell", "python/pip", "node/npm", "search", "background", "MCP", "LSP", "hooks", "artifact", "subagent", "browser", "fork", "egress"],
  disabled: ["apt", "apt-get", "apt-cache", "dpkg", "dpkg-deb", "dpkg-query"],
  quota: "workspaceBytes is rejected; memory/pids use delegated cgroup; CPU uses rlimit",
  performanceGates: { p95Ratio: 1.25, throughputRatio: .8, idleRssRatio: 1.1 },
});
await snapshot("before");

const build = await run("build", "pnpm", ["build"], 600_000);
const sources = [
  "tests/tool/session-execution-provider.spec.ts", "tests/tool/sandbox-port.spec.ts", "tests/tool/nsjail-isolation.spec.ts",
  "tests/tool/subprocess-port.spec.ts",
  "tests/tool/session-boundary-matrix.spec.ts", "tests/tool/session-egress.spec.ts", "tests/tool/session-browser.spec.ts",
  "tests/tool/session-provider-hot-update.spec.ts", "tests/tool/session-worker-crash.spec.ts", "tests/tool/validation-error.spec.ts",
  "tests/tool/session-startup-validation.spec.ts", "tests/tool/session-module-boundary.spec.ts", "tests/tool/session-generation.spec.ts",
  "tests/cli/session-sandbox-gateway-e2e.spec.ts", "tests/cli/session-sandbox-crash.spec.ts", "tests/cli/session-sandbox-cancellation.spec.ts",
  "tests/model/streaming/invocationStageBarrier.spec.ts", "tests/cli/browser-use-session-mcp-spec-preparer.spec.ts",
  "tests/cli/session-tool-composition-bundle.spec.ts", "tests/cli/session-mcp-runtime-bundle.spec.ts",
  "tests/mcp/client/McpClient.spec.ts", "tests/lsp/node-stdio-provider.spec.ts", "tests/extension/command-hook-executor.spec.ts",
  "tests/cli/project-session-runtime-bundle.spec.ts", "tests/cli/project-execution-world-bundle.spec.ts", "tests/tool/execution-world-bundle.spec.ts",
  "tests/web/fork-session-storage-provider.spec.ts", "tests/web/fork-session-projection.spec.ts", "tests/web/fork-compact-snapshot.spec.ts",
];
const focused = await run("focused-tests", "pnpm", ["exec", "tsx", "--test", ...sources], 600_000);
const performanceResult = await run("performance-tests", "pnpm", ["exec", "tsx", "--test", "tests/tool/session-performance.spec.ts"], 240_000);
const model = await run("model-smoke", "pnpm", ["exec", "tsx", "--test", "tests/cli/session-sandbox-model-smoke.spec.ts"], 300_000);

const required = {
  "nsjail-isolation.json": ["ENV-READY", "ENV-CONFIG", "ENV-TOOLS", "CORE-01", "CORE-02", "ISO-HOST-ENV", "ISO-01/02/03/04", "ISO-05/06/07/IO-01/STATE-01", "ENV-NPM-01", "APT-N/A", "ENV-02", "EXT-01", "EXT-01-HELPER", "EXT-02-OWNER", "EXT-06-PLAN-ARTIFACT", "NET-01", "LIFE-02", "LIFE-STOP-DRAIN", "LIFE-05", "LIFE-RETENTION", "LIMIT-02", "LIMIT-OUTPUT"],
  "resource-limits.json": ["LIMIT-MEMORY-PIDS-CPU"],
  "gateway-e2e.json": ["GATEWAY-ISO-AB", "GATEWAY-ISO-ABC", "GATEWAY-RESUME", "EXT-03", "EXT-04", "EXT-05-COMMAND", "EXT-05-EXTENSION", "EXT-06-DELIVERY", "EXT-06-INPUT-IMPORT", "EXT-07", "EXT-10"],
  "boundary-matrix.json": ["ISO-MATRIX-a", "ISO-MATRIX-b", "ISO-RACE-a", "ISO-RACE-b", "ISO-OS-NET-a", "ISO-OS-NET-b"],
  "egress.json": ["EXT-09", "EXT-05-HTTP", "NET-REDIRECT", "NET-PROXY-ENV", "EXT-03-streamable_http", "EXT-03-sse", "EXT-03-LOCAL-OWNERSHIP", "EXT-03-public-http"],
  "browser.json": ["EXT-08-browser-a-1", "EXT-08-browser-b-1", "EXT-08-browser-a-2", "EXT-08-EGRESS"],
  "provider-hot-update.json": ["LIFE-HOT-UPDATE"],
  "worker-crash.json": ["LIFE-WORKER-CRASH"],
  "gateway-crash.json": ["LIFE-GATEWAY-CRASH"],
  "gateway-cancellation.json": ["LIFE-GATEWAY-CANCEL", "LIFE-UNKNOWN-EFFECT"],
  "performance.json": ["LIMIT-PERFORMANCE"],
  "model-smoke.json": ["MODEL-A", "MODEL-B", "MODEL-RESUME"],
  "startup-validation.json": ["CORE-STARTUP-VALIDATION"],
  "generation-fencing.json": ["STATE-STALE-GENERATION"],
};
const evidence = {};
const cases = [];
for (const [file, ids] of Object.entries(required)) {
  const report = await readJson(join(artifactDir, file));
  evidence[file] = report ?? { status: "BLOCKED", failureReason: "Required evidence file was not produced" };
  const entries = report?.cases ?? [];
  for (const id of ids) {
    const entry = entries.find((item) => (item.caseId ?? item.id) === id);
    cases.push({
      caseId: id, status: entry?.status ?? "NOT_RUN", sessionKey: entry?.sessionKey ?? entry?.sessionKeys ?? null,
      sandboxKey: entry?.sandboxKey ?? null, generation: entry?.generation ?? null,
      request: entry?.request ?? null, response: entry?.response ?? entry?.detail ?? null,
      exitCode: entry?.exitCode ?? null, startedAt: entry?.startedAt ?? null, finishedAt: entry?.finishedAt ?? null,
      hostObservation: entry?.hostObservation ?? null,
      failureReason: entry?.failureReason ?? (!entry ? `Missing mandatory case ${id}` : report?.failureReason ?? report?.failure ?? null),
      evidenceScope: entry?.evidenceScope ?? "case",
      evidenceFile: file,
    });
  }
  for (const entry of entries) if (!ids.includes(entry.caseId ?? entry.id)) cases.push({ ...entry, caseId: entry.caseId ?? entry.id, evidenceFile: file });
}
const cgroupsAfter = environment.delegatedRoot ? await readdir(environment.delegatedRoot).catch(() => null) : null;
const cleanup = cgroupsAfter && !cgroupsAfter.some((name) => name.startsWith("session-")) ? "PASS" : "BLOCKED";
cases.push({ caseId: "LIFE-FINAL-CGROUP-CLEANUP", status: cleanup, hostObservation: { cgroupsAfter }, failureReason: cleanup === "PASS" ? null : "Delegated cgroup missing or session cgroups remain" });
await snapshot("after");
await writeFile(join(artifactDir, "case-results.jsonl"), cases.map((entry) => JSON.stringify(entry)).join("\n") + "\n");
await writeFile(join(artifactDir, "host-observations.jsonl"), Object.entries(evidence).flatMap(([file, report]) => [
  ...(report.observations ?? []).map((observation) => ({ evidenceFile: file, ...observation })),
  ...(report.cases ?? []).filter((entry) => entry.hostObservation).map((entry) => ({ caseId: entry.caseId ?? entry.id, evidenceFile: file, observation: entry.hostObservation })),
]).map((entry) => JSON.stringify(entry)).join("\n") + "\n");
const extensions = Array.from({ length: 10 }, (_, i) => {
  const id = `EXT-${String(i + 1).padStart(2, "0")}`;
  const matching = cases.filter((entry) => entry.caseId.startsWith(id));
  return { id, status: matching.length && matching.every((entry) => entry.status === "PASS") ? "PASS" : "BLOCKED", evidence: matching.map((entry) => entry.evidenceFile) };
});
await json("capabilities.json", { enabled: extensions, disabled: [{ capability: "apt/dpkg", status: cases.find((entry) => entry.caseId === "APT-N/A")?.status ?? "NOT_RUN" }] });
const checks = { build: build.status, focused: focused.status, performance: performanceResult.status, model: model.status, cleanup,
  buildOverride: process.env.PILOTDECK_BUILD_COMMAND ? "BLOCKED" : "PASS",
  linux: process.platform === "linux" && environment.rootfs ? "PASS" : "BLOCKED",
  evidence: cases.filter((entry) => entry.caseId !== "LIFE-FINAL-CGROUP-CLEANUP" && ["PASS", "N/A"].includes(entry.status))
    .every((entry) => ["sessionKey", "sandboxKey", "generation", "request", "response", "exitCode", "startedAt", "finishedAt", "hostObservation"]
      .every((key) => entry[key] !== undefined && entry[key] !== null)) ? "PASS" : "BLOCKED",
  source: environment.commit && environment.sourceSnapshot ? "PASS" : "BLOCKED",
};
const statuses = [...Object.values(checks), ...cases.map((entry) => entry.status), ...Object.values(evidence).map((report) => report.status)];
const overall = statuses.includes("FAIL") ? "FAIL" : statuses.every((status) => status === "PASS" || status === "N/A") ? "PASS" : "BLOCKED";
const summary = { runId, artifactDir, overall, conclusion: overall === "PASS" ? "完整通过" : overall === "FAIL" ? "不通过" : "受限预览通过", checks,
  mandatoryCaseCount: cases.length, counts: Object.fromEntries(["PASS", "FAIL", "BLOCKED", "NOT_RUN", "N/A"].map((status) => [status, cases.filter((entry) => entry.status === status).length])),
  commands: { build, focused, performance: performanceResult, model }, evidenceFiles: Object.keys(required), extensions,
  failedCases: cases.filter((entry) => !["PASS", "N/A"].includes(entry.status)),
};
await json("summary.json", summary);
console.log(JSON.stringify(summary, null, 2));
process.exitCode = overall === "PASS" ? 0 : 2;

async function run(name, executable, args, timeoutMs) {
  console.log(`[acceptance] ${name}: ${executable} ${args.join(" ")}`);
  const startedAt = new Date().toISOString();
  const stdout = createWriteStream(join(artifactDir, `${name}.stdout.log`));
  const stderr = createWriteStream(join(artifactDir, `${name}.stderr.log`));
  const child = spawn(executable, args, { env: testEnv, stdio: ["ignore", "pipe", "pipe"], detached: true });
  child.stdout.pipe(stdout); child.stderr.pipe(stderr);
  let error;
  let timedOut = false;
  const timeout = setTimeout(() => { timedOut = true; try { process.kill(-child.pid, "SIGKILL"); } catch {} }, timeoutMs);
  const exitCode = await new Promise((resolveExit) => {
    child.once("error", (cause) => { error = cause.message; resolveExit(null); });
    child.once("close", resolveExit);
  });
  clearTimeout(timeout);
  await Promise.all([stdout, stderr].map((stream) => stream.closed ? undefined : new Promise((done) => stream.once("close", done))));
  const status = timedOut || error ? "BLOCKED" : exitCode === 0 ? "PASS" : "FAIL";
  console.log(`[acceptance] ${name}: ${status} (exit=${exitCode})`);
  return { status, executable, args, exitCode, startedAt, finishedAt: new Date().toISOString(), timedOut, error };
}
async function json(file, value) { await writeFile(join(artifactDir, file), JSON.stringify(value, null, 2)); }
async function readJson(path) { try { return JSON.parse(await readFile(path, "utf8")); } catch { return null; } }
async function readText(path) { try { return await readFile(path, "utf8"); } catch { return null; } }
function command(executable, args) { const result = spawnSync(executable, args, { encoding: "utf8", timeout: 10_000 }); return { exitCode: result.status, stdout: result.stdout ?? "", stderr: result.stderr ?? "" }; }
async function snapshot(phase) {
  await writeFile(join(artifactDir, `processes.${phase}.txt`), command("ps", ["-eo", "pid,ppid,user,stat,cmd"]).stdout);
  await writeFile(join(artifactDir, `mountinfo.${phase}.txt`), await readText("/proc/self/mountinfo") ?? "");
  await writeFile(join(artifactDir, `cgroups.${phase}.txt`), await readText("/proc/self/cgroup") ?? "");
  if (phase === "after") for (const name of ["processes", "mountinfo", "cgroups"]) await writeFile(join(artifactDir, `${name}.txt`), await readFile(join(artifactDir, `${name}.${phase}.txt`)));
}
