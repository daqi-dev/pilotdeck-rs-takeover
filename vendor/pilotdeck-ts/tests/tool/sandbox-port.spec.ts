import assert from "node:assert/strict";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

import { createNodeSandboxPort } from "../../src/tool/execution-world/NodeSandboxPort.js";
import { createNodeSandboxedCodeRuntimePort } from "../../src/tool/execution-world/SandboxedCodeRuntimePort.js";
import { createNodeSandboxedSubprocessPort } from "../../src/tool/execution-world/SandboxedSubprocessPort.js";
import type { CodeRuntimePort } from "../../src/tool/execution-world/CodeRuntimePort.js";
import {
  DEFAULT_SANDBOX_MODE,
  SANDBOX_MODES,
  SandboxUnavailableError,
  isSandboxMode,
  resolveSandboxMode,
} from "../../src/tool/execution-world/SandboxPort.js";

const baseRequest = {
  executable: process.execPath,
  args: ["-e", "process.exit(0)"],
  cwd: process.cwd(),
  env: process.env,
};

test("subprocess requests cannot overwrite the prepared sandbox command", async () => {
  const calls: unknown[] = [];
  const signal = new AbortController().signal;
  const onStdout = () => {};
  const port = createNodeSandboxedSubprocessPort({
    sandbox: {
      async prepare(request) {
        return { executable: "/nsjail", args: ["--", request.executable, ...request.args], cwd: "/session", env: { HOME: "/home/agent" } };
      },
    },
    subprocess: {
      async executeFile(request) {
        calls.push(request);
        assert.equal(request.executable, "/nsjail");
        assert.equal(request.cwd, "/session");
        assert.deepEqual(request.env, { HOME: "/home/agent" });
        assert.equal(request.timeoutMs, 500);
        assert.equal(request.signal, signal);
        assert.equal(request.onStdout, onStdout);
        return { exitCode: 0, stdout: "", stderr: "", timedOut: false, durationMs: 1 };
      },
    },
    resolvePolicy: ({ workspaceRoot }) => ({ mode: "danger-full-access", workspaceRoot }),
    platform: "linux",
  });
  const options = { cwd: "/host", env: { HOME: "/host" }, timeoutMs: 500, signal, onStdout };
  await port.execute({ ...options, command: "echo ok" });
  await port.executeFile!({ ...options, executable: "python3", args: ["-c", "print(1)"], stdin: "payload" });
  assert.equal(calls.length, 2);
  assert.deepEqual((calls[1] as { args: string[] }).args, ["--", "python3", "-c", "print(1)"]);
});

test("sandbox definition owns its vocabulary and profile fallback", () => {
  assert.deepEqual(SANDBOX_MODES, ["read-only", "workspace-write", "danger-full-access"]);
  assert.equal(DEFAULT_SANDBOX_MODE, "danger-full-access");
  assert.equal(isSandboxMode("workspace-write"), true);
  assert.equal(isSandboxMode("unsupported"), false);
  assert.equal(resolveSandboxMode("read-only"), "read-only");
  assert.equal(resolveSandboxMode("unsupported"), DEFAULT_SANDBOX_MODE);
});

test("node sandbox adapter permits only explicitly unconfined commands", async () => {
  const sandbox = createNodeSandboxPort();
  const command = await sandbox.prepare({
    ...baseRequest,
    policy: { mode: "danger-full-access", workspaceRoot: process.cwd() },
  });
  assert.deepEqual(command, baseRequest);
});

test("node sandbox adapter fails closed when a confined policy is requested", async () => {
  const sandbox = createNodeSandboxPort({ platform: "linux" });
  await assert.rejects(
    sandbox.prepare({
      ...baseRequest,
      policy: { mode: "read-only", workspaceRoot: process.cwd() },
    }),
    (error: unknown) => error instanceof SandboxUnavailableError && error.code === "sandbox_unavailable",
  );
});

test("sandboxed code runtime prepares the exact executable through the provider", async () => {
  let preparedPolicy: unknown;
  let runtimeRequest: { executable: string; args: readonly string[] } | undefined;
  const runtime: CodeRuntimePort = {
    async resolveExecutable() { return "python3"; },
    async run(request: { executable: string; args: readonly string[] }) {
      runtimeRequest = request;
      return { exitCode: 0, exitSignal: null, stdout: "OK", stderr: "", timedOut: false, cancelled: false };
    },
    async dispose() {},
  };
  const wrapped = createNodeSandboxedCodeRuntimePort({
    runtime,
    sandbox: {
      async prepare(request) {
        preparedPolicy = request.policy;
        return { ...request, executable: "/nsjail", args: ["--", request.executable, ...request.args] };
      },
    },
    resolvePolicy: ({ workspaceRoot }) => ({ mode: "workspace-write", workspaceRoot }),
  });
  const result = await wrapped.run({
    executable: "python3",
    args: ["-c", "print(1)"],
    cwd: "/workspace/session-a",
    env: {},
    timeoutMs: 1_000,
    stdoutMaxBytes: 100,
    stderrMaxBytes: 100,
  });
  assert.deepEqual(preparedPolicy, { mode: "workspace-write", workspaceRoot: "/workspace/session-a" });
  assert.equal(runtimeRequest?.executable, "/nsjail");
  assert.deepEqual(runtimeRequest?.args, ["--", "python3", "-c", "print(1)"]);
  assert.equal(result.stdout, "OK");
});

test("macOS sandbox adapter wraps the exact argv in one DSH-equivalent Seatbelt profile", async () => {
  let probes = 0;
  const sandbox = createNodeSandboxPort({
    platform: "darwin",
    seatbeltExecutable: "/test/sandbox-exec",
    probeSeatbelt: () => {
      probes += 1;
      return true;
    },
  });

  const command = await sandbox.prepare({
    ...baseRequest,
    policy: { mode: "workspace-write", workspaceRoot: "/workspace", executionRoot: "/execution" },
  });
  const second = await sandbox.prepare({
    ...baseRequest,
    policy: { mode: "read-only", workspaceRoot: "/workspace" },
  });

  assert.equal(command.executable, "/test/sandbox-exec");
  const separator = command.args.indexOf("--");
  assert.equal(separator, 2);
  assert.deepEqual(command.args.slice(separator + 1), [baseRequest.executable, ...baseRequest.args]);
  assert.match(command.args[1] ?? "", /\(deny file-write\*\)/);
  assert.match(command.args[1] ?? "", /\(subpath "\/workspace"\)/);
  assert.match(command.args[1] ?? "", /\(subpath "\/execution"\)/);
  assert.equal(second.executable, "/test/sandbox-exec");
  assert.equal(probes, 1, "the native provider probes its selected backend once");
});

test("macOS Seatbelt denies read-only writes and permits workspace-write", { skip: process.platform !== "darwin" }, async (t) => {
  const root = mkdtempSync(join(tmpdir(), "pilotdeck-seatbelt-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const sandbox = createNodeSandboxPort();
  const script = "require('node:fs').writeFileSync(process.argv[1], 'sandboxed')";
  const readOnlyTarget = join(root, "read-only.txt");
  const readOnly = await sandbox.prepare({
    executable: process.execPath,
    args: ["-e", script, readOnlyTarget],
    cwd: root,
    env: process.env,
    policy: { mode: "read-only", workspaceRoot: root },
  });
  const denied = spawnSync(readOnly.executable, readOnly.args, {
    cwd: readOnly.cwd,
    env: readOnly.env,
    encoding: "utf8",
  });
  assert.notEqual(denied.status, 0);
  assert.equal(existsSync(readOnlyTarget), false);

  const writableTarget = join(root, "workspace-write.txt");
  const workspaceWrite = await sandbox.prepare({
    executable: process.execPath,
    args: ["-e", script, writableTarget],
    cwd: root,
    env: process.env,
    policy: { mode: "workspace-write", workspaceRoot: root },
  });
  const allowed = spawnSync(workspaceWrite.executable, workspaceWrite.args, {
    cwd: workspaceWrite.cwd,
    env: workspaceWrite.env,
    encoding: "utf8",
  });
  assert.equal(allowed.status, 0, allowed.stderr);
  assert.equal(existsSync(writableTarget), true);
});
