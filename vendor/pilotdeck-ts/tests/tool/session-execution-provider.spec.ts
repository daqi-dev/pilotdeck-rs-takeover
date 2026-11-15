import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  SessionExecutionProviderError,
  SessionExecutionProviderRegistry,
  type SessionExecutionHandle,
  type SessionExecutionProvider,
  type TrustedSessionBinding,
} from "../../src/sandbox/SessionExecutionProvider.js";
import { SessionExecutionLease } from "../../src/sandbox/SessionExecutionLease.js";
import { createNsjailSandboxModule, NsjailSessionExecutionProvider } from "../../src/sandbox/nsjail/index.js";
import { createNodeFsPort } from "../../src/tool/execution-world/NodeFsPort.js";
import { createNodeSandboxedFsPort } from "../../src/tool/execution-world/SandboxedFsPort.js";

function binding(sandboxKey = "sandbox-a"): TrustedSessionBinding {
  return {
    sessionKey: sandboxKey,
    sandboxKey,
    generation: 1,
    storage: {
      workspace: `/var/lib/pilotdeck/sessions/${sandboxKey}/workspace`,
      home: `/var/lib/pilotdeck/sessions/${sandboxKey}/home`,
      temp: `/var/lib/pilotdeck/sessions/${sandboxKey}/tmp`,
    },
    policy: { network: "deny" },
  };
}

function fakeProvider(): SessionExecutionProvider & { creates: number; disposes: number } {
  const provider = {
    id: "fake",
    contractVersion: 1 as const,
    creates: 0,
    disposes: 0,
    async probe() { return { ready: true, providerId: "fake" }; },
    async createSession(): Promise<SessionExecutionHandle> {
      provider.creates += 1;
      return {
        sandboxKey: "sandbox-a",
        generation: 1,
        guestCwd: "/workspace",
        world: {} as SessionExecutionHandle["world"],
        async stop() {},
        async dispose() { provider.disposes += 1; },
      };
    },
    async dispose() {},
  };
  return provider;
}

test("provider registry rejects duplicate ids and disposes providers once", async () => {
  const registry = new SessionExecutionProviderRegistry();
  const provider = fakeProvider();
  registry.register(provider);
  assert.throws(() => registry.register(provider), (error: unknown) =>
    error instanceof SessionExecutionProviderError && error.code === "provider_duplicate");
  await registry.dispose();
  await registry.dispose();
  assert.equal(registry.list().length, 0);
});

test("nsjail integration is exposed as an opt-in module", async () => {
  const module = createNsjailSandboxModule({
    rootfs: "/opt/pilotdeck/rootfs",
    sessionsRoot: "/var/lib/pilotdeck/sessions",
    probe: false,
  });
  assert.equal(module.provider.id, "nsjail");
  assert.equal(module.sessionExecutionStorageRoot, "/var/lib/pilotdeck/sessions");
  await module.dispose();
});

test("session lease coalesces concurrent acquire and releases one handle", async () => {
  const provider = fakeProvider();
  const lease = new SessionExecutionLease(provider, binding());
  const [a, b] = await Promise.all([lease.acquire(), lease.acquire()]);
  assert.equal(a, b);
  assert.equal(provider.creates, 1);
  await lease.release();
  await lease.release();
  assert.equal(provider.disposes, 1);
  await assert.rejects(lease.acquire(), (error: unknown) =>
    error instanceof SessionExecutionProviderError && error.code === "session_closed");
});

test("nsjail provider emits a fixed isolated command shape", () => {
  const provider = new NsjailSessionExecutionProvider({
    rootfs: "/opt/pilotdeck/rootfs",
    sessionsRoot: "/var/lib/pilotdeck/sessions",
    probe: false,
  });
  const command = provider.buildCommand({
    executable: "/bin/sh",
    args: ["-c", "id"],
    cwd: "/var/lib/pilotdeck/sessions/a/workspace",
    env: { PATH: "/usr/bin" },
  }, "/usr/bin/nsjail");
  assert.equal(command.executable, "/usr/bin/nsjail");
  assert.equal(command.args.includes("--clone_newpid"), false);
  assert.equal(command.args.includes("--clone_newnet"), false);
  assert.ok(command.args.includes("--proc_path"));
  assert.equal(command.args.includes("--proc_rw"), false);
  assert.ok(command.args.includes("/dev/null:/dev/null"));
  assert.ok(command.args.includes("--tmpfsmount"));
  assert.ok(command.args.includes("/workspace"));
  assert.ok(command.args.includes("HOME=/home/agent"));
  assert.ok(command.args.includes("PATH=/home/agent/.local/npm/bin:/home/agent/.local/bin:/usr/bin:/usr/local/bin:/bin"));
  assert.equal(command.args.at(-3), "/bin/sh");
  assert.throws(() => provider.buildCommand({ executable: "/bin/true", args: [], cwd: "/workspace", env: {} }, "/usr/bin/nsjail", "allow"),
    /controlled session egress/);

  const sessionCommand = provider.buildCommand({
    executable: "/bin/sh",
    args: ["-c", "env"],
    cwd: "/var/lib/pilotdeck/sessions/a/workspace",
    env: { PATH: "/usr/bin" },
  }, "/usr/bin/nsjail", "deny", {
    workspace: "/var/lib/pilotdeck/sessions/a/workspace",
    home: "/var/lib/pilotdeck/sessions/a/home",
    temp: "/var/lib/pilotdeck/sessions/a/tmp",
  });
  assert.ok(sessionCommand.args.includes("/var/lib/pilotdeck/sessions/a/home:/home/agent"));
  assert.ok(sessionCommand.args.includes("/var/lib/pilotdeck/sessions/a/tmp:/tmp"));
  assert.equal(sessionCommand.args.includes("--tmpfsmount"), false);
  assert.equal(sessionCommand.env.HOME, "/home/agent");
  assert.equal(sessionCommand.env.TMPDIR, "/tmp");
  assert.equal(sessionCommand.env.PYTHONUSERBASE, "/home/agent/.local");
  assert.equal(sessionCommand.env.PIP_CACHE_DIR, "/home/agent/.cache/pip");
  assert.equal(sessionCommand.env.NPM_CONFIG_PREFIX, "/home/agent/.local/npm");
  assert.equal(sessionCommand.env.NPM_CONFIG_CACHE, "/home/agent/.cache/npm");
  assert.equal(sessionCommand.env.NPM_CONFIG_USERCONFIG, "/home/agent/.config/npm/npmrc");
  assert.equal(sessionCommand.env.PATH, "/home/agent/.local/npm/bin:/home/agent/.local/bin:/usr/bin:/usr/local/bin:/bin");
  const limitedCommand = provider.buildCommand({
    executable: "/bin/true",
    args: [],
    cwd: "/var/lib/pilotdeck/sessions/a/workspace",
    env: {},
  }, "/usr/bin/nsjail", "deny", undefined, {
    network: "deny",
    maxMemoryBytes: 256 * 1024 * 1024,
    maxPids: 32,
    maxCpuSeconds: 10,
  });
  assert.ok(limitedCommand.args.includes("--cgroup_mem_max"));
  assert.ok(limitedCommand.args.includes(String(256 * 1024 * 1024)));
  assert.ok(limitedCommand.args.includes("--cgroup_pids_max"));
  assert.ok(limitedCommand.args.includes("--rlimit_cpu"));
});

test("nsjail provider rejects a workspace outside its session root", async () => {
  const provider = new NsjailSessionExecutionProvider({
    rootfs: "/opt/pilotdeck/rootfs",
    sessionsRoot: "/var/lib/pilotdeck/sessions",
    probe: false,
  });
  await assert.rejects(provider.createSession({
    ...binding("sandbox-a"),
    storage: { ...binding("sandbox-a").storage, workspace: "/var/lib/pilotdeck/sessions-other/workspace" },
  }), (error: unknown) => error instanceof SessionExecutionProviderError && error.code === "session_conflict");
});

test("nsjail provider rejects a home or temp path outside its session root", async () => {
  const provider = new NsjailSessionExecutionProvider({
    rootfs: "/opt/pilotdeck/rootfs",
    sessionsRoot: "/var/lib/pilotdeck/sessions",
    probe: false,
  });
  await assert.rejects(provider.createSession({
    ...binding("sandbox-a"),
    storage: { ...binding("sandbox-a").storage, home: "/var/lib/pilotdeck/sessions-other/home" },
  }), (error: unknown) => error instanceof SessionExecutionProviderError && error.code === "session_conflict");
});

test("nsjail rejects disk limits it cannot enforce with a plain bind mount", async () => {
  const root = mkdtempSync(join(tmpdir(), "pilotdeck-nsjail-quota-"));
  const provider = new NsjailSessionExecutionProvider({
    rootfs: join(root, "rootfs"),
    sessionsRoot: join(root, "sessions"),
    probe: false,
  });
  try {
    await assert.rejects(provider.createSession({
      ...binding("sandbox-a"),
      storage: {
        workspace: join(root, "sessions", "sandbox-a", "workspace"),
        home: join(root, "sessions", "sandbox-a", "home"),
        temp: join(root, "sessions", "sandbox-a", "tmp"),
      },
      policy: { network: "deny", workspaceBytes: 1024 },
    }), (error: unknown) => error instanceof SessionExecutionProviderError
      && error.code === "session_conflict"
      && error.message.includes("workspaceBytes"));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("nsjail enforces max active session capacity and releases it exactly once", { skip: process.platform !== "linux" }, async () => {
  const root = mkdtempSync(join(tmpdir(), "pilotdeck-nsjail-capacity-"));
  mkdirSync(join(root, "sessions"));
  const provider = new NsjailSessionExecutionProvider({
    rootfs: join(root, "rootfs"),
    sessionsRoot: join(root, "sessions"),
    probe: false,
    maxActiveSessions: 1,
    worldFactory: () => ({ dispose: async () => {} } as SessionExecutionHandle["world"]),
  });
  try {
    const a = await provider.createSession({
      ...binding("sandbox-a"),
      storage: {
        workspace: join(root, "sessions", "sandbox-a", "workspace"),
        home: join(root, "sessions", "sandbox-a", "home"),
        temp: join(root, "sessions", "sandbox-a", "tmp"),
      },
    });
    await assert.rejects(provider.createSession({
      ...binding("sandbox-b"),
      storage: {
        workspace: join(root, "sessions", "sandbox-b", "workspace"),
        home: join(root, "sessions", "sandbox-b", "home"),
        temp: join(root, "sessions", "sandbox-b", "tmp"),
      },
    }), (error: unknown) => error instanceof SessionExecutionProviderError && error.code === "capacity_exceeded");
    await a.dispose();
    await a.dispose();
    const b = await provider.createSession({
      ...binding("sandbox-b"),
      storage: {
        workspace: join(root, "sessions", "sandbox-b", "workspace"),
        home: join(root, "sessions", "sandbox-b", "home"),
        temp: join(root, "sessions", "sandbox-b", "tmp"),
      },
    });
    await b.dispose();
  } finally {
    await provider.dispose();
    rmSync(root, { recursive: true, force: true });
  }
});

test("nsjail keeps process isolation when the tool policy is danger-full-access", { skip: process.platform !== "linux" }, async () => {
  const root = mkdtempSync(join(tmpdir(), "pilotdeck-nsjail-force-sandbox-"));
  mkdirSync(join(root, "sessions"));
  const seen: { forceSandbox?: boolean } = {};
  try {
    const provider = new NsjailSessionExecutionProvider({
      rootfs: join(root, "rootfs"),
      sessionsRoot: join(root, "sessions"),
      probe: false,
      sandboxMode: "danger-full-access",
      worldFactory: (options) => {
        seen.forceSandbox = options.forceSandbox;
        return {} as SessionExecutionHandle["world"];
      },
    });
    await provider.createSession({
      ...binding("sandbox-a"),
      storage: {
        workspace: join(root, "sessions", "sandbox-a", "workspace"),
        home: join(root, "sessions", "sandbox-a", "home"),
        temp: join(root, "sessions", "sandbox-a", "tmp"),
      },
    });
    assert.equal(seen.forceSandbox, true);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("session filesystem provider rejects reads and writes outside the bound workspace", async () => {
  const root = mkdtempSync(join(tmpdir(), "pilotdeck-session-fs-"));
  const workspace = join(root, "workspace");
  const other = join(root, "other");
  mkdirSync(workspace);
  mkdirSync(other);
  writeFileSync(join(other, "secret.txt"), "private");
  const fs = createNodeSandboxedFsPort({
    fs: createNodeFsPort(),
    sandboxMode: "workspace-write",
    workspaceRoot: workspace,
  });
  try {
    assert.throws(() => fs.readFile(join(other, "secret.txt")), (error: unknown) => error instanceof Error && error.message.includes("outside the session workspace"));
    await assert.rejects(fs.writeText(join(other, "out.txt"), "nope"), (error: unknown) => error instanceof Error && error.message.includes("outside the session workspace"));
    await fs.writeText(join(workspace, "own.txt"), "ok");
    assert.equal(await fs.readFile(join(workspace, "own.txt"), { encoding: "utf8" }), "ok");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
