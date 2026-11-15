import { randomBytes } from "node:crypto";
import { openSync, closeSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { isAbsolute, join, relative, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import type { FsPort } from "../../tool/execution-world/FsPort.js";
import type { ExecutionWorkspacePort } from "../../tool/execution-world/ExecutionWorkspacePort.js";
import type { ExecutionTransportPort, ExecutionRpcTransport } from "../../tool/execution-world/ExecutionTransportPort.js";
import type { PlanStoragePort } from "../../tool/execution-world/PlanStoragePort.js";
import type { SandboxedCommand } from "../../tool/execution-world/SandboxPort.js";

/** Fence both dispatch and completion so closed generations cannot deliver late results. */
export function fenceNsjailSessionPort<T extends object>(port: T, assertActive: () => void): T {
  return new Proxy(port, {
    get(target, property) {
      const member = Reflect.get(target, property, target);
      if (typeof member !== "function") return member;
      if (property === "dispose") return member.bind(target);
      return (...args: unknown[]) => {
        try { assertActive(); } catch (error) { return Promise.reject(error); }
        const result = member.apply(target, args);
        if (result && typeof result.then === "function") return result.then((value: unknown) => { assertActive(); return value; });
        assertActive();
        return result;
      };
    },
  });
}

export function createNsjailExecutionWorkspacePort(fs: FsPort, tempRoot: string, assertActive: () => void): ExecutionWorkspacePort {
  return {
    async create(options = {}) {
      assertActive();
      options.signal?.throwIfAborted();
      const prefix = options.prefix ?? "execution_";
      if (!/^[A-Za-z0-9_-]+$/.test(prefix)) throw new Error("Invalid execution workspace prefix");
      const root = await mkdtemp(join(tempRoot, prefix));
      let closed = false;
      return {
        root,
        async writeText(path, content) {
          assertActive();
          const target = resolve(root, path);
          const child = relative(root, target);
          if (closed || isAbsolute(path) || child === ".." || child.startsWith("../")) throw new Error("Execution workspace path escapes its root or is closed");
          await fs.writeText(target, content, { allowOverwrite: true });
          return target;
        },
        async cleanup() {
          closed = true;
          await rm(root, { recursive: true, force: true });
        },
      };
    },
  };
}

export function createNsjailExecutionTransportPort(tempRoot: string, assertActive: () => void): ExecutionTransportPort & { dispose(): Promise<void> } {
  type Allocation = { fd: number; path: string; closed: boolean };
  const allocations = new WeakMap<ExecutionRpcTransport, Allocation>();
  const active = new Set<Allocation>();
  const release = async (allocation: Allocation) => {
    if (allocation.closed) return;
    allocation.closed = true;
    active.delete(allocation);
    try { await rm(allocation.path, { force: true }); }
    finally { closeSync(allocation.fd); }
  };
  return {
    create() {
      assertActive();
      const name = `r-${randomBytes(6).toString("hex")}`;
      const fd = openSync(tempRoot, "r");
      const transport: ExecutionRpcTransport = { kind: "uds", socketPath: `/proc/self/fd/${fd}/${name}`, guestSocketPath: join("/tmp", name) };
      const allocation = { fd, path: join(tempRoot, name), closed: false };
      allocations.set(transport, allocation);
      active.add(allocation);
      return transport;
    },
    async cleanup(transport) {
      const allocation = allocations.get(transport);
      if (!allocation) throw new Error("Invalid session RPC transport");
      await release(allocation);
    },
    async dispose() { await Promise.all([...active].map(release)); },
  };
}

/** Plan consumers are synchronous; their filesystem operation still executes in nsjail. */
export function createNsjailPlanStoragePort(
  workspace: string,
  prepare: (command: SandboxedCommand) => SandboxedCommand,
  assertActive: () => void,
): PlanStoragePort {
  const invoke = (operation: "mkdir" | "read", target: string) => {
    assertActive();
    const child = relative(workspace, resolve(target));
    if (child === ".." || child.startsWith("../") || isAbsolute(child)) throw new Error("Plan path is outside the session workspace");
    const command = prepare({ executable: "node", args: ["-e", "const fs=require('fs'); const r=JSON.parse(fs.readFileSync(0,'utf8')); if(r.op==='mkdir')fs.mkdirSync(r.path,{recursive:true});else process.stdout.write(fs.readFileSync(r.path,'utf8'));"], cwd: workspace, env: { PATH: "/usr/local/bin:/usr/bin:/bin" } });
    const result = spawnSync(command.executable, command.args, {
      cwd: command.cwd, env: command.env, encoding: "utf8", timeout: 30_000, maxBuffer: 4 * 1024 * 1024,
      input: JSON.stringify({ op: operation, path: join("/workspace", child) }),
    });
    if (result.status !== 0 || result.error) throw new Error(`Sandbox plan operation failed: ${result.error ?? result.stderr}`);
    return result.stdout;
  };
  return { ensureDirectory: (path) => { invoke("mkdir", path); }, readText: (path) => invoke("read", path) };
}
