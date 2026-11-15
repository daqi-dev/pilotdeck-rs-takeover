import {
  BackgroundTaskRuntime,
  type BackgroundTaskCompletionHandler,
} from "../../task/runtime/BackgroundTaskRuntime.js";
import { JsonFileBackgroundTaskSnapshotStore } from "../../task/storage/BackgroundTaskSnapshotStore.js";
import { join } from "node:path";
import {
  createNodeAttachmentDeliveryPort,
  type AttachmentDeliveryPort,
} from "./AttachmentDeliveryPort.js";
import type { CodeRuntimePort } from "./CodeRuntimePort.js";
import { createNodeCodeRuntimePort } from "./NodeCodeRuntimePort.js";
import { createNodeSandboxedCodeRuntimePort } from "./SandboxedCodeRuntimePort.js";
import { createNodeExecutionWorkspacePort } from "./NodeExecutionWorkspacePort.js";
import { createNodeFsPort } from "./NodeFsPort.js";
import { createNodeSandboxPort } from "./NodeSandboxPort.js";
import { createNodePlanStoragePort, type PlanStoragePort } from "./PlanStoragePort.js";
import { createNodeDetachedShellPort, type DetachedShellPort } from "./DetachedShellPort.js";
import { createNodeExecutionTransportPort, type ExecutionTransportPort } from "./ExecutionTransportPort.js";
import type { ExecutionWorkspacePort } from "./ExecutionWorkspacePort.js";
import type { FsPort } from "./FsPort.js";
import type { NetworkPort } from "./NetworkPort.js";
import type { FileHistoryFsPort } from "../../session/filesystem/FileHistoryFsPort.js";
import type { InstructionStoragePort } from "../../context/instructions/InstructionStoragePort.js";
import type { ToolResultSpillPort } from "../../context/budget/ToolResultSpillPort.js";
import {
  DEFAULT_SANDBOX_MODE,
  type SandboxMode,
  type SandboxPolicy,
  type SandboxPort,
} from "./SandboxPort.js";
import { createNodeSandboxedFsPort } from "./SandboxedFsPort.js";
import { createNodeShellPort, type ShellPort } from "./ShellPort.js";
import { createNodeSandboxedDetachedShellPort } from "./SandboxedDetachedShellPort.js";
import { createNodeSandboxedShellPort } from "./SandboxedShellPort.js";
import { createNodeSandboxedSubprocessPort } from "./SandboxedSubprocessPort.js";
import { createNodeSubprocessPort, type SubprocessPort } from "./SubprocessPort.js";

/** Execution-world policy supplied to the execute_code consumer. */
export type ExecuteCodeSandbox = {
  port: SandboxPort;
  /** The selected native mode, when the host can state it explicitly. */
  mode?: SandboxMode;
  resolvePolicy(input: { workspaceRoot: string; executionRoot: string }): SandboxPolicy;
};

/**
 * One project-scoped execution-world composition. It owns only providers
 * with a project lifetime; per-run workspaces and transports remain owned by
 * their respective consumers.
 */
export type ExecutionWorldBundle = {
  readonly network?: NetworkPort;
  readonly contextStorage?: {
    fileHistoryFs: FileHistoryFsPort;
    fileHistoryRoot: string;
    instructionStorage: InstructionStoragePort;
    toolResultSpill: ToolResultSpillPort;
    spillRoot: string;
  };
  readonly fs: FsPort;
  readonly subprocess: SubprocessPort;
  readonly shell: ShellPort;
  readonly detachedShell: DetachedShellPort;
  readonly attachmentDelivery: AttachmentDeliveryPort;
  readonly planStorage: PlanStoragePort;
  readonly executionWorkspace: ExecutionWorkspacePort;
  readonly codeRuntime: CodeRuntimePort;
  readonly executionTransport: ExecutionTransportPort;
  readonly executeCodeSandbox: ExecuteCodeSandbox;
  readonly backgroundTasks: BackgroundTaskRuntime;
  /** Stop new execution work and drain every project-owned provider. */
  dispose(): Promise<void>;
};

export type ExecutionWorldBundleParts = Omit<ExecutionWorldBundle, "dispose">;

export type CreateNodeExecutionWorldBundleOptions = {
  now?: () => Date;
  onBackgroundTaskCompletion?: BackgroundTaskCompletionHandler;
  /** File-effect policy selected by the project profile for process execution. */
  sandboxMode?: SandboxMode;
  /** Project identity selected by application composition for durable task state. */
  projectRoot?: string;
  /** Explicit state root for tests or alternate project storage providers. */
  backgroundTaskStateDir?: string;
  /** Optional session-bound process sandbox. */
  sandboxPort?: SandboxPort;
  /** Keep the injected provider active even for the unrestricted tool policy. */
  forceSandbox?: boolean;
  /** Fixed root used by session-bound filesystem providers. */
  workspaceRoot?: string;
  /** Session shutdown aborts foreground subprocesses as well as background work. */
  executionSignal?: AbortSignal;
  /** Bound foreground command capture independently of progress consumers. */
  maxSubprocessOutputBytes?: number;
};

/** Compose selected execution providers into one lifecycle owner. */
export function createExecutionWorldBundle(parts: ExecutionWorldBundleParts): ExecutionWorldBundle {
  let disposePromise: Promise<void> | undefined;
  return {
    ...parts,
    dispose: () => {
      if (!disposePromise) disposePromise = disposeOwnedProviders(parts);
      return disposePromise;
    },
  };
}

/** Native project-scoped execution-world provider selection. */
export function createNodeExecutionWorldBundle(
  options: CreateNodeExecutionWorldBundleOptions = {},
): ExecutionWorldBundle {
  const nativeSubprocess = createNodeSubprocessPort(undefined, { maxOutputBytes: options.maxSubprocessOutputBytes });
  const executionSignal = options.executionSignal;
  const activeCalls = new Set<Promise<unknown>>();
  const track = <T>(operation: () => Promise<T>): Promise<T> => {
    const pending = operation();
    activeCalls.add(pending);
    void pending.then(() => activeCalls.delete(pending), () => activeCalls.delete(pending));
    return pending;
  };
  const withSignal = <T extends { signal?: AbortSignal }>(request: T): T => ({
    ...request,
    signal: executionSignal
      ? request.signal ? AbortSignal.any([executionSignal, request.signal]) : executionSignal
      : request.signal,
  });
  const subprocess: SubprocessPort = {
    execute: (request) => track(() => nativeSubprocess.execute(withSignal(request))),
    executeFile: (request) => track(() => nativeSubprocess.executeFile!(withSignal(request))),
  };
  const sandboxMode = options.sandboxMode ?? DEFAULT_SANDBOX_MODE;
  const sandbox = options.sandboxPort ?? createNodeSandboxPort();
  const nodeFs = createNodeFsPort();
  const useSandbox = options.forceSandbox === true || sandboxMode !== "danger-full-access";
  const exposedSubprocess = !useSandbox
    ? subprocess
    : createNodeSandboxedSubprocessPort({
        sandbox,
        subprocess,
        resolvePolicy: ({ workspaceRoot }) => ({ mode: sandboxMode, workspaceRoot: options.workspaceRoot ?? workspaceRoot }),
      });
  const fs = !useSandbox
    ? nodeFs
    : createNodeSandboxedFsPort({ fs: nodeFs, sandboxMode, workspaceRoot: options.workspaceRoot });
  const selectedDetachedShell = !useSandbox
    ? createNodeDetachedShellPort()
    : createNodeSandboxedDetachedShellPort({
        sandbox,
        resolvePolicy: ({ workspaceRoot }) => ({ mode: sandboxMode, workspaceRoot }),
      });
  const detachedHandles = new Set<Awaited<ReturnType<DetachedShellPort["start"]>>>();
  const detachedShell: DetachedShellPort = {
    start: (request) => track(async () => {
      executionSignal?.throwIfAborted();
      const handle = await selectedDetachedShell.start(request);
      detachedHandles.add(handle);
      const abort = () => handle.terminate("SIGTERM");
      executionSignal?.addEventListener("abort", abort, { once: true });
      void handle.exit.then(() => {
        detachedHandles.delete(handle);
        executionSignal?.removeEventListener("abort", abort);
      });
      if (executionSignal?.aborted) abort();
      return handle;
    }),
  };
  const shell = !useSandbox
    ? createNodeShellPort(subprocess)
    : createNodeSandboxedShellPort({
        sandbox,
        subprocess,
        resolvePolicy: ({ workspaceRoot }) => ({ mode: sandboxMode, workspaceRoot }),
      });
  const backgroundTaskStateDir = options.backgroundTaskStateDir
    ?? (options.projectRoot ? join(options.projectRoot, ".pilotdeck", "background-tasks") : undefined);
  const snapshotStore = backgroundTaskStateDir
    ? new JsonFileBackgroundTaskSnapshotStore({ filePath: join(backgroundTaskStateDir, "state.json") })
    : undefined;
  const selectedCodeRuntime = !useSandbox
    ? createNodeCodeRuntimePort()
    : createNodeSandboxedCodeRuntimePort({
        sandbox,
        runtime: createNodeCodeRuntimePort(),
        probeCwd: options.workspaceRoot ?? options.projectRoot,
        resolvePolicy: ({ workspaceRoot }) => ({ mode: sandboxMode, workspaceRoot }),
      });
  const codeRuntime: CodeRuntimePort = {
    enforcesSandbox: selectedCodeRuntime.enforcesSandbox,
    resolveExecutable: (candidates, env, signal) => selectedCodeRuntime.resolveExecutable(
      candidates, env, withSignal({ signal }).signal,
    ),
    run: (request) => selectedCodeRuntime.run(withSignal(request)),
    dispose: () => selectedCodeRuntime.dispose?.() ?? Promise.resolve(),
  };
  const world = createExecutionWorldBundle({
    fs,
    subprocess: exposedSubprocess,
    shell,
    detachedShell,
    attachmentDelivery: createNodeAttachmentDeliveryPort(),
    planStorage: createNodePlanStoragePort(),
    executionWorkspace: createNodeExecutionWorkspacePort(),
    codeRuntime,
    executionTransport: createNodeExecutionTransportPort(),
    executeCodeSandbox: {
      port: sandbox,
      mode: sandboxMode,
      resolvePolicy: ({ workspaceRoot, executionRoot }) => ({
        mode: sandboxMode,
        workspaceRoot,
        executionRoot,
      }),
    },
    backgroundTasks: new BackgroundTaskRuntime({
      now: options.now,
      shell: detachedShell,
      onCompletion: options.onBackgroundTaskCompletion,
      ...(backgroundTaskStateDir ? { diskSpillDir: join(backgroundTaskStateDir, "output") } : {}),
      ...(snapshotStore ? { snapshotStore } : {}),
    }),
  });
  return {
    ...world,
    async dispose() {
      for (const handle of detachedHandles) handle.terminate("SIGTERM");
      const escalation = setTimeout(() => {
        for (const handle of detachedHandles) handle.terminate("SIGKILL");
      }, 1_000);
      escalation.unref();
      await world.dispose();
      await Promise.allSettled([...activeCalls]);
      await Promise.allSettled([...detachedHandles].map((handle) => handle.exit));
      clearTimeout(escalation);
    },
  };
}

async function disposeOwnedProviders(parts: ExecutionWorldBundleParts): Promise<void> {
  const results = await Promise.allSettled([
    parts.codeRuntime.dispose?.() ?? Promise.resolve(),
    parts.backgroundTasks.dispose(),
  ]);
  const failures = results
    .filter((result): result is PromiseRejectedResult => result.status === "rejected")
    .map((result) => result.reason);
  if (failures.length === 1) throw failures[0];
  if (failures.length > 1) {
    throw new AggregateError(failures, "Failed to dispose execution-world providers.");
  }
}
