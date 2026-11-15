import { access, mkdir, mkdtemp, open, readFile, realpath, rename, rm, writeFile } from "node:fs/promises";
import { accessSync, constants } from "node:fs";
import { spawnSync } from "node:child_process";
import type { SandboxMode, SandboxPort, SandboxedCommand } from "../../tool/execution-world/SandboxPort.js";
import type { ExecutionWorldBundle } from "../../tool/execution-world/ExecutionWorldBundle.js";
import { createNodeExecutionWorldBundle } from "../../tool/execution-world/ExecutionWorldBundle.js";
import { createNsjailFsPort } from "./NsjailFsPort.js";
import { createNsjailAttachmentDeliveryPort } from "./NsjailAttachmentDeliveryPort.js";
import { createNsjailExecutionTransportPort, createNsjailExecutionWorkspacePort, createNsjailPlanStoragePort, fenceNsjailSessionPort } from "./NsjailSessionPorts.js";
import { acquireNsjailCgroupLease, nsjailCgroupSessionKey } from "./NsjailCgroupLease.js";
import { bindNsjailBackgroundTasks } from "./NsjailBackgroundTasks.js";
import { createDeniedNetworkPort } from "../../tool/execution-world/NetworkPort.js";
import { createSessionEgressProxy, type SessionEgressLocalService, type SessionEgressObservation, type SessionEgressRule } from "../egress/SessionEgressProxy.js";
import { basename, dirname, isAbsolute, join, relative, resolve } from "node:path";
import {
  SessionExecutionProviderError,
  type ProviderReadiness,
  type SessionExecutionHandle,
  type SessionExecutionProvider,
  type SessionIsolationPolicy,
  type TrustedSessionBinding,
} from "../SessionExecutionProvider.js";

export type NsjailProviderOptions = {
  executable?: string;
  rootfs: string;
  sessionsRoot: string;
  sandboxMode?: SandboxMode;
  probe?: boolean;
  /** Maximum number of distinct active sandbox keys held by this provider. */
  maxActiveSessions?: number;
  /** Writable cgroup v2 delegation; never use the system cgroup root. */
  cgroupV2Root?: string;
  egressAllowlist?: readonly SessionEgressRule[];
  /** Host-only, per-session service capabilities; never supplied by a guest or wire request. */
  egressLocalServices?: (binding: TrustedSessionBinding) => readonly SessionEgressLocalService[];
  onEgress?: (binding: Pick<TrustedSessionBinding, "sessionKey" | "sandboxKey" | "generation">, observation: SessionEgressObservation) => void;
  /** Host environment is private unless explicitly selected for guest inheritance. */
  inheritEnvironmentKeys?: readonly string[];
  /** Optional replacement for the native execution-world composition. */
  worldFactory?: (options: {
    projectRoot: string;
    sandboxMode: SandboxMode;
    sandboxPort: SandboxPort;
    workspaceRoot: string;
    forceSandbox: true;
    executionSignal?: AbortSignal;
    backgroundTaskStateDir?: string;
  }) => ExecutionWorldBundle;
};

type SessionStorageMounts = Required<TrustedSessionBinding["storage"]>;

/**
 * Host-side nsjail provider. It owns readiness, session binding, policy
 * validation, exact argv construction, and session execution-world creation.
 */
export class NsjailSessionExecutionProvider implements SessionExecutionProvider {
  readonly id = "nsjail";
  readonly contractVersion = 1 as const;
  private disposed = false;
  private readonly activeSessions = new Set<string>();
  private readonly handles = new Map<string, SessionExecutionHandle>();
  private readonly bindings = new Map<string, TrustedSessionBinding>();
  private readonly pendingCreates = new Set<Promise<SessionExecutionHandle>>();
  private readonly deletingSessions = new Set<string>();
  private readonly cgroups = new Map<string, Awaited<ReturnType<typeof acquireNsjailCgroupLease>>>();
  private readonly egress = new Map<string, Awaited<ReturnType<typeof createSessionEgressProxy>>>();
  private disposePromise?: Promise<void>;

  constructor(private readonly options: NsjailProviderOptions) {
    this.options = { ...options,
      inheritEnvironmentKeys: options.inheritEnvironmentKeys ? [...options.inheritEnvironmentKeys] : undefined,
      egressAllowlist: options.egressAllowlist?.map((rule) => ({ ...rule, ports: [...rule.ports], addresses: rule.addresses ? [...rule.addresses] : undefined })),
    };
    if (options.maxActiveSessions !== undefined &&
      (!Number.isSafeInteger(options.maxActiveSessions) || options.maxActiveSessions <= 0)) {
      throw new SessionExecutionProviderError("maxActiveSessions must be a positive integer", "session_conflict");
    }
  }

  async probe(): Promise<ProviderReadiness> {
    if (this.disposed) return { ready: false, providerId: this.id, reason: "provider disposed" };
    if (process.platform !== "linux") {
      return { ready: false, providerId: this.id, reason: "nsjail provider requires Linux" };
    }
    try {
      await access(this.options.rootfs, constants.R_OK | constants.X_OK);
      await access(this.options.sessionsRoot, constants.R_OK | constants.W_OK | constants.X_OK);
    } catch (error) {
      return { ready: false, providerId: this.id, reason: `storage unavailable: ${String(error)}` };
    }
    const executable = this.options.executable ?? "nsjail";
    if (!isExecutableAvailable(executable)) {
      return { ready: false, providerId: this.id, reason: `nsjail executable is unavailable: ${executable}` };
    }
    if (this.options.probe === false) return { ready: true, providerId: this.id };
    let probeRoot: string | undefined;
    try {
      probeRoot = await mkdtemp(join(this.options.sessionsRoot, ".probe-"));
      const command = this.buildCommand({
        executable: "/bin/sh",
        args: ["-c", `test -r /proc/self/status && test ! -e /proc/${process.pid}`],
        cwd: probeRoot,
        env: { PATH: "/usr/local/bin:/usr/bin:/bin" },
      }, executable);
      const result = spawnSync(command.executable, command.args, {
        cwd: command.cwd,
        env: command.env,
        stdio: "pipe",
        timeout: 10_000,
        encoding: "utf8",
      });
      if (result.status !== 0) {
        return {
          ready: false,
          providerId: this.id,
          reason: `nsjail probe failed with status ${String(result.status)}: ${String(result.stderr ?? "").trim()}`,
        };
      }
      return { ready: true, providerId: this.id, capabilities: { filesystemIsolation: true, processIsolation: true } };
    } catch (error) {
      return { ready: false, providerId: this.id, reason: `nsjail probe failed: ${String(error)}` };
    } finally {
      if (probeRoot) await rm(probeRoot, { recursive: true, force: true });
    }
  }

  createSession(binding: TrustedSessionBinding): Promise<SessionExecutionHandle> {
    binding = { ...binding, storage: { ...binding.storage }, policy: { ...binding.policy } };
    if (this.deletingSessions.has(binding.sandboxKey)) return Promise.reject(new SessionExecutionProviderError("Session storage is being deleted", "session_conflict"));
    const pending = this.createSessionHandle(binding);
    this.pendingCreates.add(pending);
    void pending.then(() => this.pendingCreates.delete(pending), () => this.pendingCreates.delete(pending));
    return pending;
  }

  private async createSessionHandle(binding: TrustedSessionBinding): Promise<SessionExecutionHandle> {
    if (this.disposed) throw new SessionExecutionProviderError("nsjail provider is disposed", "provider_unavailable");
    validateBinding(binding, this.options.sessionsRoot);
    if (process.platform !== "linux") throw new SessionExecutionProviderError("nsjail provider requires Linux", "provider_unavailable");
    if ((binding.policy.maxMemoryBytes || binding.policy.maxPids) && !this.options.cgroupV2Root) {
      throw new SessionExecutionProviderError("Memory/pids limits require a delegated cgroupV2Root", "provider_unavailable");
    }
    const alreadyActive = this.activeSessions.has(binding.sandboxKey);
    if (alreadyActive) {
      throw new SessionExecutionProviderError(`Session already active: ${binding.sandboxKey}`, "session_conflict");
    }
    if (!alreadyActive && this.options.maxActiveSessions !== undefined &&
      this.activeSessions.size >= this.options.maxActiveSessions) {
      throw new SessionExecutionProviderError(
        `Maximum active nsjail sessions reached: ${this.options.maxActiveSessions}`,
        "capacity_exceeded",
      );
    }
    // Reserve the key before async setup so concurrent creates cannot exceed capacity.
    if (!alreadyActive) this.activeSessions.add(binding.sandboxKey);
    try {
      const storage = normalizeStorage(binding.storage);
      for (const path of [...Object.values(storage), join(storage.home, ".pilotdeck"), join(storage.home, ".local", "npm"),
        join(storage.home, ".cache", "npm"), join(storage.home, ".config", "npm")]) {
        await ensureOwnedDirectory(this.options.sessionsRoot, path);
      }
      const sessionRoot = await realpath(join(this.options.sessionsRoot, binding.sandboxKey));
      for (const [name, path] of Object.entries(storage)) {
        if (!isPathWithin(await realpath(path), sessionRoot)) {
          throw new SessionExecutionProviderError(`Session ${name} resolves outside its storage root`, "session_conflict");
        }
      }
      const generationFile = join(storage.control, "generation");
      let previousGeneration = 0;
      try {
        const value = Number((await readFile(generationFile, "utf8")).trim());
        if (!Number.isSafeInteger(value) || value < 1) throw new SessionExecutionProviderError("Invalid persisted generation", "session_conflict");
        previousGeneration = value;
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      }
      if (binding.generation <= previousGeneration) {
        throw new SessionExecutionProviderError("Stale session generation", "session_conflict");
      }
      const generationStaging = `${generationFile}.next`;
      await writeFile(generationStaging, `${binding.generation}\n`, { mode: 0o600 });
      await rename(generationStaging, generationFile);
      const cgroup = this.options.cgroupV2Root
        ? await acquireNsjailCgroupLease(this.options.cgroupV2Root, await nsjailCgroupSessionKey(dirname(storage.workspace), binding.sandboxKey), binding.generation, binding.policy)
        : undefined;
      if (cgroup) this.cgroups.set(storage.workspace, cgroup);
      let stopped = false;
      const controller = new AbortController();
      if (binding.policy.network === "allow") {
        const localServices = this.options.egressLocalServices?.(binding);
        if (!this.options.egressAllowlist?.length && !localServices?.length) throw new SessionExecutionProviderError("Network allow requires a session egress allowlist", "provider_unavailable");
        const proxy = await createSessionEgressProxy({
          directory: join(storage.control, "egress"), rules: this.options.egressAllowlist ?? [], localServices,
          signal: controller.signal, observe: (observation) => this.options.onEgress?.(binding, observation),
        });
        this.egress.set(storage.workspace, proxy);
      }
      const assertActive = () => {
        if (stopped || this.disposed) {
          throw new SessionExecutionProviderError(`Session execution is closed: ${binding.sandboxKey}`, "session_closed");
        }
      };
      const innerSandbox = this.createSandboxPort(storage, binding.policy);
      const sandboxPort: SandboxPort = {
        supportsFileMtimeSort: innerSandbox.supportsFileMtimeSort,
        prepare: async (request) => {
          assertActive();
          const prepared = await innerSandbox.prepare(request);
          assertActive();
          return prepared;
        },
      };
      const sandboxMode = this.options.sandboxMode ?? "workspace-write";
      const nativeWorld = (this.options.worldFactory ?? createNodeExecutionWorldBundle)({
        projectRoot: storage.workspace,
        sandboxMode,
        sandboxPort,
        workspaceRoot: storage.workspace,
        forceSandbox: true,
        executionSignal: controller.signal,
        maxSubprocessOutputBytes: 1024 * 1024,
        backgroundTaskStateDir: join(storage.control, "background-tasks", String(binding.generation)),
      });
      const sessionFs = createNsjailFsPort(sandboxPort, storage.workspace, {
        "/home/agent": storage.home,
        "/tmp": storage.temp,
        ...Object.fromEntries(Object.entries(guestStoragePaths).map(([name, guest]) => [guest, storage[name as keyof SessionStorageMounts]])),
      }, controller.signal);
      const sessionTransport = createNsjailExecutionTransportPort(storage.temp, assertActive);
      const world = this.options.worldFactory ? nativeWorld : {
        ...nativeWorld,
        shell: fenceNsjailSessionPort(nativeWorld.shell, assertActive),
        subprocess: fenceNsjailSessionPort(nativeWorld.subprocess, assertActive),
        detachedShell: fenceNsjailSessionPort(nativeWorld.detachedShell, assertActive),
        codeRuntime: fenceNsjailSessionPort(nativeWorld.codeRuntime, assertActive),
        backgroundTasks: bindNsjailBackgroundTasks(nativeWorld.backgroundTasks, binding, assertActive),
        network: this.egress.get(storage.workspace)?.network ?? createDeniedNetworkPort(),
        contextStorage: {
          fileHistoryFs: fenceNsjailSessionPort(sessionFs.fileHistoryFs, assertActive),
          fileHistoryRoot: join(storage.home, ".pilotdeck", "history"),
          instructionStorage: fenceNsjailSessionPort(sessionFs.instructionStorage, assertActive),
          toolResultSpill: fenceNsjailSessionPort(sessionFs.toolResultSpill, assertActive),
          spillRoot: storage.spill,
        },
        fs: fenceNsjailSessionPort(sessionFs, assertActive),
        attachmentDelivery: fenceNsjailSessionPort(createNsjailAttachmentDeliveryPort(sessionFs, join(storage.control, "exports"), join(storage.artifact, "imports")), assertActive),
        executionWorkspace: createNsjailExecutionWorkspacePort(sessionFs, storage.temp, assertActive),
        executionTransport: sessionTransport,
        async dispose() {
          try { await nativeWorld.dispose(); }
          finally { await sessionTransport.dispose(); }
        },
        planStorage: createNsjailPlanStoragePort(storage.workspace, (command) => this.buildCommand({
          ...command, executable: resolveGuestExecutable(command.executable, command.env.PATH, this.options.rootfs, storage),
        }, this.options.executable ?? "nsjail", binding.policy.network, storage, binding.policy), assertActive),
      };
      if (this.disposed) {
        stopped = true;
        controller.abort();
        await world.dispose();
        throw new SessionExecutionProviderError("nsjail provider is disposed", "provider_unavailable");
      }
      let disposePromise: Promise<void> | undefined;
      let stopPromise: Promise<void> | undefined;
      const stop = () => {
        stopped = true;
        controller.abort();
        return stopPromise ??= (async () => {
          try { await cgroup?.kill(); }
          finally {
            try { await world.dispose(); }
            finally { await this.egress.get(storage.workspace)?.dispose(); }
          }
        })();
      };
      const handle: SessionExecutionHandle = {
        sandboxKey: binding.sandboxKey,
        generation: binding.generation,
        guestCwd: "/workspace",
        hostWorkspaceRoot: storage.workspace,
        hostStorageRoot: dirname(storage.workspace),
        prepareSubprocess: (request) => sandboxPort.prepare({
          executable: request.executable,
          args: request.args,
          cwd: request.cwd,
          env: request.env,
          policy: {
            mode: sandboxMode,
            workspaceRoot: storage.workspace,
            ...request.policy,
            network: binding.policy.network,
          },
        }),
        world,
        stop,
        dispose: () => {
          disposePromise ??= stop().finally(() => cgroup?.release()).finally(() => {
            this.activeSessions.delete(binding.sandboxKey);
            this.handles.delete(binding.sandboxKey);
            this.cgroups.delete(storage.workspace);
            this.egress.delete(storage.workspace);
          });
          return disposePromise;
        },
      };
      this.handles.set(binding.sandboxKey, handle);
      this.bindings.set(binding.sandboxKey, { ...binding, storage, policy: { ...binding.policy } });
      return handle;
    } catch (error) {
      if (!alreadyActive) this.activeSessions.delete(binding.sandboxKey);
      const workspace = resolve(binding.storage.workspace);
      await this.egress.get(workspace)?.dispose();
      this.egress.delete(workspace);
      const cgroup = this.cgroups.get(workspace);
      if (cgroup) {
        await cgroup.kill();
        await cgroup.release();
        this.cgroups.delete(workspace);
      }
      throw error;
    }
  }

  async forkSession(input: { source: TrustedSessionBinding; target: TrustedSessionBinding }) {
    validateBinding(input.source, this.options.sessionsRoot);
    validateBinding(input.target, this.options.sessionsRoot);
    if (input.source.sandboxKey === input.target.sandboxKey) throw new SessionExecutionProviderError("Fork target must be a new sandbox", "session_conflict");
    const targetRoot = resolve(this.options.sessionsRoot, input.target.sandboxKey);
    await access(input.source.storage.workspace, constants.R_OK | constants.X_OK);
    try {
      await access(targetRoot);
      throw new SessionExecutionProviderError("Fork target storage already exists", "session_conflict");
    } catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
    const existingSource = this.handles.get(input.source.sandboxKey);
    const sourceBinding = this.bindings.get(input.source.sandboxKey) ?? input.source;
    if (existingSource && resolve(sourceBinding.storage.workspace) !== resolve(input.source.storage.workspace)) throw new SessionExecutionProviderError("Fork source storage mismatch", "session_conflict");
    const source = existingSource ?? await this.createSession(input.source);
    let target: SessionExecutionHandle | undefined;
    const release = async () => {
      await target?.dispose();
      if (!existingSource) await source.dispose();
    };
    try {
      target = await this.createSession(input.target);
      let copiedBytes = 0;
      let copiedEntries = 0;
      const copy = async (sourcePath: string, targetPath: string, depth = 0): Promise<void> => {
        if (depth > 64 || ++copiedEntries > 10_000) throw new Error("Fork file count or depth limit exceeded");
        const info = await source.world.fs.stat(sourcePath);
        if (info.kind === "directory") {
          await target!.world.contextStorage!.fileHistoryFs.mkdir(targetPath, { recursive: true });
          for (const entry of await source.world.fs.readDirectory(sourcePath)) {
            if (sourcePath === join(normalizeStorage(sourceBinding.storage).home, ".pilotdeck") && ["control", "browser"].includes(entry.name)) continue;
            await copy(join(sourcePath, entry.name), join(targetPath, entry.name), depth + 1);
          }
        } else if (info.kind === "file") {
          copiedBytes += info.size;
          if (copiedBytes > 100 * 1024 * 1024) throw new Error("Fork data limit exceeded");
          const bytes = await source.world.fs.readFile(sourcePath);
          if ((bytes as Uint8Array).byteLength > info.size) throw new Error("Fork source changed beyond the data limit");
          await target!.world.contextStorage!.fileHistoryFs.mkdir(dirname(targetPath), { recursive: true });
          await target!.world.contextStorage!.fileHistoryFs.writeFile(targetPath, bytes as Uint8Array);
          await target!.world.contextStorage!.fileHistoryFs.chmod(targetPath, (info.mode ?? 0o600) & 0o777);
        } else throw new Error("Fork rejects special files");
      };
      if (!target.world.contextStorage) throw new Error("Fork requires session file-history operations");
      for (const key of ["workspace", "home", "spill", "artifact"] as const) {
        await copy(normalizeStorage(sourceBinding.storage)[key], normalizeStorage(input.target.storage)[key]);
      }
      let finished = false;
      return {
        commit: async () => { if (finished) return; await release(); finished = true; },
        rollback: async () => { if (finished) return; finished = true; try { await release(); } finally { await rm(targetRoot, { recursive: true, force: true }); } },
      };
    } catch (error) {
      try { await release(); } finally { await rm(targetRoot, { recursive: true, force: true }); }
      throw error;
    }
  }

  async deleteSessionStorage(sessionKey: string): Promise<void> {
    if (!sessionKey.trim()) throw new SessionExecutionProviderError("Session key is required", "session_conflict");
    const binding = [...this.bindings.values()].find((entry) => entry.sessionKey === sessionKey);
    const sandboxKey = binding?.sandboxKey ?? Buffer.from(sessionKey).toString("base64url");
    if (this.activeSessions.has(sandboxKey) || this.deletingSessions.has(sandboxKey)) {
      throw new SessionExecutionProviderError("Cannot delete active session storage", "session_conflict");
    }
    this.deletingSessions.add(sandboxKey);
    try {
      const root = resolve(this.options.sessionsRoot, sandboxKey);
      let actual: string;
      try { actual = await realpath(root); }
      catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return; throw error; }
      if (actual !== root || !isPathWithin(actual, await realpath(this.options.sessionsRoot))) {
        throw new SessionExecutionProviderError("Session deletion path is outside its owned root", "session_conflict");
      }
      await rm(root, { recursive: true, force: true });
      this.bindings.delete(sandboxKey);
    } finally { this.deletingSessions.delete(sandboxKey); }
  }

  dispose(): Promise<void> {
    if (this.disposePromise) return this.disposePromise;
    this.disposed = true;
    this.disposePromise = (async () => {
      await Promise.allSettled([...this.pendingCreates]);
      const results = await Promise.allSettled([...this.handles.values()].map((handle) => handle.dispose()));
      const failures = results.filter((result): result is PromiseRejectedResult => result.status === "rejected");
      if (failures.length) throw new AggregateError(failures.map((result) => result.reason), "Failed to dispose nsjail sessions");
    })();
    return this.disposePromise;
  }

  createSandboxPort(storage?: TrustedSessionBinding["storage"], limits?: SessionIsolationPolicy): SandboxPort {
    const mounts = storage ? normalizeStorage(storage) : undefined;
    return {
      supportsFileMtimeSort: false,
      prepare: async (request) => this.buildCommand(
        mounts
          ? { ...request, executable: resolveGuestExecutable(request.executable, request.env.PATH, this.options.rootfs, mounts), cwd: validateCommandCwd(request.cwd, mounts) }
          : { ...request, executable: resolveGuestExecutable(request.executable, request.env.PATH, this.options.rootfs) },
        this.options.executable ?? "nsjail",
        limits?.network ?? request.policy.network ?? "deny",
        mounts,
        limits,
      ),
    };
  }

  /** Construct the fixed nsjail argv used by each session execution request. */
  buildCommand(
    command: SandboxedCommand,
    nsjailExecutable = this.options.executable ?? "nsjail",
    network: "deny" | "allow" = "deny",
    storageInput?: TrustedSessionBinding["storage"],
    limits?: SessionIsolationPolicy,
  ): SandboxedCommand {
    const proxy = storageInput && this.egress.get(resolve(storageInput.workspace));
    if (network !== "deny" && !proxy) {
      throw new SessionExecutionProviderError("Network allow requires a controlled session egress provider", "provider_unavailable");
    }
    const storage = storageInput ? normalizeStorage(storageInput) : undefined;
    const inheritedKeys = new Set(["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TERM", "COLORTERM", ...(this.options.inheritEnvironmentKeys ?? [])]);
    const environment = {
      ...Object.fromEntries(Object.entries(command.env).filter(([key]) => !(key in process.env) || inheritedKeys.has(key)).map(([key, value]) => [key, storage && value ? toGuestPath(value, storage) : value])),
      HOME: "/home/agent",
      ...(storage ? { PILOTDECK_SESSION_STORAGE_ROOT: "/home/agent" } : {}),
      TMPDIR: "/tmp",
      PYTHONUSERBASE: "/home/agent/.local",
      PIP_CACHE_DIR: "/home/agent/.cache/pip",
      NPM_CONFIG_PREFIX: "/home/agent/.local/npm",
      NPM_CONFIG_CACHE: "/home/agent/.cache/npm",
      NPM_CONFIG_USERCONFIG: "/home/agent/.config/npm/npmrc",
      PATH: guestExecutablePath(command.env.PATH, storage),
    };
    const args = [
        "--quiet",
        "--mode", "o",
        "--cwd", storage ? toGuestPath(command.cwd, storage) : "/workspace",
        "--bindmount_ro", `${this.options.rootfs}:/`,
        ...(proxy ? ["--bindmount_ro", `${proxy.directory}:/run/pilotdeck-egress`] : []),
        "--bindmount", "/dev/null:/dev/null",
        ...["zero", "random", "urandom"].flatMap((device) => ["--bindmount_ro", `/dev/${device}:/dev/${device}`]),
        ...disabledPackageManagerMounts(this.options.rootfs),
        "--bindmount", `${storage?.workspace ?? command.cwd}:/workspace`,
        "--bindmount", `${storage?.home ?? command.cwd}:/home/agent`,
        ...(storage ? Object.entries(guestStoragePaths).flatMap(([name, guest]) => {
          const host = storage![name as keyof SessionStorageMounts];
          return isPathWithin(host, storage!.home) ? [] : ["--bindmount", `${host}:${guest}`];
        }) : []),
        ...(storage ? ["--bindmount", `${storage.temp}:/tmp`] : ["--tmpfsmount", "/tmp"]),
        "--proc_path", "/proc",
        "--user", "65532",
        "--group", "65532",
        "--rlimit_nofile", "256",
        // V8 and browsers reserve virtual address space beyond their RSS.
        // Physical memory is enforced by the session cgroup instead.
        "--rlimit_as", "inf",
        "--rlimit_fsize", "128",
        ...(storage && this.cgroups.has(storage.workspace) ? ["--use_cgroupv2", "--cgroupv2_mount", this.cgroups.get(storage.workspace)!.path] : []),
        ...(limits?.maxMemoryBytes ? ["--cgroup_mem_max", String(limits.maxMemoryBytes)] : []),
        ...(limits?.maxPids || (storage && this.cgroups.has(storage.workspace)) ? ["--cgroup_pids_max", String(limits?.maxPids ?? 512)] : []),
        ...(limits?.maxCpuSeconds ? ["--rlimit_cpu", String(limits.maxCpuSeconds)] : []),
        ...Object.entries(environment).flatMap(([key, value]) => ["--env", `${key}=${value}`]),
        "--",
        ...(proxy ? [resolveGuestExecutable("node", environment.PATH, this.options.rootfs, storage), "/run/pilotdeck-egress/launch.mjs"] : []),
        storage ? toGuestPath(command.executable, storage) : command.executable,
        ...buildGuestCommandArgs(command, storage, Boolean(proxy)).map((arg) => storage ? toGuestArgument(arg, storage) : arg),
      ];
    return {
      executable: nsjailExecutable,
      args,
      cwd: command.cwd,
      env: { ...environment, ...(process.env.LD_LIBRARY_PATH ? { LD_LIBRARY_PATH: process.env.LD_LIBRARY_PATH } : {}) },
    };
  }
}

/** Directory FDs keep guest-created symlinks out of host initialization. */
async function ensureOwnedDirectory(root: string, path: string): Promise<void> {
  const parts = relative(resolve(root), resolve(path)).split("/");
  if (parts.some((part) => !part || part === "..")) throw new SessionExecutionProviderError("Invalid session directory", "session_conflict");
  let parent = await open(await realpath(root), constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW);
  try {
    for (const part of parts) {
      const child = `/proc/self/fd/${parent.fd}/${part}`;
      let next;
      try { next = await open(child, constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW); }
      catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw new SessionExecutionProviderError("Session directory contains a symlink or non-directory", "session_conflict");
        try { await mkdir(child, { mode: 0o700 }); }
        catch (createError) { if ((createError as NodeJS.ErrnoException).code !== "EEXIST") throw createError; }
        next = await open(child, constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW);
      }
      await parent.close();
      parent = next;
    }
  } finally { await parent.close(); }
}

const guestStoragePaths = {
  pipCache: "/home/agent/.cache/pip",
  npmCache: "/home/agent/.cache/npm",
  spill: "/home/agent/.pilotdeck/spill",
  artifact: "/home/agent/.pilotdeck/artifact",
  browserProfile: "/home/agent/.pilotdeck/browser/profile",
  browserDownload: "/home/agent/.pilotdeck/browser/download",
} as const;

function toGuestPath(path: string, storage: SessionStorageMounts): string {
  const mappings = { ...guestStoragePaths, workspace: "/workspace", home: "/home/agent", temp: "/tmp" };
  for (const [name, guest] of Object.entries(mappings)) {
    const root = storage[name as keyof SessionStorageMounts];
    if (isAbsolute(path) && isPathWithin(path, root)) {
      return join(guest, relative(root, path));
    }
  }
  return path;
}

function toGuestArgument(arg: string, storage: SessionStorageMounts): string {
  const separator = arg.startsWith("--") ? arg.indexOf("=") : -1;
  return separator < 0 ? toGuestPath(arg, storage)
    : `${arg.slice(0, separator + 1)}${toGuestPath(arg.slice(separator + 1), storage)}`;
}

/** apt/dpkg are intentionally unavailable in the session rootfs. */
function disabledPackageManagerMounts(rootfs: string): string[] {
  return ["/usr/bin/apt", "/usr/bin/apt-get", "/usr/bin/apt-cache", "/usr/bin/dpkg", "/usr/bin/dpkg-deb", "/usr/bin/dpkg-query"]
    .filter((path) => {
      try {
        accessSync(join(rootfs, path), constants.F_OK);
        return true;
      } catch {
        return false;
      }
    })
    .flatMap((path) => ["--bindmount", `/dev/null:${path}`]);
}

function buildGuestCommandArgs(command: SandboxedCommand, storage?: SessionStorageMounts, usesEgress = false): readonly string[] {
  if (!storage || command.executable !== "/bin/sh" || command.args[0] !== "-c" || typeof command.args[1] !== "string") {
    return command.args;
  }
  const environmentFile = "/home/agent/.pilotdeck/environment.sh";
  const commandText = command.args[1];
  const proxyKeys = "HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy NO_PROXY no_proxy";
  const wrapped = [
    ...(usesEgress ? ["readonly PILOTDECK_RUNTIME_PROXY=$HTTP_PROXY"] : []),
    `if [ -f ${environmentFile} ]; then . ${environmentFile}; fi`,
    ...(usesEgress ? ["export HTTP_PROXY=$PILOTDECK_RUNTIME_PROXY HTTPS_PROXY=$PILOTDECK_RUNTIME_PROXY ALL_PROXY=$PILOTDECK_RUNTIME_PROXY http_proxy=$PILOTDECK_RUNTIME_PROXY https_proxy=$PILOTDECK_RUNTIME_PROXY all_proxy=$PILOTDECK_RUNTIME_PROXY NO_PROXY= no_proxy="] : []),
    `trap '${usesEgress ? `unset ${proxyKeys}; ` : ""}export -p > ${environmentFile}' EXIT`,
    commandText,
  ].join("; ");
  return ["-c", wrapped, ...command.args.slice(2)];
}

function isExecutableAvailable(executable: string): boolean {
  if (executable.includes("/")) {
    try {
      return spawnSync(executable, ["--help"], { stdio: "ignore", timeout: 2_000 }).status !== null;
    } catch {
      return false;
    }
  }
  try {
    return spawnSync("which", [executable], { stdio: "ignore", timeout: 2_000 }).status === 0;
  } catch {
    return false;
  }
}

function validateBinding(binding: TrustedSessionBinding, sessionsRoot: string): void {
  if (!/^[A-Za-z0-9_-]{1,128}$/.test(binding.sandboxKey)) {
    throw new SessionExecutionProviderError("Invalid sandbox key", "session_conflict");
  }
  if (binding.generation < 1 || !Number.isSafeInteger(binding.generation)) {
    throw new SessionExecutionProviderError("Invalid session generation", "session_conflict");
  }
  if (Object.values(binding.storage).some((path) => !isAbsolute(path))) {
    throw new SessionExecutionProviderError("Session storage paths must be absolute", "session_conflict");
  }
  for (const name of ["maxMemoryBytes", "maxPids", "maxCpuSeconds", "workspaceBytes"] as const) {
    const value = binding.policy[name];
    if (value !== undefined && (!Number.isSafeInteger(value) || value <= 0)) {
      throw new SessionExecutionProviderError(`Invalid session limit: ${name}`, "session_conflict");
    }
  }
  if (binding.policy.workspaceBytes !== undefined) {
    throw new SessionExecutionProviderError(
      "workspaceBytes requires a quota-backed storage provider and is not supported by nsjail mounts",
      "session_conflict",
    );
  }
  const sessionRoot = resolve(sessionsRoot, binding.sandboxKey);
  const storage = normalizeStorage(binding.storage);
  const seen = new Set<string>();
  for (const [name, value] of Object.entries(storage)) {
    const storagePath = resolve(value);
    const storageRelative = relative(sessionRoot, storagePath);
    if (!isAbsolute(value) || storageRelative === "" || storageRelative.startsWith("..") || isAbsolute(storageRelative)) {
      throw new SessionExecutionProviderError(`${name} is outside the session storage root`, "session_conflict");
    }
    if (seen.has(storagePath)) {
      throw new SessionExecutionProviderError(`Session storage paths must be distinct: ${name}`, "session_conflict");
    }
    seen.add(storagePath);
  }
  for (const [name, path] of Object.entries(storage)) {
    if (name !== "control" && (isPathWithin(storage.control, path) || isPathWithin(path, storage.control))) {
      throw new SessionExecutionProviderError("Session control must be outside guest-owned roots", "session_conflict");
    }
  }
  for (const name of ["workspace", "home", "temp"] as const) {
    for (const other of ["workspace", "home", "temp"] as const) {
      if (name !== other && isPathWithin(storage[name], storage[other])) {
        throw new SessionExecutionProviderError("Workspace, home and temp must not overlap", "session_conflict");
      }
    }
  }
  for (const [name, guest] of Object.entries(guestStoragePaths)) {
    const host = storage[name as keyof SessionStorageMounts];
    if (isPathWithin(host, storage.workspace) || isPathWithin(host, storage.temp)
      || (isPathWithin(host, storage.home) && host !== join(storage.home, relative("/home/agent", guest)))) {
      throw new SessionExecutionProviderError(`Unsafe nested session mount: ${name}`, "session_conflict");
    }
  }
}

function normalizeStorage(storage: TrustedSessionBinding["storage"]): SessionStorageMounts {
  const home = resolve(storage.home);
  const sessionRoot = resolve(home, "..");
  return {
    workspace: resolve(storage.workspace),
    home,
    temp: resolve(storage.temp),
    pipCache: resolve(storage.pipCache ?? join(home, ".cache", "pip")),
    npmCache: resolve(storage.npmCache ?? join(home, ".cache", "npm")),
    control: resolve(storage.control ?? join(sessionRoot, "control")),
    spill: resolve(storage.spill ?? join(sessionRoot, "spill")),
    artifact: resolve(storage.artifact ?? join(sessionRoot, "artifact")),
    browserProfile: resolve(storage.browserProfile ?? join(sessionRoot, "browser", "profile")),
    browserDownload: resolve(storage.browserDownload ?? join(sessionRoot, "browser", "download")),
  };
}

function validateCommandCwd(cwd: string, storage: SessionStorageMounts): string {
  const target = resolve(cwd);
  const allowed = Object.values(storage).some((root) => isPathWithin(target, resolve(root)));
  if (!allowed) {
    throw new SessionExecutionProviderError(`Command cwd ${target} is outside the session storage`, "session_conflict");
  }
  return target;
}

function resolveGuestExecutable(
  executable: string,
  pathValue: string | undefined,
  rootfs: string,
  storage?: SessionStorageMounts,
): string {
  if (isAbsolute(executable)) {
    if (storage && Object.values(storage).some((root) => isPathWithin(executable, root))) return executable;
    if (executable.startsWith("/home/agent/")) return executable;
    try { accessSync(join(rootfs, executable), constants.X_OK); return executable; } catch {}
    // Builtin consumers resolve host interpreters and bundled rg. Select the
    // corresponding rootfs binary rather than mounting the host executable.
    if (!/^(node|rg|python(?:3(?:\.\d+)?)?)$/.test(basename(executable))) return executable;
    executable = basename(executable);
  }
  for (const guestDirectory of guestExecutablePath(pathValue, storage).split(":")) {
    if (!guestDirectory.startsWith("/")) continue;
    const hostDirectory = guestDirectory === "/home/agent" || guestDirectory.startsWith("/home/agent/")
      ? storage ? join(storage.home, guestDirectory.slice("/home/agent".length)) : undefined
      : join(rootfs, guestDirectory);
    if (!hostDirectory) continue;
    const hostCandidate = join(hostDirectory, executable);
    try {
      accessSync(hostCandidate, constants.X_OK);
      return join(guestDirectory, executable);
    } catch {
      // Try the next guest PATH entry.
    }
  }
  return executable;
}

function guestExecutablePath(pathValue?: string, storage?: SessionStorageMounts): string {
  const supplied = storage && pathValue ? toGuestPath(pathValue, storage) : pathValue;
  return [...new Set([
    "/home/agent/.local/npm/bin", "/home/agent/.local/bin",
    ...(supplied?.split(":") ?? []), "/usr/local/bin", "/usr/bin", "/bin",
  ].filter((entry) => entry.startsWith("/")))].join(":");
}

function isPathWithin(candidate: string, root: string): boolean {
  const child = relative(root, candidate);
  return child === "" || (!child.startsWith("..") && !isAbsolute(child));
}
